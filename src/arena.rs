//! Scoped bump arenas — hipp's allocation architecture (SPEC §7.5).
//!
//! This is the ONLY module in the crate permitted `unsafe` (the crate
//! root denies it; this module is the bounded exception — see SPEC §6).
//! It also hosts `AVec`, the growable collection type other modules use,
//! so that no code outside this module names an allocating type from
//! the global allocator directly (SPEC §7.5.1's load-bearing rule).
//!
//! An `Arena` is a chain of byte chunks behind a bump pointer:
//! - allocation is an alignment fix + an addition,
//! - chunks are never moved once allocated, so handed-out slices stay
//!   valid for the scope's life,
//! - reset is O(1) (watermarks move, chunks are retained),
//! - every allocation is checked against the scope's configured
//!   maximum and fails with `ArenaError` instead of aborting (R9).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::{Cell, UnsafeCell};

use crate::error::{ArenaError, Scope};

/// Backing-store choice (SPEC §7.5.6). `Global` grows chunks from the
/// global allocator up to the arena's maximum. `Caller` carves the
/// scope's maximum out of one caller-lent slab at construction, so the
/// engine needs no global allocator at all.
pub enum ArenaBacking<'m> {
    Global,
    Caller(&'m mut [u8]),
}

struct Chunk {
    ptr: *mut u8,
    len: usize,
}

struct Inner {
    chunks: Vec<Chunk>,
    /// Owned chunk allocations, held as raw pointers from
    /// `Box::into_raw` and freed in `Drop` (borrowed caller slabs are
    /// chunks without an entry here). Raw ownership, not `Box`, is
    /// what keeps the chunk pointers valid: moving a `Box` asserts
    /// unique access and would invalidate every pointer previously
    /// derived from it (a Stacked Borrows violation miri reported
    /// when the box was pushed after its pointer was taken).
    owned: Vec<*mut [u8]>,
    /// Bytes consumed in each chunk (parallel to `chunks`).
    offs: Vec<usize>,
    /// Total retained capacity (sum of chunk capacities).
    capacity: usize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for &raw in self.owned.iter() {
            // SAFETY: every entry came from `Box::into_raw` in
            // `alloc_pos`, is dropped exactly once (here), and no
            // slice handed out by the arena outlives the arena.
            drop(unsafe { Box::from_raw(raw) });
        }
    }
}

impl Inner {
    /// Shared view of `len` bytes at `start` in chunk `i` — only that
    /// range, never the whole chunk: a whole-chunk reference would
    /// overlap slices the arena has already handed out.
    fn range(&self, i: usize, start: usize, len: usize) -> &[u8] {
        let c = &self.chunks[i];
        assert!(
            start <= c.len && len <= c.len - start,
            "arena range in bounds"
        );
        // SAFETY: chunk pointers are valid for the arena's life and
        // never move (owned boxes are never reallocated after push;
        // caller slabs are contractually kept alive by the caller), and
        // the range is in bounds (checked above).
        unsafe { core::slice::from_raw_parts(c.ptr.add(start), len) }
    }
    /// Mutable view of a freshly bumped range of chunk `i`. Exactly the
    /// new range is borrowed, so it cannot alias any live slice the
    /// arena handed out earlier — including a caller copying arena
    /// bytes back into the same arena (`alloc_copy_handle` on an
    /// arena-resident source, which miri flagged when this borrowed
    /// the whole chunk).
    fn range_mut(&mut self, i: usize, start: usize, len: usize) -> &mut [u8] {
        let c = &self.chunks[i];
        assert!(
            start <= c.len && len <= c.len - start,
            "arena range in bounds"
        );
        // SAFETY: as for `range`; bump discipline guarantees the range
        // was never handed out before.
        unsafe { core::slice::from_raw_parts_mut(c.ptr.add(start), len) }
    }
}

