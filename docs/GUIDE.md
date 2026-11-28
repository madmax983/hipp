# hipp usage guide

This guide covers hipp as a library, from adding the dependency to running
on a microcontroller. The [profile pages](profiles/README.md) list every
SQL capability per build. This page is about the Rust side.

Contents:

1. [Add the dependency and choose a profile](#1-add-the-dependency-and-choose-a-profile)
2. [The model: one connection, prepared statements, steps](#2-the-model-one-connection-prepared-statements-steps)
3. [Opening a database: storage](#3-opening-a-database-storage)
4. [Running statements and reading rows](#4-running-statements-and-reading-rows)
5. [Bound parameters](#5-bound-parameters)
6. [Transactions, durability and crash recovery](#6-transactions-durability-and-crash-recovery)
7. [Errors](#7-errors)
8. [Configuration and memory](#8-configuration-and-memory)
9. [Async: executors, cancellation, threads](#9-async-executors-cancellation-threads)
10. [Implementing `Storage`](#10-implementing-storage)
11. [Embedded (`no_std`) targets](#11-embedded-no_std-targets)
12. [Clocks, time zones, VACUUM INTO, ATTACH](#12-clocks-time-zones-vacuum-into-attach)
13. [Introspection](#13-introspection)
14. [Interoperating with SQLite](#14-interoperating-with-sqlite)
15. [The CLI](#15-the-cli)
16. [Recipes](#16-recipes)

Every code fragment here comes from a program in [`examples/`](../examples)
that CI runs, or is a shortened form of one.

---

## 1. Add the dependency and choose a profile

```toml
[dependencies]
hipp = { git = "https://github.com/madmax983/hipp" }   # std + xl (everything)
```

The default features are `std` (host file storage, system clock) and `xl`
(the largest profile). On a constrained target, turn the defaults off and
choose a profile:

| you need | features | code size on thumbv7em |
|---|---|---|
| store and scan records, rowid lookups, transactions | *(none)*: the **tiny** profile | ≤ 128 KiB |
| real SQL: WHERE, joins, aggregates, indexes, UPDATE / DELETE, views, parameters | `["full"]` | ≤ 512 KiB |
| window functions, triggers, foreign keys, JSON, FTS5, WAL, date / time, ... | `["max"]` | ≤ 1 MiB |
| SQLite's join planner and other optimisations | `["xl"]` | ≤ 1.25 MiB |

```toml
hipp = { git = "https://github.com/madmax983/hipp", default-features = false, features = ["full"] }
```

Add `"std"` to any profile on a hosted target to get `FileStorage`.
Individual features (`indexes`, `params`, `json`, `wal`, ...) can be mixed
too. Each feature's comment in [`Cargo.toml`](../Cargo.toml) says what it
adds and what it implies.

A profile also decides what files you can open. A build without `wal`
refuses a WAL-mode file (`Error::WalUnsupported`), one without `utf16`
refuses a UTF-16 file (`Error::UnsupportedEncoding`), and so on. hipp never
reads a file it might misread. If you open files produced elsewhere, pick a
profile that covers them (`max` covers every file format feature).

## 2. The model: one connection, prepared statements, steps

The API mirrors SQLite's C API in Rust form:

| SQLite C | hipp |
|---|---|
| `sqlite3_open_v2` | `Connection::open(storage, config).await` |
| `sqlite3_prepare_v2` | `connection.prepare(sql).await`, one statement per call |
| `sqlite3_step` | `statement.step().await` → `Step::Row` or `Step::Done` |
| `sqlite3_column_*` | `statement.column_value(i)`, `column_i64`, `column_f64`, `column_bytes`, `column_is_null` |
| `sqlite3_bind_*`, `sqlite3_reset` | `bind_i64`, `bind_text`, ..., `reset()` (feature `params`) |
| `sqlite3_finalize` | drop the `Statement` |
| `sqlite3_close` | `connection.close().await` (or drop it) |
| `sqlite3_last_insert_rowid`, `sqlite3_changes` | `last_insert_rowid()`, `changes()`, `total_changes()` (feature `sql`) |

Rules that follow from it:

- A `Statement` borrows its `Connection` mutably, so **one statement at a
  time per connection**. Drop (or finish) a statement before preparing the
  next. The borrow checker enforces this.
- `prepare` takes **one** SQL statement. A trailing `;` is fine, but a
  second statement is a parse error (`TrailingInput`). To run a script,
  split it yourself and prepare each statement in turn.
- Everything that can touch storage is `async`. With in-memory or always-ready
  storage, `hipp::storage::block_on` drives it synchronously.

## 3. Opening a database: storage

`Connection::open` takes any value implementing `hipp::Storage`, and creates
the database if the storage is empty. If a hot rollback journal (a commit cut
off by a crash) is present, `open` recovers it before returning.

### `MemStorage`: RAM

```rust
use hipp::{Config, Connection, MemStorage};

let mut db = Connection::open(MemStorage::new(), Config::default()).await?;
```

`MemStorage` models a file (with a journal sidecar) in memory. Clones
**share** the same bytes, so you can keep a handle while the connection owns
another:

```rust
let medium = MemStorage::new();
let mut db = Connection::open(medium.clone(), Config::tiny()).await?;
// ... write ...
let image: Vec<u8> = medium.snapshot();      // a complete SQLite file image
assert_eq!(&image[..16], b"SQLite format 3\0");
let again = MemStorage::from_bytes(image);    // open those bytes later
```

`MemStorage::memory_db()` (feature `sql`) is SQLite's `:memory:`: its
`journal_mode` is `memory` and it never enters WAL mode. `MemStorage::new()`
behaves like a file, which is what you want for tests and for persisting
images yourself.

### `FileStorage`: files (feature `std`)

```rust
use hipp::FileStorage;

let storage = FileStorage::open("app.db").expect("open");  // creates it if missing
let mut db = Connection::open(storage, Config::default()).await?;
```

Sidecars are named as SQLite names them: `app.db-journal`, and with `wal`
`app.db-wal`. `FileStorage::anonymous(memory)` is a private temporary file.
See [`examples/file_db.rs`](../examples/file_db.rs).

### Your own

On a device, implement `Storage` over whatever you have: a flash partition,
an SD card, a block device, a network store. See
[§10](#10-implementing-storage).

## 4. Running statements and reading rows

```rust
use hipp::{Step, Value};

let mut st = db.prepare("SELECT id, name, score FROM players WHERE rowid = 10").await?;
while st.step().await? == Step::Row {
    let id: Option<i64> = st.column_i64(0);
    let name: Option<&[u8]> = st.column_bytes(1);   // TEXT is UTF-8 bytes
    match st.column_value(2) {
        Some(Value::Real(f)) => println!("{f}"),
        Some(Value::Integer(i)) => println!("{i}"),
        Some(Value::Null) | None => println!("NULL"),
        Some(Value::Text(t) | Value::Blob(t)) => println!("{} bytes", t.len()),
    }
}
drop(st); // release the connection for the next statement
```

- `step()` returns `Step::Row` while rows remain, then `Step::Done`. Statements
  that return no rows (DDL, INSERT without RETURNING, BEGIN, ...) return
  `Step::Done` from their first step, and that step does the work.
- Column accessors read the current row. Values borrowed from it
  (`column_bytes`, `Value::Text`) are valid until the next `step`. Copy them
  (`to_vec()`, `OwnedValue`) to keep them.
- `column_i64` / `column_f64` / `column_bytes` return `None` when the value is
  not of that class. Use `column_value` to see the storage class.
  `column_is_null` tests for NULL.
- `column_count()` gives the number of result columns. hipp has no column-name
  API. Name columns in your SQL and index them by position.
- `rowid()` is the rowid of the row a simple table scan is on, when there is
  one.

A small helper keeps call sites short. The examples use this one
([`examples/common/mod.rs`](../examples/common/mod.rs)):

```rust
async fn rows<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Vec<String>
where S::Error: core::fmt::Debug {
    let mut st = db.prepare(sql).await.expect("prepare");
    let mut out = Vec::new();
    while st.step().await.expect("step") == Step::Row {
        let cells: Vec<String> = (0..st.column_count()).map(|i| render(st.column_value(i))).collect();
        out.push(cells.join("|"));
    }
    out
}
```

### Change counts and rowids (feature `sql`)

```rust
db.prepare("UPDATE emp SET salary = salary + 5 WHERE dept = 2").await?.step().await?;
assert_eq!(db.changes(), 2);
let id = db.last_insert_rowid();       // after an INSERT
let all = db.total_changes();          // since open
```

`INSERT ... RETURNING` / `UPDATE ... RETURNING` / `DELETE ... RETURNING`
return rows through `step` like a SELECT.

## 5. Bound parameters

Feature `params` (in `full` and up; it also works on top of tiny). Prepare
once, then bind, step and reset for each execution:

```rust
let mut ins = db.prepare("INSERT INTO readings (sensor, v) VALUES (:sensor, :v)").await?;
let sensor = ins.parameter_index(":sensor").expect("named");
let v = ins.parameter_index(":v").expect("named");
for (name, value) in [("north", 21.5), ("south", 19.25)] {
    ins.bind_text(sensor, name)?;
    ins.bind_f64(v, value)?;
    ins.step().await?;
    ins.reset();               // ready to run again; bindings are kept
}
```

- Placeholders: `?`, `?NNN`, `:name`, `@name`, `$name`, numbered as SQLite
  numbers them. Indexes start at 1.
- `bind_null`, `bind_i64`, `bind_f64`, `bind_text`, `bind_blob`, and
  `bind_value(i, hipp::Value)`.
- `parameter_count()`, `parameter_index(name)`, `parameter_name(i)`.
- An unbound parameter is NULL. `clear_bindings()` sets every parameter back
  to NULL.
- Binding to a statement that has stepped and not been reset is
  `Error::ResetRequired`. A bad index is `Error::BindIndexOutOfRange`.
- In the tiny grammar (no `query`), parameters go wherever a literal goes:
  INSERT VALUES and `WHERE rowid = ?`. With `query` they can be any
  expression.

Use parameters for any value that comes from outside. It avoids quoting bugs
and SQL injection, it skips re-parsing, and a statement-arena-sized literal
cannot appear in the SQL text.

See [`examples/params.rs`](../examples/params.rs) and the `params` section of
[`examples/full.rs`](../examples/full.rs).

## 6. Transactions, durability and crash recovery

Each statement outside `BEGIN ... COMMIT` is its own transaction. Group
writes in an explicit transaction for atomicity and speed (one journal, one
sync):

```rust
for sql in ["BEGIN", "INSERT ...", "INSERT ...", "COMMIT"] {
    db.prepare(sql).await?.step().await?;
}
```

- `BEGIN [DEFERRED | IMMEDIATE | EXCLUSIVE]`, `COMMIT` / `END`, `ROLLBACK`.
- `SAVEPOINT name`, `RELEASE name`, `ROLLBACK TO name` (feature `sql`).
- A failed statement inside a transaction undoes only its own changes. The
  transaction stays open.
- `db.in_explicit_transaction()` reports a `BEGIN` bracket.
  `db.in_transaction()` reports whether a write transaction is open, including
  one left behind by a dropped future.
- `db.rollback().await` rolls back whatever is open.
- `db.close().await` rolls back an open transaction, and in WAL mode
  checkpoints and removes the `-wal`. Dropping a connection without `close` is
  safe: committed data is durable, and anything uncommitted is rolled back by
  the next open.

**Durability.** hipp uses SQLite's rollback journal, byte-compatible with
SQLite's: original pages go to `<db>-journal` and are synced before the
database file is touched. The journal's deletion is the commit point. If
power fails at any instant, the next `Connection::open` finds the hot journal
and rolls the database back to the last commit, and SQLite would do the same
with that file. [`examples/custom_storage.rs`](../examples/custom_storage.rs)
cuts power at every write of a commit and checks that each reopen finds
either the old state or the new one, never a torn file:

```text
the commit takes 12 mutating storage operations
cut at each of 12 points: 12 recovered to the old state, 0 found the commit complete, 0 torn
with 12 operations of power the commit completes: ids 1, 2, 3
```

With the `wal` feature, `PRAGMA journal_mode = WAL` switches to write-ahead
logging, in SQLite's file format (see the [max profile](profiles/max.md#write-ahead-logging-wal)).

## 7. Errors

Every fallible call returns `Result<T, hipp::Error<E>>`, where `E` is your
storage's error type. The variants name the problem and its remedy, and
carry no strings (so they are cheap on `no_std`). The ones you will meet
most:

| variant | meaning |
|---|---|
| `Parse { offset, kind }` | SQL did not parse, or uses grammar this build lacks (`kind: UnsupportedStatement` etc.). `offset` is a byte offset into the SQL. |
| `Schema { kind }` | A schema or constraint error: `UnknownTable`, `UnknownColumn`, `UniqueViolation`, `NotNullViolation`, `CheckViolation`, `ForeignKeyViolation`, `DuplicateRowid`, ... (`hipp::SchemaKind`) |
| `Unsupported` | A statement shape this build cannot execute (for example a non-rowid WHERE in tiny). |
| `ArenaExhausted { scope, needed }` | A statement, transaction or catalog arena hit its maximum. Raise it in `Config`. See [§8](#8-configuration-and-memory). |
| `CacheExhausted` | Every page-cache frame is pinned. Raise `Config::cache_pages`. |
| `TransactionState { nested }` | `BEGIN` inside a transaction (`nested: true`), or `COMMIT` / `ROLLBACK` with none open. |
| `OperationInFlight` | A commit was cut off by a dropped future. Call `resume_txn` or `rollback` first. See [§9](#9-async-executors-cancellation-threads). |
| `BadMagic`, `TruncatedHeader`, `CorruptPage { page }`, `CorruptRecord`, `CorruptJournal`, ... | The file is not a SQLite database, or it is damaged. |
| `WalUnsupported`, `UnsupportedEncoding`, `AutoVacuumUnsupported` | The file needs a feature this build does not have. |
| `NoClock` | A statement needs the time and no `Config::clock` is set. |
| `Storage(E)` | Your storage returned an error. It is passed through untouched. |

The error types implement `Debug` (not `Display`), so match on them or log
them with `{:?}`:

```rust
match db.prepare("INSERT INTO dept (name) VALUES ('eng')").await?.step().await {
    Err(hipp::Error::Schema { kind: hipp::SchemaKind::UniqueViolation }) => { /* already there */ }
    Err(e) => return Err(e),
    Ok(_) => {}
}
```

`Error::generalize` converts `Error<E>` to `Error<F>` when `E: Into<F>`,
which helps when you combine several storage types.

## 8. Configuration and memory

`Config` sets the page cache and the three arena scopes. There are two
starting points:

| | `Config::default()` (hosted) | `Config::tiny()` (embedded reference) |
|---|---|---|
| page cache (`cache_pages`) | 16 pages | 4 pages |
| statement arena (init / max) | 16 / 64 KiB | 4 / 16 KiB |
| transaction arena | 8 / 32 KiB | 2 / 8 KiB |
| catalog arena (the schema) | 16 / 256 KiB | 4 / 16 KiB |
| page size for new files | 4096 | 4096 |
| clock / time zone / VACUUM INTO writer | system ones under `std`, else none | none |
| expression / parser / view nesting limits | SQLite's defaults, reduced to fit a 2 MiB thread | small (64 / 64 / 16) |

With 4 KiB pages, `Config::tiny()` uses 56 KiB of pool and arenas, inside the
64 KiB engine-RAM gate.

Builder methods adjust either one:

```rust
let cfg = Config::default()
    .cache_pages(64)                    // more pages cached = fewer reads
    .statement_arena(32 * 1024, 1 << 20)
    .txn_arena(16 * 1024, 256 * 1024)
    .catalog_arena(16 * 1024, 1 << 20)  // large schemas
    .page_size(8192)                    // only applies when creating a file
    .nonce_seed(hardware_rng());        // seed journal nonces from real entropy
```

How memory works:

- **Arenas are scoped bump allocators.** The statement arena is reset per
  statement, the transaction arena per transaction, and the catalog arena when
  the schema reloads. An arena starts at `init` and grows in chunks up to
  `max`. Past `max` the operation fails with `ArenaExhausted { scope, needed }`.
  It does not abort, and the connection stays usable.
- **What fills the statement arena:** the parsed SQL (its literals
  included), the plan, and per-row working values. A 20 KB string literal
  does not fit `Config::tiny()`'s 16 KiB statement arena (the tiny example
  shows the error). Bind large values as parameters, or raise the maximum.
- **The page pool** holds `cache_pages` pages. A statement that needs more
  pages pinned at once than the pool holds fails with `CacheExhausted`.
- **Watching usage:** `db.arena_stats()` returns catalog / transaction high
  water marks, the pool size and the database's page count.
  `statement.arena_high_water()` gives the high-water mark of one statement.
  Size your `max` values from these, measured on your real workload.
- **Stack.** Parsing, planning and evaluation recurse, bounded by
  `max_expr_depth`, `max_parser_depth`, `max_nesting` (views / CTEs) and
  `max_trigger_depth`. The defaults fit a 2 MiB thread in a release build.
  `Config::stack_budget(bytes)` derives limits from the stack you have.
  `depth_limits(expr, parser)` sets them directly.
- `max_vtab_rows` (feature `vtab`) caps how many virtual-table rows a
  statement may hold in memory at once (so `generate_series(1)` cannot
  exhaust the heap).

## 9. Async: executors, cancellation, threads

hipp ships no executor and spawns nothing. Its futures advance state machines
stored in the `Connection` and `Statement`, so they can be driven by anything:

- **Always-ready storage** (RAM, a memory-mapped flash, a blocking file):
  `hipp::storage::block_on(fut)`, a no-op-waker poll loop. This is what the
  examples use, and `FileStorage` is built for it.
- **tokio / smol / async-std:** `.await` hipp's futures from a task. The
  futures and `Connection` are **not `Send`** (the engine uses `Rc`
  internally), so keep a connection on one thread: a current-thread runtime, a
  `LocalSet`, or a dedicated thread that owns the connection and receives
  work over a channel.
- **embassy / firmware loops:** await from a task, with your `Storage`
  returning `Pending` while a DMA transfer runs.

**Cancellation is safe.** You can drop any hipp future at an await point:

| dropped future | effect |
|---|---|
| `open` | Nothing was changed. Call `open` again (it recovers any hot journal). |
| `prepare` | Prepare again. |
| `step` on a read | The statement's cursor is kept, and the next `step` resumes without skipping a row. |
| a write, before its commit | The transaction stays open on the connection. Continue it or roll it back. |
| a write, mid-commit | The commit machine is stored in the connection, and every stage is idempotent. `prepare` returns `OperationInFlight` until you call `db.resume_txn().await` (finishes the commit) or `db.rollback().await`. If you never do, the next `open` recovers from the journal. The file is never torn. |

Implementations of `Storage` must tolerate cancellation at any await point: a
write that was cancelled may or may not have landed, which is exactly what
the journal absorbs.

## 10. Implementing `Storage`

```rust
#[allow(async_fn_in_trait)]
pub trait Storage {
    type Error;
    type Sidecar: SidecarFile<Error = Self::Error>;
    async fn len(&self) -> Result<u64, Self::Error>;
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error>;
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    async fn sync(&mut self) -> Result<(), Self::Error>;
    async fn sidecar(&mut self, kind: Sidecar) -> Result<Self::Sidecar, Self::Error>;
    // ...plus methods with defaults: file_name, log_beside, is_memory,
    // open_attached, open_named, claim_wal / release_wal
}

pub trait SidecarFile {   // the -journal (and -wal / -shm with `wal`)
    type Error;
    fn existed(&self) -> bool;                    // did it exist before this open?
    async fn len(&self) -> Result<u64, Self::Error>;
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error>;
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    async fn sync(&mut self) -> Result<(), Self::Error>;
    async fn delete(&mut self) -> Result<(), Self::Error>;   // deleting twice is Ok
}
```

The contract:

- **Byte-addressed.** hipp reads and writes whole pages at page-aligned
  offsets, plus the 100-byte header. The trait itself knows nothing about
  pages.
- `read_at` must fill the whole buffer. `set_len` grows (new pages arrive this
  way) or truncates.
- **`sync` is a durability barrier.** When it returns, everything written
  before it must survive power loss. The journal protocol's safety rests on
  this. On flash it means "programmed", not "queued".
- `sidecar(kind)` opens, creating if needed, the database's journal (and with
  `wal`, its log). `existed()` must say whether the sidecar was there before:
  recovery uses it to tell a hot journal from a fresh one. `delete` must be
  idempotent.
- `Error` is whatever you like. It comes back as `hipp::Error::Storage(e)`.

[`examples/custom_storage.rs`](../examples/custom_storage.rs) is a complete,
working implementation: a wrapper over `MemStorage` that simulates power loss
after N writes. Use it as a template. A flash implementation usually maps
the main file and the journal to two regions, keeps each one's length in a
small header, and makes `sync` wait for program completion.

Optional methods worth knowing:

- `file_name()`: what `PRAGMA database_list` shows.
- `open_attached(target)` (feature `attach`): how `ATTACH 'name'` finds
  storage when no hook is set. See also [§12](#12-clocks-time-zones-vacuum-into-attach).
- `open_named(name)`: a named sidecar, used for the super-journal of a
  multi-database commit.
- `claim_wal` / `release_wal` (feature `wal`): refuse WAL mode when another
  process holds the database.

## 11. Embedded (`no_std`) targets

hipp needs `core` + `alloc`: a global allocator and nothing else. No `libm`,
no `std`, no C library.

1. Depend on hipp with `default-features = false` and a profile.
2. Provide a `#[global_allocator]` over the RAM you dedicate to the engine.
   Budget roughly `cache_pages × page_size` plus the arena maxima (56 KiB for
   `Config::tiny()`), plus your own row copies.
3. Implement `Storage` over your medium ([§10](#10-implementing-storage)).
4. Start from `Config::tiny()`. Measure with `arena_stats()` and raise limits
   as needed.
5. Seed `Config::nonce_seed` from a hardware RNG if you have one (journal
   nonces). Supply `Config::clock` if your SQL uses the time
   ([§12](#12-clocks-time-zones-vacuum-into-attach)).
6. Drive futures with your executor, or `hipp::storage::block_on` if your
   storage completes synchronously.

[`src/bin/hipp_thumb_demo.rs`](../src/bin/hipp_thumb_demo.rs) is a complete
bare-metal program for `thumbv7em-none-eabihf` (a bump allocator over a
static heap, a panic handler, create / insert / select over `MemStorage`).
CI builds it in every profile to measure the size gates:

```bash
cargo build --profile tiny --target thumbv7em-none-eabihf --no-default-features --bin hipp_thumb_demo
```

```bash
python3 ci/size_gate.py --section tiny target/thumbv7em-none-eabihf/tiny/hipp_thumb_demo
```

CI also builds the library for `thumbv6m-none-eabi`, `riscv32imac-unknown-none-elf`
and `wasm32-unknown-unknown` in every profile.

## 12. Clocks, time zones, VACUUM INTO, ATTACH

The core has no clock, no time zone database and no file system. Each is a
hook in `Config` (or on the connection) that `std` fills in for you:

```rust
fn now_ms() -> i64 { rtc_unix_millis() }                    // Unix time in ms
fn zone(_unix_secs: i64) -> Option<i64> { Some(2 * 3600) }  // offset east of UTC, seconds

let cfg = Config::default()
    .clock(now_ms)          // DEFAULT CURRENT_TIMESTAMP, 'now', CURRENT_* (feature query/datetime)
    .tz_offset(Some(zone)); // 'localtime' / 'utc' modifiers (feature datetime)
```

- Without a clock, a statement that needs the time fails with
  `Error::NoClock`. `Config::no_clock()` removes the `std` default (useful for
  reproducible tests).
- `Config::vacuum_into(writer)` (feature `sql`) is called with the target
  name and the finished image when `VACUUM INTO 'name'` runs. Under `std` the
  default writes a new file and refuses one that already holds data.
- **ATTACH** (feature `attach`): `db.set_attach_hook(|target| ...)` maps the
  name in `ATTACH 'name' AS x` (`AttachTarget::File(name)`, `Memory`,
  `Temporary`) to storage of the main database's type, or returns `None` for
  "unable to open database". Without a hook, `Storage::open_attached` is
  asked (`FileStorage` opens files, `MemStorage` creates in-memory ones). The
  [max example](profiles/max.md#attach--detach-attach) maps names to shared
  `MemStorage` values.

## 13. Introspection

From Rust, without SQL:

```rust
for t in db.tables().iter()  { /* t.name, t.sql, t.columns, t.generated */ }
for i in db.indexes().iter() { /* i.name, i.table, i.sql, i.unique */ }
for v in db.views().iter()   { /* v.name, v.sql, v.columns */ }
let stats = db.arena_stats();          // page_count, pool_bytes, high-water marks
let temp = db.temp_db();               // the TEMP database, once created (feature sql)
```

From SQL (feature `sql`): `sqlite_schema`, `PRAGMA table_info / index_list /
index_info / index_xinfo / foreign_key_list / database_list / ...` and their
table-valued forms (`SELECT * FROM pragma_table_info('t')`),
`PRAGMA integrity_check`, and with `vtab` the `dbstat` table.

## 14. Interoperating with SQLite

- A database hipp writes is an ordinary SQLite 3 file. `sqlite3 app.db`
  opens it, and so do Python's `sqlite3` module, DB Browser and other tools.
  `PRAGMA integrity_check` there returns `ok`.
- hipp opens files SQLite writes, as far as the build's features cover the
  file's format ([§1](#1-add-the-dependency-and-choose-a-profile)).
- A rollback journal hipp leaves behind is recovered by SQLite, and SQLite's
  is recovered by hipp. With `wal`, the same holds for the `-wal` log, as long
  as only one process has the database open.
- SQL behaves as on SQLite 3.54.1: the same answers, the same type affinity
  and collation rules, the same row order where SQLite defines one. Error
  values are hipp's own enum rather than SQLite's messages. The CLI prints
  SQLite's messages.

## 15. The CLI

`hipp-cli` builds `hipp`, a shell compatible with `sqlite3`:

```bash
cargo run -p hipp-cli -- app.db
```

See [`hipp-cli/README.md`](../hipp-cli/README.md) for options, modes and
dot-commands.

## 16. Recipes

**Insert many rows fast.** One transaction and one prepared statement:

```rust
db.prepare("BEGIN").await?.step().await?;
{
    let mut ins = db.prepare("INSERT INTO t (a, b) VALUES (?, ?)").await?;
    for (a, b) in data {
        ins.bind_i64(1, a)?;
        ins.bind_text(2, b)?;
        ins.step().await?;
        ins.reset();
    }
}
db.prepare("COMMIT").await?.step().await?;
```

**Upsert a counter** (feature `sql`):

```sql
INSERT INTO kv (k, v) VALUES ('a', 5)
  ON CONFLICT (k) DO UPDATE SET hits = hits + 1, v = excluded.v;
```

**Ship a database image from a device.** Keep a `MemStorage` handle, call
`snapshot()` after a commit, and send the bytes. On the host they are an
ordinary `.db` file.

**Key-value store on tiny.** `CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)`
gives you O(log n) get (`WHERE rowid = ?` with `params`), put
(`INSERT`; replacing needs `full`'s `INSERT OR REPLACE`) and ordered scans,
with transactions and crash safety, in under 128 KiB of code.

**Full-text search on a device.** `max` includes FTS5 with SQLite's file
format, so an index built on the device can be queried on a host, and the
reverse.

**Read-mostly reporting.** Use `xl` (the default) on a host: the join planner
picks join orders and builds automatic indexes as SQLite does. Run `ANALYZE`
after loading data so the planner has statistics.
