//! Auto-vacuum files (v0.16, leaf `autovacuum`): the pin's
//! pointer-map pages and the page moves built on them (its btree.c
//! `ptrmap*`, `allocateBtreePage`, `relocatePage`,
//! `incrVacuumStep`, `autoVacuumCommit`, `btreeCreateTable` and
//! `btreeDropTable`).
//!
//! An auto-vacuum database (header offset 52 non-zero) keeps, on
//! pointer-map pages at fixed positions, one 5-byte entry per page:
//! its role (`ROOTPAGE`, `FREEPAGE`, `OVERFLOW1`, `OVERFLOW2`,
//! `BTREE`) and the page that points at it. The pin updates the
//! entries as it writes; hipp's B-tree code rebuilds whole pages,
//! so the map is instead *derived* — every tree from
//! `sqlite_schema`'s root pages, every overflow chain and the
//! freelist are walked into a [`Map`] — and written to the
//! pointer-map pages when a transaction commits ([`pre_commit`]).
//! The map is a function of the file's structure, so the derived
//! entries are the pin's entries. The page moves need the map
//! mid-transaction (a new root's slot, `PRAGMA incremental_vacuum`):
//! they derive it first and keep it current as they move pages.
//!
//! Offset 52 holds the largest root page (1 while there is no
//! table), offset 64 is 1 for `INCREMENTAL` (no page moves at
//! commit; `PRAGMA incremental_vacuum` moves them) and 0 for `FULL`
//! (every commit moves pages from the end of the file into its free
//! pages and truncates, leaving no free page).

use crate::arena::AVec;
use crate::btree::{self, KIND_INTERIOR_INDEX, KIND_INTERIOR_TABLE, KIND_LEAF_TABLE};
use crate::error::{Error, Result};
use crate::pager::{PENDING_BYTE, Pager, be_u16, be_u32, put_be_u32};
use crate::record::{Value, decode_record, decode_varint, encode_record};
use crate::storage::Storage;

/// A root page (its parent is 0).
pub const PTRMAP_ROOTPAGE: u8 = 1;
/// A page on the freelist, trunk or leaf (parent 0).
pub const PTRMAP_FREEPAGE: u8 = 2;
/// The first page of an overflow chain; the parent is the B-tree
/// page holding the cell.
pub const PTRMAP_OVERFLOW1: u8 = 3;
/// A later overflow page; the parent is the previous page of the
/// chain.
pub const PTRMAP_OVERFLOW2: u8 = 4;
/// A non-root B-tree page; the parent is its parent page.
pub const PTRMAP_BTREE: u8 = 5;

/// The lock-byte page (`PENDING_BYTE_PAGE`): never part of the
/// database.
pub fn pending_page(page_size: u32) -> u32 {
    PENDING_BYTE / page_size.max(1) + 1
}

/// The pin's `ptrmapPageno`: the pointer-map page holding `page`'s
/// entry (0 below page 2; the lock-byte page is skipped).
pub fn ptrmap_pageno(usable: u32, page_size: u32, page: u32) -> u32 {
    if page < 2 {
        return 0;
    }
    let per = usable / 5 + 1;
    let p = ((page - 2) / per) * per + 2;
    if p == pending_page(page_size) {
        p + 1
    } else {
        p
    }
}

/// `page` is a pointer-map page (`PTRMAP_ISPAGE`).
pub fn is_ptrmap(usable: u32, page_size: u32, page: u32) -> bool {
    ptrmap_pageno(usable, page_size, page) == page
}

fn corrupt<T, E>(page: u32) -> Result<T, E> {
    Err(Error::CorruptPage { page })
}

/// The derived pointer map: each page's (role, parent), indexed by
/// page number (role 0: no entry — page 1, pointer-map pages, the
/// lock-byte page).
pub struct Map {
    ty: AVec<u8>,
    par: AVec<u32>,
}

impl Map {
    fn new<E>(npage: u32) -> Result<Map, E> {
        let n = npage as usize + 1;
        let mut ty = AVec::new();
        ty.try_reserve(n)?;
        let mut par = AVec::new();
        par.try_reserve(n)?;
        for _ in 0..n {
            ty.push(0);
            par.push(0);
        }
        Ok(Map { ty, par })
    }

    /// `page`'s (role, parent); (0, 0) outside the map.
    pub fn get(&self, page: u32) -> (u8, u32) {
        let i = page as usize;
        (
            self.ty.as_slice().get(i).copied().unwrap_or(0),
            self.par.as_slice().get(i).copied().unwrap_or(0),
        )
    }

