# The tiny profile

```toml
hipp = { git = "https://github.com/madmax983/hipp", default-features = false }
```

**Budget:** 128 KiB of code on thumbv7em (ratchet 77,508 B), 0 B of engine
statics, and within 64 KiB of engine RAM under `Config::tiny()`.
**Example:** [`examples/tiny.rs`](../../examples/tiny.rs).

```bash
cargo run --example tiny --no-default-features
```

tiny is a durable record store that writes SQLite files: rowid tables,
inserts, full scans and rowid lookups, transactions with a rollback journal
and crash recovery. It has no expression evaluator. The only filter is a
rowid equality, and anything else you do in Rust while stepping a scan.

## What is in it

| | |
|---|---|
| `CREATE TABLE [IF NOT EXISTS]` | rowid tables. Columns take any declared type (affinity follows SQLite's rules). `INTEGER PRIMARY KEY` is the rowid alias. `NOT NULL` is enforced. |
| `INSERT INTO t [(cols)] VALUES (...), (...)` | literals of every class (integer, real, text, `X'..'` blob, NULL). An omitted rowid is assigned as max + 1. |
| `SELECT * \| col, ... FROM t` | a full scan in rowid order. `rowid` / `oid` / `_rowid_` are selectable. |
| `SELECT ... WHERE rowid = <literal>` | a B-tree point lookup, also through the INTEGER PRIMARY KEY alias. |
| `BEGIN [DEFERRED \| IMMEDIATE \| EXCLUSIVE]`, `COMMIT` / `END`, `ROLLBACK` | explicit transactions. Each other statement is its own. |
| Rollback journal | byte-compatible with SQLite's. A hot journal is recovered at open. |
| Storage | overflow chains for large values, freelist reuse, B-tree splits, any page size from 512 to 65536. |
| Memory | statement / transaction / catalog arenas with hard maxima, and `ArenaExhausted` instead of a panic. |
| Catalog API | `Connection::tables()`, `arena_stats()`. |

Optional on top of tiny, without the rest of `full`:

- `features = ["params"]`: bound parameters wherever the tiny grammar takes a
  literal (`INSERT ... VALUES (?, ?)`, `WHERE rowid = ?`).
- `features = ["indexes"]`: `CREATE [UNIQUE] INDEX`, `DROP INDEX`, UNIQUE
  constraints, and `WHERE <first indexed column> = <literal>` lookups.
- `features = ["std"]`: `FileStorage` for files on a host.

## Walkthrough

Each block below is a section of `examples/tiny.rs` and its real output. The
example opens with the reference embedded configuration:

```rust
let mut db = Connection::open(MemStorage::new(), Config::tiny()).await?;
```

### Create tables and insert

```sql
CREATE TABLE sensors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, room TEXT);
CREATE TABLE readings (sensor INTEGER, at INTEGER, celsius REAL, raw BLOB);
CREATE TABLE IF NOT EXISTS sensors (x);          -- no-op: it exists

INSERT INTO sensors (name, room) VALUES ('north', 'hall'), ('south', 'lab'), ('east', 'lab');
INSERT INTO sensors VALUES (10, 'roof', NULL);
INSERT INTO readings VALUES (1, 1700000000, 21.5, X'00ff'), (2, 1700000000, 19.25, NULL);
INSERT INTO readings VALUES (1, 1700000060, 22, NULL), (10, 1700000060, -4.5, X'cafe');
```

```rust
let names: Vec<_> = db.tables().iter().map(|t| String::from_utf8_lossy(t.name).into_owned()).collect();
// ["sensors", "readings"]
```

### Scan

```text
sqlite> SELECT * FROM sensors
1|north|hall
2|south|lab
3|east|lab
10|roof|
sqlite> SELECT sensor, celsius, raw FROM readings
1|21.5|X'00FF'
2|19.25|
1|22.0|
10|-4.5|X'CAFE'
sqlite> SELECT rowid, name FROM sensors
1|north
2|south
3|east
10|roof
```

`22` inserted into a `REAL` column reads back as `22.0`: column affinity
works as on SQLite.

### Look up by rowid

```text
sqlite> SELECT name FROM sensors WHERE rowid = 10
roof
sqlite> SELECT name, room FROM sensors WHERE id = 2
south|lab
sqlite> SELECT name FROM sensors WHERE id = 99
```

Any other filter needs the evaluator (`full`). In tiny, filter while
stepping:

```rust
let mut st = db.prepare("SELECT name, room FROM sensors").await?;
let mut in_lab = Vec::new();
while st.step().await? == Step::Row {
    if st.column_bytes(1) == Some(&b"lab"[..]) {
        in_lab.push(String::from_utf8_lossy(st.column_bytes(0).unwrap_or_default()).into_owned());
    }
}
// ["south", "east"]
```

### Read columns from Rust

```rust
let mut st = db.prepare("SELECT id, name, room FROM sensors WHERE rowid = 10").await?;
assert_eq!(st.step().await?, Step::Row);
assert_eq!(st.column_count(), 3);
assert_eq!(st.column_i64(0), Some(10));
assert_eq!(st.column_bytes(1), Some(&b"roof"[..]));
assert!(st.column_is_null(2));
assert_eq!(st.column_value(1), Some(Value::Text(b"roof")));
assert_eq!(st.step().await?, Step::Done);
```

### Constraint and schema errors

```text
sqlite> INSERT INTO sensors (name) VALUES (NULL)
Error: Schema { kind: NotNullViolation }
sqlite> INSERT INTO sensors VALUES (1, 'again', NULL)
Error: Schema { kind: DuplicateRowid }
sqlite> SELECT nope FROM sensors
Error: Schema { kind: UnknownColumn }
sqlite> SELECT * FROM missing
Error: Schema { kind: UnknownTable }
```

### Transactions

```sql
BEGIN;
INSERT INTO sensors (name) VALUES ('temporary');   -- visible inside the transaction
ROLLBACK;                                          -- and gone after it
```

```text
sqlite> SELECT name FROM sensors WHERE rowid = 11
temporary
sqlite> SELECT name FROM sensors WHERE rowid = 11
```

Batch writes in one transaction (one journal, one sync):

```rust
exec(&mut db, "BEGIN").await;
for i in 0..50 {
    exec(&mut db, &format!("INSERT INTO readings VALUES (3, {}, {i}.5, NULL)", 1_700_000_100 + i)).await;
}
exec(&mut db, "COMMIT").await;
// committed 50 readings in one transaction
```

### Large values and arena limits

A value larger than a page goes to an overflow chain:

```text
read back a 6000-byte value
```

A statement's parsed SQL lives in its arena (16 KiB maximum under
`Config::tiny()`), so a 20 KB literal does not fit. It fails cleanly:

