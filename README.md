# hipp

**It's Hipp to be `no_std`.**

hipp is a little database engine in Rust that **reads and writes
SQLite database files**. It gives SQLite the treatment Horton got:
`no_std` + `alloc`, arena-first, built to run anywhere LLVM targets
— microcontrollers included — with hard size and RAM gates, not
aspirations.

- **Phase:** 0.16.0 released — v0.16, the SQLite-parity push (ADR
  0001: silent wrong answers fixed ungated, new dialect behind the
  `sql` feature / leaf features, the tiny profile never grows; ADR
  0002 / 0003: the `max` profile), on the v0.15 foundation (SPEC §2).
  `CHANGELOG.md` summarises the release. v0.17 (the parity backlog:
  the v0.16 units' honest gaps) is in progress on top of it; its
  features are named for what they enable (ADR 0006: `query`,
  `write`, `sql`, `planner` replace the version-numbered features).
- **Pinned claim version:** SQLite **3.54.1** (decision D1). The
  differential suite in `tests/` runs against a locally built
  3.54.1 oracle; `PRAGMA integrity_check` passes on hipp-written
  files, and real SQLite recovers hipp's rollback journals (and,
  with the `wal` leaf, its write-ahead logs).
- **Profile measured:** `tiny` profile (`opt-level = "z"`, LTO) on
  `thumbv7em-none-eabihf`.

## Profiles and features

Four size-gated `no_std` profiles (thumbv7em, `opt-level = "z"`,
LTO; `ci/size-ratchet.toml`, ADR 0003 / ADR 0004). `default = ["std", "xl"]`.

| profile | build | size | budget |
|---|---|---|---|
| `tiny` | `--no-default-features` | 76,952 B | 128 KiB |
| `full` | `--no-default-features --features full` | 477,484 B | 512 KiB |
| `max` | `--no-default-features --features max` | 963,572 B | 1 MiB |
| `xl` | `--no-default-features --features xl` | 1,060,496 B | 1.25 MiB |

- **`tiny`**: CREATE TABLE, INSERT (VALUES), scan SELECT with
  `WHERE col = literal`, transactions, rollback journal and recovery.
- **`full`** adds the core dialect and two leaves: `indexes` (index
  B-trees, UNIQUE), `query` (joins, aggregates, the expression
  evaluator, CHECK / DEFAULT / generated columns / STRICT, ALTER ADD
  COLUMN), `write` (UPDATE / DELETE / DROP TABLE; implies `query`),
  `sql` (the rest of the dialect; implies `write`: subqueries, OR
  conflict clauses, UPDATE FROM, RETURNING, upsert, INSERT ...
  SELECT, compounds, derived tables, query shape, collations,
  operators, the function library, aggregate completion, PRAGMA,
  views and CTEs, savepoints and maintenance, ALTER / AUTOINCREMENT
  / TEMP, depth limits), `params` (bound parameters), `math` (math
  functions).
- **`max`** adds the heavyweight leaves: `window` (window functions),
  `triggers`, `foreign_keys`, `json` (JSON / JSONB), `expr_index`
  (expression and partial indexes, index-aware planning),
  `without_rowid`, `utf16`, `wal`, `datetime`, `attach`,
  `autovacuum`, `vtab` (virtual tables, generate_series, dbstat),
  `rtree`, `fts5`, `explain` (EXPLAIN QUERY PLAN).
- **`xl`** adds the leaves past `max`'s budget (ADR 0004):
  `fts5_delete` (FTS5 tombstones and secure-delete) and `planner`
  (the pin's join order, automatic indexes, Bloom filters,
  `sqlite_stat1`; MULTI-INDEX OR, min() / max() lookups, skip-scans,
  the IN operator's statistics rules; the query flattener, WHERE
  push-down, unused subquery columns, EXISTS-to-join, foreign-key
  child scans in EXPLAIN QUERY PLAN).
- Every leaf of `max` / `xl` that builds on the dialect implies
  `sql`; `math` implies only `query`. The features before 0.17 were
  named by milestone (`v04` ... `v16`, and `join_order` /
  `multi_index` / `flatten`); `CHANGELOG.md` has the old → new table.

## What v0.17 does

- **Streamed rows.** A top-level SELECT hands its rows to
  `Statement::step` one at a time, as the pin does: the join is a
  resumable nested loop whose first source is read through its
  cursor (a table's b-tree, a virtual table's xFilter / xNext /
  xColumn — generate_series and the R*Tree search row by row) and
  whose inner virtual tables are filtered again with each outer
  row's values. A statement that fails on row N returns rows 1..N-1
  first (the CLI prints them before the error), LIMIT stops the join
  (`SELECT value FROM generate_series(1) LIMIT 3` answers at once),
  OFFSET rows are not projected, and count / sum / total / avg /
  min / max without GROUP BY fold in constant memory
  (`count(*) FROM generate_series(1)` is 4294967295, not
  VtabTooManyRows). ORDER BY (unless the rowid or an index delivers
  it), GROUP BY and DISTINCT buffer as the pin's sorters do;
  `Config::max_vtab_rows` now bounds only what is held. Recursive
  CTEs stream too, a streamed join's memory does not grow with its
  outer rows, an ORDER BY naming a result column sorts on the value
  returned (`random() AS r ... ORDER BY r`), a one-row aggregate
  drops its ORDER BY as the pin does, and the CLI exits 1 after a
  stdin script whose statement failed. Ungated in the `sql`
  executor (`src/session/stream.rs`).
- **FTS5 delete writes** (leaf `fts5_delete`, in `xl`). DELETE,
  UPDATE and INSERT OR REPLACE on `contentless_delete=1` tables
  write the pin's tombstone hash pages (`fts5IndexTombstoneAdd`:
  32-slot first page, growth to `pgsz`, then `2n + 1` pages, 4- or
  8-byte keys, the rowid-0 flag) and the structure's tombstone
  counters, counted as automerge work and driving `deletemerge`; an
  UPDATE must set every indexed column (the pin's "cannot UPDATE a
  subset of columns") and one of UNINDEXED columns only is a no-op.
  With `secure-delete=1` every delete removes the row's entries
  from the segments in place (`fts5DoSecureDelete`: entries and
  terms, emptied and overflow pages, page indexes, `%_idx` rows,
  `version` 5), byte for byte with the pin in every detail mode;
  each engine's integrity-check passes on the other's files. A
  statement's deletes take time linear in the rows deleted (a
  tombstone reads one hash page; the secure-delete walk seeks the
  term's and the rowid's page). Without the leaf those writes are
  refused as in 0.16 (`src/fts5/sdel.rs`). OR FAIL / OR ROLLBACK on
  fts5 writes now resolve as on the pin (ungated).
