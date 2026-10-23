# hipp — build status

Spec: `docs/SPEC.md` rev. 0.3. Oracle pin (D1): SQLite **3.54.1**
(official amalgamation 3540100, `~/workspace/hipp-design/oracle/sqlite3`,
`--version` verified). All differential results are against the pin.

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