    fn put(&mut self, page: u32, ty: u8, parent: u32) {
        let i = page as usize;
        if let Some(t) = self.ty.as_mut_slice().get_mut(i) {
            *t = ty;
        }
        if let Some(p) = self.par.as_mut_slice().get_mut(i) {
            *p = parent;
        }
    }

    /// Record a reference to `page` found while walking: a page
    /// out of range, page 1, or one referenced twice is corruption.
    fn claim<E>(&mut self, page: u32, ty: u8, parent: u32) -> Result<(), E> {
        match self.ty.as_slice().get(page as usize) {
            Some(0) if page >= 2 => {
                self.put(page, ty, parent);
                Ok(())
            }
            _ => corrupt(page),
        }
    }
}

/// One cell's layout (the pin's `CellInfo`).
struct Cell {
    /// Bytes of the cell on its page.
    size: usize,
    local: usize,
    payload: u64,
    /// The left child (interior pages).
    child: u32,
}

fn cell_at(d: &[u8], pc: usize, kind: u8, usable: usize) -> Option<Cell> {
    let cell = d.get(pc..)?;
    let mut off = 0usize;
    let mut child = 0;
    if kind == KIND_INTERIOR_TABLE || kind == KIND_INTERIOR_INDEX {
        child = be_u32(cell.get(..4)?, 0);
        off = 4;
    }
    if kind == KIND_INTERIOR_TABLE {
        let (_, n) = decode_varint::<()>(cell.get(off..)?).ok()?;
        return Some(Cell {
            size: off + n,
            local: 0,
            payload: 0,
            child,
        });
    }
    let (plen, n1) = decode_varint::<()>(cell.get(off..)?).ok()?;
    off += n1;
    let local_max = if kind == KIND_LEAF_TABLE {
        let (_, n2) = decode_varint::<()>(cell.get(off..)?).ok()?;
        off += n2;
        usable.checked_sub(35)?
    } else {
        (usable.checked_sub(12)? * 64 / 255).checked_sub(23)?
    };
    let min = (usable.checked_sub(12)? * 32 / 255).checked_sub(23)?;
    let p = usize::try_from(plen).ok()?;
    let local = if p <= local_max {
        p
    } else {
        let surplus = min + (p - min) % usable.checked_sub(4)?;
        if surplus <= local_max { surplus } else { min }
    };
    let mut size = off.checked_add(local)?;
    if (local as u64) < plen {
        size = size.checked_add(4)?;
    }
    Some(Cell {
        size: size.max(4),
        local,
        payload: plen,
        child,
    })
}

/// A B-tree page's kind, header offset, cell count and cell
/// offsets, checked against the page.
struct PageCells {
    kind: u8,
    hdr: usize,
    cells: AVec<usize>,
}

fn page_cells<E>(d: &[u8], page: u32) -> Result<PageCells, E> {
    let h = btree::head::<E>(d, page)?;
    let mut cells = AVec::new();
    for i in 0..usize::from(h.ncell) {
        let at = h.cell_ptrs_off() + 2 * i;
        let pc = d.get(at..at + 2).map(|_| usize::from(be_u16(d, at)));
        match pc {
            Some(pc) if pc < d.len() => cells.push(pc),
            _ => return corrupt(page),
        }
    }
    Ok(PageCells {
        kind: h.kind,
        hdr: h.hdr_off,
        cells,
    })
}

async fn page_copy<S: Storage>(pager: &mut Pager<S>, page: u32) -> Result<AVec<u8>, S::Error> {
    let idx = pager.load(page).await?;
    let mut v = AVec::new();
    v.extend_from_slice(pager.frame_data(idx));
    Ok(v)
}

/// The root pages `sqlite_schema` names (tables and indexes; views
/// and triggers have 0).
async fn schema_roots<S: Storage>(pager: &mut Pager<S>) -> Result<AVec<u32>, S::Error> {
    let mut out = AVec::new();
    let mut cur = btree::ScanCursor::new(1);
    let mut buf = AVec::new();
    while cur.next_row_into(pager, &mut buf).await?.is_some() {
        let vals = decode_record::<S::Error>(buf.as_slice(), 5)?;
        if let Some(Value::Integer(r)) = vals.as_slice().get(3)
            && *r > 0
        {
            out.push(u32::try_from(*r).map_err(|_| Error::CorruptPage { page: 1 })?);
        }
    }
    Ok(out)
}

/// The overflow pointer of a cell that spills: its position.
fn ovfl_ptr_at(c: &Cell, pc: usize) -> Option<usize> {
    if (c.local as u64) < c.payload {
        pc.checked_add(c.size)?.checked_sub(4)
    } else {
        None
    }
}

