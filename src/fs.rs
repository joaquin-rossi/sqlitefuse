//! FUSE implementation: a root directory with one .jsonl file per table or view.

use crate::db::{self, Catalog, Entry, Source, Stats, Version};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use log::{debug, error, info, warn};
use rusqlite::Connection;
use rusqlite::types::Value;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

const ROOT: u64 = 1;
/// Names rarely change: cache them for 1 s.
const TTL_ENTRY: Duration = Duration::from_secs(1);
const TTL_DIR: Duration = Duration::from_secs(1);
/// File attributes are not cached: the kernel asks on every stat and before
/// reading past the size it knows. For an open file that request carries the
/// handle and gets that snapshot's size, so a read is never cut short or
/// padded because of a stale size.
const TTL_FILE: Duration = Duration::ZERO;
/// SQLite page cache per handle connection, in KiB.
const HANDLE_CACHE_KIB: u32 = 2048;
/// Already-served bytes kept behind the last requested offset, to absorb
/// kernel reads that arrive slightly out of order.
const BACK_WINDOW: u64 = 256 << 10;
/// Extra bytes requested with each chunk.
const READ_AHEAD: usize = 256 << 10;
const POOL_MAX: usize = 4;

pub struct Options {
    pub db: db::Config,
    pub lock: bool,
    pub uid: u32,
    pub gid: u32,
}

type R<T> = Result<T, Errno>;

fn eio(e: impl Display) -> Errno {
    error!("{e}");
    Errno::EIO
}

/// Maps an SQLite error to an errno.
fn sql_err(e: rusqlite::Error) -> Errno {
    sql_err_in("")(e)
}

/// Like `sql_err`, naming the file the error happened in. Hitting a size
/// limit is the database's doing, not ours: it is logged as a warning and
/// reported as EFBIG.
fn sql_err_in(file: &str) -> impl Fn(rusqlite::Error) -> Errno + '_ {
    move |e| {
        use rusqlite::ErrorCode::{DiskFull, TooBig};
        let at = if file.is_empty() {
            String::new()
        } else {
            format!("{file}: ")
        };
        match e.sqlite_error_code() {
            Some(TooBig | DiskFull) => {
                warn!("{at}{e} (see max_value and max_view)");
                Errno::EFBIG
            }
            _ => eio(format!("{at}{e}")),
        }
    }
}

fn errno_name(e: Errno) -> String {
    std::io::Error::from_raw_os_error(e.code()).to_string()
}

