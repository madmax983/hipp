# ADR 0005 — Code size: a sync core and a bytecode VM

- **Status:** accepted for Stage 1 (Tier A), 2026-11-08, with the
  measured results below. **Stages 2 and 3 accepted, 2026-11-09.**
  Stage 4 stays optional and needs its own amendment, as the Decision
  says. Stages 2-3 supersede SPEC §5.7's "tree-walk now" decision and
  implement SPEC §5.1.1's "the state machines are inside" design;
  Stage 1 changed no architecture.
- **Migration rule (added with the acceptance):** the VM is built
  behind a hidden `vm` feature that no profile enables, so every
  intermediate commit leaves the four size ratchets untouched. Under
  `vm`, a statement whose plan the code generator does not cover yet
  runs on today's executor, so the whole suite stays green at every
  step, and a CI job runs it with `vm` on. One final unit per stage
  turns the VM on in the profiles and deletes the executors it
  replaces in the same change. That unit must lower every ratchet it
  touches.
- **Context:** All four profiles are gated, and `max` is out of room.
  Measured at `8d78815` (thumbv7em, `--profile tiny`, the pinned
  toolchain; full tables in the size-pass working note `baseline.md`,
  kept outside the tree):

  | profile | .text+.rodata | budget | headroom |
  |---|---:|---:|---:|
  | `tiny` | 76,960 B | 131,072 B | 54,112 B |
  | `full` | 506,672 B | 524,288 B | 17,616 B |
  | `max` | 1,046,892 B | 1,048,576 B | **1,684 B** |
  | `xl` | 1,114,356 B | 1,310,720 B | 196,364 B |

  For comparison, SQLite 3.54.1 on the same target (`-Os`) is 543,578 B
  core-only and 681,786 B with math, FTS5, RTREE, DBSTAT and PERCENTILE.
  hipp `max` is therefore 1.54× SQLite for a comparable feature set.

  The symbol table says where the gap comes from. All figures are
  measured; the bracketed numbers are tiny / full / max / xl.

  1. **Async fn bodies are a third of every profile** [30.9 / 172.1 /
     356.0 / 364.2 KB]. They are less dense than sync code from the same
     crate: 3.01 vs 2.76 bytes per instruction, with memory ops at 34% vs
     28% of instructions and 68–71% of those in 32-bit encodings vs 47%.
     A future's locals sit behind a pointer, out of reach of Thumb's 16-bit
     encodings, and every value live across an `.await` is spilled and
     reloaded. Inflation is *estimated* at 8–16% of those bytes. The
     futures' drop glue adds 1.5 / 13.5 / 29.9 / 30.7 KB on top. Only page
     fetch, journal and commit I/O actually have to suspend. Everything
     else is async because it calls something that might.
  2. **`slice::sort` is instantiated once per (element type, comparator)**:
     0 / 22.6 / 60.4 / 72.6 KB for 30 call sites.
  3. **The expression evaluator is compiled twice** in max/xl (`E = ()`
     from `window.rs` beside `E = MemError`): 14.7 KB duplicated.
  4. **Parallel executors are live in the same build.** These are the
     streamed executor (`session/stream.rs`) beside the materialising one
     (`run_query_sub`); `project_qp_async` over `QueryPlan` beside
     `sub_project` over `SubPlan`; the subquery executor; separate
     INSERT / UPDATE / DELETE / upsert row loops; and the trigger runner
     and FK action runner. The v0.4 synchronous executor is already gone
     from every gated profile.
  5. **Smaller costs.** Deep clones of plans per execution (`Clone`
     impls 0.4 / 10.2 / 15.0 / 15.8 KB), per-type `AVec`/`Vec` growth
     code, and B-tree `distribute::<T>` copies.
  6. **There is no fmt or panic machinery left to cut** (0 B / 78 B).

  SPEC §5.7 left bytecode as "a door left open". Its revisit triggers
  include code size and "a decision to pursue EXPLAIN parity". We studied
  Turso's VDBE (MIT, read only, ideas not code: `core/vdbe/insn.rs`,
  `execute.rs`, `mod.rs`, `builder.rs`, `explain.rs`, `sorter.rs`;
  `core/translate/*`; `core/types.rs` `IOResult` / `return_if_io!`;
  `core/io/completions.rs`; `core/storage/btree.rs`, `pager.rs`) beside
  SQLite's own VDBE.