/// Walk the tree rooted at `root` into `map` (the root's own entry
/// is the caller's).
async fn walk_tree<S: Storage>(
    pager: &mut Pager<S>,
    map: &mut Map,
    root: u32,
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    let mut stack: AVec<u32> = AVec::new();
    stack.push(root);
    while let Some(page) = stack.pop() {
        let d = page_copy(pager, page).await?;
        let d = d.as_slice();
        let pcs = page_cells::<S::Error>(d, page)?;
        let interior = pcs.kind == KIND_INTERIOR_TABLE || pcs.kind == KIND_INTERIOR_INDEX;
        for &pc in pcs.cells.iter() {
            let c = cell_at(d, pc, pcs.kind, usable).ok_or(Error::CorruptPage { page })?;
            if interior {
                map.claim::<S::Error>(c.child, PTRMAP_BTREE, page)?;
                stack.push(c.child);
            }
            if let Some(at) = ovfl_ptr_at(&c, pc) {
                if at + 4 > d.len() {
                    return corrupt(page);
                }
                let first = be_u32(d, at);
                let extra = c.payload - c.local as u64;
                let per = (usable - 4) as u64;
                let n = extra.div_ceil(per);
                map.claim::<S::Error>(first, PTRMAP_OVERFLOW1, page)?;
                let mut prev = first;
                for _ in 1..n {
                    let ov = page_copy(pager, prev).await?;
                    let next = be_u32(ov.as_slice(), 0);
                    map.claim::<S::Error>(next, PTRMAP_OVERFLOW2, prev)?;
                    prev = next;
                }
            }
        }
        if interior {
            let right = be_u32(d, pcs.hdr + 8);
            map.claim::<S::Error>(right, PTRMAP_BTREE, page)?;
            stack.push(right);
        }
    }
    Ok(())
}

/// Derive the whole pointer map from the file's structure: every
/// tree, every overflow chain, the freelist. A page nothing
/// references is corruption (the pin's "never used").
pub async fn derive<S: Storage>(pager: &mut Pager<S>) -> Result<Map, S::Error> {
    let n = pager.page_count();
    let (usable, ps) = (pager.usable(), pager.page_size());
    let mut map = Map::new::<S::Error>(n)?;
    let roots = schema_roots(pager).await?;
    walk_tree(pager, &mut map, 1).await?;
    for &r in roots.iter() {
        if r > n || is_ptrmap(usable, ps, r) {
            return corrupt(r);
        }
        map.claim::<S::Error>(r, PTRMAP_ROOTPAGE, 0)?;
        walk_tree(pager, &mut map, r).await?;
    }
    // The freelist: trunks and their leaves.
    let (mut trunk, count) = (pager.header().freelist_trunk, pager.header().freelist_count);
    let mut seen = 0u32;
    while trunk != 0 {
        map.claim::<S::Error>(trunk, PTRMAP_FREEPAGE, 0)?;
        let d = page_copy(pager, trunk).await?;
        let d = d.as_slice();
        let k = be_u32(d, 4);
        if k > usable / 4 - 2 {
            return corrupt(trunk);
        }
        for i in 0..k as usize {
            map.claim::<S::Error>(be_u32(d, 8 + 4 * i), PTRMAP_FREEPAGE, 0)?;
        }
        seen = seen.saturating_add(k).saturating_add(1);
        if seen > count {
            return corrupt(trunk);
        }
        trunk = be_u32(d, 0);
    }
    if seen != count {
        return corrupt(1);
    }
    let pend = pending_page(ps);
    for p in 2..=n {
        if map.get(p).0 == 0 && p != pend && !is_ptrmap(usable, ps, p) {
            return corrupt(p);
        }
    }
    Ok(map)
}

/// Write `map` into the pointer-map pages (entries for pages past
/// the end are left as they are, as on the pin). Only pages whose
/// bytes change are written.
pub async fn write_map<S: Storage>(pager: &mut Pager<S>, map: &Map) -> Result<(), S::Error> {
    let n = pager.page_count();
    let (usable, ps) = (pager.usable(), pager.page_size());
    let per = usable / 5 + 1;
    let mut pm = 2u32;
    while pm <= n {
        let p = ptrmap_pageno(usable, ps, pm);
        let mut want = page_copy(pager, p).await?;
        let mut q = p + 1;
        while q <= n && ptrmap_pageno(usable, ps, q) == p {
            let off = 5 * (q - p - 1) as usize;
            let (t, par) = map.get(q);
            if let Some(e) = want.as_mut_slice().get_mut(off..off + 5) {
                e[0] = t;
                put_be_u32(e, 1, par);
            }
            q += 1;
        }
        let idx = pager.load(p).await?;
        if pager.frame_data(idx) != want.as_slice() {
            let idx = pager.dirty_page(p).await?;
            pager.frame_data_mut(idx).copy_from_slice(want.as_slice());
        }
        pm = match p.checked_add(per) {
            Some(x) => x,
            None => break,
        };
    }
    Ok(())
}

