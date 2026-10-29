# hipp — build status

Spec: `docs/SPEC.md` rev. 0.3. Oracle pin (D1): SQLite **3.54.1**
(official amalgamation 3540100, `~/workspace/hipp-design/oracle/sqlite3`,
`--version` verified). All differential results are against the pin.

## v0.11 — subqueries inside aggregate arguments (2026-10-29)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). This milestone lifts the
last plan-time refusal carried since v0.6: expressions like
`sum((SELECT ...))` / `count((SELECT ...))` were refused
with `Error::Unsupported` in every aggregate-argument
position. The behaviour is new dialect surface, so it lands
behind the **`v10` cargo feature** (default on, implies
`v09`; the tiny profile excludes it). The feature ladder's
next rung is `v10` because v0.10 itself was an ungated
correctness fix and took no rung. Without `v10`, the
planners refuse exactly as before (verified: the whole
v0.11 suite fails against a `v09`-only build).

### Shipped

- **Per-group-row aggregate arguments.** In the async
  group evaluator, an expression containing a subquery
  inside an aggregate's arguments now folds each outermost
  aggregate over argument values evaluated **per group
  row** through the v0.6 substitution path (each subquery
  executes against that row), then substitutes the folded
  literal back into the expression; subqueries outside the
  aggregate still resolve against the group's
  representative row, and bare columns read it as before.
  Positions covered: SELECT list, HAVING, ORDER BY — in
  top-level queries, subquery bodies, derived tables,
  compound cores, and `INSERT INTO ... SELECT` sources
  (all of which share the two group projectors).
- **One fold, two drivers.** The aggregate fold was
  refactored to operate on precomputed per-row argument
  values (`query::eval_agg_values`); the synchronous
  evaluator and the new per-row path are the same code, so
  DISTINCT handling, NULL skipping, sum's int/real
  accumulation, and group_concat's per-row separator
  cannot drift apart.
- **Representative rule preserved.** A lone min/max whose
  argument contains a subquery picks its extremum row by
  the per-row subquery values (`pick_rep_qp_async` /
  `pick_rep_sub_async` over an async extremum search); all
  other groups keep the synchronous rule untouched.

### Oracle-probing notes (all verified on the 3.54.1 pin
before implementation)

- `sum((SELECT b))` over `(1,10),(1,20),(2,30),(2,NULL)`
  is 60: a no-FROM subquery reads the group row's own
  columns, one contribution per row. Grouped by `a` it is
  30 per group. `sum((SELECT x FROM u LIMIT 1))` lands the
  uncorrelated value once per row (400 for `x = 100`).
- A subquery yielding no row for a group row contributes
  NULL: `count((SELECT 1 WHERE a = 1))` counts only the
  matching rows (2 of 4), and an all-miss fold behaves
  exactly like an all-NULL fold (`sum` NULL, `count` 0,
  `total` 0.0).
- Composition inside an argument is unrestricted on the
  pin: arithmetic mixing columns and subqueries, CASE
  between subqueries, `IN (SELECT ...)` and `EXISTS`
  forms, nested aggregates *inside* the subquery
  (`sum((SELECT sum(b) FROM t))` = 240), and compound
  subquery bodies (first row wins). group_concat's
  separator argument is per-row too
  (`group_concat((SELECT b), (SELECT ';'))`).
- A direct aggregate inside an aggregate's argument
  remains a misuse on the pin; hipp refuses it with its
  typed `Unsupported` (raised when the inner aggregate
  would evaluate in a row context), consistent with its
  typed-error discipline.

### Root finds this milestone

- **The representative row needed the async path too.**
  The synchronous `pick_rep` evaluates a lone min/max
  argument to choose the extremum row; with a subquery in
  that argument it would have failed (or, worse, silently
  kept the first row) even though the fold itself worked.
  The async wrappers pre-empt exactly that one case and
  delegate everything else to the synchronous rule.
- **One pre-existing assertion was superseded by design**
  (v0.6's `subquery_rejections` asserted aggregate-argument
  subqueries are prepare-time refusals); it now asserts
  the implemented value (1500 for the file's data), the
  same repointing pattern as v0.6–v0.8.

### Gates (run on the final tree)

- `cargo test --workspace`: **162 passed, 0 failed**
  (152 at v0.10 + 8 new core differentials in
  `tests/agg_subquery_v11.rs` + 2 new CLI differentials in
  `hipp-cli/tests/cli_v11.rs`). The core suite covers
  uncorrelated and plain-mixed arguments, correlated
  per-row evaluation incl. conditional / IN / EXISTS /
  CASE forms, GROUP BY / HAVING / ORDER BY positions,
  the min/max representative rule, DISTINCT folds and
  per-row separators, empty and all-NULL groups, nested
  positions (scalar subquery bodies, derived tables,
  compound cores, INSERT ... SELECT sources), and the
  nested-aggregate misuse refusal. A1–A11 stay green;
  the hipp-written file in the INSERT ... SELECT test
  passes `PRAGMA integrity_check` in the oracle.
- CLI: the v0.11 corpus (the fold family, correlation,
  grouping, representative rows, and an aggregate-argument
  INSERT ... SELECT) is byte-identical to the oracle
  one-shot and over stdin, and `.dump` after the writes
  replays into the oracle.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean tree
  afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v10 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features; a `v09`-only build (no `v10`)
  keeps the pre-v0.11 refusal (the v0.11 suite fails
  against it, as designed).

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 74,920 +
  .rodata 14,952 = 89,872 B of 128 KiB budget**, Δ
  **0 B** vs v0.10's 89,872 B — under the 4 KiB
  justification line, no justification needed.
  Attribution: the whole v0.11 engine (per-row fold,
  group substitution helpers, async representative
  search) lives in the `v04`/`v06`-gated modules behind
  the `v10` feature; tiny compiles none of it. Engine
  statics 0 B (.bss 98,308 B = demo heap, unchanged).
  Headroom remaining: 41,200 B.

### Honest gaps (v0.11)

- Per-row execution is per group row per subquery, with
  no uncorrelated hoisting (the standing perf gap,
  unchanged): an uncorrelated subquery inside an
  aggregate argument re-executes for every group row.
  Results are the pin's; only the cost differs.
- Bare columns beside *multiple* min/max aggregates keep
  hipp's first-row representative rule; the pin's choice
  among extremum rows in that case is implementation
  behaviour, unprobed and unchanged by this milestone
  (the lone min/max rule — the documented one — is
  differentially pinned, plain and subquery arguments
  alike).
- All pre-existing gaps stand: real-literal parsing may
  differ by 1 ulp (RISKS R5); no merge / redistribution
  on delete; WAL read stays gated on a real target
  demanding it.

## v0.10 — NOT NULL enforcement (2026-10-28)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). This milestone closes
the last parse-but-unenforced integrity constraint in the
claimed profiles: before v0.10 hipp silently ACCEPTED rows
the pin rejects. Enforcement is deliberately **not**
feature-gated — it is a correctness fix in the existing
write paths, like the v0.8 cross-affinity fix (which was
likewise ungated): the tiny profile's INSERT claims the
same semantics, so the tiny profile enforces them too.

### Shipped

- **One resolver** (`session::resolve_not_null`) runs on
  every fully-built row the write paths produce, after
  affinity / assignment resolution and BEFORE any
  uniqueness or rowid conflict detection, under the
  statement's conflict action: `Abort` (ABORT / FAIL /
  ROLLBACK / default) fails with the new
  `SchemaKind::NotNullViolation`; `Ignore` skips the row
  (INSERT) or the combination (UPDATE); `Replace`
  substitutes the column's default (affinity applied, as
  for any stored value) and degrades to the abort error
  when the default itself is NULL.
- **INSERT** (VALUES rows, INSERT ... SELECT sources, the
  REPLACE verb): the proposed row is resolved in
  `exec_insert_row`, the single funnel for all three —
  which is also where an upsert proposal is resolved,
  before conflict arbitration (below).
- **UPDATE** (including UPDATE ... FROM combinations and
  the OR REPLACE victim path): each combination's result
  row is resolved before its uniqueness checks, so a
  substituted default is what conflict detection sees.
- **UPSERT**: the proposal is resolved under the
  statement's own action before arbitration; the
  DO UPDATE result row is resolved with ABORT semantics
  unconditionally in `exec_upsert_update`.
- The catalog now carries `not_null` per column (the
  parser recorded it since v0.1 but `schema.rs` dropped
  it), so foreign files written by the oracle enforce
  identically to hipp-created ones. The rowid-alias
  column is exempt (its record field is NULL by
  construction); non-alias PRIMARY KEY columns are not.

### Oracle-probing notes (all verified on the 3.54.1 pin
before implementation)

- **Per-clause semantics:** plain / ABORT / FAIL /
  ROLLBACK error atomically (a multi-row VALUES lands
  nothing); IGNORE skips just the offending row; REPLACE
  substitutes the default even with no uniqueness
  conflict anywhere, and errors when the column has no
  non-NULL default — a pre-existing conflicting row
  survives that error untouched.
