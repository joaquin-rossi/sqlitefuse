# sqlitefuse

A read-only FUSE filesystem that shows an SQLite database as a directory: every
table and view is a JSONL file with one JSON row per line. Use `cat`, `grep`,
`jq`, `wc`, `diff` or `rsync` on your data without writing SQL.

```sh
$ sqlitefuse sales.db /mnt/sales
$ ls /mnt/sales
customers.jsonl  orders.jsonl  products.jsonl
$ head -2 /mnt/sales/customers.jsonl
{"id":1,"name":"María López","country":"AR"}
{"id":2,"name":"José Pérez","country":"AR"}
$ fusermount -u /mnt/sales
```

## Install

```sh
cargo install sqlitefuse
```

It needs FUSE 3 (`fusermount3`, in the `fuse3` package on most
distributions).

## Try it with the example

`example/shop.sql` builds a small shop database: customers, products, orders,
order items, a view with order totals, indexes, triggers and a full-text index.

Build the binary and the database, then mount it:

```sh
cargo build --release
sqlite3 /tmp/shop.db < example/shop.sql
mkdir -p /tmp/shop
target/release/sqlitefuse /tmp/shop.db /tmp/shop
```

List the tables and views. Internal tables are hidden files:

```sh
$ ls /tmp/shop
customers.jsonl  order_items.jsonl  orders.jsonl  order_totals.jsonl  products.jsonl
$ ls -a /tmp/shop | grep '^\.s'
.sqlite_schema.jsonl
.sqlite_sequence.jsonl
.sqlite_stat1.jsonl
.sqlite_stat4.jsonl
```

Read a table, one JSON object per row:

```sh
$ head -2 /tmp/shop/customers.jsonl
{"id":1,"name":"María López","email":"maria@example.com","country":"AR","avatar":{"$base64":"iVBORw0KGgo="}}
{"id":2,"name":"José Pérez","email":"jose@example.com","country":"AR","avatar":null}
```

Filter and aggregate with the usual tools:

```sh
$ jq -c 'select(.stock == 0) | {sku, name}' /tmp/shop/products.jsonl
{"sku":"TEA-BLK","name":"Black tea, 100 g"}
$ grep -c '"status":"shipped"' /tmp/shop/orders.jsonl
2
$ jq -s 'map(.total) | add' /tmp/shop/order_totals.jsonl
147.94
$ wc -l /tmp/shop/order_items.jsonl
8 /tmp/shop/order_items.jsonl
```

Table details live in extended attributes:

```sh
$ getfattr --only-values -n user.sqlite.order /tmp/shop/order_items.jsonl; echo
{"by":"pk","columns":["order_id","sku"]}
$ getfattr --only-values -n user.sqlite.rows /tmp/shop/order_items.jsonl; echo
8
$ getfattr --only-values -n user.sqlite.indexes /tmp/shop/orders.jsonl | jq -c '.[] | {name, columns, partial}'
{"name":"orders_customer","columns":["customer_id","placed_at"],"partial":false}
{"name":"orders_pending","columns":["placed_at"],"partial":true}
$ getfattr --only-values -n user.sqlite.foreign_keys /tmp/shop/order_items.jsonl | jq -c '.[] | {columns, table, to}'
{"columns":["order_id"],"table":"orders","to":["id"]}
{"columns":["sku"],"table":"products","to":["sku"]}
$ getfattr --only-values -n user.sqlite.triggers /tmp/shop/orders.jsonl | jq -c '.[] | {name, timing, event, columns}'
{"name":"orders_keep_shipped","timing":"BEFORE","event":"UPDATE","columns":["status"]}
```

Changes made through SQLite show up right away:

```sh
$ cp /tmp/shop/products.jsonl /tmp/before.jsonl
$ sqlite3 /tmp/shop.db "UPDATE products SET stock = 30 WHERE sku = 'TEA-BLK'"
$ diff /tmp/before.jsonl /tmp/shop/products.jsonl
3c3
< {"sku":"TEA-BLK","name":"Black tea, 100 g","price":7.25,"stock":0,"stock_value":0.0}
---
> {"sku":"TEA-BLK","name":"Black tea, 100 g","price":7.25,"stock":30,"stock_value":217.5}
```

The mount is read-only:

```sh
$ echo x >> /tmp/shop/orders.jsonl
bash: /tmp/shop/orders.jsonl: Read-only file system
```

Unmount when done:

```sh
fusermount -u /tmp/shop
```

## fstab

To mount at boot, add a line to `/etc/fstab` (with the binary on `PATH`):

```
/srv/data/sales.db  /mnt/sales  fuse.sqlitefuse  ro,allow_other  0 0
```

## Files

