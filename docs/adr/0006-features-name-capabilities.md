# ADR 0006 — Features name capabilities, not milestones

- **Status:** accepted (2026-11-08), in v0.17; supersedes the feature
  *names* of ADR 0001–0004 (their gating, placement and budget rules
  stand)
- **Context:** hipp's dialect grew one milestone at a time, and each
  milestone that added surface added a Cargo feature named after it:
  `v04` (joins, aggregates, the evaluator), `v05` (UPDATE / DELETE /
  DROP TABLE), `v06` (subqueries), `v07` (conflict clauses, UPDATE
  FROM, RETURNING, upsert), `v08` (INSERT ... SELECT), `v09`
  (compounds, derived tables), `v10` (subqueries in aggregate
  arguments; the v0.11 milestone, since v0.10 took no feature) and
  `v16` (the v0.16 core-dialect completion), each implying the one
  before it — a ladder of "rungs". v0.17 then split the pin's planner
  into three leaves, `join_order`, `multi_index` (implying
  `join_order`) and `flatten` (implying `join_order`), all in `xl`.
  The names said when something was built, not what it is; a reader
  had to know that `v07` meant RETURNING. Nobody builds an
  intermediate rung (no profile, test or user enables `v06` without
  `v16`), yet the code kept `cfg` fallbacks for them, and the planner
  leaves only ever ship together (`xl` carries all three). The user
  decided on 2026-11-08: "features should be what they actually are,
  not a version number".

## Decision

1. **Three dialect features replace the ladder**, each implying the
   one before it:

   | new | old | enables |
   |---|---|---|
   | `query` | `v04` | joins, aggregates + GROUP BY, the expression evaluator, CHECK / DEFAULT / generated columns / STRICT, ALTER TABLE ADD COLUMN, inert views |
   | `write` | `v05` | UPDATE, DELETE, DROP TABLE (implies `query`) |
   | `sql` | `v06` `v07` `v08` `v09` `v10` `v16` | subqueries, conflict clauses / UPDATE FROM / RETURNING / upsert, INSERT ... SELECT, compounds and derived tables, subqueries in aggregate arguments, and the v0.16 core-dialect completion (implies `write`) |

2. **One leaf `planner`** replaces `join_order`, `multi_index` and
   `flatten`, with `join_order`'s dependencies (`sql`,
   `expr_index`), in `xl`.
3. **Every other name stays**: `std`, `indexes`, `params`, `math`
   and every leaf. Every leaf that required `v16` requires `sql`
   (`math` keeps requiring only `query`; no leaf's code was widened
   or narrowed).
4. **The profiles keep their names and contents**: `tiny`
   (`--no-default-features`), `full` (`indexes`, `query`, `write`,
   `sql`, `params`, `math`), `max` (`full` + the heavyweight leaves),
   `xl` (`max` + `fts5_delete` + `planner`), `default = ["std",
   "xl"]`.
5. **Code only an intermediate rung could reach is deleted.** After
   the rename a `cfg` predicate is read under the new implications;
   one that no surviving combination can satisfy (`all(feature =
   "expr_index", not(feature = "v09"))`, `join_order` without
   `multi_index`, a `v06` branch inside a `not(feature = "v16")`
   block, ...) goes with its code, and one that is always
   true where it stands loses its attribute. Paths reached by tiny,
   `query`-only, `write`, `indexes,query`, `indexes,write` and the
   leaf lines stay.
6. **New surface is named for what it is.** Small dialect surface
   joins `sql` (ADR 0001's "the `v16` rung" now means `sql`); a new
   leaf is named for its capability, never for a milestone.

## Consequences

- **Breaking** for anyone who enables features directly:
  `--features v16` (or `v04`, `join_order`, ...) no longer exists and
  Cargo refuses it. Builds that name a profile (`full`, `max`, `xl`)
  or take the default are unaffected. CHANGELOG.md's "Breaking:
  feature renames" has the old → new table.
- `ci/feature-sets.txt` checks `query`, `write`, `sql`,
  `indexes,query`, `indexes,write`, `query,math`, `sql,<leaf>` for
  every leaf and one `sql,indexes,expr_index,planner` line (the three
  planner lines become one).
- Test file names (`*_v16.rs`, `cli_v07.rs`, ...) and STATUS.md /
  README milestone headings are history and keep their version
  numbers; feature names in them use the new spelling.
- **Evidence that the rename changed no code** (STATUS.md, v0.17
  "Features named for capabilities", has the full table): every
  mapped feature set was built before (at `3bc670b`) and after with
  the size gate's build (`--profile tiny`, thumbv7em-none-eabihf,
  `hipp_thumb_demo`). Every pair has the same functions at the same
  sizes up to layout effects, and a purely mechanical rename of
  `3bc670b` (no deletions) links to `.text` / `.rodata` byte-identical
  to the result for `query`, `write`, `max` and `xl`. The linked
  `.text + .rodata` is unchanged for tiny and full; it moves by a few
  bytes in some lines (max +8 B, xl +16 B, `query` +8 B, `write`
  −8 B, ...) because a feature's name enters cargo's `-C metadata`
  hash, the hash enters every mangled symbol name, and rustc orders a
  codegen unit's monomorphised items by symbol name, which moves
  inter-function alignment padding and the machine outliner's
  choices. The same jitter appears at `3bc670b` itself when an unused
  feature is merely enabled (`v04,zzz`: −8 B; tiny + `zzz`: −4 B),
  with no code change.
- **Ratchets.** The `[max]` / `[xl]` ratchets move to the measured
  962,300 B / 1,058,824 B for that padding (tiny and full unchanged).
  Any future rename of an enabled feature can move the profiles by a
  few bytes the same way; a ratchet change for it must show the
  symbol sizes unchanged.
