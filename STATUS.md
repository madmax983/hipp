# hipp — build status

Spec: `docs/SPEC.md` rev. 0.3. Oracle pin (D1): SQLite **3.54.1**
(official amalgamation 3540100, `~/workspace/hipp-design/oracle/sqlite3`,
`--version` verified). All differential results are against the pin.

## v0.16 — SQLite-parity push

Milestone v0.16 closes as much of the gap with the pin as
possible under ADR 0001 (silent wrong answers fixed ungated;
new surface behind the `v16` rung or a leaf feature; the tiny
profile never grows). Each unit of the push records itself
below.

### Joins, ordering and query-shape completion (2026-11-02)

**Status:** implemented, differential-tested against the
oracle, gated (below). Census (158 probes): MATCH 53 → 61,
silent-wrong DIFF 8 → 4 (the four left — CHECK, generated
columns, expression DEFAULTs, column COLLATE — belong to
other units).

**Shipped — ungated (fixes on claimed v0.4 paths):**

- **RIGHT / FULL [OUTER] JOIN.** `RIGHT`, `FULL` and
  `NATURAL` were eaten as a table *alias*, so `a RIGHT JOIN b`
  ran as an inner join. They are now join keywords (never a
  bare alias, as on the pin), parsed with SQLite's
  `sqlite3JoinType` algebra (LEFT+RIGHT = FULL, OUTER only
  qualifies an outer join, `INNER OUTER` / `LEFT CROSS` /
  bare `OUTER` refused — `ParseKind::UnknownJoinType`).
  Executor: the main nested loop in FROM order, then each
  RIGHT / FULL source — earlier first — emits its
  never-matched rows in its own row order, NULL-padded on the
  left and joined onward. One async join executor now serves
  top-level queries, subqueries and UPDATE ... FROM (three
  copies merged; `SourcePlan` is `query::SubSource`); the
  synchronous v0.4 path got the same tail.
- **NATURAL / USING** (`SchemaKind::UsingColumnMissing`,
  `ParseKind::NaturalJoinConstraint`, ambiguity as on the
  pin): `SELECT *` shows a join column once, at the left-most
  source; unqualified references follow the pin's
  `lookupName` (left-most side after INNER/LEFT, right side
  after RIGHT, `coalesce(...)` after FULL); star expansion of
  a source left of a RIGHT/FULL join reads the joined value
  (even `a.*`); qualified `t.col` still reads its own side.
- **ON clauses naming a later source:** an inner join's ON is
  hoisted into WHERE (hipp read a NULL slot and dropped every
  row); on an outer join, or anywhere once a RIGHT/FULL join
  exists, `SchemaKind::OnReferencesLaterSource`.
- **Subquery inside a non-last source's ON** read misaligned
  slots (wrong rows): the row is now padded to the full FROM
  width for such an ON, and scope bases are computed from
  each group's end slot (needed by UPDATE ... FROM, whose
  FROM clause is now its own join scope: its ON / USING never
  see the target, as on the pin).
- **ORDER BY ... NULLS FIRST / LAST** was parsed and ignored
  (and `NULLS` alone accepted): honoured at every ORDER BY
  site — queries, compound ORDER BY, subqueries, derived
  tables, INSERT ... SELECT.
- **Term rules:** out-of-range ORDER BY / GROUP BY ordinals
  error (`TermOutOfRange`; the pin's 32-bit "is integer" test
  — `ORDER BY 2147483648` stays a constant); HAVING has no
  ordinals (`HAVING 1` read item 1); a real column shadows an
  alias in GROUP BY / HAVING (not ORDER BY); aggregates in
  GROUP BY (`MisusedAggregate`); HAVING on a non-aggregate
  query (`HavingWithoutAggregate`).
- **IN over an empty set** is 0 (NOT IN 1) even for a NULL
  left operand (hipp gave NULL); a real column named `rowid`
  now beats another source's rowid instead of "ambiguous".

**Shipped — `v16` (new rung, `v16 = ["v10"]`, in `full`):**
LIMIT / OFFSET as expressions (constant or uncorrelated
subquery, both `LIMIT x OFFSET y` and `LIMIT y, x`; resolved
against an empty scope like the pin; value via the pin's
`MustBeInt` — NUMERIC affinity then lossless integer, else
`DatatypeMismatch`, raised even over no rows; negative LIMIT
= none, negative OFFSET = 0; windows saturate instead of
wrapping on 32-bit targets); result-column aliases inside
WHERE / ON / GROUP BY / HAVING / ORDER BY expressions and
their subqueries (current scope's columns, then its aliases,
then outer scopes; never in the result list itself);
`x [NOT] IN tablename` (one-column tables); string-literal
aliases (`AS 'q'`, bare `'q'`).

**Review fixes (second pass, same day):**

- *v16 alias with a subquery, used from a nested subquery
  with its own FROM* (silent wrong rows): the alias's item was
  cloned with its subquery slots numbered for its home scope,
  but evaluated after the nested sources. It is now rebased
  onto the deeper row (`query::alias_from_outer` /
  `shift_expr`: slots and rowid sources at or past the home
  stack's end move by the difference).
- *v16 aliased aggregate inside a subquery* (HAVING / ORDER
  BY re-aggregated the subquery's rows): refused
  (`Error::Unsupported`) — on the pin the aggregate stays
  the outer group's value, which hipp cannot evaluate inside
  a subquery. In WHERE both engines refuse.
- *Compound ORDER BY* (ungated): terms now resolve per core
  as in the pin's `resolveCompoundOrderBy` — an `AS` name,
  else the term resolved in that core's FROM scope alone and
  matched to the item that is the same reference
  (`CompKeys`; replaces the `(qualifier, name)` keys). After
  `RIGHT JOIN ... USING (id)`, `ORDER BY id` is `b.id`; after
  FULL, `coalesce(a.id, b.id)`; `ORDER BY b.id` is accepted
  and `a.id` refused there. Also fixes older acceptances: an
  ambiguous bare name (`FROM a, b ... ORDER BY id`) and an
  expression item's text used as a name now error, as on the
  pin.
- *Duplicate column names* (ungated): a derived table's and
  a view's duplicates become `id`, `id:1`, ... (the pin's
  `sqlite3ColumnsFromExprList`, shared with CTAS's
  `rules::unique_names`), so NATURAL / USING over such a
  derived table no longer drops the later columns from
  `SELECT *`, and `"id:1"` resolves.
- *View column lists* (ungated, `.schema` comment and
  `ViewDef.columns`): NATURAL / USING columns once, `t.*`
  unaffected, duplicates renamed.
- *v16 scalar / EXISTS LIMIT*: the pin's `LIMIT (X<>0)`
  rewrite (the `0` with NUMERIC affinity): `LIMIT 2.5`,
  `'x'`, `x'00'`, `9223372036854775808` act as `LIMIT 1`,
  `'0'` as 0, NULL still errors; OFFSET, IN subqueries and
  derived tables keep `MustBeInt`.
- *INSERT ... SELECT upsert quirk* (ungated): the refusal now
  only covers an `ON` that could still start the last
  source's join constraint; a last join with ON / USING, and
  GROUP BY / HAVING, terminate the FROM clause and are
  accepted, as on the pin (the ON-terminated form was found
  while probing the USING one).
- *UPDATE ... FROM repeating the target* (ungated): the
  same table under the same alias (or both bare) is
  `SchemaKind::UpdateTargetInFrom`, as on the pin; `FROM t AS
  t`, `FROM t AS t2` and `UPDATE t AS q ... FROM t` still run.
- *Schema qualifiers* (ungated, tiny included): `name()`
  dropped any qualifier, so `temp.s` / `foo.s` (including the
  new `x IN temp.s`) read `main.s` and `CREATE TABLE temp.q`
  wrote a main table; anything but `main.` is now
  `ParseKind::UnsupportedClause`.

**Oracle-probing notes** (stand-in 3.53.1 locally; semantics,
not error text): the pin always runs the RIGHT/FULL join's
left operand as the outer loop (EXPLAIN shows `RIGHT-JOIN b`
after the main loop) and emits unmatched right rows in scan
order — so unordered two-table RIGHT / FULL joins are
compared row-for-row, while generated multi-way corpora are
ORDER BY'd over every column (the planner may reorder inner
joins). The USING / star / coalesce rules were derived from
probes of every INNER/LEFT/RIGHT/FULL pairing in 2- and
3-way chains (`a.* FROM a FULL JOIN b USING(id)` really
yields `coalesce(a.id, b.id)`). LIMIT: `'2'`, `' 2 '`,
`'2.0'`, `2.0`, `-0.0`, `true` convert; `2.5`, `NULL`,
`'0x10'`, `1e19`, blobs do not; `LIMIT t.a` is "no such
column" even inside a correlated subquery. Aliases: `WHERE`
sees them, `ON` too (`ON k=u.a` substitutes `k` and then
reports `a` ambiguous), the result list never; `GROUP BY b`
with alias `b` and column `b` groups by the column. Review
pass: `UPDATE t ... FROM t` and `UPDATE t AS x ... FROM t AS
x` error, `FROM t AS t` runs (the rule compares table and
alias); `INSERT ... SELECT ... JOIN b ON ... ON CONFLICT` and
`... GROUP BY ... ON CONFLICT` parse; `VALUES (1),(2) UNION
SELECT 3 ORDER BY column1` matches the VALUES head's names.

