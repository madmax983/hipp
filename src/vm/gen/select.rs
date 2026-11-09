//! SELECT code generation, unit `vm_core` (ADR 0005 Stage 2): a
//! SELECT over one rowid table or none — WHERE (its constant terms
//! first, as `join_sources` computes them), result columns of any
//! subquery-free expression, ORDER BY through a sorter, LIMIT /
//! OFFSET, and full-scan, rowid-equality and rowid-range access. The
//! program streams its rows as the streamed executor does
//! (`session::stream`): the WHERE, OFFSET and result columns per row,
//! a sort of every row first for an ORDER BY the table's order does
//! not deliver.
//!
//! Every other shape (index access, joins, aggregates, DISTINCT,
//! subqueries, compounds, VALUES, CTEs and views, windows, virtual
//! and table-valued sources, WITHOUT ROWID, TEMP and attached tables,
//! `sqlite_schema`) returns `None` and runs on today's executor.

use super::super::Program;
use super::super::build::{Builder, Label};
use super::super::cursor::{SortInfo, TableInfo};
use super::super::op::{Code, EVAL_CONSTS, P4};
use super::expr::{cond, cursor_ref, evaluable, value};
use crate::arena::AVec;
use crate::query::{RExpr, SubSource};
use crate::record::{Affinity, OwnedValue};
use crate::session::QueryPlan;
use crate::sql::SortOrder;

/// A legacy scan plan's access path (`session::Access`).
pub(crate) enum ScanAccess {
    Full,
    Rowid(i64),
    Empty,
}

/// How the one source is read.
enum Path<'p> {
    Scan(core::marker::PhantomData<&'p ()>),
    /// `rowid = e` (the probe and its comparison affinity), after
    /// the range bounds' NULL check.
    #[cfg(feature = "expr_index")]
    Eq(&'p RExpr, Option<Affinity>, Bounds<'p>),
    /// A rowid range, ascending or (`reverse`) descending.
    #[cfg(feature = "expr_index")]
    Range(Bounds<'p>, bool),
}

#[cfg(feature = "expr_index")]
type Bounds<'p> = [Option<&'p crate::xplan::Bound>; 2];

/// Whether source `sp` is a plain rowid table the VM reads: no
/// derived table, join constraint, virtual or table-valued source,
/// TEMP, attached or WITHOUT ROWID table (`wr`), nor the schema
/// table.
fn plain_source(sp: &SubSource, wr: &dyn Fn(u32) -> bool) -> bool {
    let ok = sp.derived.is_none()
        && sp.on.is_none()
        && !sp.kind.pads_left()
        && !sp.kind.pads_right()
        && sp.root > 1
        && sp.affs.len() == sp.ncols
        && !wr(sp.root);
    #[cfg(feature = "sql")]
    let ok = ok && sp.rec == 0 && !sp.temp;
    #[cfg(any(feature = "json", feature = "vtab"))]
    let ok = ok && sp.tvf == 0;
    #[cfg(feature = "vtab")]
    let ok = ok && sp.vt.is_none();
    #[cfg(feature = "attach")]
    let ok = ok && sp.ext == 0;
    #[cfg(feature = "planner")]
    let ok = ok && !sp.exists;
    ok
}

/// The source's access path, when the VM covers it: a table scan, or
/// a rowid equality or range (`xplan::IdxAccess` on the rowid tree,
/// read for the empty outer row).
fn path(sp: &SubSource) -> Option<Path<'_>> {
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        let plain = a.root == 0
            && !a.lateral
            && a.cover.is_none()
            && a.bignull.is_none()
            && a.eq.len() <= 1;
        #[cfg(feature = "planner")]
        let plain = plain
            && a.auto.is_none()
            && a.bloom.is_none()
            && a.or.is_none()
            && a.first.is_none()
            && a.skip == 0;
        if !plain {
            return None;
        }
        let bounds = [a.lo.as_ref(), a.hi.as_ref()];
        if !bounds.iter().flatten().all(|b| evaluable(&b.e, 0, false)) {
            return None;
        }
        return match a.eq.as_slice().first() {
            Some(p) => match p.vals.as_slice() {
                [e] if !p.is_in && p.sub.is_none() && evaluable(e, 0, false) => {
                    Some(Path::Eq(e, p.aff, bounds))
                }
                _ => None,
            },
            None => Some(Path::Range(bounds, a.reverse)),
        };
    }
    let _ = sp;
    Some(Path::Scan(core::marker::PhantomData))
}

