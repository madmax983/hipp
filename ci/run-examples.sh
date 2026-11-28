#!/usr/bin/env bash
# Run every example in every profile it belongs to (CI job
# `examples`; also handy locally). Each profile example checks its
# own output and panics on any wrong answer, so a zero exit means
# every documented result in docs/profiles/*.md still holds.
#
#   ci/run-examples.sh            # every profile, then the runtimes
#   ci/run-examples.sh tiny full  # just these profiles
#   ci/run-examples.sh runtimes   # just the tokio / smol / embassy examples
#
# A profile's examples are its own plus every smaller profile's: a
# larger build must answer the smaller profiles' SQL identically.
set -euo pipefail
cd "$(dirname "$0")/.."

profiles=("$@")
[[ ${#profiles[@]} -eq 0 ]] && profiles=(tiny full max xl default runtimes)

# Examples per profile (cumulative), and the cargo feature flags.
examples_for() {
  case "$1" in
    tiny) echo "tiny custom_storage mem_demo" ;;
    full) echo "$(examples_for tiny) full params bolt_workload" ;;
    max) echo "$(examples_for full) max" ;;
    xl) echo "$(examples_for max) xl" ;;
    default) echo "$(examples_for xl) file_db crash_writer" ;;
    # The async-runtime examples live in their own crate (runtimes/),
    # so the core keeps zero dependencies, dev-dependencies included.
    runtimes) echo "tokio smol embassy" ;;
    *) echo "unknown profile: $1" >&2; exit 2 ;;
  esac
}
flags_for() {
  case "$1" in
    tiny) echo "--no-default-features" ;;
    full | max | xl) echo "--no-default-features --features $1" ;;
    default) echo "" ;;
    runtimes) echo "-p hipp-runtimes" ;;
  esac
}

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

for p in "${profiles[@]}"; do
  # shellcheck disable=SC2046 # word-splitting the flags is intended
  for ex in $(examples_for "$p"); do
    echo "::group::[${p}] example ${ex}"
    case "${ex}" in
      bolt_workload)
        # A profiling workload; a small n keeps it quick.
        cargo run -q --example "${ex}" $(flags_for "$p") -- 200 ;;
      crash_writer)
        # The crash-injection harness tests/crash.rs drives: set up a
        # database, then commit a second row without a crash.
        cargo run -q --example "${ex}" $(flags_for "$p") -- "${tmp}/crash.db"
        MODE=insert cargo run -q --example "${ex}" $(flags_for "$p") -- "${tmp}/crash.db" ;;
      *)
        cargo run -q --example "${ex}" $(flags_for "$p") ;;
    esac
    echo "::endgroup::"
  done
done
echo "all examples passed: ${profiles[*]}"
