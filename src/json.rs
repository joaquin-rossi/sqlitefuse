//! JSONL encoding of SQLite rows.
//!
//! Every line sqlitefuse exposes is produced by `jsonl_row(...)`, an SQL
//! function registered on each connection. Size computation and content
//! reads both use it, so they cannot drift apart.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use std::fmt::Write as _;

/// Writes `s` as a JSON string: only `"`, `\` and control characters are
/// escaped; everything else (all non-ASCII included) goes through as UTF-8.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc: &str = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0x00..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if esc.is_empty() {
            let _ = write!(out, "\\u{:04x}", b);
        } else {
            out.push_str(esc);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// Writes a JSON string, or `null`.
pub fn write_opt(out: &mut String, s: Option<&str>) {
    match s {
        Some(s) => write_str(out, s),
        None => out.push_str("null"),
    }
}

/// Writes a JSON array, with `item` writing each element.
pub fn write_list<T>(
    out: &mut String,
    items: impl IntoIterator<Item = T>,
    mut item: impl FnMut(&mut String, T),
) {
    out.push('[');
    for (i, x) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        item(out, x);
    }
    out.push(']');
}

fn write_base64(out: &mut String, bytes: &[u8]) {
    out.push_str("{\"$base64\":\"");
    B64.encode_string(bytes, out);
    out.push_str("\"}");
}

/// Writes one SQLite value as JSON (see the type mapping in the README).
pub fn write_value(out: &mut String, v: ValueRef<'_>) {
    match v {
        ValueRef::Null => out.push_str("null"),
        ValueRef::Integer(i) => {
            let _ = write!(out, "{i}");
        }
        ValueRef::Real(f) => {
            if f.is_finite() {
                // Ryu: the shortest representation that reads back to the
                // same value. It always has a '.' or an 'e', so a REAL is
                // never mistaken for an INTEGER.
                out.push_str(ryu::Buffer::new().format_finite(f));
            } else if f.is_nan() {
                // SQLite stores NaN as NULL, but a view expression could
                // still produce one.
                out.push_str("{\"$float\":\"nan\"}");
            } else if f > 0.0 {
                out.push_str("{\"$float\":\"inf\"}");
            } else {
                out.push_str("{\"$float\":\"-inf\"}");
            }
        }
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => write_str(out, s),
            Err(_) => write_base64(out, bytes),
        },
        ValueRef::Blob(bytes) => write_base64(out, bytes),
    }
}

/// Encodes column names as the first argument of `jsonl_row`: the already
/// escaped JSON keys, back to back (`"id""name"`). Returns an SQL literal.
pub fn keys_literal(names: &[String]) -> String {
    let mut s = String::new();
    for n in names {
        write_str(&mut s, n);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// Splits the keys literal into individual JSON keys (quotes included).
fn parse_keys(s: &str) -> Result<Vec<String>, String> {
    let b = s.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            return Err("jsonl_row: malformed keys".into());
        }
        let start = i;
        i += 1;
        loop {
            match b.get(i) {
                None => return Err("jsonl_row: unterminated key".into()),
                Some(b'\\') => i += 2,
                Some(b'"') => {
                    i += 1;
                    break;
                }
                Some(_) => i += 1,
            }
        }
        keys.push(s[start..i].to_string());
    }
    Ok(keys)
}