**Gates:** `ci/local-gates.sh` (full run) green — fmt,
clippy `-D warnings` (default, all-features, every
`ci/feature-sets.txt` line incl. the new `v16`), workspace
tests, no_std builds (thumbv7em / thumbv6m / riscv32imac /
wasm32, both profiles), both size gates. New suites:
`tests/query_shape_v16.rs` (27 tests: RIGHT/FULL orders and
chains, outer joins in subqueries / derived tables / INSERT
... SELECT / UPDATE ... FROM, NATURAL/USING rows and star
expansion, affinity, error agreement, ON forward references,
ON-subquery alignment, rowid precedence, NULLS at every site,
term rules, empty IN, a PRNG-generated join corpus, and the
`v16` LIMIT / alias / IN-table tests, plus one regression
test per review finding — nested alias rebasing with a
PRNG-generated alias corpus, aggregate-alias refusal,
per-core compound ORDER BY, duplicate derived names, the
subquery LIMIT rewrite, upsert after terminated FROM
clauses, UPDATE ... FROM target repeats, schema qualifiers;
each of the nine fails on 1248d47) and
`hipp-cli/tests/cli_v16_query_shape.rs` (4 byte-identical
batches, one-shot and stdin, REAL / blob rendering included,
plus the views' `.schema` column lists).

**Size:** tiny **81,616 B** (Δ **−64 B** vs 81,680; ratchet
lowered — the AST change from literal LIMIT integers to
expression handles shortened the legacy constructor paths);
full **257,117 B** (Δ **+652 B** vs 256,465, ratchet raised:
the RIGHT/FULL tail, USING/NATURAL planning, alias scopes and
LIMIT evaluation, mostly paid for by merging the three
duplicated join executors and the duplicated
`plan_query` / `plan_subquery` bodies into `plan_core`).
Review pass, vs 1248d47 (after the column-constraints unit):
tiny **81,564 B** (Δ **+40 B** vs 81,524, ratchet raised —
justified as a correctness fix on the tiny profile's own
CREATE TABLE / INSERT / scan SELECT paths: the parser's
`name()` silently turned `temp.t` into `main.t`; the other
review fixes are compiled out of tiny); full **279,455 B**
(Δ **+3,328 B** vs 276,127, ratchet raised: alias rebasing,
per-core compound ORDER BY keys, the subquery LIMIT rewrite,
view / derived name dedupe and the UPDATE ... FROM check).

**Honest gaps:**

- Output order where the pin's order is a planner accident
  (inner joins it reorders, an index-driven RIGHT-join tail
  scan) is not reproduced; hipp has no index-driven join
  planning, so tests ORDER BY there.
- An aggregate inside a subquery whose arguments name only
  outer columns belongs to the *outer* query on the pin
  (`SELECT (SELECT max(t.a) FROM u) FROM t` is one row; with
  an alias, `WHERE z = (SELECT max(z) ...)` is a misuse
  error); hipp still aggregates it in the subquery. Found
  while probing aliases; a subquery-scoping fix outside this
  unit. The same machinery is why an *aliased aggregate*
  used inside a subquery (`HAVING (SELECT c) > 1`,
  `ORDER BY (SELECT c)`) is refused rather than answered —
  the pin runs those.
- Compound ORDER BY terms are an ordinal or a (qualified)
  column name; the pin also matches a general expression
  term against an identical item (`SELECT a+0 ... UNION ...
  ORDER BY a+0`), which hipp still refuses.
- Out of scope, found by the review (older behaviour, still
  open): `x'01' || x'01'` is BLOB in hipp, TEXT on the pin;
  with min()/max() plus another aggregate the pin still takes
  bare columns from the extremum row, hipp's `pick_rep` only
  for a lone min/max. A three-part `main.t.col` reference is
  refused.
- `-9223372036854775808` is an INTEGER literal on the pin and
  a REAL in hipp (so `LIMIT -9223372036854775808` errors) —
  a literal-parsing gap outside this unit.
- `x IN view` (and NATURAL / USING with a view) stays refused:
  views are inert.
- Parenthesised join groups (`FROM (a JOIN b USING (id))`)
  are still refused; LIMIT on UPDATE / DELETE is a
  compile-time option the pin's CLI build lacks.
- Probed on the local 3.53.1 stand-in; nothing relied on is
  known to differ in 3.54.1, and CI runs the real pin.

### Column constraints and table definitions (2026-11-02)

**Status:** implemented, differential-tested against the
oracle, gated (below). Census (158 probes): MATCH 61 → 66,
silent-wrong DIFF 4 → 1 (CHECK, expression DEFAULTs and
generated columns fixed; column COLLATE belongs to another
unit). A new module `src/rules.rs` (built with `v04`) holds
the rule planner and the row codec; the catalog keeps each
rule as SQL text and every write re-resolves it.

**Shipped — ungated wherever the v0.4 evaluator is built
(fixes on claimed paths):**

- **CHECK constraints** (column-level, table-level,
  `CONSTRAINT name CHECK`) — parsed and ignored before —
  enforced on INSERT (VALUES / SELECT / REPLACE), UPDATE
  (incl. FROM) and the upsert DO UPDATE row, with the pin's
  rules: a NULL result passes; the expression reads
  affinity-converted values and the final rowid (an `INTEGER
  PRIMARY KEY` CHECK sees the auto-assigned rowid); OR IGNORE
  skips the row, every other action errors
  (`SchemaKind::CheckViolation`) — OR REPLACE does not resolve
  a CHECK; the DO UPDATE row errors even under OR IGNORE, the
  proposal is checked before arbitration (OR IGNORE skips it
  wholesale). Order: NOT NULL, generated columns, STRICT, CHECK,
  then uniqueness. An UPDATE (and DO UPDATE) checks NOT NULL
  only on assigned columns and evaluates a CHECK only when it
  reads an assigned column, a generated column computed from
  one, or a changed rowid — the pin's optimisation, observable
  on rows written under `ignore_check_constraints`. CREATE
  TABLE refuses what the pin refuses: subqueries
  (`SubqueryInConstraint`), aggregates (`MisusedAggregate`),
  unknown columns / functions.
- **DEFAULT values:** `DEFAULT (1+2)` used to store 1 (the
  parser kept the first literal); `-'3'`, `-x'01'`, `DEFAULT
  identifier`, CURRENT_TIME / DATE / TIMESTAMP used to read
  NULL. Expression defaults are now re-parsed and evaluated:
  constant ones are folded once at catalog build (so short
  records, CREATE INDEX and every reader see the pin's value);
  ones naming the clock — or failing, e.g. an unknown function
  — are evaluated per statement only when a row needs them (an
  unknown function errors at that INSERT, as on the pin, and
  not at CREATE). A column reference or subquery is refused at
  CREATE (`NotConstantDefault`). The clock is a new
  `Config::clock(fn() -> i64)` hook (Unix ms; read at most once
  per statement, formatted UTC with seconds truncated);
  without one a statement that needs it fails with the new
  `Error::NoClock`. ALTER TABLE ADD COLUMN takes only the pin's
  `sqlite3ValueFromExpr` shape (a literal, possibly signed or
  CAST: `-'3'` adds, `(1+2)` / CURRENT_TIMESTAMP do not), and a
  short record now reads the default *with the column's
  affinity* (`ADD COLUMN c INT DEFAULT '5'` reads 5, hipp
  returned '5' — fixed in every profile).
- **Generated columns** (`GENERATED ALWAYS AS (expr)` and `AS
  (expr)`, VIRTUAL / STORED) — parsed as part of the declared
  type and read as NULL before: computed in dependency order
  with the column's affinity; VIRTUAL columns are absent from
  the record, STORED ones stored in place — the pin's
  `sqlite3TableColumnToStorage` layout, verified both ways
  (hipp-written files read and pass `integrity_check` on the
  oracle, oracle-written files read, update and index
  correctly in hipp). INSERT without a column list supplies the
  non-generated columns; naming a generated column in INSERT /
  UPDATE / DO UPDATE is `GeneratedColumnWrite`; SELECT `*`,
  RETURNING and `excluded.*` include them; indexes and UNIQUE
  constraints on them (VIRTUAL included) are maintained;
  UPDATE recomputes them; NOT NULL / CHECK apply to them.
  Definition errors as on the pin: loops
  (`GeneratedColumnLoop`), DEFAULT / PRIMARY KEY on one, a
  qualified name, a table of only generated columns
  (`InvalidGeneratedColumn`), `rowid` (no such column),
  subqueries, aggregates. ALTER TABLE ADD COLUMN takes VIRTUAL
  ones (not STORED, `CannotAddColumn`), verifying existing rows
  against a generated NOT NULL column and the table's CHECKs
  (the pin's post-ADD `quick_check`).
- **UPDATE ... FROM** applies only the *last* matching FROM
  combination per target row (found while probing CHECK): the
  pin materialises the join keyed by target rowid, so earlier
  combinations are never checked — hipp used to apply each in
  turn (same final value, but NOT NULL / CHECK fired on the
  earlier ones). `FROM s RETURNING ...` no longer aliases `s`
  as RETURNING.
- **Rowid values** convert like the pin's MustBeInt in INSERT,
  UPDATE and DO UPDATE: `'8'`, `9.0`, `' 10 '`, `'1e2'` are
  rowids (they were a datatype mismatch) — fixed in every
  profile.
- **Tiny profile:** the parser refuses (new
  `ParseKind::RequiresEvaluator`) every CHECK, generated column
  and non-literal DEFAULT instead of ignoring it; it still
  takes literal defaults, bare or parenthesised, signed, TRUE /
  FALSE / NULL and `DEFAULT "quoted"`. Replacing the derived
  `PartialEq for Tok` with discriminant / `matches!` tests (it
  was instantiated for `Option<Tok>` comparisons) paid for it.

**Shipped — `v16`:** `INSERT INTO t DEFAULT VALUES` (with
RETURNING; no column list, no ON CONFLICT, as on the pin);
`CREATE TABLE ... AS SELECT` with the pin's column names
(alias, else the column's name — `rowid` or the rowid-alias
column's name for a rowid reference — else the expression
text; duplicates `:1`, `:2`, `:3`), declared types from
`sqlite3ExprAffinity` (a scalar subquery's first column's;
the pin's compound demotion / FLEXNUM) and its
`createTableStmt` text byte for byte (keyword quoting from the
pin's 147-keyword list, the 50-byte one-line threshold); and
STRICT tables (`STRICT` / `STRICT, STRICT` table options;
INT / INTEGER / REAL / TEXT / BLOB / ANY, case-insensitive,
else `StrictColumnType`; ANY has no affinity; a value the
type's affinity cannot convert is `StrictTypeViolation` under
every conflict action, generated columns included; checked
after NOT NULL and before CHECK; ALTER verifies existing rows).
WITHOUT ROWID stays refused.

**Oracle-probing notes** (stand-in 3.53.1 locally; semantics,
not error text): precedence NOT NULL → STRICT → CHECK → UNIQUE
(`('q', NULL)` reports NOT NULL; `(1.5, 'x')` with a failing
CHECK reports the type); `CHECK(a)` passes NULL, `'1abc'`,
`0.5`, fails `'abc'`, `x'00'`, `0`; `CHECK(nofunc(a))` fails
CREATE but `DEFAULT (nofunc())` fails only the INSERT that
needs it; `DEFAULT 1+2` and `DEFAULT -TRUE` are syntax errors,
`DEFAULT hello` is `'hello'`, `-x'01'` is 0, `+'abc'` stays
text, `-0x10` is -16; `b GENERATED AS (a)` declares type
GENERATED; `GENERATED ALWAYS AS (a) VIRTUAL STORED` is a syntax
error; VIRTUAL values are type-checked in STRICT tables;
`INSERT INTO t VALUES (...)` counts only non-generated columns;
`PRAGMA table_info` hides generated columns (hipp has no
PRAGMA); ALTER's `DEFAULT 5 CHECK(l > 10)` fails on a
non-empty table only. The keyword list was verified by
creating a CTAS column named after each candidate word.

**Gates:** `ci/local-gates.sh` (full run) green. New suites:
`tests/table_defs_v16.rs` (27 tests: CHECK truth rules,
affinity, rowid, precedence, upsert / REPLACE / INSERT ...
SELECT, the UPDATE ... FROM last-combination regression,
UPDATE change sets over violating rows, definition errors, an
oracle-written CHECK file; every DEFAULT form, deferred and
definition errors, OR REPLACE substitution, the clock hook —
fixed and pre-epoch times, a 50-point PRNG calendar against the
oracle's `datetime(..., 'unixepoch')`, `NoClock` — and ALTER's
default / CHECK / generated rules; generated columns both
directions incl. indexes on VIRTUAL columns, upsert, REPLACE,
definition errors; rowid conversion; a PRNG constraint corpus
over 6 random schemas × 40 random statements; and the `v16`
DEFAULT VALUES, CTAS (schema text byte-compared through the
oracle) and STRICT tests — the 21 ungated ones also pass in a
`v10`-without-`v16` build), `hipp-cli/tests/cli_v16_table_defs.rs`
(4 byte-identical batches, one-shot and stdin, REAL rendering
included, plus a `.dump` replay of a generated-column table —
the CLI's `.dump` now writes only non-generated columns, as the
pin's shell does, using the new `TableMeta::columns` /
`generated`), and parser unit tests for the tiny refusals (run by
the `--no-default-features` lib test gate).

**Size:** tiny **81,524 B** (Δ **−92 B** vs 81,616; ratchet
lowered — the `Tok` comparison rewrite saves more than the
tiny parser's new refusals and the rowid / default-affinity
fixes cost); full **276,127 B** (Δ **+19,010 B** vs 257,117,
ratchet raised; over `justify_over` (16,384 B), justified
here: the rule planner and row codec (`plan_rules`,
`validate_def`, `resolve`, `decode_rec` / `encode_rec`, ~5 KB),
expression-default evaluation with the calendar (~1.2 KB),
the parser's clause spans and stricter column grammar (~1 KB),
CTAS naming / quoting / affinity (~1.5 KB), the lexer no longer
inlined into `parse` now that `parse_expr_text` also calls it
(net ~+0.4 KB), and the write / read paths' rule plumbing in
the planner and the async step state machines (~8 KB)).

**Honest gaps:**

- `PRAGMA ignore_check_constraints` is not offered: hipp has no
  PRAGMA statements yet (another unit's surface).
- Constraint-level conflict clauses (`a UNIQUE ON CONFLICT
  IGNORE`, `NOT NULL ON CONFLICT REPLACE`) are still parsed and
  ignored: such a write errors where the pin would resolve it
  (an error, never a silent wrong answer).
- hipp's function library has no non-deterministic function,
  so the pin's "non-deterministic functions prohibited in
  generated columns" cannot arise; a unit adding `random()` or
  `'now'` must refuse them there.
- The tiny profile refuses such tables at parse, so it cannot
  open a file holding any table with a CHECK, generated column
  or expression DEFAULT (the WITHOUT ROWID precedent); it also
  refuses the unquoted `DEFAULT identifier` form that `v04`
  builds accept (distinguishing it from the clock keywords did
  not fit the ratchet).
- A double-quoted `"current_timestamp"` inside a parenthesised
  DEFAULT reads as the clock keyword (the AST does not keep
  quotedness); the pin calls it a non-constant column.
- CTAS: a fourth duplicate column name gets a random suffix on
  the pin — hipp refuses it (`Unsupported`); an explicit alias
  spelled `oid` / `_rowid_` on a rowid reference is renamed
  `rowid`; compound affinity demotion reads hipp's resolved
  expression shapes, not the pin's parse tree (they agree on
  the probed literal / column / CAST / function / CASE shapes).
  hipp cannot query `sqlite_schema` itself (another unit), so
  CTAS text is verified through the oracle reading hipp's file.
- A record shorter than its table whose missing column has a
  foldable *expression* default reads the folded value; the
  pin's `OP_Column` would read NULL — reachable only with a
  foreign writer (ALTER refuses such defaults).
- Probed on the local 3.53.1 stand-in; STRICT (3.37) and
  generated columns (3.31) are long-stable, and nothing relied
  on is known to differ in 3.54.1. CI runs the real pin.

### Collations and pattern matching (2026-11-02)

**Status:** implemented, differential-tested against the
oracle, gated (below). Census (158 probes): MATCH 66 → 72,
silent-wrong DIFF 1 → 0 (column COLLATE was the last).

**Shipped — ungated (fixes on claimed v0.4 / index paths):**

- **Column `COLLATE BINARY | NOCASE | RTRIM`** — parsed and
  ignored before (comparisons and ORDER BY ran BINARY):
  `record::Coll` (NOCASE folds ASCII `A`-`Z` only, RTRIM drops
  trailing U+0020), carried on catalog columns, resolved
  column references (`RExpr::Column(slot, aff, coll)`) and
  derived-table columns (their leftmost core's item collation,
  BINARY when none — still a column's). The pin's
  `sqlite3ExprCollSeq` / `sqlite3BinaryCompareCollSeq`
  (`query::expr_coll`, `binary_coll`): an explicit COLLATE on
  the left operand, else on the right (it propagates up through
  operators and function arguments, the pin's `EP_Collate`),
  else the left column's, else the right's, else BINARY; CAST
  and unary `+` pass a column's through, functions and scalar
  subqueries carry none, the rowid / IPK none. Applied to `=`
  `<` IS BETWEEN, CASE, joins' ON / USING / NATURAL, CHECK and
  generated-column expressions; an IN list compares by its left
  operand only (a single constant item follows the binary rule:
  the pin rewrites `x IN (c)` to `x = +c`); `x IN (SELECT ...)`
  by the binary rule against the subquery's item — its
  *rightmost* core's for a compound (carried through
  substitution as an explicit `Collate`); min / max / nullif and
  min / max aggregates use the first argument with a collation;
  ORDER BY terms, GROUP BY (grouping and group order), DISTINCT,
  count / sum / group_concat(DISTINCT) use each term's.
- **Compounds run the pin's merge** (3.5x uses
  `multiSelectOrderBy` for every UNION / INTERSECT / EXCEPT):
  both sides stably sorted by one merge key (the column
  collations, leftmost core with one per column — pushed down to
  every inner operator up to the last non-ALL one, or to all
  of them when an ORDER BY is merged in), equal keys advance the
  left side (a UNION keeps the *right* row of an NOCASE-equal
  pair; INTERSECT the left), each emitted row of a non-ALL
  operator dropped when equal to the previous one under that
  operator's own collations; trailing UNION ALLs append; an
  ORDER BY naming another collation sorts the finished compound
  (the pin evaluates it as a subquery then). A multi-row
  constant VALUES core reads as a BINARY column (the pin's
  `sqlite3MultiValues` co-routine). `combine_compound`'s
  set-based approximation is gone (`run_compound`,
  `merge_compound`).
- **Collated indexes:** `CREATE INDEX ... (c COLLATE NOCASE)`
  (was refused), table `UNIQUE (c COLLATE RTRIM)` and the
  autoindexes of NOCASE / RTRIM columns key by their collation:
  `record::KeyOrder { desc, coll }` replaces the B-tree layer's
  `desc: &[bool]`, so insertion order, prefix probes (UNIQUE
  checks, OR IGNORE / OR REPLACE / upsert victims) and the
  legacy `WHERE col = lit` index path all compare by it (that
  path only takes an index whose collation is the column's).
  Upsert targets match the pin's rule (`ON CONFLICT (a COLLATE
  x)` only an index column of collation x; a bare column any).
  Tables with such indexes are writable — they were refused as
  foreign / opaque. Files interoperate both ways (hipp-written
  indexes pass the oracle's `integrity_check`; oracle-written
  ones are maintained by hipp).
- **Unknown collation names** in CREATE TABLE / UNIQUE / CREATE
  INDEX / upsert targets are refused (`SchemaKind::
  UnknownCollation`, the pin's "no such collation sequence").
- **Table `UNIQUE (a COLLATE x DESC, b)`** dropped `b` (the
  parser skipped COLLATE only after ASC / DESC, the reverse of
  the pin's grammar): uniqueness was enforced on `a` alone.
- **GROUP BY order** (found while probing): with as many ORDER
  BY as GROUP BY terms the pin's group sorter takes the ORDER
  BY terms' ASC / DESC (`orderByGrp` set-up), so ties of the
  ORDER BY keep a descending group order — `GROUP BY a ORDER BY
  b DESC` listed equal-`b` groups ascending.
- **Bare columns of aggregate queries** (found while probing
  min / max ties): they come from the extremum row of the
  *last* min() / max() aggregate (items, ORDER BY, HAVING — the
  pin's shared `OP_CollSeq` hit flag), whatever other
  aggregates exist, and a group whose argument is all NULL
  reads its *last* row; hipp honoured only a lone min / max and
  read the first row (closes the review gap recorded by the
  query-shape unit).
- **Scalar `min()` ties:** `min(1, 1.0)` is 1.0 (the pin's
  `minmaxFunc` takes the later of equal arguments for min, the
  earlier for max).
- **LIKE matcher** (`query::like_glob`, the pin's `likeFunc` +
  `patternCompare`): operands read up to their first NUL with
  `sqlite3Utf8Read`'s decoding (invalid UTF-8 used to read as
  empty), ASCII-only case folding, patterns over 50,000 bytes
  `SchemaKind::PatternTooComplex`; iterative single-backtrack
  matching (the old recursive matcher was exponential).

**Shipped — `v16`:** the `expr COLLATE name` operator (binds
tighter than `||`, looser than unary minus; identifier or
string names, case-insensitive; ORDER BY / GROUP BY ordinals
and aliases keep the collation on top, as the pin's
`resolveAlias` does); GLOB / NOT GLOB (`*`, `?`, classes with
`^`, leading `]`, ranges, a trailing `-` literal) and `LIKE ...
ESCAPE` — both desugared at parse into the pin's `glob(p, x)` /
`like(p, x, e)` calls — and the `like()` / `glob()` functions
(escape: NULL gives NULL, not exactly one character
`SchemaKind::EscapeNotSingleChar`, checked before NULL
operands; an escape equal to `%` / `_` disables that wildcard).

**Oracle-probing notes** (stand-in 3.53.1 locally; semantics,
not error text): `'a' COLLATE BINARY = 'A' COLLATE NOCASE` is
0 (left explicit wins); `x'61' = 'a' COLLATE NOCASE` is 0 (no
cross-class effect); `upper('a' COLLATE NOCASE) = 'a'` and
`coalesce('A' COLLATE NOCASE, 1) = 'a'` are 1 (EP_Collate
through function arguments), `(SELECT 'A' COLLATE NOCASE) =
'a'` 0; `'ABC' IN (nocase_col)` 0 but `'A' IN ('a' COLLATE
NOCASE)` 1 (the one-constant rewrite) and with three items 0;
`'X' IN (SELECT a FROM t UNION SELECT 'q')` 0, reversed cores 1
(rightmost core). EXPLAIN showed every non-ALL compound as
sorter + co-routine merge (`k(1,NOCASE)`), and an ORDER BY
`COLLATE` other than the column's as the natural compound
plus a final sorter (`k(1,B)`); the survivor rules (`a UNION b`
→ `A, ABC, b` over `abc, B, a` / `ABC, b, A`) were derived from
that and checked on 150 generated compounds. Derived columns:
`(SELECT 'X' AS c UNION ALL SELECT a ...)` is BINARY, `(SELECT a
AS c ... UNION ALL SELECT 'X')` NOCASE (leftmost core). Ties:
max() / min() aggregates and DISTINCT / GROUP BY keep the first
row, ORDER BY is stable. Bare columns: `SELECT max(x), min(y),
id` reads min(y)'s row, `min(y), max(x)` max(x)'s, all-NULL
groups the last row. LIKE / GLOB: `'ÄBC' LIKE 'äbc'` 0, `'ab'
GLOB 'a[b-a]'` 1 (the `b` literal), `'a' LIKE 'a' ESCAPE NULL`
NULL, `GLOB ... ESCAPE` a wrong-argument-count error, a NUL
ends either operand, `'x' LIKE 'X' ESCAPE x'41'` 1. CTAS does
not copy a column's COLLATE. `SELECT 1 COLLATE bogus` runs on
the pin (the name is looked up only when used).

**Gates:** `ci/local-gates.sh` (full run) green. New suites:
`tests/collation_v16.rs` (18 tests: column-collation
comparisons, ordering / grouping / DISTINCT / aggregates, ties,
the GROUP BY direction and bare-column regressions, compound
merges, subqueries / derived tables / joins, CHECK / generated /
UPDATE / DELETE over collated columns, UNIQUE NOCASE / RTRIM
(with the `UNIQUE (b COLLATE RTRIM DESC, a)` regression),
CREATE INDEX COLLATE incl. `INDEXED BY` order on the oracle,
upsert targets, an oracle-written collated file written by hipp,
unknown names, the LIKE matcher and pattern limit, and under
`v16` the COLLATE operator corpus, GLOB / ESCAPE / functions,
plus two PRNG corpora — 40×18 random LIKE / ESCAPE / GLOB
pairs and 60 random collated queries; the 15 ungated ones also
pass in a `std,indexes,v10` build),
`hipp-cli/tests/cli_v16_collation.rs` (3 byte-identical
batches one-shot and over stdin, REAL / NULL / blob rendering
included, plus a `.dump` replay of a collated, indexed table),
and parser unit tests for the clause grammar.

**Size:** tiny **81,552 B** (Δ **−12 B** vs 81,564; ratchet
lowered — the collation fields and name lookups are compiled
out of the tiny profile, which has no comparison a collation
could change, and the parser's constraint-column rewrite came
out smaller); full **284,295 B** (Δ **+4,840 B** vs 279,455,
ratchet raised: collation selection and the collated
comparators (~1.5 KB), the merge-based compound evaluator
(~1 KB over the removed set version), the pattern matcher and
like / glob functions (~1.2 KB), index `KeyOrder` plumbing and
upsert-target matching, the bare-column and GROUP BY order
fixes).

**Honest gaps:**

- The tiny profile still accepts (and ignores) `COLLATE
  <unknown>` in CREATE TABLE: it has no comparisons a collation
  could change, and validating the name did not fit its
  ratchet.
- An unknown name in the `COLLATE` operator is refused at
  prepare even where the pin, which looks names up only when a
  comparison or sort uses them, runs the statement (`SELECT 1
  COLLATE bogus`). A file whose schema names a collation other
  than the three built-ins fails to open (`UnknownCollation`),
  as files with WITHOUT ROWID tables do.
- `PRAGMA case_sensitive_like` is not offered (hipp has no
  PRAGMA statements yet); LIKE is always case-insensitive for
  ASCII, the pin's default.
- A multi-row VALUES core whose rows are not constant (or whose
  first row has an affinity) is the pin's UNION ALL fallback;
  hipp takes the first row with a collation then, but does not
  re-run the merge per row-core.
- Out of scope, found while probing (older, still open): the
  relational operators bind tighter than `=` / LIKE on the pin
  (`0 = 1 < 2` is 0), hipp parses them at one level, left to
  right.
- Probed on the local 3.53.1 stand-in. The merge-based compound
  evaluation and the VALUES co-routine are recent pin internals
  (3.4x–3.5x); nothing relied on is known to differ in 3.54.1,
  and CI runs the real pin.

## v0.15 — insert-side split distribution (2026-11-01)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). INSERT — and the insert
side of every other writer — now balances the table B-tree
the way the pin does when a page overflows, instead of
splitting the overflowing page into byte-even halves in
isolation. hipp-built files therefore track the pin's
`page_count` / `freelist_count` statement-for-statement on
the pinned table workloads — the direction v0.14 could not
pin (100 uniform rows at page size 512 built 8 pages in
hipp vs 6 in the pin; both now build 6, identically at
every step). This is deliberately **not** feature-gated:
it changes no dialect surface, only the file layout
produced by existing insert paths (a pure
layout-conformance fix, on the same basis as v0.14).

### Shipped

- **Table insert balance** (`btree.rs`): the insert
  descent records its path and whether it stayed on the
  tree's rightmost spine. On overflow:
  - appending to the rightmost leaf takes the pin's
    **quick split**: the old leaf is left completely
    untouched and the new cell goes alone onto a fresh
    page (at the root, the old leaf's content is first
    copied to a new left child, allocated before the
    right page);
  - a root leaf balances over two pages by the fitted
    distribution (below), the root becoming interior;
  - any other overflowing leaf balances over a window
    of up to three consecutive siblings under its
    parent (the same centred window the delete side
    uses), redistributed over the smallest page count
    that holds the window's cells at full capacity —
    a balance can free a page as well as allocate one.
    Interior pages balance the same way on cascade,
    with divider keys modelled exactly (a divider's
    cost includes its child pointer); a root interior
    that overflows splits into two fresh pages.
- **Distribution rule** (fitted clean-room from oracle
  behaviour over ~4,700 reconstructed balance events):
  page count first (greedy in-order packing at full
  capacity, matched the pin on 1,826/1,826 table
  events); for a balance that allocates one page more
  than the window, the new (last) page receives a
  suffix of the combined sequence costing closest to
  half the content capacity and the remainder is
  distributed over the old pages by the v0.14 quota
  distribution; otherwise the quota distribution runs
  over the whole window.
- **Index insert balance**: the same window machinery
  over the index entry sequence, plus one accounting
  correction — a balance's minimum page count now
  follows a divider-aware packing (an item that lifts
  as a divider consumes no leaf space), where the
  delete-side counter charged every item to a page.
  This moved the ascending-index trace's first
  divergence from step 68 to step 133 (see gaps).

### Gates

- `cargo test --workspace` (stable): **203 passed /
  0 failed / 5 ignored** (v0.14: 193 passed; the +10 is
  this milestone's new live tests; the 5 ignored are
  the not-yet-exact differentials under Honest gaps,
  kept runnable with `--ignored`). New suite
  `tests/insert_balance_v15.rs`: twin files built from
  empty by both engines, `(page_count, freelist_count)`
  asserted after EVERY statement, final contents equal,
  oracle `integrity_check` clean on the hipp file.
  Live and green: the original 100-row repro at ps512,
  appends at ps1024/ps4096, descending and
  pseudo-random insert orders, varied cell sizes,
  ~900-byte overflow rows, INSERT INTO ... SELECT bulk
  loads, and deep trees (2,000 appends; 1,200 random)
  that split interior pages and the root. A1–A11 stay
  green; all v0.14 delete traces stay green.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean
  tree afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v10 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features.

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 78,716 +
  .rodata 2,692 = 81,408 B of 128 KiB budget**, Δ
  **+8,676 B** vs v0.14's 72,732 B (A/B-verified on
  this machine against the v0.14 tree: 70,040 + 2,692;
  the figures here are from the committed tree).
  The growth is over the 4 KiB justification line and
  is justified: unlike v0.14's delete machinery, the
  insert balance sits on the INSERT path the tiny
  profile claims, so the tiny binary pays for the
  window redistribution, the quick split, the interior
  cascade and the root-split handling that make
  hipp-built files match the pin's shape. Engine
  statics 0 B (.bss 98,308 B = demo heap, unchanged).
  Headroom remaining: 49,664 B.

### Honest gaps (v0.15)

- **Index insert distribution is not yet pin-exact.**
  Three differentials (`v15_index_ascending_keys`,
  `v15_index_scrambled_keys`,
  `v15_create_index_on_populated`, plus the indexed
  churn trace) are `#[ignore]`d with their first
  divergence steps recorded in the test file (steps
  133 / 121 / 202 / 133): a grow balance at row 68
  distributes (27,25,16) in the pin where hipp's
  fitted rule gives (26,25,17) — page counts
  reconverge a row later, but the per-step trace is
  the assertion, so it honestly fails. Extensive
  fitting (575 exactly-reconstructed index events;
  quota families static and dynamic, check-then-add
  and closest boundaries, suffix targets) tops out
  near 74% event-exact, and the pin's two boundaries
  in several events are jointly inconsistent with
  every quota family tried — the rule has a component
  the fitting has not isolated. Contents and
  integrity are unaffected; only intermediate page
  counts on index-heavy builds can differ by a page
  for a few statements.
- **Delete distribution on mixed-weight windows.**
  The build-and-churn trace (inserts pin-exact, trees
  census-identical after the insert phase) diverges
  in the delete phase at step 247: on windows whose
  cells straddle the rowid varint boundary, the pin
  distributes e.g. (27,24,18) where the v0.14 fitted
  rule gives (26,24,19), delaying a page free by two
  steps. `v15_build_and_churn` is `#[ignore]`d with
  the step recorded. This is the same open rule as
  the index gap, on the delete side; v0.14's pinned
  delete workloads (uniform weights) are unaffected.
- The interior-window phantom (content capacity minus
  the 12-byte interior header), inherited from
  v0.14, is still validated only indirectly through
  page-count traces; hipp repacks pages it writes, so
  total free space agrees with the pin while byte
  layout inside a page can differ (the pin may leave
  freeblocks). WAL read stays target-gated (RISKS
  R3); auto-vacuum / ptrmap write support has no
  recorded demand. Neither was touched.

## v0.14 — delete merge/redistribution (2026-10-31)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). DELETE — and the delete
side of UPDATE / REPLACE / upsert victims — now balances
the B-tree the way the pin does: an underflow page is
redistributed with its siblings or merged, instead of
being left in place. Delete-heavy files therefore reclaim
the same pages into the freelist the pin reclaims, and
`page_count` / `freelist_count` track the pin
statement-for-statement on the pinned workloads. This is
deliberately **not** feature-gated: it changes no dialect
surface, only the file layout produced by existing delete
paths (a pure layout-conformance fix; the recorded
candidate list named at v0.6/v0.11/v0.12/v0.13 is now
empty).

### Shipped

- **Table B-tree balance** (`btree.rs`): `btree_delete`
  records its descent path; a delete that leaves a
  non-root page with total free space greater than two
  thirds of the usable size balances a window of up to
  three consecutive siblings under the parent (centred on
  the trigger, clamped at the edges). The window's cells
  are redistributed over the smallest page count that
  holds them at full capacity; excess pages return to
  the freelist. Balances cascade up the recorded path,
  and an interior root left with a single child
  collapses (the child's content is copied into the
  root page, the child freed, the height shrinks).
- **Index B-tree balance**: the same window machinery
  over the index entry sequence — a window's combined
  sequence interleaves the window pages' entries with
  the parent dividers between them, and re-splitting
  lifts one entry per boundary back into the parent as
  the new divider. Interior-resident deletes use
  predecessor replacement: the predecessor is removed
  from its leaf first, then the target cell is re-sought
  (a balance may have moved it) and replaced.
- **Distribution rule** (`distribute`, shared by both
  trees): cumulative quotas fitted on a machine-collected
  dataset of 156 oracle balance events — see the probing
  notes.
- **Degenerate fallback preserved**: a delete that would
  empty a whole non-root subtree still falls back to
  rebuilding the tree from its surviving rows (v0.5
  design); overflow chains of removed rows return to
  the freelist on every path.

### Oracle-probing notes (verified on the 3.54.1 pin
before implementation; clean-room — file-format docs +
oracle I/O only)

- **Trigger**: a delete balances a non-root page when
  its post-delete total free space exceeds two thirds
  of usable (`free > usable * 2 / 3`, integer floor).
  A page already underfull from inserts balances on its
  first delete; inserts themselves never balance.
- **Window**: up to three consecutive siblings under one
  parent, containing the trigger — first child
  `[0..3]`, middle `[i-1..i+2]`, last `[n-3..n]`.
- **Merge decision is capacity-driven**: the window's
  combined cells go on the smallest number of pages
  that holds them at full capacity (greedy in-order
  packing by page need); excess pages are freed.
- **Distribution**: with S the window's total cost
  (cell bytes + 2 per cell), P one page's content
  capacity, and n the result page count, page i's quota
  boundary is `Q_i = i * (S + P) / (n + 1)`. A two-page
  merge splits evenly (17/17 fitted events exact); for
  three-page results the first boundary is
  check-then-add at Q_1 and the second sits at the item
  closest to Q_2 (115/139 exact, the rest within one
  cell). Earlier hypotheses (pure quotas, sequential
  remainder quotas, pairwise sweeps) scored strictly
  worse and were rejected.
- **Root collapse**: an interior root left with a
  single child is replaced by that child's content.

### Root finds

- **`index_parent_after` dropped the parent's rightmost
  child** when splicing a balanced window back into an
  index parent (the surviving-children copy loop ran
  over cell indices, not child indices). Found by the
  pre-existing `deep_tree_index_delete_and_update`
  acceptance test going corrupt after the index balance
  landed (double-referenced page, orphaned subtree);
  fixed and re-verified — the full deep-index trace now
  matches the pin step-for-step.
- **hipp's insert-side shape already diverges from the
  pin** (pre-existing, not v0.14 scope): 100 rows at
  page size 512 build 8 pages in hipp vs 6 in the pin —
  the insert split distribution differs. Delete shape
  is therefore pinned in the direction that isolates
  it (hipp deleting on pin-built trees, per-statement);
  the hipp-built direction is asserted on contents and
  integrity. This divergence is the named v0.15 scope.

### Gates (run on the final tree)

- `cargo test --workspace`: **193 passed, 0 failed**
  (178 at v0.13 + 12 new core tests in
  `tests/delete_balance_v14.rs` + 3 new CLI tests in
  `hipp-cli/tests/cli_v14.rs`), re-run green on the
  COMMITTED tree (clean status; see the loop status for
  the recorded run). The core suite pins, per statement,
  identical (page_count, freelist_count) traces against
  the oracle on tail / middle / scattered deletes,
  delete-all collapse with freelist-reuse reinsert,
  batch deletes, overflow-row deletes, page size 4096,
  a depth-3 table tree (interior cascade + height
  shrink), depth-2 and depth-3 index deletes, plus
  contents-and-integrity assertions for UPDATE / REPLACE
  / upsert victim churn and the hipp-built direction.
  A1–A11 stay green.
- CLI: delete-heavy batches through the hipp CLI on
  oracle-built files leave contents, page_count and
  freelist_count identical to the oracle-run twins, and
  pass the oracle's `integrity_check`.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean
  tree afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v10 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features. (The tiny thumb build shows the
  same 3 pre-existing feature-combination warnings as
  the base; none are new.)

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 70,040 +
  .rodata 2,692 = 72,732 B of 128 KiB budget**, Δ
  **+28 B** vs v0.13's 72,704 B — the balance machinery
  lives on delete paths the tiny profile (INSERT /
  SELECT only) does not reach, so the tiny binary is
  nearly untouched; far below the 4 KiB
  justification threshold. Engine statics 0 B (.bss
  98,308 B = demo heap, unchanged). Headroom
  remaining: 58,340 B.

### Honest gaps (v0.14)

- The within-window distribution matches the pin
  exactly in ~83% of probed balance events and is
  within one cell in the rest; no pinned workload's
  page_count / freelist trace diverged because of it,
  but a workload crafted around a boundary case could
  show a one-page-count difference after a single
  balance. The interior-window phantom (content
  capacity minus the 12-byte interior header) is a
  principled guess validated only indirectly through
  page-count traces, and windows over a two-child
  parent were not separately probed.
- hipp always repacks a page it writes; the pin can
  leave freeblocks in place on pages it did not need
  to repack. Total free space agrees; byte-level page
  layout can differ.
- The insert-side split distribution divergence
  (above) is untouched — hipp-built files can differ
  from pin-built files in page_count after inserts.
  Named as the v0.15 scope.
- WAL read stays gated on a real target demanding
  it (RISKS R3).

## v0.13 — uncorrelated-subquery hoisting (2026-10-30)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). An uncorrelated scalar /
IN / EXISTS subquery — including inside aggregate
arguments — is now executed **once per statement
execution** and its result cached, instead of
re-executing once per outer row (per group row for
aggregate arguments). This is deliberately **not**
feature-gated: it adds no dialect surface, only an
execution strategy for the existing v0.6/v0.11 paths
(the tiny profile has no subqueries either way), and —
per the probing notes — the per-row strategy was not
merely slow on the write side, it was *wrong*.

### Shipped

- **Plan-time correlation verdict** (`query.rs`): every
  `SubPlan` is stamped by the session planner with a
  unique node `id` and an `uncorrelated` flag. Resolved
  expressions index the combined (enclosing ++ own)
  evaluation row in absolute slots, so correlation is
  decidable exactly: `plan_is_uncorrelated` walks the
  plan — items, filter, grouping, having, ordering, ON
  expressions, derived sources, VALUES rows, compound
  tails, and nested subquery plans recursively, all
  against the same absolute threshold — and reports
  whether anything references a slot or rowid source
  belonging to an enclosing scope.
- **Statement-scoped result cache** (`session.rs`):
  `compute_sub_values` / `compute_sub_values_outside_agg`
  route through `sub_value_for`, which serves an
  uncorrelated plan from the statement's hoist cache
  after its first computation (keyed by node id, which
  survives the executor's per-execution plan clones, so
  nested uncorrelated nodes hoist too). The cache is
  cleared when a fresh statement execution begins.
  Correlated plans take the exact pre-v0.13 per-row path.
- **Execution counter**: `Connection::subquery_executions()`
  counts `exec_subplan` calls on the connection, so the
  win is asserted in tests without timing: an
  uncorrelated node over 200 outer rows contributes
  exactly 1 execution; a correlated EXISTS control
  contributes exactly 200.

### Oracle-probing notes (verified on the 3.54.1 pin
before implementation)

- **The pin fixes an uncorrelated subquery's value for
  the whole statement**, including when the statement
  writes the subquery's own table:
  `UPDATE t SET a = (SELECT sum(a) FROM t)` over
  (1,2,3,4) yields 10 on every row;
  `DELETE ... WHERE a = (SELECT min(a) FROM t)` deletes
  only the one minimum row; a multi-row
  `INSERT ... VALUES ((SELECT max(a) FROM t) + 1), ...`
  inserts the same value for every row; and
  `INSERT INTO u SELECT (SELECT max(x) FROM u) + 1 FROM t`
  inserts the statement-start maximum for every source
  row.
- **The v0.12 engine diverged on the UPDATE case**
  (probed on the pre-v0.13 tree): it produced
  6, 11, 20 over (1,2,3) — each row's SET subquery
  re-evaluated against the rows already written. The
  DELETE / INSERT cases already matched, because those
  paths collect their row sets (or value rows) before
  mutating. Hoisting — first use computes the
  pre-write snapshot, every later row reuses it —
  closes the UPDATE divergence as a side effect of the
  optimisation. No other silent divergences were found:
  the full read/write corpora below are identical to
  the pin.

### Gates (run on the final tree)

- `cargo test --workspace`: **178 passed, 0 failed**
  (171 at v0.12 + 5 new core tests in
  `tests/hoist_v13.rs` + 2 new CLI tests in
  `hipp-cli/tests/cli_v13.rs`), re-run green on the
  COMMITTED tree (clean status; see the loop status
  for the recorded run). The core suite covers
  read-side identity (scalar / IN / EXISTS in every
  position, empty and multi-row scalar results,
  deterministic expressions inside the subquery,
  compound and derived-table cores, correlated
  controls), aggregate-argument hoisting, the
  write-snapshot matrix above applied to twin files,
  and the execution-count assertions. A1–A11 stay
  green.
- CLI: read and write-snapshot corpora byte-identical
  to the oracle, one-shot and over stdin; hipp-written
  files pass the oracle's `integrity_check`.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean
  tree afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v10 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features. (The tiny thumb build shows the
  same 3 pre-existing feature-combination warnings as
  the base; none are new.)

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 70,012 +
  .rodata 2,692 = 72,704 B of 128 KiB budget**, Δ
  **−28 B** vs v0.12's 72,732 B — a shrink (dead-code
  noise), no justification needed. The hoisting
  machinery lives entirely in the `v06`-gated paths
  the tiny profile excludes. Engine statics 0 B (.bss
  98,308 B = demo heap, unchanged). Headroom
  remaining: 58,368 B.

### Honest gaps (v0.13)

- The only remaining recorded candidate is
  performance: no merge / redistribution on delete.
  WAL read stays gated on a real target demanding it.
- Hoisting is all-or-nothing per plan node against
  *every* enclosing scope: a subquery bound only to an
  outer scope but nested inside a correlated plan is
  re-executed per execution of that plan (its value
  genuinely varies there), and FROM-clause derived
  tables inside a subquery still materialise per outer
  row even when uncorrelated — expression subqueries
  were this milestone's scope.
- There are no volatile scalar functions in the
  claimed dialect (no `random()`, no clock functions),
  so a cached result can never go stale within a
  statement for nondeterminism reasons.

## v0.12 — real-literal parsing exactness (2026-10-29)

**Status:** implemented, differential-tested against the
pinned oracle, and gated (below). This milestone closes
RISKS R5, the last recorded silent-wrong-answer divergence:
decimal text that becomes a REAL — SQL literals, `CAST`
conversions, affinity conversions, rowid-literal coercion —
now converts bit-for-bit the way the pinned SQLite 3.54.1
oracle converts it. Like v0.8 / v0.10 it is deliberately
**not** feature-gated: it corrects claimed paths, and the
tiny profile's literal path is one of them (tiny ends up
17 KiB *smaller*; see the ratchet).