/// A scoped bump arena. Interior-mutable: allocations take `&self` and
/// return slices borrowed for the arena borrow's lifetime, which is
/// what lets a parser build a tree of slices without threading `&mut`
/// through every layer. Not `Sync` — one scope, one thread of control,
/// matching the one-in-flight rule (SPEC §5.1.2).
pub struct Arena {
    inner: UnsafeCell<Inner>,
    /// Logical bytes consumed (sum of chunk offsets, padding included).
    used: Cell<usize>,
    high: Cell<usize>,
    max: usize,
    scope: Scope,
}

const ALIGN: usize = 8;

impl Arena {
    /// Global-backed arena. Memory is retained lazily: the first chunk
    /// is allocated on first use, sized `initial` (clamped to `max`).
    pub fn new(scope: Scope, _initial: usize, max: usize) -> Arena {
        Arena {
            inner: UnsafeCell::new(Inner {
                chunks: Vec::new(),
                owned: Vec::new(),
                offs: Vec::new(),
                capacity: 0,
            }),
            used: Cell::new(0),
            high: Cell::new(0),
            max,
            scope,
        }
    }

    /// Caller-backed arena (SPEC §7.5.6): the whole scope lives inside
    /// one lent slab; `max` becomes the slab length and exhaustion is
    /// the only growth policy. The slab is NOT copied and NOT owned:
    /// the caller must keep it alive and unaliased for the arena's
    /// whole life (that loan is the contract of `ArenaBacking::Caller`).
    pub fn with_backing(scope: Scope, backing: ArenaBacking<'_>) -> Arena {
        match backing {
            ArenaBacking::Global => Arena::new(scope, 0, usize::MAX),
            ArenaBacking::Caller(slab) => {
                let len = slab.len();
                let ptr = slab.as_mut_ptr();
                let mut a = Arena::new(scope, 0, len);
                let inner = a.inner.get_mut();
                inner.capacity = len;
                inner.chunks.push(Chunk { ptr, len });
                inner.offs.push(0);
                a
            }
        }
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// Bytes consumed right now (logical bump position).
    pub fn used(&self) -> usize {
        self.used.get()
    }

    /// High-water mark of `used` since construction (SPEC §7.5.3).
    pub fn high_water(&self) -> usize {
        self.high.get()
    }

    fn bump_used(&self, delta: usize) {
        let u = self.used.get() + delta;
        self.used.set(u);
        if u > self.high.get() {
            self.high.set(u);
        }
    }

    /// Allocate `len` zeroed bytes. The returned slice lives in one
    /// chunk (contiguity is guaranteed — this is what lets the
    /// statement arena address a stored row by logical position via
    /// `slice_at`).
    // `&self` -> `&mut [u8]` is the arena's founding pattern
    // (UnsafeCell interior mutability; bump discipline guarantees
    // non-overlap — see the module safety notes).
    #[allow(clippy::mut_from_ref)]
    pub fn alloc(&self, len: usize) -> Result<&mut [u8], ArenaError> {
        self.alloc_pos(len).map(|(_, s)| s)
    }

    /// Like `alloc`, but also returns the slice's logical position
    /// (the `pos` half of an `ABytes` handle resolvable by
    /// `slice_at`). Padding is arena-internal, so positions can
    /// only be minted here.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_pos(&self, len: usize) -> Result<(usize, &mut [u8]), ArenaError> {
        let inner = unsafe { &mut *self.inner.get() };
        // Try the current (last) chunk.
        if !inner.chunks.is_empty() {
            let last_i = inner.chunks.len() - 1;
            let cur = inner.offs[last_i];
            let aligned = (cur + (ALIGN - 1)) & !(ALIGN - 1);
            if aligned + len <= inner.chunks[last_i].len {
                let pad = aligned - cur;
                inner.offs[last_i] = aligned + len;
                self.bump_used(pad + len);
                let base: usize = inner.offs[..last_i].iter().sum();
                let s = inner.range_mut(last_i, aligned, len);
                return Ok((base + aligned, s));
            }
        }
        // Need a new chunk. Caller-backed arenas never grow.
        if inner.capacity >= self.max && !inner.chunks.is_empty() {
            return Err(ArenaError {
                scope: self.scope,
                needed: len,
            });
        }
        // Geometric growth from the last chunk size.
        let last_cap = inner.chunks.last().map_or(0, |c| c.len);
        let mut new_cap = if last_cap == 0 {
            4096usize
        } else {
            last_cap.saturating_mul(2)
        };
        if new_cap < len + ALIGN {
            new_cap = len + ALIGN;
        }
        if inner.capacity + new_cap > self.max {
            // A dedicated chunk of exactly the need might still fit
            // where the geometric one did not.
            let exact = len + ALIGN;
            if inner.capacity + exact > self.max {
                return Err(ArenaError {
                    scope: self.scope,
                    needed: len,
                });
            }
            new_cap = exact;
        }
        let raw: *mut [u8] = Box::into_raw(alloc::vec![0u8; new_cap].into_boxed_slice());
        let ptr = raw.cast::<u8>();
        inner.owned.push(raw);
        inner.capacity += new_cap;
        inner.chunks.push(Chunk { ptr, len: new_cap });
        let base: usize = inner.offs.iter().sum();
        inner.offs.push(len);
        self.bump_used(len);
        let last_i = inner.chunks.len() - 1;
        Ok((base, inner.range_mut(last_i, 0, len)))
    }

