//! Index cursors and the access walk (ADR 0005 Stage 2, unit
//! `vm_joins`; the plans come from the `expr_index` access planner).
//!
//! An index cursor reads an index B-tree in key order, interior
//! entries between their subtrees, as `btree::IndexCursor` does, and
//! seeks with the pin's per-page binary search
//! (`sqlite3BtreeIndexMoveto`, `btree::IndexCursor::seek_found`). It
//! is restartable as the table cursor is: a move works on copies (a
//! step inside a leaf touches nothing until the entry is decoded; a
//! step across pages works on a copy of the page stack) and commits
//! only when no page was missing. Every page a move reads, overflow
//! pages included, stays pinned until the opcode completes, so a key
//! whose overflow chain is longer than the page cache is
//! `CacheExhausted` (a key of several pages; the corpus has none).
//!
//! The walk's per-entry test (`walk_test`), the probe sets
//! (`probe_list`) and the covering-index row (`cover_row`) port
//! `session::xindex::access_walk`, `probe_vals` and `access_rows`
//! rule for rule; the code generator lays the walk's loops out
//! (`gen::access`).

use super::cursor::{Sync, fetch};
use crate::arena::AVec;
use crate::btree::{self, KeyPart};
use crate::error::Error;
use crate::pager::{PagerCore, be_u32};
use crate::record::{Affinity, KeyOrder, OwnedValue, compare_coll, compare_coll_in};
use core::cmp::Ordering;

/// The value of a sync step, or return its pending I/O.
macro_rules! ready {
    ($e:expr) => {
        match $e? {
            Ok(v) => v,
            Err(io) => return Ok(Err(io)),
        }
    };
}

/// A deeper descent than any valid file has is a cycle.
const MAX_DEPTH: usize = 64;

/// What an index cursor reads (`P4::Index`): the index's root and
/// its key columns' directions and collations.
#[derive(Debug, Clone)]
pub(crate) struct IndexInfo {
    pub(crate) root: u32,
    pub(crate) desc: AVec<KeyOrder>,
}

/// One page on an index cursor's path: a snapshot of it and the
/// next action (a leaf: the next entry; an interior page: even, the
/// child `seq / 2` next; odd, the entry `(seq - 1) / 2`).
#[derive(Clone)]
struct Frame {
    page: u32,
    img: AVec<u8>,
    interior: bool,
    ncell: usize,
    rightmost: u32,
    seq: usize,
}

/// A cursor over an index B-tree.
pub(crate) struct IdxCursor {
    /// The `P4::Index` entry it was opened with.
    pub(crate) info: u32,
    root: u32,
    nfields: usize,
    stack: AVec<Frame>,
    /// The current entry: the key values, then the rowid.
    pub(crate) entry: AVec<OwnedValue>,
    pub(crate) valid: bool,
}

/// A snapshot of `page` as a cursor frame.
fn frame(core: &mut PagerCore<'_>, pins: &mut AVec<u32>, page: u32) -> Sync<Frame> {
    let idx = ready!(fetch(core, pins, page, true));
    let bytes = core.data(idx);
    let h = btree::head::<()>(bytes, page)?;
    let interior = match h.kind {
        btree::KIND_LEAF_INDEX => false,
        btree::KIND_INTERIOR_INDEX => true,
        _ => return Err(Error::Unsupported),
    };
    let mut img = AVec::with_capacity(bytes.len());
    img.extend_from_slice(bytes);
    Ok(Ok(Frame {
        page,
        img,
        interior,
        ncell: usize::from(h.ncell),
        rightmost: h.rightmost,
        seq: 0,
    }))
}

/// Child `i` of an interior frame (`ncell`: the right-most).
fn child(f: &Frame, i: usize) -> Result<u32, Error<()>> {
    if i == f.ncell {
        return Ok(f.rightmost);
    }
    let h = btree::head::<()>(f.img.as_slice(), f.page)?;
    let ptr = btree::cell_ptr::<()>(f.img.as_slice(), &h, i, f.page)?;
    match f.img.as_slice().get(ptr..ptr.saturating_add(4)) {
        Some(_) => Ok(be_u32(f.img.as_slice(), ptr)),
        None => Err(Error::CorruptPage { page: f.page }),
    }
}