/// The largest root page (header offset 52).
pub async fn largest_root<S: Storage>(pager: &mut Pager<S>) -> Result<u32, S::Error> {
    ensure_header(pager).await?;
    let i = pager.load(1).await?;
    Ok(be_u32(pager.frame_data(i), 52))
}

async fn set_largest_root<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<(), S::Error> {
    pager.set_header_u32(52, root).await
}

/// `allocateBtreePage`'s modes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Any page (`nearby` only steers which leaf).
    Any,
    /// Exactly `nearby` when it is free.
    Exact,
    /// A page at or below `nearby`.
    Le,
}

/// The pin's `sqlite3AbsInt32`.
fn abs32(x: i32) -> i32 {
    if x >= 0 {
        x
    } else if x != i32::MIN {
        -x
    } else {
        i32::MAX
    }
}

/// The pin's `allocateBtreePage`: a page off the freelist (`nearby`
/// and `mode` choosing which), else one more page at the end. The
/// page is made writable; its content is the caller's to set.
async fn allocate<S: Storage>(
    pager: &mut Pager<S>,
    map: &Map,
    nearby: u32,
    mode: Mode,
) -> Result<u32, S::Error> {
    let n = pager.header().freelist_count;
    let mx = pager.page_count();
    if n >= mx {
        return corrupt(1);
    }
    if n == 0 {
        let (p, _) = pager.grow_page().await?;
        return Ok(p);
    }
    let usable = pager.usable();
    let search = match mode {
        Mode::Exact => nearby <= mx && map.get(nearby).0 == PTRMAP_FREEPAGE,
        Mode::Le => true,
        Mode::Any => false,
    };
    pager.header_mut().freelist_count = n - 1;
    let mut prev = 0u32;
    let mut n_search = 0u32;
    loop {
        let trunk = if prev == 0 {
            pager.header().freelist_trunk
        } else {
            be_u32(page_copy(pager, prev).await?.as_slice(), 0)
        };
        let over = n_search > n;
        n_search += 1;
        if trunk > mx || trunk < 2 || over {
            return corrupt(if prev == 0 { 1 } else { prev });
        }
        let d = page_copy(pager, trunk).await?;
        let d = d.as_slice();
        let k = be_u32(d, 4);
        if k == 0 && !search {
            pager.dirty_page(trunk).await?;
            pager.header_mut().freelist_trunk = be_u32(d, 0);
            return Ok(trunk);
        } else if k > usable / 4 - 2 {
            return corrupt(trunk);
        } else if search && (nearby == trunk || (trunk < nearby && mode == Mode::Le)) {
            // The trunk itself is wanted: its first leaf (if any)
            // becomes the trunk in its place.
            pager.dirty_page(trunk).await?;
            let link = if k == 0 {
                be_u32(d, 0)
            } else {
                let nt = be_u32(d, 8);
                if nt > mx || nt < 2 {
                    return corrupt(trunk);
                }
                let ni = pager.dirty_page(nt).await?;
                let km = (k - 1) as usize;
                let nd = pager.frame_data_mut(ni);
                nd[0..4].copy_from_slice(&d[0..4]);
                put_be_u32(nd, 4, k - 1);
                nd[8..8 + 4 * km].copy_from_slice(&d[12..12 + 4 * km]);
                nt
            };
            if prev == 0 {
                pager.header_mut().freelist_trunk = link;
            } else {
                let pi = pager.dirty_page(prev).await?;
                put_be_u32(pager.frame_data_mut(pi), 0, link);
            }
            return Ok(trunk);
        } else if k > 0 {
            let leaf = |i: u32| be_u32(d, 8 + 4 * i as usize);
            let mut closest = 0u32;
            if nearby > 0 {
                if mode == Mode::Le {
                    for i in 0..k {
                        if leaf(i) <= nearby {
                            closest = i;
                            break;
                        }
                    }
                } else {
                    let mut dist = abs32(leaf(0).wrapping_sub(nearby) as i32);
                    for i in 1..k {
                        let d2 = abs32(leaf(i).wrapping_sub(nearby) as i32);
                        if d2 < dist {
                            closest = i;
                            dist = d2;
                        }
                    }
                }
            }
            let page = leaf(closest);
            if page > mx || page < 2 {
                return corrupt(trunk);
            }
            if !search || page == nearby || (page < nearby && mode == Mode::Le) {
                let ti = pager.dirty_page(trunk).await?;
                let td = pager.frame_data_mut(ti);
                if closest < k - 1 {
                    let last = be_u32(td, 4 + 4 * k as usize);
                    put_be_u32(td, 8 + 4 * closest as usize, last);
                }
                put_be_u32(td, 4, k - 1);
                pager.dirty_page(page).await?;
                return Ok(page);
            }
        }
        if !search {
            return corrupt(trunk);
        }
        prev = trunk;
    }
}

