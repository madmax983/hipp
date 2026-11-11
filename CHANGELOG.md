# Changelog

All notable changes to hipp. Versions follow the milestones in
`STATUS.md`; the oracle pin is SQLite 3.54.1 throughout.

## Unreleased (0.17)

### Table-valued functions beside TEMP tables (wave 15, `sync_readers` review fix)

- The sync source readers (unit `sync_readers`) were not built:
  `s3_probe`'s verdict is stop.
- Fix (ungated, `sql`; `generate_series` / `dbstat` with `vtab`): a
  TEMP write (INSERT ... SELECT, UPDATE / DELETE / INSERT VALUES with a
  subquery, CREATE TEMP TABLE ... AS, a CTE feeding the INSERT), a
  query over temp tables or a TEMP view reading `generate_series`
  failed with `UnknownFunction` (`dbstat`: `UnknownTable`); one
  reading a pragma function that names a main table
  (`pragma_table_info('m')`) read nothing. The temp database now
  computes `generate_series`; `dbstat` and pragma functions are
  main's (but for a pragma function whose lone argument names a temp
  object). A temp UPDATE / DELETE / INSERT VALUES reading `dbstat` or
  such a pragma function is refused (`Unsupported`), as one reading
  main tables is.
- Sizes: tiny 76,952 (0), full 478,212 (-8), max 964,684 (-24), xl
  1,061,344 (-88); ratchets lowered.

### The bytecode VM: Stage 3 measured first, verdict stop (wave 15, unit `s3_probe`)

- `ci/flip_probe.sh --s3` (`--cfg hipp_s3_probe` with the hidden `vm`
  feature; build-only, not in CI) measures the combined Stage 2 + 3
  flip's best case: the write executors, trigger and FK runners and
  the VM's async source readers cut, the B-tree write primitives kept:
  full 461,824 / max 868,732 / xl 962,336 B; `--s3=<category>` for
  the per-category inventory.
- Verdict: stop. With the replacement at Stage 2's measured ratio
  (1.9x) the flip projects +43.3 / +115.9 / +127.1 KB over the full /
  max / xl ratchets; going needs a VM write path at about 0.3x the
  code it replaces (STATUS.md, ADR 0005 amendment).
- No behaviour or shipped size change: tiny 76,952, full 478,220, max
  964,708, xl 1,061,432.

### The bytecode VM: the flip retry, not taken; REAL affinity through subqueries (wave 14, unit `vm_flip` retry)

- The flip (the VM as the only SELECT path) was not taken: the flip
  probe is still over every ratchet (full +16,548, max +46,668, xl
  +53,060 B), so nothing of the legacy executors or the `vm` feature
  changed (STATUS.md).
