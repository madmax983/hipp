# hipp

**It's Hipp to be `no_std`.**

hipp is a small database engine in Rust that **reads and writes SQLite
database files**. It is `no_std` + `alloc`, has zero dependencies, uses
bounded arenas instead of a general heap discipline, and is built to run
anywhere LLVM targets, microcontrollers included. Every build profile has a
hard code-size gate that CI enforces.

```text
your code ──► hipp (Rust API, async, no runtime) ──► Storage trait ──► flash / SD / file / RAM
                                                         │
                                       the bytes are a SQLite 3 database file
```

- **Files are real SQLite files.** A database hipp writes opens in `sqlite3`,
  and hipp opens files SQLite wrote. Pinned claim version: SQLite **3.54.1**.
  A differential test suite checks hipp's answers against a locally built
  3.54.1. `PRAGMA integrity_check` passes on hipp-written files, and SQLite
  recovers hipp's rollback journals (and its write-ahead logs).
- **Pick the size you can afford.** Four profiles, from a 76 KiB core
  (tables, inserts, scans, transactions) to the whole dialect: window
  functions, triggers, foreign keys, JSON, FTS5, R*Tree, WAL, ATTACH and
  SQLite's query planner.
- **Async without a runtime.** The storage trait and the API are `async fn`.
  You bring the executor (tokio, embassy, smol) or use the bundled `block_on`
  for storage that never blocks.
- **No panics on bad input or full memory.** Every failure is a named
  `hipp::Error` variant. Arena exhaustion is `Error::ArenaExhausted`, not an
  abort.