    /// Allocate and copy `data` in; returns the stored copy.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_copy(&self, data: &[u8]) -> Result<&[u8], ArenaError> {
        let out = self.alloc(data.len())?;
        out.copy_from_slice(data);
        // SAFETY: reborrow the just-written slice as shared. The arena
        // never hands overlapping mutable slices (bump discipline),
        // and the copy is complete before this reference exists.
        let ptr = out.as_ptr();
        Ok(unsafe { core::slice::from_raw_parts(ptr, data.len()) })
    }

    /// Allocate, copy `data` in, and return its `ABytes` handle.
    pub fn alloc_copy_handle(&self, data: &[u8]) -> Result<ABytes, ArenaError> {
        let (pos, out) = self.alloc_pos(data.len())?;
        out.copy_from_slice(data);
        Ok(ABytes {
            pos: pos as u32,
            len: data.len() as u32,
        })
    }

    /// Consume `bytes` of budget without retaining a slice (used to
    /// charge a scope for fixed per-transaction structures, SPEC
    /// §7.5.4). The bump stays until reset/rewind, exactly like a real
    /// allocation.
    pub fn charge(&self, bytes: usize) -> Result<(), ArenaError> {
        let _ = self.alloc(bytes)?;
        Ok(())
    }

    /// Logical position (checkpoint) — SPEC §7.5.2's per-row rewind is
    /// `checkpoint` + `rewind`.
    pub fn checkpoint(&self) -> usize {
        self.used.get()
    }

    /// Rewind consumption to `mark` (a value from `checkpoint`).
    /// Chunks are retained; handed-out slices beyond the mark must be
    /// considered dead by the caller (the scopes' reset rules, SPEC
    /// §7.5.5, are what make this safe).
    pub fn rewind(&self, mark: usize) {
        debug_assert!(mark <= self.used.get());
        let inner = unsafe { &mut *self.inner.get() };
        let mut remaining = mark;
        for off in inner.offs.iter_mut() {
            if remaining >= *off {
                remaining -= *off;
            } else {
                *off = remaining;
                remaining = 0;
            }
        }
        self.used.set(mark);
    }

    /// O(1) reset: all consumption forgotten, chunks retained.
    pub fn reset(&self) {
        let inner = unsafe { &mut *self.inner.get() };
        for off in inner.offs.iter_mut() {
            *off = 0;
        }
        self.used.set(0);
    }

    /// Read back the `len` bytes stored at logical position `pos` by a
    /// single `alloc` call (the statement arena's row addressing).
    /// Returns `None` if the range is not inside one chunk — which
    /// cannot happen for a range produced by one `alloc`, and the
    /// caller treats as corruption of its own bookkeeping.
    pub fn slice_at(&self, pos: usize, len: usize) -> Option<&[u8]> {
        let inner = unsafe { &*self.inner.get() };
        let mut base = 0usize;
        for i in 0..inner.chunks.len() {
            let used_here = inner.offs[i];
            if pos >= base && pos + len <= base + used_here {
                let start = pos - base;
                return Some(inner.range(i, start, len));
            }
            base += used_here;
        }
        None
    }
}