## Decision

1. **Stage 1 — Tier A, no architecture change. It lands first and buys
   the room the later stages need.**
   1. **One shared stable sort.** A crate-private, non-generic
      `sort_perm(n, &mut dyn FnMut(usize, usize) -> Ordering)` (a stable
      merge sort over a `u32` permutation) plus a small per-type
      apply-permutation step. All 30 `sort*` call sites use it.
      Stable-sort output is fully determined by the comparator, so
      results do not change. Unstable sorts today are only on fully keyed
      integers or tuples, where stable gives the same answer.
   2. **A monomorphic evaluator.** `query::eval` and its helpers return a
      non-generic error with no `Storage` variant. Generic callers convert
      once at the boundary (`Error::generalize` already exists for this).
      `window.rs` and `agg.rs` then stop instantiating an `E = ()` copy.
   3. **Sync islands.** Split the mostly-sync async bodies into "await
      the I/O, then call an `#[inline(never)]` sync fn". The targets are
      `build_catalog` (601 lines, 1 await), `rebuild_catalog`,
      `integrity` / `check_rows`, `vacuum`, `trigger_failed`,
      `exec_subplan_inner`, `step_query`'s materialise tail,
      `json_tvf_rows`, `dbstat_db` and `stat_decode`. Each split is kept
      only if it measures smaller.
   4. **Glue dedupe:**
      - a type-erased `AVec` growth core with thin typed wrappers;
      - `distribute` computing split points monomorphically, with only
        the final split generic;
      - plans borrowed or `Rc`-shared instead of deep-cloned per execution
        (`run_query`, `stream_open`, `exec_subplan_inner`, `cte.rs`).
2. **Stage 2 — the VM and the read path.** A register VM in `src/vm/`
   executes every SELECT shape: joins, subqueries, compounds, CTEs,
   views, windows, virtual-table and table-valued sources. The streamed
   and materialising executors, the subquery executor and the projection
   paths are deleted in the same change. The tiny statements (CREATE
   TABLE, INSERT VALUES, scan SELECT, BEGIN / COMMIT / ROLLBACK) move to
   the VM in this stage **only if tiny measures no larger**. Otherwise
   tiny keeps its direct executor and records the measurement.
3. **Stage 3 — writes on the VM (Tier B happens here, not separately).**
   INSERT (VALUES and SELECT), UPDATE (incl. FROM), DELETE, OR-conflict
   handling, UPSERT, RETURNING, triggers as subprograms (`Program`) and FK
   enforcement and actions (`FkCounter` / `FkIfZero` / action
   subprograms) all compile to programs. This deletes the per-kind write
   executors, the trigger and FK runners, and the tree-walking evaluator
   once nothing calls it. A separately unified executor without bytecode
   (Tier B) is **not** built: it would be thrown away by this stage.
4. **Stage 4 (optional, its own later ADR amendment).** Make B-tree writes
   sync too, by op-level undo (below). DDL, PRAGMA, VACUUM and ALTER move
   onto the VM or onto sync host ops, as SQLite runs them. After this,
   async remains only in the pager's I/O primitives and the one driver.
5. **Every change keeps every gate green and every ratchet
   non-increasing.** The gates are fmt, clippy over `ci/feature-sets.txt`,
   the full suite including the differential tests against the pinned
   3.54.1 oracle and the CLI byte-identical tests, the four no_std
   targets, and the four size gates. A unit that measures larger in any
   profile is re-scoped, not merged. Ratchets are lowered in the same
   commit, with the usual STATUS.md "Size Δ" entry.

