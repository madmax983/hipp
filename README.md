# hipp

**It's Hipp to be `no_std`.**

hipp is a little database engine in Rust that **reads and writes
SQLite database files**. It gives SQLite the treatment Horton got:
`no_std` + `alloc`, arena-first, built to run anywhere LLVM targets
— microcontrollers included — with hard size and RAM gates, not
aspirations.

- **Phase:** v0.6 — subqueries in expressions (scalar,
  `IN (SELECT ...)`, `EXISTS`) across SELECT / UPDATE /
  DELETE, on the v0.5 foundation (SPEC §2).
- **Pinned claim version:** SQLite **3.54.1** (decision D1). The
  differential suite in `tests/` runs against a locally built
  3.54.1 oracle; `PRAGMA integrity_check` passes on hipp-written
  files, and real SQLite recovers hipp's rollback journals.
- **Profile measured:** `tiny` profile (`opt-level = "z"`, LTO) on
  `thumbv7em-none-eabihf`.

## What v0.6 does

- Everything v0.5 does (below), plus subqueries in
  expressions behind the `v06` feature (default on, implies
  `v05`; the tiny profile excludes it): scalar subqueries,
  `x [NOT] IN (SELECT ...)`, and `EXISTS (SELECT ...)` in
  SELECT lists, WHERE, ON, GROUP BY, HAVING, ORDER BY,
  UPDATE SET/WHERE, and DELETE WHERE. Uncorrelated and
  correlated forms both run: name resolution shadows outward
  through nested scopes exactly like SQLite (inner sources
  first, then enclosing queries nearest-first), a scalar
  subquery yields its first row's first column (NULL on no
  rows), and a subquery yielding the wrong column count is a
  prepare-time error. Named boundaries: a subquery inside an
  aggregate's arguments and subqueries in INSERT VALUES are
  refused at prepare time; FROM-clause derived tables and
  UNION remain outside the grammar.

## What v0.5 does

- Everything v0.4 does (below), plus the write side behind the
  `v05` feature (default on; the tiny profile excludes it):
  `UPDATE` (with `OR IGNORE`, table aliases, and general
  expressions that read the original row), `DELETE`, and
  `DROP TABLE`, all with full index maintenance — updated and
  deleted rows move through every index (including UNIQUE
  autoindexes), dropped tables return every page to the
  freelist, and rowid reassignment follows the oracle's rules.
  Statement atomicity is preserved; name-kind refusals
  (DROP TABLE on a view, DROP VIEW on a table) match the
  oracle even under `IF EXISTS`.
- The pager now spills dirty frames to the main file
  mid-transaction (SPEC §5.2), so wide write statements no
  longer exhaust the page cache.

## What v0.4 does

- Everything v0.3 does (below), plus the v0.4 dialect behind the
  `v04` feature (default on; the tiny profile excludes it):
  nested-loop joins (`JOIN` / `LEFT JOIN` / `CROSS JOIN` /
  comma joins, `ON` expressions, aliases, self-joins),
  aggregates (`count`/`sum`/`avg`/`total`/`min`/`max`/
  `group_concat`, DISTINCT forms) with `GROUP BY` and `HAVING`,
  `ORDER BY` / `LIMIT` / `OFFSET` / `SELECT DISTINCT`, and a
  general expression evaluator — arithmetic with SQLite's
  coercion rules, comparisons with affinity adjustment,
  three-valued logic, `IN`/`BETWEEN`/`LIKE`/`CASE`/`CAST`, and
  a scalar function library (`abs`, `length`, `upper`, `lower`,
  `coalesce`, `nullif`, `typeof`, `substr`, `round`, `hex`,
  `trim`, `replace`, `instr`, `sign`, `iif`, …). `INSERT`
  values are full constant expressions. Simple single-table
  queries keep the zero-materialisation fast paths.
- `ALTER TABLE ... ADD COLUMN`, with the oracle's rewrite of
  the stored CREATE TABLE text and its refusal rules (no
  PRIMARY KEY / UNIQUE / generated columns, no NOT NULL
  without a constant default).