### Shipped

- **`parse_real` rewritten** (`sql::lexer`) as a two-step
  reproduction of the oracle's conversion (model probed
  black-box first; see the probing notes):
  (1) significant digits accumulate into a `u64` — the
  first digit whose multiply-and-add would overflow `u64`
  ends the input, and it and every later digit are dropped
  at one decimal-exponent unit each (no rounding, no
  sticky); (2) the truncated decimal is converted
  *exactly* by big-integer rational arithmetic: binary
  exponent from bit lengths, one big division for the
  mantissa, remainder-driven round-to-nearest with true
  ties to the even neighbour, subnormals through the fixed
  `2^-1074` scale, overflow to infinity at/above the
  overflow midpoint, underflow to zero.
- **One conversion engine for every decimal→REAL path.**
  The new `parse_real_signed` (leading `+`/`-`) serves the
  CAST/arithmetic prefix parser
  (`query::parse_num_prefix`), the affinity text conversion
  (`record::text_to_number`), and the rowid-literal
  coercion (`session::literal_rowid`), replacing
  `str::parse::<f64>` at all three sites. Same input text
  now yields the same bits in a literal, a `CAST`, an
  affinity conversion, and a `WHERE rowid = '...'` probe.
- **`text_to_number` integer-overflow fix:** text that is a
  well-formed integer literal past `i64` used to convert to
  nothing (the value stayed TEXT under NUMERIC / REAL
  affinity); it now converts to a REAL, as the oracle's
  does (e.g. `'18446744073709551616'` stored into a REAL
  column is the real 2^64).

