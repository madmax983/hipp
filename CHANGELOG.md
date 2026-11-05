# Changelog

All notable changes to hipp. Versions follow the milestones in
`STATUS.md`; the oracle pin is SQLite 3.54.1 throughout.

## 0.16.0 — the SQLite-parity push (2026-11-05)

Closes as much of the gap with the pinned SQLite 3.54.1 as the size
budgets allow, under ADR 0001 (silent wrong answers fixed ungated, new
surface behind the `v16` rung or a leaf feature, the tiny profile never
grows) and ADR 0002 / 0003 (the `max` profile for the heavyweight
leaves). Every item is differential-tested against the live pin.

### Profiles

| profile | features | thumbv7em size | budget |
|---|---|---|---|
| tiny | `--no-default-features` | 76,960 B | 128 KiB |
| full | `full`: every rung + `params`, `math` | 500,200 B | 512 KiB |
| max | `max` = `full` + the leaves below | 1,022,404 B | 1 MiB |

`default = ["std", "max"]`, so the CLI and the tests build everything.

### New: the `v16` rung (in `full`)

- Query shape: LIMIT / OFFSET expressions, result-column aliases in
  WHERE / ON / GROUP BY / HAVING / ORDER BY, `x IN table`.
- `expr COLLATE name`, GLOB, LIKE ... ESCAPE, like() / glob().
- Bitwise operators, IS [NOT] DISTINCT FROM, postfix NOT NULL, row
  values.
- The scalar function library (printf / format, quote, char, unicode,
  zeroblob, randomblob, random, changes / last_insert_rowid, concat,
  concat_ws, octet_length, unhex, iif / if, sqlite_version, ...).
- Aggregates: string_agg, aggregate ORDER BY, FILTER, median and the
  percentile family.
- PRAGMA statements, `sqlite_schema` as a table, `pragma_*` table-valued
  functions.
- Executed views; WITH [RECURSIVE] CTEs before every statement and in
  subqueries.
- SAVEPOINT / RELEASE / ROLLBACK TO, VACUUM [INTO], REINDEX, ANALYZE.
- ALTER TABLE RENAME / RENAME COLUMN / DROP COLUMN, AUTOINCREMENT, TEMP
  tables / views / indexes, CREATE TABLE ... AS.
- The pin's parser and evaluator depth limits.
- INDEXED BY / NOT INDEXED, three-part column names, statements across
  TEMP and main.

### New: leaf features

- `params` (in `full`): bound parameters and the Statement bind API.
- `math` (in `full`): the SQL math functions over an in-crate libm.
- `window`: window functions, named windows, every frame kind.
- `triggers`: CREATE / DROP TRIGGER, the pin's firing order, INSTEAD OF.
- `foreign_keys`: enforcement, deferred keys, every action,
  `foreign_key_check`.
- `json`: the JSON / JSONB functions, `->` / `->>`, json_each / json_tree.
- `expr_index`: expression and partial indexes, and index-aware planning
  in the pin's row order.
- `without_rowid`: WITHOUT ROWID tables, byte-compatible.
- `utf16`: UTF-16 databases.
- `wal`: write-ahead logging (single process).
- `datetime`: the date and time functions over a `Config::clock` hook.
- `attach`: ATTACH / DETACH and super-journal commits.
- `autovacuum`: auto-vacuum and incremental-vacuum files.
- `vtab`: virtual tables, generate_series, dbstat.
- `rtree`: the R*Tree module.
- `fts5`: FTS5 full-text search, byte-compatible shadow tables.
- `explain`: EXPLAIN QUERY PLAN with the pin's rows and the CLI tree.

### Fixed (ungated: claimed paths that were silently wrong)

- RIGHT / FULL / NATURAL / USING joins (RIGHT used to parse as an alias),
  NULLS FIRST / LAST, ordinal / alias term rules, `NULL IN ()`.
- CHECK constraints, expression DEFAULTs, generated columns, STRICT,
  column COLLATE (all parsed and ignored before).
- Collations in comparisons, indexes, ORDER BY, GROUP BY, DISTINCT,
  compounds and min / max.
- Operator precedence, IS TRUE / FALSE, hex and digit-separator
  literals, aggregate scoping into subqueries.
- round() digits, lazy coalesce / iif, text functions; sum() overflow,
  compensated REAL sums, group_concat separators, DISTINCT argument
  counts.
- REAL-to-text rendering fitted to the pin; the UNION-with-LIMIT-1
  shortcut; `sqlite_*` names refused in CREATE; the shared table / view /
  index namespace; constraint-level ON CONFLICT clauses.
- Access-path choices that ordered rows unlike the pin (window functions,
  compound members, GROUP BY / DISTINCT over a permuted index, DISTINCT
  over the rowid, rowid IN lists, `IS NULL` on NOT NULL keys).

### Not included

No `sqlite3_*` C ABI (D3), no multi-process locking or shared `-shm`, no
query flattening, join reordering or automatic indexes in the planner
(EXPLAIN QUERY PLAN says so), no plain EXPLAIN bytecode listing, no
loadable extensions or application-defined SQL functions.
