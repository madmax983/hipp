#!/usr/bin/env python3
"""Size gate + ratchet for the tiny profile (SPEC §4.1).

    python3 ci/size_gate.py <elf> [--ratchet ci/size-ratchet.toml]
                            [--section tiny] [--report-only]

Parses ELF32/ELF64 section and symbol tables with the standard library
only (no binutils dependency), sums .text + .rodata, computes engine
statics as .data + .bss minus the demo allocator's static heap (the
`ALLOC` symbol in hipp_thumb_demo: caller-supplied memory, not engine
statics), and enforces the hard budget and the ratchet.
"""
import argparse
import os
import struct
import sys
import tomllib


def read_elf(path):
    data = open(path, "rb").read()
    if data[:4] != b"\x7fELF":
        sys.exit(f"{path}: not an ELF file")
    is64 = data[4] == 2
    end = "<" if data[5] == 1 else ">"
    if is64:
        shoff, = struct.unpack_from(end + "Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from(end + "HHH", data, 0x3A)
    else:
        shoff, = struct.unpack_from(end + "I", data, 0x20)
        shentsize, shnum, shstrndx = struct.unpack_from(end + "HHH", data, 0x2E)
    secs = []
    for i in range(shnum):
        o = shoff + i * shentsize
        if is64:
            name, typ, _fl, addr, off, size, link, _inf, _al, entsize = struct.unpack_from(
                end + "IIQQQQIIQQ", data, o)
        else:
            name, typ, _fl, addr, off, size, link, _inf, _al, entsize = struct.unpack_from(
                end + "IIIIIIIIII", data, o)
        secs.append(dict(name=name, type=typ, addr=addr, off=off, size=size,
                         link=link, entsize=entsize))
    strtab = secs[shstrndx]

    def cstr(base, idx):
        e = data.index(b"\0", base + idx)
        return data[base + idx:e].decode("utf-8", "replace")

    for s in secs:
        s["name"] = cstr(strtab["off"], s["name"])
    syms = []
    for s in secs:
        if s["type"] != 2:  # SHT_SYMTAB
            continue
        names = secs[s["link"]]
        n = s["size"] // s["entsize"]
        for i in range(n):
            o = s["off"] + i * s["entsize"]
            if is64:
                nm, _info, _oth, shndx, val, size = struct.unpack_from(end + "IBBHQQ", data, o)
            else:
                nm, val, size, _info, _oth, shndx = struct.unpack_from(end + "IIIBBH", data, o)
            syms.append((cstr(names["off"], nm), size, shndx))
    return secs, syms


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("elf")
    ap.add_argument("--ratchet", default=os.path.join(os.path.dirname(__file__), "size-ratchet.toml"))
    ap.add_argument("--section", default="tiny")
    ap.add_argument("--report-only", action="store_true")
    a = ap.parse_args()

    secs, syms = read_elf(a.elf)
    size = {s["name"]: s["size"] for s in secs}
    text, rodata = size.get(".text", 0), size.get(".rodata", 0)
    data, bss = size.get(".data", 0), size.get(".bss", 0)
    heap = sum(sz for (nm, sz, _) in syms if "hipp_thumb_demo" in nm and "ALLOC" in nm)
    flash = text + rodata
    statics = data + bss - heap

    lines = [
        f"| metric | bytes |",
        f"|---|---|",
        f"| .text | {text:,} |",
        f"| .rodata | {rodata:,} |",
        f"| **.text + .rodata** | **{flash:,}** |",
        f"| .data + .bss | {data + bss:,} |",
        f"| demo heap (excluded) | {heap:,} |",
        f"| **engine statics** | **{statics:,}** |",
    ]
    failures = []
    notes = []
    if not a.report_only:
        cfg = tomllib.load(open(a.ratchet, "rb"))[a.section]
        ratchet, budget = cfg["ratchet"], cfg["budget"]
        sbudget, justify = cfg["statics_budget"], cfg["justify_over"]
        delta = flash - ratchet
        lines.append(f"| ratchet | {ratchet:,} (Δ {delta:+,}) |")
        lines.append(f"| hard budget | {budget:,} (headroom {budget - flash:,}) |")
        if flash > budget:
            failures.append(f"code size {flash:,} B exceeds the {budget:,} B hard budget")
        if statics > sbudget:
            failures.append(f"engine statics {statics:,} B exceed the {sbudget:,} B budget")
        if delta > 0:
            failures.append(
                f"code size {flash:,} B is {delta:+,} B over the ratchet ({ratchet:,} B). "
                f"Shrink it, or raise `ratchet` in {a.ratchet} with a justification"
                + (f" (growth over {justify:,} B needs a STATUS.md entry)" if delta > justify else ""))
        elif delta < 0:
            notes.append(f"size dropped {-delta:,} B: lower `ratchet` to {flash} in {a.ratchet} to lock it in")

    report = "\n".join(lines)
    print(report)
    for n in notes:
        print(f"note: {n}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(f"### Size gate: `{a.section}` ({os.path.basename(a.elf)})\n\n{report}\n\n")
            for n in notes:
                f.write(f"> {n}\n\n")
            for x in failures:
                f.write(f"> **FAIL:** {x}\n\n")
    for x in failures:
        print(f"FAIL: {x}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