### Oracle-probing notes (all verified on the 3.54.1 pin
before and during implementation)

- **The old parser was far worse than the recorded
  "1 ulp on pathological mantissas":** on an 869-literal
  adversarial corpus (long mantissas, halfway cases,
  subnormals, exponent extremes) its bits diverged from
  the correctly rounded value on **386** literals —
  `1e308` off by ~41 ulp (the repeated `1e16` scaling
  chunks), `0.9999999999999999` parsed to exactly 1.0,
  `1.7976931348623157e308` one ulp low.
- **The pin is not textbook IEEE rounding on long
  inputs.** A first implementation doing exact IEEE
  conversion of the full literal matched the pin on
  495/500 corpus literals; the 5 holdouts were all
  midpoint-shadow constructions, and targeted probing
  isolated the pin's actual rule (the `u64` digit
  horizon in "Shipped"): sig digit 20 of the 1.0
  midpoint flips the result while sig digit 21 is
  ignored (raising it cannot push a value over a tie —
  there is no sticky bit); the 20-digit integer
  `18446744073709553665` converts as `...53660` because
  its 20th digit's accumulation overflows `u64`
  (`...53664` behaves the same; `...560000`, whose
  dropped digit is 0, converts exactly); short exact
  ties go to the even neighbour (`72057594037927960`
  → 2^56+32), while long-digit exact ties land low
  only because their truncated form is strictly below
  the tie (`0.9999999999999999444888487687421729788184
  16595458984375` → the float below 1.0). The overflow
  midpoint in full scientific digits likewise converts
  to the largest finite float under the horizon, not
  to infinity.