```text
sqlite> INSERT INTO sensors (name) VALUES ('yyyy... (20038 bytes)
Error: ArenaExhausted { scope: Statement, needed: 20038 }
```

Raise the limit with `Config::tiny().statement_arena(4 * 1024, 64 * 1024)`,
or (with `params`) bind the value instead of writing it into the SQL.

### What tiny refuses

These need `full`. A tiny build refuses them with a named error:

```text
sqlite> UPDATE sensors SET room = 'attic'
Error: Parse { offset: 0, kind: UnsupportedStatement }
sqlite> SELECT count(*) FROM sensors
Error: Parse { offset: 12, kind: UnexpectedToken }
sqlite> SELECT * FROM sensors ORDER BY name
Error: Parse { offset: 22, kind: TrailingInput }
sqlite> SELECT name FROM sensors WHERE room = 'lab'
Error: Unsupported
sqlite> CREATE INDEX sensors_room ON sensors(room)
Error: Parse { offset: 0, kind: UnsupportedStatement }
```

DELETE is in `full` too. On tiny, keep a "deleted" flag column, or rebuild
the table.

### The bytes are a SQLite file

```rust
let stats = db.arena_stats();   // page_count=4 catalog_high_water=282B txn_high_water=80B
let image = db.storage().snapshot();
assert_eq!(&image[..16], b"SQLite format 3\0");   // database image: 16384 bytes
db.close().await?;

let mut db = Connection::open(MemStorage::from_bytes(image), Config::tiny()).await?;
// SELECT name FROM sensors WHERE rowid = 1  ->  north
```

Write `image` to a file and `sqlite3` opens it.

## Crash safety

[`examples/custom_storage.rs`](../../examples/custom_storage.rs) runs in tiny.
It implements `Storage` with a wrapper that cuts power after N writes, cuts
an INSERT's commit at every possible point, and reopens:

```bash
cargo run --example custom_storage --no-default-features
```

```text
the commit takes 12 mutating storage operations
cut at each of 12 points: 12 recovered to the old state, 0 found the commit complete, 0 torn
with 12 operations of power the commit completes: ids 1, 2, 3
```

## Sizing a tiny deployment

- **Code:** ≤ 128 KiB (`hipp_thumb_demo` measures the whole engine at about
  76 KiB).
- **RAM:** `Config::tiny()` is a 4-page pool (16 KiB at 4 KiB pages) plus
  16 / 8 / 16 KiB arena maxima. A smaller page size (`.page_size(1024)` for
  new files) shrinks the pool. Measure with `arena_stats()`.
- **Flash wear:** each commit writes the journal (original pages), syncs,
  writes the changed pages, syncs, and deletes the journal. Batch writes into
  transactions.

See the [usage guide §11](../GUIDE.md#11-embedded-no_std-targets) for the
bare-metal setup (allocator, storage, executor).
