# ADR 0007 — A smaller VM

- **Status:** proposed (2026-11-12, wave 16, unit `slim_design`).
  **Verdict: stop in full, max and xl** (below): the design is
  recorded, measured and costed, and `slim_core` does not start. The
  wave plan called this record "ADR 0006"; that number is taken
  (features named for capabilities), so it is 0007.
- **Context:** ADR 0005's bytecode VM runs every SELECT behind the
  hidden `vm` feature, but its flip never fitted. Stage 2 measured
  the VM's read path at 1.5-1.9x the read executor it replaces
  ("Stage 2 flip: measured, not taken", `vm_shrink`), and `s3_probe`
  projected the combined Stage 2 + 3 flip 43 / 116 / 127 KB over the
  full / max / xl ratchets with writes at that ratio. The owner
  decided on 2026-11-12, after `s3_probe` said stop, to try a much
  smaller VM: a redesign, not a trim. This ADR measures the four
  levers of that redesign before anything is built
  (`scratchpad/vm-slim-units.md`, unit `slim_design`).

## The budget (re-measured at bfb1582)

The flip fits only if, in every profile, the finished build is at
least 4 KB under the ratchet. With `F` the flip probe
(`ci/flip_probe.sh`), `S` the s3 probe (`--s3`), `S_w` the probe with
only the write side cut (`--s3=triggers,fk,writes`), the write side
`W = F - S_w`, the readers' own bytes `Rd = S_w - S`, and `V` today's
read VM inside `S` (`llvm-nm` on the thumbv7em ELF: `hipp::vm::*` plus
the driver in `session/drive.rs`):

    write VM + readers + read VM <= (ratchet - 4,096) - (S - V)

| bytes | full | max | xl |
|---|---:|---:|---:|
| ratchet | 478,212 | 964,684 | 1,061,344 |
| go target (ratchet − 4,096) | 474,116 | 960,588 | 1,057,248 |
| flip probe `F` | 495,088 | 1,011,444 | 1,115,124 |
| s3 probe `S` | 461,808 | 868,708 | 962,248 |
| write side `W` | 29,360 | 76,856 | 81,664 |
| readers' own `Rd` | 3,920 | 65,880 | 71,212 |
| room for `W` + `Rd`'s replacement (go target − `S`) | 12,308 | 91,880 | 95,000 |
| largest ratio to `W` + `Rd` that fits | 0.37 | 0.64 | 0.62 |
| read VM `V` in `S` | 39,978 | 64,658 | 69,450 |
| … codegen / builder | 14,646 / 1,082 | 20,986 / 1,060 | 23,632 / 1,060 |
| … dispatch / cursors and row lists | 11,480 / 4,488 | 22,512 / 7,550 | 24,592 / 7,538 |
| … driver / drop glue and `op` | 7,018 / 1,264 | 10,894 / 1,656 | 10,950 / 1,678 |
| whole-VM allowance (go target − (`S` − `V`)) | 52,286 | 156,538 | 164,450 |

(`S` moved by −16 / −24 / −88 B since `s3_probe`: the `sync_readers`
review fix. full is the binding profile: the smallest room against
the smallest deletions.)

## The levers, prototyped and measured

Each lever was built as the smallest version that builds the flip and
s3 probes for full / max / xl, in scratch worktrees at bfb1582 (not
committed; the patches are kept in the session scratchpad,
`slim/*.patch`). Deltas are against the probes above (thumbv7em,
`--profile tiny`, `.text + .rodata`); two were rebuilt in a fresh
target directory and reproduced byte for byte.

