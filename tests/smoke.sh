#!/usr/bin/env bash
# End-to-end test: builds a sample database, mounts it and checks behavior.
#
#   tests/smoke.sh                 # uses target/release/sqlitefuse
#   SQLITEFS=path/to/bin tests/smoke.sh
#   KEEP=1 tests/smoke.sh          # leave the temp dir and mount for poking around
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SQLITEFS:-$ROOT/target/release/sqlitefuse}
[[ -x $BIN ]] || { echo "binary not found: $BIN (run: cargo build --release)"; exit 1; }
for t in sqlite3 python3 getfattr fusermount3; do
    command -v "$t" >/dev/null || { echo "missing tool: $t"; exit 1; }
done

DIR=$(mktemp -d)
DB=$DIR/test.db
MNT=$DIR/mnt
mkdir "$MNT"

cleanup() {
    if [[ ${KEEP:-} == 1 ]]; then
        echo "kept: $DIR (mounted at $MNT; unmount with: fusermount3 -u $MNT)"
        return
    fi
    fusermount3 -u "$MNT" 2>/dev/null || true
    rm -rf "$DIR"
}
trap cleanup EXIT

PASS=0
FAIL=0
check() { # check "description" command...
    local desc=$1; shift
    if "$@" >/dev/null 2>&1; then
        echo "  ok   $desc"; PASS=$((PASS + 1))
    else
        echo "  FAIL $desc"; FAIL=$((FAIL + 1))
    fi
}
eq() { [[ "$1" == "$2" ]] || { echo "    expected: $2"; echo "    got:      $1"; return 1; }; }
# xa ATTR FILE: the user.sqlite.ATTR extended attribute of a file in the mount
xa() { getfattr --absolute-names --only-values -n "user.sqlite.$1" "$MNT/$2"; }
wait_mounted() { for _ in $(seq 50); do mountpoint -q "$MNT" && return; sleep 0.1; done; }

echo "== building sample database in $DIR"
sqlite3 "$DB" <<'SQL'
CREATE TABLE users(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, balance REAL, photo BLOB);
INSERT INTO users(name, balance, photo) VALUES ('María', 120.5, NULL), ('José', 0, x'89504E470D0A');
INSERT INTO users(name, balance) VALUES ('inf', 9e999), ('ctl', 'a"b\c' || char(10) || char(1));
INSERT INTO users(name, balance) VALUES (CAST(x'ff00' AS TEXT), 1.0);
CREATE TABLE kv(k TEXT, n INT, v, PRIMARY KEY(n, k)) WITHOUT ROWID;
INSERT INTO kv VALUES ('b', 1, 'x'), ('a', 1, 'y'), ('z', 0, 'w');
CREATE INDEX kv_v ON kv(v) WHERE v IS NOT NULL;
CREATE TABLE child(id INTEGER PRIMARY KEY, kv_n INT, kv_k TEXT,
    note TEXT NOT NULL DEFAULT 'x' COLLATE NOCASE,
    FOREIGN KEY (kv_n, kv_k) REFERENCES kv(n, k) ON DELETE CASCADE) STRICT;
CREATE TRIGGER child_upd AFTER UPDATE OF note ON child BEGIN SELECT 1; END;
CREATE TABLE "a/b%c"(x); INSERT INTO "a/b%c" VALUES (1);
CREATE TABLE ".dot"(x); INSERT INTO ".dot" VALUES (2);
CREATE TABLE g(a INT, b INT GENERATED ALWAYS AS (a * 2) VIRTUAL);
INSERT INTO g(a) VALUES (5);
CREATE VIEW dup AS SELECT a.x, b.x FROM "a/b%c" a, "a/b%c" b;
CREATE VIRTUAL TABLE f USING fts5(body); INSERT INTO f VALUES ('hello world');
CREATE TABLE big(id INTEGER PRIMARY KEY, a TEXT, b REAL, c BLOB);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300000)
INSERT INTO big SELECT i, 'row ' || i, i * 0.37, randomblob(i % 40) FROM n;
SQL

echo "== mounting"
check "mount succeeds" "$BIN" "$DB" "$MNT"
check "mountpoint is mounted" mountpoint -q "$MNT"

echo "== listing"
check "tables and views are listed" \
    eq "$(ls "$MNT" | tr '\n' ' ')" "%2Edot.jsonl a%2Fb%25c.jsonl big.jsonl child.jsonl dup.jsonl g.jsonl kv.jsonl users.jsonl "
