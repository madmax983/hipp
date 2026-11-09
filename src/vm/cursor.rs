//! VM cursors (ADR 0005 §"Program representation"): sync,
//! restartable reads of rowid tables, and the ORDER BY sorter.
//!
//! The ADR's cursor kinds are `Table | Index | Pseudo | Ephemeral |
//! Sorter | Vtab | Tvf`; this unit builds `Table` and `Sorter`, and
//! `Cursor` takes the others as later units add them.
//!
//! A table cursor is positioned by `rewind` / `next` / `seek_*` /
//! `last` / `prev`. Each computes the new position in temporaries
//! (a copy of the descent stack, a snapshot of the new leaf) and
//! commits it only when every page it needed was in the cache; a
//! miss returns the I/O to do (`Err(io)` inside `Ok`) with the cursor
//! as it was. The pages a descent reads are recorded in `pins`, so
//! the driver keeps them while it loads the next one.

use super::Io;
use crate::arena::AVec;
use crate::btree::{self, CellMeta};
use crate::error::Error;
use crate::pager::{Got, PagerCore, be_u32};
use crate::query::RowData;
use crate::record::{Affinity, OwnedValue, Value};
use crate::sql::SortOrder;

/// A sync step: `Ok(Ok(v))` done, `Ok(Err(io))` waiting for `io`
/// with nothing changed, `Err(e)` failed.
pub(crate) type Sync<T> = Result<core::result::Result<T, Io>, Error<()>>;

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

/// What a table cursor reads (`P4::Table`): the table's root and the
/// layout its rows decode with — a FROM source's columns as the
/// query executor builds them (`session::decode_source_row`).
#[derive(Debug, Clone)]
pub(crate) struct TableInfo {
    pub(crate) root: u32,
    pub(crate) ncols: usize,
    pub(crate) rowid_alias: Option<usize>,
    pub(crate) affs: AVec<Affinity>,
    pub(crate) defaults: AVec<OwnedValue>,
    pub(crate) gcols: Option<alloc::boxed::Box<crate::rules::GenCols>>,
}

/// A sorter's keys (`P4::Sorter`).
pub(crate) struct SortInfo {
    /// Each ORDER BY term's direction, NULLS placement and collation.
    pub(crate) orders: AVec<SortOrder>,
    /// Result values per row (the sort keys follow them).
    pub(crate) nout: usize,
    /// The LIMIT register (OFFSET in the next one) when the statement
    /// has a LIMIT: the sorter then keeps only the rows the window
    /// may still return, as the pin's does.
    pub(crate) window: Option<u32>,
}

/// One VM cursor.
pub(crate) enum Cursor {
    Closed,
    Table(alloc::boxed::Box<TableCursor>),
    Sorter(alloc::boxed::Box<Sorter>),
    /// An in-memory row list (unit `vm_joins`).
    Eph(alloc::boxed::Box<super::rows::Eph>),
    /// A GROUP BY's groups.
    Groups(alloc::boxed::Box<super::rows::Groups>),
    /// A streamed aggregate's accumulators.
    Agg(alloc::boxed::Box<super::rows::AggRun>),
    #[cfg(feature = "expr_index")]
    Index(alloc::boxed::Box<super::index::IdxCursor>),
}

/// One interior page on a table cursor's path: its children (the
/// right-most last) and the child the path takes.
#[derive(Clone)]
struct Level {
    kids: AVec<u32>,
    at: usize,
}

/// The leaf a table cursor is on: a snapshot of the page (later
/// misses may evict its frame) and its cells.
#[derive(Default)]
struct Leaf {
    img: AVec<u8>,
    cells: AVec<CellMeta>,
}

/// The current row's payload as it is copied from the leaf and its
/// overflow chain. Unlike a position it survives a miss part-way
/// through the chain: the next run of the opcode goes on from `next`
/// rather than from the start, so a chain longer than the page cache
/// completes. It is cleared whenever the cursor moves.
#[derive(Default)]
struct Payload {
    buf: AVec<u8>,
    next: u32,
    hops: u32,
    started: bool,
}

/// A cursor over a rowid table's B-tree.
pub(crate) struct TableCursor {
    root: u32,
    /// The `P4::Table` entry it was opened with.
    pub(crate) info: u32,
    levels: AVec<Level>,
    leaf: Leaf,
    at: usize,
    valid: bool,
    /// The current row, decoded (`load`), with its rowid.
    pub(crate) row: RowData,
    decoded: bool,
    pay: Payload,
}