- Fix (ungated, `sql`; both executors): a FROM subquery, view or CTE
  column whose compound affinity is REAL reads another core's
  integers as reals, as the pin's `OP_RealAffinity` does (`SELECT x
  FROM (SELECT real_c x FROM t UNION ALL SELECT 1)` is `1.0`); a
  recursive CTE's queue reads alike; a plain subquery the pin does not
  flatten too (with `planner`).
- Fix: a compound column's affinity comes from the first core whose
  item has one (`SELECT 1 x UNION SELECT real_c ...` is REAL), in
  derived tables, CREATE TABLE ... AS, recursive CTEs and PRAGMA
  table_info of a view.
- Fix: a scalar subquery's affinity in CREATE TABLE ... AS and PRAGMA
  table_info is its rightmost core's, as the pin's (and hipp's own
  comparisons') — hipp read the leftmost.
- Tests: `tests/compound_affinity_v17.rs` (repros, shapes, 240
  generated compounds, each engine), `hipp-cli/tests/cli_v17_compound_affinity.rs`.
- Sizes: tiny +0, full +232 B (478,020), max +608 B (964,684), xl
  +272 B (1,061,416); ratchets raised to the measured figures.

### The bytecode VM: shrinking it before the flip (unit `vm_shrink`, hidden feature `vm`)

- A reproducible flip probe: `ci/flip_probe.sh` builds full / max /
  xl with `vm` and `--cfg hipp_flip_probe`, which cuts today's read
  executor off where wave 13's measurement did, and prints each size
  next to its ratchet (build-only; not part of CI). STATUS.md records
  the starting figures and the largest VM, driver and legacy symbols.
- The expression evaluator is compiled once (for `Error<()>`) in
  every build with `vm`, as it already was with `window`: the full
  probe −14,476 B.
- One code generator for query plans: `vm_core`'s single-table
  generator is gone, the join generator compiles every SELECT plan
  (it now also leaves a FROM-less statement's ORDER BY terms
  uncomputed, as the pin does and the removed generator did; today's
  executor computes them and fails where a term does). With a
  subquery anywhere in such a statement the VM now answers as the
  pin, where base 5b67982's VM raised the term's error (vm-only;
  tests in `tests/vm_v17.rs`). The builder's
  common calls stay out of line; the driver awaits its row-list
  readers through one boxed future. The probe: full 512,764 →
  494,336 B, max 1,017,176 → 1,010,744 B, xl 1,121,556 → 1,114,204
  B — still 16.5 / 46.7 / 53.1 KB over the ratchets, so the flip
  stays out of reach (STATUS.md has the gap per profile).
- Plain EXPLAIN (with `vm`) lists the join generator's program for a
  single-table SELECT.
- Sizes: every shipped profile unchanged.

### The bytecode VM: review fixes of unit `vm_rest` (hidden feature `vm`)

- A window query whose only source is a view, a CTE, a FROM subquery
  or a table-valued source (json_each, generate_series, fts5 ...)
  computed its windows over only the rows its LIMIT + OFFSET kept
  (`SELECT value, count(*) OVER () FROM json_each('[1,2,3,4]') LIMIT
  2` gave 2 for 4), and inside a scalar subquery over one row: the
  reader's bound no longer reaches the source of a window query, as
  in today's executor.
- `Config::max_vtab_rows` again bounds a generate_series cursor
  wherever today's executor holds its rows: a materialised CTE, a
  compound's cores, an IN list, a FROM subquery, a scalar or EXISTS
  subquery, a RIGHT / FULL join, a window query, a trigger program.
  The VM read such a cursor row by row, uncounted, and its memory grew
  with the series; it now reads it whole under the bound, as today's
  executor does, and fails with `VtabTooManyRows` where it fails.
- Sizes: every profile unchanged (no profile enables `vm`).

### The bytecode VM flip (ADR 0005 Stage 2, unit `vm_flip`): measured, not taken; review fixes

- The flip was not made: with the VM as the only SELECT path and the
  legacy read executors deleted, every profile measured larger than
  its ratchet (best case: full +19,252 B, max +51,500 B, xl +58,540
  B), and the unit's rule is to stop rather than let a profile grow.
  The hidden `vm` feature and today's executors stay; STATUS.md has
  the measurements.
- Fix (max, xl): an enclosing query's aggregate that a nested
  aggregate query uses as an index or rowid bound (`SELECT (SELECT
  count(*) FROM u WHERE u.k <= max(t.a)) FROM t` with an index on
  `u.k`, `w.id <= max(t.a)` on an INTEGER PRIMARY KEY) is answered
  like the pin; it failed with `MisusedAggregate`. The folded value
  now reaches the nested plan's access path too.
- Fix (every profile with `query`): a repeated min() / max() no
  longer decides the bare columns (`SELECT max(a), min(a), max(a), a
  FROM t` read the max() row; the pin reads the min() row, its
  aggregate list holding each aggregate once).
- Under the hidden `vm`: such a bound now runs on the VM; one in the
  arguments of a table-valued, virtual or WITHOUT ROWID source falls
  back to today's executor (it read as NULL on the VM).
- Sizes: tiny 76,952 B (unchanged), full 477,788 B (+304), max
  964,076 B (+504), xl 1,061,144 B (+648).

### The bytecode VM: windows, virtual sources, the rest of SELECT and EXPLAIN (ADR 0005 Stage 2, unit `vm_rest`)

- Under the hidden feature `vm`, every SELECT now runs on the VM:
  window functions (today's frame engine, `window::compute`, over the
  kept rows or groups, units output in window order), virtual and
  table-valued sources (generate_series, dbstat, rtree, fts5,
  fts5vocab, json_each / json_tree, the `pragma_*` functions) as
  driver-awaited opcodes over today's readers with the streamed
  semantics (`Config::max_vtab_rows`, side-table release, an inner
  virtual table filtered again per outer row), `sqlite_schema`,
  WITHOUT ROWID tables, TEMP and attached tables, and the SELECTs of
  trigger programs and foreign-key actions. The one shape still
  handed to today's executor is one it refuses itself (an enclosing
  query's aggregate over a subquery argument); a strict run of the
  whole workspace suite (`RUSTFLAGS="--cfg hipp_vm_strict"`) passes.
- Plain `EXPLAIN <statement>` (with `explain` and `vm`) lists hipp's
  own program in the pin's eight columns (`addr`, `opcode`, `p1` ..
  `p5`, `comment`); without `vm` it is still refused. EXPLAIN QUERY
  PLAN is unchanged. No profile enables `vm`: every profile's size is
  unchanged.

### The bytecode VM: review fixes of unit `vm_subqueries` (hidden feature `vm`)

- min() / max() over a subquery argument or FILTER (the group's
  bare columns from its extremum row) and the query's own aggregates
  inside its subqueries (`SELECT (SELECT sum(t.a) FROM u) FROM t`,
  `HAVING (SELECT s) > 1`) now run on the VM; they fell back whole.
- An uncorrelated subquery's code is shared by every use of it
  (`Gosub` / `Return`) and code generation stops at an instruction
  budget: nested ORDER BY / GROUP BY / HAVING aliases of a subquery
  made the program grow exponentially, and a 749-byte statement ran
  the process out of memory before it fell back.
- Recorded as intended: on the VM a scalar or EXISTS subquery stops at
  its first row, as on the pin, where today's executor still tests
  every candidate row's WHERE (an error there, `random()` draws and
  `subquery_executions` differ). Every profile's size is unchanged.

### GROUP BY with ORDER BY and LIMIT: the right groups (review fix of unit `vm_joins`, every profile with `query`)

- A grouped query with a LIMIT stopped after the LIMIT's groups in
  GROUP BY order even when its ORDER BY sorted them again, so it
  returned the wrong groups: `GROUP BY b ORDER BY b NULLS LAST LIMIT
  2` kept the NULL group, and `GROUP BY b ORDER BY b DESC LIMIT 2`
  over an index on `b` kept the two smallest. hipp now stops early
  only where the pin does: the ORDER BY is the GROUP BY (either
  direction, the default NULLs placement) and the groups come in its
  order — sorted, or from an index read the way that delivers it.
- As on the pin, such an index (or the rowid) is now read backwards
  for `ORDER BY ... DESC`, so each group's rows come in that order
  too (`group_concat`), later groups' aggregates are not finalized
  (no error from them), and EXPLAIN QUERY PLAN shows a `USE TEMP
  B-TREE FOR ORDER BY` where the index does not deliver the ORDER BY
  (mixed directions). The VM (hidden feature `vm`) follows the same
  rule. Sizes: tiny unchanged, full +16 B, max +328 B, xl +760 B.

### The bytecode VM: subqueries, compounds, CTEs (ADR 0005 Stage 2, unit `vm_subqueries`)

- Under the hidden feature `vm`, the VM now runs scalar, IN and
  EXISTS subqueries (correlated and uncorrelated; in result columns,
  WHERE, ON, ORDER BY, HAVING, LIMIT / OFFSET, index probes and
  aggregate arguments), FROM subqueries, views and CTEs the flattener
  leaves, UNION / UNION ALL / INTERSECT / EXCEPT and VALUES, and
  recursive CTEs, as inline code in one program: today's evaluation
  order and laziness, an uncorrelated subquery once per statement, a
  failing subquery's error raised only where its value is read, and
  `Connection::subquery_executions` counting as today (except that a
  scalar or EXISTS subquery stops at its first row, as on the pin; see
  the review fixes above).
- On the VM a UNION ALL chain streams its rows (those before a failing
  row are returned, a LIMIT stops it, OFFSET rows are not computed),
  as the pin does; today's executor materialises compounds. Still no
  profile
  enables `vm`: every profile's size is unchanged.

### The bytecode VM: joins, index access, aggregates (ADR 0005 Stage 2, unit `vm_joins`)

- Under the hidden feature `vm`, the VM now runs SELECTs over any
  number of rowid tables joined in the planner's order (inner, LEFT,
  RIGHT, FULL, CROSS, NATURAL / USING, semi-joins), reading each
  through its access path — sync, restartable index cursors for
  equality, range and IN lookups, covering, expression and partial
  indexes, MULTI-INDEX OR, skip-scans, the min() / max() early-out,
  automatic indexes and Bloom filters — then aggregates (the streamed
  fold or GROUP BY groups, HAVING), DISTINCT, ORDER BY and LIMIT /
  OFFSET, with today's executor's rows, order and bare columns.
- On the VM, a RIGHT / FULL JOIN and a GROUP BY return the rows before
  a failing one, as the pin does (today's executor returns none), and
  HAVING terms the pin moves into the WHERE clause are tested per
  row. Still no profile enables `vm`: every profile's size is
  unchanged.

### The bytecode VM, behind a hidden feature (ADR 0005 Stage 2, unit `vm_core`)

- A register VM in `src/vm/` (16-byte `p1`-`p5` instructions with a
  side table, a builder with labels and an `Init` prologue, table and
  sorter cursors, the dispatch loop) runs SELECTs over one rowid
  table or none: WHERE, any subquery-free expression (one `Eval`
  opcode over `query::eval`), ORDER BY, LIMIT / OFFSET, full scans
  and rowid equality / range access. Its page reads are synchronous
  and restartable (`PagerCore`: a miss changes nothing; the driver in
  `Statement::step` reads the page and runs the opcode again), so a
  dropped step future is safe.
- Behind the hidden feature `vm`, which no profile enables: every
  profile's size is unchanged. Under `vm`, every other statement
  runs on today's executor; `Config::vm_strict` makes a SELECT the
  VM does not cover an error (`Error::VmFallback`) for tests.
- On the VM, three no-FROM SELECTs match the pin where today's
  executor errors: an OFFSET skipping an erroring row, an erroring
  ORDER BY term (the pin never computes it), and LIMIT 0 (nothing,
  not even the WHERE or OFFSET, is computed); so does `LIMIT 0 OFFSET
  <error>` over a table.
- Review fixes: the VM no longer needs a page-cache frame per B-tree
  level. A miss copies the pages the opcode had read, so a SELECT on
  the VM runs at every cache size today's executor runs at (one
  frame included) instead of failing with `CacheExhausted` on trees
  taller than the cache, and index keys with overflow chains longer
  than the cache read too. Asking `Statement::runs_on_vm` before the
  first step no longer turns `Config::vm_strict` off for that
  statement.

### Breaking: feature renames

Cargo features are named for what they enable, not for the milestone
that added them (ADR 0006, `docs/adr/0006-features-name-capabilities.md`).
The profiles keep their names and contents: builds that use `default`,
`full`, `max` or `xl` are unaffected; builds that enable an old name
directly must switch to the new one (Cargo refuses the old names).

| old feature | new feature | enables |
|---|---|---|
| `v04` | `query` | joins, aggregates + GROUP BY, the expression evaluator, CHECK / DEFAULT / generated columns / STRICT, ALTER TABLE ADD COLUMN, inert views |
| `v05` | `write` | UPDATE, DELETE, DROP TABLE (implies `query`) |
| `v06`, `v07`, `v08`, `v09`, `v10`, `v16` | `sql` | subqueries; conflict clauses, UPDATE FROM, RETURNING, upsert; INSERT ... SELECT; compounds and derived tables; subqueries in aggregate arguments; the v0.16 core-dialect completion (implies `write`) |
| `join_order`, `multi_index`, `flatten` | `planner` | the pin's join planner, index strategies and query flattener (in `xl`; implies `sql`, `expr_index`) |

- `indexes`, `params`, `math`, `std` and every other leaf keep their
  names; every leaf that required `v16` requires `sql`.
- `cfg` predicates were rewritten under the new names and simplified
  (`any(feature = "v05", feature = "indexes")` -> `any(feature =
  "write", feature = "indexes")`, `all(feature = "v07", feature =
  "foreign_keys")` -> `feature = "foreign_keys"`, ...). Code no
  surviving feature combination compiles was deleted: the IN-cost
  rule for `join_order` without `multi_index` (`xplan`), the empty
  compound merge key for `expr_index` without `v09` and the
  derived-source fallback for `v09` without `v16` (`session`), a
  `v06` subquery check inside the `not(v16)` executor, a `not(v16)`
  derived-table read inside a `v09` block, the pre-`flatten` /
  pre-`multi_index` fallbacks inside the planner modules (`xjoin`,
  `session::join`), a `not(v07)` RETURNING fallback inside the `v16`
  INSERT ... DEFAULT VALUES, a NOT NULL argument
  and two assertions of the tiny profile's code that a `v07` gate kept
  out of every build; attributes that repeat the gate of the block
  they sit in were dropped.
- `ci/feature-sets.txt`: `query`, `write`, `sql`, `indexes,query`,
  `indexes,write`, `query,math`, `sql,<leaf>` for each leaf and one
  `sql,indexes,expr_index,planner` line.
- No behaviour change and no compiled-code change: the tree links to the same
  sections as a purely mechanical rename of the previous tree (`.text`
  and `.rodata` byte-identical for `query`, `write`, `max`, `xl`).
  tiny 76,124 B and full 476,476 B are unchanged; max 962,300 B (+8)
  and xl 1,058,824 B (+16) differ by inter-function alignment padding
  only (every symbol's size is unchanged), because a feature's name
  enters the crate's metadata hash, hence every mangled symbol and
  rustc's item order; the `[max]` / `[xl]` ratchets take those
  values. Sizes per feature set in STATUS.md.

### Size: sync islands out of async bodies

- Mostly-sync async bodies keep only their I/O; the code between the
  awaits runs in `#[inline(never)]` sync functions: the schema load
  (`collect_schema_row` / `catalog_from_rows`, tiny's catalog rebuild
  too), the integrity check's per-table setup and per-row checks,
  `stream_open`'s plan and setup, one statement-undo site in
  `trigger_failed`, the ORDER BY window shared by `exec_subplan_inner`
  and `finish_produced`, and `prepare_params`, `plan_inner`,
  `nest_may_abort`, `json::tvf_rows` and `before_update_loaded` out of
  their futures. tiny −136 B (76,124), full −664 B (476,516), max
  −1,608 B (962,708), xl −1,400 B (1,059,216); ratchets lowered;
  futures' drop glue smaller in every profile. Splits that measured
  larger in any profile (vacuum, integrity's preamble, stat_decode,
  dbstat, fk_actions, eval_group_sub, and others) were reverted. No
  observable change.
