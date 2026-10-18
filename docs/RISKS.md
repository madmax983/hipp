# hipp — RISKS & UNKNOWNS (v0.1 design pass, 2026-10-16; rev. 0.3, 2026-10-16 evening)

**Revision (2026-10-16 evening):** Mark's D1–D5 decisions (SPEC §11) applied —
D1 pin is SQLite **3.54.1** (R5, R12 updated); D2 (poll-async over sync)
**overrules** the spec's sync choice and adds **R15**, the
async-complexity risk, plus updates to R2/R6 and U2; D3 (no C ABI, ever)
closes R11's first pull; D5 (narrow claim wording) resolves R10's wording
question.

**Correction (2026-10-16 21:50 CDT, Mark — D2 corrected):** rev. 0.2 read
D2 as literal composition with Horton's `BlockDevice` ecosystem via a
designed adapter. Mark corrected that (verbatim in SPEC §11): hipp gets
its **own** poll-async trait patterned on Horton's *idea* — no adapter,
no Horton dependency. R15 is repriced accordingly (the composition/
adapter complexity and Horton-version coupling come out; the async tax
itself stays), R6's D2 wording is fixed, and U9/U10 are closed/reframed —
Q1 is closed as moot.

**Refinement (2026-10-16 21:52 CDT, Mark — D2 final form):** the surface
is **runtime-agnostic async primitives** (async `Storage` trait +
operation futures, `core` only, no runtime/tokio dependency ever; caller
brings the executor). R15 shrinks again — no bespoke poll contract to
teach or document — and gains one small shape: callers mis-driving or
dropping futures, mitigated by the persisted commit stage machine and
specified drop semantics (SPEC §5.1.1). Q2 is resolved by this
refinement (SPEC §11).

Companion to `SPEC.md`. Ranked by (damage if wrong) × (likelihood of being
wrong). Each risk names its mitigation and the earliest point at which reality
will tell us the truth. Primary sources are cited as in `SPEC.md`
(`[SQFMT]` = <https://www.sqlite.org/fileformat2.html>,
`[SQL-WAL]` = <https://www.sqlite.org/wal.html>,
`[SQL-LOCK]` = <https://www.sqlite.org/lockingv3.html>,
`[TURSO-COMPAT]` = <https://github.com/tursodatabase/turso/blob/main/COMPAT.md>).

---

## R1 — The file format's edge cases are the product

**Risk.** hipp's identity claim is byte-level compatibility, and the format's
difficulty is concentrated in corners a happy-path implementation never
touches:

- **Overflow arithmetic.** The on-page byte count for an overflowing payload
  uses the `X / M / K` formulas with different `X` for table vs index pages
  [SQFMT §1.6]. An off-by-one here doesn't fail locally — it produces files
  real SQLite reads *wrong*, possibly silently.
- **Freeblocks & fragmentation.** Real files that have seen deletes carry
  freeblock chains and up to 60 fragmented free bytes per page [SQFMT §1.6].
  A reader that assumes packed cells corrupts its view of such files; a
  writer that mishandles them corrupts the file.
- **Page 1 is two things at once.** The 100-byte header shares a page with a
  table B-tree root [SQFMT §1.3, §1.6]. Every offset on page 1, and only
  page 1, shifts. Classic first-implementation bug farm.
- **In-header size validity.** The page count at header offset 28 is only
  trustworthy when offset 24 equals offset 92 [SQFMT §1.3.7]. Trusting it
  unconditionally misreads files written by older SQLite.
- **Varints are 1–9 bytes**, with the 9th byte contributing all 8 bits
  [SQFMT §1.6]. The 8-byte assumption compiles and passes small tests.
- **Serial types 10/11 are reserved** and must never appear [SQFMT §2.1];
  records may legally contain *fewer* values than the schema (pre-`ALTER`
  rows) [SQFMT §2.1]; schema format affects whether types 8/9 exist.
- **Freelist trunk pages** contain arrays of leaf page numbers plus stale
  bytes that must be treated defensively [SQFMT §1.3.8]; a bad leaf pointer
  followed trustingly is arbitrary page access.
- **The lock-byte page:** the page containing byte offset 2³⁰ is reserved
  for VFS locking and is not a B-tree page [SQFMT §1.4]. Growth logic that
  doesn't know this eventually collides with it (distant at 4096-byte
  pages, nearer at 512).