/// Where a descent stops in each page.
#[derive(Clone, Copy)]
enum Aim {
    First,
    #[cfg(feature = "expr_index")]
    Last,
    /// The first cell whose key is this rowid or more.
    Key(i64),
}

pub(crate) fn fetch(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    page: u32,
    keep: bool,
) -> Sync<usize> {
    Ok(match core.get(page, pins, keep)? {
        Got::Hit(i) => Ok(i),
        Got::Miss(io) => Err(io),
    })
}

/// Descend from `page` to a leaf, pushing the interior pages passed
/// onto `levels`: the leaf and the index of the cell `aim` picks there
/// (`Last`: the cell count; `Key`: the first cell at or past it, or
/// the cell count).
fn descend(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    mut page: u32,
    aim: Aim,
    levels: &mut AVec<Level>,
) -> Sync<(Leaf, usize)> {
    loop {
        if levels.len() > MAX_DEPTH {
            return Err(Error::CorruptPage { page });
        }
        let idx = ready!(fetch(core, pins, page, true));
        let usable = core.usable() as usize;
        let bytes = core.data(idx);
        let h = btree::head::<()>(bytes, page)?;
        if h.is_leaf() {
            if h.kind != btree::KIND_LEAF_TABLE {
                return Err(Error::Unsupported);
            }
            let mut img = AVec::with_capacity(bytes.len());
            img.extend_from_slice(bytes);
            let cells = btree::leaf_cells_meta::<()>(img.as_slice(), page, usable)?;
            let at = match aim {
                Aim::First => 0,
                #[cfg(feature = "expr_index")]
                Aim::Last => cells.len(),
                Aim::Key(k) => cells.as_slice().partition_point(|c| c.rowid < k),
            };
            return Ok(Ok((Leaf { img, cells }, at)));
        }
        let (pairs, rightmost) = btree::interior_cells::<()>(bytes, page)?;
        let mut kids = AVec::with_capacity(pairs.len().saturating_add(1));
        let mut keys = AVec::with_capacity(pairs.len());
        for (c, k) in pairs.iter() {
            kids.push(*c);
            keys.push(*k);
        }
        kids.push(rightmost);
        let at = match aim {
            Aim::First => 0,
            #[cfg(feature = "expr_index")]
            Aim::Last => keys.len(),
            Aim::Key(k) => keys.as_slice().partition_point(|x| *x < k),
        };
        let Some(&child) = kids.as_slice().get(at) else {
            return Err(Error::CorruptPage { page });
        };
        levels.push(Level { kids, at });
        page = child;
    }
}

/// The first non-empty leaf after the one `levels` leads to, and its
/// first cell; `None` at the end of the tree.
fn next_leaf(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    levels: &mut AVec<Level>,
) -> Sync<Option<(Leaf, usize)>> {
    loop {
        let Some(top) = levels.as_mut_slice().last_mut() else {
            return Ok(Ok(None));
        };
        let nxt = top.at.saturating_add(1);
        let Some(&child) = top.kids.as_slice().get(nxt) else {
            levels.pop();
            continue;
        };
        top.at = nxt;
        let (leaf, _) = ready!(descend(core, pins, child, Aim::First, levels));
        if !leaf.cells.is_empty() {
            return Ok(Ok(Some((leaf, 0))));
        }
    }
}

/// The last non-empty leaf before the one `levels` leads to, and its
/// last cell; `None` at the start of the tree.
#[cfg(feature = "expr_index")]
fn prev_leaf(
    core: &mut PagerCore<'_>,
    pins: &mut AVec<u32>,
    levels: &mut AVec<Level>,
) -> Sync<Option<(Leaf, usize)>> {
    loop {
        let Some(top) = levels.as_mut_slice().last_mut() else {
            return Ok(Ok(None));
        };
        let Some(prv) = top.at.checked_sub(1) else {
            levels.pop();
            continue;
        };
        let Some(&child) = top.kids.as_slice().get(prv) else {
            levels.pop();
            continue;
        };
        top.at = prv;
        let (leaf, n) = ready!(descend(core, pins, child, Aim::Last, levels));
        if let Some(last) = n.checked_sub(1) {
            return Ok(Ok(Some((leaf, last))));
        }
    }
}

impl TableCursor {
    pub(crate) fn new(root: u32, info: u32) -> TableCursor {
        TableCursor {
            root,
            info,
            levels: AVec::new(),
            leaf: Leaf::default(),
            at: 0,
            valid: false,
            row: RowData::empty(),
            decoded: false,
            pay: Payload::default(),
        }
    }

