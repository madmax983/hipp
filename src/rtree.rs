//! The R*Tree module (v0.16, leaf `rtree`): the pin's `rtree.c`
//! (`rtree` with 32-bit float coordinates, `rtree_i32` with 32-bit
//! integers; 1 to 5 dimensions; auxiliary `+columns`), with its
//! on-disk shadow tables byte for byte:
//!
//! - `%_node(nodeno INTEGER PRIMARY KEY, data)`: each node a blob of
//!   the table's node size (the page size less 64 at CREATE, at most
//!   51 cells; the size of node 1's blob afterwards): a 2-byte depth
//!   (node 1 only), a 2-byte cell count, then cells of an 8-byte
//!   rowid / child node number and the coordinates (4 bytes each),
//!   all big-endian. Deleting a cell moves the later cells down and
//!   leaves the last cell's old bytes behind, as the pin does.
//! - `%_parent(nodeno INTEGER PRIMARY KEY, parentnode)` for every
//!   node but the root, `%_rowid(rowid INTEGER PRIMARY KEY, nodeno,
//!   a0, a1, ...)` for every entry (its leaf, then its auxiliary
//!   values).
//!
//! The tree code is the pin's: ChooseLeaf (a containing cell first,
//! the smallest; else least growth, then least area), AdjustTree,
//! the R*-tree split (`splitNodeStartree`: per dimension, cells
//! sorted by lower then upper bound, the least total margin's
//! dimension, the least overlap then area split; root splits number
//! the right node first; no forced reinsertion: the pin's tree
//! splits every full node), and delete
//! with the removal of under-full nodes and reinsertion of their
//! cells, latest removed first. New nodes take the next `nodeno`
//! after the largest. Coordinates round outward to float32 as
//! `rtreeValueDown` / `rtreeValueUp` (one ulp further when the
//! nearest float lies inside), integers clamp to 32 bits.
//!
//! Each write runs as a pure transaction over what the shadow tables
//! hold ([`Store`], filled lazily by the caller: a [`Miss`] names
//! the row it needs and the write is run again), and its result
//! ([`Writes`]) is applied by the caller through the pager — in the
//! statement's own transaction, so journals, savepoints and crash
//! recovery cover it as any table write.

use crate::arena::AVec;
use crate::error::SchemaKind;
use crate::record::OwnedValue;
use crate::vtab::{Cons, IndexInfo, Op, Usage, VCol, VTab};

/// The shadow-table suffixes (`rtreeShadowName`).
pub const SHADOWS: [&[u8]; 3] = [b"node", b"parent", b"rowid"];

const MAX_CELLS: usize = 51;
const MAX_DEPTH: i32 = 40;
const MAX_AUX: usize = 100;

/// `rtreeInit`'s column declaration: `id INT`, each coordinate
/// `REAL` (`rtree`) or `INT` (`rtree_i32`), auxiliary columns
/// untyped; names are each argument's first token. The pin's errors
/// for a wrong count or order of columns.
pub fn declare(tab: &mut VTab, args: &[&[u8]], int: bool) -> Result<(), SchemaKind> {
    if args.len() < 3 {
        return Err(SchemaKind::RtreeTooFewColumns);
    }
    if args.len() > MAX_AUX + 3 {
        return Err(SchemaKind::RtreeTooManyColumns);
    }
    let first = |a: &[u8]| -> AVec<u8> {
        let mut out = AVec::new();
        out.extend_from_slice(crate::record::dequote_q(first_token(a)).0);
        out
    };
    let push = |tab: &mut VTab, n: AVec<u8>, decl: &'static [u8]| {
        tab.cols.push(VCol {
            name: n,
            decl,
            hidden: false,
        });
    };
    if let Some(a) = args.first() {
        push(tab, first(a), b"INT");
    }
    let mut ndim2 = 0usize;
    let mut naux = 0usize;
    let mut i = 1;
    while let Some(a) = args.get(i) {
        let a = a.trim_ascii();
        if let Some(rest) = a.strip_prefix(b"+") {
            naux += 1;
            push(tab, first(rest), b"");
        } else if naux > 0 {
            return Err(SchemaKind::RtreeAuxNotLast);
        } else {
            ndim2 += 1;
            push(tab, first(a), if int { b"INT" } else { b"REAL" });
        }
        i += 1;
    }
    if ndim2 < 2 {
        return Err(SchemaKind::RtreeTooFewColumns);
    }
    if ndim2 > 10 {
        return Err(SchemaKind::RtreeTooManyColumns);
    }
    if !ndim2.is_multiple_of(2) {
        return Err(SchemaKind::RtreeWrongColumns);
    }
    tab.ndim2 = ndim2 as u8;
    tab.naux = naux as u8;
    Ok(())
}

/// The first SQL token of `a` (`rtreeTokenLength`): a quoted name,
/// or a run of identifier characters (one byte otherwise).
fn first_token(a: &[u8]) -> &[u8] {
    let a = a.trim_ascii_start();
    let Some(&c) = a.first() else {
        return a;
    };
    let close = match c {
        b'"' | b'\'' | b'`' => c,
        b'[' => b']',
        _ => 0,
    };
    if close != 0 {
        let mut i = 1;
        while let Some(&b) = a.get(i) {
            if b == close {
                if close != b']' && a.get(i + 1) == Some(&close) {
                    i += 2;
                    continue;
                }
                return a.get(..i + 1).unwrap_or(a);
            }
            i += 1;
        }
        return a;
    }
    let n = a
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'$' || **b >= 0x80)
        .count()
        .max(1);
    a.get(..n).unwrap_or(a)
}

/// `rtreeBestIndex`: an equality on the rowid (or `id`) is a lookup
/// (strategy 1, omitted); otherwise each `=`, `<`, `<=`, `>`, `>=`
/// on a coordinate (`<=` and `>=` omitted) joins the search, as an
/// op / coordinate pair of `idxStr`.
pub fn best_index(tab: &VTab, info: &mut IndexInfo) {
    let ncons = info.cons.len();
    for ii in 0..ncons {
        let Some(&Cons { col, op, usable }) = info.cons.as_slice().get(ii) else {
            break;
        };
        if usable && col <= 0 && op == Op::Eq {
            for u in info.usage.as_mut_slice().iter_mut() {
                *u = Usage::default();
            }
            if let Some(u) = info.usage.as_mut_slice().get_mut(ii) {
                *u = Usage {
                    argv: 1,
                    omit: true,
                };
            }
            info.idx_num = 1;
            info.idx_str.clear();
            info.cost = 30.0;
            info.rows = 1;
            return;
        }
        if info.idx_str.len() >= 40 {
            continue;
        }
        if usable && col > 0 && col <= i32::from(tab.ndim2) {
            let (code, omit) = match op {
                Op::Eq => (b'A', false),
                Op::Gt => (b'E', false),
                Op::Le => (b'B', true),
                Op::Lt => (b'C', false),
                Op::Ge => (b'D', true),
                Op::Is => continue,
            };
            info.idx_str.push(code);
            info.idx_str.push(b'0' + (col - 1) as u8);
            let argv = (info.idx_str.len() / 2) as u16;
            if let Some(u) = info.usage.as_mut_slice().get_mut(ii) {
                *u = Usage { argv, omit };
            }
        }
    }
    info.idx_num = 2;
    let est: i64 = 1_048_576;
    info.rows = est >> (info.idx_str.len() / 2).min(62);
    info.cost = 6.0 * info.rows as f64;
}