/// Point `parent`'s reference to `from` at `to` instead (the pin's
/// `modifyPagePointer`).
async fn modify_pointer<S: Storage>(
    pager: &mut Pager<S>,
    parent: u32,
    from: u32,
    to: u32,
    ty: u8,
) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    let idx = pager.dirty_page(parent).await?;
    let d = pager.frame_data_mut(idx);
    if ty == PTRMAP_OVERFLOW2 {
        if be_u32(d, 0) != from {
            return corrupt(parent);
        }
        put_be_u32(d, 0, to);
        return Ok(());
    }
    let pcs = page_cells::<S::Error>(d, parent)?;
    for &pc in pcs.cells.iter() {
        if ty == PTRMAP_OVERFLOW1 {
            let c = cell_at(d, pc, pcs.kind, usable).ok_or(Error::CorruptPage { page: parent })?;
            if let Some(at) = ovfl_ptr_at(&c, pc) {
                if at + 4 > usable {
                    return corrupt(parent);
                }
                if be_u32(d, at) == from {
                    put_be_u32(d, at, to);
                    return Ok(());
                }
            }
        } else {
            if pc + 4 > usable {
                return corrupt(parent);
            }
            if be_u32(d, pc) == from {
                put_be_u32(d, pc, to);
                return Ok(());
            }
        }
    }
    let rp = pcs.hdr + 8;
    let interior = pcs.kind == KIND_INTERIOR_TABLE || pcs.kind == KIND_INTERIOR_INDEX;
    if ty != PTRMAP_BTREE || !interior || be_u32(d, rp) != from {
        return corrupt(parent);
    }
    put_be_u32(d, rp, to);
    Ok(())
}

/// Move page `from` (role `ty`, parent `parent`) to page `to`,
/// which the caller has made writable (the pin's `relocatePage`):
/// its content, its parent's pointer, and the map entries of the
/// pages it points at.
async fn relocate<S: Storage>(
    pager: &mut Pager<S>,
    map: &mut Map,
    from: u32,
    ty: u8,
    parent: u32,
    to: u32,
) -> Result<(), S::Error> {
    if from == to || from < 2 || to < 2 {
        return corrupt(from);
    }
    let src = page_copy(pager, from).await?;
    let ti = pager.dirty_page(to).await?;
    pager.frame_data_mut(ti).copy_from_slice(src.as_slice());
    let d = src.as_slice();
    if ty == PTRMAP_BTREE || ty == PTRMAP_ROOTPAGE {
        let usable = pager.usable() as usize;
        let pcs = page_cells::<S::Error>(d, from)?;
        let interior = pcs.kind == KIND_INTERIOR_TABLE || pcs.kind == KIND_INTERIOR_INDEX;
        for &pc in pcs.cells.iter() {
            let c = cell_at(d, pc, pcs.kind, usable).ok_or(Error::CorruptPage { page: from })?;
            if let Some(at) = ovfl_ptr_at(&c, pc) {
                if at + 4 > d.len() {
                    return corrupt(from);
                }
                map.put(be_u32(d, at), PTRMAP_OVERFLOW1, to);
            }
            if interior {
                map.put(c.child, PTRMAP_BTREE, to);
            }
        }
        if interior {
            map.put(be_u32(d, pcs.hdr + 8), PTRMAP_BTREE, to);
        }
    } else {
        let next = be_u32(d, 0);
        if next != 0 {
            map.put(next, PTRMAP_OVERFLOW2, to);
        }
    }
    if ty != PTRMAP_ROOTPAGE {
        modify_pointer(pager, parent, from, to, ty).await?;
    }
    map.put(to, ty, if ty == PTRMAP_ROOTPAGE { 0 } else { parent });
    Ok(())
}