/// Entry `i` of frame `f`, decoded (its overflow pages read and
/// pinned; a UTF-16 database's text read as UTF-8).
fn entry_of(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    f: &Frame,
    i: usize,
    nfields: usize,
) -> Sync<AVec<OwnedValue>> {
    let usable = core.usable() as usize;
    let bytes = f.img.as_slice();
    let h = btree::head::<()>(bytes, f.page)?;
    let ptr = btree::cell_ptr::<()>(bytes, &h, i, f.page)?;
    let l = btree::index_cell_layout::<()>(bytes, ptr, usable, f.page, f.interior)?;
    let from = ptr.saturating_add(l.payload_off);
    let local = from
        .checked_add(l.local_len)
        .and_then(|end| bytes.get(from..end))
        .ok_or(Error::CorruptPage { page: f.page })?;
    let mut pay = AVec::with_capacity(l.payload_len);
    pay.extend_from_slice(local);
    let per_page = usable.saturating_sub(4);
    let mut next = l.overflow;
    let mut hops = 0u32;
    while pay.len() < l.payload_len {
        if next == 0 {
            return Err(Error::CorruptRecord);
        }
        hops = hops.saturating_add(1);
        if hops > core.page_count().saturating_add(1) {
            return Err(Error::OverflowChainCycle);
        }
        let idx = ready!(fetch(core, pins, next, true));
        let d = core.data(idx);
        if d.len() < 4 {
            return Err(Error::CorruptPage { page: next });
        }
        let following = be_u32(d, 0);
        let take = l.payload_len.saturating_sub(pay.len()).min(per_page);
        let chunk = d
            .get(4..4usize.saturating_add(take))
            .ok_or(Error::CorruptPage { page: next })?;
        pay.extend_from_slice(chunk);
        next = following;
    }
    #[cfg(feature = "utf16")]
    if let Some(be) = crate::utf16::byte_order(core.text_enc()) {
        let raw = core::mem::take(&mut pay);
        crate::utf16::transcode_record::<()>(raw.as_slice(), be, false, &mut pay)?;
    }
    let e = btree::decode_index_entry::<()>(pay.as_slice(), nfields)?;
    Ok(Ok(e.values))
}

/// The pin's per-page binary search (`LazyPage::search`): the equal
/// entry and `true`, or the lower bound the search ended at.
fn search(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    f: &Frame,
    nfields: usize,
    probe: &[KeyPart],
    desc: &[KeyOrder],
) -> Sync<(usize, bool)> {
    let enc = core.text_enc();
    let (mut lo, mut hi) = (0usize, f.ncell);
    while lo < hi {
        let mid = lo + (hi - 1 - lo) / 2;
        let e = ready!(entry_of(core, pins, f, mid, nfields));
        match btree::cmp_entry_probe(e.as_slice(), probe, desc, enc) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Ok((mid, true))),
        }
    }
    Ok(Ok((lo, false)))
}

/// The next entry of the in-order walk `stack` describes (advancing
/// it), or `None` at the end of the tree.
fn advance(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    stack: &mut AVec<Frame>,
    nfields: usize,
) -> Sync<Option<AVec<OwnedValue>>> {
    loop {
        if stack.len() > MAX_DEPTH {
            let page = stack.as_slice().last().map_or(0, |f| f.page);
            return Err(Error::CorruptPage { page });
        }
        let Some(top) = stack.as_mut_slice().last_mut() else {
            return Ok(Ok(None));
        };
        if !top.interior {
            if top.seq >= top.ncell {
                stack.pop();
                continue;
            }
            let e = ready!(entry_of(core, pins, top, top.seq, nfields));
            top.seq = top.seq.saturating_add(1);
            return Ok(Ok(Some(e)));
        }
        if top.seq > top.ncell.saturating_mul(2) {
            stack.pop();
            continue;
        }
        if top.seq % 2 == 1 {
            let e = ready!(entry_of(core, pins, top, (top.seq - 1) / 2, nfields));
            top.seq = top.seq.saturating_add(1);
            return Ok(Ok(Some(e)));
        }
        let c = child(top, top.seq / 2)?;
        top.seq = top.seq.saturating_add(1);
        if c == 0 || c == top.page {
            return Err(Error::CorruptPage { page: top.page });
        }
        let f = ready!(frame(core, pins, c));
        stack.push(f);
    }
}