/// An offset+length handle into an `Arena` (for text/blobs stored in a
/// scope). Resolves via the owning arena; deliberately not a reference,
/// so structures holding handles are not self-referential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ABytes {
    pub pos: u32,
    pub len: u32,
}

impl ABytes {
    pub const EMPTY: ABytes = ABytes { pos: 0, len: 0 };
}

/// A growable array whose storage lives in this module (SPEC §7.5.1).
/// Used for small metadata (cell tables, descent stacks, catalog
/// entries); bulk byte payloads go through `Arena` instead.
pub struct AVec<T> {
    vec: Vec<T>,
}

impl<T> AVec<T> {
    pub fn new() -> AVec<T> {
        AVec { vec: Vec::new() }
    }
    pub fn with_capacity(cap: usize) -> AVec<T> {
        AVec {
            vec: Vec::with_capacity(cap),
        }
    }
    pub fn extend_from_slice(&mut self, b: &[T])
    where
        T: Clone,
    {
        self.vec.extend_from_slice(b);
    }
    pub fn push(&mut self, v: T) {
        self.vec.push(v);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.vec.pop()
    }
    /// Reserve room for `n` more elements, reporting an allocation
    /// the global allocator refuses (or a size overflow) as
    /// statement-arena exhaustion instead of aborting (v0.16: the
    /// function library's large results — `zeroblob(N)`,
    /// `printf('%*d', N, ...)` — size their buffers through this).
    pub fn try_reserve(&mut self, n: usize) -> Result<(), ArenaError> {
        self.vec.try_reserve(n).map_err(|_| ArenaError {
            scope: Scope::Statement,
            needed: n,
        })
    }
    pub fn len(&self) -> usize {
        self.vec.len()
    }
    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }
    pub fn clear(&mut self) {
        self.vec.clear();
    }
    pub fn as_slice(&self) -> &[T] {
        &self.vec
    }
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.vec
    }
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.vec.iter()
    }
    /// Insert `v` at `idx`, shifting later elements up; an index
    /// past the end appends (never panics).
    pub fn insert_at(&mut self, idx: usize, v: T) {
        let i = idx.min(self.vec.len());
        self.vec.insert(i, v);
    }
    /// Keep the first `n` elements (all of them when there are
    /// fewer).
    pub fn truncate(&mut self, n: usize) {
        self.vec.truncate(n);
    }
    /// Remove the first `n` elements (all of them when there are
    /// fewer), shifting the rest down.
    pub fn remove_front(&mut self, n: usize) {
        let n = n.min(self.vec.len());
        self.vec.drain(..n);
    }
    /// Replace the elements in `at..at + del` (clamped to the
    /// length) by `ins`, in place: only the elements after the
    /// range move (v0.16, `json`: a JSONB header resized while
    /// parsing shifts just its container's payload).
    pub fn replace_range(&mut self, at: usize, del: usize, ins: &[T])
    where
        T: Clone,
    {
        let len = self.vec.len();
        let at = at.min(len);
        let end = at.saturating_add(del).min(len);
        self.vec.splice(at..end, ins.iter().cloned());
    }
    /// Insert `v` at `at`, shifting later elements up (panics if
    /// `at > len`, like `Vec::insert`).
    pub fn insert(&mut self, at: usize, v: T) {
        self.vec.insert(at, v);
    }
    /// Remove and return the element at `idx`, shifting later ones
    /// down; `None` (nothing removed) past the end (v0.16, `attach`:
    /// DETACH closes up the database list).
    pub fn remove_at(&mut self, idx: usize) -> Option<T> {
        if idx < self.vec.len() {
            Some(self.vec.remove(idx))
        } else {
            None
        }
    }
}