- Each table and view is `<name>.jsonl`.
- Internal tables (`sqlite_schema`, `sqlite_sequence`, `sqlite_stat*`, and
  the tables behind full-text indexes) are hidden files, like
  `.sqlite_sequence.jsonl`. Virtual tables are not shown.
- Characters that can't go in a file name are escaped: `/` → `%2F`,
  `%` → `%25`, and a leading `.` → `%2E`.

Extended attributes (`getfattr -d file.jsonl`):

| Attribute                  | Content                                                                                                             |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `user.sqlite.sql`          | The original `CREATE TABLE` or `CREATE VIEW`                                                                        |
| `user.sqlite.kind`         | Table, view or shadow table; whether it is internal, `STRICT` or `WITHOUT ROWID`                                    |
| `user.sqlite.columns`      | Name, declared type, primary key, `NOT NULL`, default, collation, `AUTOINCREMENT` and generated flag of each column |
| `user.sqlite.indexes`      | Name, columns, uniqueness, origin, partial flag and `CREATE INDEX` of each index                                    |
| `user.sqlite.foreign_keys` | Columns, referenced table and columns, and `ON UPDATE`/`ON DELETE` actions of each foreign key                      |
| `user.sqlite.triggers`     | Name, timing, event, `UPDATE OF` columns and `CREATE TRIGGER` of each trigger                                       |
| `user.sqlite.rows`         | Exact row count                                                                                                     |
| `user.sqlite.order`        | How rows are ordered                                                                                                |

The mount directory has `user.sqlite.journal_mode` and `user.sqlite.page_size`.

## Format

Each row is a JSON object on one line, with keys in column order. Values map
like this:

| SQLite value                | JSON                                                           |
| --------------------------- | -------------------------------------------------------------- |
| NULL                        | `null`                                                         |
| INTEGER                     | number, exact up to 64 bits                                    |
| REAL                        | number, always with a `.` or exponent: `1.0`, `120.5`, `1e300` |
| REAL infinity               | `{"$float":"inf"}` or `{"$float":"-inf"}`                      |
| TEXT                        | string                                                         |
| TEXT that isn't valid UTF-8 | `{"$base64":"…"}`                                              |
| BLOB                        | `{"$base64":"…"}`                                              |

Integers above 2^53 are written exactly, but JavaScript rounds them when
parsing. `jq` 1.7 or later keeps them.

Rows always come out in the same order, so `diff` and `md5sum` work:

- Regular tables: by rowid.
- `WITHOUT ROWID` tables: by primary key.
- Views: by all columns, left to right.

`VACUUM` can renumber rows of tables without an `INTEGER PRIMARY KEY`, which
changes their order.

## When the database changes

A file you have open never mixes data from before and after a write. What
happens depends on the database's journal mode:

- **WAL**: an open file keeps showing the data as it was when it was opened.
  Writers are never blocked.
- **Rollback** (SQLite's default): if someone writes while you are reading a
  file, the read fails with an I/O error. Open the file again to get the new
  data. With `-o lock`, writers wait instead until you close the file.

## Options (`-o`)

| Option           | Effect                                                                          |
| ---------------- | ------------------------------------------------------------------------------- |
| `lock`           | In rollback mode, block writers while a file is open, so reads never fail       |
| `immutable`      | For databases that never change (read-only media, backups): no locking at all   |
| `uid=`, `gid=`   | File owner (default: the user who mounts)                                       |
| `allow_other`    | Let other users see the mount                                                   |
| `allow_root`     | Let root see the mount                                                          |
| `max_value=SIZE` | Largest TEXT or BLOB value read (default `256M`); larger ones fail with `EFBIG` |
| `max_view=SIZE`  | Largest view, in JSON bytes (default `1G`); larger ones fail with `EFBIG`       |
| `debug`          | Log every filesystem operation (same as `-d`, implies `-f`)                     |

`SIZE` takes the suffixes `K`, `M` and `G`. The limits protect against
databases that are malformed or made to exhaust memory or disk.

## Logs

In the background, messages go to syslog: `journalctl -t sqlitefuse`. With
`-f` they go to stderr.

## Man page

```sh
sqlitefuse --man | sudo tee /usr/local/share/man/man1/sqlitefuse.1 >/dev/null
```

## Limitations

- The first `stat` or `ls -l` after a write reads the whole table to compute
  its size. On tables of several GB that can take seconds.
- All files share the database's modification time, so after any write
  every table looks modified.
- Tables with more than 999 columns are not shown.

## Testing

```sh
cargo test
cargo build --release
tests/smoke.sh
```

`tests/smoke.sh` mounts a sample database in a temporary directory, checks
the behavior described above and unmounts. It needs `sqlite3`, `python3`,
`getfattr` and FUSE.
