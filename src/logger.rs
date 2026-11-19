//! Logging: to stderr while attached to a terminal (always with -f), to
//! syslog once running in the background.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

static SYSLOG: AtomicBool = AtomicBool::new(false);
static LOGGER: Logger = Logger;

struct Logger;

impl Log for Logger {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        // Other crates (fuser) only get through with warnings and errors.
        m.level() <= log::max_level()
            && (m.target().starts_with("sqlitefuse") || m.level() <= Level::Warn)
    }

    fn log(&self, r: &Record<'_>) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let msg = r.args().to_string();
        if SYSLOG.load(Ordering::Relaxed) {
            let priority = match r.level() {
                Level::Error => libc::LOG_ERR,
                Level::Warn => libc::LOG_WARNING,
                Level::Info => libc::LOG_INFO,
                Level::Debug | Level::Trace => libc::LOG_DEBUG,
            };
            let msg = CString::new(msg.replace('\0', "")).unwrap_or_default();
            // SAFETY: both strings are valid and NUL-terminated.
            unsafe { libc::syslog(priority, c"%s".as_ptr(), msg.as_ptr()) };
        } else {
            let prefix = match r.level() {
                Level::Error => "error: ",
                Level::Warn => "warning: ",
                Level::Info => "",
                Level::Debug | Level::Trace => "debug: ",
            };
            eprintln!("sqlitefuse: {prefix}{msg}");
        }
    }

    fn flush(&self) {}
}

pub fn init(debug: bool) {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
}

/// Sends everything from now on to syslog (and so to the journal, under
/// `sqlitefuse`).
pub fn to_syslog() {
    // SAFETY: the ident is a static string, as openlog requires.
    unsafe { libc::openlog(c"sqlitefuse".as_ptr(), libc::LOG_PID, libc::LOG_DAEMON) };
    SYSLOG.store(true, Ordering::Relaxed);
}