- **Stress (run, not committed):** 6,200 literals
  concentrated where the model could break — 17–26
  digit mantissas at all exponents, integers straddling
  the `u64` accumulation boundary (±2000 around
  `u64::MAX` in steps), 19–24 fraction digits near
  1.0, subnormal-range strings — zero divergences.

### Root finds this milestone

- **The horizon model was forced by the data, not
  chosen.** Textbook exactness was implemented first
  and the pin itself refuted it on five literals; the
  milestone's real scope turned out to be "match the
  pin's conversion, including its digit horizon," which
  is what a pinned-version claim means.
- **`core`'s f64 text parser was riding in the tiny
  build** through the ungated `text_to_number`.
  Routing every conversion through the new converter
  removed its tables and code from the tiny link —
  that removal, not the converter, is the v0.12 size
  story (below).
- Two test-authoring corrections during bring-up,
  recorded for honesty: the stored-comparison
  expectations for `2.5e-324` / `4.9e-324` (both parse
  to the minimum subnormal, so both match both rows),
  and the oracle's `printf('%!.25e')` rendering of a
  stored +0.0, which is `0.0e+00`.

### Gates (run on the final tree)

- `cargo test --workspace`: **171 passed, 0 failed**
  (162 at v0.11 + 7 new core differentials in
  `tests/real_literal_v12.rs` + 2 new CLI differentials
  in `hipp-cli/tests/cli_v12.rs`), re-run green on the
  COMMITTED tree (clean status; see the loop status for
  the recorded run). The core suite extracts the
  oracle's bits losslessly (the oracle writes each
  literal into a REAL column of a file; hipp reads the
  file) over a ~500-literal corpus of hand-picked edges
  plus an LCG-generated spread, checks hipp's parse of
  every literal against them, checks the reverse file
  direction via the oracle's `printf('%!.25e')`
  extraction from both files, pins 29 halfway / boundary
  cases with the pin's own bit patterns (generated from
  the exact midpoint expansions under the probed
  horizon rule and cross-checked against the oracle),
  and covers CAST / affinity conversions, stored-value
  comparisons whose row sets flip on 1 ulp, integer
  literals past `i64` / `u64`, and the exponent
  extremes. A1–A11 stay green.