- **Join ordering and automatic indexes** (leaf `planner`, in
  `xl`). A join of two or more tables, virtual tables or JSON
  table-valued functions is planned as the pin's `where.c` plans it:
  per-loop costs with prerequisites, the N-best path solver with
  ORDER BY / GROUP BY / DISTINCT sorting costs, the LEFT / CROSS JOIN
  ordering rules, automatic indexes (built once per statement,
  PARTIAL where a term reads the table alone) behind a Bloom filter,
  Bloom filters on lookups when `sqlite_stat1` says they pay, and
  the statistics ANALYZE writes read back. Rows without ORDER BY come
  in the pin's order, EXPLAIN QUERY PLAN prints the pin's loops, and
  `FROM f, m WHERE f MATCH m.q` or `FROM json_each(t.j), t` run
  instead of being refused; an automatic index turns a 2,000 x 2,000
  equi-join from 4 million comparisons into 2,000 probes. UPDATE ...
  FROM is planned as the same kind of join (the final table is the
  pin's when several FROM rows match), joins of up to 64 sources are
  ordered, likely() / unlikely() / likelihood() terms keep their index
  use and give the pin's estimates, and the statistics a connection
  holds follow DDL as the pin's do. Without the leaf joins run in
  FROM order as in 0.16 (`src/xjoin.rs`, `src/session/join.rs`).
- **Index strategies** (leaf `planner`, in `xl`). The pin's
  other loop kinds: the MULTI-INDEX OR (`WHERE a = 1 OR b = 2` reads
  each branch through its own index or the rowid, each row once, in
  the pin's branch order — for one table, as a join loop, and for
  UPDATE / DELETE; ~40 rows read where a 20,000-row scan read all),
  the min() / max() lookup (a lone `max(x)` read in x's order down
  to its first row, so the bare columns are the pin's; `SEARCH t`),
  skip-scans when `sqlite_stat1` says the leading column repeats
  (`ANY(a) AND b=?`), an IN list or subquery the statistics say not
  to probe, and the pin's loop-cost adjustment for one table (`b = 2
  AND c = 1` over an index on (b, c) uses both columns). EXPLAIN
  QUERY PLAN prints the `MULTI-INDEX OR` / `INDEX n` trees, the IN
  operator's `CREATE BLOOM FILTER` (BINARY comparisons only) and its
  `USING INDEX i FOR IN-OPERATOR` when it reads an index in place of
  the subquery; `sqlite_stat1`'s `unordered` / `sz=` / `noskipscan`
  are read. Fixed on the way (ungated): an OR of `col = other_col`
  equalities was probed as an IN list without the pin's affinity
  check, losing rows; an OR holding a COLLATE, or a one-value `IN
  (v)`, is planned as the pin's parser leaves it; `a = 'y' COLLATE
  NOCASE ORDER BY a` returned rows out of order. Without the leaf
  plans are as in 0.16 (`src/xmulti.rs`).
- **Query flattening** (leaf `planner`, in `xl`). Views, CTEs and
  FROM subqueries the pin's `sqlite3FlattenSubquery` merges into
  their reader are merged by hipp too (its numbered restrictions; a
  LEFT JOIN's right side with its NULL-row guard; the subquery's LIMIT
  and ORDER BY moving out), so their tables join the join planner's
  order and EXPLAIN QUERY PLAN prints their loops instead of a
  co-routine. The subqueries that stay take the reader's WHERE terms
  that read only them (`pushDownWhereTerms`, into HAVING for an
  aggregate's non-GROUP BY terms) and compute only the result columns
  their reader reads — views and written subqueries, never a CTE's
  reference or a correlated one, which the pin computes whole — so an
  error in a column nobody reads is not raised, as on the pin (in an
  UPDATE ... FROM too); a view's copies share one materialisation only
  while none lost a column, as the pin's `isSelfJoinView`. A WHERE
  `EXISTS (SELECT ... FROM t ...)` becomes a semi-join loop (`SEARCH t
  EXISTS`, its own terms checked at its loop), beside a LEFT JOIN the
  pin's simplification keeps, and a parent DELETE / UPDATE / INSERT
  under `PRAGMA foreign_keys` shows the pin's child-table scans,
  planned with the parent key's affinity and collation. A merge past 64
  sources is the pin's "at most 64 tables in a join". Review fixes,
  ungated: a MULTI-INDEX OR under a subquery's loop found the first
  outer row's matches only; a LIMITed FROM subquery computed its
  result columns past the LIMIT. Without the leaf subqueries plan as
  in 0.16 (`src/session/flatten.rs`).
- **Size pass: one shared sort** (no feature). Every sort in the engine
  — ORDER BY, GROUP BY, compounds, windows, aggregate ORDER BY, IN
  lists, the planner's orders, sorted writes — goes through one
  non-generic stable merge sort over an index permutation
  (`arena::sort_perm`) instead of a `slice::sort` copy per element
  type and comparator: full −26,352 B, max −62,688 B, xl −76,948 B,
  tiny unchanged, rows and tie order unchanged against the pin. Fixed
  on the way, ungated: an fts5 `ORDER BY rank` whose bm25 score is NaN
  (an infinite rank weight) sorts those rows as the pin's NULL —
  first ascending, last descending — instead of leaving them (and, in
  a mixed corpus, the finite ranks around them) where the sort happened
  to put them.
- **Size pass: monomorphic glue** (no feature). Machinery Rust
  compiled once per type is compiled once: the expression evaluator's
  helpers, conditions and aggregate walks no longer have both an
  `Error<()>` and a storage-error copy, the B-tree balance computes
  its split points once for every cell type, and a statement's plan is
  shared, not deep-copied, each time it runs (a correlated subquery
  no longer copies its plan per outer row): tiny −700 B, full
  −4,060 B, max −4,068 B, xl −4,564 B. Rows, errors and file bytes
  are unchanged against the pin.
- **Size pass: sync islands** (no feature). The sync code inside
  mostly-sync async bodies — the schema load, the integrity check's
  row checks, the streamed query's setup, subquery ORDER BY windows,
  statement preparation, trigger-failure resolution — runs in
  out-of-line sync functions, and the futures keep only their I/O:
  tiny −136 B, full −704 B, max −2,024 B, xl −1,808 B with its
  review fixes; every split that measured larger was reverted
  (STATUS.md lists both). With the two units above this closes Stage 1
  of ADR 0005: tiny −836 B, full −31,116 B, max −68,780 B (86,284 B
  of headroom under 1 MiB), xl −83,320 B. Rows, errors and file bytes
  are unchanged against the pin, except three integrity-check fixes
  found in review: "row N missing from index X" names the broken
  index (it named another index of the table), a partial index gets
  no "wrong # of entries" line (the pin keeps no such count), and a
  non-STRICT column's stored type is checked against its affinity
  ("NUMERIC value in t.c" / "TEXT value in t.c", as on the pin).
- **Features named for capabilities** (breaking rename, ADR 0006).
  `query` (was `v04`), `write` (was `v05`) and `sql` (was `v06` ...
  `v10` and `v16`) replace the milestone-numbered features, and one
  leaf `planner` (in `xl`) replaces `join_order`, `multi_index` and
  `flatten`. The profiles keep their names and contents; code only an
  intermediate feature could reach is gone. No behaviour change, and
  the code is the same: a mechanical rename of the old tree links to
  byte-identical sections. The new names alone (they enter the
  crate's metadata hash and so rustc's item order) move max by +8 B
  and xl by +16 B of alignment padding (STATUS.md has the table).
- **The bytecode VM, first unit** (hidden feature `vm`, ADR 0005
  Stage 2). A register VM (`src/vm/`) with synchronous, restartable
  page reads runs SELECTs over one rowid table or none — WHERE, any
  subquery-free expression, ORDER BY, LIMIT / OFFSET, full scans and
  rowid lookups and ranges — and hands every other statement to
  today's executor. No profile enables `vm` yet, so no profile's size
  changes; the stage's later units cover the rest of SELECT, and its
  last one turns the VM on and deletes the executors it replaces.
- **The bytecode VM, second unit** (hidden feature `vm`). Joins of
  any kind in the planner's order, every index access path (sync,
  restartable index cursors), aggregates, GROUP BY / HAVING and
  DISTINCT now run on the VM too; subqueries, compounds, CTEs / views,
  windows and virtual sources still hand off to today's executor.
- **GROUP BY with ORDER BY and LIMIT** (fix, every profile with
  `query`). A grouped query stops after its LIMIT's groups only when
  they come in the ORDER BY's order, as the pin does; an index (or the
  rowid) delivering the groups is read backwards for `ORDER BY ...
  DESC`. hipp used to return the wrong groups for `ORDER BY b NULLS
  LAST LIMIT n` and for `ORDER BY b DESC LIMIT n` over an index.
- **The bytecode VM, third unit** (hidden feature `vm`). Scalar, IN
  and EXISTS subqueries (correlated or not, anywhere in a SELECT, in
  aggregate arguments too), FROM subqueries, views and CTEs, compounds
  and VALUES, and recursive CTEs run on the VM, with today's laziness
  and once-per-statement caching (a scalar or EXISTS subquery stops at
  its first row, as on the pin); windows, virtual and table-valued
  sources and `sqlite_schema` still hand off. Its review fixes add
  min() / max() over a subquery and an enclosing query's aggregates
  inside subqueries, and share an uncorrelated subquery's code so
  nested aliases no longer blow up code generation.

## What v0.16 does

- **Joins, ordering and query-shape completion.** Ungated
  fixes to claimed v0.4 paths: `RIGHT` / `FULL [OUTER]
  JOIN` (they used to parse as a table alias and run as an
  inner join) with the pin's row order; `NATURAL` and
  `USING (...)` joins with the pin's `SELECT *` expansion
  and USING-column resolution (left-most side, right side
  after RIGHT, `coalesce` after FULL); ON clauses that name
  a later source (hoisted on inner joins, refused on outer
  ones); `ORDER BY ... NULLS FIRST / LAST` honoured
  everywhere; the pin's ordinal / alias / HAVING term rules;
  `NULL IN (<empty>)` = 0. Behind `sql` (then the new `v16` feature):
  LIMIT / OFFSET as expressions (constant or uncorrelated
  subquery, the pin's integer conversion), result-column
  aliases inside WHERE / ON / GROUP BY / HAVING / ORDER BY,
  `x IN tablename`, and string-literal aliases. Review
  fixes: compound ORDER BY names resolve per core as on the
  pin (a USING column after RIGHT / FULL is the joined
  value); a derived table's and a view's duplicate column
  names become `id:1`, ... and a view's column list shows
  NATURAL / USING columns once; `UPDATE t ... FROM t` and any
  schema qualifier but `main.` are refused instead of
  silently run against `main`; scalar / EXISTS subqueries take
  the pin's `LIMIT (X<>0)` rewrite; an alias holding a
  subquery works inside nested subqueries (one holding an
  aggregate names the alias's own query's aggregate there, as
  on the pin).
- **Column constraints and table definitions.** Ungated
  wherever the v0.4 evaluator is built (they used to be
  parsed and silently ignored): CHECK constraints on every
  write path with the pin's NULL-passes / OR IGNORE / no-REPLACE
  rules and NOT NULL → STRICT → CHECK → UNIQUE precedence;
  expression DEFAULTs (`DEFAULT (1+2)` stored 1), signed and
  identifier defaults, and CURRENT_TIME / DATE / TIMESTAMP
  through a caller-supplied `Config::clock` (`Error::NoClock`
  without one); VIRTUAL / STORED generated columns in the pin's
  record layout (files interoperate both ways), indexable,
  recomputed on UPDATE; UPDATE ... FROM applying only the last
  matching combination (in hipp's join order; the pin's join order
  with v0.17's `planner`); MustBeInt rowid
  conversion. The tiny profile refuses those clauses
  (`RequiresEvaluator`) instead of ignoring them, and still
  shrank. Behind `sql`: INSERT ... DEFAULT VALUES, CREATE TABLE
  ... AS SELECT (the pin's column names, declared types and
  schema text, byte for byte) and STRICT tables. (WITHOUT ROWID
  came later, in its own leaf: see below.) Review fixes (every profile): CREATE TABLE
  items are walked with the pin's grammar, so malformed
  constraint lists (`CHECK(a>0) bar`, `DEFAULT abs(1)`,
  `PRIMARY KEY (a) foo`) are syntax errors instead of schemas
  the pin cannot load; stored CREATE texts drop `main.` / `IF
  NOT EXISTS` as the pin's do; repeated PRIMARY KEY / UNIQUE
  sets share one autoindex; `-9223372036854775808` is an
  INTEGER; unary `+` keeps no affinity (`+a = '1'`, CTAS column
  `"+a"`); `||` is always TEXT, `abs()` follows the pin (TEXT →
  REAL, i64::MIN overflows) and REAL → TEXT uses the pin's
  `1.0e+20` layout — computed with exact integer arithmetic, so
  no build links `core::fmt`'s float formatting any more (the
  tiny profile dropped 5.5 KB); ALTER TABLE ADD COLUMN takes
  non-constant defaults, NOT NULL and STORED columns on an empty
  table; one clock read per statement; hipp-cli supplies the
  system clock; below `sql` CREATE TABLE refuses AUTOINCREMENT
  (those builds keep no `sqlite_sequence`, and such a table read
  as malformed on the pin; `sql` maintains it, see below).
- **Collations and pattern matching.** Ungated (column
  `COLLATE` was parsed and ignored): BINARY / NOCASE / RTRIM
  column collations with the pin's selection rule in every
  comparison (`=`, `<`, IS, IN, BETWEEN, CASE, joins,
  subqueries), in ORDER BY, GROUP BY, DISTINCT, min / max /
  nullif and DISTINCT aggregates, derived-table columns, and
  compounds — now evaluated with the pin's merge algorithm, so
  which of two NOCASE-equal rows survives a UNION matches too;
  collated indexes (`CREATE INDEX ... (c COLLATE NOCASE)`,
  UNIQUE NOCASE / RTRIM constraints and their autoindexes,
  upsert targets) order, find and enforce keys by their
  collation, interoperate with the pin's files both ways, and
  their tables are writable (they used to be refused as foreign);
  unknown collation names are refused (a file whose table names
  an application-registered collation still opens; only that
  table is refused). Upsert `excluded.<col>` compares with no
  collation or affinity, as on the pin. Also fixed on the way:
  GROUP BY takes the ORDER BY's directions for its group order,
  bare columns follow the last min / max aggregate (an all-NULL
  group reads its last row), scalar `min(1, 1.0)` is 1.0, LIKE
  reads operands up to a NUL and matches iteratively, a written
  `IN (list)` converts by its left operand's affinity only, and
  UPDATE OR REPLACE no longer revives rows its own REPLACE
  deleted. Behind
  `sql`: the `expr COLLATE name` operator, GLOB / NOT GLOB,
  `LIKE ... ESCAPE`, and `like()` / `glob()`.
- **Bound parameters** (leaf feature `params`, in `full`;
  not in the tiny profile, but it builds on it alone): `?`,
  `?NNN`, `:name`, `@name`, `$name` (and `#name`, the TCL
  `$a::b` / `$a(x)` forms) numbered exactly as the pin numbers
  them (a named parameter takes the next free index, repeated
  names share one, `?NNN` sets it, 32766 at most). The
  Statement API: `bind_null` / `bind_i64` / `bind_f64` /
  `bind_text` / `bind_blob` / `bind_value` by 1-based index,
  `parameter_count`, `parameter_index`, `parameter_name`,
  `clear_bindings` and `reset`, so a prepared statement is
  stepped again with new bindings (each execution plans afresh
  from the parsed tree; the statement arena is rewound, not
  grown). Unbound is NULL; a NaN binds NULL. A bound value
  takes part in affinity exactly as the literal of its storage
  class (the pin's rule), anywhere an expression goes in the full
  profile — WHERE, SET, VALUES, LIMIT / OFFSET, subqueries,
  upsert, RETURNING, compound cores, derived tables, CREATE TABLE
  ... AS SELECT — and in ORDER BY / GROUP BY as a constant (not
  an ordinal), matching a compound's identical result item
  (leftmost core first, a multi-row VALUES laid out as the pin
  lays it out) as on the pin; on top of the tiny profile, in INSERT VALUES and
  `WHERE rowid = ?`. Parameters in views, CHECK, DEFAULT and
  generated columns are refused, as on the pin. hipp-cli binds
  them like the oracle shell, from `.parameter set` (NULL
  otherwise). Found on the way and fixed ungated: `%` with a
  REAL operand is a REAL, CAST(text AS INTEGER) reads only the
  integer prefix, CAST(text AS NUMERIC) follows the pin's
  2^51 rule, substr() / round() read text integer arguments by
  their integer prefix, round() clamps its digit count to 30
  (its digits now follow the pin's printf; see the function
  library below),
  `CREATE TABLE IF NOT EXISTS t AS SELECT ...` over an existing
  `t` is a no-op before its SELECT is resolved, and hipp-cli
  prints `-0.0` as `0.0` and an exact half at the 16th digit
  rounded up, as the pin does.
- **Expression operators, literals and row values.** Ungated
  fixes to claimed v0.4 / v0.6 paths: the pin's precedence
  (`<` `<=` `>` `>=` bind tighter than `=` / IS / IN / LIKE /
  BETWEEN — `3 = 2 < 3` was 1 — and a postfix ISNULL / IN (...)
  result takes tighter operators, `x ISNULL + 1`); `x IS [NOT]
  TRUE / FALSE` as the pin's truth test (`2 IS TRUE` was 0); hex
  literals read as 16-digit two's complement
  (`0xffffffffffffffff` = -1) with the pin's "hex literal too
  big" (also for `-0x8000000000000000`); `1_000` digit
  separators and a literal glued to an identifier (`1a`,
  refused) — `1_000` used to read as `1` aliased `_000`; an
  aggregate inside a subquery whose arguments name only an
  enclosing query's columns belongs to that query (`SELECT
  (SELECT max(t.a) FROM u) FROM t` is one row, as on the pin;
  in that query's WHERE it is "misuse of aggregate"); an
  aggregate in ORDER BY alone no longer aggregates the query;
  a qualifier naming an inner source that lacks the column
  falls through to the outer scopes. Behind `sql`: `&` `|`
  `<<` `>>` `~` with the pin's integer coercion and shift
  rules, `IS [NOT] DISTINCT FROM`, postfix `NOT NULL`, and row
  values — `(a, b) = / <> / < / <= / > / >= / IS (c, d)`,
  BETWEEN, `IN ((..), (..))` (the pin's VALUES rewrite),
  `IN (SELECT a, b ...)`, CASE, multi-column scalar subqueries,
  `UPDATE ... SET (a, b) = (..)` / `= (SELECT ..)` (also in
  upsert), with the pin's errors for size mismatches and row
  values anywhere else. Review fixes: a written row-value IN
  list converts both sides under the set's affinity (`(1, 1)
  IN ((x, y), (a, b))` was 0); an enclosing query's aggregate
  inside another aggregate's arguments is "misuse of
  aggregate" (it returned a value); an enclosing query's
  aggregate in an EXISTS subquery's result list is inert, as
  on the pin (WHERE / ON / DELETE / UPDATE ran again); RETURNING
  and `excluded` never own a subquery's aggregate; an aggregate
  alias works inside HAVING / ORDER BY subqueries; a too-big hex
  literal errors only where it is evaluated (a pin file with
  one in a DEFAULT / CHECK is readable); a subquery used as an
  expression takes its comparison affinity from its rightmost
  core, as on the pin, and comparisons convert both operands
  (`'1' IN (SELECT 1 UNION SELECT i FROM c)` was 0); `x IN
  ((SELECT ...), 2)` parses; a bare reserved word is no alias
  (`SELECT 1 NOT`, `SELECT 1 AS` are syntax errors). The tiny
  profile did not grow.
- **Scalar function library.** Behind `sql`: `printf()` /
  `format()` with the pin's whole conversion set (`%d %i %u %x
  %X %o %p %r %c %s %z %q %Q %w %j %J %f %e %E %g %G %% %n`, the
  flags `- + space 0 # , !`, width / precision / `*`, the `#`
  unistr form of `%q` / `%Q`, the new 3.54 JSON forms) and its
  REAL digits byte for byte — fitted clean-room to pin output:
  the 18 / 19 internal digits, the 17-digit `!` shortening, and
  the truncated-scale rounding that makes `printf('%.0e', 450.0)`
  `4e+02`; `quote()`, `char()`, `unicode()`, `zeroblob()`,
  `randomblob()` / `random()` (an in-crate generator seeded from
  `Config::nonce_seed`), `concat()`, `concat_ws()`,
  `octet_length()`, `unhex()`, `likely()` / `unlikely()` /
  `likelihood()` (its constant-probability error), `if()` and the
  n-ary `iif()`, `sqlite_version()` (`'3.54.1'`), and
  `last_insert_rowid()` / `changes()` / `total_changes()` with the
  pin's counting rules — also as `Connection::last_insert_rowid`
  / `changes` / `total_changes`. Non-deterministic functions are
  refused in generated columns; unknown functions and wrong
  argument counts are prepare-time errors (`UnknownFunction`,
  `WrongArgumentCount`). Ungated fixes: `round()` rounds the
  pin's digits (`round(166162.7745, 3)` was .774), `coalesce` /
  `ifnull` / `iif` evaluate lazily, and `length` / `substr` /
  `trim` / `instr` / `replace` / `upper` / `lower` read text up
  to a NUL and blobs as bytes, as the pin's C functions do.
  Review fixes (ungated unless noted): a record with more than
  about 16 overflow pages no longer writes a truncated chain
  (`zeroblob(69530)` corrupted the file — a page-cache spill
  dropped chain writes); `coalesce` needs 2+ arguments and
  `ifnull` exactly 2; a subquery's error surfaces only if
  evaluation reaches it (`coalesce(1, (SELECT ...))`, an AND /
  OR the other side decides); `length` / `substr` / `trim` /
  LIKE walk text in place (no per-character list, so the pin's
  largest legal text no longer runs out of memory); `INSERT` /
  `UPDATE OR FAIL` keep the rows changed before the violation
  (it was treated as ABORT); behind `sql`, a DEFAULT reading
  `last_insert_rowid()` / `random()` is evaluated per row,
  `f(*)` is `f()` and DISTINCT is ignored on scalar functions.
  `sqlite_source_id()` stays undefined (hipp is not the pin's
  source check-in). The tiny profile grew by 8 bytes (the
  overflow-chain fix on its INSERT path).
- **Aggregate completion.** Behind `sql`: `string_agg(X, SEP)`;
  the aggregate ORDER BY (`group_concat(b, ',' ORDER BY a DESC,
  b)`) on every aggregate that takes it — a stable sort of the
  stepped rows, dropped by min / max as on the pin; DISTINCT
  keeps the first value seen, except that `f(DISTINCT x ORDER BY
  x)` deduplicates through the sort and keeps the last, as the
  pin's sorter does; `FILTER (WHERE ...)` on every aggregate
  (count(*), DISTINCT forms, min / max bare columns included);
  `count()` (and `count(ORDER BY ...)`, whose clause the pin
  drops); and the percentile family the pin builds in —
  `median()`, `percentile(Y, P)`, `percentile_cont()`,
  `percentile_disc()` with its P validation, constant-P check,
  numeric-only Y and REAL results. ORDER BY / FILTER on a
  non-aggregate are refused (`OrderByNonAggregate`,
  `FilterNonAggregate`), and they decide which query owns an
  aggregate in a subquery. Ungated fixes: `sum()` integer
  overflow is the pin's error (it returned a REAL; `total()` /
  `avg()` still go REAL), REAL sums are compensated
  (Kahan–Babuška–Neumaier: `0.1 + 0.2 + 0.3` sums to `0.6`), a
  text addend counts as REAL unless it is wholly an integer
  (`sum('12abc')` is `12.0`), a NULL group_concat separator adds
  nothing (the result was NULL), `group_concat(DISTINCT x, ',')`
  is refused (`DistinctAggregateArgs`), and a group finalizes its
  aggregates as the pin does — before HAVING, never for a group a
  HAVING term the pin moves into WHERE (constant or GROUP BY
  terms only), a group-ordered LIMIT, or an outer LIMIT reading a
  derived table keeps from forming. A NaN result (`sum()` over
  `+inf` and `-inf`, `inf - inf`) is NULL, as the pin stores it
  (it was a REAL 0.0). The tiny profile did not grow.
