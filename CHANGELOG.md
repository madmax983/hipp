# Changelog

All notable changes to hipp. Versions follow the milestones in
`STATUS.md`; the oracle pin is SQLite 3.54.1 throughout.

## Unreleased (0.17)

### FTS5 delete writes (leaf `fts5_delete`, in `xl`)

- `contentless_delete=1` tables: DELETE, UPDATE (every indexed
  column; UNINDEXED-only is a no-op, a subset is the pin's error)
  and INSERT OR REPLACE write tombstone hash pages and the
  structure's `nEntryTombstone` / `nPgTombstone` as the pin does
  (written at a hash rebuild and at the flush; tombstones count as
  automerge work and drive `deletemerge`).
- `secure-delete=1`: DELETE, UPDATE, OR REPLACE and the 'delete'
  command remove entries from the segments in place, byte for byte
  with the pin (`version` 5 and the cookie at the first one).
- Fixes on claimed fts5 paths (ungated, `fts5`): a row with no
  tokens counts into the next segment's `nEntry` across commits;
  `rowid IN (x)` is a one-row write (no statement-journal flush);
  `integrity-check` checks an external table's content only with
  rank 1; a bm25 of `0 / 0` is NULL; bm25 / snippet / ORDER BY
  rank on a row without `%_docsize`, or a table whose totals count
  no row, are the pin's corruption error; an UPDATE to a NULL
  rowid is "datatype mismatch".
- `link.x`: the sizing demo's FLASH region is 2 MiB so `xl` links.
- Sizes: tiny 76,960 B (0), full 505,416 B (0), max 1,044,172 B
  (+832, the ungated fixes), xl 1,053,532 B (+10,192: the leaf,
  9,360 B, does not fit `max`'s remaining 4,404 B).

### Streamed rows (ungated, `v16` executor; cursors with `vtab`)

- A top-level SELECT returns its rows one `step` at a time: a
  statement failing on row N returns rows 1..N-1 first (as the pin
  and its shell do), LIMIT stops reading (`generate_series(1) LIMIT
  3` answers at once), OFFSET rows are not projected, LIMIT 0 reads
  nothing.
- Virtual-table cursors drive the join row by row
  (generate_series, the R*Tree search); an inner virtual table is
  filtered again per outer row. dbstat / fts5 / fts5vocab hand out
  the rows their filter computed.
- count / sum / total / avg / min / max without GROUP BY fold in
  constant memory: `count(*) FROM generate_series(1)` is 4294967295.
- `Config::max_vtab_rows` bounds only rows a statement holds
  (DISTINCT, GROUP BY, ORDER BY without LIMIT, and the materialising
  paths: subqueries, derived tables, compounds, RIGHT / FULL joins,
  window queries, write-statement sources); an ORDER BY with a LIMIT
  keeps only the rows it may return.
- Under `v16` the synchronous v0.4 query executor is retired (the
  async executor serves every top-level query).
- Sizes: tiny 76,960 B (0), full 505,416 B (+4,464), max 1,043,340 B
  (+7,824), xl 1,043,292 B (+7,776).
- Review fixes: an ORDER BY naming a result column (alias, ordinal,
  same expression) sorts on the returned value, so a `random()`
  column comes out sorted (every executor, ungated); an aggregate
  without GROUP BY drops its ORDER BY as the pin does (only its
  aggregates still run); recursive CTEs stream (an endless one read
  through a WHERE or join under a LIMIT with a doubling bound); a
  streamed join frees the JSON documents and full-text rows of each
  finished inner loop and row, and computes an inner fts5 / dbstat
  source that reads nothing to its left once; hipp-cli exits 1 after
  a stdin script with a failed statement or dot-command and skips
  the rest of the failing input chunk. Sizes on top of the FTS5
  delete unit: tiny 0, full 506,432 B (+1,016), max 1,045,676 B
  (+1,504), xl 1,055,028 B (+1,496); every ratchet at its measured
  figure.

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
| full | `full`: every rung + `params`, `math` | 500,952 B | 512 KiB |
| max | `max` = `full` + the leaves below | 1,035,516 B | 1 MiB |

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
  over the rowid, rowid IN lists, `IS NULL` on NOT NULL keys). The window
  and compound-member changes first introduced two wrong answers, fixed
  in the same release: a window query's outer ORDER BY on a column it
  does not select was ignored (the loop's index was taken as covering
  without that column), and a UNION read through the LIMIT 1 first-row
  shortcut (written, or a scalar / EXISTS subquery's own) returned
  another first row than the pin.

### Not included

No `sqlite3_*` C ABI (D3), no multi-process locking or shared `-shm`, no
query flattening, join reordering or automatic indexes in the planner
(EXPLAIN QUERY PLAN says so), no plain EXPLAIN bytecode listing, no
loadable extensions or application-defined SQL functions.