/// Whether ORDER BY `order` is the source's own row order (the
/// streamed executor's `rowid_order`): the order of its rowid loop
/// when that delivers every term, else the rowid (or its alias)
/// ascending over a table scan.
fn rowid_order(sp: &SubSource, order: &[(RExpr, SortOrder)]) -> bool {
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        return !a.lateral && a.sorted >= order.len();
    }
    let [(e, o)] = order else {
        return false;
    };
    !o.desc
        && match e {
            RExpr::Rowid(0) => true,
            RExpr::Column(slot, ..) => sp.rowid_alias == Some(*slot),
            _ => false,
        }
}

/// The result column each ORDER BY term is (`session::order_cols`):
/// a term the same as a result item takes that item's value.
fn order_cols(order: &[(RExpr, SortOrder)], items: &[RExpr]) -> AVec<Option<usize>> {
    let mut out = AVec::new();
    for (e, _) in order.iter() {
        let mut e = e;
        while let RExpr::Collate(x, _) = e {
            e = x;
        }
        out.push(items.iter().rposition(|it| crate::query::same_expr(e, it)));
    }
    out
}

/// The program of a general query plan (`Plan::Query`), or `None`
/// when it is not a shape this unit covers. `wr`: whether a root
/// page is a WITHOUT ROWID table's.
pub(crate) fn query(qp: &QueryPlan, wr: &dyn Fn(u32) -> bool) -> Option<Program> {
    if qp.sources.len() > 1
        || !qp.compound.is_empty()
        || !qp.values.is_empty()
        || !qp.group_by.is_empty()
        || qp.having.is_some()
        || qp.has_agg
        || qp.distinct
    {
        return None;
    }
    let src = qp.sources.as_slice().first();
    let width = src.map_or(0, |s| s.ncols);
    let has = src.is_some();
    let ok = |e: &RExpr| evaluable(e, width, has);
    if !qp.items.iter().all(ok)
        || !qp.filter.as_ref().is_none_or(ok)
        || !qp.order_by.iter().all(|(e, _)| ok(e))
        || ![&qp.limit, &qp.offset]
            .iter()
            .all(|l| l.as_ref().is_none_or(|e| evaluable(e, 0, false)))
    {
        return None;
    }
    let path = match src {
        Some(sp) => {
            if !plain_source(sp, wr) {
                return None;
            }
            Some((sp, path(sp)?))
        }
        None => None,
    };
    let mut b = Builder::new();
    let end = b.label();
    let n = qp.items.len();
    // LIMIT, then OFFSET (`limit` + 1), as the pin computes them
    // before the loop: a LIMIT of 0 reads nothing, not even the
    // OFFSET (the pin's `computeLimitRegisters`).
    let lim = match &qp.limit {
        Some(l) => {
            let r = b.regs(2);
            value(&mut b, l, None, r);
            b.emit(Code::MustBeInt, r, 0, 0, 0, 0);
            b.jump(Code::IfNot, r, end, 0, 0, 0);
            if let Some(o) = &qp.offset {
                let ro = r.saturating_add(1);
                value(&mut b, o, None, ro);
                b.emit(Code::MustBeInt, ro, 0, 0, 0, 0);
            }
            Some(r)
        }
        None => None,
    };
    let off = match (lim, &qp.offset) {
        (Some(r), Some(_)) => Some(r.saturating_add(1)),
        _ => None,
    };
    let tmp = b.reg();
    // The WHERE clause's constant terms, once, before any row.
    if let Some(f) = &qp.filter {
        let k = b.p4(P4::Expr(f.clone()));
        b.emit(Code::Eval, 0, tmp, 0, k, EVAL_CONSTS);
        b.jump(Code::IfNot, tmp, end, 0, 0, 0);
    }
    // The result columns, then (for a sort) the ORDER BY keys.
    let items = b.regs(n.saturating_add(qp.order_by.len()));
    let Some((sp, path)) = path else {
        // No FROM: one row, the WHERE and OFFSET deciding whether it
        // is returned. The pin's loop over no table satisfies any
        // ORDER BY, so its terms are never computed.
        if let Some(f) = &qp.filter {
            cond(&mut b, f, None, tmp, end);
        }
        if let Some(o) = off {
            b.jump(Code::IfPos, o, end, 1, 0, 0);
        }
        for (i, it) in qp.items.iter().enumerate() {
            value(&mut b, it, None, items.saturating_add(i as u32));
        }
        b.emit(Code::ResultRow, items, n as u32, 0, 0, 0);
        b.here(end);
        b.emit(Code::Halt, 0, 0, 0, 0, 0);
        return b.finish();
    };
    let sort = !qp.order_by.is_empty() && !rowid_order(sp, qp.order_by.as_slice());
    let c = b.cursor();
    let info = b.p4(P4::Table(alloc::boxed::Box::new(TableInfo {
        root: sp.root,
        ncols: sp.ncols,
        rowid_alias: sp.rowid_alias,
        affs: sp.affs.clone(),
        defaults: sp.defaults.clone(),
        gcols: sp.gcols.clone(),
    })));
    let sorter = if sort {
        let s = b.cursor();
        let mut orders = AVec::new();
        for (_, o) in qp.order_by.iter() {
            orders.push(*o);
        }
        let k = b.p4(P4::Sorter(alloc::boxed::Box::new(SortInfo {
            orders,
            nout: n,
            window: lim,
        })));
        b.emit(Code::SorterOpen, s, 0, 0, k, 0);
        Some(s)
    } else {
        None
    };
    b.emit(Code::OpenRead, c, 0, 0, info, 0);
    let top = b.label();
    let next = b.label();
    let done = b.label();
    let step = open(&mut b, c, path, top, done);
    b.here(top);
    if let Some(stop) = step.stop {
        b.emit(Code::Rowid, c, tmp, 0, 0, 0);
        b.jump(stop.0, tmp, done, stop.1, 0, 0);
    }
    let cur = Some(c);
    if let Some(f) = &qp.filter {
        cond(&mut b, f, cur, tmp, next);
    }
    match sorter {
        None => {
            if let Some(o) = off {
                b.jump(Code::IfPos, o, next, 1, 0, 0);
            }
            for (i, it) in qp.items.iter().enumerate() {
                value(&mut b, it, cur, items.saturating_add(i as u32));
            }
            b.emit(Code::ResultRow, items, n as u32, cursor_ref(cur), 0, 0);
            if let Some(l) = lim {
                b.jump(Code::DecrJumpZero, l, done, 0, 0, 0);
            }
        }
        Some(s) => {
            // The result columns, then the sort keys right after them;
            // a key that is a result column takes its value
            // (`shown_keys`), after being computed as the executor
            // computes it.
            for (i, it) in qp.items.iter().enumerate() {
                value(&mut b, it, cur, items.saturating_add(i as u32));
            }
            for (j, (e, _)) in qp.order_by.iter().enumerate() {
                value(&mut b, e, cur, items.saturating_add((n + j) as u32));
            }
            for (j, col) in order_cols(qp.order_by.as_slice(), qp.items.as_slice())
                .iter()
                .enumerate()
            {
                if let Some(i) = col {
                    b.emit(
                        Code::Copy,
                        items.saturating_add(*i as u32),
                        items.saturating_add((n + j) as u32),
                        0,
                        0,
                        0,
                    );
                }
            }
            b.emit(Code::SorterInsert, s, items, cursor_ref(cur), 0, 0);
        }
    }
    b.here(next);
    match step.next {
        Some(code) => b.jump(code, c, top, 0, 0, 0),
        None => b.jump(Code::Goto, 0, done, 0, 0, 0),
    }
    b.here(done);
    if let Some(s) = sorter {
        // The sorted rows inside the LIMIT / OFFSET window.
        let out = b.regs(n);
        let more = b.label();
        let skip = b.label();
        b.jump(Code::SorterSort, s, end, 0, 0, 0);
        b.here(more);
        if let Some(o) = off {
            b.jump(Code::IfPos, o, skip, 1, 0, 0);
        }
        b.emit(Code::SorterData, s, out, 0, 0, 0);
        b.emit(Code::ResultRow, out, n as u32, cursor_ref(Some(s)), 0, 0);
        if let Some(l) = lim {
            b.jump(Code::DecrJumpZero, l, end, 0, 0, 0);
        }
        b.here(skip);
        b.jump(Code::SorterNext, s, more, 0, 0, 0);
    }
    b.here(end);
    b.emit(Code::Halt, 0, 0, 0, 0, 0);
    b.finish()
}

