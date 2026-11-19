//! SQLite access: connections, table catalog, sizes and chunked reads.

use crate::json;
use log::warn;
use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, params_from_iter};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often (in bytes) a checkpoint (offset → key) is recorded.
pub const CHECKPOINT_EVERY: u64 = 1 << 20;
/// Rows per chunk when reading content.
pub const CHUNK_ROWS: usize = 1000;
/// SQLITE_MAX_FUNCTION_ARG of the bundled SQLite.
const MAX_FUNCTION_ARGS: usize = 1000;

pub fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Read-only `file:` URI with the path percent-encoded.
fn uri(path: &Path, immutable: bool) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file:");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s.push_str("?mode=ro");
    if immutable {
        s.push_str("&immutable=1");
    }
    s
}

/// How to open the database, and the limits that protect against databases
/// that are malformed or crafted to exhaust memory or disk.
#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub immutable: bool,
    /// Largest TEXT or BLOB value read, and largest JSON line produced.
    pub max_value: u32,
    /// Largest view (in JSON bytes) that is sized or copied on open.
    pub max_view: u64,
}

/// SQLite's compile-time ceiling for SQLITE_LIMIT_LENGTH.
pub const MAX_LENGTH: u32 = 1_000_000_000;

/// Opens a read-only, hardened connection with `jsonl_row` registered.
/// `cache_kib` caps the page cache (used for per-handle connections).
pub fn open(cfg: &Config, cache_kib: Option<u32>) -> rusqlite::Result<Connection> {
    let c = Connection::open_with_flags(
        uri(&cfg.path, cfg.immutable),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // The schema comes from the file and may be hostile: views and triggers
    // may only use functions without side effects, and the schema can't be
    // altered through the connection even by accident.
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // A huge value (or the JSON line built from it) fails with SQLITE_TOOBIG
    // instead of exhausting memory.
    c.set_limit(
        Limit::SQLITE_LIMIT_LENGTH,
        cfg.max_value.min(MAX_LENGTH) as i32,
    )?;
    // Detect malformed pages early instead of misreading them.
    c.execute_batch("PRAGMA cell_size_check = ON")?;
    c.busy_timeout(Duration::from_secs(10))?;
    json::register(&c)?;
    if let Some(k) = cache_kib {
        c.execute_batch(&format!("PRAGMA cache_size=-{k}"))?;
    }
    Ok(c)
}

/// The error a size limit produces, so callers handle it like SQLite's own
/// SQLITE_TOOBIG.
fn too_big(msg: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
        Some(msg),
    )
}

/// Database version as seen by one connection. `data_version` can only be
/// compared against values from the same connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub data: i64,
    pub schema: i64,
}

pub fn data_version(c: &Connection) -> rusqlite::Result<i64> {
    c.prepare_cached("PRAGMA data_version")?
        .query_row([], |r| r.get(0))
}

pub fn schema_version(c: &Connection) -> rusqlite::Result<i64> {
    c.prepare_cached("PRAGMA schema_version")?
        .query_row([], |r| r.get(0))
}

pub fn version(c: &Connection) -> rusqlite::Result<Version> {
    Ok(Version {
        data: data_version(c)?,
        schema: schema_version(c)?,
    })
}

pub fn journal_mode(c: &Connection) -> rusqlite::Result<String> {
    c.prepare_cached("PRAGMA journal_mode")?
        .query_row([], |r| r.get(0))
}

pub fn page_size(c: &Connection) -> rusqlite::Result<i64> {
    c.prepare_cached("PRAGMA page_size")?
        .query_row([], |r| r.get(0))
}

// ------------------------------------------------------------------ catalog

#[derive(Debug)]
pub struct Column {
    /// Real name, used to reference the column in SQL.
    pub name: String,
    /// JSON key (same as the name unless there are duplicates).
    pub key: String,
    pub decl_type: String,
    /// Position in the primary key (1..), or 0 if not part of it.
    pub pk: i64,
    pub generated: bool,
    pub notnull: bool,
    /// Default value as an SQL expression.
    pub default: Option<String>,
    /// Collation (`None` for views).
    pub collation: Option<String>,
    pub autoincrement: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Order {
    /// Rowid table: the expression that names the rowid (`rowid`, `_rowid_`,
    /// `oid`, or the INTEGER PRIMARY KEY column if all three are shadowed).
    Rowid(String),
    /// WITHOUT ROWID table: primary key columns in declaration order.
    Pk(Vec<String>),
    /// Views (and tables whose rowid cannot be named): all columns.
    All,
}

#[derive(Debug)]
pub struct Entry {
    pub ino: u64,
    pub file: String,
    pub columns: Vec<Column>,
    pub order: Order,
    /// Extended attributes, computed when the catalog is loaded (all but
    /// `user.sqlite.rows`, which depends on the data).
    pub xattrs: Vec<(&'static str, String)>,
    /// `jsonl_row('"a""b"', "a", "b")`
    pub line_expr: String,
    /// `main."table"`
    pub from: String,
}

impl Entry {
    /// Source that reads straight from the table, if it has a usable key.
    pub fn keyed_source(&self) -> Option<Source> {
        let keys = match &self.order {
            Order::Rowid(k) => vec![k.clone()],
            Order::Pk(cols) => cols.iter().map(|c| ident(c)).collect(),
            Order::All => return None,
        };
        Some(Source {
            from: self.from.clone(),
            keys,
            line: self.line_expr.clone(),
        })
    }

