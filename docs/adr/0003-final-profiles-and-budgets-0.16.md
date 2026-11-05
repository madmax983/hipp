# ADR 0003 — The profile layout and size budgets of 0.16.0

- **Status:** accepted (2026-11-05), at the 0.16.0 release
- **Context:** ADR 0001 set the gating policy and ratchets; ADR 0002
  added the `max` alias for leaves too large for `full`. The v0.16
  push ended with eighteen leaves and the `v16` rung. This records
  where each one sits, the measured sizes the ratchets hold, and how
  much budget is left, so the next milestone starts from a written
  baseline rather than from `Cargo.toml` archaeology.

## Decision

1. **Three profiles, all gated** (thumbv7em-none-eabihf, `opt-level =
   "z"`, LTO, the pinned toolchain, `ci/size_gate.py`):

   | profile | build | ratchet | budget | headroom |
   |---|---|---|---|---|
   | `tiny` | `--no-default-features` | 76,960 B | 131,072 B | 54,112 B |
   | `full` | `--features full` | 500,200 B | 524,288 B | 24,088 B |
   | `max` | `--features max` | 1,022,404 B | 1,048,576 B | 26,172 B |

   Engine statics stay 0 B in every profile (budget 8 KiB).

2. **`full`** = `indexes`, `v04` … `v10`, `v16`, `params`, `math`: the
   core dialect an embedded target with half a megabyte gets. It is
   closed to new leaves at this release: 24 KiB of headroom is kept
   for fixes to claimed paths.

3. **`max`** = `full` + `window`, `triggers`, `foreign_keys`, `json`,
   `expr_index`, `without_rowid`, `utf16`, `wal`, `datetime`,
   `attach`, `autovacuum`, `vtab`, `rtree`, `fts5`, `explain`.
   `default = ["std", "max"]`. Each leaf compiles alone on its own
   `ci/feature-sets.txt` line with only its declared dependencies.

4. **The next leaf that does not fit `max`'s remaining 26 KiB gets
   its own alias outside `max`** (an ADR names it and its budget);
   `max` is not raised past 1 MiB without one. A leaf may move from
   `max` into `full` only with a `full` budget decision.

5. **Ratchets move down freely and up only with a STATUS.md
   justification** (ADR 0001); growth past the per-profile threshold
   (tiny 4 KiB, full 16 KiB, max 32 KiB) in one change needs an
   explicit argument there.

## Consequences

- `README.md`'s profile table and `CHANGELOG.md` quote these numbers;
  a change to `ci/size-ratchet.toml` updates them.
- Embedded users choose `tiny`, `full`, or `full` plus the individual
  leaves they need; nothing in `max` leaks into `full` or `tiny`.