**Mitigation.** §8's bidirectional differential harness from day one (not
"after it works"); golden fixtures chosen to hit each bullet (the 500-row
interior-root fixture and 10 KiB overflow fixture already exist / are in
SPEC §9.2); decoders written check-first, fuzzed (R8).
**Earliest truth:** A2/A3/A5 acceptance tests. If overflow or freelist
handling is wrong, it shows there, before any user does.

## R2 — Crash atomicity is a protocol, and sync lies

**Risk.** The rollback protocol is only atomic if `sync()` means what the
pager thinks it means, in the order journal-then-data-then-delete
[SPEC §5.3; SQL-LOCK §4–5]. Failure modes: an embedded flash driver whose
"sync" is a no-op; an OS that reorders the journal delete before the data
flush; a torn page write on a device whose write unit is smaller than a
page (512-byte pages on a device with no atomic sector story). Horton's
crash discipline transfers, but Horton's threat model was a single device
it controlled; hipp's `Storage` is implemented by *callers* whose honesty
the engine cannot verify.

**Mitigation.** The `Storage::poll_sync` contract is documented as a durability
claim, not a hint (SPEC §5.1); the rollback journal itself is byte-exact
SQLite format (SPEC §5.3) so real SQLite can recover a crashed hipp file —
and A6 tests exactly that, cross-tool; one `no_std` wrinkle inside it:
the journal checksum needs a fresh random nonce per transaction, and
embedded targets have no obvious entropy source, so the nonce source is a
documented per-target decision, never a silent constant; crash injector enumerates every write/sync
boundary and hipp assumes the worst interleaving the trait permits;
`MemStorage` test double has a "sync does nothing" mode so tests prove the
*pager's ordering* rather than the device's kindness. Document per-target
sync semantics in each `Storage` implementation, as an acceptance
requirement for any implementation hipp ships.
Under D2 the commit sequence is additionally a resumable state machine
(SPEC §5.3): a caller that stops polling mid-commit, or drops the
statement, must land in the same pre-/post-image world as a crash —
A6 tests drop-at-every-poll-point alongside power loss, and R15 owns the
state-machine-specific failure modes.
**Earliest truth:** A6 crash-injection test. **Residual risk remains real**
on exotic flash — this is the risk most likely to survive v0.1 in reduced
form, and the README should say so.

## R3 — WAL is where expectations will inflate

**Risk.** A large share of modern SQLite usage defaults to (or is tutorial-
ised into) WAL mode. The first time hipp refuses a user's real file, it
will probably be a WAL file (header bytes 18/19 = `2`, verified in the
`probe/wal.db` fixture). Saying "rollback only" is easy in a spec and hard
in a GitHub issue thread. WAL also *cannot* be half-implemented: reads need
the wal-index or a full `-wal` scan, commits are frames with checksums, and
checkpointing has its own reader-blocking rules [SQL-WAL §2]. There is also
a subtle trap: opening a WAL-mode database *file* read-only while ignoring
a live `-wal` sidecar returns **stale, valid-looking data** — the worst
possible failure shape.

**Mitigation.** v0.1 detects version `2` and errors by name
(`WalUnsupported`, SPEC A7) — never a silent stale read. Phase ordering in
SPEC §2 puts WAL *read* before WAL *write* deliberately. The decision
trigger is explicit: a named Tier-1 target that needs WAL, not issue-thread
gravity.
**Earliest truth:** first contact with users' real files. Expect it early;
the error message is the mitigation.

## R4 — Locking: v0.1's single-process assumption will be violated

**Risk.** SPEC §5.3 specifies the `UNLOCKED/SHARED/RESERVED/PENDING/
EXCLUSIVE` model [SQL-LOCK §3] but v0.1 does not enforce it across
processes. If two processes (or a hipp process and real SQLite) write one
file concurrently in that window, the outcome is not "an error", it is
corruption of a user's database — the one unrecoverable PR event for a
database project. The hot-journal test ("no RESERVED lock ⇒ hot") also
means a hipp process that crashes mid-write and a *reader* that opens the
file can race the recovery itself if the reader ignores locking [SQL-LOCK
§4.1].

**Mitigation.** v0.1 crate docs state the exclusive-open assumption in the
first paragraph, not a footnote; `Connection::open` takes an OS-level
exclusive lock in the host `std` implementation from the start (cheap where `std`
exists; embedded single-device targets genuinely have no second process);
the `AdvisoryLock` trait exists in the API from day one so enforcement is
an implementation feature, not a pager redesign.
**Decision trigger:** the moment a second consumer wants shared-file
access, locking jumps the queue ahead of dialect work (SPEC §2 phases
reorder by fiat, not by roadmap inertia).

