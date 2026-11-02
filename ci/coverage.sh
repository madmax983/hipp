#!/usr/bin/env bash
# Line/region coverage of the core crate (src/) under the whole workspace
# test suite, using only the pinned toolchain's llvm-tools component
# (rustup component add llvm-tools) — no coverage driver to drift out of
# step with the compiler's profile format.
#
#   ci/coverage.sh                 report + target/coverage/lcov.info
#   ci/coverage.sh --fail-under N  also fail below N% line coverage
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

fail_under=""
if [[ "${1:-}" == "--fail-under" ]]; then fail_under="${2:?percent}"; shift 2; fi

host="$(rustc -vV | sed -n 's/^host: //p')"
tools="$(rustc --print sysroot)/lib/rustlib/${host}/bin"
[[ -x "${tools}/llvm-profdata" ]] || { echo "llvm-tools missing: rustup component add llvm-tools" >&2; exit 1; }

out="${PWD}/target/coverage"
rm -rf "${out}/profraw"
mkdir -p "${out}/profraw"
export CARGO_TARGET_DIR="${out}/build"
# Host-only build, so overriding RUSTFLAGS cannot drop the thumbv7em
# linker flags from .cargo/config.toml.
export RUSTFLAGS="-C instrument-coverage"
export LLVM_PROFILE_FILE="${out}/profraw/%p-%m.profraw"

skip=()
oracle="${HIPP_ORACLE:-../hipp-design/oracle/sqlite3}"
if ! "${oracle}" --version 2>/dev/null | grep -q '^3\.54\.1 '; then
  skip=(-- --skip literal_bits_match_oracle_on_corpus)
fi
cargo test --workspace --no-fail-fast "${skip[@]}"

"${tools}/llvm-profdata" merge -sparse "${out}"/profraw/*.profraw -o "${out}/hipp.profdata"

# llvm-cov takes the first binary positionally, the rest via -object.
objects=()
while IFS= read -r exe; do
  if [[ ${#objects[@]} -eq 0 ]]; then objects+=("${exe}"); else objects+=(-object "${exe}"); fi
done < <(
  cargo test --workspace --no-run --message-format=json 2>/dev/null |
    python3 -c 'import json,sys
seen=set()
for line in sys.stdin:
    try: m=json.loads(line)
    except ValueError: continue
    exe=m.get("executable")
    if m.get("reason")=="compiler-artifact" and exe and exe not in seen:
        seen.add(exe); print(exe)')

ignore='(/\.cargo/|/rustc/|/rustlib/|/tests/|/examples/|hipp-cli/|src/bin/)'
"${tools}/llvm-cov" report --summary-only --instr-profile "${out}/hipp.profdata" \
  --ignore-filename-regex="${ignore}" "${objects[@]}" | tee "${out}/summary.txt"
"${tools}/llvm-cov" export --format=lcov --instr-profile "${out}/hipp.profdata" \
  --ignore-filename-regex="${ignore}" "${objects[@]}" > "${out}/lcov.info"

if [[ -n "${fail_under}" ]]; then
  pct="$(awk '/^TOTAL/ {print $(NF-3)}' "${out}/summary.txt" | tr -d '%')"
  awk -v p="${pct}" -v f="${fail_under}" 'BEGIN { exit !(p + 0 >= f + 0) }' ||
    { echo "line coverage ${pct}% is below ${fail_under}%" >&2; exit 1; }
fi
