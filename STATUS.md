# hipp — build status

Spec: `docs/SPEC.md` rev. 0.3. Oracle pin (D1): SQLite **3.54.1**
(official amalgamation 3540100, `~/workspace/hipp-design/oracle/sqlite3`,
`--version` verified). All differential results are against the pin.

## v0.17 — parity backlog

Milestone v0.17 closes the honest gaps the v0.16 units recorded,
still against the 3.54.1 pin, under ADR 0001 (gating) and ADR 0004
(the `xl` profile for leaves past `max`). Each unit records itself
below.

### Streamed rows: virtual-table cursors and row-at-a-time statements (2026-11-05)

**Shipped** (`src/session/stream.rs`; the vtab cursors in
`session::vtab` / `session::rtree`):

- **A streamed top-level SELECT.** `Statement::step` returns each
  row as the join makes it, instead of materialising the result in
  the first step: the nested loop of `join_sources` unrolled into a
  resumable state machine (one level per FROM source, `join_sources`'
  row order, LEFT JOIN pads, the WHERE clause's constant terms first,
  the lateral pre-checks of `lateral_ok`). The first source is read
  through its cursor — a table's b-tree cursor, or a virtual table's
  xFilter / xNext / xEof / xColumn / xRowid (`vtab_open` /
  `vtab_next`); each inner source is opened again for every
  combination of the sources before it, so an inner virtual table is
  filtered again with the outer row's values, as the pin's xFilter
  (its arguments, and the usable `value = t.a`-style constraints the
  planner hands it). A source hipp computes as a whole (a derived
  table, view or CTE, a TEMP or attached table, an index lookup, a
  JSON table-valued function, an inner plain table) is computed the
  first time the join reaches it, not before the statement starts.
  So a statement failing on row N (a `json()` of malformed text, an
  `abs()` overflow, a LIKE ESCAPE error, a malformed `json_each`
  document of the third outer row) returns rows 1..N-1 first, as on
  the pin — for plain scans, joins, DISTINCT, an ORDER BY that the
  rowid or an index loop delivers, derived tables / views read by
  the streaming outer query, and every module (generate_series,
  dbstat, R*Tree, fts5, fts5vocab). The CLI prints those rows before
  the error, as the pin's shell does.
- **LIMIT / OFFSET.** LIMIT stops the join: `SELECT value FROM
  generate_series(1) LIMIT 3` returns its first row at the first
  `step` (it was VtabTooManyRows past `Config::max_vtab_rows`), and
  so does a join or a WHERE over an endless series. OFFSET rows are
  skipped before their result columns are computed, as on the pin
  (hipp computed them and could fail on a row it then dropped).
  LIMIT 0 reads nothing.
- **Constant-memory aggregates.** An aggregate query without GROUP
  BY / HAVING / ORDER BY whose aggregates are count / sum / total /
  avg / min / max (FILTER allowed; no DISTINCT, aggregate ORDER BY or
  subquery in the result) folds row by row (`agg::SumCtx` and the
  pin's min / max comparison), the bare columns' row kept by
  `pick_rep_from`'s rule. `count(*) FROM generate_series(1)` answers
  4294967295 as the pin does (in a release build; a debug build
  needs about half an hour for 2^32 rows); the tests run count /
  sum / min / max / total over generate_series(1, 50000000) under a
  64 KiB statement arena and `max_vtab_rows(1000)` with the thread's
  heap peak bounded (a counting allocator), against the pin's answer.
- **Blocking operators** buffer what they need, as the pin's sorters
  do: an ORDER BY that no loop delivers (the projected rows and keys;
  with a LIMIT only the OFFSET + LIMIT rows it may still return —
  sorted stably, so the earliest of equal rows stay), GROUP BY and
  the other aggregates (the joined rows, then the materialising
  executor's grouping projection), DISTINCT (the distinct rows so
  far). **`Config::max_vtab_rows` now applies only where rows are
  held**: per generate_series cursor opening under DISTINCT, GROUP BY
  / a non-folding aggregate, or an ORDER BY without LIMIT; and on the
  paths that still materialise a cursor (subqueries, derived tables
  and CTEs, compound members, RIGHT / FULL joins, window queries,
  trigger programs, INSERT ... SELECT / UPDATE ... FROM / CREATE
  TABLE ... AS sources) — documented on the field.
- **One executor under `v16`.** The synchronous v0.4 query executor
  (`materialise_query_sources`, `join_all`, `project_rows`) now
  serves only the builds without the rung; with it every top-level
  query that does not stream runs the async executor
  (`run_query_sub`), which the streamed modes also finish through.
  That kept `max` inside its budget.
- `IdxAccess::sorted` (leaf `expr_index`): how many ORDER BY terms
  an index loop delivers, outside `explain` too, so a lone source
  whose loop delivers the ORDER BY streams.
- A streamed statement's JSON documents and full-text rows (the
  `Env` side tables its rows point into) go as soon as nothing reads
  them: what a loop added when it ends (an inner lateral json_each's
  documents, an inner fts5 filter's rows), what a row's WHERE and
  result columns added once the row is dealt with; a cached source's
  rows and a kept row (an aggregate's bare-column row, a buffered
  GROUP BY row) pin theirs until the statement ends. A statement
  reset part-way drops them, and one dropped part-way leaves them to
  the next top-level statement's first step
  (`Connection::stream_left`).
- **Review fixes** (2026-11-05):
  - An ORDER BY term that is a result column (an alias or ordinal,
    or the same expression, under COLLATE too) sorts on the value
    the column returned, as the pin's `iOrderByCol`: `SELECT
    random() AS r FROM t ORDER BY r` came out unsorted (the key was
    a second evaluation), with or without LIMIT, in every executor
    (ungated, `v04`).
  - An aggregate query without GROUP BY drops its ORDER BY, as the
    pin (`sSort.pOrderBy = 0`): only the aggregates in its terms
    still accumulate, so `SELECT count(*) FROM t ORDER BY json('x')`
    answers instead of failing (ungated, `v04`); a window query
    keeps it, as on the pin. Such a query now folds in constant
    memory.
  - Recursive CTEs stream: a query reading one (first or inner
    source, a view over one, MATERIALIZED) returns the rows before a
    failing one; an endless one read through a WHERE or a join under
    a LIMIT is computed under a bound that doubles as the join reads
    it (`Stream::grow`, the materialising executor's growing bound).
  - A streamed join's memory no longer grows with its outer rows
    (the side tables above; 1.2M outer rows over a lateral json_each
    held 143 MB), and an inner fts5 / fts5vocab / dbstat source that
    reads nothing to its left is computed once per statement, not
    per outer row (it was ~12x slower than before the unit).
  - hipp-cli: a stdin script with a failed statement or dot-command
    exits 1 (at its end, or at `.exit`), and the statements after
    the failing one in the same input chunk are skipped, as the
    pin's shell does.

**Oracle-probing notes** (verified on the 3.54.1 pin):

- The pin's shell prints the rows a statement returned before its
  error, then the error; one-shot and `-bail` stop there, stdin
  scripts go on with the next input chunk (the rest of the failing
  chunk — its line, or the lines of an unfinished statement — is
  skipped) and exit 1 at the end or at `.exit`.
- An ORDER BY term naming a result column (alias — the first match,
  under COLLATE too —, ordinal, or `sqlite3ExprCompare`-equal
  expression, the last match) sorts on the returned value: `ORDER BY
  r` over `random() AS r` is sorted, `ORDER BY +r` and `ORDER BY
  random() COLLATE nocase` are not. A flattened subquery evaluates
  its random() column again (`SELECT r FROM (SELECT random() r ...
  ORDER BY r)` is not sorted on the pin).
- An aggregate without GROUP BY ignores its ORDER BY (HAVING or
  not, LIMIT or not); the aggregates in it still run
  (`ORDER BY sum(abs(...))` overflows), the rest of the term not
  (`ORDER BY sum(a) + json('x')` answers). With a window function
  the ORDER BY stays.
- A view's or subquery's result columns the outer query never reads
  are not computed (`SELECT a FROM v` answers where `v.b` would
  overflow) unless the subquery is DISTINCT, aggregate, windowed,
  MATERIALIZED or a compound other than UNION ALL, or its ORDER BY
  names the column.
- Streams: plain scans, joins (inner, LEFT, virtual tables and
  json_each either side), DISTINCT (each new row at once), ORDER BY
  the rowid or an index delivers (`ORDER BY a DESC` on an INTEGER
  PRIMARY KEY too), derived tables, views and CTEs (MATERIALIZED
  too) read by a streaming outer query, UNION ALL members.
- Buffers (no row before the error): ORDER BY of another column,
  GROUP BY / HAVING, aggregates.
- OFFSET rows' result columns are not evaluated (`json(... a = 1
  ...) LIMIT 1 OFFSET 1` succeeds); their WHERE is. LIMIT 0 runs
  nothing (`count(json('x')) ... LIMIT 0` succeeds). With DISTINCT or
  ORDER BY the skipped rows are computed.
- `json_each('[1,2,')` fails before any row (the document is parsed
  at xFilter); over `t, json_each(t.j)` the earlier documents' rows
  come first.
- The bare column of an aggregate without min / max is the first
  row's (`SELECT value, count(*) FROM generate_series(1, 5)` is
  `1|5`); with min / max the extremum row's.
- The pin reorders an inner join (and a LEFT JOIN whose WHERE
  rejects the NULL row, which it makes inner) by cost; the tests use
  CROSS JOIN, which it never reorders (join ordering is another
  v0.17 unit's).

**Tests:** `tests/stream_v17.rs` (differential, rows before the
error compared: plain scans, blocking operators, derived tables /
views, joins, inner virtual tables re-filtered, LIMIT early exit and
first-row latency, OFFSET, generate_series, dbstat on a pin-written
file, R*Tree, fts5 and fts5vocab, constant memory, streamed rows not
accumulating, the bounds of blocking operators and the top-N sort,
transactions with a statement failing part-way, reset / drop /
re-step part-way, a generated corpus of 160 joins mixing tables and
modules with error injection, LEFT JOIN, LIMIT and OFFSET);
`hipp-cli/tests/cli_v17_stream.rs` (byte-identical stdout over stdin
and one-shot, errors included; the exit status and the skipped rest
of a failing input chunk). Review regressions in `stream_v17.rs`:
ORDER BY a random() result column (alias, ordinal, the same
expression, LIMIT, DISTINCT, GROUP BY, a subquery's, CREATE TABLE
... AS), one-row aggregates dropping their ORDER BY, recursive CTEs
(bounded, endless under LIMIT, inner, through a WHERE / join with
the growing bound, a view, MATERIALIZED), and the side tables of a
streamed json_each / fts5 join bounded by the counting allocator. `tests/vtab_v16.rs`'s
`series_row_bound` now expects the pin's answer for a streamed
`count(*)` past the bound, and the bound for DISTINCT / ORDER BY.

**Gates:** `ci/local-gates.sh` (full run): fmt, clippy (default,
all features, every line of `ci/feature-sets.txt`), workspace tests,
the four no_std targets and the four size gates — "all gates
passed". `depth_v16` / `triggers_v16` also pass built with debug
info (the stream's futures are boxed off the step future, so
`Config::stack_budget`'s constants are unchanged).

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 500,952 → 505,416 (+4,464: the streamed executor, partly paid
for by retiring the synchronous executor under `v16`); max 1,035,516
→ 1,043,340 (+7,824: the above plus the cursor plumbing of `vtab` /
`rtree`); xl 1,035,516 → 1,043,292 (+7,776; no leaf of its own yet).
Part B (plain scans and joins) is an ungated fix of the `v16`
executor: tiny does not contain it, and full stays under its 512 KiB
budget. Review fixes (measured on top of the FTS5 delete unit): tiny
76,960 (0: ORDER BY and aggregates are `v04`); full 505,416 → 506,432
(+1,016); max 1,044,172 → 1,045,676 (+1,504; 2,900 B of headroom
left); xl 1,053,532 → 1,055,028 (+1,496). Each ratchet is its
measured figure (the xl ratchet had been left 48 B above its
measurement).

**Honest gaps:**

- A view's or derived table's result columns that the outer query
  never reads are still computed (closed in v0.17 by `flatten`: unused
  co-routine / materialisation columns are not computed, and flattened
  bodies never evaluate them; pre-existing; the pin omits them —
  see the probing notes): `CREATE VIEW v AS SELECT a, abs(a - 2 -
  9223372036854775807) AS b FROM t; SELECT a FROM v` fails with
  IntegerOverflow where the pin answers. Doing it safely needs the
  pin's column-usage rule over every expression a plan holds (index
  probes, virtual-table constraints and OR branches, lateral
  arguments as well as the clauses), which is larger than `max`'s
  remaining 2.9 KB; left for a query-flattening unit.
- Only a top-level SELECT streams. A scalar / IN / EXISTS subquery,
  a derived table's own rows, a compound (UNION ALL members stream on
  the pin), a RIGHT / FULL join, window queries, trigger-program
  SELECTs and the sources of INSERT ... SELECT / UPDATE ... FROM /
  CREATE TABLE ... AS still materialise: `SELECT (SELECT count(*)
  FROM generate_series(1))` is still VtabTooManyRows, and the
  `last_insert_rowid()`-inside-INSERT-...-SELECT gap of the function
  unit stays open.
- dbstat, fts5, fts5vocab and rtreecheck compute their rows at
  xFilter (bounded by the file) and hand them out row by row; only
  generate_series and the R*Tree search are incremental. So a
  module-side error part-way (a corrupt page or node) fails before
  the first row, and a LIMIT bounds only what a lone first source
  computes (`derived_need`, as before). On an inner side such a
  module, when it reads nothing to its left, is computed once per
  statement (the pin filters it again per outer row: the same rows).
- An endless recursive CTE read through a WHERE or a join under a
  LIMIT is computed again under each doubled bound; a
  non-deterministic CTE (random()) can then differ between the rows
  already returned and the rest. Without a LIMIT such a query does
  not end (as before; the pin streams it until interrupted).
- The ORDER BY / result-column match is structural (`same_expr`):
  an alias of a CASE or subquery column is evaluated again for its
  key, and of two equal non-deterministic columns the last is the
  key (the pin: the alias's own).
- An inner plain table is read once, at its first reach, and kept
  (as before); the pin reads it again per outer row.
- An ORDER BY delivered by a multi-source join's outer loop, or by
  a lone table's own order under the `full` profile (no
  `expr_index`) other than its rowid ascending, still sorts.
- DISTINCT is still quadratic in the distinct rows.
- The WHERE clause is tested on complete rows (as before): a term on
  the outer source only, which the pin tests at the outer loop, can
  fail there on the pin when the inner source is empty.

### FTS5 delete writes: tombstones and secure-delete (2026-11-05)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated: new leaf `fts5_delete` (requires `fts5`). Placement (ADR
0004): `max` + the leaf measures 1,053,532 B, past `max`'s 1,048,576 B
budget (4,404 B were left), so the leaf joins `xl` (`xl = ["max",
"fts5_delete"]`); its own `v16,vtab,without_rowid,math,fts5,fts5_delete`
line in `ci/feature-sets.txt`. Without the leaf the 0.16 refusals
stand (DELETE / UPDATE / REPLACE on contentless_delete tables
`Unsupported`, deletes under `secure-delete=1` `Fts5SecureDelete`).

**Shipped** (`src/fts5/sdel.rs`; the session glue in `session::fts5`):

- **Tombstones** (`contentless_delete=1`). DELETE, UPDATE and INSERT
  OR REPLACE look the row up in `%_docsize` (its sizes come off the
  totals, as the pin's averages show), then add its rowid to the
  tombstone hash of every segment whose origin range holds the row's
  origin (`sqlite3Fts5IndexContentlessDelete`: `nEntryTombstone` of
  the first found, oldest level first, `nContentlessDelete` per page
  write). The hash is the pin's (`fts5IndexTombstoneAddToPage` /
  `Rebuild` / `Rehash`): key size 4 until a rowid needs 8, the
  rowid-0 flag in byte 1, a page refused at half full, the first
  hash one 32-slot page, a single page regrown to `4 * nElem` slots
  up to `(pgsz - 8) / szKey`, then `2n + 1` pages, retried with
  `2n + 1` more until every key fits. The structure's counters change
  in the cached structure (`FtState::pstruct`, which queries in the
  same transaction read): written at once when a hash is rebuilt and
  at the next flush otherwise; the flush counts the tombstones as
  leaves of automerge work and runs even with no pending terms. An
  UPDATE must set every indexed column ("cannot UPDATE a subset of
  columns on fts5 contentless-delete table", `Fts5ContentlessSubset`,
  rowid changes included); an UPDATE of UNINDEXED columns alone is a
  no-op on any contentless table (it was refused). `deletemerge`,
  the merges' tombstone handling and the readers were already the
  pin's (0.16 review fixes); they now run on hipp-written files.
- **Secure delete** (`secure-delete=1`, every content mode: DELETE,
  UPDATE, OR REPLACE, the 'delete' command of contentless and
  external-content tables). A flush writes no delete markers: each
  pending delete (`fts5FlushOneHash`'s `bSecureDelete` branch, the
  term written only once something of it is) removes the row's entry
  from the newest segment holding it (`fts5FlushSecureDelete`,
  `fts5DoSecureDelete`), literally as the pin: the entry's bytes and
  the next entry's rowid delta re-based; the term too when it was its
  doclist's only entry (the next term on the page takes over its key
  with the shared prefix re-split), or the term at the end of an
  earlier page when every page in between is empty; overflowing
  position lists cut from the pages after (`fts5SecureDeleteOverflow`:
  emptied pages written as `00000004`, the rest shifted, the page
  index re-based); the first-rowid header cleared, the page index
  rebuilt without the removed key; the `%_idx` row of a page left with
  no term deleted (page 1 keeps its own). The doclist indexes are left
  stale, as on the pin. The first such delete sets `version` 5 and
  moves the cookie on; a delete whose pending entry is a delete plus
  a new position list rewrites the entry without its flag.
- The connection keeps what the pin's `Fts5Config` keeps: the
  `version` a statement loaded (a 'rebuild' / 'delete-all' writes 4
  under it; the commit's flush loads it again) and a `secure-delete`
  setting a rollback took the row of, until the pin would reparse
  its schema (review fix, below).

**Fixes on claimed fts5 paths** (ungated, leaf `fts5`, each found by
the new corpus and kept as a regression):

- A row with no tokens counts into the next segment's `nEntry` (the
  pin's `nPendingRow` outlives a commit that flushed nothing) —
  `contentless_delete` structure records differed.
- `rowid IN (x)` is the pin's `rowid = x` (its parser rewrites a
  one-value list): no statement-journal flush before it.
- `integrity-check` compares an external-content table with its
  content only with rank 1 (the pin's `iArg`): hipp reported
  corruption where the pin passes.
- bm25 / `rank` of `0 / 0` (rows of no tokens) is NULL, not 0.0;
  bm25, snippet and ORDER BY rank on a row with no `%_docsize` entry,
  or on a table whose totals count no row, are the pin's corruption
  error (they answered).
- An UPDATE to a NULL rowid is "datatype mismatch" (the old rowid was
  kept).

**Oracle-probing notes (live pin):**

- Tombstone pages: `FTS5_TOMBSTONE_ROWID(segid, i)`; byte 0 the key
  size, byte 1 the rowid-0 flag, bytes 4..8 the element count, slots
  big-endian. A first delete writes a 136-byte page (32 slots); the
  totals lose the row's `%_docsize` sizes. The structure record
  shows `nPgTombstone` / `nEntryTombstone` inside the transaction
  right after a hash rebuild, otherwise only after the flush; the
  write counter moves by one per tombstone at the flush.
- `version` moves to 5 (and the cookie on) at the flush that meets
  the first pending delete, even when the row's entry is not found;
  deleting a missing rowid writes nothing. The pin tests the delete
  flag on the **first byte** of the size varint, so a pending entry
  whose position list is 64..127 bytes long (first byte odd) also
  moves it.
- 'rebuild' / 'delete-all' write `version` 4 without the cookie: a
  flush later in the same statement still sees 5, the commit's flush
  (or the next statement) sees 4 and moves it again.
- A rolled-back `secure-delete` setting stays on in the connection
  (`sqlite3Fts5ConfigLoad` has no default for it) until the schema
  is reparsed (see the review fixes).
- A secure delete of an entry that the pin's walk mis-parses (a
  detail=none delete-and-reinsert marker written before the setting
  was on) grows the page by a byte: reproduced as written; see the
  gaps.
- detail=none: a row deleted and written again in one pending batch
  keeps the rows after it in the doclist.

**Tests:** `tests/fts5_delete_v17.rs` (10 tests: tombstone pages in
three detail modes and page sizes — growth to multi-page hashes,
8-byte keys, rowid 0, negative rowids, deletes in a transaction with
the structure dumped part-way; contentless_delete DELETE / UPDATE /
REPLACE semantics and errors; deletemerge / automerge / merge /
optimize over tombstones; secure deletes in six detail / page size /
prefix configurations — single terms, page heads, runs of pages,
400-token position lists; secure-delete on contentless 'delete',
external content kept by triggers, histories with delete markers
from before the setting, the setting toggled, 'rebuild'; savepoints,
ROLLBACK TO and ROLLBACK over pending tombstones and secure deletes;
12 regressions; 40 generated workloads of random configuration and
write / delete / update / replace / optimize / merge / rebuild /
delete-all / transaction sequences and 40 page-level workloads of
long doclists over small pages (`HIPP_FTS5_DELETE_CORPUS` for more:
1,300 seeds of the scratch generators ran clean), every shadow table
compared statement by statement, then each engine's integrity-check
on the other's file; files moving between the engines both ways).
`tests/crash.rs` `v17_crash_fts5_delete_writes` (tombstones with a
savepoint rolled back, deletemerge, secure deletes, secure deletes
then optimize: every crash point recovers to the pre- or post-image).
`hipp-cli/tests/cli_v17_fts5_delete.rs` (byte-identical stdout,
error lines included). `tests/fts5_v16.rs` `secure_delete_is_not_ignored`
now compares the deletes with the pin when the leaf is built.

**Gates:** `ci/local-gates.sh` (full) passes: fmt, clippy `-D
warnings` (default, all features, every feature-set line), workspace
tests, the no_std targets, the four size gates; `depth_v16` /
`triggers_v16` also pass built with debug info (the fts5 write path
stays boxed; `Config::stack_budget` unchanged). `link.x`'s FLASH
region grew to 2 MiB so the `xl` sizing demo links (it overflowed
the 1 MiB region; the measured sections are unchanged).

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 505,416 → 505,416 (0); max 1,043,340 → 1,044,172 (+832: the
ungated fts5 fixes above); xl 1,043,292 → 1,053,532 (+10,240: the
fixes and the leaf, which is xl − max = 9,360 B — the tombstone
hash, the secure-delete walk and page edits, the flush branch and
the session glue). Ratchets: `[max]` 1,044,172, `[xl]` 1,053,532.

**Honest gaps:**

- After the pin's own secure delete corrupts a detail=none page (a
  row whose newest entry is a delete-and-reinsert marker written
  while `secure-delete` was off), hipp writes the same bytes, but its
  later reads and merges of that page differ from the pin's; the
  corpus keeps detail=none tables in secure-delete from the start.
- The pin seeks with `%_idx` and the (stale) doclist indexes; hipp
  seeks the term's page by the pages' first terms and, inside a
  doclist, the rowid's page by the pages' first rowids. Both skip a
  rowid the segment's tombstone hash holds (review fix: hipp did
  not), and the corpora find no entry they locate differently; on a
  doclist index pointing past a page a secure delete emptied, the
  pin may read the next term's rowids, which hipp does not mimic.
- Flush points still follow the 0.16 statement-journal heuristic
  (see the FTS5 section's gap). A rolled-back transaction that moved
  the structure cookie (a config command, or the first secure
  delete's `version` 5) leaves the pin's connection holding the new
  cookie until it next reads the structure; a 'rebuild' or
  'delete-all' in that window writes the pin's held cookie where
  hipp writes the file's (the generated corpora query the table
  after each rollback; only the `secure-delete` setting is kept).
- A matched row whose `%_docsize` entry is gone is an error for
  bm25 / snippet / ORDER BY rank, but a `rank` column read without
  ORDER BY rank still answers; ORDER BY rowid DESC over such rows
  fails before the first row (the pin returns the rows before it).
- Not done (still refused): `contentless_unindexed=1`, `insttoken`,
  `tokendata=1`, `locale=1`.

### FTS5 delete writes — review fixes (2026-11-06)

**Status:** six review findings on the `fts5_delete` unit, each
reproduced against the live pin and fixed with a regression test
(`tests/fts5_delete_v17.rs`: `tombstoned_entries_stay_under_secure_delete`,
`schema_reload_forgets_rolled_back_setting`,
`contentless_update_rowid_values`, `fts5_conflict_resolutions`,
`bulk_deletes_alike`; `hipp-cli/tests/cli_v17_fts5_delete.rs`
`review_fix_outputs`).

- **A tombstoned entry stays under secure-delete** (leaf
  `fts5_delete`). With `secure-delete=1` on a `contentless_delete`
  table the pin's `fts5FlushSecureDelete` finds the entry with a
  segment iterator, which skips a rowid in the segment's tombstone
  hash (`fts5MultiIterIsDeleted`): a row deleted (tombstoned) and
  written again — INSERT OR REPLACE, UPDATE, DELETE then INSERT —
  leaves the old entry in place. hipp cut it out of the leaf
  (`%_data` differed until an 'optimize'); `sdel::secure_delete` now
  leaves an entry whose segment's hash holds the rowid.
- **The schema reparse forgets a rolled-back `secure-delete`**
  (`fts5_delete`). The pin keeps the setting in its `Fts5Config`,
  which is rebuilt whenever the schema is reparsed: after a
  successful ALTER TABLE (ADD / RENAME / RENAME COLUMN / DROP
  COLUMN, of any table), VACUUM (INTO too), a ROLLBACK / ROLLBACK
  TO / failure-ended transaction that undid a schema change, and
  `PRAGMA writable_schema = RESET`. A CREATE or DROP alone, or a
  failed ALTER, keeps it (probed one by one). hipp kept it through
  all of them (`Connection::fts5_schema_reset`, called from the
  statement step, `Connection::rollback` and the pragma).
- **`rowid = '1'` is not a move** (`fts5_delete`). A contentless
  UPDATE compares the rowids as `sqlite3_value_int64` reads them
  (`'1'`, `' 1'`, `'+1'` are row 1; `'x'`, `x'01'`, `2.0`, NULL
  are moves): an UPDATE of UNINDEXED columns alone with such a rowid
  is the no-op (changes() 1), not `Fts5ContentlessSubset`; a
  non-integer that reads as the old rowid (`1.0`, `'1e0'`) is
  "datatype mismatch" after the contentless rules, as on the pin.
  On row 0, NULL, `''` and `x''` read as rowid 0 too, so they do not
  move it either, and the no-op is "datatype mismatch" for them
  (found while fixing; hipp let the no-op through). 3 old rowids ×
  3 tables × 3 SETs × 22 new rowids compared.
- **Linear-time deletes** (`fts5_delete`). A tombstone write read
  every `%_data` row for each deleted row, and each secure delete
  walked its segment from the first page, so deleting n rows in one
  statement cost O(n × index). A tombstone write now reads the
  structure and the one hash page the rowid maps to (all of that
  segment's pages only when the hash is rebuilt); the secure-delete
  walk binary-searches the term's page by the pages' first terms
  and, inside a doclist, goes on from the last page whose first
  rowid is at most the target. Debug CLI, `DELETE FROM t WHERE
  rowid % 2 = 0` after N one-term rows: contentless_delete 0.44 s /
  0.92 s / 2.4 s / 4.5 s and secure-delete 0.94 s / 2.2 s / 4.5 s /
  9.3 s for N = 2,000 / 4,000 / 8,000 / 16,000 (were 1.03 s /
  3.05 s / 10.6 s and 1.63 s / 4.67 s / 13.9 s up to 8,000; a plain
  fts5 table takes 0.24 s / 0.48 s / 0.95 s / 2.0 s): linear, the
  rest being the b-tree write of each changed page. With `pgsz`
  65,536 each tombstone rewrites its (up to 64 KiB) hash page, as
  the pin does: 8,333 deletes of 10,000 rows take 6.2 s (debug),
  bytes identical to the pin's.
- **OR FAIL / OR ROLLBACK on fts5 writes** (ungated, leaf `fts5`;
  older than the unit). An fts5 write's OR clause was not consulted
  when it failed, so both acted as ABORT. Now OR ROLLBACK ends the
  transaction (the pin's `OP_VUpdate` sets `errorAction` from the
  clause) and OR FAIL keeps the rows written before the conflict
  together with their pending terms (the kept statement no longer
  restores the fts5 state taken before it), in every content mode,
  with and without secure-delete.
- **Docs**: the claims below that a rolled-back setting "stays on"
  and that both engines find the same entry are now qualified.

**Oracle-probing notes:** the tombstone skip holds in detail full,
column and none; the reparse list above is from the pin one
statement at a time (a CREATE TABLE ... AS SELECT failing in
autocommit keeps the setting). A rollback that moved the structure
cookie leaves the pin's connection holding it until the next
structure read; a 'rebuild' or 'delete-all' first writes that held
cookie (honest gap, below).

**Tests:** the five tests above (the generated workloads now give
`contentless_delete` tables secure-delete half the time, and roll
back a `secure-delete` setting followed by VACUUM, ALTER TABLE ADD
COLUMN, a rename of the table and back, or a CREATE TABLE);
seeds 0..400 of the workload generator and 0..700 of the
page-level generator ran; one page-level seed (291) differs, on
identical `%_data` bytes, where the pin misreads its own index (a
phrase / AND query seeking 'b' returns a row with no 'b'; 'b' alone
and the same query after an 'optimize' do not; hipp answers from the
entries): the stale-doclist-index honest gap above.

**Gates:** `ci/local-gates.sh` (full) passes; `depth_v16` /
`triggers_v16` pass with debug info.

**Size Δ** (thumbv7em, `--profile tiny`, against b034139): tiny
76,960 B (+0); full 506,480 B (+0); max 1,046,444 B (+640: the
ungated OR FAIL / OR ROLLBACK resolution of fts5 writes); xl
1,102,956 B (−104: the leaf's seeking walk and page-at-a-time
tombstone reads came out smaller than the whole-index loads).
Ratchets `[max]` 1,046,444 and `[xl]` 1,102,956.

**Honest gaps:**

- The cookie a rolled-back transaction leaves in the pin's
  connection (above) is not kept by hipp: a 'rebuild' or
  'delete-all' that is a table's first structure access after such a
  rollback writes the file's cookie, the pin its held one.
- hipp refuses to rename a virtual table a trigger names
  (`VtabNotAlterable`, v0.16); the generated external-content
  workloads rename none.

### Join ordering, automatic indexes and Bloom filters (2026-11-06)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated: new leaf `join_order` (requires `v16` and `expr_index`:
it extends `xplan`'s candidate-loop model rather than adding a second
planner). Placement (ADR 0004): `max` + the leaf measures 1,093,732 B
of thumbv7em code, past `max`'s 1,048,576 B budget, so the leaf joins
`xl` (`xl = ["max", "fts5_delete", "join_order"]`); its own
`v16,indexes,expr_index,join_order` line in `ci/feature-sets.txt`.
Without the leaf joins run in FROM order, each source with its own
best loop, exactly as in 0.16.

**Shipped** (`src/xjoin.rs`, `src/session/join.rs`; hooks in `xplan`,
`session::{xindex, stream, eqp, vtab, json_tvf, maint}`):

- **The pin's join planner** for a SELECT core of two to 64 sources (63
  before the review fixes)
  (tables of the main database, virtual tables, JSON table-valued
  functions; no RIGHT / FULL join): every candidate loop of each
  source with its prerequisites (`whereLoopAddBtree` /
  `whereLoopAddBtreeIndex`, nOut lowered only by the terms the
  prerequisites make computable, `whereLoopOutputAdjust`), inserted
  through `whereLoopInsert` (`whereLoopAdjustCost`,
  `whereLoopFindLesser`), the N-best `wherePathSolver` (mxChoice 1 /
  5 / 12, 18 with the star-query heuristic), the interstage
  heuristic, sorting costs for ORDER BY / GROUP BY / DISTINCT
  (`wherePathSatisfiesOrderBy` with its reverse mask), and the
  ordering constraints: a LEFT JOIN's right side and every source of
  an explicit CROSS JOIN run after the sources before them; a plain
  join after them is free again. The core is laid out in the chosen
  order (sources permuted, every expression renumbered,
  `query::remap_plan`), so both executors — the materialising one
  and the v0.17 streamed one — run it unchanged; inner ON terms move
  to the latest source they read. The planner reads the WHERE clause
  as the pin does: LEFT JOIN strength reduction over the WHERE with
  every ON clause appended (`sqlite3ExprImpliesNonNullRow`), the
  pin's constant propagation (only for two or more AND terms,
  INTEGER PRIMARY KEY columns included), `x IS [NOT] NULL` on a NOT
  NULL column as the constant 0 / 1 (`sqlite3ExprCanBeNull`; not for
  a LEFT JOIN's right side), equivalence classes (`a = b AND b op v`
  probes `a`), the virtual terms in the pin's term-list order
  (commuted copies, BETWEEN halves and `x>NULL` after the written
  terms, analysed last to first), and a non-covering index scan's
  `nLookup` reduction for the terms the index covers.
- **Automatic indexes** (`constructAutomaticIndex`): for an `=` / `IS`
  term on a column no index starts with (`termCanDriveIndex`,
  `columnIsGoodIndexCandidate`), a loop with the pin's setup cost; the
  executors build it once per statement (the table's rows sorted on
  the key columns, under their collations, then the other columns the
  statement reads, then the rowid — the index's order) and probe it
  by binary search. PARTIAL when a WHERE (or the LEFT JOIN's own ON)
  term reads the table alone or no table at all. EXPLAIN QUERY PLAN
  prints `SEARCH x USING AUTOMATIC [PARTIAL] COVERING INDEX (c=?)`.
- **Bloom filters**, built and consulted (they change no rows): in
  front of every automatic index whose key collations are BINARY,
  and — with `sqlite_stat1` rows for the tables up to it — in front
  of an index or rowid lookup run more often than its table has rows
  that is self-culling and keyed on BINARY columns
  (`whereCheckIfBloomFilterIsUseful`), with the pin's pull-down
  (a later lookup's filter is built with an earlier one's when its
  prerequisites allow). The pin's `filterHash` (integers by value,
  every string alike, every blob alike). A lookup with a filter reads
  the table's rows, so its index no longer "covers" (as on the pin).
  EXPLAIN QUERY PLAN prints `BLOOM FILTER ON x (c=?)` where the pin
  does.
- **`sqlite_stat1` read back** at schema load and after ANALYZE (in
  the same connection, either engine's ANALYZE): `nRowLogEst` and each
  index's `aiRowLogEst` replace the defaults (an index without a row
  lifting a small table to the pin's 1,000-row floor),
  `columnIsGoodIndexCandidate`'s statistics rule, the Bloom filters.
- **Virtual tables in the order** (`whereLoopAddVirtual`): one loop per
  constraint set the module's xBestIndex accepts (all usable, each
  distinct set of other sources in increasing order, none), costed
  from estimatedCost / estimatedRows (fts5's estimates are now the
  pin's), plus `whereLoopAddOr`'s loop for an OR of MATCH terms; the
  module's constraints are then planned in the chosen order. So
  `FROM f, m WHERE f MATCH m.q`, `FROM generate_series(1, t.n), t` and
  `FROM json_each(t.j), t` now run (they were refused) with the
  pin's rows and plans.
- **Ungated fixes:** the commuted copy of a join term `a.k = b.k`
  kept the collation of its new left operand, so a lookup on `b.k`
  compared BINARY where the comparison is `a.k`'s NOCASE (rows were
  lost through an index or automatic index); `,` in FROM parses as
  the pin's plain inner join (`JoinKind::Inner`), only the CROSS
  keyword making a CROSS JOIN (`JT_CROSS`), so the FROM-subquery
  flattening rule that tests for a following CROSS JOIN (the pin's
  17d) no longer fires for a comma.

**Probing notes (live pin; the 3.54.1 amalgamation's `where.c` read
alongside):** the pin reorders only past the CROSS / LEFT rules above
(`b CROSS JOIN c, a WHERE ...` may run `a` before `c`); an explicit
`WHERE 1`, or `x IS NOT NULL` on a NOT NULL column (folded to 1 by the
resolver), makes an automatic index PARTIAL; `x IS NOT NULL` adds the
virtual `x>NULL` range even without STAT4, and it loses to a covering
full scan through `whereLoopAdjustCost` against the equality loop on
the same index; the IN iterations of an index's first columns are
charged to its deeper loops (`rRun += nInMul + nIn`), and it is
`whereLoopAdjustCost` that still makes `b IN (...) AND c = ?` prefer
an index on (b, c) to one on c; a top-level `IS NOT NULL` makes a
LEFT JOIN inner only as the first AND term of the WHERE (or of an ON
clause appended to it); a Bloom filter needs statistics for every
table up to its loop, and an automatic index on a NOCASE comparison
has none.

**Tests:** `tests/join_order_v17.rs` — `generated_joins_match_pin`
(60 seeds by default, `HIPP_JO_SEEDS` for more: 2- to 6-way joins over
random rowid / INTEGER PRIMARY KEY / WITHOUT ROWID schemas with and
without indexes, no statistics / the pin's ANALYZE / hipp's ANALYZE,
comma, JOIN, LEFT and CROSS joins, every kind of WHERE term, ORDER BY /
GROUP BY / DISTINCT / LIMIT; rows compared in order, plans as text),
`curated_plans_and_rows`, `statistics_and_bloom_filters`,
`virtual_tables_reordered`, `regressions`, `automatic_index_is_faster`
(2,000 x 2,000: the automatic index ~16 ms against ~1.3 s for the
nested loop in a debug build; asserted 10x). Measured over 1,600
generated queries: no row set differs, 12 (0.75%) come in another
order, 106 (6.6%) print another plan — the test allows 2% and 10%.
`hipp-cli/tests/cli_v17_join_order.rs`: rows and EXPLAIN QUERY PLAN
trees byte-for-byte, with either engine's statistics, and the
virtual-table cases. `tests/eqp_v16.rs`'s two documented differences
(join order, automatic index) now match the pin and moved to its
checked corpus. Every v0.16 suite stays green.

**Gates:** `ci/local-gates.sh` (full run) passes. tests/depth_v16.rs
and tests/triggers_v16.rs pass with debug info. The debug
`STACK_BASE` was re-measured: an INSERT's step path in the `xl`
build with debug info needs ~500 KiB before any nesting (the step
future's poll frame alone ~230 KiB; probed on HEAD too, the same
path already filled the test's old 512 KiB thread to its last page), so
`STACK_BASE` is 512 KiB and depth_v16's small debug thread 640 KiB
(the budgeted limits on it are those of the old pair).

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 506,432 → 506,480 (+48: the ungated collation fix); max
1,045,676 → 1,045,804 (+128: the same fix in `xplan` and the
gated-out hooks' residue); xl 1,055,028 → 1,103,060 (+48,032: the
planner, automatic indexes, Bloom filters, statistics, virtual-table
loops — over `justify_over`'s 32 KiB, justified here: the pin's
planner is its largest single component, and the leaf sits in `xl`,
1.25 MiB, with 207,660 B left).

**Honest gaps:**

- Plans: 6.6% of the generated corpus (mostly 4- to 6-way joins)
  still prints another plan; the classes seen: path-cost ties among
  several automatic indexes and CROSS / LEFT constraints in 5- and
  6-way joins; the IN early-out / seek-scan rules of
  `whereLoopAddBtreeIndex` with statistics (`x IN (...)` on an index
  continued by the rowid; closed in v0.17 by `multi_index`: the
  statistics rule); indexes containing the INTEGER PRIMARY
  KEY column (closed in v0.17: probed matching with `multi_index`'s
  single-table insertion); a WITHOUT ROWID PRIMARY KEY's BETWEEN
  printing only its lower bound (a v0.16 single-table class; closed
  in v0.17: both bounds print — a secondary index's PRIMARY KEY
  suffix range is still not probed); transitive equivalences
  through a LEFT JOIN the WHERE made inner. Rows differ in order in
  0.75% of queries (the same plans), never in content.
- Not modelled: the MULTI-INDEX OR of b-tree tables, skip-scans
  (both closed in v0.17 by `multi_index`),
  `sqlite_stat4`, LIKE-length estimates (likelihood() estimates:
  closed by the review fixes below), the
  EXISTS-to-join rewrite (closed in v0.17 by `flatten`), the ordered-inner-loop `ORDER BY ... LIMIT`
  case. A join containing a FROM subquery, view, CTE (closed in v0.17
  by `flatten` for those the pin flattens: their tables join the order),
  TEMP or attached table, or a RIGHT / FULL join, keeps FROM order (the pin
  may reorder those too; the rows are the same). Automatic indexes
  on views and subqueries (the pin's cheaper `rSetup - 25`) are not
  built.
- The `whereOmitNoopJoin` LEFT JOIN that EXPLAIN QUERY PLAN leaves out
  (DISTINCT, or a one-row loop whose table nothing else reads) is
  still run by hipp (same rows).
- A Bloom filter on a lookup is consulted only for single-value
  probes (an IN list or subquery probe goes straight to the lookup),
  and one keyed on an index expression admits every probe; both are
  still printed, as the pin builds them.
- Statistics written to `sqlite_stat1` by hand are read at the next
  schema load or ANALYZE (as on the pin); a rolled-back ANALYZE keeps
  its statistics in the connection until then (probed by the review
  fixes below: as on the pin, unless the ANALYZE created
  `sqlite_stat1`, a schema change whose rollback reloads).

### Join ordering — review fixes (2026-11-06)

**Status:** ten review findings on the `join_order` unit, each
reproduced against the live pin; nine fixed with regression tests
(`tests/join_order_v17.rs`: `group_by_in_loop_order`,
`likelihood_wrapped_terms`, `left_join_strength_reduction`,
`statistics_follow_ddl`, `update_from_is_a_join`, `join_of_64_tables`,
`expression_covering_and_column_bounds`,
`tvf_arguments_across_cross_join`; `hipp-cli/tests/cli_v17_join_order.rs`
`review_fix_outputs`), one a doc fix. Gating as the unit (leaf
`join_order`, in `xl`) except where noted.

- **A GROUP BY a join delivers in another column order** (`GROUP BY
  t1.a, t1.b` over an index on `(b, a)`, `GROUP BY c, id` over the
  rowid): the pin emits the groups as its loops give them and prints
  no group sorter; hipp printed the same plan but sorted the groups.
  The join's first loop now keeps an access path that says the
  groups arrive in order, whatever it is, and the executors honour it
  for joins. The same bug for one table read in rowid order (`SELECT
  c, id, count(*) FROM t GROUP BY c, id`) is fixed too (`expr_index`,
  the v0.16 planner).
- **likely() / unlikely() / likelihood() and COLLATE around a WHERE
  term** are removed when the WHERE clause is split
  (`sqlite3WhereSplit` / `whereClauseInsert`), so `likely(b.ax = a.x)`
  still drives an index, a rowid lookup or an automatic index (it ran
  as a nested loop: 1.38 s for a 2,000 x 2,000 join). The term's
  probability is the pin's `truthProb` (`LogEst(P * 2^27) - 270`;
  likely() and unlikely() now carry their 0.9375 / 0.0625 as
  likelihood's second argument), inherited by its commuted copy,
  BETWEEN halves, `x>NULL` and OR-to-IN children (`markTermAsChild`),
  and it replaces the default estimates where the pin's does: an
  equality on an index's table column (`nOut += truthProb`), a range
  bound (`whereRangeAdjust`), an unused term (`whereLoopOutputAdjust`).
  A wrapped `col = constant` fixes nothing for constant propagation
  (`findConstInWhere` looks at `=` and AND nodes only). Single-table
  plans follow the same rules with the leaf (below it, `max`'s budget
  keeps 0.16's reading of a wrapped term as a plain filter).
- **LEFT JOIN strength reduction** skips likely() / COLLATE wrappers
  (`sqlite3ExprSkipCollateAndLikely`), and `x IS DISTINCT FROM NULL`
  is the pin's NOTNULL (`IS NOT DISTINCT FROM NULL` its ISNULL: the
  parser's `binaryToUnaryIfNull`, `v16`), so both null-reject.
- **The statistics a connection holds** follow the pin's model
  (`StatState`): per table `nRowLogEst` / `TF_HasStat1`, per index
  `aiRowLogEst` / `hasStat1`, set when the schema is read
  (`sqlite3InitOne`: tables at 200, indexes at their defaults, then
  `sqlite3AnalysisLoad`) and by ANALYZE (which clears the flags but
  not a table's estimate), and otherwise only following this
  connection's CREATE / DROP (a dropped index's table keeps its
  estimate; a new index's defaults come from it, lifting it to the
  1,000-row floor). ALTER TABLE (every form), VACUUM, the rollback
  of a schema change (ROLLBACK, ROLLBACK TO) and another connection's
  schema change read schema and statistics again. hipp re-read
  `sqlite_stat1` after every DDL instead. Rows are read as
  `analysisLoader` reads them: counts wrap (`tRowcnt`), a row's index
  is found by name in the whole schema (it may belong to another
  table: its estimate then also becomes the row's table's), an
  unknown index makes the row its table's own count, every value is
  read as text.
- **UPDATE ... FROM is planned as a join** of the target and the FROM
  sources (`FromJoin`): the join is run once in the pin's order before
  any row changes, each target rowid keeping the last combination that
  order gives it (the pin's ephemeral table keyed by rowid), then the
  rows are updated in rowid order. With several matching FROM rows the
  final table is now the pin's (it was the last in FROM rowid order),
  EXPLAIN QUERY PLAN prints the join's loops, and the automatic index
  makes a 2,000 x 2,000 UPDATE ... FROM ~0.9 s against 3.3 s before in
  a debug build (asserted 5x against the same join `NOT INDEXED`).
  WITHOUT ROWID targets and joins the planner does not take (FROM
  subqueries, RIGHT joins, ...) keep the per-row join in FROM order.
- **64-source joins** are planned (the pin's limit; hipp stopped at
  63 and ran a 64-way chain join as a nested loop, >60 s; now ~0.3 s).
- **Expression-index covering and column BETWEEN bounds** (v0.16
  classes; the BETWEEN rule under `expr_index`, the covering rule with
  the leaf for `max`'s budget): an index whose expressions hold
  every use of the columns it lacks covers a SELECT (the pin's
  `whereIsCoveringIndex`, `WHERE_EXPRIDX`): costed and printed as
  COVERING, while hipp still reads the table row for the expressions'
  values (same rows). A BETWEEN's halves get their commuted copies
  (`t1.b BETWEEN t2.y AND t2.x` probes `t2.y <= ?`), in the pin's
  term order.
- **A table-valued argument past a CROSS JOIN** (`json_each(t.j)
  CROSS JOIN t`): the pin offers xBestIndex no constraint for it, so
  json_each runs without that argument (no JSON input: no rows; no
  root: `$`) and the argument's `hidden column = value` term stays a
  WHERE term. hipp refused these; it now gives the pin's rows and
  `VIRTUAL TABLE INDEX 0:` plan. A RIGHT / FULL join (the planner does
  not order it) is still refused, as an error on both engines.
- **Docs:** `cli_v17_join_order.rs`'s header claimed `.eqp on`
  coverage; hipp's shell has no `.eqp`, the test uses EXPLAIN QUERY
  PLAN statements.

**Probing notes (live pin):** after ANALYZE, DROP INDEX, CREATE TABLE
/ VIEW / TRIGGER, DROP TABLE, REINDEX and PRAGMA keep the statistics
in memory; RENAME COLUMN, RENAME TABLE, ADD COLUMN, DROP COLUMN,
VACUUM, ROLLBACK and ROLLBACK TO of a schema change reload them;
`ANALYZE a` after a DROP INDEX clears `b`'s TF_HasStat1 but keeps its
estimate; a stat row inserted by hand is not read by a following
CREATE INDEX (defaults); `json_each('[1,2]', t.j) CROSS JOIN t` keeps
only the rows whose root (`$`) equals `t.j`; with statistics,
`likely(t.a = u.x)` makes an index lookup on `u.x` cost a full table's
rows, so the pin scans both tables.

**Tests:** the eight new `tests/join_order_v17.rs` cases above (the
DDL sequences run in one connection of each engine, plans and rows
after every step; UPDATE ... FROM's final tables, plans and RETURNING
rows; the 64-table chain; the CROSS JOIN arguments) and the CLI's
`review_fix_outputs`. The generated corpus is unchanged at 1,600
queries (400 seeds): no row set differs, 12 orders and 106 plans
differ, as before the fixes.

**Gates:** `ci/local-gates.sh` (full run) passes; tests/depth_v16.rs
and tests/triggers_v16.rs pass with debug info.

**Size Δ** (thumbv7em, `--profile tiny`, against the ratchets after
the FTS5 review fixes): tiny 76,960 → 76,960 (0); full 506,480 →
506,672 (+192: the parser's `IS DISTINCT FROM NULL` null test, `v16`,
and the LEFT JOIN rule's likely() skip); max 1,046,444 → 1,046,892
(+448: the `expr_index` fixes — groups over a rowid scan, BETWEEN's
commuted halves, the join-free hooks; truth probabilities, the
likely() stripping and expression covering are gated with the leaf,
which keeps max 1,684 B under its budget); xl 1,102,956 → 1,114,356
(+11,400: truth probabilities, the statistics model, UPDATE ... FROM
as a join, expression covering, the CROSS JOIN argument rule).
Ratchets `[full]` 506,672, `[max]` 1,046,892, `[xl]` 1,114,356.

**Honest gaps (beyond the unit's list above):**

- A single-table loop with more equality columns that costs the same
  as its prefix on the same index is not preferred
  (`whereLoopAdjustCost` is modelled for joins only): `WHERE b = 2
  AND c = 1` over an index on `(b, c)` prints `(b=?)` (v0.16 class,
  found while probing; the rows are the same). (Closed in v0.17 by
  `multi_index`: one table's loops pass through `whereLoopInsert`.)
- An expression-covered index still reads the table row (the pin reads
  the expression's value from the index entry).
- `sqlite_stat1`'s `sz=N` / `unordered` / `noskipscan` options are
  not read. (Closed in v0.17 by the `multi_index` review fixes.)

### Index strategies: MULTI-INDEX OR, min() / max(), skip-scans, IN with statistics (2026-11-06)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated: new leaf `multi_index` (requires `join_order`: it adds
loops to the join planner's model). Placement (ADR 0004): `max` +
the leaf needs `join_order` too, far past `max`'s 1,048,576 B, so the
leaf joins `xl` (`xl = ["max", "fts5_delete", "join_order",
"multi_index"]`); its own `v16,indexes,expr_index,join_order,multi_index`
line in `ci/feature-sets.txt`. Without the leaf the plans are 0.16's
(and `join_order`'s).

**Shipped** (`src/xmulti.rs`; hooks in `xplan`, `xjoin`,
`session::{xindex, join, eqp}`):

- **MULTI-INDEX OR** (`whereLoopAddOr`, `WHERE_MULTI_OR`). An OR term
  (not one the pin rewrites to IN as its only plan; see below) whose
  every branch — a comparison, `IN`, `IS NULL`, or an AND group — has
  a loop using a term on the source is a loop of its own: per branch
  the `(prereq, rRun, nOut)` of its loops (`whereOrInsert`, three per
  prerequisite set; adjusted against the source's loops already
  listed, `whereLoopAdjustCost`; the branch may use the WHERE clause's
  own terms, `whereScanNext` reading `pOuter`), summed over the
  branches, `rRun + 1`; it competes in `whereLoopInsert` and the path
  solver like any loop (no index order: `wherePathSatisfiesOrderBy`
  returns 0 at it). When chosen, each branch is planned again on the
  table alone with the WHERE's pushed terms (`pAndExpr`: indexable,
  no subquery; in a join, only the sources already joined) and read
  in turn; a row is returned at the first branch whose condition it
  satisfies (what the pin's RowSet test leaves), so the rows are the
  pin's, in its order, each once. Branches are taken in the OR
  clause's term order: written branches `INDEX 1..n`, then the
  commuted copies of column-to-column comparisons, last first
  (`INDEX n+1` is the copy of the last branch). A WITHOUT ROWID
  table's branches never cover (the pin reads the row by its key).
  One table, a join loop (prerequisites from the branches' probes,
  lateral when a branch reads an earlier source), LEFT JOIN (`LEFT-JOIN`
  on each branch's loop), and UPDATE / DELETE (the rowids collected,
  sorted, deduplicated; never one-pass). EXPLAIN QUERY PLAN prints
  `MULTI-INDEX OR`, `INDEX n`, each branch's IN-probe and condition
  subqueries, then its loop.
- **OR-to-IN as the pin decides it** (`exprAnalyzeOrTerm` case 1): the
  first branch's left column, then the first term on another table
  (the commuted copies), every branch `K = value`, values with no
  other affinity than K's. As probed, the rewrite no longer hides the
  OR from the MULTI-INDEX OR: the IN loop and the OR loop compete on
  cost (`t.a = u.x OR t.a = u.y` with `t.a` unindexed reads `u`
  through a MULTI-INDEX OR); an IN hipp made that the pin does not is
  dropped. **whereCombineDisjuncts**: a two-branch OR's comparisons
  of one column with one value combine (`x < 5 OR x = 5` is also the
  virtual term `x <= 5`).
- **min() / max()** (`minMaxQuery`, `WHERE_ORDERBY_MIN` / `_MAX`): a
  core without GROUP BY / HAVING whose one aggregate call (repeats
  count once) is `min(x)` / `max(x)` without FILTER (DISTINCT and an
  aggregate ORDER BY allowed) is planned for `x` (min: NULLs last when
  `x` can be NULL, the pin's `KEYINFO_ORDER_BIGNULL`; max: `x DESC`),
  one table or a join. When the loop delivers that order with no IN
  on it — an index, the rowid, a reverse scan, or the table's own
  order with `x` held constant (`WHERE x IS NULL`) — the statement
  reads only the first row of the loop that passes the WHERE
  (`sqlite3WhereMinMaxOptEarlyOut`): the bare columns are that row's
  (all-NULL arguments included: min's second pass, max's first row).
  EXPLAIN QUERY PLAN prints `SEARCH` for every loop (`SEARCH t`,
  `SEARCH w USING PRIMARY KEY`), and no `f(DISTINCT)` / `f(ORDER BY)`
  sorter when the order is delivered.
- **Skip-scans** (end of `whereLoopAddBtreeIndex`): with
  `sqlite_stat1` rows (`hasStat1`), an index whose leading column(s)
  have no term and repeat 18 or more times per value (LogEst 42) is
  probed per distinct value (`nIter + 5` more iterations), `ANY(a) AND
  b=?`, ranges after it too; read in key order (hipp finds the
  distinct values with one walk of the index).
- **The IN operator with statistics**: an IN whose `K` values cost
  more than scanning the `M` rows the columns before it match
  (`M + logK + 10 < nIn + logN`) is a filter unless it is the loop's
  first IN (the pin's seek-scan).
- **whereLoopInsert / whereLoopAdjustCost for one table**: a single
  table's loops (a WITHOUT ROWID table's after the 0.16
  `insert_loops` pass) go through the join planner's insertion, so a
  loop with more equality columns than its equally costed prefix wins
  (`WHERE b = 2 AND c = 1` over an index on `(b, c)` prints `(b=? AND
  c=?)`), and the IN iterations of earlier columns are charged at
  every deeper level (`rRun += nInMul`) as for a join.
- EXPLAIN QUERY PLAN fixes (ungated, `explain`): an uncorrelated IN
  subquery evaluated as a filter (not an index probe) shows `CREATE
  BLOOM FILTER` after its loops; `SELECT DISTINCT count(*) ...` (an
  aggregate without GROUP BY) shows `USE TEMP B-TREE FOR DISTINCT`.
  With `join_order`: a join's WHERE subqueries are shown after the
  loop that makes their term computable (the pin codes them there).
- **Fixed** (ungated, `expr_index`): the OR-to-IN rewrite ignored the
  values' affinity, so `x.c0 = y.c1 OR x.c0 = y.c2` (a TEXT key, a
  REAL value) probed the key with REAL values and lost rows (55 rows
  on the pin, 26 in hipp, found by the generated corpus); the pin's
  `okToChngToIN` affinity check now applies. A row-value comparison
  `(a, b) > (1, 1)` (hipp's lexicographic OR expansion) is wrapped in
  a double NOT under the leaf so it is not taken for an OR of branches
  (a MULTI-INDEX OR there returned the pin's rows in another order).

**Probing notes (live pin):** the MULTI-INDEX OR is costed with the
WHERE's other terms visible to each branch (`a=1 OR c0>4` beside `c2
IN (6,8,'x')` runs `(c2=? AND rowid>?)` in branch 2, and beats the IN
loop with 3 values but not with 2); a WITHOUT ROWID table's branches
print no COVERING; LEFT JOIN branches end in ` LEFT-JOIN`, the
`MULTI-INDEX OR` line does not; an OR rewritten to IN still drives a
MULTI-INDEX OR (`t.b = u.x OR t.b = u.y` with `t.b` indexed reads `t`
by IN, with `t.b` unindexed reads `u` by OR: `INDEX 3` / `INDEX 4`);
min() / max() needs no index to print `SEARCH t`; `max(abs(a))` keeps
`SEARCH t USING COVERING INDEX ta` (a covering scan, no early-out);
HAVING or FILTER, a second aggregate (`count(*)`), GROUP BY turn the
optimisation off, DISTINCT and an aggregate ORDER BY do not; min()'s
all-NULL answer takes the first NULL row's bare columns (BIGNULL's
second pass), `WHERE x IS NULL` makes even a full scan stop at its
first row; `CREATE BLOOM FILTER` follows every uncorrelated IN
subquery used as a filter (NOT IN, a result column, VALUES and
constant rows included), never an index probe's or a correlated one.

**Tests:** `tests/multi_index_v17.rs` — curated: single-table OR
(34 queries), OR in joins (13), UPDATE / DELETE through OR (plans,
RETURNING rows, the tables after, per engine on its own file), min() /
max() (55, all-NULL and DESC-index cases), skip-scans (no stats, the
pin's ANALYZE, statistics below the threshold), the IN operator with
and without statistics, single-table cost ties (5 schemas),
regressions (the affinity bug, row values, the EXPLAIN fixes);
`generated_corpus_matches_pin` (40 seeds by default, `HIPP_MI_SEEDS`:
single-table and two-table queries with OR terms of comparisons, IN
lists / subqueries, IS NULL, BETWEEN and AND groups, IN subqueries,
min() / max() with bare columns, on rowid / IPK / WITHOUT ROWID
tables with 1- to 3-column indexes, a third with ANALYZE): 4,000
queries at 400 seeds, no row set differs, 1 order and 53 plans (1.3%)
differ — the test allows 1 in 200 and 2 in 100;
`multi_index_or_is_faster` (20,000 rows, `a = 5 OR b = 7`: the OR
reads ~40 rows, net of a one-row baseline, best of 3, asserted 5x
faster than `NOT INDEXED`; ~11 ms against ~48 ms in a debug CLI).
`hipp-cli/tests/cli_v17_multi_index.rs`: three byte-identical scripts
(OR rows and trees, min() / max(), skip-scans after ANALYZE, cost ties,
UPDATE / DELETE). `tests/eqp_v16.rs`: `documented_differences` loses
its MULTI-INDEX OR and max() cases (now checked against the pin);
the generator adds OR terms, IN subqueries and min() / max(); 15 of
4,800 plans differ at 400 seeds — the limit is now 1 in 200 (was 1
in 100). `tests/join_order_v17.rs`: the generator adds OR terms (with
join columns), IN subqueries and min() / max(); 15 orders and 126
plans of 1,600 at 400 seeds — the limits are now 3 in 200 orders and
9 in 100 plans (were 2 and 10; the default 60 seeds measure 3 and 20
of 240). Every other v0.16 / v0.17 suite stays green.

**Gates:** `ci/local-gates.sh` (full run) passes. tests/depth_v16.rs
and tests/triggers_v16.rs pass with debug info (the stack constants
are unchanged). `tests/stream_v17.rs`'s random-ORDER BY regression no
longer compares how many distinct `random() % 5` values each engine
drew (a coin toss: the gate run met 9 against the pin's 8); it still
checks they come sorted.

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 506,672 → 506,672 (0: the ungated fixes are in `expr_index` /
`explain` code); max 1,046,892 → 1,047,060 (+168: the OR-to-IN
affinity check, the Bloom filter and DISTINCT lines; 1,516 B of the
budget left); xl 1,114,356 → 1,135,004 (+20,648: the OR loops and
their executor, min() / max(), skip-scans, the IN rule, single-table
insertion, the EXPLAIN trees; 175,716 B left). Ratchets `[max]`
1,047,060, `[xl]` 1,135,004.

**Honest gaps:**

- Plans: 1.3% of the generated corpus prints another plan. Classes
  seen: a branch's sub-plan that would itself be a MULTI-INDEX OR
  (hipp plans branches with b-tree loops only); a WITHOUT ROWID
  secondary index probed on its PRIMARY KEY suffix (`(c3=? AND c0>?
  AND c0<?)`: hipp stops at the key columns); `REUSE LIST SUBQUERY n`
  and the order of two IN subqueries' lines; equivalence-class terms
  moving a subquery to an earlier loop; the pin's vector range on a
  key (`(a,b)>(?,?)`; hipp scans, same rows).
- min() / max() in a join reads every joined row (fold, first-wins:
  the pin's value and bare columns except when every argument is
  NULL); a max() over an ascending index reads the index to its end
  (hipp's index cursor walks forward only; the answer is the pin's).
  `count(DISTINCT x)` over x's index still sorts (v0.16 class).
- Skip-scans walk the whole index once to find the leading values
  (the pin seeks past each); `sqlite_stat4` is not modelled.
  (`sqlite_stat1`'s `noskipscan` / `sz=` / `unordered`: closed in the
  review fixes below.)
- The IN early-out (`WHERE_IN_EARLYOUT`) and seek-scan are run-time
  shortcuts the plan text does not show; hipp probes every IN value.
- `CREATE BLOOM FILTER` is not shown under a compound IN subquery
  (the pin prints one per member); a vector `(a, b) IN (SELECT ...)`
  plans as in 0.16.

### Index strategies — review fixes (2026-11-06)

**Status:** the review's eight findings on the `multi_index` unit,
each reproduced on the live 3.54.1 pin and fixed at its root (one
documented as a gap). Gating as the unit: the fixes on paths `max`
already claims are ungated, the IN operator's own index (new EXPLAIN
surface) is in `multi_index`.

**Fixed:**

- **COLLATE inside an OR** (ungated in `xplan`, and in `xmulti`): the
  pin analyses an OR term that holds an explicit COLLATE anywhere
  outside a subquery (its `EP_Collate`, propagated through operators,
  function arguments and CASE; a COLLATE around the whole OR does not
  count) as no OR at all: neither a MULTI-INDEX OR, nor an OR-to-IN,
  nor `whereCombineDisjuncts`. hipp built a MULTI-INDEX OR (rows in
  branch order, the pin's in rowid order) and, ungated, an IN seek.
  Also: a branch's `likely()` / `unlikely()` wrapper no longer stops
  the OR-to-IN rewrite (`a='x' OR likely(a='y')` is the pin's IN seek).
- **The one-value IN** (ungated, `xplan`): `x IN (v)` with a constant
  `v` is the pin's parser's `x = +v`. It is now an equality term
  everywhere (compared as the IN compares: the key's affinity and
  collation), so `d='' OR d IN (-2)` is the pin's `d IN ('', -2)` IN
  seek and `b IN (3) ORDER BY c` over `(b, c)` sorts nothing.
- **OR-to-IN on an indexed expression** (`multi_index`): `in_key` now
  takes the pin's `exprMightBeIndexed` keys (a column, or an
  expression of one source in one of its expression indexes), so
  `a*2=14 OR a*2=4` is the pin's IN seek again. The same rule makes
  `t.a*2 = r.j` a term of `t` for the MULTI-INDEX OR in a join (hipp
  had no OR loop there), and a branch's sub-plan no longer covers by
  expressions (the pin plans it with no SELECT: `USING INDEX tex`).
- **The IN operator's Bloom filter** (ungated, `explain`): shown only
  when every comparison of the IN is BINARY (a NOCASE left column, or
  `x COLLATE NOCASE IN (...)`, has none on the pin). **The IN
  operator's own index** (`multi_index`; the pin's `isCandidateForInOpt`
  and `sqlite3FindInIndex`): an uncorrelated `SELECT cols FROM table`
  with no WHERE, GROUP BY, aggregate, LIMIT or compound (ORDER BY and
  DISTINCT allowed; not a view or FROM subquery, merged or not) reads
  the table in place of the subquery: `USING ROWID SEARCH ON TABLE u
  FOR IN-OPERATOR` for the rowid, `USING INDEX i FOR IN-OPERATOR` for
  the newest non-partial index whose leading columns are the result
  columns (any order) under the comparisons' collations, the
  affinities allowing it (a numeric left side needs a numeric column);
  when an index loop probes with the values, a UNIQUE index of exactly
  those columns. The probe keeps its left operand for this (nothing
  evaluates it).
- **min() / max() SEARCH text** (`multi_index`): every loop of a min()
  / max() query prints SEARCH, FROM subqueries, CTEs and virtual tables
  too; a lone `max(col)` offers a virtual table its `ORDER BY col DESC`
  (generate_series `INDEX 0xb:`; min()'s NULLS LAST order is not
  offered, as on the pin). STATUS's "every loop prints SEARCH" is now
  true.
- **`sqlite_stat1` keywords** (`multi_index`; `decodeIntArray`): after
  the integers, `unordered` (no range constraint, no ORDER BY and no
  covering full scan through the index), `sz=N` (its row size, at
  least 2) and `noskipscan` (no skip-scan) are read; they reset at
  each load.
- **min() / max() over an expression index** (`join_order`): an
  aggregate's arguments count in the expression-covering walk, so
  `min(a+1)` over an index on `a+1` prints `USING COVERING INDEX te`;
  an aggregate query's bare key expression (`SELECT a+1, max(a+1)`,
  `SELECT a+1 ... GROUP BY a+1`) keeps it from covering, as probed.
- Found on the way: **an equality's collation against the ORDER BY's**
  (ungated, `xplan`; a silent wrong order): `a = 'y' COLLATE NOCASE
  ORDER BY a` took `a` for constant and returned `y`, `Y`, `y` in rowid
  order; the pin's `wherePathSatisfiesOrderBy` holds a column constant
  only under the ORDER BY / GROUP BY term's collation. **A min() / max()
  over an IN probe on its argument** (`multi_index`): the pin holds the
  IN column constant, reads each value's entries forward and folds
  every row (no reverse scan); hipp scanned in reverse for max(), so
  the bare columns came from another row (an IN on an earlier key
  column keeps the loop's direction, as on the pin).

**Not fixed (honest gap):** after ANALYZE, `t.a IN (SELECT ...)` on a
small WITHOUT ROWID table's PRIMARY KEY still scans where the pin
seeks (base 0.16 had the same gap; the row set agrees, regression
`rv9` checks the rows). The pin keeps the IN loop although hipp's
model of its costs (`rRun` with `nIn` 46, `nOut` clamped to the
table) has the full scan cheaper; not modelled.

**Probing notes (live pin):** the COLLATE rule holds for `COLLATE
BINARY` and for a collation that matches the index (`c COLLATE NOCASE
= 'x'` over a NOCASE index), inside AND branches (`a=2 OR (b=1 AND
'x'='x' COLLATE NOCASE)`), function arguments and CASE, not inside a
scalar or IN subquery; `likely(b='x')` as a branch costs the OR out
(SCAN), `unlikely` keeps it. `'Y' IN ('y' COLLATE NOCASE)` is 0 (the
IN's collation is the left side's). The IN operator's index: the
newest qualifying index is taken (`u2(a,e)` over `u1(a)` and the
autoindex), INDEXED BY / NOT INDEXED in the subquery are ignored, a
WITHOUT ROWID table's PRIMARY KEY prints `sqlite_autoindex_w_1`, an
INTEGER PRIMARY KEY DESC is no rowid; for a probing loop a non-UNIQUE
index is never used and the rowid always is. min()/max(): `max(value)`
of generate_series passes DESC (0x08), `max(value+1)` and `min(value)`
pass no order.

**Tests:** `tests/multi_index_v17.rs` `review_regressions` (rv1-rv9:
every repro above with variants, joins included); the generated
corpus adds COLLATE inside OR terms, one-value IN lists, an expression
index and its key, the IN operator's own index, NOCASE columns and
`sqlite_stat1` keywords: 4,000 queries at 400 seeds, no row set
differs, 2 orders (one a min() whose bare column came from another row
of the same minimum under another plan, now counted as an order) and
47 plans (1.2%) — the limits tighten to 1 in 400 orders and 3 in 200
plans (were 1 in 200 and 2 in 100; the default 40 seeds measure 1 and
2 of 400). `tests/eqp_v16.rs`: the generator adds the same forms; 12
of 4,800 plans differ at 400 seeds (15 before, with fewer forms) — the
limit tightens to 1 in 400 (was 1 in 200); `documented_differences`
loses `b IN (SELECT x FROM u)` (the pin's `USING INDEX ux FOR
IN-OPERATOR` now matches). `tests/join_order_v17.rs`: the generator
adds COLLATE inside OR terms and one-value IN lists; 10 orders and 133
plans of 1,600 at 400 seeds (2 and 21 of 240 at the default 60) — the
order limit tightens to 1 in 100 (was 3 in 200), the plan limit stays
9 in 100 (the default seeds measure 8.75%).
`hipp-cli/tests/cli_v17_multi_index.rs` `review_outputs`: one more
byte-identical script.

**Gates:** `ci/local-gates.sh` (full run) passes; tests/depth_v16.rs
and tests/triggers_v16.rs pass with debug info.

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 506,656 → 506,656 (0: the ungated fixes are `expr_index` /
`explain` code); max 1,047,052 → 1,047,868 (+816: the COLLATE and
one-value IN rules, the ORDER BY collation check, the Bloom filter's
collation test; 708 B of the budget left); xl 1,150,692 → 1,156,732
(+6,040: the IN operator's index, expression keys, `sqlite_stat1`
keywords, the min() / max() SEARCH text and virtual-table order, the IN-argument rule).
Ratchets `[max]` 1,047,868, `[xl]` 1,156,732.

### Query flattening, WHERE push-down, unused columns, EXISTS-to-join (2026-11-06)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated: new leaf `flatten` (requires `join_order`). Placement
(ADR 0004): `max` is already within 1,524 B of its budget, so `max`
+ the leaf is past 1 MiB and the leaf joins `xl` (`xl = ["max",
"fts5_delete", "join_order", "multi_index", "flatten"]`); its own
`v16,indexes,expr_index,join_order,flatten` line in
`ci/feature-sets.txt`. Without the leaf subqueries, views and CTEs
plan and run exactly as before.

**Shipped** (`src/session/flatten.rs`; hooks in `session::cte`, the
core planner, `xjoin`, `session::join`, `session::stream`, `eqp`,
`fkey`):

- **The query flattener** (`sqlite3FlattenSubquery`). The view /
  CTE expansion marks each FROM subquery (view, CTE reference or
  written subquery) the pin flattens (`FromSource::flat`): the
  existing restriction model (`cte::flattens`: no aggregate,
  DISTINCT, OFFSET, FROM-less body; the LIMIT rules 8 / 9 / 13 / 15 /
  19 / 21, ORDER BY rules 11 / 16 and the complex-result rule, the
  compound rules) plus 22 / 25 / 26 / 28 and 3 — fixed on the way:
  the right side of a FULL JOIN is never flattened (it was modelled
  as a LEFT one) and a LEFT JOIN's right side is refused under a
  DISTINCT reader, not a GROUP BY one (probed: `... LEFT JOIN (SELECT
  ...) s ... GROUP BY u.x` flattens) — which also corrects the
  shared-materialization model built on it. A marked body is planned
  without access paths or join order (`Connection::flat_raw`), and
  once the reader's clauses are resolved its sources are spliced into
  the reader's FROM clause in its place: the first one takes its join
  and ON clause, its WHERE joins the reader's (ahead of it, as the
  pin's `sqlite3ExprAnd(pWhere, pParent->pWhere)`), or the LEFT
  JOIN's ON clause; every reference to one of its columns becomes the
  column's expression, through nested subqueries and their access
  paths; under a LEFT JOIN a merged expression that is not a column
  of the merged table is guarded by its NULL row (the pin's
  `TK_IF_NULL_ROW`: `SELECT ... LEFT JOIN (SELECT a, 1 AS one FROM t)
  s` gives NULL for `one` on an unmatched row); its LIMIT moves out,
  and its ORDER BY when the reader is a lone non-aggregate source
  without one (the pin drops one that orders nothing). The merged
  tables then join the join planner's order as ordinary sources and
  EXPLAIN QUERY PLAN prints their loops (`SEARCH t USING INTEGER
  PRIMARY KEY` where hipp printed `CO-ROUTINE v` / `SCAN v`). A view's
  columns keep the names the pin gives them before it flattens
  (`SubPlan::view_names`). A marked body the splice cannot take is
  planned again as before.
- **WHERE push-down** (`pushDownWhereTerms`) into the FROM subqueries
  that stay co-routines or materialisations: a WHERE term (or a term
  of the subquery's own inner or LEFT JOIN ON clause) reading only
  its columns through deterministic expressions without subqueries is
  copied into it — each column replaced by its expression, into every
  arm of a UNION ALL (with the leftmost arm's collation where an arm's
  differs, as the pin's `pCList`), into its WHERE, or its HAVING when
  it aggregates and the term reads more than its GROUP BY terms (the
  pin's `havingToWhere` moves the rest to WHERE) — and the body is
  planned again, so `SELECT * FROM vagg WHERE b = 3` searches the
  index inside the co-routine. Not into a LIMITed, recursive,
  windowed, MATERIALIZED or shared body, a VALUES body, a compound
  other than UNION ALL, or past a RIGHT / FULL join. The term stays in
  the reader too.
- **Unused subquery columns** (the stream unit's honest gap): the
  result columns of a co-routine or materialisation that nothing in
  its reader reads (the walk covers every expression, ON clause,
  table-valued argument, access path and virtual-table OR branch) are
  not computed, unless the body is DISTINCT, an aggregate, windowed,
  MATERIALIZED or shared, a compound other than UNION ALL, or its
  ORDER BY names the column — so `SELECT a FROM v` answers where
  `v.b` would overflow, as on the pin (a flattened view never
  evaluates it either). An ungated-path correction: the row set is
  unchanged; only errors the pin never raises are no longer raised
  (corrected in the review fixes below: a CTE reference and a
  correlated FROM subquery keep every column, as on the pin).
- **EXISTS-to-join** (3.54's `existsToJoin`): an `EXISTS (SELECT ...
  FROM t WHERE w)` AND term of a WHERE clause, over one table of main
  with no aggregate, compound, window function or LIMIT of its own,
  becomes the literal 1 (kept: it makes an automatic index PARTIAL,
  as on the pin), `t` a semi-join source of the core
  (`SubSource::exists`) and `w` a WHERE term. The join planner orders
  it after every source its terms read, with each of its non-automatic
  loops yielding one row (probed placements: before a rowid lookup it
  does not read, after an unrelated full scan); both executors stop
  its loop at its first row that makes a joined row. EXPLAIN QUERY
  PLAN prints `SEARCH t EXISTS ...`. Not under NOT / OR, from an ON
  clause, in UPDATE / DELETE, over a view, CTE or FROM subquery (the
  pin rewrites before it flattens the subquery's own FROM), nor with
  a RIGHT / FULL join.
- **Foreign-key child scans** in EXPLAIN QUERY PLAN (the v0.16 EQP
  gap): under `PRAGMA foreign_keys = ON` a parent DELETE prints, after
  its loops, one line per key naming the table (the most recently
  created child first) — `SCAN c`, `SEARCH c USING COVERING INDEX i
  (k=?)` through the index whose leading columns are most of the
  key's, `SEARCH c USING INTEGER PRIMARY KEY (rowid=?)` — and an
  UPDATE changing a parent key prints them for the old row and again
  for the new.

**Oracle-probing notes** (live 3.54.1 pin):

- Flattened: views, CTEs (once or several times, NOT MATERIALIZED),
  FROM subqueries, nested ones, a LEFT JOIN's one-table right side
  (non-column result expressions guarded), a body with LIMIT under a
  lone reader without WHERE (`SCAN t`, the LIMIT moving out), a body's
  ORDER BY under a lone reader without its own (`SCAN t USING
  COVERING INDEX tb`), the left side of a RIGHT JOIN. Kept: aggregate
  and DISTINCT bodies, a LIMIT under a join, MATERIALIZED, an ordered
  body under a reader with a function in its result list (`abs(a)
  FROM (... ORDER BY b)`, co-routine), an ordered body under an
  aggregate reader, the right side of a FULL JOIN or of a LEFT JOIN
  under DISTINCT (whose LEFT JOIN the pin then omits).
- A UNION ALL body whose arms' column affinities agree is flattened
  into a compound reader (`COMPOUND QUERY` / `MERGE (UNION ALL)` of
  the reader per arm); one whose affinities differ (17h) stays a
  co-routine with the WHERE pushed into each arm.
- Push-down: into aggregate (`SEARCH t ... (b=?)` inside the
  co-routine for a GROUP BY column), DISTINCT and UNION ALL bodies;
  not into a LIMITed one; not a term calling random(); an
  uncorrelated scalar subquery term is pushed (its `SCALAR SUBQUERY`
  shown in the co-routine too); a term reading another table is not.
  A LEFT JOIN's right side takes its ON terms; a WHERE term that makes
  the LEFT JOIN inner is pushed too.
- EXISTS-to-join: correlated and uncorrelated EXISTS, two of them,
  under an aggregate, GROUP BY or DISTINCT reader, beside a LEFT JOIN,
  inside a scalar subquery, over a DISTINCT body or a virtual table
  (`SCAN g EXISTS VIRTUAL TABLE INDEX ...`); not NOT EXISTS, OR, a
  join body, a body with LIMIT or an aggregate, a FROM subquery body,
  an ON clause, UPDATE / DELETE. An automatic index on the semi-join
  table is PARTIAL (the WHERE's literal 1).
- FK child scans: none with `foreign_keys` OFF, none for a child-side
  write or a parent UPDATE of non-key columns; CASCADE keys are
  scanned too; a composite key without an index is `SCAN c3`.

**Tests:** `tests/flatten_v17.rs` — `flattened_plans_match_pin`,
`kept_subqueries_and_push_down_match_pin`, `exists_to_join_matches_pin`,
`foreign_key_child_scans_match_pin`, `unused_columns_are_not_evaluated`
(rows in order and plans, error against error),
`view_column_names_survive_flattening`, `regressions`, and
`generated_corpus_matches_pin` (30 seeds × 12 queries by default,
`HIPP_FLAT_SEEDS` for more: views — plain, aggregate, join, DISTINCT
—, CTEs with and without MATERIALIZED hints and recursive, FROM
subqueries with ORDER BY, LIMIT and UNION ALL, comma / JOIN / LEFT /
CROSS joins, EXISTS / IN terms, ORDER BY and LIMIT; no row set may
differ and no error disagree; measured over 360 queries: no row set differs, no error disagrees, 32 (8.9%) come in another order and 163 (45%) print another plan — the test allows 10% and 50%; the plan differences are the gaps below, chiefly co-routines and materialisations kept in FROM order in a join and UNION ALL bodies not flattened into a compound reader). `hipp-cli/tests/cli_v17_flatten.rs`:
rows and EXPLAIN QUERY PLAN trees byte for byte (flattened views and
subqueries, push-down, EXISTS loops, FK child scans, unused columns).
`tests/eqp_v16.rs`'s documented differences for the flattened view
and the EXISTS subquery now match the pin and moved to its checked
corpus; `tests/hoist_v13.rs` counts its EXISTS executions over
EXISTS with a LIMIT (the pin, and now hipp, run the others as join
loops).

**Gates:** `cargo fmt --all` and `ci/local-gates.sh` (full run) end with
"==> all gates passed". `tests/depth_v16.rs` and `tests/triggers_v16.rs`
pass built with debug info (`flatten_core` is out of line, so the
recursive planning frame does not grow; `Config::stack_budget`'s
constants are unchanged).

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 506,672 → 506,656 (−16: `result_col_names` split into
`col_names_of`, which the flattener reuses); max 1,047,060 →
1,047,052 (−8, the same); xl 1,135,004 → 1,150,692 (+15,688: the
splice, push-down, column pruning, EXISTS-to-join, the semi-join loop
in both executors, the FK child-scan lines and the expansion's
marking). `max` + the leaf measures 1,121,684 B (it pulls in
`join_order`), past the 1 MiB budget, hence `xl` (160,028 B of its
1.25 MiB left). Each ratchet is its measured figure.

**Honest gaps:**

- Not flattened (the pin flattens; rows are the same, plans differ):
  UNION ALL bodies into a compound reader (17), bodies over a virtual
  table, table-valued function, TEMP or attached table, bodies beside
  one, readers that are compound members, readers or bodies with a
  RIGHT / FULL join, a LEFT JOIN's right side that is a FROM subquery
  or WITHOUT ROWID table, bodies with a top-level COLLATE result
  column; flattening is bottom-up (a body flattens its own subqueries
  first), where the pin works top-down, which can name a different
  co-routine after a LIMIT.
- Push-down: not into the arms of UNION / INTERSECT / EXCEPT, not a
  term holding a subquery, not a WHERE term past a LEFT JOIN the
  WHERE makes inner (closed in the review fixes: the LEFT JOIN
  simplification now runs first).
- EXISTS-to-join is not applied over a virtual table.
- Co-routines and materialisations still keep FROM order in a join
  (the pin orders them, and builds automatic indexes and Bloom
  filters on them: `SEARCH s USING AUTOMATIC COVERING INDEX`).
- The LEFT JOIN that EXPLAIN QUERY PLAN omits (`whereOmitNoopJoin`)
  stays as before.

### Query flattening — review fixes (2026-11-06)

**Status:** the review's ten findings on the `flatten` unit, each
reproduced on the live 3.54.1 pin and fixed at its root, or recorded
below as a gap. Gating as the unit: fixes to `flatten`'s own surface
stay in the leaf; the four on paths `max` / `full` already claim are
ungated (marked).

**Fixed:**

- **A CTE's unused columns are computed** (high; a regression of the
  unit): the pin never omits the result columns of a CTE reference it
  does not flatten (its `disableUnusedSubqueryResultColumns` skips
  `isCte`), nor of a correlated FROM subquery — so `WITH r(n,e) AS
  (SELECT a, abs(-9223372036854775807-1) FROM t1 LIMIT 3) SELECT r.n
  FROM r, t1 WHERE r.n=t1.a` is "integer overflow" there (and under a
  WHERE, a LEFT JOIN, NOT MATERIALIZED twice, UNION ALL, a scalar
  subquery). hipp answered rows. `SubPlan::is_cte` (from
  `CteUse::is_cte`) and the plan's correlation now keep every column;
  views and written FROM subqueries are still pruned.
- **More than 64 sources after a merge** (high): the 64-table check
  ran on the FROM list as written, so two views of 33 tables each
  merged into 66 sources, overflowed the join's `u64` source masks (a
  debug-build panic in EXPLAIN QUERY PLAN, a hang on execution). The
  flattener now refuses a core past 64 sources with the pin's "at
  most 64 tables in a join" (`TooManyTables`); EXISTS-to-join stops at
  64 sources (it stopped at 63).
- **MULTI-INDEX OR under a kept subquery** (high, ungated in
  `multi_index`'s single-table path): `xmulti::choose` never marked an
  OR loop whose branches read an earlier source as lateral, so under a
  co-routine or materialisation's loop (the FROM-order executor) its
  rows were found once, for the first outer row: `... FROM (SELECT a,
  c FROM m1 LIMIT 9) s, m2 WHERE m2.x=s.a OR m2.y=s.c` lost the second
  row's matches (LEFT JOIN too).
- **EXISTS-to-join beside a LEFT JOIN** (medium): the LEFT JOIN
  simplification ran after the EXISTS body's WHERE had joined the
  core's, so `w.x = t2.x` made `t1 LEFT JOIN t2` inner and reordered
  it. The simplification (`join::simplify_left`) now runs first in
  `flatten_core`, before any merge, push-down or EXISTS-to-join, as
  the pin's `sqlite3Select` does (never in a core with a RIGHT / FULL
  join, the pin's `JT_LTORJ`, and with only inner joins' ON clauses
  counted — `tests/query_shape_v16.rs`'s corpus caught a FULL JOIN's
  ON clause making the LEFT JOIN before it inner while this was
  built) — which also closes two gaps: a WHERE
  term on a flattened LEFT JOIN body's constant column (`s.one = 1`)
  makes it inner, and a WHERE term is pushed into a LEFT JOIN's right
  side the WHERE makes inner.
- **UPDATE ... FROM computed unread columns** (medium): the omission
  now covers an UPDATE ... FROM's subqueries and views (read by its
  SET expressions, WHERE and ON clauses): `UPDATE t4 SET b=s.a FROM
  (SELECT a, abs(...) e FROM t1 LIMIT 3) s ...` updates, as on the
  pin.
- **Shared materialisations, as the pin chooses them** (medium): a
  view's copies in one FROM clause share one MATERIALIZE only while
  none lost a column (the pin marks a changed copy, as it marks a
  pushed-down one, so `isSelfJoinView` skips it): `SELECT r.n FROM vv
  r, vv r2` is `CO-ROUTINE vv` + `MATERIALIZE vv` and computes each,
  so the unread overflowing column is never raised; a lone copy is
  materialised when a later one stands beside it (rule 5 of
  `fromClauseTermCanBeCoroutine`), and a view brought in by a merged
  body joins its reader's group (`CteUse::grp`, settled per core in
  `prune_sources`). A NOT MATERIALIZED CTE read twice in one FROM
  clause is one shared MATERIALIZE (`CteUse::lazy`), while a
  reference in a co-routine's place is computed alone (so `random()`
  differs between two scalar subqueries, as on the pin). A CTE read
  inside an `x IN (SELECT ...)` counts twice (probed: it is then a
  MATERIALIZE, never a CO-ROUTINE; the pin resolves the IN's SELECT
  twice).
- **Uncorrelated EXISTS was quadratic** (medium): a WHERE term whose
  last source is a semi-join source is now checked at that loop
  (`join.rs`, moved into its ON clause), where the pin codes it, so
  `SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x >
  1000000)` scans `u` once instead of running the whole join below
  each of its rows (`tests/hoist_v13.rs` keeps its LIMITed EXISTS
  counts; the new `uncorrelated_exists_join_is_linear` checks the
  plain one).
- **Foreign-key child scans** (low): each child loop is now planned as
  the pin's `fkScanChildren` read — `parent_value = child.key`, the
  parent value carrying the parent column's affinity and (unless it
  is the INTEGER PRIMARY KEY) its collation — so an IPK / INT parent
  never searches a TEXT / BLOB / untyped child index, a NOCASE child
  index is no use for a BINARY key, a key the index does not hold
  whole is no covering read, and a full scan reads a narrower covering
  index (`SCAN c2 USING COVERING INDEX c2k`). INSERT on a parent now
  prints its scans: one round per REPLACE that can delete (the rowid
  when supplied, each UNIQUE index whose action is REPLACE), the old
  and new rows of an upsert DO UPDATE changing a key, then the
  inserted row's — only for deferred keys unless the INSERT is a
  multi-row write (SELECT, several VALUES rows, a REPLACE, RETURNING,
  INSERT triggers, a key-changing DO UPDATE).
- **EXISTS-to-join beside a row value** (low): `(a,b)=(2,20)` resolves
  to two AND terms; the written-term count now counts them, so the
  EXISTS beside it is converted (a count that cannot place the terms
  converts nothing).
- **The generators and thresholds** (medium): `tests/eqp_v16.rs` has
  a second generated test, `generated_subquery_plans_match_pin`
  (flattened FROM subqueries and CTE bodies, LIMITed ones, EXISTS made
  joins, a LIMITed compound subquery, IN over a FROM subquery — none of
  3,200 differ at 400 seeds, so none may); `tests/join_order_v17.rs`
  adds two joins per schema reading tables through views and FROM
  subqueries and adding EXISTS terms (limits of their own: 2 orders and
  13 plans in 100; measured 2 and 15 of 120 at 60 seeds, 11 and 101 of
  800 at 400 — the misses are the join cost model's, reached through
  the merged sources), its four original joins unchanged; the flatten
  corpus gains overflowing result columns in a view, a LIMITed view, a
  CTE and a FROM subquery, and its limits tighten from 10% / 50% to
  7% / 45% (measured 5.8% / 39% over 360, 6.0% / 43% over 3,600).

Found while widening the corpora, fixed too:

- **A plain LIMITed FROM subquery projected every row** (ungated,
  `v06`): under a reader with a WHERE (so not flattened) the body's
  result columns were computed for all its rows before the LIMIT
  kept 8; a later row's overflowing column was raised. A core without
  aggregate, ORDER BY or DISTINCT now projects only the rows its LIMIT
  keeps (`exec_subplan_inner`'s cap).
- **A probe's scalar subquery in EXPLAIN QUERY PLAN** (ungated,
  `expr_index`): a subquery in an index probe's value or range bound
  is shown with its loop, before the WHERE's other subqueries, as the
  pin codes it (`... AND a=(SELECT 2 ...)` printed its `SCALAR
  SUBQUERY` after the IN's `LIST SUBQUERY`).
- **EXPLAIN QUERY PLAN lost a nested subquery's own source** past a
  merge or a semi-join source (ungated, `join_order`): `remap_plan`
  now moves the nested sources' column base with their slots, so the
  IN operator's own index (`USING INDEX i FOR IN-OPERATOR`) is found
  again there.

**Oracle-probing notes** (live 3.54.1 pin): a CTE reference
co-routine / materialisation raises an error in any column (a LIMITed
CTE under a WHERE, a join, a LEFT JOIN, twice NOT MATERIALIZED, UNION
ALL of two references, a scalar subquery); a flattened CTE does not;
a correlated FROM subquery computes every column, the reader's own
correlation does not count. Views read twice: `SELECT r.n FROM vv r, vv
r2` (LIMIT view, unread `e`) is CO-ROUTINE + MATERIALIZE; with every
column read one shared MATERIALIZE; reading `r.n, r2.e` is CO-ROUTINE +
MATERIALIZE; `t1, vv r, vv r2` with only `r2` pruned is two
MATERIALIZEs; aggregate or DISTINCT views twice share one. NOT
MATERIALIZED aggregate CTE twice in one FROM: one MATERIALIZE (and the
pin builds an automatic index on the second read); in two scalar
subqueries: two co-routines (`random()` differs). A CTE read inside an
IN list — in WHERE or the result list, nested in a scalar subquery
there, in a compound arm — is a MATERIALIZE; in a scalar subquery or
EXISTS a CO-ROUTINE. Parent INSERT child scans: none for a one-row
INSERT, OR IGNORE, DO NOTHING or a DO UPDATE of non-key columns; one
round for several VALUES rows or a SELECT; REPLACE of a supplied rowid
adds a round per conflict check; a deferred key is scanned for any
INSERT; `INSERT OR REPLACE INTO p(v) ...` (no rowid, no UNIQUE) scans
deferred keys only. The pin's materialisation is lazy (`OP_Once` at
the first row reaching the loop): a later source that is never reached
is never computed, so its errors never fire — hipp materialises before
the join (a gap below; the corpus keeps erroring CTEs first).

**Tests:** `tests/flatten_v17.rs` — `review_regressions` (every
finding's repro, rows in order and plans; rows only for the automatic
index the pin builds on a second materialisation), `flattened_join_
past_64_sources_is_refused`, `update_from_omits_unused_columns`,
`uncorrelated_exists_join_is_linear`, `foreign_key_child_scan_rules_
match_pin` (plans), and the widened `generated_corpus_matches_pin`;
`tests/eqp_v16.rs` `generated_subquery_plans_match_pin`;
`tests/join_order_v17.rs`'s widened corpus;
`hipp-cli/tests/cli_v17_flatten.rs` `review_regressions`.

**Gates:** `cargo fmt --all` and `ci/local-gates.sh` (full run) end with
"==> all gates passed"; `tests/depth_v16.rs` and `tests/triggers_v16.rs`
pass built with debug info (STACK_* unchanged).

**Size Δ** (thumbv7em, `--profile tiny`): tiny 76,960 → 76,960 (0);
full 506,656 → 506,728 (+72: the LIMITed plain core's projection
cap, ungated `v06`); max 1,047,868 → 1,048,036 (+168: the cap and the
probe subqueries shown with their loop; 540 B of the 1 MiB budget
left); xl 1,156,732 → 1,159,132 (+2,400: the CTE / correlation / view
group rules of the column omission and its UPDATE ... FROM walk, the
lazy NOT MATERIALIZED sharing, the IN double count, the LEFT JOIN
simplification moved first, the semi-join term placement, the 64-source
check, the planned FK child loops and the INSERT rounds). Each ratchet
is its measured figure.

**Honest gaps** (left):

- Co-routines and materialisations still keep FROM order in a join,
  with no automatic index or Bloom filter on them (most of the
  corpus's plan differences), and UNION ALL bodies are not flattened
  into a compound reader.
- A materialisation is computed before the join, where the pin
  computes it at the first row that reaches its loop: a CTE (or other
  body computing every column) behind a loop that yields no row raises
  its error in hipp alone.
- Nested EXISTS: hipp converts bottom-up (the inner EXISTS becomes a
  join of the body, which then has two sources and is refused), the
  pin top-down (the outer becomes `SEARCH t2 EXISTS`, the inner stays
  a correlated subquery).
- A row value beside an EXISTS converts it, but the pin then orders
  the semi-join after the row value's loop (`SEARCH t ... ; SCAN u
  EXISTS`) where hipp puts it first.
- Push-down into a windowed subquery on its PARTITION BY columns is
  not done (the pin does: `SEARCH t1 USING COVERING INDEX t1b (b=?)`
  inside the co-routine); a subquery with ORDER BY is not flattened
  under a window-function reader or inside a compound arm (the pin
  does).
- A LEFT JOIN's right-side subquery the WHERE makes inner is still
  refused by the expansion's flattening rules (decided before the
  plan's simplification), so it stays a co-routine where the pin
  flattens it.
- UPDATE ... FROM: no flattening or push-down (rows are the pin's, and
  unread columns are not computed; the plan keeps its co-routine).
- Parent-INSERT child scans do not count INSERT triggers that would
  make the write multi-row in every case, nor the upsert's own
  conflict target (its REPLACE rounds come from the statement's and
  constraints' actions).
- The `min()` bare-column tie: a join's `min(x), y` with ties in `x`
  may take `y` from another row than the pin (join order), as before.

### Merging `main` (bolt index-insert speed-up) and one copy of the evaluator (2026-11-08)

Merging `main` ([madmax983/hipp#18](https://github.com/madmax983/hipp/pull/18): the index leaf rebuilt from page bytes, `src/btree.rs`) on top of this wave conflicted in `ci/size-ratchet.toml` and took the merged `max` to 1,048,996 B, 420 B over its 1 MiB hard budget (full 507,592, xl 1,160,028).

The first size-pass item (ADR 0005 draft, Tier A "monomorphic evaluator") makes the room: `query::eval` was compiled twice in `max` / `xl`, once for the executors' storage error type and once for `Error<()>`, which the window functions use to evaluate frame offsets. Under `window` the evaluator's body (`eval_body`) is now compiled once, for `Error<()>`, and `eval` re-types its errors through `Error::map_storage` (the evaluator never touches storage, so the `Storage` arm cannot occur). Without `window` there was only one instantiation, so `eval` calls the body directly and `full` does not pay for the wrapper. Rows and errors are unchanged: every differential suite and the CLI suites pass against the pin.

**Size Δ** against the merged tree (thumbv7em, `--profile tiny`): tiny 76,960 (0); full 507,592 (0); max 1,048,996 → 1,031,072 (−17,924); xl 1,160,028 → 1,142,128 (−17,900). Against this wave before the merge: full 506,728 → 507,592 (+864, `main`'s index-insert change); max 1,048,036 → 1,031,072 (−16,964), now 17,504 B under its budget; xl 1,159,132 → 1,142,128 (−17,004). Each ratchet is its measured figure.

## v0.16 — SQLite-parity push

**Released as 0.16.0 (2026-11-05; `CHANGELOG.md`).** Profiles
(ADR 0003): tiny 76,960 B (budget 128 KiB), full 500,952 B (512
KiB: every rung + `params`, `math`), max 1,035,516 B (1,022,404 B at
the release, + 9,664 B of FTS5 review fixes, + 2,704 B of EXPLAIN
QUERY PLAN review fixes, + 744 B from merging the lazy index-cursor
decoding of `docs/perf-baselines.md`, which also took full from
500,200 B; 1 MiB: full +
`window`, `triggers`, `foreign_keys`, `json`, `expr_index`,
`without_rowid`, `utf16`, `wal`, `datetime`, `attach`,
`autovacuum`, `vtab`, `rtree`, `fts5`, `explain`). The last unit,
EXPLAIN QUERY PLAN, closes the push; the honest gaps of each unit
below are the next milestone's backlog.

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
  for a lone min/max. ~~A three-part `main.t.col` reference is
  refused.~~ (closed by the honest-gap sweep, below)
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
- ~~Constraint-level conflict clauses (`a UNIQUE ON CONFLICT
  IGNORE`, `NOT NULL ON CONFLICT REPLACE`) are still parsed and
  ignored~~ (closed by the honest-gap sweep, below).
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
  FAIL and OR ROLLBACK match the pin there. ~~Column-level conflict
  clauses (`NOT NULL ON CONFLICT FAIL`) are still parsed and
  ignored~~ (closed by the honest-gap sweep, below).
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
  (Closed in v0.17 for a top-level aggregate without GROUP BY of
  count / sum / total / avg / min / max: folded row by row.)

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
  through the table-valued forms, are refused. (~~Table-valued
  pragmas on TEMP objects~~ (closed by the honest-gap sweep, below): `pragma_table_info('t')` /
  `('t', 'temp')`, `pragma_index_list`, `pragma_index_info` ...)
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
  not carried (its affinity is). ~~Three-part column names
  (`main.sqlite_schema.name`) do not parse (hipp-wide).~~ (closed by the honest-gap sweep, below)
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
  support has no recorded demand"; ~~left open~~ closed by the
  honest-gap sweep's review fixes: every write to such a file is now
  refused, `Error::AutoVacuumUnsupported`). Producing a fresh auto-vacuum
  image would hand back a file the next hipp write damages; the
  refusal leaves the file untouched, and `PRAGMA auto_vacuum = 0;
  VACUUM` converts it, as on the pin.

Remaining gaps from the review: ~~a constraint-level `ON CONFLICT
ROLLBACK` is matched by constraint kind, not by the constraint
that failed — a table with `a NOT NULL ON CONFLICT ROLLBACK, b NOT
NULL` ends the transaction on either column's NULL (the pin: only
`a`'s), and a UNIQUE index created with CREATE UNIQUE INDEX takes
the table's UNIQUE-constraint ROLLBACK; an explicit `OR ABORT`
parses as the default, so it does not override a constraint-level
ROLLBACK as it does on the pin. Constraint-level IGNORE / REPLACE
/ FAIL remain parsed and not acted on (as before).~~ (closed by the honest-gap sweep, below). Gates: `cargo
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

- ~~A statement that names a temp table (or a TEMP view reading
  one) and a main object (`SELECT ... FROM main_t, temp_t`,
  `CREATE TEMP TABLE x AS SELECT * FROM main_t`, `INSERT INTO
  temp_t SELECT ... FROM main_t`, a TEMP view over both) is refused
  (`Unsupported`)~~ (closed by the honest-gap sweep, below); a temp UPDATE / DELETE that reads main is
  still refused. TEMP views over main tables are read (review fix,
  below), but `PRAGMA table_info` of one fails (the temp database
  plans it without main), and an ALTER TABLE of a main table such
  a view reads is refused where the pin rewrites the view.
  ~~Three-part names (`temp.x.a`) do not parse;
  `pragma_table_info('x', 'temp')` is refused~~ (closed by the honest-gap sweep, below); `VACUUM temp INTO`
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
- ~~Declared `ON CONFLICT REPLACE` on a column constraint is not
  honoured (the constraints unit's gap)~~ (closed by the honest-gap sweep, below); the REPLACE victim order
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
  pragma off can cancel another statement's violation, and a counter
  below zero fails as one above it, review fix below). Immediate
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
  (no INSERT triggers, RETURNING, REPLACE, or upsert update firing
  UPDATE triggers or changing a key) fails at once on an immediate
  key and, as a parent, resolves nothing.
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
  (an UPDATE action's triggers inherit ABORT — an `INSERT OR IGNORE`
  there fails; a CASCADE DELETE's inherit nothing, review fix
  below), a frame of its own for `changes()` / `last_insert_rowid()`,
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
  `isSetNullAction` quirk: a child-row lookup is skipped when the
  program the pin compiled last in the statement is that key's SET
  NULL action (probed: `INSERT OR REPLACE INTO t VALUES (1, 99)`
  succeeds with 99 missing) — replayed over the pin's whole compile
  order since the review fixes below.
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
  while the pragma is ON and the table has rows (ungated under
  `v16`, `SchemaKind::ReferencesDefault`; an empty table takes it
  since the review fixes). Wave 3's ALTER TABLE RENAME
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
- `isSetNullAction`: see the review fixes below (now replayed over
  the whole compile order; its remaining approximations are listed
  there).
- Mismatch errors inside a trigger body's own action tree surface
  when the body runs (hipp validates one level at prepare).
- Child scans are full scans of the child table per parent row
  changed (O(parents × children)); child indexes are not used.
- SET DEFAULT evaluates a non-constant default once per action
  statement, not per row.
- `defer_foreign_keys` is cleared after table-reading queries,
  writes and (since the review fixes) the pragmas whose programs
  read the file, in autocommit; it is per database: setting it does
  not reach the TEMP database's statements.
- An action program's statement journal: a depth error inside
  BEGIN undoes the statement (as for triggers, STATUS above).

### Foreign-key enforcement — review fixes (2026-11-03)

Five review findings, each reproduced on the live pin first, all
fixed; the generated corpora (the original one and a new one whose
trigger programs write the keyed tables) then found eight more,
also fixed. Regression tests: `tests/foreign_keys_v16.rs`
`review_fixes` / `review_fixes_programs` (and
`generated_corpus_programs`), `hipp-cli/tests/cli_v16_foreign_keys.rs`
`review_fixes_byte_identical`.

**Fixed (review findings):**

- **INSTEAD OF trigger bodies' writes were never checked.** A write
  of a view ended with a bare commit, so the violations its trigger
  bodies counted were reset by the next statement: orphans committed.
  `exec_instead` now ends with the statement's foreign-key check
  (`fk_end_commit`), RETURNING rows of such a write are served
  before an autocommit deferred failure (`serve_returning_fk`), as
  for a table.
- **Single-row INSERT: the key was checked before the rowid
  conflict.** The plain INSERT path's key check (which halts at once
  for a single-row INSERT) ran before `btree_insert` found a
  duplicate rowid; the pin finds the conflict first. So the error
  was the key's, and under OR ROLLBACK the transaction stayed open
  (a later COMMIT kept earlier rows). The rowid is now looked up
  first while keys are checked (the conflict-aware path and multi-row
  writes already did).
- **ADD COLUMN ... REFERENCES ... DEFAULT x refused on an empty
  table.** The pin raises it through `sqlite3ErrorIfNotEmpty`; it is
  now one of ADD COLUMN's table-has-rows refusals (the first the pin
  emits, so it wins over the others), `AlterAdd::empty_only` now
  carrying the error kind.
- **A FAIL halt in a trigger program with violations pending**
  (`RAISE(FAIL)`, a UNIQUE conflict under an outer OR FAIL) aborted
  but reported the program's error; it now reports "FOREIGN KEY
  constraint failed" (`trigger_failed` replaces the error).
- **`isSetNullAction` beyond REPLACE.** The pin skips a child-row
  lookup when the program it compiled *last in the whole statement*
  is that key's SET NULL action — after a BEFORE trigger, REPLACE
  deletion or upsert that compiled it, or in a program after a
  statement that did. `fkey::Sim` now replays the pin's compile
  order once per top-level write (only when some key has SET NULL):
  each program compiled once per statement and conflict action
  (`TriggerPrg` and its `orconf`); an INSERT's BEFORE triggers,
  REPLACE deletions (DELETE triggers only under
  `recursive_triggers`) or upsert UPDATE, the lookup, AFTER
  triggers; an UPDATE's and a DELETE's triggers all first (the pin
  compiles them for `sqlite3TriggerColmask`), REPLACE deletions,
  the lookup, the actions; an action program's UPDATE under ABORT,
  a CASCADE's DELETE inheriting nothing; a program's statements in
  turn. The skipped keys are recorded per (program, statement) and
  per top-level / upsert site; each statement picks its own when it
  loads its keys. The old REPLACE-only model (`fk_note_replace`) is
  gone.

**Fixed (found by the corpora):**

- A CASCADE DELETE's triggers ran under ABORT (an `INSERT OR IGNORE`
  in them failed); the pin compiles them with no conflict action.
- An upsert DO UPDATE counted as a multi-row write whatever it did,
  so a mismatched key it leaves alone failed the INSERT ("foreign
  key mismatch"); it is one only when it fires UPDATE triggers or
  changes a key (`sqlite3FkRequired`), `Connection::upsert_multi`.
- A counter below zero passed the checks: the pin's
  `sqlite3VdbeCheckFkImmediate` / `CheckFkDeferred` fail any non-zero
  count (probed: two orphans left with the pragma off, one counted
  violation, then the parent row: −1, COMMIT fails).
- An UPDATE changing no key checked self-referencing keys anyway;
  the pin's `sqlite3FkRequired` checks none then.
- `defer_foreign_keys` survived the pragmas whose programs read the
  file (probed with the schema loaded: table_info, index_list,
  integrity_check, user_version, page_count, journal_mode, table_list
  … clear it; the flags, encoding, cache_size, auto_vacuum,
  synchronous do not; foreign_key_check / foreign_key_list only when
  a table they read has keys; a pragma that fails never ran).
- `nest_may_abort` counted any key as an abort point when the pragma
  is on; it now follows the pin's `sqlite3MayAbort` calls
  (`fk_may_abort`): an immediate key's lookup of a row added, an
  immediate key's parent removed unless CASCADE / SET NULL, a
  RESTRICT program, only for writes requiring keys, and the action
  programs' writes walked in turn (a SET NULL's own key not looked
  up) — so a depth error inside BEGIN keeps the changes when the
  pin does.
- An upsert DO UPDATE did not read its row again after BEFORE
  UPDATE triggers (`triggers`): a row they — or a cascade they ran —
  deleted was updated anyway, a column they changed was overwritten.
  It now does what UPDATE does: a vanished row is skipped, the
  unassigned columns take their current values, the index entries
  removed are the stored row's.

**Oracle-probing notes:** INSTEAD OF writes fail on immediate
violations (and deferred in autocommit, RETURNING rows first); the
rowid conflict's "UNIQUE constraint failed: c.id" under every
conflict clause, OR ROLLBACK then ending the transaction; ADD COLUMN
REFERENCES DEFAULT on an empty table; `RAISE(FAIL)` with pending
violations; `isSetNullAction` after BEFORE INSERT / UPDATE triggers,
in later statements of a program, through INSTEAD OF bodies, upsert,
and the cache by conflict action (a BEFORE UPDATE trigger compiled
under ABORT by a SET NULL program and again under OE_Default by the
statement makes the second compile the head); negative counters;
which pragmas clear `defer_foreign_keys` (in one connection — a
fresh connection's first schema load clears it too, see gaps); the
abort points (deferred and CASCADE / SET NULL keys hold none, an
UPDATE of a non-key column none).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run
end with "==> all gates passed". The corpora agreed with the pin
over 30+ seeds × 300 scripts each (both generators).

**Size:** tiny **76,924 B** (Δ **0**). Full **486,132 B** (Δ **+56 B**
vs the 486,076 ratchet, raised): ADD COLUMN's empty-only refusal
carrying its error kind (ungated `v16`). Max **606,204 B** (Δ
**+6,948 B** vs 599,256, raised; under the 32 KiB threshold): the
compile-order replay, the abort-point model, the INSTEAD OF / FAIL /
rowid / upsert fixes.

**Honest gaps (after the review fixes):**

- The compile-order replay keys trigger programs by name and the
  conflict action hipp's runtime passes, which matches the pin's
  `orconf` for every path probed; a trigger whose body fails to plan
  contributes no programs. DROP TABLE's implicit DELETE is not
  replayed (its SET NULL programs' own sites never matter: all their
  key values are NULL).
- A fresh connection's first schema load clears `defer_foreign_keys`
  on the pin (`PRAGMA defer_foreign_keys = ON` as the first
  statement, then any schema-reading statement); hipp loads the
  schema when it opens, so it keeps the flag there.
- `nest_may_abort` (triggers unit) still counts an UPDATE of a table
  with a rowid alias as an abort point, where the pin opens no
  statement journal unless the rowid changes: a depth error inside
  BEGIN then undoes the statement where the pin keeps it (probed
  with the pragma off too; the program corpus leaves
  `recursive_triggers` off for it).
- The program corpus keeps bodies that update the child tables off
  the child tables: such scripts can grow the table per row updated
  and run for minutes on both engines.
- The original subsection's other gaps stand (hash order of the
  whole-database `foreign_key_check`, full child scans, SET DEFAULT
  evaluated once per action statement).

### JSON functions (2026-11-03)

**Status:** implemented, differential-tested against the live 3.54.1
pin (`/home/user/hipp-design/oracle/sqlite3`), gated behind the new
leaf feature `json` (in `max`, not `full`). Census (225 probes):
MATCH 186, HIPP_ERR 10, DIFF 2, BOTH_ERR 27 — the four JSON probes
(`f_json`, `f_json_ops`, `f_json_each`, `f_json_agg`) moved
HIPP_ERR → MATCH; no JSON probe is left in the other columns.

**Shipped — leaf feature `json`** (`json = ["v16"]`, in `max`;
`v16,json` in `ci/feature-sets.txt`; new `src/json.rs`,
`src/session/json_tvf.rs`; `ScalarFn::Json`, `AggKind::Json`,
`Tok::Ptr`):

- `json`, `jsonb`, `json_array` / `jsonb_array`, `json_object` /
  `jsonb_object`, `json_extract` / `jsonb_extract` (one path: the
  SQL value; several: a JSON array), the `->` / `->>` operators
  (lexed as `TK_PTR` at the precedence of `||`, left-associative; an
  INTEGER right side is `[N]` / `[#-N]`, an all-alphanumeric label
  `.label`, `[...]` itself, anything else `."text"`), `json_insert`
  / `json_replace` / `json_set` / `json_array_insert` and their
  `jsonb_` forms (`$[#]` appends; NULL paths skipped; `$` replaces
  the whole document), `json_remove` / `jsonb_remove`, `json_patch`
  / `jsonb_patch` (RFC 7396 over JSONB, the pin's in-place
  algorithm), `json_type`, `json_valid` (flags 1 / 2 / 4 / 8),
  `json_quote`, `json_array_length`, `json_error_position`,
  `json_pretty`, the aggregates `json_group_array` /
  `json_group_object` (and `jsonb_`; DISTINCT, aggregate ORDER BY,
  FILTER, and as window functions when `window` is built), and the
  table-valued `json_each` / `json_tree` / `jsonb_each` /
  `jsonb_tree` in FROM with all ten columns (`key value type atom
  id parent fullkey path`, hidden `json` / `root`), the rowid, the
  optional root path, joins, and correlated (lateral) arguments
  (`FROM t, json_each(t.doc)`, LEFT JOIN, inside subqueries).
- **The JSONB format, byte for byte:** text is translated as the pin
  does (minimal headers; numbers and strings keep their source text
  — `json('1e5')` is `1e5`, a hex literal is INT5 — TEXT / TEXTJ /
  TEXT5 / TEXTRAW chosen by the escapes a string holds, Infinity as
  `9e999`), edits splice the blob with the pin's size-adjustment
  walk and its `jsonBlobOverwrite` trick (a value replaced by one up
  to 8 bytes shorter gets a denormalized header instead of a move:
  `jsonb_set('[12345]','$[0]',1)` is `6BE30000000131`), a blob
  argument is JSONB when the pin's `jsonArgIsJsonb` says so (a short
  blob that reads like JSON text must pass the strict check), and
  JSONB built from text that hits the nesting limit is the pin's
  partial blob.
- **JSON5 input:** comments, unquoted keys (identifier bytes,
  `\uXXXX`), single quotes, `+`, `.5` / `5.`, hex, trailing commas,
  Infinity / NaN / QNaN / SNaN in any case, the JSON5 white space
  set and escapes (`\x41`, `\'`, `\v`, `\0`, line continuations) —
  rendered canonically (`A`, `\u000b`, `5.0`, `0.5`, `9e999`).
  `json_error_position` reports the pin's character offsets, the
  quirks included (trailing garbage is position 1; a container's
  bad continuation reports where the probe started).
- **Values:** REALs into JSON text render as the pin's REAL → TEXT
  conversion (`0.30000000000000004`, `1.0e+300`), ±Inf as
  `9.0e+999` (into JSONB by the edit functions: `9e999`); integers beyond i64 come
  back REAL, `-9223372036854775808` INTEGER, a 16-digit hex with
  the top bit set a positive REAL; strings decode with the pin's
  escape table (lone surrogates kept, invalid escapes dropped, a
  trailing line continuation a NUL).
- **The JSON subtype** (`query::eval_json_arg`): a JSON function's
  text result is JSON to the JSON function it feeds (`json_array(json('[1]'))`
  is `[[1]]`), through `+x`, COLLATE, CAST AS TEXT, CASE,
  coalesce / ifnull, iif, nullif, scalar and aggregate min / max,
  likely / unlikely / likelihood, a JSON aggregate (window results
  too), and a `json_each` `value` that is an array or object
  (`json::Op::EachValue`); lost through tables, subqueries, views,
  CTEs and every other function, as on the pin.
- **Limits:** text nesting past 1000 is "malformed JSON"; a path
  lookup more than 1000 calls deep is "JSON path too deep" (the
  pin's count: one call per step plus the final one, a created
  substructure continuing the count); rendering a container deeper
  than 1000 (pretty: any node) or merging deeper than 1000 is
  "JSON nested too deep". Every walk is iterative: no recursion on
  input-derived depth.
- **Errors** (remedy-named `SchemaKind` variants): `JsonMalformed`,
  `JsonBadPath`, `JsonPathTooDeep`, `JsonNestedTooDeep`,
  `JsonBlobValue`, `JsonObjectOddArgs`, `JsonObjectLabel`,
  `JsonOddArgs`, `JsonValidFlags`, `JsonNotArrayElement`; wrong
  argument counts and `json_each(a, b, c)` are prepare-time
  `WrongArgumentCount`.
- **Table-valued sources** (`session/json_tvf.rs`): a call plans as
  a source whose placeholder derived plan holds the resolved
  arguments as its one VALUES row, so every plan walker
  (correlation, shifts, aggregates, subqueries) sees them; arguments
  that read only enclosing queries run once per execution, ones
  that read a source to the left run per row of the sources before
  it, after the WHERE terms (and an inner join's ON terms) that read
  only those sources — the pin tests those first, so `FROM t,
  json_each(t.j) WHERE json_valid(t.j)` never parses a malformed j.
  `key`, `value` and `atom` hold the element's JSONB and decode
  when read (the pin computes columns lazily: `count(*)` over a
  document too deep to render answers); a scalar subquery's first
  row stops the cursor there. An element whose header does not
  decode stops the cursor with "malformed JSON" (the pin's).
- **Ungated fix (claimed v0.4 paths): conditions short-circuit as
  the pin's jump code does** (`query::cond`, `cond_row_sub`): WHERE,
  ON, UPDATE / DELETE / upsert WHERE, aggregate FILTER, searched
  CASE WHEN and iif conditions stop an AND at a term that is not
  true and an OR at one that is, so `DELETE FROM t WHERE
  json_valid(a) AND json_array_length(a) = 3` (and `x = 0 AND
  abs(-9223372036854775808) > x` in a WHERE) no longer fail on rows
  the first term rejects. A skipped operand that is a deterministic
  constant is still computed in WHERE / ON / FILTER (the pin factors
  constant terms out of its loop, so `WHERE x = 1 AND
  abs(-9223372036854775808) > 0` fails there), never in CASE WHEN /
  iif. Value contexts (`SELECT a AND b`) still evaluate both
  operands, as the pin does.

**Oracle-probing notes** (all on the live pin; recorded before
implementation, refined by the corpora): the JSONB headers are
minimal on translation and after edits, but untouched parts of a
blob argument keep their own (even 9-byte) headers; the overwrite
trick applies to replacements 1..=8 bytes shorter (`d` must fit a
larger header: 1/2/4/8 from a 1-byte header, 1/3/7 from 2, 2/6 from
3, 4 from 5); `jsonb_array` / `jsonb_group_array` go through text
(`TEXTJ` for an escaped string) while `jsonb_set` stores a SQL text
as TEXTRAW; `json_array_insert` exists in 3.54 (errors "not an
array element" when the path's last byte is not `]`, before
anything else); `json_valid` reads FLAGS with `sqlite3_value_int64`
before looking at X (`json_valid(NULL, 0)` errors); `jsonb_each`
/ `jsonb_tree` exist and return JSONB for containers; the hidden
columns' values (`json` is the input text up to its first NUL, or
the blob); `json_tree` with a root path reports the root row's key
from the path text (`'$.a[1]'` gives the key `a[1]`, the pin's
`jsonEachPathLength`); `->` / `->>` bind tighter than `*` and unary
minus binds tighter than `->`; the depth thresholds above (probed
with crafted JSONB at 998..1002 levels); `x = 1 AND json('x')` in a
SELECT list fails on the pin, but not as a CASE WHEN / iif / WHERE
condition.

**Bugs the corpora found during the unit** (all fixed, regression
lines in `tests/json_v16.rs` `regressions`): `json_patch` read the
target's stale header after converting a non-object (the pin bounds
payloads by the pre-edit size); `json_array_insert` raised "not an
array element" only after a missing intermediate index; hex digits
in `\u` escapes of blobs use the pin's nibble rule (`\u\x8c041` is
U+C041); FLOAT5 `-` alone is malformed; JSONB detection in
`json_error_position`, `json_each` and edit values is the strict
`jsonArgIsJsonb`; eagerly decoded `json_each` columns failed
`count(*)`; a malformed element made the cursor loop; scalar
lookup found the math functions first and hid the JSON ones (`json`
vs `math` ordering); the `json_each` WHERE pre-test (above) and a
LEFT JOIN's ON, which the pin does *not* test first.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run end
with "==> all gates passed" (`v16,json` compiles warning-free alone).
New suites: `tests/json_v16.rs` (16 tests: JSONB / JSON5 text,
values, paths and operators, edits / patch / overwrite, subtype
rules, aggregates and windows, table-valued functions, schema and
write paths, nesting limits, large documents, remedy-named errors,
regressions, and three generated corpora — random JSON / JSON5 /
malformed text with random paths through every function, random
JSONB blobs with denormalized headers and corrupt bytes, random
documents in a table through aggregates, windows, lateral joins and
per-row edits; 1,500 cases each by default, `HIPP_JSON_CORPUS`
scales them, 10,000 agreed); `src/json.rs` unit tests;
`hipp-cli/tests/cli_v16_json.rs` (2 tests, byte-identical one-shot
and over stdin: pretty text, raw blobs, tables and aggregates, and a
generated corpus of valid documents through json / jsonb / paths /
edits / json_tree).

**Size:** tiny **76,924 B** (Δ **0**). Full **486,316 B** (Δ **+184 B**
vs 486,132, ratchet raised): the ungated condition short-circuit
(`query::cond` with its constant factoring, `cond_row_sub`) — the
leaf itself is not in `full`. Max **638,756 B** (Δ **+32,552 B** vs
606,204, raised; just under the 32 KiB threshold): the JSON engine
(text / JSON5 parser, renderer, pretty printer, paths, edits, merge
patch, validity check, the `json_each` cursor) and its wiring.

**Honest gaps:**

- The subtype does not survive a FROM subquery, view or CTE over
  `json_each`'s `value` (`SELECT json_array(v) FROM (SELECT value v
  FROM json_each('[[1]]'))` is `["[1]"]`; the pin flattens the
  subquery and keeps it), nor an aggregate min / max whose argument
  is only sometimes JSON (`min(json_extract(...))`).
- A `json_each` value column compares with no collation (it reads
  through the subtype wrapper), where the pin's column has BINARY:
  only a comparison against a NOCASE / RTRIM column differs.
- `json_each` / `json_tree` without arguments (`FROM json_each WHERE
  json = ...`, the hidden-column constraint form) is refused
  (`Unsupported`), as are arguments reading a source to the right
  (the pin reorders the join; closed in v0.17: the `join_order`
  planner runs that source first) and a lateral call on the right of a
  RIGHT / FULL join; `json_each()` with empty parentheses has no
  rows, as on the pin.
- Rows stream on the pin: a statement that fails part-way prints the
  rows before the failure there, none in hipp (both fail). (Closed
  in v0.17 for top-level queries: streamed rows.) Columns
  other than key / value / atom are computed for every row, and an
  object whose last label has no value reads NULL where the pin reads
  past the blob's end.
- No parse cache: each call re-translates its text argument (the
  pin keeps recent parses per statement), so a large document read
  many times per row costs that many parses; edits copy the blob.
- (HAVING / CHECK short-circuit and the WHERE constant-term model:
  fixed by the review below.)

### JSON functions — review fixes (2026-11-03)

Six review findings, each reproduced first (release build, live pin).

**Fixed (regression tests in `tests/json_v16.rs`:
`large_documents_scale`, `review_regressions`, `tvf_in_triggers`):**

- **`json_each` / `json_tree` memory.** Rows no longer carry the
  document: the call's document goes into a per-statement registry
  (`Env::json_docs`, `json::TvfDocs`; `Statement::step` drops what
  its step added when it returns, a nested trigger step only its
  own), and the key / value / atom / hidden `json` columns hold
  16-byte handles decoded when read (`json::each_column`, a new
  `Op::EachJson` wrapper for `json`). A lateral call also clears the
  left-hand values that only its arguments read before joining its
  rows (`SubSource::tvf_dead`, planned per core from every other
  reader: items, WHERE, GROUP BY, HAVING, ORDER BY, ON clauses,
  other sources' plans, index probes), so `FROM d, json_each(d.j)`
  no longer copies `d.j` into each row. Measured (release): 20,001
  integers through `FROM d, json_each(d.j)` 0.10 s / 37 MB (was
  35-53 s / 8.3 GB); a 4.9 MB object of 200,000 members 0.8 s /
  330 MB through `json_each` and 2.8 s / 1 GB through `json_tree`
  (600,001 rows), both SIGKILLed before.
- **Quadratic text parse.** `splice` rebuilt the whole blob each
  time a container's header shrank on close; it now edits in place
  (`AVec::replace_range`, the pin's memmove — only the container's
  own payload moves). `json_valid` over 1.2 MB of 100,000 objects
  1.7 s → 0.03 s; `j ->> '$.k5[1]'` over 4.9 MB 30.7 s → 0.06 s.
- **NEW / OLD in a table-valued function's arguments** inside a
  trigger body (`json_each(new.a)`, also in a scalar subquery, an
  `IN (SELECT key FROM json_each(new.a))`, UPDATE ... FROM
  `json_each(new.a)`): the binder (`sql::trig`) now binds
  `FromSource::args`, with only the sources before the call in scope
  (`json_each(new.a) AS new` still reads the trigger row), and ALTER
  TABLE RENAME COLUMN rewrites them.
- **AND / OR / NOT jump code, ungated** (claimed paths): `query::cond`
  now answers "is it true" / "is it false" with early stops through
  NOT as well (`NOT (json_valid(b) AND b ->> 0 = 1)`), and is used
  by aggregate FILTER over a table (`agg_row`, which the earlier fix
  missed), HAVING (`cond_group_sub`) and CHECK (`cond_false`, a CHECK
  fails only when false). With subqueries (`cond_row_sub`, now an
  async walk) a subquery runs only when its operand is reached, and
  an AND / OR whose left operand holds a subquery and right one does
  not tests the right one first (the pin's `exprEvalRhsFirst`). A
  WHERE clause (`where_row_sub`) tests its terms in order, those
  holding a correlated subquery last (`TERM_VARSELECT`) — except a
  top-level `x IN (SELECT ...)`, which the pin tests in place
  (probed).
- **WHERE constant terms, ungated.** The per-row "evaluate a
  skipped constant" model is gone (it failed `a = 1 OR json('[') IS
  NULL` and a LEFT JOIN ON constant, which the pin never reaches).
  Instead the top-level constant AND terms of WHERE and inner-join
  ON clauses are computed once, in order, before any row is read,
  stopping at the first that is not true, which then rejects every
  row — so they fail over an empty table too, as on the pin
  (`query::constant_terms_hold` for UPDATE / DELETE,
  `join_constants_hold` for SELECT cores, sync and async joins).
  For a SELECT this follows the pin's constant propagation: a term
  `column = literal` (BINARY, the last for a column, scanning right
  to left) stands the literal, with the column's affinity, in for
  the column in the other terms (a BLOB-affinity column only as a
  comparison operand), so `x = 1 AND (x = 2 OR json('[') IS NULL)`
  fails up front and `a = 1 AND json('[') IS NULL AND a = 2` does
  not (`2 = 1` stops first).
- **Compound scalar subquery subtype.** `json_array((SELECT
  json('[1]') UNION ALL SELECT 2))` is `[[1]]` as on the pin: a
  compound (not INTERSECT) whose arms' result columns are each
  always-JSON text or never text keeps the subtype on a text value
  (`query::sub_literal`).
- **Found while fixing:** with HAVING now stopping AND / OR early,
  `HAVING count(*) > 5 AND sum(v) > 0` skipped `sum(v)`'s overflow;
  the pin finalizes every aggregate of a group, the HAVING's
  included, before testing it — now so in all three HAVING paths
  (`tests/aggregates_v16.rs` caught it).

**Not changed:** item 6 of the AND/OR finding (an index- or
rowid-satisfiable term tested first) already matched the pin after
the `expr_index` unit (`WHERE b -> 0 = 1 AND a = 2` with an index on
`a` or `a INTEGER PRIMARY KEY`), and is now pinned by tests.

**Oracle-probing notes:** constant terms evaluate in written order
and stop at the first false one (`1 = 2 AND json('[')` succeeds on
an empty table, the reverse fails); FILTER, CASE WHEN, iif and a
LEFT JOIN's ON factor nothing; an uncorrelated EXISTS term is
computed up front, an uncorrelated scalar subquery term is not;
`x NOT IN (correlated)` and `(x IN (correlated)) IS NOT NULL` wait,
`x IN (correlated)` does not; a compound keeps the producing arm's
subtype (UNION / UNION ALL / EXCEPT), INTERSECT drops it; pragma
table-valued functions inside a trigger body or a view fail on the
pin when used ("unsafe use of virtual table") — hipp runs them
(pre-existing, the PRAGMA unit's surface).

**Gates:** `cargo fmt --all`; full `ci/local-gates.sh` ends with
"==> all gates passed". `tests/json_v16.rs` grows to 19 tests.

**Size:** tiny **76,924 B** (Δ **0**; the `json_each` document
release sits inside `step`'s body, so no extra future layer reaches
the tiny build). Full **489,396 B** (Δ **+3,096 B** vs 486,300,
ratchet raised, headroom 34,892 B): all ungated — the async
condition walk (`cond_is_sub`, `where_row_sub`), the WHERE
constant-term pass with constant propagation, the HAVING / CHECK
jump code and HAVING's aggregate finalization, the trigger binder's
TVF arguments. Max **677,252 B** (Δ **+6,208 B** vs 671,044, raised):
the above plus the `json_each` document registry and handles, the
lateral dead-slot planning and the compound-subtype rule.

**Honest gaps (remaining):**

- A join still copies each left row into every combination: when a
  large left value is read by something besides a lateral call's
  arguments (`FROM d, json_each(d.j) WHERE length(d.j) > 5`), or for
  any plain table join (`FROM d, t2` with 20,000 rows of t2 against a
  100 KB `d.j` aborts on allocation, pre-existing and not JSON's),
  memory is O(rows × size). Reading the hidden `json` column copies
  the document per row read (`count(json)` over 20,000 rows of a
  100 KB document takes 2 GB).
- Rows are still built for the whole call up front (O(rows) small
  rows: ~1.6 KB each with the join's copy), where the pin streams.
- Constant propagation is not transitive (`t.a = u.a AND u.a = 2 AND
  (t.a = 3 OR json('[') IS NULL)` fails on the pin, not in hipp),
  binds only literal values, and is skipped when a RIGHT / FULL join
  is present; an uncorrelated EXISTS term is not computed up front;
  a correlated subquery's outer-column terms are not constants.
- A compound scalar subquery with an arm that may return plain text
  (`SELECT json('[1]') UNION ALL SELECT 'x'`) drops the subtype even
  when the JSON arm produced the value (hipp does not track which
  arm a row came from).

### Expression indexes, partial indexes and index-aware planning (2026-11-03)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated behind the new leaf feature `expr_index` (`expr_index =
["v16", "indexes"]`, in `max`, not `full`; `v16,expr_index` in
`ci/feature-sets.txt`). The unit measured +32.3 KB of `max`, which
would leave `full` (486 KB) less than 6 KB under the ~500 KiB line
ADR 0002 draws, so it is a leaf, as the unit's brief directs. Census
(246 probes, 21 new): MATCH 203, HIPP_ERR 8, DIFF 2, BOTH_ERR 33 —
`partial_index` and `expr_index` moved HIPP_ERR → MATCH, every new
`xi_*` probe matches (15) or fails on both (6).

**Shipped — leaf feature `expr_index`** (new `src/xplan.rs`,
`src/session/xindex.rs`; `schema::IndexExt`, `query::SubSource::access`):

- **Definitions.** `CREATE [UNIQUE] INDEX ... ON t(<expr> [COLLATE c]
  [ASC|DESC], ...) [WHERE <expr>]`: every key item is an expression;
  a bare column, a lone string (the pin's `sqlite3StringToId`) and
  either under a top-level COLLATE stay column items (`(a)`, `'a'`,
  `(a COLLATE NOCASE)`); an expression column's collation is its
  explicit COLLATE, else BINARY. The pin's errors as remedy-named
  kinds: `SubqueryInIndex`, `NonDeterministic` (random, randomblob,
  changes, total_changes, last_insert_rowid, sqlite_version),
  `QualifiedIndexColumn` (`t.a` in a key expression), `MisusedAggregate`,
  `MisusedWindow`, `UnknownColumn` (`rowid` in a key expression, an
  unknown name), `RowValueMisused`, `ParameterNotAllowed`; the WHERE
  allows `t.b` and `rowid`. Stored texts are the pin's; ALTER TABLE
  RENAME / RENAME COLUMN rewrite them, DROP COLUMN refuses a column an
  index expression or WHERE reads, all as the pin (the alter.rs token
  walk already covered both).
- **Maintenance.** One key function (`ix_key` / `xindex::ext_key`)
  serves every write path: INSERT (plain, OR REPLACE, OR IGNORE,
  REPLACE, upsert DO UPDATE / NOTHING), UPDATE (incl. OR REPLACE),
  DELETE, the REPLACE victim delete, CREATE INDEX population, REINDEX
  and integrity_check. A row a partial index's WHERE does not make true
  (NULL excludes) has no entry, so UNIQUE is enforced only among the
  admitted rows. VIRTUAL generated columns are computed before the
  expressions read them; REAL columns read as REALs (`rules::eval_row`).
- **Upsert targets** (the pin's `sqlite3UpsertAnalyzeTarget`): target
  items are expressions; the newest UNIQUE index whose key columns are
  exactly the items (any order, an item's COLLATE matching) is the
  target, a partial one only when the target's WHERE is the same
  expression as the index's (`b > 0` matches `(b > 0)`, not `0 < b` or
  `b > 1` — probed); such a target arbitrates only that index's
  conflicts (`UpsertPlan::target_root`).
- **Introspection.** `PRAGMA index_xinfo` / `index_info` give an
  expression column cid -2 and no name (an INTEGER PRIMARY KEY column
  keeps its cid, as on the pin); `index_list`'s `partial`;
  integrity_check computes each row's entry and, for a partial index,
  compares its entry count with the admitted rows (the pin's counter);
  VACUUM copies the indexes byte for byte; ANALYZE writes their stats.
- **Files the pin made** with such indexes are read and written; the
  pin's integrity_check passes on every file hipp wrote in the suites.
  An index hipp cannot compute (a function it lacks, e.g. the pin's
  `date()`) makes writes to its table fail (`UnknownFunction`) rather
  than leave the index stale; reads work.
- **Index-aware planning** (`xplan`): each FROM source of every query
  core (top level, subqueries, views, CTE bodies, trigger programs),
  and each UPDATE / DELETE target, gets the access path the pin's
  planner would pick for it from the schema and the query: the rowid
  B-tree (`=`, IN, ranges, via a seekable `ScanCursor::from_rowid`) or
  an index (an equality / IN prefix, then a range — also on the
  trailing rowid —, or a full scan), a partial index only when the
  WHERE implies its WHERE (`sqlite3ExprImpliesExpr` with the pin's
  `exprImpliesNotNull` walk), an expression index when the query names
  the same expression (`query::same_expr`). Terms: `=`, `IS`, `<`,
  `<=`, `>`, `>=`, `BETWEEN`, `IN (list)`, `IS NULL`, the pin's
  OR-to-IN rewrite and its virtual `x > NULL` for `x IS NOT NULL`; a
  term is usable only under the index column's collation (any for IS
  NULL) and when the pin's `indexAffinityOk` allows it, and its probe
  is converted by the comparison's affinity (`query::index_probe_aff`).
  The chooser reproduces the pin's no-statistics cost model in its
  `LogEst` arithmetic (`whereLoopAddBtree[Index]`, `whereRangeScanEst`,
  `whereLoopOutputAdjust`, `whereSortingCost`, the two-pass
  `wherePathSolver` with its (cost, rows, unsorted-cost) tie order,
  `sqlite3DefaultRowEst`, `szEst` widths, covering / ORDER BY / partial
  full index scans) and the pin's "ordered inner loop" scan direction.
  The executor reads only the rows the path finds, in its order, and
  still evaluates every WHERE / ON term on them, so a plan can change
  the order and the work, never the rows. A join source whose probes
  read an earlier source is looked up per combination (`join_step`);
  RIGHT / FULL joins keep whole tables. Rows that arrive in GROUP BY
  order are grouped as they arrive (a DESC index emits groups largest
  first, as the pin skips its group sorter). An UPDATE visits rows in
  rowid order (the pin's two-pass write) unless it is the pin's
  one-pass loop — no RETURNING, UPDATE trigger, foreign keys, FROM,
  REPLACE, rowid change or WHERE subquery, and the loop's index not
  changed by the SET — which keeps the index order (an `UPDATE OR FAIL`
  stops where the pin stops); DELETE visits rowid order. A probe value
  may be an uncorrelated or correlated subquery (evaluated on the row
  padded to the core's width); non-deterministic values are not
  probed. `SELECT col FROM t` on a table with an index goes through the
  general planner (the pin may read a covering index).

**Oracle-probing notes** (all on the live pin, before and during
implementation): the error matrix above (`'x'` as a key item is a
column name, `1` and `(a, a) = (1, 1)` in a WHERE are accepted, `x.a`
is "no such column", `main.t.b` in a WHERE is accepted — hipp's parser
refuses three-part names anywhere, a pre-existing gap); index_xinfo
rows (`(d)` over an INTEGER PRIMARY KEY is cid 3); `UNIQUE constraint
failed: index 'u'` for an expression index; upsert target matching is
an exact expression compare; RENAME COLUMN to `[new col]` writes
`"new col"`. Planner: no-ORDER-BY output follows the chosen index
(ranges in key order, IN lists in the key column's direction — a DESC
column walks them largest first —, `SELECT b FROM t` through a narrower
covering index, `ORDER BY b DESC` over an ascending index gives ties in
reverse rowid order, GROUP BY over `b DESC` emits groups descending);
ties between equal-cost indexes go to the newest (the pin's prepended
`pIndex` list), except that among loops delivering the ORDER BY the one
with fewer key columns wins (`(d)` over `(d, a)`); an IN on an earlier
column charges its iterations at its own level only (`b IN (16 values)
AND c = ?` still prefers `(b, c)` to `(c)`); `x IS NULL` uses an index of
any collation; `a + 1 = 1` implies `a IS NOT NULL` for a partial index;
an ORDER BY a loop does not deliver still reverses it when the loop's
IN columns held constant would (`d IN (...) ORDER BY b DESC` over
`(d, b)`); UPDATE / DELETE with RETURNING or triggers visit rowid order,
a one-pass UPDATE the index order (`UPDATE OR FAIL ... WHERE b > 2`).
The pin's oracle build has no STAT4 (`PRAGMA compile_options`).

**Bugs found during the unit** (all fixed, regression lines in
`tests/expr_index_v16.rs` `regressions`): IN lists walked ascending on a
DESC key column; `IS NULL` refused for an index of another collation;
equal-cost ties broken by rows instead of the unsorted cost (a covering
scan lost to the table scan); `exprImpliesNotNull` without the
arithmetic walk (a partial index ignored); IN columns treated as ending
the delivered ORDER BY; groups re-sorted after an index delivered a
DESC GROUP BY order; a scalar-subquery probe evaluated on the unpadded
outer row (wrong slots — caught by `tests/subqueries.rs`); the legacy
`SELECT col FROM t` path skipping covering index scans.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run end
with "==> all gates passed" (`v16,expr_index` compiles warning-free
alone, as does `indexes`). New suites: `tests/expr_index_v16.rs` (13
tests: partial and expression index maintenance, definition errors,
introspection, ALTER / REINDEX / VACUUM, pin-made files written by hipp
and checked by the pin, planner order, write order, joins and
subqueries, regressions, two generated corpora — random schemas /
indexes / data / queries compared row-for-row in order, random writes
with RETURNING / OR FAIL / REPLACE / GROUP BY / correlated subqueries /
joins plus the pin's integrity_check on every file — 150 and 75 cases by
default, `HIPP_EXPRIDX_CORPUS` scales them, 800 / 400 agreed — and a
100,000-row table where 300 index lookups run faster than 3 full scans
and agree with the pin), `src/xplan.rs` unit tests (LogEst, szEst),
`hipp-cli/tests/cli_v16_expr_index.rs` (3 tests, byte-identical
one-shot and over stdin: schema texts and pragmas, planner order, a
generated corpus). Existing suites updated where they asserted the old
refusals (`tests/indexes.rs` — plus a new test that an uncomputable
index blocks writes —, `tests/maintenance_v16.rs`, `tests/pragma_v16.rs`).

**Size:** tiny **76,924 B** (Δ **0**). Full **486,300 B** (Δ **−16 B**,
ratchet lowered: the write paths' shared key helper). Max **671,044 B**
(Δ **+32,288 B** vs 638,756, raised; just under the 32 KiB threshold):
the index definitions and maintenance (~7 KB) and the planner, its cost
model and the access-path executor (~25 KB).

**Honest gaps:**

- The planner is a model of the pin's, not the pin's: joins run in FROM
  order (the pin may reorder them; the suites compare join rows without
  their order), no automatic indexes, no MULTI-INDEX OR, no LIKE prefix
  ranges, no skip-scans, and `sqlite_stat1` / `sqlite_stat4` are never
  read (after ANALYZE the pin may choose another index; hipp keeps the
  default estimates). (Closed in v0.17 by the `join_order` leaf: join
  order, automatic indexes, Bloom filters and `sqlite_stat1`; MULTI-
  INDEX OR, LIKE ranges, skip-scans and `sqlite_stat4` remain.) Some tie and cost details remain unexplained: a
  partial index whose WHERE is `x IS NOT NULL` competing with an index
  led by `x` (the pin's choice there depends on creation order in a way
  the model does not reproduce; 1 in ~1,000 generated cases still
  differs in row order).
- Row order of an UPDATE treats foreign keys conservatively (any FK
  enforcement makes it rowid order; the pin only when the FK columns
  change) ~~and detects constraint-level `ON CONFLICT REPLACE` by the
  table text~~ (the honest-gap sweep: the pin's `bReplace`, an index
  declared REPLACE whose key the SET changes).
- ~~`INDEXED BY` / `NOT INDEXED` are not parsed.~~ (closed by the honest-gap sweep, below) (A covering index scan
  read each table row by rowid; fixed by the review below for rowid
  tables.)
- An expression index's key value takes no affinity (the pin applies the
  expression's affinity, which only matters for a CAST to REAL of an
  integral value: equal keys either way).
- ~~Three-part names (`main.t.b`) are refused everywhere, so such a
  partial index WHERE written by the pin is a definition hipp cannot
  compute: writes to its table are refused.~~ (closed by the honest-gap sweep, below)

### Expression indexes and index-aware planning — review fixes (2026-11-04)

Five review findings, each reproduced first against the live pin and
the pre-fix build (debug builds, `select count(*) ...` over a
10,000-row table with an index on `a`).

**Fixed (regression tests in `tests/expr_index_v16.rs`:
`review_regressions`, `generated_review`; CLI
`cli_v16_expr_index_review_byte_identical`):**

- **Index-driven reads slower than the scans they replaced** (medium).
  The cause was not the subqueries: every row an access path found was
  re-read through `btree::seek_rowid`, which copied each page on the
  way down and parsed every cell of it, about 100 µs a row in a debug
  build. That cost hit covering scans (`count(*)` now reads a narrower
  index, as the pin does), rowid ranges, and any WHERE whose plan
  changed to an index walk. Three fixes. `seek_rowid` now
  binary-searches the borrowed frame and copies only the found cell's
  payload, and bounds the descent depth so a cyclic file errors. It is
  gated with the leaf (`expr_index`, whose plans seek per row), so
  builds without it keep the old seek and tiny does not grow (the
  ungated form measured +492 B tiny). A rowid
  range streams its rows from the cursor instead of seeking them again.
  A covering index builds its rows from the entries (the
  `IdxAccess::cover` map, rowid tables only), so no table lookup
  happens. To keep that safe, the planner's "columns read" now also
  counts a table-valued source's arguments (`core_reads`). Measured
  (debug, 10k rows; earlier figures are the pre-fix build):
  `count(*) FROM t` 1.05 s → 0.026 s (the no-index table 0.024 s);
  `WHERE exists (SELECT 1 FROM s WHERE x=7)` 1.13 s → 0.031 s;
  `WHERE +a IN (SELECT x FROM s)` 1.11 s → 0.040 s; `WHERE rowid >
  5000` 0.52 s → 0.034 s. An uncorrelated subquery was already
  computed once per statement (`sub_value_for`'s hoist).
- **`x IN (SELECT ...)` never used an index** (medium). It is now a
  term (`xplan::TOp::InSub`) costed as the pin does (a subquery is
  taken to return 25 rows, LogEst 46). The probe set is the
  subquery's values, computed once when uncorrelated, converted by the
  comparison affinity, and sorted and deduplicated under the IN
  collation (`query::in_sub_coll`). Rows therefore come out in the
  pin's ephemeral-table key order (ints, then text). This works on
  plain, expression and rowid keys, in SELECT cores, lateral join
  lookups, and UPDATE / DELETE targets (uncorrelated only, not
  UPDATE ... FROM). `a IN (SELECT x FROM s)` over 10k rows went from
  1.08 s to 0.009 s. One bug turned up while doing this: a write's
  probes were evaluated on the bare empty row, so a subquery read its
  own columns from the wrong slots and found nothing (`DELETE ...
  WHERE a IN (SELECT x FROM s WHERE x = 3)` deleted nothing). The
  probes now evaluate on a NULL row of the table's width.
- **`NOT BETWEEN` / `NOT IN (SELECT ...)` implied a partial index's
  `IS NOT NULL`** (low). The negation flag now acts as the pin's
  `seenNot` in `exprImpliesNotNull`, so the pin's table scan and its
  row order are kept.
- **A commuted comparison did not imply a partial WHERE** (low).
  `0 < b` now implies `b > 0`, as the pin's `exprAnalyze` commutes a
  comparison whose right operand is a column (`xplan::implies1`).
- **`ORDER BY x NULLS LAST` / `x DESC NULLS FIRST` over an index that
  puts the NULLs at the other end** (low). The first ORDER BY column
  after the equality columns is now delivered, as the pin's
  `WHERE_BIGNULL_SORT` two-pass scan does. In each probe, the rows
  whose key is NULL move to the requested end in scan order
  (`IdxAccess::bignull`). Ties come out as on the pin.

**Probing notes:** the pin probes `IN (SELECT ...)` once per sorted,
deduplicated value (`SEARCH t USING COVERING INDEX i (a=?)`), for a DESC
key column largest first. With `NULLS LAST` over `(a DESC, b)` it scans
the index backwards and reads the NULL keys in a second pass. With
`DESC NULLS FIRST` it reads them first. In both cases the scan
direction within the NULLs holds. An equality prefix (`x = 1 ORDER BY a
NULLS LAST` over `(x, a DESC)`) keeps the two-pass scan; on an IN prefix
it runs per value.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run end
with "==> all gates passed". The generated corpora agreed at
`HIPP_EXPRIDX_CORPUS=1500`, except one `generated_queries` case
(`genq1497`) that also differs on the pre-fix build. It is the known
gap below.

**Size:** tiny **76,924 B** (Δ **0**; the fast seek is gated with the
leaf, see above). Full **489,268 B** (Δ **0**; `query::in_sub_coll` is
leaf-only, so the IN-subquery collation helper `full` uses is
unchanged). Max **704,360 B** (Δ **+4,884 B** vs 699,476, ratchet
raised): the binary-search seek, row streaming and covering rows, the
IN-subquery term and probe, the BIGNULL scan and the implication
fixes.

**Honest gaps (updated):** the covering-scan row lookup gap is closed
for rowid tables (WITHOUT ROWID tables still read the row through
their key). `IN (SELECT ...)` on a row value (`(a, b) IN (SELECT ...)`)
and a correlated IN subquery in an UPDATE / DELETE WHERE are not
probed; they are filtered over the scan, which gives the same rows in
rowid order, as before. The partial-`x IS NOT NULL`-versus-index-on-`x`
tie (about 1 in 1,000 generated cases, row order only) remains.

### WITHOUT ROWID tables (2026-11-04)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated behind the new leaf feature `without_rowid`
(`without_rowid = ["v16", "indexes"]`, in `max`, not `full`;
`v16,without_rowid` in `ci/feature-sets.txt`). It measured +22.2 KB
of `max`; `full` has ~35 KB of headroom, but ADR 0002 sends a
storage subsystem of that weight to `max`, so it is a leaf. Probe
lists (~140 statements, `census/` style, one-shot vs the pin) all
match or fail on both sides.

**Design.** A WITHOUT ROWID table is the pin's: an index B-tree
(the *clustered* tree) whose records hold the PRIMARY KEY fields
first, then every other stored column in table order. The rest of
hipp is rowid-shaped, so `src/wr.rs` translates at the B-tree
boundary: the pager carries the WITHOUT ROWID layouts (rebuilt
with the catalog) and a per-statement table of *handles* — `i64`s
standing in for rowids, each bound to a row's canonical key record
(an open-addressing hash makes the same key always the same
handle). `ScanCursor`, `seek_rowid`, `btree_insert` (= "the
handle's row becomes this record", deleting its old entry and any
entry at the new key), `btree_delete`, `new_rowid` (a fresh
handle), `collect_table_pages` and the index entry points
(`index_insert` / `index_delete` / `index_prefix_rowids` /
`IndexCursor::seek` / `next_entry` / `index_check_entry`) dispatch
on the root. The table's PRIMARY KEY is listed as an automatic
index whose root is the table's (`IndexDef::clustered`), so UNIQUE
checks, REPLACE victims, upsert targets and foreign-key parent
lookups find rows in the clustered tree, while its "maintenance" is
the row write itself; a secondary index's `key, rowid` entries
become `key, key columns` on the way in and back on the way out.
Without the feature the dispatch compiles out (`index_insert` /
`index_delete` are the untranslated functions re-exported), which
is why `full` shrank.

**Shipped — leaf feature `without_rowid`:**

- **Definitions** as the pin: `WITHOUT ROWID` in any case, with or
  around STRICT; `WITHOUT <other>` is `ParseKind::UnknownTableOption`,
  no PRIMARY KEY `ParseKind::PrimaryKeyMissing`, AUTOINCREMENT
  `SchemaKind::AutoincrementWithoutRowid`; key columns NOT NULL (the
  NOT NULL error, ignoring a PRIMARY KEY's `ON CONFLICT`); INTEGER
  PRIMARY KEY is no alias (`'abc'` is stored); a key column repeated
  with the same collation is dropped (`PRIMARY KEY(a, a)`, kept under
  another: `(a, a COLLATE NOCASE)`); `UNIQUE(a)` repeating the key
  merges into it; the key takes its autoindex number among the
  UNIQUE sets but has no schema row and no tree of its own.
- **`rowid` / `oid` / `_rowid_`** are unknown names in SELECT,
  WHERE, RETURNING, UPDATE SET, INSERT column lists, CREATE INDEX
  (keys and partial WHERE), CHECK, generated columns and trigger
  NEW / OLD; a column so named reads as itself.
  `last_insert_rowid()` ignores these inserts; `changes()` counts
  them.
- **Layout, byte for byte** (records and cells; page allocation
  differs, see the review fixes below): clustered records as above (VIRTUAL
  columns not stored, rows short of later ADD COLUMNs read their
  defaults); secondary indexes append the key columns they do not
  hold with the same collation, with the key's collation and
  direction — ascending for a UNIQUE constraint's own index (the
  pin's `bAscKeyBug`, shown by index_xinfo); cells of at least 4
  bytes (the pin pads a 3-byte cell — a one-column key of a small
  integer); overflow by the index-cell local-payload rules.
- **Writes:** INSERT, OR IGNORE / REPLACE / FAIL / ABORT, upsert
  (DO NOTHING, DO UPDATE also of the key), RETURNING, UPDATE moving
  a row whose key changes, DELETE, the REPLACE victims, triggers,
  foreign keys (WITHOUT ROWID parents found through their key,
  children scanned, CASCADE / SET NULL), savepoints and crash
  recovery through the existing journal. A handle names its key, as
  the pin's two-pass writes name a row by its key: after
  `UPDATE OR REPLACE m SET a = a + 1` moves row 1 onto key 2 (its
  victim deleted), the loop's next key finds the moved row and moves
  it again — `2|1, 3|1, 4|1` as on the pin.
- **Order:** scans in key order (DESC / collated keys); the planner
  (`xplan`, with `expr_index`) models these tables the pin's way —
  no rowid loops, the clustered key an index (covering, its width all
  stored columns), every index's full scan a candidate, a secondary
  index covering its key and appended columns (a UNIQUE constraint's
  own index only its key, the pin's `colNotIdxed` being computed
  before the append), the pin's `whereLoopInsert` list (loops
  compete per `iSortIdx` class; a later equal loop does not displace
  an earlier one, a better one takes its place), GROUP BY delivered
  by the key order; UPDATE / DELETE: the WHERE's columns only, an
  index lookup costed as not covering, a key change two-pass, the
  two-pass visit in key order.
- **Schema statements:** PRAGMA table_info (key columns `notnull`
  1), table_xinfo, table_list (`wr` 1), index_list (the key as
  `sqlite_autoindex_t_N`, origin `pk`), index_info / index_xinfo
  (the clustered key with the other stored columns BINARY, a
  secondary index with its appended key columns, never cid -1),
  integrity_check (hipp's own, and the pin's on hipp's files);
  ALTER TABLE RENAME / RENAME COLUMN (texts), ADD COLUMN, DROP
  COLUMN (records rewritten in place as the pin rewrites them,
  `encode_record_plain`; key columns refused), DROP TABLE (the tree
  freed once), VACUUM (trees copied raw), REINDEX (the key skipped),
  ANALYZE (the key's row named by the table, as on the pin), CREATE
  INDEX on a populated table, TEMP tables.

**Oracle-probing notes** (all on the live pin, before and during the
work): the record layouts above, dumped from pin-written pages
(`CREATE TABLE t(a INT, b TEXT, c, PRIMARY KEY(c, a))` stores
`[c, a, b]`, an index on `b` stores `[b, c, a]`); no schema row for
the key, `sqlite_autoindex_t_2` for `a PRIMARY KEY, b UNIQUE`, `_1`
for `a UNIQUE, b PRIMARY KEY`; index_list newest first with the key
in its constraint's place; the error texts named above; `INSERT ...
VALUES('abc')` into an INTEGER key; `UPDATE t SET a = a + 1` over
keys 1, 2, 3 is a UNIQUE error, `a = a + 10 RETURNING *` gives 11,
12, 13; two-pass writes (RETURNING, triggers, key changes) visit key
order; one-pass UPDATE / DELETE walk the chosen index; the planner
cases above (`SELECT *` through the newest equally wide covering
index; `x UNIQUE` declared before a composite key never chosen to
cover the key columns; `UPDATE t SET d = 1 WHERE a > 5` takes the
PRIMARY KEY over an index on `a` alone, `UPDATE t SET d = 1` the
narrowest index); DROP COLUMN rewrites the clustered records with
serial type 1 for 0 / 1 and leaves unaffected indexes alone; 3-byte
cells padded to 4 (the pin's "Multiple uses for byte" otherwise).

**Bugs found during the unit** (fixed; regression lines in
`tests/without_rowid_v16.rs` `regressions`): REINDEX rebuilt the
clustered key as an index (emptying the table); DROP TABLE freed the
tree twice; CREATE INDEX filled an index with rowid entries before
the catalog knew it; DROP COLUMN rewrote by the old layout; ANALYZE
named the key's statistics by its index; 3-byte cells (above);
UPDATE OR REPLACE onto a victim's key processed rows by identity,
not by key. **Ungated, on a claimed path** (`expr_index` planner,
also rowid tables): an `IS` / `IS NULL` lookup through a UNIQUE index
counted as a one-row loop, so its GROUP BY groups came out unsorted
(`SELECT b, count(*) FROM t WHERE a IS NULL GROUP BY b`; the pin's
WHERE_ONEROW needs `=`).

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run end
with "==> all gates passed". New suites: `tests/without_rowid_v16.rs`
(15 tests: definitions and errors, rowid names, last_insert_rowid,
writes and conflicts, key order and lookups, the pin's scan order,
secondary indexes and pragmas, ALTER / DROP / VACUUM / REINDEX /
ANALYZE, triggers / foreign keys / transactions, overflowing rows and
keys, pin-made files written by hipp, hipp-made files read and
VACUUMed by the pin, regressions, and two generated corpora — random
schemas with composite / collated / DESC / repeated keys, UNIQUE
constraints and indexes, random writes (OR IGNORE / REPLACE / FAIL,
upsert, RETURNING, key updates, deletes) and queries compared in
order with the pin's integrity_check on hipp's file, 120 cases by
default; and files crossing between the engines mid-sequence both
ways, 30 by default — `HIPP_WR_CORPUS` scales them: 1,450 / 1,000
agreed before the last fix, 1,200 / 300 after it),
`tests/crash.rs` `v16_crash_without_rowid` (a crash at every write
boundary of a splitting / key-moving / deleting transaction recovers
to the pre- or post-image), `hipp-cli/tests/cli_v16_without_rowid.rs`
(3 tests, byte-identical one-shot and over stdin: schema texts and
pragmas, writes and order, a generated corpus); `tests/table_defs_v16.rs`
now expects `WITHOUT ROWID, STRICT` to work as on the pin.

**Size:** tiny **76,924 B** (Δ **0**). Full **489,268 B** (Δ **−128 B**,
ratchet lowered: the index entry points restructured so the
untranslated functions are the public ones there). Max **699,476 B**
(Δ **+22,224 B** vs 677,252, raised; under the 32 KiB threshold): the
translation layer and handles (~12 KB), catalog / pragma / ALTER /
VACUUM support (~5 KB) and the planner model (~5 KB).

**Honest gaps:**

- Without `without_rowid` (the tiny and `full` profiles) a file
  holding a WITHOUT ROWID table still fails to open (its schema does
  not parse there), as before this unit.
- Handles cost memory per statement for every row a statement
  touches (a scan of N rows binds N keys); a point lookup by key
  goes index seek → handle → seek again.
- The planner model has the expression-index unit's limits, plus:
  equality on a secondary index's appended key columns is not a
  usable term (the order matches, the cost estimate may not), and
  the UPDATE / DELETE costing rule ("lookups not covering, full
  scans covering the WHERE's columns") is fitted from probes, not
  derived.
- `PRAGMA table_list`'s row order (the pin's hash order) differs
  from the pin's once there are more than ~8 tables — pre-existing,
  for rowid tables too; the suites list tables by name.
- ~~Constraint-level `ON CONFLICT` clauses are still ignored for every
  table (pre-existing).~~ (closed by the honest-gap sweep, below)

### WITHOUT ROWID tables — review fixes (2026-11-04)

Seven review findings, each reproduced first on the pre-fix build and
compared with the live 3.54.1 pin.

**Fixed (regression tests in `tests/without_rowid_v16.rs`:
`review_regressions` (with a generated part: random large keys moved
and deleted at page sizes 1024 and 4096), `schema_text_ends_as_the_pins`;
the generated corpora now also draw texts holding NUL; CLI
`cli_v16_without_rowid_review_byte_identical`):**

- **Panic / CorruptRecord in the index delete rebalance** (high;
  ungated, `indexes`: rowid-table indexes too). A delete's window
  balance pulls the parent's dividers down and lifts new ones, which
  can be larger. The parent then no longer fit (the page builder's
  `content -= len` underflowed: a panic at the default page size on a
  WITHOUT ROWID UPDATE that grows keys to ~1 KB), or the window
  needed more pages than it had (a page written with cells over its
  pointer array, read back as `CorruptRecord` by the next DELETE at
  page size 1024). The delete balance now checks both before writing
  any page and, when either fails, hands the window to the insert-side
  balance (`IndexBalance::Grow`), which allocates pages and cascades a
  parent overflow upward (`cascade_index_up`), as the pin's
  `balance_nonroot` grows on delete. `build_index_image` refuses a
  cell list that does not fit its page (`CorruptPage`) instead of
  writing it. Same pattern on the table side, found by the new
  generated test: an insert balance that grows its window by two pages
  indexed past the allocated pages (a panic), and a root leaf whose
  cells need three pages (a near-page row inserted between two halves
  of a full leaf) was refused `Unsupported`. Both are fixed (allocate
  as many pages as the partition needs; a root leaf splits into any
  number of children). These are ungated INSERT fixes on the tiny
  profile's path.
- **NOCASE over text holding NUL** (high; ungated, every profile with
  collations). The pin's `sqlite3StrNICmp` stops at the first NUL of
  the left string; only the lengths decide after it. hipp compared
  every byte, so a NOCASE key tree was out of the pin's order (its
  integrity_check: "row not in PRIMARY KEY order") and UNIQUE let
  through keys the pin calls equal. `record::compare_text` now stops
  there too, which also fixes NOCASE ORDER BY / GROUP BY / DISTINCT /
  comparisons and rowid-table autoindexes.
- **PRAGMA index_info / index_xinfo of a WITHOUT ROWID table's name**
  (high). They describe the table's PRIMARY KEY index, as the pin's
  `sqlite3PrimaryKeyIndex` fallback does (also `pragma_index_info('t')`);
  a rowid table's name still gives no rows.
- **ALTER TABLE DROP COLUMN of the last column, or of a column before
  a key column** (medium). `wr::drop_field` read the dropped field's
  place from the layout rebuilt after the schema change: past the end,
  or a key field, so the statement failed with `CorruptRecord`. It now
  counts the place from the key width and the stored non-key fields
  before the column, which is the same in both layouts. (The earlier
  section's "DROP COLUMN rewrote by the old layout" fix covered only
  the case where the next column was a non-key one.)
- **Stored schema text after table options** (medium). The pin's
  `sqlite3EndTable` keeps a table with options (STRICT, WITHOUT ROWID)
  up to the statement's `;`, whitespace and comments included (to the
  end of the text when there is no `;`), and a plain table up to its
  closing `)` (a comment after it is dropped); `sqlite3CreateIndex`
  keeps an index up to its `;` as well. hipp trimmed the whitespace,
  and kept a comment after a plain table's `)`. `parser::schema_text_end`
  now gives the pin's end. The CLI no longer trims lines' trailing
  spaces or a statement's trailing whitespace, so `.schema` and
  `sqlite_schema.sql` match over stdin and one-shot. Gated on `v16` or
  `indexes`: in the tiny profile the comment after a plain table's `)`
  is still kept (the ungated form measured +232 B tiny; the pin reads
  the text either way).

**Not fixed:**

- **index_xinfo collation spelling** (low, predates the unit: rowid
  tables too). The pin prints the collation name as written (`nocase`,
  `NoCase`, `BiNaRy`; `BINARY` when none is written); hipp prints
  `BINARY` / `NOCASE` / `RTRIM`. Keeping the spelling means carrying it
  through the parser's `Expr::Collate`, the catalog's column and index
  definitions and the autoindex sets: that belongs to the collations
  subsystem, not this unit.
- **page_count of hipp-written WITHOUT ROWID files** (low, docs). hipp's
  index B-tree split policy (shared with ordinary indexes) can use a few
  more pages than the pin (31 vs 29 for 281 medium rows). Records and
  cells are byte-compatible, integrity checks pass both ways, and the
  contents and order match. Page allocation is not the pin's, and the
  earlier section's "byte for byte" refers to the record and cell
  layout only.

**Probing notes:** `sqlite3StrNICmp(a, b, min(len))` then the length
difference: `x'610078' = x'610079' COLLATE NOCASE` is 1 on the pin and
`x'0061'` equals `x'0062'`. Stored texts (one-shot and stdin):
`... WITHOUT ROWID ;` keeps the space, `... STRICT /* x */  ` with no `;`
keeps the comment and both spaces, `CREATE TABLE a1(a) /* x */ ;` stores
`CREATE TABLE a1(a)`, `CREATE INDEX i ON t(a)   ` keeps the spaces, a
view still trims trailing whitespace but keeps the comment.
`index_xinfo` on the pin: `PRIMARY KEY(a, b COLLATE "Binary")` shows
`Binary`; `c collate 'NOCASE'` shows `NOCASE`.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` run end
with "==> all gates passed". The generated corpora agreed at
`HIPP_WR_CORPUS=1100` (before the NUL values were added) and 600 (after).

**Size:** tiny **77,004 B** (Δ **+80 B**, ratchet raised; justified:
INSERT fixes on the tiny profile's path, a panic when an insert
balance grows by two pages and an `Unsupported` refusal of a three-way
root leaf split). Full **490,628 B** (Δ **+1,360 B**, raised; ungated
fixes on claimed paths: the growing index delete balance, the page
builder's fit check, NOCASE, the schema-text end). Max **705,768 B**
(Δ **+1,408 B** vs 704,360, raised): the same plus the index_info
fallback and the DROP COLUMN position.

**Honest gaps (updated):** besides the two above, PRAGMA table-valued
functions (`pragma_index_info('x')` and the others) do not see TEMP
objects (they read only `main`); this predates the unit and affects
rowid tables too. The unit's earlier gaps stand.

### UTF-16 databases (2026-11-04)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated behind the new leaf feature `utf16` (`utf16 = ["v16"]`,
in `max`, not `full`; `v16,utf16` in `ci/feature-sets.txt`). Without
it a file whose header text encoding is 2 or 3 is refused at open
with `Error::UnsupportedEncoding` exactly as before (unit test
`pager::tests::utf16_header` runs in the `full` lib tests; the
`differential.rs` a7 check is gated), and `PRAGMA encoding =
'UTF-16...'` on an empty database is still that error.

**Design.** Text stays UTF-8 inside the engine. A UTF-16 file's
records are translated at the B-tree boundary
(`utf16::transcode_record`): `btree::assemble_payload_into` — the
one path every table row and index key is read through — turns its
TEXT values into a lossless UTF-8 form (an unpaired surrogate kept
as its three-byte form; review fix below), and `btree_insert` /
`index_insert_core` / the index-delete predecessor cell turn them
back into the file's UTF-16 (serial types counting UTF-16 bytes), so
everything above (catalog, ALTER, VACUUM, WITHOUT ROWID's `wr`
layer, triggers, foreign keys) works unchanged. What the pin exposes
of the UTF-16 bytes is rebuilt from the UTF-8 form by
`utf16::Units` (its `READ_UTF8` / `WRITE_UTF16` arithmetic): the
connection's encoding (`record::TextEnc`, a zero-sized type without
the leaf) reaches every BINARY comparison as a parameter —
`record::compare_coll_in`, `query::order_cmp`, `btree::compare_entries`
and the probe / integrity comparisons, the compound merge, GROUP BY
sorts, aggregates, the window engine, xindex probe sorting and range
bounds, WITHOUT ROWID handle sorting — and the evaluator reads it
from `query::Env::enc` (`Ctx::enc`).

**Shipped — leaf feature `utf16`:**

- Files with header encoding 2 / 3 open, read and write; the
  header field is written (with the first schema change) as the pin
  writes it; VACUUM / VACUUM INTO keep it; a TEMP database stores
  text as main does.
- `PRAGMA encoding = 'UTF-8' | 'UTF8' | 'UTF-16' | 'UTF16' |
  'UTF-16le' | 'UTF16le' | 'UTF-16be' | 'UTF16be'` (any case;
  `UTF-16` alone is little-endian, the pin's native order on its
  x86-64 build; `PRAGMA temp.encoding` is the same setting) while
  the encoding is open — the database has no schema, its encoding
  field is still 0 (a new file, or one that only got header writes)
  and neither main nor TEMP has had a schema change since the open
  (`Pager::enc_open`; review fix below); the last choice wins; once fixed any value,
  even an unknown one, is ignored (an unknown name while open is
  `SchemaKind::UnknownEncoding`, as before). `PRAGMA encoding` and
  `pragma_encoding` report `UTF-8` / `UTF-16le` / `UTF-16be`.
- BINARY over the UTF-16 bytes (memcmp, then length) everywhere text
  is ordered: ORDER BY, index B-trees (insert, seek, range scans,
  UNIQUE, integrity_check), min / max (aggregate, scalar, the
  bare-column rule), GROUP BY output, UNION / INTERSECT / EXCEPT,
  windows (partitions, ORDER BY, RANGE frames, min / max frames),
  aggregate ORDER BY, IN-list probe order, comparisons. NOCASE and
  RTRIM compare the UTF-8 form.
- Functions: `hex(text)` and `CAST(text AS BLOB)` give the UTF-16
  bytes (`CAST(12 AS BLOB)` too; `hex(12)` stays `3132`, as on the
  pin); `octet_length` counts UTF-16 bytes (twice the rendering for
  numbers); a BLOB read as text or as a number through text decodes
  from UTF-16 — `CAST(blob AS TEXT / INTEGER / REAL / NUMERIC)`,
  `||`, arithmetic, `WHERE blob`, LIKE / GLOB (operator and
  function), upper / lower / trim / replace / instr (unless both are
  blobs) / unicode / printf / concat / unhex / char / round / abs /
  zeroblob / randomblob arguments, group_concat / string_agg; length,
  hex, quote, substr's first argument, typeof keep the bytes; a
  function's text result goes through the pin's UTF-8 → UTF-16
  conversion (`utf16::normalize`: a lone surrogate becomes U+FFFD),
  except the ones that return an argument (coalesce, nullif, iif,
  min / max, likely); the PRAGMA table-valued functions' internal
  literals are UTF-16-aware.
- Invalid stored UTF-16 is held losslessly and shown as the pin
  shows it: an odd trailing byte dropped (also by `CAST(blob AS
  TEXT)`); an unpaired surrogate kept as its 3-byte form, so copies,
  index builds, `||`, CAST, equality and BINARY order see the stored
  units; where the pin reads text through its UTF-16 → UTF-8
  conversion (result columns, function arguments, LIKE, NOCASE /
  RTRIM, JSON) hipp reads `utf16::view`, which combines a surrogate
  with whatever unit follows it.

**Fixed on the way (ungated, the v16 function library):**
`unicode()` replaced U+FFFE / U+FFFF with 65533; the pin returns
65534 / 65535 (`unicode(char(65534))`).

**Oracle-probing notes** (live pin, before implementing): the
UTF-16le order is the low byte first (`'' < U+0100 < U+1F600 < 'a'`),
UTF-16be puts astral characters below U+E000; NOCASE and RTRIM order
like UTF-8 in both (`'a' < U+0100`); an index scan and `x > 'a'`
follow the BINARY bytes; `hex(char(55296))` is `FDFF` (FFFD) while
`hex(CAST(x'00D8' AS TEXT))` is `00D8`, `x || ''` and
`coalesce(x, 1)` keep it, `upper` / `quote` / `trim` / `substr` /
`group_concat` give FFFD; `char(65534)` stores FFFE; `CAST(x'410042'
AS TEXT)` is `A` (`4100`); `x'3100' + 1` is 2 (UTF-16le) and
`x'0031' = 1` is 0; `CASE WHEN x'0031'` is true in UTF-16be;
`sum(x'3100')` is 1.0 (a blob stays non-integer); `upper(x'41')` is
empty; `PRAGMA encoding` after `SELECT 1`, `BEGIN`, a schema read or
`PRAGMA page_size` still changes it, after `CREATE TEMP TABLE` it
does not, after CREATE + DROP TABLE it does not; a file whose field
is 0 (written by `PRAGMA user_version` alone) chooses on reopen;
`PRAGMA encoding = 'bogus'` is "unsupported encoding" only while
open. One early probe showed `unicode()` of a stored lone surrogate
as 55296; repeated probes give 65533 everywhere (that value is not
tested). `concat()` of a blob reads past the converted text on the
pin (`concat('<', x'61006200', '>')` is `<ab`, garbage after the
NUL): not modelled, not tested.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` end
with "==> all gates passed". New suites: `tests/utf16_v16.rs` (10
tests: PRAGMA names and reporting, when the encoding is fixed, a
reopened field-0 file, the BINARY order of every query shape in both
byte orders with and without indexes, text functions, blobs read as
text, invalid UTF-16, VACUUM / ALTER / TEMP / WITHOUT ROWID with
overflow pages, files written by each engine read, extended and
integrity-checked by the other, and a generated corpus of random
Unicode text — BMP, astral, combining, NUL-containing, random
`char(N)` including surrogates — with random writes and 15 query
shapes in alternating byte orders, 24 cases by default,
`HIPP_UTF16_CORPUS` scales it: 300 / 300 agreed); every statement is
compared with the live pin and the pin's integrity_check runs on
hipp's file; unit tests in `src/utf16.rs` (translations, BINARY
order, records) and `src/pager.rs`; `hipp-cli/tests/cli_v16_utf16.rs`
(byte-identical one-shot and over stdin in UTF-16le / be / native,
`.schema` / `.tables` of non-ASCII names, each CLI reading the other's
file, a generated corpus). `tests/differential.rs` a7 and
`tests/pragma_v16.rs` now expect UTF-16 to work when the leaf is
built.

**Size:** tiny **77,004 B** (Δ **0**; the header check keeps its old
code without the leaf). Full **490,692 B** (Δ **+64 B**, ratchet
raised: the ungated `unicode()` fix and codegen shifts from the
zero-sized encoding parameters — no UTF-16 code is in `full`). Max
**713,504 B** (Δ **+7,736 B** vs 705,768, raised; under the 32 KiB
threshold): the conversions and record translation (~3 KB), the
evaluator's UTF-16 paths (~2.5 KB), the encoding-aware comparisons
and PRAGMA / pager / VACUUM support.

**Review fixes (2026-11-04).** Seven findings reproduced on the
live pin, all fixed with regressions in `tests/utf16_v16.rs`
(`regression_*`, plus surrogate-heavy raw units in the generated
corpus: 300 / 300 agreed) and `hipp-cli/tests/cli_v16_utf16.rs`
(`unpaired_surrogates_render_as_the_pin`):

- **Unpaired surrogates corrupted indexes and lookups** (high). The
  boundary decode combined any surrogate with the next unit (the
  pin's display conversion), so `00D8 6100` was held as U+10061 and
  re-encoded `00D8 61DC`: hipp's CREATE INDEX / REINDEX wrote keys
  that did not match the pin's rows (the pin's integrity_check
  failed), seeks on a valid pin file missed (`a = char(0x10000)`
  found the wrong row, hipp's integrity_check reported a missing
  entry), and within one statement `CAST(x'DC00D800' AS TEXT)`
  equalled `char(0x10000)` (false UNIQUE violations, reordered
  `hex()`). Root cause fixed: `utf16::decode_into` is now lossless
  (only a high + low pair combines), the pin's lossy conversion is
  `decode_pin_into` / `view`, applied where the pin reads
  `sqlite3_value_text`: `Statement::step`'s result columns, scalar
  function arguments (`eval_scalar16`), aggregate and window
  group_concat arguments (`agg::arg16`), the LIKE operator, NOCASE /
  RTRIM (`record::compare_coll_in`) and JSON arguments; `||` joins a
  split pair (`utf16::canon`). `x || 'a'` across a trailing
  surrogate now matches too (an earlier gap).
- **Math functions and sign() on a BLOB** (high). The math functions
  and `sign()` keep a BLOB argument (NULL, as in UTF-8); `log(B, X)`
  reads X through its UTF-16 text, as the pin's
  `sqlite3_value_double`. Same pattern fixed in unary minus,
  `randomblob(N)`, `sum` / `avg` / `total` (aggregate and window) and
  window group_concat / string_agg (an earlier gap). Ungated fix
  found on the way: `sign()` of a BLOB or of a text that is not
  wholly a number (`sign('2x')`) was 1 / 0 in every encoding; the
  pin's is NULL.
- **JSON on a non-JSONB BLOB** (high). A BLOB in a document or path
  position (`json::Op::reads_text`; `json_each` / `json_tree` too)
  decodes from UTF-16 when it is not JSONB; value positions still
  reject it; text arguments read as `view`; JSON text results (and
  the JSON aggregates') go through `utf16::normalize`, so a lone
  surrogate becomes U+FFFD (an earlier gap); `CAST(blob AS TEXT)`
  inside a JSON argument decodes too (it read raw bytes).
- **Header-only writes fixed the encoding** (high). With `utf16` a
  new file's schema-format and encoding fields stay 0 until the
  first schema change, as in the pin's file (`pager::initial_page1`;
  the header bytes now match after `PRAGMA user_version`), and the
  encoding stays open across header writes in the same connection
  and on reopen in either engine (the earlier reopen gap). A schema
  change, even rolled back or inside a released savepoint, fixes it
  (`Pager::schema_changed`, set by `bump_schema_cookie`).
- **`PRAGMA temp.encoding = ...` ignored** (medium). The encoding is
  a connection-wide pragma (`Prag::connection_wide`); the TEMP
  database follows main's (`sync_temp`). Only a TEMP schema change
  fixes the encoding — reading `temp.sqlite_master`, `PRAGMA
  temp.user_version` or `temp.page_size` do not, as on the pin
  (hipp used to fix it as soon as the TEMP database existed).
- **Docs** (low): the interop and gap claims above and in README
  are corrected.

Probed on the pin for these fixes: the `view` cases above (`length`,
`unicode`, `quote`, `substr`, `instr`, LIKE / GLOB, `upper`,
`printf`, `group_concat`, NOCASE all read the combined character;
`hex`, `CAST AS BLOB`, `octet_length`, `coalesce`, `min`, `||` keep
the bytes); every math function, `log(2, blob)`, unary minus,
`randomblob`, sum / avg / total and their window forms on BLOBs in
all three encodings; 26 JSON calls with BLOB arguments in all three
encodings (error-vs-error agreement for value positions); 18 PRAGMA
encoding scripts (header writes, rolled-back CREATE, savepoints,
BEGIN IMMEDIATE, TEMP reads / writes / DDL).

**Size after the review fixes:** tiny **76,964 B** (Δ **0**). Full
**490,996 B** (Δ **+80 B**, ratchet raised: the ungated `sign()`
fix). Max **737,376 B** (Δ **+5,296 B** vs 732,080, raised): the
lossless decode / `view` / `canon`, the argument and result
translations in functions, aggregates, windows and JSON, and the
encoding-open tracking.

**Honest gaps:**

- `concat()` / `concat_ws()` with BLOB arguments: the pin reads past
  its converted text (`concat_ws(x'32002E003500', 'a', 'b')` is
  `a2.5` there); hipp gives the decoded text.
- A TEXT bound through `bind_value` that is not valid UTF-8 and
  holds a surrogate's three-byte form is stored as that surrogate
  (not probed: the pin's conversion of such bound text may differ).
- `UTF-16` alone always means little-endian (the pin's on x86-64; a
  big-endian pin build would choose big-endian).
- A hipp-created file gets its page 1 written at open (the pin's
  stays empty until the first write), and its change counter after a
  header-only write differs from the pin's by one (pre-existing, every
  encoding). `.dump` differs from the pin's for every encoding
  (pre-existing).
- ~~`INDEXED BY` is not parsed (pre-existing; the regression tests use
  the planner's own index choice).~~ (closed by the honest-gap sweep, below)

### WAL journal mode (2026-11-04)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated behind the new leaf feature `wal` (`wal = ["v16"]`, in
`max`, not `full`; `v16,wal` in `ci/feature-sets.txt`). Without it a
file whose header says WAL is refused at open with
`Error::WalUnsupported`, as before (`pager::tests::wal_header` and
`wal_database_refused_at_open` run in the `full` lib tests;
`tests/differential.rs` a7 is gated).

**Design.** The storage trait gains two sidecar kinds
(`Sidecar::Wal`, `Sidecar::Shm`, gated) through the same
`Storage::sidecar` call, so the `-wal` is reachable in no_std on any
caller storage (`MemStorage` keeps one buffer per kind; `FileStorage`
names them `<db>-wal` / `<db>-shm`), plus `Storage::locked_elsewhere`
(since the review fixes `claim_wal` / `release_wal`; default `false`; `FileStorage` on Linux reads `/proc/locks` for a
POSIX lock another process holds on the database or its `-shm` — a
live pin connection). `src/wal.rs` is the log: the pin's format
(header, salts, cumulative checksums in either byte order, commit
frames), recovery as the pin's (`Wal::recover`), the heap wal-index
(page → newest frame, sorted `AVec`), appends that overwrite a page
the transaction already logged and reseal the checksums before the
commit frame (the pin's `iReCksum`), restarts and
`journal_size_limit` truncation. The pager reads pages through the
log, spills dirty pages into it as uncommitted frames, commits by
appending the resident dirty pages in page order with the commit
frame last (two new resumable stages, `WalFrames` / `WalSync`),
rolls back by forgetting the transaction's frames, checkpoints
(`Pager::checkpoint`), switches modes (`enter_wal` / `leave_wal`:
file-format bytes through a rollback-journal transaction, in the
pin's order) and retires the log at close (`close_wal`). The
database file changes only in a checkpoint. Savepoints keep their
page images in the journal file's savepoint area as in rollback mode
(from a zero first sector, deleted when the transaction ends).
`Connection::close` (ungated; a rollback without the leaf) is new;
hipp-cli closes at exit and on `.open`, as the pin's shell.

**Shipped — leaf feature `wal`:**

- Reading: a header with bytes 18 / 19 = 2, or a non-empty `-wal`
  beside any header (the pin reads it then too), opens in WAL mode
  with the committed frames replayed — page 1's header and the page
  count from the last commit frame; a log with a bad magic, version,
  page size or header checksum is empty; frames stop at the first
  bad salt / checksum / page 0; frames after the last commit are
  ignored. A log beside an empty database file is deleted, as on the
  pin. Big-endian logs read and extend in their order.
- `PRAGMA journal_mode = WAL | DELETE` both ways, reporting the mode
  in force; the pin's prefix match of the name; no change (silently)
  once the open transaction has written, "cannot change into / out
  of wal mode from within a transaction" (`SchemaKind::
  WalModeInTransaction`) in one that has not; the TEMP database
  stays out of WAL mode.
- Writing: the pin's frame layout and order, page 1 only when the
  transaction changed it (the change counter moves only then, as
  there), LE checksums, random salts for a new log; a restart after
  a full checkpoint (sequence + 1, first salt + 1).
- `PRAGMA wal_checkpoint [(PASSIVE | FULL | RESTART | TRUNCATE |
  NOOP)]` (others PASSIVE): `0|log|checkpointed`, `0|-1|-1` outside
  WAL mode; refused inside a transaction that read or wrote
  (`SchemaKind::CheckpointLocked`, either mode); the pin's first
  checkpoint after entering WAL mode opens its log with a read.
  `PRAGMA wal_autocheckpoint` (connection-wide; ≤ 0 → 0), the
  automatic PASSIVE checkpoint after a commit at that many frames;
  `PRAGMA journal_size_limit` (and `pragma_journal_size_limit`);
  `PRAGMA synchronous` at every level (OFF: no syncs; NORMAL: the
  log synced only by checkpoints; FULL / EXTRA: at every commit).
- Close: checkpoint, `-wal` and a stale `-shm` deleted, the file
  still marked WAL (the pin's persistent WAL mode).
- VACUUM in WAL mode writes the image as one transaction of frames
  and keeps the page size; VACUUM INTO writes a rollback-mode file
  with the pending page size, as on the pin.
- A database a live SQLite connection holds is refused at open (and
  at the switch to WAL) with `Error::DatabaseBusy`.

**Fixed on the way (ungated, the `v16` PRAGMA code):** `PRAGMA
journal_mode = w` (any prefix of a mode name) was a silent query —
now the pin's mode (refused without the leaf for WAL);
`PRAGMA synchronous = ...` inside a transaction is the pin's prepare
error (`SchemaKind::SafetyLevelInTransaction`).

**Oracle-probing notes** (live pin, before implementing): the
switch writes bytes 18 / 19 = 2 in a rollback transaction (change
counter + 1; on a new file page 1 is written with fields 44 / 56 still
0); the `-wal` appears at the first read or write after it, not at
the pragma; frames come page-sorted, page 1 only when changed; magic
`0x377f0682` on the x86-64 pin; a new log has sequence 0 and random
salts; after every frame is checkpointed the next write rewrites the
header (sequence + 1, salt 1 + 1, new salt 2) without truncating the
file, unless `journal_size_limit` trims it at the first commit
(to max(limit, frames' end)); TRUNCATE empties it and the next header
takes sequence + 1; `PRAGMA wal_checkpoint` is `0|-1|-1` in rollback
mode and "database table is locked" in a transaction that touched
the file (BEGIN alone is fine, `PRAGMA user_version` counts); NOOP
copies nothing; `wal_autocheckpoint = -3` / `'abc'` / `99999999999`
report 0, `2.7` 2; `journal_size_limit = -5` is -1, `'x'` 0;
`synchronous = 7` is 0, `4` reports 4, `bogus` / `-1` 1; inside a
transaction a safety-level change fails at prepare; the shell closes
(checkpoint, `-wal` and `-shm` removed) even after an error;
`locking_mode = EXCLUSIVE` leaves only the `-wal` on a kill; a
header saying rollback with a non-empty valid `-wal` is read through
it and the mode reported `wal`, a 0-byte one leaves rollback mode
alone; an empty database file deletes its `-wal`; VACUUM in WAL mode
ignores a pending page size, VACUUM INTO uses it and writes 1 / 1; a
live pin holds a POSIX read lock on its database's SHARED range and
on the `-shm` DMS byte (exclusive mode: a write lock on the
database), which `/proc/locks` shows.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` end
with "==> all gates passed". New suites: `tests/wal_v16.rs` (15
tests: PRAGMA values and switches, safety levels and limits,
checkpoints of every mode with `-wal` sizes compared between
statements, VACUUM / VACUUM INTO, the pin's killed logs read and
extended (stale `-shm`), 50 rewritten logs — both byte orders,
truncated at every frame, every salt and checksum broken, a bad
header, a torn last frame, trailing junk — read exactly as the pin
reads them, header / empty-log cases, hipp's logs parsed by an
independent parser and read, recovered, checkpointed and extended by
the pin, a live pin connection refused, the errors, savepoints, and
two generated corpora of WAL workloads — writes, savepoints,
statement failures, checkpoints, mode switches, VACUUM — on both
engines, outputs and files compared and each engine reading the
other's log, 30 + 15 by default, `HIPP_WAL_CORPUS` scales them: 400
agreed); `tests/crash.rs` gains four WAL crash matrices (every
write of frame appends with spills and in-place rewrites, savepoints
and a failing statement, every checkpoint mode, restarts,
`journal_size_limit`, the automatic checkpoint, mode switches both
ways, VACUUM, the checkpoint at close — each recovers the state
after the completed statements or after the one in flight, passes
hipp's and the pin's integrity checks and reads the same on the pin)
and the cross-tool case (a hipp process killed at every write of a
WAL workload and its close, recovered by the pin, read alike by
hipp; `crash_writer` modes `wal_setup` / `wal_txn`); unit tests in
`src/wal.rs` and `src/pager.rs`; `hipp-cli/tests/cli_v16_wal.rs`
(byte-identical PRAGMA rows one-shot and over stdin, the one-shot
error stop, the oracle's killed log read, `.open`, a generated
corpus; the files both CLIs leave compared). `tests/pragma_v16.rs`
and `tests/differential.rs` now expect WAL to work with the leaf.

**Size:** tiny **76,964 B** (Δ **−40 B**, ratchet lowered: the
`MemSidecar` writes lost two `expect`s). Full **490,916 B** (Δ
**+224 B**, ratchet raised: the ungated journal_mode prefix match and
safety-level refusal; no WAL code is in `full`). Max **732,080 B** (Δ
**+18,576 B** vs 713,504, raised; under the 32 KiB threshold): the log
format and recovery (`src/wal.rs`), the pager's WAL paths (reads,
spills, the commit stages, checkpoints, switches, close), the
PRAGMAs and the `Connection::close` wiring.

**Honest gaps:**

- Frame counts match the pin's for the probed workloads, but not in
  general: after deletes hipp's B-trees are not always laid out page
  for page as the pin's (pre-existing), so a transaction may log
  other pages; hipp's page cache (`Config::cache_pages`, 16 by
  default) spills into the log long before the pin's (2000 KiB); the
  pin's "don't write" of pages freed in a transaction is not modelled;
  a ROLLBACK TO may leave other pages dirty than on the pin. The
  generated corpora compare checkpoint rows by meaning (frames
  present, all checkpointed), the hand-written suites exactly.
  (An UPDATE that rewrote unchanged index entries and rows was a
  further, common cause: fixed in the review fixes below.)
- The pin creates the empty `-wal` at its first read in WAL mode;
  hipp at its first frame. The checkpoint's "locked" rule reads
  "a page was read since BEGIN / SAVEPOINT" (a schema re-read after
  another statement's schema change counts).
- Single process: a `-shm` held by a live connection is detected only
  on Linux (`/proc/locks`) and only at open and at the switch to WAL;
  hipp never writes a `-shm`, so a pin connection opened later
  beside hipp's is not coordinated with (never do that). `busy` is
  always 0. `PRAGMA locking_mode` stays refused. (A second hipp
  connection is refused since the review fixes below.)
- `journal_mode` PERSIST / TRUNCATE / MEMORY / OFF are still refused
  (from WAL mode too). (The CLI's `:memory:` database entered WAL
  mode: fixed below.)
- `PRAGMA encoding` after `journal_mode = WAL` on a new database is
  ignored by hipp (its page 1 already says UTF-8); the pin still
  takes it.
- (Without the leaf, a rollback-mode header with a non-empty `-wal`
  beside it was read from the database file. This gap's claim that
  only hand-placed files get there was wrong — the pin opens such a
  database in WAL mode and commits into the log; fixed below.)
- A log whose page size differs from the database's with committed
  frames is refused (`CorruptPage`). `synchronous = NORMAL` in
  rollback mode syncs as FULL; EXTRA does not sync the directory. A
  commit whose final sync fails is rolled back in memory after its
  commit frame's salts are overwritten, best effort.

### WAL journal mode — review fixes (2026-11-04)

Four review findings, each reproduced first on the pre-fix build and
compared with the live 3.54.1 pin.

**Fixed (regression tests: `tests/wal_v16.rs`
`a_second_hipp_connection_is_refused`, `memory_db_stays_memory`;
`src/pager.rs` `log_beside_rollback_header` (run by the tiny, `full`
and `max` lib tests); `hipp-cli/tests/cli_v16_wal.rs`
`dot_open_same_wal_db_then_killed`, `dot_open_failure_substitutes_memory`,
`memory_database_stays_memory`, `update_frames_match`):**

- **`.open` of the WAL database already open lost commits on a crash**
  (high). hipp-cli opened the new connection before closing the old
  one; the new one rebuilt its heap wal-index from the `-wal`, then
  the old one's close checkpointed and unlinked that file, and the new
  one's later commits went to the unlinked inode — shown by its
  SELECTs, on no file once the process died. The CLI now closes first,
  as the pin's shell does, and when the new open fails it carries on
  with a substitute in-memory database, as the pin does (it kept the
  old one). The root cause — two connections on one WAL database in
  one process — is closed in the library too: the storage hook
  `Storage::locked_elsewhere` became `claim_wal` / `release_wal`.
  `FileStorage` takes an exclusive `flock` on the database file when a
  database opens in or enters WAL mode (Unix; SQLite's POSIX locks
  neither take nor see it) and drops it on leaving WAL mode, at close
  or when dropped; a second hipp connection, in this process or
  another, gets `DatabaseBusy` at open or at `journal_mode = WAL`.
  The live-pin check (`/proc/locks`) is unchanged. `crash_writer`'s
  wrapper forwards the hooks.
- **Builds without `wal` read a stale main file beside the pin's
  committed frames** (high; every build without the leaf, the tiny
  profile included). The no-wal open path looked only at header bytes
  18 / 19, but the pin opens a database in WAL mode whenever a
  non-empty `-wal` sits beside it (probed: 100 bytes of junk there,
  `journal_mode` reports `wal` and commits go into the log with the
  header still 1 / 1). hipp then returned the old rows and committed
  into the main file under the log, where the pin's replay hid them.
  A new hook, `Storage::log_beside` (default `false`; `FileStorage`
  looks for a non-empty `<db>-wal`), is consulted at open without the
  leaf: such a database is refused with `WalUnsupported`. (A log beside
  an empty file is refused there too, where the pin would delete it.)
  With the leaf the log is read through `Sidecar::Wal`, as before. The
  earlier gap's "only hand-placed" claim and README's "never read
  stale" were wrong until now.
- **An UPDATE logged pages the pin leaves clean** (medium; `wal`).
  Every UPDATE deleted and reinserted every index entry and the row,
  so an index no assigned column reaches, or a row whose record did
  not change, was written: one frame per such page too many in
  `wal_checkpoint`'s `log|checkpointed` (`t(a PRIMARY KEY, b)`, `SET
  b = 'y'`: 0|2|2 against the pin's 0|1|1). Probed page by page on the
  pin (its `-wal` listed by `.system`): an index is left alone unless
  an assigned column, a generated column computed from one, its WHERE
  or the rowid (a WITHOUT ROWID primary key) reaches it; a plain one
  that is reached is rewritten even with its entry unchanged (`SET a =
  a` on `t(a)` logs the index page); one with an expression or VIRTUAL
  generated key part only when its entry changes (`t(a+0)`, `SET a =
  a`: nothing; a WITHOUT ROWID row whose key value changes rewrites
  every entry the change reaches, as its entries end in the key — the
  first gate run caught a version that compared handles there); the row is rewritten only when its key is assigned or
  its record changes, and a record of the same size is overwritten in
  place, only its local part and overflow pages whose bytes differ
  written (the pin's `btreeOverwriteCell`: `SET b = 'z'` past a 5000-byte
  blob logs one overflow page). `rewrite_stored_row` (UPDATE and
  upsert DO UPDATE) now does all that (`btree::btree_overwrite`,
  `TableRules::changed_cols`, the CHECK path's change set factored
  out). Gated on `wal`, where the counts show: without the leaf the
  old rewrite stays (rollback mode then journals the extra pages,
  which no result shows), so `full` does not grow.
- **In-memory databases entered WAL mode** (medium). The pin keeps a
  `:memory:` database in journal mode `memory`: every `journal_mode`
  query and assignment (WAL, DELETE, OFF, …) reports `memory`,
  `wal_checkpoint` is `0|-1|-1`; hipp reported `delete` (before this
  unit too), then switched to WAL and checkpointed. A new hook,
  `Storage::is_memory` (`v16`, default `false`), and
  `MemStorage::memory_db()` mark an in-memory database (plain
  `MemStorage::new()` still models a file, as the tests and crash
  injectors need); the CLI's `:memory:` uses it. Ungated within the
  `v16` PRAGMA code. (`PRAGMA temp.journal_mode` stays `delete`, as on
  the pin.)

**Probing notes:** the pin's shell `.open` closes first, and on a
failed open prints its error and "using substitute in-memory
database". UPDATE pages (two-row tables, one leaf per tree, the
pin's frames listed): `t(a)` / `SET a = 1` with a already 1 → the
index page; `t(a+0)`, `t(lower(a))`, `t(a, b || '')` → none; `t(c)
WHERE a > 0`, `SET a = a` → the index page; `t(c) WHERE a > 5` (row
outside the index before and after), `SET a = 2` → the table page
only; `c AS (a + 1) VIRTUAL` indexed, `SET a = a` → none, STORED → the
index page; `t(a INTEGER PRIMARY KEY, …)` with `SET a = a` → the table
and every plain index, not an expression index; WITHOUT ROWID `SET b =
b` → none, `SET a = a` → table and index. A REAL column's integral
value is stored by the pin as an integer (serial type 1), by hipp as
an 8-byte real (pre-existing); both read 1.0.

**Gates:** `cargo fmt --all` and the full `ci/local-gates.sh` end
with "==> all gates passed". `update_frames_match`'s generated part
agreed exactly (rows, checkpoint row, integrity) at
`HIPP_WAL_CORPUS=400`.

**Size:** tiny **76,976 B** (Δ **0**: the no-wal check is a default
method `MemStorage` does not override). Full **498,252 B** (Δ
**+72 B**, ratchet raised: the ungated in-memory journal-mode fix in the
`v16` PRAGMA code). Max **748,212 B** (Δ **+2,952 B** vs 745,260,
raised): the UPDATE page rules and `btree_overwrite`, the WAL claim.

**Honest gaps (updated):**

- The pin's `hasFK > 1` case (an UPDATE of a parent key that an FK
  with an ON UPDATE action references rewrites every index) is not
  modelled: hipp logs only the indexes the change reaches.
- A record whose size changes is still deleted and reinserted (the
  pin frees and reallocates its overflow pages differently: a
  5000-byte blob grown by one byte logs 4 frames on the pin, 2 on
  hipp); pre-existing layout differences after deletes stand.
- A row the pin wrote with a REAL column's integral value is a
  different record to hipp (integer vs 8-byte real, pre-existing), so
  hipp rewrites it where the pin finds it unchanged.
- The WAL claim is advisory and Unix-only (`flock`); a non-Unix
  `FileStorage` claims nothing. Rollback-mode connections take no
  lock, so two hipp connections on one rollback-mode file are still
  not coordinated (pre-existing).
- Without the leaf a non-empty `-wal` beside an empty database file
  is refused rather than deleted as the pin does; a caller's own
  storage must implement `log_beside` to be checked.
- The unit's other gaps stand.

### Honest-gap sweep: constraint conflict clauses, INDEXED BY, three-part names, TEMP across databases (2026-11-04)

**Shipped** (each probed on the live 3.54.1 pin first):

- **Constraint-level `ON CONFLICT` clauses** (ungated from `v07`: a
  silent wrong answer on claimed paths — a `NOT NULL ON CONFLICT
  FAIL` multi-row INSERT lost the rows before the failing one, an
  `ON CONFLICT IGNORE` / `REPLACE` errored where the pin resolves).
  The parser keeps each NOT NULL / PRIMARY KEY / UNIQUE clause
  (`ColumnDef::nn_conflict`, `ConstraintSet::conflict`; a repeated
  constraint merges, two different clauses are the pin's
  "conflicting ON CONFLICT clauses specified",
  `ParseKind::ConflictingOnConflict`); the catalog carries them
  (`Column::nn_conflict`, `TableDef::ipk_conflict`,
  `IndexDef::conflict`). A statement without an `OR` clause now
  parses as `ConflictAction::Unset`, and the planner resolves each
  constraint by `ConflictAction::over` (explicit statement clause >
  constraint clause > ABORT) into `InsCol::nn_act`,
  `Plan::{Insert,Update}::ipk_act`, `Update::nn_acts` and
  `IndexPlan::act`. Indexes are checked in the pin's list order —
  newest first, every index whose own clause is REPLACE after the
  others — with a rowid REPLACE deferred past them unless the
  statement says REPLACE; the first violated constraint that does not
  resolve by REPLACE decides (IGNORE skips the row, ABORT / FAIL /
  ROLLBACK fail it), and only all-REPLACE conflicts delete victims.
  NOT NULL ... REPLACE substitutes the default; a NULL default fails
  as ABORT after the later columns had their turn (the pin's second
  pass). The failing constraint's own action is recorded
  (`Statement::fail_act`), so FAIL keeps and ROLLBACK ends exactly
  as that constraint says (this replaces the review-era text scan
  `rollback_constraints`, which matched by constraint *kind*). CHECK
  ignores its clause, as on the pin; the column-level `CHECK ...
  ON CONFLICT` stays a syntax error. Interplay: an upsert's DO UPDATE
  resolves by ABORT whatever the clauses say (regression found here:
  `INSERT OR FAIL ... DO UPDATE` kept the rows before a failing DO
  UPDATE; the pin keeps none); trigger bodies take the firing
  statement's explicit clause, else their constraints'; REPLACE
  victims fire DELETE triggers under `recursive_triggers` and FK
  actions as statement REPLACE does; the pin's `bReplace` (UPDATE
  one-pass order) now reads the indexes' clauses instead of the
  table text. Builds below `v07` (tiny, `indexes`, `v04` .. `v06`)
  refuse FAIL / IGNORE / REPLACE constraint clauses at parse
  (`ParseKind::RequiresConflictResolution`, the CHECK precedent);
  ROLLBACK / ABORT need nothing.
- **`INDEXED BY name` / `NOT INDEXED`** (`v16`) on FROM sources and
  UPDATE / DELETE targets (`IndexHint`, `query::Hint`): "no such
  index" (`UnknownIndex`) for a name the table lacks and on a view
  or CTE; with `expr_index`, "no query solution"
  (`SchemaKind::NoQuerySolution`) for a partial index the WHERE
  (and the source's ON) does not imply, and the planner (`xplan::
  choose`) considers only the named index, its full scan always a
  candidate (the pin's `isIndexedBy`), or no index at all (rowid
  lookups stay; ignored on WITHOUT ROWID tables, as the pin's
  `notIndexed` is). Without `expr_index` the clause is checked and
  the scan keeps rowid order. `t INDEXED BY` is never an alias.
- **Three-part column names** `schema.table.column` (`v16`,
  `Expr::Column::schema`) in every expression context the pin allows
  them — select items, WHERE / GROUP BY / HAVING / ORDER BY, ON,
  correlated subqueries, UPDATE / DELETE, CHECK texts, partial-index
  WHERE texts, views, trigger bodies — the table part matching the
  source's qualifier (its alias when it has one), the schema the
  source's database (`SourceDesc::db`: main or temp; a derived
  table, CTE or RETURNING row is none). Pin-written files with such
  CHECKs / partial indexes are no longer opaque, and ALTER TABLE
  RENAME rewrites the names as the pin does. Generated columns and
  index keys refuse them, `schema.table.*` is a syntax error (pin).
- **TEMP across databases** (`v16`): a statement of the main
  database reads TEMP tables (`Connection::plan_temp_source`,
  `Statement::scan_temp`): joins, subqueries, compounds, `x IN
  temp.t`, INSERT / UPDATE / DELETE of a main table and CREATE TABLE
  ... AS in main that read temp; an unqualified name is temp first,
  `main.x` / `temp.x` are honoured (`FromSource::in_temp`). A temp
  INSERT ... SELECT and CREATE TEMP TABLE ... AS SELECT that read
  main run their SELECT on main and feed the rows to the temp
  database (`Route::TempFed`, `Plan::Temp::feed`; a CTAS changes no
  counter, as on the pin). TEMP views over main and temp tables
  together are inlined into main statements. Table-valued pragmas
  (`pragma_table_info`, `pragma_index_list`, `pragma_index_info`,
  ...) see TEMP objects, temp first, and take the `'temp'` schema.
- **VACUUM of an auto-vacuum file / `PRAGMA incremental_vacuum`**:
  **kept as the documented refusal.** The pin's pointer-map pages
  would have to be maintained by every page allocation, free and
  B-tree balance on the write path; that is a pager project well
  past this unit's budget, and a fresh auto-vacuum image would be
  damaged by the next hipp write. `PRAGMA auto_vacuum = 0; VACUUM`
  converts. (~~A hipp write to a pin-made auto-vacuum file leaves
  the maps stale~~: the review fixes refuse every such write.)

**Oracle-probing notes** (live pin, `sqlite3 DB` over stdin, one
statement per line): `NOT NULL ON CONFLICT IGNORE` skips the row,
`REPLACE` takes the DEFAULT (none or NULL: ABORT), FAIL keeps earlier
rows; `UNIQUE ON CONFLICT IGNORE` + `UNIQUE ON CONFLICT FAIL` on one
row: the newer index decides (b checked before a); an explicit
CREATE UNIQUE INDEX (OE_Abort) is checked before an older
constraint's IGNORE; `a UNIQUE REPLACE, b UNIQUE IGNORE` over (1,2)
conflicting on both: IGNORE wins (REPLACE last); `id INTEGER PRIMARY
KEY ON CONFLICT REPLACE, b UNIQUE ON CONFLICT IGNORE`: IGNORE wins
(rowid REPLACE deferred); table-level `CHECK (...) ON CONFLICT IGNORE`
parses and is ignored, column-level is "near ON: syntax error"; `OR
ABORT` overrides a constraint ROLLBACK; `a NOT NULL ON CONFLICT
ROLLBACK, b NOT NULL`: b's NULL aborts the statement only; DO UPDATE
under `INSERT OR FAIL` keeps nothing; a TEMP-less trigger body
inherits the outer explicit clause. `INDEXED BY ib WHERE a > 0`
scans ib (b order); `INDEXED BY main.ia` and `INSERT ... INDEXED BY`
are syntax errors; a view / CTE source is "no such index"; a partial
index `WHERE c > 'x'` with `WHERE c = 'z'` is "no query solution"
(only identical / IS NOT NULL implications count); NOT INDEXED on a
WITHOUT ROWID table still walks its covering index. `main.x.a FROM t
AS x` resolves, `main.t.a FROM t AS x` does not; `temp.t.a FROM t`
resolves to the temp t that shadows main; RETURNING / generated
columns / index expressions refuse three-part names. CTAS (main or
temp) leaves changes() / total_changes() / last_insert_rowid()
alone; `CREATE TEMP TABLE IF NOT EXISTS u AS ...` over an existing u
reads nothing.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed" (fmt, clippy on default /
all-features / every `ci/feature-sets.txt` line, workspace tests,
the four no_std targets, the tiny / full / max size gates).

**Size:** tiny **76,976 B** (Δ **+12 B** vs 76,964, ratchet raised to
it; a correctness fix on the CREATE TABLE path the tiny profile
claims, within the +256 B allowance: below `v07` a constraint-level
FAIL / IGNORE / REPLACE is refused at parse instead of ignored, so a
multi-row INSERT no longer loses the pin's FAIL rows silently). Full
**498,180 B** (Δ **+7,184 B** vs 490,996, ratchet raised; under 500
KiB): the per-constraint resolution (ungated from `v07`) and the
`v16` surface — INDEXED BY checks, three-part names, temp sources,
temp feeds, pragma rows from temp. Max **745,260 B** (Δ **+7,884 B**
vs 737,376, ratchet raised): the same plus the forced / forbidden
index planning and "no query solution" (`expr_index`).

**Tests:** `tests/gaps_v16.rs` — five hand corpora (constraint
clauses incl. upsert / trigger / FK / WITHOUT ROWID interplay and the
regressions above; INDEXED BY incl. joins, subqueries, partial,
expression and WITHOUT ROWID indexes; three-part names incl. temp
shadowing; main/temp statements; pragma functions on temp objects),
a generated corpus of 160 random constraint-clause schemas and
writes (in and out of transactions, every statement clause) and 96
random INDEXED BY / NOT INDEXED queries whose unordered rows must
come in the pin's order, and a pin-written file with three-part
CHECK / partial-index / view texts that hipp writes and renames.
Every case compares rows, failing statements and the pin's `.dump`
of both files, plus the pin's integrity check of hipp's.
`hipp-cli/tests/cli_v16_gaps.rs`: four byte-identical batches,
one-shot and over stdin. `tests/temp_v16.rs` now asserts the mixed
statements run (and the temp UPDATE / DELETE refusal).

**Honest gaps:**

- A temp UPDATE / DELETE whose expressions read main tables, and an
  INSERT ... SELECT into temp with an upsert, RETURNING, a leading
  WITH or bound parameters, are still refused (`Unsupported`): the
  temp database cannot read main, and only a whole SELECT can be fed
  across. TEMP triggers on main tables (and temp triggers whose
  bodies write main tables) stay refused, as before.
- Temp tables read by a main statement are scanned in rowid order
  with no index access path (a temp index is not planned there, and
  `INDEXED BY` on them is checked only), so unordered output over a
  temp index can differ in order from the pin's; a WITHOUT ROWID
  temp table is refused in a mixed statement.
- `PRAGMA table_info` / `pragma_table_info` of a TEMP view over main
  tables fails (the temp database plans the view), and
  `temp.pragma_table_info(...)` (a schema-qualified function) and
  correlated pragma-function arguments stay refused.
- Constraint resolution approximations: with an upsert, the pin
  checks the target index first and then the rowid (`upsertIpkDelay`);
  hipp checks the rowid first — they differ only in which error a row
  violating both the rowid and a non-target index under different
  clauses reports. A REPLACE deletion whose DELETE triggers re-create
  a conflict is re-checked under ABORT (as for statement REPLACE).
- Without `expr_index` (the `full` profile) INDEXED BY / NOT INDEXED
  are checked but do not change the scan order; a partial index named
  there is an index hipp cannot read (`Unsupported`).
- VACUUM of an auto-vacuum file and `PRAGMA incremental_vacuum` stay
  refused (above); since the review fixes, so does every other write
  to such a file.
- ~~Pin-written CHECK / partial-index texts with three-part names
  are no longer opaque~~ overclaimed: a `temp.` or unknown schema
  part on a main table was still refused — closed by the review
  fixes, as are the regressions and refusals listed there.

### Honest-gap sweep — review fixes (2026-11-04)

**Fixed** (each reproduced and compared with the live 3.54.1 pin):

- *Main views and triggers read TEMP tables* (regression of the
  sweep, `v16` / `triggers`): the new temp-first routing reached the
  bodies of views and triggers stored in main, so a temp table
  shadowed their tables (`SELECT * FROM v` answered the temp rows; a
  trigger wrote rows read from a temp table) and stood in for a
  missing one where the pin says "no such table: main.t". The pin
  binds such a body's unqualified names to its own database
  (`sqlite3FixSrcList`). The view / CTE expander now marks every
  table name it walks in a main view's body (`Expander::fix_main`,
  set by `view` and `expand_parsed_view`) and in a trigger program's
  statements (`TrigState::fix_main`, set by `plan_step` and by the
  ALTER TABLE re-plan of trigger texts) as main-qualified, once the
  body's own CTEs are resolved. ALTER TABLE of a main table that a
  temp table shadows rewrites its views and triggers again (it
  failed with "no such table" / "no such column").
- *INDEXED BY / NOT INDEXED on a trigger body's UPDATE / DELETE was
  stored* (`triggers`), after which the pin found the schema
  malformed: now the pin's parse error
  (`SchemaKind::TriggerIndexedTarget`, checked before RETURNING, as
  on the pin). Elsewhere in a body (a SELECT, UPDATE ... FROM, a
  subquery) the clause stays allowed.
- *Writes to an auto-vacuum file corrupted it* (ungated: a claimed
  write path). hipp keeps no pointer-map pages, and an INSERT or
  CREATE that allocated pages left the pin's maps stale (its
  integrity check: "Failed to read ptrmap key"). `Pager` now notes a
  non-zero header offset 52 at open and `begin_write` refuses
  (`Error::AutoVacuumUnsupported`), leaving the file byte-identical;
  reads work, and `PRAGMA auto_vacuum = NONE; VACUUM` — whose image
  is plain, so `replace_image` lifts the refusal for it alone —
  converts the file, after which writes work.
- *NOT INDEXED on a view or CTE* was "no such index"; the pin accepts
  it (only INDEXED BY names an index there).
- *Three-part names.* The schema part of a CHECK's or partial
  index's name is ignored, any word, as on the pin (`NC_IsCheck` /
  `NC_PartIdx`): `CHECK (temp.u.a > 0)` on a main table, `bogus.t.a`,
  and `main.t.b` in a TEMP table's CHECK were refused (by the router
  or the resolver), and pin-written files with them failed every
  INSERT (`query::DB_ANY`; the router now places such a `temp.` in a
  CREATE TABLE / CREATE INDEX). Resolution filters sources by
  database (`SourceDesc::in_db`, `query::group_column`) instead of
  checking the first source of the name: `main.t.b, temp.t.b FROM
  main.t, temp.t` was "ambiguous", `(SELECT main.t.b FROM temp.t)
  FROM main.t` failed, and `*` / `t.*` over `main.t, temp.t` were
  "ambiguous" (the pin expands them per database). Table-valued
  functions (`pragma_*`, `json_each` / `json_tree`) are main's, the
  empty temp schema temp's. The temp schema table answers to
  `sqlite_temp_master` / `sqlite_temp_schema` (with a schema
  written, also to `sqlite_master` / `sqlite_schema`) and no longer
  to a bare `sqlite_master` / `sqlite_schema` qualifier (the pin's
  `isValidSchemaTableName`): `sqlite_temp_master.name FROM
  sqlite_temp_master` was "no such column", `sqlite_schema.name FROM
  temp.sqlite_schema` answered.
- *INDEXED BY a partial index on a LEFT JOIN's right table* was "no
  query solution" whenever the implying term sat in WHERE. The pin
  makes the join inner when the WHERE rejects the table's NULL row
  (`sqlite3ExprImpliesNonNullRow`, modelled as `xplan::Src::
  null_rejects`: comparisons, arithmetic, IN lists and BETWEEN by
  their left operand, AND / OR, a top-level IS NOT NULL only where
  the pin's AND chain tries its left operands; IS / IS NOT, IS NULL,
  functions, LIKE and CASE do not count), and then the WHERE may
  imply the index.
- *CREATE TABLE ... AS over a TEMP view* (main CTAS) was "no such
  table": the views are now written in after its `AS`
  (`inline_temp_views`, `start`). A TEMP CTAS or temp INSERT ...
  SELECT over a TEMP view of main tables, or over a main view, failed
  the same way: the fed SELECT now gets the temp views inlined
  (`route`) and its views and CTEs expanded (`expand_ast`) like any
  main statement.
- Code: `resolve_col_ref` has its `#[inline(never)]` back (the
  three-part check had been inserted between the attribute and the
  function), and the three-part doc no longer claims `temporary` is
  accepted (the pin takes only `temp`).

**Oracle-probing notes:** `CREATE VIEW mv AS SELECT * FROM t` over a
temp-only t is stored and fails when read ("no such table: main.t");
`... FROM temp.t` in a main view or trigger is "cannot reference
objects in database temp"; FK actions name their child table and are
unaffected by a temp table of its name. `DELETE FROM t NOT INDEXED`
in a trigger body: "the NOT INDEXED clause is not allowed on UPDATE
or DELETE statements within triggers" (before the RETURNING error);
`SELECT ... FROM t INDEXED BY i` there is fine. `SELECT * FROM
(SELECT 1) NOT INDEXED` and `pragma_table_info('t') NOT INDEXED` are
syntax errors on both. `CHECK (main.x.a > 0)` (wrong table) is "no
such column: x.a"; in generated columns `temp.t.a` is "no such
column" and `main.t.a` '"." operator prohibited'. `main.s.x FROM
(SELECT 1 x) s` and `main.c.x` of a CTE: "no such column". `u LEFT
JOIN t INDEXED BY tp ... WHERE t.b LIKE '1' AND t.b IS NOT NULL` is
"no query solution" (LIKE is a function; the trailing IS NOT NULL is
walked, not tried), while `WHERE u.x > 0 AND (t.b IS NOT NULL AND
u.y > 0)` answers. On the pin an auto-vacuum file takes `PRAGMA
user_version = 3`; hipp refuses that header write too.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **−16 B** vs 76,976, ratchet lowered;
the ungated auto-vacuum write refusal costs a few bytes in the
pager, and the code around it compiled smaller). Full **498,456 B**
(Δ **+204 B** vs 498,252, ratchet raised; under 500 KiB): the main
views / triggers mark, the database filter of three-part names, the
temp schema table's names, the CTAS / fed-SELECT temp-view inlining.
Max **749,488 B** (Δ **+1,276 B** vs 748,212, ratchet raised): the same
plus the trigger-body index-clause check (`triggers`) and the LEFT
JOIN null-rejection test (`expr_index`).

**Tests:** `tests/gaps_v16.rs` — `main_objects_read_main_only` (view
and trigger shadowing and missing tables, ALTER re-plans, FK
actions), `indexed_by_review` (trigger bodies, derived sources, 22
LEFT JOIN forms), `indexed_by_trigger_leaves_file_readable`,
`three_part_review` (CHECK / partial / generated / index-key texts,
main and temp side by side, table-valued functions, the temp schema
table's names), `pin_written_ignored_qualifiers` (a pin-written file
hipp writes), `temp_views_feed_writes` (main and TEMP CTAS, temp
INSERT over TEMP and main views), `auto_vacuum_file_is_not_written`
(FULL and INCREMENTAL: refused writes leave the file byte-identical,
then the conversion). `hipp-cli/tests/cli_v16_gaps.rs`:
`review_fixes`, byte-identical one-shot and over stdin.

**Honest gaps (still open):**

- Every write to an auto-vacuum file is refused, header-only ones
  (`PRAGMA user_version`) included, which the pin performs; VACUUM
  of such a file keeps its refusal unless `auto_vacuum = NONE` was
  set first.
- `PRAGMA table_info` of a TEMP view over main tables still fails
  (above).

### Date and time functions (2026-11-04)

**Shipped** (leaf `datetime`, requires `v16`; in `max`, not `full`):

- `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`,
  `strftime()`, `timediff()` and the `CURRENT_DATE` / `CURRENT_TIME`
  / `CURRENT_TIMESTAMP` keywords in any expression (`src/datetime.rs`).
  The engine is a model of the pin's `date.c`: its `DateTime` record
  (integer-millisecond Julian day `iJD` plus civil fields, each half
  valid or not, the raw-number, error, subsec, UTC and local flags,
  the `floor` day count), every parser, modifier and formatter
  updating those fields in the pin's order with C's truncating
  integer division / remainder, `core` `f64` only (sums, products,
  quotients: IEEE-identical), and x86-64's float-to-integer
  conversion (NaN / out of range is the "integer indefinite"). The
  `%f` / `%J` / subsec `%s` / `timediff` seconds go through
  `func::printf`'s pin-fitted `%06.3f` / `%.16g` / `%.3f`; julianday()
  is a REAL rendered by `record::render_real`. The engine is not
  generic over the storage error (one instantiation).
- Time values: `[-]YYYY-MM-DD`, then spaces or `T`s, `HH:MM[:SS[.F]]`
  (fraction truncated at .999, rounded below), `[+-]HH:MM` / `Z`
  zones, bare times (2000-01-01), numbers (Julian day when in
  0..5373484.5; `'unixepoch'` / `'auto'` / `'julianday'` as the first
  modifier), numeric text (the pin's whole-text `sqlite3AtoF`),
  `'now'`, `'subsec'` / `'subsecond'` as a value; BLOBs read as text,
  a NUL ends any text.
- Modifiers: `NNN days|hours|minutes|seconds|months|years` (signed,
  fractional, the `float` limits, months / years through the
  day-of-month overflow and its `nFloor`), `[+-]YYYY-MM-DD[ HH:MM[:SS[.F]]]`,
  `[+-]HH:MM[:SS[.F]]`, `start of day|month|year`, 3.54's
  `end of day|month|year`, `weekday N` and 3.54's backward
  `weekday -N`, `unixepoch`, `julianday`, `auto`, `localtime`,
  `utc` (the pin's four-guess inversion), `subsec` / `subsecond`,
  `ceiling`, `floor`.
- `Config::clock` (existing) is now read by `'now'` too; under `std`
  `Config::default()` supplies the system clock (`systime::now_ms`;
  it used to be `None`, so `Config::default().no_clock()` is the new
  way to say "no clock"). The statement's instant is cached in
  `query::Env` and owned by one statement from its prepare (whose
  plan may read it for a DEFAULT) through its execution, trigger and
  foreign-key programs included; another statement running in
  between, or `Statement::reset`, makes the next read fresh
  (`Env::claim_clock` / `own_clock` / `release_clock`).
- New `Config::tz_offset: Option<TzOffset>` (`fn(i64) -> Option<i64>`:
  Unix seconds → seconds east of UTC; `None` result is
  `SchemaKind::LocalTimeUnavailable`, the pin's "local time
  unavailable"). No hook is UTC (the `no_std` default); under `std`
  the default is `systime::local_offset`, which reads `TZ` as glibc
  does: unset → `/etc/localtime`, empty → UTC, a leading `:` dropped,
  a TZif file under `$TZDIR` or `/usr/share/zoneinfo` (64-bit data,
  the first standard type before the first transition, the footer
  POSIX rule at or after the last), else a POSIX TZ string (`Jn`,
  `n`, `Mm.w.d`, `/time` up to ±167 h, southern-hemisphere rules; a
  daylight name without rules takes M3.2.0,M11.1.0), else UTC. The
  hook is asked, as the pin asks `localtime_r`, only about instants
  in 1970-01-01 .. 2038-01-18: others are moved to the year of the
  same leap phase in 1997..2003 and back. The CLI dropped its own
  clock and uses the defaults.
- "Pure" contexts: a CHECK constraint, an index expression or a
  partial index's WHERE (`Ctx::pure`) and a generated column (no
  connection state) fail the statement with
  `SchemaKind::NonDeterministic` when a date function reads the
  clock (`'now'`, no arguments, `'subsec'` as the value) or the zone
  (`'localtime'`, `'utc'`), as the pin's `sqlite3NotPureFunc` does;
  the CURRENT_* keywords are non-deterministic functions there
  (refused in generated columns and indexes at CREATE, allowed in a
  CHECK).
- Ungated (every profile with the parser): an unquoted `CURRENT_TIME`
  / `CURRENT_DATE` / `CURRENT_TIMESTAMP` parses as a call of no
  arguments (the pin's `CTIME_KW`), so a table's own quoted
  `"current_date"` column no longer stands in for the bare keyword
  (a silent wrong answer: `SELECT current_date = 5 FROM x` was 1),
  and `current_date()` is a syntax error. Without the leaf the call
  resolves to the DEFAULT clock pseudo-columns (renamed with a
  leading NUL, which no column name holds) and is "no such function"
  anywhere else; DEFAULT CURRENT_* keeps working (lib test
  `rules::clock_tests` runs in both `full` and `max`).

**Oracle-probing notes** (all on the live 3.54.1 pin; the binary's
strings listed the modifier names, which is how `end of ` was found):

- 3.54.1 knows no `%y` (NULL, as every unknown conversion and a
  trailing `%`); the known set is `d e f F G g H I j J k l m M p P R
  s S T u U V w W Y %`. `%J` is `%.16g`; `%Y` of year -12 is `-012`.
- New in 3.54 against the 3.4x code: `end of day|month|year` (23:59:59.999;
  month end resets the floor count, year end does not, on a raw number
  it is NULL); `weekday -N` (back to the latest such weekday, a no-op
  on one; `-0` is backward Sunday, ±7 and fractions NULL); hour 24 only
  as exactly `24:00:00[.000]`, normalised to the next midnight when a
  date is given (at parse) or when the Julian day is computed — so
  `datetime('24:00', 'start of day')` is still 2000-01-01; an explicit
  `+00:00` / `-00:00` zone marks the value UTC (`'utc'` is then a
  no-op) while a non-zero one does so only once applied
  (`datetime('12:00+01:00', 'utc')` in New York is 16:00).
- Quirks reproduced: a value made invalid by a modifier and then
  `'localtime'`'d comes back (the error flag is cleared): year 2000 if
  the error came from the year range, year 0 if from the Julian-day
  range, its negative milliseconds printed as `'0' + digit`
  punctuation (`00.,''`); day-of-month overflow survives `start of
  day` (`2001-02-31 00:00:00`) and a lone YYYY-MM-DD with day > 28 is
  normalised; a fraction of 400 digits is NaN and the date NULL; a
  `[+-]YYYY-MM-DD HH:MM` modifier negates its time too;
  `'+24:00'` adds nothing (the time-of-day is taken modulo a day).
- 'now' is stable per `sqlite3_step` on the pin (re-read when a step
  starts); `SELECT current_date()` is a syntax error; `timediff` with
  other than two arguments is a prepare-time error.

**Tests:** `tests/datetime_v16.rs` (11 tests): a fixed corpus of ~400
expressions (every form, modifier and conversion, the boundaries,
quirks and a regression case per bug found while fitting: hour 24,
zero-offset UTC, NaN fractions, the backward weekday, `end of`, `%y`)
and a generated corpus (xorshift64: random values in all input forms,
edge years, leap days, negative / huge Julian days, fractional seconds,
random modifier chains, random strftime formats; 6,000 cases in UTC
and 2,500 in each other zone, `HIPP_DATETIME_CORPUS` scales it —
160,000 ran clean once) and a DST corpus (3,000 instants around the
transitions and outside 1970..2037 through localtime / utc chains),
each byte-compared as `quote()` against the pin in UTC,
America/New_York, Asia/Kolkata and Australia/Lord_Howe (pin under
`TZ`, hipp through `systime::zone_offset`); 'now' against the pin's
value for the clock's instant; a ticking clock read once per
statement (DEFAULTs, a trigger, a multi-row INSERT, re-execution);
`NoClock`; `LocalTimeUnavailable`; the pure contexts, the keywords
vs a `"current_date"` column, timediff's arity and a UTF-16 database
as statement scripts on both engines (success / failure and results
must agree, the pin checks hipp's file). `hipp-cli/tests/cli_v16_datetime.rs`:
a rendering batch (with CURRENT_* defaults, a date CHECK and a date
index) and a 400-statement generated batch, byte-identical one-shot
and over stdin in UTC, New York and Kolkata (the CLI reads `TZ`
itself). Ad-hoc fuzzing while fitting also covered Sao_Paulo,
Chatham, Casablanca, Godthab, Gaza, `EST5EDT`, `XYZ-5:30`, a
`M3.5.0/-2` rule, `:Europe/Paris`, empty and unknown `TZ` (all clean).
`tests/indexes.rs` now uses `unistr_quote()` as its function hipp
lacks (it used `date()`); `tests/table_defs_v16.rs` asks for
`no_clock()` where it tests `NoClock`.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed". (One earlier run failed
`views_cte_v16::shared_materializations` on a `random() % 1000`
collision on the pin's side — a pre-existing flake, unrelated; it
passed on rerun.)

**Size:** tiny **76,960 B** (Δ **0**). Full **498,880 B** (Δ **+424 B**
vs 498,456, ratchet raised; the ungated keyword fix: the parser's call
node and the pseudo-column resolution). Max **765,344 B** (Δ
**+15,856 B** vs 749,488, ratchet raised): the leaf is about 15.4 KB
on thumbv7em (soft double arithmetic, the modifier parser and the
formatters). **`full` decision:** `full` with `datetime` measures
518,192 B, over the 512,000 B line, so the leaf stays in `max` only
(ADR 0002).

**Honest gaps:**

- 'now' is read once per statement execution, where the pin re-reads
  it at each `sqlite3_step`: a streaming SELECT that crosses a
  millisecond shows one instant on hipp and possibly several on the
  pin. A statement prepared before another statement runs reads the
  clock again at its first step (plan-time DEFAULTs keep the value
  read at prepare). Without `params` an INSERT's DEFAULTs are
  evaluated at prepare (as before). The TEMP database's connection
  has its own cache, so a statement mixing main and temp can see two
  instants.
- The zone hook gives an offset only (see the review fixes below
  for what `systime` now models of glibc and what it still does
  not). `systime` reads the zone file on every conversion (no
  global cache by design).
- The pin's "non-deterministic use of date() in ..." message is
  `SchemaKind::NonDeterministic` (the same kind as the CREATE-time
  refusal); in a generated column a date function that needs the
  clock and has no connection state reports the same.
- Not in `full` (above); without the leaf, `SELECT current_date`
  is refused rather than answered.

### Date and time functions — review fixes (2026-11-04)

Four review findings, each reproduced on the live pin first; all
fixed, with regression tests.

**Fixed:**

- `'end of month'` (leaf `datetime`) is now the pin's "first
  midnight of the next month, less one millisecond": in December
  9999 that midnight is out of range, so the value is an error
  (NULL, and `'localtime'` / `'utc'` after it give the pin's
  error-recovery `2000-01-01 00:00:00`). On a value already invalid,
  the fallback 2000-01-01 is taken before the subtraction, so `'utc'`
  after it gives `1999-12-31 23:59:59`. hipp used to set the last day
  and 23:59:59.999 directly. `'end of day'` / `'end of year'` do set
  the time directly on the pin, and keep doing so.
- `'floor'` after a `24:00` value with a day-of-month overflow
  (`'2024-02-30 24:00:00'`) no longer moves back a day: the pin folds
  the overflow into the next-midnight normalisation, with or without
  a zone suffix (`compute_jd` clears the floor count for hour 24).
- `systime` (std only) is now a model of glibc 2.39's `localtime`,
  not just of well-formed zones. POSIX TZ strings are parsed as
  `__tzset_parse_tz` does, partial results included. A missing or
  malformed rule part keeps the offsets and whatever the rule parser
  filled in before it stopped, so `EST5EDT,M3.2.0` is the US rules,
  trailing junk keeps the rule with a 00:00 change, and a failed
  start rule leaves both changes near January 1st. A missing
  daylight name leaves a daylight offset of 0. The `<...>` names,
  `%hu` scanning (blanks, signs, 16-bit wrap) and the
  southern-hemisphere test follow glibc. TZif files are read with
  `__tzfile_read`'s checks, so a file glibc rejects is UTC on both
  sides: a type index out of range, a DST flag other than 0/1, an
  abbreviation index past the names, counts that overrun the file,
  or no byte left for the footer. A version byte other than 0 means
  64-bit data, and the footer is everything between the newline and
  the last byte (up to a NUL). glibc's transition search is used,
  and leap-second records now correct the offset, as
  `__tz_convert` does. A daylight name without rules takes the
  `posixrules` file from `TZDIR`, its transitions shifted as the
  pin's glibc shifts them. In a TZif footer, such a string computes
  with the zeroed rules, as on glibc. An empty `TZDIR` is the
  default directory.
- Ungated parser fix: an unquoted CURRENT_* keyword followed by `.`
  is a syntax error at the dot, as the pin's CTIME_KW is. This covers
  `current_date.x` and the `current_date.*` result column, in SELECT
  and RETURNING. hipp used to read it as a qualified column.

**Oracle-probing notes:** to fit glibc's `__tzfile_default`, I
used hand-made `posixrules` files under `TZDIR`. The standard-time
shift is the string's offset less the file's latest standard
offset. The daylight-time shift ignores the file's daylight
offsets. glibc frees `old_tz` on this path, so it re-parses the zone
on every `localtime` call. Each such re-parse shifts by the
previous call's reset `rule_dstoff` (the string's own daylight
offset), so there is no shift. The first conversion of a process
sees 0 instead. hipp models the steady state, and the CLI tests
warm up with one conversion. glibc's month table read for a rule
month outside 1..12 (kept from a failed `M` parse) was also probed:
reads before the table come out 0, and reads past it depend on the
libc build.

**Tests:**
- `tests/datetime_v16.rs`:
  - 27 new fixed-corpus cases.
  - `end_of_combinations_match_pin`: 10,206 combinations of base
    values, a modifier before, `end of day / month / year` and up to
    two modifiers after, in UTC, with every third case also in New
    York.
  - 12 new statements in `clock_keywords_never_read_a_column`
    (success or failure must agree).
- `hipp-cli/tests/cli_v16_datetime.rs`:
  - `tz_strings_match_pin`: 25 fixed strings, including every
    review repro, and 150 xorshift mutations.
  - `tzif_files_match_pin`: 13 crafted files covering footers, leap
    records, bad indices, flags and the version byte, and 150
    truncated or corrupted real zone files.
  - `rule_less_zones_use_posixrules`: Chicago, Berlin and Sydney as
    `posixrules`, plus the system file.
  - All are byte-compared against the pin under `TZ`.
- `systime` lib tests: `partial_rules_keep_the_offsets` and
  `hostile_files_never_panic` (2,000 corruptions, extreme instants).
- Ad hoc: 3,000 more corrupted files and 1,400 more TZ strings, with
  no differences outside the month-table case below.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **0**: the parser fix is ungated
but the tiny profile does not build the changed paths). Full
**500,184 B** (Δ **0**). Max **794,912 B** (Δ **+104 B** vs 794,808,
ratchet raised): `'end of month'` now goes through `computeJD`.
`systime` is std-only and is not in any size profile. `full`
decision unchanged: the leaf stays in `max` only.

**Honest gaps:**

- A POSIX rule month outside 1..12 that a failed parse leaves
  behind (`M13.1.0`, `M35.2.0`) makes glibc read outside its month
  table. hipp reads 0 there, which matches the pin before the table
  but not past it.
- The first `localtime` conversion of a pin process for a rule-less
  daylight zone (`XST3XDT`) shifts the daylight transitions by the
  zone's daylight offset; hipp gives the result of every later
  conversion. In a TZif footer, a rule-less daylight zone also makes
  glibc replace the zone's file with `posixrules` for later
  conversions; hipp does not.
- At the exact instant of a leap second, glibc's `tm_sec` is 60; the
  offset-only hook cannot say so (the correction itself is applied).
  A file with no types, where glibc reads past its array or asserts,
  is treated as unreadable.

### ATTACH / DETACH and several databases (2026-11-04)

**Shipped** (leaf `attach`, requires `v16`; in `max`, not `full`;
new `src/session/attach.rs`, `Plan::{Att, Attach}`,
`Ast::{Attach, Detach}`, `FromSource::schema`, `SourceDesc::dbname`,
`SubSource::ext`, `SchemaKind::{DatabaseInUse, TooManyAttached,
CannotOpenDatabase, CannotDetach, NoSuchDatabase, DatabaseLocked,
AttachEncoding, CrossDatabaseReference}`):

- *The model.* Each attached database is a `Connection` of the main
  one's storage type, owned by it (`Connection::attached`) — the TEMP
  database's design (`session::temp`) generalised to N databases, not
  a parallel path: a statement is routed before it is planned
  (`route_attach`, ahead of the TEMP router) by the pin's resolution —
  `aux.x` is aux's, an unqualified name temp's, then main's, then each
  attached database's in attach order. A statement whose names all
  live in one attached database runs there with that database's
  qualifiers cut (three-part names included), so DDL, ALTER TABLE
  (all forms), DROP, VACUUM [INTO], REINDEX, ANALYZE, `PRAGMA aux.x`,
  `aux.sqlite_schema`, indexes, views, triggers, foreign keys,
  AUTOINCREMENT, WITHOUT ROWID, STRICT, generated columns and WAL work
  inside it exactly as in main. A write of an attached table that
  reads other databases runs there too, the tables it reads elsewhere
  handed over as *external sources* (`ExtTab`: columns at prepare,
  rows — read in rowid / key order from their own database, VIRTUAL
  columns computed — when it runs; nothing the statement does can
  change those databases, so the rows read first are the rows). Any
  other statement naming an attached object (a query over several
  databases, a write of main reading attached tables, main or TEMP
  CREATE TABLE ... AS, `INSERT INTO temp.x SELECT` via the TEMP feed)
  runs on main, where `ext_source` makes each attached table such a
  source; a three-part `aux.t.c` reaches it by `SourceDesc::dbname`.
- *ATTACH / DETACH.* `ATTACH [DATABASE] expr AS expr [KEY expr]`
  (operands evaluated at step, subqueries allowed; a lone identifier
  is its own text, the pin's `resolveAttachExpr`; NULL / `''` is a
  private temporary database, `':memory:'` an in-memory one; KEY is
  evaluated and ignored), the pin's checks in its order: more than 10
  ("too many attached databases - max 10"), the name against `main`,
  `temp` and the attached names in any case ("database X is already in
  use"), the open ("unable to open database"), then the encoding
  (a non-empty database of another encoding is refused, an empty one
  takes main's). An ATTACH inside a transaction replays BEGIN and the
  open savepoints into the new database. `DETACH [DATABASE] expr`:
  "no such database" (`temp` before the TEMP database exists),
  "cannot detach database main / temp", "database X is locked" when
  the open transaction has read or written it. `Connection::close`
  closes the attached databases (WAL checkpoint and `-wal` removal).
  `PRAGMA database_list` numbers them from 2 as the pin does.
- *Storage.* `Connection::set_attach_hook(FnMut(AttachTarget) ->
  Option<S>)` lets a `no_std` caller supply storage for a name; without
  a hook `Storage::open_attached` is asked (new trait method, default
  none): `MemStorage` makes `:memory:` / temporary databases,
  `FileStorage` opens files relative to the working directory, as the
  pin does, and backs `:memory:` / `''` with a private temporary file
  (`FileStorage::anonymous`, removed when dropped; `is_memory` for the
  former, so its journal mode is `memory`). A file already open on the
  connection (main or attached) is refused (`Unsupported`): two pagers
  over one file would cache each other's pages stale.
- *Transactions and the super-journal.* BEGIN / COMMIT / ROLLBACK /
  SAVEPOINT / RELEASE / ROLLBACK TO apply to every database (as to the
  TEMP one); a failure that ends one database's transaction (OR
  ROLLBACK, an I/O-class error) ends them all. A COMMIT (or the RELEASE
  that commits) first checks every attached database's deferred
  foreign keys; when two or more rollback-journal databases with a
  file name (not WAL, not `:memory:`, synchronous not OFF — the pin's
  `aMJNeeded`) have written and main has a file name, it runs the
  pin's protocol (`commit_all`, `Pager::commit_phase_one`): the
  super-journal `<main>-mjXXXXXX9XX` (six hex digits, `9`, two more,
  re-drawn while the name exists) holding each journal's full name
  NUL-terminated, synced; in each journal, at the next sector boundary
  (synchronous FULL), the pointer record — lock-byte page number,
  name, length, the pin's signed-byte checksum, journal magic — after
  which the journal is truncated there, synced, and the database
  written and synced (phase one); deleting the super-journal is the
  commit point; then each journal is deleted (phase two). Otherwise
  each database commits on its own, as in the pin's simple case.
  Super-journals are opened through the new `Storage::open_named`
  (default none: such storage writes none). Ungated within `v16`
  (a correctness fix on the open path): hot-journal recovery reads the
  pointer (the pin's `readSuperJournal`) — a journal whose
  super-journal is gone belongs to a committed transaction and is
  deleted unplayed; one whose super-journal exists is rolled back and
  the super-journal deleted once no journal it lists points at it
  (`pager_delsuper`). (Before, hipp rolled back every hot journal, so
  a pin-made multi-database commit cut between its commit point and
  the journal deletions came back half committed.)
- *Cross-database rules.* A view or trigger of one database may not
  name another's object with a qualifier ("view / trigger X cannot
  reference objects in database Y", `CrossDatabaseReference`); its
  unqualified names bind to its own database (a main view over a name
  only an attached database has fails when read, "no such table:
  main.t"); `CREATE INDEX aux.i ON aux.t` is the pin's syntax error;
  an unqualified CREATE INDEX / TRIGGER is main's. A table-valued
  pragma's qualifier is ignored (`aux.pragma_table_info('t')` is
  `pragma_table_info('t')`), its schema argument chooses the
  database, and an unqualified argument follows the search order.
  `PRAGMA table_list` lists every database (an attached one under its
  own name); connection-wide pragmas (`foreign_keys`, `encoding`,
  `collation_list`, ...) under an attached name set the connection's
  and are copied into each attached database (`sync_att`).
- *hipp-cli*: `.databases` (`name: file r/w`, `""` without a file),
  `.schema` and `.tables` list attached objects as `name.x` (quoted
  as the shell quotes a name).
- *Parser* (with `attach` only): any schema qualifier parses (the
  router checks it); without the leaf a qualifier other than main /
  temp stays a parse refusal and ATTACH / DETACH stay
  `UnsupportedStatement`.

**Oracle-probing notes** (live 3.54.1 pin, scripts over stdin; the
multi-database commit traced with `strace`): an attached name is
compared without case and may be any text (`'w x'`, `7`, `''`, the
result of `'x' || 'y'` or a subquery); `ATTACH NULL AS n` is a
temporary database, `AS NULL` the name `''`; the limit is checked
before the name (`ATTACH 'x' AS ''` with ten attached: "too many");
the same file may be attached twice; `ATTACH a AS b` attaches the file
`a`, `AS c || 'd'` is "no such column: c". Detaching inside a
transaction works for an untouched database and is "database X is
locked" once read or written. `CREATE INDEX ix ON u` where only an
attached `u` exists: "no such table: main.u"; `DROP TABLE u` drops
the attached one; `DROP TABLE IF EXISTS nosuch.t` succeeds;
`CREATE TABLE nosuch.t`, `PRAGMA nosuch.x`, `VACUUM nosuch` are
"unknown database"; reads of `nosuch.t` "no such table: nosuch.t".
Stored texts drop the created name's qualifier (`CREATE VIEW v AS
SELECT * FROM a.t` keeps the body's; a trigger keeps `ON a.t`). A
TEMP trigger may sit on an attached table. The pin's commit: journals
written first; super-journal created `O_EXCL`, names written and
fsynced; per database the pointer record at the sector boundary
(page number `PENDING_BYTE / page_size + 1`), the journal fsynced, its
header rewritten with the record count and fsynced again, the
database written and fsynced; the super-journal unlinked; then the
journals. Killing the pin (strace fault injection) on entry to each
write, fsync, truncate and unlink of that commit, and recovering:
before the unlink of the super-journal everything rolls back, after
it everything is committed.

**Tests:** `tests/attach_v16.rs` (14 tests) — hand corpora for
ATTACH / DETACH (names, limits, errors, `:memory:` / temporary,
inside transactions and savepoints), name resolution (search order,
unknown schemas, three-part names, `x IN aux.t`, NATURAL / USING,
schema tables), schema objects in attached databases (AUTOINCREMENT,
WITHOUT ROWID, STRICT, generated columns, views, every ALTER form,
triggers and the cross-database rules, foreign keys, VACUUM [INTO],
REINDEX, ANALYZE, integrity checks), schema-scoped pragmas and
table-valued pragmas (incl. a WAL attached database), statements
across databases (joins, compounds, CTEs, correlated subqueries,
INSERT ... SELECT both ways, UPDATE / DELETE reading other databases,
UPDATE ... FROM, CTAS in main / aux / TEMP, upsert, RETURNING),
transactions (savepoints, OR ROLLBACK / OR FAIL across databases,
deferred foreign keys at COMMIT, DETACH locks), encodings (`utf16`),
files interoperating both ways, the `no_std` hook over a `MemStorage`
registry, the same-file refusal, the documented refusals, a generated
corpus of random three-database scripts (xorshift64: writes, reads,
transactions, savepoints, DETACH / re-ATTACH; 24 by default,
`HIPP_ATTACH_CORPUS=400` ran clean) and the regressions below; every
script runs on both engines in twin directories and compares rows,
failing statements and the pin's integrity check and `.dump` of every
database file. `tests/crash.rs`: `v16_crash_multi_database_commit`,
`v16_crash_three_databases_and_savepoints`,
`v16_crash_attached_only_commit` (a storage over a shared in-memory
file system dies at every mutating op of a multi-database commit;
hipp recovers every database to all-pre or all-post, bytes included,
and the same crashed files written to disk are recovered alike by
the real pin, integrity checks included) and
`v16_cross_tool_hipp_recovers_pin_super_journal` (the pin killed on
entry to each pwrite / fsync / ftruncate / unlink of its own
multi-database commit; hipp recovers in place exactly as the pin does;
skipped where `strace` is missing). `hipp-cli/tests/cli_v16_attach.rs`:
byte-identical batches one-shot and over stdin (`PRAGMA
database_list`, cross-database statements) and a `.databases` /
`.schema` script. `tests/query_shape_v16.rs`: `foo.s` is "no such
table" with the leaf.

**Regressions found and fixed while building it:** a table-valued
pragma under an attached qualifier was routed to that database (it
listed only its tables); a WITHOUT ROWID table read from another
database was refused; connection-wide pragmas under an attached name
were "unknown database"; `PRAGMA aux.table_list` named the database
`main`; a RELEASE that committed several databases through the
super-journal left the attached ones inside a transaction (their
savepoints had gone with phase two), so their later autocommit writes
were never committed; `DROP ... IF EXISTS nosuch.x` failed in the
parameter path; `aux.t.c` in a write of aux that read main was not
cut; `SELECT main.t.x FROM aux.t` answered instead of "no such
column"; a main view's names reached attached tables.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **0**: the super-journal recovery is
gated on `v16`, see the gaps). Full **500,184 B** (Δ **+1,304 B** vs
498,880, ratchet raised; under 512 KiB): the super-journal-aware
hot-journal recovery (about 1,480 B, a correctness fix on the open
path; the rest of the unit compiled `full` 176 B smaller). Max
**794,808 B** (Δ **+29,464 B** vs 765,344, ratchet raised; under the
32 KiB justification line): the router, external sources, ATTACH /
DETACH, the commit protocol and the attached connections' recursion
points (boxed futures).

**Honest gaps:**

- External sources are read whole, in rowid / key order, with no
  index access path: an unordered query whose pin plan walks an index
  of an attached table (or an `INSERT ... SELECT` without ORDER BY
  feeding one) can list rows in another order. `INDEXED BY` on such a
  source is not checked there.
- A correlated subquery (or a join) over another database's table
  scans that table's rows once per evaluation; since the review fix
  they are read from the file once per execution and kept, but the
  scan stays linear per outer row (the review's 3000 x 3000 lookup:
  about 10 s in a debug build, where the pin seeks its rowid).
- Refused (`Unsupported`) where the pin answers: an attached database's
  view read together with another database's table; a TEMP trigger on
  a main or attached table (the TEMP unit's gap), or one writing a
  schema-qualified table (`INSERT INTO main.m ...` / `a.t`: a trigger
  program runs inside the database that fires it, with no router to
  the others); a TEMP UPDATE / DELETE / INSERT VALUES reading
  another database (only `INSERT ... SELECT` / CTAS feed the temp
  database); a three-part name of another database inside a write of
  an attached table; an unqualified name meaning a table of another
  database in a statement that also names the same-named table of the
  attached database it writes; attaching a file the connection already
  has open.
- An unqualified `PRAGMA integrity_check` / `quick_check` /
  `foreign_key_check` checks main only (the pin checks every
  database); `PRAGMA aux.x` checks aux.
- A string-literal schema qualifier (`'aux'.t`) does not parse
  (`"aux".t` does).
- A super-journal whose writing was cut before any journal pointed at
  it stays behind (as on the pin). Below `v16` (tiny, the lower
  rungs) recovery still rolls back every hot journal, so a pin-made
  multi-database commit cut after its commit point would come back
  half committed there; those builds have no storage that names
  files (`open_named`) of their own.
- `PRAGMA table_list` right after an AUTOINCREMENT table is created
  lists `sqlite_sequence` before it, where the pin (in that session)
  lists the table first (pre-existing hash-order difference, main
  included; once the file is reopened the pin's order is hipp's).
- Outside a transaction a plain `ANALYZE` / `REINDEX` commits each
  database on its own (the pin's one statement commits them together
  through the super-journal).
- ATTACH of a file that does not exist leaves a 4,096-byte empty
  database where the pin leaves a 0-byte file: hipp's `Pager::open`
  writes page 1 of every empty file it opens (main included, in every
  profile — the tiny profile's open path). The file is valid and reads
  the same on both engines (`PRAGMA aux.page_count` is 0 on both).
- `PRAGMA aux.foreign_key_list(t)` inside a transaction locks aux
  against DETACH whether or not `t` has foreign keys (the pin only
  when it has some).

### ATTACH / DETACH — review fixes (2026-11-04)

Nine findings; seven fixed with regressions
(`tests/attach_v16.rs`: `review_regressions`, `bound_attach_operands`,
`documented_refusals`; `hipp-cli/tests/cli_v16_attach.rs`:
`review_regressions`, `super_journal_directory_syncs`), one narrowed to
a clean refusal, one recorded as a gap.

- *Bound parameters in ATTACH / DETACH* evaluated as NULL (`bind_ast`
  did not visit their operands), so `ATTACH :f AS :n` attached a
  private temporary database named `''`. The operands (and `KEY`) are
  bound now; re-executing after `reset` with new bindings attaches
  again.
- *Cross-database rule for views and triggers* (the pin's `DbFixer`).
  It is now one AST check (`foreign_names`): every FROM / target name
  of the statement, table-valued functions included (new
  `temp::Kind::Func`), must carry no qualifier but its own database's;
  column qualifiers and three-part column names are not checked, as on
  the pin. A trigger's WHEN clause (as `SELECT 1 WHERE ...`) and each
  body statement are parsed and checked (the WHEN clause was not: a
  trigger in `a` reading `main.t` there was stored and then read a's
  own `t` when it fired). Main triggers are checked too (their body and
  WHEN clause could name attached tables), and so are main views before
  the router's nothing-attached shortcut (`CREATE VIEW v AS SELECT *
  FROM a.pragma_table_info('t')` was stored). Two over-refusals went
  with the old token scan: `SELECT main.t.x FROM t` in a trigger of
  `a`, and `SELECT a.x FROM t AS a` in a main trigger, are accepted as
  on the pin. Same pattern without `attach` (ungated within `v16` +
  `triggers`): a `main.`-named trigger's body / WHEN clause naming a
  `temp.` object was stored; it is `TriggerTempReference` now.
- *Plain ANALYZE skipped attached databases* (no `a.sqlite_stat1`).
  `ANALYZE` without a name, `REINDEX` without one and `REINDEX
  <collation>` now run on every attached database after main
  (`maint_attached`; TEMP stays skipped by ANALYZE, as on the pin).
- *Own-database three-part names in stored objects*: a view or trigger
  of `a` writing `a.t.x` failed with "no such column" when read or
  fired (so a trigger made its table unwritable). An attached
  connection's own sources answer to its alias
  (`SourceDesc::dbname`, `Connection::own_dbname`).
- *TEMP triggers writing qualified tables* were reported as the pin's
  "qualified table names are not allowed ..." error, which the pin
  applies only to the other databases' triggers. They are refused as
  `Unsupported` now; running them needs trigger programs dispatched
  through the top-level router (TEMP triggers on main tables are the
  TEMP unit's existing refusal). README no longer says "everywhere the
  pin takes them".
- *DETACH locks*: reading an attached database through a table-valued
  pragma (schema argument, an object found there, or `pragma_table_list`
  / `PRAGMA table_list`) inside a transaction marks it used
  (`Attached::used` is a `Cell` now: the pragma source is planned
  through `&self`). The reverse, probed alongside: `PRAGMA
  aux.page_size` / `journal_mode` / `cache_size` / `synchronous` /
  `auto_vacuum` / `journal_size_limit` / `wal_checkpoint` read no page
  on the pin and no longer lock (`Prag::no_lock`, `OnRoute::lock`).
- *Directory syncs* (`FileStorage`, std only, so no profile size): a
  sidecar this handle created syncs its directory on its first sync —
  the rollback journal, the super-journal and the WAL, as the pin's
  `unixSync` with `UNIXFILE_DIRSYNC` — and a file opened by name
  (`open_named`: the super-journal) syncs its directory after its
  deletion, the multi-database commit point (`unixDelete(..,
  dirSync=1)`); the super-journal is created `O_EXCL`. Traced: hipp's
  two-database commit now shows the pin's directory fsyncs.
- *Performance* (narrowed): on main, an external source's rows are read
  from their database once per execution and kept until `reset`
  (`scan_ext`, `forget_ext_rows`); the review's 3000-row correlated
  lookup went from about 20 s to 10 s (debug). The remaining
  per-evaluation scan is in the gaps above; TEMP tables read by main
  (`scan_temp`) have the same per-evaluation read (pre-existing, TEMP
  unit).
- *Not fixed*: the 4,096-byte file an ATTACH of a new name leaves (see
  the gaps above: the core's open path, main included).

**Found along the way, not this unit's** (pre-existing, left as they
were): the pin refuses a view over a table-valued pragma when read
("unsafe use of virtual table"), hipp reads it; `PRAGMA cache_size`
reports 16 where the pin reports -2000; hipp-cli's `.parameter set`
does not create the TEMP database as the pin's does; hipp bumps the
file change counter on writes the pin skips (an ANALYZE that changes
nothing, REINDEX).

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **0**). Full **500,200 B** (Δ **+16 B**
vs 500,184, ratchet raised): the ungated fixes in the TEMP router (a
`main.`-named trigger's `temp.` references, the TEMP trigger refusal);
the rest is gated on `attach` (`Kind::Func` included). Max **815,328
B** (Δ **+1,400 B** vs 813,928, ratchet raised): the AST cross-database
check, ANALYZE / REINDEX over attached databases, the lock flag, the
row cache. (A first cut instantiated the parser over `()` for the
trigger check: +44 KiB; it uses the storage's error type now.)

### Auto-vacuum files (2026-11-04)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated (leaf `autovacuum`, implies `v16`, in `max` —
ADR 0002 — not `full`; its own `v16,autovacuum` line in
`ci/feature-sets.txt`). Without the leaf today's refusal stands
unchanged: an auto-vacuum file is read, every write is refused
with `Error::AutoVacuumUnsupported` and leaves the file
byte-identical (pinned by the `pager` unit test
`auto_vacuum_file_refused_without_leaf`, run by the gates'
`--features full` lib tests, and the existing suites' tests,
now `cfg(not(feature = "autovacuum"))`).

**Shipped (`src/autovac.rs`, `autovacuum`):**

- **Pointer maps, derived.** The pin keeps its map with
  `ptrmapPut` calls along every write path; hipp's B-tree code
  rebuilds whole pages, so the map is instead derived: at each
  commit of an auto-vacuum file (`Pager::drive_commit` →
  `autovac::pre_commit`) every tree named by `sqlite_schema`
  (rowid tables, indexes, WITHOUT ROWID and expression-index trees
  alike — the walk reads raw pages), every overflow chain and the
  freelist are walked into a `Map` (role, parent per page:
  ROOTPAGE / FREEPAGE / OVERFLOW1 / OVERFLOW2 / BTREE), and the
  pointer-map pages whose bytes differ are rewritten. The map is a
  function of the file's structure, so the entries are the pin's
  whatever path wrote the pages — inserts, deletes, balance splits
  and merges, overflow chains made and freed, freelist trunk / leaf
  changes, index, WITHOUT ROWID and expression-index writes,
  REINDEX, ALTER, triggers, foreign-key actions. A page nothing
  references, or one referenced twice, is corruption
  (`CorruptPage`), never a guessed entry. Entries past the end of
  the file are left as they are, as on the pin. Pointer-map pages
  sit at the pin's positions (`ptrmapPageno`, the lock-byte page
  skipped) for every page size; growing the file over one makes it
  a zeroed pointer-map page and allocates the next
  (`Pager::grow_page`, the pin's `allocateBtreePage` at the end).
- **Root placement.** `btree::alloc_root` (every CREATE TABLE /
  INDEX / automatic index / `sqlite_sequence` / `sqlite_stat1` /
  CREATE TABLE AS root, and VACUUM's) is, on an auto-vacuum file,
  the pin's `btreeCreateTable`: the page after header offset 52
  (the largest root), skipping pointer-map and lock-byte pages,
  taken with `BTALLOC_EXACT` (searching the freelist when the map
  says it is free); a B-tree or overflow page there is moved out of
  the way first (`relocatePage` / `modifyPagePointer`), then offset
  52 is updated.
- **Drop.** DROP TABLE / DROP INDEX on an auto-vacuum file follow
  `sqlite3CodeDropTable` / `destroyRootPage` / `btreeDropTable`:
  the `sqlite_sequence` row and the schema rows first, then each
  tree largest root first — its pages freed in `clearDatabasePage`
  order (each cell's subtree, its overflow chain, the right
  subtree), the root freed if it is the largest, else the largest
  root copied into its slot, freed, and its `sqlite_schema` row
  pointed at the slot (`autovac::schema_set_root`); offset 52
  decremented past pointer-map pages.
- **FULL** (`autoVacuumCommit`): at commit, `finalDbSize`, then
  `incrVacuumStep(commit)` from the last page down — free pages
  dropped, used pages moved into the free pages the pin's
  `BTALLOC_ANY` allocation hands out (trunk leaf 0 swapped with the
  last, an empty trunk itself) — then the freelist emptied and the
  file truncated. **INCREMENTAL:** commits keep free pages; `PRAGMA
  incremental_vacuum[(N)]` runs N of the pin's
  `sqlite3BtreeIncrVacuum` steps (`BTALLOC_EXACT` for a free last
  page, `BTALLOC_LE` below the final size otherwise) and returns
  one zero-column row per page removed; N is read with
  `sqlite3GetInt32` (`'2abc'` is 2, `' 2'`, `9999999999`, `''`,
  `0`, negatives and no argument mean every page). It works on a
  FULL file inside a transaction too; on a plain or the temp
  database it does nothing; `pragma_incremental_vacuum` is "no such
  table", as on the pin.
- **Truncation is crash- and savepoint-safe.** Pages cut off are
  dropped from the commit's write set. (As first shipped, every
  page past the new end was journaled and sub-journaled up front;
  the review fixes below replace that with the pin's scheme:
  truncation after the journal's deletion, no record for a cut
  page unless it is reused.)
- **PRAGMA auto_vacuum.** The mode is always kept for the next
  VACUUM; an empty database takes FULL / INCREMENTAL at once
  (header offsets 52 = 1 and 64 written and committed, so the page
  size is fixed from then on, as the pin's pragma creates the
  file); an auto-vacuum file switches between FULL and INCREMENTAL
  in place (offset 64); to or from NONE waits for VACUUM. The
  connection's mode (`Pager::av_mode`) is the pin's in-memory one:
  it survives ROLLBACK / ROLLBACK TO, and the next write puts it
  back into the header (`ensure_header`, the pin's `newDatabase`).
  The getter reports it.
- **VACUUM / VACUUM INTO** build the image as an auto-vacuum
  database when the file is one (or `auto_vacuum` was set first):
  roots in the pin's order after the largest, pointer-map pages
  skipped, the map written at the image's commit; offsets 52 / 64
  kept. `auto_vacuum = NONE; VACUUM` still converts to a plain file,
  `= FULL; VACUUM` converts a plain one.
- **WAL, savepoints, ATTACH.** The upkeep runs in `drive_commit`,
  so WAL commits (the commit frame carries the truncated size; the
  checkpoint truncates the file), RELEASE / COMMIT of savepoints and
  each attached database's commit (its own pager) all get it.

**Ungated fix:** `Pager::free_page` filled a freelist trunk to its
capacity (`usable/4 - 2` leaves); the pin's `freePage2` stops at
`usable/4 - 8` (so files stay readable by SQLite before 3.6.0).
hipp's trunks, and so the pages a FULL commit or
incremental_vacuum moves into, now match the pin's
(`freelist_trunk_capacity`). Tiny: no change in size (one constant).

**Ungated fix (found by the generated corpus):** when a delete left
page 1 — `sqlite_schema`'s root — an interior page with no cell,
`collapse_table_root` copied the child into it without checking that
it fits beside page 1's 100-byte header; a full child's cells were
packed over the pointer array, and the pin then found the schema
malformed. A 512-byte-page file whose schema had two leaves was
corrupted by a DROP TABLE or ALTER TABLE ADD COLUMN (plain files
too, at HEAD before this unit). Now a child that does not fit stays
under a cell-less root, as the pin's `balance_nonroot` does (it
shrinks only when `hdrOffset <= nFree`), so the pin's page 1 is
matched (`page1_root_keeps_an_oversized_child`).

**Oracle-probing notes (live pin):** `PRAGMA auto_vacuum = FULL`
alone on a new file writes a 1-page file with offset 52 = 1;
offset 52 is the largest root page (3 for the first table: page 2
is the pointer map). Values: `none` / `full` / `incremental`, 0 /
1 / 2, `'full'`, `"FULL"`; `1.5` is 1 (atoi); 3, -1, 7 and `abc`
are 0. `auto_vacuum = 1` then `page_size = 1024`: the size stays
4096; the reverse order applies both. After `user_version = 5` the
mode cannot change. `BEGIN; auto_vacuum = 1; ROLLBACK` leaves an
empty file but the getter says 1, and the next CREATE makes the
file auto-vacuum (likewise `SAVEPOINT` / `ROLLBACK TO`). On an
auto-vacuum file `= 0` keeps the getter's value (the change waits
for this connection's VACUUM); FULL <-> INCREMENTAL is immediate.
VACUUM INTO keeps the mode (or the pending one). DROP TABLE of a
table and its UNIQUE index (roots 4, 5, largest 7): 7 moves to 5,
6 to 4, freelist trunk 7 with leaf 6, offset 52 = 5. CREATE TABLE
after data moves the leaf at the slot to the end of the file.
`incremental_vacuum(N)` prints one empty line per page in list
mode; `PRAGMA temp.incremental_vacuum`
and a plain file print nothing. On a FULL file freelist_count is 0
after every commit; inside a transaction it counts the free pages
and `incremental_vacuum` removes them. WAL + FULL truncates
logically at commit and physically at the checkpoint. Page
choice: hipp writes a row's table cell before its index entries
(the pin the reverse) and splits some index pages differently, so
files with indexes written row by row can place pages
differently (as plain files already do); without that the pin's page counts, pointer maps and root pages are
matched page for page (`layouts_match_the_pin`).

**Tests:** `tests/autovacuum_v16.rs` (live pin, one connection per
script, rows and failing statements compared; both integrity
checks; layouts — page count, freelist count, offsets 52 / 64 and
every pointer-map entry — compared where page choice is
deterministic): `layouts_match_the_pin` (roots, drops moving roots
of tables / indexes / `sqlite_sequence`, CREATE moving a leaf,
overflow chains, FULL truncation, incremental, incremental DROP),
`page_sizes_and_map_pages` (512 … 65536, maps over several
pointer-map pages, both modes), `pragma_auto_vacuum` (22 scripts:
values, empty database, switches, ROLLBACK / ROLLBACK TO keeping
the mode), `incremental_vacuum_arguments` (18 argument forms plus
temp / plain / FULL-in-a-transaction / table-valued / ROLLBACK /
savepoint cases), `vacuum_of_auto_vacuum_files` (VACUUM keeping /
setting / dropping / switching the mode, a page-size change, 5
VACUUM INTO cases), `files_cross_both_ways` (pin files written by
hipp and hipp files written by the pin, both modes, two page
sizes), `transactions_and_wal` (ROLLBACK, nested savepoints, a
statement rollback, WAL in both modes and a journal-mode switch),
`integrity_check_of_damaged_maps` (7 damaged pin files: the pin's
messages and limits on hipp), `freelist_trunk_capacity` (the
regression), and generated corpora `generated_corpus_journal` (40
scripts) / `generated_corpus_wal` (20; `HIPP_AV_CORPUS` scales
both) of random CREATE / INSERT / DELETE / UPDATE / DROP /
incremental_vacuum / VACUUM / BEGIN ... COMMIT|ROLLBACK / SAVEPOINT
scripts on FULL and INCREMENTAL files at 512, 1024 and 4096-byte
pages (also expression indexes, WITHOUT ROWID tables, CREATE TABLE
AS, ALTER TABLE ADD COLUMN, DROP INDEX, REINDEX and ANALYZE): rows
and errors equal, FULL never leaves a free page after a commit,
both integrity checks pass (500 scripts run clean with
`HIPP_AV_CORPUS=250`). `page1_root_keeps_an_oversized_child` (the
page-1 regression, plain and both modes). `tests/crash.rs`:
`v16_crash_autovacuum_full` (a FULL delete commit's page moves and
truncation, a transaction freeing and reusing pages, DROP INDEX
moving a root, CREATE moving a page) and
`v16_crash_incremental_vacuum` (`incremental_vacuum(7)`, all, and
inside a transaction with inserts): every kill point recovers in
hipp to the pre- or post-image byte for byte, and the pin, given
the crashed file and journal, recovers to the pre- or post-rows
with its integrity check ok. `hipp-cli/tests/cli_v16_autovacuum.rs`
(4 tests: FULL, INCREMENTAL and pragma / VACUUM batches
byte-identical one-shot and over stdin; a pin-written file written
by hipp's shell).

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh` run
(the 3.54.1 pin live) ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **0**: the root allocation is a
macro, so builds without the leaf pay no extra future; the two
ungated fixes compiled to the same size). Full **500,184 B** (Δ
**0**; the leaf is not in `full`). Max **813,928 B** (Δ **+19,016**
vs 794,912, ratchet raised; under the 32 KiB justification
threshold): `src/autovac.rs` (derive, write, allocate, relocate,
FULL commit, incremental steps, root placement, drop), the pager's
growth / tail keeping and the PRAGMA / VACUUM / DROP wiring.

**Honest gaps:**

- **Commit cost.** The map is derived by walking the whole file at
  every commit of an auto-vacuum file (and at each CREATE and
  `incremental_vacuum`): correct by construction, but O(pages) per
  commit where the pin's upkeep is O(pages changed). Plain files
  pay nothing.
- **Page choice where hipp's B-tree differs.** hipp's general
  allocation (`alloc_page` takes a trunk's last leaf; the pin's
  callers pass `nearby` hints), its table-before-index insert order
  and its index split policy are unchanged, so after such writes
  files can differ from the pin's page by page (same contents,
  valid maps, both integrity checks pass). The moves this unit
  owns (root placement, DROP, FULL commit, incremental_vacuum) use
  the pin's allocator and match given the same freelist.
- The byte content of free pages and of pointer-map entries past
  the end of the file is not the pin's (never read by either
  engine).
- `PRAGMA auto_vacuum` on the TEMP database and the pin's
  `sqlite3_autovacuum_pages` callback (no SQL surface) are not
  modelled.

### Auto-vacuum files — review fixes (2026-11-04)

Four review findings on the unit above; all four addressed.

- **Cut-off pages were journaled (fixed).** `Pager::keep_tail`
  wrote every page past the new end of the file to the rollback
  journal and the savepoint sub-journal before a FULL commit or a
  `PRAGMA incremental_vacuum` step, free leaves included, so the
  journal grew as large as the freed data. A FULL `DROP TABLE` of
  a 12 MB table wrote 3,013 page images, against 10 on the pin.
  Probed on the pin with `strace`: it `unlink`s the journal and
  only then `ftruncate`s the file (its `pager_end_transaction`),
  and fetches free leaves without their content. So until the
  journal is gone, the cut-off pages are still on disk for a
  rollback, and none of them needs a record. hipp now does the
  same. The commit's `SyncData` stage only grows an auto-vacuum
  file; it is shrunk after `DeleteJournal`, best effort (the
  commit point has passed, and a longer file is valid: its header
  gives the page count, as the pin's does after a crash there).
  `keep_tail` is gone. The pages a truncation cut off keep their
  frames, and they are recorded only when they are written again.
  That happens when the file grows back into them
  (`Pager::grow_frame`): they then take a journal record (if
  original) and a savepoint record before they are zeroed.
  `autovac::allocate` no longer makes a freelist leaf writable
  itself: incremental_vacuum's `BTALLOC_EXACT` leaf is cut, not
  written, and the other callers make it writable themselves.
  Savepoints now allow for a file that shrinks mid-transaction.
  `sp_note` records a page within *any* open savepoint's size (the
  pin's `subjRequiresPage`), not only the newest one's.
  `sp_rollback` keeps, and restores, the pages an older savepoint
  still covers. A page that is journaled but has left the write
  set has its image copied to the sub-journal. These rules matter
  only when the file shrinks, so they are gated by the leaf. Now
  the review's DROP TABLE and incremental_vacuum cases each write 7
  page images in all, journal and database together (3,015 and
  3,011 before). The incremental_vacuum file came out
  byte-identical to the pin's. Regressions:
  `tests/crash.rs` `v16_crash_autovacuum_cut_pages_not_journaled`
  (a FULL DROP of ~300 pages and an incremental_vacuum of ~300 free
  pages each take fewer than 60 storage operations, where three
  per page were needed before; crash matrices of those, of a
  transaction that grows back into the pages it cut, and of a FULL
  delete whose commit moves pages: every kill point recovers in
  hipp to the pre-image, or to the post-image possibly followed by
  the not-yet-truncated tail, and the pin recovers the same files
  with integrity_check ok). The crash matrices accept that tail.
  `tests/autovacuum_v16.rs` `savepoints_across_a_shrinking_file`
  covers ROLLBACK TO a savepoint taken after an incremental_vacuum,
  then one taken before it, with the file grown back in between.
  It fails if `sp_rollback` drops the older savepoint's pages.
- **Found while testing: integrity_check inside a transaction
  (fixed).** hipp writes the derived pointer map only at commit,
  so `BEGIN; DELETE ...; PRAGMA integrity_check` on an auto-vacuum
  file reported "Bad ptr map entry" for every entry the
  transaction had changed. The pin's map is always current, so
  its check says `ok`. Inside an open write transaction, the
  check now writes the derived map first (`autovac::refresh_map`).
  Outside a transaction the committed map is checked as it is, so
  damaged maps are still reported
  (`integrity_check_of_damaged_maps`). Covered by
  `savepoints_across_a_shrinking_file` and the CLI `attached_file`
  batch.
- **Lock-byte page untested (now tested).** `lock_byte_page`
  crafts a sparse 1 GiB file at 64 KiB pages: the pin's page 1 and
  pointer maps, and a table of four rows whose 4,095-page overflow
  chains fill pages 4 to 16384. Each overflow page holds only its
  4-byte link, so about 64 MB is written. Statement by statement on
  both engines, it grows the file past page 16385 (a CREATE moving
  page 4 to 16386, an insert), shrinks it back across that page (a
  FULL `DROP TABLE`; on INCREMENTAL, `incremental_vacuum(2)` and
  then all), and creates a table again. Rows, page counts, freelist
  counts, offsets 52 / 64 and every pointer-map entry match the pin
  after each statement. The lock-byte page's slot holds no entry,
  each file ends at its page count, and the pin's integrity_check
  passes on both files. hipp's own integrity check and `count(*)`
  read all 268 MB of each row's payload, which is too slow for a
  test, so the matching layouts stand in for them.
- **ATTACH interplay untested (now tested).**
  `attached_auto_vacuum_files` runs six scripts with an attached
  file and compares each file's layout with the pin's:
  `aux.auto_vacuum` on an empty attached file; CREATE / CREATE
  INDEX / DROP moving roots in it; `aux.incremental_vacuum(N)` and
  the unqualified pragma (main's, a no-op); a FULL aux inside a
  BEGIN ... COMMIT across main and aux, and a ROLLBACK; both files
  auto-vacuum in different modes with savepoints across them;
  `VACUUM aux`, `VACUUM aux INTO`, `aux.auto_vacuum = 0; VACUUM
  aux`; a plain aux converted by VACUUM. `tests/crash.rs`
  `v16_crash_multi_database_auto_vacuum` (through
  `multi_crash_matrix_with`) covers a FULL attached file truncated
  inside a super-journal commit, and an INCREMENTAL one vacuumed
  inside it. Every kill point recovers all three databases to one
  side, in hipp and in the pin, with integrity_check ok. The CLI
  suite adds `attached_file`, byte-identical one-shot and over
  stdin. The incremental_vacuum steps and the integrity check
  inside a transaction run on an attached file.
- **README overclaimed page-count parity (fixed).** The README
  bullet now says that the moves this leaf owns choose the pin's
  pages, so layouts match given the same tree shapes. Where hipp's
  general B-tree shapes trees differently, page counts can differ.
  Reproduced on the pin: an index plus deleting every second row
  gives 58 pages against 57; `ALTER TABLE DROP COLUMN`, an UPDATE
  resizing blobs, REINDEX and DELETE merging differ too, as the
  honest gaps above say. A plain aux converted by VACUUM and then
  deleted from gives 10 pages against 9: the same cause, so that
  test case uses DROP instead.

**Gates:** `cargo fmt --all`, then the full `ci/local-gates.sh`
run (the 3.54.1 pin live) ends with "==> all gates passed".

**Size:** tiny **76,960 B** (Δ **0**). Full **500,200 B** (Δ
**0**: every new rule is gated by the leaf, and `sp_rollback`'s
limit is a macro so that builds without the leaf keep no extra
local across its awaits — a plain `let` cost 8 B there). Max
**815,792 B** (Δ **+464** vs 815,328, ratchet raised): the deferred
truncation, `grow_frame`, the savepoint rules and `refresh_map`,
less `keep_tail`.

**Honest gaps (added):**

- `Pager::alloc_page`, which every file uses and which predates
  this unit, still journals a freelist leaf that was free before
  the transaction when it reuses it: at most one page per page of
  new data. The pin skips this (it fetches the leaf with
  `NOCONTENT` unless the leaf was freed in the same transaction).
  Unlike the fixed finding, this cost scales with the data written,
  not the data freed. Matching the pin needs a per-transaction
  record of freed pages in the core pager, which the tiny profile
  would pay for, so it is left as is.
- hipp's integrity check and `count(*)` read every overflow page
  of a row (the pin's skip the payload). On very large rows this is
  slow; it is not specific to auto-vacuum files.
- hipp writes page 1 of an empty main database file whenever it
  opens one, even if nothing is ever written to it (`SELECT 1` is
  enough); the pin leaves a 0-byte file. This predates this unit
  and is not specific to it, so the attached tests do not compare
  a main file that the pin left empty.

### Virtual tables, generate_series, dbstat and the R*Tree (2026-11-05)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated: leaf `vtab` (implies `v16`) for the framework,
`generate_series` and `dbstat`; leaf `rtree` (implies `vtab`); both
in `max` (ADR 0002), not `full`; their own `v16,vtab` and
`v16,vtab,rtree` lines in `ci/feature-sets.txt`. Without `vtab` a
file holding a virtual table is refused at open, as before.

**Shipped:**

- **One module abstraction** (`src/vtab.rs`), Rust traits modelled
  on `sqlite3_module`, no C ABI: `Vtab::connect` (xCreate /
  xConnect: declared columns, HIDDEN), `best_index` over an
  `IndexInfo` (aConstraint column / op / usable, aOrderBy; out
  argvIndex / omit, idxNum, idxStr, orderByConsumed, cost, rows),
  `VtCursor` (xFilter / xNext / xEof / xColumn / xRowid),
  `eponymous`, `creatable`, `writable` (xUpdate), `shadow_name`
  (xShadowName). The planner (`session::vtab`): a virtual-table
  FROM source plans like a JSON table-valued function (whose lateral
  machinery it now shares: `SourcePlan::tvf` / `tvf_dead`, the
  per-row argument evaluation); once WHERE is resolved each
  `col op expr` term (`=`, `<`, `<=`, `>`, `>=`, `IS`, either way
  round, a BETWEEN's halves after the other terms), each inner ON
  term and each table-valued-function argument (an `=` on the next
  hidden column) becomes a constraint, usable when it reads only
  enclosing queries and sources to its left; the module's choice
  picks the filter's arguments and omitted terms leave the WHERE.
  Transactions, savepoints and statement rollback: every module
  keeps its state in the file, so the pager's journals are its
  xBegin / xSync / xCommit / xRollback / savepoint hooks.
- **CREATE VIRTUAL TABLE [IF NOT EXISTS] t USING m[(args)]**: the
  pin's row (`table`, root 0, `CREATE VIRTUAL TABLE ` + the text
  from the name token to the closing parenthesis; empty arguments
  dropped), the name rules (taken, `sqlite_` reserved). DROP TABLE
  deletes it, then (xDestroy) an R*Tree's `%_node`, `%_rowid`,
  `%_parent` (auto-vacuum files too). A file holding a virtual
  table of a module hipp lacks opens: the table is listed (PRAGMA
  table_list `virtual`), statements touching it are "no such
  module", and its shadow tables (any `<t>_*` table: hipp cannot
  ask the module) are listed read only — not read when hipp's
  grammar cannot parse their text (fts3/4 quote names as strings).
  Shadow tables refuse INSERT / UPDATE / DELETE / DROP / ALTER /
  CREATE TRIGGER (the shell's defensive mode); virtual tables
  refuse CREATE INDEX / TRIGGER and ALTER except an R*Tree's
  RENAME TO (its shadow tables renamed, the pin's quoted text).
  PRAGMA table_info / table_xinfo (hidden columns) / table_list
  (`virtual`, `shadow`); `.schema` prints the pin's `/* t(cols) */`
  line and `.dump` the pin's `writable_schema` rows.
- **generate_series** (the shell's ext/misc/series.c): eponymous
  only (CREATE ... USING it is "no such module"), hidden `start` /
  `stop` / `step`, its `seriesBestIndex` (start / stop / step `=`;
  `value` / rowid `=` / `IS` / `>` / `>=` / `<` / `<=`, the last of
  each kind used and omitted; the "first argument ... missing or
  unusable" error, also when the start reads a source the pin's
  no-prerequisite plan cannot use), `seriesFilter`'s 64-bit
  arithmetic (wrap-safe spans, REAL bounds rounded inward, NULL
  argument → no rows, step 0 → 1, `i64::MIN` steps).
- **dbstat** (`SQLITE_ENABLE_DBSTAT_VTAB`): every column of
  `statColumn` for the main database (`sqlite_schema` then each tree
  in schema order, or name order when ORDER BY name), `statNext`'s
  walk (page, its cells' overflow pages, children, right child;
  `/000/` and `/000+000000` paths), `unused` / `payload` /
  `mx_payload` as `statDecodePage` and `getLocalPayload` compute
  them, `aggregate = 1` rows, `schema` / `name` constraints,
  `dbstat(schema, aggregate)`, and `CREATE VIRTUAL TABLE ... USING
  dbstat`.
- **R*Tree** (`src/rtree.rs`, `session::rtree`): `rtree` /
  `rtree_i32`, 1-5 dimensions, up to 100 `+aux` columns, the
  argument errors; the shadow tables with the pin's text, the root
  node of `min(page_size - 64, 4 + 51 × cell)` bytes. The tree code
  is the pin's: ChooseLeaf, AdjustTree, the R*-tree
  `splitNodeStartree` (margin-then-overlap-then-area, the pin's merge
  sorts), root splits numbering the right node first, delete with
  under-full nodes removed and their cells reinserted (latest
  removed first), the root collapsed, nodes numbered after the
  largest `%_node` key. Writes are a pure function over a `Store`
  of the shadow rows read so far (a `Miss` reads one more and the
  write runs again); its result goes through the pager in the
  statement's transaction. Coordinates round outward to float32
  (`rtreeValueDown` / `Up`); `rtree_i32` clamps to 32 bits; the id
  is `sqlite3_value_int64` of the value; no affinity on any value.
  "rtree constraint failed" and UNIQUE under OR ABORT / FAIL /
  IGNORE / REPLACE / ROLLBACK as `rtreeUpdate` resolves them; UPDATE
  in scan order, DELETE in rowid order (the pin's RowSet). Reads:
  `rtreeBestIndex` (rowid `=` first; else each `=`, `<`, `<=`, `>`,
  `>=` on a coordinate, `<=` / `>=` omitted, 20 at most),
  `rtreeFilter`'s value classes (text → TRUE / FALSE, NULL never,
  ±2^48 integers widen `<` / `>`), the depth-first walk.
  `rtreecheck([schema,] t)` (every message of `rtreeCheckTable`;
  "Schema corrupt or not an rtree" for a narrow table),
  `rtreenode(n, blob)`, `rtreedepth(blob)`.

**Probing notes (live pin):**

- The pin's 3.54.1 R*Tree never forces a reinsertion: every full
  node splits. (`Reinsert` and `iReinsertHeight` exist in rtree.c,
  but its node images match only the split-only model: fitted
  first as "reset to −1 per insert", which disagreed on the first
  leaf overflow, then "0 per connection, only rising", which
  disagreed on the first internal overflow; split-only matches
  every generated corpus.)
- An UPDATE / DELETE of an R*Tree leaves `last_insert_rowid()` at
  the last shadow row the module's internal INSERTs wrote; hipp
  approximates the pin's node-release order (exact for one-node
  trees).
- `dbstat('nosuch')` and `dbstat(NULL)` return no rows, no error;
  an empty (never written) database has no rows; `dbstat` in a view
  is "unsafe use of virtual table" on the pin (hipp runs it).
- `rtreecheck('nosuch')` is "SQL logic error"; `rtreenode(257, b)`
  is NULL (no `u8` truncation of the dimension count).
- The pin shell allows CREATE INDEX on a shadow table (hipp
  refuses: its module writes would not maintain the index; a file
  with one refuses R*Tree writes).
- The pin's shell is built with assertions: `generate_series(1, 10)
  ORDER BY value DESC LIMIT 3` aborts it in `seriesNext`; the tests
  avoid that shape.

**Tests:** `tests/vtab_v16.rs` (CREATE / DROP storage and errors,
unknown-module file, generate_series corpus of 46 queries + a
generated corpus of 120 with random bounds / steps / constraints,
dbstat byte for byte on a pin-written file, PRAGMAs);
`tests/rtree_v16.rs` (CREATE across page sizes, a writes corpus,
rtree_i32, float32 boundaries, 30 query shapes, functions and
errors, transactions / savepoints / every conflict clause, 14
generated 1-5-dimension workloads (`HIPP_RTREE_CORPUS` for more)
compared statement by statement with every shadow row byte for
byte and each engine's rtreecheck / integrity_check run on the
other's file, files moving between engines mid-workload);
`tests/crash.rs` `v16_crash_rtree_writes` (every crash point of
split-heavy transactions, an autocommit DELETE, a failing statement
and a savepoint); `hipp-cli/tests/cli_v16_vtab.rs` (byte-identical
stdout: generate_series, R*Tree reals, `.schema`, dbstat).
Regression tests for the bugs found: the pin's split-only tree
(generated corpora), DELETE in rowid order, OR FAIL / OR ROLLBACK on
"rtree constraint failed", `rtreenode`'s dimension check, the
fresh-database `dbstat`, DROP of an unknown module's table,
`rtreecheck` of a narrow table, the legacy planner's virtual-table
names.

**Gates:** `ci/local-gates.sh` (full) passes: fmt, clippy `-D
warnings` (default, all features, every feature-set line including
`v16,vtab` and `v16,vtab,rtree`), workspace tests, the no_std targets,
the three size gates.

**Size:** tiny **76,960 B** (Δ **0**; the catalog loader keeps
its tiny-profile code path unchanged); full **500,200 B** (Δ **0**:
both leaves are in `max` only); max **874,832 B** (Δ **+59,040 B**,
ratchet raised to it — over the 32 KiB justification threshold:
the module framework, its planner pass and statements ≈ 18 KiB,
generate_series ≈ 3 KiB, dbstat ≈ 6 KiB, the R*Tree with its
shadow-table writes, queries and checks ≈ 32 KiB; 173,744 B of the
1 MiB budget remain).

**Honest gaps:**

- json_each / json_tree and the pragma table-valued functions are
  not moved onto the module traits: they share the planner's
  table-valued machinery (`SourcePlan::tvf`, lateral arguments)
  with virtual tables, and moving them would grow `full`.
- IN lists are not offered to xBestIndex (they filter in WHERE), and
  the pin's OR / IN multi-lookup plans are not made: unordered
  results of `rowid IN (...)` or `a = 1 OR b < 2` over an R*Tree can
  come in tree order where the pin's come in lookup order. ORDER BY
  is always sorted by hipp (orderByConsumed is ignored; same rows);
  LIMIT / OFFSET constraints are not passed to modules.
- A virtual-table source whose arguments read a source to its
  *right* is refused (the pin reorders the join; closed in v0.17:
  the `join_order` planner orders virtual tables by their
  xBestIndex costs); so is one on the
  right of a RIGHT / FULL join whose table-valued arguments read the
  left side (its ON clause no longer counts: review fix).
- `dbstat` inside a view or trigger runs (the pin's "unsafe use of
  virtual table").
- PRAGMA integrity_check does not run the R*Tree's xIntegrity (use
  `rtreecheck()`); `rtreecheck` reads only `main`.
- `last_insert_rowid()` after an R*Tree UPDATE / DELETE of a
  multi-level tree can differ (the pin's internal-INSERT order is
  approximated); a new rowid past 9223372036854775807 is the
  smallest unused one (the pin's is random).
- MATCH (geometry callbacks), UPSERT, RETURNING and UPDATE ... FROM
  on an R*Tree, CREATE INDEX on a shadow table, ALTER TABLE RENAME
  of an R*Tree while the schema has views or triggers, and virtual
  tables in TEMP or attached databases are refused.
- A virtual table of a module hipp lacks has no columns for hipp
  (PRAGMA table_list's `ncol` is 0 where the pin's module declares
  them); its `<t>_*` tables are read only (hipp cannot ask the
  module which are shadows).
- hipp holds a virtual-table cursor's rows in memory (as its
  executor does every source's): more than `Config::max_vtab_rows`
  rows from one cursor fail (`VtabTooManyRows`) where the pin
  streams them — `count(*) from generate_series(1)` is an error,
  not 4294967295. (The executor-wide form — a huge join or
  recursive CTE — is the engine's, not this unit's.)
  (Closed in v0.17 for top-level queries: streamed rows,
  `session::stream`; subqueries and the other materialising paths
  keep the bound.)
- hipp-cli's `.tables` does not list virtual tables.

### Virtual tables and the R*Tree — review fixes (2026-11-05)

**Fixed** (each with a regression test against the live pin):

- **generate_series, descending with `value` bounds** (wrong rows):
  the filter moved the first value down to the upper bound with the
  pin's test inverted (`base < iMax`), so a consumed `value =` /
  `<` / `<=` / BETWEEN returned rows above it
  (`generate_series(100,1,-10) WHERE value < 55` gave 60). A step of
  `i64::MIN` steps by 2^63 (MAX, then -1), as the pin; hipp used
  `i64::MAX`. `tests/vtab_v16.rs` `series_descending_constraints`
  (a corpus and 200 generated descending cases).
- **generate_series out of memory** (process abort): a cursor's
  rows are drained into memory, so `count(*) from
  generate_series(1)` asked for tens of GB. The drain is now bounded
  by the new `Config::max_vtab_rows` (default 2^22) and fallible
  allocation, failing with `SchemaKind::VtabTooManyRows`; a LIMIT
  the module honours stays under it. Streaming the executor is out
  of this unit's scope (honest gap above). `series_row_bound`.
- **Shadow-table name clashes** (corrupt schema): CREATE VIRTUAL
  TABLE now checks every shadow table xCreate makes (an R*Tree's
  three; fts5's `_data` / `_idx` / `_config` and `_content` /
  `_docsize` when kept) — hipp wrote a second `r_node` beside the
  user's, a schema the pin calls malformed. Names a module claims
  (`xShadowName`) are refused to CREATE TABLE / VIEW / INDEX /
  TRIGGER / VIRTUAL TABLE, RENAME TO and TEMP objects
  (`ReservedName`, the pin's "object name reserved for internal
  use"). `shadow_names_are_reserved`.
- **R*Tree node cell counts**: a node claiming more cells than fit is
  `VtabCorrupt` on every read path (search, rowid lookup), as the
  pin's nodeAcquire; the search clamped it and returned stale cells
  (118 rows of a 100-row table). `tests/rtree_v16.rs` `review_fixes`.
- **Duplicate R*Tree column names** (`rtree(id,a,a)`, `+a` beside
  `a`, any case): `DuplicateColumn`, after the argument-count errors,
  as `sqlite3_declare_vtab`; hipp created tables the pin cannot read.
- **RIGHT JOIN of a virtual table**: the ON terms of a RIGHT / FULL
  join stay join conditions (they were offered to xBestIndex, which
  made the source lateral and refused). `right_join_of_virtual_tables`.
- **Unknown-module files**: the CREATE TABLE grammar takes a string
  literal as a column name under `vtab` (the pin's `nm`; fts3 / fts4
  declare `'c0x'`), and a string-literal table name under `vtab` (it
  was `fts5` only). So their shadow tables read, VACUUM and a full
  integrity_check run, and PRAGMA table_list counts their columns.
  (With the `fts5` leaf, fts5 files are no longer "unknown"; in a
  `v16,vtab` build their WITHOUT ROWID `_idx` / `_config` need the
  `without_rowid` leaf, as any such table.) `unknown_module_file_opens`.
- **`total_changes()` after R*Tree writes**: the pin counts the rows
  its internal statements change — the root's INSERT at CREATE, each
  `%_node` write (repeats included: `nodeRelease` writes a node every
  time it is released, which during a delete's reinsertion is once
  per reinserted cell), `%_rowid` / `%_parent` writes and deletes,
  `rtreeNewRowid`'s placeholder row, the auxiliary UPDATE. The tree
  code now counts them (`Writes::count`) and models the delete
  path's releases (`release_but_root`); they join `total_changes()`
  only. CREATE leaves `last_insert_rowid()` at 1, as the pin's.
  Checked over 300 generated workloads and the existing generated
  corpora (now printing the counters every 7 writes); the shadow
  rows stay byte-identical.
- **`.dump`** (hipp-cli): the pin shell's script byte for byte for
  virtual tables — the `/* WARNING: ... DEFENSIVE ... */` and `PRAGMA
  foreign_keys=OFF;` header (the pin needs both to replay it),
  `PRAGMA writable_schema=OFF` after the views / triggers / indexes,
  `x'..'` blobs, `CREATE TABLE IF NOT EXISTS` for any quoted table
  name — and `.dump PATTERN...` (the pin's LIKE filter; a virtual
  table brings its `name_*` tables). hipp replays dumps through the
  new `PRAGMA writable_schema` (leaf `vtab`): while on, the one
  `INSERT INTO sqlite_schema(...)` row a dump writes for a virtual
  table (`'table'`, name twice, 0, `CREATE VIRTUAL TABLE ...`,
  literals) registers it without xCreate, and shadow tables take the
  dump's rows; anything else written to `sqlite_schema` is refused.
  `hipp-cli/tests/cli_v16_vtab.rs` `dump_of_virtual_tables` (R*Tree,
  fts5 and fts4 tables; each engine's dump replayed in both),
  `tests/vtab_v16.rs` `writable_schema_replays_a_dump`.
- **`dbstat(schema)`**: an unknown database at CREATE is
  `UnknownDatabase` (the pin's "no such database"); `temp` and
  attached databases are walked (they gave no rows).
  `dbstat_other_databases`.
- **README** no longer claims integrity_check runs the R*Tree check
  or that unknown-module files refuse only that table's statements
  without saying which.

**Not changed:** the pin's shell runs in defensive mode, where
`PRAGMA writable_schema = ON` is a silent no-op (it reads back 0)
unless the script carries the `.dump` WARNING header; hipp has no
defensive switch, honours the pragma and reads back 1.
`last_insert_rowid()` after an R*Tree UPDATE /
DELETE of a multi-level tree (the known gap above; the pin's last
internal INSERT depends on its node-cache releases, which hipp
models only for the delete path).

**Probing notes (live pin):** the pin's `.dump` of a virtual table
replays only with its WARNING comment *followed by* `PRAGMA
foreign_keys=OFF;` (either alone fails with "table sqlite_master may
not be modified"); `dbstat('temp')` before any temp object lists
nothing; `CREATE TABLE x_content` *before* a contentless fts5 `x` is
allowed (only the shadow tables xCreate makes are checked).

**Gates:** `ci/local-gates.sh` (full) passes.

**Size:** tiny **76,960 B** (Δ **0**: the parser's string-literal
column name is `vtab`-only); full **500,200 B** (Δ **0**); max
**1,002,788 B** (Δ **+3,096 B** over 999,692, ratchet raised: the
writable_schema replay ≈ 1.3 KiB, shadow-name checks ≈ 0.7 KiB,
dbstat of other databases ≈ 0.5 KiB, the change counting and
bounded drain the rest; 45,788 B of the 1 MiB budget remain).

### FTS5 full-text search (2026-11-05)

**Status:** implemented, differential-tested against the live
3.54.1 pin, gated: leaf `fts5` (implies `vtab`, `without_rowid` for
the WITHOUT ROWID `%_idx` / `%_config`, and `math` for bm25's `log`),
in `max` (ADR 0002), not `full`; its own `v16,vtab,without_rowid,
math,fts5` line in `ci/feature-sets.txt`. It is a module on the
`vtab` framework (no second virtual-table path). Without the leaf a
file holding an fts5 table opens and the table is "no such module",
as before.

**Shipped:**

- **CREATE VIRTUAL TABLE t USING fts5(...)** (`src/fts5/config.rs`):
  columns (`UNINDEXED`; `rank` / `rowid` / duplicates refused),
  `prefix=`, `tokenize=`, `content=` / `content_rowid=`,
  `content=''`, `contentless_delete=` (on contentless tables),
  `columnsize=`, `detail=full|column|none`, the pin's argument
  errors; the shadow tables `%_data`, `%_idx`, `%_content`,
  `%_docsize`, `%_config` created in the pin's order with its text
  (`CREATE TABLE 't_data'(...)`; the parser now accepts a string
  literal as a CREATE TABLE name with the leaf), the empty structure
  record (`V2` marker for contentless_delete) and averages, the
  `version` row. DROP removes them; ALTER TABLE RENAME renames them
  with the pin's quoted text (syncing the table first, as
  `sqlite3Fts5StorageRename` does).
- **Tokenizers** (`src/fts5/tok.rs`): `unicode61` with
  `remove_diacritics 0/1/2`, `tokenchars`, `separators`,
  `categories`; `ascii` with `tokenchars` / `separators`; `porter`
  over any inner tokenizer (the pin's generated stemmer steps);
  `trigram` with `case_sensitive` and `remove_diacritics`. The
  Unicode category, case-folding and diacritic tables
  (`src/fts5/unicode.rs`, ~10 KiB) are generated in-crate from the
  pin's own tokenizer output over every code point (clean-room: no
  SQLite source was read or copied), so the categories, folds and
  diacritic stripping are the pin's, astral planes included.
- **The index, byte for byte** (`src/fts5/mod.rs`): the pending-term
  hash with the pin's byte accounting, the flush into a new level-0
  segment (leaf pages of `pgsz` with the pin's term / rowid / pgidx
  encoding and page-splitting of long doclists, doclist indexes,
  `%_idx` rows), the structure record (levels, segments, write
  counter, cookie), averages, segment ids (smallest unused);
  automerge (work units of 64 pages, `automerge` 0-64), crisismerge,
  usermerge, `merge` with positive / negative page budgets,
  `optimize` (and its incremental form), promotion of segments,
  incremental merges with trimmed input segments — the multi-segment
  iterator is the pin's tournament (older duplicates skipped in its
  order, which decides where trimmed segments restart). Deletes and
  updates write the pin's delete markers; `delete-all`, `rebuild`,
  `integrity-check` (the index compared with the tokenized content,
  `%_docsize` and the averages), `pgsz` / `hashsize` / `automerge` / `crisismerge` /
  `usermerge` / `deletemerge` / `rank` settings with the pin's value
  rules and cookie bump. The whole `%_data` image of a table is read
  per write (an in-memory store whose writes are applied through the
  pager in the pin's order).
- **Flush points** (`session::fts5`): commit (xSync); every
  savepoint (user SAVEPOINT, and the statement savepoint the pin
  opens for a statement with a statement journal); RELEASE /
  ROLLBACK TO's level rules; a rowid below the last pending one (or
  equal without a delete); `hashsize`; config commands; fts5vocab
  cursors and RENAME (a full sync). ROLLBACK and ROLLBACK TO discard
  pending terms; autocommit writes fired by triggers are flushed in
  the same transaction.
- **Queries** (`src/fts5/expr.rs`): the full query grammar (phrases,
  `+`, `prefix*`, `^`, `NEAR(... , N)`, column filters `col:`,
  `{a b}:`, `-col:` / `-{...}:`, AND / OR / NOT precedence, implicit
  AND, parentheses, quoting, the pin's syntax errors and the
  detail=column/none refusals), evaluated with the pin's node
  semantics (candidate rows of an AND whose phrase tests fail stay
  "nomatch" and expose no positions; NOT's left side; empty phrases
  dropped from NEAR groups and implicit ANDs); prefix queries read
  the prefix indexes exactly as the pin chooses them. `t MATCH q`,
  `col MATCH q`, `t = q`, `t(q)`, rowid `=` / ranges, several MATCH
  terms ANDed; `ORDER BY rank` (the pin's sorter: rows by the live
  rank, the sorted cursor's phrase data on detail != full) with
  `rank MATCH 'bm25(...)'` and the `rank` setting; `ORDER BY rowid
  [DESC]`. LIKE / GLOB on a trigram table (no diacritics folding)
  become the pin's pattern queries (`sqlite3Fts5ExprPattern`:
  runs of three or more characters, ANDed token by token on
  detail != full), still checked by the WHERE clause.
- **Auxiliary functions** (`src/fts5/aux.rs`): `bm25()` with
  per-column weights (column sizes from `%_docsize`, the tokenized
  content, or -1 on contentless tables without it), `highlight()`
  and `snippet()` with the pin's instance iteration, sentence
  scoring and corruption checks; on detail=column/none the instance
  lists come from retokenizing the row, as the pin's.
- **fts5vocab** `row` / `col` / `instance` over a table (NULL for
  zero counts, the pin's column names), with the sync its cursor
  does.
- The entry points the statement engine awaits (sync, savepoint,
  release, writes, reads) box their futures, so statements re-entered
  by trigger programs keep their stack frames small (1000-deep
  recursive triggers still fit the CLI's stack).
- Transactions, savepoints and crash safety: the shadow tables are
  ordinary pages under the pager's journals; `tests/crash.rs` covers
  flush, merge, optimize, statement and savepoint rollback and an
  external-content table kept by triggers.

**Probing notes (live pin):**

- Shadow tables: created data, idx, content, docsize, config, their
  names quoted as string literals; after ALTER TABLE RENAME as
  double-quoted identifiers. An empty table's structure record is
  `00000000 000000` (cookie, levels, segments, write counter; with
  `FF000001` after the cookie for contentless_delete) and its
  averages record is empty.
- Pending terms are flushed at commit and by **every** xSavepoint:
  a user SAVEPOINT, and the statement savepoint of statements with
  a statement journal (an fts5 INSERT of several rows or from a
  SELECT, UPDATE / DELETE not by `rowid =`, statements on tables
  with triggers, multi-row writes on tables with constraints).
  RELEASE of level i flushes only when deeper savepoints were seen;
  ROLLBACK TO discards. Queries read pending terms without flushing,
  but a prefix scan of them resets the delete flag, so the next
  write of the same rowid flushes; fts5vocab cursors and RENAME do a
  full sync (pending terms and the averages).
- A write with a rowid below the last pending one flushes first; so
  does one equal to it unless the previous op was its delete.
- Merges: which duplicate entry the tournament iterator skips
  decides where an incrementally merged input segment is trimmed
  (`%_data` bytes differ otherwise). `merge` N > 0 only merges a
  level with at least `usermerge` segments; N < 0 merges any level
  with more than one.
- `snippet()` reports corruption (an instance past the column size,
  e.g. contentless `columnsize=0`, where sizes are -1) only for the
  **last** column it scans: the next column's xColumnText overwrites
  the error code. bm25 on such tables uses -1 per column.
- `ORDER BY rank` on detail=column/none: rows come in live-rank
  order but the auxiliary functions see the sorted cursor's phrase
  data (all phrases populated, then CheckPoslists), so the
  displayed rank is recomputed and can differ from the order.
- `-col : x` leaving no columns is not EOF; a nested filter that
  intersects to nothing is. NEAR drops phrases a tokenizer makes
  empty (trigram tokens under three characters).
- A column filter on detail=none is an error; so is any phrase or
  NEAR query on detail=column/none. `fts5vocab(db, t, type)` is only
  accepted in TEMP ("wrong number of vtable arguments" in main).
- LIKE / GLOB constraints on trigram tables are passed to xBestIndex
  (`L` / `G` in idxStr, not omitted); case-sensitive trigram takes
  GLOB only; with remove_diacritics no pattern is used. Patterns
  with no three-character run give a full scan.
- An unknown rank function errors only when the rank is used.
- An automatic rowid after 9223372036854775807 is random on the pin
  (the tests avoid it); the shell truncates SQL text at a NUL.

**Tests:** `tests/fts5_v16.rs` (18 tests: CREATE text and argument
errors, RENAME; 14 tokenizer configurations over multi-script text
compared through fts5vocab and `%_data` bytes; a 4,355-word porter
corpus built to reach every rule; 54 queries of every operator ×
5 query shapes (MATCH, `=`, table-valued, column MATCH, ORDER BY
rank, highlight, snippet, bm25 with weights) on detail=full / column
/ none, prefix indexes, porter, contentless and external-content
tables; plans and errors; snippets / highlights; trigram LIKE /
GLOB; every special command with `%_data` / `%_idx` / `%_config`
bytes; rowids and conflict clauses; transactions and savepoints;
external content kept by triggers; fts5vocab; 14 regressions, one
per bug found; 40 generated workloads (`HIPP_FTS5_CORPUS` for more;
500 run clean locally, plus ~800 seeds of the scratch generator) of
random configuration — tokenizer, detail, prefix, columnsize,
UNINDEXED, normal / external-with-triggers / contentless content,
fts5vocab, pgsz / automerge / crisismerge / usermerge / hashsize /
rank settings — with inserts, multi-row inserts, REPLACE, deletes,
updates, rowid moves, optimize / rebuild / merge, transactions and
savepoints and random queries, comparing every output line and the
shadow tables statement by statement, then the pin's
integrity-check on hipp's file and hipp's on the pin's; files moving
between the engines mid-workload both ways). `tests/crash.rs`
`v16_crash_fts5_writes`; `hipp-cli/tests/cli_v16_fts5.rs`
(byte-identical stdout: queries, ranks and REAL rendering,
highlight / snippet, fts5vocab and commands, external content and
trigram LIKE, error lines, `.schema`).

**Gates:** `ci/local-gates.sh` (full) passes: fmt, clippy `-D
warnings` (default, all features, every feature-set line including
`v16,vtab,without_rowid,math,fts5`), workspace tests, the no_std
targets, the three size gates.

**Size:** tiny **76,960 B** (Δ **0**); full **500,200 B** (Δ **0**:
the leaf is in `max` only); max **999,692 B** (Δ **+124,860 B**,
ratchet raised to it — over the 32 KiB justification threshold: the
generated Unicode tables ≈ 10 KiB, the four tokenizers and porter
≈ 9 KiB, the index writer / merger / reader ≈ 35 KiB, the query
parser and evaluator ≈ 22 KiB, bm25 / highlight / snippet ≈ 9 KiB,
the session glue (writes, flush points, special commands,
integrity-check, vocab, planning) ≈ 38 KiB; FTS5 is the largest
SQLite extension and has no smaller faithful form; 48,884 B of the
1 MiB budget remain, so it stays in `max` without a new ADR).

**Honest gaps:**

- `contentless_delete=1` tables are created and written byte for
  byte, and the pin's tombstones are read and merged (review fixes
  below), but DELETE / UPDATE on them (writing tombstone pages) is
  refused (`Unsupported`); deletes while `secure-delete=1` is set
  are refused (`Fts5SecureDelete`: hipp does not remove entries in
  place) (closed in v0.17: leaf `fts5_delete` writes the tombstones
  and makes the secure deletes, see "FTS5 delete writes" above);
  `insttoken`, `locale=1`, `tokendata=1` and
  `contentless_unindexed=1` are refused.
- fts5 tables and fts5vocab tables in TEMP or attached databases
  are refused (so is the three-argument fts5vocab form); custom
  tokenizers and auxiliary functions cannot be registered.
- An unknown rank function (`rank MATCH 'nosuch()'`) is an error
  even when the rank is never read (the pin's is lazy).
- Query results are materialized before the first row: an error
  mid-query (e.g. snippet's corruption check on a later row) fails
  the statement where the pin first returns the earlier rows.
- The statement-journal flush follows a heuristic of the pin's
  `usesStmtJournal` (multi-row writes, triggers, constraints); a
  statement shape it misjudges flushes pending terms at a different
  point, giving different (valid) segment layouts. Writes fired by
  triggers inside a multi-row outer statement in an explicit
  transaction approximate the pin's savepoint nesting likewise (a
  failing such statement now drops their terms, as the pin's
  xRollbackTo does: review fixes below).
- The `hashsize` accounting follows the pin's byte formula for
  ordinary terms; very long tokens may flush at slightly different
  points.
- Every write reads the table's whole `%_data` into memory, and
  queries read the doclists they need from it: correct at any size,
  but slow and memory-hungry for very large indexes.
- PRAGMA integrity_check does not run fts5's xIntegrity (use
  `INSERT INTO t(t) VALUES('integrity-check')`).
- The hidden table column's value (`SELECT t FROM t`) is a row
  handle, not the pin's cursor pointer value (both meaningless).

### FTS5 full-text search — review fixes (2026-11-05)

Eleven review findings on the FTS5 unit, each reproduced against the
live 3.54.1 pin. Fixes on claimed fts5 paths are in the `fts5` leaf
(in `max`); two touch the shared `vtab` framework (MATCH under an
OR, a LEFT JOIN's ON constraints).

**Fixed:**

- **Pin-written tombstones** (corruption): a `contentless_delete=1`
  table the pin deleted rows of keeps them in its segments, hidden by
  per-segment tombstone hash pages. hipp did not read those pages, so
  queries returned deleted rows and hipp's `optimize` / merges copied
  them back into the index for good (the pin's integrity-check still
  passed). The reader and the merge iterator now apply the pin's
  `fts5MultiIterIsDeleted` / `fts5IndexTombstoneQuery` (key size,
  slot hashing, the rowid-0 flag), merges drop tombstoned entries
  and recount `nEntry` as the pin, and `fts5IndexFindDeleteMerge`
  (the `deletemerge` setting) picks merge levels as the pin does.
  Writing tombstones (DELETE on such tables) stays refused (closed
  in v0.17: leaf `fts5_delete`).
- **Pin-set `secure-delete`** (corruption): hipp ignored it and wrote
  ordinary delete markers, leaving the deleted terms on disk and a
  `%_data` unlike the pin's. Deletes (DELETE, UPDATE, OR REPLACE,
  the 'delete' command) now fail with `Fts5SecureDelete` while it is
  set; the 'secure-delete' command itself is accepted (so hipp can
  turn it off, after which deletes match the pin byte for byte).
  Settings are stored as the pin stores them: numeric text becomes
  its integer (`'64'` was stored as text — found while probing).
- **Failing statement in a transaction** (corruption): a multi-row
  INSERT that failed after its AFTER INSERT trigger had indexed
  earlier rows into an external-content table left those terms
  pending, so they were flushed at COMMIT (phantom rowid, both
  integrity-checks failing). A statement with the pin's statement
  journal now restores the transaction's fts5 state after its
  start-of-statement flush when it fails (the pin's xRollbackTo).
- **NEAR inside an OR** (wrong bm25 / highlight / snippet): a failed
  NEAR test dropped every phrase's positions; the pin keeps what its
  test left (phrases before the failing one, the failing one empty,
  later ones holding the buffers of the last row the node tested),
  and the OR exposes them. Fixing it found two more: a row missing a
  term of a phrase after a failing one was still a NEAR candidate,
  and a column-filtered prefix scan (`fts5SetupPrefixIter`) keeps no
  row the filter empties.
- **snippet() with a negative token count**: clamped to 0..INT_MAX
  like the pin (`-1` and `4294967295` were both used as -1).
- **External content missing a row**: an error (the pin's "fts5:
  missing row N from content table", `VtabCorrupt`) when the
  statement reads a content column or an auxiliary function reads
  the text; `SELECT rowid` / `count(*)` / bm25 with column sizes
  still answer, as on the pin (the planner now records the pin's
  `colUsed` for fts5 sources).
- **INSERT with an integral REAL rowid** (`7.0`, `CAST(8 AS REAL)`,
  `'9.0'`): accepted through the pin's `OP_MustBeInt`; an UPDATE's
  new rowid still refuses them, as on the pin.
- **MATCH under an OR / in a LEFT JOIN's ON**: `t MATCH 'a' OR t
  MATCH 'b'`, `... OR rowid = 4`, `... OR rowid > 3`, `... OR (t
  MATCH 'b' AND rowid < 3)` run as the pin's multi-index OR (each
  branch's filter in turn, rows unioned in branch order, rowid
  comparisons rechecked, bm25 0.0 on a rowid-only branch); `m LEFT
  JOIN t ON t MATCH m.q` passes the ON constraints to the module per
  left row (generate_series ON constraints under LEFT JOIN work too).
- **INSERT ... RETURNING and UPDATE ... FROM** on fts5 tables: the
  RETURNING row is the pin's (evaluated before xUpdate: rowid -1
  unless given, hidden columns NULL, `*` the visible columns,
  `last_insert_rowid()` of the previous row); UPDATE ... FROM
  applies every joined row, the last winning. UPDATE / DELETE
  RETURNING stay refused, as on the pin.
- **Auxiliary functions in an aggregate query** (`sum(bm25(t))`,
  `count(*), bm25(t)`, GROUP BY / HAVING): `Fts5Context`, the pin's
  "unable to use function ... in the requested context".

**Not fixed (and why):**

- **MATCH whose argument comes from a table listed later in FROM**
  (`FROM t, m WHERE t MATCH m.q`): the pin reorders the join; hipp
  runs sources in FROM order (no join reordering anywhere, see
  `xplan`), so the constraint cannot be evaluated first. Write the
  argument's table first (`FROM m, t`), which both engines run.
  (Closed in v0.17: the `join_order` planner puts `m` first, as the
  pin does.)
- **MATCH / aux functions in views and triggers** fail on the pin's
  shell because it sets `trusted_schema=0` (the library default is
  1). hipp has no `trusted_schema` (the same holds for `dbstat` and
  `rtreedepth()` in views): it behaves as the library default.

**Probing notes (live pin):** tombstone pages are
`FTS5_TOMBSTONE_ROWID(segid, pg)`; the page for a rowid is `rowid %
nPgTombstone`, its slot `(rowid / nPgTombstone) % nSlot` with linear
probing; key size 4 or 8 from byte 0, the rowid-0 flag in byte 1.
`secure-delete` leaves `version` at 4 until the pin's first secure
delete. `sqlite3_value_numeric_type()` converts a setting's value in
place, so `%_config` stores the integer. A multi-index OR's rows come
branch by branch; a branch not usable by fts5 (`a = 'x'`) makes the
whole OR a full scan, where MATCH errors.

**Tests:** `tests/fts5_v16.rs`: `pin_tombstones_are_honoured` (24
generated pin-written contentless_delete files with deletes, every
detail mode, continued by both engines with inserts / optimize /
merge / automerge; outputs, fts5vocab and shadow tables equal, the
pin's integrity-check on hipp's copy), `secure_delete_is_not_ignored`,
`review_regressions` (one case per finding and per bug the fixes'
own fuzzing found), `generated_near_or_queries_match` (40 seeds of
random NEAR / OR / AND / NOT / column-filter / prefix queries, every
row's bm25, highlights and snippet in both rowid orders and rank
order; ~700 seeds of the scratch version ran clean);
`hipp-cli/tests/cli_v16_fts5.rs` `review_fixes_output`.

**Gates:** `ci/local-gates.sh` (full) passes.

**Size:** tiny **76,960 B** (Δ **0**); full **500,200 B** (Δ **0**);
max **1,032,068 B** (Δ **+9,664 B**, ratchet raised to it: the
tombstone reader and deletemerge ≈ 1.5 KiB, the multi-index OR ≈ 3
KiB, RETURNING / UPDATE FROM ≈ 1.5 KiB, the NEAR buffers, colUsed and
the rest ≈ 3.5 KiB; 16,508 B of the 1 MiB budget remain).

**Honest gaps:** the two "not fixed" items above; and query results
are still materialized, so a statement whose later row fails (an
auxiliary function on a LEFT JOIN's padded row, a missing content
row) reports the error without the rows the pin printed first.

### EXPLAIN QUERY PLAN (2026-11-05)

**Status:** implemented, differential-tested against the live 3.54.1
pin, gated: leaf `explain` (implies `v16`), in `max` (ADR 0002), not
`full`; its own `v16,explain` line in `ci/feature-sets.txt`. Without
the leaf `EXPLAIN` is an unsupported statement, as before.

**Shipped:**

- `EXPLAIN QUERY PLAN <statement>` (`session::eqp`): the statement is
  parsed and planned as usual (its errors are this statement's, at
  their offsets) and never run; the result is the pin's four columns
  `id`, `parent`, `notused`, `detail`. `parent` links the rows into
  the pin's tree; `id` numbers them in order (the pin's are bytecode
  addresses) and `notused` is 0. `Statement::is_explain()` is the
  pin's `sqlite3_stmt_isexplain` (2). Parameters bind and are
  ignored; re-execution returns the rows again. Plain `EXPLAIN` (a
  bytecode listing) is refused, `ParseKind::ExplainBytecode`; a
  nested EXPLAIN is a syntax error, as on the pin.
- The rows describe the plan hipp executes, in the pin's words: one
  loop per FROM source (`SCAN t`, `SCAN t USING [COVERING] INDEX i`,
  `SEARCH t USING [COVERING] INDEX i (a=? AND b>? AND b<?)` with the
  rowid / PRIMARY KEY suffix columns and `<expr>` keys, `SEARCH t
  USING INTEGER PRIMARY KEY (rowid=?)`, `SEARCH w USING PRIMARY KEY
  (k=?)`, ` LEFT-JOIN`, `RIGHT-JOIN t`), `SCAN x VIRTUAL TABLE INDEX
  n:str` (hex for generate_series / dbstat, fts5's order flags as the
  pin's 0x20 / 0x40 / 0x80, json_each / json_tree's 1 / 3, pragma
  functions' 0), `SCAN CONSTANT ROW`, `SCAN n-ROW VALUES CLAUSE` /
  `SCAN n CONSTANT ROWS`; the sorters (`USE TEMP B-TREE FOR ORDER BY`
  / `LAST TERM OF` / `LAST n TERMS OF ORDER BY`, `GROUP BY`,
  `DISTINCT` — including the pin's DISTINCT-as-GROUP-BY rewrite —
  and `f(DISTINCT)` / `f(ORDER BY)`); `[CORRELATED] SCALAR SUBQUERY n` / `[CORRELATED]
  LIST SUBQUERY n` where they are evaluated (IN probes with their
  loop); `CO-ROUTINE` / `MATERIALIZE` of FROM subqueries, views and
  CTEs by the pin's `fromClauseTermCanBeCoroutine` rule (a shared
  materialization once), recursive CTEs' `SETUP` / `RECURSIVE STEP`,
  window co-routines `(subquery-n)` per window class, compound
  `MERGE (op)` / `LEFT` / `RIGHT` and `COMPOUND QUERY` / `LEFT-MOST
  SUBQUERY` / `UNION ALL` trees (UNION's LIMIT 1 shortcut as UNION
  ALL); UPDATE / DELETE target loops (COVERING when the pin collects
  rowids first), UPDATE ... FROM, INSERT ... SELECT / VALUES with
  subqueries / RETURNING / an upsert's DO UPDATE, CREATE TABLE ... AS, INSTEAD OF writes of
  views; nothing for statements without a plan (DDL, PRAGMA,
  transactions, one-row INSERT VALUES). Statements routed to the TEMP
  or an attached database are explained there.
- The pin's select numbers: the parser numbers each core when the
  pin's grammar reduces it (subqueries first, a compound's cores left
  to right with the ORDER BY / LIMIT in the last one, WITH bodies
  first, each VALUES row its own select, constant multi-row VALUES
  two), a view's copy is renumbered after the statement's selects in
  copy order, window co-routines after those; a compound subquery
  shows its last core's number.
- CLI: the pin shell's `eqp_render` tree for an EXPLAIN QUERY PLAN
  statement (nothing for an empty plan).
- Planner fixes found by comparing plans, ungated in the
  `expr_index` / `window` code (they decide row order):
  a core with window functions reads its rows in its last window
  class's order (the pin's innermost window subquery's ORDER BY; ties
  of a window's order were broken by the outer ORDER BY's index);
  members of a merged compound are planned for the merge key (every
  column, or an ORDER BY of result numbers); a GROUP BY / DISTINCT
  takes its terms in any order of the index (`GROUP BY a, b` over an
  index on (b, a) was sorted, groups in another order than the
  pin's); a DISTINCT over the rowid is redundant (the planner was
  given an order the pin drops); a rowid IN list delivers rowid
  order; no `IS NULL` term on a NOT NULL key column (INTEGER PRIMARY
  KEY, WITHOUT ROWID key, declared NOT NULL); generate_series reads
  the ORDER BY (its index number says so). Parser: a statement-level
  WITH clause and `IN ((SELECT ...), ...)` no longer count their
  selects twice (numbering only).

**Probing notes (live pin):**

- The shell renders every EQP statement as the tree whatever
  `.mode` is set; an empty plan prints nothing (not even `QUERY
  PLAN`). `notused` is the loop's run cost and ids are VDBE
  addresses (seen through Python's 3.53 module; not reproduced).
- `SCAN` names the alias, else `[schema.]name` as written;
  `MATERIALIZE` / `CO-ROUTINE` name a view or CTE by its own name,
  else the alias, else `(subquery-n)`.
- Rows are emitted in code-generation order: without GROUP BY the
  aggregates' `f(DISTINCT)` / `f(ORDER BY)` sorters first, then FROM
  subqueries, loops (with their IN-probe subqueries and ON
  subqueries), WHERE subqueries, GROUP BY sorter, HAVING, result
  subqueries, (with GROUP BY: the aggregates' sorters here), DISTINCT,
  ORDER BY subqueries, ORDER BY sorter. The aggregates' sorters come
  in `aFunc` order (result list, ORDER BY, HAVING), one per distinct
  call, `f(DISTINCT)` before `f(ORDER BY)` for each; a DISTINCT call
  whose one ORDER BY key is its one argument shows only `f(ORDER BY)`.
- 3.54 merges UNION / INTERSECT / EXCEPT even without ORDER BY
  (`MERGE (op)`, members sorted by every column); UNION ALL members
  after the last merging operator are `UNION ALL` siblings of the
  MERGE; a pure UNION ALL chain is `COMPOUND QUERY`.
- A DISTINCT whose ORDER BY is its result list (any directions) is
  run as a GROUP BY labelled `DISTINCT`; through a one-row rowid
  lookup it still sorts for the ORDER BY. An aggregate without GROUP
  BY drops its ORDER BY sorter; a window query whose ORDER BY
  repeats the first window class's keys drops its outer sorter.
- INSERT VALUES rows: one row is no plan; constant rows are one
  `n-ROW VALUES CLAUSE` co-routine (two select numbers); rows with
  subqueries are `n CONSTANT ROWS`, one select per row.
- Triggers' programs are not shown. An UPDATE / DELETE loop through
  an index is COVERING only when the pin collects the rowids first
  and the loop is not a one-row lookup: RETURNING, a trigger for the
  event, foreign keys the write must check (`sqlite3FkRequired`: an
  UPDATE only when it changes a key), OR REPLACE, a rowid change, a
  WHERE subquery; never for a WITHOUT ROWID table, a UNIQUE `=`
  lookup, or because the SET changes the loop's index.
- An upsert's DO UPDATE shows its WHERE's and SET's subqueries after
  the INSERT's, then the RETURNING's twice (once for the update, once
  for the insert). `SELECT count(*) FROM t` names the table, never
  its alias or schema (the pin's `isSimpleCount` loop). `SCAN` keeps
  a `temp.` / attached schema as written, on every route.

**Differences from the pin (hipp prints what it runs):** no query
flattening (FROM subqueries, views and CTEs stay CO-ROUTINE /
MATERIALIZE loops) and no WHERE push-down (both closed in v0.17 by
`flatten`, as far as its honest gaps say); no join reordering (FROM
order, so a rowid lookup into an earlier table is not used); no
automatic indexes or Bloom filters (closed in v0.17 by `join_order`:
the pin's join order, AUTOMATIC [PARTIAL] COVERING INDEX and BLOOM
FILTER lines); EXISTS stays a subquery (no
`SCAN u EXISTS`; closed in v0.17 by `flatten`); IN subqueries are lists (no `USING INDEX i FOR
IN-OPERATOR` / `USING ROWID SEARCH ON TABLE`; closed in v0.17 by the
`multi_index` review fixes); no MULTI-INDEX OR; no
min() / max() optimization (`SEARCH t USING COVERING INDEX i`, or a
bare `SEARCH t` without an index) (both closed in v0.17 by
`multi_index`: the `MULTI-INDEX OR` / `INDEX n` trees and the
min() / max() `SEARCH` loops); an UPDATE / DELETE WHERE key
compared with a scalar subquery does not drive the target loop
(`SCAN t`; SELECT uses it); RIGHT /
FULL joins and UPDATE ... FROM sources read whole tables (UPDATE
... FROM: closed in v0.17 by the `join_order` review fixes, planned as
a join);
`count(DISTINCT x)` sorts even over x's index; DELETE without WHERE
deletes row by row (`SCAN t`; the pin truncates); an INSTEAD OF
UPDATE / DELETE shows the view's co-routine. The plain-EXPLAIN
bytecode listing has no equivalent. All pinned in
`documented_differences`.

**Tests:** `tests/eqp_v16.rs` (9 tests: 203 curated statements over
loops, sorters, subqueries, structures and writes, each hipp's tree
against the pin's shell on the same file; the documented differences
as hipp goldens that must still differ from the pin; the API — four
columns, `is_explain`, nothing runs (file state checked with the
pin), parameters, reset, plain / broken / nested EXPLAIN refused as
on the pin; a generated corpus — 40 random schemas × 12 queries by
default, `HIPP_EQP_SEEDS` for more: 2,400 plans at 200 seeds match
at 99.6%, the rest listed below; regressions for each planner fix,
comparing rows with data). `hipp-cli/tests/cli_v16_eqp.rs` (two
scripts, byte-identical stdout: trees, empty plans, plans between
result rows, comments and case, `.parameter`, errors).

**Gates:** `ci/local-gates.sh` (full) passes.

**Release:** the workspace and `hipp-cli` are 0.16.0; `CHANGELOG.md`
summarises every v0.16 leaf and ungated fix; README's profile table
and non-goals and ADR 0003 record the final layout and budgets.

**Size:** tiny **76,960 B** (Δ **0**); full **500,200 B** (Δ **0**:
the leaf is in `max` only, and the planner fixes live in
`expr_index` / `window` code); max **1,022,404 B** (Δ **+19,616 B**,
ratchet raised to it; under the 32 KiB threshold: the walker and
its rendering ≈ 14 KiB, numbering and planner fixes ≈ 3 KiB, the
CTE / view hooks and API ≈ 2 KiB). 26,172 B of the 1 MiB budget
remain.

**Honest gaps:**

- hipp's cost model (`xplan`) departs from the pin's in some loop
  choices, which EQP shows and which can order rows differently: an
  IN on a later column of a multi-column index, a second equality
  column of a two-column index (closed in v0.17 by `multi_index`:
  whereLoopAdjustCost for one table), ties between equally costed indexes,
  a WITHOUT ROWID table's secondary index against its primary key
  (and their key-suffix probes), text literals compared with numeric
  columns. The generated corpus therefore uses rowid tables and
  single-column indexes and allows 1 in 100 plans to differ; the
  curated corpus covers the rest.
- `id` is a row number, not a bytecode address; `notused` is 0.
- Select numbers inside view copies and window co-routines follow
  the pin's observed order on the probed shapes; deeper nests (a
  view in a view's subquery, multi-row VALUES inside compounds) may
  number differently.
- VALUES labels inside compounds and CTEs follow hipp's shapes, not
  always the pin's (`SCAN 2-ROW VALUES CLAUSE` where the pin says `SCAN
  2 CONSTANT ROWS` in a CTE body; a UNION of VALUES members sorts no
  VALUES member).
- FK-action and trigger programs are not shown (as on the pin), but
  neither are hipp's FK child scans the pin shows for a parent
  DELETE (`SCAN child`) (closed in v0.17 by `flatten`: parent DELETE /
  UPDATE child scans are printed).

### EXPLAIN QUERY PLAN — review fixes (2026-11-05)

**Status:** all nine review findings reproduced and fixed, each with
a regression test against the live pin. The two wrong answers were
introduced by the unit's own planner changes (ungated `expr_index` /
`window` code; the release notes no longer present those changes as
plain fixes). The rest are EXPLAIN QUERY PLAN text (leaf `explain`).

**Fixed:**

- *Wrong answer:* a window query with an outer ORDER BY on a column
  outside its result list ignored the ORDER BY (`SELECT a, sum(a)
  OVER (ORDER BY b) FROM t ORDER BY c`): the window key replaced the
  ORDER BY in the planner's input, so its columns dropped out of the
  needed set and an index was taken as covering (EQP printed `USING
  COVERING INDEX`); the sorter then sorted NULLs. `CoreInfo` now
  carries the core's own ORDER BY as columns read (`also_reads`). The
  same input change also made `OVER ()` read through the ORDER BY's
  index; an empty window key now means the natural order, as on the
  pin.
- *Wrong answer:* a UNION read through the pin's LIMIT 1 first-row
  shortcut (a written `LIMIT 1`, or the implicit one of a scalar /
  EXISTS subquery without a LIMIT) planned its members for the merge
  key, so the left-most member's first row was the rowid order's, not
  its own plan's (`SELECT (SELECT a FROM t UNION SELECT 99)`).
  `merge_member` now returns no key for that shape; the parser marks
  scalar / EXISTS subqueries (`SelectStmt::first_row`, `expr_index`).
- Without `expr_index` (e.g. `v16,explain`) the single-table fast
  path's index lookup printed `SCAN t`; it prints `SEARCH t USING
  INDEX i (col=?)`, and a lookup a NULL or non-numeric literal
  short-circuits prints the seek it stands for. (Checked by hand on a
  `std,full,explain` build: the test suites run under `max`.)
- UPDATE / DELETE loops printed COVERING whenever hipp's own UPDATE
  took two passes (a SET changing the loop's index), for a WITHOUT
  ROWID table, for a one-row UNIQUE lookup, and whenever `PRAGMA
  foreign_keys` was on; now the pin's rule (probing notes above).
- `USE TEMP B-TREE FOR f(ORDER BY)` is shown (it never was), with the
  pin's function names (`string_agg`, `median`, the `json_` /
  `jsonb_` group functions); grouped queries print the aggregates'
  sorters after the loop, the GROUP BY sorter and the result
  subqueries; a call repeated in the ORDER BY counts once.
- An upsert's DO UPDATE subqueries (WHERE, SET, and the RETURNING it
  fires) were missing.
- A VALUES core used as a subquery numbered one select too many
  (`IN (VALUES (1))` is `LIST SUBQUERY 1`).
- A statement routed to the TEMP or an attached database dropped its
  schema qualifiers (`SCAN tt` for `temp.tt`, `sqlite_schema` for
  `sqlite_temp_master`); they are put back as written. Found on the
  way: `SELECT count(*) FROM t AS q` named the alias (the pin's
  count loop names the table) on every route.
- Docs: CHANGELOG / README describe the two regressions; ADR 0003
  counts seventeen leaves.

**Tests:** `tests/eqp_v16.rs::review_fixes_match_pin` (rows of the
window / UNION shapes vs the pin, and 70 plans: write loops,
aggregate sorters, upserts, VALUES numbers, count loops),
`routed_qualifiers_match_pin` (19 plans on TEMP / attached routes);
`hipp-cli/tests/cli_v16_eqp_review.rs` (two byte-identical
scripts). A scratch differential fuzzer over the regression classes
(window + outer ORDER BY, UNION / EXISTS / scalar UNION with and
without LIMIT 1, DISTINCT LIMIT; 300 random schemas × 12 queries,
rows vs the pin) found no difference after the fixes;
`HIPP_EQP_SEEDS=200` keeps the generated plans within the corpus
threshold.

**Gates:** `ci/local-gates.sh` (full) passes.

**Size:** tiny **76,960 B** (Δ **0**); full **500,200 B** (Δ **0**:
the new field and checks are under `expr_index` / `explain`); max
**1,034,772 B** (Δ **+2,704 B**, ratchet raised; 13,804 B of the 1
MiB budget remain). Merging `main`'s lazy index-cursor decoding
(`src/btree.rs`, `docs/perf-baselines.md`) then took full to
**500,952 B** (+752 B) and max to **1,035,516 B** (+744 B; 13,060 B
remain); both ratchets are set to those measured values. The merge also
pushed a debug build's IN-subquery path past the 512 KiB thread of
`budgeted_limits_on_a_small_stack_refuse_cleanly` (about 12 KiB more
stack under the lazy cursor's nested async calls, more under
`-C instrument-coverage`, and more with debug info on, as CI
builds): the debug `STACK_BASE` of `Config::stack_budget` is
re-measured at 384 KiB (was 320 KiB; 368 KiB was the smallest that
held in CI's plain test build), so the derived limits refuse before
the stack runs out in every build CI runs. Release constants are unchanged.

**Honest gaps:**

- hipp does not parse several ON CONFLICT clauses on one INSERT (the
  pin's chained upsert: a syntax error in hipp); one clause is
  explained as above.
- The foreign-key part of the COVERING rule asks `fk_target` (hipp's
  `sqlite3FkRequired`), and the pin's FK child scans on a parent
  write are still not shown.
- The TEMP / attached qualifiers are put back by name: a statement
  reading one table both qualified and unqualified (without aliases)
  prints the qualifier on both loops.

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
