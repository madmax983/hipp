# hipp v0.1 — build status (code-start 2026-10-17, milestone complete 2026-10-18)

Spec: `~/workspace/hipp-design/SPEC.md` rev. 0.3 (copied to `docs/SPEC.md`).
Pushed to GitHub 2026-10-18 — madmax983/hipp, remote main tip 6bd49a31 (8 commits via API replay; remote SHAs differ from the local history below).

## Oracle

- Pin (D1): SQLite **3.54.1**, built from the official amalgamation
  (3540100) at `~/workspace/hipp-design/oracle/sqlite3`, `--version`
  verified 3.54.1. **All differential results below are against the
  pin — claim tests count.** (`/usr/bin/sqlite3` 3.45.1 exists only
  as a fallback the harness does not use when the pin is present.)

## Slices

- [x] S1 scaffold: Cargo, error, arena, record codec (b4f9d06)
- [x] S2 storage: async trait, MemStorage, FileStorage (std)
- [x] S3 pager: header, cache, journal protocol, recovery, commit machine (89d7ea5)
- [x] S4 btree: scan cursor, insert, splits, overflow, freelist (89d7ea5)
- [x] S5 sql + schema + session API (Connection/Statement) (072097b)
- [x] S6 examples (mem_demo, crash_writer, hipp_thumb_demo) + thumb gates
- [x] S7 acceptance tests A1–A11 (all passing — below)
- [x] S8 fmt clean (write-mode + git status), clippy `-D warnings` clean
      on all targets, README, final commits

## Acceptance (§9.2) — ALL PASS

| # | Status | Evidence |
|---|---|---|
| A1 | PASS | `tests/differential.rs::a1` — hipp creates a file; A1 script incl. rowid 9007199254740993, NULLs, 1.5, x'00ff', empty text/blob; reopen + scan identical (plus `tests/smoke.rs` MemStorage round trip) |
| A2 | PASS | `::a2` — oracle `integrity_check` = ok; oracle SELECT matches byte-for-byte |
| A3 | PASS | `::a3` — oracle-written files scanned identically at page sizes 4096 and 512 |
| A4 | PASS | `::a4` — 500×80B rows; root page byte 0x05; oracle-valid; ordered scan |
| A5 | PASS | `::a5` — 10 KiB text overflow both directions (oracle verifies length + content) |
| A6 | PASS | `tests/crash.rs` — kill at EVERY mutating-op boundary of an INSERT commit (in-process): reopened state is byte-exact pre- or post-image, never torn. Cross-tool: hipp process `exit(42)` mid-commit at every boundary → **real SQLite 3.54.1 recovers hipp's journal**, integrity ok, counts agree with hipp's view |
| A7 | PASS | `::a7` — WalUnsupported / UnsupportedEncoding / BadMagic / TruncatedHeader / CorruptPage{2}; no panics |
| A8 | PASS | `tests/fuzz_smoke.rs` — debug smoke: 23k fixture mutations + 20k parser mutations, no panic/hang. **Spec-scale pass: 10⁶ mutated inputs (probe.db, release build) in 81s, clean.** Found + fixed a real bug: cell-pointer bounds in `parse_leaf_cell` |
| A9 | PASS | `tests/arenas.rs` — tiny stmt arena → SELECT prepare `ArenaExhausted{Statement}`; tiny txn arena → INSERT `ArenaExhausted{Transaction}` with file byte-identical + oracle-valid |
| A10 | PASS | `tests/gates.rs` + thumb build (below) |
| A11 | PASS | `tests/pending.rs` — Pending injected at every I/O point (1/2/5/13 per op): outcomes byte-identical; future dropped mid-commit → prepare refused with `OperationInFlight`, `resume_txn` finishes to post-image, drop + reopen recovers to pre-image |

## §4.1 gate measurements (A10)

- Linked binary, `cargo build --profile tiny --target
  thumbv7em-none-eabihf --no-default-features --bin hipp_thumb_demo`:
  **.text 60,868 + .rodata 14,836 = 75,704 B ≤ 128 KiB budget.**
- Statics: the binary's entire .bss (98,308 B) is the demo's caller
  heap symbol; **engine .data+.bss outside caller memory = 0 B
  ≤ 8 KiB budget.**
- Tiny-config RAM (`tests/gates.rs::a10_tiny_config_ram_budget`,
  A4-shaped 500-row workload under `Config::tiny()`): pool 16 KiB +
  catalog/txn/stmt arena high-waters all inside their 16/8/16 KiB
  maxima; total ≤ 64 KiB.
- Scan loop (`::a10_scan_loop_zero_steady_state_allocations`):
  **0 global allocations** across 290 steady-state rows in a warm
  page (counting allocator); full-scan page-refill cost bounded at
  ≤6 allocations per page turned (snapshot + cell metadata).

## Tests

25 tests, all green on stable, edition 2024: 6 unit (arena/record)
+ 19 integration (smoke 2, differential 6, crash 2, arenas 2,
pending 3, gates 2, fuzz 2). Doctests: none.

## Deviations / honest notes

- `block_on` helper lives in `hipp::storage` (core crate) rather than
  examples/hipp-std as §10 sketches — dev-tooling placement, no engine impact.
- §10 layout: session API lives in `src/session.rs` (re-exported from
  lib.rs) rather than inside lib.rs itself.
- Journal observation (empirical): a real SQLite journal mid-transaction
  has zeroed magic and nrec=0 — the header becomes hot only at SQLite's
  commit point. hipp writes its full header at `begin_write`; format-valid
  and strictly safer. A6's cross-tool case proves real SQLite recovers it.
- Read-side REAL-affinity forcing implemented in the scan (a REAL column
  may store integral values under an integer serial type; fetch yields
  Real). Found by A3, fixed same day.
- CREATE TABLE constraint grammar is accepted wider than v0.1 enforces:
  `NOT NULL`/`CHECK`/`UNIQUE`/`REFERENCES` parse (foreign-file catalog
  reparse requires it) but are NOT enforced; only `PRIMARY KEY`
  (rowid-alias rule) has semantics in v0.1. `WITHOUT ROWID` is rejected.
- Real-literal parsing is a simple ≤17-significant-digit parser; may
  differ from SQLite's exact parser by 1 ulp on pathological mantissas (R5).
- Session-layer row decode allocates one small vector per `step()`
  (capacity = column count); the zero-allocation gate above is stated
  at the cursor layer, where the scan loop lives.
- `finish_root_split` in btree.rs is a defensive `Unsupported` path:
  splits are resolved level-by-level with the root case handled inline
  (`is_root`), so it is unreachable by construction; A4 exercises root
  splits of both leaf and interior roots.
- Fuzz corpus is fixture-mutation only (no structure-aware generation);
  10⁶-input pass used the small fixture (many.db mutations run at
  smoke scale, 3k, because deep mutated scans are slow in debug).