- Review fixes in the integrity check (ungated, `tests/integrity_v17.rs`
  live against the pin): "row N missing from index X" names the broken
  index (the plans were reversed twice, so a table with two or more
  indexes named another one, and a WITHOUT ROWID table's broken index
  came out as its `sqlite_autoindex`); a partial index gets no "wrong #
  of entries" line (the pin keeps no such count); a non-STRICT column's
  stored type is checked against its affinity, as the pin does ("NUMERIC
  value in t.c" for a number in a TEXT column, "TEXT value in t.c" for
  convertible text in a numeric one; hipp said `ok`). With them the unit
  ends at full 476,476 B, max 962,292 B, xl 1,058,808 B (tiny 76,124 B);
  ratchets lowered again.
- ADR 0005 (`docs/adr/0005-code-size-sync-core-and-vm.md`) is accepted
  for Stage 1 with its measured results: tiny −836 B, full −31,116 B,
  max −68,780 B, xl −83,320 B over the three size units; Stages 2–4
  (the VM) stay proposed.

### Size: monomorphic glue

- The evaluator's remaining duplicate instantiations are gone:
  `walk_aggs_mut` / `walk_plan_aggs_mut` are monomorphic, the compound
  ORDER BY matcher shares the resolver's instantiation, and under
  `window` `cond_is` re-types one `Error<()>` body as `eval` does, with
  the evaluator's own helpers calling the bodies directly. full
  −1,844 B (479,396), max −2,036 B (966,348), xl −2,044 B (1,063,136),
  tiny unchanged; ratchets lowered. No observable change
  (`tests/size_v17.rs`).