// ---------------------------------------------------------------
// Geometry and node images
// ---------------------------------------------------------------

/// A table's shape.
#[derive(Debug, Clone, Copy)]
pub struct Geom {
    pub ndim2: usize,
    pub int: bool,
    pub node_size: usize,
}

impl Geom {
    /// Bytes per cell.
    pub fn bpc(&self) -> usize {
        8 + 4 * self.ndim2
    }
    pub fn max_cells(&self) -> usize {
        self.node_size.saturating_sub(4) / self.bpc().max(1)
    }
    pub fn min_cells(&self) -> usize {
        self.max_cells() / 3
    }
    /// A coordinate as a double (`DCOORD`).
    fn d(&self, c: u32) -> f64 {
        if self.int {
            f64::from(c as i32)
        } else {
            f64::from(f32::from_bits(c))
        }
    }
    /// The node size a new table gets on a file of `page_size`
    /// (`getNodeSize` at xCreate).
    pub fn create_size(ndim2: usize, page_size: u32) -> usize {
        let n = (page_size as usize).saturating_sub(64);
        n.min(4 + (8 + 4 * ndim2) * MAX_CELLS)
    }
}

/// One cell: a rowid (leaf) or child node number, and raw coordinate
/// bits.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cell {
    pub rowid: i64,
    pub c: [u32; 10],
}

fn rd16(d: &[u8], at: usize) -> usize {
    match d.get(at..at + 2) {
        Some(&[a, b]) => usize::from(a) << 8 | usize::from(b),
        _ => 0,
    }
}

fn wr16(d: &mut [u8], at: usize, v: usize) {
    if let Some(s) = d.get_mut(at..at + 2) {
        s.copy_from_slice(&(v as u16).to_be_bytes());
    }
}

fn rd32(d: &[u8], at: usize) -> u32 {
    match d.get(at..at + 4) {
        Some(&[a, b, c, e]) => u32::from_be_bytes([a, b, c, e]),
        _ => 0,
    }
}

fn rd64(d: &[u8], at: usize) -> i64 {
    let mut b = [0u8; 8];
    if let Some(s) = d.get(at..at + 8) {
        b.copy_from_slice(s);
    }
    i64::from_be_bytes(b)
}

/// Cells in a node image.
pub fn ncell(d: &[u8]) -> usize {
    rd16(d, 2)
}

/// The depth a root image records.
pub fn depth_of(d: &[u8]) -> i32 {
    rd16(d, 0) as i32
}

fn get_cell(g: &Geom, d: &[u8], i: usize) -> Cell {
    let off = 4 + g.bpc() * i;
    let mut c = Cell {
        rowid: rd64(d, off),
        c: [0; 10],
    };
    for (k, slot) in c.c.iter_mut().enumerate().take(g.ndim2) {
        *slot = rd32(d, off + 8 + 4 * k);
    }
    c
}

fn put_cell(g: &Geom, d: &mut [u8], cell: &Cell, i: usize) {
    let off = 4 + g.bpc() * i;
    if let Some(s) = d.get_mut(off..off + 8) {
        s.copy_from_slice(&cell.rowid.to_be_bytes());
    }
    for k in 0..g.ndim2 {
        let at = off + 8 + 4 * k;
        if let (Some(s), Some(v)) = (d.get_mut(at..at + 4), cell.c.get(k)) {
            s.copy_from_slice(&v.to_be_bytes());
        }
    }
}

/// `nodeInsertCell`: true when the node is full (nothing written).
fn node_insert(g: &Geom, d: &mut [u8], cell: &Cell) -> bool {
    let n = ncell(d);
    if n < g.max_cells() {
        put_cell(g, d, cell, n);
        wr16(d, 2, n + 1);
        false
    } else {
        true
    }
}

/// `nodeDeleteCell`: later cells move down; the old last cell's
/// bytes stay.
fn node_delete(g: &Geom, d: &mut [u8], i: usize) {
    let n = ncell(d);
    let bpc = g.bpc();
    let dst = 4 + bpc * i;
    let src = dst + bpc;
    let len = n.saturating_sub(i + 1) * bpc;
    if src + len <= d.len() {
        d.copy_within(src..src + len, dst);
    }
    wr16(d, 2, n.saturating_sub(1));
}

/// `nodeZero`: everything but the depth.
fn node_zero(d: &mut [u8]) {
    for b in d.iter_mut().skip(2) {
        *b = 0;
    }
}

fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// `cellUnion`.
fn union(g: &Geom, a: &mut Cell, b: &Cell) {
    let mut i = 0;
    while i + 1 < 10 && i < g.ndim2 {
        let (lo, hi) = (a.c[i], a.c[i + 1]);
        let (blo, bhi) = (b.c[i], b.c[i + 1]);
        if g.int {
            let (x, y) = (lo as i32, blo as i32);
            a.c[i] = (if x < y { x } else { y }) as u32;
            let (x, y) = (hi as i32, bhi as i32);
            a.c[i + 1] = (if x > y { x } else { y }) as u32;
        } else {
            a.c[i] = fmin(f32::from_bits(lo), f32::from_bits(blo)).to_bits();
            a.c[i + 1] = fmax(f32::from_bits(hi), f32::from_bits(bhi)).to_bits();
        }
        i += 2;
    }
}

/// `cellContains`: `b` lies inside `a`.
fn contains(g: &Geom, a: &Cell, b: &Cell) -> bool {
    let mut i = 0;
    while i + 1 < 10 && i < g.ndim2 {
        let out = if g.int {
            (b.c[i] as i32) < (a.c[i] as i32) || (b.c[i + 1] as i32) > (a.c[i + 1] as i32)
        } else {
            f32::from_bits(b.c[i]) < f32::from_bits(a.c[i])
                || f32::from_bits(b.c[i + 1]) > f32::from_bits(a.c[i + 1])
        };
        if out {
            return false;
        }
        i += 2;
    }
    true
}

/// `cellArea`: float differences taken in float (int: in 64 bits),
/// the product in double, highest dimension first.
fn area(g: &Geom, p: &Cell) -> f64 {
    let mut a = 1.0f64;
    let mut d = g.ndim2;
    while d >= 2 {
        d -= 2;
        let (lo, hi) = (p.c[d], p.c[d + 1]);
        let diff = if g.int {
            (i64::from(hi as i32) - i64::from(lo as i32)) as f64
        } else {
            f64::from(f32::from_bits(hi) - f32::from_bits(lo))
        };
        a *= diff;
    }
    a
}

/// `cellMargin`.
fn margin(g: &Geom, p: &Cell) -> f64 {
    let mut m = 0.0f64;
    let mut d = g.ndim2;
    while d >= 2 {
        d -= 2;
        m += g.d(p.c[d + 1]) - g.d(p.c[d]);
    }
    m
}

/// `cellOverlap` of `p` with one cell.
fn overlap(g: &Geom, p: &Cell, q: &Cell) -> f64 {
    let mut o = 1.0f64;
    let mut j = 0;
    while j + 1 < 10 && j < g.ndim2 {
        let x1 = {
            let (a, b) = (g.d(p.c[j]), g.d(q.c[j]));
            if a > b { a } else { b }
        };
        let x2 = {
            let (a, b) = (g.d(p.c[j + 1]), g.d(q.c[j + 1]));
            if a < b { a } else { b }
        };
        if x2 < x1 {
            o = 0.0;
            break;
        }
        o *= x2 - x1;
        j += 2;
    }
    o
}

