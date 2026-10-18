# hipp v0.1 — build status (code-start 2026-10-17)

Spec: `~/workspace/hipp-design/SPEC.md` rev. 0.3 (copied to `docs/SPEC.md`).
Local only — nothing pushed to GitHub.

## Oracle

- Pin (D1): SQLite **3.54.1**. Local `/usr/bin/sqlite3` is **3.45.1** — pre-pin.
- 3.54.1 oracle build: IN FLIGHT (subagent, `~/workspace/hipp-design/oracle/`).
- Until the 3.54.1 oracle verifies, ALL differential results are marked
  **"pre-pin oracle — claim tests do not count yet"**.

## Slices

- [ ] S1 scaffold: Cargo, error, arena, record codec
- [ ] S2 storage: async trait, MemStorage, FileStorage (std)
- [ ] S3 pager: header, cache, journal protocol, recovery, commit machine
- [ ] S4 btree: scan cursor, insert, splits, overflow, freelist
- [ ] S5 sql + schema + session API (Connection/Statement)
- [ ] S6 examples + thumb gate measurement
- [ ] S7 acceptance tests A1–A11 (as far as honestly possible)
- [ ] S8 fmt/clippy, README, final commits

## Acceptance (§9.2)

| # | Status | Evidence |
|---|---|---|
| A1 | not run | |
| A2 | not run | |
| A3 | not run | |
| A4 | not run | |
| A5 | not run | |
| A6 | not run | |
| A7 | not run | |
| A8 | not run | |
| A9 | not run | |
| A10 | not run | |
| A11 | not run | |

## Deviations / honest notes

- (none yet)