    pub fn xattr(&self, name: &str) -> Option<&str> {
        self.xattrs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn order_json(order: &Order) -> String {
    match order {
        Order::Rowid(_) => r#"{"by":"rowid"}"#.to_string(),
        Order::Pk(cols) => {
            let mut s = String::from(r#"{"by":"pk","columns":"#);
            json::write_list(&mut s, cols, |s, c| json::write_str(s, c));
            s.push('}');
            s
        }
        Order::All => r#"{"by":"all"}"#.to_string(),
    }
}

fn columns_json(cols: &[Column]) -> String {
    let mut s = String::new();
    json::write_list(&mut s, cols, |s, c| {
        s.push_str("{\"name\":");
        json::write_str(s, &c.key);
        s.push_str(",\"type\":");
        json::write_str(s, &c.decl_type);
        s.push_str(&format!(
            ",\"pk\":{},\"notnull\":{},\"default\":",
            c.pk > 0,
            c.notnull
        ));
        json::write_opt(s, c.default.as_deref());
        s.push_str(",\"collation\":");
        json::write_opt(s, c.collation.as_deref());
        s.push_str(&format!(
            ",\"autoincrement\":{},\"generated\":{}}}",
            c.autoincrement, c.generated
        ));
    });
    s
}

pub struct Catalog {
    pub schema_version: i64,
    /// Sorted by file name.
    pub entries: Vec<Arc<Entry>>,
    by_ino: HashMap<u64, Arc<Entry>>,
    by_file: HashMap<String, Arc<Entry>>,
}

impl Catalog {
    pub fn by_ino(&self, ino: u64) -> Option<&Arc<Entry>> {
        self.by_ino.get(&ino)
    }
    pub fn by_file(&self, file: &str) -> Option<&Arc<Entry>> {
        self.by_file.get(file)
    }
}

/// Escapes a table name for use as a file name.
/// `%` → `%25`, `/` → `%2F`, and a leading `.` → `%2E` (so no user table
/// looks hidden or collides with an internal one). Internal tables get a
/// leading `.` and are therefore hidden.
pub fn file_name(table: &str, hidden: bool) -> String {
    let mut s = String::with_capacity(table.len() + 8);
    if hidden {
        s.push('.');
    }
    for (i, ch) in table.chars().enumerate() {
        match ch {
            '%' => s.push_str("%25"),
            '/' => s.push_str("%2F"),
            '.' if i == 0 => s.push_str("%2E"),
            c => s.push(c),
        }
    }
    s.push_str(".jsonl");
    s
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Loads the catalog. Runs inside a transaction so the table list and
/// `schema_version` come from the same snapshot.
pub fn load_catalog(c: &Connection) -> rusqlite::Result<Catalog> {
    c.execute_batch("BEGIN")?;
    let r = load_catalog_tx(c);
    let _ = c.execute_batch("COMMIT");
    r
}

fn load_catalog_tx(c: &Connection) -> rusqlite::Result<Catalog> {
    let schema_version = schema_version(c)?;

    let mut sqls: HashMap<String, Option<String>> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT name, sql FROM main.sqlite_schema WHERE type IN ('table','view','index')",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            sqls.insert(r.get(0)?, r.get(1)?);
        }
    }

    // Triggers by table name (case-insensitive, like SQLite), sorted by name.
    let mut triggers: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    {
        let mut st = c.prepare("SELECT name, tbl_name, sql FROM main.sqlite_schema WHERE type = 'trigger' ORDER BY name")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let table: String = r.get(1)?;
            triggers
                .entry(table.to_lowercase())
                .or_default()
                .push((r.get(0)?, r.get(2)?));
        }
    }

    let mut list: Vec<(String, String, bool, bool)> = Vec::new();
    {
        let mut st = c.prepare(
            "SELECT name, type, wr, strict FROM pragma_table_list WHERE schema = 'main'",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            list.push((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, i64>(3)? != 0,
            ));
        }
    }

    let mut entries: Vec<Entry> = Vec::new();
    for (name, kind, without_rowid, strict) in list {
        if kind == "virtual" {
            continue;
        }
        let is_view = kind == "view";
        let hidden = kind == "shadow" || name.to_ascii_lowercase().starts_with("sqlite_");
        let file = file_name(&name, hidden);
        if file.len() > 255 {
            warn!("skipping {name:?}: file name is longer than 255 bytes");
            continue;
        }
        let columns = match load_columns(c, &name, is_view) {
            Ok(cols) => cols,
            Err(e) => {
                warn!("skipping {name:?}: {e}");
                continue;
            }
        };
        let visible = columns.iter().filter(|(_, hidden)| !hidden).count();
        if visible == 0 {
            continue;
        }
        if visible + 1 > MAX_FUNCTION_ARGS {
            warn!(
                "skipping {name:?}: more than {} columns",
                MAX_FUNCTION_ARGS - 1
            );
            continue;
        }

        let order = if is_view {
            Order::All
        } else if without_rowid {
            let mut pk: Vec<&Column> = columns
                .iter()
                .map(|(c, _)| c)
                .filter(|c| c.pk > 0)
                .collect();
            pk.sort_by_key(|c| c.pk);
            Order::Pk(pk.iter().map(|c| c.name.clone()).collect())
        } else {
            let shadowed = |alias: &str| {
                columns
                    .iter()
                    .any(|(c, _)| c.name.eq_ignore_ascii_case(alias))
            };
            if let Some(a) = ["rowid", "_rowid_", "oid"]
                .into_iter()
                .find(|a| !shadowed(a))
            {
                Order::Rowid(a.to_string())
            } else {
                let pk: Vec<&Column> = columns
                    .iter()
                    .map(|(c, _)| c)
                    .filter(|c| c.pk > 0)
                    .collect();
                if pk.len() == 1 && pk[0].decl_type.eq_ignore_ascii_case("INTEGER") {
                    Order::Rowid(ident(&pk[0].name))
                } else {
                    Order::All
                }
            }
        };

        let (indexes, foreign_keys) = match load_indexes(c, &name, &sqls)
            .and_then(|i| Ok((i, load_foreign_keys(c, &name)?)))
        {
            Ok(v) => v,
            Err(e) => {
                warn!("skipping {name:?}: {e}");
                continue;
            }
        };
        let triggers = triggers_json(triggers.get(&name.to_lowercase()).map_or(&[][..], |v| v));
        let kind = format!(
            "{{\"type\":\"{kind}\",\"internal\":{hidden},\"strict\":{strict},\"without_rowid\":{without_rowid}}}"
        );

        let mut cols: Vec<Column> = columns
            .into_iter()
            .filter(|(_, h)| !h)
            .map(|(c, _)| c)
            .collect();
        dedupe_keys(&mut cols);
        let keys: Vec<String> = cols.iter().map(|c| c.key.clone()).collect();
        let mut line_expr = format!("jsonl_row({}", json::keys_literal(&keys));
        for col in &cols {
            line_expr.push_str(", ");
            line_expr.push_str(&ident(&col.name));
        }
        line_expr.push(')');

        let mut xattrs = Vec::new();
        if let Some(sql) = sqls.get(&name).cloned().flatten() {
            xattrs.push(("user.sqlite.sql", sql));
        }
        xattrs.extend([
            ("user.sqlite.kind", kind),
            ("user.sqlite.columns", columns_json(&cols)),
            ("user.sqlite.indexes", indexes),
            ("user.sqlite.foreign_keys", foreign_keys),
            ("user.sqlite.triggers", triggers),
            ("user.sqlite.order", order_json(&order)),
        ]);

        entries.push(Entry {
            ino: 0,
            file,
            from: format!("main.{}", ident(&name)),
            columns: cols,
            order,
            xattrs,
            line_expr,
        });
    }

    entries.sort_by(|a, b| a.file.cmp(&b.file));
    let mut used: HashSet<u64> = HashSet::new();
    for e in &mut entries {
        let mut ino = fnv1a(&e.file);
        while ino <= 1 || used.contains(&ino) {
            ino = ino.wrapping_add(1);
        }
        used.insert(ino);
        e.ino = ino;
    }

    let entries: Vec<Arc<Entry>> = entries.into_iter().map(Arc::new).collect();
    let by_ino = entries.iter().map(|e| (e.ino, e.clone())).collect();
    let by_file = entries
        .iter()
        .map(|e| (e.file.clone(), e.clone()))
        .collect();
    Ok(Catalog {
        schema_version,
        entries,
        by_ino,
        by_file,
    })
}

/// Columns of a table or view. The bool tells whether it is a hidden column
/// (of a virtual table), which is not exported.
fn load_columns(
    c: &Connection,
    table: &str,
    is_view: bool,
) -> rusqlite::Result<Vec<(Column, bool)>> {
    let mut st = c.prepare("SELECT name, type, pk, hidden, \"notnull\", dflt_value FROM pragma_table_xinfo(?1, 'main')")?;
    let mut rows = st.query([table])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let name: String = r.get(0)?;
        let hidden: i64 = r.get(3)?;
        // Collation and AUTOINCREMENT are only available for tables.
        let (collation, autoincrement) = if is_view || hidden == 1 {
            (None, false)
        } else {
            match c.column_metadata(Some("main"), table, name.as_str()) {
                Ok((_, coll, _, _, autoinc)) => {
                    (coll.map(|s| s.to_string_lossy().into_owned()), autoinc)
                }
                Err(_) => (None, false),
            }
        };
        out.push((
            Column {
                key: name.clone(),
                name,
                decl_type: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                pk: r.get(2)?,
                generated: hidden == 2 || hidden == 3,
                notnull: r.get::<_, i64>(4)? != 0,
                default: r.get(5)?,
                collation,
                autoincrement,
            },
            hidden == 1,
        ));
    }
    Ok(out)
}

/// Describes a table's foreign keys as a JSON array:
/// `[{"columns":[..],"table":..,"to":[..],"on_update":..,"on_delete":..}]`.
/// In declaration order (SQLite numbers them backwards). A `to` entry is
/// `null` when it refers to the parent's primary key implicitly.
fn load_foreign_keys(c: &Connection, table: &str) -> rusqlite::Result<String> {
    let mut st = c.prepare(
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete
         FROM pragma_foreign_key_list(?1, 'main') ORDER BY id DESC, seq",
    )?;
    let mut rows = st.query([table])?;
    // (id, parent table, from columns, to columns, on update, on delete)
    type Fk = (
        i64,
        String,
        Vec<String>,
        Vec<Option<String>>,
        String,
        String,
    );
    let mut fks: Vec<Fk> = Vec::new();
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if fks.last().is_none_or(|f| f.0 != id) {
            fks.push((id, r.get(1)?, Vec::new(), Vec::new(), r.get(4)?, r.get(5)?));
        }
        let f = fks.last_mut().unwrap();
        f.2.push(r.get(2)?);
        f.3.push(r.get(3)?);
    }

