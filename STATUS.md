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

### CI vs the real pin (2026-11-02)

The local oracle was a stand-in (a CLI list-mode shim over
Python's bundled SQLite **3.53.1**); CI's first run against the
real **3.54.1** pin failed wave 1's new tests on five
behaviours. What CI showed, what changed, and what is inferred:

- **REAL → text** (ungated; tiny path). CI showed the pin's CLI
  list mode prints a REAL exactly as its core REAL → TEXT
  conversion (CAST AS TEXT) does, and 3.53's conversion gives
  the same strings. hipp-cli's `fmt_real` and the test helper
  in `tests/params_v16.rs` had their own `%!.15g`; both now call
  `record::render_real`, the one source of truth. That function
  was correctly rounded and so differed from the pin on about 9%
  of random doubles (`574.423710258671` is `574.42371025867101`
  there). It is now fitted **clean-room from oracle output
  only** (no SQLite source): `lexer::pin_digits` reproduces the
  pin's 18 or 19 internal digits (shown by `printf('%!.26g')`):
  scale by `10^(17 - floor(log10 2^e))` with that power of ten
  truncated to a 64-bit mantissa, round half up. Matched on
  120 000 random doubles before any rendering was fitted. The
  short-or-17 choice was fitted next: 13 truncated digits when
  they read back and digits 14..16 are `000` or the scale was
  downward; else, when digits 15..16 are `99`, the 14-digit
  rounding when it reads back; else 17 digits, half up from the
  18/19 (a double rounding, as on the pin:
  `-589569.98659691867`). Corpus: 4.2 M doubles through the
  Python model and **2 M through hipp's SQL path, 0
  mismatches** against the stand-in core (random bit patterns,
  subnormals, 1–17-digit decimals and their neighbours, integers
  near 2^53 / 2^63 / 10^k, powers of ten, halfway cases).
  `tests/real_text_v16.rs` keeps 4 000 of them (scalable with
  `HIPP_REAL_CORPUS`) plus the CI goldens. Inferred: that the
  3.54.1 core equals the 3.53.1 core here (CI's strings all
  agreed; the corpus runs live against the pin in CI). `parse_real`
  and the read-back test now share `lexer::int_pow10_to_f64`,
  which is why tiny got smaller (−48 B).
- **IN-list collation** (ungated). `'A' IN ('a' COLLATE NOCASE)`
  is 0 on the pin, 1 on 3.53 (which rewrote one constant item
  to `x = +item`). An IN list now uses only its left operand's
  collation, explicit or column. Inferred: a single item's
  column collation is ignored too.
- **UNION with LIMIT 1** (ungated, v0.9 compound path). CI:
  `SELECT a FROM t LIMIT (SELECT 2 UNION SELECT 1)` keeps 2 rows
  on the pin (the scalar subquery is 2, not the sorted 1). The
  3.54.0 release notes say "UNION queries with a LIMIT of exactly
  1 are now handled faster". hipp now takes the first row in
  left-to-right core order when every operator is UNION, there
  is no ORDER BY and no OFFSET, and the LIMIT is the integer
  literal 1. A scalar or EXISTS subquery with no LIMIT of its
  own now gets that literal (`query::first_row_limit`); EXISTS
  values do not change. Pinned by CI: the scalar-subquery case.
  Inferred, and checked live on CI by `pin_inferred`: the
  written `LIMIT 1`; that mixed UNION / UNION ALL / EXCEPT
  chains, `LIMIT +1`, bound limits and a subquery's own `LIMIT X`
  (the pin's `X <> 0` rewrite) keep the sorted answer.
- **`q.*` over RIGHT / FULL USING** (ungated). CI:
  `SELECT a.* FROM a FULL JOIN b USING (id)` shows a NULL `id`
  on the b-only rows, so `a.*` reads a's own columns. 3.53 read
  the coalesced value. Qualified `q.*` now always expands to q's
  own columns. Unqualified `*` and an unqualified `id` coalesce
  as before (CI agreed on those). Inferred, and checked live on
  CI: the RIGHT JOIN and NATURAL RIGHT / FULL forms.
- **`.schema` view comment** (hipp-cli). The pin prints
  `/* v(a,"+b",b,Q) */`. A column name now prints bare only if
  it is a plain identifier (ASCII letter or `_`, then letters,
  digits or `_`) and not an SQL keyword. Otherwise it is
  double-quoted, with `"` doubled. Inferred: keyword quoting,
  using SQLite's documented list of 147 keywords.

Tests for the three engine behaviours that changed assert the
pin's answer. Against the live oracle when it is the pin;
otherwise against the CI-recorded goldens (`pin`), or, for the
inferred neighbours, against rows derived from the evidenced
rule (`pin_inferred`, labelled so). **Size:** tiny **75,960 B**
(Δ **−48 B**, ratchet lowered); full **291,531 B** (Δ **+264 B**,
ratchet raised to it: the UNION first-row rule, the
`first_row_limit` literal, the reworked digits).
Left open: round() still reads the exact decimal expansion
rather than `pin_digits` (see the bound-parameters gaps).

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
  after RIGHT, `coalesce(...)` after FULL); unqualified `*`
  over a source left of a RIGHT/FULL join reads the joined value
  (`a.*` reads a's own columns on the 3.54.1 pin — see "CI vs
  the real pin"); qualified `t.col` still reads its own side.
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
  open): ~~`x'01' || x'01'` is BLOB in hipp, TEXT on the pin~~
  (fixed by the column-constraints review pass, below);
  with min()/max() plus another aggregate the pin still takes
  bare columns from the extremum row, hipp's `pick_rep` only
  for a lone min/max. A three-part `main.t.col` reference is
  refused.
- ~~`-9223372036854775808` is an INTEGER literal on the pin and
  a REAL in hipp~~ — fixed by the column-constraints review pass
  (below).
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
  earlier ones). "Last" is in hipp's join order (FROM order,
  rowid scans), which is the pin's for scans; where the pin
  builds an automatic index for an equality join it is that
  index's order instead (a choice SQLite documents as
  arbitrary — see the gaps). `FROM s RETURNING ...` no longer aliases `s`
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

**Review fixes (second pass, same day):**

- *Malformed constraint lists* (corruption, every profile): the
  column parser scanned an item for keywords, so trailing junk —
  `CHECK(a>0) bar`, `DEFAULT abs(1)` (stored 'abs'), `DEFAULT
  "x" y`, `DEFAULT - - 1`, `DEFAULT (1)(2)`, `PRIMARY KEY (a)
  foo`, `VARCHAR(10) foo`, a column after a table constraint,
  `INDEX (a)` — was accepted and stored verbatim, a schema the
  pin then refuses to load. Every CREATE TABLE item (and ALTER's
  column) is now walked clause by clause with the pin's grammar
  (`typetoken`, `carglist`, `conslist` with optional commas
  between table constraints, ON CONFLICT, REFERENCES / MATCH /
  ON DELETE …, [NOT] DEFERRABLE, AUTOINCREMENT inside a table
  PRIMARY KEY), with the pin's 68 non-fallback keywords refused
  as names / type words / DEFAULT identifiers. The same walker
  serves the tiny profile, which therefore gained it too, and
  now takes `DEFAULT identifier` (only the clock keywords are
  refused there). Found on the way: a column named `key` was
  silently dropped (items starting with KEY were skipped);
  `CHECK(a) CHECK(b)` without a comma kept only the first.
- *Repeated PRIMARY KEY / UNIQUE sets* (corruption, `indexes`):
  `a PRIMARY KEY UNIQUE`, `UNIQUE(a), UNIQUE(a DESC)` got two
  autoindexes — "orphan index" on the pin. A set repeating an
  earlier one (same columns in order, same effective collations)
  is dropped as the pin's `sqlite3CreateIndex` does; a repeating
  PRIMARY KEY makes the earlier UNIQUE set the key's.
- *Stored CREATE text* (corruption, every profile): `CREATE
  TABLE main.t` was stored verbatim ("corrupt database" on the
  pin). The text is now rebuilt as the pin's: `CREATE TABLE ` /
  `CREATE VIEW ` / `CREATE [UNIQUE] INDEX ` and the statement
  from the bare name on (so `IF NOT EXISTS` and the keywords'
  case are not kept either; `parser::schema_text_start`). `ON
  main.t` in CREATE INDEX is a syntax error, as on the pin.
  (`temp.` / other schemas were already refused, CTAS included.)
- *CTAS names* (`v16`): unary `+` was dropped by the parser, so
  `+a` named its column `a` with INT affinity. The AST and the
  evaluator now keep it (`Expr::Pos` / `RExpr::Pos`: the
  operand's value and collation, no affinity) — which also
  fixes `+a = '1'` comparing with affinity (a silent wrong answer
  on the claimed v0.4 path) and derived / view column names.
  An unaliased column reference takes the table's spelling (`A`,
  `S.B` → `a`, `b`, via the new `SubPlan::result_named`), as do
  view column lists; an alias spelled `oid` is kept.
- *`-9223372036854775808`* (every profile): the pin's
  `codeInteger` makes `-` over that literal (through `+` and
  parentheses) INTEGER i64::MIN; hipp folded it to a REAL and
  stored that through DEFAULT, generated columns, CHECK, ALTER
  and plain INSERT. `- -9223372036854775808` overflows to REAL.
- *Evaluator values now on disk*: `||` is always TEXT (a blob
  operand gave BLOB); `abs()` follows the pin's `absFunc` —
  INTEGER i64::MIN is `SchemaKind::IntegerOverflow`, TEXT / BLOB
  give REAL (`abs('5')` = 5.0); REAL → TEXT (`||`, CAST, TEXT
  affinity — the tiny profile's INSERT too) is the pin's: 15
  significant digits if they read back as the value (through
  the pin's own `parse_real`), else 17, laid out as `%!.17g`
  (`1.0e+20`, `1.0e-05`, `10000000000000000.0`); it used to print
  every digit (`100000000000000000000`, and 5e-324 as forty
  zeros). The digits come from exact big-integer arithmetic
  (`lexer::real_digits`), so no build links `core::fmt`'s float
  formatting any more.
- *ALTER TABLE ADD COLUMN on an empty table*: the pin refuses a
  non-constant default, NOT NULL with no / a literal NULL
  default, and a STORED column only when the table has rows
  (`sqlite3ErrorIfNotEmpty`); hipp refused them always. They
  are now checked at step time (`Plan::AlterAdd::empty_only`).
- *AUTOINCREMENT* (corruption, found while probing the grammar,
  every profile): it was parsed and ignored, and a hipp-created
  AUTOINCREMENT table has no `sqlite_sequence` — the pin's next
  INSERT reports "database disk image is malformed". CREATE
  TABLE now refuses it (`Error::Unsupported`); files the pin
  wrote with one still load and stay writable.
- *Clock*: `Config::clock` was read once per clock-default
  column, so one row could mix two instants (CURRENT_DATE
  1970-01-01 with CURRENT_TIME 00:00:00). A statement now reads
  it at most once (`rules::Clock`). hipp-cli supplies the system
  clock (it had none, so every CURRENT_* default failed with
  `NoClock` there).

Review-pass tests (each fails on 104c4da): in
`tests/table_defs_v16.rs` — `malformed_definitions_are_refused_like_the_pin`
(every review reproduction plus the same pattern across the
column grammar; verdicts agree and every accepted schema passes
the oracle's `integrity_check` on hipp's file),
`prng_definition_grammar_corpus` (120 PRNG definitions from valid
and malformed fragments), `repeated_constraint_sets_share_one_autoindex`,
`main_qualifier_is_left_out_of_the_schema_text`,
`unary_plus_drops_affinity`, `create_table_as_select_names_follow_resolution`,
`minimum_integer_literal_through_rules`,
`stored_values_match_the_pins_evaluator`,
`alter_add_column_on_an_empty_table`,
`clock_is_read_once_per_statement` (a ticking clock across
midnight), `autoincrement_is_refused_at_create_and_read_from_foreign_files`;
in `hipp-cli/tests/cli_v16_table_defs.rs` a byte-identical review
batch (plus the oracle reading both schemas and the view's
`.schema` column list) and the CLI clock; parser unit tests for
the grammar (both profiles), the i64::MIN literal and the schema
text start.

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

Review pass, vs 104c4da (after the collations unit): tiny
**76,048 B** (Δ **−5,504 B** vs 81,552, ratchet lowered — the
REAL → TEXT digits now come from the lexer's existing big-integer
helpers, which drops `core::fmt`'s float formatting (~9.7 KB:
Grisu / Dragon, both decimal formatters); that pays for the
clause-by-clause CREATE TABLE walker (~3 KB), the
`schema_text_start` re-lex with its AUTOINCREMENT refusal, the
i64::MIN literal rule and the REAL layout); full **280,395 B**
(Δ **−3,900 B** vs 284,295, ratchet lowered; the same saving less
the unary-plus node, CTAS naming, autoindex dedupe and ALTER's
empty-table path).

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
  or expression DEFAULT (the WITHOUT ROWID precedent). (Its
  refusal of the unquoted `DEFAULT identifier` form is gone: the
  review pass's grammar walker tells the clock keywords apart.)
  Without `indexes` it does not check COLLATE names (it has no
  collations), where the pin refuses an unknown one.
- A double-quoted `"current_timestamp"` inside a parenthesised
  DEFAULT reads as the clock keyword (the AST does not keep
  quotedness); the pin calls it a non-constant column.
- CTAS: a fourth duplicate column name gets a random suffix on
  the pin — hipp refuses it (`Unsupported`); compound affinity
  demotion reads hipp's resolved
  expression shapes, not the pin's parse tree (they agree on
  the probed literal / column / CAST / function / CASE shapes).
  hipp cannot query `sqlite_schema` itself (another unit), so
  CTAS text is verified through the oracle reading hipp's file.
- A record shorter than its table whose missing column has a
  foldable *expression* default reads the folded value; the
  pin's `OP_Column` would read NULL — reachable only with a
  foreign writer (ALTER refuses such defaults).
- Review pass gaps: the UPDATE ... FROM "last combination" is
  hipp's join order — with an equality join on an unindexed FROM
  table the pin builds an automatic index and takes the
  combination last in that index's (key, columns) order instead
  (`s = (1,1),(1,9),(1,3)`: pin 9, hipp 3; SQLite calls the
  choice arbitrary), so tests keep one matching row per target
  or a cross join. (REAL → TEXT digits: fixed bit-exact later
  in v0.16, see "CI vs the real pin".)
  `-0x8000000000000000` is "hex literal too big" on the pin and
  a REAL in hipp. AUTOINCREMENT tables cannot be created (no
  `sqlite_sequence` upkeep), and hipp's INSERTs into a foreign
  one do not advance its `sqlite_sequence` row (the pin then
  takes max(rowid) + 1, so the file stays valid, but a rowid
  hipp wrote and someone deleted can come back). A clock default is
  evaluated when the statement is prepared, so re-stepping a
  prepared statement reuses that instant (the pin reads it per
  execution). The non-fallback keyword list was probed on the
  stand-in (`CREATE TABLE t(a K)` for each of the 147 keywords).
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
  generated-column expressions; an IN list compares (and, since
  the review fixes, converts affinity) by its left operand only
  (3.53 rewrote a single constant item to `x = +c` and so took
  its COLLATE; the 3.54.1 pin does not — see "CI vs the real
  pin"); `x IN (SELECT ...)`
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

**Review fixes (second commit):**

- **Upsert `excluded.<col>`** (a regression of the first
  commit) took the target column's collation, and (older) its
  affinity. On the pin it is a register reference with
  neither: `excluded.a = 'x'` is BINARY over a NOCASE `a`,
  `excluded.a = a` takes `a`'s NOCASE, and a TEXT column's
  `'1'` is not `1`. `SourceDesc::registers` resolves such
  references as `+col` with no collation
  (`RExpr::Column(.., None)`).
- **`UNIQUE (a DESC COLLATE NOCASE, b)`** (accepted with `b`
  dropped, stored as SQL the pin cannot load) was already a
  syntax error after the column-constraints review commit's
  clause-by-clause walk; regression tests added (also for
  PRIMARY KEY and CREATE INDEX).
- **UPDATE OR REPLACE** (older, any collation) visited rows an
  earlier REPLACE in the same statement had deleted, and wrote
  them back. The pin collects rowids and re-reads each one: a
  deleted row is skipped, and a rowid another row moved into
  is updated from its current content (`SET id = id + 1,
  v = v || 'x'` over ids 1, 2 leaves `3|axx`). `exec_update`
  now re-reads each row (`reload_write_row`) once a REPLACE
  deletion or a rowid change has happened.
- **`x IN (list)` affinity** (older): a written list converts
  both sides by the LEFT operand's affinity only (no affinity:
  no conversion), as the pin's `OP_Eq` / ephemeral-index paths
  do. hipp used the binary-comparison rule, so `d IN (e)` with
  `d` untyped `'1'` and `e INTEGER 1` gave 1 (pin 0). `IN
  (SELECT ...)` keeps the binary rule (`InList`'s new
  `from_sub` flag marks the substituted list).
- **Files naming a non-built-in collation** failed to open at
  all (a regression for files of applications that register
  collations). Now only that table is refused (gap below).
- Not changed: GROUP BY over a column with a differently
  collated index shows the pin's index-order representative
  (plan-dependent; gap below).

**Gates:** `ci/local-gates.sh` (full run) green. New suites:
`tests/collation_v16.rs` (23 tests after the review fixes:
COLLATE after ASC / DESC, upsert `excluded`, UPDATE OR REPLACE
with a 12-statement PRNG corpus over NOCASE / RTRIM UNIQUE,
IN-list affinity, and a file naming an unknown collation join
the first 18: column-collation
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
pass in a `std,indexes,v10` build, 20 after the review),
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
fixes). Review fixes: tiny **76,016 B** (Δ **−32 B** vs the
76,048 ratchet the later units left; ratchet lowered; code
layout only, as none of the fixes is in the tiny profile); full
**289,995 B** (Δ **+1,240 B** vs 288,755, ratchet raised: the
UPDATE row re-read, the IN-list affinity rule and its flag,
register references, the foreign-collation table handling).

**Honest gaps:**

- The tiny profile still accepts (and ignores) `COLLATE
  <unknown>` in CREATE TABLE: it has no comparisons a collation
  could change, and validating the name did not fit its
  ratchet.
- An unknown name in the `COLLATE` operator is refused at
  prepare even where the pin, which looks names up only when a
  comparison or sort uses them, runs the statement (`SELECT 1
  COLLATE bogus`).
- A table whose stored SQL names a collation hipp does not
  have (an application-registered one) is listed, but every
  statement that reads or writes it is refused
  (`UnknownCollation`). The pin reads it and errors only where
  the collation is used, e.g. `SELECT * FROM t` runs there. An
  index naming one stays opaque (its table's writes refused;
  the pin errors there too). `.dump` stops at such a table.
  DROP TABLE works.
- GROUP BY / DISTINCT without ORDER BY over a column with a
  *differently* collated index (`ON k(a COLLATE BINARY)` on a
  NOCASE `a`): the pin scans the covering index, so a group's
  shown value is the first row in that index's order (`B`);
  hipp shows the first in rowid order (`b`). This is
  plan-dependent row choice, the same class as the older
  BINARY-index unordered-output differences; matching it means
  modelling the pin's covering-index choice, a planner project.
- `x IN ((SELECT ...))` (a lone parenthesised subquery item,
  which the pin rewrites to `x IN (SELECT ...)`) is a parse
  error in hipp (older, a refusal).
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

### Bound parameters (2026-11-02)

**Status:** implemented, differential-tested against the
oracle, gated behind the new leaf feature `params` (in `full`;
`params = []`, so it builds alone on top of the tiny profile —
`ci/feature-sets.txt` has its line).

**Shipped — `params`:**

- **Syntax and numbering** (`sql::lexer::ParamTable`, assigned
  in the lexer in source order, which is the pin's
  `sqlite3ExprAssignVarNumber` order): `?` takes one past the
  largest index so far; `?NNN` takes NNN (leading zeros
  allowed) and names that index `?NNN` unless it has a name;
  `:name` / `@name` / `$name` / `#name` reuse the index of an
  identical spelling (case-sensitive) else take the next one;
  the TCL forms `$a::b` and `$a(x)` lex as one name. Errors:
  `?0`, `?32767`, an overflowing NNN (`ParseKind::
  VariableNumber`), an index past 32766 (`TooManyVariables`),
  `#1`, a bare sigil, `$a(x y)` (`UnexpectedToken`).
- **Statement API** (doc-commented): `bind_null` / `bind_i64` /
  `bind_f64` (NaN binds NULL, as on the pin) / `bind_text` /
  `bind_blob` / `bind_value` by 1-based index
  (`Error::BindIndexOutOfRange`; binding a stepped, un-reset
  statement is `Error::ResetRequired`, the pin's
  `SQLITE_MISUSE`), `parameter_count`, `parameter_index(name)`,
  `parameter_name(i)` (the pin's answers, including `?1` not
  finding `?01` and nameless `?` having no name),
  `clear_bindings`, and `reset`, which makes any prepared
  statement executable again (it was single-shot). Unbound is
  NULL.
- **Binding** (`sql::bind::bind_ast`): each execution plans a
  copy of the parsed tree with every parameter replaced by its
  value as the literal of the same storage class, so affinity,
  comparison, constant folding and the tiny profile's literal
  paths treat it exactly as the pin treats a bound value (which,
  like a literal, has no affinity or collation). The one
  difference the pin makes — an integer literal in ORDER BY /
  GROUP BY is an ordinal, a parameter is not — is encoded: such a
  term becomes `+value`, or, in a compound ORDER BY, the ordinal
  of the first result item (leftmost core first) that is the
  same parameter (same index and spelling: the pin's
  `resolveCompoundOrderBy` + `sqlite3ExprCompare`), with a
  multi-row VALUES laid out as the pin's `sqlite3MultiValues`
  builds it (rows sharing a co-routine never match; see the
  review fixes); anything else there is "does not match any
  column", as on the pin.
- **Re-execution:** the first `step` after a `reset` or a
  binding change plans again from the retained parse tree (a
  plan captures the catalog and an expression DEFAULT's clock
  read, so every execution plans afresh — `DROP TABLE`
  re-executed is "no such table", CTAS "already exists"),
  bringing the catalog up to date first, and rewinding the
  statement arena to its pre-plan mark (asserted: 200 executions
  do not grow it). A plan that fails for the unbound (all-NULL)
  state with an error a binding could change (a constant row's
  NOT NULL / CHECK / MustBeInt failure, say) is deferred to the
  first `step` (`Plan::Deferred`; `column_count` is 0 until
  then). Only such value errors are deferred (a deny-list:
  constraint, type, rowid, overflow and ESCAPE / pattern
  errors); every other error — grammar, name resolution, a name
  already taken — is `prepare`'s, as on the pin.
- **Where:** in the full profile every expression position —
  WHERE, SET, VALUES, LIMIT / OFFSET, scalar / IN / EXISTS
  subqueries, upsert targets and DO UPDATE, RETURNING, compound
  cores and VALUES, derived tables, joins' ON, HAVING, aggregate
  arguments, INSERT ... SELECT, UPDATE ... FROM, CREATE TABLE ...
  AS SELECT (columns named by the parameter text: `"?"`,
  `"? + 1"`, `":n"`, byte-identical to the pin). On top of the
  tiny profile: INSERT VALUES and `WHERE <col> = ?` (rowid seeks).
  Refused, as on the pin, in CREATE TABLE (other than its AS
  SELECT), ALTER TABLE and CREATE VIEW
  (`SchemaKind::ParameterNotAllowed`; `DEFAULT ?` stays a syntax
  error). A parameter in a stored schema text resolves to NULL
  (the pin's schema-parse rule).
- **hipp-cli** binds like the oracle shell: each parameter by
  its name (a nameless `?` as `?N`) from `.parameter set NAME
  VALUE` (VALUE evaluated as SQL, else taken as text), NULL
  otherwise; `.parameter unset / clear / list / init`. It now
  steps every statement to completion (a deferred plan knows its
  columns only once stepped).
- **`examples/params.rs`**: prepare once, bind / step / reset
  per row, re-run a query with new bindings.

**Shipped — ungated (found by the generated corpus; claimed
v0.4 / CLI paths, wrong with or without parameters):**

- `%` with a REAL or real-looking operand is a REAL computed on
  both operands' integer values — a text operand's integer
  *prefix* (`'1e18' % 2` is 1.0); hipp answered an INTEGER.
- `CAST(text AS INTEGER)` reads only the integer prefix
  (`sqlite3Atoi64`: `CAST('1e18' AS INTEGER)` is 1, hipp gave
  10^18).
- `CAST(text AS NUMERIC)` is the pin's `sqlite3VdbeMemNumerify`:
  an integer-shaped prefix that fits i64 is an INTEGER, any
  other number only when integral with |x| < 2^51 —
  `CAST('9223372036854775808' AS NUMERIC)` and `CAST('1e18' AS
  NUMERIC)` are REALs (hipp gave integers).
- hipp-cli printed a REAL -0.0 as `-0.0`; the pin's printf has
  no negative zero (`SELECT -0.0` prints `0.0`; the value keeps
  its sign — `atan2` shows it on the pin).

**Review fixes (2026-11-02):**

- *Compound ORDER BY parameter, leftmost core decides* (wrong
  order, no error): hipp scanned the cores from the rightmost.
  `SELECT ?1, 2 UNION SELECT 3, ?1 ORDER BY ?1` with 9 now sorts
  by column 1 (`3|9`, `9|2`), as the pin does.
- *Multi-row VALUES cores* (rows where the pin errors): rows of a
  multi-row VALUES matched one by one. They are now laid out as
  `sqlite3MultiValues` lays them out (probed with real bindings
  on the 3.53.1 library; the co-routine form is 3.46+). A row
  joins a co-routine, whose `column1..N` items never match, when
  the row is constant (has no subquery) and the core before it
  is that co-routine, or is a plain row that is constant with no
  top-level CAST under any COLLATE. Otherwise the row stays a
  core of its own and can match. A multi-row VALUES to the right
  of an operator is a subquery, so it never matches. Every
  built-in function, LIKE and GLOB included, counts as constant.
- *Errors at prepare*: the deferral list is a deny-list of value
  errors (it was an allow-list of prepare errors), and CREATE
  TABLE ... AS SELECT checks its name when planned, before the
  SELECT is resolved, as `sqlite3StartTable` does. So
  `prepare("CREATE TABLE t AS SELECT ?")` over an existing `t`
  is `DuplicateTable` at prepare. A CLI-visible wrong answer was
  fixed too: `CREATE TABLE IF NOT EXISTS t AS SELECT nope` over
  an existing `t` is now a no-op. hipp used to fail it with "no
  such column".
- *IN-list affinity of a bound value*: `? IN (int_col)` with
  `'5'` is 0. The evaluator fix landed in 2ea68e6, and regression
  cases for the bound form are in the affinity matrix.
- *Ungated, integer arguments read as the pin's
  `sqlite3_value_int64`*: substr()'s start / length and round()'s
  digit count take a text's integer prefix (`substr(x, '1e1')`
  starts at 1, hipp read 10). Further round() fixes:
  - The digit count is clamped to 0..=30. `round(1.5,
    1000000000000)` looped 10^12 times.
  - round() follows the pin's `%!.*f` rounding: the exact decimal
    expansion, rounded half away from zero at n places or at the
    26th significant digit, read back. hipp scaled by 10^n, so
    `round(2.675, 2)` gave 2.68 (the pin gives 2.67) and `round(1.5,
    31)` gave 9.2e-13.
  - substr()'s coordinate arithmetic saturates at the i64 bounds.
    It used to overflow and panic, as in `substr(x, -2,
    9223372036854775807)`.
- *Ungated, hipp-cli*: a REAL whose 16th significant digit is an
  exact 5 printed rounded to even (`5833858491548785.0` gave
  `5.83385849154878e+15`). It now rounds half away from zero, as
  the pin's printf does. 12,000 random doubles print the same as
  the oracle.
- *Tiny + params in CI*: `tests/params_tiny_v16.rs` (which runs
  in the default test run, so in CI) builds `tests/params_v16.rs`
  with `--no-default-features --features std,params` in its own
  target directory. It requires the three tiny-grammar tests to
  run and pass against the tiny parser, which no other test run
  reaches. It takes about 10 s.
- The `Error::ResetRequired` doc no longer says `clear_bindings`
  can return it. `clear_bindings` is infallible.