/// `SortByDimension`: by lower bound, then upper bound.
fn sort_by_dim(g: &Geom, idx: &mut [usize], dim: usize, cells: &[Cell], spare: &mut [usize]) {
    let n = idx.len();
    if n < 2 {
        return;
    }
    let nl = n / 2;
    {
        let (l, r) = idx.split_at_mut(nl);
        sort_by_dim(g, l, dim, cells, spare);
        sort_by_dim(g, r, dim, cells, spare);
    }
    let Some(sp) = spare.get_mut(..nl) else {
        return;
    };
    sp.copy_from_slice(idx.get(..nl).unwrap_or(&[]));
    let (mut il, mut ir) = (0usize, 0usize);
    let nr = n - nl;
    let co = |c: usize, k: usize| cells.get(c).map_or(0.0, |x| g.d(x.c[(2 * dim + k).min(9)]));
    while il < nl || ir < nr {
        let take_left = il != nl && {
            ir == nr || {
                let (l1, l2) = (co(sp[il], 0), co(sp[il], 1));
                let (r1, r2) = (co(idx[nl + ir], 0), co(idx[nl + ir], 1));
                l1 < r1 || (l1 == r1 && l2 < r2)
            }
        };
        if take_left {
            idx[il + ir] = sp[il];
            il += 1;
        } else {
            idx[il + ir] = idx[nl + ir];
            ir += 1;
        }
    }
}

// ---------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------

/// The float just below / above `f` (`f` finite or infinite).
fn step_f32(f: f32, up: bool) -> f32 {
    if f.is_nan() {
        return f;
    }
    let b = f.to_bits();
    let r = if f == 0.0 {
        if up { 1 } else { 0x8000_0001 }
    } else if (f > 0.0) == up {
        b.wrapping_add(1)
    } else {
        b.wrapping_sub(1)
    };
    f32::from_bits(r)
}

/// `rtreeValueDown`: the nearest float, one ulp lower when that is
/// above the value.
pub fn value_down(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) > d {
        step_f32(f, false)
    } else {
        f
    }
}

/// `rtreeValueUp`.
pub fn value_up(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) < d {
        step_f32(f, true)
    } else {
        f
    }
}

/// An `rtree_i32` coordinate: the 64-bit integer value clamped to 32
/// bits.
fn value_i32(v: &OwnedValue) -> i32 {
    let i = crate::func::int64_value(v);
    i.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// A new row's coordinates (`aData[3..]`), rounded as the pin
/// stores them; `Err(k)`: pair `k` (0-based) has its lower bound
/// above its upper ("rtree constraint failed").
pub fn coords(g: &Geom, vals: &[OwnedValue]) -> Result<[u32; 10], usize> {
    let mut c = [0u32; 10];
    let mut i = 0;
    while i + 1 < 10 && i < g.ndim2 {
        let lo = vals.get(i).unwrap_or(&OwnedValue::Null);
        let hi = vals.get(i + 1).unwrap_or(&OwnedValue::Null);
        if g.int {
            let (a, b) = (value_i32(lo), value_i32(hi));
            if a > b {
                return Err(i / 2);
            }
            c[i] = a as u32;
            c[i + 1] = b as u32;
        } else {
            let a = value_down(crate::func::real_value(lo));
            let b = value_up(crate::func::real_value(hi));
            if a > b {
                return Err(i / 2);
            }
            c[i] = a.to_bits();
            c[i + 1] = b.to_bits();
        }
        i += 2;
    }
    Ok(c)
}

/// A coordinate as the module's xColumn returns it.
pub fn coord_value(g: &Geom, c: u32) -> OwnedValue {
    if g.int {
        OwnedValue::Integer(i64::from(c as i32))
    } else {
        OwnedValue::Real(f64::from(f32::from_bits(c)))
    }
}

// ---------------------------------------------------------------
// The shadow tables: what is known, what a write changes
// ---------------------------------------------------------------

/// A row the current transaction needs from a shadow table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// `%_node` row.
    Node(i64),
    /// `%_parent` row.
    Parent(i64),
    /// `%_rowid` row.
    Rowid(i64),
    /// The largest `%_node` key (a new node is numbered after it).
    NodeMax,
    /// The largest `%_rowid` key (a new rowid follows it).
    RowidMax,
    /// Every `%_node` key (the largest was deleted).
    NodeKeys,
    /// Every `%_rowid` key.
    RowidKeys,
}

/// Why a write stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    Miss(Miss),
    /// The shadow tables are inconsistent (`SQLITE_CORRUPT_VTAB`).
    Corrupt,
    /// The new rowid exists (`UNIQUE constraint failed: t.id`).
    Unique,
}

type R<T> = Result<T, Fail>;

fn find<V: Clone>(m: &AVec<(i64, V)>, k: i64) -> Option<V> {
    m.as_slice()
        .binary_search_by_key(&k, |e| e.0)
        .ok()
        .and_then(|i| m.as_slice().get(i))
        .map(|e| e.1.clone())
}

fn put<V>(m: &mut AVec<(i64, V)>, k: i64, v: V) {
    match m.as_slice().binary_search_by_key(&k, |e| e.0) {
        Ok(i) => {
            if let Some(e) = m.as_mut_slice().get_mut(i) {
                e.1 = v;
            }
        }
        Err(i) => m.insert_at(i, (k, v)),
    }
}

/// The shadow-table rows read so far (`None`: known absent).
#[derive(Debug, Clone, Default)]
pub struct Store {
    pub nodes: AVec<(i64, Option<AVec<u8>>)>,
    pub parents: AVec<(i64, Option<i64>)>,
    pub rowids: AVec<(i64, Option<i64>)>,
    /// The largest `%_node` / `%_rowid` key (`Some(None)`: empty
    /// table; `None`: not read yet).
    pub node_max: Option<Option<i64>>,
    pub rowid_max: Option<Option<i64>>,
    /// Every key, read only when the largest was deleted.
    pub node_keys: Option<AVec<i64>>,
    pub rowid_keys: Option<AVec<i64>>,
}

impl Store {
    pub fn add_node(&mut self, k: i64, v: Option<AVec<u8>>) {
        put(&mut self.nodes, k, v);
    }
    pub fn add_parent(&mut self, k: i64, v: Option<i64>) {
        put(&mut self.parents, k, v);
    }
    pub fn add_rowid(&mut self, k: i64, v: Option<i64>) {
        put(&mut self.rowids, k, v);
    }