/// The pin's `finalDbSize`: the page count once `n_free` free pages
/// (and the pointer-map pages they no longer need) are gone.
fn final_db_size(usable: u32, ps: u32, n_orig: u32, n_free: u32) -> u32 {
    let n_entry = usable / 5;
    let n_ptrmap = n_free
        .wrapping_sub(n_orig)
        .wrapping_add(ptrmap_pageno(usable, ps, n_orig))
        .wrapping_add(n_entry)
        / n_entry;
    let pend = pending_page(ps);
    let mut fin = n_orig.wrapping_sub(n_free).wrapping_sub(n_ptrmap);
    if n_orig > pend && fin < pend {
        fin = fin.wrapping_sub(1);
    }
    while fin > 0 && (is_ptrmap(usable, ps, fin) || fin == pend) {
        fin -= 1;
    }
    fin
}

/// The pin's `incrVacuumStep`: deal with page `last`, the end of the
/// file — off the freelist if free, else moved into a free page
/// (any free page at or below `fin` when committing; the first one
/// at or below `fin` otherwise). Returns false when the freelist is
/// empty (`SQLITE_DONE`). Without `commit` the file then ends one
/// page earlier.
async fn incr_vacuum_step<S: Storage>(
    pager: &mut Pager<S>,
    map: &mut Map,
    fin: u32,
    last: u32,
    commit: bool,
) -> Result<bool, S::Error> {
    let (usable, ps) = (pager.usable(), pager.page_size());
    let pend = pending_page(ps);
    if !is_ptrmap(usable, ps, last) && last != pend {
        if pager.header().freelist_count == 0 {
            return Ok(false);
        }
        let (ty, parent) = map.get(last);
        if ty == PTRMAP_ROOTPAGE || ty == 0 {
            return corrupt(last);
        }
        if ty == PTRMAP_FREEPAGE {
            if !commit {
                let got = allocate(pager, map, last, Mode::Exact).await?;
                if got != last {
                    return corrupt(last);
                }
            }
        } else {
            let (mode, near) = if commit {
                (Mode::Any, 0)
            } else {
                (Mode::Le, fin)
            };
            let free = loop {
                let db_size = pager.page_count();
                let p = allocate(pager, map, near, mode).await?;
                if p > db_size {
                    return corrupt(p);
                }
                if !(commit && p > fin) {
                    break p;
                }
            };
            if free >= last {
                return corrupt(free);
            }
            relocate(pager, map, last, ty, parent, free).await?;
        }
    }
    if !commit {
        let mut l = last;
        loop {
            l -= 1;
            if !(l == pend || is_ptrmap(usable, ps, l)) {
                break;
            }
        }
        pager.keep_tail(l).await?;
        pager.header_mut().page_count = l;
    }
    Ok(true)
}

/// Commit-time upkeep of an auto-vacuum file (from `drive_commit`):
/// in FULL mode the pin's `autoVacuumCommit` — pages from the end
/// moved into the free pages, the file truncated, the freelist
/// emptied — then the pointer map written.
pub async fn pre_commit<S: Storage>(pager: &mut Pager<S>) -> Result<(), S::Error> {
    ensure_header(pager).await?;
    let mut map = derive(pager).await?;
    if pager.header().auto_vacuum == 1 {
        let (usable, ps) = (pager.usable(), pager.page_size());
        let orig = pager.page_count();
        if is_ptrmap(usable, ps, orig) || orig == pending_page(ps) {
            return corrupt(orig);
        }
        let n_free = pager.header().freelist_count;
        if n_free > 0 {
            let fin = final_db_size(usable, ps, orig, n_free);
            if fin > orig || fin < 1 {
                return corrupt(1);
            }
            let mut i = orig;
            while i > fin {
                if !incr_vacuum_step(pager, &mut map, fin, i, true).await? {
                    break;
                }
                i -= 1;
            }
            pager.keep_tail(fin).await?;
            let h = pager.header_mut();
            h.freelist_trunk = 0;
            h.freelist_count = 0;
            h.page_count = fin;
        }
    }
    write_map(pager, &map).await
}

/// `PRAGMA incremental_vacuum(limit)`: up to `limit` steps of the
/// pin's `sqlite3BtreeIncrVacuum`, each removing the file's last
/// page (moving it into a free page first when it is in use).
/// Returns the steps taken: the pin's result rows, one per step.
pub async fn incremental<S: Storage>(pager: &mut Pager<S>, limit: u32) -> Result<u32, S::Error> {
    if !pager.autovac() || pager.header().freelist_count == 0 {
        return Ok(0);
    }
    pager.begin_write().await?;
    let mut map = derive(pager).await?;
    let (usable, ps) = (pager.usable(), pager.page_size());
    let mut steps = 0;
    while steps < limit {
        let orig = pager.page_count();
        let n_free = pager.header().freelist_count;
        if n_free >= orig {
            return corrupt(1);
        }
        if n_free == 0 {
            break;
        }
        let fin = final_db_size(usable, ps, orig, n_free);
        if orig < fin {
            return corrupt(1);
        }
        if !incr_vacuum_step(pager, &mut map, fin, orig, false).await? {
            break;
        }
        steps += 1;
    }
    Ok(steps)
}

