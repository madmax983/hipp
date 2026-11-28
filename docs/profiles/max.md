# The max profile

```toml
hipp = { git = "https://github.com/madmax983/hipp", default-features = false, features = ["max"] }
```

**Budget:** 1 MiB of code on thumbv7em (ratchet 1,002,324 B). `no_std`.
**Example:** [`examples/max.rs`](../../examples/max.rs).

```bash
cargo run --example max --no-default-features --features max
```

max is [full](full.md) plus the heavyweight leaves that do not fit full's
512 KiB (ADR 0002). It is the smallest profile that opens **every** SQLite
file format feature: WAL, UTF-16, WITHOUT ROWID, auto-vacuum, virtual
tables. Use it when your files come from, or go to, desktop SQLite.

| feature | adds |
|---|---|
| `window` | window functions: `OVER`, named windows, ROWS / RANGE / GROUPS frames with EXCLUDE, every built-in window function, every aggregate as a window function |
| `triggers` | `CREATE TRIGGER` BEFORE / AFTER / INSTEAD OF, `WHEN`, `NEW.` / `OLD.`, `RAISE()`, recursive triggers |
| `foreign_keys` | enforcement under `PRAGMA foreign_keys = ON`: CASCADE / SET NULL / SET DEFAULT / RESTRICT, deferred keys, `PRAGMA foreign_key_check` |
| `json` | the JSON functions, `->` / `->>`, JSONB (byte-identical to SQLite's), `json_each` / `json_tree`, JSON5 input |
| `expr_index` | expression indexes, partial indexes, and index-aware planning for single tables |
| `without_rowid` | `WITHOUT ROWID` tables, stored as SQLite stores them |
| `utf16` | UTF-16le / UTF-16be databases |
| `wal` | write-ahead logging, in SQLite's `-wal` format |
| `datetime` | `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`, `strftime()`, `timediff()`, `CURRENT_*` |
| `attach` | `ATTACH` / `DETACH`, cross-database queries, atomic multi-file commits |
| `autovacuum` | `auto_vacuum = FULL / INCREMENTAL` files, kept correct on every write |
| `vtab` | virtual tables: `generate_series`, `dbstat` |
| `rtree` | the R*Tree module, with SQLite's shadow tables |
| `fts5` | FTS5 full-text search, with SQLite's shadow tables |
| `explain` | `EXPLAIN QUERY PLAN` |

Every leaf can also be enabled on its own on top of `sql` (for example
`features = ["sql", "json"]`). [`ci/feature-sets.txt`](../../ci/feature-sets.txt)
lists the minimal set for each one, and CI builds every set.

The example's connection uses a fixed clock and a fixed UTC+2 zone:

```rust
fn fixed_clock() -> i64 { 1_767_323_045_678 }            // 2026-01-02 03:04:05.678 UTC
fn plus_two(_unix_seconds: i64) -> Option<i64> { Some(2 * 3600) }
let cfg = Config::default().clock(fixed_clock).tz_offset(Some(plus_two));
```

It starts from a `sales` table: two regions, three months.

## Window functions (`window`)

```text
sqlite> SELECT region, month, amount,
                sum(amount) OVER (PARTITION BY region ORDER BY month) AS running,
                rank() OVER (PARTITION BY region ORDER BY amount DESC) AS rnk
         FROM sales ORDER BY region, month
north|1|100|100|3
north|2|150|250|1
north|3|120|370|2
south|1|80|80|2
south|2|80|160|2
south|3|200|360|1
sqlite> SELECT month, amount, lag(amount) OVER w, lead(amount, 1, 0) OVER w, row_number() OVER w
         FROM sales WHERE region = 'south' WINDOW w AS (ORDER BY month)
1|80||80|1
2|80|80|200|2
3|200|80|0|3
sqlite> SELECT id, round(avg(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), 2),
                dense_rank() OVER (ORDER BY amount), ntile(2) OVER (ORDER BY id),
                first_value(amount) OVER (ORDER BY id GROUPS UNBOUNDED PRECEDING)
         FROM sales WHERE region = 'north'
1|125.0|1|1|100
2|123.33|3|1|100
3|135.0|2|2|100
```

The built-ins are `row_number`, `rank`, `dense_rank`, `percent_rank`,
`cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value` and
`nth_value`, plus every aggregate (including `FILTER`, `string_agg`,
`json_group_array`). The frame engine steps and inverts in SQLite's order, so
REAL running sums round exactly as SQLite's do.

## Triggers (`triggers`)

```sql
CREATE TABLE audit (at INTEGER, what TEXT);
CREATE TRIGGER sales_ins AFTER INSERT ON sales
BEGIN INSERT INTO audit VALUES (NEW.id, 'insert ' || NEW.region); END;
CREATE TRIGGER sales_upd AFTER UPDATE OF amount ON sales WHEN NEW.amount > OLD.amount
BEGIN INSERT INTO audit VALUES (NEW.id, 'raised ' || OLD.amount || '->' || NEW.amount); END;
CREATE TRIGGER no_neg BEFORE INSERT ON sales WHEN NEW.amount < 0
BEGIN SELECT RAISE(ABORT, 'negative amount'); END;

INSERT INTO sales (region, month, amount) VALUES ('east', 1, 10);
UPDATE sales SET amount = amount + 5 WHERE id IN (1, 7);
```

```text
sqlite> SELECT * FROM audit
7|insert east
1|raised 100->105
7|raised 10->15
sqlite> INSERT INTO sales (region, month, amount) VALUES ('east', 2, -1)
Error: Schema { kind: RaiseAbort }
```

INSTEAD OF triggers make a view writable:

```sql
CREATE VIEW totals AS SELECT region, sum(amount) AS total FROM sales GROUP BY region;
CREATE TRIGGER totals_ins INSTEAD OF INSERT ON totals
BEGIN INSERT INTO sales (region, month, amount) VALUES (NEW.region, 0, NEW.total); END;
INSERT INTO totals VALUES ('west', 42);
```

```text
sqlite> SELECT total FROM totals WHERE region = 'west'
42
```

Triggers fire in SQLite's order with its recursion rules
(`PRAGMA recursive_triggers`, a depth limit set by
`Config::max_trigger_depth`). `ALTER TABLE` rewrites trigger texts as SQLite
does. Without `triggers`, writes to a table that has a trigger are refused,
never run without it.

## Foreign keys (`foreign_keys`)

```sql
PRAGMA foreign_keys = ON;
CREATE TABLE author (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT,
    author INTEGER REFERENCES author(id) ON DELETE CASCADE ON UPDATE CASCADE);
INSERT INTO author VALUES (1, 'le guin'), (2, 'banks');
INSERT INTO book VALUES (10, 'the dispossessed', 1), (11, 'excession', 2), (12, 'use of weapons', 2);
```

```text
sqlite> INSERT INTO book VALUES (13, 'orphan', 99)
Error: Schema { kind: ForeignKeyViolation }
sqlite> UPDATE author SET id = 20 WHERE id = 2      -- cascades to book.author
sqlite> SELECT id, author FROM book ORDER BY id
10|1
11|20
12|20
sqlite> DELETE FROM author WHERE id = 20            -- cascades: both books go
sqlite> SELECT title FROM book
the dispossessed
sqlite> PRAGMA foreign_key_check
```

`ON DELETE` / `ON UPDATE` CASCADE, SET NULL, SET DEFAULT, RESTRICT and NO
ACTION are supported, as are composite and self-referencing keys,
`DEFERRABLE INITIALLY DEFERRED` (checked at COMMIT),
`PRAGMA defer_foreign_keys`, and DROP TABLE's implicit delete. Without
`foreign_keys`, a write to a table in a foreign key is refused while the
pragma is ON.

## JSON (`json`)

```sql
CREATE TABLE events (id INTEGER PRIMARY KEY, body TEXT);
INSERT INTO events (body) VALUES
  ('{"kind":"click","pos":{"x":10,"y":20},"tags":["a","b"]}'),
  ('{"kind":"key","code":65,"tags":[]}');
```

```text
sqlite> SELECT body ->> '$.kind', body -> '$.pos', json_extract(body, '$.pos.x'), json_array_length(body, '$.tags') FROM events
click|{"x":10,"y":20}|10|2
key|||0
sqlite> SELECT json_object('a', 1, 'b', json_array(1, 2, 'x')), json('{ "z" : [1,2] }')
{"a":1,"b":[1,2,"x"]}|{"z":[1,2]}
sqlite> SELECT json_set('{"a":1}', '$.b', 2), json_remove('[1,2,3]', '$[1]'), json_patch('{"a":1,"b":2}', '{"b":null,"c":3}')
{"a":1,"b":2}|[1,3]|{"a":1,"c":3}
sqlite> SELECT json_type(body, '$.code'), json_valid(body), json_valid('{bad') FROM events WHERE id = 2
integer|1|0
sqlite> SELECT key, value FROM events, json_each(events.body, '$.tags') WHERE events.id = 1
0|a
1|b
sqlite> SELECT json_group_array(region) FROM (SELECT DISTINCT region FROM sales ORDER BY 1)
["east","north","south","west"]
sqlite> SELECT hex(jsonb('[1,"a"]')), json(jsonb('[1,"a"]'))
4B13311761|[1,"a"]
```

The functions are `json`, `jsonb`, `json_array`, `json_object`,
`json_extract`, `->`, `->>`, `json_insert` / `json_replace` / `json_set` /
`json_remove` / `json_array_insert` (and their `jsonb_` forms), `json_patch`,
`json_type`, `json_valid`, `json_quote`, `json_array_length`,
`json_error_position`, `json_pretty`, `json_group_array`,
`json_group_object`, `json_each` and `json_tree`. JSON5 input, the JSON
subtype rules and the 1000-level nesting limit behave as on SQLite.

## Expression and partial indexes (`expr_index`)

```sql
CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER);
INSERT INTO users (email, active) VALUES ('Ann@X.io', 1), ('bob@y.io', 0), ('CAT@z.io', 1);
CREATE UNIQUE INDEX users_email_lower ON users(lower(email));
CREATE INDEX users_active ON users(email) WHERE active = 1;
```

```text
sqlite> SELECT id FROM users WHERE lower(email) = 'cat@z.io'
3
sqlite> INSERT INTO users (email, active) VALUES ('ann@x.io', 0)
Error: Schema { kind: UniqueViolation }
sqlite> EXPLAIN QUERY PLAN SELECT email FROM users WHERE lower(email) = 'cat@z.io'
`--SEARCH users USING INDEX users_email_lower (<expr>=?)
sqlite> PRAGMA index_xinfo(users_email_lower)
0|-2||0|BINARY|1
1|-1||0|BINARY|0
```

Expression and partial indexes are maintained on every write path, and
UNIQUE is enforced over the rows a partial index's WHERE admits. The access
planner answers equality, IN and range terms (and ORDER BY) through the
rowid, PRIMARY KEY, UNIQUE, ordinary, partial and expression indexes, in
SQLite's row order.

## WITHOUT ROWID tables (`without_rowid`)

```sql
CREATE TABLE settings (scope TEXT, key TEXT, value, PRIMARY KEY (scope, key)) WITHOUT ROWID;
INSERT INTO settings VALUES ('ui', 'theme', 'dark'), ('net', 'port', 8080), ('ui', 'font', 'mono');
```

```text
sqlite> SELECT scope, key, value FROM settings
net|port|8080
ui|font|mono
ui|theme|dark
sqlite> UPDATE settings SET value = 'light' WHERE scope = 'ui' AND key = 'theme'
sqlite> SELECT value FROM settings WHERE scope = 'ui' AND key = 'theme'
light
sqlite> SELECT rowid FROM settings
Error: Schema { kind: UnknownColumn }
```

Rows are stored in a clustered index B-tree keyed by the PRIMARY KEY, byte
for byte as SQLite stores them, and work on every read and write path.

## Date and time (`datetime`)

```text
sqlite> SELECT date('2026-03-15'), time('2026-03-15 13:45:10'), datetime(0, 'unixepoch')
2026-03-15|13:45:10|1970-01-01 00:00:00
sqlite> SELECT date('2026-01-31', '+1 month'), date('2026-03-15', 'start of month', '-1 day'), date('2026-03-15', 'weekday 0')
2026-03-03|2026-02-28|2026-03-15
sqlite> SELECT julianday('2000-01-01 12:00:00'), unixepoch('2026-01-01'), strftime('%Y week %W day %j', '2026-03-15')
2451545.0|1767225600|2026 week 10 day 074
sqlite> SELECT timediff('2026-03-15', '2026-01-01')
+0000-02-14 00:00:00.000
sqlite> SELECT datetime('now'), datetime('now', 'localtime'), CURRENT_DATE, strftime('%H:%M:%f', 'now')
2026-01-02 03:04:05|2026-01-02 05:04:05|2026-01-02|03:04:05.678
```

This is a model of SQLite's `date.c`: every time-value form and modifier,
applied in its order with its rounding. `'now'` reads `Config::clock` once
per statement, and `'localtime'` / `'utc'` read `Config::tz_offset`. Under
`std` both default to the system's.

## Virtual tables (`vtab`)

```text
sqlite> SELECT group_concat(value, ',') FROM generate_series(1, 10, 3)
1,4,7,10
sqlite> SELECT value * value FROM generate_series(1, 5) WHERE value % 2 = 1
1
9
25
dbstat: sales uses 1 page(s)
```

The modules are `generate_series` (eponymous) and `dbstat` (eponymous, or
`CREATE VIRTUAL TABLE ... USING dbstat(schema)`). A file holding a virtual
table of a module hipp lacks still opens. Only statements touching that
table are refused ("no such module"). `Config::max_vtab_rows` bounds how many
rows a statement may hold in memory.

## R*Tree (`rtree`)

```sql
CREATE VIRTUAL TABLE zones USING rtree(id, min_x, max_x, min_y, max_y, +label);
INSERT INTO zones VALUES (1, 0, 10, 0, 10, 'home'), (2, 5, 15, 5, 15, 'park'), (3, 20, 30, 20, 30, 'lake');
```

```text
sqlite> SELECT id, label FROM zones WHERE min_x <= 7 AND max_x >= 7 AND min_y <= 7 AND max_y >= 7
1|home
2|park
sqlite> SELECT rtreecheck('zones')
ok
```

`rtree` and `rtree_i32` tables take 1 to 5 dimensions and auxiliary
`+columns`. The shadow tables are byte-compatible with SQLite's (same node
layout, split algorithm and float32 rounding), and `rtreecheck()`,
`rtreenode()` and `rtreedepth()` are provided. Geometry callbacks (`MATCH`)
are refused.

## FTS5 full-text search (`fts5`)

```sql
CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize = 'porter');
INSERT INTO docs VALUES
  ('Rust embedded', 'Running databases on microcontrollers with no_std'),
  ('SQLite files', 'Reading and writing the SQLite file format'),
  ('Gardening', 'Growing tomatoes in small spaces');