    /// Commit a new position (or the end, `None`); whether there is a
    /// row.
    fn commit(&mut self, levels: AVec<Level>, at: Option<(Leaf, usize)>) -> bool {
        self.levels = levels;
        self.decoded = false;
        self.pay.started = false;
        self.pay.buf.clear();
        match at {
            Some((leaf, i)) => {
                self.leaf = leaf;
                self.at = i;
                self.valid = true;
            }
            None => {
                self.leaf = Leaf::default();
                self.at = 0;
                self.valid = false;
            }
        }
        self.valid
    }

    /// Move within the current leaf (no I/O).
    fn step_to(&mut self, i: usize) -> bool {
        self.at = i;
        self.decoded = false;
        self.pay.started = false;
        self.pay.buf.clear();
        true
    }

    /// The first row; false for an empty table.
    pub(crate) fn rewind(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        let mut lv = AVec::new();
        let (leaf, _) = ready!(descend(core, pins, self.root, Aim::First, &mut lv));
        let at = if leaf.cells.is_empty() {
            ready!(next_leaf(core, pins, &mut lv))
        } else {
            Some((leaf, 0))
        };
        Ok(Ok(self.commit(lv, at)))
    }

    /// The last row; false for an empty table.
    #[cfg(feature = "expr_index")]
    pub(crate) fn last(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        let mut lv = AVec::new();
        let (leaf, n) = ready!(descend(core, pins, self.root, Aim::Last, &mut lv));
        let at = match n.checked_sub(1) {
            Some(i) => Some((leaf, i)),
            None => ready!(prev_leaf(core, pins, &mut lv)),
        };
        Ok(Ok(self.commit(lv, at)))
    }

    /// The next row; false past the last one.
    pub(crate) fn next(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        if !self.valid {
            return Ok(Ok(false));
        }
        let nxt = self.at.saturating_add(1);
        if nxt < self.leaf.cells.len() {
            return Ok(Ok(self.step_to(nxt)));
        }
        let mut lv = self.levels.clone();
        let at = ready!(next_leaf(core, pins, &mut lv));
        Ok(Ok(self.commit(lv, at)))
    }

    /// The previous row; false before the first one.
    #[cfg(feature = "expr_index")]
    pub(crate) fn prev(&mut self, core: &mut PagerCore<'_>, pins: &mut AVec<u32>) -> Sync<bool> {
        if !self.valid {
            return Ok(Ok(false));
        }
        if let Some(p) = self.at.checked_sub(1) {
            return Ok(Ok(self.step_to(p)));
        }
        let mut lv = self.levels.clone();
        let at = ready!(prev_leaf(core, pins, &mut lv));
        Ok(Ok(self.commit(lv, at)))
    }

    /// The row whose rowid is `k`; false (and no row) when there is
    /// none.
    pub(crate) fn seek_rowid(
        &mut self,
        core: &mut PagerCore<'_>,
        pins: &mut AVec<u32>,
        k: i64,
    ) -> Sync<bool> {
        let mut lv = AVec::new();
        let (leaf, j) = ready!(descend(core, pins, self.root, Aim::Key(k), &mut lv));
        let hit = leaf.cells.as_slice().get(j).is_some_and(|c| c.rowid == k);
        Ok(Ok(self.commit(lv, if hit { Some((leaf, j)) } else { None })))
    }

    /// The first row whose rowid is `k` or more.
    #[cfg(feature = "expr_index")]
    pub(crate) fn seek_ge(
        &mut self,
        core: &mut PagerCore<'_>,
        pins: &mut AVec<u32>,
        k: i64,
    ) -> Sync<bool> {
        let mut lv = AVec::new();
        let (leaf, j) = ready!(descend(core, pins, self.root, Aim::Key(k), &mut lv));
        let at = if j < leaf.cells.len() {
            Some((leaf, j))
        } else {
            ready!(next_leaf(core, pins, &mut lv))
        };
        Ok(Ok(self.commit(lv, at)))
    }