check "internal tables are hidden files" test -f "$MNT/.sqlite_sequence.jsonl" -a -f "$MNT/.sqlite_schema.jsonl"
check "FTS shadow tables are hidden files" test -f "$MNT/.f_content.jsonl"
check "virtual table is not listed" test ! -e "$MNT/f.jsonl"
check "permissions are 0444 / 0555" eq "$(stat -c %a "$MNT" "$MNT/users.jsonl" | tr '\n' ' ')" "555 444 "

echo "== content"
check "types are encoded" eq "$(cat "$MNT/users.jsonl")" '{"id":1,"name":"María","balance":120.5,"photo":null}
{"id":2,"name":"José","balance":0.0,"photo":{"$base64":"iVBORw0K"}}
{"id":3,"name":"inf","balance":{"$float":"inf"},"photo":null}
{"id":4,"name":"ctl","balance":"a\"b\\c\n\u0001","photo":null}
{"id":5,"name":{"$base64":"/wA="},"balance":1.0,"photo":null}'
check "WITHOUT ROWID is ordered by PK" eq "$(cat "$MNT/kv.jsonl")" '{"k":"z","n":0,"v":"w"}
{"k":"a","n":1,"v":"y"}
{"k":"b","n":1,"v":"x"}'
check "generated columns are included" eq "$(cat "$MNT/g.jsonl")" '{"a":5,"b":10}'
check "duplicate view columns get a suffix" eq "$(cat "$MNT/dup.jsonl")" '{"x":1,"x:1":1}'
check "escaped names resolve" eq "$(cat "$MNT/a%2Fb%25c.jsonl")" '{"x":1}'

echo "== sizes and xattrs"
for f in users kv g dup big; do
    check "st_size == bytes read ($f)" eq "$(stat -c %s "$MNT/$f.jsonl")" "$(wc -c < "$MNT/$f.jsonl")"
done
check "user.sqlite.rows" eq "$(xa rows big.jsonl)" 300000
check "user.sqlite.order" eq "$(xa order kv.jsonl)" '{"by":"pk","columns":["n","k"]}'
check "user.sqlite.indexes" eq "$(xa indexes kv.jsonl)" \
    '[{"name":"kv_v","unique":false,"origin":"index","partial":true,"columns":["v"],"sql":"CREATE INDEX kv_v ON kv(v) WHERE v IS NOT NULL"},{"name":"sqlite_autoindex_kv_1","unique":true,"origin":"primary key","partial":false,"columns":["n","k"],"sql":null}]'
check "user.sqlite.indexes is empty for views" eq "$(xa indexes dup.jsonl)" '[]'
check "user.sqlite.kind" eq "$(xa kind child.jsonl)" '{"type":"table","internal":false,"strict":true,"without_rowid":false}'
check "user.sqlite.kind (internal)" eq "$(xa kind .sqlite_sequence.jsonl)" '{"type":"table","internal":true,"strict":false,"without_rowid":false}'
check "user.sqlite.foreign_keys" eq "$(xa foreign_keys child.jsonl)" \
    '[{"columns":["kv_n","kv_k"],"table":"kv","to":["n","k"],"on_update":"NO ACTION","on_delete":"CASCADE"}]'
check "user.sqlite.triggers" eq "$(xa triggers child.jsonl)" \
    '[{"name":"child_upd","timing":"AFTER","event":"UPDATE","columns":["note"],"sql":"CREATE TRIGGER child_upd AFTER UPDATE OF note ON child BEGIN SELECT 1; END"}]'
check "user.sqlite.columns details" eq "$(xa columns child.jsonl | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[3], separators=(",", ":")))')" \
    '{"name":"note","type":"TEXT","pk":false,"notnull":true,"default":"'"'x'"'","collation":"NOCASE","autoincrement":false,"generated":false}'
check "user.sqlite.journal_mode on root" eq "$(xa journal_mode .)" delete