## Stage 1 results (measured, 2026-11-08)

Stage 1 shipped as three units (STATUS.md, "Size pass"), each split
or edit measured on all four profiles and kept only when no profile
grew. `.text + .rodata` on thumbv7em, `--profile tiny`, against the
ratchets when the stage started (`xl` had grown by four leaves since
the `8d78815` baseline above):

| profile | stage start | 1.1 shared sort | 1.2 + 1.4 mono / glue | 1.3 sync islands | after | Δ |
|---|---:|---:|---:|---:|---:|---:|
| `tiny` | 76,960 | 0 | −700 | −136 | **76,124** | −836 |
| `full` | 507,592 | −26,352 | −4,060 | −704 | **476,476** | −31,116 |
| `max` | 1,031,072 | −62,688 | −4,068 | −2,024 | **962,292** | −68,780 |
| `xl` | 1,142,128 | −76,948 | −4,564 | −1,808 | **1,058,808** | −83,320 |

Headroom after Stage 1: tiny 54,948 B, full 47,812 B, max 86,284 B,
xl 251,912 B. Engine statics stay 0 B. Futures' drop glue fell in
every profile (max 29,906 → 29,830 B). Rows, errors, file bytes and
EXPLAIN QUERY PLAN output are unchanged against the pin, except one
deliberate parity fix found by the comparator audit (an fts5 NaN rank
now sorts as the pin's NULL) and three integrity-check parity fixes
found in the sync-islands review (column 1.3 includes them: they
removed more code than they added).

What the measurements taught, for Stages 2-4:

- **1.1 and 1.2 delivered as estimated.** No `core::slice::sort`
  symbol remains; the evaluator has one instantiation per function.
- **1.4's type-erased `AVec` core shipped nothing.** `alloc`'s
  `RawVecInner` already shares the growth path; per-type stubs are
  22 B. Both experiments grew every profile (or cost 17.7% more
  instructions), so the remaining push-site glue is left to the VM,
  whose registers are preallocated.
- **1.3 (sync islands) returned about a quarter of its estimate.**
  A local that never lives across an `.await` is already on the stack,
  so moving sync code out of a `poll` body saves only what register
  pressure in the large body cost: 3–8% of the moved bytes for large
  islands with many locals, and a net loss (call, arguments,
  prologue, lost constants and shared outlined fragments) for small
  ones. Of 32 measured splits, 10 were kept. The large "mostly-sync"
  bodies (`step`, `exec_vtab`, `vtab_rows_of`, `stream_step`,
  `exec_alter16`) are large because they inline async callees, which
  only Stage 2 / 3 remove. The inflation estimate behind Stage 4
  (8–16% of the async bodies) should therefore be read as an upper
  bound: what Stage 2–3 save comes from deleting duplicate executors
  and their futures, not from density.

## Design (Stages 2–4)

### How hipp suspends: restart, not resume

Turso keeps the VM resumable at any point. Every storage operation returns
`IOResult<T> = Done(T) | IO(IOCompletions)` (`core/types.rs`, with
`return_if_io!` to propagate it). The B-tree and pager are hand-written
state machines: `WriteState`, `BalanceSubState`, `CursorSeekState`,
`OverflowState`, `CommitState`, `AllocatePageState`, … in
`core/storage/btree.rs` and `pager.rs`. An opcode that hits I/O calls
`state.suspend_on_io(io)` and returns `InsnFunctionStepResult::IO` with
`pc` unchanged (`op_next` in `core/vdbe/execute.rs`). The dispatch loop
(`normal_step` / `dispatch_loop` in `core/vdbe/mod.rs`) first checks the
parked completion (`finish_pending_io`). `Statement::step` returns
`StepResult::IO`, and the caller runs the I/O loop and steps again.
Completions are `Arc`'d callback objects that also implement `Future`
(`core/io/completions.rs`).

That design is correct but expensive in code: Turso's `btree.rs` is
16,386 lines. hipp keeps what makes it cheap — plain synchronous Rust
below the VM — by making **the instruction the restart unit**:

- **The pager is split.** `PagerCore` is non-generic: frames, header,
  dirty and journaled lists, CLOCK bookkeeping. `Pager<S>` owns `S`, the
  sidecars and the async I/O primitives. Sync code asks the core for a
  page:
  - a hit returns the frame;
  - a miss returns `Pending(Io::Read(pgno))` and changes nothing;
  - no clean victim returns `Pending(Io::Spill)`.
- **Read opcodes are restartable by construction.** These are Rewind,
  Next, Seek*, Column with an overflow chain, and Idx*. They compute the
  new cursor position and output values into temporaries, and commit
  them only when nothing is missing. `pc` advances only on completion,
  so a miss simply re-executes the opcode after the page arrives.
- **Pages read by an attempt stay pinned until the opcode completes.**
  An opcode therefore needs at most "pages per op" frames: about the tree
  depth plus siblings, the same bound SQLite has. Running out is today's
  `Error::CacheExhausted`.
- **In Stages 2–3, B-tree writes are not made sync.** Insert, IdxInsert,
  Delete, IdxDelete, CreateBtree and Destroy run as **driver-awaited
  ops**:
  - the VM leaves the record, the cursor and the flags in registers and
    returns `Exec::Write(op)`;
  - the driver awaits today's async B-tree function (unchanged code, so
    the same page-allocation and balance decisions and the same file
    bytes);
  - the driver stores the result, for example a new rowid, sets
    `pc + 1` and re-steps.

  Crash and cancellation behaviour of writes is today's. If a step
  future is dropped mid-write, a flag left set makes the next step fail
  the statement through the existing statement rollback rather than
  re-apply the write.