/// How a loop opened by `open` advances and when it stops.
struct Loop {
    /// `Next` or `Prev`; `None` for a one-row lookup.
    next: Option<Code>,
    /// The end test at the top of each row: (comparison, bound
    /// register) — the row's rowid past the bound ends the loop.
    stop: Option<(Code, u32)>,
}

/// Position cursor `c` on the first row of `path` (jumping to `done`
/// when there is none) and fall through to `top`.
fn open(b: &mut Builder, c: u32, path: Path<'_>, top: Label, done: Label) -> Loop {
    let _ = top;
    match path {
        Path::Scan(_) => {
            b.jump(Code::Rewind, c, done, 0, 0, 0);
            Loop {
                next: Some(Code::Next),
                stop: None,
            }
        }
        #[cfg(feature = "expr_index")]
        Path::Eq(e, aff, bounds) => {
            // The bounds are computed first and a NULL one finds no
            // row; then the one rowid (`xindex::access_walk`).
            bounds_null_check(b, bounds, done);
            let r = b.reg();
            value(b, e, None, r);
            let k = b.p4(P4::Aff(aff));
            b.emit(Code::Affinity, r, 0, 0, k, 0);
            b.jump(Code::SeekRowid, c, done, r, 0, 0);
            Loop {
                next: None,
                stop: None,
            }
        }
        #[cfg(feature = "expr_index")]
        Path::Range(bounds, reverse) => {
            let regs = bound_regs(b, bounds, done);
            let (from, to) = if reverse {
                (regs[1], regs[0])
            } else {
                (regs[0], regs[1])
            };
            let (seek, first) = if reverse {
                (Code::SeekLE, Code::Last)
            } else {
                (Code::SeekGE, Code::Rewind)
            };
            match from {
                Some(r) => {
                    let whole = b.label();
                    b.jump(Code::IsNull, r, whole, 0, 0, 0);
                    b.jump(seek, c, done, r, 0, 0);
                    b.jump(Code::Goto, 0, top, 0, 0, 0);
                    b.here(whole);
                    b.jump(first, c, done, 0, 0, 0);
                }
                None => b.jump(first, c, done, 0, 0, 0),
            }
            Loop {
                next: Some(if reverse { Code::Prev } else { Code::Next }),
                stop: to.map(|r| (if reverse { Code::Lt } else { Code::Gt }, r)),
            }
        }
    }
}

