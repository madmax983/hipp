# CLAUDE.md: working on hipp

hipp is a `no_std` + `alloc`, zero-dependency Rust database engine that reads
and writes SQLite 3 files, with SQL behaviour checked against a pinned
SQLite **3.54.1** oracle. Start from [`README.md`](README.md). The library
usage guide is [`docs/GUIDE.md`](docs/GUIDE.md) and the per-profile
reference is [`docs/profiles/`](docs/profiles/README.md).

## Layout

- `src/`: the core crate. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) has
  the module map. `src/session.rs` (Connection / Statement / planner /
  executor) is the centre, and `src/session/*` holds its feature-gated parts.
- `hipp-cli/`: the `sqlite3`-compatible shell (`hipp` binary), used for
  differential tests.
- `tests/`: integration and differential tests, one file per unit
  (`*_v16.rs`, `*_v17.rs`). `tests/common` drives the oracle.
- `examples/`: `tiny.rs`, `full.rs`, `max.rs`, `xl.rs` (one per profile,
  self-checking), `custom_storage.rs`, `file_db.rs`, `params.rs`,
  `mem_demo.rs`, `bolt_workload.rs` (profiling), `crash_writer.rs` (driven by
  `tests/crash.rs`). Shared helpers are in `examples/common/mod.rs`.
- `ci/`: gate scripts (`local-gates.sh`, `run-examples.sh`, `size_gate.py`,
  `build-oracle.sh`, `coverage.sh`), `size-ratchet.toml`, `feature-sets.txt`.
- `docs/`: `GUIDE.md`, `profiles/`, `ARCHITECTURE.md`, `SPEC.md` (the
  original design, a historical record), `RISKS.md`, `adr/`, `HISTORY.md`,
  `perf-baselines.md`.
- `STATUS.md`: the build log. Each unit of work records its evidence, size
  deltas and honest gaps there. It is large, so grep it rather than read it.
- `CHANGELOG.md`: user-facing changes, under "Unreleased" until a release.

## Profiles and features

`tiny` (no features) ⊂ `full` ⊂ `max` ⊂ `xl` (the default, with `std`).
Each is size-gated on thumbv7em (`ci/size-ratchet.toml`). Features name
capabilities (ADR 0006), and each one's comment in `Cargo.toml` says what it
adds and what a build without it refuses. Gate code with
`#[cfg(feature = "...")]`, and add a line to `ci/feature-sets.txt` for a new
leaf.

## Commands

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
ci/run-examples.sh
```

```bash
ci/local-gates.sh --quick
```

The oracle tests need `HIPP_ORACLE` or `../hipp-design/oracle/sqlite3`
(`ci/build-oracle.sh` builds it). Python's `sqlite3` module (3.53.x) works
as a quick sanity check when the pin is unavailable, but it is not the pin.

## Rules

- **Parity:** match SQLite 3.54.1's answers, row order, errors and file
  bytes. Probe the pin before changing behaviour. A silent wrong answer is
  fixed ungated, in every profile (ADR 0001).
- **Refuse, never misread:** a build without a feature returns a parse error,
  `Error::Unsupported` or a named error. It never skips a structure or reads a
  file it might misread.
- **Size:** a change that shrinks a profile lowers its ratchet in the same
  commit. Growth needs a ratchet comment, and `STATUS.md` justification past
  `justify_over`. Tiny does not grow for dialect work.
- **No panics** on caller input or exhausted memory: return an `Error`. No new
  `unsafe` outside `arena`.
- **Docs move with code:** a new or changed capability updates its profile
  page and, for SQL, a `check` in that profile's example (CI runs the
  examples, so the documented output stays true). API changes update
  `docs/GUIDE.md`. Every change gets a `CHANGELOG.md` entry. See
  [`CONTRIBUTING.md`](CONTRIBUTING.md).
- The toolchain is pinned (1.97.0). Never set `RUSTFLAGS` globally.
- The `vm` feature is hidden and frozen (ADR 0005 / 0007). It is in no
  profile.
