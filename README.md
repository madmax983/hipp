# hipp

**It's Hipp to be `no_std`.**

hipp is a little database engine in Rust that **reads and writes
SQLite database files**. It gives SQLite the treatment Horton got:
`no_std` + `alloc`, arena-first, built to run anywhere LLVM targets
— microcontrollers included — with hard size and RAM gates, not
aspirations.

- **Phase:** v0.2 — workspace + CLI + first dialect growth on the
  v0.1 file-format foundation (SPEC §2, §9.4).
- **Pinned claim version:** SQLite **3.54.1** (decision D1). The
  differential suite in `tests/` runs against a locally built
  3.54.1 oracle; `PRAGMA integrity_check` passes on hipp-written
  files, and real SQLite recovers hipp's rollback journals.
- **Profile measured:** `tiny` profile (`opt-level = "z"`, LTO) on
  `thumbv7em-none-eabihf`.

## What v0.2 does

- `Connection::open` over any async `Storage` (hipp's own
  runtime-agnostic trait: `async fn`, no executor, no tokio, no
  dependencies — the caller brings the runtime: tokio, embassy,
  smol, or a trivial `block_on`).
- `CREATE TABLE` (rowid tables, `INTEGER PRIMARY KEY` alias,
  `IF NOT EXISTS`), multi-row `INSERT ... VALUES (...), (...)`,
  full-scan `SELECT` in rowid order plus `WHERE rowid = <literal>`
  point lookups, streamed via `step()`.
- Explicit transactions: `BEGIN` / `COMMIT` / `ROLLBACK` over the
  resumable commit machine.
- Table B-trees with leaf/interior/root splits, overflow chains
  (10 KiB+ values), freelist reuse, rollback-journal transactions
  byte-compatible with SQLite's format — including hot-journal
  recovery on open, and resumable commits: a future dropped
  mid-commit is finished by `resume_txn()` or recovered by the next
  open, never torn.
- Three arena scopes (statement / transaction / catalog) with
  caller-configured maxima; exhaustion is
  `Error::ArenaExhausted`, never a panic.

## Measured gates (SPEC §4.1, tiny profile)

| Gate | Budget | Measured (v0.2) |
|---|---|---|
| Linked `.text` + `.rodata` (thumbv7em) | ≤ 128 KiB | **80,784 B** (Δ +5,080 B vs v0.1's 75,704 B) |
| Engine statics (`.data` + `.bss`, excl. caller heap) | ≤ 8 KiB | **0 B** |
| Engine RAM, tiny config (pool + arenas) | ≤ 64 KiB | within (see `tests/gates.rs`) |
| Steady-state scan allocations after warm-up | 0 | **0** (`tests/gates.rs`) |

Reproduce: `cargo build --profile tiny --target
thumbv7em-none-eabihf --no-default-features --bin hipp_thumb_demo`
and `cargo test --workspace`.

## hipp-cli

A std binary shell (`hipp-cli/`, decision D4): interactive,
stdin-batch and one-shot modes with list-format output
byte-diffable against the pinned oracle CLI, plus `.tables`,
`.schema`, `.dump`, `.open`, `.exit`. Its first job is
differential testing. It is a separate crate and contributes
0 bytes to the core's tiny build.

## Non-goals (SPEC §3)

No WAL (files in WAL mode are rejected with `WalUnsupported`), no
indexes (v0.3), no general `WHERE` (only `rowid = <literal>`),
no `ORDER BY`/joins/aggregates, no bound parameters, no
`UPDATE`/`DELETE`/`DROP`, no UTF-16, no multi-process locking —
and **no `sqlite3_*` C ABI, ever** (D3): hipp is a Rust library,
not a drop-in replacement.

## Layout

A workspace: the zero-dependency core crate at the root plus
`hipp-cli/`. Core `src/`: `arena` (scoped bump arenas — the only
`unsafe`), `storage` (async trait + Mem/File impls), `pager`
(page cache, journal protocol, commit machine), `btree`,
`record` (serial codec, affinity), `schema` (catalog), `sql/`
(lexer, parser, AST), `session` (Connection/Statement, planner,
tree-walking executor), `error` (remedy-named errors).
Design: `docs/SPEC.md` (rev. 0.3) and `docs/RISKS.md`. Build
status and acceptance evidence: `STATUS.md`.

## License

Apache-2.0.