- **Substitution precedes uniqueness:** under OR
  REPLACE, a substituted default that collides with an
  existing row deletes that row as the victim (INSERT
  and UPDATE forms both probed). NOT NULL also wins
  error precedence over UNIQUE and duplicate-rowid
  errors in INSERT, UPDATE, and DO UPDATE.
- **Upsert proposals are checked before arbitration**
  (the brief's open question, and the naive reading is
  wrong): a violating proposal errors even when a
  conflict exists and DO UPDATE never reads the value.
  Under OR IGNORE + upsert the proposal is skipped
  wholesale — the DO UPDATE does NOT run; under OR
  REPLACE + upsert it is substituted and arbitration
  proceeds. The DO UPDATE result then aborts on a NULL
  under EVERY statement action, OR REPLACE included; a
  false DO UPDATE WHERE skips the check with the update.
- **Final-row rule:** the check reads the row after all
  assignments, so `SET b = NULL, b = 'x'` is legal and
  `SET b = 'x', b = NULL` is not (UPDATE and DO UPDATE).
- **Keys:** `INTEGER PRIMARY KEY NOT NULL` still
  auto-assigns on NULL (alias exemption); `INTEGER
  PRIMARY KEY DESC NOT NULL`, TEXT / INT primary keys
  with NOT NULL, and a table-level PK over a NOT NULL
  column all reject NULL. A TEXT PRIMARY KEY *without*
  NOT NULL still accepts NULLs (v0.3 heritage behaviour,
  unchanged).

### Root finds this milestone

- **The catalog dropped `not_null`.** The parser has
  recorded the constraint since v0.1 and ALTER validates
  against it since v0.4, but `schema::Column` had no
  such field — enforcement first needed it threaded
  through catalog construction.
- **Omitted-column defaults were stored raw.** The
  substitution rule forced the question of what value a
  default materialises as: the pin applies the column's
  affinity (`DEFAULT '7'` in an INT column stores the
  integer 7), but hipp's INSERT row build cloned the raw
  literal, storing the text `'7'` for an omitted column.
  The row build now affinity-applies defaults, so the
  omitted, supplied, and substituted forms of a value
  agree; the new differential pins all three.

### Gates (run on the final tree)

- `cargo test --workspace`: **152 passed, 0 failed**
  (142 at v0.9 + 8 new core differentials in
  `tests/not_null_v10.rs` + 2 new CLI differentials in
  `hipp-cli/tests/cli_v10.rs`). The new suites cover the
  abort family incl. expression NULLs and multi-row
  atomicity, IGNORE skips incl. RETURNING, REPLACE
  substitution incl. victim-making defaults and the
  no-default degradation, UPDATE actions incl.
  last-assignment-wins and FROM combinations, upsert
  proposal/DO UPDATE semantics under all statement
  actions, the PK/alias matrix, error precedence, an
  oracle-written schema enforced by hipp, and the tiny
  configuration enforcing too. A1–A11 stay green; every
  hipp file `integrity_check`s in the oracle.
- CLI: the v0.10 corpus (IGNORE skips, REPLACE
  substitution, upsert proposal resolution, alias +
  affinity corners) is byte-identical to the oracle
  one-shot and over stdin, and `.dump` after v0.10
  writes replays into the oracle.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean
  tree afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v09 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features.
- Tiny (below). Pre-implementation evidence, for the
  record: the new core suite did not compile against
  the v0.9 tree (`SchemaKind::NotNullViolation` did not
  exist), and the v0.9 hipp CLI accepted and stored a
  NULL into a NOT NULL column (`1|1|`) where the pin
  exits 1 with "NOT NULL constraint failed".

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 74,920 +
  .rodata 14,952 = 89,872 B of 128 KiB budget**, Δ
  **+360 B** vs v0.9's 89,512 B — under the 4 KiB
  justification line. Attribution: the whole delta is
  the ungated enforcement (the resolver, the per-plan
  `not_null` plumbing, the new error kind) — tiny pays
  it because tiny's INSERT is one of the corrected
  paths. Engine statics 0 B (.bss 98,308 B = demo heap,
  unchanged). Headroom remaining: 41,200 B.

### Honest gaps (v0.10)

- Non-literal defaults keep the parser's pre-existing
  approximation (v0.4): a `DEFAULT` that is not a plain
  literal is recorded as no default (or its leading
  literal), so OR REPLACE substitution uses the
  recorded default — a NOT NULL column whose real
  default is a non-NULL expression (e.g.
  `DEFAULT (1+2)`, `CURRENT_TIMESTAMP`) errors under OR
  REPLACE where the pin would substitute the computed
  value.
- When one row carries both a bad rowid value
  (datatype mismatch) and a NULL NOT NULL column, hipp
  reports the datatype mismatch (targets are applied
  before the check); the pin's precedence there is
  unprobed.
- FAIL vs ABORT inside an explicit transaction keeps
  the engine's pre-existing simplification (any
  statement error aborts the bracket); the pin's FAIL
  keeps the statement's earlier rows inside a
  transaction.
- All pre-existing gaps stand: subqueries inside
  aggregate arguments stay refused; real-literal
  parsing may differ by 1 ulp (RISKS R5);
  uncorrelated-subquery hoisting absent; no merge /
  redistribution on delete; WAL read stays gated on a
  real target demanding it.

## v0.9 — compound SELECTs and derived tables (2026-10-27)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). This milestone closes the
last grammar holes named in every milestone since v0.6:
compound SELECTs and FROM-clause derived tables, behind the
`v09` cargo feature (default on, implies `v08`; the tiny
profile excludes it).

### Base note (Bolt re-baseline)

v0.9 stacks on `a3b80ec` — the v0.8 tree plus Mark's two
external Bolt perf commits (`ecf225b` index_insert
single-pass leaf probe, `a3b80ec` join_rows accumulator),
adopted as the loop baseline on 2026-10-27 after a full
green suite run. The tiny profile was **re-measured on the
untouched base** before any v0.9 work: **89,312 B** (.text
74,360 + .rodata 14,952) — identical to the v0.8 landing.
The Bolt commits contribute **0 B** to tiny (index_insert
sits behind `indexes`, join_rows behind `v04`), so the
ratchet baseline for the v0.9 delta is 89,312 B.

### Shipped

- **Compound SELECTs** (`UNION`, `UNION ALL`, `INTERSECT`,
  `EXCEPT`) between SELECT cores and `VALUES` cores, in
  every position a select statement appears: standalone
  statements, scalar / `IN` / `EXISTS` subquery bodies,
  `INSERT INTO ... SELECT` sources, and derived tables.
  The `SelectStmt` AST carries a flat source-order tail of
  `(CompoundOp, core)` pairs — SQLite's operators all share
  one precedence and associate left-to-right, so no tree is
  needed — and each core plans independently before the
  combination runs.
- **Compound semantics, probed on the pin first** (see
  below): raw storage-equality dedup with NULLs equal and
  no affinity whatsoever; sorter-order output for the dedup
  operators; first-occurrence survivors except that UNION's
  right operand replaces equal left rows; compound
  `ORDER BY` restricted to output ordinals and result-column
  names drawn from *any* constituent core (qualified names
  included), resolved at prepare time; per-core column-count
  checking (`SchemaKind::ColumnCountMismatch`); cores carry
  no `ORDER BY` / `LIMIT` of their own.
- **Derived tables**: `(select-stmt) [[AS] alias]` in the
  general query planner, the subquery planner (including
  inside `EXISTS` / scalar / `IN` bodies), and
  `UPDATE ... FROM`. The inner statement is planned against
  enclosing scopes only (no LATERAL: sibling sources are
  invisible, exactly like the pin) but may correlate with an
  enclosing query. Result columns take their names from the
  inner statement (`column1`, `column2`, ... for VALUES
  cores); the derived source exposes **no rowid** unless a
  result column is literally named `rowid`. Derived sources
  work over compounds, VALUES cores, aggregates, and joins,
  and nest.
- **VALUES cores** as first-class select cores: standalone
  `VALUES` statements, `VALUES` inside compounds and
  `IN (VALUES ...)`, with column names `column1`, ... .
- Source rows in the executor now carry `Option<i64>`
  rowids (table rows `Some`, derived rows `None`), which is
  what lets the rowid fallback distinguish a real table from
  a derived source.

### Oracle-probing notes (all verified on the 3.54.1 pin
before implementation)

- **Precedence surprise:** all four compound operators
  share SQLite's single precedence and are
  left-associative — INTERSECT does *not* bind tighter than
  UNION on the pin (the coordinator brief's prior was
  wrong; probing caught it). `SELECT 1 UNION SELECT 2
  INTERSECT SELECT 2` yields `{2}`.
- **Dedup equality is raw:** `SELECT 1 UNION SELECT '1'`
  keeps both rows; a TEXT column unioned with integer
  literals keeps the integers too (no affinity conversion,
  contradicting the "compound affinity" phrasing in the
  planning notes — the comparison is `compare_values` raw:
  numeric cross-type equality only, text never equals blob,
  NULL equals NULL). Dedup output order is sorter order,
  ascending on all columns.