/// The range bounds' values, compared with their columns' affinity;
/// a NULL one finds no row.
#[cfg(feature = "expr_index")]
fn bounds_null_check(b: &mut Builder, bounds: Bounds<'_>, done: Label) {
    let mut regs = [None, None];
    for (slot, bd) in regs.iter_mut().zip(bounds) {
        if let Some(bd) = bd {
            let r = b.reg();
            value(b, &bd.e, None, r);
            let k = b.p4(P4::Aff(bd.aff));
            b.emit(Code::Affinity, r, 0, 0, k, 0);
            *slot = Some(r);
        }
    }
    for r in regs.into_iter().flatten() {
        b.jump(Code::IsNull, r, done, 0, 0, 0);
    }
}

/// The range bounds as rowid range ends (`xindex::rowid_span`): each
/// computed and converted in turn, then made a rowid (NULL: open),
/// a bound no rowid satisfies ending the loop at once.
#[cfg(feature = "expr_index")]
fn bound_regs(b: &mut Builder, bounds: Bounds<'_>, done: Label) -> [Option<u32>; 2] {
    let mut regs = [None, None];
    for (slot, bd) in regs.iter_mut().zip(bounds) {
        if let Some(bd) = bd {
            let r = b.reg();
            value(b, &bd.e, None, r);
            let k = b.p4(P4::Aff(bd.aff));
            b.emit(Code::Affinity, r, 0, 0, k, 0);
            *slot = Some((r, bd.incl));
        }
    }
    let mut out = [None, None];
    for (i, (slot, r)) in out.iter_mut().zip(regs).enumerate() {
        if let Some((r, incl)) = r {
            let flags = u8::from(i == 0) | (u8::from(incl) << 1);
            b.jump(Code::RowidBound, r, done, 0, 0, flags);
            *slot = Some(r);
        }
    }
    out
}