echo "== reads"
check "two reads give the same bytes" eq "$(md5sum < "$MNT/big.jsonl")" "$(md5sum < "$MNT/big.jsonl")"
check "every line is valid JSON matching SQLite" python3 - "$DB" "$MNT/big.jsonl" <<'PY'
import json, sqlite3, sys
rows = sqlite3.connect(sys.argv[1]).execute("SELECT id, a, b FROM big ORDER BY id").fetchall()
got = [json.loads(l) for l in open(sys.argv[2], "rb")]
assert [(r["id"], r["a"], r["b"]) for r in got] == rows
PY
check "random offset reads, mmap and tail" python3 - "$MNT/big.jsonl" <<'PY'
import mmap, os, random, subprocess, sys
p = sys.argv[1]
whole = open(p, "rb").read()
fd = os.open(p, os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
for _ in range(200):
    off = random.randrange(len(whole)); n = random.randrange(1, 300000)
    assert os.pread(fd, n, off) == whole[off:off + n]
assert mmap.mmap(fd, 0, prot=mmap.PROT_READ)[:] == whole
assert subprocess.check_output(["tail", "-n", "1", p]) == whole.splitlines(True)[-1]
PY

check "8 concurrent readers at random offsets" python3 - "$MNT/big.jsonl" <<'PY'
import os, random, sys, threading
p = sys.argv[1]
whole = open(p, "rb").read()
errors = []
def reader(seed):
    rng = random.Random(seed)
    fd = os.open(p, os.O_RDONLY)
    try:
        for _ in range(100):
            off = rng.randrange(len(whole)); n = rng.randrange(1, 200000)
            if os.pread(fd, n, off) != whole[off:off + n]:
                errors.append((seed, off, n))
    finally:
        os.close(fd)
threads = [threading.Thread(target=reader, args=(i,)) for i in range(8)]
for t in threads: t.start()
for t in threads: t.join()
assert not errors, errors[:3]
PY

echo "== changes while mounted"
check "new rows are visible right after a commit" python3 - "$DB" "$MNT/g.jsonl" <<'PY'
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1]); p = sys.argv[2]
before = open(p, "rb").read()
c.execute("INSERT INTO g(a) VALUES (7)"); c.commit()
after = open(p, "rb").read()
assert after == before + b'{"a":7,"b":14}\n' and os.stat(p).st_size == len(after)
PY
check "created table appears, dropped table disappears" bash -c "
    sqlite3 '$DB' \"CREATE TABLE fresh(x); INSERT INTO fresh VALUES ('hi')\" &&
    [[ \$(cat '$MNT/fresh.jsonl') == '{\"x\":\"hi\"}' ]] &&
    sqlite3 '$DB' 'DROP TABLE fresh' && [[ ! -e '$MNT/fresh.jsonl' ]]"
check "rollback mode: a commit during a long read gives EIO" python3 - "$DB" "$MNT/big.jsonl" <<'PY'
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
fd = os.open(sys.argv[2], os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.pread(fd, 65536, 1_000_000)
c.execute("INSERT INTO g(a) VALUES (8)"); c.commit()
try:
    os.pread(fd, 65536, 5_000_000)
except OSError as e:
    assert e.errno == 5
else:
    raise AssertionError("read did not fail")
PY

echo "== WAL mode"
fusermount3 -u "$MNT"
sqlite3 "$DB" "PRAGMA journal_mode=wal" >/dev/null
"$BIN" "$DB" "$MNT"
check "an open file keeps its snapshot; a new open sees the change" python3 - "$DB" "$MNT/big.jsonl" <<'PY'
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1]); p = sys.argv[2]
fd = os.open(p, os.O_RDONLY)
size = os.fstat(fd).st_size
first = os.pread(fd, 1 << 20, 0)
c.execute("UPDATE big SET a = 'CHANGED' WHERE id % 2 = 0"); c.commit()
data, off = first, len(first)
while chunk := os.pread(fd, 1 << 20, off):
    data += chunk; off += len(chunk)
assert len(data) == size == os.fstat(fd).st_size and b"CHANGED" not in data
assert b"CHANGED" in open(p, "rb").read()
PY

echo "== lock option"
fusermount3 -u "$MNT"
sqlite3 "$DB" "PRAGMA journal_mode=delete" >/dev/null
"$BIN" "$DB" "$MNT" -o lock
check "-o lock blocks writers while a file is open" python3 - "$DB" "$MNT/g.jsonl" <<'PY'
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1], timeout=0.3)
fd = os.open(sys.argv[2], os.O_RDONLY); os.read(fd, 10)
try:
    c.execute("INSERT INTO g(a) VALUES (9)"); c.commit()
except sqlite3.OperationalError:
    pass
else:
    raise AssertionError("commit went through")
c.rollback(); os.close(fd)
c.execute("INSERT INTO g(a) VALUES (9)"); c.commit()
PY
fusermount3 -u "$MNT"