| lever | prototype | flip Δ (full / max / xl) | s3 Δ (full / max / xl) |
|---|---|---:|---:|
| 1. sync VM | `vm_step` split into a sync `vm_enter` / `vm_turn` (`#[inline(never)]`) and an async loop that only services I/O and `Exec::Call` | +536 / +148 / +232 | −40 / −160 / −192 |
| 2. coarse writes over shared cores | INSERT with folded VALUES rows compiled to `Write` ops (begin, one per row over registers, end) that the driver runs through the per-row core today's executor uses (`exec_insert_row`) and two statement cores split out of `exec_insert` (`insert_begin` / `insert_end`, which `exec_insert` now calls too) | +2,336 / +1,768 / +1,748 | +17,360 / +23,728 / +24,004 |
| 2. coarse read shape over a shared core | the streamed executor folds its aggregates through the VM's `AggRun` (one copy of `stream::agg_step`'s rules) | −80 / −88 / −112 | −72 / −72 / −112 |
| 3. smaller code generator | every index access path is one coarse row-list op over the shared access core (`HOST_ACCESS`, today's `access_rows`); the VM's own index-walk codegen and its 11 index opcodes go (max / xl: full has no `expr_index`) | — / −8,280 / −10,776 | — / −10,448 / −11,952 |
| 4. smaller dispatch | the 17 opcodes that run once per statement or group moved out of `op_join` into one out-of-line slow path | +568 / −240 / +128 | +32 / −440 / −728 |
| 2 + 2 + 3 together | the combination (within ±520 B of the sum of its parts) | +2,256 / −7,392 / −9,332 | +17,272 / +13,728 / +12,332 |

The INSERT VALUES prototype passes `tests/write_side.rs`,
`write_side_v07.rs`, `autoinc_v16.rs`, `insert_balance_v15.rs`,
`insert_select_v08.rs` and `vm_v17.rs` with `--features vm`; the
coarse access prototype passes `vm_v17`, `vm_joins_v17` and
`vm_rest_v17` strict and fails two queries of `vm_subqueries_v17`
(below).

What each taught:

1. **The step future was the sync VM inlined.** `vm_turn` takes
   5,776 of the old future's 6,224 B (full) and 6,968 of 7,440 B
   (xl): sync-ifying the step saves nothing by itself (Stage 1's
   "sync islands" finding again: −40 to −192 B on the s3 probe, +148
   to +536 B on the flip probe). Lever 1's value is only in what it
   enables: sync reader cores the VM and the write paths can share.
2. **A coarse write op costs about what it replaces.** On the s3
   probe (legacy write executors cut) the INSERT VALUES program costs
   +17,360 / +23,728 / +24,004 B, against +16,664 / +24,136 /
   +25,432 B for leaving today's whole INSERT executor alive (every
   INSERT shape): **1.04x / 0.98x / 0.94x**. The per-row core *is* the
   executor (constraint checks, conflict resolution, REPLACE deletes,
   index maintenance, AUTOINCREMENT, triggers and FK calls); the loop
   around it is small, so a VM that shares the core deletes only the
   loop and adds its own glue (+1.7-2.3 KB on the flip probe, where
   the cores are linked anyway: the `Write` opcode, its generator,
   and — in the prototype — a second driver loop). Fine-grained write
   opcodes (ADR 0005's table) would be larger still (`s3_probe`
   assumed 1.9x). Sharing a read core with the legacy read path pays
   only until the flip: −1,364 / −2,012 B in the `full,vm` / `xl,vm`
   builds, −72 to −112 B in the probes, where the legacy copy is
   already cut.
3. **Coarse sources pay in max / xl.** Routing index access through
   the reader core the probe already counts (`Rd`) deletes 10.4 /
   12.0 KB of VM (codegen and opcodes) — the one lever that moves a
   profile by more than noise. Its prototype reads the access bounds
   through the legacy reader, which evaluates them itself, so a bound
   that reads a VM subquery slot or an enclosing query's aggregate
   (`... WHERE u.k <= max(t.a)`, `JOIN w ON w.p = (SELECT min(k) ...)`)
   returned no rows: the design passes bound values in registers
   instead (the VM computes them today for its own walk).
4. **Dispatch is the opcodes' own code.** Outlining once-per-group
   opcodes moved the probes by −728 to +568 B, as `vm_shrink`'s
   outlining did: noise, not a lever.

## Extrapolation (method)

The projection of the finished flip is

    S + r × W + ρ × Rd + (measured read-VM savings)

- `r`, the write side's replacement ratio, is the INSERT prototype's
  measured ratio (numerator: the VM INSERT VALUES program with its
  shared cores; denominator: today's INSERT executor). Every write
  kind (UPDATE incl. FROM, DELETE, upsert, RETURNING, OR-conflict
  paths, trigger programs, FK actions) has the same structure — a
  loop around a per-row core the VM would share — so the ratio is
  applied to all of `W`. **Central:** the worst profile's ratio,
  1.042, in every profile. **Pessimistic:** central, plus the
  measured INSERT VALUES glue once more for every write shape the
  prototype does not cover (INSERT ... SELECT and subquery VALUES,
  RETURNING, UPDATE, DELETE in full; also trigger programs, FK
  actions and INSTEAD OF in max / xl: 4 / 7 shapes). **Optimistic:**
  the best profile's ratio, 0.944.
- `ρ`, the readers' ratio: 1.0 (central and pessimistic: sync
  rewrites of the same readers; lever 1 and Stage 1's sync islands
  saved next to nothing by taking sync code out of async bodies; no
  sync reader was prototyped); 0.84 optimistic (ADR 0005's
  async-inflation upper bound, 16%).
- Read-VM savings: the levers the design takes, as measured, not
  scaled up (L3 and the shared fold core; L1 / L4 only in the
  optimistic row). Measured savings are taken as they are because
  each prototype is the whole lever, not a sample of it.