    /// Take a write's changes as the new known state.
    pub fn absorb(&mut self, w: &Writes) {
        fn bump(max: &mut Option<Option<i64>>, k: i64, live: bool) {
            match (*max, live) {
                (Some(m), true) if m.is_none_or(|m| k > m) => *max = Some(Some(k)),
                (Some(Some(m)), false) if m == k => *max = None,
                _ => {}
            }
        }
        for (k, v) in w.nodes.iter() {
            bump(&mut self.node_max, *k, v.is_some());
        }
        for (k, v) in w.rowids.iter() {
            bump(&mut self.rowid_max, *k, v.is_some());
        }
        for (k, v) in w.nodes.iter() {
            put(&mut self.nodes, *k, v.clone());
            if let Some(keys) = self.node_keys.as_mut() {
                let at = keys.as_slice().binary_search(k);
                match (at, v.is_some()) {
                    (Err(i), true) => keys.insert_at(i, *k),
                    (Ok(i), false) => {
                        keys.remove_at(i);
                    }
                    _ => {}
                }
            }
        }
        for (k, v) in w.parents.iter() {
            put(&mut self.parents, *k, *v);
        }
        for (k, v) in w.rowids.iter() {
            put(&mut self.rowids, *k, *v);
            if let Some(keys) = self.rowid_keys.as_mut() {
                let at = keys.as_slice().binary_search(k);
                match (at, v.is_some()) {
                    (Err(i), true) => keys.insert_at(i, *k),
                    (Ok(i), false) => {
                        keys.remove_at(i);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// What one write changes, by key (`None`: the row is deleted).
#[derive(Debug, Clone, Default)]
pub struct Writes {
    pub nodes: AVec<(i64, Option<AVec<u8>>)>,
    pub parents: AVec<(i64, Option<i64>)>,
    pub rowids: AVec<(i64, Option<i64>)>,
    /// The inserted row's auxiliary values (its `%_rowid` row then
    /// takes them).
    pub aux: Option<(i64, AVec<OwnedValue>)>,
    /// The key of the last shadow row written: the pin writes them
    /// with INSERT statements, which leave it as the connection's
    /// `last_insert_rowid()` after an UPDATE or DELETE.
    pub last_insert: Option<i64>,
}

/// A node in memory (`RtreeNode`): `no` 0 until numbered; `live`
/// while the node hash holds it.
struct Node {
    no: i64,
    data: AVec<u8>,
    parent: Option<usize>,
    dirty: bool,
    live: bool,
}

/// One write's working state (`Rtree` plus its node hash).
pub struct Tree<'s> {
    g: Geom,
    store: &'s Store,
    nodes: AVec<Node>,
    depth: i32,
    /// `pDeleted`, removed nodes and their heights (last removed
    /// last).
    deleted: AVec<(usize, i32)>,
    pub w: Writes,
}

/// A new row for [`Tree::update`].
#[derive(Debug, Clone)]
pub struct NewRow {
    /// The id column (`aData[2]`): `None` when NULL.
    pub rowid: Option<i64>,
    pub c: [u32; 10],
    pub aux: AVec<OwnedValue>,
}

impl<'s> Tree<'s> {
    pub fn new(g: Geom, store: &'s Store) -> Tree<'s> {
        Tree {
            g,
            store,
            nodes: AVec::new(),
            depth: 0,
            deleted: AVec::new(),
            w: Writes::default(),
        }
    }

    fn node(&self, i: usize) -> R<&Node> {
        self.nodes.as_slice().get(i).ok_or(Fail::Corrupt)
    }
    fn node_mut(&mut self, i: usize) -> R<&mut Node> {
        self.nodes.as_mut_slice().get_mut(i).ok_or(Fail::Corrupt)
    }
    fn data(&self, i: usize) -> &[u8] {
        self.nodes
            .as_slice()
            .get(i)
            .map_or(&[], |n| n.data.as_slice())
    }
    fn cell(&self, i: usize, k: usize) -> Cell {
        get_cell(&self.g, self.data(i), k)
    }
    fn ncell(&self, i: usize) -> usize {
        ncell(self.data(i))
    }
    fn no(&self, i: usize) -> i64 {
        self.nodes.as_slice().get(i).map_or(0, |n| n.no)
    }
    fn parent(&self, i: usize) -> Option<usize> {
        self.nodes.as_slice().get(i).and_then(|n| n.parent)
    }

    /// `nodeHashLookup`.
    fn lookup(&self, no: i64) -> Option<usize> {
        if no == 0 {
            return None;
        }
        self.nodes.iter().position(|n| n.live && n.no == no)
    }

    fn read_node(&self, no: i64) -> R<Option<AVec<u8>>> {
        if let Some(v) = find(&self.w.nodes, no) {
            return Ok(v);
        }
        find(&self.store.nodes, no).ok_or(Fail::Miss(Miss::Node(no)))
    }

    fn read_parent(&self, no: i64) -> R<Option<i64>> {
        if let Some(v) = find(&self.w.parents, no) {
            return Ok(v);
        }
        find(&self.store.parents, no).ok_or(Fail::Miss(Miss::Parent(no)))
    }

    fn read_rowid(&self, r: i64) -> R<Option<i64>> {
        if let Some(v) = find(&self.w.rowids, r) {
            return Ok(v);
        }
        find(&self.store.rowids, r).ok_or(Fail::Miss(Miss::Rowid(r)))
    }

    /// `nodeAcquire`.
    fn acquire(&mut self, no: i64, parent: Option<usize>) -> R<usize> {
        if let Some(i) = self.lookup(no) {
            if parent.is_some() {
                let n = self.node_mut(i)?;
                if n.parent.is_none() {
                    n.parent = parent;
                }
            }
            return Ok(i);
        }
        let data = self.read_node(no)?.ok_or(Fail::Corrupt)?;
        if data.len() != self.g.node_size {
            return Err(Fail::Corrupt);
        }
        if no == 1 {
            self.depth = depth_of(data.as_slice());
            if self.depth > MAX_DEPTH {
                return Err(Fail::Corrupt);
            }
        }
        if ncell(data.as_slice()) > self.g.max_cells() {
            return Err(Fail::Corrupt);
        }
        self.nodes.push(Node {
            no,
            data,
            parent,
            dirty: false,
            live: true,
        });
        Ok(self.nodes.len() - 1)
    }

    /// `nodeNew`.
    fn node_new(&mut self, parent: Option<usize>) -> usize {
        let mut data = AVec::new();
        for _ in 0..self.g.node_size {
            data.push(0u8);
        }
        self.nodes.push(Node {
            no: 0,
            data,
            parent,
            dirty: true,
            live: false,
        });
        self.nodes.len() - 1
    }

    /// The largest key now of a table whose B-tree maximum is
    /// `max` (all keys `keys`), given this write's changes `w`.
    fn max_key(
        w: &AVec<(i64, Option<impl Sized>)>,
        max: Option<Option<i64>>,
        keys: Option<&AVec<i64>>,
        misses: (Miss, Miss),
    ) -> R<Option<i64>> {
        let max = max.ok_or(Fail::Miss(misses.0))?;
        let deleted = |k: i64| matches!(w.as_slice().binary_search_by_key(&k, |e| e.0), Ok(i) if w.as_slice().get(i).is_some_and(|e| e.1.is_none()));
        let mut best = match max {
            Some(m) if deleted(m) => {
                let keys = keys.ok_or(Fail::Miss(misses.1))?;
                keys.iter().rev().copied().find(|k| !deleted(*k))
            }
            m => m,
        };
        for (k, v) in w.iter() {
            if v.is_some() && best.is_none_or(|b| *k > b) {
                best = Some(*k);
            }
        }
        Ok(best)
    }

    /// The node number after the largest `%_node` key now.
    fn next_node_no(&self) -> R<i64> {
        let m = Self::max_key(
            &self.w.nodes,
            self.store.node_max,
            self.store.node_keys.as_ref(),
            (Miss::NodeMax, Miss::NodeKeys),
        )?;
        m.unwrap_or(0).checked_add(1).ok_or(Fail::Corrupt)
    }

    /// The rowid after the largest `%_rowid` key now (`rtreeNewRowid`;
    /// past the largest possible, the smallest unused one — the pin
    /// picks a random unused one there).
    fn next_rowid(&self) -> R<i64> {
        let m = Self::max_key(
            &self.w.rowids,
            self.store.rowid_max,
            self.store.rowid_keys.as_ref(),
            (Miss::RowidMax, Miss::RowidKeys),
        )?;
        match m {
            None => Ok(1),
            Some(m) if m < i64::MAX => Ok(m + 1),
            Some(_) => {
                let mut want = 1i64;
                while self.read_rowid(want)?.is_some() {
                    want = want.checked_add(1).ok_or(Fail::Corrupt)?;
                }
                Ok(want)
            }
        }
    }

    /// `nodeWrite`.
    fn node_write(&mut self, i: usize) -> R<()> {
        if !self.node(i)?.dirty {
            return Ok(());
        }
        let mut no = self.no(i);
        if no == 0 {
            no = self.next_node_no()?;
        }
        let data = self.node(i)?.data.clone();
        put(&mut self.w.nodes, no, Some(data));
        self.w.last_insert = Some(no);
        let n = self.node_mut(i)?;
        n.no = no;
        n.dirty = false;
        n.live = true;
        Ok(())
    }

    fn overwrite(&mut self, i: usize, cell: &Cell, k: usize) {
        let g = self.g;
        if let Some(n) = self.nodes.as_mut_slice().get_mut(i) {
            put_cell(&g, n.data.as_mut_slice(), cell, k);
            n.dirty = true;
        }
    }

    /// `nodeRowidIndex`.
    fn rowid_index(&self, i: usize, rowid: i64) -> R<usize> {
        (0..self.ncell(i))
            .find(|k| self.cell(i, *k).rowid == rowid)
            .ok_or(Fail::Corrupt)
    }

    /// `nodeParentIndex`.
    fn parent_index(&self, i: usize) -> R<usize> {
        let p = self.parent(i).ok_or(Fail::Corrupt)?;
        self.rowid_index(p, self.no(i))
    }

    fn rowid_write(&mut self, rowid: i64, node: i64) {
        put(&mut self.w.rowids, rowid, Some(node));
        self.w.last_insert = Some(rowid);
    }

    fn parent_write(&mut self, child: i64, parent: i64) {
        put(&mut self.w.parents, child, Some(parent));
        self.w.last_insert = Some(child);
    }

    /// `ChooseLeaf`.
    fn choose_leaf(&mut self, cell: &Cell, height: i32) -> R<usize> {
        let g = self.g;
        let mut node = self.acquire(1, None)?;
        let mut ii = 0;
        while ii < self.depth - height {
            let n = self.ncell(node);
            let mut best = 0i64;
            let mut found = false;
            let mut min_area = 0.0f64;
            for k in 0..n {
                let c = self.cell(node, k);
                if contains(&g, &c, cell) {
                    let a = area(&g, &c);
                    if !found || a < min_area {
                        best = c.rowid;
                        min_area = a;
                        found = true;
                    }
                }
            }
            if !found {
                let mut min_growth = 0.0f64;
                for k in 0..n {
                    let mut c = self.cell(node, k);
                    let a = area(&g, &c);
                    union(&g, &mut c, cell);
                    let growth = area(&g, &c) - a;
                    if k == 0 || growth < min_growth || (growth == min_growth && a < min_area) {
                        min_growth = growth;
                        min_area = a;
                        best = c.rowid;
                    }
                }
            }
            node = self.acquire(best, Some(node))?;
            ii += 1;
        }
        Ok(node)
    }

    /// `AdjustTree`.
    fn adjust_tree(&mut self, node: usize, cell: &Cell) -> R<()> {
        let g = self.g;
        let mut p = node;
        let mut cnt = 0;
        while let Some(parent) = self.parent(p) {
            cnt += 1;
            if cnt > 100 {
                return Err(Fail::Corrupt);
            }
            let ic = self.parent_index(p)?;
            let mut c = self.cell(parent, ic);
            if !contains(&g, &c, cell) {
                union(&g, &mut c, cell);
                self.overwrite(parent, &c, ic);
            }
            p = parent;
        }
        Ok(())
    }

    /// `updateMapping`.
    fn update_mapping(&mut self, rowid: i64, node: usize, height: i32) -> R<()> {
        if height > 0
            && let Some(ch) = self.lookup(rowid)
        {
            let mut p = Some(node);
            while let Some(x) = p {
                if x == ch {
                    return Err(Fail::Corrupt);
                }
                p = self.parent(x);
            }
            self.node_mut(ch)?.parent = Some(node);
        }
        let no = self.no(node);
        if height == 0 {
            self.rowid_write(rowid, no);
        } else {
            self.parent_write(rowid, no);
        }
        Ok(())
    }

    /// `rtreeInsertCell`.
    fn insert_cell(&mut self, node: usize, cell: &Cell, height: i32) -> R<()> {
        if height > 0
            && let Some(ch) = self.lookup(cell.rowid)
        {
            self.node_mut(ch)?.parent = Some(node);
        }
        let g = self.g;
        let full = {
            let n = self.node_mut(node)?;
            let full = node_insert(&g, n.data.as_mut_slice(), cell);
            if !full {
                n.dirty = true;
            }
            full
        };
        // The pin's R*Tree never reinserts (its `iReinsertHeight`
        // stays 0 and the root always splits; oracle-fitted on the
        // node images): a full node splits.
        if full {
            self.split(node, cell, height)
        } else {
            self.adjust_tree(node, cell)?;
            let no = self.no(node);
            if height == 0 {
                self.rowid_write(cell.rowid, no);
            } else {
                self.parent_write(cell.rowid, no);
            }
            Ok(())
        }
    }

    /// `splitNodeStartree`: fills `left` / `right`, returns their
    /// bounding boxes.
    fn split_startree(&mut self, cells: &[Cell], left: usize, right: usize) -> R<(Cell, Cell)> {
        let g = self.g;
        let n = cells.len();
        let ndim = g.ndim2 / 2;
        let m = g.min_cells();
        let mut sorted: AVec<AVec<usize>> = AVec::new();
        let mut spare: AVec<usize> = AVec::new();
        for _ in 0..n {
            spare.push(0);
        }
        for d in 0..ndim {
            let mut v = AVec::new();
            for j in 0..n {
                v.push(j);
            }
            sort_by_dim(&g, v.as_mut_slice(), d, cells, spare.as_mut_slice());
            sorted.push(v);
        }
        let at = |d: usize, k: usize| -> Cell {
            sorted
                .as_slice()
                .get(d)
                .and_then(|v| v.as_slice().get(k))
                .and_then(|j| cells.get(*j))
                .copied()
                .unwrap_or_default()
        };
        let mut best_dim = 0usize;
        let mut best_split = 0usize;
        let mut best_margin = 0.0f64;
        for d in 0..ndim {
            let mut mg = 0.0f64;
            let mut best_overlap = 0.0f64;
            let mut best_area = 0.0f64;
            let mut best_left = 0usize;
            let mut nl = m;
            while nl + m <= n {
                let mut l = at(d, 0);
                let mut r = at(d, n.saturating_sub(1));
                for k in 1..n.saturating_sub(1) {
                    let c = at(d, k);
                    if k < nl {
                        union(&g, &mut l, &c);
                    } else {
                        union(&g, &mut r, &c);
                    }
                }
                mg += margin(&g, &l);
                mg += margin(&g, &r);
                let o = overlap(&g, &l, &r);
                let a = area(&g, &l) + area(&g, &r);
                if nl == m || o < best_overlap || (o == best_overlap && a < best_area) {
                    best_left = nl;
                    best_overlap = o;
                    best_area = a;
                }
                nl += 1;
            }
            if d == 0 || mg < best_margin {
                best_dim = d;
                best_margin = mg;
                best_split = best_left;
            }
        }
        let mut lbox = at(best_dim, 0);
        let mut rbox = at(best_dim, best_split);
        for k in 0..n {
            let c = at(best_dim, k);
            let (target, bbox) = if k < best_split {
                (left, &mut lbox)
            } else {
                (right, &mut rbox)
            };
            if let Some(nd) = self.nodes.as_mut_slice().get_mut(target) {
                node_insert(&g, nd.data.as_mut_slice(), &c);
                nd.dirty = true;
            }
            union(&g, bbox, &c);
        }
        Ok((lbox, rbox))
    }

    /// `SplitNode`.
    fn split(&mut self, node: usize, cell: &Cell, height: i32) -> R<()> {
        let n = self.ncell(node);
        let mut cells: AVec<Cell> = AVec::new();
        for k in 0..n {
            cells.push(self.cell(node, k));
        }
        cells.push(*cell);
        {
            let nd = self.node_mut(node)?;
            node_zero(nd.data.as_mut_slice());
            nd.dirty = true;
        }
        let root = self.no(node) == 1;
        let (left, right) = if root {
            let r = self.node_new(Some(node));
            let l = self.node_new(Some(node));
            self.depth += 1;
            let d = self.depth as usize;
            let nd = self.node_mut(node)?;
            nd.dirty = true;
            wr16(nd.data.as_mut_slice(), 0, d);
            (l, r)
        } else {
            let p = self.parent(node);
            (node, self.node_new(p))
        };
        for i in [left, right] {
            let nd = self.node_mut(i)?;
            for b in nd.data.as_mut_slice().iter_mut() {
                *b = 0;
            }
        }
        let (mut lbox, mut rbox) = self.split_startree(cells.as_slice(), left, right)?;
        self.node_write(right)?;
        if self.no(left) == 0 {
            self.node_write(left)?;
        }
        rbox.rowid = self.no(right);
        lbox.rowid = self.no(left);
        if root {
            let p = self.parent(left).ok_or(Fail::Corrupt)?;
            self.insert_cell(p, &lbox, height + 1)?;
        } else {
            let parent = self.parent(left).ok_or(Fail::Corrupt)?;
            let ic = self.parent_index(left)?;
            self.overwrite(parent, &lbox, ic);
            self.adjust_tree(parent, &lbox)?;
        }
        let rp = self.parent(right).ok_or(Fail::Corrupt)?;
        self.insert_cell(rp, &rbox, height + 1)?;
        let mut new_right = false;
        for k in 0..self.ncell(right) {
            let r = self.cell(right, k).rowid;
            self.update_mapping(r, right, height)?;
            if r == cell.rowid {
                new_right = true;
            }
        }
        if root {
            for k in 0..self.ncell(left) {
                let r = self.cell(left, k).rowid;
                self.update_mapping(r, left, height)?;
            }
        } else if !new_right {
            self.update_mapping(cell.rowid, left, height)?;
        }
        Ok(())
    }

    /// `fixBoundingBox`.
    fn fix_bbox(&mut self, node: usize) -> R<()> {
        let g = self.g;
        let mut cur = node;
        while let Some(parent) = self.parent(cur) {
            let n = self.ncell(cur);
            let mut bx = self.cell(cur, 0);
            for k in 1..n {
                let c = self.cell(cur, k);
                union(&g, &mut bx, &c);
            }
            bx.rowid = self.no(cur);
            let ic = self.parent_index(cur)?;
            self.overwrite(parent, &bx, ic);
            cur = parent;
        }
        Ok(())
    }

    /// `fixLeafParent`.
    fn fix_leaf_parent(&mut self, leaf: usize) -> R<()> {
        let mut child = leaf;
        while self.no(child) != 1 && self.parent(child).is_none() {
            if let Some(pno) = self.read_parent(self.no(child))? {
                let mut t = Some(leaf);
                let mut seen = false;
                while let Some(x) = t {
                    if self.no(x) == pno {
                        seen = true;
                        break;
                    }
                    t = self.parent(x);
                }
                if !seen {
                    let p = self.acquire(pno, None)?;
                    self.node_mut(child)?.parent = Some(p);
                }
            }
            child = self.parent(child).ok_or(Fail::Corrupt)?;
        }
        Ok(())
    }

    /// `deleteCell`.
    fn delete_cell(&mut self, node: usize, k: usize, height: i32) -> R<()> {
        self.fix_leaf_parent(node)?;
        let g = self.g;
        {
            let nd = self.node_mut(node)?;
            node_delete(&g, nd.data.as_mut_slice(), k);
            nd.dirty = true;
        }
        if self.parent(node).is_some() {
            if self.ncell(node) < g.min_cells() {
                self.remove_node(node, height)?;
            } else {
                self.fix_bbox(node)?;
            }
        }
        Ok(())
    }

    /// `removeNode`.
    fn remove_node(&mut self, node: usize, height: i32) -> R<()> {
        let ic = self.parent_index(node)?;
        let parent = self.parent(node).ok_or(Fail::Corrupt)?;
        self.node_mut(node)?.parent = None;
        self.delete_cell(parent, ic, height + 1)?;
        let no = self.no(node);
        put(&mut self.w.nodes, no, None);
        put(&mut self.w.parents, no, None);
        self.node_mut(node)?.live = false;
        self.deleted.push((node, height));
        Ok(())
    }

    /// `reinsertNodeContent`.
    fn reinsert_content(&mut self, node: usize, height: i32) -> R<()> {
        for k in 0..self.ncell(node) {
            let c = self.cell(node, k);
            let ins = self.choose_leaf(&c, height)?;
            self.insert_cell(ins, &c, height)?;
        }
        Ok(())
    }

    /// `rtreeDeleteRowid`.
    fn delete_rowid(&mut self, rowid: i64) -> R<()> {
        let root = self.acquire(1, None)?;
        if let Some(leaf_no) = self.read_rowid(rowid)? {
            let leaf = self.acquire(leaf_no, None)?;
            let k = self.rowid_index(leaf, rowid)?;
            self.delete_cell(leaf, k, 0)?;
        }
        put(&mut self.w.rowids, rowid, None);
        if self.depth > 0 && self.ncell(root) == 1 {
            let child_no = self.cell(root, 0).rowid;
            let child = self.acquire(child_no, Some(root))?;
            self.remove_node(child, self.depth - 1)?;
            self.depth -= 1;
            let d = self.depth as usize;
            let nd = self.node_mut(root)?;
            wr16(nd.data.as_mut_slice(), 0, d);
            nd.dirty = true;
        }
        while let Some((n, h)) = self.deleted.pop() {
            self.reinsert_content(n, h)?;
        }
        Ok(())
    }

    /// One `xUpdate` (`rtreeUpdate`): delete `old`, then insert
    /// `new` (with `replace`, a row holding the new rowid is deleted
    /// first; else that is `Fail::Unique`). Returns the inserted
    /// rowid.
    pub fn update(
        &mut self,
        old: Option<i64>,
        new: Option<&NewRow>,
        replace: bool,
    ) -> R<Option<i64>> {
        let mut have = None;
        if let Some(nr) = new
            && let Some(r) = nr.rowid
        {
            if old != Some(r) && self.read_rowid(r)?.is_some() {
                if replace {
                    self.delete_rowid(r)?;
                    self.flush()?;
                } else {
                    return Err(Fail::Unique);
                }
            }
            have = Some(r);
        }
        // The pin writes a delete's nodes as it releases them, before
        // the insert begins.
        if let Some(o) = old {
            self.delete_rowid(o)?;
            self.flush()?;
        }
        let Some(nr) = new else {
            self.flush()?;
            return Ok(None);
        };
        let rowid = match have {
            Some(r) => r,
            None => self.next_rowid()?,
        };
        let cell = Cell { rowid, c: nr.c };
        let leaf = self.choose_leaf(&cell, 0)?;
        self.insert_cell(leaf, &cell, 0)?;
        if !nr.aux.is_empty() {
            self.w.aux = Some((rowid, nr.aux.clone()));
        }
        self.flush()?;
        Ok(Some(rowid))
    }

    /// Every node still dirty is written (`nodeRelease`).
    fn flush(&mut self) -> R<()> {
        // Deepest first, the root last: the order the pin releases
        // (and so writes) the nodes of the path it acquired.
        for i in (0..self.nodes.len()).rev() {
            let n = self.node(i)?;
            if n.live && n.dirty {
                self.node_write(i)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------
// Queries
// ---------------------------------------------------------------

/// One search constraint (`RtreeConstraint`).
#[derive(Debug, Clone, Copy)]
struct Constraint {
    /// `idxStr` op, or TRUE `?` / FALSE `@`.
    op: u8,
    coord: usize,
    value: f64,
}

/// `rtreeFilter`'s constraints from `idxStr` and its arguments.
fn constraints(idx_str: &[u8], argv: &[OwnedValue]) -> AVec<Constraint> {
    let mut out = AVec::new();
    for (k, pair) in idx_str.chunks_exact(2).enumerate() {
        let (Some(&o), Some(&c)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (mut op, coord) = (o, usize::from(c.wrapping_sub(b'0')));
        let v = argv.get(k).cloned().unwrap_or(OwnedValue::Null);
        let v = match v {
            OwnedValue::Text(t) => {
                crate::record::text_to_number(t.as_slice()).unwrap_or(OwnedValue::Text(t))
            }
            x => x,
        };
        let value = match v {
            OwnedValue::Integer(i) => {
                if !(-(1i64 << 48)..(1i64 << 48)).contains(&i) || i == -(1i64 << 48) {
                    if op == b'C' {
                        op = b'B';
                    }
                    if op == b'E' {
                        op = b'D';
                    }
                }
                i as f64
            }
            OwnedValue::Real(r) => r,
            OwnedValue::Null => {
                op = b'@';
                0.0
            }
            _ => {
                op = if op == b'C' || op == b'B' { b'?' } else { b'@' };
                0.0
            }
        };
        out.push(Constraint { op, coord, value });
    }
    out
}

fn coord_at(g: &Geom, d: &[u8], at: usize) -> f64 {
    g.d(rd32(d, at))
}

/// `rtreeLeafConstraint` / `rtreeNonleafConstraint` for the cell at
/// byte `off` of node image `d`.
fn passes(g: &Geom, cons: &[Constraint], d: &[u8], off: usize, leaf: bool) -> bool {
    for c in cons.iter() {
        let ok = match c.op {
            b'?' => true,
            b'@' => false,
            _ if leaf => {
                let x = coord_at(g, d, off + 8 + 4 * c.coord);
                match c.op {
                    b'B' => x <= c.value,
                    b'C' => x < c.value,
                    b'D' => x >= c.value,
                    b'E' => x > c.value,
                    _ => x == c.value,
                }
            }
            _ => {
                let base = off + 8 + 4 * (c.coord & !1);
                let lo = coord_at(g, d, base);
                let hi = coord_at(g, d, base + 4);
                match c.op {
                    b'A' => c.value >= lo && c.value <= hi,
                    b'B' | b'C' => c.value >= lo,
                    _ => c.value <= hi,
                }
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// A search in progress (`rtreeStepToLeaf` with no MATCH: every
/// score is 0, so the pin's priority queue walks depth first, in
/// cell order).
pub struct Search {
    cons: AVec<Constraint>,
    /// (node image, level: 1 for a leaf, next cell, node number).
    stack: AVec<(AVec<u8>, i32, usize, i64)>,
    /// The node the search waits for, and its level.
    want: Option<(i64, i32)>,
    /// The root was asked for.
    started: bool,
}

/// What [`Search::step`] wants or found.
#[derive(Debug, Clone, Copy)]
pub enum Found {
    /// Node `no`'s image (pass it to the next step).
    Need(i64),
    Row(Cell),
    Done,
    Corrupt,
}

impl Search {
    /// A search of `idxStr` / `argv` from the root.
    pub fn new(idx_str: &[u8], argv: &[OwnedValue]) -> Search {
        Search {
            cons: constraints(idx_str, argv),
            stack: AVec::new(),
            want: None,
            started: false,
        }
    }

    /// The next leaf cell, or the node to fetch (`img` answers the
    /// last `Need`; `None` for a missing node).
    pub fn step(&mut self, g: &Geom, img: Option<AVec<u8>>) -> Found {
        if !self.started {
            self.started = true;
            self.want = Some((1, 0));
            return Found::Need(1);
        }
        if let Some((no, level)) = self.want.take() {
            let Some(img) = img else {
                return Found::Corrupt;
            };
            if img.len() != g.node_size {
                return Found::Corrupt;
            }
            let level = if no == 1 && self.stack.is_empty() {
                let d = depth_of(img.as_slice());
                if d > MAX_DEPTH {
                    return Found::Corrupt;
                }
                d + 1
            } else {
                level
            };
            self.stack.push((img, level, 0, no));
        }
        while let Some(top) = self.stack.as_mut_slice().last_mut() {
            let n = ncell(top.0.as_slice()).min(g.max_cells());
            if top.2 >= n {
                self.stack.pop();
                continue;
            }
            let k = top.2;
            top.2 += 1;
            let off = 4 + g.bpc() * k;
            let level = top.1;
            if !passes(g, self.cons.as_slice(), top.0.as_slice(), off, level == 1) {
                continue;
            }
            let cell = get_cell(g, top.0.as_slice(), k);
            if level <= 1 {
                return Found::Row(cell);
            }
            if self.stack.iter().any(|s| s.3 == cell.rowid) || self.stack.len() > 64 {
                return Found::Corrupt;
            }
            self.want = Some((cell.rowid, level - 1));
            return Found::Need(cell.rowid);
        }
        Found::Done
    }
}

/// The rowid lookup (`idxNum` 1): the value must be an integer (or a
/// real equal to one).
pub fn lookup_rowid(v: &OwnedValue) -> Option<i64> {
    let v = match v {
        OwnedValue::Text(t) => crate::record::text_to_number(t.as_slice())?,
        x => x.clone(),
    };
    match v {
        OwnedValue::Integer(i) => Some(i),
        OwnedValue::Real(r) => {
            let i = crate::query::num_to_i64(crate::query::Num::Real(r));
            (i as f64 == r).then_some(i)
        }
        _ => None,
    }
}

/// The leaf cell for `rowid` in node image `d`.
pub fn leaf_cell(g: &Geom, d: &[u8], rowid: i64) -> Option<Cell> {
    (0..ncell(d).min(g.max_cells()))
        .map(|k| get_cell(g, d, k))
        .find(|c| c.rowid == rowid)
}

// ---------------------------------------------------------------
// rtreecheck(), rtreenode(), rtreedepth()
// ---------------------------------------------------------------

/// `rtreeCheckTable`'s report lines (none: "ok"), over the node tree
/// of a table with `ndim` dimensions (`int` read from its first row,
/// as the pin's does), with every shadow row in `st`; `counts` are
/// the `%_rowid` / `%_parent` row counts.
pub fn check(ndim: usize, int: bool, st: &Store, counts: (i64, i64)) -> AVec<AVec<u8>> {
    let mut ck = Checker {
        ndim,
        int,
        nleaf: 0,
        nonleaf: 0,
        report: AVec::new(),
        nerr: 0,
    };
    ck.node(st, 0, None, 1);
    for (tbl, expect, actual) in [
        (&b"_rowid"[..], ck.nleaf, counts.0),
        (b"_parent", ck.nonleaf, counts.1),
    ] {
        if expect != actual {
            let mut m = AVec::new();
            m.extend_from_slice(b"Wrong number of entries in %");
            m.extend_from_slice(tbl);
            m.extend_from_slice(b" table - expected ");
            crate::record::render_int(expect, &mut m);
            m.extend_from_slice(b", actual ");
            crate::record::render_int(actual, &mut m);
            ck.msg(m);
        }
    }
    ck.report
}

struct Checker {
    ndim: usize,
    int: bool,
    nleaf: i64,
    nonleaf: i64,
    report: AVec<AVec<u8>>,
    nerr: usize,
}

fn num(out: &mut AVec<u8>, v: i64) {
    crate::record::render_int(v, out);
}

impl Checker {
    fn msg(&mut self, m: AVec<u8>) {
        if self.nerr < 100 {
            self.report.push(m);
        }
        self.nerr += 1;
    }

    fn coord_lt(&self, a: u32, b: u32) -> bool {
        if self.int {
            (a as i32) < (b as i32)
        } else {
            f32::from_bits(a) < f32::from_bits(b)
        }
    }

    fn node(&mut self, st: &Store, depth: i32, parent: Option<&[u8]>, no: i64) {
        let Some(Some(img)) = find(&st.nodes, no) else {
            let mut m = AVec::new();
            m.extend_from_slice(b"Node ");
            num(&mut m, no);
            m.extend_from_slice(b" missing from database");
            self.msg(m);
            return;
        };
        let d = img.as_slice();
        if d.len() < 4 {
            let mut m = AVec::new();
            m.extend_from_slice(b"Node ");
            num(&mut m, no);
            m.extend_from_slice(b" is too small (");
            num(&mut m, d.len() as i64);
            m.extend_from_slice(b" bytes)");
            self.msg(m);
            return;
        }
        let mut depth = depth;
        if parent.is_none() {
            depth = depth_of(d);
            if depth > MAX_DEPTH {
                let mut m = AVec::new();
                m.extend_from_slice(b"Rtree depth out of range (");
                num(&mut m, i64::from(depth));
                m.push(b')');
                self.msg(m);
                return;
            }
        }
        let n = ncell(d);
        let bpc = 8 + self.ndim * 8;
        if 4 + n * bpc > d.len() {
            let mut m = AVec::new();
            m.extend_from_slice(b"Node ");
            num(&mut m, no);
            m.extend_from_slice(b" is too small for cell count of ");
            num(&mut m, n as i64);
            m.extend_from_slice(b" (");
            num(&mut m, d.len() as i64);
            m.extend_from_slice(b" bytes)");
            self.msg(m);
            return;
        }
        for i in 0..n {
            let off = 4 + i * bpc;
            let val = rd64(d, off);
            let cell = d.get(off + 8..off + bpc).unwrap_or(&[]);
            self.cell_coords(no, i, cell, parent);
            if depth > 0 {
                self.mapping(st, false, val, no);
                self.node(st, depth - 1, Some(cell), val);
                self.nonleaf += 1;
            } else {
                self.mapping(st, true, val, no);
                self.nleaf += 1;
            }
        }
    }

    fn cell_coords(&mut self, no: i64, i: usize, cell: &[u8], parent: Option<&[u8]>) {
        for d in 0..self.ndim {
            let (c1, c2) = (rd32(cell, 8 * d), rd32(cell, 8 * d + 4));
            let corrupt = |m: &mut AVec<u8>, rel: bool| {
                m.extend_from_slice(b"Dimension ");
                num(m, d as i64);
                m.extend_from_slice(b" of cell ");
                num(m, i as i64);
                m.extend_from_slice(b" on node ");
                num(m, no);
                m.extend_from_slice(b" is corrupt");
                if rel {
                    m.extend_from_slice(b" relative to parent");
                }
            };
            if self.coord_lt(c2, c1) {
                let mut m = AVec::new();
                corrupt(&mut m, false);
                self.msg(m);
            }
            if let Some(p) = parent {
                let (p1, p2) = (rd32(p, 8 * d), rd32(p, 8 * d + 4));
                if self.coord_lt(c1, p1) || self.coord_lt(p2, c2) {
                    let mut m = AVec::new();
                    corrupt(&mut m, true);
                    self.msg(m);
                }
            }
        }
    }

    fn mapping(&mut self, st: &Store, leaf: bool, key: i64, val: i64) {
        let tbl: &[u8] = if leaf { b"%_rowid" } else { b"%_parent" };
        let got = if leaf {
            find(&st.rowids, key).flatten()
        } else {
            find(&st.parents, key).flatten()
        };
        match got {
            None => {
                let mut m = AVec::new();
                m.extend_from_slice(b"Mapping (");
                num(&mut m, key);
                m.extend_from_slice(b" -> ");
                num(&mut m, val);
                m.extend_from_slice(b") missing from ");
                m.extend_from_slice(tbl);
                m.extend_from_slice(b" table");
                self.msg(m);
            }
            Some(got) if got != val => {
                let mut m = AVec::new();
                m.extend_from_slice(b"Found (");
                num(&mut m, key);
                m.extend_from_slice(b" -> ");
                num(&mut m, got);
                m.extend_from_slice(b") in ");
                m.extend_from_slice(tbl);
                m.extend_from_slice(b" table, expected (");
                num(&mut m, key);
                m.extend_from_slice(b" -> ");
                num(&mut m, val);
                m.extend_from_slice(b")");
                self.msg(m);
            }
            Some(_) => {}
        }
    }
}

/// `rtreenode(ndim, blob)`: `{rowid c1 c2 ...}` per cell, the
/// coordinates as `%g` of their float reading; NULL for a dimension
/// outside 1..=5 or a blob too short.
pub fn rtreenode(ndim: i64, blob: &[u8]) -> Option<AVec<u8>> {
    if !(1..=5).contains(&ndim) {
        return None;
    }
    let ndim = ndim as usize;
    if blob.len() < 4 {
        return None;
    }
    let g = Geom {
        ndim2: 2 * ndim,
        int: false,
        node_size: blob.len(),
    };
    let n = ncell(blob);
    if blob.len() < n * g.bpc() {
        return None;
    }
    let mut out = AVec::new();
    for k in 0..n {
        let c = get_cell(&g, blob, k);
        if k > 0 {
            out.push(b' ');
        }
        out.push(b'{');
        num(&mut out, c.rowid);
        for j in 0..g.ndim2 {
            out.push(b' ');
            let v = f64::from(f32::from_bits(c.c.get(j).copied().unwrap_or(0)));
            let mut fmt = AVec::new();
            fmt.extend_from_slice(b"%g");
            let args = [OwnedValue::Text(fmt), OwnedValue::Real(v)];
            if let Ok(OwnedValue::Text(t)) = crate::func::printf::<()>(&args) {
                out.extend_from_slice(t.as_slice());
            }
        }
        out.push(b'}');
    }
    (!out.is_empty()).then_some(out)
}