- **Not-yet-migrated statements run as `Exec::Host(kind)`.** The driver
  awaits the existing async executor. This is how migration stays
  incremental: a statement kind is "on the VM" exactly when its program
  stops containing `Host`. DDL, PRAGMA and maintenance stay `Host` until
  Stage 4.
- **Stage 4 adds op-level undo.** A write opcode runs under a mark. A
  page first dirtied in this attempt has its pre-image recorded, together
  with the header fields (page count, freelist head and count). A miss
  restores them and returns `Pending`. Journal records are written
  before mutation, as today, via `Pending(Io::Journal(pgno))`, so the
  storage call sequence the crash tests enumerate does not change. This
  is what lets the B-tree write path and the pager logic drop their
  async bodies (12.3 / 44.4 / 62.4 / 62.4 KB today).

The public API stays async and runtime-agnostic. `Statement::step` becomes
the only async function above the pager, and it is small:

```text
loop {
    match vm.run(&mut cx) {               // sync; non-generic over S
        Row  => return Ok(Step::Row),
        Done => return Ok(Step::Done),
        Pending(io)  => pager.service(io).await?,   // read / spill / journal
        Write(op)    => vm.complete(op, btree_write(op).await?),
        Host(kind)   => vm.complete_host(legacy(kind).await?),
    }
}
```

Dropping that future at any `.await` is safe, because the VM state (pc,
registers, cursors) lives in the `Statement`, which is SPEC §5.1.1's
contract. A caller with an always-ready `Storage` drives it with
`block_on` as today. The VM, its opcodes, the cursors and all codegen are
**monomorphic**: they see `PagerCore`, never `S`. Only the driver and the
I/O primitives are generic over `S`.

### Program representation