**Oracle-probing notes** (numbering: the C API of the system
libsqlite3 3.45 via ctypes — `sqlite3_bind_parameter_count /
_name / _index` — whose rules have not changed in years, with
the 32766 limit from the local 3.53.1 stand-in, the system build
allowing 250000; values: the stand-in's Python binding): `SELECT
?5, :a` has 6 parameters, `:a` = 6; `:a, ?1` shares index 1
named `:a` (`?1` is not found by name); `?2, :a, ?1` names 1..3
`?1 ?2 :a`; `?a` is `?` aliased `a`, `?1a` is `?1` aliased `a`;
`:` / `@` / `$` / `#` alone are unrecognized tokens, `#1` a
syntax error. A TEXT `'5'` bound against INTEGER / NUMERIC /
REAL columns converts by the column's affinity exactly as the
literal does, `? = 5` with `'5'` is 0, `LIMIT ?` takes `'2'` and
`2.0` but errors on 2.5 / NULL / 'abc', `INSERT ... (rowid)
VALUES (?)` takes `'100'` / 101.0 and errors on 'abc' / 102.5,
a NaN binds NULL, `-?` over i64::MIN is the REAL 9.2e18. `ORDER
BY ?` (1) keeps scan order and `GROUP BY ?` makes one group;
`SELECT ?1 UNION ALL SELECT 2 ORDER BY ?1` sorts by column 1
(also under COLLATE and DESC) but `ORDER BY +?1`, `:a ... ORDER
BY ?1` (same index, other spelling) and `? ... ORDER BY ?`
(other index) are "does not match". Views / CHECK / generated
columns: "parameters prohibited"; `DEFAULT (?)`: "not constant";
`DEFAULT ?`: syntax error; CTAS allowed. The oracle shell binds
from `temp.sqlite_parameters` by name (`?N` for nameless) and
NULL otherwise (shell.c's `bind_prepared_stmt`).

**Gates:** `ci/local-gates.sh` (full run) green. New suites:
`tests/params_v16.rs` (13 tests: the numbering table and errors
against the probed C-API answers, the bind / reset API
contract, the tiny grammar's positions and API — the three
tiny-grammar tests also pass in a `std,params` build — a 200-row
prepared-INSERT re-execution corpus with the arena bound, the
affinity matrix of 15 bound values × 9 column expressions × 5
operators plus storage into every affinity, every expression
position (35 reads, 13 writes, CTAS schema text), ORDER / GROUP
BY constants and compound matches with hand-written oracle SQL,
error-vs-error agreement and deferred value errors,
re-execution across schema changes, a 400-case generated
expression corpus with random bindings, and the CAST / `%`
regressions; every case compares hipp-with-bindings to the
oracle with the values inlined, and every write is checked by
the oracle reading both files with `quote()` and
`integrity_check`), `hipp-cli/tests/cli_v16_params.rs` (unbound
parameters one-shot and over stdin, and a `.parameter set`
batch of every storage class against inlined literals,
byte-identical, with the `-0.0` regression).

**Size:** tiny **76,048 B** (Δ **0**; `params` is not in the
tiny profile and its code compiles out); full **288,755 B**
(Δ **+8,360 B** vs 280,395, ratchet raised: `params` +7,936 B —
the lexer numbering, the tree-binding walker and the parse-tree
clone it plans from, the Statement API and re-planning — and the
CAST / `%` fixes +424 B). For information, tiny + `params`
(`--no-default-features --features params`) is **84,672 B**
(+8,624 B over tiny).

*Review fixes:* tiny **76,016 B**, Δ **0** against the current
ratchet (the 76,048 figure above was measured before 2ea68e6).
`params` stays out of the tiny profile, and the fixed evaluator,
CTAS and compound-ORDER-BY paths are not in it. Full **291,267
B**, Δ **+1,272 B** against 289,995, and the ratchet is raised to
that figure. round()'s exact rounding is about +460 B, the
multi-row VALUES layout and leftmost match about +560 B, and the
CTAS name check at plan time with its re-plan about +300 B. For
information, tiny + `params` is now **85,048 B** (+376 B, the
VALUES layout of `sql::bind`).

**Gates (review fixes):** `cargo fmt --all` and the full
`ci/local-gates.sh` run end with "==> all gates passed".
`tests/params_v16.rs` now has 14 tests: the compound ORDER BY
corpus gained 24 cases (leftmost core, VALUES co-routine layout,
match and no-match), the affinity matrix gained `? IN (col)`
forms, and there are new round() and substr() regressions,
including a 600-case random round() corpus and the i64 extremes.
`tests/params_tiny_v16.rs` is new: it runs the tiny + `params`
tests in CI. `hipp-cli/tests/cli_v16_params.rs` has 3 tests: the
new one covers the compound ORDER BY, the text integer arguments
and REAL ties, byte-identical.

**Honest gaps:**

- Every execution after the first plans again (from the
  retained parse tree, not the SQL): correct by construction,
  but a re-executed statement pays planning each time.
- A plan that fails for the all-NULL bindings with a value error
  is deferred, so such a statement reports `column_count` 0 until
  it is stepped.
- The compound ORDER BY rules for a multi-row VALUES follow the
  3.53.1 library, probed with real bindings. The tests check them
  against hand-written literal SQL, because the oracle shell
  cannot bind. The co-routine rule depends on the version (3.46
  and later), and the pin is 3.54.1.
- Found, not fixed (existing, outside this unit): when a compound
  has an ORDER BY that it merges directly, hipp breaks ties
  between rows by all of their columns. On the pin, UNION ALL
  ties keep input order: `SELECT 1, 2 UNION ALL SELECT 1, 0 ORDER
  BY 1` is `1|2`, `1|0` on the pin and `1|0`, `1|2` in hipp. The
  corpora avoid such ties.
- round() reads its exact decimal expansion, while the pin reads
  the digits its double-double decoder gives (about 20). They
  could differ only for a value whose rounding is decided past
  about the 18th significant digit. 6,000 random cases agree.
- Bound text / blob bytes are copied into the statement arena
  when planned, so a bound value counts against `stmt_max` as
  the same literal in the SQL would.
- hipp-cli does not implement the shell's test-only magic
  parameter names (`$int_N`, `$text_X`, `:NAN`, `:INF`);
  `.parameter list` follows shell.c's layout (`%-*s %s` with
  `quote()`d values) but was not verified against the pin
  (the stand-in has no `.parameter`).
- In a pre-`v16` build with the evaluator (`v04`..`v10`, not in
  `ci/feature-sets.txt`) LIMIT / OFFSET take only an integer
  literal, so a parameter there is a syntax error.
- Found here, fixed since (see "CI vs the real pin"): the pin
  converts a REAL to TEXT with up to 17 digits of its own
  (inexact) decoding — `86255.9396259168 || ''` is
  `86255.939625916799` there. Still open: a -0.0 stored in a REAL column keeps its sign
  in hipp's record where the pin's reads back +0.0. Unordered
  results that the pin returns in index order (a `GLOB` prefix
  over a UNIQUE column) can differ in order, as before.
- Probed on the local 3.53.1 stand-in and the system 3.45 C API;
  parameter numbering and binding semantics are long stable,
  and CI runs the real 3.54.1 pin.

### Expression operators, literals and row values (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin (`/home/user/hipp-design/oracle/sqlite3`, `--version`
3.54.1), gated as below. New surface is behind `v16`; the fixes
to claimed paths are ungated.

**Shipped — ungated (silent wrong answers on claimed v0.4 / v0.6
paths):**

- *Precedence.* The expression parser is now a
  precedence-climbing loop over the pin's `parse.y` levels: OR <
  AND < NOT < `= == != <> IS IN LIKE GLOB BETWEEN ISNULL NOTNULL`
  < `< <= > >=` < `& | << >>` < `+ -` < `* / %` < `||` <
  COLLATE < unary `- + ~`. `<` and `=` used to share a level
  (`3 = 2 < 3` was 1, `0 = 1 > 2` was 0). A postfix operator's
  result takes tighter operators after it (`x ISNULL + 1` is
  `(x ISNULL) + 1`, `1 IN (1) + 1` is 2), BETWEEN's lower bound
  runs to its own AND and its upper bound takes only operators
  tighter than `=`, and a unary operand may start with NOT
  (`- NOT 0 = 1`).
- *`x IS [NOT] TRUE / FALSE`* is the pin's truth test
  (`TK_TRUTH`) when the bare keyword is the whole right operand:
  any true value IS TRUE, NULL is neither (`2 IS TRUE`, `0.5 IS
  TRUE`, `'a' IS FALSE` were 0). Written as the CASE it equals.
- *Hex literals* (evaluator builds): 16 significant digits as
  two's complement (`0xffffffffffffffff` = -1,
  `0x8000000000000000` = i64::MIN; hipp refused anything past
  `0x7fff...`), leading zeros free, more digits the pin's "hex
  literal too big" (`ParseKind::HexLiteralTooBig`), as is `-`
  over `0x8000000000000000` through parentheses / unary `+`
  (`-(+0x8000000000000000)`, `- - 0x8...`; `0 - 0x8...` is the
  REAL). `'0x10'` text is never hex (CAST, affinity, `+0`).
- *Digit separators and glued identifiers* (evaluator builds):
  the pin's tokenizer — `_` between two digits (hex digits in a
  hex literal) in every part, `1_000` = 1000 (hipp read `1`
  aliased `_000`, a silent wrong value), and any number followed
  directly by an identifier character (`1a`, `1_`, `1e_5`,
  `0x1g`, `1$`) is "unrecognized token" (hipp read `1a` as `1`
  aliased `a`). `-9_223_372_036_854_775_808` is the INTEGER.
  The tiny profile keeps its old scan (it has no position where
  either reading was accepted, and refuses hex past
  `0x7fff...`, as before).
- *Aggregate scoping* (`RExpr::Agg::lvl`): an aggregate whose
  arguments reference no column of its own query but some
  enclosing query's belongs to the innermost such query (the
  pin's `sqlite3ReferencesSrcList` walk); no column at all keeps
  it local. The owner counts it as its own (`has_agg` /
  `collect_aggs` walk subquery plans by depth, so `SELECT
  (SELECT max(t.a) FROM u) FROM t` is one row and the min/max
  bare-column rule sees it), folds it per group and substitutes
  the value into the subquery plan before executing it
  (`subst_owned_aggs`, in `eval_group_sub`); the subquery does
  not aggregate over it (`(SELECT max(t.a) + b FROM u)` keeps
  u's rows, `GROUP BY max(t.a)` there is legal). "misuse of
  aggregate" where the owner cannot aggregate: its WHERE (also
  through an alias), GROUP BY, an aggregate's arguments, UPDATE
  / DELETE; an enclosing query's aggregate directly in a WHERE
  is refused unless that WHERE's own query aggregates (the
  pin's NC_AllowAgg rule).
- *ORDER BY aggregate:* an aggregate in ORDER BY alone no longer
  makes the query an aggregate one (`SELECT a FROM t ORDER BY
  max(a)` returned one row; the pin: "misuse of aggregate").
  An aggregate directly inside another's arguments is refused
  when it resolves, whichever queries own the two
  (`sum(max(a))`, and `(SELECT sum(c + max(t.a)) FROM u)` —
  see the review fixes below).
- *Qualified names fall through:* `t.a` where an inner source
  is aliased `t` but has no column `a` resolves in the enclosing
  scopes, as the pin's `lookupName` does (hipp: "no such
  column").

**Shipped — `v16`:**

- `&`, `|`, `<<`, `>>` (one level, left-associative) and unary
  `~`: operands as 64-bit integers (`sqlite3VdbeIntValue`: a
  text / blob's integer prefix, a REAL truncated and saturated),
  NULL in -> NULL out, always INTEGER; a negative shift count
  shifts the other way, a count >= 64 gives 0 (-1 for a negative
  value shifted right) — `OP_ShiftLeft` / `OP_ShiftRight`.
- `IS [NOT] DISTINCT FROM` (IS NOT / IS; `x IS NOT DISTINCT FROM
  TRUE` is the truth test, as on the pin) and postfix `NOT
  NULL`.
- Row values (`Expr::Vector`, `SchemaKind::RowValueMisused`):
  `(a, b) op (c, d)` for `= == <> != < <= > >= IS [NOT]` is
  written as scalar comparisons — AND of the element tests for
  `=` / IS, OR for `<>` / IS NOT, lexicographic `a1 < b1 OR (a1
  = b1 AND ...)` otherwise — so affinity and collation stay per
  element and three-valued logic gives the pin's NULLs; BETWEEN
  and CASE (base and WHEN rows) build on it. `(a, b) IN ((..),
  (..))` and `IN (SELECT a, b ...)` evaluate natively
  (`ScalarFn::Vector` in an `InList`): the pin rewrites a
  row-value IN list as `IN (VALUES ...)`, whose set takes each
  column's comparison affinity from the left element and the
  RIGHTMOST row's element (`exprINAffinity`) and applies it to
  both sides — the probe and every row's stored values
  (`adjust_set`; probed: `(c,b) IN ((a,'q'),(2,'y'))` finds
  nothing where `((2,'y'),(a,'q'))` finds row 2, and `(1, 1)
  IN ((x, y), (a, b))` with INTEGER x, y and TEXT a, b is 1). A multi-column scalar subquery is a row value
  (`SubPlan::scalar_col`: one plan copy per column; the hoist
  cache keys on (node, column)); row-major subquery values for
  row-value IN. `UPDATE ... SET (a, b) = (x, y)` / `= (SELECT
  ...)` (also in upsert DO UPDATE) splits into per-column
  assignments (`Expr::RowItem`). Errors as on the pin: sizes
  that differ, a row value nested in one, or anywhere but those
  operands (arithmetic, NOT, IS NULL, functions, CAST, COLLATE,
  ORDER / GROUP BY, LIMIT, result columns, INSERT values) are
  `RowValueMisused`; wrong subquery widths and "N columns
  assigned M values" are `ColumnCountMismatch`.

**Oracle-probing notes** (all on the live 3.54.1 pin):
`1<<63` = i64::MIN, `1<<64` = 0, `1<<-1` = 0, `8>>-1` = 16,
`-1>>64` = -1, `1<<-64` = 0, `-1<<-100` = -1; `~5.7` = -6,
`~-5.7` = 4, `~'abc'` = -1; `'1e3'|0` = 1, `'12abc'|1` = 13, `'
12 '|0` = 12, `1e19|0` = i64::MAX, `-1e19|0` = i64::MIN,
`'9223372036854775808'|0` = i64::MAX; `1|2&3` = 3, `6&3|8` =
10, `1&3<<2` = 4 (one level, left to right); `1 + 1 << 1 + 1` =
8, `1 || 2 << 1` = 24, `1 << 1 || 2` = 4096. `0x1_0` = 16,
`0x1ffffffffffffffff`, `-0x8000000000000000`,
`-(0x8000000000000000)`, `- - 0x8...` are "hex literal too big";
`0x`, `0x1g`, `0x_1`, `1__0`, `1_`, `1_e5`, `1e_5`, `1a`, `1$`
"unrecognized token"; `.5_1` = 0.51, `1e1_0` = 1e10. `1 ISNULL
+ 1` = 1, `2 = 1 ISNULL + 1` = 1, `1 NOT NULL + 1` = 2, `NOT 1
NOT NULL` = 0, `2 BETWEEN 1 = 1 AND 3` = 1, `'a' LIKE 'b' = 0`
= 1, `0 = 'a' LIKE 'b'` = 0, `1 BETWEEN 1 OR 0 AND 3` is
"incomplete input". `(1,NULL)<(1,2)` NULL, `(1,NULL)<(2,0)` 1,
`(2,NULL)<(1,0)` 0, `(1,NULL)<=(1,NULL)` NULL; `(1,2) IN ((1,2),
(NULL,1))` 1, `(1,2) IN (VALUES (1,NULL))` NULL, `(1,2) IN
(VALUES (2,NULL))` 0; `(1,2) IN ()` 0; `(1,2) IN ((SELECT
1,2))` 1 but `IN ((SELECT 1,2),(3,4))` an error; `(SELECT 1,2)
IN ((1,2))` misused but `IN (SELECT 1,2)` fine; `CASE (1,2) WHEN
(1,2)` works; `(SELECT 1,2 UNION SELECT 0,0) = (0,0)` is 0 (the
UNION first-row rule); `SET (a,a) = (5,6)` keeps 6, `SET (rowid,
a)` works, `SET (a,b) = (SELECT 7,8 WHERE 0)` writes NULLs.
Aggregates: `SELECT a, (SELECT count(t.a) FROM u) FROM t` is
`1|3`; `(SELECT count(t.a) FROM e)` over an empty e is one NULL
row; `(SELECT (SELECT max(u.b) FROM e) FROM u)` makes the u
level aggregate (three NULL rows); `(SELECT 1 FROM u WHERE b =
max(t.a))` is "misuse of aggregate function" but `(SELECT
count(*) FROM u WHERE b < min(t.a))` runs; derived tables
inside the subquery count at the subquery's level (`(SELECT x
FROM (SELECT max(t.a) AS x))` is 3).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh`
run end with "==> all gates passed". New suites:
`tests/expr_v16.rs` (23 tests: precedence fixed cases and a
480-expression generated corpus over every level; truth tests
over a table; DISTINCT FROM / NOT NULL; bitwise fixed cases, a
720-expression generated corpus over every storage class and
the shift counts around 0 / 64, columns of every affinity,
generated columns and UPDATE; hex literals, their errors and
affinity contexts (typed columns, DEFAULTs); digit separators
and glued identifiers; the smallest integer literal; row-value
comparisons, a 420-expression generated corpus, IN lists /
VALUES / subqueries with the rightmost-row affinity rule, scalar
subquery rows, 43 error-vs-error cases, UPDATE / upsert SET
rows, CHECK and DELETE; aggregate scoping — 58 statements, the
misuse cases, the alias cases, the qualifier fall-through and a
generated corpus over random tables). Every write is checked by
the oracle reading both files and `integrity_check`.
`hipp-cli/tests/cli_v16_expr.rs` (2 tests, byte-identical
one-shot and over stdin, REAL results included). Updated:
`tests/agg_subquery_v11.rs` accepts `sum(sum(b))` refused at
prepare (the pin's behaviour) as well as at step.

**Size:** tiny **75,960 B** (Δ **0**: the bitwise / row-value
code is `v16`, the precedence / scoping code is in the
evaluator, and the tiny lexer keeps its number scan — it also
dropped an `expect`). Full **301,379 B** (Δ **+9,848 B** vs
291,531, ratchet raised to it): the precedence-climbing parser,
row values (resolution, IN evaluation, the per-column subquery
parts, UPDATE SET rows), the bitwise operators, the evaluator's
number lexer and the aggregate-scope walkers.

**Review fixes (2026-11-02, all re-probed on the live pin;
regression tests in `tests/expr_v16.rs`, CLI lines in
`hipp-cli/tests/cli_v16_expr.rs`):**

- *Row-value IN list affinity* (`v16`): the set affinity was
  applied to the left operand only, so `(1, 1) IN ((x, y), (a,
  b))` with INTEGER x, y and TEXT a, b was 0 (the pin: 1, its
  VALUES set stores x, y as TEXT too). `adjust_set` now converts
  both sides; a 300-expression generated corpus over every
  affinity and literal kind guards it.
- *Mixed-owner nested aggregates* (ungated): `(SELECT sum(c +
  max(t.a)) FROM u)` (sum owned by u, max by t) passed prepare
  and returned 25; the pin's resolver clears NC_AllowAgg while
  it walks an aggregate's arguments, so any aggregate directly
  inside another's is "misuse of aggregate function". The check
  now runs as the aggregate resolves (`direct_agg`).
- *EXISTS result lists* (ungated): an enclosing query's
  aggregate in an EXISTS subquery's result list or ORDER BY was
  counted as that query's, refusing `WHERE EXISTS (SELECT
  max(t.a) FROM u)` (MisusedAggregate) and the same in ON /
  DELETE / UPDATE (Unsupported). The pin never evaluates those
  terms: such aggregates are now marked inert
  (`RExpr::Agg::inert`, `mark_exists_inert`, for plans whose
  compound operators are all UNION / UNION ALL — under INTERSECT
  / EXCEPT the pin reports the misuse, and so does hipp), are no
  misuse in any clause, do not make the EXISTS an aggregate
  query (`EXISTS (SELECT max(t.a) FROM u WHERE c > 100)` is
  false), evaluate as NULL, and still make the owner an
  aggregate query from its result list (`SELECT EXISTS (SELECT
  max(t.a) FROM u) FROM t` is one row — `has_result_agg`).
- *RETURNING / excluded* (ungated): `RETURNING (SELECT
  max(t.a))` and `DO UPDATE SET b = (SELECT max(excluded.b))`
  were refused; the pin's owner search never stops at a
  RETURNING row or the `excluded` row, so the subquery owns
  the aggregate (`ScopeGroup::agg_owner`). `RETURNING max(t.a)`
  and `(SELECT count(*) FROM u WHERE c > max(t.a))` stay errors.
- *Aggregate aliases in subqueries* (`v16`, item (f)'s alias
  case): `HAVING (SELECT s) > 0` / `ORDER BY (SELECT s)` over
  `sum(a) AS s` were refused; the copied alias's aggregates now
  have their owner level raised by the subquery depth (and are
  marked `alias`, which the pin never re-checks, so they are no
  misuse in the subquery's WHERE). A plan holding an aggregate
  an enclosing query owns is now correlated
  (`plan_has_foreign_agg`), else `(SELECT s)` over `count(*)`
  was hoisted and gave one count for every group. In WHERE /
  GROUP BY (`(SELECT s)` there), or nested (`(SELECT max(s))`),
  it is the pin's misuse.
- *Too-big hex literals* (evaluator builds): the lexer refused
  them, so a pin-written file with `DEFAULT 0x10000000000000000`
  could not be read at all, and CREATE TABLE refused them in a
  DEFAULT or CHECK. The pin raises "hex literal too big" only
  while coding the expression: the lexer now emits
  `Tok::HexTooBig`, the parser keeps `Expr::HexTooBig`, and
  resolution refuses it — so DEFAULT and CHECK texts are
  accepted (CREATE TABLE leaves such a CHECK to the writes,
  `rules::has_big_hex`) and only the INSERT / UPDATE that
  evaluates them fails; a generated column with one is still
  refused at CREATE, as on the pin.
- *Subquery comparison affinity* (ungated, the same pattern
  found elsewhere): a subquery used as an expression took its
  affinity from the LEFTMOST compound core and VALUES rows and
  nested scalar subqueries carried none; the pin reads the
  RIGHTMOST core (`'1' IN (SELECT 1 UNION SELECT i FROM c)` and
  `(SELECT 1 UNION SELECT i FROM c) = '1'` over INTEGER i were
  0), a one-row VALUES's row, the `sqlite3MultiValues` rule for
  longer VALUES (as `core_colls` already did for collations),
  and a nested scalar subquery's own result
  (`sub_col_affinity`, `item_result_affinity`). The pin's
  comparison applies its affinity to BOTH operands (TEXT only
  when either is a string), which matters once a compound's
  value comes from a core of another affinity (`(SELECT 1 UNION
  SELECT t FROM c) = 1` over TEXT t was 0) — `adjust_cmp` now
  does; an IN over a subquery converts both sides
  unconditionally, as the pin stores its set (`adjust_set`).
- *`x IN ((SELECT ...), 2)`* (ungated parser): a list led by a
  parenthesised scalar subquery was a syntax error; it is the
  pin's list.
- *Bare aliases* (ungated parser): a bare alias may no longer
  be a reserved keyword and `AS` needs a name — `SELECT 1 NOT`,
  `SELECT a NOT, a FROM t`, `SELECT 1 AS` returned values, the
  pin gives syntax errors. A census of 70 keywords in four alias
  positions agrees with the pin (except OFFSET, below).

Review-fix gates: `cargo fmt --all` and the full
`ci/local-gates.sh` end with "==> all gates passed";
`tests/expr_v16.rs` has 35 tests (12 new: the regressions
above, with generated corpora of 300 row-value IN lists, 120
compound / VALUES sets and 600 comparisons over every affinity),
and
`tests/query_shape_v16.rs`'s alias-aggregate test now compares
with the pin instead of expecting a refusal. Size: tiny
**75,960 B** (Δ **0**); full **315,255 B** (Δ **+3,204 B** vs
the 312,051 B ratchet the scalar-function unit left; ratchet
raised): the inert / alias aggregate flags and walkers, the
owner-scope flag, `HexTooBig` plumbing, the set / comparison
affinity rules and the rightmost-core affinity.

**Honest gaps:**

- The pin folds constants before coding, so `SELECT 1 WHERE 0
  AND 0x10000000000000000` runs there; hipp refuses every
  too-big hex literal it resolves. CREATE VIEW with one is
  refused (hipp plans a view's SELECT for its width; the pin
  stores it), and ALTER TABLE ADD COLUMN ... DEFAULT with one on
  a non-empty table is refused (the pin stores the literal's
  text as the value). A CHECK holding a too-big hex literal is
  not resolved at CREATE at all, so its unknown columns are
  reported only by the first write (the pin: at CREATE).
- `OFFSET` is still refused as a bare alias (the pin's keyword
  fallback allows it; pre-existing).
- A multi-row VALUES whose co-routine starts after a
  non-constant row (the pin's `sqlite3MultiValues` switching
  form mid-list) takes the last row's affinity in hipp; the
  probed shapes (all constant, or any non-constant row) agree.
  `typeof((SELECT t FROM c UNION SELECT 1 LIMIT 1))` over TEXT t
  holding '1' is `text` on the pin, `integer` on hipp (a scalar
  subquery's UNION row order; derived tables agree) — so
  comparisons against such a subquery can differ.
- A column named `true` / `false` does not shadow the keyword
  (pre-existing for every TRUE / FALSE; now also for `x IS
  TRUE`, which the pin then reads as `x IS <column>`).
- An enclosing query's aggregate whose arguments hold a
  subquery is refused (`Unsupported`); the pin folds it. An
  aggregate of an enclosing query inside a subquery that is
  itself another query's aggregate argument (`(SELECT
  sum((SELECT max(t.a) FROM e)) FROM u)`) is refused with
  `Unsupported` (the pin: misuse) — error vs error.
- The written row-value IN list takes each element's collation
  from the rightmost row; collations on other rows were probed
  only in the cases tested.
- A multi-column scalar subquery used as a row value runs once
  per element it supplies (correlated ones per row and element);
  results are the same, the cost is not the pin's.
- `MATCH`, `REGEXP` and the JSON `->` / `->>` operators remain
  unparsed (their functions do not exist in hipp).
- UPDATE / DELETE refuse an enclosing query's aggregate with
  `Unsupported`, not `MisusedAggregate` (pre-existing kind).

### Scalar function library (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin (`/home/user/hipp-design/oracle/sqlite3`, `--version`
3.54.1), gated as below. Census (now 177 probes): on the 158
earlier probes MATCH 80 → 95 (HIPP_ERR 70 → 55); 19 new probes
for this unit — 14 match, 4 refused by both, `sqlite_source_id()`
left HIPP_ERR on purpose; the old binary was silently wrong on 2
of them (round digits, text with a NUL).

**Shipped — `v16`** (new `src/func.rs`, `ScalarFn` variants,
`query::Env`):

- `printf()` / `format()`: the pin's conversion loop — flags `-
  + space 0 # , !` in any order (last sign flag wins), width and
  precision digits accumulated in 32 bits and masked to 31, `*`
  read as a C int (negative width left-justifies, i32::MIN is 0),
  `l` / `ll` skipped; `%d %i %u %x %X %o %p %r` (ordinal) with
  `,` grouping where the pin allows it and `#` prefixes; `%c`
  (one UTF-8 character, precision = repeat count, NUL for an
  empty / NULL argument); `%s %z` (`!` counts characters);
  `%q %Q %w` and the `#` form (backslashes doubled and control
  characters `\u00xx` — always for %q, for %Q only when one is
  present, then wrapped in `unistr(...)`); `%j` / `%J` (3.54's
  JSON string escapes: `\" \\ \b \f \n \r \t`, other C0 as
  `\u00xx`, NULL as nothing / `null`); `%f %e %E %g %G` from the
  pin's digits; `%%` (width applies), `%n` (nothing). An unknown
  conversion, or `%T` / `%S`, ends the output; missing arguments
  are NULL (0 / 0.0 / no text); the format and text arguments end
  at a NUL; a result of 1,000,000,000 bytes or more is
  `SchemaKind::TooBig` (checked before allocating; large buffers
  are reserved with `AVec::try_reserve`, so allocation failure is
  `ArenaExhausted`, not an abort).
- `quote()` (REAL as REAL -> TEXT, infinities `±9.0e+999`, TEXT up
  to a NUL), `char()`, `unicode()` (the pin's UTF-8 reader:
  overlong / surrogate / U+FFFE-F are U+FFFD, a stray continuation
  byte is itself), `zeroblob()`, `randomblob()` / `random()`
  (splitmix64 over a per-connection `Cell`, seeded from
  `Config::nonce_seed`; negative draws folded as the pin folds
  them, so never i64::MIN), `concat()`, `concat_ws()`,
  `octet_length()`, `unhex(X[, Y])`, `likely()` / `unlikely()` /
  `likelihood()` (P must be a REAL literal in 0.0 ..= 1.0:
  `SchemaKind::LikelihoodNotConstant`), `if()` and iif() with two
  or more arguments (a CASE of pairs with an optional ELSE),
  `sqlite_version()` = `'3.54.1'`.
- `last_insert_rowid()` / `changes()` / `total_changes()` and
  `Connection::last_insert_rowid` / `changes` / `total_changes`:
  every inserted row (INSERT, REPLACE, an upsert's insert) counts
  and moves the last rowid at once — later VALUES rows, DEFAULTs,
  CHECKs and RETURNING see it (a VALUES row reading it is
  evaluated just before its own insert); upsert DO UPDATE and
  UPDATE count once per row (also UPDATE ... FROM with several
  matches), DELETE per row, REPLACE's victims not at all;
  `changes()` / `total_changes()` move at the statement's end
  (inside it they read the previous statement), a failed write
  sets `changes()` to 0 and keeps the rowids it inserted; SELECT,
  DDL, CREATE TABLE ... AS SELECT and transaction control leave
  all three alone; a rolled-back write still counts.
- Generated columns refuse the non-deterministic functions
  (`SchemaKind::NonDeterministic`); CHECK and DEFAULT accept them.
- Unknown functions are `SchemaKind::UnknownFunction` and wrong
  argument counts (scalar and aggregate) `WrongArgumentCount`,
  both at prepare (they were `Unsupported`, ungated).

**Shipped — ungated (silent wrong answers on claimed v0.4 paths):**

- `round(X, N)` printed the exact binary expansion; it now rounds
  the pin's own digits as its `%!.*f` does (`record::round_digits`
  over `record::fp_decode`): `round(166162.7745, 3)` is .775 and
  `round(188.30472979193155, 13)` ...161 on the pin (hipp gave
  .774 / ...149). The STATUS gap from "CI vs the real pin" is
  closed.
