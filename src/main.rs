//! sqlitefuse: mounts an SQLite database as a directory of .jsonl files, one
//! per table or view. Read-only.

mod db;
mod fs;
mod json;
mod logger;
mod man;

use clap::{ArgAction, CommandFactory, Parser};
use fuser::{Config, MountOption, Session, SessionACL};
use log::{error, info, warn};
use std::path::PathBuf;
use std::process::exit;

#[derive(Parser, Debug)]
#[command(name = "sqlitefuse", about, author, version, after_long_help = include_str!("help.txt"))]
struct Cli {
    /// SQLite database to mount
    #[arg(required_unless_present = "man")]
    database: Option<PathBuf>,

    /// Directory to mount it on
    #[arg(required_unless_present = "man")]
    mountpoint: Option<PathBuf>,

    /// Mount options, separated by commas (see MOUNT OPTIONS)
    #[arg(short = 'o', value_name = "OPTIONS", action = ArgAction::Append)]
    options: Vec<String>,

    /// Stay in the foreground and log to stderr
    #[arg(short = 'f')]
    foreground: bool,

    /// Log every filesystem operation; implies -f
    #[arg(short = 'd')]
    debug: bool,

    /// Print the man page
    #[arg(long, exclusive = true)]
    man: bool,
}

#[derive(Debug, PartialEq)]
struct MountOpts {
    immutable: bool,
    lock: bool,
    debug: bool,
    uid: u32,
    gid: u32,
    acl: SessionACL,
    max_value: u32,
    max_view: u64,
}

const DEFAULT_MAX_VALUE: u32 = 256 << 20;
const DEFAULT_MAX_VIEW: u64 = 1 << 30;

/// A number of bytes with an optional K, M or G suffix (powers of 1024).
fn parse_size(s: &str) -> Option<u64> {
    let (num, mult) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 1 << 10),
        (i, 'm' | 'M') => (&s[..i], 1 << 20),
        (i, 'g' | 'G') => (&s[..i], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mult)
}

fn parse_options(opts: &[String], uid: u32, gid: u32) -> Result<MountOpts, String> {
    let mut m = MountOpts {
        immutable: false,
        lock: false,
        debug: false,
        uid,
        gid,
        acl: SessionACL::Owner,
        max_value: DEFAULT_MAX_VALUE,
        max_view: DEFAULT_MAX_VIEW,
    };
    for o in opts
        .iter()
        .flat_map(|s| s.split(','))
        .filter(|s| !s.is_empty())
    {
        let (k, v) = o.split_once('=').map_or((o, None), |(k, v)| (k, Some(v)));
        let invalid = || format!("invalid value in {o}");
        let num = |v: Option<&str>| v.and_then(|v| v.parse::<u32>().ok()).ok_or_else(invalid);
        let size = |v: Option<&str>| {
            v.and_then(parse_size)
                .filter(|&n| n > 0)
                .ok_or_else(invalid)
        };
        match k {
            "lock" => m.lock = true,
            "immutable" => m.immutable = true,
            "debug" => m.debug = true,
            "uid" => m.uid = num(v)?,
            "gid" => m.gid = num(v)?,
            "allow_other" => m.acl = SessionACL::All,
            "allow_root" => m.acl = SessionACL::RootAndOwner,
            "max_value" => {
                let n = size(v)?;
                if n > db::MAX_LENGTH as u64 {
                    return Err(format!("max_value can be at most {}", db::MAX_LENGTH));
                }
                m.max_value = n as u32;
            }
            "max_view" => m.max_view = size(v)?,
            "rw" => warn!("ignoring 'rw': the mount is always read-only"),
            // Options mount(8) and fstab pass to the helper; they change nothing.
            "ro"
            | "defaults"
            | "auto"
            | "noauto"
            | "user"
            | "nouser"
            | "users"
            | "owner"
            | "nofail"
            | "_netdev"
            | "dev"
            | "nodev"
            | "suid"
            | "nosuid"
            | "exec"
            | "noexec"
            | "atime"
            | "noatime"
            | "relatime"
            | "strictatime"
            | "nodiratime"
            | "diratime"
            | "async"
            | "sync"
            | "dirsync"
            | "default_permissions"
            | "fsname"
            | "subtype" => {}
            k if k.starts_with("x-") || k.starts_with("comment") => {}
            _ => return Err(format!("unknown option: {o}")),
        }
    }
    Ok(m)
}

fn die(msg: impl std::fmt::Display) -> ! {
    error!("{msg}");
    exit(1)
}

/// Goes to the background. The parent waits for the child to confirm the
/// mount over a pipe and exits with 0 or 1 accordingly.
fn daemonize() -> libc::c_int {
    let mut fds = [0; 2];
    // SAFETY: POSIX calls with valid arguments; no threads exist yet.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            die("pipe failed");
        }
        match libc::fork() {
            -1 => die("fork failed"),
            0 => {
                libc::close(fds[0]);
                libc::setsid();
                fds[1]
            }
            _ => {
                libc::close(fds[1]);
                let mut b = 0u8;
                let n = libc::read(fds[0], (&mut b as *mut u8).cast(), 1);
                libc::_exit(if n == 1 && b == b'1' { 0 } else { 1 });
            }
        }
    }
}