- `btree::distribute` / `distribute_insert` compute their group sizes
  in the monomorphic `split_counts` / `insert_counts`; only the copy
  into groups stays per cell type. tiny −700 B (76,260), full −960 B
  (478,436), max −960 B (965,388), xl −992 B (1,062,144); page images
  unchanged (`insert_balance_v15`, `delete_balance_v14` vs the pin).
- Plans are no longer deep-copied per execution: `Plan::Query`,
  `Plan::Pragma` and `Plan::Instead` hold an `Rc`, the executors and
  the streamed cursor borrow or share it, and `exec_subplan_inner` (a
  subquery's every run — per outer row when correlated) no longer
  clones its plan. full −1,256 B (477,180), max −1,072 B (964,316), xl
  −1,528 B (1,060,616), tiny unchanged.
- Measured and not kept: a hand-written type-erased `AVec` growth core
  (+460 to +2,024 B; `alloc`'s `RawVecInner` already shares the growth
  path) and an out-of-line `AVec::push` (−88 to −1,968 B, +17.7%
  instructions on the bolt workload).

### Size: one shared stable sort

- Every `sort` / `sort_by` / `sort_by_key` / `sort_unstable*` in `src/`
  (31 call sites, plus `agg::sort_stable`) now goes through
  `arena::sort_perm`, one non-generic stable bottom-up merge sort over
  an index permutation, with a per-type `arena::sort_by` adapter: no
  `core::slice::sort` code is left in any profile. full −26,352 B
  (481,240), max −62,688 B (968,384), xl −76,948 B (1,065,180), tiny
  unchanged; ratchets lowered. Output is unchanged for every total
  order (`tests/size_v17.rs`).
- Fix (ungated): the fts5 rank sorter treated a NaN bm25 score as equal
  to every score; it now sorts as the pin's NULL (first ascending, last
  descending, ties in rowid order). The old comparator was not an
  order: with finite ranks among NaN ones it also returned the finite
  ranks out of order, so no total order could keep 0.16's output; this
  is a deliberate, pin-matching change in an otherwise pure refactor.