    /// The last row whose rowid is `k` or less.
    #[cfg(feature = "expr_index")]
    pub(crate) fn seek_le(
        &mut self,
        core: &mut PagerCore<'_>,
        pins: &mut AVec<u32>,
        k: i64,
    ) -> Sync<bool> {
        let mut lv = AVec::new();
        let (leaf, j) = ready!(descend(core, pins, self.root, Aim::Key(k), &mut lv));
        let at = if leaf.cells.as_slice().get(j).is_some_and(|c| c.rowid == k) {
            Some((leaf, j))
        } else if let Some(p) = j.checked_sub(1) {
            Some((leaf, p))
        } else {
            ready!(prev_leaf(core, pins, &mut lv))
        };
        Ok(Ok(self.commit(lv, at)))
    }

    /// The current row's rowid.
    pub(crate) fn rowid(&self) -> Option<i64> {
        if !self.valid {
            return None;
        }
        self.leaf.cells.as_slice().get(self.at).map(|c| c.rowid)
    }

    /// Decode the current row into `row` (once per position): its
    /// payload from the leaf snapshot and the overflow chain, then
    /// the record as `info` lays it out.
    pub(crate) fn load(
        &mut self,
        core: &mut PagerCore<'_>,
        pins: &mut AVec<u32>,
        info: &TableInfo,
    ) -> Sync<()> {
        if self.decoded {
            return Ok(Ok(()));
        }
        let cell = match self.leaf.cells.as_slice().get(self.at) {
            Some(c) if self.valid => *c,
            _ => return Err(Error::Unsupported),
        };
        if !self.pay.started {
            let local = cell
                .local_off
                .checked_add(cell.local_len)
                .and_then(|end| self.leaf.img.as_slice().get(cell.local_off..end))
                .ok_or(Error::CorruptRecord)?;
            self.pay.buf.clear();
            self.pay.buf.extend_from_slice(local);
            self.pay.next = cell.overflow;
            self.pay.hops = 0;
            self.pay.started = true;
        }
        let per_page = (core.usable() as usize).saturating_sub(4);
        while self.pay.buf.len() < cell.payload_len {
            let page = self.pay.next;
            if page == 0 {
                return Err(Error::CorruptRecord);
            }
            let hops = self.pay.hops.saturating_add(1);
            if hops > core.page_count().saturating_add(1) {
                return Err(Error::OverflowChainCycle);
            }
            let idx = ready!(fetch(core, pins, page, false));
            let d = core.data(idx);
            let following = match d.get(..4) {
                Some(_) => be_u32(d, 0),
                None => return Err(Error::CorruptPage { page }),
            };
            let take = cell
                .payload_len
                .saturating_sub(self.pay.buf.len())
                .min(per_page);
            let chunk = d
                .get(4..4usize.saturating_add(take))
                .ok_or(Error::CorruptPage { page })?;
            self.pay.buf.extend_from_slice(chunk);
            self.pay.next = following;
            self.pay.hops = hops;
        }
        // A UTF-16 database's record reads as UTF-8 above the B-tree
        // (`btree::assemble_payload_into`).
        #[cfg(feature = "utf16")]
        if let Some(be) = crate::utf16::byte_order(core.text_enc()) {
            let raw = core::mem::take(&mut self.pay.buf);
            crate::utf16::transcode_record::<()>(raw.as_slice(), be, false, &mut self.pay.buf)?;
        }
        decode_row(
            info,
            self.pay.buf.as_slice(),
            cell.rowid,
            &mut self.row.vals,
        )?;
        self.row.rowids.clear();
        self.row.rowids.push(Some(cell.rowid));
        self.decoded = true;
        Ok(Ok(()))
    }
}

/// A table row's values in query-row form (the query executor's
/// `decode_source_row`): the rowid alias reads as the rowid, a field
/// past the record's end takes its column's default, REAL columns
/// force reals, generated columns are computed.
fn decode_row(
    t: &TableInfo,
    payload: &[u8],
    rowid: i64,
    out: &mut AVec<OwnedValue>,
) -> Result<(), Error<()>> {
    out.clear();
    if let Some(g) = &t.gcols {
        let mut rec = crate::rules::decode_rec::<()>(
            payload,
            t.ncols,
            t.defaults.as_slice(),
            Some(g.virt.as_slice()),
        )?;
        g.fill::<()>(rec.as_mut_slice(), rowid, true)?;
        let rd = crate::rules::eval_row(t.rowid_alias, t.affs.as_slice(), rec.as_slice(), rowid);
        for v in rd.vals {
            out.push(v);
        }
        return Ok(());
    }
    let mut dec: AVec<Value<'_>> = AVec::new();
    crate::record::decode_record_into::<()>(payload, t.ncols, &mut dec)?;
    for i in 0..t.ncols {
        let mut v = if Some(i) == t.rowid_alias {
            OwnedValue::Integer(rowid)
        } else if let Some(d) = dec.as_slice().get(i) {
            OwnedValue::from_value(*d)
        } else {
            t.defaults
                .as_slice()
                .get(i)
                .cloned()
                .unwrap_or(OwnedValue::Null)
        };
        if t.affs.as_slice().get(i) == Some(&Affinity::Real)
            && let OwnedValue::Integer(x) = v
        {
            v = OwnedValue::Real(x as f64);
        }
        out.push(v);
    }
    Ok(())
}