| projection | full | max | xl |
|---|---:|---:|---:|
| optimistic | 492,705 (+18,589) | 985,479 (+24,891) | 1,086,173 (+28,925) |
| central | 496,249 (+22,133) | 1,004,152 (+43,564) | 1,106,490 (+49,242) |
| pessimistic | 505,593 (+31,477) | 1,016,528 (+55,940) | 1,118,726 (+61,478) |
| (± is against the go target) | | | |
| best ratio reached ((replacement − savings) / (`W` + `Rd`)), central | 1.03 | 0.95 | 0.94 |
| … optimistic | 0.93 | 0.82 | 0.81 |
| ratio go needs | 0.37 | 0.64 | 0.62 |

Every row is over the go target in every profile; the projection is
under every profile's budget (512 KiB / 1 MiB / 1.25 MiB) in all
three rows.

## Decision

**Stop, in full, max and xl.** No lever measured, alone or
combined, brings the flip under any ratchet: even the optimistic
projection is 18.6 / 24.9 / 28.9 KB over the go target. `slim_core`
does not start; nothing ships; the `vm` feature, the probes and every
ratchet stay as they are.

The design below is the smallest VM the measurements support. It is
recorded so that it can be built as specified if the owner continues
the VM for reasons other than size (EXPLAIN parity, the restart
design) or after a re-baseline.

### The design (if built)

1. **One sync step, one driver.** `Vm::run` stays sync over
   `PagerCore`. `Exec` gains `Write(kind)`: the driver loop in
   `drive.rs` services `Pending(Io)`, `Call` (readers) and `Write` in
   one loop (the prototype's second write loop is not repeated).
2. **Writes are coarse ops over today's per-row cores.** One opcode,
   `Write` (`p5` the kind, `p1 .. p1 + p3` the row's registers):
   `INSERT_BEGIN` / `INSERT_ROW` / `INSERT_END` (`insert_begin`,
   `exec_insert_row`, `insert_end`); `UPDATE_ROW`, the per-row body
   of today's `exec_update` loop split out into one function (the SET
   values in registers, then its constraint checks, conflict
   resolution, BEFORE / AFTER triggers, FK checks and actions, and its
   apply tail `rewrite_stored_row`, in today's order); `DELETE_ROW`,
   likewise the per-row body of `exec_delete` (triggers, FK, then
   `delete_row_completely`); `RETURNING_ROW` (today's RETURNING
   capture); each statement's `*_BEGIN` / `*_END` (trigger and FK
   loading, AUTOINCREMENT read and write back, the deferred FK check
   and autocommit, as today). A row source is
   the read program it already is (VALUES rows as `Value` / `Eval`
   loads; INSERT ... SELECT, UPDATE and DELETE scans as the join
   generator's loops, whose `ResultRow` becomes the `Write`). Triggers
   and FK actions stay inside the cores, which call `fire` /
   `fk_*` exactly where they do today. Today's executors become thin
   wrappers over the same cores (`exec_insert` = begin; rows; end, as
   the prototype already does), so until the flip both paths run one
   copy of every rule.
3. **Sources are coarse row-list ops over shared reader cores.** Index
   access paths (equality, ranges, IN lists, MULTI-INDEX OR,
   skip-scans, covering, partial and expression indexes), automatic
   indexes, table-valued functions, virtual tables, TEMP / attached
   and WITHOUT ROWID tables fill a row list through one reader core
   each (today's `access_rows`, `auto_rows`, `json_tvf_rows`,
   `vtab_open` / `vtab_rows_of`, `scan_temp`, `scan_source`), made
   sync over `PagerCore` (restartable: a page miss returns
   `Pending(Io::Read)` before any state changes, as the B-tree cursors
   do), so the VM and the write paths share one copy. Access bounds
   and probe values are computed by the VM (`Eval`, subquery slots,
   enclosing aggregates) and passed in registers; the core does not
   evaluate them. The VM's own index walk (`vm::index`, `IdxOpen` …
   `SkipAdd`, `G::walk` / `combos` / `probe_set` / `or_rows` /
   `rowid_range_rows`) is deleted.
4. **The rest of the read VM stays** (joins' loops, aggregates over
   one shared fold core, subqueries, compounds, windows): no measured
   lever shrinks it.

**Semantics are today's by construction**: the VM calls the functions
today's executor calls, in the order it calls them (constraint checks,
conflict resolution and REPLACE deletes, index writes, the table
write, BEFORE / AFTER triggers, FK counters and actions, RETURNING
capture; `changes()`, `total_changes()`, `last_insert_rowid()`,
AUTOINCREMENT, OR FAIL's kept rows through `write_failed`). A `Write`
leaves `pc` at itself until its core completes (`Vm::write_done`); a
step future dropped inside a core must leave the database and the VM
as today's executor leaves them after a dropped future, which the
tests prove against today's executor on cloned `MemStorage` images
(dropped futures at every await, the smallest page cache,
`tests/crash.rs`). The pin compiles a multi-row VALUES as a coroutine
feeding one insert loop (41 instructions for two rows, 31 for one);
hipp's listing is its own (ADR 0005: the shape, not the content, is
the claim).