/// `-> ok` or `-> error` for the debug log.
fn outcome<T>(r: &R<T>, ok: impl FnOnce(&T) -> String) -> String {
    match r {
        Ok(v) => ok(v),
        Err(e) => errno_name(*e),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Meta {
    conn: Connection,
    catalog: Arc<Catalog>,
}

struct Cached {
    version: Version,
    stats: Arc<Stats>,
}

/// Size and checkpoint cache for one table. The mutex also coalesces
/// concurrent computations: the second caller waits and finds the result.
type Slot = Arc<Mutex<Option<Cached>>>;

/// Data version a handle saw. `Unique` when it could not be tied to a known
/// version (a commit landed right during the open).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum VKey {
    At(Version),
    Unique(u64),
}

/// Which version filled the kernel page cache for an inode, and how many
/// handles are open for each version.
#[derive(Default)]
struct InodeOpens {
    owner: Option<VKey>,
    open: HashMap<VKey, usize>,
}

enum Mode {
    /// Read transaction held from open until release (WAL, `-o lock` or
    /// `-o immutable`).
    Snapshot,
    /// Rollback without lock: data_version is compared before each chunk.
    Checked(i64),
    /// Content materialized into a temporary table on open (views).
    Detached,
}

struct HandleState {
    /// File name, for messages.
    file: String,
    conn: Connection,
    src: Source,
    stats: Arc<Stats>,
    mode: Mode,
    /// Bytes starting at `buf_start`; the next chunk starts at
    /// `buf_start + buf.len()`, right after the row whose key is `cursor`.
    buf: Vec<u8>,
    buf_start: u64,
    cursor: Option<Vec<Value>>,
    done: bool,
}

struct Handle {
    ino: u64,
    vkey: VKey,
    size: u64,
    /// Process (tgid) that opened it.
    owner: u32,
    state: Mutex<HandleState>,
}

pub struct SqliteFs {
    opts: Options,
    wal: PathBuf,
    meta: Mutex<Meta>,
    slots: Mutex<HashMap<u64, Slot>>,
    pool: Mutex<Vec<Connection>>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    opens: Mutex<HashMap<u64, InodeOpens>>,
    next_id: AtomicU64,
}

impl SqliteFs {
    pub fn new(opts: Options) -> rusqlite::Result<Self> {
        let conn = db::open(&opts.db, None)?;
        let catalog = Arc::new(db::load_catalog(&conn)?);
        let mut wal = opts.db.path.clone().into_os_string();
        wal.push("-wal");
        Ok(SqliteFs {
            wal: wal.into(),
            opts,
            meta: Mutex::new(Meta { conn, catalog }),
            slots: Mutex::default(),
            pool: Mutex::default(),
            handles: Mutex::default(),
            opens: Mutex::default(),
            next_id: AtomicU64::new(1),
        })
    }

    /// Current version according to the metadata connection. Reloads the
    /// catalog if the schema changed.
    fn current(&self) -> R<(Version, Arc<Catalog>)> {
        let mut m = lock(&self.meta);
        for _ in 0..5 {
            let v = db::version(&m.conn).map_err(sql_err)?;
            if v.schema == m.catalog.schema_version {
                return Ok((v, m.catalog.clone()));
            }
            m.catalog = Arc::new(db::load_catalog(&m.conn).map_err(sql_err)?);
            info!("schema changed: {} files", m.catalog.entries.len());
        }
        Err(eio("the schema keeps changing"))
    }

    /// With `conn` inside BEGIN: starts its snapshot and returns the matching
    /// metadata-connection version, with the catalog entry for `ino` as of
    /// that snapshot. If the version is the same before and after the
    /// snapshot starts, no commit happened in between and the snapshot is
    /// exactly that version; otherwise the version is `None`.
    fn snapshot(&self, conn: &Connection, ino: u64) -> R<(Option<Version>, Arc<Entry>)> {
        let (v1, _) = self.current()?;
        let schema = db::schema_version(conn).map_err(sql_err)?;
        let (v2, cat) = self.current()?;
        if cat.schema_version != schema {
            return Err(eio("the schema changed during the operation"));
        }
        let entry = cat.by_ino(ino).cloned().ok_or(Errno::ENOENT)?;
        Ok(((v1 == v2).then_some(v1), entry))
    }

    /// Stats for `ino` at version `snap`: the cached ones if they match,
    /// otherwise `compute`'s, which are cached when the version is known.
    /// The slot stays locked while computing, so concurrent requests for the
    /// same table wait for one computation instead of repeating it.
    fn stats_at(
        &self,
        ino: u64,
        snap: Option<Version>,
        compute: impl FnOnce() -> R<Stats>,
    ) -> R<Arc<Stats>> {
        let Some(version) = snap else {
            return compute().map(Arc::new);
        };
        let slot = self.slot(ino);
        let mut cached = lock(&slot);
        if let Some(c) = &*cached
            && c.version == version
        {
            return Ok(c.stats.clone());
        }
        let stats = Arc::new(compute()?);
        *cached = Some(Cached {
            version,
            stats: stats.clone(),
        });
        Ok(stats)
    }

    fn slot(&self, ino: u64) -> Slot {
        lock(&self.slots).entry(ino).or_default().clone()
    }

    fn mtime(&self) -> SystemTime {
        let m = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (m(&self.opts.db.path), m(&self.wal)) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) => a,
            (None, b) => b.unwrap_or(SystemTime::UNIX_EPOCH),
        }
    }

    fn attr(&self, ino: u64, kind: FileType, size: u64) -> FileAttr {
        let t = self.mtime();
        let dir = kind == FileType::Directory;
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind,
            perm: if dir { 0o555 } else { 0o444 },
            nlink: if dir { 2 } else { 1 },
            uid: self.opts.uid,
            gid: self.opts.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn take_conn(&self) -> R<Connection> {
        if let Some(c) = lock(&self.pool).pop() {
            return Ok(c);
        }
        db::open(&self.opts.db, Some(HANDLE_CACHE_KIB)).map_err(sql_err)
    }

    fn put_conn(&self, c: Connection) {
        let mut p = lock(&self.pool);
        if p.len() < POOL_MAX {
            p.push(c);
        }
    }

    /// Size (and checkpoints) of a table at the current version, from the
    /// cache or computed with an auxiliary connection.
    fn stats(&self, ino: u64) -> R<Arc<Stats>> {
        let (v, cat) = self.current()?;
        if cat.by_ino(ino).is_none() {
            return Err(Errno::ENOENT);
        }
        if let Some(c) = &*lock(&self.slot(ino))
            && c.version == v
        {
            return Ok(c.stats.clone());
        }
        let conn = self.take_conn()?;
        let res = conn.execute_batch("BEGIN").map_err(sql_err).and_then(|()| {
            let (snap, entry) = self.snapshot(&conn, ino)?;
            self.stats_at(ino, snap, || {
                match entry.keyed_source() {
                    Some(src) => db::compute_stats(&conn, &src),
                    None => db::compute_size(&conn, &entry, self.opts.db.max_view),
                }
                .map_err(sql_err_in(&entry.file))
            })
        });
        let _ = conn.execute_batch("COMMIT");
        self.put_conn(conn);
        res
    }

    /// File attributes for process `pid`. On Linux, `fstat` does not send the
    /// handle with GETATTR, so if that process has the file open on a
    /// snapshot of a different size, it gets its snapshot's size (from its
    /// most recent open).
    fn file_attr(&self, ino: u64, pid: u32) -> R<FileAttr> {
        let mut size = self.stats(ino)?.size;
        let others: Vec<(u64, u32, u64)> = lock(&self.handles)
            .iter()
            .filter(|(_, h)| h.ino == ino && h.size != size)
            .map(|(fh, h)| (*fh, h.owner, h.size))
            .collect();
        if !others.is_empty() {
            let me = tgid(pid);
            if let Some((_, _, s)) = others.into_iter().filter(|o| o.1 == me).max_by_key(|o| o.0) {
                size = s;
            }
        }
        Ok(self.attr(ino, FileType::RegularFile, size))
    }

    fn handle(&self, fh: u64) -> Option<Arc<Handle>> {
        lock(&self.handles).get(&fh).cloned()
    }

    fn do_open(&self, ino: u64, pid: u32) -> R<(u64, FopenFlags)> {
        let conn = db::open(&self.opts.db, Some(HANDLE_CACHE_KIB)).map_err(sql_err)?;
        conn.execute_batch("BEGIN").map_err(sql_err)?;
        let (snap, entry) = self.snapshot(&conn, ino)?;
        let at = sql_err_in(&entry.file);

        let (src, stats, mode) = match entry.keyed_source() {
            Some(src) => {
                let stats =
                    self.stats_at(ino, snap, || db::compute_stats(&conn, &src).map_err(&at))?;
                let wal = db::journal_mode(&conn)
                    .map_err(&at)?
                    .eq_ignore_ascii_case("wal");
                let mode = if wal || self.opts.lock || self.opts.db.immutable {
                    Mode::Snapshot
                } else {
                    let dv = db::data_version(&conn).map_err(&at)?;
                    conn.execute_batch("COMMIT").map_err(&at)?;
                    Mode::Checked(dv)
                };
                (src, stats, mode)
            }
            None => {
                let (src, stats) =
                    db::materialize(&conn, &entry, self.opts.db.max_view).map_err(&at)?;
                conn.execute_batch("COMMIT").map_err(&at)?;
                // Others only use the size and row count of a view.
                let stats = Arc::new(stats);
                self.stats_at(ino, snap, || {
                    Ok(Stats {
                        size: stats.size,
                        rows: stats.rows,
                        checkpoints: Vec::new(),
                    })
                })?;
                (src, stats, Mode::Detached)
            }
        };

        let fh = self.next_id.fetch_add(1, Ordering::Relaxed);
        let vkey = snap.map(VKey::At).unwrap_or(VKey::Unique(fh));

        // The kernel has a single page cache per inode. Keep it if it holds
        // this same version; otherwise drop it. If handles of another
        // version are open and using it, this one reads uncached so bytes
        // from two versions never mix.
        let flags = {
            let mut opens = lock(&self.opens);
            let st = opens.entry(ino).or_default();
            let others = st.open.iter().any(|(k, n)| *k != vkey && *n > 0);
            let flags = if st.owner == Some(vkey) {
                FopenFlags::FOPEN_KEEP_CACHE
            } else if !others {
                st.owner = Some(vkey);
                FopenFlags::empty()
            } else {
                FopenFlags::FOPEN_DIRECT_IO
            };
            *st.open.entry(vkey).or_default() += 1;
            flags
        };

        let h = Handle {
            ino,
            vkey,
            size: stats.size,
            owner: tgid(pid),
            state: Mutex::new(HandleState {
                file: entry.file.clone(),
                conn,
                src,
                stats,
                mode,
                buf: Vec::new(),
                buf_start: 0,
                cursor: None,
                done: false,
            }),
        };
        lock(&self.handles).insert(fh, Arc::new(h));
        Ok((fh, flags))
    }

    fn fetch(h: &mut HandleState, max_len: usize) -> R<db::Chunk> {
        let check = match h.mode {
            Mode::Checked(dv) => Some(dv),
            _ => None,
        };
        if let Some(dv) = check {
            h.conn.execute_batch("BEGIN").map_err(sql_err)?;
            match db::data_version(&h.conn) {
                Ok(cur) if cur == dv => {}
                Ok(_) => {
                    let _ = h.conn.execute_batch("ROLLBACK");
                    warn!(
                        "{}: the database changed during a read (rollback mode without -o lock)",
                        h.file
                    );
                    return Err(Errno::EIO);
                }
                Err(e) => {
                    let _ = h.conn.execute_batch("ROLLBACK");
                    return Err(sql_err_in(&h.file)(e));
                }
            }
        }
        let r = db::fetch_chunk(&h.conn, &h.src, h.cursor.as_deref(), &mut h.buf, max_len);
        if check.is_some() {
            let _ = h.conn.execute_batch("COMMIT");
        }
        r.map_err(sql_err_in(&h.file))
    }

    fn trim(h: &mut HandleState, keep_from: u64) {
        if keep_from > h.buf_start {
            let d = ((keep_from - h.buf_start) as usize).min(h.buf.len());
            h.buf.drain(..d);
            h.buf_start += d as u64;
        }
    }

    /// Fills the buffer with the requested range and returns its bounds in it.
    fn do_read(h: &mut HandleState, offset: u64, size: u32) -> R<(usize, usize)> {
        let total = h.stats.size;
        if offset >= total {
            return Ok((0, 0));
        }
        let want_end = (offset + size as u64).min(total);
        let buf_end = h.buf_start + h.buf.len() as u64;

        if offset < h.buf_start || offset > buf_end + db::CHECKPOINT_EVERY {
            let cps = &h.stats.checkpoints;
            let i = cps.partition_point(|(o, _)| *o <= offset);
            let (cp_off, cp_key) = if i == 0 {
                (0, None)
            } else {
                (cps[i - 1].0, Some(cps[i - 1].1.clone()))
            };
            // Keep streaming from the cursor if it is closer.
            if !(offset >= buf_end && buf_end >= cp_off) {
                h.buf.clear();
                h.buf_start = cp_off;
                h.cursor = cp_key;
                h.done = false;
            }
        }

        let keep_from = offset.saturating_sub(BACK_WINDOW);
        while h.buf_start + (h.buf.len() as u64) < want_end {
            if h.done {
                return Err(eio(format!(
                    "{}: content ended before the computed size",
                    h.file
                )));
            }
            let max_len = (want_end - h.buf_start) as usize + READ_AHEAD;
            let chunk = Self::fetch(h, max_len)?;
            if chunk.last_key.is_some() {
                h.cursor = chunk.last_key;
            }
            h.done = chunk.done || chunk.rows == 0;
            Self::trim(h, keep_from);
        }
        Self::trim(h, keep_from);
        Ok((
            (offset - h.buf_start) as usize,
            (want_end - h.buf_start) as usize,
        ))
    }

    fn xattr_names(&self, ino: u64) -> R<Vec<&'static str>> {
        if ino == ROOT {
            return Ok(vec!["user.sqlite.journal_mode", "user.sqlite.page_size"]);
        }
        let (_, cat) = self.current()?;
        let e = cat.by_ino(ino).ok_or(Errno::ENOENT)?;
        Ok(e.xattrs
            .iter()
            .map(|(n, _)| *n)
            .chain(["user.sqlite.rows"])
            .collect())
    }

    fn xattr_value(&self, ino: u64, name: &str) -> R<Vec<u8>> {
        if ino == ROOT {
            let m = lock(&self.meta);
            return match name {
                "user.sqlite.journal_mode" => db::journal_mode(&m.conn).map_err(sql_err),
                "user.sqlite.page_size" => db::page_size(&m.conn)
                    .map(|n| n.to_string())
                    .map_err(sql_err),
                _ => Err(Errno::NO_XATTR),
            }
            .map(String::into_bytes);
        }
        let (_, cat) = self.current()?;
        let e = cat.by_ino(ino).ok_or(Errno::ENOENT)?;
        if name == "user.sqlite.rows" {
            return Ok(self.stats(ino)?.rows.to_string().into_bytes());
        }
        e.xattr(name)
            .map(|v| v.as_bytes().to_vec())
            .ok_or(Errno::NO_XATTR)
    }
}

