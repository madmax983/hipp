# Profiles

A profile is a named set of cargo features with a code-size budget that CI
enforces. Each profile contains the one before it:

```text
tiny  ⊂  full  ⊂  max  ⊂  xl
```

| | [tiny](tiny.md) | [full](full.md) | [max](max.md) | [xl](xl.md) |
|---|---|---|---|---|
| cargo | `default-features = false` | `features = ["full"]` | `features = ["max"]` | `features = ["xl"]` (default, with `std`) |
| code budget (thumbv7em, `opt-level = "z"`, LTO) | 128 KiB | 512 KiB | 1 MiB | 1.25 MiB |
| ratchet (CI fails above it) | 77,508 B | 498,188 B | 1,002,324 B | 1,106,664 B |
| example | [`tiny.rs`](../../examples/tiny.rs) | [`full.rs`](../../examples/full.rs) | [`max.rs`](../../examples/max.rs) | [`xl.rs`](../../examples/xl.rs) |

The ratchet figures are the ceilings in
[`ci/size-ratchet.toml`](../../ci/size-ratchet.toml) at the time of writing.
That file is the source of truth. A change that shrinks a profile lowers its
ratchet, and one that grows it needs a reviewed justification. ADR 0003 and
ADR 0004 set the budgets.

Each page lists everything its profile adds, with SQL and the exact output
of its example program. CI runs every example in its own profile and in
every larger one (`ci/run-examples.sh`), so the answers shown here are
tested, and a larger profile is checked to answer the smaller profiles' SQL
identically.

## Capability matrix

| capability | feature | tiny | full | max | xl |
|---|---|:-:|:-:|:-:|:-:|
| Read and write SQLite 3 files, any page size 512–65536 | core | ✓ | ✓ | ✓ | ✓ |
| `CREATE TABLE` (rowid tables, INTEGER PRIMARY KEY, NOT NULL, IF NOT EXISTS) | core | ✓ | ✓ | ✓ | ✓ |
| `INSERT ... VALUES` (multi-row) | core | ✓ | ✓ | ✓ | ✓ |
| `SELECT` full scans, `WHERE rowid = literal` | core | ✓ | ✓ | ✓ | ✓ |
| `BEGIN` / `COMMIT` / `ROLLBACK`, rollback journal, crash recovery | core | ✓ | ✓ | ✓ | ✓ |
| Overflow pages, freelist reuse, bounded arenas | core | ✓ | ✓ | ✓ | ✓ |
| Bound parameters | `params` | (opt-in) | ✓ | ✓ | ✓ |
| Indexes, UNIQUE | `indexes` | (opt-in) | ✓ | ✓ | ✓ |
| Expressions, WHERE / ORDER BY / LIMIT, joins, aggregates, CHECK / DEFAULT / generated / STRICT | `query` | | ✓ | ✓ | ✓ |
| UPDATE, DELETE, DROP TABLE | `write` | | ✓ | ✓ | ✓ |
| Subqueries, upsert, RETURNING, compounds, views, CTEs, PRAGMA, savepoints, VACUUM, ALTER, TEMP, the function library | `sql` | | ✓ | ✓ | ✓ |
| Math functions | `math` | | ✓ | ✓ | ✓ |
| Window functions | `window` | | | ✓ | ✓ |
| Triggers | `triggers` | | | ✓ | ✓ |
| Foreign keys | `foreign_keys` | | | ✓ | ✓ |
| JSON / JSONB | `json` | | | ✓ | ✓ |
| Expression and partial indexes, index-aware planning | `expr_index` | | | ✓ | ✓ |
| WITHOUT ROWID tables | `without_rowid` | | | ✓ | ✓ |
| UTF-16 databases | `utf16` | | | ✓ | ✓ |
| Write-ahead logging | `wal` | | | ✓ | ✓ |
| Date and time functions | `datetime` | | | ✓ | ✓ |
| ATTACH / DETACH | `attach` | | | ✓ | ✓ |
| Auto-vacuum files | `autovacuum` | | | ✓ | ✓ |
| Virtual tables: generate_series, dbstat | `vtab` | | | ✓ | ✓ |
| R*Tree | `rtree` | | | ✓ | ✓ |
| FTS5 full-text search | `fts5` | | | ✓ | ✓ |
| EXPLAIN QUERY PLAN | `explain` | | | ✓ | ✓ |
| Cost-based join order, automatic indexes, Bloom filters, MULTI-INDEX OR, skip-scan, flattener | `planner` | | | | ✓ |
| FTS5 contentless-delete tombstones, secure-delete | `fts5_delete` | | | | ✓ |
| `FileStorage`, system clock and zone, VACUUM INTO a file | `std` | add to any profile | | | |

"(opt-in)" means the feature works on top of tiny without the rest of
`full`: `features = ["params"]` or `["indexes"]`.

## What a smaller build does with SQL it lacks

It refuses, and never guesses:

- SQL outside the build's grammar is `Error::Parse { kind: UnsupportedStatement | UnexpectedToken | TrailingInput, .. }`.
- A statement it parses but cannot execute is `Error::Unsupported`.
- A file that needs a missing leaf is refused at open: a WAL file
  (`WalUnsupported`), a UTF-16 file (`UnsupportedEncoding`), a file holding a
  WITHOUT ROWID table or a virtual table. An auto-vacuum file opens, but
  writes fail with `AutoVacuumUnsupported`.
- A write that would leave a structure stale is refused: an index the build
  cannot maintain (an expression index without `expr_index`), a table with
  triggers without `triggers`, or a table in a foreign key while
  `PRAGMA foreign_keys` is ON without `foreign_keys`.

## Choosing

- **Logging sensor data or a key-value store on a microcontroller:** tiny, or
  tiny + `params` + `indexes`.
- **An embedded app that needs real queries:** full.
- **Files coming from or going to desktop SQLite, which may use any feature
  (WAL, JSON, FTS5, ...):** max, the smallest profile that opens every file
  format.
- **A host, or anywhere 1.25 MiB of code is cheap:** xl. The planner only
  changes speed, not results, so a database built and tested under xl gives
  the same answers under max.