impl<T> Default for AVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Clone for AVec<T> {
    fn clone(&self) -> Self {
        AVec {
            vec: self.vec.clone(),
        }
    }
}

impl<T: PartialEq> PartialEq for AVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.vec == other.vec
    }
}

impl<T: Eq> Eq for AVec<T> {}

impl<T: core::fmt::Debug> core::fmt::Debug for AVec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.vec.iter()).finish()
    }
}

impl<T> FromIterator<T> for AVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(it: I) -> Self {
        AVec {
            vec: it.into_iter().collect(),
        }
    }
}

impl<T> IntoIterator for AVec<T> {
    type Item = T;
    type IntoIter = alloc::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.vec.into_iter()
    }
}

impl<T> core::ops::Index<usize> for AVec<T> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        &self.vec[i]
    }
}
impl<T> core::ops::IndexMut<usize> for AVec<T> {
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.vec[i]
    }
}

// ---------- the shared stable sort (v0.17 size pass) ----------

/// Runs this short are insertion-sorted before the merge passes.
#[cfg_attr(not(feature = "query"), allow(dead_code))] // the tiny profile sorts nothing
const SORT_RUN: usize = 12;

/// Stably sort the index list `v` under `cmp`: afterwards no index
/// is followed by one that `cmp` orders `Less` than it, and indexes
/// `cmp` calls `Equal` keep their relative order. `cmp` sees the
/// indexes, not the elements they name, so this one non-generic
/// routine serves every sort in the crate (v0.17 size pass: one
/// copy instead of a `slice::sort` instantiation per element type
/// and comparator).
///
/// A bottom-up merge sort: runs of `SORT_RUN` are insertion-sorted
/// in place, then merged pairwise between `v` and one scratch buffer
/// of the same length. An element moves ahead of an earlier one only
/// when `cmp(later, earlier)` is `Less` — `slice::sort_by`'s rule —
/// so for a total order the output is exactly `sort_by`'s. Never
/// panics, whatever `cmp` returns.
#[cfg_attr(not(feature = "query"), allow(dead_code))] // the tiny profile sorts nothing
pub(crate) fn sort_idx(v: &mut [usize], cmp: &mut dyn FnMut(usize, usize) -> core::cmp::Ordering) {
    use core::cmp::Ordering::Less;
    let n = v.len();
    if n < 2 {
        return;
    }
    // Already in order, or strictly against it (reversing is then
    // stable: no two are equal), costs one pass — an ORDER BY over
    // rows that arrive sorted, either way, does not pay the merges.
    let (mut asc, mut desc) = (true, true);
    for w in v.windows(2) {
        if let [p, x] = *w {
            if cmp(x, p) == Less {
                asc = false;
            } else {
                desc = false;
            }
        }
        if !asc && !desc {
            break;
        }
    }
    if asc {
        return;
    }
    if desc {
        v.reverse();
        return;
    }
    let mut lo = 0usize;
    while lo < n {
        let hi = lo.saturating_add(SORT_RUN).min(n);
        for i in lo.saturating_add(1)..hi {
            let mut j = i;
            while j > lo {
                let (Some(&x), Some(&p)) = (v.get(j), v.get(j - 1)) else {
                    break;
                };
                if cmp(x, p) != Less {
                    break;
                }
                v.swap(j - 1, j);
                j -= 1;
            }
        }
        lo = hi;
    }
    if n <= SORT_RUN {
        return;
    }
    let mut buf: AVec<usize> = AVec::with_capacity(n);
    buf.extend_from_slice(v);
    let mut in_buf = false;
    {
        let (mut src, mut dst): (&mut [usize], &mut [usize]) = (v, buf.as_mut_slice());
        let mut width = SORT_RUN;
        while width < n {
            let mut lo = 0usize;
            while lo < n {
                let mid = lo.saturating_add(width).min(n);
                let hi = mid.saturating_add(width).min(n);
                let (mut i, mut j) = (lo, mid);
                for k in lo..hi {
                    let take_right = j < hi
                        && (i >= mid
                            || match (src.get(i), src.get(j)) {
                                (Some(&x), Some(&y)) => cmp(y, x) == Less,
                                _ => false,
                            });
                    let pick = if take_right {
                        j += 1;
                        src.get(j - 1)
                    } else {
                        i += 1;
                        src.get(i - 1)
                    };
                    if let (Some(d), Some(&x)) = (dst.get_mut(k), pick) {
                        *d = x;
                    }
                }
                lo = hi;
            }
            core::mem::swap(&mut src, &mut dst);
            in_buf = !in_buf;
            width = width.saturating_mul(2);
        }
    }
    if in_buf {
        for (d, s) in v.iter_mut().zip(buf.iter()) {
            *d = *s;
        }
    }
}

