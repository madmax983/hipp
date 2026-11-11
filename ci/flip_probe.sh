#!/usr/bin/env bash
# The Stage 2 flip probe (ADR 0005, unit `vm_shrink`): what the full /
# max / xl profiles would measure with the bytecode VM as the only
# SELECT path. Build-only: `--cfg hipp_flip_probe` (with the hidden
# `vm` feature) cuts today's read executor off at the entry points the
# wave-13 measurement cut (`Statement::step_query`'s streamed and
# materialising executors, `Statement::exec_subplan_need`), so LTO
# drops it; the binaries are never run. A best case: the VM-backed
# replacements the write paths would still need are not counted.
#
#   ci/flip_probe.sh [profile ...]    (default: full max xl)
#
# Same target, profile, binary and measure as the size gates
# (thumbv7em, `--profile tiny`, `hipp_thumb_demo`, .text + .rodata),
# in its own target directory (`target/flip-probe`, override with
# FLIP_PROBE_TARGET_DIR) so the cfg does not churn the gates' builds.
# Prints each probe next to its profile's ratchet in
# ci/size-ratchet.toml. Not wired into CI or ci/local-gates.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="${FLIP_PROBE_TARGET_DIR:-target/flip-probe}"
elf="$dir/thumbv7em-none-eabihf/tiny/hipp_thumb_demo"
profiles=("$@")
[ ${#profiles[@]} -eq 0 ] && profiles=(full max xl)

ratchet() {
    python3 -B - "$1" <<'EOF'
import sys, tomllib
print(tomllib.load(open("ci/size-ratchet.toml", "rb")).get(sys.argv[1], {}).get("ratchet", ""))
EOF
}

measure() {
    python3 -B - "$1" <<'EOF'
import sys
sys.path.insert(0, "ci")
import size_gate
secs, _ = size_gate.read_elf(sys.argv[1])
print(sum(s["size"] for s in secs if s["name"] in (".text", ".rodata")))
EOF
}

printf '| profile | ratchet | flip probe | probe - ratchet |\n|---|---:|---:|---:|\n'
for p in "${profiles[@]}"; do
    # RUSTFLAGS replaces .cargo/config.toml's target rustflags, so the
    # linker script is passed again.
    RUSTFLAGS="-C link-arg=-Tlink.x --cfg hipp_flip_probe" CARGO_TARGET_DIR="$dir" \
        cargo build -q --profile tiny --target thumbv7em-none-eabihf \
        --no-default-features --features "$p,vm" --bin hipp_thumb_demo
    size=$(measure "$elf")
    r=$(ratchet "$p")
    if [ -n "$r" ]; then
        printf '| %s | %s | %s | %+d |\n' "$p" "$r" "$size" "$((size - r))"
    else
        printf '| %s | - | %s | - |\n' "$p" "$size"
    fi
    if [ -n "${FLIP_PROBE_KEEP:-}" ]; then cp "$elf" "$FLIP_PROBE_KEEP/probe-$p.elf"; fi
done