/// One sorter row: the result values, the ORDER BY keys, the rowid.
struct SRow {
    out: AVec<OwnedValue>,
    keys: AVec<OwnedValue>,
    rowid: Option<i64>,
}

/// An ORDER BY sorter (in memory, as the materialising executor's
/// sort): rows are kept as they come and sorted stably with the
/// executor's comparator (`query::order_keys_cmp`), so equal rows
/// keep their arrival order, as on the pin.
pub(crate) struct Sorter {
    /// The `P4::Sorter` entry it was opened with.
    pub(crate) info: u32,
    rows: AVec<SRow>,
    perm: AVec<usize>,
    at: usize,
    /// The rowid of the row `data` last read.
    pub(crate) rowid: Option<i64>,
}

fn rows_cmp(
    x: &SRow,
    y: &SRow,
    orders: &[SortOrder],
    enc: crate::record::TextEnc,
) -> core::cmp::Ordering {
    crate::query::order_keys_cmp(
        x.keys.as_slice(),
        y.keys.as_slice(),
        orders.iter().copied(),
        enc,
    )
}

impl Sorter {
    pub(crate) fn new(info: u32) -> Sorter {
        Sorter {
            info,
            rows: AVec::new(),
            perm: AVec::new(),
            at: 0,
            rowid: None,
        }
    }

    /// Add one row from `vals` (result values, then keys). With a
    /// row window `(skip, take)`, the sorter keeps only the rows the
    /// window may still return once it holds twice as many and more
    /// (the streamed executor's bound): sorted stably first, so the
    /// earliest of equal rows stay.
    pub(crate) fn insert(
        &mut self,
        info: &SortInfo,
        vals: &[OwnedValue],
        rowid: Option<i64>,
        window: Option<(usize, usize)>,
        enc: crate::record::TextEnc,
    ) {
        let n = info.nout.min(vals.len());
        let (o, k) = vals.split_at(n);
        let mut out = AVec::with_capacity(o.len());
        out.extend_from_slice(o);
        let mut keys = AVec::with_capacity(k.len());
        keys.extend_from_slice(k);
        self.rows.push(SRow { out, keys, rowid });
        if let Some((skip, take)) = window {
            let keep = skip.saturating_add(take);
            if self.rows.len() >= keep.saturating_mul(2).saturating_add(64) {
                let orders = info.orders.as_slice();
                crate::arena::sort_by(self.rows.as_mut_slice(), &mut |x, y| {
                    rows_cmp(x, y, orders, enc)
                });
                self.rows.truncate(keep);
            }
        }
    }

    /// Sort the rows (stable); false when there are none.
    pub(crate) fn sort(&mut self, info: &SortInfo, enc: crate::record::TextEnc) -> bool {
        let rows = self.rows.as_slice();
        let orders = info.orders.as_slice();
        self.perm =
            crate::arena::sort_perm(rows.len(), &mut |a, b| match (rows.get(a), rows.get(b)) {
                (Some(x), Some(y)) => rows_cmp(x, y, orders, enc),
                _ => core::cmp::Ordering::Equal,
            });
        self.at = 0;
        !self.perm.is_empty()
    }

    /// The current row's result values into `dest` (moved out: each
    /// row is read once).
    pub(crate) fn data(&mut self, dest: &mut [OwnedValue]) {
        let row = self
            .perm
            .as_slice()
            .get(self.at)
            .and_then(|&i| self.rows.as_mut_slice().get_mut(i));
        if let Some(r) = row {
            self.rowid = r.rowid;
            for (d, v) in dest.iter_mut().zip(r.out.as_mut_slice().iter_mut()) {
                *d = core::mem::replace(v, OwnedValue::Null);
            }
        }
    }

    /// The next row; false past the last one.
    pub(crate) fn next(&mut self) -> bool {
        self.at = self.at.saturating_add(1);
        self.at < self.perm.len()
    }
}