```

```text
sqlite> SELECT title FROM docs WHERE docs MATCH 'run'
Rust embedded
sqlite> SELECT title FROM docs WHERE docs MATCH 'sqlite OR tomato*' ORDER BY rank
SQLite files
Gardening
sqlite> SELECT highlight(docs, 1, '[', ']') FROM docs WHERE docs MATCH 'sqlite'
Reading and writing the [SQLite] file format
sqlite> SELECT title FROM docs WHERE docs MATCH 'NEAR(file format, 2)'
SQLite files
```

The table options are `UNINDEXED` columns, `prefix=`, tokenizers
(`unicode61`, `ascii`, `porter`, `trigram`), `content=` / `content_rowid=`,
`columnsize=` and `detail=`. Queries take the full query language (phrases,
`prefix*`, `^`, `NEAR`, column filters, AND / OR / NOT), `rank`, `bm25()`,
`highlight()` and `snippet()`. `fts5vocab` tables, the special INSERT
commands (`'integrity-check'`, `'optimize'`, `'merge'`, ...) and automerge are
supported. The shadow tables are byte-compatible with SQLite's. Deletes on
contentless tables and `secure-delete` are in [xl](xl.md).

## EXPLAIN QUERY PLAN (`explain`)

```text
sqlite> EXPLAIN QUERY PLAN SELECT * FROM sales WHERE region = 'north' ORDER BY month
`--SEARCH sales USING INDEX sales_region (region=?)
`--USE TEMP B-TREE FOR ORDER BY
sqlite> EXPLAIN QUERY PLAN SELECT * FROM sales WHERE id = 3
`--SEARCH sales USING INTEGER PRIMARY KEY (rowid=?)
sqlite> EXPLAIN QUERY PLAN SELECT region, count(*) FROM sales GROUP BY region
`--SCAN sales USING COVERING INDEX sales_region
```

The rows hold SQLite's four columns (`id`, `parent`, `notused`, `detail`).
The detail text is SQLite's wording for the plan hipp actually runs.
`id` / `parent` are hipp's own numbering (SQLite uses bytecode addresses),
and the CLI prints the same tree the `sqlite3` shell does. Plain `EXPLAIN`
(a bytecode listing) is refused.

## ATTACH / DETACH (`attach`)

Without `std` there is no file system, so the example maps attached names to
storage with a hook:

```rust
let files: Rc<RefCell<HashMap<Vec<u8>, MemStorage>>> = Rc::default();
let hook_files = files.clone();
db.set_attach_hook(move |target| match target {
    AttachTarget::File(name) => Some(hook_files.borrow_mut().entry(name.to_vec()).or_default().clone()),
    AttachTarget::Memory => Some(MemStorage::memory_db()),
    AttachTarget::Temporary => Some(MemStorage::new()),
});
```

```sql
ATTACH DATABASE 'archive.db' AS archive;
CREATE TABLE archive.old_sales AS SELECT * FROM sales WHERE month = 1;
```

```text
sqlite> SELECT name FROM pragma_database_list ORDER BY seq
main
archive
sqlite> SELECT s.region, s.amount, o.amount FROM sales s JOIN archive.old_sales o USING (id) ORDER BY s.id
north|105|105
south|80|80
east|15|15
```

One transaction can span both files. Its commit is made atomic by SQLite's
super-journal, which either engine recovers:

```sql
BEGIN;
DELETE FROM archive.old_sales WHERE region = 'east';
UPDATE sales SET amount = 0 WHERE region = 'east';
COMMIT;
DETACH archive;
```

The attached file is a database of its own:

```text
sqlite> SELECT region FROM old_sales          -- opened from archive.db's bytes
north
south
```

SQLite's limit of 10 attached databases applies, as does its name lookup
order (temp, main, then attached in attach order). Schema-qualified names
work everywhere SQLite takes them. Under `std`, `FileStorage` opens attached
files itself.

## Write-ahead logging (`wal`)

```text
sqlite> PRAGMA journal_mode = WAL
wal
-wal holds 12392 bytes before the checkpoint
sqlite> PRAGMA wal_checkpoint(TRUNCATE)
0|0|0
sqlite> SELECT msg FROM log           -- after close + reopen
one
two
three
sqlite> PRAGMA journal_mode
wal
```

Commits append frames to `<db>-wal` in SQLite's layout (salts, cumulative
checksums, commit markers), so SQLite reads and recovers hipp's log, and the
reverse. Opening replays committed frames and stops at the first invalid
one. `PRAGMA wal_checkpoint` (PASSIVE / FULL / RESTART / TRUNCATE), automatic
checkpoints (`wal_autocheckpoint`), `synchronous` and `journal_size_limit`
all work. `Connection::close` checkpoints and deletes the log.

WAL here is **single-process**, as SQLite's `locking_mode=EXCLUSIVE`: the
wal-index lives in heap memory and no `-shm` is written. A `-shm` that
another process holds is refused (`DatabaseBusy`). Without `wal`, a WAL file
is refused at open (`WalUnsupported`) rather than read stale.

## UTF-16 databases (`utf16`)

```sql
PRAGMA encoding = 'UTF-16le';          -- before the database has content
CREATE TABLE words (w TEXT);
INSERT INTO words VALUES ('héllo'), ('wörld');
```

```text
sqlite> PRAGMA encoding
UTF-16le
sqlite> SELECT w, length(w), hex(w) FROM words WHERE w = 'héllo'
héllo|5|6800E9006C006C006F00
```

The API is UTF-8. The file stores UTF-16 (header bytes 56..60 = 2), and
`hex()`, `CAST(... AS BLOB)` and the BINARY collation see UTF-16 bytes as on
SQLite. Without `utf16`, such a file is refused at open
(`UnsupportedEncoding`).

## Auto-vacuum (`autovacuum`)

```sql
PRAGMA auto_vacuum = FULL;             -- on an empty database
CREATE TABLE blobs (b BLOB);
INSERT INTO blobs SELECT randomblob(3000) FROM generate_series(1, 20);
DELETE FROM blobs;
```

```text
auto_vacuum = FULL: 23 pages -> 3 pages after the DELETE's commit
sqlite> PRAGMA auto_vacuum
1
sqlite> PRAGMA integrity_check
ok
```

The pointer-map pages are maintained on every write, and FULL truncates at
each commit. INCREMENTAL mode runs `PRAGMA incremental_vacuum(N)`. Without
`autovacuum`, an auto-vacuum file is read but every write is refused
(`AutoVacuumUnsupported`).