- `coalesce` / `ifnull` / `iif` evaluated every argument
  (`coalesce(1, abs(-9223372036854775808))` raised "integer
  overflow"; the pin returns 1); they are lazy now, as the pin
  compiles them.
- The text functions read text as the pin's C functions do: up to
  the first NUL, a lead byte of 0xc0+ taking its continuation
  bytes (invalid UTF-8 included) — `length(CAST(x'610062' AS
  TEXT))` is 1 (was 3), `substr` likewise; a BLOB's `substr`
  counts bytes (`substr(x'c3a9', 1, 1)` was the whole é); `instr`
  of two BLOBs counts bytes; `trim`'s set ends at a NUL and
  matches whole characters (`trim(x'c3a9c3', 'é')` is x'c3');
  `replace` with a NUL-led pattern keeps the subject; `upper` /
  `lower` of a BLOB are TEXT (they returned the BLOB).

**Review fixes** (all reproduced first, then compared with the
live pin; regression tests in `tests/functions_v16.rs` and the
CLI suite's `review` batch):

- **Overflow chains** (ungated; corruption on a tiny-claimed
  INSERT path, already in the base): a record whose overflow tail
  needed more pages than the page cache holds (≈16 with the
  default cache, 4 in the tiny profile) wrote a truncated chain —
  `alloc_page` spilled the fresh zeroed frames, `write_overflow`
  re-read them *clean*, and their writes were lost on the next
  eviction. `zeroblob(69530)` (69529 passed) gave the pin's
  "overflow list length is 15 but should be 17" and a
  CorruptRecord read. The reloaded frame is now marked dirty (the
  same rule `write_leaf_page` follows). Every other fresh-page
  writer already did.
- **Argument counts** (ungated): `coalesce` takes 2+ arguments and
  `ifnull` exactly 2, as on the pin (`coalesce(1)`, `ifnull(1)`,
  `ifnull(NULL, NULL, 5)` used to answer). A sweep of every
  function in scope with 0..4 arguments now agrees with the pin.
  `count()` (count(*)) already worked after the aggregate unit.
- **`f(*)` and DISTINCT on scalar functions** (`v16`): `f(*)` is
  `f()` (`random(*)`, `printf(*)` is NULL, `abs(*)` a wrong
  argument count) and DISTINCT is ignored (`abs(DISTINCT -1)`,
  `printf(DISTINCT '%d', 1)`), as on the pin. They were
  `Unsupported`.
- **Subqueries read only when reached** (ungated, v0.6 path): the
  executor computes subquery nodes before it evaluates an
  expression, so an error inside one surfaced even where the pin
  never runs it (`coalesce(1, (SELECT abs(-9223372036854775808)))`
  is 1 there). A subquery that fails with an evaluation error
  (`Error::Schema`) now becomes `SubValue::Fail` →
  `RExpr::Raise`, raised only if evaluation reads it. I/O,
  corruption and arena errors still fail at once. AND / OR skip
  their right side when the left side decides and the right side
  holds such a node. This matches the pin, which short-circuits
  an AND / OR whose operand has a subquery, while `a AND abs(b)`
  runs both sides there too.
- **Text functions walk text in place** (ungated): `char_ranges`
  was a list of 16-byte ranges per character (`length()` of a
  100 MB text peaked at 1.6 GB; the pin's largest legal value,
  `length(printf('%*.*s', 999999999, 5, 'x'))`, was OOM-killed).
  It is now an iterator. `length` counts, and `substr` finds its
  byte span in one walk. The same pattern in LIKE / GLOB (a
  4-byte code point per subject character) now decodes the
  subject on the fly; only the pattern (≤ 50,000 bytes) is
  decoded into a list. The 999,999,999-byte case answers in 2.2 s
  at 957 MB peak (release), like the pin.
- **Per-row DEFAULTs** (`v16`): a DEFAULT reading connection state
  (`random()`, `last_insert_rowid()`, `changes()`, ...) was
  evaluated once at prepare. Now `InsCol::per_row` keeps its
  resolved expression and evaluates it for each row:
  `b DEFAULT (last_insert_rowid())` over `VALUES (11), (12),
  (13)` is 0, 11, 12 (was 0, 0, 0). OR REPLACE, INSERT ...
  SELECT and upsert rows behave the same way, and each row draws
  its own `random()`.
- **OR FAIL** (ungated; claimed v0.5 / v0.7 paths). hipp parsed
  FAIL as ABORT, so a constraint violation rolled back the rows
  the statement had already changed. STATUS v0.7 called the two
  "observably" the same in autocommit, which is false for
  multi-row statements. `ConflictAction::Fail` now keeps them. On
  a UNIQUE / PRIMARY KEY, NOT NULL or CHECK violation, the earlier
  rows stay: committed in autocommit, or left in the open
  transaction. `changes()` counts them (the pin's 1|2, was 0|1).
  Other errors (overflow, datatype mismatch, STRICT) still undo
  the statement, as on the pin. Conflicts are found before a row
  is written, so no partial row is kept.
- **A VALUES row that fails to evaluate** (`v16`): constant rows
  are folded at prepare, so `VALUES (30, 'x'), (31,
  abs(-9223372036854775808))` failed before any row was
  inserted. On the pin, row 30 goes in first, so a failed
  statement leaves `last_insert_rowid()` at 30. Now a row that
  fails to fold makes the statement evaluate its VALUES row by row
  at step time.

**Oracle-probing notes** (all on the live 3.54.1 pin; the REAL
digit rules fitted clean-room from output: 16 000 cases through a
Python model, then 4 × 3 000 through hipp's SQL path, all
random cases with no mismatch): %e / %g rounding to fewer than 17
significant digits reads `floor(r * T)` at scale `10^(iRound - p)`
(`T` the truncated 64-bit power of ten of `lexer::pin_digits`, `p`
its binary estimate of the decimal exponent) and rounds half up at
the next digit — so an exact tie with trailing zeros rounds *down*
(`%.0e` of 450.0 is `4e+02`, of 45.0 `5e+01`; 12350.0 at three
digits rounds up, being one decade past its estimate); %f rounds
the 18 / 19 digits instead (`printf('%.3f', 166162.7745)` .775
while `%.8e` shows ...774); any `!` conversion rounding to exactly
17 digits takes REAL -> TEXT's shortening (`%!.17g` of 0.1 is
0.1, `%!.16e` too, `%!.18g` is not); `#` drops the sign of a
negative value printing as zero unless `+` / space asks
(`%#.0f` of -0.4 is `0.`, `%.0f` is `-0`); precision is capped at
100,000,000 (`%.*f` of 10^9); infinity with `0` prints `9` and
999 zeros; `printf()` / `printf(NULL)` are NULL; `printf('%c',
'')` is a NUL; `%.2q` of NULL is `(N`, `%.0J` of NULL `null`;
`%#q` doubles backslashes even without a control character;
`quote(1e16)` is `10000000000000000.0`; `char(NULL)` is a NUL,
out-of-range code points U+FFFD; `unhex('4 142', ' ')` is NULL
(separators only between pairs); `iif(0,1,0,2)` is NULL;
`likelihood(1, (0.5))` and `5e-1` pass, `+0.5`, `-0.0`, `1`,
`?` fail; `sqlite_version()` is non-deterministic for generated
columns; `INSERT INTO t SELECT last_insert_rowid() FROM s` sees
each earlier row's rowid (3, 1, 2) unless the source is the
target; changes counts per the rules above (probed one by one).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed". New suites:
`tests/functions_v16.rs` (16 tests: printf fixed cases, the JSON
/ unistr forms, REAL conversions with every rounding rule above,
a 900-format generated corpus over every storage class compared
as hex, a 700-double generated corpus through the REAL
conversions, round() and quote(), round() regressions — one
value panicked in an early fit —, TooBig, the value functions,
argument-count / unknown-function / likelihood errors at prepare,
a generated value-function corpus over a table of every storage
class, the text-function and laziness fixes, the change counters
and last-insert rowid across two scripts compared line by line
with a pin stdin session and with the `Connection` methods after
every statement, the random stream's types / ranges /
per-seed determinism, non-deterministic functions in definitions).
`hipp-cli/tests/cli_v16_functions.rs` (3 tests, byte-identical
one-shot and over stdin).

**Size after the review fixes:** tiny **75,968 B** (Δ **+8 B**
vs 75,960, ratchet raised to it). This is the overflow-chain
correctness fix on the tiny profile's INSERT path: one
`mark_frame_dirty` call in `write_overflow`. The FAIL variant
and its keep logic are gated on `v05`, where OR FAIL parses, so
the tiny insert error path is unchanged. Full **327,667 B**
(Δ **+2,216 B** vs 325,451, ratchet raised to it): deferred
subquery errors, the OR FAIL path, per-row defaults, and the
in-place text walkers. The full `ci/local-gates.sh` run ends
with "==> all gates passed". Census (now 195 probes): the 186
earlier probes are unchanged (MATCH 120, HIPP_ERR 52, BOTH_ERR
14). Of the 9 new review probes, 6 match and 3 are refused by
both engines: `coalesce(1)`, `ifnull(…3 args)`, and the OR FAIL
script, whose one-shot stops at the error on both sides.

**Size (unit as first shipped):** tiny **75,960 B** (Δ **0**: the function library is
`v16`; the counters' hooks and the round() / text fixes live in
`v16` / evaluator code the tiny profile does not build). Full
**312,051 B** (Δ **+10,672 B** vs 301,379, ratchet raised to it):
the printf engine and its digit paths, the value functions, the
connection state threaded through every evaluation context
(`Ctx` became a struct carrying `&Env`), the change-counter hooks
and the function-name table.

**Honest gaps:**

- `sqlite_source_id()` is undefined (`UnknownFunction`): it names
  the pin's source check-in, which hipp is not.
- `last_insert_rowid()` inside an INSERT ... SELECT source reads
  the pre-statement value for every row (hipp materialises the
  source first; the pin streams it and later rows see earlier
  inserts' rowids, unless the source reads the target). A
  DEFAULT reading it is per row (review fix), so only the SELECT
  list itself is affected. A VALUES list with a subquery is
  still materialised before the first insert. If one of its rows
  fails to evaluate, `last_insert_rowid()` keeps the
  pre-statement value, where the pin has the rows before it.
- An UPDATE OR REPLACE that substitutes a NOT NULL column's
  connection-state DEFAULT evaluates that default once per
  statement, not per row.
- The deferred subquery errors are evaluation errors only: an
  AND / OR short-circuits its right side only when that side
  holds a failed subquery. The pin short-circuits whenever an
  operand has a subquery, which can differ when the same side
  also raises an error of its own. The pin also folds `X AND 0`
  to 0 at prepare (`abs(-9223372036854775808) AND 0` is 0 there;
  hipp raises), an expression-folding gap outside this unit.
- OR ABORT inside an explicit transaction still rolls back the
  whole transaction (the documented v0.5 simplification); OR
  FAIL and OR ROLLBACK match the pin there. Column-level conflict
  clauses (`NOT NULL ON CONFLICT FAIL`) are still parsed and
  ignored (see "Column constraints and table definitions").
- `random()` / `randomblob()` are not the pin's ChaCha20 stream —
  types, lengths and ranges match, values do not (and the seed is
  `Config::nonce_seed`, shared with the journal nonces).
- `likelihood(X, +0.5)` is accepted: the parser folds a unary `+`
  into the literal, which the pin keeps as an operator and
  refuses.
- `unhex()`'s separator set is matched by bytes per UTF-8
  character, not by decoded code point (they differ only for
  invalid UTF-8 in Y).
- The printf result limit is the pin's default
  SQLITE_LIMIT_LENGTH; hipp has no runtime limit setting.

### Aggregate completion (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin (`/home/user/hipp-design/oracle/sqlite3`, `--version`
3.54.1; its build has `SQLITE_ENABLE_PERCENTILE`), gated as below.
Census (186 probes): on the 177 earlier probes MATCH 109 → 113
(`f_group_concat_order`, `f_string_agg`, `f_filter`, `f_median`;
HIPP_ERR 56 → 52); 9 new probes for this unit — 7 match, 2 are
errors on both (sum overflow, DISTINCT argument count). The old
binary was silently wrong on 4 of them (sum overflow, the
compensated sum, the NULL separator, `group_concat(DISTINCT x,
',')`). Folding moved to a new `src/agg.rs` (built with `v04`).

**Shipped — `v16`:**

- `string_agg(X, SEP)` (exactly two arguments; group_concat's
  fold) and `count()` (= `count(*)`).
- The aggregate ORDER BY, `f(args ORDER BY term, ...)` with ASC /
  DESC / NULLS FIRST / LAST and collations, on every aggregate:
  the stepped rows are sorted stably (ties keep input order —
  the pin's sorter carries a sequence number), so it orders
  group_concat / string_agg, and the steps of sum / total / avg
  (REAL sums are order-sensitive: `sum(x ORDER BY k)` over
  1e308, 1e308, -1e308 is Inf, descending 1e308) and of the
  percentile checks. min / max drop it (EXPLAIN shows no sorter
  on the pin). An integer term is a constant, not an ordinal.
  DISTINCT keeps the first equal value in input order, then
  sorts; when the only ORDER BY term is the argument itself
  (`query::same_expr`, the pin's `sqlite3ExprCompare`: COLLATE
  makes it differ, parentheses do not), the pin deduplicates
  through its sorter index and the *last* of equal keys survives
  (`group_concat(DISTINCT c ORDER BY c)` over NOCASE 'A', 'a' is
  `a`; `typeof(sum(DISTINCT x ORDER BY x))` over 1.0, 1 is
  integer). The pin's grammar takes ORDER BY after a possibly
  empty argument list (not after `*`); with no arguments it drops
  the clause unresolved, so `count(ORDER BY nosuch)` is
  `count(*)` and `abs(ORDER BY x)` is `abs()` (review fix: hipp
  refused `count(ORDER BY x)`).
- `FILTER (WHERE expr)` on every aggregate: a row whose filter is
  NULL or false is not stepped and nothing else of it is
  evaluated; FILTER is no reserved word (`count(*) filter` is an
  alias). A rejected row never loads min / max bare columns
  (none passing: the group's first row, as on the pin).
- FILTER and ORDER BY count for aggregate ownership in
  subqueries: `(SELECT count(*) FILTER (WHERE t.a > 1) FROM u)`
  is the outer query's (one row), `(SELECT sum(t.a) FILTER
  (WHERE u.k > 1) FROM u)` the subquery's. Aggregates inside
  them are misuse; on a non-aggregate (scalar min / max
  included) they are `SchemaKind::OrderByNonAggregate` /
  `FilterNonAggregate`. Layout: `RExpr::Agg` keeps the keys and
  the filter in `args` behind its arguments (`AggExt`), so every
  walker — column shifts, subquery collection, ownership,
  nested-aggregate misuse — sees them; `query::agg_parts` takes
  a node apart. min / max's ORDER BY keys are not kept in the
  node, but the ownership walk and the misuse check read them
  before they are dropped, as the pin's resolver does (review
  fix: `(SELECT max(o.x ORDER BY i.y) FROM i)` was the outer
  query's aggregate, collapsing it to one row, and `max(x ORDER
  BY count(*))` ran).
- `median(Y)`, `percentile(Y, P)` (P in 0 ..= 100),
  `percentile_cont(Y, P)` / `percentile_disc(Y, P)` (0 ..= 1):
  per stepped row, P is checked first (its numeric type: numeric
  text counts, a blob or NULL does not —
  `PercentileFraction`), then compared with the first row's
  fraction within 0.001 (`PercentileFractionVaries`), then a
  NULL Y is skipped, a TEXT / BLOB Y is `PercentileNotNumeric`
  (no conversion: `'3'` is refused), an infinite one
  `PercentileInfinite`; the result is the REAL at `P * (n - 1)`
  of the sorted values, linearly interpolated except for
  `percentile_disc`; no stepped value is NULL. The errors depend
  on bound values, so with `params` they are deferred to step.

**Shipped — ungated (silent wrong answers on claimed v0.4
paths):**

- `sum()` follows the pin's `sumStep`: an exact integer sum while
  every addend is INTEGER, else a Kahan–Babuška–Neumaier REAL sum
  (integers at or past 2^52 split as the pin splits them); an
  integer overflow is `SchemaKind::IntegerOverflow` unless a later
  REAL addend absorbs it (hipp returned a REAL); `total()` / `avg()`
  read the compensated REAL. `sum(0.1, 0.2, 0.3)` was
  `0.60000000000000009`, `sum(1e18, 1, -1e18)` was 0.0.
- A text addend is INTEGER only when it is wholly an integer
  (`' 5 '`, `'+7'`); any other text or blob adds its numeric
  prefix as a REAL (`sum('  12abc')` is `12.0`, hipp said 12).
- group_concat's separator is each value's own; a NULL one adds
  nothing (the whole result was NULL).
- DISTINCT with other than one argument is
  `SchemaKind::DistinctAggregateArgs` (`group_concat(DISTINCT b,
  ',')` ran), checked after the argument count as on the pin.
- Group finalization (found while probing the overflow error):
  the pin finalizes every aggregate of a group before HAVING
  decides, so an erroring one fails the statement even for a
  group HAVING drops (hipp skipped them); HAVING's AND terms
  without aggregates or subqueries are moved into WHERE there, so
  a group failing one never forms (`GROUP BY k HAVING k = 1`
  ignores the overflowing group); and a grouped query with a
  LIMIT, no DISTINCT and output in group order (no ORDER BY, or
  one naming exactly the GROUP BY terms) stops finalizing at
  OFFSET + LIMIT — so `(SELECT sum(v) FROM g GROUP BY k)` reads
  only the first group (`session::having_early`, `group_aggs`,
  `group_cap`; top-level queries and subqueries).

**Review fixes** (2026-11-02, all reproduced on the pin first):

- min / max ORDER BY ownership and nested-aggregate misuse (see
  the ORDER BY bullets above; `v16`).
- `f(ORDER BY ...)` with no arguments parses (`v16`).
- A NaN REAL is NULL, as `sqlite3VdbeMemSetDouble` stores it:
  sum / total / avg over `+inf` and `-inf` (hipp answered 0.0
  REAL, so `count(s)` over it was 1 and INSERT ... SELECT wrote a
  NaN into the file), and the operators `inf - inf`, `0 * inf`,
  `inf / inf` (`query::real_value`; ungated, the operators are
  claimed v0.4 paths).
- An outer LIMIT bounds the groups a derived table finalizes
  (ungated: a regression of the sum-overflow error). When a query
  reads exactly one derived table with no WHERE, grouping,
  aggregates, ORDER BY or DISTINCT, the pin runs the subquery as
  a coroutine and stops after OFFSET + LIMIT rows, so `SELECT x
  FROM (SELECT k x, sum(v) FROM g GROUP BY k) LIMIT 1` succeeds
  although a later group overflows; hipp passes the bound into
  the subquery's group loop, through nested derived tables
  (`session::derived_need`, `exec_subplan_need`).
- HAVING-to-WHERE follows the pin's `havingToWhere` (ungated):
  only with a GROUP BY; a term moves only if it passes
  `sqlite3ExprIsConstantOrGroupBy` — literals, operators,
  deterministic functions (`sqlite_version()` counts; `random()`,
  `changes()`, `total_changes()`, `last_insert_rowid()`,
  `randomblob()` do not) and GROUP BY terms whose collation is
  BINARY (matched as `sqlite3ExprCompare` < 2: a COLLATE on top
  of either side is ignored); and the literal 0 / FALSE never
  moves, nor does an AND the pin's parser folds to 0 (an operand
  that is the literal 0 and no function call — LIKE / GLOB
  included — on either side). So `HAVING random() = 5`, `HAVING
  0`, `HAVING k = 1 AND 0`, `HAVING bare_col = 1`, a NOCASE group
  column and a query without GROUP BY now finalize every group
  and raise the overflow as the pin does (`session::having_early`,
  `const_or_group`, `group_match`, `always_false`).

**Oracle-probing notes** (all on the live 3.54.1 pin): `EXPLAIN`
shows an ephemeral sorter with `OP_Sequence` for every aggregate
ORDER BY but min / max, and a second ephemeral (`OP_Found`) for
the first-seen DISTINCT test unless the ORDER BY is the argument
(then the sorter alone, no sequence column); `group_concat(DISTINCT
a ORDER BY b)` over 1, 2, 3, NULL, 2, '7' gives `7,1,2,3` (the v
row's 2 dropped in input order); `group_concat(x, s)` uses the
current row's separator (`ab3c` with a NULL second separator,
`b3c` when the first value is NULL); `group_concat('')` is `''`;
`percentile(a, '50')` works, `'50x'` / `x'01'` / NULL fail even
over NULL Y rows, a varying P fails at the first row past 0.001
(`50` vs `50.05` passes), `percentile(a, true)` is 1.04;
`median(-0.0)` renders `0.0`; `sum()` after an overflow: a later
1.0 or text / blob (REAL addends) absorbs it, a later INTEGER,
`'1'` or NULL does not; `sum('5e0')` is REAL, `sum(CAST(x'3500' AS
TEXT))` INTEGER 5; `select k, sum(v) ... group by k limit 1`,
`having k = 1`, `(select sum(v) ... group by k)` and `EXISTS (...)`
succeed while `having count(*) < 2`, `limit 1 offset 1`, `order by
sum(v) limit 1` and `1 IN (...)` raise the overflow; FILTER on a
min / max with no passing row leaves the bare column at the
group's first row.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed". New suites:
`tests/aggregates_v16.rs` (20 tests: string_agg / ORDER BY
corpus and refusals, DISTINCT + ORDER BY interplay, FILTER and its
refusals, FILTER with bare columns, clause ownership in
subqueries, percentile values and every percentile error kind, a
bound-P test through the Statement API, the sum overflow / KBN /
text-addend regressions, the separator and DISTINCT-argument
regressions, mixed storage classes and collations, empty input
and HAVING / ORDER BY in compound and subquery bodies, the group
finalization rules, and a 240-statement PRNG corpus of random
aggregates with random DISTINCT / ORDER BY / FILTER over mixed
storage classes — `HIPP_AGG_CORPUS` / `HIPP_AGG_SEED` scale it;
4 × 1 500 extra statements agreed while developing; the review
fixes added six: min / max ORDER BY ownership, its misuse
refusals, argument-less ORDER BY, NaN results through queries and
the file, outer LIMITs over derived tables, and a
HAVING-to-WHERE matrix of 63 statements) and `hipp-cli/tests/cli_v16_aggregates.rs`
(6 byte-identical batches, one-shot and over stdin: ORDER BY,
FILTER, percentile, sums and separators, CREATE TABLE ... AS
SELECT with its `.schema`, the review fixes).

**Size:** tiny **75,960 B** (Δ **0**: the evaluator is not in the
tiny profile). Full **325,451 B** (Δ **+10,196 B** vs 315,255,
ratchet raised to it): the percentile family, the KBN sum
context, the ORDER BY / DISTINCT sort (one hand-written stable
merge sort over row indices — two `slice::sort_by`
instantiations cost 6 KB more), FILTER and clause resolution
(`AggExt`, `same_expr`), and the finalization rules in the three
group loops.

Review fixes: tiny **76,048 B** (Δ **0**; none of the changed
code is in the tiny profile). Full **346,455 B** (Δ **+1,480 B**
vs 344,975 after the math unit, ratchet raised to it): the
HAVING-to-WHERE walkers (`const_or_group`, `has_call`), the
derived-table LIMIT bound, the NaN-to-NULL helper and the kept
min / max keys. Census: unchanged on the 205 earlier probes (135
match); 6 new probes for these fixes — 4 match, 2 are errors on
both (the misuse refusal, the HAVING `random()` overflow).

**Honest gaps:**

- Window functions (`OVER`) are not implemented (another leaf;
  since shipped as the `window` leaf — see "Window functions");
  `sum(a) OVER ()` and an aggregate with FILTER / ORDER BY plus
  OVER stay refused.
- The DISTINCT-equals-ORDER-BY test compares columns, literals,
  operators, casts, COLLATE and scalar calls; a CASE or
  subquery argument repeated in the ORDER BY takes the
  first-seen path, where the pin keeps the last of equal values.
- Group finalization: a DISTINCT grouped query, a compound core
  and a derived table the pin pushes a WHERE into (`SELECT *
  FROM (SELECT k, sum(v) ... GROUP BY k) WHERE k = 1`) finalize
  every group in hipp, so an erroring aggregate in a group the
  pin never forms fails there. The outer-LIMIT bound covers only
  a reader with no WHERE, DISTINCT, ORDER BY, grouping or join:
  the pin's coroutine also stops early under `WHERE x < 5 LIMIT
  1`, `SELECT DISTINCT ... LIMIT 1`, a UNION ALL derived table or
  a join whose outer loop is the derived table, where hipp
  finalizes every group and can raise an error the pin never
  reaches.
- HAVING-to-WHERE: hipp's parser folds FALSE, `-0` and `+0` into
  the literal 0, so `HAVING -0` / `+0` (movable on the pin) and
  an AND over FALSE (which the pin's parser does not fold) keep
  every group in hipp and can raise an error the pin skips.
  The pin's error after partial output (rows of earlier groups
  are printed before the overflow) is not reproduced: hipp
  returns the error alone.
- The argument-less ORDER BY is dropped after parsing, but hipp
  checks collation names while parsing, so `count(ORDER BY x
  COLLATE nosuch)` is refused where the pin runs it.
- When several aggregates (or rows) would raise different errors,
  which one hipp reports may differ from the pin's; the tests
  compare errored-vs-succeeded and hipp's kind, never the pin's
  text.
- Aggregate folding is still materialise-then-fold (each group's
  rows held in memory) and DISTINCT is quadratic, as before.

### Math functions (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin (`/home/user/hipp-design/oracle/sqlite3`, `--version`
3.54.1, built with SQLITE_ENABLE_MATH_FUNCTIONS over glibc 2.39's
libm), gated as below. Census (now 205 probes): on the 195
earlier probes `f_math` moved HIPP_ERR → MATCH (MATCH 126 → 127,
HIPP_ERR 52 → 51); of 10 new probes 8 match, `m_arity` is
refused by both, and `m_hyper` differs by one ulp in
`atanh(0.25)` (glibc's `log1p`, see the gaps).

**Shipped — leaf feature `math`** (new `src/math.rs`, Cargo
feature `math = ["v04"]`, in `full`, `v04,math` in
`ci/feature-sets.txt`; `ScalarFn::Math(MathFn)`):

- The SQL functions `acos acosh asin asinh atan atan2 atanh ceil
  ceiling cos cosh degrees exp floor ln log(X) log(B, X) log10
  log2 mod pi pow power radians sin sinh sqrt tan tanh trunc`,
  case-insensitive, deterministic (generated columns, CHECK,
  DEFAULT, WHERE / ORDER BY and write paths all accept them), with
  the pin's argument counts (`WrongArgumentCount` at prepare;
  `pi(*)` is `pi()`, `sin(*)` a wrong count, DISTINCT ignored,
  FILTER refused — the shared scalar-function rules).
- Argument rules (`math::eval`): an argument counts only as
  `sqlite3_value_numeric_type` makes it a number — INTEGER, REAL,
  or TEXT that is wholly a number (`record::text_to_number`:
  ASCII spaces around it, read to its first NUL; `'1e400'` is
  Inf, `'0x10'` / `'1_000'` / `'inf'` / `''` are not numbers);
  NULL, BLOB and other text give NULL. `ceil` / `floor` / `trunc`
  return an INTEGER argument unchanged (also integer text: `ceil('5')`
  is 5, `ceil('5.0')` 5.0). Every NaN result is NULL. The
  logarithms are NULL at or below zero; `log(B, X)` is NULL for B
  at or below 1 (its `ln(B) <= 0`) and reads X as
  `sqlite3_value_double` — any value's numeric prefix
  (`log(2, x'38')` and `log(2, '8abc')` are 3.0, `log(2, x'')`
  NULL) — then `ln(X) / ln(B)` in double (`log(10, 1000)` is
  2.9999999999999996, as on the pin). `degrees(x)` is
  `x * (180 / M_PI)`, `radians(x)` `x * (M_PI / 180)`.
- `hipp::math`, a public in-crate floating-point library (no
  libm exists in `no_std`; no std float math; zero dependencies):
  `sqrt` (exact: integer square root of the 108-bit scaled
  mantissa, round half even), `fmod` (exact: mantissa remainder
  ten bits at a time), `ceil` / `floor` / `trunc` (bit masks);
  `exp`, `expm1`, `ln`, `log1p`, `log2`, `pow`, `sin`, `cos`,
  `tan`, `asin`, `acos`, `atan`, `atan2` correctly rounded by
  clean-room methods: double-double arithmetic (Dekker products,
  no FMA), `exp` by `k ln 2` reduction, a 9-term Taylor series of
  `expm1(r/1024)` and ten exact squarings, `ln` by exponent
  reduction and the `2 atanh(f/(2+f))` series, trig by
  Payne–Hanek reduction against 1,472 bits of 2/π (computed
  offline with integer Machin arithmetic; the `0xa2f9836e4e44152`
  prefix checks it) then Taylor series on |r| <= π/4, `atan` by
  three angle halvings and its series; `pow` follows C99 Annex F
  and computes every exact power exactly (powers of two with
  integral `e * y`; otherwise, for `x = a * 2^e` with `a` odd and
  `y = n / 2^k` with `n` odd, `k <= 5`: `a = t^(2^k)`, `2^k | e n`
  and `t^n` within 106 bits — the only way `x^y` can be a dyadic
  rational short enough to be a halfway case — so ties round to
  even, fractional exponents included); one shared `scale_round` rounds into the
  subnormal range and to infinity. 55,000 random arguments
  checked against a 400-digit Python `Decimal` reference: 0
  misrounded.
- Fitted to the pin's output (clean room, by testing
  hypotheses against glibc results through Python's `math`,
  which links the same libm — 27,000 random calls first showed
  Python's `math` and the pin agree bit for bit): `log10` is
  `(k * lo + ln(m) / ln 10) + k * hi` with `x = m * 2^k`, `m` in
  [1, 2) (in [1/2, 1) with `k + 1` below 1) and log10(2) split
  with its low mantissa bits clear (0 mismatches over 145,000
  doubles); the hyperbolic functions are the textbook identities
  over `expm1` / `exp` / `log1p` / `ln` / `sqrt` in double
  (`sinh = ±(t + t/(t+1))/2` with `t = expm1|x|`, `2t - t²/(t+1)`
  below 1, `e^|x|/2` from 22; `cosh`, `tanh`, `asinh`, `acosh`,
  `atanh` likewise; 0–6 mismatches in 60,000 through the pin's
  own building blocks). hipp evaluates them over its correctly
  rounded building blocks. Their branch points are fitted too
  (see the gaps).

**Shipped — ungated** (silent wrong answers on the tiny-claimed
INSERT path and every comparison): `record::text_to_number` (NUMERIC
/ INTEGER / REAL column affinity, comparison affinity, the
aggregates' numeric text) trimmed Unicode whitespace and refused a
number followed by a NUL. The pin trims only `sqlite3Isspace`'s
ASCII set and reads text to its first NUL: `char(160) || '5'` and
`'5' || char(133)` stay TEXT (hipp stored 5), `'5' || char(0) ||
'x'` is 5 (hipp kept the text).

**Review fixes (second commit, same day):**

- `pow` misrounded exact halfway results with a fractional
  exponent (`pow(68661293089, 1.5)` = 262033^3, a 54-bit odd
  integer, came out ...938 for ...936; `pow(590211519630.0625,
  1.25)` and `pow(50360764242731682758656, 3.5)` likewise —
  about half of such ties). Only integer exponents were computed
  exactly; `math::pow_exact` now covers every exact case (above).
  Regressions: `pow_exact_halfway_cases_round_to_even` (1,500
  generated `pow(t^(2^k) * 2^(2^k j), n / 2^k)` cases, k = 0..5,
  each checked against an exact big-integer rounding in the test,
  and the review's three cases against the pin),
  `pow_integer_grid_is_exactly_rounded` (all 9,680 `pow(a, b)`,
  a in -60..60, b in 0..79, against the exact rounding), unit test
  `pow_exact_ties`, and CLI lines.
- Ungated (silent wrong answers on claimed paths): the numeric
  prefix reading behind arithmetic, CAST, `sum` / `total` / `avg`
  and `log(B, X)`'s X (`query::parse_num_prefix`) required the
  whole buffer to be UTF-8 and skipped Latin-1 spaces as raw
  bytes would: `log(2, x'38ff')` and `log(10, cast(x'313030ff' AS
  TEXT))` were NULL, `x'38ff' + 0` was 0 (pin: 3.0, 2.0, 8). It now
  reads bytes like the pin's `sqlite3AtoF`, skipping only the
  `sqlite3Isspace` set. The same Unicode-trim pattern sat in
  `WHERE rowid = '<text literal>'` (`'\u{a0}5'` found rowid 5; the
  pin finds none): it now goes through `record::text_to_number`.
  Regression: `numeric_prefix_reads_bytes`.
- Documentation: the mismatch figures below were understated for
  some argument families; they now list the hot spots (exact `pow`
  ties, small integer powers, `acosh` near 1, the hard trig
  reduction argument).

**Oracle-probing notes** (all on the live pin; the CLI's
`ieee754_to_blob()` gives the exact bits): `ceil(-0.5)`,
`trunc(-0.0)`, `sin(-0.0)` and `sqrt(-0.0)` are -0.0 (the CLI and
printf render it `0.0`); `sqrt('-0')` is +0 (integer text);
`ceil('abc')`, `ceil(x'31')` NULL (not 0.0); `log(0)`, `ln(0)`
NULL (C gives -inf); `pow(0, -1)` Inf, `pow(-1, 1e300)` 1.0,
`pow(2, -1075)` 0.0 (the tie rounds to even), `pow(-8, 1.0/3)` NULL;
`mod(7, 3)` is REAL 1.0, `mod(7, 0)` NULL; `atan2(-0.0, -1)` -π;
`atanh(1)` Inf, `acosh(0.5)` NULL; `exp(709.79)` and `sinh(-711)`
±Inf, `cosh(710)` finite; `sin(1e400)` NULL; `ceil(9223372036854775807)`
and `trunc('-9223372036854775808')` stay INTEGER; `ceil('9223372036854775808')`
is REAL. glibc 2.39's own accuracy (measured against hipp's
correctly rounded results, which a `Decimal` check confirmed on
every sampled disagreement): `exp`, `ln`, `log2`, `pow`, `sin`,
`cos`, `tan`, `asin`, `acos`, `atan`, `atan2` misround 0.01–0.16%
of random arguments by one ulp (`pow(x, -1)` for `x` one ulp
below a power of two among them); `expm1` 7.2% and `log1p` 2.0%
(fdlibm-style); the pin's `atan2(y, x)` for a tiny ratio is
`y / x` rounded once (a subnormal tie rounds to even, where
`atan` lies just below it — hipp now does the same).

**Mismatch rates vs the pin** (bitwise, 10^6 random arguments
per function over five distributions — random bit patterns,
[-1, 1], [-30, 30], log-uniform magnitudes, 1 ± 10^-6 — through
the same libm via a scratch harness; max difference in ulps):
`sqrt` `ceil` `floor` `trunc` `mod` (8 × 10^5) `degrees` `radians`
`pi`: **0**. `log10` 0.008% (2), `ln` 0.010% (1), `acos` 0.015%
(1), `log2` 0.026% (1), `asin` 0.034% (1), `acosh` 0.036% (1),
`atan2` 0.041% (1), `cosh` 0.045% (1), `exp` 0.048% (1), `pow`
0.066% (1), `cos` 0.072% (1), `atan` 0.076% (1), `sin` 0.10%
(1), `tan` 0.16% (1), `atanh` 1.5% (1), `tanh` 3.8% (3), `asinh`
4.5% (1), `sinh` 5.1% (2). These rates depend on the argument
distribution (the mixture above includes out-of-domain arguments,
NULL on both sides, and wide ranges); the hot spots, measured
against the live pin over 20,000 arguments each unless noted:
`acosh` 2.4% on uniform [1, 5], 1.1% on 1 + log-uniform, 0.44% on
[1, 30]; `atanh` 7.4%, `asinh` 7.9%, `sinh` 7.5% (2 ulp) and `tanh`
11.6% (3 ulp) on [-1, 1]; `ln` 0.15% and `log10` 0.10% (2 ulp) on
[0.5, 2]; `pow` 0.27% on integer a < 1000, b < 40, 0.48% (46 of
9,680) on the grid a in -60..60, b in 0..79 — mostly exact ties,
where glibc does not round half to even (`pow(9, 17)` =
16677181699666569 is 16677181699666570.0 on the pin,
16677181699666568.0 in hipp; `pow(-17, 13)` likewise; the CLI
shows these digits) — and on exact ties with fractional
exponents glibc is wrong in 89 of 145 (61%; e.g. `pow(18^16 *
2^-48, 1.0625)`). The largest difference found: `cos` and `tan`
of 6381956970095103 * 2^797 (5.319372648326541e+255, the classic
hard reduction argument) are 8 and 14 ulp from the pin — hipp's
are the correctly rounded values (a 1,300-digit `Decimal`
reduction; unit test `hard_reduction_argument`), glibc's lose bits
there; random bit patterns otherwise stay within 1 ulp (0.06–0.12%
for `sin` / `cos` / `tan`). In the test corpus against the live
pin (1,500 per function, `tests/math_v16.rs`): 0 for the exact
functions and `exp`, `ln`, `log`, `log10`, `log2`, `log(B, X)`,
`pow`; 1–4 for trig and inverse trig; `cosh` 5, `acosh` 6,
`asinh` 12, `tanh` 59, `atanh` 108, `sinh` 109 (the [-30, 30] /
[-1, 1] / [-3, 3] ranges where glibc's `expm1` / `log1p` sit).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed" (`v04,math` compiles warning-free
alone). New suites: `tests/math_v16.rs` (6 tests, 10 after the
review fixes — the two exact-`pow` suites, the byte-wise numeric
prefix and the hard trig argument vs the pin: the generated
bitwise corpus per function with its mismatch bound and ulp cap,
structural edge cases — ±0, subnormals, the largest doubles,
infinities, NaN-producing arguments, overflow / underflow
thresholds, the C99 pow / atan2 / fmod tables — bit for bit but
two one-ulp cases (`atanh(±0.5)`, glibc's `log1p(2)`), argument
coercion over every storage class for every function, arity
errors, math in generated / CHECK / DEFAULT columns and writes,
and the text → number regression); `src/math.rs` unit tests (exact
and special values, the SQL argument rules; after the review,
also `pow_exact_ties` and `hard_reduction_argument`);
`hipp-cli/tests/cli_v16_math.rs` (2 tests, byte-identical
one-shot and over stdin: special-value rendering and a generated
corpus of the exact functions).

**Size:** tiny **76,048 B** (Δ **+80 B** vs 75,968, ratchet raised
to it): the text → number correctness fix on the tiny profile's
INSERT affinity path — the NUL cut and the ASCII-only trim check
(three variants were measured; this one is the smallest; the math
library itself is not in the tiny build). Full **344,975 B** (Δ
**+17,308 B** vs 327,667, ratchet raised to it; over the 16 KiB
`justify_over`, justified here): the soft-float double-double
library on thumbv7em (9.4 KB of `math::` symbols: Payne–Hanek with
its 184-byte table, the series, `scale_round`, the integer `sqrt`),
5.8 KB inlined into `eval_scalar` (the per-function entry points)
and the u128 builtins it pulls in.

*Review fixes:* tiny **76,044 B** (Δ **−4 B** vs 76,048, ratchet
lowered: the byte-wise `parse_num_prefix` drops the whole-buffer
UTF-8 check; `literal_rowid` reuses `text_to_number`). Full
**346,207 B** (Δ **−248 B** vs the current 346,455 ratchet,
lowered: the general `pow_exact` replaces the integer-only exact
branch and the UTF-8 check goes). Gates: `cargo fmt --all` and
the full `ci/local-gates.sh` run end with "==> all gates passed".

**Honest gaps:**

- Not bit-identical everywhere: where glibc is not correctly
  rounded, hipp (which is) differs, mostly by one ulp — rates and
  hot spots above (up to 14 ulp at the hard trig argument; exact
  `pow` ties, where glibc does not round half to even, cannot be
  matched without reproducing glibc's internals);
  `sinh` / `tanh` / `asinh` / `atanh` inherit glibc's `expm1` /
  `log1p` errors (`atanh(0.5)` is ...478 on the pin, ...489 in
  hipp, the true value). Matching those would mean reproducing
  glibc's polynomial coefficients, which output fitting cannot
  recover; `tanh` can differ by up to 3 ulp through `1 - 2/(t+2)`.
- The hyperbolic branch points were fitted by sweeping the
  arguments around each (every 997th to 2^26th double over
  ±2^21 .. ±2^40 ulps): `asinh`'s 2^28 and 2, `cosh`'s ln(2)/2 and
  the `sinh` / `cosh` exp split sit at their values rounded up to
  the high 32 bits (0 mismatches after, 1,052 / 176 / 204 / 28
  before); `acosh`'s 2 and the round values 22, 1, 0.5, 2^-28,
  2^-55 compare exactly. A ±3-ulp sweep of every threshold now
  differs only where a building block (`expm1`, `log1p`, `exp`)
  does. Thresholds were not swept beyond these.
- `log10` is the fitted formula, not correctly rounded (up to
  2 ulp from the true value; it equals the pin's wherever its
  `ln` does).
- hipp's correct rounding is by double-double evaluation (about
  2^-95 relative before the final rounding), not a proof: a
  hard case within 2^-95 of a midpoint could misround; none was
  found in 55,000 `Decimal`-checked or 2.2 × 10^7 glibc-compared
  arguments (nor in the 11,180 exact `pow` cases the review fix
  checks against big-integer rounding).
- Arithmetic over text / a blob whose number is followed by
  spaces and a NUL (`x'3500' + 0`, `('5' || char(0)) * 1`): the
  pin makes the result REAL (5.0), hipp INTEGER (5). The value
  agrees; only the type differs. A pre-existing arithmetic
  quirk outside this unit, found while probing the review fix;
  not changed here.

### PRAGMA statements and sqlite_schema (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated as below. Census (211 probes; 6 more than the
math unit's run, added by the aggregate review): the 19 PRAGMA /
`sqlite_schema` probes went HIPP_ERR → MATCH but `collation_list`
(DIFF: the pin's shell registers `decimal` and `uint`); totals
MATCH 156, HIPP_ERR 33, DIFF 2 (that one and `m_hyper`), BOTH_ERR
20 (`fk_enforce`: the pin enforces the foreign key, hipp refuses
the write).

**Shipped — `v16`** (new `src/session/pragma.rs`;
`Ast::Pragma`, `Plan::Pragma`):

- Grammar: `PRAGMA [schema.]name [= value | (value)]`, the value
  a signed number (as written; `+` dropped, `-` kept), a name or
  string (dequoted), or `ON` / `DELETE` / `DEFAULT`. `main.` is
  the only schema; another name is `SchemaKind::UnknownDatabase`
  even for an unknown pragma; `temp.` refuses a known pragma
  (hipp has no temp database). An unknown name is ignored, as on
  the pin; the 39 other names of the pin's `pragma_list` are
  refused (`Error::Unsupported`) rather than ignored.
- Header values: `user_version`, `application_id` (read and
  write, inside the transaction — ROLLBACK undoes them; the
  argument through the pin's `sqlite3Atoi`: sign, `0x` hex up to
  8 digits, decimal to the first non-digit, overflow 0),
  `schema_version` (writes are no-ops, as in the pin's defensive
  shell), `page_size` (a write re-creates a pristine file — this
  open created it and nothing has committed — with the new size;
  otherwise, or for an invalid size, nothing happens, as on the
  pin), `page_count` (0 for a pristine file, like the pin's empty
  one), `freelist_count`, `encoding` (`UTF-8`; UTF-16 names on a
  pristine file are `UnsupportedEncoding`, unknown names
  `SchemaKind::UnknownEncoding`; ignored once the file has
  content), `auto_vacuum` (the header's mode; a change the pin
  would make is refused — hipp keeps no pointer maps).
- What hipp does, reported and never pretended: `journal_mode`
  is `delete` (a mode change other than to `delete` is refused;
  an unknown mode name queries, as on the pin), `synchronous` is
  2 (FULL: hipp syncs the journal before any page moves and the
  file before the journal goes; other levels refused),
  `cache_size` is the page pool (`Config::cache_pages`; a
  different size refused), `busy_timeout` is stored and returned
  (hipp takes no locks, so it never waits).
- Flags: `foreign_keys` (a no-op inside a transaction, as on the
  pin; while ON, INSERT / UPDATE / DELETE / DROP TABLE on a table
  that is a child or parent in any foreign key is refused —
  hipp does not enforce them), `recursive_triggers`,
  `ignore_check_constraints` (skips CHECK on INSERT / UPDATE /
  upsert / ALTER ADD COLUMN and in the integrity check),
  `case_sensitive_like` (LIKE and `like()` compare ASCII letters
  exactly while on, CHECK constraints included; no getter, as on
  the pin). Words via the pin's `getSafetyLevel` (on / yes /
  true / off / no / false; a number is true unless 0 — `256` is
  true; `full` / `extra` / other words false).
- Introspection, byte for byte: `table_info` / `table_xinfo`
  (re-parsing the stored CREATE text: declared type, NOT NULL,
  the DEFAULT text as written with `(...)` unwrapped, PRIMARY
  KEY position, hidden 2 / 3 for VIRTUAL / STORED; table_info
  skips generated columns and numbers the rest; views get the
  pin's `sqlite3SubqueryColumnTypes` types — a column reference
  keeps its declared type when the expression's affinity agrees,
  else the affinity's name, `NUM` for NUMERIC, none for an
  expression without affinity, compound demotion to BLOB;
  `sqlite_schema` and the temp names give the five schema
  columns), `index_list` (newest first; origin c / u / pk;
  partial when the stored SQL has WHERE), `index_info` /
  `index_xinfo` (the alias column for a rowid key, DESC,
  collation names, the trailing rowid row; an index hipp keeps
  opaque is refused), `foreign_key_list` (the parser now keeps
  FOREIGN KEY / REFERENCES clauses: ids newest first, NO ACTION
  / RESTRICT / SET NULL / SET DEFAULT / CASCADE, `MATCH` always
  `NONE`, ON INSERT dropped), `table_list` (tables and views, the
  schema table as `sqlite_schema`, then `temp|sqlite_temp_schema`
  unless `main.` was written; STRICT flag; an argument matches
  the pin's internal names `sqlite_master` / `sqlite_temp_master`),
  `database_list` (`0|main|<absolute path>`: a new defaulted
  `Storage::file_name`, the canonical path for `FileStorage`),
  `collation_list` (BINARY, NOCASE, RTRIM).
- `integrity_check` / `quick_check` (`[N]` a limit — the
  pin reads the token's digits, so `(-5)` is 5 and `(2.9)` 2 —
  or a table name; unknown names `UnknownTable`): first the
  pin's B-tree check over the freelist, every root (each table's,
  then its indexes', in the schema order below) and the unused
  pages, with its message shapes — `Page N: never used`, `2nd
  reference to page N`, `invalid page number N`, `Freelist: size
  is X but should be Y`, `freelist leaf count too big on page N`,
  `Tree R page P cell C: Rowid X out of order`, `Offset X out of
  range A..B`, `Extends off end of page`, `Bad cell header size`,
  `overflow list length is X but should be Y`, `Child page depth
  differs`, `Tree R page P: btreeInitPage() returns error code
  11`, `free space corruption`, `Multiple uses for byte B of page
  P`, `Fragmentation of X bytes reported as Y on page P` — joined
  into one value under `*** in database main ***`; then `wrong #
  of entries in index I` for every table, then per row (the
  pin's scan ordinal, not the rowid) `NULL value in T.C`,
  `non-TYPE value in T.C` (STRICT), `CHECK constraint failed in
  T`, `row N missing from index I`, `non-unique entry in index
  I` (the full check only); the limit counts every message. A
  corrupt page met while reading rows is returned as the error
  after the rows already produced, as the pin does; a table with
  nothing to check per row is only walked, not decoded.
- `sqlite_schema` / `sqlite_master` (any case, `main.` allowed)
  read as an ordinary table (`type name tbl_name rootpage sql`,
  root page 1: scans, joins, subqueries, INSERT ... SELECT, CTAS,
  views); INSERT / UPDATE / DELETE / DROP / ALTER / CREATE INDEX
  on it are `SchemaKind::SchemaTableReadOnly`; `sqlite_temp_schema`
  / `sqlite_temp_master` read as empty.
- Table-valued functions in FROM: `pragma_<name>(args)` for the
  introspection and value pragmas (`pragma_table_info('t')`,
  `pragma_index_list`, `pragma_user_version`, ...; a second
  argument `main` is the schema, another gives no rows; too many
  arguments `WrongArgumentCount`), named by the pin's column
  names, qualifiable by the function name or an alias; planned
  as a derived table of the rows.

**Shipped — ungated** (silent wrong answers on claimed CREATE
paths): CREATE TABLE / INDEX / VIEW of a `sqlite_*` name (any
case) is `SchemaKind::ReservedName`, as on the pin — hipp
created such tables (files that already hold one still load).
And, found while probing table_info over views (v0.4 view
column names): a view's rowid column is named by the table's
rowid-alias column (`e`, deduplicated `e:1`) or `rowid` for any
spelling (`oid` → `rowid:1`), and a VALUES view has columns
`column1`, ... (it had none).

**Oracle-probing notes** (live pin): the pin's PRAGMA answers
through its shell — `collation_list` shows the shell's `decimal`
and `uint` too; `schema_version` writes are ignored (defensive
mode). Value text conversions as above (`' 5'` is 0, `'5x'` 5,
`1e3` 1, `'0x10'` 16, `2147483648` 0); `user_version = NULL` /
`1+1` / `(1)` / `x'01'` / `1_000` are syntax errors, `ON` /
`DELETE` / `DEFAULT` / `"7"` / `[7]` are values. Writes return no
row except `busy_timeout` and `journal_mode`; `page_count = 5`
and `freelist_count = 5` return the count. `table_list` and the
integrity check walk the schema hash table: for up to three
tables and views it is newest first, then `sqlite_schema`;
larger schemas rehash (at the fifth entry; the pin's hash
function did not fit `strHash` modulo any table size, so it was
not reproduced). index_list is newest first (autoindexes in
reverse constraint order). The integrity limit counts each
B-tree message and each row; quick_check also reports `wrong #
of entries`; the page check counts a leaf's cells even when one
is damaged; `row N` is the scan ordinal; a table with only a
damaged overflow chain or cell pointer gives no scan error
(the pin reads record headers only), a bad page type or child
does. Corruptions were built by editing oracle-written files
(Python's `sqlite3` with `writable_schema` for the probes; byte
edits in the tests).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh`
run end with "==> all gates passed". New suites:
`tests/pragma_v16.rs` (20 tests: grammar and error-vs-error
agreement, refusals, generated header-value and flag-word
corpora, case_sensitive_like over a generated LIKE corpus,
ignore_check_constraints on every write path, the foreign-key
refusal, a generated table / index / foreign-key introspection
corpus plus fixed cases and view types, table_list contents,
database / collation lists, the integrity check on 24 corrupted
files and a clean multi-level one — page-level, multi-level
trees, schema-edited NOT NULL / CHECK / STRICT / UNIQUE /
generated — with every limit form,
sqlite_schema reads and write refusals, reserved names, the
table-valued functions, and the view-rowid regression);
`hipp-cli/tests/cli_v16_pragma.rs` (4 tests, byte-identical
one-shot and over stdin, integrity messages included).

**Size:** tiny **76,068 B** (Δ **+24 B** vs 76,044, ratchet raised
to it): the reserved-name check on the tiny profile's CREATE
TABLE path, the correctness fix above (the PRAGMA code is not in
the tiny build; the view-naming fixes are gated to `v04`, where
views are used). Full **379,991 B** (Δ **+33,784 B** vs 346,207,
ratchet raised to it; over the 16 KiB `justify_over`, justified
here): the PRAGMA executor and rows (`run_pragma` 7.7 KB,
`pragma_rows` 5.2 KB on thumbv7em), the integrity checker's page
walk and row checks (inlined into `Statement::step`), view type
derivation, the FOREIGN KEY / DEFAULT-text capture in the
parser, and the table-valued-function planning.

**Review fixes (2026-11-02).** All fifteen findings reproduced
on the live pin and fixed but the out-of-reach parts noted in the
gaps; each has a regression test (`tests/pragma_v16.rs`
`regression_*`, `hipp-cli/tests/cli_v16_pragma.rs`
`cli_v16_pragma_review_*`).

- Ungated (claimed CREATE TABLE / INSERT paths):
  - Declared types are kept as the pin's `sqlite3AddColumn`
    keeps them (`record::decl_token`: `sqlite3DequoteToken`;
    `record::decl_type_q`: then `sqlite3Dequote`). table_info /
    table_xinfo / view types show `INT`, `TEXT`, `my type`,
    `x] TEX`, `te"xt`, `a` (for `"a" "b"`), `int` (for
    `"int"(3)`); affinity reads the dequoted form (`[x] TEXT` is
    NUMERIC, `""` NUMERIC); the rowid-alias rule and STRICT read
    the token form (`"INTEGER" PRIMARY KEY` and `[integer]` are
    the alias — hipp wrote an autoindex the pin calls orphan);
    CAST reads `sqlite3Dequote` alone (`CAST(x AS [a] CHARX)` is
    NUMERIC).
  - A table-level `PRIMARY KEY (a DESC)` on an INTEGER column is
    the rowid alias (the pin passes no sort order there; only
    the column constraint's DESC is the quirk) — hipp wrote an
    orphan autoindex.
  - FOREIGN KEY column lists are validated as the pin's
    `sqlite3CreateForeignKey` does (`SchemaKind::ForeignKeyColumns`,
    new: a column's REFERENCES names at most one parent column,
    a table key as many as it lists; an unknown child column,
    `rowid` included, is `UnknownColumn`), and so are PRIMARY
    KEY / UNIQUE lists (`no such column`; with the `indexes`
    feature, where constraint sets live — the tiny profile keeps
    none) — hipp wrote schemas the pin could not open.
  - `name(args)` in FROM naming a table or view is
    `UnknownFunction` (the pin's "'t' is not a function"); the
    arguments were ignored.
- `v16`:
  - `PRAGMA page_size` (and `encoding`, `auto_vacuum`) on an
    empty file is fixed once a write transaction has begun, even
    rolled back (the pin's `BTS_PAGESIZE_FIXED`;
    `Pager::layout_open`); `BEGIN IMMEDIATE` / `EXCLUSIVE` (a
    new `Ast::BeginImmediate`) count, as they take the write lock.
  - The integrity check: no `ok` before a deferred error; partial
    indexes are left out of `wrong # of entries` (as on the pin);
    the full check of a table with an expression or partial index
    (hipp keeps such indexes opaque) is refused before any output,
    the quick check answers. The row phase makes the pin's
    `OP_Column` record-header test on each row's right-most
    stored column and answers a failure with one row, `database
    disk image is malformed`, stopping (as the pin's
    `op_column_corrupt` handler does; probed over every
    header-size class); `Bad cell header size` is the pin's
    first-byte test (0 or 1). The index probe for `row N missing
    from index` / `non-unique entry` follows the pin's seek: its
    binary search per page (also used by `IndexCursor::seek`
    now), its raw-record comparison (`xRecordCompare` with the
    integer / BINARY-text fast paths, a field's body read only
    when its type needs it, corruption reported only when the
    comparison then reads as equal), and only the cells it
    visits decoded. Auto-vacuum files get the pin's header test
    (`max rootpage (X) disagrees with header (Y)`, or `incremental
    _vacuum enabled with a max rootpage of zero` without
    auto-vacuum) and pointer-map checks (`Bad ptr map entry key=K
    expected=(T,P) got=(T,P)`, `Failed to read ptrmap key=K`,
    `Page N: pointer map referenced`, with the pin's prefixes —
    `Tree R page P right child:` for the rest of an interior
    page). After `Multiple uses for byte`, the fragmentation test
    still runs when the overlap was the heap's last entry.
    `integrity_check(N)` with N outside 32 bits names a table.
    On deterministic corpora of single-byte edits of oracle files
    (1,500 of the review's index-heavy 512-byte-page file, 900 of
    an auto-vacuum one), hipp's answer differs from the pin's in
    12 and 2, every one a hipp error where the pin reads a cell
    lying off its page (11) or reads only the columns it checks
    past a broken overflow pointer (3) — see the gaps; the
    review's sample went from about 20 in 200 to 1 in 200.
  - A header claiming more pages than the file holds fails to
    open, as on the pin (it was read as empty pages); the
    encoding field's low two bits are read (0, unset, is UTF-8:
    the pin's file after only `PRAGMA user_version = N` opens),
    and the first schema change on such a file sets its schema
    format (4) and encoding (1), as the pin's CREATE TABLE does.
  - `case_sensitive_like` reaches generated columns
    (`GenCols::case_like`, captured at planning); once set at all,
    LIKE / `like()` in a generated column is `NonDeterministic`
    on CREATE TABLE and ALTER TABLE ADD COLUMN (which re-checks the
    whole table), as the pin re-registers `like()`.
  - `table_list` counts a view's planned columns (0 only when it
    cannot be planned, as on the pin).
  - Table-valued pragmas take the pin's hidden columns only:
    `arg` for a name pragma, `schema` for table_info & co. and
    `auto_vacuum` / `cache_size` / `journal_mode` / `page_count` /
    `page_size` / `synchronous`; more arguments are
    `WrongArgumentCount` ("max N"), a schema other than main /
    temp `UnknownDatabase`; temp gives no objects, and its header
    values are refused. A user table or view of the name shadows
    the function (with arguments: not a function).
  - `sqlite_master` and `sqlite_schema` both qualify the
    unaliased main schema table (`SourceDesc::named`).
- Not a hipp bug any more at HEAD: the view named
  `pragma_user_version` (finding 2) — views execute since the
  views unit, which reads the view first; regression-tested.

**Gates (review):** `cargo fmt --all`, then the full
`ci/local-gates.sh` ends with "==> all gates passed".
`tests/pragma_v16.rs` now holds 35 tests (16 review regressions:
generated declared-type and damaged-file corpora among them),
`hipp-cli/tests/cli_v16_pragma.rs` 6. Census unchanged (211
probes: MATCH 160, HIPP_ERR 29, DIFF 2, BOTH_ERR 20).

**Size after the review:** tiny **76,324 B** (Δ **+208 B** vs
76,116, ratchet raised to it; the unit's total is +232 B, within
ADR 0001's +256 B for correctness fixes on claimed paths): the
FOREIGN KEY column-list checks (+~160 B: the shared
`known_columns` loop and the arity test) and the declared-type
dequoting behind affinity and the rowid alias (+~50 B). Moved
behind `v16` to stay inside the allowance: the open-path header
fixes (empty-file encoding, page count beyond the file, schema
format on first DDL) and the PRIMARY KEY / UNIQUE column check
(with `indexes`). Full **404,128 B** (Δ **+8,459 B** vs 395,669,
ratchet raised): the pointer-map and record-header checks, the
pin-faithful index probe and record comparison, the
table-valued arity rules, and the generated-column LIKE plumbing.

**Honest gaps:**

- Schema hash order (`table_list`, the integrity check's table
  order) is reproduced for up to three tables and views only;
  beyond that hipp stays newest first (rows equal as sets).
- `collation_list` matches the pin's library (three built-ins),
  not its shell, whose `decimal` / `uint` collations hipp-cli
  lacks. `function_list` and the other 38 refused names have no
  principled hipp answer yet (`function_list`'s flag bits mirror
  the pin's internals).
- Table-valued pragmas take constant arguments only (the
  correlated `JOIN pragma_table_info(m.name)` idiom is refused),
  expose no hidden `arg` / `schema` columns (`WHERE arg = 't'`
  is unsupported), and `pragma_integrity_check` /
  `pragma_quick_check` are refused in FROM.
- No temp database: `temp.` pragmas, and header values of temp
  through the table-valued forms, are refused.
- Foreign keys are refused under `foreign_keys = ON`, not
  enforced (ADR 0001's `foreign_keys` leaf is the place for
  that); `recursive_triggers` is stored with no triggers to
  apply it to.
- `case_sensitive_like` and schema loading: the pin loads the
  schema lazily, so after `PRAGMA case_sensitive_like` as a new
  connection's first statement it refuses a file that already
  holds a LIKE generated column ("malformed database schema");
  hipp loads the schema at open and reads such a table. The flag
  is captured when a statement is planned (a statement prepared
  before the PRAGMA keeps the old setting).
- The integrity check on damaged files: a cell whose offset or
  size lies off its page makes hipp stop with a corruption error
  where the pin reads past the page into its buffer and carries
  on (its answer depends on that memory); a row whose overflow
  chain is broken stops hipp, which assembles whole rows, where
  the pin reads only the columns it checks; the expression /
  partial index full check is refused (above); index records are
  compared as the pin compares them, but their headers are not
  run through `OP_Column`'s test. A file whose schema names an
  out-of-range root page opens in hipp (the pin refuses it as
  malformed at open); a schema format above 4 opens in hipp too
  (the pin's "unsupported file format").
- PRIMARY KEY / UNIQUE column lists are not validated in the
  tiny profile (it keeps no constraint indexes; such a table
  already lacks the pin's autoindex there).
- View column types: a scalar subquery item's declared type is
  not carried (its affinity is). Three-part column names
  (`main.sqlite_schema.name`) do not parse (hipp-wide).
- `CREATE TABLE x AS SELECT * FROM sqlite_schema`: the pin's
  SELECT also sees its own all-NULL placeholder schema row; hipp's
  does not.
- Found outside this unit while writing the CLI suite: hipp's
  expression grammar reads `notnull` as a column name where the
  pin's treats it as the postfix NOTNULL operator.

### Executed views and common table expressions (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated as below. Census (211 probes): `view_exec`,
`cte`, `cte_rec`, `cte_multi` went HIPP_ERR → MATCH; totals MATCH
160, HIPP_ERR 29, DIFF 2, BOTH_ERR 20.

**Shipped — `v16`** (new `src/session/cte.rs`; `Ast::With`,
`SelectStmt::with`, `FromSource::{qualified, cte}`,
`SubPlan::rec`, `SubSource::rec`):

- *Expansion before planning* (`Connection::expand_ast`): every
  FROM source naming a view or an in-scope CTE becomes a derived
  table holding a copy of its SELECT, so views and CTEs are read
  wherever a FROM subquery is — joins (NATURAL / USING / RIGHT /
  FULL), subqueries, `x IN name`, compound members, view-on-view,
  UPDATE ... FROM, INSERT ... SELECT, CREATE TABLE ... AS,
  RETURNING and upsert expressions. Name lookup follows the pin's
  `selectExpander` / `withExpand`: unqualified names search the
  visible WITH clauses innermost first (every CTE of one clause
  sees the others, forward references included; a CTE body sees
  only its own clause and those around it), then tables, then
  views; `main.x` is never a CTE; a view body sees none of the
  reader's CTEs; a DML target is never a CTE. A view or CTE
  without an alias is qualified by its name.
- *Views:* `CREATE VIEW v(a, b)` names the columns (the length is
  checked when the view is read: `ColumnCountMismatch`); CREATE
  VIEW resolves nothing (a missing table or column is the reading
  statement's error, as on the pin); a cycle is
  `SchemaKind::CircularView`; INSERT into a view is `IsView`
  (UPDATE / DELETE / DROP TABLE already were). A view's
  unaliased column references are named by their tables' own
  columns (`SELECT rowid FROM t` names the INTEGER PRIMARY KEY);
  a CTE's and a FROM subquery's keep the spelling — the pin
  resolves only a view's SELECT before naming its columns.
  `Connection::view_columns` gives the planned names (a column
  list of any length as written); PRAGMA `table_info` of a view
  and the CLI's `.schema` comment use them (no comment for a view
  that cannot be read, the view name quoted when needed), and the
  view type derivation now expands views and CTEs through
  `expand_view` (the PRAGMA unit's `inline_views` is gone), looks
  through derived columns to their items' affinities, and works
  for views over derived tables.
- *WITH:* `WITH [RECURSIVE] name [(cols)] AS [[NOT] MATERIALIZED]
  (select), ...` before SELECT / VALUES (any select statement,
  so also inside subqueries, derived tables, INSERT ... SELECT,
  CREATE TABLE / VIEW ... AS) and before INSERT / REPLACE /
  UPDATE / DELETE (`Ast::With`, unwrapped by the expansion);
  `DuplicateCteName` (checked used or not), column lists
  (`ColumnCountMismatch` on use), the MATERIALIZED / NOT
  MATERIALIZED hints (honoured since the review fixes below).
  Bound parameters inside CTE bodies bind (`sql::bind`).
- *WITH RECURSIVE* (the pin's `withExpand` +
  `generateWithRecursiveQuery`): a CTE that is a UNION / UNION
  ALL compound whose rightmost cores (those joined by the
  rightmost operator) name it in their own FROM clause is
  recursive; those cores are the steps, the cores before them
  the setup (itself any compound). `CircularReference` for any
  other self-reference (setup, non-UNION compound, subquery-only
  reference, mutual CTE cycles), `RecursiveReferences` for two
  references in one step or one inside a step's subquery /
  the CTE's LIMIT, `RecursiveAggregate` for an aggregate or
  GROUP BY step. Execution (`Statement::exec_recursive`):
  LIMIT / OFFSET evaluated first (`LIMIT 0` runs nothing); setup
  rows enter the queue; each row taken from the queue is output
  unless OFFSET skips it, the run stops once LIMIT (or the
  reader's `need`, so `SELECT ... FROM c LIMIT 3` over an endless
  CTE terminates) is reached, else the row becomes the one row of
  the recursive reference (`SubSource::rec`, read from
  `Statement::rec_rows`) for every step, whose rows join the
  queue. FIFO without ORDER BY; with one, a binary heap on (the
  ORDER BY key resolved by the compound rules, arrival sequence).
  UNION drops every row equal (under the compound's column
  collations) to one that entered before — setup rows included.
  The recursive table's column affinities are the CTE's compound
  ones (`rules::compound_affinity`): the steps are planned with
  the setup's and re-planned once if that demotes a column.
  Steps are never hoisted as uncorrelated; a recursive CTE inside
  a correlated subquery runs per outer row.

**Shipped — ungated** (silent wrong answers on claimed paths):

- *One schema namespace* (tiny path): CREATE TABLE over a view
  of its name succeeded (DuplicateTable now; IF NOT EXISTS a
  no-op), over an index of its name too (`DuplicateIndex`, even
  under IF NOT EXISTS); CREATE INDEX over a view's name
  succeeded and over a table's name was a silent no-op under IF
  NOT EXISTS (`DuplicateTable` now, as on the pin). One
  `Catalog::taken` scan.
- *Same-named sources* (v0.4): `SELECT t.a FROM t, t`, `t.*` /
  `*` over two sources named `t` (or two derived tables aliased
  alike, or a view twice) read the first source; the pin reports
  "ambiguous column name". A qualified name now counts every
  source of that name like an unqualified one (a shared USING
  column stays resolvable), and `*` over several sources checks
  each `q.col` it expands to.
- *Derived compound affinity* (v0.9): a FROM subquery that is a
  compound took its leftmost core's column affinity; the pin
  demotes it (`sqlite3SubqueryColumnTypes`): TEXT with a numeric
  item in another core, or numeric with a text one, become none;
  a numeric CAST becomes NUMERIC. Shared with CTAS and the PRAGMA
  type derivation (`rules::compound_affinity`).
- *Declared types in PRAGMA table_info* (v16, PRAGMA unit): a
  type that is one of the pin's standard names (INT, INTEGER,
  REAL, TEXT, BLOB, ANY, any case) prints in its spelling
  (`integer` → `INTEGER`), for tables and views.

**Oracle-probing notes** (live pin): CREATE VIEW over a missing
table / column, with a column list of the wrong length, or
reading itself all succeed; the reading statement fails
("expected 2 columns for 'v' but got 1", "view v is circularly
defined", "no such table: main.t"). A view's PRAGMA table_info
and `.schema` comment use its column list whatever its length
(types then empty); an unreadable view has no comment.
`table_list` reports a view's column count from the connection's
cached state (0 for a view not yet read in a fresh connection,
the list's length after CREATE VIEW in the same one) — hipp keeps
the catalog's count. CTEs: forward references work (`WITH x AS
(SELECT * FROM y), y AS (...)`), unused CTEs are never checked
(`WITH x(a) AS (SELECT 1, 2) SELECT 5` is 5) except for duplicate
names; `WITH t AS (...) INSERT INTO t ...` writes the real t,
`main.t` reads it; RECURSIVE is optional. Recursive queue:
`(1,0) ∪ children 2n, 2n+1 ORDER BY 1 DESC` gives 1 3 7 6 2 5 4
(a max-heap); `SELECT 5 UNION ALL SELECT 3 ... ORDER BY 1` gives
3 2 1 0 5 4 3 2 1 0; ties leave in arrival order; `LIMIT 4 OFFSET
2` gives 3 4 5 6 (skipped rows still recurse); `LIMIT 0` gives
nothing; the CTE's ORDER BY names the leftmost core's result
columns, not the CTE's column list (`ORDER BY n` over `c(n) AS
(SELECT 1 ...)` is "does not match any column"). Several
recursive steps run per row in order; a step reference on the
right of a LEFT JOIN is accepted. A text setup column with a
numeric step (`SELECT a FROM t UNION ALL SELECT x + 1 ...`) has
no affinity: `x < 3` is false for `'1'`. Derived compounds of
mixed affinity: a WHERE term over them is planner-dependent on
the pin — `WHERE x > 1` and `x <> 2` evaluate per core (each
core's own affinity) while `x = 2`, `x < 3`, `IS`, BETWEEN and
the result list use the demoted (none) affinity; hipp always
uses the demoted affinity (gap below). `SELECT rowid FROM t` in a
FROM subquery or CTE is a column named `rowid`, in a view the
INTEGER PRIMARY KEY's name.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh`
run end with "==> all gates passed". New suites:
`tests/views_cte_v16.rs` (16 tests: views read everywhere and in
writes, view errors, both-way interoperability with oracle-written
views — CTE and compound bodies included —, view table_info,
CTEs before SELECT and writes with the files checked by the pin,
the CTE error matrix, the recursive queue semantics corpus,
recursive self-joins compared as multisets, a generated recursive
corpus of 120 CTEs — `HIPP_CTE_CORPUS` scales it; 3 000 ran clean
—, a generated view / CTE corpus over a random table, bound
parameters in CTEs, and one regression test per ungated fix);
`hipp-cli/tests/cli_v16_views_cte.rs` (4 tests: views, CTEs and a
generated recursive batch byte-identical one-shot and over stdin,
and the `.schema` view comments over stdin). `tests/query.rs` and
`tests/subqueries.rs` assert the executed view where they
asserted the inert one (under `v16`; unchanged without it).

**Size:** tiny **76,116 B** (Δ **+48 B** vs 76,068, ratchet
raised to it; justified: the shared-namespace check on the tiny
profile's CREATE TABLE path — one `Catalog::taken` scan over
indexes, tables and views — a correctness fix). Full **395,669 B**
(Δ **+15,678 B** vs 379,991, ratchet raised to it; under the
16 KiB `justify_over`): the expander, the recursive planner and
queue executor, the WITH grammar, the namespace / ambiguity /
affinity fixes and the view column naming.

**Honest gaps:**

- (Updated by the review fixes below.) An endless recursive CTE
  stops at its own LIMIT or at a bounded reader that reads it as
  its first source; one read second in a join, or under an
  ORDER BY / GROUP BY / DISTINCT / aggregate, never ends — as on
  the pin. A view or CTE that does not share a materialization is
  computed per reference (and per outer row when correlated); no
  index helps a WHERE over one.
- A WHERE comparison over a view / CTE / FROM subquery that is a
  compound of mixed column affinity: hipp uses the demoted
  affinity everywhere; the pin pushes some operators (`>`, `>=`,
  `<>`, ...) into each core and compares with that core's
  affinity (planner-dependent; the generated corpus avoids it).
- The CTE's ORDER BY matches result columns by name or ordinal
  only, not by expression (`ORDER BY x + 1` matching a step's
  `x + 1` item is "no such column" in hipp; the pin accepts it) —
  a pre-existing limit of hipp's compound ORDER BY.
- `PRAGMA table_list`'s view column count is the catalog's
  (the pin's depends on whether the connection has read the
  view). Triggers (INSTEAD OF on views) are not supported.
- Found outside this unit: hipp's parser accepts keywords as
  bare names in a CREATE VIEW column list (`v(select)`), which
  the pin rejects.

**Review fixes (2026-11-03)** — all behind `v16` except where
marked; each has a regression test in `tests/views_cte_v16.rs`
(and `hipp-cli/tests/cli_v16_views_cte.rs` for the rendered ones):

- *Shared materializations* (high): every reference pasted a
  fresh copy of the CTE body, so a random() body gave each
  reference its own rows (`WITH c AS (SELECT abs(random()) % 1e5
  r) SELECT count(*) FROM c WHERE r IN (SELECT r FROM c)` was 0,
  1 on the pin), and the hints were thrown away. Probed (results
  and EXPLAIN QUERY PLAN): the pin flattens a reference into its
  reader when its query flattener can, and otherwise
  materializes; all references of one CTE that are not flattened
  share one materialization, computed once per statement (even
  inside a correlated subquery), when the CTE is MATERIALIZED or
  has no hint and was expanded twice or more; a single use runs
  as a co-routine (per execution). A view named twice in one FROM
  clause — or brought into it by a flattened view — shares one
  (`isSelfJoinView`) unless flattened or a WHERE / inner-ON term
  that reads only it is pushed into it. Recursive CTEs are never
  flattened. The expansion now counts each CTE's references and
  models the flattener's restrictions that hold before planning
  (`session::cte::flattens`: no FROM, aggregate, DISTINCT, OFFSET,
  LIMIT under a join / aggregate / LIMIT / WHERE / DISTINCT /
  compound reader, ORDER BY under an ordered or aggregate reader
  or a complex-result first source, compound rules 17a–f, the
  right side of a LEFT / FULL / RIGHT join; a scalar or EXISTS
  subquery counts as LIMITed, as the pin adds its LIMIT before
  flattening). A second walk settles `CteUse::mat`; the planner
  copies it to `SubPlan::mat` for an uncorrelated plan and the
  executor serves every reader of one id from
  `Statement::mats` (cleared per execution; a recursive CTE's
  entry may hold only a bounded reader's prefix and is extended
  when a later reader needs more).
- *RETURNING* (ungated v0.7 fix, low): a DELETE's RETURNING ran
  before the row's delete (`DELETE ... WHERE a = 2 RETURNING
  (SELECT count(*) FROM t)` was 3, 2 on the pin), and an
  uncorrelated subquery was hoisted across the rows (`UPDATE t SET
  b = a * 10 RETURNING (SELECT sum(b) FROM t)` repeated 10; the
  pin gives 10, 30, 60). The pin runs RETURNING after each row's
  change as a trigger program: its subqueries run afresh per row
  (`eval_returning` sets the hoist cache aside), but every FROM
  subquery, view or CTE inside it is materialized once for the
  statement (probed: `RETURNING (SELECT group_concat(x) FROM
  (SELECT b x FROM t))` repeats the first row's value), which the
  expansion marks (`Expander::returning`).
- *Endless recursive CTEs* (medium): `SELECT x FROM c WHERE x > 5
  LIMIT 2` over an endless CTE ran forever and grew without
  bound; the pin reads a first FROM source as a co-routine and
  stops. A query (top level or subquery) whose first source is a
  recursive CTE (directly or through plain cores) and that has no
  grouping, aggregate, ORDER BY, DISTINCT or RIGHT / FULL join
  now reads it with a doubling row bound until its LIMIT is met
  or the CTE runs out (`streams_first`, `need_bounds`); a scalar
  or EXISTS subquery reads one row (the pin's implicit LIMIT 1).
  A CTE shared by two readers is materialized whole, as on the
  pin (so an endless one never ends on either).
- *Nesting limits* (medium): the expansion refused views or CTEs
  nested 63 deep (and FROM subqueries 64 deep, a regression of
  this unit). Probed: the pin reads 999 nested views or CTEs and
  refuses 1000 ("VIEWs and/or subqueries nested too deep"); it
  also refuses a statement once the SELECTs copied by CTE
  references reach 1000 — side-by-side references and views
  expanded earlier included (a chain of 500 CTEs whose bodies hold
  a FROM subquery reads, 501 does not; 1100 `(SELECT x FROM v)`
  then one CTE reference does not) — so a CTE whose references
  double per level fails at once instead of growing as 2^n
  (low finding). hipp: `SchemaKind::NestedTooDeep` for both
  (`MAX_COPIES` 1000, counted at each CTE reference), and the
  nesting bound is the new `Config::max_nesting`: hipp's planner
  recurses per level (~9 KiB of stack in release, ~30 KiB in
  debug, measured), so the default is 250 (an 8 MiB thread),
  `Config::tiny()` 16; the CLI sets the pin's 1000, runs on a
  512 MiB-stack thread and gives the statement arena 4 MiB (999
  nested views need ~70 KiB of it). FROM subqueries are no longer
  counted.
- *CREATE VIEW IF NOT EXISTS over an index's name* (low, v0.4
  path) did nothing; it is `DuplicateIndex`, as on the pin. Over
  a table or view of the name it stays a no-op.
- *`t.rowid` over two sources named `t` with a rowid* (low, v0.4
  path) read the first; it is `AmbiguousColumn` (a derived `t`
  without a rowid column does not count; one with a real `rowid`
  column wins, as on the pin).
- *Quoted declared types in PRAGMA table_info* (low): already
  fixed by the PRAGMA unit's review (`shown_type` dequotes as
  `sqlite3AddColumn` does); a regression test for tables and the
  views over them was added.
- *`SELECT *` with no FROM clause* (low, ungated v0.4 path) gave
  one row of no columns (in a view, CTE, FROM subquery, scalar
  subquery, compound); it is the pin's "no tables specified",
  `SchemaKind::NoTablesSpecified`.

**Review-fix gates:** `cargo fmt --all` and the full
`ci/local-gates.sh` run end with "==> all gates passed". Census
unchanged (171 MATCH). Size: tiny **76,200 B** (Δ **0**; the
fixes on tiny-claimed paths compile out of it); full **430,016
B** (Δ **+5,344 B** vs 424,672, ratchet raised to it): the
flattener model and materialization ids in the expansion, the
shared-materialization cache, the bounded first-source loop and
the RETURNING / rowid / `SELECT *` / CREATE VIEW fixes.

**Review-fix honest gaps:**

- Whether a reference shares a materialization follows a model
  of the pin's planner. Known differences, all visible only
  through non-deterministic functions (random(), randomblob()):
  the pin runs the first of two same-named views as a co-routine
  when its reader aggregates or reads only some columns (`SELECT
  count(*) FROM w a, w b` with a LIMIT view: not shared on the
  pin, shared in hipp); a NOT MATERIALIZED CTE that cannot be
  flattened is still shared by the pin between references of one
  FROM clause (hipp: never); the flattener's affinity rule for
  compounds (17h) and nested flattening beyond views brought into
  a FROM clause are not modelled; inside RETURNING a view named
  twice is materialized twice. Results without such functions do
  not depend on any of this.
- `MAX_COPIES` counts copied SELECTs; the pin's counter includes
  something more (998 side-by-side references read on the pin,
  999 do not; hipp reads 999). A view chain past
  `Config::max_nesting` is `NestedTooDeep` where the pin (up to
  999) reads it; raise the limit with the stack to match.
- The SQL parser itself still recurses per nesting level of
  parentheses and subqueries, unbounded (pre-existing, outside
  this unit): about 1000 nested FROM subqueries overflow the
  CLI's stack in a debug build.

### Savepoints and maintenance statements (2026-11-02)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated as below. Census (225 probes, 14 new):
`savepoint`, `vacuum`, `reindex`, `analyze` went HIPP_ERR →
MATCH; totals MATCH 171, HIPP_ERR 25, DIFF 2, BOTH_ERR 27 (the
new BOTH_ERR probes are error-vs-error agreements). `BEGIN
DEFERRED / IMMEDIATE / EXCLUSIVE [TRANSACTION]` and `END
[TRANSACTION]` were already accepted.

**Shipped — `v16`** (new `src/session/maint.rs`; `Ast::{Savepoint,
Release, RollbackTo, Vacuum, Reindex, Analyze}`, `Plan::Maint`,
`Pager::{sp_*, replace_image}`, `btree::index_payloads`,
`Config::vacuum_into`, `storage::write_new_file`):

- *Savepoints.* `SAVEPOINT nm`, `RELEASE [SAVEPOINT] nm`,
  `ROLLBACK [TRANSACTION] TO [SAVEPOINT] nm` (`nm` as in PRAGMA:
  identifiers, keywords such as `begin` / `end` / `main`, strings,
  `""`). The pager keeps a stack of savepoint marks (name,
  whether it opened the transaction, header snapshot,
  sub-journal position); a savepoint outside a transaction sets
  the explicit bracket and is the transaction savepoint, whose
  release commits; RELEASE pops the newest mark of that name
  (ASCII case ignored) and everything newer; ROLLBACK TO restores
  and keeps it; COMMIT / END / ROLLBACK / a resumed commit clear
  the stack; BEGIN inside one is `TransactionState`; unknown names
  `SchemaKind::NoSuchSavepoint`. All of it is per-connection
  pager state.
- *The sub-journal.* Before a page first changes after the
  newest mark, `Pager::sp_note` records where its mark-time image
  is: for a page the transaction has not touched, its rollback-
  journal record (written next, holding that very image); for
  any other, a copy written into the journal file's *savepoint
  area*, which starts past the largest rollback journal the
  transaction can write (every original page once) plus one
  record of zeros. Recovery — hipp's and the pin's — stops at that
  zero record (page 0, bad checksum), so a crash anywhere still
  rolls back to the transaction's start; each copy is followed by
  four zero bytes so the file never ends in the journal magic
  (the pin would read a super-journal name there). No page image
  is held in RAM, so the transaction arena bounds nothing new.
  `sp_rollback` replays each recorded page once (earliest record
  wins), drops pages past the mark's size, restores the header
  (schema cookie, freelist, page count, user_version /
  application_id) and rebuilds the catalog.
- *Statement savepoints.* Every write statement inside a
  transaction (`Statement::open_stmt`) runs under an anonymous
  mark: the `abort_write!` macro, replacing the thirteen inline
  "roll back and close the bracket" sites, rolls back only that
  mark and keeps the transaction open; OR FAIL releases it
  (`keep_stmt`); `close_stmt` rolls back any other failed step
  and releases a successful one. Storage, arena, cache and
  journal errors still roll back the whole transaction, as the
  pin's I/O / out-of-memory errors do. This replaces the v0.2
  rule ("a failed statement aborts the whole bracket") wherever
  `v16` is built; `tests/dialect.rs` asserts both rules by
  feature.
- *VACUUM [main | temp] [INTO expr].* The schema rows are read
  raw and re-emitted in the pin's order — each table (rootpage >
  0, `sqlite_sequence` excepted) followed by its automatic
  indexes and, after the first AUTOINCREMENT table, by
  `sqlite_sequence`; then the explicit indexes; then views,
  triggers and virtual tables (rows copied unchanged) — into a
  fresh in-memory pager (`Pager<MemStorage>`) at the new page
  size; table records are copied byte for byte, keeping rowids
  unless the table has neither an INTEGER PRIMARY KEY nor any
  index (renumbered 1..n, as the pin's non-xfer copy does; VACUUM
  INTO keeps them all), and index keys are copied raw in key
  order (`btree::index_payloads`). Page 1's header is then
  patched as the pin leaves it: change counter + 1 (1 for INTO),
  schema cookie + 1, freelist 0, format 4, the default cache size,
  user_version and application_id kept, auto-vacuum off.
  In place, `Pager::replace_image` journals every original page,
  syncs the journal, writes pages 2.. then page 1, sets the
  length and syncs, then deletes the journal (the commit point;
  any failure before it replays the journal) and rebuilds the
  cache — the page size may change. `PRAGMA page_size` on a
  non-empty file and an `auto_vacuum` change are now kept for the
  next VACUUM (INTO included), as on the pin; a VACUUM that would
  produce an auto-vacuum file is refused (`Unsupported`: hipp
  keeps no pointer maps), so `PRAGMA auto_vacuum = 0; VACUUM`
  converts one. VACUUM INTO hands the image to
  `Config::vacuum_into` (std default: create the file, refusing
  one that holds data — `SchemaKind::VacuumTarget`; `None` on
  no_std: `Unsupported`). Inside a transaction:
  `VacuumInTransaction`; an unknown schema: `UnknownDatabase`;
  `VACUUM temp` does nothing (hipp has no temp schema).
- *REINDEX* `[collation | [main.] table-or-view | [main.]
  index]`, looked up as the pin does (a collation name first,
  unqualified only; then a table, view or `sqlite_schema`; then an
  index; else `SchemaKind::ReindexTarget`): each index is
  emptied (its pages freed, its root kept) and refilled from its
  table, unique checks included; an index hipp cannot compute
  (expression / partial: `opaque`) is left as it is — hipp never
  writes its table, so it is still exact.
- *ANALYZE* `[main | [main.] index | [main.] table-or-view]`: the
  pin's `sqlite_stat1` (created as `CREATE TABLE
  sqlite_stat1(tbl,idx,stat)` on first use, in the same
  transaction): per index `"N d1 .. dk"` with `dj = (N + D - 1) /
  D` over `D` distinct `j`-column prefixes (NULLs equal, each key
  column's collation deciding), 2 lowered to 1 when `10 N <= 11
  D`; a table with no index gets `"N"` (idx NULL); empty tables
  and empty indexes get no row; tables named `sqlite_*` are
  skipped; a view or system table only creates the table. Old
  rows go first exactly as the pin deletes them (`tbl = name`,
  `idx = name`, or all); new rows take rowid max + 1 (or an unused
one past the maximum: see the review fixes). Indexes are
  emitted in reverse creation order, tables in reverse schema
  order (see the gap). hipp's planner does not read the
  statistics: they are written so the files interoperate, and the
  pin plans the same over hipp's file as over its own
  (`EXPLAIN QUERY PLAN` compared in the tests).

**Shipped — ungated** (found while testing this unit):

- *Rollback journal* (tiny path, crash safety): a page the
  transaction allocated past the original size, spilled from the
  cache and then changed again got a journal record; replay
  stops at such a record (page past the original size), so every
  record after it was lost to a crash — torn file. Pages past the
  original size are never journaled now (recovery truncates
  them). +8 B.
- *Freelist reuse* (`v05` / `indexes`, crash safety): a page freed
  and reallocated in one transaction was zeroed without being
  journaled; a crash after its new image landed restored B-trees
  that still pointed at it. A popped freelist leaf within the
  original size is journaled (and noted for savepoints) first.
- *Failed replay* (tiny path): `Pager::rollback` took the journal
  sidecar out of the pager and lost it when the replay failed, so
  the next transaction panicked (`expect`). It is borrowed in
  place now — which also shrank the tiny profile.
- *CREATE INDEX on `rowid` / `oid` / `_rowid_`* (`indexes`): hipp
  built an index on the pseudo-column; the pin refuses it ("no
  such column"). `UnknownColumn` now (an INTEGER PRIMARY KEY
  column by its own name is still the rowid).

**Oracle-probing notes** (live pin, before implementation):
`SAVEPOINT` outside a transaction starts one; `RELEASE` of it
commits, so a following `ROLLBACK` is "no transaction is active";
inside `BEGIN`, releasing the outermost savepoint commits nothing.
`savepoint a; savepoint b; savepoint a; release a; release a`
releases the inner `a`, then the outer one with `b`. ROLLBACK TO
keeps the savepoint (it can be rolled back to again, then
released). Unknown names: "no such savepoint: x"; `SAVEPOINT a;
BEGIN`: "cannot start a transaction within a transaction";
`savepoint 123`, `release savepoint;`, `savepoint main.x` are
syntax errors; `savepoint savepoint`, `"" ` and keywords are
names. changes() / total_changes() / last_insert_rowid() are not
rolled back. A UNIQUE failure inside a transaction keeps the
transaction and the earlier rows; `INSERT ... VALUES (41), (40)`
failing on its second row leaves no 41. `PRAGMA user_version`
and CREATE TABLE are undone by ROLLBACK TO. VACUUM: "cannot
VACUUM from within a transaction" (INTO too); `vacuum nosuch` is
"unknown database"; schema cookie + 1, change counter + 1;
`user_version`, `application_id`, `default_cache_size` kept;
schema order tables-with-autoindexes, `sqlite_sequence` after
the AUTOINCREMENT table's autoindexes, other indexes, views and
triggers; a table without INTEGER PRIMARY KEY or index gets
rowids 1..n, every other keeps them, VACUUM INTO keeps all; INTO
an existing database: "output file already exists", an empty
file is fine, a non-database file "file is not a database"; INTO
writes change counter 1 and applies the pending page size without
changing the source's. `PRAGMA page_size = N` on a non-empty file
still reads the old size until VACUUM; 1000 is ignored. REINDEX:
`main.nocase`, `temp.ia` and unknown names are "unable to identify
the object to be reindexed"; views and `sqlite_schema` are
accepted; it works inside transactions. ANALYZE: the counts above
(`"4 2"` for (1, 2, NULL, NULL): NULLs equal; `"11 1"` for 10
distinct among 11 rows; `"12 2"` for 12); partial indexes always
get a row, with the table's own `"N"` row when only partial
indexes exist; `ANALYZE nosuch` is "no such table"; `ANALYZE
temp` creates `temp.sqlite_stat1`, which then hides the main one
for unqualified reads. A whole-database ANALYZE orders tables by
the pin's in-memory schema hash: reverse creation order while it
holds fewer than five entries (`sqlite_schema` and `sqlite_stat1`
count), a scrambled order beyond that (`a b c d` → `a d b c`);
probed and modelled against the hash shapes that fit SQLite's
documented design — none matched, so hipp does not reproduce it.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed". New suites:
`tests/savepoint_v16.rs` (9 tests: nesting, names, statement-level
rollback in BEGIN and in savepoints — UNIQUE / NOT NULL / CHECK /
STRICT failures, multi-row and INSERT ... SELECT failures half
way, OR FAIL / OR IGNORE, a failing CREATE UNIQUE INDEX —, schema
and header rollback, connection counters, large savepoints across
splits, overflow pages, the freelist and cache spills, a
generated corpus of 24 random interleavings of writes and
transaction statements, and the Connection API; every script runs
on both engines continuing past errors, comparing rows, failing
statements and the oracle's integrity check and `.dump` of both
files); `tests/maintenance_v16.rs` (12 tests: VACUUM against the
pin's own VACUUM of the same file, pending page sizes, errors,
VACUUM INTO, pin-made files with AUTOINCREMENT / triggers /
default_cache_size, the expression-index refusal, a generated
VACUUM + ANALYZE corpus with collations, NULLs and overflow
records, REINDEX forms, ANALYZE rows / rowids / deletions /
transactions, the pin's plans over hipp's statistics, `ANALYZE
temp`, and the CREATE INDEX regression); `tests/crash.rs` (+5
tests: every crash point of a savepoint transaction, of one inside
BEGIN with a half-failed statement, of a spilling transaction
under a 4-page cache with ROLLBACK TO across the spills, of
freelist reuse, and of VACUUM at 4096 → 4096 / 1024 / 16384-byte
pages recovers byte-exactly to the pre- or post-image; real
SQLite recovers hipp's savepoint journals at every kill point —
both journal fixes were confirmed by reverting them: the spill
and freelist tests then tear the file);
`hipp-cli/tests/cli_v16_savepoint.rs` (3 tests, byte-identical
one-shot and over stdin).

**Size:** tiny **76,200 B** (Δ **−124 B** vs 76,324, ratchet
lowered to it): the borrowed-sidecar rollback shrank it, the
journal fix costs 8 B, and the abort sites stayed inline in the
tiny profile (`abort_write!` is a macro; an `async fn` helper cost
96 B). Full **424,672 B** (Δ **+20,544 B** vs 404,128, ratchet
raised to it; over the 16 KiB `justify_over`, justified here):
savepoint marks and the journal-file sub-journal, statement
savepoints, the VACUUM rebuild and image replacement, REINDEX,
the ANALYZE statistics, their grammar and plans.

**Honest gaps:**

- A whole-database ANALYZE writes its rows in reverse schema
  order; the pin's order (its schema hash) agrees only while the
  schema hash is small (≤ 3 tables with `sqlite_stat1` absent,
  ≤ 2 with it present). The rows themselves are the pin's; the
  pin's planner does not depend on their order. hipp's planner
  ignores `sqlite_stat1`.
- VACUUM and ANALYZE refuse (`Unsupported`) a file with an
  expression or partial index (hipp cannot order or count one);
  REINDEX leaves such an index as it is. `ANALYZE temp` is
  refused (no temp schema); `VACUUM temp INTO` is refused.
- VACUUM builds the whole new image in memory (a `MemStorage`
  pager) before writing it: RAM proportional to the database.
  Its page layout can differ from the pin's (hipp's appends vs
  the pin's xfer copy and balance: 148 vs 147 pages for one test
  file at 512-byte pages); data, schema, rowids and header
  values agree.
- The savepoint area's offset grows with the database (the
  largest possible rollback journal): sparse on a file system,
  but a `MemStorage` journal allocates the gap.
- `Config::vacuum_into` reports success as a bool. (VACUUM INTO
  takes any text expression since the review fixes below.)
- Statement-level rollback is `v16` only: without it a failed
  statement still ends the whole bracket (the v0.2 rule).
- Found outside this unit: after the same statements hipp's file
  can have one more freelist page than the pin's (B-tree merge
  differences on DELETE), and the change counter can differ by
  one before any VACUUM.

**Review fixes (2026-11-03).** Seven findings against this unit,
each reproduced on the live pin first, fixed with a regression
test (`tests/savepoint_v16.rs`
`conflict_rollback_ends_the_transaction`,
`page_size_survives_rollback_to`; `tests/maintenance_v16.rs`
`drop_clears_statistics`, `analyze_names_as_stored`,
`vacuum_autoincrement_is_a_token`, `new_rowid_after_the_largest`,
`vacuum_into_expressions`, `vacuum_of_an_auto_vacuum_file`;
`tests/differential.rs` `insert_after_the_largest_rowid`;
`hipp-cli/tests/cli_v16_savepoint.rs` `review_fixes_byte_identical`):

- *ROLLBACK TO across `PRAGMA page_size` panicked* (`v16`):
  `sp_rollback` put back the header a savepoint saved, page size
  included, over frames `reformat` had already resized; the next
  write ran 4096-byte metadata over 1024-byte frames
  (`copy_from_slice` panic). The pin keeps the new size: it is not
  transactional. `Pager::reformat` now rebases every open mark on
  the reformatted empty file. Files and `:memory:` both.
- *OR ROLLBACK undid only the statement* (`v16`): hipp parsed OR
  ROLLBACK as ABORT, which was right only while every failure
  ended the bracket; under statement savepoints the transaction
  stayed open and a later COMMIT / RELEASE succeeded where the pin
  says "no transaction is active" / "no such savepoint". New
  `ConflictAction::Rollback` (`v16`; it resolves as ABORT
  everywhere except what the failure undoes): a UNIQUE / PRIMARY
  KEY / NOT NULL / CHECK violation under it ends the whole
  transaction (`Statement::resolves_rollback`, consulted by
  `stmt_rollback`), in BEGIN and SAVEPOINT transactions, INSERT
  and UPDATE alike. A constraint-level `ON CONFLICT ROLLBACK`
  (hipp otherwise parses constraint conflict clauses without
  acting on them) is honoured the same way under the default
  action: `parser::rollback_constraints` finds which constraint
  kinds (NOT NULL; UNIQUE / PRIMARY KEY, column or table
  constraint) carry it, and a violation of that kind ends the
  transaction. Probed: OR IGNORE / REPLACE / FAIL / ABORT override
  it, and an upsert's DO UPDATE failing resolves by ABORT whatever
  the OR clause says (`Statement::upsert_failed`). A failure that
  ends the transaction now also rolls back the TEMP database's, and
  a TEMP statement's ROLLBACK resolution ends the main one (both
  were left open). Autocommit is unchanged (ROLLBACK = ABORT there).
- *DROP left stale `sqlite_stat1` rows* (ungated, `v05` /
  `indexes`: pin-made files carry statistics too): DROP TABLE now
  deletes `tbl = name` and DROP INDEX `idx = name` rows from every
  `sqlite_stat1` .. `sqlite_stat4` present, comparing the stored
  name exactly, as the pin's `sqlite3ClearStatTables`; before the
  pages are freed (DROP TABLE sqlite_stat1 included). Probed: DROP
  VIEW, ALTER TABLE RENAME / RENAME COLUMN / DROP COLUMN leave the
  rows, as hipp does. Also found here: `ANALYZE T` / `ANALYZE I`
  for a table `t` / index `i` deleted by the name as typed and so
  duplicated the rows; ANALYZE now uses the stored name
  (`sqlite_master` for the schema table), as the pin.
- *VACUUM placed `sqlite_sequence` by substring*: the
  AUTOINCREMENT test matched the bytes anywhere (a string
  default, a quoted name, a comment); it is the keyword token now
  (`parser::has_autoincrement`, as the catalog uses).
- *ANALYZE failed when `sqlite_stat1` held rowid
  9223372036854775807* (`DatatypeMismatch`). Same pattern
  elsewhere: INSERT without a rowid failed there, and the schema
  row's rowid was computed unchecked (`m + 1`, an overflow on a
  file-derived value). With `v16`, `btree::new_rowid` takes the
  smallest unused positive rowid (the pin picks a random unused
  one; `DatabaseFull` only when all are taken) for INSERT,
  `sqlite_stat1`, `sqlite_sequence` and the schema table. Without
  `v16` (tiny): INSERT still fails cleanly (`DatatypeMismatch`)
  and the schema row's rowid saturates, so its insert fails as a
  duplicate rather than wrapping.
- *VACUUM INTO took only a string literal*: it takes any
  expression now, resolved against no table and evaluated when the
  statement runs (`'a' || 'b'`, a subquery, a bound `?`); a value
  that is not text is the pin's "non-text filename"
  (`VacuumTarget`); `VACUUM INTO ''` — the pin's anonymous
  temporary database — builds the image and discards it,
  succeeding, without needing `Config::vacuum_into`.
- *VACUUM of an auto-vacuum file is refused* (`Unsupported`):
  **not fixed, kept as a documented refusal.** The pin rebuilds it
  with pointer maps and keeps `auto_vacuum = 1`; hipp writes no
  pointer maps, and probing showed that hipp's ordinary writes to
  a pin-made auto-vacuum file already leave pointer-map entries
  stale ("Failed to read ptrmap key" in the pin's integrity check
  after an INSERT that allocates pages — a pre-existing gap of the
  write path, recorded since v0.15 as "auto-vacuum / ptrmap write
  support has no recorded demand"). Producing a fresh auto-vacuum
  image would hand back a file the next hipp write damages; the
  refusal leaves the file untouched, and `PRAGMA auto_vacuum = 0;
  VACUUM` converts it, as on the pin.

Remaining gaps from the review: a constraint-level `ON CONFLICT
ROLLBACK` is matched by constraint kind, not by the constraint
that failed — a table with `a NOT NULL ON CONFLICT ROLLBACK, b NOT
NULL` ends the transaction on either column's NULL (the pin: only
`a`'s), and a UNIQUE index created with CREATE UNIQUE INDEX takes
the table's UNIQUE-constraint ROLLBACK; an explicit `OR ABORT`
parses as the default, so it does not override a constraint-level
ROLLBACK as it does on the pin. Constraint-level IGNORE / REPLACE
/ FAIL remain parsed and not acted on (as before). Gates: `cargo
fmt --all` and the full `ci/local-gates.sh` run end with "==> all
gates passed". Size: tiny **76,224 B** (Δ **−4 B** vs 76,228,
ratchet lowered; the schema row's checked rowid costs a few bytes,
offset elsewhere). Full **471,016 B** (Δ **+2,968 B** vs 468,048,
ratchet raised): the ROLLBACK resolution and constraint scan, stat
clearing on DROP, `new_rowid`, the VACUUM INTO expression.

### ALTER TABLE, AUTOINCREMENT and TEMP tables (2026-11-03)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated as below. Every script in the new suites runs
on both engines continuing past errors, comparing rows (stored
schema texts included), failing statements, and the oracle's
`PRAGMA integrity_check` and `.dump` of both files.

**Shipped — `v16`** (new `src/session/alter.rs`,
`src/session/temp.rs`; `Ast::{AlterRename, AlterRenameColumn,
AlterDropColumn}`, `Plan::{Alter16, Temp}`, `TableDef::autoinc`,
`query::AlterTrace`, `record::encode_record_plain`,
`Connection::temp_db`, `SchemaKind::{AutoincrementNotIntegerPk,
RowidAliasName, CannotDropColumn, DatabaseFull}`):

- *AUTOINCREMENT.* `INTEGER PRIMARY KEY AUTOINCREMENT` and
  `PRIMARY KEY (a [DESC] AUTOINCREMENT)` on the rowid alias
  (anything else: `AutoincrementNotIntegerPk`). The first such
  table creates `sqlite_sequence` with the pin's text `CREATE
  TABLE sqlite_sequence(name,seq)`, its schema row and root page
  right after that table's automatic indexes. An INSERT reads the
  table's row once (the first whose `name` equals the table's
  name byte for byte; `seq` as CAST AS INTEGER, NULL / no row
  reading 0); a generated rowid is one past both the largest rowid
  and that counter; every rowid the statement inserts or
  generates raises it (ignored rows, upsert DO UPDATE and REPLACE
  included); the row is written back when the statement completes
  (a new row at max rowid + 1 when there was none — even for an
  INSERT of no rows — else rewritten in place when it grew); an
  INSERT that fails part way (OR FAIL) writes nothing; i64::MAX
  handed out or present is `DatabaseFull` ("database or disk is
  full"). DROP TABLE deletes the table's sequence rows; RENAME TO
  renames them; hand edits (`UPDATE / DELETE / INSERT INTO
  sqlite_sequence`) are read as the pin reads them. `.dump`
  (hipp-cli) writes `DELETE FROM sqlite_sequence;` + INSERTs last,
  as the oracle shell does, and `.tables` leaves `sqlite_*` out.
- *ALTER TABLE t RENAME TO u / RENAME [COLUMN] a TO b / DROP
  [COLUMN] a.* The stored texts are edited token by token, every
  other byte kept, exactly as the pin's `renameEditSql` writes
  them (RENAME TO always `"u"`; RENAME COLUMN bare where the old
  token was bare and the new name was written bare, else `"b"`,
  with a space before a following `"`). The table's own text (its
  name, columns, PRIMARY KEY / UNIQUE / FOREIGN KEY lists, `t.`
  qualifiers and column references in CHECK and generated
  expressions), every `REFERENCES t (cols)`, its indexes (`ON t`,
  key and WHERE column references, `sqlite_autoindex_t_N` names,
  `tbl_name`) and every view: a view is expanded and planned with
  an `AlterTrace` on the table, so the planner itself records
  which FROM names, qualifiers and column tokens resolved to it
  (aliases, CTEs, subqueries, USING, `x IN t` decided exactly as
  in a query). The pin's checks: no `sqlite_*` table, no view,
  name not taken, not a rowid alias, no duplicate column, every
  view must plan after the change. DROP COLUMN removes the
  definition text (the last one from the comma before it to the
  table constraints / `)`), refuses PRIMARY KEY / UNIQUE / the
  only column and one an index, CHECK, generated column, foreign
  key or view uses, and rewrites every row without it
  (`drop_rewrite`: short rows' defaults materialised, 0 / 1
  stored as one-byte integers like the pin's rewrite — serial 1,
  not 8 / 9 — so the files are byte-compatible). A file with
  triggers is refused (`Unsupported`).
- *TEMP tables, views and indexes.* A per-connection temp
  database: a second, private `Connection<MemStorage>`
  (`Connection::temp`), created on first use and dropped with the
  connection, never written to the file. Statements are routed
  before planning (`Connection::route`) by the pin's name
  resolution: `CREATE TEMP | TEMPORARY TABLE / VIEW`, `CREATE
  TABLE temp.t`, `temp.`-qualified names, `sqlite_temp_schema` /
  `sqlite_temp_master` / `temp.sqlite_schema`, and unqualified
  names of temp objects (temp before main) run there with `TEMP`
  and the `temp.` qualifiers cut out of the text; CREATE INDEX
  follows its table; DROP TABLE drops the temp one first;
  `REINDEX` / `ANALYZE` of temp objects and `ANALYZE temp` (which
  makes `temp.sqlite_stat1`, then hiding main's) run there; a name
  in neither database binds to neither (so `CREATE TEMP VIEW v AS
  SELECT * FROM later` is stored, as on the pin).
  BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO
  apply to both databases (a temp database created inside a
  transaction first replays it), `last_insert_rowid()` /
  `changes()` / `total_changes()` follow both, bound parameters
  reach routed statements, `PRAGMA temp.x`, `table_info` of a
  temp table, `table_list` and `database_list` (`1|temp|` once it
  exists) show it; its schema has the pin's texts and root pages.
  AUTOINCREMENT and ALTER TABLE work there too, including the
  pin's double edit of TEMP views on RENAME COLUMN (below).
  hipp-cli `.schema` prints temp objects as `CREATE TABLE
  temp.x` / `/* temp.v(...) */` and `.tables` as `temp.x`.

**Shipped — ungated** (found while probing this unit):

- *AUTOINCREMENT without `v16`* (tiny path, INSERT): hipp inserted
  into a pin-made AUTOINCREMENT table and left `sqlite_sequence`
  behind, so after a DELETE the pin handed out a rowid
  AUTOINCREMENT had already used. Such a table is read-only there
  now (`Unsupported`), as CREATE already refused the clause.
  +24 B (of the +28 B below).
- *DROP TABLE of an AUTOINCREMENT table* (`v05`): its
  `sqlite_sequence` row stayed behind (the pin deletes it).
- *System tables* (`indexes` / `v04` / `v05`): hipp indexed,
  ALTER ... ADD COLUMN'd and dropped `sqlite_sequence` (and other
  `sqlite_*` tables); the pin refuses all three (only
  `sqlite_stat*` may be dropped) — `SchemaKind::SystemTable`.
- *ALTER TABLE ADD COLUMN on a table with table constraints*
  (`v04`): the column was appended after the constraints, a text
  that no longer parses (the next open failed). It is spliced in
  after the last column definition, the pin's `addColOffset`.

**Oracle-probing notes** (live pin): `sqlite_sequence`'s root page
follows the first AUTOINCREMENT table's autoindexes; the counter
rules above, including `UPDATE sqlite_sequence SET seq = 'abc'` /
`7.7` (read as 0 / 7: the next rowid is 2 / 8), two rows of one
name (the first wins and is the one rewritten), `name = 'T'` (not
the table's row: byte comparison), a negative explicit rowid (the
row is written with 0), and a ROLLBACK undoing the counter. RENAME: `"u"` always for RENAME TO; `'u'`, `[u]` and `"u"`
as new column names all produce `"u"`; comments and spacing kept;
"there is already another table or index with this name", "object
name reserved for internal use", "cannot rename to rowid alias",
"error in table t after rename: duplicate column name", "error in
view v after rename: ambiguous column name" are errors here too.
DROP COLUMN: "cannot drop PRIMARY KEY column", "cannot drop UNIQUE
column", "no other columns exist", "error in index i / table t /
view v after drop column: no such column"; the pin's rewritten
rows store integers 0 / 1 as one-byte values (serial type 1). TEMP:
a view without TEMP is a main view even over temp tables (and then
reads main only: "no such table: main.x"); `CREATE TEMP TABLE
main.t` and `CREATE INDEX temp.i ON main_table` are errors;
`main.sqlite_temp_schema` is "no such table"; plain `ANALYZE` leaves
the temp database alone; temp root pages start at 2; `REINDEX temp`
names an object called `temp` ("unable to identify"). The pin's
RENAME COLUMN of a TEMP table edits its TEMP views twice (the
main-schema UPDATE and the temp-trigger/view UPDATE both run on the
temp schema): the second pass plans the already-edited view
against the old columns and fails ("error in view v: no such
column: z") whenever the view names the column — unless every
edited name is a bare `"z"`, which the second pass reads as a
string (DQS). hipp reproduces this rule (`plan_view_text`) without
having the DQS fallback in general.

**Review fixes** (each reproduced on the pin, each with a
regression test):

- *DROP COLUMN past a comma in a comment* (`t(a, b, /* x,y */ c)`,
  `c, -- ,\n d`): the cut scanned bytes back to a `,`, so a comma
  inside a comment (or quoted name) cut the text there; the edited
  text no longer parsed, the error came after the catalog rebuild
  had begun, and the session's catalog kept entries whose arena was
  gone — `no such table: t` for the rest of the session, and a
  second `CREATE TABLE t` wrote a duplicate schema row the pin
  rejects as malformed. The comma is now the top-level `,` token
  before the column (and before the constraints that follow), as
  on the pin; a failed ALTER rebuilds the catalog from the restored
  file, and (with `v16`) a failed catalog build leaves an empty
  catalog the next prepare rebuilds, never a stale one.
- *Connection-wide pragmas on TEMP tables*: `foreign_keys`,
  `case_sensitive_like`, `ignore_check_constraints`,
  `recursive_triggers`, `busy_timeout` are copied to the temp
  database before each statement there (a CHECK was enforced under
  `ignore_check_constraints`, LIKE ignored `case_sensitive_like`,
  and foreign-key writes ran unchecked where main refuses them);
  `PRAGMA temp.foreign_keys = ON` (any of these, and
  `collation_list`) sets the connection's, as on the pin.
- *"database or disk is full"* (AUTOINCREMENT past i64::MAX, no
  rowid left) now rolls back the whole transaction, as the pin's
  SQLITE_FULL does (the next COMMIT is "no transaction is active");
  hipp failed only the statement, so COMMIT kept earlier writes.
- *Quoted `"sqlite_temp_schema"` / `[sqlite_temp_master]` / ...*
  returned the temp database's own (empty) temp schema; any quoting
  is renamed now — and only where it names the table (an alias
  `AS sqlite_temp_master` is left alone).
- *`CREATE TEMP [UNIQUE] INDEX`* ran; it is a syntax error on the
  pin and now in hipp.
- *`temp.a` as a column of a table or alias called `temp`* was
  refused (a regression of this unit): a `temp` that qualifies a
  column in the AST is no schema qualifier now; `x IN temp.t` is
  routed by the name's text (it was refused).
- *A TEMP view over main tables* (declared gap, closed in part):
  `CREATE TEMP VIEW v AS SELECT * FROM main_t` is stored, and a
  statement reading such views (directly or through other TEMP
  views) and no temp table runs on main with the views written in
  as common table expressions (`inline_temp_views`; a statement
  CTE of a clashing name is refused). A TEMP view's text is now
  stored as written (`temp.t`, `"sqlite_temp_master"` kept, as on
  the pin; hipp used to cut them), and the temp database reads its
  own `sqlite_temp_schema` as its schema table.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed". New suites: `tests/alter_v16.rs`
(8 tests: RENAME TO / RENAME COLUMN / DROP COLUMN matrices, views
following the rename through aliases, CTEs, subqueries and
compound ORDER BY, pin-made files altered by hipp and hipp-made
files read and altered by the pin, a generated corpus);
`tests/autoinc_v16.rs` (8 tests: creation and schema order, the
counter rules, hand-edited sequence rows, i64::MAX, transactions
and savepoints, interoperating files, a generated corpus of random
INSERT / REPLACE / upsert / DELETE / sequence edits);
`tests/temp_v16.rs` (7 tests: 47 temp-schema cases, the pin's
corner cases above, file untouched and temp gone on close, the
mixed-database refusal, parameters, and a generated corpus of
random main / temp / shadowing / transaction interleavings — 10
cases by default, `HIPP_TEMP_CORPUS=200` ran clean);
`hipp-cli/tests/cli_v16_alter.rs` (6 tests: byte-identical ALTER /
AUTOINCREMENT / TEMP batches one-shot and over stdin, `.schema`
with temp objects, `.tables`, `.dump` replaying the counter).
Review fixes added `alter_v16::{drop_column_skips_commas_in_comments,
failed_alter_keeps_the_catalog}`, `autoinc_v16::full_ends_the_transaction`
(13 transaction shapes), `temp_v16::{review_fixes,
temp_views_over_main_tables, main_alter_under_temp_views_refused}`
and `cli_v16_alter::review_fixes_byte_identical`;
`HIPP_TEMP_CORPUS=200` still runs clean.
`tests/table_defs_v16.rs` and `tests/query_shape_v16.rs` now assert
the AUTOINCREMENT and `temp.` behaviour per feature;
`tests/maintenance_v16.rs` (`ANALYZE temp`) and
`tests/pragma_v16.rs` (`PRAGMA temp.x`) compare what they used to
expect refused with the pin.

**Size:** tiny **76,228 B** (Δ **+28 B** vs 76,200, ratchet raised
to it): a correctness fix on the INSERT path the tiny profile
claims — the read-only refusal of a pin-made AUTOINCREMENT table
(24 B; see above), within the +256 B allowance. Full **468,048 B**
(Δ **+38,032 B** vs 430,016, ratchet raised to it; over the
16 KiB `justify_over`, justified here): the ALTER TABLE text
editor and view tracing, DROP COLUMN's row rewrite, AUTOINCREMENT
bookkeeping, and the TEMP database (a second monomorphised
`Connection<MemStorage>` — most of the growth — plus routing).

Review fixes: tiny **76,224 B** (Δ **0**: every change is behind
`v16`, the catalog guard included). Full **475,016 B** (Δ
**+4,000 B** vs 471,016, ratchet raised to it): TEMP-view
inlining, the connection-wide pragma copy, the token-aware DROP
COLUMN cut and the catalog guard.

**Honest gaps:**

- A statement that names a temp table (or a TEMP view reading
  one) and a main object (`SELECT ... FROM main_t, temp_t`,
  `CREATE TEMP TABLE x AS SELECT * FROM main_t`, `INSERT INTO
  temp_t SELECT ... FROM main_t`, a TEMP view over both) is refused
  (`Unsupported`): hipp's two databases never meet inside one
  statement. TEMP views over main tables are read (review fix,
  below), but `PRAGMA table_info` of one fails (the temp database
  plans it without main), and an ALTER TABLE of a main table such
  a view reads is refused where the pin rewrites the view.
  Three-part names (`temp.x.a`) do not parse;
  `pragma_table_info('x', 'temp')` is refused; `VACUUM temp INTO`
  is refused (plain `VACUUM temp` does nothing, as on the pin for
  an in-memory temp database). The earlier "`ANALYZE temp` is
  refused" gap is closed.
- ALTER TABLE refuses a file holding triggers (hipp has none and
  cannot parse them); the TEMP view double-edit is modelled for
  RENAME COLUMN as probed, and the DQS fallback only there.
- Root pages after DROP TABLE: hipp reuses freed pages from the
  freelist in another order than the pin (the pin takes the
  smallest free leaf for a new root first; hipp the trunk's last
  leaf), so a table re-created after a DROP can get different
  `rootpage` numbers (main and temp alike; the files are valid
  and read identically). Pre-existing pager behaviour, found
  here; left for a pager unit (it is on the tiny path).
- hipp-cli `.tables` prints one name per line (the oracle shell
  columnates); `.dump` still differs from the shell's around
  `PRAGMA foreign_keys` / `writable_schema` and `sqlite_stat1`.

### Parser and evaluator depth limits (2026-11-03)

**Status:** implemented, differential-tested against the live
3.54.1 pin, **ungated** (a crash fix on claimed paths, and the
limits are where the pin refuses). Before it, the parser, the
resolver, the planner, the evaluator and the tree's Clone / Drop
recursed once per nesting level with no bound: 100 000 nested
parentheses, signs, NOTs, calls, CASEs or subqueries in one
statement (or in a stored CHECK / view text) overflowed the stack,
and so did a long run of signs in the tiny profile's `INSERT ...
VALUES (- - ... 1)` (`literal_at` recursed per sign).

**Oracle-probing notes** (the pin's build: `MAX_EXPR_DEPTH=1000`,
`MAX_FUNCTION_ARG=1000`, `MAX_COLUMN=2000`,
`MAX_COMPOUND_SELECT=500`, `MAX_VARIABLE_NUMBER=32766`,
`MAX_LIKE_PATTERN_LENGTH=50000`, `MAX_SQL_LENGTH=1000000000`;
every boundary below was found by bisection on the pin):

- *Expression height* ("Expression tree is too large (maximum
  depth 1000)") is the pin's `Expr.nHeight`, checked as each node
  is made: a leaf is 1 (`t.a` is 2, a DOT node); a node is one more
  than its tallest operand; parentheses add nothing; `+` over a `+`
  node re-uses it and `-` over one turns it into a `-` node (so
  2494 `+` signs parse, 999 `-` do); a row value is a node of height
  1 whatever its elements; `x ISNULL` / NOTNULL / `IS [NOT] NULL` /
  `NOT NULL` over a numeric, string or blob literal (under any
  signs) folds to the integer 0 / 1 (`1 ISNULL ISNULL ...` never
  grows); `x IN ()` folds to a literal; `x IN (c)` with one
  constant item and no COLLATE is `x = +c` (one node more), with
  one scalar-subquery item it is `x IN (SELECT ...)`; NOT LIKE /
  BETWEEN / IN / GLOB add a NOT node; a call is one more than its
  tallest argument (FILTER, aggregate ORDER BY and OVER count
  nothing); a subquery node is one more than its select's tallest
  result column, WHERE, HAVING, GROUP BY, ORDER BY or LIMIT node
  (LIMIT and OFFSET are one node) over all compound members.
- *Resolution adds heights*: the pin's `sqlite3ResolveExprNames`
  adds each resolved expression's height to `Parse.nHeight`, so a
  subquery's expressions count with the heights of every
  expression enclosing it: 43 nested scalar subqueries (sum of
  heights 990) read, 44 do not; 41 nested `x IN (SELECT ...)`. FROM
  subqueries and CTE bodies are resolved in their select's context,
  not inside its expressions (`(SELECT 1 FROM (SELECT - - - - 1))`
  counts 2 + 5). ON / USING terms are ANDed into the WHERE before
  resolution (`sqlite3ProcessJoin`; `ON <5> WHERE 1` resolves a
  height-6 tree). ORDER BY / GROUP BY ordinals are not resolved.
  A CREATE VIEW body is not resolved until the view is read (the pin
  creates a view no query can read).
- *Compound sorter*: a member of UNION / EXCEPT / INTERSECT, or of
  any compound with an ORDER BY, gets one node over each result
  column (a COLLATE the pin adds), so its columns stop at 999;
  UNION ALL alone adds none.
- *Parser stack* ("Recursion limit"): the pin's LALR stack holds
  2500 symbols, counted from the bottom entry: `SELECT (((1)))`
  holds `SELECT distinct sclp scanpt` then one `(` per level (2493
  levels), a call `idj LP distinct` per level (831), CASE `CASE
  case_operand WHEN expr THEN` (498), a FROM subquery `SELECT
  distinct selcollist FROM stl_prefix LP` (414), a core's end
  `SELECT distinct selcollist from where_opt groupby_opt having_opt
  orderby_opt limit_opt`. The bases of about 90 contexts were probed
  and modelled (`INSERT VALUES` 8 / 10 / 9 / 11 by position, `UPDATE SET`
  9, `WHERE` of UPDATE 10, of DELETE 7, `CREATE VIEW` 9 + the
  select's, `ON` 10, `LIMIT` 10, `OFFSET` 12, CTE bodies 6 / 8 (+1
  RECURSIVE), compound members 2 up, RETURNING, upsert, ...).
- *Counts*: "too many terms in compound SELECT" past 500 SELECTs
  (multi-row VALUES is one), "too many arguments on function f" past
  1000, "too many columns on t" / "in index" / "in result set"
  (after `*`) / "in set list" past 2000 (ALTER TABLE ADD COLUMN
  too), "too many terms in ORDER BY clause" / "GROUP BY" past 2000,
  "at most 64 tables in a join". The existing bound-parameter and
  LIKE / GLOB pattern limits were confirmed.
- *Found while probing* (ungated fix): `ORDER BY 2 NOTNULL` is the
  ordinal 1 on the pin, `ORDER BY 1 ISNULL` / `GROUP BY 1 ISNULL`
  the out-of-range ordinal 0 — the null-test fold makes an integer
  literal; hipp sorted and grouped by a constant.
- *Probed for the review fixes:* a view or CTE body is resolved
  where it is read, inside the reader's expressions, so the sums
  add up along a chain: views `v_i AS SELECT <m signs>(SELECT a
  FROM v_{i-1})` read at m = 998 / 498 / 331 / 198 for 1 / 2 / 3 / 5
  views, CTEs at 997 / 497 / 331 / 197 (likewise through IN, ON,
  LIMIT and EXISTS readers). A view is one more permissive because
  the pin keeps its tree reduced (`EXPRDUP_REDUCE`: a leaf loses its
  height), so a lone leaf it resolves (`SELECT a`, the `1` of
  `(SELECT 1)`) adds 0 there. An unused CTE is never resolved (its
  2001 result columns, 2001 ORDER BY terms or 44 nested subqueries
  pass), nor is a CREATE VIEW body until read; "too many columns
  in result set" covers RETURNING (after `*`) and a multi-row VALUES,
  not an INSERT's column list or one-row VALUES, and an upsert's
  SET takes any number of terms. A compound past 500 SELECTs is
  accepted when its last member is a one-row VALUES (600 SELECTs
  then `VALUES(1)` read, `VALUES(1),(2)` do not).
  `sqlite3ExprIsInteger` looks through COLLATE, unary `+` / `-`
  and parentheses (`ORDER BY -(-2)` is the ordinal 2, `-(2)` out of
  range) but not TRUE / FALSE. `DEFAULT (<signs> 1)` takes the
  pin's two limits at 7 symbols for the first column and 9 for a
  later one or ADD COLUMN (`+`: 2492 / 2490; `-`: 999). A first
  CREATE TABLE whose schema row no longer fits page 1 makes page 1
  an interior page with no cells over one child (`balance_deeper`),
  and a leaf under page 1 is never quick-split (`balance_nonroot`).

**Shipped (ungated):** `sql::parser::Limits` (`Config::limits`,
`Config::max_expr_depth`, `Config::max_parser_depth`,
`Config::depth_limits`, `Config::stack_budget`;
`ParseKind::{ExprTooDeep, StatementTooLong, TooManyColumns,
TooManyTerms, TooManyArguments, TooManyCompoundTerms}`,
`SchemaKind::{TooManyColumns, TooManyTables}`). The parser carries
the pin's two counts as it reads: `held` (the LALR symbols below
the construct, checked on the way down so no recursion outruns it)
and `eh` / `ek` (the height and kind of the expression just
parsed), with the resolution sum carried up per select (`rsub`,
`sel_r`) and the ON chain folded (`OnChain`). Every parse site
takes the connection's limits: statements, stored schema texts at
catalog load, view bodies at expansion (`parse_view`: the body's
resolution depth is checked when it is read, not at CREATE VIEW),
CHECK / DEFAULT / generated re-parses, ALTER's text walks.
`literal_at` walks sign runs without recursion (tiny profile
included, with the pin's limits on the run in `INSERT VALUES`,
`WHERE rowid =` and a parenthesised DEFAULT — `sign_run`). The null-test fold (above). CREATE TABLE items
nest parentheses up to the parser limit (they stopped at 32). The
count limits above. The parser's `sql[a..b]` slices became checked
`span`s; `cmp_op` / `arith`'s `unreachable!()` arms an error.

**Review fixes (2026-11-03, ungated):** view and CTE bodies are
checked where they are read: the parser records on each subquery
the height of the expression the pin resolves it in
(`SelectStmt::rctx`, marked when that expression is complete) and
each select's own resolution depth (`SelectStmt::rdepth`); the
view / CTE expansion carries the sum down (`Expander::ctx`) and
refuses a body whose depth there passes the limit (a chain of views
or CTEs used to pass any length — wrong answers, then a stack
overflow; the review's 170-CTE chain aborted the CLI). CTE bodies
are parsed deferred (`defer`, like a CREATE VIEW body) and a view's
lone leaves count 0 (`reduced`). The width limits only resolution
applies (result set, ORDER BY / GROUP BY terms, VALUES rows) are
checked on the bodies the expansion copies, not in an unused CTE or
a CREATE VIEW; RETURNING is held to 2000 columns after `*` before
any row changes; an INSERT's one VALUES row and an upsert's SET are
not limited; the compound limit exempts a one-row VALUES tail. The
ordinal fold (`ordinal_term`) follows `sqlite3ExprIsInteger`. A
parenthesised DEFAULT sign run is held to both limits at its
column's stack base (hipp stored schemas the pin cannot open).
`btree_insert` balances page 1 as the pin does (the empty page-1
quick split panicked with a subtract overflow; a schema row of
about 3950 bytes in a fresh file).

*Stack.* With the tree height and the parser stack bounded, every
recursive walk of one statement's text is too, and the resolution
sum bounds the expressions of the view and CTE bodies it reads
together; the bodies' own nesting (FROM levels, planner and executor
frames per body) is bounded by `Config::max_nesting`, not by
`stack_budget` (see below). To make the bound affordable the hot
recursive functions were split so one nesting level puts a small
frame on the stack: the parser's primary / unary / parenthesis /
call / CASE / CAST / column paths, `resolve_scoped` (common nodes
inline, the rest in `resolve_other` and one function per node
kind), `eval` (likewise; `arith` / `cmp_op` evaluate operands then
call out), the FROM planner (`plan_derived_source`,
`synth_source`), the FROM-source parser. Measured on x86_64
(release / debug, bytes per level): 911 / 3.7 K per nested
parenthesis (was 2.1 K / 12 K), 640 / 3.3 K per height of a `+`
chain (2.9 K / 24 K), 1.3 K / 9.5 K per nested call (3.1 K /
26 K), 1.5 K / 8.2 K per nested CASE (2.9 K / 24 K), 8.9 K / 25 K
per nested FROM subquery (parse, plan and execution). Worst
statement at the pin's limits: 4.3 MiB release, 13 MiB debug; at
the defaults (expressions 1000 deep, a 500-symbol parser stack):
1.4 MiB / 5.3 MiB (a 2 MiB release thread, an 8 MiB debug one).
`Config::stack_budget(bytes)` uses `STACK_BASE + parser_depth *
STACK_PER_SYMBOL + expr_depth * STACK_PER_HEIGHT` (64 K + 1.5 K +
704 B release, 320 K + 4 K + 3.4 K debug; a debug build needs 270
KiB for `SELECT 1` — the step future alone is 100 KiB).
`Config::tiny()` takes 64 / 64. The CLI sets the pin's limits on
its 512 MiB thread. A view or CTE level read inside another costs
up to about 24 KiB in release (a view reading the last one in an IN
subquery; 12 KiB through a scalar subquery) and 130 KiB in debug:
the default `max_nesting` of 250 needs about 6 MiB in release (an 8
MiB thread) and 32 MiB in debug — measured, and documented on
`Config::max_nesting`.

**Tests:** `tests/depth_v16.rs` (8 tests): 36 nesting shapes at the
pin's limits on both sides of every boundary vs the live pin
(answers and error classes), the same shapes under
`stack_budget(512 KiB)` (256 KiB in release) and under the
defaults on an 8 MiB thread — each accepted up to hipp's boundary
with the pin's answer, refused past it, and refused 50 times
deeper, never an overflow; 18 count limits on both sides; a
generated corpus (xorshift, `HIPP_DEPTH_CORPUS`, default 24 cases,
300 ran clean) of random wrappers around random operands in random
statement contexts, the boundary bisected on the pin; the
regressions (null-test ordinals, CHECK past 32 parentheses, the
CREATE VIEW deferral, IN-list rewrites, the ON chain, LIMIT /
OFFSET, the compound sorter); 100 000-deep hostile statements on a
default thread. `tests/depth_tiny_v16.rs` (3 tests, also built and
run without the v0.4 grammar by `tiny_build_runs_these`): sign runs
in every literal position of the tiny grammar at the pin's
boundaries (searched live) on a small thread, 100 000-sign runs,
2000 / 2001 columns (and the pin reading hipp's 2000-column file).
Parser unit tests: the statement length limit, the ON fold.
`hipp-cli/tests/cli_v16_depth.rs` (2 tests): a byte-identical
batch of 15 shapes at and past their boundaries, the ordinal fold
and the count limits. `tests/views_cte_v16.rs`'s pin-limit
configuration now sets the pin's depth limits too. Review fixes:
`tests/depth_v16.rs` +6 — view and CTE chains (1 to 5 bodies,
reading the last in a scalar / IN / ON / LIMIT / EXISTS position,
and a view's lone leaves) on both sides of the pin's boundary
searched live; the review's crash chains refused on a 2 MiB
release thread under the defaults; 27 ORDER BY / GROUP BY /
compound ordinal terms; the deferred width limits (RETURNING with
the table state after, INSERT column lists, upsert SET, unused
CTEs, CREATE VIEW bodies and their readers); compounds ending in
VALUES; ALTER TABLE ADD COLUMN DEFAULT sign runs.
`tests/depth_tiny_v16.rs` +2 (also in the tiny build): DEFAULT
sign runs at both column bases with the pin reading the file, and
schema rows that stop fitting page 1 (page shapes equal to the
pin's). `hipp-cli/tests/cli_v16_depth.rs` +1: a byte-identical
batch of all of these.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed".

**Size:** tiny **76,476 B** (Δ **+252 B** vs 76,224, ratchet raised
to it): correctness fixes on the tiny profile's INSERT / SELECT /
CREATE TABLE paths, within the +256 B allowance — the sign-run
walk without recursion and its pin limits (the crash), the 2000-
column checks of CREATE TABLE and the result list. Full **482,328 B**
(Δ **+7,432 B** vs 474,896, ratchet raised to it) and max **482,312 B**
(Δ **+7,408 B** vs 474,904): the parser's two counts in every
construct, the resolution-depth fold, the count limits and the
split walkers. Review fixes (on top of the window unit's 76,476 /
482,304 / 524,224): tiny **76,728 B** (Δ **+252 B**, ratchet raised):
correctness fixes on CREATE TABLE, within the +256 B allowance —
the page-1 split (a panic) and the DEFAULT sign-run limits (a file
the pin cannot open), with `sign_run` shared by the tiny literal
paths; full **484,116 B** (Δ **+1,812 B**) and max **526,372 B**
(Δ **+2,148 B**): the resolution marks and checks, the deferred width
checks, the ordinal fold. `link.x`'s sizing FLASH region grew to
1 MiB (max's budget, ADR 0002): max had reached the old 512 KiB.

**Honest gaps:**

- A flattened FROM subquery, view or CTE column that the outer
  query reads becomes, on the pin, a copy of the column's expression
  under a COLLATE node; the copy's height (+1, plus the reading
  expression's) is checked there and not in hipp, so hipp accepts
  `SELECT * FROM (SELECT <height 1000> FROM t)` where the pin refuses
  it (the pin refuses at 999 + 1).
- NATURAL join terms are not counted in the WHERE tree (USING ones
  are), nor is the pin's shortcut of `ON 0`; in a view body the ON
  chain's AND nodes, which the pin rebuilds over its reduced copy,
  are counted at their parse-time heights. A subquery inside a
  WINDOW clause definition counts its own expression's height, not
  that of the call that uses the window. CHECK, expression DEFAULT
  and generated expressions are re-parsed alone and counted at the
  first column's base (a later column's sits two symbols higher on
  the pin; a parenthesised signed-literal DEFAULT is exact);
  likewise ALTER TABLE ADD COLUMN's. RETURNING after a
  several-clause upsert and CAST type arguments use approximate
  bases.
- `Config::stack_budget` budgets one statement's text; the views and
  CTEs it reads nest up to `max_nesting` levels deeper, which a
  small stack must lower by hand (the per-level costs are on that
  field). Nested FROM subqueries inside each body multiply with it.
- The join limit is counted per FROM clause (the pin counts after
  flattening, so a join of flattened subqueries can pass 64 there).
- The stack costs are x86_64 measurements; other targets' frames
  differ (32-bit ones are usually smaller), so `stack_budget` is an
  estimate there — leave headroom. The debug build's 270 KiB base
  (the step future) is untouched.
- `SELECT * FROM (SELECT 1,1,1,1,1)` is still `Unsupported`
  (pre-existing: past three duplicate names the pin switches to a
  random column-name suffix).
- The audit for other panics reachable from SQL covered the parser
  and the evaluator, and (review) the page-1 split; the
  `unreachable!()` / `expect` / slice indexing left in `session.rs`
  and `btree.rs` guard structural invariants (plan kinds, a B-tree
  descent's parent, non-empty balance groups), not data, and were
  not rewritten.
- `sqlite_schema` pages now split as the pin's do, but a balance of
  many leaves under page 1 can still put a cell or two on a
  different sibling than the pin (80 / 78 where the pin has 81 / 77
  at 400 tables: the v0.15 window distribution), so the files are
  not byte-identical there; both read and pass integrity checks.

### Window functions (2026-11-03)

**Status:** implemented behind the new leaf feature `window`
(implies `v16`; in `max`, not `full`, per ADR 0002; its own
`ci/feature-sets.txt` line `v16,window`), differentially tested
against the live 3.54.1 pin. Census: the four `window_*` probes
(row_number, partitioned running sum, rank / dense_rank, lag /
lead) went from HIPP_ERR to MATCH.

**Shipped (`window`):**

- Grammar: `f(args) [FILTER (WHERE ...)] OVER (...)` and `OVER
  name` after any call (OVER is a keyword only after `)` with `(`
  or a name after it — a string, a quoted, plain or fallback
  identifier or a join keyword, never FROM / WHERE / UNION /
  LIMIT / ..., as in the pin's `analyzeOverKeyword` / `getToken`;
  WINDOW only before `name AS`; FILTER after OVER is a syntax
  error); window definitions `[base] [PARTITION BY ...] [ORDER BY
  ...] [frame]`; frames `ROWS | RANGE | GROUPS` with one bound or
  `BETWEEN` two (UNBOUNDED PRECEDING, `expr` PRECEDING, CURRENT
  ROW, `expr` FOLLOWING, UNBOUNDED FOLLOWING) and EXCLUDE NO
  OTHERS / CURRENT ROW / GROUP / TIES; the pin's "unsupported frame
  specification" (`ParseKind::UnsupportedFrame`); a frame offset
  that is not constant (a column, a call, a subquery) becomes NULL
  as `sqlite3WindowOffsetExpr` makes it; bound parameters count as
  constant (the bind walker reaches window definitions); the
  WINDOW clause after HAVING, one per core.
- Named windows (`sqlite3WindowChain` / `sqlite3WindowUpdate`):
  `OVER w` copies w whole (frame included); `OVER (w ...)` and a
  later `x AS (w ...)` inherit PARTITION BY / ORDER BY and refuse
  restating PARTITION BY, a second ORDER BY or a base with an
  explicit frame (`WindowOverride`); the latest definition of a
  name wins; the first definition's base is never looked up; names
  compare by their raw text, quotes included (`OVER [w]` names no
  window `w`), case-insensitively; unused windows are not resolved;
  a subquery sees only its own WINDOW clause (`NoSuchWindow`).
- Functions: row_number, rank, dense_rank, percent_rank, cume_dist,
  ntile, lag / lead (1-3 arguments: the offset is SQL arithmetic on
  the row's 1-based position, so `lag(x, -1)` is lead, `lag(x,
  'x')` the row itself, a fractional offset finds nothing; only the
  buffered rows exist, so `lag(x, -2)` is NULL), first_value,
  last_value, nth_value, and every aggregate — count / sum / total
  / avg / min / max / group_concat / string_agg and median /
  percentile / percentile_cont / percentile_disc — with FILTER.
  The built-ins take the pin's fixed frames, which also decide
  their window class.
- The frame engine (`src/window.rs`, `Sim`): a transcription of
  `sqlite3WindowCodeStep` / `windowCodeOp` / `windowCodeRangeTest`
  checked against the pin's `EXPLAIN` programs: rows arrive into
  the partition buffer one at a time, three cursors step rows into
  the aggregate, invert them out and return rows, with the
  countdown registers (`IfPos`), the `regEnd - regStart` rewrite of
  `a FOLLOWING AND b FOLLOWING`, the empty-frame shortcut, RANGE /
  GROUPS peer-group loops, the RANGE offset tests (DESC subtracts,
  NULLS LAST / DESC NULLS FIRST tested apart, a text or blob key
  takes no arithmetic, the term's collation) and the guards that
  keep the start behind the end. `sum` / `total` / `avg` follow
  that exact step / inverse sequence through the pin's KBN sum
  (`SumCtx::inverse`, the pin's `sumInverse`), so REAL results
  round identically and a sum stays REAL once a REAL row passed
  through the frame; `group_concat` / `string_agg` keep the pin's
  byte-length bookkeeping (`groupConcatInverse`); min / max over a
  frame not starting UNBOUNDED PRECEDING keep the last of equal
  values (the pin's ephemeral index), else the first. A frame with
  any EXCLUDE (NO OTHERS included) runs the pin's full scan: a
  fresh aggregate per row over the frame minus the excluded rows.
- Classes and output order: calls with equal windows (frame and
  EXCLUDE included; two windows over the same aggregate, `OVER
  (ORDER BY sum(v))`, are equal) share a class; classes are
  numbered in resolution order (result list, then ORDER BY); the
  pin nests the second class inside the first and so on, each
  sorting its input stably by partition and order keys, so rows
  come out in the first class's order. An ORDER BY term naming a
  result column (alias, ordinal, the same call; the pin's
  `resolveAlias` copy), and an alias inside any other ORDER BY
  term, is a *copy* of that column's calls: computed again, never
  in the first class (`sqlite3WindowLink` does not link it), so it
  nests below the result list's windows; the named column then
  shows the copy's values (the pin reads it back from the sorter
  key) — under DISTINCT the column keeps its own values and the
  term sorts by them (`window::order_copies`). A compound's cores
  sort by its ORDER BY, extended to every column under UNION /
  INTERSECT / EXCEPT (`multiSelectOrderBy`), with the same copies
  (`window::compound_sorts`). DISTINCT, the outer ORDER BY
  (stable) and LIMIT apply after the windows; a group LIMIT no
  longer stops a windowed query's groups early, nor does a
  reader's LIMIT bound a windowed derived table's input. Windows
  over groups see the groups in GROUP BY order with the innermost
  class's sort directions when it has as many keys as the GROUP BY
  (the rewrite's subquery ORDER BY the group sorter borrows) —
  never the outer ORDER BY's.
- Where calls may stand: result columns and ORDER BY terms of a
  SELECT core (and each VALUES row, a one-row core on the pin);
  anywhere else — WHERE, GROUP BY (ordinals and aliases included),
  HAVING, ON, LIMIT, write statements, CHECK, inside an
  aggregate's or another window call's arguments, PARTITION BY,
  ORDER BY or FILTER, a trigger's WHEN — the pin's misuse
  (`MisusedWindow`, also for an alias of a window call there and
  for a built-in without OVER); in a column DEFAULT the pin's "not
  constant" (`NotConstantDefault`); in the last recursive SELECT
  of a recursive CTE `RecursiveWindow`. Aggregates inside window arguments belong to the core
  (`sum(sum(c)) OVER (ORDER BY a)` over GROUP BY a), and make it
  an aggregate query.
- Errors (new `SchemaKind`s): `NotWindowFunction` (a scalar
  function, multi-argument min / max), `WindowDistinct`,
  `WindowClauseMisused` (FILTER on a built-in, an aggregate ORDER
  BY with OVER), `NoSuchWindow`, `WindowOverride`,
  `RecursiveWindow`,
  `RangeOffsetOrder`, `FrameStartOffset` / `FrameEndOffset` (ROWS /
  GROUPS: `MustBeInt` then >= 0; RANGE: a non-negative number,
  numeric text included — checked when a partition starts, so not
  over no rows), `NthValueArgument` (per returned row; per stepped
  row under EXCLUDE), `NtileArgument` (the partition's first row's
  value).
- Plumbing: `RExpr::Win(Box<WinCall>, children)` holds the
  arguments, FILTER, PARTITION BY and ORDER BY terms as children,
  so every walker (correlation, subquery collection, aggregate
  ownership, alias rebasing) sees them; without `window` the
  variant's payload is uninhabited and its walker arms compile
  away. The three projection paths (`project_rows`,
  `project_qp_async`, `sub_project`) evaluate each unit's inputs,
  call `window::compute`, then project in the window order with
  the values filled in. View / CTE expansion, temp routing and
  parameter binding walk window definitions.

**Oracle-probing notes** (live pin, results and `EXPLAIN`):
ties keep input order in every window sort (vdbesort is stable
here); three windows over one table nest first-outermost (proved by
row_number tie order); `sqlite3WindowLink` puts calls with unequal
windows into the subquery in result-list order; the window
sorter's order is the output order without an outer ORDER BY, an
outer ORDER BY re-sorts stably; `count(*), count(*) OVER () ...
WHERE 0` is `0|1` (one group); DISTINCT keeps the first row in
window order; `VALUES (row_number() OVER ()), (2)` runs (each row is
a core) while `INSERT ... VALUES (row_number() OVER ())` is misuse;
`EXPLAIN` of `ROWS 1 PRECEDING AND 1 FOLLOWING` and `RANGE 1
PRECEDING AND 2 FOLLOWING` matched the transcribed control flow op
for op (AggStep / AggInverse / IfPos / peer `Compare` / `Jump`
loops, the flush); `sum(x) OVER (ROWS 1 PRECEDING)` over 1, 2.5, 3,
4 ends `7.0`; `ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING` returns
every row from a fresh accumulator (total 0.0, sum NULL, count 0);
min / max with NOCASE ties: `'a'` after `'A'` in a sliding frame,
`'A'` from UNBOUNDED PRECEDING; `ntile(a)` reads the first row's a
only; offsets: `1.0` and `'1'` pass for ROWS, `1.5` and `(SELECT 1)`
fail, RANGE takes `1.5`, `'1'`, `' 1 '`, refuses `'x'`, `x'01'`, NULL
and negatives; `?1` in a frame offset is a constant there.

**Bugs found while testing (fixed, regression tests):** window
names were compared dequoted (`OVER [w]` found `w`); bound
parameters in frame offsets were neither treated as constant nor
bound (the bind walker did not reach window definitions).

**Review fixes (2026-11-03, regression tests in
`tests/window_v16.rs` and the CLI `ordering` batch):**

- A window call in a column DEFAULT (CREATE TABLE, ALTER TABLE ADD
  COLUMN) was stored, leaving a schema the pin refuses to load
  ("default value of column [b] is not constant"); it is now
  `NotConstantDefault`, as `exprNodeIsConstant` refuses any
  EP_WinFunc node.
- Windows over groups under an outer ORDER BY of the same length
  as the GROUP BY (`GROUP BY k ORDER BY k DESC`, `ORDER BY v
  DESC`) saw the groups in the ORDER BY's direction (`group_desc`
  borrowed it); they now see the GROUP BY order with the innermost
  window's directions, as the pin's rewritten subquery does.
- `f(...) over FROM t` (and before WHERE, UNION, LIMIT, ...) is an
  alias again: OVER followed by a reserved keyword is not the
  keyword.
- `cannot use window functions in recursive queries`: a window call
  in the rightmost recursive SELECT is `RecursiveWindow` (checked
  before the aggregate check; the anchor, earlier recursive
  SELECTs and subqueries may hold windows, as on the pin).
- `row_number(*)` and the other zero-argument built-ins take `*`
  (the pin's `f(*)` is `f()`); every other function with `*` is
  `WrongArgumentCount`, not `Unsupported`.
- ORDER BY terms naming window columns (and compound ORDER BYs)
  changed the pin's window nesting but not hipp's, so tie-dependent
  values (row_number, first_value, lag, group_concat over ties)
  differed — now modelled as above. Two windows ordered or
  partitioned by the same aggregate no longer form two classes
  (`same_expr` compares aggregates under `window`).
- Same pattern elsewhere: a trigger's WHEN was planned as `SELECT
  <expr>`, so window calls and aggregates there ran; it is now
  planned as a WHERE (`SELECT 1 WHERE <expr>`), giving the pin's
  `MisusedWindow` / `MisusedAggregate` when the trigger fires.

**Tests:** `tests/window_v16.rs` (20 tests; the review fixes
added `default_refusals`, `over_as_alias`, `recursive_cte_windows`,
`star_arguments`, `trigger_when_misuse`,
`groups_ignore_outer_order`, `order_by_window_columns` and
`generated_order_group_corpus` — ORDER BY aliases / ordinals /
negated aliases / fresh calls, DISTINCT, LIMIT, GROUP BY with
aggregates in window arguments and sort keys; about 6 000 such
generated queries and 1 000 compound ones from a scratch fuzzer
agreed in values with the pin while fixing): built-ins, aggregates
over frames, RANGE offsets, named windows (and every refusal),
the step / inverse order (REAL stickiness, rounding, overflow),
group_concat's inverse with varying separators, output order
(nested classes, DISTINCT, LIMIT, joins order-insensitively),
windows over groups, windows in subqueries / views / CTEs /
compounds / VALUES / CTAS / INSERT ... SELECT / UPDATE / DELETE,
every refusal kind, bound parameters through the Statement API
(re-executed), and a generated corpus (xorshift, `HIPP_WIN_CORPUS`
default 240, `HIPP_WIN_SEED`) of random calls over random frames,
partitions and orders on six random tables with mixed storage
classes, rounding REALs and big integers — 1 500 extra statements
(seed 7) and about 2 400 more from a scratch fuzzer agreed while
developing. `hipp-cli/tests/cli_v16_window.rs` (5 tests):
byte-identical batches one-shot and over stdin (built-ins, frames
with REAL rendering, shapes, CTAS `.schema` and `.parameter`
offsets) plus the oracle's integrity check of hipp's file.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed".

**Size:** tiny **76,476 B** (Δ **0**: nothing of it is in the tiny
profile). Full **482,304 B** (Δ **−24 B** vs 482,328, ratchet
lowered: `RExpr::OutRef`, never built, gave way to the
uninhabited-without-`window` `Win` variant at the enum's end). Max
**524,224 B** (Δ **+41,912 B** vs 482,312, ratchet raised to it;
over the 32 KiB justification threshold): the frame engine and its
functions (`src/window.rs`), the grammar (OVER, WINDOW, frames) and
the resolver for calls and named windows, and the window paths of
the three projection loops — the whole leaf, which ADR 0002 put in
`max` for exactly this.

**Size after the review fixes:** tiny **76,924 B** (Δ **0**),
full **484,644 B** (Δ **0**: the `same_expr` aggregate arm and the
window paths are `window`-only), max **563,552 B** (Δ **+3,312 B**
vs 560,240, ratchet raised: ORDER BY / compound copies, the class
numbering, the group-direction rule and the recursive-CTE check).

**Honest gaps:**

- The parser's depth model counts nothing for window definitions
  (their expressions are parsed with an approximate stack base);
  an extreme nesting inside an OVER clause is bounded by hipp's
  limits, not at the pin's exact boundary.
- When the pin fails after printing rows (an `ntile` / `nth_value`
  / sum-overflow error in a later partition), hipp returns the
  error alone; the tests compare the refusal only.
- Percentile window aggregates are recomputed over the frame per
  row (each stepped row is checked as on the pin, but a P that
  varies per row is only checked across the frame, not against the
  first row ever stepped); min / max over sliding frames and
  EXCLUDE frames are rescanned per row: O(frame) per row (rank and
  peer groups are O(1) after an O(n) pass; sum / count /
  group_concat are incremental).
- A WINDOW definition no call uses is never resolved, so ALTER
  TABLE RENAME COLUMN leaves its column names unchanged in a view
  (the pin's rename walks it).
- Row order with ties between joined rows follows hipp's join
  order, which may differ from the pin's planner; the tests use
  unique keys or compare such results as sets.
- Errors from different calls / rows in one statement may be
  reported in another order than the pin's (the kinds and
  errored-vs-succeeded match).
- ORDER BY copies follow the plain-SELECT rule in every core; when
  the pin materializes a subquery or CTE with an ORDER BY naming a
  window column (`MATERIALIZED`, or its planner's choice), it keeps
  the column's own values and sorts by them, as under DISTINCT, so
  tie-dependent values there can differ. The identical-expression
  form of an ORDER BY reference is recognised for a bare window
  call only (`ORDER BY row_number() OVER (...)`), not for a larger
  expression holding one.
- Row order across the cores of a compound with an ORDER BY
  differs from the pin's merge for rows tied on the ORDER BY (not
  window-specific: `SELECT b, k FROM t UNION ALL SELECT 0, 0 ORDER
  BY 2 DESC` shows it too); the window values match, and the tests
  compare compounds as sets. A LIMIT over such ties can then keep
  other rows than the pin.

### Triggers (2026-11-03)

**Status:** implemented behind the new leaf feature `triggers`
(implies `v16`; in `max`, not `full`, per ADR 0002; its own
`ci/feature-sets.txt` line `v16,triggers`), differentially tested
against the live 3.54.1 pin. Ungated (every profile): a file's
triggers are never skipped silently any more — hipp used to run an
INSERT into a pin-made table with a trigger and simply not fire it.

**Shipped (`triggers`):**

- `CREATE [TEMP] TRIGGER [IF NOT EXISTS] [schema.]name [BEFORE |
  AFTER | INSTEAD OF] DELETE | INSERT | UPDATE [OF col, ...] ON
  table [FOR EACH ROW] [WHEN expr] BEGIN stmt; ... END`, stored
  with the pin's text (`CREATE TRIGGER ` and the statement from the
  bare name to its END) and `tbl_name` as written after ON; `DROP
  TRIGGER [IF EXISTS] [schema.]name`; trigger names a namespace of
  their own per database; the pin's refusals (duplicate name,
  reserved `sqlite_` name, system table, missing table, BEFORE /
  AFTER on a view, INSTEAD OF on a table, qualified INSERT / UPDATE
  / DELETE targets, RETURNING, DEFAULT VALUES, WITH before a
  write, parameters, RAISE outside a trigger — a CHECK holding one
  is accepted until a row is written, as on the pin). DROP TABLE
  and DROP VIEW drop the triggers on them (ungated).
- Firing (`session/trigger.rs`): triggers fire most recently
  created first. Each program statement is parsed again from the
  stored text, its `NEW.` / `OLD.` references bound to the row
  (`sql/trig.rs`: no comparison affinity and the column's collation
  for a column, INTEGER affinity for the rowid / INTEGER PRIMARY
  KEY; a FROM source or write target called `new` / `old` shadows
  them), planned and stepped as a statement of its own on the same
  connection; it never commits or rolls back itself. INSERT: BEFORE
  sees defaults, affinities and generated columns, `NEW.rowid` -1
  until chosen; rowid, constraints and the write follow; AFTER sees
  the stored row. UPDATE / DELETE: rows collected first, each read
  again at its turn (gone: skipped), BEFORE triggers, read once
  more (UPDATE: unassigned columns take the current values; NEW's
  generated columns in BEFORE UPDATE are computed from the assigned
  columns and those a firing BEFORE UPDATE trigger reads as NEW.x,
  the others NULL — review fix), write, AFTER
  triggers with the first-read OLD. `UPDATE OF` matches the SET
  list. WHEN clauses, RAISE(IGNORE / ROLLBACK / ABORT / FAIL, msg)
  (the message is any expression; `Connection::raise_message`),
  `PRAGMA recursive_triggers`, the pin's depth limit
  (`Config::max_trigger_depth`, the CLI sets the pin's 1000).
  Failure resolution as the pin's: an explicit OR clause of the
  firing statement overrides every body statement's (and cascades),
  a constraint failure resolves by the body statement's action or
  the table's ON CONFLICT ROLLBACK, FAIL keeps the changes so far,
  ROLLBACK ends the transaction, IGNORE skips the row and the
  remaining triggers of it. changes() / last_insert_rowid() and the
  firing statement's count are a frame per program; a failed
  program statement counts nothing in total_changes(). REPLACE
  deletions fire DELETE triggers under recursive_triggers (an
  IGNORE keeps the row and the write then fails; the row is checked
  again after them, under ABORT); an upsert's DO UPDATE fires the
  UPDATE triggers, its statements under ABORT.
- INSTEAD OF triggers make a view writable: INSERT (VALUES or
  SELECT, column lists), UPDATE (`SELECT *, <set values> FROM view
  WHERE ...`, an alias qualifies nothing — the pin's quirk) and
  DELETE fire them per view row with no affinity on NEW; the view
  itself changes nothing (`changes()` 0); "cannot modify v because
  it is a view" when no trigger (with a matching `UPDATE OF`) exists;
  no upsert into a view.
- Prepare-time checks: every body statement of the triggers a write
  may fire is planned over all-NULL rows when the write is prepared
  (the pin compiles them then), so "no such column: old.a" fails
  even an `INSERT ... SELECT ... WHERE 0`; value-only errors wait.
- ALTER TABLE RENAME / RENAME COLUMN / DROP COLUMN edit trigger
  texts token by token as the pin: the ON table, UPDATE OF columns,
  NEW / OLD columns, write targets with their column lists and SET
  targets, upsert targets, `excluded.` columns (RENAME TO renames
  the `excluded` qualifier itself, as the pin does), and every
  reference the statements resolve to the table (planned as SELECTs
  under the existing alter trace). Every trigger must read before
  and after the change, as the pin checks it (SET targets and
  INSERT column lists are not checked there: dropping a column only
  a SET names succeeds on the pin, and here).
- TEMP: a trigger on a TEMP table (TEMP or not, as on the pin) lives
  in the temp database; `temp.` qualifiers, `DROP TRIGGER
  temp.x`, the pin's "temporary trigger may not have qualified
  name" / "cannot reference objects in database temp".
- hipp-cli splits statements with the pin's `sqlite3_complete`
  state machine (a trigger ends at a `;` after `; END`), `.schema`
  shows triggers (`CREATE TRIGGER temp.` for the temp database's)
  and `.dump` writes views, triggers, then indexes after the data
  (the pin's order, `\n;` after a text holding `--`).

**Ungated fixes:** writes (INSERT, UPDATE, DELETE, REPLACE,
upsert) to a table with a trigger are refused without `triggers`
(`SchemaKind::TableHasTriggers`; a trigger hipp cannot parse refuses
them with it), every profile — tiny included; DROP TABLE / DROP
VIEW drop their triggers; `0 AND x` / `x OR 1` with an integer
literal operand no longer evaluate `x` (the pin simplifies them:
`0 AND abs(-9223372036854775808)` is 0, not "integer overflow");
hipp-cli's `.dump` object order.

**Oracle-probing notes** (live pin): stored text from the bare name
(`main.`, TEMP, IF NOT EXISTS gone); triggers on `T` stored with
`tbl_name` `T`; firing order is reverse creation for main triggers
(TEMP triggers on main tables fire first, in creation order — not
built here); NEW in BEFORE INSERT has defaults / affinity /
generated columns and rowid -1; `new.a = '5'` over INT 5 is 0,
`new.rowid = '1'` 1, `'B' = new.b` (NOCASE) 1; a view's NEW has no
affinity; DELETE / UPDATE re-read rows (row 2 deleted by a BEFORE
trigger is skipped, row 3 updated by one is seen updated); BEFORE
UPDATE's NEW generated columns are computed over the pin's new-column
mask (see the review fixes); recursion: a trigger never
re-enters itself without recursive_triggers, other triggers on the
same table do; 1000 programs pass, the 1001st is "triggers nested
too deep", counted before WHEN; RAISE(IGNORE) in a BEFORE trigger
abandons the row and the row's other triggers, in a nested
statement only that statement's row; FAIL / ABORT / ROLLBACK as
documented; the conflict-override matrix (7 outer × 6 body forms,
inside BEGIN); changes() inside a program is the last completed
statement's of that frame, else the frame's entry value;
last_insert_rowid() restored after a program; REPLACE + delete
triggers only under recursive_triggers, an IGNORE there turns into
"UNIQUE constraint failed"; upsert DO UPDATE fires BEFORE INSERT
then UPDATE triggers (not AFTER INSERT), its update under ABORT
(an OR IGNORE body statement fails); INSTEAD OF: `changes()` 0,
RETURNING returns NEW, `UPDATE v AS w SET a = w.a` is "no such
column: w.a"; body errors at prepare (zero rows too); ALTER texts
(quoted forms `old."aa"`, `OLD."aa"` from `[a]`, `"t 2".id`,
`"uu".xx` for `excluded.x`); every ALTER checks every trigger; the
shell's stdin reading follows `sqlite3_complete` (`BEGIN END;`
swallows the following lines). A depth error inside BEGIN keeps the
statement's changes on the pin when its program has no abort point
(no statement journal) — modelled since the review fixes.

**Bugs found while testing (fixed, regression tests in
`tests/triggers_v16.rs::regressions` and the matrices):** a failed
program statement counted in total_changes(); program statements
opened statement savepoints and so undid their own part of a FAIL;
an upsert's update ran its triggers under the outer conflict
action; a REPLACE victim kept by IGNORE, or re-created by its
DELETE triggers, was overwritten; RAISE() was folded at plan time
(`SELECT RAISE(FAIL, 'f') WHERE 1 = 2` raised); a RAISE(IGNORE)
left a stale failure resolution that turned a later FAIL into
ABORT; the depth limit counted one level early.

**Tests:** `tests/triggers_v16.rs` (22 tests): stored text and the
namespace, INSERT order / NEW values / counters, UPDATE OF / WHEN /
collation, NEW / OLD affinity, re-read rows, generated columns, the
RAISE matrix (7 × BEFORE / AFTER), RAISE messages and IGNORE
nesting, the conflict-override matrix (42 scripts plus UPDATE /
DELETE / table-level ROLLBACK), recursion and depth (999 / 1000 /
1001, hipp's lowered limit), REPLACE and upsert (both pragma
values), INSTEAD OF views, prepare-time errors, savepoints and
statement rollback, three ALTER corpora, TEMP triggers, the TEMP
trigger on a main table refusal, files the pin wrote (and the pin
firing hipp's triggers), regressions, and a generated corpus
(xorshift, `HIPP_TRIGGER_CORPUS` default 40, `HIPP_TRIGGER_SEED`)
of random triggers (timing, event, OF, WHEN, RAISE, recursion,
nested writes) over random writes, transactions and savepoints —
about 3 600 generated scripts over nine seeds agreed while
developing. Every script also checks the oracle reads hipp's file
(integrity check and `.dump` equal). `tests/triggers_off_v16.rs` (3
tests, also built and run in the tiny profile and `full`): a
pin-made file's triggered table refuses writes without the feature
and fires with it, other tables stay writable, DROP TABLE / VIEW
drop the triggers. `hipp-cli/tests/cli_v16_triggers.rs` (6 tests):
multi-line triggers over stdin, the `sqlite3_complete` edge cases,
RAISE and 1000-deep recursion, `.schema`, `.dump` (byte-identical
but for the pin's PRAGMA line, replayed into both engines), one-shot
scripts. `tests/alter_v16.rs` and `hipp-cli/tests/cli_v04.rs` were
updated (a file with triggers now alters as on the pin; `.dump`
writes views before indexes, as the pin).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed".

**Size:** tiny **76,924 B** (Δ **+196 B** vs 76,728, ratchet
raised to it): a correctness fix on the INSERT path the tiny
profile claims — the catalog notes the tables a file's triggers are
on (`TableDef::triggered`) and INSERT refuses them
(`TableHasTriggers`) instead of silently not running the trigger;
within the +256 B allowance. Full **484,644 B** (Δ **+528 B** vs
484,116, ratchet raised): the same refusal in UPDATE / DELETE, DROP
TABLE / DROP VIEW removing triggers, and the `0 AND x` / `x OR 1`
fix — no trigger code. Max **560,240 B** (Δ **+33,868 B** vs
526,372, ratchet raised; over the 32 KiB justification threshold):
the whole leaf — CREATE / DROP TRIGGER, the binder, the program
runner and its failure resolution, the per-row hooks of every write
path, INSTEAD OF planning and the ALTER trigger editor — which ADR
0002 put in `max` for exactly this.

**Honest gaps** (as of the review fixes below):

- A TEMP trigger on a main table is refused (`Unsupported`), as is
  a trigger in the temp database whose program writes a main table
  ("no such table" when it fires — an error, never a skipped
  trigger): the TEMP database is a separate private connection and
  a statement runs in one database only. Firing across the two
  needs one statement journal over both connections, and DROP /
  ALTER of the main table propagated into the temp schema; that is
  a change of the TEMP design, not of this unit.
- UPDATE ... FROM of a view is refused (`Unsupported`). A RETURNING
  subquery of a view write whose FROM names the view itself is
  computed once (the pin computes it per row: hipp reads the view as
  a derived table there).
- `0 AND nosuch` is still "no such column" (the pin drops the other
  operand unresolved).
- A trigger program's INSERT ... SELECT / DELETE / UPDATE stopped
  part-way by a RAISE keeps the rows before the failing one only for
  the pin's streaming shapes hipp re-runs row by row (a lone scan of
  one table without sort, grouping, compound, subquery or window;
  UPDATE / DELETE one-pass): a join or a derived table there keeps
  none. An UPDATE whose WHERE the pin answers through an index it
  also updates is treated as one-pass (the pin is not).
- A WHERE evaluated past a LIMIT still errors (hipp evaluates the
  whole WHERE first; the pin stops at the LIMIT) — a query-engine
  gap that a body's `INSERT ... SELECT ... LIMIT` shows too.
- The abort-point analysis behind a depth failure in a transaction
  is conservative (an unrecognised construct counts as an abort
  point, i.e. hipp undoes the statement) and assumes nothing of
  foreign keys (any `PRAGMA foreign_keys=ON` counts as one).
- Declared `ON CONFLICT REPLACE` on a column constraint is not
  honoured (the constraints unit's gap); the REPLACE victim order
  matches the pin for OR REPLACE / REPLACE statements.
- Trigger programs are parsed and planned per row: correct, but
  O(rows × program size) planning work. Each nested program costs
  about 15 KiB of stack in a release build and 262 KiB in a debug
  one (`STACK_PER_TRIGGER`); the default `max_trigger_depth` is 24.
  A body statement with deep expressions costs its own statement
  budget per level on top.

### Triggers — review fixes (2026-11-03)

Thirteen review findings, each reproduced on the live pin first.

**Fixed (all with regression tests in `tests/triggers_v16.rs`,
`review_*`, a generated corpus, and `hipp-cli/tests/cli_v16_triggers.rs::review_fixes_render_as_the_pin`):**

- BEFORE UPDATE: NEW's generated columns are computed, not NULL —
  from the assigned columns and the columns some firing BEFORE
  UPDATE trigger (WHEN included, `UPDATE OF` respected) reads as
  `NEW.x`, the others NULL, columns past the 32nd always loaded (the
  pin's `sqlite3TriggerColmask`).
- A RAISE(FAIL | IGNORE) part-way through a body's INSERT ...
  SELECT, DELETE or UPDATE keeps the rows written before it where
  the pin streams (INSERT ... SELECT: target without INSERT
  triggers, SELECT not reading it, a lone scan without sort /
  grouping / compound / subquery; DELETE / UPDATE one-pass: no
  triggers for the change, no foreign keys, no subquery in WHERE, no
  rowid change, no REPLACE). hipp re-runs such a SELECT row by row
  to the failing row (`insert_prefix`) and keeps the scanned prefix
  of a WHERE (`scan_write_rows_into`, `one_pass`); IGNORE's rows
  count in total_changes(). Found while testing: a trigger program's
  multi-row VALUES insert into a table with INSERT triggers now
  computes every row first, as the pin's temporary table does.
- AND / OR (ungated, parser): only a lone integer token of a 32-bit
  value (decimal or hex, in any parentheses) or TRUE / FALSE
  short-circuits the other operand; `-1`, `+0`, `-0`, `1_0`,
  `2147483648`, `0x80000000`, `1+0` leave it evaluated (marked with a
  unary `+` the evaluator's `int_lit` does not see), so `-1 OR
  abs(-9223372036854775808)` is "integer overflow" and a trigger's
  `+0 AND RAISE(ABORT, ...)` raises, as on the pin.
- REPLACE across several UNIQUE indexes deletes the conflicting rows
  most recently created index first (ungated: the write plans list
  indexes in the pin's prepended order), visible in DELETE triggers.
- RETURNING: an uncorrelated subquery whose FROM does not name the
  written table directly is computed once, when the first row
  reaches it (`ret_hoist`), so later AFTER-trigger (or foreign-key
  action) writes do not show; one naming it is per row, as probed.
  UPDATE's RETURNING now runs before the row's AFTER triggers, as
  INSERT's and DELETE's. Ungated: `RETURNING t.*` is the pin's error
  (`SchemaKind::ReturningTableStar`; hipp expanded it).
- Prepare-time checks cover the whole trigger nest (each trigger
  once, at most `max_trigger_depth` deep, whatever the WHEN
  clauses), plus the DELETE triggers a REPLACE fires under
  recursive_triggers.
- `Config::default().max_trigger_depth` is 24 (`DEFAULT_TRIGGER_DEPTH`;
  100 overflowed a 2 MiB thread in some builds and an 8 MiB debug
  main thread); `Config::stack_budget` lowers it to a quarter of its
  room over `STACK_PER_TRIGGER` (15 KiB release / 262 KiB debug,
  measured). README no longer claims the 1000-program limit for the
  library (the CLI sets it).
- A trigger-depth failure inside an explicit transaction keeps the
  statement's changes when no program of it holds an abort point
  (`nest_may_abort`: non-inlined function calls — coalesce, ifnull,
  iif, likely, unlikely, likelihood and count / sum / total / avg /
  group_concat are none, nullif and LIKE are — RAISE(ABORT), ABORT
  constraints of written tables, foreign keys), as the pin without a
  statement journal; probed over 12 body shapes.
- Writes of a view through INSTEAD OF triggers take RETURNING (NEW
  for INSERT / UPDATE, OLD for DELETE, after the triggers, rows a
  RAISE(IGNORE) skipped not returned, no rowid, no affinity) and an
  assignment of the view's rowid (the triggers fire, nothing
  changes).
- `CREATE TRIGGER main.x ... ON tt` resolves `tt` in main ("no such
  table" over a temp-only table; over both, the trigger goes on
  main's), and `ON temp.tt` with a `main.` name is "cannot reference
  objects in database temp".

**Not fixed:** the TEMP-trigger-on-main-table pair (see the honest
gaps above: a design change of the TEMP database).

**Oracle-probing notes:** the BEFORE UPDATE mask counts only
triggers that fire for the SET list; INSERT ... SELECT with ORDER
BY, GROUP BY, UNION ALL over the target, or INSERT triggers on the
target keeps nothing; DISTINCT and WHERE stream; DELETE over an
index streams; a DELETE / UPDATE WHERE with a subquery does not;
`2147483647 OR x` is simplified, `2147483648 OR x` not,
`00000000000002 OR x` is; RETURNING `(SELECT (SELECT count(*) FROM
t))` and `(SELECT count(*) FROM (SELECT * FROM t))` are computed
once, `(SELECT count(*) FROM log UNION ALL SELECT 1 FROM t)` per
row; a view's RETURNING `(SELECT count(*) FROM log)` once, after the
first row's INSTEAD OF triggers; `RETURNING v.*` is refused like
`t.*`.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed".

**Size** (vs the tree after the foreign-key unit): tiny **76,924 B**
(Δ 0); full **486,076 B** (Δ **+688 B**, ratchet raised: the
ungated AND / OR literal rule in the parser, the index order, the
RETURNING `t.*` refusal); max **599,256 B** (Δ **+8,824 B**, ratchet
raised: the leaf's fixes — new-column mask, part-way writes,
RETURNING cache and view RETURNING, nested validation, abort-point
analysis).

### Foreign-key enforcement (2026-11-03)

**Status:** implemented behind the new leaf feature `foreign_keys`
(implies `v16` and `indexes`; in `max`, not `full`, per ADR 0002; its
own `ci/feature-sets.txt` line `v16,foreign_keys`), differentially
tested against the live 3.54.1 pin. Without the leaf, wave 3's
refusal stays: while `PRAGMA foreign_keys` is ON a write to a table
in a foreign key is refused (`Unsupported`), never run unchecked.

**Shipped (`foreign_keys`, `src/session/fkey.rs`):**

- The pin's **counters**, not a per-row verdict: a child row added
  whose parent key is missing counts +1, a child row removed whose
  parent is missing −1 (only while the counter is non-zero); a parent
  row removed counts +1 per child still naming it, a parent row added
  −1 per child it satisfies (again only while non-zero; the counter
  can then go negative — the pin's quirk, so an orphan left with the
  pragma off can cancel another statement's violation). Immediate
  keys count per top-level statement and fail it at its end
  ("FOREIGN KEY constraint failed", `SchemaKind::ForeignKeyViolation`,
  the statement undone even under OR FAIL / IGNORE / ROLLBACK, the
  transaction staying open); `DEFERRABLE INITIALLY DEFERRED` keys
  (a column-level deferrable clause applies to the table's most
  recent key, as `sqlite3DeferForeignKey`; `NOT DEFERRABLE INITIALLY
  DEFERRED` is immediate) and every key under `PRAGMA
  defer_foreign_keys` count per transaction: COMMIT and a committing
  RELEASE fail with the transaction open, an autocommit statement
  fails at its end, a savepoint saves the counters and ROLLBACK TO
  restores them, a statement rollback restores its start values.
  The pin's single-row special case: a top-level single-row INSERT
  (no INSERT triggers, RETURNING, REPLACE or upsert update) fails at
  once on an immediate key and, as a parent, resolves nothing.
- **Matching** as the pin: a child value takes the parent column's
  affinity and is looked up in the parent's PRIMARY KEY / UNIQUE
  index (`MustBeInt` for a rowid parent; a non-integer has none), a
  self-referencing new row may be its own parent; parent-side scans
  compare `parent = child` with both comparison affinities and the
  parent's collation; action programs compare `OLD.pk = fk` with no
  affinity on OLD (INTEGER for the rowid) — so a cascade can miss
  rows the count found (probed: a REAL parent and TEXT children
  '1', '1.0', ' 1': the count finds three, the CASCADE deletes one,
  the DELETE fails).
- **Actions** after the row's change (BEFORE triggers, checks,
  write, actions, AFTER triggers — the pin's order): ON DELETE /
  ON UPDATE CASCADE, SET NULL, SET DEFAULT (the child column's
  default; NULL for a generated one), RESTRICT (immediate even for a
  deferred key, skipped under defer_foreign_keys), NO ACTION; ON
  UPDATE only when `NOT (old IS new)` under the parent collation.
  Each runs as the pin's action program: a statement built as an AST
  over `Expr::TrigValue` (now shared by `triggers` and
  `foreign_keys`), under ABORT whatever the firing statement says
  (its triggers' bodies inherit ABORT — an `INSERT OR IGNORE` there
  fails), a frame of its own for `changes()` / `last_insert_rowid()`,
  counted towards `Config::max_trigger_depth` (now also a
  `foreign_keys` setting), firing the child's triggers when
  `triggers` is built; its failure aborts the firing statement.
- **Keys resolve when the write is prepared** (`fk_validate`), for
  the whole program tree the pin compiles then (a mismatch two
  cascades away fails the statement): "foreign key mismatch"
  (`ForeignKeyMismatch`) for a parent key that is no PRIMARY KEY /
  UNIQUE index with the parent columns' collations, "no such table"
  for a missing parent — only for the keys a statement uses, with
  the pin's rules (a single-row INSERT into the parent uses none; an
  UPDATE that changes any key locates every key on its table, its
  `sqlite3FkOldmask`).
- **REPLACE and upsert:** REPLACE victims are deleted with their
  checks and actions (an UPDATE OR REPLACE loads all the table's
  keys for them); DO UPDATE checks as an UPDATE. The pin's
  `isSetNullAction` quirk is reproduced for top-level writes: when
  a REPLACE compiles the deletion programs of a self-referencing
  ON DELETE SET NULL key last, that key's new-row lookups are
  skipped (probed: `INSERT OR REPLACE INTO t VALUES (1, 99)` succeeds
  with 99 missing; not when compiling that program compiles UPDATE
  triggers or ON UPDATE actions after it).
- **DROP TABLE** first runs the pin's implicit `DELETE FROM` when
  the table is a parent, or a child with deferred keys and
  outstanding deferred violations (its own triggers silent, missing
  parents empty, its unusable keys ignored; cascades run), then
  fails before dropping on immediate violations; its count is the
  drop's `changes()` when the pin compiles that DELETE.
- RETURNING: immediate violations withhold the rows; deferred ones
  fail an autocommit statement after its rows, as the pin's
  `OP_FkCheck` / halt order.
- `PRAGMA foreign_key_check [table]` (`table|rowid|parent|fkid`,
  rows in rowid order, keys in list order, the parent as written, a
  missing parent table reporting every keyed row, a mismatch failing
  the pragma before any row) and `PRAGMA defer_foreign_keys` (cleared
  when a transaction ends, as on the pin). `ALTER TABLE ... ADD
  COLUMN` of a REFERENCES column with a non-NULL default is refused
  while the pragma is ON (ungated under `v16`,
  `SchemaKind::ReferencesDefault`). Wave 3's ALTER TABLE RENAME
  already rewrote REFERENCES in child tables (verified; tested here).
- The parser records `ForeignKey::deferred`. COMMIT also checks the
  TEMP database's counters.

**Oracle-probing notes** (live pin, scripts in the unit's
scratchpad): the counter semantics above, including `INSERT INTO s
VALUES (7, 200), (100, NULL)` resolving an orphan 6→100 left with the
pragma off and so admitting 7→200; `INSERT OR FAIL` with a counted
violation undoes the whole statement; `INSERT OR IGNORE` of a row
that conflicts never counts it; `INSERT OR ROLLBACK` keeps the
transaction; RETURNING rows are not printed for an immediate
violation but are for a deferred one in autocommit; COMMIT /
RELEASE failures leave the transaction (and `defer_foreign_keys`)
on; `defer_foreign_keys` resets when any transaction ends, also a
read in autocommit (`SELECT * FROM t`), not after `SELECT 1` or a
pragma; `PRAGMA foreign_keys` is a no-op inside a transaction;
changes() = 1 and total_changes counts the cascades; inside cascade
programs changes() is the frame's entry value; the order BEFORE
DELETE trigger, action programs (grandchild SET NULL inside the
child's delete), AFTER DELETE trigger; mismatch / no-such-table
errors are prepare-time ("Parse error"), and which keys a statement
locates; a TEMP child may only reference a TEMP parent and a main
child only a main one ("no such table: temp.p"); MATCH clauses are
accepted and `foreign_key_list` says NONE; cascade depth: a
chain past the 1000-program limit fails ("triggers nested too deep",
the whole statement undone); DROP TABLE runs the implicit DELETE with cascades, not
its own triggers, and leaves `changes()` at its count (3), even 0
when the DELETE is compiled and skipped; a failed DROP's cascades
stay in total_changes(). `isSetNullAction`: the skip depends on
which program the pin compiled last (probed over a dozen schemas:
UPDATE triggers on the table and two self-referencing keys change
it; DELETE triggers and other tables' CASCADE / RESTRICT keys do
not). The whole-database `foreign_key_check` walks the pin's schema
hash table; its hash function could not be fitted from orders
probed for 1–40 tables (the PRAGMA unit's gap): hipp reproduces the
order up to three tables and views.

**Bugs found while testing (fixed; regression tests in
`tests/foreign_keys_v16.rs::regressions*`):** an UPDATE OR REPLACE
victim ran only the actions of the keys the UPDATE assigns;
RETURNING rows were withheld before a deferred failure; a key error
raised when the write ran (not when it was prepared) reset
changes(); mismatches inside the action-program tree, and the
`sqlite3FkOldmask` locate-all rule, were not reported; DROP TABLE's
DELETE reported its own unusable keys; `isSetNullAction`; an INSERT
counted DELETE / UPDATE triggers as making it multi-row; DROP TABLE
left `changes()` alone where the pin set it.

**Tests:** `tests/foreign_keys_v16.rs` (21 tests; also run with
`--no-default-features --features std,v16,foreign_keys`, i.e.
without `triggers`): counting and the orphan quirk, deferred keys
and savepoints, `defer_foreign_keys`, every action, affinity /
collation / composite keys in any column order, self-reference,
mismatch and missing parents, DROP TABLE, ALTER TABLE, REPLACE and
upsert, conflict clauses and RETURNING, `foreign_key_check`,
cascade depth (1000 / 1001), MATCH, the pragma off, triggers fired by
cascades (with RAISE), files the pin wrote (and the pin enforcing
hipp's), regressions, and a generated corpus (xorshift,
`HIPP_FK_CORPUS` default 60, `HIPP_FK_SEED`) of random parents (IPK,
UNIQUE, composite PK), children, grandchildren on UNIQUE columns,
self-referencing tables, actions, deferral, logging triggers and
random INSERT / REPLACE / upsert / UPDATE / DELETE / INSERT ... SELECT,
transactions, savepoints, pragma toggles, DROP TABLE and
`foreign_key_check` — about 5 000 generated scripts over 40 seeds
agreed while developing, each also checked by the pin reading
hipp's file (integrity check and `.dump`). `hipp-cli/tests/
cli_v16_foreign_keys.rs` (6 tests): cascades and checks, the check
output, deferred commits and RETURNING, a `.dump` replayed with keys
on, one-shot exit statuses, cascades firing triggers.
`tests/pragma_v16.rs`'s refusal test now runs in builds without the
leaf.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed".

**Size:** tiny **76,924 B** (Δ **0**; the plain INSERT path keeps
its tiny code: the key check is a `foreign_keys` block). Full
**485,388 B** (Δ **+744 B** vs 484,644, ratchet raised): the ungated
`v16` parts only — `ForeignKey::deferred` and the deferrable-clause
reading, and ALTER TABLE ADD COLUMN's "Cannot add a REFERENCES column
with non-NULL default value" check (`SchemaKind::ReferencesDefault`);
no enforcement code. Max **590,432 B** (Δ **+26,880 B** vs 563,552,
ratchet raised; under the 32 KiB justification threshold): the whole
leaf — key resolution, lookups and scans, the counters, action
programs, the write-path hooks, DROP TABLE's DELETE,
`foreign_key_check` / `defer_foreign_keys`.

**Honest gaps:**

- The whole-database `foreign_key_check` lists tables in hipp's
  approximation of the pin's hash order (exact up to three tables
  and views); `foreign_key_check(t)` is exact. `pragma_foreign_key_
  check(...)` as a table-valued function is refused.
- `isSetNullAction` is modelled for top-level writes and the
  nested programs named above; inside trigger / action programs,
  and where a program was compiled earlier in the same statement,
  hipp checks the new row (the pin's outcome depends on its whole
  compile order).
- Mismatch errors inside a trigger body's own action tree surface
  when the body runs (hipp validates one level at prepare).
- Child scans are full scans of the child table per parent row
  changed (O(parents × children)); child indexes are not used.
- SET DEFAULT evaluates a non-constant default once per action
  statement, not per row.
- `defer_foreign_keys` is cleared after table-reading queries and
  writes in autocommit, not after the pragmas that read the file
  (the pin's `bIsReader`); it is per database: setting it does not
  reach the TEMP database's statements.
- An action program's statement journal: a depth error inside
  BEGIN undoes the statement (as for triggers, STATUS above).

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