echo "== odd inputs and limits"
ODD=$DIR/odd.db
sqlite3 "$ODD" <<'SQL'
CREATE TABLE "名前 with spaces"(x); INSERT INTO "名前 with spaces" VALUES ('ok');
CREATE TABLE "it's ""quoted"""(x); INSERT INTO "it's ""quoted""" VALUES (1);
CREATE TABLE blob5m(b); INSERT INTO blob5m VALUES (randomblob(5000000));
CREATE TABLE blob2m(b); INSERT INTO blob2m VALUES (randomblob(2000000));
CREATE VIEW endless AS WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT i FROM n;
SQL
"$BIN" "$ODD" "$MNT" -o max_value=4M,max_view=64K
check "unicode and spaces in table names" eq "$(cat "$MNT/名前 with spaces.jsonl")" '{"x":"ok"}'
check "quotes in table names" eq "$(cat "$MNT/it's \"quoted\".jsonl")" '{"x":1}'
check "a 2 MB blob is read whole" python3 - "$ODD" "$MNT/blob2m.jsonl" <<'PY'
import base64, json, os, sqlite3, sys
blob = sqlite3.connect(sys.argv[1]).execute("SELECT b FROM blob2m").fetchone()[0]
data = open(sys.argv[2], "rb").read()
assert len(data) == os.stat(sys.argv[2]).st_size
assert base64.b64decode(json.loads(data)["b"]["$base64"]) == blob
PY
check "a value above max_value fails with EFBIG" bash -c "cat '$MNT/blob5m.jsonl' 2>&1 >/dev/null | grep -q 'File too large'"
check "an endless view fails with EFBIG (max_view)" bash -c "timeout 10 cat '$MNT/endless.jsonl' 2>&1 >/dev/null | grep -q 'File too large'"
check "the mount keeps working after limit errors" eq "$(cat "$MNT/名前 with spaces.jsonl")" '{"x":"ok"}'
fusermount3 -u "$MNT"

BAD=$DIR/corrupt.db
sqlite3 "$BAD" "CREATE TABLE small(x); INSERT INTO small VALUES (1);
    CREATE TABLE junk(x); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000)
    INSERT INTO junk SELECT hex(randomblob(100)) FROM n;"
# Overwrite pages in the middle of the file with garbage.
python3 -c "
import os, sys
with open(sys.argv[1], 'r+b') as f:
    f.seek(40 * 4096); f.write(os.urandom(64 * 4096))
" "$BAD"
"$BIN" "$BAD" "$MNT"
check "a corrupt table fails to read" bash -c "! cat '$MNT/junk.jsonl' >/dev/null 2>&1"
check "the rest of a corrupt database is still readable" eq "$(cat "$MNT/small.jsonl")" '{"x":1}'
fusermount3 -u "$MNT"

echo "== logging and man page"
"$BIN" "$DB" "$MNT" -d 2>"$DIR/debug.log" &
PID=$!
wait_mounted
cat "$MNT/g.jsonl" >/dev/null
fusermount3 -u "$MNT"; wait $PID || true
check "-d logs filesystem operations to stderr" grep -q 'debug: lookup "g.jsonl" -> ino' "$DIR/debug.log"
check "-d logs mount and unmount" bash -c "grep -q '^sqlitefuse: mounted' '$DIR/debug.log' && grep -q '^sqlitefuse: unmounted' '$DIR/debug.log'"
check "--man prints a man page" bash -c "'$BIN' --man | grep -q '^\.TH sqlitefuse 1'"

echo "== mount errors and signals"
echo "not a database, but long enough to fill an SQLite header ........................................" > "$DIR/bad.db"
check "a non-SQLite file is rejected" bash -c "! '$BIN' '$DIR/bad.db' '$MNT'"
check "unknown options are rejected" bash -c "! '$BIN' '$DB' '$MNT' -o views"
check "nothing is left mounted after errors" bash -c "! mountpoint -q '$MNT'"
"$BIN" "$DB" "$MNT" -f & PID=$!
wait_mounted
kill -TERM $PID; wait $PID || true
check "SIGTERM in the foreground unmounts cleanly" bash -c "! mountpoint -q '$MNT'"

if [[ ${KEEP:-} == 1 ]]; then "$BIN" "$DB" "$MNT"; fi

echo
echo "passed: $PASS, failed: $FAIL"
[[ $FAIL -eq 0 ]]