- Views as stored SQL (inert): `CREATE VIEW` validates its
  SELECT and stores it verbatim; views are catalogued, listed
  by the CLI, and never executed — selecting from one is a
  named error, not a silent wrong answer.

## What v0.3 does

- `Connection::open` over any async `Storage` (hipp's own
  runtime-agnostic trait: `async fn`, no executor, no tokio, no
  dependencies — the caller brings the runtime: tokio, embassy,
  smol, or a trivial `block_on`).
- `CREATE TABLE` (rowid tables, `INTEGER PRIMARY KEY` alias,
  `IF NOT EXISTS`), multi-row `INSERT ... VALUES (...), (...)`,
  full-scan `SELECT` in rowid order plus `WHERE rowid = <literal>`
  point lookups, streamed via `step()`.
- Index B-trees (behind the `indexes` feature, default on):
  `CREATE [UNIQUE] INDEX`, `DROP INDEX`, UNIQUE enforcement for
  explicit indexes and for column/table constraints via implicit
  `sqlite_autoindex_*` indexes, and `WHERE <first indexed
  column> = <literal>` lookups through the planner's access
  seam. Expression, partial and non-BINARY-collated foreign
  indexes are read as opaque metadata; writes to their tables
  are refused rather than allowed to desynchronise them.
- Explicit transactions: `BEGIN` / `COMMIT` / `ROLLBACK` over the
  resumable commit machine.
- Table B-trees with leaf/interior/root splits, overflow chains
  (10 KiB+ values), freelist reuse, rollback-journal transactions
  byte-compatible with SQLite's format — including hot-journal
  recovery on open, and resumable commits: a future dropped
  mid-commit is finished by `resume_txn()` or recovered by the next
  open, never torn.
- Three arena scopes (statement / transaction / catalog) with
  caller-configured maxima; exhaustion is
  `Error::ArenaExhausted`, never a panic.

## Measured gates (SPEC §4.1, tiny profile)

| Gate | Budget | Measured (v0.6) |
|---|---|---|
| Linked `.text` + `.rodata` (thumbv7em) | ≤ 128 KiB | **88,984 B** (Δ +48 B vs v0.5's 88,936 B; the v0.6 engine is feature-gated out of tiny — the delta is the ungated AST gaining the three subquery `Expr` variants) |
| Engine statics (`.data` + `.bss`, excl. caller heap) | ≤ 8 KiB | **0 B** |
| Engine RAM, tiny config (pool + arenas) | ≤ 64 KiB | within (see `tests/gates.rs`) |
| Steady-state scan allocations after warm-up | 0 | **0** (`tests/gates.rs`) |

Reproduce: `cargo build --profile tiny --target
thumbv7em-none-eabihf --no-default-features --bin hipp_thumb_demo`
and `cargo test --workspace`.

## hipp-cli

A std binary shell (`hipp-cli/`, decision D4): interactive,
stdin-batch and one-shot modes with list-format output
byte-diffable against the pinned oracle CLI, plus `.tables`,
`.schema`, `.dump`, `.open`, `.exit`. Its first job is
differential testing. It is a separate crate and contributes
0 bytes to the core's tiny build.

## Non-goals (SPEC §3)

No WAL (files in WAL mode are rejected with `WalUnsupported`),
no expression or partial indexes, no subqueries, no bound
parameters, no `UPDATE`/`DELETE`/`DROP TABLE`, views are
stored but never executed, no UTF-16, no multi-process
locking — and **no `sqlite3_*` C ABI, ever** (D3): hipp is a
Rust library, not a drop-in replacement.

## Layout

A workspace: the zero-dependency core crate at the root plus
`hipp-cli/`. Core `src/`: `arena` (scoped bump arenas — the only
`unsafe`), `storage` (async trait + Mem/File impls), `pager`
(page cache, journal protocol, commit machine), `btree`,
`record` (serial codec, affinity), `schema` (catalog), `sql/`
(lexer, parser, AST), `query` (resolved-expression evaluator,
v0.4), `session` (Connection/Statement, planner, tree-walking
executor), `error` (remedy-named errors).
Design: `docs/SPEC.md` (rev. 0.3) and `docs/RISKS.md`. Build
status and acceptance evidence: `STATUS.md`.

## License

Apache-2.0.
