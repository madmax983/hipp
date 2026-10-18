# hipp v0.1 — build status (code-start 2026-10-17)

Spec: `~/workspace/hipp-design/SPEC.md` rev. 0.3 (copied to `docs/SPEC.md`).
Local only — nothing pushed to GitHub.

## Oracle

- Pin (D1): SQLite **3.54.1**.
- 3.54.1 oracle BUILT AND VERIFIED at `~/workspace/hipp-design/oracle/sqlite3`
  (amalgamation 3540100 from sqlite.org, `--version` = 3.54.1, 2026-10-16).
  Differential results below are against the pin — **claim tests count**.
- Harness: `tests/common/mod.rs` finds the pinned oracle first
  (`HIPP_ORACLE` env overrides); `/usr/bin/sqlite3` is 3.45.1 (fallback only).

## Slices

- [x] S1 scaffold: Cargo, error, arena, record codec (commit b4f9d06)
- [x] S2 storage: async trait, MemStorage, FileStorage (std)
- [x] S3 pager: header, cache, journal protocol, recovery, commit machine (89d7ea5)
- [x] S4 btree: scan cursor, insert, splits, overflow, freelist (89d7ea5)
- [x] S5 sql + schema + session API (Connection/Statement)
- [ ] S6 examples + thumb gate measurement
- [ ] S7 acceptance tests A1–A11 (as far as honestly possible)
- [ ] S8 fmt/clippy, README, final commits

## Acceptance (§9.2)

| # | Status | Evidence |
|---|---|---|
| A1 | PASS (self-check in `tests/smoke.rs::mem_round_trip`; full A1 script in `tests/differential.rs` constants, run under A2/A3) | smoke + differential suites green 2026-10-18 |
| A2 | PASS | `tests/differential.rs::a2` — hipp file → oracle `integrity_check` ok, SELECT matches byte-for-byte |
| A3 | PASS | `tests/differential.rs::a3` — oracle file → hipp scan identical; at page size 4096 AND 512 |
| A4 | PASS | `tests/differential.rs::a4` — 500×80B rows, root byte 0x05, oracle-valid, ordered scan |
| A5 | PASS | `tests/differential.rs::a5` — 10 KiB text both directions |
| A6 | not run | crash injector pending |
| A7 | PASS | `tests/differential.rs::a7` — WalUnsupported / UnsupportedEncoding / BadMagic / TruncatedHeader / CorruptPage, no panics |
| A8 | not run | |
| A9 | not run | |
| A10 | not run | thumbv7em-none-eabihf target installed 2026-10-18 |
| A11 | not run | |

## Deviations / honest notes

- `block_on` helper lives in `hipp::storage` (core crate) rather than
  examples/hipp-std as §10 sketches — dev-tooling placement, no engine impact.
- Journal observation (empirical, vs 3.45.1 captures): a real SQLite journal
  mid-transaction has zeroed magic and nrec=0 — the header becomes hot only
  at SQLite's commit point. hipp writes its full header at `begin_write`;
  that is format-valid (header + checksummed records precede any data-page
  write either way) and strictly safer. Cross-tool recovery is still A6's
  test to prove.
- SQLite's read-side REAL-affinity forcing is implemented in the scan
  (a REAL column may store integral values with an integer serial type;
  fetch must yield Real). Found by A3, fixed same day.
- CREATE TABLE constraint grammar is accepted wider than v0.1 enforces:
  `NOT NULL`/`CHECK`/`UNIQUE`/`REFERENCES`/etc. parse (catalog reparse of
  foreign files requires it) but are NOT enforced; only `PRIMARY KEY`
  (rowid-alias rule) has semantics in v0.1. `WITHOUT ROWID` is rejected.
- Real-literal parsing is a simple ≤17-significant-digit parser; may differ
  from SQLite's exact parser by 1 ulp on pathological mantissas (R5).
- §10 layout: session API lives in `src/session.rs` (re-exported from
  lib.rs) rather than inside lib.rs itself.