- **The instruction.** `Op { code: u8, p5: u8, p1, p2, p3, p4: u32 }` is a
  fixed 16 bytes. `p4` indexes a per-program side table:
  `P4::{Value, Func(id, argc), Coll, KeyInfo, Affinity, Table, Sub(prog)}`.
  This is SQLite's p1–p5 shape, chosen over Turso's enum-with-fields
  `Insn` (221 variants, `core/vdbe/insn.rs`). With uniform fields, **one
  generic formatter prints any instruction**. Turso needs a 2,934-line
  `explain.rs` that formats each variant.
- **Registers and cursors.** Registers are `AVec<OwnedValue>` (the
  existing value type). Cursors are a
  `Table | Index | Pseudo | Ephemeral | Sorter | Vtab | Tvf` enum built on
  the existing `btree::ScanCursor` and index cursors. `Ephemeral` and
  `Sorter` are in-memory (hipp's TEMP and sorter memory already are),
  bounded by the arena limits. Sorting uses the Stage 1 shared stable
  sort with record comparison under `KeyOrder` / collation, so the pin's
  "earliest of equal rows stay" ordering holds. Turso's sorter spills
  chunks to disk with a heap merge (`core/vdbe/sorter.rs`); hipp does not
  need that.
- **The builder.** It provides register and cursor allocation, labels
  with fixups resolved at `finish()`, and constant hoisting into the
  `Init` prologue. This follows Turso's `ProgramBuilder`
  (`alloc_register(s)`, `allocate_label`, `preassign_label_to_next_insn`,
  `resolve_labels`, `constant_spans`) and SQLite's
  `sqlite3VdbeMakeLabel`.
- **Codegen consumes today's plans** (`Plan`, `QueryPlan`, `SubPlan`,
  `SourcePlan`, `IdxAccess`). The parser, binder, CTE expander and
  planners, including `xjoin` and `xplan`, are unchanged. This is the
  "retarget Plan to bytecode" seam SPEC §5.7 kept. Codegen lives in
  `src/vm/gen/{select,expr,insert,update,delete,trigger,fk}.rs`.

### Instruction set, mapped onto hipp's feature gates

Each `Opcode` variant and its match arm carry the `#[cfg]` of the feature
that needs it, so `tiny` compiles about 25 opcodes and `full` about 95.

| group | opcodes | gate |
|---|---|---|
| control | Init, Goto, Halt (p4: error), Eq / Ne (jump), IsNull / NotNull | always |
| control+ | Lt / Le / Gt / Ge (p5 affinity + NULL rule, p4 collation), If / IfNot, Gosub / Return, IfPos / DecrJumpZero / OffsetLimit, Once, Compare / Jump | `query` |
| coroutines | InitCoroutine / Yield / EndCoroutine (FROM subqueries, views, CTEs), BeginSubrtn (scalar subqueries) | `sql` |
| values | Integer, Int64, Real, String8, Blob, Null, Copy / SCopy / Move, Affinity, MustBeInt | always (Copy… `query`) |
| | Variable | `params` |
| expressions | Add … Remainder, Concat, And / Or / Not, Cast, Function (p4: id into the existing function library) | `query` (`math`, `datetime`, `json`, fts5 aux functions are ids, not opcodes) |
| | BitAnd / BitOr / ShiftLeft / ShiftRight / BitNot, row-value Compare | `sql` |
| tables | OpenRead, OpenWrite, Rewind, Next, Column, Rowid, SeekRowid / NotExists, MakeRecord, NewRowid, Insert†, CreateBtree†, Close | always |
| | Last, Prev, NullRow (LEFT / RIGHT / FULL) | `query` |
| | Delete†, Destroy†, Clear† | `write` |
| indexes | SeekGE / GT / LE / LT, IdxGE / GT / LE / LT, IdxRowid, Found / NotFound / NoConflict, IdxInsert†, IdxDelete† | `indexes` |
| sorting / temp | OpenEphemeral, SorterOpen / Insert / Sort / Data / Next, RowSetAdd / Test | `query` (sets, compounds and the recursive CTE queue: `sql`) |
| aggregates | AggStep / AggFinal (p4: `AggKind`, driving `agg.rs`) | `query` |
| | AggInverse / AggValue + frame ops over `window.rs` | `window` |
| subprograms | Program (trigger body, with OLD / NEW parameter registers and RAISE(IGNORE) target), Param, ResetCount | `triggers` |
| | FkCounter / FkIfZero / FkCheck | `foreign_keys` |
| virtual tables | VOpen, VFilter, VColumn, VNext, VRowid, VUpdate (generate_series, dbstat, rtree, fts5, fts5vocab) | `vtab` (+ `json` for the json_each / json_tree cursor kind) |
| txn / schema | Transaction, AutoCommit (BEGIN / COMMIT / ROLLBACK), SetCookie, ParseSchema, Host (p4: legacy kind) | always |
| | Savepoint | `sql` |
| | WITHOUT ROWID / UTF-16 / expression / partial index behaviour | flags on Open* and codegen; no new opcodes |