/// Registers `jsonl_row(keys, v1, v2, ...)` on the connection.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "jsonl_row",
        -1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            if ctx.is_empty() {
                return Err(rusqlite::Error::UserFunctionError(
                    "jsonl_row: missing keys argument".into(),
                ));
            }
            let keys =
                ctx.get_or_create_aux(0, |v: ValueRef<'_>| -> Result<Vec<String>, String> {
                    let s = v.as_str().map_err(|e| e.to_string())?;
                    parse_keys(s)
                })?;
            if keys.len() != ctx.len() - 1 {
                return Err(rusqlite::Error::UserFunctionError(
                    "jsonl_row: number of values does not match number of keys".into(),
                ));
            }
            let mut out = String::with_capacity(16 * ctx.len());
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(k);
                out.push(':');
                write_value(&mut out, ctx.get_raw(i + 1));
            }
            out.push('}');
            Ok(out)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: ValueRef<'_>) -> String {
        let mut s = String::new();
        write_value(&mut s, x);
        s
    }

    #[test]
    fn values() {
        assert_eq!(v(ValueRef::Null), "null");
        assert_eq!(v(ValueRef::Integer(i64::MAX)), "9223372036854775807");
        assert_eq!(v(ValueRef::Integer(i64::MIN)), "-9223372036854775808");
        assert_eq!(v(ValueRef::Real(120.5)), "120.5");
        assert_eq!(v(ValueRef::Real(1.0)), "1.0");
        assert_eq!(v(ValueRef::Real(0.1)), "0.1");
        assert_eq!(v(ValueRef::Real(1e300)), "1e300");
        assert_eq!(v(ValueRef::Real(-2.5e-8)), "-2.5e-8");
        assert_eq!(v(ValueRef::Real(f64::INFINITY)), r#"{"$float":"inf"}"#);
        assert_eq!(v(ValueRef::Real(f64::NEG_INFINITY)), r#"{"$float":"-inf"}"#);
        assert_eq!(
            v(ValueRef::Text("María \"x\"\n\u{1}".as_bytes())),
            r#""María \"x\"\n\u0001""#
        );
        assert_eq!(v(ValueRef::Text(&[0xff, 0x00])), r#"{"$base64":"/wA="}"#);
        assert_eq!(v(ValueRef::Blob(b"\x89PNG")), r#"{"$base64":"iVBORw=="}"#);
    }

    #[test]
    fn row_function() {
        let c = Connection::open_in_memory().unwrap();
        register(&c).unwrap();
        let keys = keys_literal(&["id".into(), "na'me".into(), "a\"b".into()]);
        let sql = format!("SELECT jsonl_row({keys}, 1, 'José', NULL)");
        let s: String = c.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert_eq!(s, r#"{"id":1,"na'me":"José","a\"b":null}"#);
    }

    use proptest::prelude::*;
    use rusqlite::types::Value;

    fn sqlite_value() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<i64>().prop_map(Value::Integer),
            any::<f64>()
                .prop_filter("SQLite stores NaN as NULL", |f| !f.is_nan())
                .prop_map(Value::Real),
            ".*".prop_map(Value::Text),
            proptest::collection::vec(any::<u8>(), 0..64).prop_map(Value::Blob),
        ]
    }

    fn check(expected: &Value, got: &serde_json::Value) -> Result<(), TestCaseError> {
        use serde_json::json;
        match expected {
            Value::Null => prop_assert!(got.is_null()),
            Value::Integer(i) => prop_assert_eq!(got.as_i64(), Some(*i)),
            Value::Real(f) if f.is_finite() => {
                // Written as a float (never like an integer) that reads back exactly.
                prop_assert!(got.is_f64(), "{} is not a float", got);
                prop_assert_eq!(got.as_f64().map(f64::to_bits), Some(f.to_bits()));
            }
            Value::Real(f) => {
                let name = if *f > 0.0 { "inf" } else { "-inf" };
                prop_assert_eq!(got, &json!({ "$float": name }));
            }
            Value::Text(t) => prop_assert_eq!(got.as_str(), Some(t.as_str())),
            Value::Blob(b) => prop_assert_eq!(got, &json!({ "$base64": B64.encode(b) })),
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn rows_round_trip(
            row in proptest::collection::vec((".*", sqlite_value()), 1..8)
        ) {
            // Distinct keys, in the given order.
            let mut keys: Vec<String> = Vec::new();
            for (k, _) in &row {
                let mut k = k.clone();
                while keys.contains(&k) {
                    k.push('_');
                }
                keys.push(k);
            }
            let c = Connection::open_in_memory().unwrap();
            register(&c).unwrap();
            let sql = format!("SELECT jsonl_row({})", std::iter::repeat_n("?", row.len() + 1).collect::<Vec<_>>().join(", "));
            let mut params: Vec<Value> = vec![Value::Text(keys.iter().map(|k| { let mut s = String::new(); write_str(&mut s, k); s }).collect())];
            params.extend(row.iter().map(|(_, v)| v.clone()));
            let line: String = c.query_row(&sql, rusqlite::params_from_iter(params), |r| r.get(0)).unwrap();

            prop_assert!(!line.contains('\n'));
            let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&line).unwrap();
            prop_assert_eq!(obj.keys().cloned().collect::<Vec<_>>(), keys);
            for ((_, expected), got) in row.iter().zip(obj.values()) {
                check(expected, got)?;
            }
        }
    }
}