impl IdxCursor {
    pub(crate) fn new(info: u32, ii: &IndexInfo) -> IdxCursor {
        IdxCursor {
            info,
            root: ii.root,
            nfields: ii.desc.len().saturating_add(1),
            stack: AVec::new(),
            entry: AVec::new(),
            valid: false,
        }
    }

    fn commit(&mut self, stack: AVec<Frame>, e: Option<AVec<OwnedValue>>) -> bool {
        self.stack = stack;
        match e {
            Some(e) => {
                self.entry = e;
                self.valid = true;
            }
            None => {
                self.entry.clear();
                self.valid = false;
            }
        }
        self.valid
    }

    /// The first entry; false for an empty index.
    pub(crate) fn rewind(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        let mut st = AVec::new();
        st.push(ready!(frame(core, pins, self.root)));
        let e = ready!(advance(core, pins, &mut st, self.nfields));
        Ok(Ok(self.commit(st, e)))
    }

    /// The first entry at or after `probe` (a full-length probe:
    /// prefixes are padded with `KeyPart::Min`); false when none.
    pub(crate) fn seek(
        &mut self,
        core: &mut PagerCore<'_>,
        pins: &mut AVec<u32>,
        probe: &[KeyPart],
        desc: &[KeyOrder],
    ) -> Sync<bool> {
        let mut st: AVec<Frame> = AVec::new();
        let mut page = self.root;
        loop {
            if st.len() > MAX_DEPTH {
                return Err(Error::CorruptPage { page });
            }
            let mut f = ready!(frame(core, pins, page));
            let (i, equal) = ready!(search(core, pins, &f, self.nfields, probe, desc));
            if !f.interior {
                f.seq = i;
                st.push(f);
                break;
            }
            // The divider itself when equal (yielded next); else child
            // `i`, then divider `i` once that subtree is done.
            f.seq = i.saturating_mul(2).saturating_add(1);
            if equal {
                st.push(f);
                break;
            }
            let c = child(&f, i)?;
            if c == 0 || c == page {
                return Err(Error::CorruptPage { page });
            }
            st.push(f);
            page = c;
        }
        let e = ready!(advance(core, pins, &mut st, self.nfields));
        Ok(Ok(self.commit(st, e)))
    }

    /// The next entry; false past the last one.
    pub(crate) fn next(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        if !self.valid {
            return Ok(Ok(false));
        }
        // A step inside the current leaf: nothing changes until the
        // entry is decoded.
        if let Some(top) = self.stack.as_slice().last()
            && !top.interior
            && top.seq < top.ncell
        {
            let e = ready!(entry_of(core, pins, top, top.seq, self.nfields));
            if let Some(top) = self.stack.as_mut_slice().last_mut() {
                top.seq = top.seq.saturating_add(1);
            }
            self.entry = e;
            return Ok(Ok(true));
        }
        let mut st = self.stack.clone();
        let e = ready!(advance(core, pins, &mut st, self.nfields));
        Ok(Ok(self.commit(st, e)))
    }

    /// The current entry's rowid (its last field, an integer).
    pub(crate) fn rowid(&self) -> Result<i64, Error<()>> {
        match self.entry.as_slice().last() {
            Some(OwnedValue::Integer(r)) if self.valid => Ok(*r),
            _ => Err(Error::CorruptRecord),
        }
    }
}

/// One equality probe set (`P4::Probe`; `xindex::probe_vals`).
#[derive(Debug, Clone)]
pub(crate) struct ProbeInfo {
    /// Values in registers `p3 .. p3 + n`.
    pub(crate) n: usize,
    pub(crate) aff: Option<Affinity>,
    pub(crate) is: bool,
    pub(crate) is_in: bool,
    /// The probed key column's direction and collation.
    pub(crate) key: KeyOrder,
    /// The walk is backwards.
    pub(crate) reverse: bool,
}

