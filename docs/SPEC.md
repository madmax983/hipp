# hipp — SPEC v0.1 (design foundation) — rev. 0.3, 2026-10-16 (D1–D5 applied; D2 corrected)

> **Status:** design only, local, pre-code. Repo: <https://github.com/madmax983/hipp>
> (public, created 2026-10-16, seed only: `.gitignore` + Apache-2.0 `LICENSE`,
> default branch `main`). Nothing in this spec has been pushed anywhere.
> **Date:** 2026-10-16. **Rust edition:** 2024 (Mark's standing rule, no exceptions).
> **Update, 2026-10-16 13:59 CDT (Mark):** the allocation strategy is settled —
> arenas, arena-first (§7.5). "I think Arenas are what we go with anyway?
> We want it to be FAST."
> **Revision, 2026-10-16 evening (rev. 0.2):** Mark decided D1–D5 (§11
> decision log); this revision applies them throughout. D1: pin = SQLite
> **3.54.1**, newest at code-start. D2: **overrules** the original sync
> `Storage` in favour of poll-async (§5.1 rewritten; the sync reasoning
> survives as a superseded note there and as RISKS R15). D3: no
> `sqlite3_*` C ABI, ever (§2, §5.8). D4: CLI pulled forward to v0.2
> (§9.4, §10). D5: public claim wording is the narrow one — "reads and
> writes SQLite database files" (§1, §2).
> **Revision, 2026-10-16 21:50 CDT (rev. 0.3) — D2 CORRECTED by Mark:**
> rev. 0.2 read D2 as hipp *composing with / adapting to* Horton's actual
> `BlockDevice` ecosystem. Mark corrected that: hipp gets its **own**
> poll-async storage trait, *patterned on* Horton's idea — a similar
> thing, not an adapter. Poll-async stands; the literal composition (the
> §5.1 block-cache adapter, any Horton dependency, Q1) is removed. See
> the §11 D2 entry for the verbatim correction and the corrected reading.
> **Refined minutes later (21:52 CDT) — D2's final form:** Mark: *"just
> enabling the async primitives? Then someone can use tokio if they have
> it?"* Yes: hipp's primary I/O surface is **runtime-agnostic async
> primitives** — an `async fn` `Storage` trait and operation futures,
> built only on `core`'s `Future`/`Poll`/`Waker` machinery. The caller
> brings the executor (tokio, embassy, smol, or a trivial `block_on`);
> hipp core has **no runtime dependency and no tokio dependency, ever**.
> The hand-rolled `poll_*` surface is demoted to an internal
> implementation detail (§5.1/§5.1.1); Q2 is resolved by this refinement
> (§11). This whole D2 chain — sync → "compose with Horton" →
> Horton-as-pattern → async primitives — is one rev. 0.3, recorded in
> full in the §11 decision log.

## 0. Decision summary — read this, skip the rest if you trust it

| Question | Verdict |
|---|---|
| Allocator | **`no_std` + `alloc` baseline, arena-first internally (settled, Mark 2026-10-16).** Not Horton's zero-alloc law, and not scattered global allocation either: all internal allocation flows through scoped arenas (§7.5) — per-statement, per-transaction, and one long-lived catalog/connection arena — with caller-owned fixed page buffers in the pager and **no allocation in the I/O path**. The global allocator, where one exists, only ever grows arena chunks. (§7) |
| Compatibility order | **File format first**, then SQL dialect subset, expressed through a Rust-only API. A C ABI is **out of scope, permanently** (D3, 2026-10-16). (§2) |
| Journal mode at v0.1 | **Rollback journal only.** WAL is read-rejected with a named error, not silently misread. (§5.3) |
| Executor | **Tree-walking (Volcano-style) over an arena-allocated plan tree.** No VDBE/bytecode in v0.1. The plan tree is the seam where bytecode could land later. (§5.7) |
| Storage I/O style | **Runtime-agnostic async primitives, caller-provided, byte-addressed** — hipp's **own** `async fn` `Storage` trait + operation futures on `core` machinery only (D2 final form, 2026-10-16 21:52; §5.1). No runtime/tokio dependency, ever; the caller brings the executor. No Horton dependency or adapter; internally, explicit resumable state machines in the structs provide drop safety (§5.1.1). |
| Page size | Runtime value read from the file (512–65536), never a const generic. Create defaults to 4096. (§5.2) |
| Text on disk | Stored and carried as **byte slices** (`&[u8]`, arena/page-backed), not `String`. (§5.5) |
| Concurrency at v0.1 | One connection, one writer, exclusive-open assumption. Multi-process locking is specified (§5.3) but not implemented in the milestone. |
| First milestone | Create/open a real SQLite-format file; `CREATE TABLE` + `INSERT` + full-scan `SELECT` round-trip **in both directions against the real `sqlite3`** (§9). Pinned claim version: **SQLite 3.54.1** (D1). The local CLI is 3.45.1 (`/usr/bin/sqlite3`) — below the pin; it must be upgraded/replaced before claim tests count (§8, §11). |
| Size & speed | **Hard gates, not aspirations (Mark: "fast and tiny. because embedded.").** v0.1 ships only if it passes the §4.1 budgets on `thumbv7em-none-eabihf`: ≤ 128 KiB `.text`+`.rodata` for the tiny profile, ≤ 8 KiB static RAM, ≤ 64 KiB total engine RAM in the tiny reference configuration, and zero steady-state global allocations in the scan hot loop. Features outside the tiny profile compile out via cargo features (§4.1, §2). |

## 1. Pitch

**hipp is a little database that reads and writes SQLite database files, for anywhere LLVM targets.**

Horton proved Mark's shop can build a serious storage engine under embedded
constraints: `#![no_std]`, caller-owned storage, crash discipline, measured
budgets. Turso proves the other half: SQLite compatibility — file format and SQL
dialect — is achievable in Rust; its own compatibility document states
the SQLite file format is fully supported and tracks a pinned SQLite version
(3.50.4) for differential testing [TURSO-COMPAT].

Nobody in that picture is building for the targets Horton lives on. Turso is a
hosted, `std`, async-runtime engine. SQLite itself is C and assumes a hosted OS VFS.
hipp's I/O is its own async-primitive surface in Horton's idiom (D2):
an async `Storage` trait and operation futures that require **no async
runtime from hipp** — the caller drives them with whatever executor
they have, tokio on a host or embassy on a chip (§5.1.1) — which is
exactly the part of the async story a microcontroller can afford.
hipp's lane is the intersection neither occupies:

> Give SQLite the no_std treatment Horton got. A database file that is a real
> SQLite file, opened by a `no_std + alloc` Rust engine that runs on an
> ESP32-S3, in a kernel, in WASM, on bare metal — anywhere LLVM targets and
> an allocator exists, or the caller can hand the arenas memory (§7.5).

**Fast and tiny are requirements, not polish — because embedded is the
lane.** A hosted server already has SQLite and Turso; a microcontroller
with 256 KiB of RAM has neither. Every design choice in this spec is
therefore answerable to two questions: what does it cost in bytes
(§4.1's gates make the answer a number, checked in CI), and what does it
cost per row (the arena strategy, §7.5, exists so the hot path's answer is
"no allocation, no per-object free, no fragmentation").

**Name.** For **D. Richard Hipp**, creator and principal maintainer of SQLite
[CMU-HIPP][WIKI-HIPP]. **Tagline: "It's Hipp to be no_std."**

That differentiation is the whole strategy, and it disciplines every choice
below: if a feature only matters on a hosted desktop, it is late; if a
shortcut breaks the file format, it is never.

## 2. Compatibility is three axes, not one claim

Conflating these is how SQLite-shaped projects drown. hipp treats them as
separate axes — two it pursues in phases, one it has decided against
permanently:

- **(a) File-format compatible** — hipp reads and writes byte-level real
  `.db` files: a file hipp writes passes `PRAGMA integrity_check` in real
  SQLite and returns the inserted rows; a file real SQLite writes is scannable
  by hipp. This is the *identity* of the project.
- **(b) SQL-dialect compatible** — the SQL hipp accepts means what SQLite
  means by it: type affinity, comparison order, `INTEGER PRIMARY KEY` rowid
  aliasing, NULL semantics. This is a *long tail*, grown statement by
  statement under differential testing.
- **(c) C-API compatible — OUT OF SCOPE, permanently (D3, 2026-10-16).**
  `open / prepare / step / column` is a proven API *shape*, and hipp's Rust
  API follows that shape idiomatically (§5.8). Actually exporting a
  link-compatible `sqlite3_*` C ABI is a different, much larger commitment
  (error-code fidelity, memory ownership across FFI, callback registration)
  — and Mark's decision is **no**: not in v0.1, not in a later phase, not
  behind a feature. hipp is a Rust library, **not a drop-in SQLite
  replacement**, and no wording anywhere in this spec should be read as
  leaving that door open.

### Phases

| Phase | Axis | Scope |
|---|---|---|
| **v0.1** | (a) + a slice of (b) | §9 milestone: create/open, `CREATE TABLE`, `INSERT`, full-scan `SELECT`, autocommit via rollback journal. Rowid table B-trees only. |
| **v0.2** | (b) | `WHERE` (rowid point lookup + simple predicates), `UPDATE`, `DELETE`, explicit `BEGIN/COMMIT/ROLLBACK`, bound parameters, `ORDER BY`/`LIMIT` via in-memory sort. **Plus the `hipp` CLI** (D4, §9.4) — a tooling deliverable riding this phase, not a compatibility axis. |
| **v0.3** | (a)+(b) | Index B-trees: `CREATE INDEX`, secondary-index lookup, `UNIQUE` enforcement. Freelist reuse hardening. |
| **v0.4** | (b) | Joins (nested-loop), aggregates + `GROUP BY`, expression library growth, `ALTER TABLE ADD COLUMN`, views as stored SQL (inert). |
| **later** | (a) | WAL **read** support, then WAL write — only if a real target demands it (see RISKS R3). Auto-vacuum/ptrmap only if files produced by auto-vacuum SQLite must be *written*, not just read. |
| **never** | (c) | A `sqlite3_*` C ABI in any form — no `hipp-ffi` crate, no feature flag. Decided against, permanently (D3, §11). |

**Claim wording (D5, 2026-10-16).** The public claim is the narrow one,
in Mark's words: **hipp "reads and writes SQLite database files."** That
sentence — with a phase and the pinned version attached wherever a claim
is made precisely (e.g. "…the SQLite 3.54.1 file format, v0.1 profile") —
is the claim template for the README, crate metadata, and announcements.
Unqualified "SQLite-compatible" is marketing, not an engineering
statement, and hipp will not make it as a claim. (Inside this spec,
"compatible" remains ordinary engineering shorthand for axes (a)/(b)
against the pin; it is never a claim template.) The pin itself (D1):
**SQLite 3.54.1**, the newest release at code-start, verified 2026-10-16
[SQL-REL]; header `SQLITE_VERSION_NUMBER` (offset 96) = 3054001.

**Build profiles are part of the claim.** Full compatibility pulls toward
size — every dialect feature is code — so the SQL surface is feature-gated
and a build names its profile (§4.1):

- **`tiny` profile** (`--no-default-features` + core features): file-format
  core, rollback journal, rowid table B-trees + overflow, `CREATE TABLE` /
  `INSERT` / scan `SELECT`, UTF-8. *Excludes:* index B-trees, WAL, UTF-16
  transcoding (files error cleanly), and every dialect feature past the
  v0.1 grammar. This is the microcontroller build, and the one the §4.1
  size gates bind.
- **Full profile** (default features): grows by phase — v0.2 dialect,
  v0.3 index B-trees, later WAL — each feature a cargo flag that compiles
  *out* when off, so a target pays code size only for the dialect it uses.

A `tiny` build claiming the full profile's compatibility would be exactly
the unqualified claim this section forbids; docs and crate metadata label
profiles explicitly.

## 3. Explicit non-goals for v0.1

Do not build, stub, or design-around these in the milestone:

1. WAL — reading or writing. Header read/write version `2` ⇒ `Error::WalUnsupported` on open for write; read-only scan of a WAL-mode file is also deferred, because committed frames may live only in the `-wal` sidecar.
2. Index B-trees, `CREATE INDEX`, `UNIQUE`/`PRIMARY KEY` constraints other than the `INTEGER PRIMARY KEY` rowid alias. (The B-tree code is written generically enough not to preclude them; see §5.4.)
3. `WHERE`, `ORDER BY`, `LIMIT`, joins, aggregates, subqueries, expressions beyond literals and column references in `SELECT`.
4. Bound parameters (`?`, `?NNN`, `:name`). Literals only.
5. `UPDATE` / `DELETE` / `DROP TABLE`.
6. Explicit transactions. Every statement is its own autocommit transaction.
7. UTF-16 files: creating is UTF-8 only; opening a UTF-16le/be file returns `Error::UnsupportedEncoding`, never a silent mojibake.
8. Auto-vacuum / incremental-vacuum (ptrmap pages), reserved-bytes-per-page ≠ 0 on write, `WITHOUT ROWID` tables, virtual tables, triggers, foreign keys, collations beyond `BINARY`.
9. Multi-process access and advisory locking enforcement. v0.1 assumes it is the only process touching the file (§5.3 specifies the eventual model).
10. A CLI **in v0.1**. The CLI is decided (D4) and scheduled for **v0.2** (§9.4); v0.1 stays library-only, and a `std` example binary in `examples/` is enough to demo the milestone.
11. An async **runtime** — executor, task spawning, timers, channels.
    hipp exposes async *primitives* at the `Storage` boundary and in its
    operation futures (D2, §5.1), driven by the **caller's** executor;
    the core ships no runtime and depends on none — no tokio, ever.
12. Performance *tuning* beyond the foundations — no micro-optimisation,
    no benchmark-chasing until the differential suite is green. The
    foundations themselves are not tuning and are in scope from day one:
    arena-first allocation (§7.5) and the §4.1 size/RAM gates, because
    retrofitting either is a rewrite.
13. A `sqlite3_*` C ABI — listed here for completeness, but unlike the
    rest of this section this is not a v0.1 deferral: it is **permanently
    out of scope** (D3, §2). No FFI crate, no feature flag, no "later".

## 4. Hard constraints

Borrowed from Horton's discipline, adjusted for a SQL engine:

- `#![no_std]` in the core crate, with `extern crate alloc`. `std` appears
  only in tests, examples, and the host storage implementation (§5.1), which lives
  behind a feature or in a separate crate.
- **Zero dependencies in the core crate for v0.1.** Lexer, parser, varint,
  record codec, B-tree: all hand-rolled. (Horton runs an empty
  `[dependencies]` table; hipp keeps that line until a dependency earns its
  place in an ADR.)
- No panics in library code: no `unwrap`/`expect`/unchecked indexing on
  file-derived data. Every offset, page number, varint, and chain read from a
  file is checked — a `.db` file is *untrusted input* (RISKS R8).
- Every failure is a `Result` with a remedy-named error (§5.8), following
  Horton's error-model pattern.
- **Arena-first allocation (§7.5).** No code outside the `arena` module
  touches the global allocator directly. Everything else allocates from a
  scoped arena or from the pager's caller-configured fixed page pool.
- RAM/heap is **measured, not estimated**: per-arena high-water marks plus
  heap watermark tests with a counting allocator, in the spirit of
  Horton's `tests/profile.rs` budget gate (§7.3), and the hard budgets of
  §4.1 are CI gates, not documentation.
- **Async primitives, no runtime (§5.1).** The core's async surface is
  `async fn`s and futures built only on `core` (`Future`/`Poll`/`Waker`);
  it never spawns, blocks, or depends on an executor — no tokio, no
  runtime crate, ever (the §4.1 tiny/no_std gates depend on this).
  Operation futures borrow state stored in the `Connection`/`Statement`
  structs (§5.1.1); any executor in the building is the caller's.
- Toolchain: stable, `RUSTUP_TOOLCHAIN=stable`, edition 2024. A `no_std`
  gate in CI (build the core for a bare-metal target) is part of the
  milestone definition of done, not a later cleanup (§9.3).

### Target tiers

- **Tier 1 (design centre):** microcontrollers and other embedded targets —
  ESP32-S3 (e.g. via `esp-alloc`), Cortex-M / RISC-V with an allocator,
  WASM (`wasm32-unknown-unknown`), kernels and firmware. A global allocator
  is the baseline; a target *without* one is still reachable by supplying
  the arenas' memory directly (§7.5.6) — that is a designed configuration,
  not a port.
- **Tier 2 (hosted):** Linux/macOS/Windows via the `std` file implementation — this
  is where the differential test suite runs. Hosted comfort never sets a
  budget; Tier 1 does (§4.1).
- **Not a target:** a target with no allocator *and* no spare RAM to lend
  an arena. There is no honest version of a SQL engine there, and v0.1
  does not pretend otherwise (§7.4).

### 4.1 Hard gates — size, RAM, speed (Waymaker tradition)

Waymaker ships under CI-enforced byte budgets (≤768 B RAM, ≤12–13 KiB
flash for its kernel); hipp inherits the habit at database scale. These
are **gates**: CI fails, the milestone is not done (§9.3). Numbers bind
the `tiny` profile (§2) built for **`thumbv7em-none-eabihf`**
(Cortex-M4F — the conservative stand-in for ESP32-S3-class parts; the
ESP32-S3 xtensa build is checked where the toolchain allows, but the
budget is proven on the target CI can always run), release profile
`opt-level = "z"`, LTO on, `panic = "abort"` in the demo harness.

| Gate | Budget (v0.1) | Measured how |
|---|---|---|
| Code size | `.text` + `.rodata` ≤ **128 KiB** (stretch: 96 KiB) for the `hipp_mem_demo` binary, tiny profile | `cargo size` / section dump in CI on `thumbv7em-none-eabihf` |
| Static RAM | `.data` + `.bss` ≤ **8 KiB** (engine statics only; caller buffers excluded and accounted below) | section dump, same build |
| Total engine RAM, tiny reference config | ≤ **64 KiB**, composed of: page pool 4 pages × 4096 = 16 KiB · catalog arena ≤ 16 KiB · statement arena ≤ 16 KiB · transaction arena ≤ 8 KiB · statics ≤ 8 KiB. Excludes the caller's SQL input buffer and storage-device internals, which the caller owns and counts. | arena high-water marks (§7.5) + section sizes in the profile test, A4-style workload at page size 4096 |
| Steady-state allocation | **0** global allocations after warm-up in a repeated prepare/step scan loop within arena caps; **0** allocations per row in a full-scan hot loop | counting allocator in tests (§8) |
| Reset cost | arena reset is O(1) in live objects (chunk retention, §7.5.3); no per-object drop/free anywhere in statement teardown | code rule + allocation/drop counters in tests |
| Speed regression | host `MemStorage` scan/insert benches may not regress > 20% against the recorded baseline without an ADR | benches with a checked-in baseline (§8); absolute throughput targets are set from the first landing's numbers, not invented here |

If a v0.1 feature cannot fit the gates, the feature moves to the full
profile or a later phase — the gates do not move to fit the feature.
That sentence is the whole point of writing the numbers down before code
exists.

## 5. Architecture

```
  ┌────────────────────────────────────────────────────────────┐
  │ Session API   Connection · Statement (prepare/step/column) │  §5.8
  ├────────────────────────────────────────────────────────────┤
  │ Executor      tree-walking over an arena Plan tree         │  §5.7
  ├────────────────────────────────────────────────────────────┤
  │ Planner       AST → Plan (scan / insert / create only)     │  §5.7
  ├────────────────────────────────────────────────────────────┤
  │ SQL frontend  lexer → parser → AST (statement-arena trees) │  §5.6
  ├────────────────────────────────────────────────────────────┤
  │ Schema        sqlite_schema rows → in-memory catalog       │  §5.5
  ├────────────────────────────────────────────────────────────┤
  │ Record codec  Value ⇄ record (header of serial types)      │  §5.5
  ├────────────────────────────────────────────────────────────┤
  │ B-tree        table B-tree (+ index B-tree from v0.3)      │  §5.4
  ├────────────────────────────────────────────────────────────┤
  │ Pager         page cache · rollback journal · txn state    │  §5.3
  ├────────────────────────────────────────────────────────────┤
  │ Storage       caller-provided async Storage trait          │  §5.1
  └────────────────────────────────────────────────────────────┘
```

One crate, `hipp` (lib name `hipp`), modules per layer, for v0.1. Both
questions that once argued for waiting are now answered — the C ABI is
closed (D3) and the CLI is scheduled (D4, v0.2, as its own `std` crate in
a workspace, §10) — and neither changes v0.1's single-crate shape.
Layers only talk down; the B-tree never sees SQL, the parser never
sees pages.

**Memory cross-cuts every layer (§7.5):** three arena scopes — connection/
catalog (long-lived), transaction, statement — plus the pager's fixed page
pool. A layer does not choose where its allocations live; its scope does,
and the table in §7.5.2 is normative.

### 5.1 Storage — hipp's own async trait: runtime-agnostic primitives (REVISED 2026-10-16, D2; CORRECTED 21:50; REFINED 21:52)

> **Revision note (2026-10-16, Mark's D2) — this section was rewritten.**
> v0.1 of this spec chose a **synchronous** `Storage` trait. The reasoning
> was: a SQL `step()` is inherently a synchronous pull iterator; async pager
> I/O is the single largest complexity Turso carries (its compat doc
> documents same-connection write restrictions that follow from suspending
> writers mid-statement [TURSO-COMPAT]); a sync trait buys simplicity, and
> Horton's *philosophy* (caller-owned devices) could be borrowed without
> its poll-async *trait*. Mark overruled the sync choice in favour of
> poll-async. The sync reasoning is preserved in this note and in RISKS
> R15 — it correctly prices what poll-async costs — but it is superseded
> as guidance. Everything below is the async design.
>
> **Correction note (2026-10-16 21:50 CDT, Mark) — read this before the
> rest of the section.** Rev. 0.2 of this section went further than Mark
> meant: it designed literal composition with Horton's devices — a
> block-cache/read-modify-write adapter implementing hipp `Storage` over
> a Horton `BlockDevice`, a `hipp-horton` ownership question (Q1), shared
> test doubles. Mark corrected that, verbatim: *"Oh! I think I got
> confused. I thought we were using the horton idea as similar thing,
> not an actually adapter lol"*. Corrected reading (interpretation,
> flagged in §11; Mark can veto): **hipp gets its own poll-async storage
> trait, patterned on Horton's idea** — caller-provided device, `poll_*`
> + `Context` contract style, caller-driven — self-contained, with **no
> Horton dependency, no adapter, and no requirement that a Horton device
> work with hipp unmodified**. Horton is cited below as the pattern's
> origin and precedent, nothing more. The adapter design and Q1 are
> removed/closed as of this correction; if raw block devices ever matter,
> a bridge is an implementer-side concern and a possible future optional
> extra — explicitly out of v0.1–v0.2 scope and **not designed here**.
>
> **Refinement note (2026-10-16 21:52 CDT, Mark) — the surface itself.**
> Mark, verbatim: *"I think even going one further, just enabling the
> async primitives? Then someone can use tokio if they have it?"* Final
> form of D2: the corrected poll-*pattern* is exposed as **standard async
> Rust** — an `async fn` `Storage` trait and operation futures — not as a
> bespoke hand-rolled `poll_*` public API. The explicit state machines
> of §5.1.1 remain, but as the *internal implementation* and the
> drop-safety mechanism, not the public story. hipp enables the
> primitives; the caller brings the runtime. (This matches Mark's
> standing rustgres call of 2026-10-09: runtime-agnostic future
> primitives, no tokio adapter crate.)

**What Horton does** [HORTON-SPEC §4.3]: a poll-based async `BlockDevice`:

```rust
pub trait BlockDevice {
    type Error;
    const BLOCK: usize;
    fn poll_read_block(&self, cx: &mut Context<'_>, id: u64, buf: &mut [u8])
        -> Poll<Result<(), Self::Error>>;
    fn poll_write_block(&mut self, cx: &mut Context<'_>, id: u64, buf: &[u8])
        -> Poll<Result<(), Self::Error>>;
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
}
```

Horton's device layer is poll-based underneath (so, per its ADR-0001,
it needs no executor and no unstable features), and Horton's `Db` API
above it is `async fn`s the host drives with any executor. hipp takes
that two-layer shape and makes the **top layer the public one** (D2
refinement): what a hipp user sees is ordinary async Rust.

- **Async primitives are the primary surface.** `Storage` is an
  `async fn` trait (RPITIT — see the trait sketch below), and every
  engine operation — `open`, `prepare`, `step`, `commit` — is an
  `async fn` / operation future built **only** on `core`'s
  `Future`/`Poll`/`Waker` machinery. hipp core has **no runtime
  dependency and no tokio dependency, ever** — the tiny/`no_std` gates
  (§4.1) depend on it, and so does the pitch: *"someone can use tokio if
  they have it"* — or embassy, or smol, or a hand loop. The caller's
  executor polls hipp's futures; wake registration is the executor's
  standard contract, threaded through the `Storage` futures hipp awaits.
- **Hipp-native, self-contained.** The trait, its test doubles, and its
  implementations are hipp's own. A device written for Horton is written
  in a similar idiom but does **not** satisfy hipp's trait and is not
  expected to work with hipp unmodified; there is no adapter making it
  do so (correction note above; RISKS R7/R15). hipp's counting/failing
  test device — the crash-injector pattern of §8 — is written against
  hipp's trait, and gains a second axis: `Pending` injection (§8, A11).
- **Caller owns the device; the engine owns no I/O policy and names no OS.**
  Associated `Error` type; engine errors wrap it, never replace it.
  `sync` (the counterpart of Horton's `poll_flush`) is a first-class,
  explicit durability barrier — hipp's commit protocol is built from
  ordered syncs in the same spirit.
- Whole-unit I/O with a length contract, no partial magical reads.

**Why the pattern still needs hipp's own shape.** Even as a pattern,
Horton's trait cannot be hipp's trait — not because of async, but because
of shape:

| Horton assumption | SQLite reality | hipp adaptation |
|---|---|---|
| Fixed `const BLOCK` device geometry | Page size is a *file property* (512–65536, header offset 16), varying per database [SQFMT §1.2]. Runtime, not const. | hipp's trait is **byte-addressed** (`read_at`/`write_at` with offsets), page size stays a pager-level runtime value. |
| One device, fixed regions (WAL / tables / manifest) | A database is a byte-addressed *file* plus named sidecars (`-journal`, later `-wal`/`-shm`) that are created, truncated, and deleted. | The trait has a sidecar open plus `set_len`/`delete` operations a block device doesn't need. |
| Append-only + immutable tables; never overwrite in place | SQLite's pager **overwrites pages in place** under journal protection. Raw NOR flash therefore cannot be the native model (RISKS R7). | Unchanged by D2, corrected or not — hipp's trait stays byte-addressed. Running it over raw block/flash media is an implementer-side concern (wear/erase translation beneath an implementation of this trait); a bridge of that kind is a possible future optional extra, out of v0.1–v0.2 scope, and is **not designed here**. |

So what transfers is the idiom, at exactly one level: hipp's own trait is
Horton-*inspired* — a hipp `Storage` implementation and a Horton
`BlockDevice` are recognisably the same *kind* of thing (caller-provided,
async, executor-agnostic), and that is where the relationship ends.
There is no second level: no adapter, no shared test doubles, no
co-versioning, and (per the 21:50 correction) no open ownership question
— Q1 is closed as moot in §11. v0.1's milestone targets (host files,
RAM) are the whole of v0.1's storage story.

**hipp's trait** (async, byte-addressed, page-honest):

```rust
pub trait Storage {
    type Error;
    type Sidecar: SidecarFile<Error = Self::Error>;
    /// Length of the main database file in bytes.
    async fn len(&self) -> Result<u64, Self::Error>;
    /// Read exactly `buf.len()` bytes at `offset` (page-aligned in practice;
    /// the header probe reads 100 bytes at offset 0 — the format's one
    /// sanctioned sub-page read [SQFMT §1.2]).
    async fn read_at(&self, offset: u64, buf: &mut [u8])
        -> Result<(), Self::Error>;
    async fn write_at(&mut self, offset: u64, buf: &[u8])
        -> Result<(), Self::Error>;
    /// Grow/truncate the main file. Growth is how new pages are allocated.
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    /// Durability barrier for the main file. Must not resolve `Ok`
    /// unless previously written bytes survive the target's stated crash
    /// model.
    async fn sync(&mut self) -> Result<(), Self::Error>;
    /// Open (creating if absent) one of this database's sidecar files.
    /// v0.1 needs exactly one kind: the rollback journal.
    async fn sidecar(&mut self, kind: Sidecar)
        -> Result<Self::Sidecar, Self::Error>;
}

pub enum Sidecar { RollbackJournal /* Wal, WalIndex: later phases */ }
```

**How the trait is async on `no_std`, concretely.** `async fn` in a
trait is stable Rust (RPITIT): each method desugars to a function
returning an associated `impl Future` type, generic over the
implementor. Consequences, all deliberate: **no `dyn Storage`** in the
core (no trait objects, no boxing, no allocation for dispatch — engines
are generic over `S: Storage` and monomorphise); no dependency (no
`async-trait` macro crate — §4's zero-dependency line holds); an
implementor just writes `async fn`s, in whatever style their platform
already uses. Internally, the pager's state machines (§5.1.1) need to
drive these futures one poll at a time *without* an executor and
without owning them indefinitely: the core keeps a **crate-private raw
poll trait** (`RawStorage`: `poll_read_at`/`poll_write_at`/`poll_sync`/…
in the older hand-rolled shape) as an implementation detail, with one
internal bridge — for any `S: Storage`, a core wrapper holds the single
in-flight `S` future in a slot inside the operation state and polls it.
That wrapper is not public API, not a second trait users implement, and
not a compatibility surface; it exists so the public story can be plain
async while the engine's insides stay explicit state machines. If the
first implementation spike shows the bridge fighting the borrow
checker, the fallback is to have state machines own the in-flight
future directly (its concrete type is known per `S`); the public trait
does not change either way.

`SidecarFile` carries the same `read_at / write_at / set_len / sync`
async shape plus a `delete` operation, because the rollback-journal
protocol ends by deleting the journal (or zeroing its header)
[SQL-LOCK §4.1].

A separate, optional `AdvisoryLock` trait (async `acquire(Shared /
Reserved / Exclusive)` / `release`) is defined in v0.1's API surface but
may be a no-op for embedded single-device targets; the host
implementation maps it to OS file locks in a later phase. Defining it
now costs nothing; retrofitting it would cost the pager its shape.

A RAM implementation (`MemStorage`, `alloc`-only, no `std`) ships in the
core crate: it is the test workhorse, the crash-injector host, and a real
deployment option for transient databases. `MemStorage`'s futures are
always immediately `Ready` — async does not force suspension on devices
that don't have any; a blocking flash or file implementation may
likewise complete on first poll. `Pending` exists for devices that
genuinely suspend (DMA, SD, network-backed storage).

#### 5.1.1 Who drives — the caller's executor; the state machines are inside

D2's final form makes the public surface ordinary async Rust — and that
creates the one problem this section exists to solve. **A dropped
future loses whatever lived inside it:** suspend a `step()` mid-B-tree
descent inside an `async fn`, drop the future (task cancelled, timeout,
caller lost interest), and anything stored in the future's locals is
gone. A database cannot work that way. So the primacy is split, and the
split is the design:

- **Publicly: futures.** `Connection::open`, `Statement::step`,
  `Connection::prepare`, `Connection::resume_txn` (§5.3) are `async fn`s.
  Each returns a future that **borrows** its `Connection`/`Statement`
  (`&mut self`) and owns nothing authoritative — it is a driver handle,
  not a container.
- **Internally: explicit state machines.** All durable progress —
  descent stack, current page pin, journal stage, commit stage — lives
  in state-machine fields stored **in the structs**, in arena/pool
  memory whose lifetimes §7.5 already fixes. The future merely advances
  the stored machine. This is implemented over the crate-private raw
  poll layer (§5.1): the machine is polled, the in-flight device future
  occupies one slot in the operation state, and a **checkpoint rule**
  binds the two together: *before any await that could suspend, the
  machine writes its checkpoint (stage + position) into the struct.*
  A future is therefore always droppable at an await point and
  re-creatable from the checkpoint — dropping costs at most the
  in-flight single device operation, never the operation's progress.

**Drop semantics, per operation (normative — this is public contract):**

| Dropped future | Consequence |
|---|---|
| `open` mid-open | No `Connection` exists yet; nothing to resume. The file is untouched (open mutates nothing before the header validates); retry `open` from scratch. A hot journal, if present, is recovered by that retry. |
| `prepare` mid-parse | The half-built plan dies with the *statement slot*, not the future — re-`prepare` restarts the parse cheaply; the connection and any transaction are unaffected. |
| `step` mid-scan (read) | Cursor state is in the `Statement`; a fresh `step` future continues from the cursor's checkpoint — the row in flight is re-produced, never skipped. |
| Write statement mid-execution (before commit stage) | Dirty pages live in the pager's cache and the journal protocol hasn't reached its commit point: the transaction is still *open* in the `Connection`. The caller may continue it with another statement, or call `resume_txn` to commit/roll back (§5.3). Nothing is half-written to the file. |
| **Any future mid-commit** | The commit **stage machine is persisted in the `Connection`** (§5.3), and every stage is idempotent. Dropping the driver future changes nothing on disk: the next `resume_txn().await` — or, if never resumed, journal recovery at the next `open` — finishes or unwinds the transaction. Never torn. |
| A `Storage` future itself, mid-device-op | The device sees a cancelled operation. Implementations must tolerate cancellation at any await point (a stated `Storage` implementor contract): a write may or may not have landed, which is exactly the uncertainty the journal protocol already exists to absorb. |

- **Who drives, per tier:** the caller, always. Tier 2 hosts: tokio
  (their choice — Mark's framing), smol, or any executor, including a
  trivial `block_on`. Tier 1: an embassy task, a firmware loop with a
  no-op-waker `block_on` for always-`Ready` devices (which then behave
  synchronously — async costs nothing there), or hand-polling a future
  with their own `Waker`. hipp itself is never in the scheduling
  business, ships no executor, and spawns nothing.
- **Size accounting (feeds §4.1):** the state machines are struct
  fields, so their size is a *constant* the profile test asserts
  (`size_of` of the operation-state enums is a §4.1/A10 gate input). The
  one stored-future slot (§5.1) is sized per concrete `Storage`
  implementor by monomorphisation — for the reference implementations
  it is likewise asserted in the profile test. The honest cost async
  adds is **code**: one resumable state machine per operation instead of
  straight-line sync code. §4.1's gates do not move for it (R15); the
  mitigation is a single generic op-driver pattern shared by all
  operations, no combinator machinery, no `async` trait objects.

#### 5.1.2 Cursors, suspension, and one operation at a time

- A cursor's state — descent stack, current position, **pin as a page-pool
  slot index** (§5.2) — lives in its owning `Statement`, so a cursor
  *spans* awaits by construction: suspension changes nothing it points
  at (pool slots don't move; arena chunks don't move, §7.5.3). Row values
  handed out by a completed `step` keep §5.5's validity contract
  (until the next `step`); an in-flight `step` hands out nothing.
- **One in-flight operation per connection.** Operation futures borrow
  their `Connection`/`Statement` mutably, so in the primary async API a
  second operation *while one is suspended* does not compile — the
  borrow checker enforces the rule the poll design had to check by hand.
  The runtime check survives only where futures can't reach: calling
  `resume_txn` (or `open`-recovery) while a transaction stage is already
  being driven returns `Error::OperationInFlight` — remedy: drive the
  suspended operation to completion first. hipp is
  single-connection-sequential at v0.1 anyway (§0); the rule exists so
  two state machines can never interleave writes to one pager.
- Statement drop never strands the engine: execution state dies with the
  statement, but **transaction state lives in the `Connection`** (§5.3),
  so dropping a statement (or its future) mid-commit leaves a resumable
  — or, if never resumed, a hot-journal-recoverable — transaction, never
  a torn one.

### 5.2 Pager & page cache

- Page numbers are `u32`, 1-based; page 1 carries the 100-byte database
  header before its B-tree header [SQFMT §1.3, §1.6].
- The pager owns: page size, database size in pages, file change counter,
  and the dirty-page set for the current transaction.
- **Cache:** caller-configured capacity *in pages* (not bytes — page size
  varies), backed by **caller-owned fixed page buffers**: the pool is
  carved once at `open` from the connection arena (§7.5.2) — or supplied
  outright by the caller as a byte slice — and never grows, shrinks, or
  reallocates for the life of the connection. **No allocation occurs in
  the pager's read/write/sync path, at all**; a page is a pool slot index,
  not a pointer, so pins survive any arena chunk growth elsewhere.
  Eviction is CLOCK, the same simple scheme Horton's block cache uses —
  but note the difference: Horton's cache is an optimisation over immutable
  tables, hipp's cache is *the* write staging area. Pinned (dirty or
  in-use) frames are never evicted; a cache full of pinned frames is
  `Error::CacheExhausted`, a remedy-named error ("finish the statement /
  enlarge the cache"), not a spill protocol. Spill-to-journal mid-transaction
  is real SQLite behaviour and is deferred.
- All page access goes through the pager. Nothing above §5.3 touches
  `Storage` directly. Ever.
- The pager validates the header on open, in this order: magic string
  `"SQLite format 3\0"` at offset 0; page size (power of two 512–32768, or
  the `1` sentinel for 65536) at 16; read/write versions at 19/18
  (`2` ⇒ WAL rules, §3.1); reserved bytes at 20; payload fractions at
  21–23 must be 64/32/32; text encoding at 56 (1 = UTF-8; 2/3 ⇒ §3.7);
  in-header database size at 28 is trusted **only** when it is non-zero and
  the change counter at 24 equals the version-valid-for number at 92 —
  otherwise the size comes from the actual file length [SQFMT §1.3.7].
  Every one of these checks exists because a real file in the wild violates
  the naive version of it.

### 5.3 Transactions — rollback journal, Horton-style commit discipline

v0.1 implements exactly one protocol, the classic one [SQL-LOCK §4–5],
[SQL-WAL §2]:

1. **Begin (first write):** snapshot originals — before any page is modified
   in cache, its original image is appended to the rollback journal, along
   with the original database size in pages in the journal header.
2. **Commit:** (i) `sync().await` the journal; (ii) write all dirty pages to the
   main file — page 1 among them, already carrying the incremented file
   change counter and the updated in-header size / version-valid-for
   fields; (iii) `sync().await` the main file; (iv) delete the journal — the
   delete *is* the commit point.
3. **Rollback:** replay journal images into the main file, truncate to the
   recorded original size, `sync().await`, delete the journal.
4. **Recovery on open:** a journal that exists, is well-formed, and has no
   live `RESERVED` lock against it is *hot* and is rolled back before the
   first read [SQL-LOCK §4.1]. In v0.1's single-process world: a journal
   present at open is hot by definition.

**Under D2, this protocol is a resumable state machine, not a
straight-line routine.** The commit/rollback stage — `WriteJournalPage(n)
→ SyncJournal → WriteDataPage(n) → SyncData → DeleteJournal → Done`, and
rollback's mirror — is persisted as an explicit state enum in the
`Connection` (§5.1.1), together with the next page index, so any
driving of the operation future (`step`, `resume_txn`) advances it as far as the device's
`Ready`s allow and a `Pending` anywhere in the sequence simply pauses
it. The ordering invariants above are properties of the state machine's
transitions, not of a call stack: a stage cannot be skipped, and a
`Pending` sync must be re-polled, never assumed. Every stage is
idempotent or safely repeatable (re-writing the same journal image,
re-syncing, deleting an already-deleted journal all succeed), which is
what makes abandonment safe: drop the statement, drop the connection,
lose power — the file is left either pre-image, post-image, or carrying
a hot journal the next open rolls back, which is precisely the crash
case §8 injects. Transaction state lives in the `Connection`, never in
a statement or a future, so a statement dropped mid-commit leaves the
transaction *resumable* via `Connection::resume_txn().await` — and
until it is finished or rolled back, the one-in-flight rule (§5.1.2)
refuses a second driver with `OperationInFlight`.

**The journal is a format, not a private scratch file** [SQFMT §3]. If
hipp crashes mid-transaction, the next opener may be real SQLite, so the
`-journal` sidecar must be byte-exact SQLite format, not a hipp invention:
an 8-byte magic header (`d9 d5 05 f9 20 a1 63 d7`), then big-endian
fields for the segment page count (or −1 for "to end of file"), a random
checksum nonce (fresh per transaction — the engine needs a caller-supplied
entropy source or nonce counter on `no_std`; a predictable nonce weakens
stale-sector detection and must be documented per target), the initial
database size in pages, the assumed sector size, and the page size; the
header is zero-padded to one full sector, in its own sector, before any
page records. Each page record is `u32 page number | original page
content | u32 checksum`, where the checksum starts from the nonce and
sums bytes sampled every 200 bytes working backwards from `page_size −
200` — sparse by design in the format, and implemented exactly as
specified, not "improved". A page appears at most once per journal.
Freelist *leaf* pages carry no information and are the format's one
sanctioned exemption from journalling. hipp writes page count as −1 in
v0.1 (single segment) and must *read* segmented journals (count > 0,
repeated headers) during hot-journal recovery, because SQLite writes both
shapes. The crash-injection suite (§8) therefore also has a cross-tool
case: hipp is killed mid-commit and **real SQLite** performs the recovery
(A6 extension) — journal-format fidelity is tested against the other
implementation, not against hipp's own reader.

This is Horton's manifest-commit pattern wearing different clothes: stage
everything, one ordered sync sequence, a single atomic commit point, and a
recovery rule that always lands on a committed state. The crash injector
(§8) enumerates every write/sync boundary in the sequence above.

**Locking, specified now, implemented later.** Rollback-mode SQLite uses
`UNLOCKED → SHARED → RESERVED → PENDING → EXCLUSIVE`, with the lock-byte
page at offset 2³⁰ mediating multi-process access [SQL-LOCK §3],
[SQFMT §1.2]. v0.1 defines the states in the pager and takes the whole
sequence internally (so the state machine's *shape* is tested), but does not
promise inter-process exclusion until the host `AdvisoryLock` implementation lands.
Two hipp processes on one file in v0.1 is **unsupported and documented as
such**, loudly, in the crate docs — silent corruption is the only
unacceptable outcome (RISKS R4).

**Why not WAL first:** WAL inverts the protocol — changes append to a
`-wal` file, commit is a commit-frame, reads consult a wal-index in shared
memory (`-shm`), and a checkpoint protocol copies frames back [SQL-WAL §2].
That shared-memory requirement is precisely what many `no_std` targets
don't have, it makes the database a three-file object (bad for "database as
an application file format" uses), and rollback mode is the format's
default. hipp files are created in rollback mode (header versions `1/1`).
WAL is a later phase gated on a concrete target needing it.

### 5.4 B-tree layer

Implements the format's page model faithfully [SQFMT §1.6]:

- Page types: `0x02` interior index, `0x05` interior table, `0x0A` leaf
  index, `0x0D` leaf table. v0.1 writes only table pages; the type byte is
  validated on every page read, and encountering index pages on a scan path
  in v0.1 is `Error::Unsupported` (they can only belong to indexes, which
  v0.1 doesn't create but a foreign file may contain — scanning a *table*
  never touches them).
- Page layout: 8-byte leaf / 12-byte interior header (first-freeblock,
  cell count, cell-content start with `0` meaning 65536, fragmented free
  bytes, rightmost pointer on interior pages), cell pointer array in key
  order, cells growing from the end of usable space, freeblock chain.
  Insertions maintain the pointer array sorted; the cell *content* area is
  append-at-front — physical order is not logical order.
- **Defragmentation** is implemented from the start (repack when free space
  suffices but is fragmented): it is ~30 lines and every real-world file
  with deletions needs the reader to at least tolerate freeblocks.
- Table B-tree cells: leaf = `varint payload_len | varint rowid | payload |
  [u32 overflow page]`; interior = `u32 left child | varint rowid`
  (the divider key is the largest rowid in the left subtree's ordering
  convention per [SQFMT §1.6]).
- **Overflow** is not optional garnish; it is format correctness for any
  payload larger than a page fraction. With usable size `U` and payload
  `P`: table-leaf threshold `X = U − 35`; if `P > X`, bytes kept on-page
  `K = M + ((P − M) mod (U − 4))` with `M = ((U − 12)·32/255) − 23`, using
  `K` if `K ≤ X` else `M`. Index pages use `X = ((U − 12)·64/255) − 23`.
  Overflow pages chain via a leading big-endian u32 next-page pointer,
  carrying `U − 4` payload bytes each [SQFMT §1.6–1.7]. The reader *and*
  writer both implement this in v0.1; the acceptance test includes a
  10 KiB text value at page size 4096 (§9.2).
- **Balance:** insert path implements leaf split and root split; interior
  split propagates up a remembered path (no parent pointers exist in the
  format — the cursor records its descent stack, which is also why the
  executor wants allocation, §7.2). Balance-by-redistribution and merge on
  delete arrive with `DELETE` in v0.2.
- **Freelist:** allocation pops the freelist first (trunk page at header
  offset 32, count at 36 [SQFMT §1.3.8]); growth appends at EOF. v0.1 never
  *returns* pages to the freelist (no `DELETE`/`DROP`), but must *consume*
  foreign freelist pages correctly when inserting into a SQLite-made file,
  including zeroing/validating trunk leaf arrays defensively.
- Page allocation/deallocation updates happen inside the pager's dirty set
  so a rollback restores them for free — that is the entire point of
  journalling originals.

Cursor API (internal): `seek(rowid)`, `first()`, `next()`, `insert(rowid,
payload)`, `payload()` with overflow reassembly into the statement arena's
reusable row buffer (§7.5.2) or a caller buffer — never a fresh allocation
per row. The descent path stack lives in the statement arena (its depth is
runtime data, §7.2). Cursors pin their current page (by pool index, §5.2),
and a cursor's state lives in its owning `Statement` — never in the
transaction arena, and never across statements (§7.5.5). That placement
is also what D2 requires: the cursor state sits in the struct, not in a
future, so a cursor spans `Pending` suspensions without losing its
descent or its pin (§5.1.2).

### 5.5 Record codec & schema

**Record format** [SQFMT §2.1]: a payload is a header (varint header-length
including itself, then one varint serial type per column) followed by the
body. Serial types, in brief: `0` NULL; `1..=6` big-endian integers of
1/2/3/4/6/8 bytes; `7` big-endian f64; `8`/`9` the constants 0/1 (schema
format 4); `10`/`11` reserved, never in a well-formed file — a reader must
reject, not improvise; even `N ≥ 12`: blob of `(N−12)/2` bytes; odd
`N ≥ 13`: text of `(N−13)/2` bytes.

```rust
pub enum Value<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),   // bytes, in the database encoding — NOT String (§0)
    Blob(&'a [u8]),
}
```

Decoded values are **arena slices, not owned vectors**: a row decoded
during `step()` points into the current page or into the statement arena's
overflow-reassembly buffer (§7.5.2), and stays valid until the next
`step()` on that statement (the per-row checkpoint rewind, §7.5.2) or
finalize — the same validity contract SQLite's `sqlite3_column_*` offers,
stated in the type. A caller who needs a value to outlive that copies the
bytes; an owned `OwnedValue` (the one place a `Vec` may appear at the API
surface) exists for exactly that, on request, never by default on the
hot path.

- Text as bytes is a deliberate correction of a documented Turso limitation:
  Turso represents text as a Rust `String` and substitutes U+FFFD for
  non-UTF-8 bytes where SQLite preserves them [TURSO-COMPAT]. hipp carries
  bytes end-to-end; UTF-8 interpretation happens only at API boundaries
  that ask for it, lossily and explicitly.
- Integers encode at the *smallest* serial type that fits — byte-exactness
  with SQLite's writer is not required (any valid encoding reads back the
  same value), but smallest-fit is what SQLite does and keeps files small.
- A record may contain **fewer** values than the schema has columns (files
  written before an `ALTER TABLE ADD COLUMN`); missing trailing columns
  materialise as their declared defaults [SQFMT §2.1]. The codec therefore
  decodes against a column count, not in isolation.
- Comparison order for index keys (v0.3, but the comparator is written in
  v0.1 because `SELECT` ordering by rowid needs its integer half):
  NULL < numeric (int and real compare *numerically across types*) < text
  (by collation, `BINARY` = memcmp) < blob (memcmp) [SQFMT §2.2].
- **Affinity** is where dialect compatibility actually lives. v0.1
  implements the minimum honest subset: column affinity derived from the
  declared type string (contains `INT` ⇒ INTEGER; `CHAR`/`CLOB`/`TEXT` ⇒
  TEXT; `BLOB` or empty ⇒ BLOB; `REAL`/`FLOA`/`DOUB` ⇒ REAL; else NUMERIC),
  applied on `INSERT` to literals, and the `INTEGER PRIMARY KEY` rule: that
  column is the rowid, stored in the record as NULL, read back from the key
  [SQFMT §2.3]. The full affinity/conversion matrix is v0.2 differential
  work, explicitly (RISKS R5).

**Schema:** page 1 is the root of the `sqlite_schema` table B-tree, whose
rows are records of `(type, name, tbl_name, rootpage, sql)` [SQFMT §2.6].
On open, hipp scans it, re-parses each `sql` text with its own parser (the
parser must therefore accept the full `CREATE TABLE` grammar SQLite emits,
including quoted identifiers it may not itself generate), and builds the
in-memory catalog, allocated in the **catalog arena** (§7.5.2): table
definitions with column names, declared types, affinities, root pages,
and the rowid-alias column index, all as arena-backed collections and
string slices. The catalog is rebuilt if the schema cookie (header
offset 40) changes between statements — the prepared-statement
invalidation rule [SQFMT §1.3.9] — and a rebuild is the catalog arena's
only reset, permitted only when no statement is live (§7.5.5). `CREATE TABLE`
appends a `sqlite_schema` row and bumps the schema cookie inside the same
journal transaction as the new root page's allocation.

### 5.6 SQL frontend

Hand-written lexer + recursive-descent parser producing an AST allocated
entirely in the **statement arena** (§7.5.2) — no `Box`, no per-node
global allocation; teardown is the arena's bulk reset:

```rust
pub enum Statement<'a> {
    CreateTable { name: Ident<'a>, columns: AVec<'a, ColumnDef<'a>>, if_not_exists: bool },
    Insert { table: Ident<'a>, columns: Option<AVec<'a, Ident<'a>>>, values: AVec<'a, Expr<'a>> },
    Select { columns: SelectList<'a>, table: Ident<'a> },   // v0.1: scans only
}
pub enum Expr<'a> { Null, Integer(i64), Real(f64), Text(&'a [u8]), Blob(&'a [u8]),
                Column(usize) /* bound in planner */, Neg(&'a Expr<'a>) }
// AVec: an arena-backed growable array (amortised bump growth, §7.5.3).
```

- Keywords are ASCII-case-insensitive; identifiers may be bare, `"…"`,
  `` `…` ``, or `[…]` quoted (SQLite accepts all four; files in the wild use
  all four).
- String literals use `''` escaping; blob literals are `x'…'`; numbers are
  decimal integers (with overflow-to-real fallback, as SQLite) and floats
  with exponents. Hex integer literals (`0x…`) are SQLite-supported and
  cheap: include.
- Errors are structured (`Error::Parse { offset, kind }`), never strings in
  the core — `std` formatting layers can render them.
- Recursion depth in the parser is explicitly capped (a hostile or corrupt
  schema SQL text must not blow an embedded stack); cap exceeded ⇒
  `Error::Parse`, not a crash (RISKS R8).
- The grammar is a *subset by construction*: anything outside it is a parse
  error naming the offset, not a silent approximation. Growing the grammar
  is v0.2+ work driven by the differential corpus.

### 5.7 Planner & executor — tree-walk now, bytecode is a door left open

**Decision: tree-walking executor over an arena-allocated plan tree; no VDBE.**

```rust
pub enum Plan<'a> {
    CreateTable { /* resolved TableDef reference (catalog arena) */ },
    Insert { root: PageNo, rowid_alias: Option<usize>, values: AVec<'a, Expr<'a>> },
    Scan { root: PageNo, output: AVec<'a, usize> /* column indexes */ },
}
```

The plan lives beside the AST in the statement arena and dies with it.
Execution is Volcano-style: a `Scan` holds a B-tree cursor and yields one
decoded row (`&[Value]`, §5.5) per `next()`; `Statement::step()`'s
future is a thin driver over it (§5.1.1). (Terminology: "executor" in
this section is the *query* executor. hipp has no *task* executor —
§5.1.1.) The iterator's position lives in the `Statement` struct, so a
suspension — or a dropped `step` future — mid-scan loses nothing. Why not bytecode, when both SQLite and Turso compile to a VDBE
[TURSO-README]?

1. **Compatibility doesn't need it.** File format and dialect semantics are
   observable; the execution strategy is not (except `EXPLAIN`, deferred).
   Bytecode buys nothing on either v0.1 axis.
2. **Size and debuggability.** A register VM, opcode set, and codegen pass
   is easily half again the code of the rest of v0.1, on targets where code
   size is a budget line, and it inserts an opaque layer between a SQL bug
   and its cause exactly when the differential suite starts failing daily.
3. **The seam is cheap now, expensive later.** The AST → Plan → execute
   split means a later phase can retarget Plan to bytecode (for code size,
   for prepared-statement density, or for an `EXPLAIN` story) without
   touching the parser, planner logic, B-tree, or API. Choosing tree-walk
   *without* that split would be the real lock-in; with it, it's a deferral.

Revisit triggers (any one suffices to open the bytecode ADR): profiling
shows interpreter overhead dominating on a Tier-1 target; statement-cache
memory pressure from arena plan trees on a real workload; a decision to
pursue `EXPLAIN` parity.

### 5.8 Session API — Rust-only (D3), async-first (D2 final form)

hipp's API is a **Rust API** (D3, 2026-10-16: no `sqlite3_*` C ABI is in
scope, ever — §2). Its *shape* deliberately follows the
`open / prepare / step / column` rhythm every SQLite user knows, because
that shape is proven — but it is expressed with Rust ownership, Rust
lifetimes, and `core` futures, and it promises no C-level compatibility
of any kind. Dropping a `Statement` *is* finalizing it.

The `async fn` forms — the primary API (§5.1.1) — read the way SQLite
users expect:

```rust
let mut db = Connection::open(storage, Config::default()
    .cache_pages(16)                 // fixed page pool, carved at open (§5.2)
    .statement_arena(16 * 1024, 64 * 1024)   // initial, max bytes (§7.5.3)
    .txn_arena(8 * 1024, 32 * 1024)
    .catalog_arena(16 * 1024, 256 * 1024)).await?;
let mut stmt = db.prepare("INSERT INTO t VALUES (1, 'one')").await?;
stmt.step().await?;                  // -> Step::Done | Step::Row
let mut q = db.prepare("SELECT id, name FROM t").await?;
while q.step().await? == Step::Row {
    let id: i64 = q.column(0)?;      // typed access over Value
    let name: &[u8] = q.column_bytes(1)?;
}
```

Underneath, every one of those futures drives the explicit state
machine stored in its struct over the crate-private raw poll layer
(§5.1.1) — that layer is internal, not a second public API.
Executor-less callers — part of the Tier-1 centre of gravity — drive
the same futures with a trivial no-op-waker `block_on` or by polling
them by hand; nothing about the API changes. `prepare` runs lexer →
parser → planner against the current schema cookie (and is therefore an
I/O operation, hence async: a stale cookie forces a catalog rebuild,
§5.5); `step` executes incrementally for scans (one row per completed
`step` — the cursor is the state, §5.1.2); `column*` reads the current
row and is plain synchronous field access, no I/O, no await.

**Errors** (core enum, no strings, remedy-named in Horton's style):

| Variant | Meaning | Remedy |
|---|---|---|
| `BadMagic`, `TruncatedHeader` | not a SQLite file / shorter than 100 bytes | — |
| `UnsupportedPageSize`, `UnsupportedEncoding` | header field outside v0.1 envelope | — |
| `WalUnsupported` | header version 2 (or live `-wal` present) | open with SQLite, checkpoint, retry (later phase) |
| `ReservedBytesUnsupported` | offset-20 value ≠ 0 on a write path | — |
| `CorruptPage { page }`, `CorruptRecord`, `BadVarint`, `BadSerialType` | file bytes violate the format | — (file is damaged or hostile) |
| `OverflowChainCycle`, `PageOutOfRange` | hostile/corrupt linkage | — |
| `Schema { .. }` | unknown table/column, duplicate table | fix the SQL |
| `Parse { offset, kind }` | SQL outside the v0.1 grammar | fix the SQL |
| `CacheExhausted` | all cache frames pinned | finish statement / raise `cache_pages` |
| `OperationInFlight` | a second driver (`resume_txn`/recovery) was started while a transaction stage on this connection is already being driven (§5.1.2) | drive the suspended operation to completion, or finish/abort the transaction via `resume_txn` |
| `ArenaExhausted { scope, needed }` | an arena hit its configured max mid-operation (§7.5.4) | raise that arena's max in `Config`, shrink the statement, or supply more arena memory; a write in flight is rolled back, never half-committed |
| `Storage(E)` | the caller's device error | — |

## 6. What transfers from Horton — and what pointedly does not

| Horton pattern | hipp verdict |
|---|---|
| Caller-owned storage, engine owns no I/O policy | **Adopt** (§5.1) |
| One atomic commit point; stage, sync in order, then flip | **Adopt** — the rollback-journal protocol *is* this (§5.3) |
| Crash injector enumerating every write of an operation | **Adopt** (§8) — highest-value test technique in the shop |
| Differential oracle testing (Horton: a `BTreeMap` model) | **Adopt, upgraded** — the oracle is real SQLite itself (§8) |
| Decoder fuzzing with arbitrary bytes; "corrupt ≠ absent" | **Adopt** (§8) |
| Remedy-named, string-free error enum | **Adopt** (§5.8) |
| Measured RAM budgets as a CI gate | **Adopt, hardened** — per-arena high-water marks + the §4.1 byte budgets as CI gates |
| Bump allocator over caller-provided memory (`scratch-bump` feature) | **Adopt and promote** — Horton's optional scratch trick becomes hipp's core allocation strategy: scoped arenas, two of them caller-suppliable (§7.5) |
| Manifest with magic + CRC copies | **Reject** — the SQLite header *is* the root pointer; hipp invents no second one |
| LSM: memtable, SSTables, compaction, manifests, slots | **Reject entirely** — the format is a page B-tree; none of the LSM machinery maps |
| Fixed regions / whole-device layout | **Reject** — hipp manages a growable file, not a device |
| Const-generic sizing of everything | **Reject** — page size, column counts, payload sizes are runtime data (§7.2) |
| Async, caller-provided device (Horton's `BlockDevice` idea) | **Adopt as pattern (D2 final form, 2026-10-16)** — what transfers is the *idea*: hipp's I/O foundation is its **own** byte-addressed async `Storage` trait and operation futures, runtime-agnostic, in the idiom Horton's two-layer shape set (§5.1). Not transferred: Horton's trait itself, its device ecosystem, its test doubles, or any adapter between them — hipp depends on none of Horton. |
| Zero-alloc, zero-dep, `forbid(unsafe)` | **Partially adopt** — zero-dep yes; zero-alloc no (§7); `forbid(unsafe)` is the default posture, with any exception requiring an ADR and a bounded module |

## 7. The allocator verdict

### 7.1 Verdict

**hipp requires allocation, and gets it from arenas. `#![no_std]` +
`extern crate alloc` is the baseline, and the strategy on top of it —
settled by Mark, 2026-10-16 ("I think Arenas are what we go with anyway?
We want it to be FAST") — is arena-first (§7.5):** scoped bump arenas own
essentially all internal allocation, the global allocator's only job is
growing arena chunks, and on targets with no global allocator the caller
supplies the arenas' memory directly. Mark's lean ("go allocator on this
one") is correct, and this section is the rigorous version of why — because
the interesting question is not whether zero-alloc is *possible* (with
enough const generics, a subset is) but what it would *cost the product*,
and why arenas capture most of zero-alloc's speed and footprint wins
without paying that cost.

### 7.2 What zero-alloc would actually cost, subsystem by subsystem

Horton's zero-alloc law works because its universe is closed: keys and
values are bounded byte strings (`KEY_MAX`, `VAL_MAX`), tables hold bounded
entries, and *every* size in the system is a const generic chosen by the
caller at compile time. A SQL engine's universe is open in exactly the
places that matter:

- **Parser.** SQL text is unbounded input; an AST for `INSERT … VALUES
  (…), (…), …` has arity known only at parse time. Zero-alloc means a
  const-generic `MAX_COLUMNS`/`MAX_VALUES` that rejects real schema files
  hipp didn't write (a file-format-compat *failure*, not a style issue).
  The arena parser (§7.5) is the resolution: runtime-sized trees, bump
  allocated, freed in O(1) — the cost zero-alloc was trying to avoid
  (per-node heap traffic) is gone; only the const-generic *limits* are.
- **Records & values.** A record's size is data-dependent: a `TEXT` value is
  as long as it is. Zero-alloc forces every value through caller buffers
  with `BufferTooSmall` retry dances at *every* layer, or a hard
  `MAX_PAYLOAD` that makes hipp unable to read ordinary files (a 10 KiB
  text column is unremarkable). Horton's own error table shows this tax is
  affordable for a KV API; multiplied across parser, planner, executor, and
  column access it becomes the API. Under §7.5, decoded values are slices
  into a page or the statement arena's reassembly buffer (§5.5) — no
  copies, no retries, no per-value allocation.
- **B-tree splits.** Splitting a page redistributes a *runtime* number of
  cells; a split can cascade to the root along a descent path of runtime
  depth, and the cursor must remember that path. Zero-alloc makes that a
  const-generic (`MAX_DEPTH`) plus worst-case stack arrays on every
  cursor; the statement arena makes it a bump-allocated path stack (§5.4)
  paid only when a split actually happens.
- **Planner / executor.** Joins, sorts, and grouping are inherently
  variable-size working-set operations. v0.1's executor is scan-only, but
  the *architecture* must not foreclose v0.2's `ORDER BY` sort or v0.4's
  hash/nested-loop intermediates. Zero-alloc here doesn't simplify the
  design; it amputates the roadmap. Statement/transaction arenas give
  those future working sets a home with a bulk free (§7.5.2).
- **Schema.** The catalog is runtime-sized, built from file contents.
  There is no compile-time bound to generic over — so it lives in the
  long-lived catalog arena (§5.5, §7.5.2), rebuilt wholesale on schema
  change instead of node-by-node.

The pattern: **in Horton, bounds are *policy* the caller chooses; in a
SQLite-compatible engine, bounds are *facts about files other programs
wrote*.** Const-generics can express policy. They cannot express facts.

### 7.3 What `no_std + alloc` keeps — and the discipline that replaces the law

Everything hipp exists for survives the allocator requirement:

- **Targets:** ESP32-S3 with a heap, Cortex-M/RISC-V with `alloc`, WASM,
  kernels with a global allocator, Tallow-style firmware that provides
  one — and, via caller-supplied arena memory (§7.5.6), targets with no
  global allocator at all.
- **Portability shape:** the core still names no OS, no filesystem, no
  thread, no clock. Beyond `core`, the only platform services it assumes
  are an allocator *or* a caller memory loan (§7.5.6).
- **Boundedness, honestly restated:** what embedded callers actually need
  is not zero allocation, it's *no unbounded growth*. hipp recovers that
  with runtime limits in `Config`, all enforced, all erroring cleanly:
  `cache_pages` (the fixed page pool, §5.2), per-arena maxima (§7.5.3),
  `max_sql_bytes`, `max_columns`, `max_payload_bytes`. And the arenas
  restore the *number* Horton's discipline runs on: worst-case engine RAM
  is the sum of configured caps (§4.1's table is exactly that sum for the
  tiny profile), measurable per scope from arena high-water marks, and
  asserted in tests — a regression that doubles memory use fails CI like
  a wrong answer.

### 7.4 What is still not promised

A fully allocation-free hipp with Horton's const-generic shape. That
would be a different, smaller product (a fixed-schema embedded store
wearing SQLite's file format as an export format), and if Mark ever wants
it, it should be chartered as such — not smuggled in as a constraint on
this one. What §7.5 *does* promise is narrower and, for real targets,
sufficient: no global allocator is required if the caller can lend the
arenas memory, and no allocation of any kind happens on the pager's I/O
path or the scan hot loop.

### 7.5 The arena strategy (settled 2026-10-16)

Arenas are not an optimisation hipp might adopt later; they are the
allocation architecture, chosen because speed is a requirement (Mark,
§7.1) and because they serve footprint just as directly (§4.1).
Horton's spec reached for the same tool in miniature — a dependency-free
bump allocator over caller-provided memory for internal scratch
[HORTON-SPEC §10] — hipp promotes it from feature to foundation.

#### 7.5.1 Why arenas are the fast *and* tiny answer

- **Allocation cost ≈ a pointer bump.** No free lists, no size classes,
  no locks; an AST node or a decoded row costs an alignment fix and an
  addition.
- **Teardown is O(1) and drop-free.** A statement's entire working set —
  tokens, AST, plan, expression temporaries, materialised rows — dies by
  resetting a watermark. There is no per-object `Drop`, no free-list
  churn, and therefore no per-row destructor cost in a scan.
- **No fragmentation within a scope.** Objects in an arena die together,
  so the classic long-uptime embedded death (RISKS R9) — interleaved
  lifetimes slowly sawing the heap into unusable slivers — does not apply
  inside a scope. Chunk memory retained after a reset is reused by the
  next statement, so steady-state footprint is the high-water mark, not
  the sum of every statement ever run.
- **Locality.** A plan tree or a row's values sit contiguously, in use
  order — the cache-friendly layout a general allocator cannot promise.
- **Fallible by construction.** Arena allocation returns `Result`, where
  the global allocator *aborts* on failure. On Tier-1 targets that turns
  out-of-memory from a device kill into `Error::ArenaExhausted` (§7.5.4).

**The load-bearing rule:** no code outside the `arena` module allocates
from the global allocator directly — no stray `Box`, `Vec`, or `String` in
parser, planner, executor, B-tree, or pager. Everything allocates from a
scope's arena through its API (`AVec`, slices, `&'a` tree nodes). This is
enforceable by lint/grep in CI, and it is what makes §7.5.6 a
configuration instead of a rewrite.

#### 7.5.2 The scopes

| Scope | Lifetime | Holds | Reset rule |
|---|---|---|---|
| **Connection / catalog arena** | the `Connection` | schema catalog (`TableDef`s, name slices, parsed schema), any future prepared-statement cache, connection bookkeeping. The pager's page pool (§5.2) is carved from caller/configured memory alongside it at `open` and managed as fixed slots, not bump space. | Reset **only** on catalog rebuild (schema-cookie change), and only when no statement is live (§7.5.5). Chunk growth may occur here on `CREATE TABLE`; it is the cold path. |
| **Transaction arena** | one transaction (v0.1: one autocommit statement's write phase) | dirty-page list, journal-staging metadata, B-tree split scratch, freelist bookkeeping | Reset at transaction end — **after** commit's journal-delete stage has completed in the §5.3 state machine (or rollback has fully replayed). Nothing in it may be needed one instant after the transaction closes. |
| **Statement / query arena** | one prepared statement through finalize | lexer tokens, AST (§5.6), plan (§5.7), expression-evaluation temporaries, sort/group working sets (later phases), cursor descent stack, overflow-reassembly row buffer | Bulk reset on finalize/drop. **Per-row checkpoint:** `step()` records a watermark before decoding a row and rewinds to it on the next `step()`, so a million-row scan uses one row's worth of arena, not a million rows'. Row values handed to the caller are valid until that next `step()` (§5.5). |
| *(nested)* **Row checkpoint** | one `step()` | the current row's decoded values and temporaries | the rewind mechanism above — a checkpoint inside the statement arena, not a fourth arena. |

#### 7.5.3 Sizing and growth

- Each arena is a chain of chunks behind a bump pointer. Chunks are never
  moved or compacted once allocated (addresses handed out stay valid for
  the scope's life — §7.5.5 relies on this).
- `Config` sets **initial** and **maximum** bytes per scope (§5.8).
  Defaults are profile-dependent: the tiny reference configuration of
  §4.1 (statement 16 KiB / txn 8 KiB / catalog 16 KiB maxima) vs the
  hosted defaults in §5.8's example (larger maxima, same initials).
- Growth is geometric (chunk sizes double up to a per-chunk ceiling) and
  stops at the scope's maximum. A single object larger than the current
  chunk gets a dedicated chunk if the maximum allows — a 10 KiB overflow
  payload in a 16 KiB statement arena is normal operation, not an error.
- **Reset retains chunks.** Reset moves the watermark, not the memory
  back to the backing store; after warm-up, a workload that fits its caps
  performs **zero** backing allocations (§4.1's steady-state gate). The
  backing store is reclaimed only when the owning scope itself dies
  (statement drop may return its chunks; the catalog arena's die with the
  connection).
- Every arena tracks a **high-water mark**, exposed in `Config`-visible
  stats: sizing a deployment is reading three numbers off a test run, not
  guessing (§7.3).

#### 7.5.4 Exhaustion — the mid-query fallback

An arena running out mid-operation is a designed outcome, never a panic
and never silent truncation:

- Any allocation that would exceed its scope's maximum fails with
  `Error::ArenaExhausted { scope, needed }` (§5.8) at the point of
  request. All arena allocations are checked; there is no fallible path
  that "usually" has room.
- **At prepare time** (statement arena): the statement simply fails to
  prepare. No state has changed; nothing is cached anywhere.
- **During a read** (statement arena): the statement aborts; the error is
  the result of `step()`. The database is untouched by definition.
- **During a write** (statement or transaction arena): the statement
  aborts **and its transaction is rolled back** via the §5.3 journal
  protocol before the error returns. Arena exhaustion can therefore never
  half-commit — the journal, not the arena, is the atomicity mechanism,
  and dirty pages live in the fixed page pool (never in an arena), so an
  exhausted arena cannot strand a half-written page image.
- **No silent spill.** v0.1 does not fall back to unbounded global
  allocation when an arena caps out. Growth-to-maximum through the
  backing store *is* the fallback, and the maximum is the caller's
  decision. A caller who prefers "slow over failed" sets a generous
  maximum on a hosted target; a caller on a microcontroller gets the
  hard bound they configured. One mechanism, both policies.

#### 7.5.5 What must never live in a resettable arena

Bulk reset is only safe because these rules are absolute:

1. **Nothing that outlives its statement** may live in the statement
   arena: not catalog entries (catalog arena), not prepared plans meant
   for reuse (v0.1 has no cross-execution plan cache for exactly this
   reason; when one arrives, its plans are copied into the connection
   arena or re-planned per execution), not results a caller holds across
   statements (§5.5's validity contract).
2. **Page cache contents and pins** never live in any arena. Frames are
   fixed pool slots (§5.2), pins are slot indices — a pin taken before an
   arena reset, chunk growth, or statement teardown is still the same
   page after it.
3. **Cursors spanning statements are not a thing.** A cursor's state
   lives in its owning `Statement` struct; its descent stack may live in
   that statement's arena (same lifetime, by construction). No cursor,
   iterator, or row reference may borrow transaction-arena memory — a
   read transaction closes at finalize, and anything still pointing into
   the transaction arena afterwards would dangle across the reset.
4. **Dirty-page images** live in the page pool, and the transaction
   arena holds only *metadata about* them (page numbers, ordering) —
   small, and dead the moment commit/rollback completes (§7.5.2's reset
   ordering is normative, not advisory).
5. **The catalog** may be *rebuilt* only when no statement is live:
   live statements hold catalog references (root pages, column maps), so
   a schema-cookie change is observed at statement boundaries — prepare
   the next statement against the rebuilt catalog — never by resetting
   the catalog arena under a running statement.

#### 7.5.6 The no-allocator escape hatch, now structural

Because of the §7.5.1 rule, every byte hipp allocates comes from an
arena, and every arena sits on a **backing store**:

```rust
pub enum ArenaBacking<'m> {
    Global,                    // chunks from the global allocator (baseline)
    Caller(&'m mut [u8]),      // one caller-owned slab, sub-carved per scope
    // CallerPerScope { catalog: &'m mut [u8], txn: &'m mut [u8], stmt: &'m mut [u8] }
}
```

- With `Global` backing, a scope grows geometrically to its maximum
  (§7.5.3) — the normal hosted/ESP32-with-heap configuration.
- With `Caller` backing, the engine needs **no global allocator at all**:
  the caller lends a static buffer at `Connection::open`, scopes are
  carved from it at their configured maxima, a scope that exhausts its
  carve returns `ArenaExhausted` (§7.5.4) instead of growing, and the page
  pool is carved from the same loan. This is Horton's caller-provided
  bump memory [HORTON-SPEC §10] generalised to the whole engine — the
  escape hatch is not a port or a feature flag, it is the same code with
  a different backing enum.
- What the caller must still provide on such a target: the memory itself,
  sized by §4.1-style accounting, and a `Storage` (§5.1). Nothing else
  changes — not the parser, not the B-tree, not the format.

## 8. Testing & verification strategy

The oracle is not a model. The oracle is SQLite.

- **Bidirectional differential harness** (the milestone's centre of mass):
  - *hipp → SQLite:* hipp executes a scripted/random statement sequence;
    the resulting file is opened by the real `sqlite3` CLI;
    `PRAGMA integrity_check` must say `ok`, and `SELECT` output must match
    hipp's own scan, byte for byte.
  - *SQLite → hipp:* the same sequence runs in `sqlite3`; hipp opens the
    resulting file and its scan must match. Fixtures generated this way are
    checked in as golden files (small, page size 512 to keep them small —
    which also exercises non-default page sizes on every run).
  - **Pinned oracle: SQLite 3.54.1** (D1, verified 2026-10-16
    [SQL-REL]). The local CLI on this machine is **3.45.1** (`sqlite3
    --version`) — nine minor releases behind the pin. It must be
    upgraded/replaced to 3.54.1 **before any compatibility claim is
    tested**; until then its output is exploratory only. The harness
    asserts `sqlite3 --version` against the pin and refuses to run claim
    tests on a mismatch, and records the oracle's version in every
    failure report regardless — a differential failure against the wrong
    version is a lie in either direction.
- **Crash injection**, Horton's method, at the `Storage` boundary: a
  wrapper device that drops/torn-writes from the *n*-th write onward, for
  every *n* in a commit sequence; after reopen, the database must be at
  exactly the pre- or post-transaction state, and `integrity_check` (via
  hipp's own reopen + the sqlite3 CLI on exported bytes) must pass.
  Under D2 the injection matrix gains a second axis: the harness also
  **drops the operation future (and, in the hard variant, the
  statement/connection) at every await point** of a commit (between any
  two device operations, at every `Pending`), because a dropped future
  must be indistinguishable from a crash (§5.1.1, §5.3).
- **Pending injection (D2)**, the async twin of crash injection: a wrapper
  `Storage` implementation whose futures return `Pending` *k* times at
  every I/O point (registering wake correctly, and in one hostile variant
  *without* waking, where the test — not the engine — owns the timeout)
  must not change any outcome: every §9 flow completes identically once
  driving continues, no operation ever makes progress it cannot account
  for, and a second driver attempted mid-commit yields
  `OperationInFlight` (A11).
- **Decoder fuzzing:** varint, record, page-header, and cell decoders fed
  arbitrary and mutated-fixture bytes; the contract is *never panic, never
  loop, never allocate unboundedly on a length field* — lengths are checked
  against remaining bytes before any allocation (a hostile header claiming
  a 2 GiB text value in a 4 KiB page is `CorruptRecord`, not an OOM).
- **Arena discipline tests** (§7.5): (i) a counting global allocator
  asserts **zero** backing allocations after warm-up in a repeated
  prepare/step/finalize loop inside configured caps (the §4.1
  steady-state gate); (ii) *exhaustion injection* — every test in the
  differential suite re-run with each arena capped at a few hundred
  bytes, asserting the only new outcome is `ArenaExhausted` (plus a
  rolled-back file for writes), never a panic or a corrupt file;
  (iii) reset-safety — in test builds, reset regions are poison-filled
  and statements are checked not to touch them after a rewind/reset;
  (iv) per-arena high-water marks are recorded for the §4.1 reference
  workload and asserted against the budget table.
- **Size gates** (§4.1): the tiny-profile `thumbv7em-none-eabihf` build's
  section sizes and static RAM are asserted in CI on every change, not
  sampled occasionally — a budget you check monthly is a rumour.
- **Property round-trips:** `Value → record → Value` identity across the
  serial-type space, including integer width boundaries
  (127/128, 32767/32768, 2³¹, 2⁴⁷, `i64::MIN`), `-0.0`, and empty
  text/blob (serial types 13/12 — zero body bytes, distinct from NULL).
- **Probe fixtures already on disk** (`probe/`, generated 2026-10-16 with
  the local **3.45.1** CLI during this design pass — pre-pin, so they are
  format probes, not claim evidence; regenerate them against 3.54.1 when
  the oracle is upgraded, §11): `probe.db` — 2 pages, 4096-byte
  pages, schema format 4, UTF-8, a 3-row table including rowid
  `9007199254740993`, `NULL`s, a REAL, and blobs; `many.db` — 500 rows ×
  80-byte payloads = 14 pages whose root is an *interior* table page
  (`0x05`) over 13 leaves (`0x0D`) — the split-path fixture; `wal.db` — a
  WAL-mode file whose header bytes 18/19 are `2/2`, the rejection fixture.

## 9. First milestone — v0.1, shippable definition

### 9.1 Scope (and only this)

1. `Connection::open` over any `Storage` (async, §5.1.1): create-new (initialise page 1:
   header + empty `sqlite_schema` leaf, page size 4096, schema format 4,
   UTF-8, rollback mode) and open-existing (full header validation §5.2,
   hot-journal recovery §5.3, catalog build §5.5).
2. `CREATE TABLE name (col TYPE, …)` — rowid tables, optional
   `INTEGER PRIMARY KEY` column, optional `IF NOT EXISTS`.
3. `INSERT INTO name [(cols)] VALUES (literal, …)` — one row per statement;
   literals: NULL, integers, reals, `'text'`, `x'blob'`. Rowid
   auto-assignment = max existing rowid + 1 (1 on empty), explicit rowid via
   the alias column or a hidden `rowid` column name.
4. `SELECT * | col, … FROM name` — full table scan in rowid order,
   streaming via `step()`.
5. Autocommit durability per §5.3 for `CREATE TABLE` and `INSERT`.
6. Overflow read+write (§5.4), page splits including root split (§5.4),
   freelist consumption (§5.4).
7. The three arena scopes (§7.5.2) with caller/global backing (§7.5.6),
   `Config` sizing (§7.5.3), and exhaustion semantics (§7.5.4); the fixed
   page pool carved at open (§5.2).

### 9.2 Acceptance tests — all must pass, no partial credit

| # | Test | Oracle |
|---|---|---|
| A1 | hipp creates a file, creates `t(id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB)`, inserts rows incl. rowid `9007199254740993`, NULLs, `1.5`, `x'00ff'`, empty text/blob; reopens; scan returns identical values | hipp self-check |
| A2 | The A1 file, opened by real `sqlite3`: `integrity_check` = `ok`; `SELECT` output matches hipp's scan exactly | `sqlite3` CLI |
| A3 | A file created by real `sqlite3` with the A1 script: hipp opens and scans identical values | hipp vs CLI `.dump` |
| A4 | 500 rows × 80-byte payloads inserted by hipp: root page becomes interior (`0x05`), file passes A2-style CLI validation, all 500 rows scan back in order | both |
| A5 | A 10 KiB text value round-trips (overflow chain) in both directions (hipp-written read by CLI; CLI-written read by hipp) | both |
| A6 | Crash injection at every write boundary of an `INSERT` commit, **and operation future / statement / connection dropped at every await point of the commit** (D2): reopened state is always pre- or post-image, never torn; sqlite3 CLI agrees. Plus the cross-tool case: a hipp process killed mid-commit leaves a journal that **real SQLite** rolls back correctly (§5.3) | injector + CLI |
| A7 | Opening `probe/wal.db` (WAL header) returns `WalUnsupported`; opening a UTF-16 file returns `UnsupportedEncoding`; opening a truncated/garbage file returns a `Corrupt*`/`BadMagic` error — no panics anywhere | hipp |
| A8 | Fuzz smoke: decoders survive 10⁶ mutated inputs from the golden fixtures without panic/hang | hipp |
| A9 | Arena exhaustion: a tiny statement arena makes `SELECT` fail with `ArenaExhausted` (no panic); a tiny transaction arena makes `INSERT` fail with `ArenaExhausted` **and** the file is byte-identical to its pre-statement state under the sqlite3 CLI | hipp + CLI |
| A10 | §4.1 gates: tiny-profile `thumbv7em` build within code/static budgets (operation-state enum sizes, §5.1.1, counted in the static figure); tiny reference config's measured arena high-waters + page pool ≤ 64 KiB total; zero steady-state global allocations after warm-up in the scan loop | CI |
| A11 | Pending injection (D2): every A1–A5 flow re-run against a `Storage` whose futures return `Pending` *k* times at every I/O point — outcomes identical; an operation future dropped mid-commit leaves a transaction that `resume_txn` finishes or the next open recovers; a second driver attempted mid-commit returns `OperationInFlight` | hipp |

### 9.3 Definition of done

- A1–A8 green on stable toolchain, edition 2024, `cargo fmt` clean,
  `clippy -D warnings` (pedantic where the shop standard applies).
- Core crate builds as `#![no_std]` for one bare-metal target in CI
  (the gate exists from day one; a `std` leak discovered in v0.3 is a
  redesign, discovered in v0.1 it's a typo).
- Per-arena high-water marks for the A4 workload within the §4.1 budget
  table, under a counting allocator (§7.3, §7.5.3).
- **A9 and A10 pass** — arena semantics and the size/RAM/speed gates are
  part of "done", not post-launch cleanup (§4.1).
- README states the claim in D5's wording ("reads and writes SQLite
  database files"), the phase, the build profile, the pinned claim
  version (**3.54.1**, D1), and the non-goals (§3) — the honesty is part
  of the ship.

### 9.4 v0.2 — the CLI lands here (D4), alongside the first dialect growth

> **Interpretation flag (2026-10-16):** Mark's D4 answer was the single
> word "CLI", given to the question "CLI in v0.2, or library-only until
> v0.4?" This spec reads it as **CLI in v0.2** — the earlier of the two
> options offered, i.e. pulled forward from the original deferral, but
> not into v0.1. If Mark meant CLI in *v0.1*, this section moves up a
> phase and §9.1 grows; the flag is recorded in the §11 decision log so
> the correction is one word from him.

The v0.2 engine items, in order: multi-row `VALUES`, `WHERE rowid = ?`
with literals (then bound parameters), explicit transactions — each is
small once v0.1's pager and B-tree are trustworthy, and each would have
doubled v0.1's test matrix for no format risk retired.

**The CLI (`hipp-cli`, a `std` binary crate — the core stays `no_std`):**

- **Jobs, in priority order:** (1) differential-testing tool — the §8
  harness scripts `hipp` and the pinned `sqlite3` CLI over the same
  corpus and diffs outputs, which is cheaper and more honest than a
  bespoke comparison layer for every new statement; (2) demo surface —
  the thing a human can `cargo run` to watch hipp read a real file.
- **Minimum surface:** `hipp <file.db>` interactive and
  `hipp <file.db> "SQL…"` one-shot (plus a RAM-backed transient database
  when no file is given). It executes whatever SQL the engine's current
  phase accepts — at v0.2: the v0.1 subset plus the v0.2 items above —
  and prints result rows in a `sqlite3`-list-like format stable enough
  to diff against the oracle's. Dot-commands at minimum: `.tables`,
  `.schema [table]`, `.dump`, `.open`, `.exit`/`.quit`. One-shot errors
  go to stderr with a non-zero exit code; interactive errors print and
  the session continues.
- **Non-goals for the v0.2 CLI:** no `sqlite3` CLI flag/format parity
  beyond the diffable basics, no `.import`/`.export` formats, no shell
  history/editing luxuries. It is a test instrument that happens to be a
  demo, not a product shell.

## 10. Proposed repo layout (when Mark approves code)

```
hipp/
├── Cargo.toml            # single crate "hipp", edition 2024
├── src/
│   ├── lib.rs            # #![no_std], extern crate alloc, public API
│   ├── arena.rs          # Arena, AVec, ArenaBacking — the ONLY module that touches the global allocator (§7.5.1)
│   ├── storage.rs        # async Storage / SidecarFile / AdvisoryLock traits (§5.1) + internal raw poll layer, MemStorage
│   ├── pager.rs          # header, page cache, journal protocol, txn state
│   ├── btree.rs          # page/cell codec, cursor, split, overflow, freelist
│   ├── record.rs         # varint, serial types, Value, comparison
│   ├── schema.rs         # sqlite_schema scan, catalog
│   ├── sql/              # lexer.rs, parser.rs, ast.rs
│   ├── plan.rs           # planner + tree-walking executor
│   └── error.rs
├── tests/
│   ├── differential.rs   # sqlite3 CLI harness (std, test-only)
│   ├── crash.rs          # Storage-boundary injector
│   ├── fuzz_smoke.rs
│   └── fixtures/         # golden .db files (page size 512)
└── examples/
    └── mem_demo.rs       # MemStorage end-to-end, no std required beyond harness
```

Host file storage (`FileStorage` over `std::fs`, futures that complete
immediately over blocking calls) lands with the test harness as a
test-support module first, and graduates to a `hipp-std` crate or a
`std` feature when a real consumer appears. Per Q2's resolution (§11,
D2 refinement), `hipp-std` is **not** a designed public convenience
surface: hosted users drive hipp with their own executor (tokio if they
have it). A `block_on` helper and driven examples may live in
`hipp-std`/examples as optional dev tooling, nothing more, unless Mark
says otherwise.

**v0.2 workspace change (D4):** the repo converts to a Cargo workspace
when the CLI lands (§9.4): the `hipp` core crate exactly as above, plus
`hipp-cli/` — a `std` binary crate depending on `hipp` + `hipp-std`,
holding no engine logic of its own. There is no `hipp-ffi` crate and
never will be (D3).

## 11. Decision log — D1–D5 (Mark, 2026-10-16) and open questions

All five decisions were made by Mark on **2026-10-16** (evening), in
answer to this section's original "decisions only Mark can make" list.
The original recommendations are recorded beside the answers where Mark
overruled them — the record matters more than tidiness.

| # | Question | Mark's answer | Spec's original recommendation | Applied where |
|---|---|---|---|---|
| **D1** | Pinned compatibility version | **"newest"** | Pin the newest release at code-start (agreed) | Pin = **SQLite 3.54.1** (released 2026-10-16), verified 2026-10-16 against sqlite.org's homepage "Latest Release" and news page [SQL-REL]. `SQLITE_VERSION_NUMBER` for the header field (offset 96) = **3054001**. §2 (claims), §8 (oracle discipline), §9.3. |
| **D2** | Sync Storage now, or compose with Horton's async devices? | **"Compose with Horton's async devices"** — **CORRECTED 2026-10-16 21:50**, then **REFINED 21:52** (full chain below the table) | Sync (overruled) | **Final form:** §5.1 **rewritten**: hipp's **own runtime-agnostic async primitives** — an `async fn` byte-addressed `Storage` trait + operation futures on `core` machinery only, no runtime/tokio dependency ever; resumable state machines in the structs are the internal implementation + drop-safety story (§5.1.1), over a crate-private raw poll layer; one-in-flight rule (§5.1.2); §5.3 commit stage machine; §5.7/§5.8 async-first API; §6, §8/A6/A11, §9.2, §10. No Horton dependency, no adapter (rev. 0.2's block-cache/RMW adapter design is removed). Sync reasoning preserved as the superseded note in §5.1; the remaining async cost is priced in RISKS R15 (shrunk again rev. 0.3). Q2 resolved by the refinement. |
| **D3** | Real `sqlite3_*` C ABI ever in scope? | **"no"** | No commitment now, door left open (overruled — the door is closed) | §2 axis (c) = **out of scope, permanently**; §3.13; §5.8 (Rust-only API); §10 (no FFI crate); RISKS R11. hipp is a Rust library, **not** a drop-in SQLite replacement. |
| **D4** | CLI in v0.2, or library-only until v0.4? | **"CLI"** | Accept the deferral (overruled) | Read as **CLI in v0.2** — see the interpretation flag in §9.4 and below. §3.10, §9.4 (minimum CLI surface), §10 (`hipp-cli` crate in the v0.2 workspace). |
| **D5** | Public claim wording | **"reads and writes SQLite database files"** | The narrower claim (agreed) | §1, §2 claim templates, §9.3 README rule: the public claim is the narrow wording, with phase + pinned version attached where a claim is made precisely. Unqualified "SQLite-compatible" is not used as a claim anywhere. RISKS R10 status updated. |

**D2 correction (2026-10-16 21:50 CDT) — supersedes part of the rev.
0.2 application of D2.** Mark's original answer was "Compose with
Horton's async devices", and rev. 0.2 applied it literally: hipp's trait
in Horton's contract, a designed block-cache/read-modify-write adapter
implementing hipp `Storage` over a Horton `BlockDevice`, shared test
doubles, and open question Q1 (who owns the adapter). Mark then
corrected himself, verbatim: *"Oh! I think I got confused. I thought we
were using the horton idea as similar thing, not an actually adapter
lol"*. **Corrected reading (this log's interpretation of that sentence,
flagged as such — Mark can veto it):** D2 was never a choice to compose
with, adapt to, or depend on Horton's actual `BlockDevice` ecosystem.
What Mark meant is that hipp gets its **own** poll-async storage trait,
patterned on Horton's idea — caller-provided device, `poll_*` +
`Context` contract style, caller-driven — a similar thing of hipp's own,
self-contained. The async-vs-sync question still resolves to
**poll-async** (the correction does not revert to sync); what dies is
only the literal composition. **Consequences:** the §5.1 adapter design
is removed; there is no adapter crate in any plan; Horton remains cited
as the pattern's origin/precedent only; §5.1.1's state-machine design is
unchanged in substance (the state-machine core follows from async
itself; the 21:52 refinement below changed its *primacy*, not its
existence); RISKS R15 is repriced (adapter/composition complexity and
Horton-version coupling come out, the async tax itself stays). **Q1 is
CLOSED as moot by this correction** — there is no adapter to own, and
the on-device region layout for a hypothetical future bridge is not a
hipp question (a future bridge, if one is ever wanted, is an
implementer-side concern, out of v0.1–v0.2 scope and undesigned).

**D2 refinement (2026-10-16 21:52 CDT) — D2's final form.** Two minutes
after the correction, Mark went one step further, verbatim: *"I think
even going one further, just enabling the async primitives? Then
someone can use tokio if they have it?"* Recorded as D2's final form:
hipp's primary I/O surface is **runtime-agnostic async primitives** —
an `async fn` `Storage` trait (RPITIT; no `dyn`, no boxing, no
allocation for dispatch) and operation futures — built only on `core`'s
`Future`/`Poll`/`Waker` machinery. **hipp core has no runtime dependency
and no tokio dependency, ever.** The caller brings the executor: tokio
on hosted (their choice), embassy on embedded, smol, or a trivial
`block_on` / no-op-waker loop for always-`Ready` devices. (This matches
Mark's standing rustgres decision, 2026-10-09: runtime-agnostic future
primitives, no tokio adapter crate.) The full D2 chain, for the record:
**sync (spec v0.1) → "compose with Horton's async devices" (Mark's
first answer, rev. 0.2) → Horton-as-pattern, hipp's own trait (21:50
correction) → async primitives as the primary surface (21:52
refinement).** What the refinement changed in the design: the
hand-rolled `poll_*` surface is **no longer the public API** — it
survives only as a crate-private raw poll layer the internal state
machines run on (§5.1); the state machines remain the implementation
and the drop-safety story, with per-operation drop semantics specified
in §5.1.1 (checkpoint-before-await; a dropped mid-commit future leaves
a transaction `resume_txn` finishes or the next open recovers). **Q2 is
resolved by this refinement** (see below).

**D4 interpretation flag.** Mark's answer was one word — "CLI" — to a
two-option question (v0.2 vs library-only until v0.4). This log records
it as the earlier option, **v0.2**. It is an interpretation, flagged here
and in §9.4, because "CLI" could also have meant "yes, and sooner —
v0.1". One word from Mark moves it; nothing in v0.1's design waits on
the answer.

**What D1's pin means operationally.** The pinned version is **3.54.1**;
this machine's oracle CLI is **3.45.1** (2024-01-30) — nine minor
releases behind the pin. Differential results against 3.45.1 are *not*
evidence for or against a 3.54.1 claim: before any compatibility claim
is tested, the oracle must be upgraded/replaced to 3.54.1 and the §8
probe fixtures regenerated against it (they are currently 3.45.1-era
format probes — useful, but pre-pin). The harness asserts the oracle's
version against the pin and refuses to run claim tests on a mismatch
(§8). Claim version bumps after this are deliberate, separate commits
with fixture regeneration as a visible diff (RISKS R12).

### Open questions

- **Q1 — ~~Horton composition seam~~ — CLOSED as moot, 2026-10-16
  21:50 (D2 correction).** Rev. 0.2 asked who owns the adapter between
  hipp's byte-addressed trait and Horton's whole-block `BlockDevice`
  (a `hipp-horton` crate, a Horton module, or Tallow-side glue) and what
  the on-device region layout for database + journal would be. Mark's
  correction (D2 entry above) removes the premise: hipp does not compose
  with Horton, so there is **no adapter to own**, and the region layout
  for a hypothetical future bridge is **not a hipp question** — it
  belongs to whoever ever builds that bridge, outside v0.1–v0.2 scope.
  Nothing waits on Q1.
- **Q2 — ~~Hosted convenience surface~~ — RESOLVED 2026-10-16 21:52
  by the D2 refinement.** Q2 asked whether hosted users also get a
  *blocking* sync convenience wrapper in `hipp-std`, or whether
  poll/async-only is the public story. The refinement answers it: there
  is **no blocking wrapper in the core story** — hosted convenience *is*
  the async-primitives surface plus an executor the user already has
  ("someone can use tokio if they have it"). Any `hipp-std` material is
  reduced to optional examples/dev tooling (a `block_on` helper, driven
  examples; §10), **not a designed public surface**, unless Mark says
  otherwise. Nothing waits on Q2.

## Sources

All format claims above are from the SQLite project's own documentation;
all Turso claims from Turso's own repo docs. No Turso or SQLite code was
consulted or copied — this is a clean-room design from published formats.

- [SQFMT] SQLite Database File Format — <https://www.sqlite.org/fileformat2.html>
- [SQL-WAL] SQLite Write-Ahead Logging — <https://www.sqlite.org/wal.html>
- [SQL-LOCK] File Locking and Concurrency in SQLite Version 3 — <https://www.sqlite.org/lockingv3.html>
- [TURSO-COMPAT] Turso SQLite Compatibility (`COMPAT.md`) — <https://github.com/tursodatabase/turso/blob/main/COMPAT.md>
- [TURSO-README] Turso README (in-process Rust database, SQLite-compatible, VDBE architecture) — <https://github.com/tursodatabase/turso>
- [CMU-HIPP] Carnegie Mellon Database Group, "SQLite, A Database for the Edge of the Network (Richard Hipp)" — speaker bio: "Richard Hipp is the creator and principal maintainer of SQLite" — <https://db.cs.cmu.edu/events/databaseology-2015-richard-hipp-sqlite/>
- [WIKI-HIPP] D. Richard Hipp — primary author of SQLite — <https://en.wikipedia.org/wiki/D._Richard_Hipp>
- [SQL-REL] SQLite Latest Release — sqlite.org homepage ("Latest Release: Version 3.54.1 (2026-10-16)", page updated 2026-10-16) and Recent SQLite News — <https://sqlite.org/news.html>. Pin verified 2026-10-16 (D1).
- [HORTON-SPEC] Local: `~/workspace/horton/SPEC.md` — v0.17, §4.3 (`BlockDevice`), §10 (bump-allocator precedent).
