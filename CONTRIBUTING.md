# Contributing to hipp

## Toolchain

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.97.0, with the
`thumbv7em-none-eabihf` target). The size ratchets depend on that exact
codegen, so a toolchain bump re-baselines `ci/size-ratchet.toml` in the same
commit. Do not set `RUSTFLAGS` globally: it would replace the thumbv7em
linker-script flags in `.cargo/config.toml`.

## The oracle

The differential tests compare hipp against SQLite **3.54.1** (decision D1).
`ci/build-oracle.sh DIR` downloads the official amalgamation, verifies its
SHA3, and builds `DIR/sqlite3`. Tests find it at
`../hipp-design/oracle/sqlite3` or through `HIPP_ORACLE`:

```bash
ci/build-oracle.sh ../hipp-design/oracle
```

```bash
HIPP_ORACLE=/path/to/sqlite3 cargo test --workspace
```

## The gates

CI runs these jobs (`.github/workflows/ci.yml`), and `ci/local-gates.sh` runs
them locally in order (`--quick` skips the full test suite):

| gate | command |
|---|---|
| format | `cargo fmt --all -- --check` |
| lint | `cargo clippy --workspace --all-targets -- -D warnings`, also with `--all-features` |
| feature matrix | every line of `ci/feature-sets.txt` builds warning-free with `--no-default-features --features <set>`, and the lib unit tests run in tiny / full / max / xl |
| examples | `ci/run-examples.sh`: every example in its own profile and every larger one, plus the tokio / smol / embassy examples in `runtimes/` |
| tests | `cargo test --workspace` against the oracle, and again with `--features vm` |
| no_std | the library builds for thumbv7em, thumbv6m, riscv32imac and wasm32 in every profile |
| size | `ci/size_gate.py` checks each profile's `hipp_thumb_demo` against its ratchet and budget |
| miri | the arena's `unsafe`, and an end-to-end engine run |
| coverage | `ci/coverage.sh --fail-under 80` |

```bash
ci/local-gates.sh --quick
```

## Size ratchets

`ci/size-ratchet.toml` holds a `ratchet` (the most code each profile may
have, measured on thumbv7em with `--profile tiny`) and a hard `budget`.

- A change that **shrinks** a profile lowers its ratchet in the same commit.
  CI prints the new figure.
- A change that **grows** a profile raises the ratchet, with a comment saying
  why. Growth past `justify_over` bytes needs a justification in `STATUS.md`.
  The tiny profile does not grow for dialect work (ADR 0001).

To measure a profile:

```bash
cargo build --profile tiny --target thumbv7em-none-eabihf --no-default-features --features full --bin hipp_thumb_demo
```

```bash
python3 ci/size_gate.py --section full target/thumbv7em-none-eabihf/tiny/hipp_thumb_demo
```

## Features and profiles

- Features name capabilities (ADR 0006). A new leaf gets a feature that
  implies only what it needs, a comment in `Cargo.toml` saying what it adds
  and what a build without it does, and a line in `ci/feature-sets.txt`.
  It joins `max` or `xl` by size (ADR 0002 / 0004).
- A build without a feature **refuses** what the feature would add: a parse
  error, `Unsupported`, or a named error at open. It never misreads a file
  or skips a structure silently.
- A fix for a silent wrong answer goes in ungated, in every profile
  (ADR 0001).

## Tests

- No behaviour ships without a test. Differential tests (`tests/common`)
  compare hipp's rows, errors and files with the oracle's.
- Each unit of work has its own test file (`tests/<unit>_v17.rs`, with a CLI
  counterpart under `hipp-cli/tests/` where the shell's output matters).
- `tests/crash.rs` injects a crash at every write of a commit. Changes to the
  pager or journal must keep it passing.

## Documentation

Keep the docs true to the code in the same change:

- **A new capability** gets an entry in its profile page
  (`docs/profiles/<profile>.md`). If it is user-visible SQL, it also gets a
  check in that profile's example (`examples/<profile>.rs`), using `check` /
  `plan` / `fails` from `examples/common`, so CI verifies the output the docs
  show. Paste the output from a run of the example into the page.
- **A changed API** updates `docs/GUIDE.md` and the rustdoc.
- **Every change** gets a `CHANGELOG.md` entry under "Unreleased". A unit of
  work records its evidence, sizes and honest gaps in `STATUS.md`.
- **A significant decision** gets an ADR in `docs/adr/`.

`HIPP_EXAMPLE_KEEP_GOING=1 cargo run --example ...` reports every mismatch in
an example instead of stopping at the first. That helps when you edit one.

## Style

- `cargo fmt`, and clippy with `-D warnings`.
- Every failure a caller can cause is a remedy-named `Error` variant, never
  a panic. Avoid `unwrap()` / `expect()` in library code. Where one stays,
  the invariant it relies on should be evident from the code or a comment.
- `unsafe` is confined to `arena` (and the no-op waker in
  `storage::block_on`). `#![deny(unsafe_code)]` holds everywhere else.
- Doc comments on public items.