/// A new table's or index's root page (the pin's `btreeCreateTable`):
/// in an auto-vacuum file the page after the largest root (skipping
/// pointer-map pages and the lock-byte page), the page there moved
/// out of the way; elsewhere any page. The page is zeroed and
/// writable.
pub async fn alloc_root<S: Storage>(pager: &mut Pager<S>) -> Result<u32, S::Error> {
    if !pager.autovac() {
        let (p, _) = pager.alloc_page().await?;
        return Ok(p);
    }
    pager.begin_write().await?;
    let mut map = derive(pager).await?;
    let (usable, ps) = (pager.usable(), pager.page_size());
    let mut root = largest_root(pager).await?;
    if root > pager.page_count() {
        return corrupt(root);
    }
    root += 1;
    while is_ptrmap(usable, ps, root) || root == pending_page(ps) {
        root += 1;
    }
    let mv = allocate(pager, &map, root, Mode::Exact).await?;
    if mv != root {
        let (ty, parent) = map.get(root);
        if ty == PTRMAP_ROOTPAGE || ty == PTRMAP_FREEPAGE || ty == 0 {
            return corrupt(root);
        }
        relocate(pager, &mut map, root, ty, parent, mv).await?;
    }
    let idx = pager.dirty_page(root).await?;
    pager.frame_data_mut(idx).fill(0);
    set_largest_root(pager, root).await?;
    Ok(root)
}

/// Free every page of the tree rooted at `root` but the root, in the
/// pin's order (`clearDatabasePage`: each cell's subtree, then its
/// overflow chain, then the right subtree, then the page).
async fn clear_tree<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<(), S::Error> {
    let usable = pager.usable() as usize;
    // Post-order without recursion: (page, next cell to visit).
    let mut order: AVec<u32> = AVec::new();
    let mut stack: AVec<(u32, AVec<u8>, usize)> = AVec::new();
    let d = page_copy(pager, root).await?;
    stack.push((root, d, 0));
    let limit = pager.page_count() as usize;
    while let Some((page, d, i)) = stack.pop() {
        // Deeper than any B-tree the format allows: a cycle.
        if stack.len() > 64 {
            return corrupt(root);
        }
        let pcs = page_cells::<S::Error>(d.as_slice(), page)?;
        let interior = pcs.kind == KIND_INTERIOR_TABLE || pcs.kind == KIND_INTERIOR_INDEX;
        let ncell = pcs.cells.len();
        if i < ncell {
            let pc = pcs.cells.as_slice()[i];
            let c =
                cell_at(d.as_slice(), pc, pcs.kind, usable).ok_or(Error::CorruptPage { page })?;
            if interior {
                // Visit the child first, then come back for this
                // cell's overflow (marked by i + ncell + 1).
                let child = c.child;
                stack.push((page, d, i + ncell + 1));
                let cd = page_copy(pager, child).await?;
                stack.push((child, cd, 0));
            } else {
                push_chain(pager, &c, pc, d.as_slice(), &mut order, limit).await?;
                stack.push((page, d, i + 1));
            }
        } else if i > ncell && i <= 2 * ncell {
            // Back from cell (i - ncell - 1)'s subtree.
            let ci = i - ncell - 1;
            let pc = pcs.cells.as_slice()[ci];
            let c =
                cell_at(d.as_slice(), pc, pcs.kind, usable).ok_or(Error::CorruptPage { page })?;
            push_chain(pager, &c, pc, d.as_slice(), &mut order, limit).await?;
            let nxt = if ci + 1 < ncell { ci + 1 } else { ncell };
            stack.push((page, d, nxt));
        } else if i == ncell && interior {
            let right = be_u32(d.as_slice(), pcs.hdr + 8);
            stack.push((page, d, 2 * ncell + 1));
            let rd = page_copy(pager, right).await?;
            stack.push((right, rd, 0));
        } else {
            if page != root {
                order.push(page);
            }
        }
        if order.len() > limit {
            return corrupt(root);
        }
    }
    for &p in order.iter() {
        pager.free_page(p).await?;
    }
    Ok(())
}

