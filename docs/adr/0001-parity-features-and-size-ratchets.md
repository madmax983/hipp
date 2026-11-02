# ADR 0001 — SQLite-parity features under size ratchets

- **Status:** accepted (2026-11-02)
- **Context:** v0.15 matched the pinned oracle (SQLite 3.54.1) on a
  narrow dialect. A census of 158 probe statements against the pin
  found 53 matching, 88 refused, and 8 *silently wrong* (RIGHT/FULL
  and NATURAL joins, `NULLS FIRST/LAST`, CHECK, expression
  DEFAULTs, generated columns, column COLLATE). The goal is to close
  as much of that gap as possible while keeping the `tiny` profile
  inside its ratchet and the whole core `no_std` + zero-dependency.

## Decision

1. **Silent wrong answers are fixed first, ungated.** A statement the
   claimed grammar already accepts must either produce the pin's
   answer or be refused with a remedy-named error. Fixes to claimed
   paths are not feature-gated (the v0.8/v0.10/v0.12 precedent).
2. **New dialect surface is feature-gated** and compiles out:
   - `v16` — a new ladder rung (implies `v10`) for core-SQL
     completion: join/ordering completion, constraints, collations,
     operators, the scalar/aggregate function library, PRAGMAs and
     `sqlite_schema`, CTEs and executed views, savepoints, ALTER
     TABLE completion, AUTOINCREMENT, maintenance statements.
   - **Leaf features** for separable subsystems, each depending only
     on what it needs: `params` (bound parameters), `math`,
     `datetime`, `json`, `window`, `triggers`, `foreign_keys`, and
     later storage features. Leaves never depend on each other
     unless declared in `Cargo.toml`; code touching two leaves uses
     `cfg(all(...))`.
   - Everything joins `full` (the full `no_std` profile); `default`
     is `["std", "full"]`.
3. **Ratchets are CI gates** (`ci/size_gate.py`, `ci/size-ratchet.toml`):
   - `tiny` (`--no-default-features`, thumbv7em, `opt-level = "z"`,
     LTO): fails above `ratchet`; hard budget 128 KiB; engine
     statics ≤ 8 KiB. Parity work must not grow it — gate the code.
   - `full` (`--features full`): ratcheted too, so dialect growth is
     a reviewed number in the diff, never a surprise.
   - The toolchain is pinned (`rust-toolchain.toml`) so the numbers
     are reproducible.
4. **Everywhere means everywhere:** the core builds for
   thumbv7em, thumbv6m (no atomic read-modify-write), riscv32imac
   and wasm32 in both profiles. No global mutable state, no atomics
   beyond load/store, no `std`, no float-math intrinsics from `std`
   (math functions are implemented in-crate), no clock: wall-clock
   features (`'now'`, `CURRENT_TIMESTAMP`) read a caller-supplied
   `Config` hook and error cleanly without one.
5. **Every feature ships with differential tests** against the pin
   (core `tests/*.rs` and, where output formatting matters, CLI
   byte-identical tests in `hipp-cli/tests/`), and every feature set
   in `ci/feature-sets.txt` compiles warning-free.

## Consequences

- The feature matrix grows linearly with leaves (each alone, plus
  `full`), not combinatorially.
- A build names its profile; docs say which features a claim covers.
- `ci/local-gates.sh` runs the same gates locally.