### Per-profile byte budget (what go would require)

The whole-VM allowance split at the measured floors (write side at
the best measured ratio, readers at 0.84), the rest for the read VM:

| bytes | full | max | xl |
|---|---:|---:|---:|
| whole-VM allowance | 52,286 | 156,538 | 164,450 |
| write VM (coarse ops + shared cores), ≥ 0.944 × `W` | 27,716 | 72,552 | 77,091 |
| readers (shared sync cores), ≥ 0.84 × `Rd` | 3,293 | 55,339 | 59,818 |
| **read VM allowed** | **21,277** | **28,647** | **27,541** |
| read VM measured after the taken levers | 39,906 | 54,138 | 57,386 |
| … of which codegen + builder (allowed) | 15,700 (8,371) | 18,459 (9,768) | 20,403 (9,792) |
| … dispatch | 11,459 (6,110) | 18,849 (9,974) | 20,320 (9,752) |
| … cursors and row lists | 4,480 (2,389) | 6,322 (3,345) | 6,229 (2,989) |
| … driver, glue | 8,267 (4,408) | 10,508 (5,560) | 10,434 (5,008) |

(Measured figures are `V` minus the taken levers' s3 deltas,
allocated to the components by today's shares; the parenthesised
figures are the allowance at the same shares.) With the central
write ratio and readers at 1.0 the read VM would have to fit in
17,773 / 10,574 / 8,144 B.

### Migration order (if built)

1. The shared aggregate fold core (pays in `vm` builds now).
2. `Exec::Write` in the one driver loop, `insert_begin` /
   `insert_end` split out of `exec_insert`, INSERT VALUES on the VM.
3. The other write kinds over their per-row cores, then RETURNING,
   INSERT ... SELECT, UPDATE ... FROM.
4. Index access through the shared access core with bounds in
   registers; the VM's index walk deleted.
5. Sync reader cores (`PagerCore`) for every `Exec::Call` source.
6. The combined flip, only if `ci/flip_probe.sh --s3` plus the
   replacement measures 4 KB under every ratchet (it does not, on
   these figures).

## What would close the gap

- **A re-baseline** of the full / max / xl ratchets by about +22.1 /
  +43.6 / +49.2 KB (central; +18.6 / +24.9 / +28.9 optimistic). These
  are the projections' distances from the go target, so they already
  include the 4 KB headroom; against the ratchets themselves the
  central projection is +18.0 / +39.5 / +45.1 KB (+18,037 / +39,468 /
  +45,146 B; optimistic +14.5 / +20.8 / +24.8 KB). Unlike `s3_probe`'s
  projection, this one fits every budget, max included (re-baselined
  max ratchet 1,008,248 < 1,048,576 B).
- **A read VM at 45-55% of today's** (full: 21.3 KB or less against
  39.9 KB): nothing measured supports it — `vm_shrink`'s levers took
  18.4 / 6.4 / 7.4 KB (14.5 KB of full's by compiling the evaluator
  once, not by shrinking the VM), this unit's L1 / L4 nothing, and L3
  (max / xl only) 16-17% of the read VM.
- **Tier B, one unified executor without bytecode** (ADR 0005,
  "Alternatives considered"), is the fair comparison the measurements
  now favour: the bytecode layer is the cost — the read VM is 1.5-2.8x
  the read executor it replaces (Stage 2 and `s3_probe`'s
  cross-check), coarse writes are ~1.0x, and a read core shared with
  the legacy path pays only while both paths exist — and Tier B never
  pays for a VM. Its own saving — the duplication between today's
  streamed and materialising executors and the per-kind write loops —
  is still an estimate (ADR 0005: −10…−20 KB in full); it needs its
  own probe before any unit builds it.

## Consequences

- No code, behaviour or size changes: the four ratchets stay at
  76,952 / 478,212 / 964,684 / 1,061,344 B.
- `slim_core` does not start (its spec: "Start only if `slim_design`
  says go").
- ADR 0005 Stages 2-3 stay unflipped; whether to continue the VM for
  reasons other than size, re-baseline, or measure Tier B is the
  owner's decision.