- CLI: the v0.12 corpus exposes the parse through
  comparison operators (0/1 outputs that flip on
  1 ulp) and CAST / stored-value comparisons —
  byte-identical to the oracle one-shot and over
  stdin; the stored-literals test compares the
  oracle's exact extraction from a hipp-written file
  against its own file.
- `cargo clippy --workspace --all-targets -- -D
  warnings`: clean. `cargo fmt` (write mode): clean
  tree afterwards. Feature matrix compiles: default,
  no-default, std-only, v04..v10 ladders, indexes-only,
  indexes+v05, all-features; lib tests pass under
  no-default-features. (The tiny thumb build shows the
  same 3 pre-existing feature-combination warnings as
  the base; none are new.)

### Size (ratchet)

- thumbv7em-none-eabihf `tiny`: **.text 70,040 +
  .rodata 2,692 = 72,732 B of 128 KiB budget**, Δ
  **−17,140 B** vs v0.11's 89,872 B (base re-measured
  with the identical command at 89,872 B before the
  delta was taken). The milestone is an ungated
  correctness fix, so tiny compiles the new converter —
  but routing the affinity path away from
  `str::parse::<f64>` drops `core`'s float-parse tables
  (most of the old 14,952 B `.rodata`) and code from
  the tiny link, and the exact converter is smaller
  than what it replaced. Engine statics 0 B (.bss
  98,308 B = demo heap, unchanged). Headroom
  remaining: 58,340 B.

### Honest gaps (v0.12)

- **R5 is closed.** What remains on the recorded list
  is performance only: no uncorrelated-subquery
  hoisting (an uncorrelated subquery still re-executes
  per outer row — now also inside aggregate arguments,
  v0.11) and no merge / redistribution on delete.
  WAL read stays gated on a real target demanding it.
- The digit-horizon model is probed behaviourally
  (clean-room: oracle I/O, never SQLite source) and
  verified on the committed corpus plus the
  6,200-literal stress above; a decimal shape outside
  every probed family could in principle still
  diverge, as with any black-box model.
- Conversion cost is quadratic in mantissa digit
  count for pathological inputs (the exact rational
  is built over all kept digits); ordinary literals
  are unaffected, and the kept prefix is bounded by
  the `u64` horizon plus dropped-digit counting.

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