    let mut s = String::new();
    json::write_list(
        &mut s,
        &fks,
        |s, (_, parent, from, to, on_update, on_delete)| {
            s.push_str("{\"columns\":");
            json::write_list(s, from, |s, c| json::write_str(s, c));
            s.push_str(",\"table\":");
            json::write_str(s, parent);
            s.push_str(",\"to\":");
            json::write_list(s, to, |s, c| json::write_opt(s, c.as_deref()));
            s.push_str(",\"on_update\":");
            json::write_str(s, on_update);
            s.push_str(",\"on_delete\":");
            json::write_str(s, on_delete);
            s.push('}');
        },
    );
    Ok(s)
}

/// Describes a table's triggers as a JSON array:
/// `[{"name":..,"timing":..,"event":..,"columns":[..],"sql":..}]`.
/// `columns` lists the columns of `UPDATE OF` triggers and is `null` for the
/// rest. `timing` and `event` are `null` if the SQL can't be parsed.
fn triggers_json(triggers: &[(String, Option<String>)]) -> String {
    let mut s = String::new();
    json::write_list(&mut s, triggers, |s, (name, sql)| {
        let parsed = sql.as_deref().and_then(parse_trigger);
        s.push_str("{\"name\":");
        json::write_str(s, name);
        s.push_str(",\"timing\":");
        json::write_opt(s, parsed.as_ref().map(|p| p.0));
        s.push_str(",\"event\":");
        json::write_opt(s, parsed.as_ref().map(|p| p.1));
        s.push_str(",\"columns\":");
        match parsed.as_ref().and_then(|p| p.2.as_ref()) {
            Some(cols) => json::write_list(s, cols, |s, c| json::write_str(s, c)),
            None => s.push_str("null"),
        }
        s.push_str(",\"sql\":");
        json::write_opt(s, sql.as_deref());
        s.push('}');
    });
    s
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Punct(char),
}

/// Splits SQL into words, quoted identifiers/strings (unquoted) and
/// punctuation, skipping whitespace and comments.
fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if matches!(ch, '"' | '\'' | '`' | '[') {
            let close = if ch == '[' { ']' } else { ch };
            let mut v = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == close {
                    // A doubled quote is an escaped quote (not for [...]).
                    if close != ']' && chars.get(i + 1) == Some(&close) {
                        v.push(close);
                        i += 2;
                        continue;
                    }
                    break;
                }
                v.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Quoted(v));
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else {
            out.push(Tok::Punct(ch));
            i += 1;
        }
    }
    out
}