- **Math functions** (leaf feature `math`, in `full`; needs only
  `query`): every function the pin builds with
  SQLITE_ENABLE_MATH_FUNCTIONS — `acos` `acosh` `asin` `asinh`
  `atan` `atan2` `atanh` `ceil` / `ceiling` `cos` `cosh`
  `degrees` `exp` `floor` `ln` `log(X)` `log(B, X)` `log10`
  `log2` `mod` `pi` `pow` / `power` `radians` `sin` `sinh`
  `sqrt` `tan` `tanh` `trunc` — with the pin's argument rules
  (only INTEGER, REAL and wholly numeric TEXT count; NULL, BLOB
  and other text give NULL; `log(B, X)` reads X as any value's
  numeric prefix), NULL for every NaN result (`sqrt(-1)`,
  `acos(2)`, `mod(1, 0)`, `sin(1e400)`), INTEGER results from
  `ceil` / `floor` / `trunc` of an integer, and the pin's REAL
  rendering (`Inf`, `-Inf`). There is no libm in `no_std`, so
  `hipp::math` is an in-crate floating-point library: exact
  integer `sqrt`, `fmod`, `ceil` / `floor` / `trunc`; Payne–Hanek
  reduction and double-double series for the rest, correctly
  rounded; `log10` and the hyperbolic functions follow the
  pin's own formulas (fitted to its output) over those. The pin
  uses glibc, which is not correctly rounded everywhere, so
  0.01–0.3% of `exp` / `ln` / trig results (up to 2.4% of `acosh`
  near 1, up to 11% of `sinh` / `tanh` / `asinh` / `atanh`,
  through glibc's `expm1` / `log1p`) differ, mostly by 1–3 ulp
  (14 ulp for `tan` at the classic hard reduction argument).
  Exact `pow` results round half to even in hipp, while glibc
  misses many such ties (89 of 145 in the test corpus), so small integer powers are a
  hot spot visible in CLI output (`pow(9, 17)` is
  16677181699666568.0 in hipp, 16677181699666570.0 on the pin);
  STATUS.md lists the measured rates. Ungated fixes on the way:
  text → number conversion (column affinity, comparisons, `WHERE
  rowid = '<text>'`) trims only ASCII spaces and reads text to its
  first NUL, as the pin does (`char(160) || '5'` was stored as 5,
  `'5' || char(0) || 'x'` as text); the numeric prefix behind
  arithmetic and CAST reads bytes, so a non-UTF-8 tail no longer
  zeroes it (`x'38ff' + 0` is 8).

- **PRAGMA statements and `sqlite_schema`** (behind `sql`):
  `PRAGMA [main.]name [= value | (value)]` with the pin's value
  grammar and conversions; header values (`user_version`,
  `application_id` read and write inside transactions;
  `page_size` before the first write, or for the next VACUUM;
  `page_count`,
  `freelist_count`, `schema_version`, `encoding`,
  `auto_vacuum`); what hipp actually does (`journal_mode` is
  `delete`, `synchronous` FULL, `cache_size` its page pool,
  `busy_timeout`) — any write hipp cannot honour is refused, never
  pretended; the flags `foreign_keys` (enforced with the
  `foreign_keys` leaf; without it, while on, writes to tables in a
  foreign key are refused),
  `recursive_triggers` (honoured with `triggers`),
  `ignore_check_constraints` (really skips
  CHECK) and `case_sensitive_like` (really changes LIKE);
  `table_info`, `table_xinfo` (the pin's dequoted declared
  types, hidden generated columns, view column types),
  `index_list`, `index_info`, `index_xinfo`, `foreign_key_list`,
  `table_list`, `database_list` byte for byte, and
  `collation_list` (the pin's library's three; its shell adds
  two); `integrity_check` / `quick_check` with the pin's page
  checks — auto-vacuum pointer maps and header fields included
  — its record-header test ("database disk image is malformed")
  and its index seek, message shapes, order and limit (a full
  check of a table with an expression or partial index is
  refused without `expr_index`); the table-valued `pragma_table_info('t')` and
  friends in FROM with the pin's hidden-column arities;
  `sqlite_schema` / `sqlite_master` readable as ordinary tables
  under either name (writes refused). Unknown pragma names are
  ignored as on the pin; the pin's other known pragmas
  (`function_list`, `locking_mode`, ...) are refused. Ungated
  (review fixes included): CREATE of a `sqlite_*` name is
  refused; declared types are dequoted as the pin keeps them
  (affinity, the rowid alias and STRICT read that form: `"INTEGER"
  PRIMARY KEY` is the alias, `[x] TEXT` is NUMERIC);
  `PRIMARY KEY (a DESC)` as a table constraint is the rowid
  alias; FOREIGN KEY and PRIMARY KEY / UNIQUE column lists are
  validated (hipp wrote schemas the pin could not read); a table
  named with arguments is "not a function". The tiny profile grew
  232 B for these (STATUS.md). Gaps: the pin's schema hash order
  (`table_list` with more than three objects), correlated
  table-valued arguments, no temp database, cells off their page
  in a damaged file (STATUS.md).

- **Executed views and common table expressions** (behind
  `sql`): views are read wherever a table is — FROM, joins
  (NATURAL / USING / RIGHT / FULL), subqueries, `x IN view`,
  compound members, view-on-view, UPDATE ... FROM, INSERT ...
  SELECT, CREATE TABLE ... AS — with `CREATE VIEW v(a, b)`
  column lists, the pin's lazy CREATE VIEW (a missing table,
  a wrong-length column list or a cycle is the reader's error)
  and its refusals of writes to a view. `WITH [RECURSIVE]`
  before SELECT / VALUES / INSERT / REPLACE / UPDATE / DELETE
  and inside any subquery: several CTEs, forward references,
  column lists, the pin's materialization (a MATERIALIZED CTE,
  or one read twice that the pin does not flatten, is computed
  once and shared, as is a view a FROM clause names twice), the
  pin's scoping (a CTE
  shadows a table, never a DML target or a `main.` name; a view
  never sees the reader's CTEs). Recursive CTEs run the pin's
  queue — FIFO, or a priority queue (ties in arrival order)
  under the CTE's ORDER BY; UNION drops rows seen before;
  LIMIT / OFFSET count dequeued rows; several recursive SELECTs;
  a bounded reader (LIMIT through a WHERE or a join, a scalar or
  EXISTS subquery) stops an endless one — with its circular /
  multiple-reference / recursive-aggregate errors and its
  nesting limits (999 nested views or CTEs read, as on the pin,
  in the CLI; `Config::max_nesting`, default 250, bounds the
  stack a library caller needs). PRAGMA
  `table_info` and the CLI's `.schema` comment use a view's
  planned columns. Ungated fixes: tables, views and indexes
  share one namespace (CREATE TABLE over a view's name worked;
  tiny +48 B), `t.a` / `*` over two sources named `t` is
  ambiguous, a derived compound's column affinity is demoted
  across its cores, standard type names print as the pin's;
  review fixes: CREATE VIEW IF NOT EXISTS over an index's name
  errors, `t.rowid` over two `t`s is ambiguous, `SELECT *` with
  no FROM is "no tables specified", and RETURNING runs after
  each row's change (a DELETE's subqueries saw the row still
  there; uncorrelated ones were frozen at the first row). Gaps:
  where the pin's planner picks a co-routine or a flattening
  the model does not foresee, a random() body may be shared
  differently; mixed-affinity compound views compare by the
  demoted affinity where the pin pushes some WHERE terms into
  each core (STATUS.md).