## R5 — SQL semantics are a swamp; the file format is the shallow end

**Risk.** Affinity conversions, cross-type numeric comparison, collation,
`REAL`-that-looks-like-integer storage optimisation [SQFMT §2.3], NULL
propagation, date/time-as-text-or-julian ambiguity — the dialect's long
tail is measured in thousands of special cases, and Turso, with orders of
magnitude more resources, still documents its query language and C API as
*partially* supported against a pinned version [TURSO-COMPAT]. hipp will
get individual semantics wrong; the only question is whether it finds out
from its own differential suite or from users.

**Mitigation.** Phase discipline (SPEC §2): v0.1 ships a deliberately tiny
grammar and says so. Every v0.2+ statement lands *with* differential tests
against the pinned oracle (D1) before it lands at all, including a growing
corpus of adversarial cases (affinity matrix, comparison matrix) generated
combinatorially rather than hand-picked. Claims use D5's wording and
always carry phase + pinned-version qualifiers (pin: 3.54.1, D1).
**Unknown:** whether the SQLite TCL test suite is practically usable as a
hipp harness in this environment (size, Tcl dependency, and redistribution
terms all unverified at design time). Investigate in v0.2 planning; do not
assume it.

## R6 — Turso occupies the hosted lane; hipp's moat is a constraint, not a feature list

**Risk.** "SQLite-compatible database in Rust" is, as of this design pass,
already a shipped product with ~21k commits, multi-language bindings, an
MVCC mode, and a stated ambition to be a general database core
[TURSO-README via SPEC sources]. If hipp competes on dialect coverage,
features, or hosted performance, it loses by construction. Worse, a casual
observer (or a tired maintainer at month six) may judge hipp by Turso's
scoreboard.

**Update (2026-10-16, D2 final form):** hipp's I/O is now async —
hipp's **own** async-primitive surface, patterned on Horton's idea, with
no Horton dependency or composition (SPEC §5.1) — so "no async" is no
longer a differentiator and must not be claimed. What survives,
precisely: no async *runtime* — hipp ships no executor, spawns nothing,
and is driven by the caller's executor or polling (SPEC §5.1.1), where
Turso's model centres on its async runtime. The async *complexity* hipp
has now taken on is priced in R15.
**Mitigation / differentiation, stated as a falsifiable thesis:** hipp wins
exactly where Turso's assumptions end — `#![no_std]`, no OS VFS, no async
runtime (poll-async I/O, caller-polled), caller-owned storage, a single
small crate with zero dependencies, heap use capped by caller config. Every roadmap decision is tested against
"does this matter on a microcontroller / in a kernel / in WASM?" If a
proposed feature only matters on a hosted server, it is Turso's feature,
not hipp's. And since Mark's word (2026-10-16) is "fast and tiny, because
embedded", the thesis is no longer rhetorical: SPEC §4.1 turns it into CI
gates (≤128 KiB code, ≤64 KiB engine RAM in the tiny reference config,
zero steady-state allocations), and SPEC §7.5's arenas are the mechanism
that makes the speed half achievable at that size.
**Earliest truth:** the v0.1 CI gates (SPEC §4.1, A10) either demonstrate
the thesis or expose it as a slogan.

## R7 — SQLite assumes overwrite-in-place; raw flash disagrees

**Risk.** The SQLite pager model rewrites pages in place under journal
protection. Raw NOR flash (a headline embedded target class, and Horton's
home turf via its 4 KiB-sector `FlashBlockDevice` [HORTON-SPEC §1]) cannot
overwrite a programmed word without a sector erase. A naive file-on-flash
adapter will either fail, wear catastrophically (journal + data + freelist
churn on the same sectors), or silently violate the sync contract (R2).