/// Confirms the mount to the waiting parent, logs to syslog from now on and
/// lets go of the terminal.
fn detach(ready_fd: libc::c_int) {
    logger::to_syslog();
    // SAFETY: our own descriptors; /dev/null exists.
    unsafe {
        libc::write(ready_fd, b"1".as_ptr().cast(), 1);
        libc::close(ready_fd);
        let _ = libc::chdir(c"/".as_ptr());
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
}

/// Blocks SIGINT, SIGTERM and SIGHUP in every thread and returns the set,
/// so a dedicated thread can handle them with sigwait.
fn block_signals() -> libc::sigset_t {
    // SAFETY: the sigset_t is initialized with sigemptyset.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn main() {
    let cli = Cli::parse();
    if cli.man {
        match man::render(Cli::command(), &mut std::io::stdout()) {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                eprintln!("sqlitefuse: {e}");
                exit(1);
            }
            _ => {}
        }
        return;
    }

    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let opts = parse_options(&cli.options, uid, gid);
    let debug = cli.debug || opts.as_ref().is_ok_and(|o| o.debug);
    logger::init(debug);
    let opts = opts.unwrap_or_else(|e| die(e));
    let foreground = cli.foreground || debug;

    let (Some(db_path), Some(mountpoint)) = (cli.database, cli.mountpoint) else {
        unreachable!("clap requires both unless --man");
    };
    // Resolve to absolute paths before daemonizing (which changes to /).
    let db_path = std::fs::canonicalize(&db_path)
        .unwrap_or_else(|e| die(format!("{}: {e}", db_path.display())));
    if !db_path.is_file() {
        die(format!("{}: not a file", db_path.display()));
    }
    let mountpoint = std::fs::canonicalize(&mountpoint)
        .unwrap_or_else(|e| die(format!("{}: {e}", mountpoint.display())));
    if !mountpoint.is_dir() {
        die(format!("{}: not a directory", mountpoint.display()));
    }

    let db = db::Config {
        path: db_path.clone(),
        immutable: opts.immutable,
        max_value: opts.max_value,
        max_view: opts.max_view,
    };
    // Check that it is a readable SQLite database before mounting.
    if let Err(e) = db::open(&db, None).and_then(|c| db::load_catalog(&c)) {
        die(format!(
            "{}: cannot open as an SQLite database: {e}",
            db_path.display()
        ));
    }

    let ready_fd = if foreground { None } else { Some(daemonize()) };

    // Connections are opened after the fork: SQLite does not support
    // inheriting them.
    let fs = fs::SqliteFs::new(fs::Options {
        db,
        lock: opts.lock,
        uid: opts.uid,
        gid: opts.gid,
    })
    .unwrap_or_else(|e| die(format!("{}: {e}", db_path.display())));

    let mut cfg = Config::default();
    cfg.mount_options = vec![
        MountOption::RO,
        MountOption::FSName(db_path.to_string_lossy().replace(',', "_")),
        MountOption::Subtype("sqlitefuse".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::NoAtime,
    ];
    cfg.acl = opts.acl;
    cfg.n_threads = Some(std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 8)));
    cfg.clone_fd = true;

    let sigs = block_signals();
    let mut session = Session::new(fs, &mountpoint, &cfg)
        .unwrap_or_else(|e| die(format!("could not mount on {}: {e}", mountpoint.display())));
    info!("mounted {} on {}", db_path.display(), mountpoint.display());

    let mut unmounter = session.unmount_callable();
    std::thread::spawn(move || {
        let mut sig = 0;
        // SAFETY: sigs is a valid set, blocked in every thread.
        unsafe { libc::sigwait(&sigs, &mut sig) };
        let _ = unmounter.unmount();
    });

    if let Some(fd) = ready_fd {
        detach(fd);
    }
    if let Err(e) = session.run() {
        die(e);
    }
    info!("unmounted {}", mountpoint.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("4k"), Some(4096));
        assert_eq!(parse_size("256M"), Some(256 << 20));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("M"), None);
        assert_eq!(parse_size("1.5G"), None);
        assert_eq!(parse_size("99999999999999999999G"), None);
    }

    #[test]
    fn options() {
        let o = |s: &str| parse_options(&[s.to_string()], 1000, 100);
        let m = o("ro,lock,uid=5,allow_other,max_view=2M,noatime,x-systemd.automount").unwrap();
        assert!(m.lock && !m.immutable);
        assert_eq!(
            (m.uid, m.gid, m.acl, m.max_view),
            (5, 100, SessionACL::All, 2 << 20)
        );
        assert_eq!(m.max_value, DEFAULT_MAX_VALUE);
        assert!(o("debug").unwrap().debug);
        assert!(o("views").is_err());
        assert!(o("uid=abc").is_err());
        assert!(o("max_value=0").is_err());
        assert!(o("max_value=2G").is_err());
        assert!(o("max_view").is_err());
    }
}
