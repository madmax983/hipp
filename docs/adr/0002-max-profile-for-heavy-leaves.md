# ADR 0002 — A `max` profile for the leaves that outgrow `full`

- **Status:** accepted (2026-11-03); revisit if the `full` budget is
  raised
- **Context:** ADR 0001 put every new dialect feature into the `full`
  alias, whose no_std size is ratcheted against a 512 KiB budget.
  After the third parity wave (PRAGMAs, executed views and CTEs,
  savepoints and maintenance, ALTER TABLE / AUTOINCREMENT / TEMP),
  `full` measures 474,896 B: about 49 KiB of headroom. The remaining
  parity subsystems (window functions, triggers, foreign-key
  enforcement, JSON) are each larger than that headroom, and folding
  them into `full` would either break its budget or force the
  budget up without a decision about what embedded targets need.

## Decision

1. **`full` keeps its meaning and its budget.** It is the embedded
   "whole core dialect" profile: every ladder rung plus the leaves
   that fit (`params`, `math`). Its ratchet and 512 KiB budget are
   unchanged.
2. **A new alias `max = ["full", ...]` carries the heavyweight
   leaves.** Each such leaf (`window`, `triggers`, `foreign_keys`,
   `json`, ...) is still a separate feature that depends only on what
   it needs, compiles alone (its own line in `ci/feature-sets.txt`),
   and is added to `max`, not to `full`.
3. **`default = ["std", "max"]`.** Host builds, the CLI and the
   differential test suites get the whole engine, so every leaf is
   tested against the pinned oracle in CI.
4. **`max` is ratcheted too.** `ci/size-ratchet.toml` gains a `[max]`
   section (1 MiB budget, 32 KiB justification threshold), measured
   by the size job and `ci/local-gates.sh` on the same thumbv7em
   demo binary. The no_std target builds and the no_std library
   tests also run with `max`, so the heavyweight leaves stay
   `no_std`.

## Consequences

- An embedded user picks `full` (bounded by 512 KiB) or opts into
  individual leaves on top of it; nothing in `max` leaks into `full`
  or `tiny`.
- Promoting a leaf from `max` into `full` later is a one-line change
  plus a `full` ratchet bump, if the budget is raised.
- The `full` gate no longer measures everything that ships, so the
  `max` gate is the number that tracks total engine growth.