/// Extracts timing, event and `UPDATE OF` columns from a CREATE TRIGGER.
fn parse_trigger(sql: &str) -> Option<(&'static str, &'static str, Option<Vec<String>>)> {
    let t = tokenize(sql);
    let is =
        |i: usize, kw: &str| matches!(t.get(i), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw));
    let mut i = t
        .iter()
        .position(|x| matches!(x, Tok::Word(w) if w.eq_ignore_ascii_case("TRIGGER")))?
        + 1;
    if is(i, "IF") {
        i += 3; // IF NOT EXISTS
    }
    i += 1; // name
    if t.get(i) == Some(&Tok::Punct('.')) {
        i += 2; // schema.name
    }
    let timing = if is(i, "BEFORE") {
        i += 1;
        "BEFORE"
    } else if is(i, "AFTER") {
        i += 1;
        "AFTER"
    } else if is(i, "INSTEAD") && is(i + 1, "OF") {
        i += 2;
        "INSTEAD OF"
    } else {
        "BEFORE" // SQLite's default
    };
    let event = if is(i, "INSERT") {
        "INSERT"
    } else if is(i, "DELETE") {
        "DELETE"
    } else if is(i, "UPDATE") {
        "UPDATE"
    } else {
        return None;
    };
    i += 1;
    let mut columns = None;
    if event == "UPDATE" && is(i, "OF") {
        i += 1;
        let mut cols = Vec::new();
        while let Some(tok) = t.get(i) {
            match tok {
                Tok::Word(w) if w.eq_ignore_ascii_case("ON") => break,
                Tok::Word(w) | Tok::Quoted(w) => cols.push(w.clone()),
                Tok::Punct(_) => {}
            }
            i += 1;
        }
        columns = Some(cols);
    }
    Some((timing, event, columns))
}

