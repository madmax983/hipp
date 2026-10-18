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
    /// Keeps owned chunk allocations alive (parallel bookkeeping only;
    /// borrowed caller slabs are chunks without an entry here).
    _owned: Vec<Box<[u8]>>,
    /// Bytes consumed in each chunk (parallel to `chunks`).
    offs: Vec<usize>,
    /// Total retained capacity (sum of chunk capacities).
    capacity: usize,
}

impl Inner {
    fn chunk(&self, i: usize) -> &[u8] {
        // SAFETY: chunk pointers are valid for the arena's life and
        // never move (owned boxes are never reallocated after push;
        // caller slabs are contractually kept alive by the caller).
        unsafe { core::slice::from_raw_parts(self.chunks[i].ptr, self.chunks[i].len) }
    }
    fn chunk_mut(&mut self, i: usize) -> &mut [u8] {
        // SAFETY: as above; bump discipline means two live mutable
        // slices into one chunk never overlap in time-and-range.
        unsafe { core::slice::from_raw_parts_mut(self.chunks[i].ptr, self.chunks[i].len) }
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
                _owned: Vec::new(),
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
    pub fn alloc(&self, len: usize) -> Result<&mut [u8], ArenaError> {
        self.alloc_pos(len).map(|(_, s)| s)
    }

    /// Like `alloc`, but also returns the slice's logical position
    /// (the `pos` half of an `ABytes` handle resolvable by
    /// `slice_at`). Padding is arena-internal, so positions can
    /// only be minted here.
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
                let s = &mut inner.chunk_mut(last_i)[aligned..aligned + len];
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
        let mut owned = alloc::vec![0u8; new_cap].into_boxed_slice();
        let ptr = owned.as_mut_ptr();
        inner._owned.push(owned);
        inner.capacity += new_cap;
        inner.chunks.push(Chunk {
            ptr,
            len: new_cap,
        });
        let base: usize = inner.offs.iter().sum();
        inner.offs.push(len);
        self.bump_used(len);
        let last_i = inner.chunks.len() - 1;
        Ok((base, &mut inner.chunk_mut(last_i)[0..len]))
    }

    /// Allocate and copy `data` in; returns the stored copy.
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
                return Some(&inner.chunk(i)[start..start + len]);
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
    pub fn push(&mut self, v: T) {
        self.vec.push(v);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.vec.pop()
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
    fn slice_at_roundtrip() {
        let a = Arena::new(Scope::Statement, 0, 1 << 20);
        let pos = a.checkpoint();
        let _ = a.alloc_copy(b"abcdefghij").expect("alloc");
        let got = a.slice_at(pos, 10).expect("slice");
        assert_eq!(got, b"abcdefghij");
    }
}