/// The stable sort permutation of `0..n` under `cmp` (see
/// `sort_idx`): position `k` of the result holds the index of the
/// element that sorts `k`-th.
#[cfg_attr(not(feature = "query"), allow(dead_code))] // the tiny profile sorts nothing
pub(crate) fn sort_perm(
    n: usize,
    cmp: &mut dyn FnMut(usize, usize) -> core::cmp::Ordering,
) -> AVec<usize> {
    let mut p: AVec<usize> = (0..n).collect();
    sort_idx(p.as_mut_slice(), cmp);
    p
}

/// Rearrange a sequence by `perm` (from `sort_perm`): afterwards
/// position `k` holds what position `perm[k]` held. `swap(a, b)`
/// exchanges two positions of the sequence; both are always below
/// `perm.len()`. One pass over the permutation's cycles, so each
/// element moves at most once per cycle step; entries out of range
/// (never produced by `sort_perm`) are skipped, not followed.
#[cfg_attr(not(feature = "query"), allow(dead_code))] // the tiny profile sorts nothing
pub(crate) fn apply_perm(mut perm: AVec<usize>, swap: &mut dyn FnMut(usize, usize)) {
    let p = perm.as_mut_slice();
    let n = p.len();
    for i in 0..n {
        let mut cur = i;
        while let Some(slot) = p.get_mut(cur) {
            let next = *slot;
            *slot = cur;
            if next == i || next == cur || next >= n {
                break;
            }
            swap(cur, next);
            cur = next;
        }
    }
}