- **Savepoints and maintenance statements** (behind `sql`):
  `SAVEPOINT` / `RELEASE [SAVEPOINT]` / `ROLLBACK [TRANSACTION]
  TO [SAVEPOINT]` with the pin's nesting (a SAVEPOINT outside a
  transaction starts one and releasing it commits; ROLLBACK TO
  keeps the savepoint open; names ignore ASCII case; "no such
  savepoint"), and **statement-level rollback**: a statement that
  fails inside a transaction now undoes only itself — also half
  way through a multi-row write — and the transaction stays open,
  as on the pin (the tiny profile keeps the old whole-bracket
  abort). The undo images live in the journal file past the
  largest rollback journal the transaction can write, so no page
  image is held in RAM and a crash still restores the
  transaction's start (real SQLite recovers those journals too).
  `VACUUM [main] [INTO expr]` rebuilds the file compactly with
  the pin's schema order, rowid rules and header values, a
  pending `PRAGMA page_size` applied, as one journaled
  transaction (`Config::vacuum_into` writes VACUUM INTO's image;
  the `std` default creates the file). `REINDEX [collation | table
  | index]`, and `ANALYZE` writing `sqlite_stat1` exactly as the
  pin computes it (hipp's planner does not read it; the pin then
  plans the same over hipp's file). Ungated fixes found on the
  way: two crash-recovery holes in the rollback journal (a page
  the transaction allocated, spilled and changed again was
  journaled, and replay stopped at that record; a freelist page
  freed and reused in one transaction was not journaled), a
  failed journal replay losing the sidecar (a panic next), and
  `CREATE INDEX ... (rowid)` (the pin's "no such column"); the
  tiny profile shrank 124 B. Review fixes: `OR ROLLBACK` and a
  constraint-level `ON CONFLICT ROLLBACK` end the whole
  transaction again; ROLLBACK TO across `PRAGMA page_size` keeps
  the new size (it panicked); DROP TABLE / DROP INDEX delete their
  `sqlite_stat1` rows (ungated); VACUUM INTO takes any text
  expression (`?`, `''`); a new rowid past 9223372036854775807
  takes an unused one. Gaps: whole-database ANALYZE row order (the
  pin's schema hash), VACUUM of a file with an expression or
  partial index without `expr_index`, VACUUM of an auto-vacuum file
  without the `autovacuum` leaf (refused: no pointer-map upkeep; the
  honest-gap sweep refuses every other write to such a file too, and
  the leaf lifts both) (STATUS.md).
- **ALTER TABLE, AUTOINCREMENT and TEMP tables** (behind `sql`):
  `ALTER TABLE t RENAME TO u`, `RENAME [COLUMN] a TO b` and `DROP
  [COLUMN] a` edit the stored schema texts token by token exactly
  as the pin does — byte-identical `sqlite_schema` rows for the
  table, its indexes (automatic index names too), `REFERENCES`
  elsewhere, CHECK / generated expressions and every view (the
  planner traces which names resolve to the table) — with the
  pin's refusals, and DROP COLUMN rewrites every row the way the
  pin's rewrite stores it. `AUTOINCREMENT` keeps the pin's
  `sqlite_sequence` (its CREATE text and root page order, the
  max-ever rowid rule, "database or disk is full" at i64::MAX,
  REPLACE / upsert / hand-edited rows), so files interoperate both
  ways. `CREATE TEMP | TEMPORARY TABLE / VIEW`, indexes on temp
  tables, `temp.` names and `sqlite_temp_schema` (bare or quoted)
  use a per-connection in-memory temp database (temp names first,
  transactions and savepoints applied to both, the connection's
  pragmas such as `foreign_keys` / `case_sensitive_like` holding
  there too, never written to the file, gone on close); a TEMP
  view may read main tables. `CREATE TEMP INDEX` is a syntax
  error, as on the pin. A failed ALTER leaves the session's
  catalog as the file is, and "database or disk is full" rolls the
  transaction back as the pin does. hipp-cli's `.schema` /
  `.tables` / `.dump` follow. Ungated
  fixes: a pin-made AUTOINCREMENT table is read-only below `sql`
  (an INSERT left the counter behind), DROP TABLE removes its
  `sqlite_sequence` row, `sqlite_*` tables can no longer be
  indexed, altered or dropped, and ADD COLUMN on a table with
  table constraints no longer writes unparsable text (tiny +28 B).
  Gaps: a temp UPDATE / DELETE that reads main tables is refused
  (statements mixing the two databases otherwise run since the
  honest-gap sweep), as is ALTER TABLE of a main table a TEMP view reads;
  files with triggers cannot be altered without the `triggers`
  feature (with it, their texts are rewritten as the pin's);
  re-created tables can get
  other root page numbers than on the pin (STATUS.md).
- **Parser and evaluator depth limits (crash hardening).**
  Ungated: within its limits no statement or stored schema text
  overflows the stack (100 000 nested parentheses, signs, calls or
  subqueries used to; in the tiny profile a long run of signs in
  `INSERT ... VALUES` did). hipp counts what the pin counts — the
  expression tree's height (`SQLITE_LIMIT_EXPR_DEPTH` 1000, with
  the heights of the expressions a subquery sits in added during
  resolution: 43 nested scalar subqueries read, 44 do not; a view
  or CTE body counts where it is read, so a chain of views each
  reading the last in a subquery adds up) and its LALR parser
  stack (2500 symbols: 2493 nested parentheses, 831 nested calls,
  414 nested FROM subqueries) — and refuses past them as the pin
  does ("Expression tree is too large", "Recursion limit"), at the
  pin's exact boundary in every probed context.
  `Config::max_expr_depth` / `max_parser_depth` bound the stack one
  statement's text needs (`Config::stack_budget(bytes)` derives
  them from a thread's stack; the defaults, 1000 and 500, fit a 2
  MiB thread in a release build); view and CTE bodies read inside
  one another add up to `Config::max_nesting` levels on top
  (default 250: an 8 MiB thread in release, about 32 MiB in a
  debug build). The CLI runs the pin's limits. The hot recursive
  walkers (parser, resolver, evaluator, FROM planner) were split
  so each nesting level costs a few hundred bytes in release. Also
  matched: 500 compound SELECTs (unless the last is a one-row
  VALUES), 1000 call arguments, 2000 columns in a table, index,
  result set (after `*`, RETURNING too), multi-row VALUES or
  UPDATE SET list, 2000 ORDER BY / GROUP BY terms (result sets and
  terms only where the pin resolves the select: not in an unused
  CTE or a CREATE VIEW body), 64 joined tables, the statement
  length. Ungated wrong-answer fixes: `ORDER BY 2 NOTNULL`,
  `-(-2)`, `+(2)` are ordinals as on the pin and `ORDER BY TRUE`
  is not; a schema row too large for page 1 next to the file
  header no longer panics (page 1 now splits as the pin's does).
  Tiny +252 B (STATUS.md).
- **Window functions** (new leaf feature `window`, in `max` —
  ADR 0002 — not `full`). `f(...) [FILTER (WHERE ...)] OVER
  (PARTITION BY ... ORDER BY ... frame)`, `OVER name` and the
  WINDOW clause with the pin's inheritance rules and refusals;
  ROWS / RANGE / GROUPS frames with every bound and EXCLUDE
  clause, RANGE offsets over mixed storage classes; row_number,
  rank, dense_rank, percent_rank, cume_dist, ntile, lag, lead,
  first_value, last_value, nth_value and every aggregate (the
  percentile family included) as window functions. The frame
  engine transcribes the pin's generated program (checked against
  its `EXPLAIN`), so sliding REAL sums round as they do there and
  rows come out in the pin's order (nested window classes, each
  sorting stably). Window calls in WHERE / GROUP BY / HAVING,
  inside aggregates, in write statements and in a trigger's WHEN
  are the pin's misuse; in a DEFAULT "not constant"; in a recursive
  CTE's recursive SELECT refused. ORDER BY terms naming window
  columns, compound ORDER BYs and GROUP BY feed the windows the
  pin's way, so tie-dependent values match. Max +45.2 KB; tiny and
  full unchanged (STATUS.md).

- **Triggers** (new leaf feature `triggers`, in `max` — ADR
  0002 — not `full`). CREATE [TEMP] TRIGGER (BEFORE / AFTER /
  INSTEAD OF, INSERT / DELETE / UPDATE [OF], WHEN) stored with the
  pin's text, DROP TRIGGER; bodies of INSERT / UPDATE / DELETE /
  SELECT with NEW. / OLD. (the pin's affinity and collation rules),
  RAISE(IGNORE / ROLLBACK / ABORT / FAIL) with the pin's
  resolution, conflict-clause override, recursive_triggers, a
  trigger depth limit (`Config::max_trigger_depth`: 24 by default,
  sized for a 2 MiB thread; the CLI sets the pin's 1000 programs),
  changes() / last_insert_rowid() frames,
  REPLACE and upsert interplay, and INSTEAD OF triggers that make
  views writable. ALTER TABLE rewrites trigger texts as the pin
  does (its `excluded.` quirk included). Pin-made files with
  triggers are read and written with their triggers firing; a
  build without the feature refuses writes to a triggered table
  (ungated, every profile — they used to skip the trigger
  silently). TEMP triggers on main tables are refused. Tiny +196 B,
  full +528 B (the ungated refusals), max +33.9 KB (STATUS.md).
  Review fixes: BEFORE UPDATE computes NEW's generated columns over
  the pin's new-column mask; a RAISE part-way through a body's
  streaming INSERT ... SELECT / DELETE / UPDATE keeps the rows
  before it; the whole trigger nest is checked at prepare; a depth
  failure in a transaction keeps the rows when no program can
  abort (the pin's missing statement journal); INSTEAD OF view
  writes take RETURNING and a rowid assignment; RETURNING
  subqueries are computed once unless they read the written table
  directly. Ungated with them: `0 AND x` / `x OR 1` short-circuit
  only over a lone 32-bit integer token (`-1 OR x` evaluates `x`),
  REPLACE deletes its victims in the pin's index order, and
  `RETURNING t.*` is refused as on the pin. Full +688 B, max +8.8 KB.
- **Foreign-key enforcement** (new leaf feature `foreign_keys`, in
  `max` — ADR 0002 — not `full`). With `PRAGMA foreign_keys = ON`
  writes are checked as the pin checks them — with its violation
  *counters*: immediate keys fail the statement at its end (even
  under OR FAIL / IGNORE), `DEFERRABLE INITIALLY DEFERRED` keys and
  `PRAGMA defer_foreign_keys` fail COMMIT / a committing RELEASE
  with the transaction left open, savepoints restore the counts,
  and a multi-row statement may fix its own violations (an orphan
  left with the pragma off can cancel one, the pin's quirk; a
  counter driven below zero fails as one above it). ON
  DELETE / ON UPDATE CASCADE, SET NULL, SET DEFAULT, RESTRICT and NO
  ACTION run as the pin's action programs (under ABORT, own
  `changes()` frames, the trigger depth limit, firing the child's
  triggers); composite and self-referencing keys with the pin's
  three comparison rules; "foreign key mismatch" / "no such table"
  when the write is prepared, over the whole cascade tree; REPLACE,
  upsert, RETURNING, INSTEAD OF trigger bodies, DROP TABLE's
  implicit DELETE and the pin's `isSetNullAction` quirk (replayed
  over its whole compile order); `PRAGMA foreign_key_check [t]` and
  `defer_foreign_keys`. Without the leaf the refusal stays.
  Tiny unchanged, full +800 B (the ungated deferrable /
  ADD COLUMN REFERENCES fixes), max +33.8 KB with the review fixes
  (STATUS.md).

- **JSON functions** (new leaf feature `json`, in `max` — ADR
  0002 — not `full`): `json()` / `jsonb()` with the pin's JSONB
  format byte for byte (its overwrite-in-place headers included),
  JSON5 input with canonical output, `json_array` / `json_object`,
  `json_extract` and the `->` / `->>` operators, `json_insert` /
  `replace` / `set` / `array_insert` / `remove` (`$[#]` appends),
  `json_patch`, `json_type`, `json_valid` (flags), `json_quote`,
  `json_array_length`, `json_error_position`, `json_pretty`, every
  `jsonb_` form, `json_group_array` / `json_group_object` (also as
  window functions), and `json_each` / `json_tree` (and `jsonb_`) in
  FROM with all their columns, correlated to the sources on their
  left. The JSON subtype rules, the pin's errors and its 1000-level
  nesting and path limits; every walk is iterative, text parses in
  linear time, and `json_each` rows hold handles into the document
  rather than copies of it (linear memory at 200,000 elements).
  Ungated: WHERE / ON / FILTER / HAVING / CHECK / CASE WHEN / iif
  conditions stop AND / OR / NOT early as the pin's jump code does
  (subquery operands last, correlated WHERE terms after the rest),
  and a WHERE clause's constant terms run once before any row, with
  the pin's constant propagation. Review fixes also bind trigger
  NEW / OLD inside table-valued function arguments. Sizes in
  STATUS.md.
- **Expression indexes, partial indexes and index-aware planning**
  (new leaf feature `expr_index`, in `max` — ADR 0002 — not
  `full`): `CREATE [UNIQUE] INDEX ... ON t(a + b, lower(c) COLLATE
  NOCASE DESC) [WHERE expr]` with the pin's definition errors
  (subqueries, non-deterministic functions, aggregates, window
  calls, `t.a` / `rowid` in key expressions, parameters), maintained
  by INSERT / UPDATE / DELETE / REPLACE / upsert (UNIQUE over the
  rows the WHERE admits; `ON CONFLICT (a + b)` and `ON CONFLICT (a)
  WHERE <the index's WHERE>` targets), PRAGMA index_list /
  index_info / index_xinfo (cid -2), integrity_check (a partial
  index's entry count included), REINDEX, VACUUM, ANALYZE and ALTER
  TABLE RENAME / RENAME COLUMN / DROP COLUMN as the pin; files the
  pin made with such indexes are read and written. The planner reads
  equality, IN (list or `IN (SELECT ...)`), range, IS NULL and
  OR-of-equality terms (and ORDER BY / GROUP BY / DISTINCT, NULLS
  FIRST / LAST included) through the rowid, PRIMARY KEY, UNIQUE,
  ordinary, partial and expression indexes in SELECT, subqueries,
  joins (lookups per outer row), UPDATE and DELETE, choosing the
  index the pin's cost model chooses without statistics, so rows
  come in the pin's order (covering index scans, ORDER BY ties in
  the index's direction, GROUP BY over a DESC index, an UPDATE OR
  FAIL's partial work). A point lookup in a 100,000-row table reads
  a few pages instead of the table, and a covering index's rows
  come from its entries without table lookups. Without the leaf such indexes in
  a file are refused for writes (never left stale) and scans read
  rowid order. Tiny unchanged, full −16 B, max +32.3 KB (STATUS.md).
- **WITHOUT ROWID tables** (new leaf feature `without_rowid`, in
  `max` — ADR 0002 — not `full`): `CREATE TABLE ... WITHOUT ROWID`
  (also with STRICT) stored exactly as the pin stores it — a
  clustered index B-tree keyed by the PRIMARY KEY (a repeated key
  column dropped, key collations and DESC ordering the tree) with
  the other stored columns after it, secondary indexes and UNIQUE
  constraints holding the key columns instead of a rowid, cells of
  at least 4 bytes, overflow by the index-cell rules — so files
  interoperate both ways and the pin's integrity_check passes on
  hipp's. The pin's rules and errors: PRIMARY KEY required, no
  AUTOINCREMENT, `WITHOUT <other>` refused, `rowid` / `oid` /
  `_rowid_` unknown everywhere (SELECT, WHERE, RETURNING, SET,
  INSERT columns, indexes, CHECK, trigger NEW / OLD), NOT NULL key
  columns, INTEGER PRIMARY KEY no rowid alias, `last_insert_rowid()`
  untouched. Every path works on them: INSERT, OR IGNORE / REPLACE,
  upsert (also updating the key), RETURNING, UPDATE moving a row
  whose key changes (two-pass, in key order, as the pin), DELETE,
  triggers, foreign keys with such parents and children,
  savepoints, crash recovery, joins, views, subqueries, PRAGMA
  table_info / table_list / index_list / index_info / index_xinfo
  (the key as `sqlite_autoindex_t_N`, origin `pk`), integrity_check,
  ALTER TABLE RENAME / ADD COLUMN / RENAME COLUMN / DROP COLUMN,
  DROP TABLE, VACUUM, REINDEX and ANALYZE. Scans follow the pin's
  planner for these tables (its cheapest covering index, newest on a
  tie, its UPDATE / DELETE costing). Without the leaf a file holding
  such a table is refused at open, as before. Fixed on the way: an
  `IS NULL` lookup through a UNIQUE index no longer counts as one
  row (GROUP BY groups came unsorted). Review fixes: an index
  delete rebalance that panicked or wrote a corrupt page when its
  new dividers were larger (also ordinary indexes) and two table
  insert-balance cases that grew by more than one page; NOCASE
  stops at a NUL byte as the pin's; `PRAGMA index_info(t)` of a
  WITHOUT ROWID table describes its key; DROP COLUMN of a last
  column; stored texts keep the whitespace after table options
  and indexes as the pin's do. Records and cells match the pin's
  byte for byte, but page allocation does not (a few more pages);
  index_xinfo prints collation names in upper case. Sizes in
  STATUS.md.
- **UTF-16 databases** (new leaf feature `utf16`, in `max` — ADR
  0002 — not `full`): files whose header text encoding is 2
  (UTF-16le) or 3 (UTF-16be) open, read and write as on the pin,
  and `PRAGMA encoding = 'UTF-16' | 'UTF-16le' | 'UTF-16be'` (or
  `UTF16...`, any case; `UTF-16` alone is little-endian, the pin's
  native order; `PRAGMA temp.encoding` too) chooses one while the
  database has no schema — header-only writes such as `PRAGMA
  user_version` leave it open, a schema change (even rolled back,
  TEMP too) fixes it, as on the pin — and `PRAGMA encoding`
  reports it. Records (and sqlite_schema's text)
  are translated at the B-tree boundary, so serial types count
  UTF-16 bytes and files interoperate both ways (the pin's
  integrity_check passes on hipp's and hipp's on the pin's, unpaired
  surrogates included: text is held losslessly, so index builds and
  copies keep the stored units; VACUUM keeps the encoding).
  The BINARY collation compares the UTF-16 bytes with memcmp as
  the pin does — UTF-16le orders by the low byte first, UTF-16be
  sorts astral characters below U+E000 — in ORDER BY, indexes,
  ranges, min / max, GROUP BY, compounds, windows and aggregates;
  NOCASE and RTRIM compare the UTF-8 form, as the pin defines them
  for UTF-8 only. hex() and CAST(... AS BLOB) give the UTF-16
  bytes, octet_length counts them, a blob read as text (CAST, ||,
  upper, LIKE, numbers from blobs, sum, group_concat, JSON
  documents) decodes from UTF-16 while the math functions and
  sign() give NULL for it, function results go through the pin's
  conversion (a lone surrogate becomes U+FFFD), and text with
  unpaired surrogates shows as the pin shows it. Without the leaf
  such a file is refused at open (`UnsupportedEncoding`), never
  misread. Fixed on the way (ungated): `unicode(char(65534))` is
  65534, and `sign()` of a BLOB or of a text that is not wholly a
  number is NULL, as on the pin. Gaps and sizes in STATUS.md.
- **WAL journal mode** (new leaf feature `wal`, in `max` — ADR 0002
  — not `full`): a database whose header says WAL (or with a
  non-empty `-wal` beside it) opens with the committed frames of
  its log replayed — the pin's header and frame checks, both
  checksum byte orders, stop at the first invalid frame, frames
  after the last commit ignored — so data the pin left
  uncheckpointed is visible. `PRAGMA journal_mode = WAL | DELETE`
  switches both ways (the pin's prefix match of the name, its
  in-transaction rules); commits append frames in the pin's layout
  (header, salts, cumulative checksums, commit frames, pages
  rewritten in place within a transaction, log restarts after a
  full checkpoint), so the pin reads, recovers and checkpoints
  hipp's log. `PRAGMA wal_checkpoint` (PASSIVE / FULL / RESTART /
  TRUNCATE / NOOP, the `busy|log|checkpointed` row), the automatic
  checkpoint (`wal_autocheckpoint`, 1000 by default),
  `journal_size_limit`, every `synchronous` level, and
  `Connection::close` (checkpoint, `-wal` and a stale `-shm`
  removed, as the pin's close; the CLI closes at exit). Single
  process, as the pin's `locking_mode=EXCLUSIVE`: the wal-index
  lives in heap memory, no `-shm` is written, and a database a
  live SQLite connection, or another hipp connection (this
  process's too), holds in WAL mode is refused (`DatabaseBusy`). An
  in-memory database (the CLI's `:memory:`,
  `MemStorage::memory_db`) stays in journal mode `memory`, as on
  the pin. An UPDATE writes only the pages the pin writes (an
  unchanged row or index entry is not rewritten), so checkpoint
  counts match.
  Savepoints and statement rollback work inside WAL transactions;
  crash injection at every write (frames, checkpoints, restarts,
  mode switches, close) recovers the last committed state, which
  the pin's integrity check accepts. Without the leaf a database
  whose header says WAL, or with a non-empty `-wal` beside it
  (which the pin opens in WAL mode whatever the header says), is
  refused at open (`WalUnsupported`), never read stale. Fixed on
  the way (ungated, `sql`): `PRAGMA journal_mode = w` (a prefix)
  is the pin's WAL, no longer a silent query; `PRAGMA synchronous
  = ...` inside a transaction is the pin's error. Gaps and sizes
  in STATUS.md.
- **Honest-gap sweep.** Constraint-level `ON CONFLICT` clauses
  (`NOT NULL` / `UNIQUE` / `PRIMARY KEY ... ON CONFLICT ROLLBACK |
  ABORT | FAIL | IGNORE | REPLACE`) are honoured instead of
  ignored (ungated from `sql`; below it FAIL / IGNORE / REPLACE are
  refused at parse) with the pin's precedence — an explicit `OR`
  clause, then the constraint's, then ABORT — its index order
  (REPLACE indexes and the rowid REPLACE last), NOT NULL ...
  REPLACE defaults, CHECK ignoring its clause, and the upsert /
  trigger / foreign-key interplay. Behind `sql`: `INDEXED BY name`
  / `NOT INDEXED` in FROM, UPDATE and DELETE (the pin's "no such
  index" and "no query solution"; with `expr_index` the planner uses
  exactly that index, or none); three-part column names
  `main.t.col` / `temp.t.col` everywhere the pin takes them, so
  pin-written CHECK and partial-index texts that use them are no
  longer opaque; and statements across the TEMP and main databases
  — main statements reading temp tables, `INSERT INTO temp.x SELECT
  ... FROM main.y` and `CREATE TEMP TABLE x AS SELECT ...` over main,
  TEMP views over both, pragma functions on temp objects. Without
  the `autovacuum` leaf VACUUM of an auto-vacuum file stays refused
  (no pointer-map upkeep), and since the review fixes so does every
  other write to such a file (they used to leave the pin's pointer
  maps stale); the leaf (below) writes them. Review fixes
  also keep views and triggers stored in main reading main only (a
  temp table no longer shadows their tables), refuse INDEXED BY on a
  trigger body's UPDATE / DELETE as the pin does, ignore the schema
  part of CHECK / partial-index names as the pin does, and tell
  `main.t` from `temp.t` in one statement. Gaps and sizes in
  STATUS.md.
- **Date and time functions** (leaf `datetime`, in `max`; too big
  for `full`'s budget). `date()`, `time()`, `datetime()`,
  `julianday()`, `unixepoch()`, `strftime()` (every conversion the
  pin knows; `%y` and the other unknown ones are NULL, as on
  3.54.1), `timediff()` and `CURRENT_DATE` / `CURRENT_TIME` /
  `CURRENT_TIMESTAMP` in any expression, as a model of the pin's
  `date.c`: its integer-millisecond Julian day, every time-value
  form (`YYYY-MM-DD`, `T` or spaces, `HH:MM[:SS[.F]]`, zones,
  `24:00`, Julian-day and Unix numbers, `'now'`), every modifier
  in its order and rounding (NNN units with the month / year
  overflow rules, `+/-YYYY-MM-DD HH:MM`, `start of` / 3.54's `end
  of`, `weekday N` and 3.54's backward `weekday -N`, `unixepoch`,
  `julianday`, `auto`, `localtime`, `utc`, `subsec`, `ceiling` /
  `floor`), byte for byte on a generated corpus in four time zones.
  `'now'` reads `Config::clock` once per statement execution;
  `'localtime'` / `'utc'` read the new `Config::tz_offset` hook (UTC
  without one); under `std` both default to the system clock and
  the `TZ` zone, read as glibc's `localtime` reads it (TZif files
  with glibc's checks, leap corrections and footer rules; POSIX TZ
  strings with its partial-parse results and `posixrules` fallback;
  no dependency). In a CHECK, index expression or generated column,
  reading either fails as on the pin. Ungated: the bare keywords are
  calls, never a table's own "current_date" column (they used to
  resolve as one), and `current_date()` / `current_date.x` are
  syntax errors. Review fixes: `'end of month'` in December 9999 or
  on an invalid value, and `'floor'` after a `24:00` overflow, now
  match the pin. Without the leaf the functions stay unknown.
- **ATTACH / DETACH and several databases** (leaf `attach`, in
  `max`). `ATTACH [DATABASE] expr AS expr [KEY expr]` (a file,
  `':memory:'`, `''` / NULL for a private temporary database) and
  `DETACH [DATABASE] expr` with the pin's limit of 10 and its
  errors (already in use, cannot detach main / temp, no such
  database, database is locked inside a transaction that used it,
  the encoding rule; bound parameters as operands);
  `PRAGMA database_list`, schema-qualified names (`aux.t`,
  `aux.t.col`, `aux.sqlite_schema`, `PRAGMA aux.x`, `VACUUM aux
  [INTO]`, `REINDEX` / `ANALYZE` / `ALTER` / `DROP` / `CREATE TABLE
  / INDEX / VIEW / TRIGGER aux.x`; a plain `ANALYZE` / `REINDEX`
  covers every database; views and triggers keep to their own
  database, WHEN clauses included) and the pin's unqualified-name
  order (temp, main, then attached in attach order). Not yet: TEMP
  triggers on main or attached tables, or writing schema-qualified
  tables (refused cleanly), and index access paths into another
  database's tables (they are read whole, once per execution). Each attached
  database is a connection of its own owned by the main one — the
  TEMP database's model made N-ary — so every feature works inside
  it; statements across databases (joins, subqueries, compounds,
  `INSERT INTO aux.t SELECT ... FROM main.t` both ways, UPDATE /
  DELETE reading other databases, `UPDATE ... FROM`, CREATE TABLE
  ... AS) read the other databases' tables as external sources.
  One transaction spans every database, savepoints included; a
  rollback-journal commit of several files is atomic through the
  pin's super-journal (`<main>-mjXXXXXX9XX`, the journal-tail
  pointer), and hot-journal recovery honours it — crash-tested at
  every write boundary, with the real pin recovering hipp's crashed
  files and hipp recovering the pin's. `no_std` callers supply an
  attached name's storage through `Connection::set_attach_hook`;
  `FileStorage` opens files. hipp-cli gains `.databases`.
- **Auto-vacuum files** (leaf `autovacuum`, in `max`). Files with
  pointer maps are written as the pin writes them: every entry
  (ROOTPAGE, FREEPAGE, OVERFLOW1, OVERFLOW2, BTREE) on the pin's
  pointer-map pages at every page size, derived from the trees and
  the freelist at each commit so every write path keeps them right;
  CREATE TABLE / INDEX put the root after the largest root, moving
  the page there out of the way; DROP TABLE / INDEX move the largest
  root into the freed slot and rewrite its `sqlite_schema.rootpage`;
  `auto_vacuum = FULL` moves pages from the end into the free pages
  and truncates at every commit (the pin's `autoVacuumCommit`);
  `INCREMENTAL` keeps them for `PRAGMA incremental_vacuum[(N)]` (the
  pin's argument rules, one empty row per page). `PRAGMA
  auto_vacuum` sets the mode of an empty database (writing its
  header at once, the mode surviving a ROLLBACK as on the pin),
  switches FULL and INCREMENTAL in place, and leaves a change to or
  from NONE to VACUUM, which (VACUUM INTO too) otherwise keeps the
  mode. The moves this leaf owns — root placement, DROP, FULL's
  commit, incremental_vacuum — choose the pin's pages, so given the
  same tree shapes the root pages, page counts and pointer maps are
  the pin's; where hipp's general B-tree shapes trees differently
  (delete merging, index splits, ALTER, UPDATE resizing rows,
  REINDEX) page counts can differ, with the same contents and valid
  maps. Each engine writes the other's files (attached ones too),
  integrity checks pass both ways (inside a transaction as well),
  and crashes during the page moves or incremental_vacuum recover
  (in hipp and in the pin) to the last commit. As on the pin, pages
  cut off the end get no journal record: the file is truncated only
  after the journal is deleted. Ungated: freelist trunks hold the
  pin's `usable/4 - 8` leaves. Without the leaf such files stay
  read only (`AutoVacuumUnsupported`).
- **Virtual tables, `generate_series`, `dbstat` and the R*Tree**
  (leaves `vtab` and `rtree`, in `max`). One internal module
  abstraction modelled on the pin's `sqlite3_module` (Rust traits,
  no C ABI: xBestIndex with constraint usage / omit / idxNum /
  idxStr / cost, filter cursors, xUpdate, hidden columns, eponymous
  modules, shadow-table names). `CREATE VIRTUAL TABLE [IF NOT
  EXISTS] t USING m(args)` and its DROP store and remove exactly the
  pin's `sqlite_schema` rows, and refuse names the shadow tables
  need or a module reserves; a file with a virtual table of a
  module hipp lacks opens, its shadow tables read as ordinary
  tables (fts3 / fts4's `'quoted'` declarations included), VACUUM
  and integrity_check run, and only statements reading or writing
  that table are refused ("no such module"). `generate_series(start, stop,
  step)` as the pin's shell ships it (hidden `start` / `stop` /
  `step`, its constraint handling on `value`, its 64-bit
  arithmetic and its "first argument" error) — hipp holds a
  cursor's rows in memory, so more than `Config::max_vtab_rows`
  (about 4 million) fail cleanly (`VtabTooManyRows`) where the pin
  streams; `dbstat` byte for byte on the same file (pages, overflow
  chains, `aggregate`, `schema`, `name`; `main`, `temp` and attached
  databases). `rtree` / `rtree_i32` of 1-5 dimensions with
  `+aux` columns: the shadow tables `%_node` / `%_parent` /
  `%_rowid` are written byte-compatibly with the pin (node size,
  ChooseLeaf, the R*-tree split, delete with node removal and
  reinsertion, node numbering, float32 rounding outward), so
  either engine continues the other's files and each one's
  `rtreecheck()` passes on the other's (hipp's PRAGMA
  integrity_check checks the shadow tables as tables, not the
  tree: use `rtreecheck()`); `changes()` / `total_changes()` count
  as the pin's (its internal shadow-row statements included); queries
  with `=`, `<`, `<=`, `>`, `>=`, BETWEEN and rowid lookups through
  xBestIndex; the pin's constraint errors under every conflict
  clause; `rtreecheck()`, `rtreenode()`, `rtreedepth()`; ALTER
  TABLE RENAME. Shadow tables are read only, as under the pin
  shell's defensive mode, except under `PRAGMA writable_schema`,
  which replays a `.dump` (hipp-cli's is the pin shell's, byte for
  byte, `.dump PATTERN` included, and either engine reloads either
  engine's).
- **FTS5 full-text search** (leaf `fts5`, in `max`; on the `vtab`
  framework). `CREATE VIRTUAL TABLE t USING fts5(...)` with columns,
  `UNINDEXED`, `prefix=`, `content=` / `content_rowid=` (external
  content kept by triggers), `content=''` (contentless),
  `columnsize=0`, `detail=full|column|none` and the tokenizers
  `unicode61` (`remove_diacritics 0/1/2`, `tokenchars`,
  `separators`, `categories`; the Unicode tables generated in-crate),
  `ascii`, `porter <inner>` and `trigram` (`case_sensitive`,
  `remove_diacritics`). The index is written byte for byte as the
  pin writes it — `%_data` (structure record, averages, leaf pages,
  doclist indexes), `%_idx`, `%_content`, `%_docsize`, `%_config` —
  with the pin's pending-term flush points (commit, savepoints,
  statement savepoints, rowid order, `hashsize`) and its segment
  merging (automerge, crisismerge, usermerge, `merge`, `optimize`),
  so either engine continues the other's files and the pin's
  `INSERT INTO t(t) VALUES('integrity-check')` passes on hipp's. A
  pin-made `contentless_delete=1` table's tombstones hide its deleted
  rows from hipp's queries and are applied by hipp's merges (the
  pin's `deletemerge` too); the deletes hipp cannot write as the pin
  does — tombstones, and deletes under a pin-set `secure-delete=1` —
  are refused rather than written another way.
  Queries: the full query syntax (phrases, `prefix*`, `^`,
  `NEAR(..., N)`, `col:` / `{a b}:` / `-col:` filters, AND / OR /
  NOT with the pin's precedence and implicit AND, parentheses,
  quoting), `MATCH`, `t = 'q'` and `t('q')`, LIKE / GLOB patterns on
  trigram tables, rowid ranges, `ORDER BY rank` with `rank MATCH
  'bm25(...)'` and the `rank` setting, MATCH under an OR (the pin's
  multi-index OR) and in a LEFT JOIN's ON, `bm25()` with weights,
  `highlight()`, `snippet()`, and `fts5vocab` tables (`row`, `col`,
  `instance`). The special INSERT commands `delete`, `delete-all`,
  `rebuild`, `optimize`, `merge`, `automerge`, `crisismerge`,
  `usermerge`, `pgsz`, `hashsize`, `rank`, `secure-delete` (stored;
  deletes refused while it is on) and `integrity-check`; INSERT ...
  RETURNING and UPDATE ... FROM; ALTER TABLE RENAME. Transactions,
  savepoints and crashes recover the shadow tables like any other
  table's pages.

- **EXPLAIN QUERY PLAN** (leaf `explain`, in `max`).
  `EXPLAIN QUERY PLAN <statement>` returns the pin's four columns
  (`id`, `parent`, `notused`, `detail`) describing the plan hipp
  executes, in the pin's words — SCAN / SEARCH loops with INTEGER
  PRIMARY KEY, PRIMARY KEY, [COVERING] INDEX and the probed keys,
  LEFT-JOIN / RIGHT-JOIN, the USE TEMP B-TREE sorters, (CORRELATED)
  SCALAR / LIST SUBQUERY with the pin's select numbers, CO-ROUTINE /
  MATERIALIZE, recursive CTEs, MERGE / COMPOUND QUERY trees, window
  co-routines, VIRTUAL TABLE INDEX, VALUES and constant rows, and the
  UPDATE / DELETE / INSERT plans — and the CLI prints the shell's
  tree. The statement is planned, never run; plain EXPLAIN (bytecode)
  is refused. Where hipp's strategy differs (no flattening, join
  reordering and automatic indexes before v0.17's `planner`,
  MULTI-INDEX OR, min/max lookup) it
  prints what it runs (STATUS.md). Comparing plans fixed access-path
  choices that ordered rows unlike the pin (window functions,
  compound members, GROUP BY / DISTINCT over a permuted index, rowid
  IN lists, `IS NULL` on NOT NULL keys); the review of the unit found
  that two of these introduced wrong answers of their own (a window
  query's outer ORDER BY on an unselected column was ignored, and a
  UNION's LIMIT 1 shortcut returned another first row), both fixed
  before release, along with the `f(ORDER BY)` sorters, upsert
  subqueries, write-loop COVERING and TEMP / attached qualifiers in
  the plans.

## What v0.13 does

- Everything v0.12 does (below), plus **uncorrelated-
  subquery hoisting** — a performance milestone that
  also fixes a wrong answer. It is *not* feature-gated:
  it changes no dialect surface, only how often existing
  subquery plans execute (the tiny profile has no
  subqueries either way). At plan time the scoped
  resolver's absolute slot numbering decides whether a
  subquery plan references any enclosing scope; if not,
  the executor computes it on first use in a statement,
  caches the scalar / IN-list / EXISTS result under the
  plan's node id, and serves every other outer row (or
  group row, inside aggregate arguments) from the cache.
  Probing the pin first showed why this is also
  correctness: SQLite fixes an uncorrelated subquery's
  value for the whole statement, so `UPDATE t SET a =
  (SELECT sum(a) FROM t)` writes the pre-statement sum
  to every row — the per-row engine wrote a climbing
  6, 11, 20 instead. The win is asserted, not timed:
  `Connection::subquery_executions()` counts subquery
  plan executions, and the suite pins an uncorrelated
  node over 200 rows at exactly 1 (a correlated control
  stays at exactly 200).

## What v0.12 does

- Everything v0.11 does (below), plus **real-literal
  parsing exactness** — the last recorded
  silent-wrong-answer divergence (RISKS R5). It is
  *not* feature-gated: like the v0.8 / v0.10 fixes it
  corrects claimed paths. Probing the pin first showed
  the old parser was off by ~41 ulp at `1e308` (its
  17-digit accumulation plus chunked power-of-ten
  scaling), and that the pin's own conversion is not
  textbook IEEE rounding on long inputs: significant
  digits accumulate into a `u64`, the first digit that
  would overflow it ends the input (later digits are
  ignored entirely), and the truncated decimal is then
  converted exactly, ties to even. hipp now performs
  that same conversion — exact big-integer rational
  arithmetic in `no_std` — for literals and, through
  one shared converter, for `CAST` / affinity /
  rowid-literal text conversions. Integer text past
  `i64` under NUMERIC / REAL affinity now converts to
  a REAL too, as the oracle's does. A side effect the
  ratchet likes: dropping `core`'s f64 text parser from
  the tiny link makes the tiny build 17,140 B smaller.

## What v0.11 does

- Everything v0.10 does (below), plus **subqueries inside
  aggregate arguments** behind what is now the `sql` feature
  (default on; the tiny profile excludes it). The
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
  of the claimed profiles behind what is now the `sql` feature
  (default on; the tiny profile excludes it):
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
  correctness-and-completion pass behind what is now the `sql`
  feature (default on; the tiny profile excludes it):
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
  completion behind what is now the `sql` feature (default on;
  the tiny profile excludes it): `OR REPLACE` /
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
  expressions behind what is now the `sql` feature (default on,
  implies `write`; the tiny profile excludes it): scalar subqueries,
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
  `write` feature (default on, implies `query`; the tiny profile
  excludes it):
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
  `query` feature (default on; the tiny profile excludes it):
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
  seam. Expression and partial foreign indexes are read as
  opaque metadata; writes to their tables are refused rather
  than allowed to desynchronise them. (Since v0.16, NOCASE /
  RTRIM-collated indexes are maintained, not opaque, and with
  `expr_index` expression and partial indexes are too.)
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

| Gate | Budget | Measured (0.16.0) |
|---|---|---|
| Linked `.text` + `.rodata` (thumbv7em) | ≤ 128 KiB | **76,960 B** (`full` 481,240 B of 512 KiB, `max` 968,384 B of 1 MiB, `xl` 1,065,180 B of 1.25 MiB; ADR 0003 / 0004) |
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
`.schema`, `.dump`, `.open`, `.databases` (v0.16, with attached
databases in `.schema` / `.tables`), `.parameter` (v0.16: `set`,
`unset`, `clear`, `list`, `init`, bound by name as the oracle
shell binds them), `.exit`. Its first job is
differential testing. It is a separate crate and contributes
0 bytes to the core's tiny build.

## Non-goals (SPEC §3)

Each leaf left out of a build refuses what it would add, never
misreads it: without `wal` a WAL file (or a non-empty `-wal`) is
refused (`WalUnsupported`); without `without_rowid`, `utf16` or `vtab`
a file holding such tables or text is refused at open; without
`autovacuum` an auto-vacuum file is read but not written; without
`attach`, ATTACH is refused; without `explain`, EXPLAIN is. Without
`indexes`, `query`, `write` and `sql` (the tiny profile) there are no
indexes, joins, subqueries, UPDATE / DELETE or views.

Not planned in any profile: multi-process locking or a shared `-shm`
wal-index (WAL is single-process; one connection per database); a
shared page cache (a file is attached once); plain EXPLAIN's
bytecode listing (hipp has no VDBE); loadable
extensions and application-defined SQL functions; and **no `sqlite3_*`
C ABI, ever** (D3): hipp is a Rust library, not a drop-in
replacement.

## Layout

A workspace: the zero-dependency core crate at the root plus
`hipp-cli/`. Core `src/`: `arena` (scoped bump arenas — the only
`unsafe`), `storage` (async trait + Mem/File impls), `pager`
(page cache, journal protocol, commit machine), `btree`,
`record` (serial codec, affinity), `schema` (catalog), `sql/`
(lexer, parser, AST, parameter binding), `query` (resolved-expression evaluator,
v0.4), `agg` (aggregate folding: the pin's sum / percentile
rules, aggregate ORDER BY and DISTINCT, v0.16), `func` (printf and
the v0.16 scalar function library), `window` (window functions:
the pin's frame engine, feature `window`, v0.16), `json` (the JSON
functions and JSONB, feature `json`, v0.16),
`rules` (CHECK / generated-column / STRICT / DEFAULT
rules and the generated-column row codec, v0.16), `session`
(Connection/Statement, planner, tree-walking executor;
`session/pragma` the PRAGMA statements and integrity check,
`session/cte` view / CTE expansion and the recursive queue,
`session/maint` savepoints, VACUUM, REINDEX and ANALYZE,
`session/trigger` trigger programs, `session/json_tvf` the
`json_each` / `json_tree` sources, v0.16; `sql/trig` the NEW. /
OLD. binder), `wr` (WITHOUT ROWID trees behind the rowid-shaped
B-tree interface, feature `without_rowid`, v0.16), `utf16`
(UTF-16 text conversions and the record translation of UTF-16
files, feature `utf16`, v0.16),
`datetime` (the date and time functions, feature `datetime`,
v0.16), `autovac` (auto-vacuum pointer maps and page moves,
feature `autovacuum`, v0.16), `systime` (the host clock and time zone, feature `std`),
`error` (remedy-named errors).
Design: `docs/SPEC.md` (rev. 0.3) and `docs/RISKS.md`. Build
status and acceptance evidence: `STATUS.md`.

## License

Apache-2.0.