/// Process a thread belongs to (FUSE reports the thread id).
fn tgid(pid: u32) -> u32 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Tgid:").and_then(|v| v.trim().parse().ok()))
        })
        .unwrap_or(pid)
}

fn reply_xattr(reply: ReplyXattr, size: u32, data: R<Vec<u8>>) {
    match data {
        Err(e) => reply.error(e),
        Ok(d) if size == 0 => reply.size(d.len() as u32),
        Ok(d) if d.len() > size as usize => reply.error(Errno::ERANGE),
        Ok(d) => reply.data(&d),
    }
}

impl Filesystem for SqliteFs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let r = match name.to_str() {
            Some(name) if parent.0 == ROOT => self.current().and_then(|(_, cat)| {
                let ino = cat.by_file(name).ok_or(Errno::ENOENT)?.ino;
                self.file_attr(ino, req.pid())
            }),
            _ => Err(Errno::ENOENT),
        };
        debug!(
            "lookup {name:?} -> {}",
            outcome(&r, |a| format!("ino {}, {} bytes", a.ino, a.size))
        );
        match r {
            Ok(attr) => reply.entry_with_ttls(&TTL_FILE, &TTL_ENTRY, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = ino.0;
        let r = if ino == ROOT {
            self.current()
                .map(|_| (TTL_DIR, self.attr(ROOT, FileType::Directory, 0)))
        } else if let Some(h) = fh.and_then(|fh| self.handle(fh.0))
            && h.ino == ino
        {
            // GETATTR on an open handle: that snapshot's size.
            Ok((TTL_FILE, self.attr(ino, FileType::RegularFile, h.size)))
        } else {
            self.file_attr(ino, req.pid()).map(|a| (TTL_FILE, a))
        };
        debug!(
            "getattr ino {ino} fh {:?} -> {}",
            fh.map(|f| f.0),
            outcome(&r, |(_, a)| format!("{} bytes", a.size))
        );
        match r {
            Ok((ttl, a)) => reply.attr(&ttl, &a),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = if ino.0 == ROOT {
            Err(Errno::EISDIR)
        } else if flags.acc_mode() != OpenAccMode::O_RDONLY {
            Err(Errno::EROFS)
        } else {
            self.do_open(ino.0, req.pid())
        };
        debug!(
            "open ino {} -> {}",
            ino.0,
            outcome(&r, |(fh, f)| format!("fh {fh} {f:?}"))
        );
        match r {
            Ok((fh, f)) => reply.opened(FileHandle(fh), f),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(h) = self.handle(fh.0) else {
            return reply.error(Errno::EBADF);
        };
        let mut st = lock(&h.state);
        let r = Self::do_read(&mut st, offset, size);
        debug!(
            "read fh {} at {offset}, {size} bytes -> {}",
            fh.0,
            outcome(&r, |(s, e)| format!("{} bytes", e - s))
        );
        match r {
            Ok((s, e)) => reply.data(&st.buf[s..e]),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Called on every close(); nothing is ever written, so nothing to do.
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        debug!("release fh {}", fh.0);
        if let Some(h) = lock(&self.handles).remove(&fh.0) {
            let mut opens = lock(&self.opens);
            if let Some(st) = opens.get_mut(&h.ino)
                && let Some(n) = st.open.get_mut(&h.vkey)
            {
                *n -= 1;
                if *n == 0 {
                    st.open.remove(&h.vkey);
                }
            }
            // The connection (and its transaction) closes when the handle drops.
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if ino.0 != ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let r = if ino.0 == ROOT {
            self.current().map(|(_, cat)| cat)
        } else {
            Err(Errno::ENOTDIR)
        };
        debug!(
            "readdir from {offset} -> {}",
            outcome(&r, |c| format!("{} files", c.entries.len()))
        );
        let cat = match r {
            Ok(cat) => cat,
            Err(e) => return reply.error(e),
        };
        let dots = [
            (ROOT, FileType::Directory, "."),
            (ROOT, FileType::Directory, ".."),
        ];
        let files = cat
            .entries
            .iter()
            .map(|e| (e.ino, FileType::RegularFile, e.file.as_str()));
        for (i, (ino, kind, name)) in dots
            .into_iter()
            .chain(files)
            .enumerate()
            .skip(offset as usize)
        {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let size = std::fs::metadata(&self.opts.db.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let files = self
            .current()
            .map(|(_, c)| c.entries.len() as u64)
            .unwrap_or(0);
        reply.statfs(size.div_ceil(4096), 0, 0, files + 1, 0, 4096, 255, 4096);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let v = match name.to_str() {
            Some(n) => self.xattr_value(ino.0, n),
            None => Err(Errno::NO_XATTR),
        };
        debug!(
            "getxattr ino {} {name:?} -> {}",
            ino.0,
            outcome(&v, |d| format!("{} bytes", d.len()))
        );
        reply_xattr(reply, size, v);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let v = self.xattr_names(ino.0).map(|names| {
            let mut out = Vec::new();
            for n in names {
                out.extend_from_slice(n.as_bytes());
                out.push(0);
            }
            out
        });
        debug!(
            "listxattr ino {} -> {}",
            ino.0,
            outcome(&v, |d| format!("{} bytes", d.len()))
        );
        reply_xattr(reply, size, v);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        if mask.contains(fuser::AccessFlags::W_OK) {
            reply.error(Errno::EROFS)
        } else {
            reply.ok()
        }
    }
}