/// Stably sort `v` by `cmp`, exactly as `slice::sort_by` would for a
/// total order, through the shared `sort_perm` (v0.17 size pass:
/// only this thin adapter is instantiated per element type).
#[cfg_attr(not(feature = "query"), allow(dead_code))] // the tiny profile sorts nothing
pub(crate) fn sort_by<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> core::cmp::Ordering) {
    let perm = {
        let s: &[T] = v;
        sort_perm(s.len(), &mut |a, b| match (s.get(a), s.get(b)) {
            (Some(x), Some(y)) => cmp(x, y),
            _ => core::cmp::Ordering::Equal,
        })
    };
    let n = v.len();
    apply_perm(perm, &mut |a, b| {
        if a < n && b < n {
            v.swap(a, b);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_and_reset() {
        let a = Arena::new(Scope::Statement, 0, 1024);
        let s = a.alloc_copy(b"hello").expect("alloc");
        assert_eq!(s, b"hello");
        let before = a.used();
        let _ = a.alloc(100).expect("alloc2");
        a.rewind(before);
        assert_eq!(a.used(), before);
        a.reset();
        assert_eq!(a.used(), 0);
        let s2 = a.alloc_copy(b"world").expect("after reset");
        assert_eq!(s2, b"world");
    }

    #[test]
    fn exhaustion_is_an_error_not_a_panic() {
        let a = Arena::new(Scope::Transaction, 0, 64);
        let r = a.alloc(128);
        assert!(matches!(
            r,
            Err(ArenaError {
                scope: Scope::Transaction,
                ..
            })
        ));
    }

    #[test]
    fn copy_arena_resident_bytes_back_in() {
        // Regression (miri, Stacked Borrows): copying bytes that live
        // in this arena back into it must not create a mutable borrow
        // overlapping the source slice.
        let a = Arena::new(Scope::Statement, 0, 1 << 20);
        let pos = a.checkpoint();
        let _ = a.alloc_copy(b"hello arena").expect("alloc");
        let src = a.slice_at(pos, 11).expect("slice");
        let h = a.alloc_copy_handle(src).expect("copy back in");
        assert_eq!(
            a.slice_at(h.pos as usize, h.len as usize),
            Some(&b"hello arena"[..])
        );
        // Force a new chunk while the source is still borrowed.
        let big = a.alloc_copy_handle(&[7u8; 8192]).expect("grow");
        assert_eq!(big.len, 8192);
        assert_eq!(src, b"hello arena");
    }

    #[test]
    fn slice_at_roundtrip() {
        let a = Arena::new(Scope::Statement, 0, 1 << 20);
        let pos = a.checkpoint();
        let _ = a.alloc_copy(b"abcdefghij").expect("alloc");
        let got = a.slice_at(pos, 10).expect("slice");
        assert_eq!(got, b"abcdefghij");
    }

    // ---------- the shared stable sort ----------

    /// A hand-rolled xorshift PRNG (deterministic, no dependencies).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// The reference: a stable insertion sort (obviously correct).
    fn reference<T: Clone>(v: &[T], less: &dyn Fn(&T, &T) -> bool) -> Vec<T> {
        let mut out: Vec<T> = Vec::new();
        for x in v {
            let mut at = out.len();
            while at > 0 && less(x, &out[at - 1]) {
                at -= 1;
            }
            out.insert(at, x.clone());
        }
        out
    }

    /// Sort `(key, original position)` pairs by key alone through the
    /// shared sort; the positions witness stability.
    fn by_key(keys: &[i64]) -> Vec<(i64, usize)> {
        let mut v: Vec<(i64, usize)> = keys.iter().copied().zip(0..).collect();
        sort_by(&mut v, &mut |a, b| a.0.cmp(&b.0));
        v
    }

    fn check(keys: &[i64]) {
        let got = by_key(keys);
        let pairs: Vec<(i64, usize)> = keys.iter().copied().zip(0..).collect();
        let want = reference(&pairs, &|a, b| a.0 < b.0);
        assert_eq!(got, want, "keys {keys:?}");
    }

    #[test]
    fn sort_empty_and_one() {
        check(&[]);
        check(&[7]);
        assert!(sort_perm(0, &mut |_, _| core::cmp::Ordering::Equal).is_empty());
        assert_eq!(
            sort_perm(1, &mut |_, _| core::cmp::Ordering::Less).as_slice(),
            &[0]
        );
    }

    #[test]
    fn sort_is_stable_on_all_equal() {
        for n in [2usize, 11, 12, 13, 24, 25, 100] {
            let keys: Vec<i64> = (0..n).map(|_| 5).collect();
            let got = by_key(&keys);
            let pos: Vec<usize> = got.iter().map(|p| p.1).collect();
            assert_eq!(pos, (0..n).collect::<Vec<_>>(), "n {n}");
        }
    }

    #[test]
    fn sort_reverse_and_sorted_runs_around_the_cutoff() {
        for n in [
            SORT_RUN - 1,
            SORT_RUN,
            SORT_RUN + 1,
            2 * SORT_RUN - 1,
            2 * SORT_RUN,
            2 * SORT_RUN + 1,
            4 * SORT_RUN + 3,
            257,
        ] {
            let rev: Vec<i64> = (0..n as i64).rev().collect();
            check(&rev);
            let fwd: Vec<i64> = (0..n as i64).collect();
            check(&fwd);
            // Descending with ties: not reversed wholesale (that
            // would break stability), merged.
            let rev_ties: Vec<i64> = (0..n as i64).rev().map(|i| i / 2).collect();
            check(&rev_ties);
            // Sorted but for the last element.
            let mut late: Vec<i64> = (0..n as i64).collect();
            late.push(-1);
            check(&late);
            // Two interleaved runs of equal keys.
            let saw: Vec<i64> = (0..n as i64).map(|i| i % 3).collect();
            check(&saw);
        }
    }

    #[test]
    fn sort_matches_a_stable_reference_on_random_input() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..400 {
            let n = r.below(if round % 10 == 0 { 600 } else { 70 }) as usize;
            // Few distinct keys, so ties (and stability) matter.
            let span = 1 + r.below(if round % 3 == 0 { 4 } else { 1000 }) as i64;
            let keys: Vec<i64> = (0..n)
                .map(|_| r.below(span as u64) as i64 - span / 2)
                .collect();
            check(&keys);
        }
    }

    #[test]
    fn sort_by_descending_comparator_keeps_ties_in_order() {
        let mut r = Rng(42);
        let keys: Vec<i64> = (0..300).map(|_| r.below(9) as i64).collect();
        let mut v: Vec<(i64, usize)> = keys.iter().copied().zip(0..).collect();
        sort_by(&mut v, &mut |a, b| b.0.cmp(&a.0));
        let pairs: Vec<(i64, usize)> = keys.iter().copied().zip(0..).collect();
        assert_eq!(v, reference(&pairs, &|a, b| a.0 > b.0));
    }

    #[test]
    fn sort_moves_non_copy_elements() {
        // Owned elements move through `apply_perm`'s swaps intact.
        let mut r = Rng(7);
        let mut v: Vec<AVec<u8>> = (0..90)
            .map(|_| {
                let mut a = AVec::new();
                for _ in 0..r.below(4) {
                    a.push(b'a' + r.below(3) as u8);
                }
                a
            })
            .collect();
        let want = reference(&v, &|a, b| a.as_slice() < b.as_slice());
        sort_by(&mut v, &mut |a, b| a.as_slice().cmp(b.as_slice()));
        assert_eq!(v, want);
    }

    #[test]
    fn sort_idx_sorts_arbitrary_indexes() {
        // `sort_idx` orders the given index values, not positions.
        let keys = [30i64, 10, 20, 10, 0];
        let mut v = [4usize, 1, 3, 0, 2];
        sort_idx(&mut v, &mut |a, b| keys[a].cmp(&keys[b]));
        assert_eq!(v, [4, 1, 3, 2, 0]);
    }

    #[test]
    fn apply_perm_tolerates_a_bad_permutation() {
        // Never produced by `sort_perm`, but a malformed one must
        // neither panic nor loop: out-of-range entries are skipped.
        let mut v = *b"abc";
        let mut p: AVec<usize> = AVec::new();
        for x in [2usize, 9, 0] {
            p.push(x);
        }
        apply_perm(p, &mut |a, b| v.swap(a, b));
        let mut p: AVec<usize> = AVec::new();
        for x in [1usize, 1, 1] {
            p.push(x);
        }
        apply_perm(p, &mut |a, b| v.swap(a, b));
        // Swaps only: still the same three bytes.
        assert!(b"abc".iter().all(|c| v.contains(c)));
        let mut w = *b"xyz";
        let mut p: AVec<usize> = AVec::new();
        for x in [1usize, 2, 0] {
            p.push(x);
        }
        apply_perm(p, &mut |a, b| w.swap(a, b));
        assert_eq!(&w, b"yzx");
    }

    #[test]
    fn sort_survives_an_inconsistent_comparator() {
        // A comparator that is not an order must not panic, loop or
        // lose elements: the result is still a permutation.
        for n in [5usize, 13, 40, 300] {
            let mut flip = Rng(n as u64 + 1);
            let p = sort_perm(n, &mut |_, _| match flip.below(3) {
                0 => core::cmp::Ordering::Less,
                1 => core::cmp::Ordering::Equal,
                _ => core::cmp::Ordering::Greater,
            });
            let mut seen: Vec<bool> = (0..n).map(|_| false).collect();
            for &i in p.iter() {
                assert!(!seen[i], "index {i} twice");
                seen[i] = true;
            }
            assert_eq!(p.len(), n);
        }
    }
}