/// Append a cell's overflow chain to `order`.
async fn push_chain<S: Storage>(
    pager: &mut Pager<S>,
    c: &Cell,
    pc: usize,
    d: &[u8],
    order: &mut AVec<u32>,
    limit: usize,
) -> Result<(), S::Error> {
    let Some(at) = ovfl_ptr_at(c, pc) else {
        return Ok(());
    };
    if at + 4 > d.len() {
        return Err(Error::CorruptPage { page: 0 });
    }
    let usable = pager.usable() as u64;
    let mut n = (c.payload - c.local as u64).div_ceil(usable - 4);
    let mut p = be_u32(d, at);
    while n > 0 {
        n -= 1;
        if p < 2 || p > pager.page_count() || order.len() > limit {
            return Err(Error::CorruptPage { page: p });
        }
        order.push(p);
        if n > 0 {
            p = be_u32(page_copy(pager, p).await?.as_slice(), 0);
        }
    }
    Ok(())
}

/// Drop the tree rooted at `root` (the pin's `btreeDropTable`): its
/// pages freed; in an auto-vacuum file the largest root moved into
/// its slot unless it is that root. Returns the moved root's old
/// page (0: none) — the caller points `sqlite_schema` at `root`
/// instead. Roots are dropped largest first, so none of them moves.
pub async fn drop_tree<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<u32, S::Error> {
    clear_tree(pager, root).await?;
    if !pager.autovac() {
        pager.free_page(root).await?;
        return Ok(0);
    }
    let (usable, ps) = (pager.usable(), pager.page_size());
    let mut max = largest_root(pager).await?;
    let moved = if root == max {
        pager.free_page(root).await?;
        0
    } else {
        if max <= root || max > pager.page_count() {
            return corrupt(max);
        }
        let src = page_copy(pager, max).await?;
        let ti = pager.dirty_page(root).await?;
        pager.frame_data_mut(ti).copy_from_slice(src.as_slice());
        pager.free_page(max).await?;
        max
    };
    let pend = pending_page(ps);
    loop {
        max = max.saturating_sub(1);
        if !(max == pend || (max > 1 && is_ptrmap(usable, ps, max))) {
            break;
        }
    }
    set_largest_root(pager, max).await?;
    Ok(moved)
}

/// Point the `sqlite_schema` row whose root page is `from` at `to`
/// (the pin's `UPDATE sqlite_schema SET rootpage = ...` after a
/// root moved).
pub async fn schema_set_root<S: Storage>(
    pager: &mut Pager<S>,
    from: u32,
    to: u32,
) -> Result<(), S::Error> {
    let mut cur = btree::ScanCursor::new(1);
    let mut buf = AVec::new();
    let mut hit: Option<(i64, AVec<u8>)> = None;
    while let Some(rowid) = cur.next_row_into(pager, &mut buf).await? {
        let vals = decode_record::<S::Error>(buf.as_slice(), 5)?;
        if matches!(vals.as_slice().get(3), Some(Value::Integer(r)) if *r == i64::from(from)) {
            let mut out: AVec<Value<'_>> = AVec::new();
            for (i, v) in vals.iter().enumerate() {
                out.push(if i == 3 {
                    Value::Integer(i64::from(to))
                } else {
                    *v
                });
            }
            let mut rec = AVec::new();
            encode_record(out.as_slice(), &mut rec);
            hit = Some((rowid, rec));
            break;
        }
    }
    let Some((rowid, rec)) = hit else {
        return corrupt(1);
    };
    btree::btree_delete(pager, 1, rowid).await?;
    btree::btree_insert(pager, 1, rowid, rec.as_slice()).await
}

/// Make an empty database an auto-vacuum one (`PRAGMA auto_vacuum =
/// FULL | INCREMENTAL` before its first table, or the image VACUUM
/// builds): header offsets 52 (the largest root, 1 for none) and 64,
/// inside a write transaction the caller commits.
pub async fn set_mode<S: Storage>(pager: &mut Pager<S>, mode: u8) -> Result<(), S::Error> {
    pager.set_av_mode(mode);
    ensure_header(pager).await
}

/// The header of a database the connection made auto-vacuum names
/// it so (the pin's `newDatabase`, run again by the first write
/// after a ROLLBACK undid the header): offset 52 at least 1, offset
/// 64 the mode's.
async fn ensure_header<S: Storage>(pager: &mut Pager<S>) -> Result<(), S::Error> {
    let mode = pager.av_mode();
    if mode == 0 {
        return Ok(());
    }
    let i = pager.load(1).await?;
    let d = pager.frame_data(i);
    if be_u32(d, 52) != 0 {
        return Ok(());
    }
    pager.set_header_u32(52, 1).await?;
    pager.set_header_u32(64, u32::from(mode == 2)).await?;
    pager.header_mut().auto_vacuum = mode;
    Ok(())
}