### Size: one copy of the expression evaluator

- With `window`, `query::eval` is compiled once (for `Error<()>`) instead
  of once per error type: max −17,924 B, xl −17,900 B, tiny and full
  unchanged (first item of the ADR 0005 size pass; it makes room for
  `main`'s index-insert speed-up, which had taken `max` past 1 MiB).

### Query flattening (leaf `flatten`, in `xl`)

- The query flattener (`sqlite3FlattenSubquery`): a view, CTE or
  FROM subquery the pin merges into its reader is planned without
  access paths and spliced into the reader's FROM clause (its WHERE
  into the reader's or the LEFT JOIN's ON clause, its columns
  substituted through nested subqueries, a LEFT JOIN's merged
  expressions guarded by the NULL row, its LIMIT and ORDER BY moving
  out), so the merged tables join the join planner's order. Views keep
  their pre-flattening column names.
- WHERE push-down (`pushDownWhereTerms`) into the subqueries that stay
  co-routines or materialisations (every arm of a UNION ALL; HAVING
  for an aggregate's non-GROUP BY terms).
- The result columns a co-routine or materialisation's reader never
  reads are not computed (DISTINCT, aggregate, windowed, MATERIALIZED,
  shared and non-UNION ALL compound bodies excepted, and columns their
  ORDER BY names): `SELECT a FROM v` answers where `v.b` overflows.
- EXISTS-to-join (`existsToJoin`): `SEARCH t EXISTS` semi-join loops,
  ordered after the sources they read, in both executors.
- EXPLAIN QUERY PLAN prints the foreign-key child scans of a parent
  DELETE / UPDATE.
- Fixed in the flattener model the shared materialisations follow: a
  FULL JOIN's right side is never flattened, a LEFT JOIN's right side
  is refused under DISTINCT (not GROUP BY).
- Tests: `tests/flatten_v17.rs`, `hipp-cli/tests/cli_v17_flatten.rs`;
  `tests/eqp_v16.rs`'s flattened-view and EXISTS differences now match
  the pin. Size: tiny 0, full −16 B, max −8 B, xl +15,688 B.
- Review fixes: a CTE's reference and a correlated FROM subquery
  compute every column (the omission raised no error where the pin
  does); a merge past 64 sources is "at most 64 tables in a join" (the
  source masks overflowed); the LEFT JOIN simplification runs before
  any merge, push-down or EXISTS-to-join (an EXISTS made a LEFT JOIN
  inner); UPDATE ... FROM omits unread subquery / view columns; a
  view's copies share a materialisation only while none lost a column,
  a NOT MATERIALIZED CTE read twice in one FROM clause is shared, a CTE
  inside an IN counts twice (the pin's CO-ROUTINE / MATERIALIZE
  choice); an uncorrelated EXISTS's own term is checked at its loop
  (it was quadratic); FK child scans follow the parent key's affinity
  and collation, show the covering read correctly, and INSERT on a
  parent prints its rounds; a row value beside an EXISTS no longer
  stops the conversion. Ungated: a MULTI-INDEX OR under a kept
  subquery's loop found only the first outer row's matches (rows
  lost); a LIMITed plain FROM subquery computed columns of rows past
  its LIMIT; a probe's scalar subquery is shown with its loop in
  EXPLAIN QUERY PLAN; a nested subquery's IN-operator index is found
  past a merge. Tests: review regressions, a subquery corpus in
  `tests/eqp_v16.rs` (none may differ), views / FROM subqueries /
  EXISTS in `tests/join_order_v17.rs` under limits of their own,
  overflowing columns and tighter limits (7% / 45%) in the flatten
  corpus. Size: tiny 0, full +72 B, max +168 B, xl +2,400 B.

### Index strategies (leaf `multi_index`, in `xl`)

- The MULTI-INDEX OR (`whereLoopAddOr`): an OR term whose every branch
  has an index or rowid loop is a loop of its own, costed as the pin
  costs it (the branches may use the WHERE clause's other terms); when
  chosen each branch is planned again with the WHERE's pushed terms,
  read in turn, a row returned at the first branch it satisfies — the
  pin's rows, in its order, each once. One table, a join loop (the
  commuted copies' `INDEX 3`), LEFT JOIN, UPDATE and DELETE. EXPLAIN
  QUERY PLAN prints `MULTI-INDEX OR` / `INDEX n` trees.
- min() / max(): a lone `min(x)` / `max(x)` (no GROUP BY, HAVING or
  FILTER) is planned for x's order (NULLs last for min) and stops at
  the first row passing the WHERE, so the bare columns are the pin's;
  every loop prints `SEARCH`, a `f(DISTINCT)` the order delivers sorts
  nothing.
- Skip-scans of an index's leading columns when `sqlite_stat1` says
  each value repeats 18 or more times (`ANY(a) AND b=?`); the IN
  operator's statistics rule (an IN that costs more than the scan it
  saves is a filter); whereLoopInsert / whereLoopAdjustCost for one
  table's loops (`b = 2 AND c = 1` over an index on (b, c) uses both);
  whereCombineDisjuncts (`x < 5 OR x = 5` is also `x <= 5`); the pin's
  OR-to-IN rule (the first branch's column, then the commuted copies').
- EXPLAIN QUERY PLAN (ungated, `explain`): an uncorrelated IN
  subquery used as a filter shows its `CREATE BLOOM FILTER`; an
  aggregate query's DISTINCT shows its temp B-tree. With `join_order`:
  a join's WHERE subqueries are shown after the loop that computes
  them.
- Fixed (ungated, `expr_index`): an OR of equalities on one column
  became an IN probe even when the values' affinity differs from the
  column's (`x.c0 = y.c1 OR x.c0 = y.c2` with a TEXT `c0` and a REAL
  `c1` lost rows); the pin's `okToChngToIN` check is applied.