† driver-awaited in Stages 2–3; sync under op-undo in Stage 4.

**Amendment (Stage 2, unit `vm_core`, 2026-11-09): expressions run
through one `Eval` opcode in Stage 2.** Its `p4` indexes a resolved
expression (`RExpr`) in the program's side table, evaluated by the
existing sync `query::eval` against the row a cursor holds (or the
empty row); a `p5` mode evaluates it as a WHERE condition
(`query::cond`'s jump semantics) or as the WHERE clause's constant
terms (`query::join_constants_hold`). The table's per-operator
opcodes (Add … Remainder, Concat, And / Or / Not, Cast, Function)
are not built in Stage 2. Reasoning: `query::eval` must stay until
Stage 3 anyway (every write path evaluates through it), so a second
evaluator spelled as opcodes would only add bytes (and a second
place for evaluation order, laziness, affinity and collation rules
to drift), while one `Eval` keeps those rules exactly today's. The
comparisons and jumps that drive loops are real opcodes (`If`,
`IfNot`, `IfPos`, `DecrJumpZero`, `IsNull`, `Gt`, `Lt`). Stage 3,
which deletes the tree-walker once nothing else calls it, revisits
the per-operator opcodes. Two smaller deviations from the text
above: `Op` is `{code: u8, p5: u8, p1: u16, p2, p3, p4: u32}`, since
four `u32` operands and two bytes do not fit 16 bytes (`p1` holds a
cursor or register number; a program that needs more than 65,535
falls back); and the `params` bind path needs no `Variable` opcode
in Stage 2, because the binder substitutes the bound values into the
statement before it is planned (rule 6: the binder is unchanged), so
each execution compiles its own plan.

**`tiny` must not grow.** Tiny's programs need Init, Transaction,
OpenRead / OpenWrite, Rewind, Next, Column, Rowid, SeekRowid, Eq / Ne,
the constant loads, MakeRecord, NewRowid, Insert, CreateBtree, SetCookie,
ParseSchema, AutoCommit, ResultRow and Halt. That set replaces today's
5.9 KB `Statement::step` future, `project_current` and the scan future.
*Estimate:* the VM core plus these opcodes is about 3.5 KB, codegen
about 2 KB and the driver about 0.5 KB. Stage 2 ships tiny on the VM only
if its measured size does not increase.

### EXPLAIN comes for free

With programs, `EXPLAIN <stmt>` compiles `<stmt>` and returns one row per
instruction, as the pin does: `addr, opcode, p1, p2, p3, p4, p5, comment`.
The uniform `Op` fields make this a single formatter plus an opcode-name
table, about 2–3 KB of mostly `.rodata`, under the existing `explain`
leaf (max / xl). The name table is not compiled outside that leaf.

**The claim is the pin's shape (columns, types, row per instruction), not
the pin's listing.** hipp's programs are not SQLite's, so the content
differs. The differential tests check the shape against the pin, and
hipp-only golden tests check the content. The CLI can indent the listing
as the pin shell does. EXPLAIN QUERY PLAN stays as it is (`session/eqp.rs`,
derived from the plan).

### Keeping differential tests and byte compatibility

- **What the pin can observe does not change.** File format, B-tree,
  record and pager code are untouched in Stages 2–3: writes call the same
  async B-tree functions with the same arguments. Codegen **ports the
  order of today's executors and does not redesign it**:
  - constraint checks, REPLACE deletes, index writes, the table write;
  - trigger firing order;
  - FK counter points;
  - evaluation order of side-effecting functions (`random()`,
    `changes()`, `'now'` once per statement);
  - lazy CASE / coalesce / iif.

  File bytes, rows, errors, `changes()` and `last_insert_rowid()` are
  therefore identical.
- **Shadow mode during migration only.** A hidden feature `vm_shadow`
  (never in any profile; CI job only, removed with the last legacy
  executor) runs each migrated statement through both executors on
  cloned `MemStorage` images. It asserts equal rows, errors and file bytes
  across the whole test corpus.
- **Restartability is tested with the existing `Config::cache_pages`.**
  The full suite also runs with the smallest page cache, so nearly every
  opcode misses and re-executes. The crash tests (`tests/crash.rs`) see
  the same storage-call sequence for writes.

### Size estimate (bytes; *estimates* except the Stage 1 inputs, which are measured symbol totals)

| stage | tiny | full | max | xl |
|---|---:|---:|---:|---:|
| baseline (measured) | 76,960 | 506,672 | 1,046,892 | 1,114,356 |
| S1 Tier A | −0.8…−1.9 K → **~75.7 K** | −26…−34 K → **~476.7 K** | −78…−90 K → **~962.9 K** | −90…−103 K → **~1,018 K** |
| S1 Tier A (**measured**, from the stage start) | −0.8 K → **76,124** | −31.1 K → **476,476** | −68.8 K → **962,292** | −83.3 K → **1,058,808** |
| S2 VM + read path | 0…−3 K → **~74.2 K** | −12…−30 K → **~454.7 K** | −30…−55 K → **~920.9 K** | −32…−58 K → **~973.4 K** |
| S3 writes / triggers / FK | 0 → **~74.2 K** | −8…−18 K → **~441.7 K** | −20…−40 K → **~890.9 K** | −20…−40 K → **~943.4 K** |
| S4 (optional) sync writes + DDL / PRAGMA | −1…−2.5 K → **~72.5 K** | −10…−20 K → **~426.7 K** | −20…−35 K → **~863.9 K** | −20…−35 K → **~916.4 K** |

Reasoning:

- **S1.** These are the measured bytes of the code being removed, minus a
  small replacement:
  - sort: 22.6 / 60.4 / 72.6 KB removed, about 1.5–4 KB added;
  - evaluator: the 14.7 KB duplicate in max / xl;
  - islands: 8–16% of the 7.8 / 47.9 / 60.4 / 66.1 KB of mostly-sync
    bodies;
  - glue: a fraction of the 2.9 / 6.4 / 11.8 / 13.0 KB `AVec`/`Vec`
    code, the 0.7–1.1 KB `distribute` excess and the clone impls.
- **S2 / S3.** The executor code to be deleted is about 44 KB in full for
  reads, about 30 KB for writes, and roughly double in max (the symbol
  lists in `baseline.md` §4.3), *including* its async inflation and
  future drop glue. Against that, a VM core plus read opcodes of about
  14 / 22 KB (full / max), write opcodes of about 6 / 10 KB, and codegen
  that is 0.6–0.8× the size of the executor it replaces. The ranges are
  wide on purpose.
- **S4.** 8–16% of the async bodies that remain (B-tree writes, pager
  logic, PRAGMA / maint / ALTER), plus their futures' drop glue, minus
  about 1–2 KB of undo machinery.
- **Sanity check.** After S3, `max` would be about 1.31× SQLite's 681,786
  B (from 1.54×). The remaining gap is Rust drop and clone glue, the
  fts5 / json engines and hipp's richer planner.

## Alternatives considered

- **Tier B: one unified executor without bytecode.** This would mean:
  - one plan type, with `QueryPlan` folded into `query::SubPlan`;
  - one join engine, `stream.rs`'s resumable nested loop for every
    query, where materialising is streaming into a buffer, including
    RIGHT / FULL second passes, compounds and windows;
  - one projection and grouping path, replacing `project_qp_async`,
    `sub_project` and `win_project_async`;
  - one row-change routine `change_row(target, old, new, conflict)`. It
    would run constraints, then conflict resolution with REPLACE deletes,
    then index maintenance, the table write, BEFORE / AFTER triggers, FK
    counters and actions, and RETURNING capture. Triggers and FK actions
    would re-enter the same pipeline. This is how SQLite shares
    `sqlite3GenerateConstraintChecks` / `sqlite3CompleteInsertion` /
    `sqlite3GenerateRowDelete` across INSERT, UPDATE, DELETE and UPSERT.

  *Estimated* saving: tiny 0, full −10…−20 KB, max / xl −18…−35 KB. It
  is rejected as a separate stage because it removes the duplication but
  keeps every layer async, and Stage 2–3 delete the same code. It is the
  fallback if this ADR's VM is rejected.
- **Turso-style resumable state machines below the VM.** Rejected for
  size: every B-tree and pager operation would become a hand-written
  state enum. Restart-per-instruction gets the same cancellation safety
  with sync code.
- **A sync storage core without a VM** (sync B-tree with a per-operation
  async retry shim). Evaluated: the shims (one per operation, about
  120–250 B each) eat most of the gain, which is about −0.3…−1.8 KB in
  tiny and −5…−10 KB in full. The executor above stays async. It is kept
  only as the shape Stage 4 takes for writes.

## Consequences

- **`max` regains room.** Stage 1 alone took it from 1,684 B of
  headroom (at `8d78815`) to 85,868 B (measured), and `full` to
  47,772 B. The ratchets move down at every stage.
- **Async remains only where I/O is.** After Stage 3 it is the I/O layer
  plus driver-awaited writes and `Host` statements; after Stage 4, only
  the I/O primitives. The `Storage` / `SidecarFile` traits do not change.
  Cancellation safety becomes structural: VM state lives in the
  `Statement`.
- **The VM is monomorphic.** Std builds with several `Storage` types stop
  duplicating the engine (not gated, but compile time and binary size
  improve).
- **Plain EXPLAIN stops being refused** in `explain` builds. Its content is
  hipp's bytecode, documented as such.
- **Some debugging moves into programs.** Bugs now live in codegen plus
  opcodes rather than in executors. The EXPLAIN listing and the
  `vm_shadow` diff are the tools for this.
- **SPEC changes.** §5.7 is rewritten, §5.1.1 gains the restart rule, and
  RISKS.md gets the risks below.
- **Risks:**
  1. **VM plus codegen larger than estimated.** Mitigated by per-unit
     size gates and tiny's "ship only if not larger" rule.
  2. **A restartability bug** (an opcode with a side effect before a
     miss). Mitigated by the rule "registers, cursors and `pc` are written
     only on completion", review, and the tiny-cache suite run.
  3. **Differential drift in evaluation order or affinity.** Mitigated by
     porting the order and by the shadow diff.
  4. **Performance.** Interpretation vs tree-walk is roughly neutral, and
     removing per-row plan clones and futures helps. `docs/perf-baselines.md`
     is re-measured at each stage.
  5. **The migration is long.** The `Host` seam keeps every intermediate
     state shippable.
  6. **The memory bound on pinned pages per opcode.** It is the same as
     SQLite's and errors as today.