- **Survivor rule:** dedup is first-occurrence-wins within
  each operand, but a UNION right row *replaces* an equal
  left row — `1 UNION 1.0` yields the real `1.0`, `1.0
  UNION 1` yields the integer `1`. INTERSECT / EXCEPT keep
  left rows.
- **Compound ORDER BY** accepts only (a) in-range ordinals,
  (b) bare names matching a result-column name from any
  constituent core, (c) qualified names matching a plain
  column item's source qualifier and name in any core.
  Expressions, constants, unknown names, and out-of-range
  ordinals are prepare errors. The sort comparison is raw.
- **Derived-table rules:** alias optional; a column-name
  list after the alias is a *syntax error* on the pin;
  parenthesised compound operands are a syntax error
  everywhere; `INSERT ... SELECT ... UNION ... ON CONFLICT`
  parses (the compound terminates the SELECT, so the v0.8
  bare-FROM upsert quirk refusal does not apply to compound
  sources).
- **VALUES-tail quirk:** if the *last* core of a compound
  is a VALUES core, a trailing `ORDER BY` / `LIMIT` is a
  syntax error on the pin (`VALUES (1) LIMIT 1` fails,
  `VALUES (1) UNION SELECT 2 ORDER BY 1` works). Reproduced
  in the parser.

### Gates (run on the final tree)

- `cargo test --workspace`: **142 passed, 0 failed** (131
  at v0.8 + 9 new core differentials in
  `tests/compound_v09.rs` + 2 new CLI differentials in
  `hipp-cli/tests/cli_v09.rs`, counts summed from the test
  log). The new suites cover operator precedence, raw
  equality incl. NULLs and cross-class pairs, ORDER BY
  acceptance *and* refusal shapes, width-mismatch / core
  ORDER BY / parenthesised-operand refusals, VALUES cores
  incl. the trailing-tail refusal, derived tables incl. the
  rowid / sibling-correlation / alias-column-list refusals,
  compounds as INSERT sources and subquery bodies, and
  `UPDATE ... FROM` over derived tables. A1–A11 conformance
  checks (in the suite) stay green; every `Twins` file
  passes `PRAGMA integrity_check` in the oracle.
- CLI: the v0.9 corpus (compounds, VALUES, derived tables,
  compound INSERT, UPDATE ... FROM over a derived table)
  is byte-identical to the oracle one-shot and over stdin,
  and `.dump` after v0.9 writes replays into the oracle.
- `cargo clippy --workspace --all-targets -- -D warnings`:
  clean. `cargo fmt` (write mode): clean tree afterwards.
  Feature matrix compiles: default, no-default, std-only,
  v04-only ... v09-only ladders, indexes-only, indexes+v05,
  all-features.
