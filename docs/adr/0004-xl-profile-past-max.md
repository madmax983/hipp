# ADR 0004 — An `xl` profile for the leaves past `max`

> **Naming superseded by ADR 0006 (2026-11-08).** The feature names
> below are those of their time. `v04` is now `query`, `v05` is
> `write`, `v06` ... `v10` and `v16` are one feature `sql`, and
> `join_order` / `multi_index` / `flatten` are one leaf `planner` (in
> `xl`). The decisions recorded here stand; where this ADR says "the
> `v16` rung", read `sql`. The rest of the text is kept as written.

- **Status:** accepted (2026-11-05), at the start of v0.17
- **Context:** ADR 0003 closed the 0.16.0 release with `max` at
  1,035,516 B of its 1,048,576 B budget (13,060 B of headroom) and
  ruled that the next leaf that does not fit `max` gets its own alias
  outside it, named by an ADR, rather than raising `max` past 1 MiB.
  The v0.17 backlog (STATUS.md's honest gaps: streamed virtual-table
  rows, FTS5 tombstone and secure-delete writes, the pin's join
  ordering, and the EXPLAIN QUERY PLAN strategies that follow from
  it) is larger than that headroom.

## Decision

1. **A new alias `xl = ["max", …]`** holds every leaf that does not
   fit `max`'s remaining budget. `max` keeps its 1 MiB budget and its
   ratchet; it is not raised.
2. **`default = ["std", "xl"]`**, so the CLI and the differential
   tests build everything, as `max` did before.
3. **Budget 1,310,720 B (1.25 MiB)** for
   `--no-default-features --features xl` on thumbv7em-none-eabihf
   (`--profile tiny`, `ci/size_gate.py --section xl`), with the same
   ratchet rules as the other profiles (`justify_over` 32 KiB, engine
   statics 8 KiB). The ratchet starts at `max`'s measured size, since
   `xl` holds no extra leaf yet.
4. **Placement rule.** A new leaf measures itself against `max`
   first: if `max` + the leaf stays within 1 MiB it joins `max`;
   otherwise it joins `xl`. A leaf may move from `xl` into `max` only
   when it fits. Ungated fixes and `v16`-rung surface still grow
   `full` (and so every profile) under ADR 0001.
5. **CI** builds, tests and size-gates `xl` beside the other three
   profiles (`.github/workflows/ci.yml`, `ci/local-gates.sh`), and
   `ci/feature-sets.txt` lists `xl` and `std,xl`.

## Consequences

- `max` stays a fixed, 1 MiB target for embedded builds that want
  the whole 0.16 dialect; `xl` is for hosts and large targets.
- The tiny, full and max ratchets are unchanged by this ADR.