**Mitigation.** Be explicit that v0.1's storage targets are byte-addressable
media with overwrite semantics (RAM, hosted files, managed flash / FTL,
SD/eMMC with a filesystem or block layer beneath). Raw-NOR support, if it
ever comes, is an implementer-side project outside hipp's current scope
(log-structured or copy-on-write page mapping beneath an implementation
of the `Storage` trait — the trait's byte-addressed shape leaves room for
such an implementation without hipp designing it), explicitly **out of
v0.1–v0.2 scope and not designed now**, and **not** implied by "runs on
ESP32-S3", where the realistic v0.1 path is a filesystem or FTL partition.
Do not let Horton's flash story be misread as hipp inheriting it — and
per the corrected D2, hipp designs no bridge to Horton's `BlockDevice`
either.
**Decision trigger:** a concrete raw-flash target with sector geometry and
a wear budget, or it stays out.

## R8 — A `.db` file is hostile input

**Risk.** hipp will be pointed at files produced by other programs, other
decades, and eventually other people. Attack/bug surface: page numbers
pointing outside the file; overflow chains that cycle forever; cell
pointer arrays claiming offsets inside the page header; length fields that
claim gigabytes inside a 4 KiB page (allocation-bomb if a decoder trusts a
length before checking remaining bytes — a *specifically allocator-shaped*
risk that Horton's bounded buffers never faced); parser recursion on
hostile schema SQL; pathological B-trees (depth, fanout) from corrupt
interior pages.

**Mitigation.** Check-before-allocate as a coding rule in every decoder
(SPEC §8); all page numbers bounds-checked against the pager's page count
before any read; overflow traversal carries a visited-count cap tied to
file size; parser recursion capped (SPEC §5.6); fuzz targets for every
decoder from v0.1, with the no-panic/no-hang/no-unbounded-alloc contract.
Miri over the core test subset, following the shop's Horton habit.
**Earliest truth:** A8 fuzz smoke — necessary, not sufficient; the fuzz
budget should grow with every decoder added.

## R9 — The allocator is a dependency that can fail *by aborting*

**Risk.** Rust's global allocation failure handler aborts; it does not
return an error hipp can propagate. On a Tier-1 target with a small heap,
a large `SELECT` result, a big payload, or fragmentation over a long
uptime can therefore *kill the device process*, where Horton's design
would have returned `ArenaFull` with a remedy. Fragmentation is the quiet
version: page-cache frames of one size are friendly, but variable-length
`Value`s and AST nodes interleaved over months of uptime on a small heap
are the classic embedded death. This is the honest price of SPEC §7's
verdict, and pretending otherwise would be worse than paying it.

**Update (2026-10-16): the arena strategy (SPEC §7.5) retires most of
this risk's original shape.** Fragmentation was a *general-allocator*
disease; scoped bump arenas with bulk reset have no interleaved-lifetime
free pattern to fragment, per-scope lifetimes are separated by
construction (catalog vs transaction vs statement), hot paths allocate
nothing at all (fixed page pool, per-row checkpoint rewind), and arena
allocation is *fallible* — exhaustion returns `ArenaExhausted` with a
remedy instead of aborting. What survives: (i) the catalog arena can grow
across a long uptime of schema churn on a `Global`-backed target, and its
backing allocation can still abort there; (ii) a caller-supplied slab
(§7.5.6) sized too small fails statements — by design, but a deployment
can still be mis-sized, which is why §7.5.3 exports high-water marks;
(iii) runtime caps (`max_payload_bytes`, cache pages, SQL length) remain
what makes worst-case RAM a *computable function of Config* (SPEC §7.3)
— that computability is the actual embedded guarantee hipp sells.
**Residual:** acknowledged in the README. On `Global` backing, an
allocator that can abort remains a documented platform property, like
the sync contract (R2).

## R10 — Naming, trademark, and clean-room hygiene

**Risk.** Three distinct exposures, none verified by counsel at design
time: (1) "SQLite" is a trademark of the SQLite project — descriptive use
("reads SQLite database files") and product-naming use are different
things, and hipp's tagline/claims need the deliberate decision SPEC §11 D5
calls for; (2) SQLite's own code and test suite have their own terms —
hipp is designed clean-room from the *published format documentation*,
and that posture must survive contact with deadlines (no pasting SQLite C,
no "just checking" implementation details from its source; the format docs
are the specification); (3) Turso is MIT-licensed — reading its code for
implementation answers would contaminate the same clean-room story and is
out of bounds for the same reason; its *docs* are the interface.

**Mitigation.** Keep the sources list in SPEC's Sources section accurate
as the design grows — it is the clean-room paper trail. When in doubt about
a behaviour, resolve it by differential test against the oracle binary
(observing behaviour) rather than by reading either codebase.
**Status (updated 2026-10-16):** the wording question is **decided (D5)** —
the public claim is the narrow, descriptive one: hipp "reads and writes
SQLite database files" (SPEC §2). That removes the product-naming exposure
from hipp's own claims; what remains open is only the clean-room hygiene
above and the fact that no counsel has reviewed any of it — legal
certainty is still beyond this design pass.

## R11 — Scope gravity: the C API and the dialect will both pull

**Risk.** The two most predictable scope failures: (1) someone (possibly
Mark, possibly an early adopter) wants actual `sqlite3_*` ABI compatibility
"since you're compatible anyway" — that is a second product (error codes,
UTF-16 APIs, callback/vtable surfaces, `sqlite3_malloc` ownership rules)
hiding inside a sentence; (2) dialect requests arrive ordered by *who asks
loudest*, not by phase value, and v0.2 quietly becomes "all of SQL, badly".

**Mitigation.** SPEC §2's phase table is the contract. The ABI pull now
has a stronger gate than a roadmap entry: **D3 (2026-10-16) decided it —
no `sqlite3_*` C ABI, ever** (SPEC §2 axis (c), §3.13). The phrase to use
in issues is no longer "separate phases" for the ABI; it is "closed by
decision, not deferred." Dialect growth remains gated by
differential-corpus-first; axes (a) and (b) are pursued in phases, axis
(c) is not an axis of the product at all.

## R12 — Version drift cuts both ways

**Risk.** The *format* has been stable since 2004 [SQFMT preamble], which
is hipp's great good fortune. The *semantics* hipp is compared against are
a moving target: hipp's pin is **3.54.1** (D1, 2026-10-16); Turso pins
3.50.4 [TURSO-COMPAT]; this machine's oracle is still 3.45.1 — **below
the pin**, so its differential results are not claim evidence until it is
upgraded (SPEC §8, §11); users will test against whatever their distro ships. A differential
failure can mean "hipp is wrong", "the oracle changed behaviour", or
"both are right under different schema formats" (e.g. serial types 8/9
only exist at schema format 4 [SQFMT §2.1]).

**Mitigation.** D1 is decided: pin 3.54.1, record it everywhere, upgrade
it deliberately, in its own commits, with fixture regeneration as a
visible diff; the harness asserts the oracle version against the pin and
refuses claim tests on a mismatch (SPEC §8). Never chase a differential
failure across two oracle versions in the same debugging session.

## R13 — Full compatibility pulls toward size; tiny is the product

**Risk.** This is the structural tension of the whole project, now named
as a risk because Mark's requirement (2026-10-16) is explicit: *fast and
tiny, because embedded.* Every compatibility increment is code: WAL is a
second pager protocol, index B-trees are a second cell model, each SQL
function and expression form is parser + planner + executor + tests. Real
SQLite is roughly a quarter-million lines; Turso's tree is comparably
large. A hipp that chased the full surface on the phase roadmap would,
somewhere around v0.4, quietly stop fitting on the targets it exists for —
without any single commit being "the mistake". The failure mode is drift,
not a decision.

**Mitigation.** The tension is managed by construction, not vigilance:
(1) the §4.1 budgets are CI gates with fixed numbers — a feature that
doesn't fit doesn't land in the tiny profile, full stop; (2) the build
*profiles* of SPEC §2 make inclusion a per-target choice — the tiny
profile permanently includes only the format core + v0.1 grammar, and
everything later (index B-trees, WAL, wider dialect) is a cargo feature
that compiles out; (3) compatibility claims are per-profile (SPEC §2), so
"tiny build can't open this WAL file / parse this statement" is a
documented profile boundary, not a compatibility scandal; (4) any gate
increase requires an ADR naming what was bought with the bytes — gates
may move, but only in writing, never by accumulation.
**Earliest truth:** the first `thumbv7em` size report (U7). If the v0.1
grammar alone blows the 128 KiB gate, the response is code discipline
(table-driven lexer, shared decode paths) or a smaller gate scope —
decided in the open, in an ADR.

## R14 — Feature gates can fork the product into untested combinations

**Risk.** R13's mitigation creates its own hazard: a feature-gated SQL
surface means a *matrix* of builds. Combinations nobody tests will exist
(e.g. index B-trees on, WAL off, some dialect flag on), and "compatible"
behaviour can quietly differ between profiles beyond the documented
inclusions — a parser flag that changes record encoding would be a
file-format divergence wearing a feature flag. Untested matrix cells are
also where embedded users actually live (they pick the smallest build
that fits), so the least-tested build may be the most-deployed one.

**Mitigation.** Three rules: (1) features may gate *surface* (grammar,
protocols, API) but never *format semantics* — record codec, B-tree, and
pager behaviour are identical in every profile, so any hipp build reads
any hipp file its profile can open; (2) the differential suite runs in
CI against exactly two configurations — tiny and full — and those are
the only two profiles hipp claims; bespoke feature combinations are
build-your-own and labelled as such; (3) the tiny profile is a first-class
CI citizen (it carries the §4.1 gates), not a release-time afterthought.
**Earliest truth:** the first time a bug report says "works in the full
build" — the two-configuration rule exists to make that sentence rare.

---

## R15 — Async, hipp's own: the complexity tax, minus composition, minus the bespoke contract (added 2026-10-16, D2; repriced 2026-10-16 21:50 correction + 21:52 refinement)

**Risk.** D2 was Mark's call and this risk does not relitigate it — it
prices it. The original spec chose sync `Storage` precisely because
"async pager I/O is the single largest complexity Turso carries"
(superseded note, SPEC §5.1); that complexity now lives in hipp, in
`no_std` form.

**What the 21:50 correction took OUT of this price (rev. 0.3).** Rev.
0.2's literal-composition reading carried costs that are now gone
entirely: the block-cache/read-modify-write adapter design and its
correctness surface (sub-block writes, sidecar region layout on a
file-less device), adapter ownership/governance (the former Q1),
co-versioning with Horton (a Horton trait change becoming a hipp
problem), and validating hipp against someone else's device ecosystem.
None of that exists anymore: the trait is hipp-native, its implementors
and test doubles are hipp's own, and nothing outside hipp can break it
by changing.

**What the 21:52 refinement took out (also rev. 0.3).** The corrected
design briefly made a hand-rolled `poll_*` API the public surface —
a bespoke contract every user would have to learn, every implementor
would have to get subtly right, and hipp would have to document and
defend forever. That cost is gone too: the public surface is idiomatic
async Rust (`async fn` trait, operation futures), which users and
executors already understand, and the poll layer is demoted to a
crate-private implementation detail (SPEC §5.1). In exchange the
refinement adds one small, named shape, priced with the rest below:
**mis-driven futures** — a caller cancels a task, times out, or simply
drops an operation future mid-flight, possibly mid-commit, and expects
the database to still make sense. What remains — the real price of
async, now mostly *internal* — is:

- **State-machine bugs replace call-stack bugs.** Every operation that
  can suspend is an explicit resumable state machine in a struct (SPEC
  §5.1.1). A missed state, a stage that advances on `Pending`, a resume
  that re-runs a non-idempotent stage — these are the new corruption
  class, and they hide exactly where sync code couldn't go wrong.
- **Livelock by silent device.** A device that returns `Pending` and
  never registers/wakes hangs the caller forever, and hipp — having no
  executor and no clock — cannot even detect it. The contract is the
  device's; a hostile or buggy one wedges the engine politely.
- **Abandonment semantics (the refinement's new shape).** Callers *will*
  drop operation futures mid-flight — task cancellation and timeouts
  make this routine under real executors, not exotic. If any operation
  kept essential state in the future or a statement instead of the
  `Connection`, abandonment would tear a transaction (SPEC §5.1.1/§5.3
  forbid this by construction, and §5.1.1 now specifies per-operation
  drop semantics as public contract — the risk is an implementation
  quietly violating it, or a user never reading it).
- **Test matrix multiplication.** Every flow now exists in three
  interleavings: always-`Ready`, `Pending`-injected, and
  dropped-at-an-await-point. The v0.1 matrix was sized for sync; D2 grows
  it without growing the milestone's calendar.
- **Code-size pressure.** One resumable state machine per operation costs
  more `.text` than straight-line sync code, against a §4.1 gate
  (≤128 KiB) that does not move. Death here is a thousand `Poll` branches.

**Mitigation.** (1) State lives in structs, never futures (SPEC §5.1.1) —
enforce by API shape: operation futures borrow the structs and own
nothing (checkpoint-before-await); the state machines beneath them run
on a crate-private poll layer users never see. (2) Commit/rollback
stages are idempotent by design and the stage machine is persisted in
the `Connection` (SPEC §5.3), so resume-after-drop via `resume_txn` —
or recovery at next open — is a tested property, not a hope; drop
semantics per operation are documented as contract (SPEC §5.1.1), so
mis-driving a future has a specified, safe outcome instead of an
undefined one. (3) Pending injection and drop-at-every-await-point are
acceptance tests (A11, A6), in the harness from the first async code,
not retrofitted. (4) One generic op-driver pattern for all state
machines; no combinators, no trait objects; operation-state sizes are
counted in the A10 gate. (5) The one-in-flight rule (SPEC §5.1.2) —
now largely borrow-checker-enforced in the async API — removes
interleaved-operation states from the design entirely.
(6) Waker discipline is inherited from the standard `Future` contract
hipp's surface is built on (no bespoke poll contract to teach — the
refinement's dividend), and the always-`Ready` path (RAM, blocking
flash, host files) is the Tier-1 default — most real devices never
return `Pending` at all.
**Earliest truth:** A11 plus Pending-injection against hipp's own test
doubles and the first real `Storage` implementations hipp ships
(`MemStorage`, the host file implementation). If the state machines are
wrong, they are wrong there, in tests, first. (Rev. 0.2 said "the first
composition bring-up against a real Horton device" — that milestone no
longer exists; Q1 is closed and U9/U10 below are closed/reframed
accordingly.)

**Honest new total.** After both rev. 0.3 moves, R15 is a
single-crate, mostly *internal* complexity risk: the state machines,
livelock, test-matrix, and code-size shapes remain, plus the small new
drop/mis-drive shape — but the adapter/composition term is deleted,
the bespoke-poll-contract term is deleted, and the surface users touch
is the most standard async Rust there is. It stays on the register
because the internal machinery is real, but it no longer plausibly
ranks top-five: the expensive halves of D2 turned out to be the two
readings Mark corrected away, and what is left is bounded by code hipp
wholly owns.

---

## Unknowns register (things nobody knows yet, honestly)

| # | Unknown | How it gets resolved |
|---|---|---|
| U1 | Minimum heap at which the v0.1 milestone workload comfortably runs on ESP32-S3-class hardware | Heap-watermark tests (§7.3) + first Tier-1 bring-up |
| U2 | ~~Whether sync `Storage` or Horton-style async~~ — **RESOLVED 2026-10-16 by D2, final form 21:52**: hipp's own runtime-agnostic async primitives, patterned on Horton's idea; no composition was ever actually wanted (Mark's correction + refinement, SPEC §11). Its rev. 0.2 successor question was U9, now closed with it. | — |
| U9 | ~~The Horton composition seam: the byte-over-block adapter and its ownership (SPEC §11, Q1)~~ — **CLOSED AS MOOT 2026-10-16 21:50 (D2 correction)**: there is no adapter, so there is nothing to own and no seam to test. The on-device region layout for a hypothetical future bridge to a `BlockDevice` is not a hipp unknown; it belongs to whoever builds such a bridge, outside v0.1–v0.2 scope. | — |
| U10 | Do hipp's own real `Storage` implementations ever return `Pending` without a wake path hipp can rely on (R15's livelock shape), or are the ones hipp ships effectively always-`Ready` under a caller's polling? (Reframed 2026-10-16 21:50: rev. 0.2 asked this about Horton's devices, which are no longer in the picture.) | Pending-injection against hipp's own test doubles plus audit of the first shipped implementations (`MemStorage`, host file) |
| U3 | TCL test suite usability (R5) | v0.2 planning spike |
| U4 | Real-world page-size distribution in files users will actually bring (512 fixtures vs 4096 default vs WAL-era files) | Fixture corpus growth; `open` telemetry in examples |
| U5 | Whether any Tier-1 target actually needs WAL in the next year (R3's trigger) | A named target, not speculation |
| U6 | Overflow/defrag bug density in the first B-tree implementation | A5/A6 + fuzz results; plan one hardening pass before v0.2 regardless |
| U7 | Actual `.text` size of the hand-rolled parser + B-tree + codec on `thumbv7em-none-eabihf` — the 128 KiB gate in SPEC §4.1 is an informed budget, not a measurement | First size report once code exists; R13 governs what happens if it misses |
| U8 | Whether the tiny reference RAM config (64 KiB, SPEC §4.1) holds for real schemas (hundreds of tables) or only toy catalogs — catalog arena growth is the suspect | Differential fixtures with large schemas; per-arena high-water marks (SPEC §7.5.3) |