- Tiny (below).

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 74,560 + .rodata
  14,952 = 89,512 B of 128 KiB budget**, Δ **+200 B** vs
  the re-measured base (89,312 B) — under the 4 KiB
  justification line. Attribution: the `v09` engine is
  fully feature-gated out of tiny; the +200 B is ungated
  AST struct growth (`SelectStmt`'s compound / values
  fields, `FromSource`'s derived field) flowing through
  the always-compiled parser. Engine statics 0 B (.bss is
  the demo's caller-supplied heap, unchanged).

### Honest gaps (v0.9)

- Compound `ORDER BY` name resolution takes the first
  matching core scanning left to right; a name that appears
  at *different* result positions in different cores is an
  untested edge (the pin's behaviour there is unprobed).
- All pre-existing gaps stand: uncorrelated-subquery
  hoisting is still absent (a compound core re-runs per
  outer row when correlated; results are correct,
  performance is not); subqueries inside aggregate
  arguments stay refused; NOT NULL constraints parse but
  are still not enforced; real-literal parsing may differ
  from the oracle by 1 ulp on pathological mantissas
  (RISKS R5); WAL read stays gated on a real target
  demanding it.

## v0.8 — correctness-and-completion pass (2026-10-26)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the
loop status.

### Scope shipped

- **Cross-affinity comparison correctness** in the v0.4
  evaluator (not feature-gated — the evaluator is already
  outside the tiny profile): every comparison operand now
  carries an affinity *class* (NUMERIC / TEXT / BLOB / none)
  derived from its expression shape — a column reference or
  CAST carries its column/cast affinity, a rowid reference
  is NUMERIC-class, anything else has none. SQLite's rules
  then apply between *any* two operands: NUMERIC-class on
  exactly one side applies NUMERIC affinity to the other
  side's value; TEXT-class against no affinity applies TEXT
  affinity to the no-affinity side; every other pairing
  converts nothing. This closes the silent wrong-answer
  divergence carried since v0.4 (`int_col = text_col`
  compared raw storage classes). CASE's base form takes the
  same adjustment (it compares like `=`); NULLIF keeps its
  binary comparison, matching the oracle. A scalar / IN
  subquery whose single result item is a direct column
  reference, CAST, or rowid now carries that affinity into
  the enclosing comparison too (`'002' = (SELECT int_col
  ...)`) — the substitution path represents such results as
  an affinity-carrying literal node (`RExpr::AffLiteral`)
  whose value is untouched (a CAST would have corrupted it:
  CAST 'abc' AS NUMERIC is 0, affinity leaves 'abc' alone).
- **INSERT completion** behind the `v08` cargo feature
  (default on, implies `v07`; the tiny profile excludes
  it): `INSERT INTO ... SELECT` with the full SELECT
  grammar as the row source (joins, aggregates + GROUP BY,
  DISTINCT, ORDER BY / LIMIT / OFFSET, no-FROM selects,
  explicit column lists), composing with OR conflict
  clauses, UPSERT, and RETURNING through the existing
  per-row insert path; and subqueries in INSERT VALUES
  (scalar, `IN (SELECT ...)`, `EXISTS`). Both new sources
  are **fully materialised / evaluated before the first
  row is inserted**, which is the oracle's observable
  semantics on both points (probed below).

### Root finds this milestone

- **Oracle probes before code** pinned the affinity rules
  the tests then encode: a BLOB-affinity column is its own
  class, *not* "no affinity" (`text_col = blob_col` over
  `'1'` vs integer `1` is false; `text_col = 1` is true);
  `rowid = '1'` matches (rowid is NUMERIC-class); BETWEEN
  is pairwise comparison affinity, not the
  apply-the-left-affinity-to-both-bounds special rule (the
  distinguishing probe `'10' BETWEEN 2 AND 3` over TEXT /
  INTEGER columns is true pairwise, false under the
  special rule); and NULLIF compares binary.
- **VALUES subqueries snapshot the pre-statement table.**
  Probed on the pin: on an empty `t`,
  `INSERT INTO t VALUES ((SELECT count(*) FROM t)),
  ((SELECT count(*) FROM t))` inserts **0, 0** — every
  row's subqueries evaluate before any row lands. The
  executor therefore evaluates all source rows (VALUES
  subqueries and INSERT ... SELECT alike) before the
  first insert; a self-insert
  (`INSERT INTO t SELECT * FROM t`) terminates on the
  pre-statement rows for the same reason.
- **The upsert-after-SELECT parser quirk is reproduced,
  not widened.** On the pin, `INSERT INTO d SELECT ...
  FROM t ON CONFLICT ...` is a syntax error unless the
  SELECT's FROM clause is terminated by WHERE, ORDER BY,
  or LIMIT (a FROM-less SELECT takes an upsert freely).
  hipp refuses exactly the unterminated-FROM shape at plan
  time rather than accepting a superset of the oracle's
  grammar.
- **One pre-existing test was superseded by design**
  (v0.6's `subquery_rejections` asserted INSERT VALUES
  subqueries are prepare-time refusals); it now asserts
  the implemented behaviour and the landed row, the same
  repointing pattern as v0.6/v0.7.

### Tests — 131, all green (v0.7: 121)

Counted as the full `cargo test --workspace` sum across
every suite. (Correction, same day: the first v0.8 STATUS
draft said 191/181 — a miscount from a garbled test-run
listing; the log-verified sums are 131 and 121.) New: `tests/insert_select_v08.rs` (8) —
cross-affinity column pairs and compound positions
(CASE / BETWEEN / IN / joins / subquery-result affinity)
as row-for-row differentials over an oracle-built file,
cross-affinity UPDATE / DELETE filters, INSERT ... SELECT
(copies, expressions, aggregates, self-insert, empty
sources, no-FROM selects), INSERT ... SELECT with
conflict clauses / UPSERT / RETURNING including the
unterminated-FROM refusal, an oracle-writes-first
INSERT ... SELECT, and VALUES subqueries with their
snapshot semantics and rejections — plus
`hipp-cli/tests/cli_v08.rs` (2): a v0.8 corpus
byte-identical through both CLIs and a `.dump` replay
check. A1–A11 acceptance tests stay green; clippy
`--workspace --all-targets -- -D warnings` clean; the
feature matrix (no-default, std-only, v04-only through
v08-only, indexes-only, indexes+v05, all-features) all
compiles.

### Tiny gates (SPEC §4.1), re-measured

- Linked binary (tiny profile, thumbv7em): **.text 74,360 +
  .rodata 14,952 = 89,312 B** ≤ 128 KiB. Δ vs v0.7's
  89,168 B: **+144 B** — under the 4 KiB ratchet line, no
  justification needed. The v08 engine contributes 0 B to
  tiny (behind `v08`); the delta is the ungated AST/parser
  gaining the INSERT source split (VALUES vs SELECT).
- Engine statics 0 B outside caller heap (.bss 98,308 B =
  demo heap, unchanged); tiny RAM + zero-alloc scan gates
  green (`tests/gates.rs`).

### Honest gaps

- INSERT ... SELECT sources and VALUES rows are
  materialised in the statement arena before inserting;
  very wide sources are bounded by the statement arena
  (ArenaExhausted, never a panic) where SQLite streams.
  RETURNING over an INSERT ... SELECT inherits v0.7's
  buffered-RETURNING note (values identical, timing
  differs).
- A subquery result item that is not a direct column
  reference / CAST / rowid carries no affinity into an
  enclosing comparison; that matches the oracle for the
  probed expression shapes, but compound result items
  (e.g. `SELECT a FROM t UNION ...`) are outside the
  grammar entirely (UNION is v0.9 scope).
- Carried: no FROM-clause derived tables or UNION
  anywhere; subqueries execute once per outer row with no
  uncorrelated hoisting; subquery inside an aggregate's
  arguments plan-refused. The SPEC §2 phase table's last
  item, WAL read, remains gated on a real target demanding
  it.

## v0.7 — write-side completion (2026-10-25)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the
loop status.

### Scope shipped

- **Conflict clauses everywhere** behind the `v07` cargo
  feature (default on, implies `v06`; the tiny profile
  excludes it): `INSERT` / `UPDATE` accept `OR REPLACE`,
  `OR IGNORE`, `OR ABORT`, `OR FAIL`, `OR ROLLBACK` (the
  last three are the same abort in hipp, as they observably
  are in the oracle in autocommit), and the `REPLACE` verb
  is `INSERT OR REPLACE`. REPLACE deletes **every**
  conflicting row — a proposal can kill two different rows
  at once, one through the rowid and one through a UNIQUE
  index — then inserts; updated rows take the same path
  against the rows they collide with.
- **UPSERT**: `INSERT ... ON CONFLICT [(target)] DO
  NOTHING | DO UPDATE SET ... [WHERE ...]`, with no-target
  arbitration over any uniqueness conflict, target
  arbitration over exactly the rowid or one unique index's
  key set (canonicalised: the rowid-alias column and
  `rowid` spellings are the same target, DESC / COLLATE
  decorations accepted), `excluded.<col>` for the proposal
  and bare / table-qualified names for the original row in
  SET expressions and the DO UPDATE WHERE. A conflict the
  clause does not arbitrate falls back to the statement's
  own conflict action (abort by default), atomically.
- **UPDATE ... FROM**: the target row is joined against
  materialised FROM sources (full v0.4 source grammar,
  JOIN / LEFT JOIN / aliases, self-FROM included); SET and
  WHERE evaluate per (target, FROM) combination against
  the target's ORIGINAL values, a row matched by several
  combinations is rewritten once per combination, and the
  last combination's result is what remains.
- **RETURNING** on INSERT / UPDATE / DELETE: SELECT-shaped
  items (`*`, `t.*`, expressions, aliases) including
  correlated subqueries through the v0.6 machinery;
  aggregates are refused. INSERT returns rows in VALUES
  order, UPDATE returns post-update values (one row per
  updated target row), DELETE returns pre-delete values,
  OR IGNORE / DO NOTHING skips return nothing, and an
  upsert DO UPDATE returns the updated row.

### Root finds this milestone

- **Oracle probes before code** pinned the semantics the
  tests then encode: REPLACE's two-victim case; UPDATE
  FROM's last-combination-wins computed from original
  values (`v=10` over FROM values 1, 2, 4 leaves 14, not
  17); the upsert fallback rule above; DO UPDATE's bare
  names binding the original row; a qualified SET
  left-hand side being a syntax error, not a resolution
  error.
- **The conflict-target WHERE is resolved but never
  evaluated.** Probed on the pin: `ON CONFLICT(u) WHERE 0
  DO NOTHING` arbitrates exactly like `WHERE 1` (the row
  is skipped, no error), while `WHERE nosuchcol` errors
  with "no such column". The predicate only has meaning
  for partial indexes; hipp creates none, so the planner
  name-resolves it for validation and drops it.
- **One apply tail for every write.** UPDATE, OR REPLACE
  victim deletion, and upsert DO UPDATE all funnel through
  the same rewrite/delete helpers, so index maintenance
  lives in exactly one place — that is what keeps
  `integrity_check` green across every new delete path.
- One pre-existing assertion was superseded by design
  (v0.5's `update_or_ignore_skips_conflicts` asserted
  UPDATE OR REPLACE is a parse refusal); it now asserts
  the implemented behaviour.

### Tests — 121, all green (v0.6: 103)

- New `tests/write_side_v07.rs` (16): twin-file
  differentials (hipp writes one file, the oracle CLI a
  twin; final contents compared from the oracle side,
  `integrity_check` on the hipp file) covering INSERT OR
  REPLACE incl. the two-victim and `REPLACE INTO` cases,
  default-abort preservation, UPDATE OR REPLACE / OR
  IGNORE on UNIQUE and rowid conflicts, RETURNING for all
  three verbs (order, star, aliases, rowid, subqueries,
  aggregate refusal), upsert basics / targets / WHERE /
  multi-row / RETURNING / syntax rejections, UPDATE FROM
  (basic, multi-match last-wins, joins + aliases +
  self-FROM + LEFT JOIN, RETURNING scope), and an
  oracle-writes-first both-directions file.
- New `hipp-cli/tests/cli_v07.rs` (2): a v0.7 corpus
  byte-identical through both CLIs (one-shot and stdin),
  and a `.dump` replay check.
- The full suite re-runs on the committed tree before the
  push (see the loop status for the recorded run).

### Gates

- clippy `--workspace --all-targets -D warnings` clean;
  `cargo fmt` write-mode + clean tree; feature matrix
  (no-default, std-only, v04-only, v05-only, v06-only,
  v07-only, indexes-only, indexes+v05, all-features) all
  compile; A1–A11 stay green.
- Tiny profile (thumbv7em): `.text` 74,208 + `.rodata`
  14,960 = **89,168 B** ≤ 128 KiB. Δ **+184 B** vs v0.6's
  88,984 B — under the 4 KiB ratchet line, no justification
  needed. The v07 engine contributes 0 B to tiny (behind
  the feature); the delta is the ungated AST/parser growth
  (conflict action on INSERT/UPDATE, upsert and RETURNING
  AST nodes, the REPLACE verb). Engine statics 0 B outside
  caller heap (`.bss` 98,308 B = demo heap, unchanged);
  tiny-config RAM and zero-steady-state-scan-allocation
  gates green (`tests/gates.rs`).

### Honest gaps

- RETURNING rows are buffered: in autocommit the write
  executes and commits before the first RETURNING row is
  served, where SQLite streams rows as the statement
  runs. Values are identical; only the timing differs,
  and only across connections.
- A row deleted as an OR REPLACE victim can still be
  visited later by the same statement's own scan snapshot
  (writes materialise their scan first) and rewritten;
  SQLite's cursor never sees it. Not differentially
  covered.
- Rowid-changing UPDATE ... FROM with several matching
  combinations applies later combinations against the
  moved row; the combinations' expressions still read
  the original values.
- Carried from v0.6: subqueries in INSERT VALUES are
  refused and `INSERT INTO ... SELECT` is not in the
  grammar; no FROM-clause derived tables or UNION;
  cross-affinity column-to-column comparison keeps the
  v0.4 simplification (probed divergence; it was v0.7's
  named stretch item and the write-side scope plus gates
  consumed the run — it carries forward).

## v0.6 — subqueries in expressions (2026-10-25)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the
loop status.

### Scope shipped

- **Subqueries in expressions** behind the `v06` cargo
  feature (default on, implies `v05`; the tiny profile
  excludes it): scalar subqueries `(SELECT ...)`,
  `x [NOT] IN (SELECT ...)`, and `EXISTS (SELECT ...)` in
  every expression site the engine evaluates — SELECT list,
  WHERE, JOIN ON, GROUP BY, HAVING, ORDER BY, UPDATE SET and
  WHERE, DELETE WHERE. Subqueries nest, and their own SELECT
  is the full v0.4 grammar (joins, aggregates, GROUP BY,
  ORDER BY / LIMIT, further subqueries).
- **Correlation is first-class.** Name resolution is scoped:
  a subquery's own FROM sources answer first, then enclosing
  queries' sources nearest-first (SQLite's shadowing rule),
  with qualified references reaching any enclosing level.
  Resolution happens once at prepare time against the
  combined scope, producing expressions over one flat
  combined row (outer values ++ inner values); ambiguity and
  unknown-name errors are prepare-time, in the right scope.
- **Execution by substitution.** The v0.4 evaluator is
  synchronous and pager-free, so the executors compute each
  subquery node against the pager for the outer row at hand
  (async), then substitute the result into the expression
  tree — a literal for scalar / EXISTS, an IN-list of
  literals for IN — and run the unchanged evaluator. Scalar
  semantics are oracle-probed: first row's first column,
  NULL on no rows, EXISTS false under `LIMIT 0`, the NOT IN
  NULL trap (a NULL anywhere in the list makes NOT IN match
  nothing). A scalar / IN subquery yielding more or fewer
  than one column is a prepare-time `ColumnCountMismatch`
  (the oracle's "sub-select returns 2 columns" case).
- Aggregate queries take the same path with the group's
  representative row as the subqueries' outer context —
  subqueries compose with bare columns exactly as SQLite
  composes them.

### Root finds this milestone

- **The async call graph is a layout cycle.** The natural
  implementation (evaluate → compute subqueries → execute
  subplan → project → evaluate …) makes every future in the
  cycle contain the next, and rustc rejects the infinite
  layout (E0391). Two links are boxed: subplan execution
  itself, and the recursive nested-loop joins (top-level and
  subplan). That is the whole fix; no executor or runtime
  crept in (D2 intact).
- **Oracle probes before code** pinned the semantics the
  tests then encode: scalar takes the first row (not an
  error) on multi-row results, the width error is real,
  `EXISTS (SELECT ... LIMIT 0)` is 0, and correlated
  references through two scope levels resolve to the
  outermost row.
- One pre-existing assertion was superseded by design
  (v0.4's `query_rejections` listed subqueries as refused);
  it now points here.

### Tests — 103, all green (v0.5: 93)

- New `tests/subqueries.rs` (8): scalar differentials
  (uncorrelated, correlated, nested, empty-set, ORDER
  BY/LIMIT steering, in CASE/coalesce/arithmetic), IN
  differentials incl. correlated and expression left sides,
  the IN/NOT IN NULL-semantics suite, EXISTS incl. double
  correlation and HAVING-bearing subqueries, shadowing
  (unqualified names bind inner, qualified reach outer,
  two-alias self-correlation, subqueries in ON / GROUP BY /
  HAVING / ORDER BY), write-side differentials (UPDATE ...
  WHERE IN, UPDATE SET = correlated scalar, DELETE ...
  WHERE EXISTS / NOT IN — hipp-mutated file vs oracle-
  mutated twin, integrity-checked), subqueries over a
  hipp-built file, and the rejection matrix (width,
  aggregate-argument, INSERT VALUES, unknown names in
  scope, views as subquery sources).
- New `hipp-cli/tests/cli_v06.rs` (2): a subquery corpus
  (reads plus subquery-driven UPDATE/DELETE) byte-identical
  through the hipp and oracle CLIs, one-shot and stdin;
  `.dump` after the writes replays into a fresh oracle
  database identically.
- A1–A11 stay green; clippy `--workspace --all-targets
  -D warnings` clean; `cargo fmt` applied with a clean
  tree; the feature matrix (no-default, std-only, v04-only,
  v05-only, v06-only, indexes-only, indexes+v05,
  all-features) all compiles.

### §4.1 gates (re-measured)

- Linked binary (tiny profile, thumbv7em): **.text 74,032 +
  .rodata 14,952 = 88,984 B ≤ 128 KiB.**
  **Delta vs v0.5's 88,936 B: +48 B** — under the 4 KiB
  ratchet line, no justification needed. The v0.6 engine
  (scoped resolver, SubPlan planner/executor) contributes
  0 B to tiny — it lives behind `v06`/`v04`; the +48 B is
  the ungated AST/parser gaining the three subquery `Expr`
  variants. Headroom left: 42,088 B.
- Statics: `.bss` 98,308 B is again entirely the demo's
  caller heap; **engine .data+.bss outside caller memory =
  0 B ≤ 8 KiB.**
- Tiny-config RAM and zero steady-state scan allocations:
  `tests/gates.rs` green (unchanged).

### Honest notes / divergences

- A subquery inside an aggregate's arguments
  (`sum((SELECT ...))`) is refused at plan time with
  `Error::Unsupported`: it would need per-group-row
  execution. Subqueries *around* aggregates, and aggregates
  *inside* subqueries, both work.
- Subqueries in INSERT VALUES are refused (VALUES are
  constant-folded at plan time, where no executor exists);
  `INSERT INTO ... SELECT` is not in the grammar at all.
- No FROM-clause derived tables and no UNION anywhere —
  both remain outside the grammar engine-wide, subqueries
  included.
- Subqueries execute once per outer row, correlated or
  not: there is no uncorrelated hoisting yet, so an
  uncorrelated subquery in a hot WHERE re-runs per row.
  Correctness-first, like the v0.4 materialising planner.
- UPDATE's WHERE subqueries observe the pre-statement
  table (matches are materialised before any row moves);
  SET subqueries run per row as the statement applies, so
  they observe rows the same statement already updated —
  SQLite's row-by-row behaviour.
- A subquery in a JOIN's ON clause is evaluated against
  the partially joined row; references to sources not yet
  joined read as NULL (the evaluator's lenient row reads),
  where SQLite scopes ON to the joined sources.
- Cross-affinity column-to-column comparisons keep the
  v0.4 simplification (affinity adjustment only for a
  direct column operand against a non-column): probed
  against the oracle, `int_col = text_col` applies NUMERIC
  affinity in SQLite even between two columns. Pre-existing,
  not a subquery gap; the correlated differentials use
  type-consistent keys, as real schemas do.

## v0.5 — UPDATE, DELETE, DROP TABLE + pager spill (2026-10-24)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the loop
status.

### Scope shipped (SPEC §2 v0.5)

- **UPDATE / DELETE / DROP TABLE** behind the `v05` cargo
  feature (default on, implies `v04`; the tiny profile excludes
  it). The write side deferred since v0.2 is now covered end to
  end, with full index maintenance (insert-only before v0.5).
- **Grammar** (oracle-probed): `UPDATE [OR IGNORE] t [[AS]
  alias] SET col = expr, ... [WHERE expr]` and `DELETE FROM t
  [[AS] alias] [WHERE expr]` with the v0.4 general expression
  evaluator on both sides; SET targets are bare column names
  (a qualified target is a syntax error in the oracle too);
  `DROP TABLE [IF EXISTS] name`. OR REPLACE is parse-rejected,
  UPDATE ... FROM / ORDER BY / LIMIT / RETURNING are not
  supported (see honest notes).
- **Executors**: matching rows are materialised first (the
  row set stays stable while the tree is rewritten), SET
  expressions evaluate against the ORIGINAL row, then each row
  is applied as index-delete → row delete → row insert →
  index-insert. Rowid / rowid-alias assignment follows the
  oracle: integers move the row (collision → DuplicateRowid),
  NULL or non-integer is a datatype-mismatch error (unlike
  INSERT's NULL auto-assign). UNIQUE pre-checks run per unique
  index before any page is touched; OR IGNORE skips a
  conflicting row and continues. Statement atomicity is the
  engine's existing rule (pager rollback on error).
- **Index delete** (was insert-only): a recursive true-B-tree
  delete. Interior-resident entries are replaced by their
  predecessor (greatest entry of the left subtree) with the
  replacement fit-checked BEFORE the predecessor is touched;
  the entry's overflow chains are freed and re-owned
  canonically. See the first root find for the rebuild
  fallbacks.
- **DROP TABLE**: every page of the table and its indexes
  (including sqlite_autoindex trees and overflow chains)
  returns to the freelist; schema rows (table + indexes) are
  deleted and the schema cookie bumped. Name-kind refusals are
  oracle-exact: DROP TABLE on a view and DROP VIEW on a table
  are errors even under IF EXISTS (new `Error::IsView` /
  `Error::IsTable` kinds); UPDATE/DELETE on a view is refused.
  All three statements work inside explicit transactions,
  including full rollback of a DROP TABLE.
- **Pager spill** (SPEC §5.2, deferred until now): a dirty
  frame can be written to the main file mid-transaction to
  free a cache slot. The page's original is already journaled,
  so this is SQLite's own protocol; rollback replays the
  journal whenever a spill happened, and commit's page-write
  stage loads spilled pages back instead of requiring them
  resident. Wide write statements no longer die with
  CacheExhausted at the default 16-frame cache (wide
  multi-row INSERTs had the same cliff).

### Root finds this milestone

- **A non-root B-tree page may never be empty.** The first
  delete implementation left emptied leaves linked (dividers
  stay valid separators, or so the reasoning went); the
  oracle's integrity_check rejects such trees. Bisected on
  oracle-built 512-byte-page trees: deleting exactly the 29
  rows of the first leaf corrupted the file, `id % 3 = 0`
  deletes (no leaf empties) were clean. SQLite rebalances;
  hipp's no-merge design **rebuilds the B-tree from its
  surviving entries** when a delete would empty a non-root
  leaf — the signal is raised before any mutation on that
  path, and the rebuild skips only the original target
  (payloads carried verbatim for tables; the index variant
  collects decoded entries). The same rule covers the
  predecessor dance: if removing the predecessor would empty
  its leaf, the top level rebuilds without the target and the
  predecessor survives in its leaf.
- **`create_empty_root` silently dropped its page image at
  commit** (latent since v0.2): it used `load` +
  `mark_frame_dirty`, which neither journals the page nor
  registers it in the commit write set — and the set flag
  makes a later `dirty_page` skip registration entirely.
  Harmless for its only previous caller (freshly allocated
  pages are registered by the allocator); the table rebuild
  resetting a live interior root was the first existing-page
  caller, and the symptom was a tree page simultaneously on
  the freelist. `create_empty_root` now goes through
  `dirty_page`.
- **SET expressions read the original row**: `SET b = 1,
  b = b + 100` on b=10 yields 110 in the oracle (assignments
  do not see each other), with the last assignment winning —
  encoded in the executor and the tests.
- **DROP VIEW on a table under IF EXISTS silently no-opped
  in v0.4**; the oracle errors ("use DROP TABLE to delete
  table t"). Fixed in the v0.5 kind checks.

### Tests — 93, all green (v0.4: 76)

- New `tests/write_side.rs` (15): UPDATE/DELETE differentials
  both directions (hipp writes → oracle reads; oracle writes
  → hipp reads and writes on), general expressions in SET and
  WHERE, rowid/alias assignment rules (move, collision,
  datatype mismatch), index maintenance incl. point lookups
  through updated keys, mid-statement UNIQUE failure rolling
  the whole statement back, OR IGNORE skips, deep 512-byte
  trees built by each engine and mutated by the other,
  overflow pages returning to the freelist and being reused
  without file growth, DROP TABLE page return + schema rows
  gone + name reuse, view/table name-kind refusal matrix,
  and explicit-transaction commit/rollback of all three
  statements.
- New `hipp-cli/tests/cli_v05.rs` (2): a write-side corpus
  byte-identical through the hipp and oracle CLIs; `.dump`
  after UPDATE/DELETE/DROP TABLE replays into a fresh oracle
  database and reads back identically.
- First runs, reported as they happened: the suite opened at
  11/15 — deep-tree deletes died with CacheExhausted (→ the
  spill above), one test used a qualified SET target the
  oracle also rejects, and deep deletes corrupted (→ the
  empty-page invariant and the `create_empty_root` fix
  above). The remaining failures were test-side arithmetic
  (`'upd-' || 7` is `'upd-7'`, not `'upd-0007'`; a freelist
  expectation that ignored the mid-statement rebuild).
- A1–A11 stay green; clippy `--workspace --all-targets
  -D warnings` clean; the full feature matrix (no-default,
  std-only, v04-only, v05-only, indexes-only, indexes+v05)
  compiles clean; `cargo fmt` applied with a clean tree.

### §4.1 gates (re-measured)

- Linked binary (tiny profile, thumbv7em): **.text 73,984 +
  .rodata 14,952 = 88,936 B ≤ 128 KiB.**
  **Delta vs v0.4's 88,336 B: +600 B** — under the 4 KiB
  ratchet line. The v0.5 dialect itself contributes 0 B to
  tiny (feature-gated out, like v04); the growth is the
  ungated core machinery every profile shares: the pager
  spill and the delete-rebuild path (`btree_delete` was
  already ungated; its rebuild helper and
  `collect_table_pages` now are too). Headroom left:
  42,136 B.
- Statics: `.bss` 98,308 B is again entirely the demo's caller
  heap; **engine .data+.bss outside caller memory = 0 B
  ≤ 8 KiB.**
- Tiny-config RAM and zero steady-state scan allocations:
  `tests/gates.rs` green (unchanged).

### Honest notes / divergences

- OR REPLACE is rejected at parse time (`Error::Parse`,
  unsupported clause). The oracle deletes the conflicting
  row and proceeds; the machinery to support it (delete on
  conflict, reusing the OR IGNORE path) is a named follow-up,
  not a silent gap.
- UPDATE ... FROM is not supported (the FROM clause is
  rejected as trailing input); correlated updates must be
  written as expressions over the target row.
- Subqueries in SET/WHERE and RETURNING remain unsupported,
  as everywhere else in hipp (pre-existing).
- Aggregates in SET/WHERE resolve during planning and are
  refused with `Error::Unsupported` (the oracle raises a
  misuse-of-aggregate error; hipp's errors are typed, not
  text-matched, everywhere).
- No merge or redistribution on delete: underfull pages stay
  linked (format-legal, integrity-clean). A delete that
  empties a non-root leaf rebuilds that B-tree — O(tree) for
  that delete, so bulk range deletes are quadratic in the
  worst case (divided by leaf capacity). An interior index
  replacement whose predecessor payload cannot fit the
  interior page also rebuilds.
- UPDATE/DELETE materialise their matching rows before
  rewriting (statement memory grows with matched rows; the
  tiny profile has no write statements).
- A failed statement inside an explicit transaction aborts
  the whole bracket (pre-existing engine rule, stricter than
  SQLite's statement-level rollback).
- Spill writes dirty pages to the main file before commit;
  within hipp's single-connection model (SPEC §5.1) the
  journal keeps crash recovery exact, but a second opener
  mid-transaction could observe uncommitted page images.

## v0.4 — joins, aggregates + GROUP BY, expressions, ALTER, inert views (2026-10-23)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the loop
status.

### Scope shipped (SPEC §2 v0.4)

- **Expression evaluator** (`src/query.rs`, behind the `v04`
  cargo feature, default on; the tiny profile excludes it):
  arithmetic with SQLite's coercion rules (prefix numeric parse
  for text, integer division truncates, `/`/`%` by zero yield
  NULL), `||`, the comparison family with comparison-affinity
  adjustment for direct column operands, three-valued
  AND/OR/NOT, `IS`/`IS NOT`, `IN (list)`, `BETWEEN`, `LIKE`,
  `CASE` (both forms), `CAST` (dedicated prefix-parse rules),
  and a scalar library: abs, length, upper, lower, coalesce,
  ifnull, nullif, typeof, substr, round, hex, trim/ltrim/rtrim,
  replace, instr, sign, iif, scalar min/max. INSERT VALUES now
  takes full constant expressions (plan-time evaluated).
- **General WHERE**: any expression over the FROM sources; the
  legacy fast paths (full scan / rowid seek / index lookup, zero
  materialisation) still serve the simple shapes, and a simple
  shape that only fails as `Unsupported` falls through to the
  general planner.
- **Joins**: nested-loop `JOIN`/`INNER JOIN`/`LEFT [OUTER]
  JOIN`/`CROSS JOIN`/comma joins with `ON` expressions, table
  aliases, self-joins, qualified column resolution with
  ambiguity detection.
- **Aggregates + GROUP BY**: count/sum/avg/total/min/max/
  group_concat (with DISTINCT forms), GROUP BY (keys,
  expressions, ordinals and output aliases — as in ORDER BY),
  HAVING, and SQLite's bare-column rule including the lone
  min/max extremum-row representative. GROUP BY emits groups in
  key order, like the oracle's sorter.
- **ORDER BY / LIMIT / OFFSET / SELECT DISTINCT** through the
  query planner (stable sort, SQLite NULL placement falls out
  of the comparison order), including ordinals, output aliases,
  and expression keys.
- **ALTER TABLE ADD COLUMN**: stored CREATE TABLE text is
  rewritten exactly as SQLite does it (`, <coldef>` before the
  closing paren, schema rowid preserved); existing rows read
  the new column's default via the short-record rule — no record
  migration. Refuses (oracle-matching) PRIMARY KEY / UNIQUE /
  generated columns, NOT NULL without a non-NULL constant
  default, and non-constant defaults.
- **Views as stored SQL (inert)**: CREATE VIEW validates its
  SELECT at prepare time (tables, columns, explicit column-list
  width) and stores the statement verbatim (root page 0);
  DROP VIEW; the catalog derives view column names (explicit
  list, column refs, aliases, `*` expansion, expression text).
  Selecting FROM a view is `Error::Unsupported` — views are
  never executed in v0.4. hipp-cli: `.tables` lists views,
  `.schema` prints them with the oracle's `/* name(cols) */`
  comment line, `.dump` replays them after the table data (and
  its output now follows the oracle's dump order throughout:
  tables+data, then indexes, then views).

### Root finds this milestone

- **substr's negative length counts back from the start
  point**, and the y=0 rule only adjusts a positive length;
  the full coordinate algorithm was pinned by probing the
  oracle (`substr('abcdef', 2, -1)` is `'a'`). The first
  implementation produced `''`/`'abc'` there.
- **round(x, -1) behaves as round(x, 0)** in SQLite (its scale
  loop never runs for a negative count) — hipp first divided
  by ten (123.456 → 120.0 vs the oracle's 123.0).
- **sum() of non-numeric text is real 0.0**, not integer 0
  (`sum('abc')` = 0.0): sum/total/avg use a dedicated
  contribution conversion distinct from arithmetic's.
- GROUP BY terms resolve as output aliases/ordinals first
  (SQLite's rule, shared with HAVING and ORDER BY) — found
  when `GROUP BY parity` (an alias) failed to prepare.

### Tests — 76, all green (v0.3: 64)

- New `tests/query.rs` (10): joins differential (14 queries,
  incl. LEFT/self/3-table), aggregates + GROUP BY (21 queries
  incl. empty-table and bare-column min/max), expressions (25
  queries incl. coercion, three-valued logic, CAST matrix),
  ORDER BY/LIMIT/DISTINCT (14 queries), substr/round edge
  battery, INSERT expressions verified from the oracle side,
  ALTER both directions (stored-SQL byte equality + integrity),
  ALTER rejections, views both directions (stored row shape,
  inert refusal, DROP VIEW), planner rejections (aggregates in
  WHERE, unknown functions, ambiguous columns, subqueries).
- New `hipp-cli/tests/cli_v04.rs` (2): one-shot v0.4 corpus
  byte-identical to the oracle; `.schema` with views
  byte-identical incl. the column-comment line; `.dump`
  replayed into a fresh oracle database reproduces the schema.
- Two pre-existing assertions were updated where v0.4
  supersedes them by design (a non-rowid predicate in
  tests/dialect.rs and an unindexed predicate in
  tests/indexes.rs were `Error::Unsupported` in v0.2/v0.3;
  both are now served by the query planner, covered above).
- A1–A11 stay green; clippy `--workspace --all-targets
  -D warnings` clean; the `v04`-off and `indexes`-off
  configurations compile clean.

### §4.1 gates (re-measured)

- Linked binary (tiny profile, thumbv7em): **.text 73,392 +
  .rodata 14,944 = 88,336 B ≤ 128 KiB.**
  **Delta vs v0.3's 84,524 B: +3,812 B** — under the 4 KiB
  ratchet line. The query engine itself contributes 0 B to
  tiny (the `v04` feature gates the evaluator, the query
  planner/executor, and the ALTER/VIEW grammar); the growth is
  ungated core work every profile shares: new lexer tokens,
  CREATE TABLE constraint-fact capture (NOT NULL / generated /
  non-constant defaults, needed to validate ADD COLUMN),
  view cataloguing (views are recorded as metadata in every
  profile), and the INSERT plan-time value plumbing. Headroom
  left: 42,736 B.
- Statics: `.bss` 98,308 B is again entirely the demo's caller
  heap; **engine .data+.bss outside caller memory = 0 B ≤ 8 KiB.**
- Tiny-config RAM and zero steady-state scan allocations:
  `tests/gates.rs` green (unchanged; the legacy scan paths are
  untouched).

### Honest notes / divergences

- The query planner materialises: joins/aggregates/general
  predicates copy rows into statement-owned vectors. Fine at
  v0.4's correctness-first stage; the §4.1 zero-allocation gate
  binds the legacy scan hot loop, which is unchanged, and the
  tiny profile never runs the query engine.
- Comparison affinity is applied when an operand is a *direct*
  column reference (SQLite's documented main case); affinity
  does not propagate through nested expressions, and IN-list
  affinity follows the same direct-column rule.
- Bare columns in an aggregate query read the group's first
  row (SQLite picks an arbitrary row; the lone min/max case
  follows SQLite's documented extremum-row rule exactly).
- Integer overflow in `sum()` falls back to a real
  accumulation; SQLite raises an overflow error there. `+`/`-`
  overflow likewise yields a real rather than SQLite's error.
- ADD COLUMN validates defaults that are plain literals;
  a parenthesised expression default such as `(1+2)` is read
  as its leading literal (a pre-existing CREATE TABLE
  approximation), where the oracle evaluates the expression.
- View column derivation for foreign views whose SQL is
  outside hipp's grammar yields no derived names (the view is
  still catalogued and listed); the CLI's `/* name(cols) */`
  comment then prints an empty list where the oracle prints
  resolved names.
- LIKE matching is per-character with ASCII case folding,
  like SQLite's default; non-ASCII text is matched on chars
  decoded as UTF-8 (invalid UTF-8 falls back to an empty
  haystack — SQLite would match byte-wise).

## v0.3 — index B-trees + freelist reuse hardening (2026-10-22)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the loop
status.

### Scope shipped (SPEC §2 v0.3)

- **Index B-trees** (SQFMT §1.5/§1.6) behind the `indexes` cargo
  feature (default on; the tiny profile excludes it per §2):
  key = record of the indexed columns with the rowid appended;
  true B-tree insert — interior divider keys are real entries
  (verified against the oracle: promoted keys are findable only
  in interior pages); index overflow with the §1.6 X/M/K
  formulas; ASC/DESC per column.
- **CREATE [UNIQUE] INDEX** on plain column names (expressions,
  partial indexes and non-BINARY collations parse-reject);
  population of pre-existing rows; index maintenance on INSERT.
- **Secondary-index lookup** through the planner's access seam:
  `WHERE <first indexed column> = <literal>` uses the index
  (plan-time literal conversion with the column's affinity,
  including `'2'`/`2.0` forms); composite indexes serve prefix
  lookups; a UNIQUE index wins when several match.
- **UNIQUE enforcement** via implicit (`sqlite_autoindex_*`)
  indexes: column and table-level UNIQUE sets, TEXT/composite
  PRIMARY KEYs, and the `INTEGER PRIMARY KEY DESC` quirk (not a
  rowid alias — verified against the oracle, as is NULL into a
  TEXT PRIMARY KEY being legal heritage behaviour: duplicate
  NULLs never conflict anywhere).
- **DROP INDEX** (autoindexes refuse with a named error) — the
  freelist producer that makes reuse testable end to end.
- **Freelist hardening** (SQFMT §1.3.8): `free_page` mirrors the
  oracle's trunk/leaf behaviour; `alloc_page` now validates
  foreign freelists before handing pages out — duplicate leaves,
  out-of-range pointers, overflowing leaf counts and trunk
  cycles are `Error::CorruptPage`, never a page handed out twice.
- **Opaque foreign indexes:** an index whose SQL the grammar
  cannot represent (expressions, partial, collations) is
  catalogued without resolution; reads work, INSERTs into its
  table refuse with `Error::Unsupported` rather than silently
  desynchronising it.
- hipp-cli: `.schema` prints explicit indexes after their table
  (byte-identical to the oracle shell on the test corpus);
  `.dump` replays them; implicit indexes never print.

### Root finds this milestone

- **Index interior keys are entries, not separators:** with
  512-byte pages and 300 rows, the oracle's index root holds
  divider keys (e.g. key 520) that appear in NO leaf — the
  lookup descent must equality-check interior keys. (Found by
  decoding oracle pages byte-by-byte after a naive B+-tree
  assumption produced wrong divisibility counts.)
- **Exact int/real comparison:** the record comparator cast
  integers to f64, equating 9007199254740993 with
  9007199254740992.0. `compare_values` now cross-compares by
  truncation + fraction with 2^63 bounds (unit-tested).

### Tests — 64, all green (v0.2: 45)

- New `tests/indexes.rs` (16): CREATE INDEX both directions
  (oracle `integrity_check` + schema-row equality + index-leaf
  page kind), oracle-built indexes read/maintained by hipp,
  populated-table creation, lookup matrix (types, affinity
  conversions, misses, NULL), composite prefix, DESC, deep tree
  at 512-byte pages (interior-resident keys both directions),
  2000-byte overflow keys, UNIQUE from all four origins incl.
  multi-row statement atomicity, autoindex naming/order/SQL-
  NULL shape vs oracle, CREATE UNIQUE INDEX over duplicates
  fails atomically, CREATE INDEX inside transactions
  (commit/rollback), DROP INDEX + page reuse bound, autoindex
  drop refusal, oracle multi-trunk freelist consumption, three
  hand-mangled freelist corruptions → `CorruptPage`, opaque
  expression index (reads ok, writes refused).
- New `hipp-cli/tests/cli_indexes.rs` (2): `.schema` with
  indexes byte-identical to the oracle; `.dump` replays indexes
  into a fresh oracle database.
- A1–A11 all still green; clippy `-D warnings` clean; the
  `indexes`-off (tiny) configuration compiles and its lib tests
  pass.

### §4.1 gates (re-measured)

- Linked binary (`--profile tiny --target thumbv7em-none-eabihf
  --no-default-features --bin hipp_thumb_demo`):
  **.text 69,604 + .rodata 14,920 = 84,524 B ≤ 128 KiB.**
  **Delta vs v0.2's 80,784 B: +3,740 B** — under the 4 KiB
  ratchet line. The index engine itself contributes 0 B to
  tiny (feature-gated); the growth is ungated core
  correctness: CREATE TABLE constraint parsing (the syntax
  must be consumed in every profile), freelist-consumption
  validation, and the exact int/real comparator. Headroom
  left: 46,548 B.
- Statics: `.bss` 98,308 B is again entirely the demo's caller
  heap; **engine .data+.bss outside caller memory = 0 B ≤ 8 KiB.**
- Tiny-config RAM and zero steady-state scan allocations:
  `tests/gates.rs` green (unchanged).

### Honest notes / divergences

- Index-aware lookup is equality on the index's **first**
  column only (plus the rowid seam from v0.2); richer
  predicates remain `Error::Unsupported` / parse-rejected.
  This is the §2 seam growing one step, as designed.
- No DELETE / UPDATE / DROP TABLE yet: index maintenance is
  insert-only, and `btree_delete` exists solely to remove a
  dropped index's schema row (it does no merge/redistribution —
  an emptied schema leaf stays linked, which the format allows
  and `integrity_check` accepts).
- NOT NULL is still parse-but-unenforced everywhere; for
  PRIMARY KEY columns this matches the oracle's verified
  3.54.1 behaviour (NULL into a TEXT PRIMARY KEY succeeds).
- The tiny profile records indexes as opaque metadata only:
  it never creates, maintains or enforces them (SPEC §2
  exclusion), and — as before any index support existed —
  writing an indexed table from the tiny profile would
  desynchronise its indexes. Standard-profile connections
  refuse such writes safely (opaque rule above).
- All v0.2 honest notes still hold unless superseded here (in
  particular: a statement error inside an explicit transaction
  aborts the whole bracket — including a UNIQUE violation).

## v0.2 — workspace, hipp-cli, first dialect growth (2026-10-22)

Pushed to GitHub (madmax983/hipp) via API replay under the
conformance-loop mandate; remote tip + tree recorded in the loop
status. (v0.1 was pushed 2026-10-18, remote tip 6bd49a31.)

### Scope shipped (SPEC §9.4)

- **Workspace** per §10: root `hipp` core crate (unchanged layout,
  still zero-dependency `no_std` + `alloc`) + `hipp-cli` member.
  Core and CLI versions 0.2.0.
- **hipp-cli** (D4): std binary `hipp` — interactive, stdin-batch
  and one-shot modes; list-format output byte-identical to the
  pinned oracle CLI over the shared subset; dot-commands
  `.tables` / `.schema` / `.dump` / `.open` / `.exit` (`.quit`).
  The CLI is a separate std crate and contributes **0 bytes** to
  the core's thumbv7em tiny build (ratchet rule 4).
- **Multi-row VALUES** in INSERT: per-row widths checked at plan
  time; a failing row aborts the whole statement (atomic).
- **`WHERE rowid = <literal>`** on SELECT: lowers to a point
  lookup (`btree::seek_rowid`) when the column is the rowid or
  its alias; literals coerce per SQLite's comparison rules
  (`'3'`, `2.0`, `'1.0'` match; `2.5`, `'abc'`, `NULL`, blobs
  match nothing, never error). The plan's `Access` enum
  (FullScan / Rowid / Empty) is the seam general WHERE hangs off;
  a predicate on any other column is `Error::Unsupported`.
- **Explicit transactions** BEGIN / COMMIT / ROLLBACK
  (`TRANSACTION` keyword optional; `END` = COMMIT): the pager
  transaction opens lazily at first write (read-only brackets
  never touch the journal); write statements skip autocommit
  while a bracket is open; COMMIT drives the resumable commit
  machine; new error `Error::TransactionState` for nested BEGIN
  and COMMIT/ROLLBACK without a bracket.

### Root finds this milestone

- **Pager header snapshot:** a rollback from the `Writing`
  stage deleted the journal but did not restore in-memory header
  fields (schema cookie, freelist, page count) mutated by the
  aborted transaction. `begin_write` now snapshots the header and
  the `Writing`-stage rollback restores it (the commit-begun path
  already re-read page 1). This hardens autocommit error paths
  too, not just explicit ROLLBACK.
- **Oracle shell caret notation:** the sqlite3 CLI does NOT print
  blob/text bytes raw in list mode — bytes < 0x20 (except tab and
  newline) print as `^` + (byte+0x40), and TEXT values truncate
  at the first NUL byte. Verified byte-by-byte against the
  pinned oracle over the full 0x00–0x1F range; hipp-cli now
  renders identically. (A `cat -v` smoke test masked this; the
  byte-compare differential caught it.)

### Tests — 45, all green (v0.1: 25)

- Core: 6 unit + 32 integration — smoke 2, differential (A1–A5,
  A7) 6, crash (A6) 2, arenas (A9) 2, gates (A10) 2, fuzz (A8) 2,
  pending (A11) 6, **dialect 10 (new)**.
- `tests/dialect.rs`: multi-row VALUES both directions
  (hipp→oracle and oracle→hipp, `integrity_check` ok), atomicity
  of width-mismatch and duplicate-rowid failures; WHERE rowid=
  hit/miss matrix (alias, `rowid`/`oid`/`_rowid_`, negative,
  converted literals), deep-tree seeks with oracle agreement;
  explicit transactions — COMMIT visibility from the oracle side,
  ROLLBACK invisibility (rows *and* a created table), CREATE in
  a bracket, state errors, statement-error aborts the bracket.
- A11 extended (`tests/pending.rs`): COMMIT future of an
  explicit transaction dropped mid-commit → `OperationInFlight`
  for a second driver, `resume_txn` finishes to post-image and
  closes the bracket; ROLLBACK future dropped mid-flight →
  bracket survives, retry restores pre-image; INSERT future
  dropped mid-write inside a bracket → ROLLBACK resolves,
  pre-image on the connection and after reopen.
- hipp-cli: **7 differential tests** (`hipp-cli/tests/cli_diff.rs`)
  — one-shot and stdin-batch stdout **byte-identical** to the
  oracle over a corpus covering all of the above plus real
  rendering (`%!.15g`) and control-byte blobs; `.tables`/`.schema`
  exact; `.dump` re-imported into the oracle renders identically;
  `.open` switching verified from the oracle side; `:memory:`;
  error cases exit nonzero with stderr.

### §4.1 gates (re-measured)

- Linked binary (`--profile tiny --target thumbv7em-none-eabihf
  --no-default-features --bin hipp_thumb_demo`):
  **.text 65,884 + .rodata 14,900 = 80,784 B ≤ 128 KiB.**
  **Delta vs v0.1 baseline 75,704 B: +5,080 B (+6.7%)** — over
  the 4 KiB ratchet line; justification in the loop status
  (rowid seek path + multi-row insert + transaction control +
  header snapshot; CLI contributes 0 B). Headroom left: 50,288 B.
- Statics: `.bss` 98,308 B is again entirely the demo's caller
  heap; **engine .data+.bss outside caller memory = 0 B ≤ 8 KiB.**
- Tiny-config RAM and zero steady-state scan allocations:
  `tests/gates.rs` green (unchanged from v0.1).

### Honest notes / divergences

- A write-statement error inside an explicit transaction aborts
  the **whole bracket** (pager rollback + bracket closed) —
  stricter than SQLite, which aborts only the statement. Safe
  direction; named here until statement-level savepoints exist.
- CLI divergences from the oracle shell (by design, D4 scope):
  `.tables` prints one name per line (oracle uses a grid);
  errors print hipp's remedy-named Debug form (`Error: Parse {
  offset, kind }`, `Error: Schema { kind }`, …) and exit 1;
  prompts only when stdin is a terminal; no `.import`/`.export`,
  no modes beyond list, no line editing.
- `WHERE` beyond `rowid =` is not implemented: other predicate
  shapes are parse-rejected (`UnsupportedClause`) and a
  non-rowid equality column is `Error::Unsupported`.
- Real rendering in the CLI implements SQLite's `%!.15g` for the
  corpus in the differential tests (fixed/scientific ranges,
  integral reals, 15 significant digits); exotic values outside
  the corpus are best-effort.
- `literal`→rowid text coercion mirrors SQLite's well-formed
  rule (whole string must convert); leading/trailing spaces are
  trimmed.

## v0.1 — file-format compatibility (2026-10-18) — summary

CREATE TABLE + single-row INSERT + full-scan SELECT; B-tree
splits + overflow; rollback journal byte-exact; hot-journal
recovery; resumable commits. Acceptance A1–A11 all PASS vs the
3.54.1 pin (evidence: `tests/differential.rs`, `tests/crash.rs`,
`tests/arenas.rs`, `tests/gates.rs`, `tests/pending.rs`,
`tests/fuzz_smoke.rs`; A8 included a 10⁶-input spec-scale pass).
Tiny gates at ship: .text+.rodata **75,704 B**, engine statics
0 B, 25 tests green. Full v0.1 record: git history (STATUS at
8b8ebbc) — including its honest notes, which still hold unless a
v0.2 note above supersedes them.