- Tests: `tests/multi_index_v17.rs`, `hipp-cli/tests/cli_v17_multi_index.rs`;
  `tests/eqp_v16.rs` and `tests/join_order_v17.rs` generate OR terms,
  IN subqueries and min() / max() and their limits are tighter.
  Size: xl +20,648 B (1,135,004 B); max +168 B; full 0; tiny 0.
- Review fixes: an OR term holding an explicit COLLATE (outside a
  subquery) is no MULTI-INDEX OR, IN or combined term, as on the pin
  (ungated for the IN); `x IN (v)` on a constant is the pin's `x = +v`
  equality (ungated); OR-to-IN and MULTI-INDEX OR branches take
  indexed expressions as keys (`a*2=14 OR a*2=4`, `t.a*2 = r.j`); the
  IN operator's Bloom filter is shown only for BINARY comparisons
  (ungated) and its own index or rowids read in place of the subquery
  print `USING INDEX i FOR IN-OPERATOR` / `USING ROWID SEARCH ON TABLE
  t FOR IN-OPERATOR`; every min() / max() loop prints SEARCH (FROM
  subqueries, CTEs, virtual tables; max() offers generate_series its
  DESC order) and `min(a+1)` is covered by an index on `a+1`;
  `sqlite_stat1`'s `unordered`, `sz=N` and `noskipscan` are read; a
  min() / max() over an IN probe on its argument folds every row forward (the pin's
  bare columns). Fixed on the way (ungated): `a = 'y' COLLATE NOCASE
  ORDER BY a` treated `a` as constant and returned rows out of order.
  Size: xl +6,040 B (1,156,732 B); max +816 B (1,047,868 B); full 0;
  tiny 0.

