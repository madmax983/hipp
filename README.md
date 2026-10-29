# hipp

**It's Hipp to be `no_std`.**

hipp is a little database engine in Rust that **reads and writes
SQLite database files**. It gives SQLite the treatment Horton got:
`no_std` + `alloc`, arena-first, built to run anywhere LLVM targets
— microcontrollers included — with hard size and RAM gates, not
aspirations.

- **Phase:** v0.11 — subqueries inside aggregate arguments
  (`sum((SELECT ...))` and friends, evaluated per group
  row), on the v0.10 foundation (SPEC §2).
- **Pinned claim version:** SQLite **3.54.1** (decision D1). The
  differential suite in `tests/` runs against a locally built
  3.54.1 oracle; `PRAGMA integrity_check` passes on hipp-written
  files, and real SQLite recovers hipp's rollback journals.
- **Profile measured:** `tiny` profile (`opt-level = "z"`, LTO) on
  `thumbv7em-none-eabihf`.

## What v0.11 does

- Everything v0.10 does (below), plus **subqueries inside
  aggregate arguments** behind the `v10` feature (default
  on, implies `v09`; the tiny profile excludes it — the
  feature ladder's next rung is `v10` because v0.10 itself
  was an ungated correctness fix and took no rung). The
  v0.6 plan-time refusal is lifted in every aggregate-
  argument position: SELECT list, HAVING, and ORDER BY, in
  top-level queries, subquery bodies, derived tables,
  compound cores, and `INSERT INTO ... SELECT` sources.
  The pin's semantics, probed before implementation: the
  argument is evaluated **per group row** — each subquery
  executes against that row (correlation binds the row,
  and a no-FROM subquery reads the row's own columns), a
  subquery yielding no row contributes NULL for that row,
  and the aggregate folds the per-row values, DISTINCT and
  group_concat separators included. Subqueries outside the
  aggregate in the same expression still resolve against
  the group's representative row, and a lone min/max over
  a subquery argument keeps the bare-column rule (bare
  columns read the extremum row, by the per-row value).

## What v0.10 does

- Everything v0.9 does (below), plus **NOT NULL
  enforcement** — the last parse-but-unenforced integrity
  constraint in the claimed profiles. It is *not*
  feature-gated: like the v0.8 affinity fix, it corrects
  existing write paths, so the tiny profile (whose INSERT
  claimed the same semantics) enforces it too. A NULL
  proposed for a NOT NULL column is resolved under the
  statement's conflict action, with the pinned oracle's
  exact per-clause rules (all probed before
  implementation): ABORT / FAIL / ROLLBACK error with
  `SchemaKind::NotNullViolation`; OR IGNORE skips the row
  (INSERT) or the combination (UPDATE ... FROM); OR
  REPLACE substitutes the column's default — affinity
  applied, as for any stored value — and errors when the
  default itself is NULL. The check runs before any
  uniqueness or rowid conflict is considered (NOT NULL
  wins error precedence, and a substituted default can
  itself make a REPLACE victim). An upsert proposal is
  resolved before arbitration; the DO UPDATE result row
  is then checked with abort semantics unconditionally.
  The rowid-alias column is exempt; a non-alias PRIMARY
  KEY column (TEXT PK, `INTEGER PRIMARY KEY DESC`) is
  not.

## What v0.9 does

- Everything v0.8 does (below), plus the last grammar holes
  of the claimed profiles behind the `v09` feature (default
  on, implies `v08`; the tiny profile excludes it):
  **compound SELECTs** — `UNION`, `UNION ALL`, `INTERSECT`
  and `EXCEPT` between SELECT cores and `VALUES` cores,
  anywhere a select statement appears (statements, subquery
  bodies, `INSERT INTO ... SELECT` sources, derived tables),
  with the pinned oracle's exact rules, all probed before
  implementation: the four operators share one precedence and
  associate to the left; row equality for dedup is the raw
  storage comparison with NULLs equal and **no affinity
  applied** (`1 UNION '1'` keeps both rows); dedup operators
  emit in the oracle's sorter order; a UNION's right side
  replaces equal left rows (`1 UNION 1.0` yields the real);
  and a compound's `ORDER BY` accepts only output ordinals
  and result-column names — from any constituent core,
  qualified names included — never general expressions.
  **Derived tables** — `(SELECT ...) [[AS] alias]` (also
  `(VALUES ...)`) in the general query planner, the subquery
  planner, and `UPDATE ... FROM`: the alias is optional, no
  column-name list may follow it (a syntax error on the pin),
  the result exposes no rowid, and the subquery is planned
  against enclosing scopes only — SQLite has no LATERAL, so
  sibling sources are invisible to it, while correlation
  with an enclosing query works. A standalone `VALUES`
  statement and `VALUES` cores inside compounds (including
  `IN (VALUES ...)`) complete the surface; the pin's quirk
  that a trailing ORDER BY / LIMIT after a trailing VALUES
  core is a syntax error is reproduced.

## What v0.8 does

- Everything v0.7 does (below), plus the
  correctness-and-completion pass behind the `v08` feature
  (default on, implies `v07`; the tiny profile excludes it):
  `INSERT INTO ... SELECT` — the full SELECT grammar as a row
  source (joins, aggregates, GROUP BY, DISTINCT, ORDER BY /
  LIMIT, no-FROM selects, column lists), composing with the
  v0.7 machinery (`OR` conflict clauses, UPSERT, RETURNING);
  the source is fully materialised before the first row is
  inserted, so a self-insert doubles the table exactly once,
  as in SQLite; and subqueries in `INSERT VALUES` (scalar,
  `IN (SELECT ...)`, `EXISTS`), likewise all evaluated
  against the pre-statement table before any row lands.
  One oracle quirk is reproduced, not widened: an upsert
  clause after an INSERT ... SELECT whose FROM clause is
  not terminated by WHERE / ORDER BY / LIMIT is a syntax
  error in SQLite, and hipp refuses the same shape.
- A correctness fix in the comparison evaluator (not
  feature-gated — it lives in the v0.4 evaluator): SQLite's
  comparison-affinity rules now apply between any two
  operands, not only column-vs-literal. A NUMERIC-affinity
  column compared against a TEXT-affinity column converts
  the text side (`int_col = text_col` with `'002'` stored is
  true, where v0.7 silently compared raw storage classes
  and answered false); TEXT vs BLOB affinity converts
  nothing (a BLOB column is its own affinity class, not
  "no affinity"); CAST operands and rowid references carry
  their affinity class; and a scalar / IN subquery whose
  result item is a direct column reference carries that
  column's affinity into the enclosing comparison. CASE's
  base form compares with affinity, like `=`; NULLIF still
  compares binary, as SQLite does.

## What v0.7 does

- Everything v0.6 does (below), plus the write-side
  completion behind the `v07` feature (default on, implies
  `v06`; the tiny profile excludes it): `OR REPLACE` /
  `OR IGNORE` / `OR ABORT` conflict clauses on INSERT and
  UPDATE (plus the `REPLACE` verb), where REPLACE deletes
  every row the proposal or update collides with — through
  the rowid and through UNIQUE indexes alike; UPSERT
  (`INSERT ... ON CONFLICT [(target)] DO NOTHING | DO
  UPDATE SET ... [WHERE ...]`) with `excluded.*` proposal
  references and the oracle's fallback to the statement's
  own conflict action for conflicts the clause does not
  arbitrate; `UPDATE ... FROM` with per-combination
  evaluation against the target's original values (last
  matching combination wins); and `RETURNING` on INSERT /
  UPDATE / DELETE, including subquery expressions.
  Named boundaries: RETURNING rows are buffered until the
  write completes (values identical, timing differs from
  SQLite's streaming); the upsert conflict-target WHERE is
  name-resolved but never evaluated, matching the oracle
  for the non-partial indexes hipp creates.

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
  aggregate's arguments are refused at prepare time;
  FROM-clause derived tables and UNION remain outside the
  grammar. (Subqueries in INSERT VALUES, refused in v0.6,
  are implemented in v0.8.)

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

| Gate | Budget | Measured (v0.11) |
|---|---|---|
| Linked `.text` + `.rodata` (thumbv7em) | ≤ 128 KiB | **89,872 B** (Δ **0 B** vs v0.10's 89,872 B; the v0.11 engine lives entirely behind the `v10` feature and the v04/v06-gated modules, so the tiny profile pays nothing for it) |
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
