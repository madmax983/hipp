#!/usr/bin/env bash
# Run the CI gates locally, in CI order:
#   fmt -> clippy -> feature matrix -> tests -> no_std targets -> size ratchet
#
#   ci/local-gates.sh            all gates
#   ci/local-gates.sh --quick    skip the full test suite
#
# Tests use the pinned oracle at ../hipp-design/oracle/sqlite3 (build it
# with ci/build-oracle.sh) or $HIPP_ORACLE.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
quick=0
[[ "${1:-}" == "--quick" ]] && quick=1

step() { printf '\n==> %s\n' "$*"; }

step "fmt"
cargo fmt --all -- --check

step "clippy (default, all targets)"
cargo clippy --workspace --all-targets -- -D warnings
step "clippy (all features, all targets)"
cargo clippy --workspace --all-targets --all-features -- -D warnings

step "feature matrix"
while IFS= read -r line; do
  [[ -z "${line}" || "${line}" == \#* ]] && continue
  f="${line}"; [[ "${f}" == "-" ]] && f=""
  echo "  --features '${f}'"
  cargo clippy -q --lib --no-default-features --features "${f}" -- -D warnings
done < ci/feature-sets.txt
cargo test -q --lib --no-default-features
cargo test -q --lib --no-default-features --features full

if [[ "${quick}" == 0 ]]; then
  step "tests (workspace)"
  oracle="${HIPP_ORACLE:-../hipp-design/oracle/sqlite3}"
  skip=()
  if ! "${oracle}" --version 2>/dev/null | grep -q '^3\.54\.1 '; then
    # Local convenience only: CI always runs against the real pin.
    echo "  WARNING: oracle at ${oracle} is not the 3.54.1 pin;"
    echo "  skipping the pin-only test literal_bits_match_oracle_on_corpus."
    skip=(-- --skip literal_bits_match_oracle_on_corpus)
  fi
  cargo test --workspace --no-fail-fast "${skip[@]}"
fi

step "no_std targets"
for t in thumbv7em-none-eabihf thumbv6m-none-eabi riscv32imac-unknown-none-elf wasm32-unknown-unknown; do
  if rustup target list --installed | grep -qx "${t}"; then
    cargo build -q --lib --release --target "${t}" --no-default-features
    cargo build -q --lib --release --target "${t}" --no-default-features --features full
    echo "  ${t}: ok"
  else
    echo "  ${t}: target not installed, skipped (rustup target add ${t})"
  fi
done

step "size gate: tiny"
cargo build -q --profile tiny --target thumbv7em-none-eabihf --no-default-features --bin hipp_thumb_demo
python3 ci/size_gate.py --section tiny target/thumbv7em-none-eabihf/tiny/hipp_thumb_demo
step "size gate: full"
cargo build -q --profile tiny --target thumbv7em-none-eabihf --no-default-features --features full --bin hipp_thumb_demo
python3 ci/size_gate.py --section full target/thumbv7em-none-eabihf/tiny/hipp_thumb_demo
step "all gates passed"
