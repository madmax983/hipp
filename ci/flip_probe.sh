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
#   ci/flip_probe.sh [--s3] [profile ...]    (default: full max xl)
#
# `--s3` (ADR 0005 Stage 3, unit `s3_probe`): adds `--cfg
# hipp_s3_probe`, which keeps the cuts above and also cuts what Stage 3
# deletes, at its entry points, so LTO drops what only they reach:
#   readers   the async source readers the VM reaches through
#             `Exec::Call` (`vm_call` / `vm_host_next`: TEMP, attached,
#             WITHOUT ROWID, automatic-index, access-path, vtab / TVF,
#             R*Tree, fts5, json_each / json_tree, dbstat, pragma TVFs);
#   triggers  the trigger runner (`fire_as`, `replace_victim`,
#             `insert_prefix`, `trigger_failed`, `step_instead`);
#   fk        the foreign-key runner (`fk_new_row`, `fk_old_row`,
#             `fk_actions`, the compile replay `fk_sim`);
#   writes    the write executors (`step_insert` / `step_update` /
#             `step_delete`: INSERT VALUES and SELECT, UPDATE incl.
#             FROM, DELETE, upsert, RETURNING, the OR-conflict paths),
#             keeping the B-tree write primitives a VM write opcode
#             calls (`Statement::s3_write_primitives`).
# DDL, PRAGMA, VACUUM, ALTER and integrity_check keep what they call.
# `--s3=<cat>[,<cat>...]` cuts only those categories (the per-category
# inventory: `--cfg hipp_flip_probe --cfg hipp_s3_probe="<cat>"`). The
# best case of the combined Stage 2 + 3 flip: the VM write codegen,
# opcodes, trigger / FK subprograms and sync source readers replacing
# the cut code are not counted (STATUS.md, "ADR 0005 Stage 3:
# s3_probe", estimates them). Own target directory
# (`target/flip-probe-s3`).
#
set -euo pipefail
cd "$(dirname "$0")/.."

cfgs="--cfg hipp_flip_probe"
label="flip probe"
dflt=target/flip-probe
case "${1:-}" in
--s3)
    shift
    cfgs="--cfg hipp_s3_probe"
    label="s3 probe"
    dflt=target/flip-probe-s3
    ;;
--s3=*)
    cats="${1#--s3=}"
    shift
    for c in ${cats//,/ }; do
        case "$c" in
        readers | triggers | fk | writes) cfgs="$cfgs --cfg hipp_s3_probe=\"$c\"" ;;
        *) echo "flip_probe.sh: unknown --s3 category: $c" >&2; exit 2 ;;
        esac
    done
    label="s3 probe ($cats)"
    dflt=target/flip-probe-s3
    ;;
esac
dir="${FLIP_PROBE_TARGET_DIR:-$dflt}"
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

printf '| profile | ratchet | %s | probe - ratchet |\n' "$label"
printf '|---|---:|---:|---:|\n'
for p in "${profiles[@]}"; do
    # RUSTFLAGS replaces .cargo/config.toml's target rustflags, so the
    # linker script is passed again.
    RUSTFLAGS="-C link-arg=-Tlink.x $cfgs" CARGO_TARGET_DIR="$dir" \
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