/// A probe set's values: converted by the comparison's affinity,
/// NULLs dropped unless `IS`, an IN list sorted and deduplicated
/// under the key's collation, in the key column's order (backwards
/// for a reverse walk).
pub(crate) fn probe_list(
    p: &ProbeInfo,
    vals: &[OwnedValue],
    enc: crate::record::TextEnc,
) -> AVec<OwnedValue> {
    let coll = p.key.coll;
    let mut out: AVec<OwnedValue> = AVec::new();
    for v in vals.iter() {
        let v = crate::query::probe_value(v.clone(), p.aff);
        if matches!(v, OwnedValue::Null) && !p.is {
            continue;
        }
        out.push(v);
    }
    if p.is_in {
        crate::arena::sort_by(out.as_mut_slice(), &mut |a, b| {
            compare_coll_in(&a.as_value(), &b.as_value(), coll, enc)
        });
        let mut d: AVec<OwnedValue> = AVec::new();
        for v in out.iter() {
            let dup = d.as_slice().last().is_some_and(|l| {
                compare_coll(&l.as_value(), &v.as_value(), coll) == Ordering::Equal
            });
            if !dup {
                d.push(v.clone());
            }
        }
        out = d;
    }
    if p.reverse != p.key.desc {
        out.as_mut_slice().reverse();
    }
    out
}

/// The access walk's test of one index entry (`P4::Walk`): the
/// loop body of `xindex::access_walk` for one combination of the
/// equality values.
#[derive(Debug, Clone)]
pub(crate) struct Walk {
    /// The combination's values, in registers `prefix ..`.
    pub(crate) prefix: u32,
    pub(crate) nprefix: usize,
    pub(crate) desc: AVec<KeyOrder>,
    /// The range applies to key column `k` (`ko` its order).
    pub(crate) on_key: bool,
    pub(crate) k: usize,
    pub(crate) ko: KeyOrder,
    /// The range bounds: register, inclusive.
    pub(crate) lo: Option<(u32, bool)>,
    pub(crate) hi: Option<(u32, bool)>,
    /// The bounds apply to the rowid after the key.
    pub(crate) range_rowid: bool,
    /// The rowid's own equality values (a row list).
    pub(crate) rowids: Option<u32>,
    /// The key field whose NULL flags the row (WHERE_BIGNULL_SORT).
    pub(crate) bignull: Option<usize>,
    /// The register a found entry's NULL flag is written to.
    pub(crate) flag: u32,
}

/// What `walk_test` decides for an entry.
pub(crate) enum WalkStep {
    /// The entry's row is found (with its NULL flag).
    Found(bool),
    /// Skip it.
    Skip,
    /// The walk is over.
    Stop,
}

/// A rowid range end (`xindex::rowid_bound`): `None` when no rowid
/// satisfies it, `Some(None)` for an open end.
fn rowid_limit(b: Option<(u32, bool)>, regs: &[OwnedValue], lower: bool) -> Option<Option<i64>> {
    match b {
        None => Some(None),
        Some((r, incl)) => {
            let v = regs.get(r as usize).unwrap_or(&OwnedValue::Null);
            crate::session::rowid_end(v, lower, incl)
        }
    }
}