### Join ordering and automatic indexes (leaf `join_order`, in `xl`)

- Joins of tables, virtual tables and JSON table-valued functions are
  planned as the pin's `where.c` plans them (loop costs with
  prerequisites, the N-best path solver, sorting costs, LEFT / CROSS
  JOIN constraints, LEFT JOIN simplification, constant propagation):
  rows without ORDER BY come in the pin's order and EXPLAIN QUERY
  PLAN prints the pin's loops.
- Automatic indexes (PARTIAL when a term reads the table alone),
  built once per statement, and Bloom filters, built and consulted:
  before every automatic index and, with `sqlite_stat1`, before
  lookups that run more often than their table has rows.
- `sqlite_stat1` is read back at schema load and after ANALYZE.
- `FROM t, m WHERE t MATCH m.q`, `FROM generate_series(1, t.n), t` and
  `FROM json_each(t.j), t` run (they were refused).
- Fixed (ungated): the commuted copy of a join term kept the wrong
  collation, so a lookup through an index on the other column could
  lose rows under a NOCASE comparison; `,` in FROM is a plain inner
  join, not a CROSS JOIN.
- Tests: `tests/join_order_v17.rs`, `hipp-cli/tests/cli_v17_join_order.rs`.
  Size: xl +48,032 B (1,103,060 B); max +128 B; full +48 B; tiny 0.