/// Describes a table's indexes as a JSON array, sorted by name:
/// `[{"name":..,"unique":..,"origin":..,"partial":..,"columns":[..],"sql":..}]`.
/// `origin` is `index` (CREATE INDEX), `unique` (UNIQUE constraint) or
/// `primary key`. A column is `null` when the index is on an expression;
/// `sql` has the full definition (`null` for automatic indexes).
fn load_indexes(
    c: &Connection,
    table: &str,
    sqls: &HashMap<String, Option<String>>,
) -> rusqlite::Result<String> {
    let mut list: Vec<(String, bool, String, bool)> = Vec::new();
    {
        let mut st = c.prepare(
            "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1, 'main')",
        )?;
        let mut rows = st.query([table])?;
        while let Some(r) = rows.next()? {
            list.push((
                r.get(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get(2)?,
                r.get::<_, i64>(3)? != 0,
            ));
        }
    }
    list.sort_by(|a, b| a.0.cmp(&b.0));

    let mut xinfo =
        c.prepare("SELECT name FROM pragma_index_xinfo(?1, 'main') WHERE key = 1 ORDER BY seqno")?;
    let mut s = String::new();
    s.push('[');
    for (i, (name, unique, origin, partial)) in list.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let columns: Vec<Option<String>> = xinfo
            .query_map([name], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let origin = match origin.as_str() {
            "u" => "unique",
            "pk" => "primary key",
            _ => "index",
        };
        s.push_str("{\"name\":");
        json::write_str(&mut s, name);
        s.push_str(&format!(
            ",\"unique\":{unique},\"origin\":\"{origin}\",\"partial\":{partial},\"columns\":"
        ));
        json::write_list(&mut s, &columns, |s, c| json::write_opt(s, c.as_deref()));
        s.push_str(",\"sql\":");
        json::write_opt(&mut s, sqls.get(name).and_then(|v| v.as_deref()));
        s.push('}');
    }
    s.push(']');
    Ok(s)
}

/// Guarantees unique JSON keys. SQLite already renames duplicate view
/// columns (`x`, `x:1`), so this almost never changes anything.
fn dedupe_keys(cols: &mut [Column]) {
    // Real names are reserved first, so a suffix never takes the name of a
    // later column.
    let names: HashSet<String> = cols.iter().map(|c| c.key.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    for c in cols.iter_mut() {
        if seen.insert(c.key.clone()) {
            continue;
        }
        let mut n = 2;
        loop {
            let k = format!("{}:{n}", c.name);
            if !names.contains(&k) && seen.insert(k.clone()) {
                c.key = k;
                break;
            }
            n += 1;
        }
    }
}

// ------------------------------------------------------- size and content

/// Where lines come from, and the key they are ordered and resumed by.
#[derive(Debug, Clone)]
pub struct Source {
    pub from: String,
    pub keys: Vec<String>,
    pub line: String,
}

impl Source {
    fn key_list(&self) -> String {
        self.keys.join(", ")
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub size: u64,
    pub rows: u64,
    /// (offset where a row starts, key of the previous row), roughly every
    /// 1 MiB. Offset 0 is implicit.
    pub checkpoints: Vec<(u64, Vec<Value>)>,
}

/// Walks the source in order, summing the byte length of each line and
/// recording checkpoints in the same pass.
pub fn compute_stats(c: &Connection, src: &Source) -> rusqlite::Result<Stats> {
    let keys = src.key_list();
    let sql = format!(
        "SELECT {keys}, length(CAST({} AS BLOB)) + 1 FROM {} ORDER BY {keys}",
        src.line, src.from
    );
    let nk = src.keys.len();
    let mut st = c.prepare(&sql)?;
    let mut rows = st.query([])?;
    let mut stats = Stats::default();
    let mut next_mark = CHECKPOINT_EVERY;
    while let Some(r) = rows.next()? {
        let len: i64 = r.get(nk)?;
        stats.size += len as u64;
        stats.rows += 1;
        if stats.size >= next_mark {
            let key = (0..nk)
                .map(|i| r.get::<_, Value>(i))
                .collect::<rusqlite::Result<_>>()?;
            stats.checkpoints.push((stats.size, key));
            next_mark = (stats.size / CHECKPOINT_EVERY + 1) * CHECKPOINT_EVERY;
        }
    }
    Ok(stats)
}

/// Size and row count without checkpoints, for views (their content is
/// materialized on open instead). Stops with SQLITE_TOOBIG once the size
/// passes `max` bytes, so a huge or endless view (a recursive CTE, say)
/// can't keep a request busy forever.
pub fn compute_size(c: &Connection, entry: &Entry, max: u64) -> rusqlite::Result<Stats> {
    let sql = format!(
        "SELECT length(CAST({} AS BLOB)) + 1 FROM {}",
        entry.line_expr, entry.from
    );
    let mut st = c.prepare(&sql)?;
    let mut rows = st.query([])?;
    let mut stats = Stats::default();
    while let Some(r) = rows.next()? {
        stats.size += r.get::<_, i64>(0)? as u64;
        stats.rows += 1;
        if stats.size > max {
            return Err(too_big(format!(
                "{} is larger than max_view ({max} bytes)",
                entry.from
            )));
        }
    }
    Ok(stats)
}

const SNAP: &str = "temp.\"sqlitefuse_snapshot\"";

/// Copies the content of a view (or a table without a usable key) into a
/// temporary table on the connection, in the stable order: all columns left
/// to right and, as the final tie-breaker, the line itself (two rows that
/// tie on all of that produce exactly the same bytes). Fails with
/// SQLITE_TOOBIG if the content is larger than `max` bytes.
pub fn materialize(c: &Connection, entry: &Entry, max: u64) -> rusqlite::Result<(Source, Stats)> {
    // Measured first, streaming: with ORDER BY, SQLite sorts every row
    // before inserting any, so an endless view would never reach the cap on
    // the temporary database below.
    compute_size(c, entry, max)?;
    let mut order: Vec<String> = entry.columns.iter().map(|col| ident(&col.name)).collect();
    order.push("1".into());
    c.execute_batch("DROP TABLE IF EXISTS temp.\"sqlitefuse_snapshot\"")?;
    let page = c
        .query_row("PRAGMA temp.page_size", [], |r| r.get::<_, i64>(0))?
        .max(512) as u64;
    // In case a view with random() comes out bigger the second time; with
    // room for SQLite's overhead on top of the JSON bytes.
    let pages = max.saturating_mul(2) / page + 1024;
    c.execute_batch(&format!(
        "PRAGMA temp.max_page_count = {pages};
         CREATE TEMP TABLE \"sqlitefuse_snapshot\"(line TEXT);
         INSERT INTO {SNAP}(line) SELECT {} FROM {} ORDER BY {};",
        entry.line_expr,
        entry.from,
        order.join(", ")
    ))?;
    let src = Source {
        from: SNAP.into(),
        keys: vec!["rowid".into()],
        line: "line".into(),
    };
    let stats = compute_stats(c, &src)?;
    Ok((src, stats))
}

pub struct Chunk {
    pub rows: usize,
    pub last_key: Option<Vec<Value>>,
    pub done: bool,
}

/// Appends to `out` the lines that follow `after` (keyset pagination, never
/// OFFSET). Stops after `CHUNK_ROWS` rows or once `out` exceeds `max_len`
/// bytes.
pub fn fetch_chunk(
    c: &Connection,
    src: &Source,
    after: Option<&[Value]>,
    out: &mut Vec<u8>,
    max_len: usize,
) -> rusqlite::Result<Chunk> {
    let keys = src.key_list();
    let nk = src.keys.len();
    let sql = match after {
        None => format!(
            "SELECT {keys}, {} FROM {} ORDER BY {keys} LIMIT {CHUNK_ROWS}",
            src.line, src.from
        ),
        Some(_) => {
            let qs = vec!["?"; nk].join(", ");
            let (l, r) = if nk > 1 {
                (format!("({keys})"), format!("({qs})"))
            } else {
                (keys.clone(), qs)
            };
            format!(
                "SELECT {keys}, {} FROM {} WHERE {l} > {r} ORDER BY {keys} LIMIT {CHUNK_ROWS}",
                src.line, src.from
            )
        }
    };
    let mut st = c.prepare_cached(&sql)?;
    let mut rows = match after {
        None => st.query([])?,
        Some(k) => st.query(params_from_iter(k.iter()))?,
    };
    let mut n = 0;
    let mut last = None;
    let mut cut = false;
    while let Some(r) = rows.next()? {
        out.extend_from_slice(r.get_ref(nk)?.as_bytes()?);
        out.push(b'\n');
        n += 1;
        if out.len() >= max_len || n == CHUNK_ROWS {
            last = Some(
                (0..nk)
                    .map(|i| r.get::<_, Value>(i))
                    .collect::<rusqlite::Result<_>>()?,
            );
            cut = out.len() >= max_len && n < CHUNK_ROWS;
            break;
        }
    }
    // If the loop ended on its own there are no rows left and the key is
    // not needed.
    Ok(Chunk {
        rows: n,
        last_key: last,
        done: n < CHUNK_ROWS && !cut,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A database file built from `sql`, opened read-only like sqlitefuse does.
    struct TestDb {
        dir: PathBuf,
        cfg: Config,
    }

    impl TestDb {
        fn new(sql: &str) -> TestDb {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "sqlitefuse-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("test.db");
            Connection::open(&path).unwrap().execute_batch(sql).unwrap();
            let cfg = Config {
                path,
                immutable: false,
                max_value: 1 << 20,
                max_view: 1 << 30,
            };
            TestDb { dir, cfg }
        }

        fn open(&self) -> Connection {
            open(&self.cfg, None).unwrap()
        }

        fn catalog(&self) -> Catalog {
            load_catalog(&self.open()).unwrap()
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn entry<'a>(cat: &'a Catalog, file: &str) -> &'a Entry {
        cat.by_file(file)
            .unwrap_or_else(|| panic!("{file} not in catalog"))
    }

    /// All lines of a source, fetched chunk by chunk from `after`.
    fn read_all(c: &Connection, src: &Source, after: Option<Vec<Value>>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cursor = after;
        loop {
            let chunk = fetch_chunk(c, src, cursor.as_deref(), &mut out, usize::MAX).unwrap();
            if chunk.done {
                return out;
            }
            cursor = chunk.last_key;
        }
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("users", false), "users.jsonl");
        assert_eq!(file_name("a/b%c", false), "a%2Fb%25c.jsonl");
        assert_eq!(file_name(".dot", false), "%2Edot.jsonl");
        assert_eq!(file_name("a.b", false), "a.b.jsonl");
        assert_eq!(file_name("sqlite_stat1", true), ".sqlite_stat1.jsonl");
        assert_eq!(file_name(".x", true), ".%2Ex.jsonl");
    }

    proptest! {
        #[test]
        fn file_names_are_unique(a in ".{0,12}", b in ".{0,12}", ha: bool, hb: bool) {
            prop_assume!((a.as_str(), ha) != (b.as_str(), hb));
            prop_assert_ne!(file_name(&a, ha), file_name(&b, hb));
        }

        #[test]
        fn tokenizer_never_panics(sql in "\\PC{0,80}") {
            let _ = parse_trigger(&sql);
        }
    }

    #[test]
    fn catalog_contents() {
        let db = TestDb::new(
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, x);
             CREATE TABLE w(a, b, PRIMARY KEY (b, a)) WITHOUT ROWID;
             CREATE TABLE shadowed(rowid, oid, _rowid_);
             CREATE TABLE \".hidden?\"(x);
             CREATE VIEW v AS SELECT x FROM t;
             CREATE VIRTUAL TABLE f USING fts5(body);
             INSERT INTO t(x) VALUES (1);",
        );
        let cat = db.catalog();
        let files: Vec<&str> = cat.entries.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(
            files,
            [
                "%2Ehidden?.jsonl",
                ".f_config.jsonl",
                ".f_content.jsonl",
                ".f_data.jsonl",
                ".f_docsize.jsonl",
                ".f_idx.jsonl",
                ".sqlite_schema.jsonl",
                ".sqlite_sequence.jsonl",
                "shadowed.jsonl",
                "t.jsonl",
                "v.jsonl",
                "w.jsonl",
            ]
        );
        assert_eq!(entry(&cat, "t.jsonl").order, Order::Rowid("rowid".into()));
        assert_eq!(
            entry(&cat, "w.jsonl").order,
            Order::Pk(vec!["b".into(), "a".into()])
        );
        assert_eq!(entry(&cat, "shadowed.jsonl").order, Order::All);
        assert_eq!(entry(&cat, "v.jsonl").order, Order::All);
        assert!(entry(&cat, "v.jsonl").keyed_source().is_none());
        assert!(
            entry(&cat, ".f_content.jsonl")
                .xattr("user.sqlite.kind")
                .unwrap()
                .contains(r#""type":"shadow","internal":true"#)
        );

        // Inode numbers are stable across loads.
        let again = db.catalog();
        for e in &cat.entries {
            assert_eq!(again.by_file(&e.file).unwrap().ino, e.ino);
        }
    }

    #[test]
    fn stats_and_resuming() {
        // Enough rows for a few checkpoints, with a composite text key.
        let db = TestDb::new(
            "CREATE TABLE w(k TEXT, n INT, v, PRIMARY KEY (k, n)) WITHOUT ROWID;
             WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 30000)
             INSERT INTO w SELECT 'k' || (i % 7), i, printf('%.*c', i % 200, 'x') FROM s;",
        );
        let c = db.open();
        let cat = load_catalog(&c).unwrap();
        let src = entry(&cat, "w.jsonl").keyed_source().unwrap();
        let stats = compute_stats(&c, &src).unwrap();
        let all = read_all(&c, &src, None);

        assert_eq!(stats.rows, 30000);
        assert_eq!(stats.size, all.len() as u64);
        assert!(
            stats.checkpoints.len() >= 3,
            "{} checkpoints",
            stats.checkpoints.len()
        );
        for (offset, key) in &stats.checkpoints {
            assert_eq!(
                all[*offset as usize - 1],
                b'\n',
                "checkpoint not at a row boundary"
            );
            assert_eq!(
                read_all(&c, &src, Some(key.clone())),
                all[*offset as usize..]
            );
        }

        // Same bytes when stopping early because of max_len.
        let mut out = Vec::new();
        let chunk = fetch_chunk(&c, &src, None, &mut out, 10).unwrap();
        assert_eq!(chunk.rows, 1);
        assert!(!chunk.done);
        assert_eq!(read_all(&c, &src, chunk.last_key), all[out.len()..]);
    }

    #[test]
    fn views_are_ordered_and_sized() {
        // 1 and 1.0 tie in SQL but not in JSON; the line breaks the tie.
        let db = TestDb::new(
            "CREATE TABLE t(x);
             INSERT INTO t VALUES (2), (1.0), ('a'), (1), (NULL);
             CREATE VIEW v AS SELECT x FROM t;",
        );
        let c = db.open();
        let cat = load_catalog(&c).unwrap();
        let v = entry(&cat, "v.jsonl");
        let (src, stats) = materialize(&c, v, 1 << 20).unwrap();
        let all = read_all(&c, &src, None);
        assert_eq!(stats.size, all.len() as u64);
        assert_eq!(
            String::from_utf8(all.clone()).unwrap(),
            "{\"x\":null}\n{\"x\":1.0}\n{\"x\":1}\n{\"x\":2}\n{\"x\":\"a\"}\n"
        );
        let size = compute_size(&c, v, 1 << 20).unwrap();
        assert_eq!((size.size, size.rows), (all.len() as u64, 5));
    }

    #[test]
    fn size_limits() {
        let db = TestDb::new(
            "CREATE TABLE big(b); INSERT INTO big VALUES (randomblob(2000000));
             CREATE VIEW endless AS
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT i FROM n;",
        );
        let c = db.open();
        let cat = load_catalog(&c).unwrap();

        let big = entry(&cat, "big.jsonl").keyed_source().unwrap();
        let err = compute_stats(&c, &big).unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::TooBig));

        let endless = entry(&cat, "endless.jsonl");
        let err = compute_size(&c, endless, 1 << 16).unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::TooBig));
        let err = materialize(&c, endless, 1 << 16).unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::TooBig));
    }

    #[test]
    fn untrusted_schema() {
        // A view can't call a function marked as having side effects, even
        // a harmless one registered by the application.
        let db = TestDb::new("CREATE TABLE t(x); CREATE VIEW v AS SELECT side_effect(x) FROM t;");
        let c = db.open();
        c.create_scalar_function(
            "side_effect",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| Ok(1),
        )
        .unwrap();
        let err = c.query_row("SELECT * FROM v", [], |_| Ok(())).unwrap_err();
        assert!(
            err.to_string().contains("unsafe use of side_effect"),
            "{err}"
        );
        // The connection is read-only.
        assert!(c.execute_batch("CREATE TABLE z(y)").is_err());
    }

    #[test]
    fn metadata_xattrs() {
        let db = TestDb::new(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE NOT NULL DEFAULT 'x');
             CREATE TABLE c(id INTEGER PRIMARY KEY AUTOINCREMENT, p_id INT REFERENCES p ON DELETE CASCADE,
                            q INT, r INT, FOREIGN KEY (q, r) REFERENCES p(id, name)) STRICT;
             CREATE UNIQUE INDEX c_q ON c(q) WHERE q > 0;
             CREATE INDEX c_expr ON c(q + r);
             CREATE TRIGGER c_ins AFTER INSERT ON c BEGIN SELECT 1; END;
             CREATE VIEW v AS SELECT name FROM p;",
        );
        let cat = db.catalog();
        let p = entry(&cat, "p.jsonl");
        let c = entry(&cat, "c.jsonl");
        let v = entry(&cat, "v.jsonl");

        assert_eq!(
            c.xattr("user.sqlite.kind").unwrap(),
            r#"{"type":"table","internal":false,"strict":true,"without_rowid":false}"#
        );
        assert_eq!(
            v.xattr("user.sqlite.kind").unwrap(),
            r#"{"type":"view","internal":false,"strict":false,"without_rowid":false}"#
        );
        assert!(p.xattr("user.sqlite.columns").unwrap().contains(
            r#"{"name":"name","type":"TEXT","pk":false,"notnull":true,"default":"'x'","collation":"NOCASE","autoincrement":false,"generated":false}"#
        ));
        assert!(c.xattr("user.sqlite.columns").unwrap().contains(r#""name":"id","type":"INTEGER","pk":true,"notnull":false,"default":null,"collation":"BINARY","autoincrement":true"#));
        assert!(
            v.xattr("user.sqlite.columns")
                .unwrap()
                .contains(r#""collation":null"#)
        );
        assert_eq!(
            c.xattr("user.sqlite.foreign_keys").unwrap(),
            r#"[{"columns":["p_id"],"table":"p","to":[null],"on_update":"NO ACTION","on_delete":"CASCADE"},{"columns":["q","r"],"table":"p","to":["id","name"],"on_update":"NO ACTION","on_delete":"NO ACTION"}]"#
        );
        assert_eq!(
            c.xattr("user.sqlite.indexes").unwrap(),
            r#"[{"name":"c_expr","unique":false,"origin":"index","partial":false,"columns":[null],"sql":"CREATE INDEX c_expr ON c(q + r)"},{"name":"c_q","unique":true,"origin":"index","partial":true,"columns":["q"],"sql":"CREATE UNIQUE INDEX c_q ON c(q) WHERE q > 0"}]"#
        );
        assert_eq!(
            c.xattr("user.sqlite.triggers").unwrap(),
            r#"[{"name":"c_ins","timing":"AFTER","event":"INSERT","columns":null,"sql":"CREATE TRIGGER c_ins AFTER INSERT ON c BEGIN SELECT 1; END"}]"#
        );
        for name in [
            "user.sqlite.indexes",
            "user.sqlite.foreign_keys",
            "user.sqlite.triggers",
        ] {
            assert_eq!(v.xattr(name), Some("[]"), "{name}");
        }
        assert_eq!(
            v.xattr("user.sqlite.sql"),
            Some("CREATE VIEW v AS SELECT name FROM p")
        );
        assert_eq!(c.xattr("user.sqlite.order").unwrap(), r#"{"by":"rowid"}"#);
    }

    #[test]
    fn dedupe() {
        let col = |n: &str| Column {
            name: n.into(),
            key: n.into(),
            decl_type: String::new(),
            pk: 0,
            generated: false,
            notnull: false,
            default: None,
            collation: None,
            autoincrement: false,
        };
        let mut cols = vec![col("x"), col("x"), col("x:2"), col("x")];
        dedupe_keys(&mut cols);
        let keys: Vec<&str> = cols.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["x", "x:3", "x:2", "x:4"]);
    }

    #[test]
    fn trigger_parsing() {
        assert_eq!(
            parse_trigger("CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; END"),
            Some(("AFTER", "INSERT", None))
        );
        assert_eq!(
            parse_trigger("create trigger if not exists main.t delete on x begin select 1; end"),
            Some(("BEFORE", "DELETE", None))
        );
        assert_eq!(
            parse_trigger("CREATE TRIGGER \"after\" INSTEAD OF UPDATE ON v BEGIN SELECT 1; END"),
            Some(("INSTEAD OF", "UPDATE", None))
        );
        assert_eq!(
            parse_trigger(
                "CREATE TRIGGER t /* c */ BEFORE UPDATE OF a, \"b c\", [d] ON x BEGIN SELECT 1; END"
            ),
            Some((
                "BEFORE",
                "UPDATE",
                Some(vec!["a".into(), "b c".into(), "d".into()])
            ))
        );
        assert_eq!(parse_trigger("CREATE TRIGGER"), None);
    }
}