/// The program of a legacy scan plan (`Plan::Scan`): `out` lists the
/// result columns (`None`: the rowid), `aff` their affinities.
pub(crate) fn scan(
    root: u32,
    rowid_alias: Option<usize>,
    ncols: usize,
    out: &[Option<usize>],
    aff: &[Affinity],
    defaults: &[OwnedValue],
    access: ScanAccess,
) -> Option<Program> {
    let mut b = Builder::new();
    let end = b.label();
    if let ScanAccess::Empty = access {
        b.here(end);
        b.emit(Code::Halt, 0, 0, 0, 0, 0);
        return b.finish();
    }
    // The decode forces REAL only for the result columns that are
    // REAL (the legacy projection's rule).
    let mut affs = AVec::new();
    for _ in 0..ncols {
        affs.push(Affinity::Blob);
    }
    for (o, a) in out.iter().zip(aff.iter()) {
        if let Some(slot) = o.and_then(|i| affs.as_mut_slice().get_mut(i)) {
            *slot = *a;
        }
    }
    let mut defs = AVec::new();
    defs.extend_from_slice(defaults);
    let c = b.cursor();
    let info = b.p4(P4::Table(alloc::boxed::Box::new(TableInfo {
        root,
        ncols,
        rowid_alias,
        affs,
        defaults: defs,
        gcols: None,
    })));
    b.emit(Code::OpenRead, c, 0, 0, info, 0);
    let n = out.len();
    let regs = b.regs(n);
    let top = b.label();
    let one = match access {
        ScanAccess::Rowid(r) => {
            let k = b.reg();
            let v = b.p4(P4::Value(OwnedValue::Integer(r)));
            b.hoist(Code::Value, k, v);
            b.jump(Code::SeekRowid, c, end, k, 0, 0);
            true
        }
        _ => {
            b.jump(Code::Rewind, c, end, 0, 0, 0);
            false
        }
    };
    b.here(top);
    for (i, o) in out.iter().enumerate() {
        let r = regs.saturating_add(i as u32);
        match o {
            Some(col) => b.emit(Code::Column, c, *col as u32, r, 0, 0),
            None => b.emit(Code::Rowid, c, r, 0, 0, 0),
        }
    }
    b.emit(Code::ResultRow, regs, n as u32, cursor_ref(Some(c)), 0, 0);
    if !one {
        b.jump(Code::Next, c, top, 0, 0, 0);
    }
    b.here(end);
    b.emit(Code::Halt, 0, 0, 0, 0, 0);
    b.finish()
}