- Review fixes: a GROUP BY a join (or a rowid scan) delivers in
  another column order emits its groups in loop order, as the pin's
  plan says; likely() / unlikely() / likelihood() and COLLATE around a
  WHERE term no longer hide it from the planner, and their probability
  is the pin's `truthProb`; the LEFT JOIN simplification sees through
  them and takes `x IS DISTINCT FROM NULL` as NOTNULL; the statistics
  held in a connection follow DDL as the pin's (DROP INDEX keeps the
  table's estimate, ALTER / VACUUM / a rolled-back schema change
  reload) and stat rows are decoded as `analysisLoader` does; UPDATE
  ... FROM is planned as a join (the pin's final table when several
  FROM rows match, automatic indexes); 64-source joins are planned;
  expression indexes cover SELECTs that read only their expressions,
  and BETWEEN with column bounds probes the bounds' table (the latter
  under `expr_index`);
  a table-valued argument past a CROSS JOIN is left out of the call as
  on the pin instead of being refused. Size: xl +11,400 B (1,114,356 B);
  max +448 B (1,046,892 B); full +192 B (506,672 B); tiny 0.

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
- Review fixes: under secure-delete an entry a tombstone hides stays
  in place (the pin's iterator skips tombstoned rowids); a
  rolled-back `secure-delete` setting is forgotten when the pin would
  reparse its schema (ALTER TABLE, VACUUM, a rollback undoing a
  schema change, `writable_schema = RESET`); a contentless UPDATE
  with `rowid = '1'` on row 1 is not a move (and with
  `rowid = NULL` on row 0 is the pin's "datatype mismatch"); tombstone and
  secure-delete writes are linear in the rows a statement deletes
  (were quadratic); OR FAIL keeps an fts5 write's rows before the
  conflict and OR ROLLBACK ends the transaction (ungated, `fts5`:
  both acted as ABORT).
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