- **Not a drop-in for libsqlite3.** hipp is a Rust library with no
  `sqlite3_*` C ABI, and that will not change (see [Limits](#limits)).

Version: **0.16.0** released. 0.17 (the SQLite-parity backlog) is in
progress on `main`. See [`CHANGELOG.md`](CHANGELOG.md).

## Quick start

hipp is not on crates.io yet. Depend on it from git:

```toml
[dependencies]
# Default features: `std` (FileStorage, the system clock) + the `xl` profile.
hipp = { git = "https://github.com/madmax983/hipp" }

# Or a no_std profile, e.g. the core dialect for an embedded target:
# hipp = { git = "https://github.com/madmax983/hipp", default-features = false, features = ["full"] }
```

```rust
use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

fn main() {
    block_on(async {
        // Any `Storage` works: MemStorage (RAM), FileStorage (std), or your own.
        let mut db = Connection::open(MemStorage::new(), Config::default()).await?;

        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO t (name) VALUES ('ada'), ('grace')",
        ] {
            let mut st = db.prepare(sql).await?;
            while st.step().await? == Step::Row {}
        }

        let mut q = db.prepare("SELECT id, name FROM t").await?;
        while q.step().await? == Step::Row {
            let id = q.column_i64(0).unwrap_or_default();
            let name = q.column_bytes(1).map(String::from_utf8_lossy);
            println!("{id} {name:?}");
        }
        Ok::<(), hipp::Error<hipp::storage::MemError>>(())
    })
    .expect("query failed");
}
```

On disk, with `std`:

```rust,ignore
let storage = hipp::FileStorage::open("app.db").expect("open file");
let mut db = Connection::open(storage, Config::default()).await?;
// app.db is an ordinary SQLite file; app.db-journal is its rollback journal.
```

The [usage guide](docs/GUIDE.md) covers the rest: statements and columns,
parameters, transactions, errors, configuration and memory, custom
storage, async executors, embedded targets, and the CLI.

## Profiles

A profile is a set of cargo features. Each is size-gated on
`thumbv7em-none-eabihf` (`opt-level = "z"`, LTO), and CI fails a change that
grows a profile past its ratchet (`ci/size-ratchet.toml`; ADR 0003 / 0004).

| profile | cargo flags | code size (ratchet) | budget | what you get |
|---|---|---|---|---|
| [`tiny`](docs/profiles/tiny.md) | `default-features = false` | ≤ 77,508 B | 128 KiB | rowid tables, INSERT, full scans and rowid lookups, transactions, rollback journal and crash recovery |
| [`full`](docs/profiles/full.md) | `features = ["full"]` | ≤ 498,188 B | 512 KiB | + the core SQL dialect: indexes, expressions, joins, aggregates, UPDATE / DELETE, subqueries, upsert, views, CTEs, PRAGMA, savepoints, bound parameters, math functions |
| [`max`](docs/profiles/max.md) | `features = ["max"]` | ≤ 1,002,324 B | 1 MiB | + window functions, triggers, foreign keys, JSON / JSONB, expression and partial indexes, WITHOUT ROWID, UTF-16, WAL, date / time, ATTACH, auto-vacuum, virtual tables, R*Tree, FTS5, EXPLAIN QUERY PLAN |
| [`xl`](docs/profiles/xl.md) | `features = ["xl"]` (the default, with `std`) | ≤ 1,106,664 B | 1.25 MiB | + SQLite's cost-based join planner, automatic indexes, Bloom filters, MULTI-INDEX OR, skip-scans, the subquery flattener; FTS5 tombstones and secure-delete |

Every profile is `no_std`. Add `std` to any of them for `FileStorage`, the
system clock and time zone, and `VACUUM INTO` a file. Engine statics
(`.data` + `.bss`) are gated at 8 KiB in every profile; the tiny build
measures 0 B. The tiny profile also has a RAM gate: `Config::tiny()` runs
within 64 KiB of engine RAM (`tests/gates.rs`).

Each profile page lists every capability with SQL you can paste and the
exact output, taken from the profile's example program. CI runs each example
in its own profile and in every larger one, so the documented answers are
tested:

```bash
cargo run --example tiny --no-default-features
```

```bash
cargo run --example full --no-default-features --features full
```

```bash
cargo run --example max --no-default-features --features max
```

```bash
cargo run --example xl --no-default-features --features xl
```

You can also pick individual features instead of a profile (for example
`features = ["indexes", "params"]` on top of tiny). Each feature lists what
it implies in [`Cargo.toml`](Cargo.toml), and
[`ci/feature-sets.txt`](ci/feature-sets.txt) is the set CI builds.

**A smaller profile refuses, it never misreads.** SQL outside the build's
profile fails with a parse or `Unsupported` error. A file that needs a
missing feature (a WAL file without `wal`, a UTF-16 file without `utf16`, a
WITHOUT ROWID table without `without_rowid`) is refused at open with a named
error, and an auto-vacuum file without `autovacuum` opens read-only.

## The CLI

`hipp-cli` builds a `hipp` binary that behaves like the `sqlite3` shell: same
command line, output modes and dot-commands, with output that can be diffed
byte for byte against the real shell. Its main job is differential testing.

```bash
cargo run -p hipp-cli -- demo.db "CREATE TABLE t(a, b)" "INSERT INTO t VALUES (1, 'x')"
```

```bash
cargo run -p hipp-cli -- -box demo.db "SELECT * FROM t"
```

See [`hipp-cli/README.md`](hipp-cli/README.md).

## Limits

These are left out of every profile on purpose:

- **One process per database.** There is no multi-process locking. WAL mode
  runs as SQLite's `locking_mode=EXCLUSIVE` does: the wal-index lives in heap
  memory and no `-shm` file is written. Open each database from one
  connection.
- **No `sqlite3_*` C ABI, ever** (decision D3). hipp is not a drop-in
  replacement for libsqlite3.
- **No loadable extensions and no application-defined SQL functions.**
- **No plain `EXPLAIN` bytecode listing.** hipp executes a tree, not
  bytecode. `EXPLAIN QUERY PLAN` works (`max` and up) and prints SQLite's
  plan text. Its `id` / `parent` numbers are hipp's own.
- **No shared page cache.**

Known gaps in SQLite parity are tracked unit by unit in
[`STATUS.md`](STATUS.md) under "Still open".

## Documentation

| | |
|---|---|
| [Usage guide](docs/GUIDE.md) | How to use the library, start to finish |
| [Profiles](docs/profiles/README.md) | [`tiny`](docs/profiles/tiny.md), [`full`](docs/profiles/full.md), [`max`](docs/profiles/max.md), [`xl`](docs/profiles/xl.md): every capability, with tested examples |
| [`examples/`](examples) | Runnable programs: one per profile, plus custom storage, files, parameters |
| [CLI](hipp-cli/README.md) | The `hipp` shell |
| [Architecture](docs/ARCHITECTURE.md) | How the engine is put together, module by module |
| [Design spec](docs/SPEC.md), [risks](docs/RISKS.md), [ADRs](docs/adr) | Why it is built this way |
| [`CHANGELOG.md`](CHANGELOG.md), [history](docs/HISTORY.md), [`STATUS.md`](STATUS.md) | What changed, when, and the evidence |
| [Performance baselines](docs/perf-baselines.md) | Instruction-count baselines from `bolt_workload` |
| [Contributing](CONTRIBUTING.md) | Building, testing, gates, and the rules of the house |

API docs: `cargo doc --open`.

## Building and testing

The toolchain is pinned (`rust-toolchain.toml`, Rust 1.97.0) because the size
ratchets depend on the exact codegen.

```bash
cargo test --workspace
```

```bash
ci/run-examples.sh
```

```bash
ci/local-gates.sh --quick
```

The differential tests need the pinned SQLite 3.54.1 oracle. Build it with
`ci/build-oracle.sh` (it downloads and SHA3-verifies the amalgamation) or
point `HIPP_ORACLE` at one. [`CONTRIBUTING.md`](CONTRIBUTING.md) covers the
full gate list, the size ratchet and the feature matrix.

## License

Apache-2.0.