/// The test (see `Walk`), with `regs` the VM's registers and
/// `rowids` the rowid equality values.
pub(crate) fn walk_test(
    w: &Walk,
    vals: &[OwnedValue],
    regs: &[OwnedValue],
    rowids: Option<&[OwnedValue]>,
    enc: crate::record::TextEnc,
) -> Result<WalkStep, Error<()>> {
    let (lo_r, hi_r) = if w.range_rowid {
        (
            rowid_limit(w.lo, regs, true),
            rowid_limit(w.hi, regs, false),
        )
    } else {
        (Some(None), Some(None))
    };
    let (Some(lo_r), Some(hi_r)) = (lo_r, hi_r) else {
        return Ok(WalkStep::Stop);
    };
    let reg = |r: u32| regs.get(r as usize).unwrap_or(&OwnedValue::Null);
    for j in 0..w.nprefix {
        let p = reg(w.prefix.saturating_add(j as u32));
        let c = w.desc.as_slice().get(j).map(|o| o.coll).unwrap_or_default();
        let same = vals
            .get(j)
            .is_some_and(|v| compare_coll(&v.as_value(), &p.as_value(), c) == Ordering::Equal);
        if !same {
            return Ok(WalkStep::Stop);
        }
    }
    let rowid = match vals.last() {
        Some(OwnedValue::Integer(r)) => *r,
        _ => return Err(Error::CorruptRecord),
    };
    if w.on_key {
        let Some(v) = vals.get(w.k) else {
            return Ok(WalkStep::Skip);
        };
        let desc = w.ko.desc;
        if matches!(v, OwnedValue::Null) {
            return Ok(if desc { WalkStep::Stop } else { WalkStep::Skip });
        }
        if let Some((r, incl)) = w.lo {
            let c = compare_coll_in(&v.as_value(), &reg(r).as_value(), w.ko.coll, enc);
            if c == Ordering::Less || (c == Ordering::Equal && !incl) {
                return Ok(if desc { WalkStep::Stop } else { WalkStep::Skip });
            }
        }
        if let Some((r, incl)) = w.hi {
            let c = compare_coll_in(&v.as_value(), &reg(r).as_value(), w.ko.coll, enc);
            if c == Ordering::Greater || (c == Ordering::Equal && !incl) {
                return Ok(if desc { WalkStep::Skip } else { WalkStep::Stop });
            }
        }
    }
    if lo_r.is_some_and(|l| rowid < l) {
        return Ok(WalkStep::Skip);
    }
    if hi_r.is_some_and(|h| rowid > h) {
        return Ok(WalkStep::Stop);
    }
    if let Some(rv) = rowids
        && !rv.iter().any(|v| rowid_eq(v) == Some(rowid))
    {
        return Ok(WalkStep::Skip);
    }
    let null = w
        .bignull
        .is_some_and(|f| matches!(vals.get(f), Some(OwnedValue::Null)));
    Ok(WalkStep::Found(null))
}

/// A probe value as an exact rowid (`xindex::rowid_eq`).
fn rowid_eq(v: &OwnedValue) -> Option<i64> {
    match v {
        OwnedValue::Integer(i) => Some(*i),
        OwnedValue::Real(f) if (*f as i64) as f64 == *f => Some(*f as i64),
        _ => None,
    }
}

/// A covering index's row layout (`P4::Cover`): per entry field,
/// the table column it holds; the table's shape.
#[derive(Debug, Clone)]
pub(crate) struct CoverInfo {
    pub(crate) cover: AVec<Option<usize>>,
    pub(crate) ncols: usize,
    pub(crate) rowid_alias: Option<usize>,
    pub(crate) affs: AVec<Affinity>,
}

/// The row a covering index's entry builds (`xindex::access_rows`):
/// the key columns it holds, every other column NULL, read as a
/// query reads a row.
pub(crate) fn cover_row(c: &CoverInfo, vals: &[OwnedValue], rowid: i64) -> AVec<OwnedValue> {
    let mut rec: AVec<OwnedValue> = AVec::new();
    for _ in 0..c.ncols {
        rec.push(OwnedValue::Null);
    }
    for (j, col) in c.cover.iter().enumerate() {
        if let (Some(col), Some(v)) = (col, vals.get(j))
            && let Some(slot) = rec.as_mut_slice().get_mut(*col)
        {
            *slot = v.clone();
        }
    }
    crate::rules::eval_row(c.rowid_alias, c.affs.as_slice(), rec.as_slice(), rowid).vals
}

/// The seek probe of registers `vals`: the values, then `Min` up to
/// the index's key length plus the rowid.
pub(crate) fn probe_key(vals: &[OwnedValue], nkey: usize) -> AVec<KeyPart> {
    let mut probe = AVec::new();
    for v in vals.iter() {
        probe.push(KeyPart::Val(v.clone()));
    }
    while probe.len() < nkey.saturating_add(1) {
        probe.push(KeyPart::Min);
    }
    probe
}

/// Whether the entry's first `n` key values differ from `last`'s
/// (a skip-scan's next prefix).
#[cfg(feature = "planner")]
pub(crate) fn new_prefix(
    last: Option<&[OwnedValue]>,
    vals: &[OwnedValue],
    n: usize,
    desc: &[KeyOrder],
) -> bool {
    let Some(l) = last else {
        return true;
    };
    !l.iter()
        .zip(vals.iter().take(n))
        .enumerate()
        .all(|(j, (x, y))| {
            let c = desc.get(j).map(|o| o.coll).unwrap_or_default();
            compare_coll(&x.as_value(), &y.as_value(), c) == Ordering::Equal
        })
}
