//! Window functions (v0.16, feature `window`).
//!
//! A window call `f(args) [FILTER (WHERE ...)] OVER (PARTITION BY
//! ... ORDER BY ... frame)` resolves to an `RExpr::Win` node: a
//! `WinCall` (what the call is) and its children — the arguments,
//! the FILTER, the PARTITION BY and the ORDER BY terms, which the
//! executor evaluates for every input row of the window (a joined
//! row, or a group of an aggregate query). `compute` then runs the
//! windows over the whole input and gives each call a value per
//! input row; `fill` puts those values in place of the `Win` nodes
//! before the result columns and ORDER BY terms evaluate.
//!
//! The model is the pin's (`sqlite3WindowRewrite` and
//! `sqlite3WindowCodeStep`, read off its `EXPLAIN` output):
//!
//! - Calls whose windows are equal (partition, order, frame and
//!   EXCLUDE all the same — `sqlite3WindowCompare`) form one
//!   *class*; classes are numbered in resolution order. The pin
//!   evaluates the first class over a subquery that evaluates the
//!   second, and so on, each sorting its input stably by its
//!   partition and order keys, so `compute` sorts for the last
//!   class first and the first class last; the rows come out in
//!   the first class's order (what the pin returns without an
//!   outer ORDER BY).
//! - The built-in functions take the pin's fixed frames
//!   (`sqlite3WindowUpdate`: `row_number` ROWS UNBOUNDED PRECEDING
//!   to CURRENT ROW, `rank` / `dense_rank` RANGE, `percent_rank`
//!   GROUPS CURRENT ROW to UNBOUNDED FOLLOWING, `cume_dist` GROUPS
//!   1 FOLLOWING to UNBOUNDED FOLLOWING, `ntile` ROWS CURRENT ROW to
//!   UNBOUNDED FOLLOWING, `lead` ROWS UNBOUNDED to UNBOUNDED, `lag`
//!   ROWS UNBOUNDED PRECEDING to CURRENT ROW), which decide their
//!   class.
//! - Each partition is run through a transcription of the pin's
//!   frame program (`Sim`): rows arrive one at a time into the
//!   partition buffer, and three cursors (start, current, end)
//!   step rows into the aggregate, invert them out of it and
//!   return rows, with the pin's countdown registers, peer-group
//!   loops and RANGE offset tests. The aggregates that keep state
//!   (`sum` / `total` / `avg`: the pin's Kahan–Babuška–Neumaier sum
//!   with its `sumInverse`; `group_concat` / `string_agg`: its
//!   byte-length bookkeeping) follow exactly that step / inverse
//!   sequence, so REAL sums round as on the pin and an INTEGER sum
//!   turns REAL once a REAL row has passed through the frame;
//!   everything else is computed from the frame the cursors
//!   delimit. A frame with an EXCLUDE clause (NO OTHERS included)
//!   is the pin's full scan: a fresh aggregate over the frame's
//!   rows minus the excluded ones, for every row.
//!
//! Only core `f64` arithmetic; no float math intrinsics.

use crate::agg::SumCtx;
use crate::arena::AVec;
use crate::error::SchemaKind;
use crate::query::{AggKind, RExpr, truthy};
use crate::record::{Affinity, OwnedValue, apply_affinity, compare_coll, compare_values};
use crate::sql::ast::{FrameExclude, FrameUnit};
use crate::sql::{BinOp, SortOrder};

/// What a window call computes (v0.16, `window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinFn {
    /// An aggregate used as a window function.
    Agg(AggKind),
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
}

impl WinFn {
    /// The built-in window function `name` takes (`None`: not
    /// one), with its argument count range.
    pub fn builtin(name: &[u8]) -> Option<(WinFn, usize, usize)> {
        const FNS: &[(&[u8], WinFn, usize, usize)] = &[
            (b"row_number", WinFn::RowNumber, 0, 0),
            (b"rank", WinFn::Rank, 0, 0),
            (b"dense_rank", WinFn::DenseRank, 0, 0),
            (b"percent_rank", WinFn::PercentRank, 0, 0),
            (b"cume_dist", WinFn::CumeDist, 0, 0),
            (b"ntile", WinFn::Ntile, 1, 1),
            (b"lag", WinFn::Lag, 1, 3),
            (b"lead", WinFn::Lead, 1, 3),
            (b"first_value", WinFn::FirstValue, 1, 1),
            (b"last_value", WinFn::LastValue, 1, 1),
            (b"nth_value", WinFn::NthValue, 2, 2),
        ];
        FNS.iter()
            .find(|(n, ..)| crate::sql::eq_ci(name, n))
            .map(|(_, f, lo, hi)| (*f, *lo, *hi))
    }

    /// Whether the pin gives this function a fixed frame
    /// (`sqlite3WindowUpdate`); first_value / last_value /
    /// nth_value and the aggregates keep the window's.
    fn fixed_frame(self) -> Option<WinFrame> {
        use Bound::*;
        let (unit, start, end) = match self {
            WinFn::RowNumber | WinFn::Lag => (FrameUnit::Rows, UnboundedPreceding, CurrentRow),
            WinFn::Rank | WinFn::DenseRank => (FrameUnit::Range, UnboundedPreceding, CurrentRow),
            WinFn::PercentRank => (FrameUnit::Groups, CurrentRow, UnboundedFollowing),
            WinFn::CumeDist => (
                FrameUnit::Groups,
                Following(RExpr::Literal(OwnedValue::Integer(1))),
                UnboundedFollowing,
            ),
            WinFn::Ntile => (FrameUnit::Rows, CurrentRow, UnboundedFollowing),
            WinFn::Lead => (FrameUnit::Rows, UnboundedPreceding, UnboundedFollowing),
            _ => return None,
        };
        Some(WinFrame {
            unit,
            start,
            end,
            exclude: FrameExclude::None,
        })
    }
}

/// A resolved frame boundary (v0.16, `window`): offsets are
/// constant expressions, evaluated when a partition starts.
#[derive(Debug, Clone)]
pub enum Bound {
    UnboundedPreceding,
    Preceding(RExpr),
    CurrentRow,
    Following(RExpr),
    UnboundedFollowing,
}

impl Bound {
    fn kind(&self) -> u8 {
        match self {
            Bound::UnboundedPreceding => 0,
            Bound::Preceding(_) => 1,
            Bound::CurrentRow => 2,
            Bound::Following(_) => 3,
            Bound::UnboundedFollowing => 4,
        }
    }
    fn offset(&self) -> Option<&RExpr> {
        match self {
            Bound::Preceding(e) | Bound::Following(e) => Some(e),
            _ => None,
        }
    }
}

/// A resolved window frame (v0.16, `window`).
#[derive(Debug, Clone)]
pub struct WinFrame {
    pub unit: FrameUnit,
    pub start: Bound,
    pub end: Bound,
    pub exclude: FrameExclude,
}

impl WinFrame {
    /// The pin's implicit frame: RANGE BETWEEN UNBOUNDED PRECEDING
    /// AND CURRENT ROW.
    pub fn implicit() -> WinFrame {
        WinFrame {
            unit: FrameUnit::Range,
            start: Bound::UnboundedPreceding,
            end: Bound::CurrentRow,
            exclude: FrameExclude::None,
        }
    }

    fn same(&self, o: &WinFrame) -> bool {
        let off = |a: Option<&RExpr>, b: Option<&RExpr>| match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => crate::query::same_expr(x, y),
            _ => false,
        };
        self.unit == o.unit
            && self.exclude == o.exclude
            && self.start.kind() == o.start.kind()
            && self.end.kind() == o.end.kind()
            && off(self.start.offset(), o.start.offset())
            && off(self.end.offset(), o.end.offset())
    }
}

/// One window call (v0.16, `window`; see `RExpr::Win`). Its node's
/// children are `nargs` arguments, then the FILTER when `filter`,
/// then `npart` PARTITION BY terms, then one ORDER BY term per
/// `order` entry (whose collations the planner has set).
#[derive(Debug, Clone)]
pub struct WinCall {
    pub func: WinFn,
    /// `count(*)` / `count()`.
    pub star: bool,
    pub nargs: usize,
    pub filter: bool,
    pub npart: usize,
    pub order: AVec<SortOrder>,
    /// The frame the call runs over (a built-in's fixed one).
    pub frame: WinFrame,
    /// The call's number within its SELECT core, in resolution
    /// order.
    pub slot: usize,
    /// The call's window class within the core (`classify`).
    pub class: usize,
    /// An ORDER BY copy of a result column's call (`order_copies`):
    /// never in the outermost class.
    pub copy: bool,
    /// The slot whose value this node shows (`order_copies`; None:
    /// its own).
    pub shown: Option<usize>,
}

impl WinCall {
    /// A call of `func` over a window with `frame` (None: the
    /// implicit one); a built-in's fixed frame replaces it.
    pub fn new(
        func: WinFn,
        star: bool,
        nargs: usize,
        filter: bool,
        npart: usize,
        order: AVec<SortOrder>,
        frame: Option<WinFrame>,
    ) -> WinCall {
        let frame = func
            .fixed_frame()
            .or(frame)
            .unwrap_or_else(WinFrame::implicit);
        WinCall {
            func,
            star,
            nargs,
            filter,
            npart,
            order,
            frame,
            slot: 0,
            class: 0,
            copy: false,
            shown: None,
        }
    }

    fn part_at(&self) -> usize {
        self.nargs + usize::from(self.filter)
    }

    fn order_at(&self) -> usize {
        self.part_at() + self.npart
    }
}

// ---------------------------------------------------------------
// Walking the Win nodes of an expression.

/// Call `f` on every direct child of `e` that evaluates against the
/// same row (subquery plans are other cores and are not entered).
fn each_child<'x>(e: &'x RExpr, f: &mut dyn FnMut(&'x RExpr)) {
    match e {
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => {}
        RExpr::InSub(x, ..)
        | RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => f(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            f(a);
            f(b);
        }
        RExpr::InList(x, l, ..) => {
            f(x);
            l.iter().for_each(f);
        }
        RExpr::Between(x, a, b, _) => {
            f(x);
            f(a);
            f(b);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                f(b);
            }
            for (w, t) in whens.iter() {
                f(w);
                f(t);
            }
            if let Some(o) = otherwise {
                f(o);
            }
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
            args.iter().for_each(f)
        }
    }
}

/// `each_child` over mutable children.
fn each_child_mut(e: &mut RExpr, f: &mut dyn FnMut(&mut RExpr)) {
    match e {
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => {}
        RExpr::InSub(x, ..)
        | RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => f(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            f(a);
            f(b);
        }
        RExpr::InList(x, l, ..) => {
            f(x);
            l.as_mut_slice().iter_mut().for_each(f);
        }
        RExpr::Between(x, a, b, _) => {
            f(x);
            f(a);
            f(b);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                f(b);
            }
            for (w, t) in whens.as_mut_slice() {
                f(w);
                f(t);
            }
            if let Some(o) = otherwise {
                f(o);
            }
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
            args.as_mut_slice().iter_mut().for_each(f)
        }
    }
}

/// Visit the Win nodes of `e` (not inside subqueries), outermost
/// first. A Win node's children hold none (the resolver refuses
/// nested window calls).
fn visit<'x>(e: &'x RExpr, f: &mut dyn FnMut(&'x RExpr)) {
    if let RExpr::Win(..) = e {
        f(e);
        return;
    }
    each_child(e, &mut |c| visit(c, f));
}

/// Whether `e` holds a window call outside its subqueries.
pub fn has_win(e: &RExpr) -> bool {
    let mut any = false;
    visit(e, &mut |_| any = true);
    any
}

/// The distinct window calls of a core's result columns and ORDER
/// BY terms (an alias or ordinal repeats a node), in slot order.
pub fn calls<'x>(items: &'x [RExpr], order: &'x [(RExpr, SortOrder)]) -> AVec<&'x RExpr> {
    let mut out: AVec<&RExpr> = AVec::new();
    let mut add = |e: &'x RExpr| {
        if let RExpr::Win(c, _) = e
            && !out
                .iter()
                .any(|o| matches!(o, RExpr::Win(d, _) if d.slot == c.slot))
        {
            let at = out
                .iter()
                .position(|o| matches!(o, RExpr::Win(d, _) if d.slot > c.slot))
                .unwrap_or(out.len());
            out.insert_at(at, e);
        }
    };
    for e in items.iter().chain(order.iter().map(|(e, _)| e)) {
        visit(e, &mut add);
    }
    out
}

/// Number the window classes of a core (`WinCall::class`), as
/// the pin nests them (`sqlite3WindowLink` / `sqlite3WindowRewrite`):
/// class 0 is the first call's window with every later call of the
/// same window — ORDER BY copies (`order_copies`) excepted, which
/// the pin never links there; every other call is pushed into a
/// subquery, whose first call's window is class 1 (copies now
/// included), and so on. Calls are taken in slot order.
pub fn classify(items: &mut [RExpr], order: &mut [(RExpr, SortOrder)]) {
    let mut classes: AVec<(usize, usize)> = AVec::new();
    {
        let list = calls(items, order);
        let mut left: AVec<&RExpr> = list;
        let mut class = 0usize;
        while !left.is_empty() {
            let top = class == 0;
            let head = left
                .iter()
                .position(|e| matches!(e, RExpr::Win(c, _) if !(top && c.copy)))
                .unwrap_or(0);
            let Some(&h) = left.as_slice().get(head) else {
                break;
            };
            let RExpr::Win(hc, hk) = h else { break };
            let mut rest = AVec::new();
            for e in left.iter() {
                let RExpr::Win(c, kids) = e else { continue };
                if !(top && c.copy) && same_window(c, kids.as_slice(), hc, hk.as_slice()) {
                    classes.push((c.slot, class));
                } else {
                    rest.push(*e);
                }
            }
            if rest.len() == left.len() {
                // The head itself always joins (a lone copy).
                classes.push((hc.slot, class));
                let mut keep = AVec::new();
                for e in rest.iter() {
                    if !matches!(e, RExpr::Win(c, _) if c.slot == hc.slot) {
                        keep.push(*e);
                    }
                }
                rest = keep;
            }
            left = rest;
            class += 1;
        }
    }
    let set = |e: &mut RExpr| set_class(e, classes.as_slice());
    for e in items.iter_mut() {
        set(e);
    }
    for (e, _) in order.iter_mut() {
        set(e);
    }
}

/// The pin's ORDER BY copies (v0.16 review fix, `window`): an ORDER
/// BY term naming a result column (`refs[i]`: by alias, ordinal or
/// as the same expression) is a copy of that column's expression
/// (`resolveAlias`), and so is an alias inside any other term —
/// its window calls are separate calls, computed again, nested
/// below the result list's own (`classify`). Every ORDER BY call
/// is renumbered after the result columns' slots, in walk order.
/// A result column a reference term names then shows the copy's
/// values (the pin reads it back from the sorter key) — except
/// under DISTINCT, where the term sorts by the column's own.
pub fn order_copies(
    items: &mut [RExpr],
    order: &mut [(RExpr, SortOrder)],
    refs: &[Option<usize>],
    distinct: bool,
) {
    let mut item_slots: AVec<usize> = AVec::new();
    for e in items.iter() {
        visit(e, &mut |w| {
            if let RExpr::Win(c, _) = w {
                item_slots.push(c.slot);
            }
        });
    }
    let mut next = item_slots.iter().map(|s| s + 1).max().unwrap_or(0);
    for (i, (e, _)) in order.iter_mut().enumerate() {
        let whole = refs.get(i).copied().flatten().is_some();
        visit_mut(e, &mut |c| {
            c.copy = whole || item_slots.as_slice().contains(&c.slot);
            c.slot = next;
            next = next.saturating_add(1);
        });
    }
    // The last term naming a column wins (the pin's loop).
    for j in 0..items.len() {
        let Some(i) = refs.iter().rposition(|r| *r == Some(j)) else {
            continue;
        };
        let (Some(it), Some((oe, _))) = (items.get_mut(j), order.get_mut(i)) else {
            continue;
        };
        let mut src: AVec<usize> = AVec::new();
        visit(it, &mut |w| {
            if let RExpr::Win(c, _) = w {
                src.push(c.slot);
            }
        });
        let mut dst: AVec<usize> = AVec::new();
        visit(oe, &mut |w| {
            if let RExpr::Win(c, _) = w {
                dst.push(c.slot);
            }
        });
        let (from, to) = if distinct { (oe, &src) } else { (it, &dst) };
        let mut k = 0usize;
        visit_mut(from, &mut |c| {
            c.shown = to.as_slice().get(k).copied();
            k += 1;
        });
    }
}

/// The DESC flag of each sort key of a core's innermost window
/// class (partition terms ascending, then its ORDER BY), or None
/// without window calls (v0.16 review fix). The pin's rewrite puts
/// the grouping in the innermost subquery with that sort as its
/// ORDER BY, whose directions the group sorter then borrows.
pub fn inner_sort_desc(items: &[RExpr], order: &[(RExpr, SortOrder)]) -> Option<AVec<bool>> {
    let list = calls(items, order);
    let inner = list
        .iter()
        .filter_map(|e| match e {
            RExpr::Win(c, _) => Some(c),
            _ => None,
        })
        .fold(None::<&WinCall>, |m, c| match m {
            Some(b) if b.class >= c.class => Some(b),
            _ => Some(c),
        })?;
    let mut out = AVec::new();
    for _ in 0..inner.npart {
        out.push(false);
    }
    for o in inner.order.iter() {
        out.push(o.desc);
    }
    Some(out)
}

/// The result columns each core of a compound sorts by on the pin
/// (v0.16 review fix, `window`; `multiSelectOrderBy`): the
/// compound's ORDER BY columns `order`, which every UNION /
/// INTERSECT / EXCEPT (`ops[i]` joins core `i + 1`) extends with
/// the remaining columns for its two sides; index 0 is the head.
pub fn compound_sorts(
    ops: &[crate::sql::CompoundOp],
    order: &[usize],
    width: usize,
) -> AVec<AVec<usize>> {
    let mut t: AVec<usize> = AVec::new();
    for &j in order {
        t.push(j);
    }
    let mut out: AVec<AVec<usize>> = AVec::new();
    for _ in 0..=ops.len() {
        out.push(AVec::new());
    }
    for i in (0..ops.len()).rev() {
        if ops.get(i) != Some(&crate::sql::CompoundOp::UnionAll) {
            for j in 0..width {
                if !t.as_slice().contains(&j) {
                    t.push(j);
                }
            }
        }
        if let Some(o) = out.as_mut_slice().get_mut(i + 1) {
            *o = t.clone();
        }
    }
    if let Some(o) = out.as_mut_slice().first_mut() {
        *o = t;
    }
    out
}

/// One compound core's share of `compound_sorts` (v0.16 review fix,
/// `window`): the columns `cols` it sorts by become ORDER BY copies
/// of its window columns (`order_copies`), kept in its `order`
/// (a compound core's ORDER BY is otherwise empty and never sorts
/// it), and its classes are numbered again.
pub fn compound_copies(
    items: &mut [RExpr],
    order: &mut AVec<(RExpr, SortOrder)>,
    cols: &[usize],
    distinct: bool,
) {
    let mut refs: AVec<Option<usize>> = AVec::new();
    for &j in cols {
        if let Some(e) = items.get(j)
            && has_win(e)
        {
            order.push((
                e.clone(),
                SortOrder {
                    desc: false,
                    nulls_first: true,
                    coll: Default::default(),
                },
            ));
            refs.push(Some(j));
        }
    }
    if refs.is_empty() {
        return;
    }
    order_copies(items, order.as_mut_slice(), refs.as_slice(), distinct);
    classify(items, order.as_mut_slice());
}

/// `visit`, mutably: `f` on every Win node's call.
fn visit_mut(e: &mut RExpr, f: &mut dyn FnMut(&mut WinCall)) {
    if let RExpr::Win(c, _) = e {
        f(c);
        return;
    }
    each_child_mut(e, &mut |x| visit_mut(x, f));
}

/// Whether two resolved window calls are the same call (the pin's
/// `sqlite3ExprCompare` of an ORDER BY term with a result column):
/// function, arguments and window alike.
pub fn same_call(a: &RExpr, b: &RExpr) -> bool {
    let (RExpr::Win(x, xk), RExpr::Win(y, yk)) = (a, b) else {
        return false;
    };
    x.func == y.func
        && x.star == y.star
        && x.nargs == y.nargs
        && x.filter == y.filter
        && same_window(x, xk.as_slice(), y, yk.as_slice())
        && xk.len() == yk.len()
        && xk
            .iter()
            .zip(yk.iter())
            .all(|(p, q)| crate::query::same_expr(p, q))
}

fn set_class(e: &mut RExpr, classes: &[(usize, usize)]) {
    if let RExpr::Win(c, _) = e {
        if let Some((_, k)) = classes.iter().find(|(s, _)| *s == c.slot) {
            c.class = *k;
        }
        return;
    }
    each_child_mut(e, &mut |x| set_class(x, classes));
}

/// `sqlite3WindowCompare` without the FILTER: frame, PARTITION BY
/// and ORDER BY (directions included) all equal.
fn same_window<'x>(a: &WinCall, ak: &'x [RExpr], b: &WinCall, bk: &'x [RExpr]) -> bool {
    let part = |c: &WinCall, k: &'x [RExpr]| k.get(c.part_at()..c.order_at()).unwrap_or(&[]);
    let ord = |c: &WinCall, k: &'x [RExpr]| k.get(c.order_at()..).unwrap_or(&[]);
    let (ap, ao, bp, bo) = (part(a, ak), ord(a, ak), part(b, bk), ord(b, bk));
    let same = |x: &[RExpr], y: &[RExpr]| {
        x.len() == y.len()
            && x.iter()
                .zip(y.iter())
                .all(|(p, q)| crate::query::same_expr(p, q))
    };
    a.frame.same(&b.frame)
        && same(ap, bp)
        && same(ao, bo)
        && a.order.len() == b.order.len()
        && a.order
            .iter()
            .zip(b.order.iter())
            .all(|(x, y)| x.desc == y.desc && x.nulls_first == y.nulls_first)
}

/// Replace every window call in `e` (outside subqueries) by its
/// value: `vals[slot]`.
pub fn fill(e: &mut RExpr, vals: &[OwnedValue]) {
    if let RExpr::Win(c, _) = e {
        let v = vals
            .get(c.shown.unwrap_or(c.slot))
            .cloned()
            .unwrap_or(OwnedValue::Null);
        *e = RExpr::Literal(v);
        return;
    }
    each_child_mut(e, &mut |x| fill(x, vals));
}

/// The children of a window call node (empty for any other node).
pub fn children(e: &RExpr) -> &[RExpr] {
    match e {
        RExpr::Win(_, k) => k.as_slice(),
        _ => &[],
    }
}

// ---------------------------------------------------------------
// The computation.

/// The windows of one SELECT core over its whole input (v0.16,
/// `window`): `calls` as `calls` lists them, and per input row
/// (`inputs[u]`) each call's children's values, in `calls` order.
/// Returns the order the rows come out in (the first class's sort)
/// and, per input row, every call's value indexed by its slot.
pub fn compute(
    calls: &[&RExpr],
    inputs: &[AVec<AVec<OwnedValue>>],
) -> Result<(AVec<usize>, AVec<AVec<OwnedValue>>), SchemaKind> {
    let n = inputs.len();
    let mut perm: AVec<usize> = AVec::with_capacity(n);
    for u in 0..n {
        perm.push(u);
    }
    let nslots = calls
        .iter()
        .filter_map(|e| match e {
            RExpr::Win(c, _) => Some(c.slot + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut res: AVec<AVec<OwnedValue>> = AVec::with_capacity(n);
    for _ in 0..n {
        let mut r = AVec::with_capacity(nslots);
        for _ in 0..nslots {
            r.push(OwnedValue::Null);
        }
        res.push(r);
    }
    let mut parsed: AVec<(&WinCall, &[RExpr])> = AVec::new();
    for e in calls {
        if let RExpr::Win(c, k) = e {
            parsed.push((c, k.as_slice()));
        }
    }
    let nclass = parsed.iter().map(|(c, _)| c.class + 1).max().unwrap_or(0);
    for class in (0..nclass).rev() {
        let mut members: AVec<usize> = AVec::new();
        for (i, (c, _)) in parsed.iter().enumerate() {
            if c.class == class {
                members.push(i);
            }
        }
        let Some(&rep) = members.as_slice().first() else {
            continue;
        };
        let Some(&(rc, rk)) = parsed.as_slice().get(rep) else {
            continue;
        };
        // The class's sort: partition terms ascending (NULLs
        // first, each term's collation), then the ORDER BY.
        let mut dirs: AVec<SortOrder> = AVec::new();
        for p in rk.get(rc.part_at()..rc.order_at()).unwrap_or(&[]) {
            dirs.push(SortOrder {
                desc: false,
                nulls_first: true,
                coll: crate::query::expr_coll(p).unwrap_or_default(),
            });
        }
        let npart = dirs.len();
        dirs.extend_from_slice(rc.order.as_slice());
        let keys = |u: usize| -> &[OwnedValue] {
            inputs
                .get(u)
                .and_then(|r| r.as_slice().get(rep))
                .and_then(|v| v.as_slice().get(rc.part_at()..))
                .unwrap_or(&[])
        };
        let cmp = |a: usize, b: usize| {
            crate::query::order_keys_cmp(keys(a), keys(b), dirs.iter().copied())
        };
        crate::agg::sort_stable(&mut perm, &|a, b| cmp(a, b) != core::cmp::Ordering::Greater);
        // Partitions: runs of rows with equal partition keys.
        let mut lo = 0usize;
        while lo < n {
            let Some(&first) = perm.as_slice().get(lo) else {
                break;
            };
            let mut hi = lo + 1;
            while let Some(&u) = perm.as_slice().get(hi) {
                let a = keys(first).get(..npart).unwrap_or(&[]);
                let b = keys(u).get(..npart).unwrap_or(&[]);
                if crate::query::order_keys_cmp(a, b, dirs.iter().copied())
                    != core::cmp::Ordering::Equal
                {
                    break;
                }
                hi += 1;
            }
            let rows = perm.as_slice().get(lo..hi).unwrap_or(&[]);
            run_partition(
                rc,
                rep,
                members.as_slice(),
                parsed.as_slice(),
                inputs,
                rows,
                &mut res,
            )?;
            lo = hi;
        }
    }
    Ok((perm, res))
}

/// One partition of one class: run the frame program, computing
/// every member's values into `res`.
#[allow(clippy::too_many_arguments)]
fn run_partition(
    rc: &WinCall,
    rep: usize,
    members: &[usize],
    parsed: &[(&WinCall, &[RExpr])],
    inputs: &[AVec<AVec<OwnedValue>>],
    rows: &[usize],
    res: &mut AVec<AVec<OwnedValue>>,
) -> Result<(), SchemaKind> {
    let mut okeys: AVec<&[OwnedValue]> = AVec::with_capacity(rows.len());
    for &u in rows {
        okeys.push(
            inputs
                .get(u)
                .and_then(|r| r.as_slice().get(rep))
                .and_then(|v| v.as_slice().get(rc.order_at()..))
                .unwrap_or(&[]),
        );
    }
    let mut ms: AVec<Member<'_>> = AVec::new();
    for &i in members {
        if let Some(&(c, k)) = parsed.get(i) {
            ms.push(Member::new(c, k, i));
        }
    }
    // Peer groups (the ranking functions): each position's group
    // number, and each group's first and one-past-last position.
    let mut gno: AVec<usize> = AVec::with_capacity(rows.len());
    let mut gbounds: AVec<(usize, usize)> = AVec::new();
    let peers = |a: usize, b: usize| {
        let (Some(x), Some(y)) = (okeys.as_slice().get(a), okeys.as_slice().get(b)) else {
            return false;
        };
        crate::query::order_keys_cmp(x, y, rc.order.iter().copied()) == core::cmp::Ordering::Equal
    };
    for p in 0..rows.len() {
        if p == 0 || !peers(p, p - 1) {
            gbounds.push((p, p));
        }
        gno.push(gbounds.len().saturating_sub(1));
        if let Some(b) = gbounds.as_mut_slice().last_mut() {
            b.1 = p + 1;
        }
    }
    let part = Part {
        rows,
        inputs,
        keys: okeys.as_slice(),
        order: rc.order.as_slice(),
        gno: gno.as_slice(),
        gbounds: gbounds.as_slice(),
    };
    let full = rc.frame.exclude != FrameExclude::None;
    let mut sink = |ev: Ev| -> Result<(), SchemaKind> {
        for m in ms.as_mut_slice() {
            m.event(ev, &part, full, res)?;
        }
        Ok(())
    };
    let mut sim = Sim {
        frame: &rc.frame,
        order: rc.order.as_slice(),
        keys: okeys.as_slice(),
        offsets: (
            eval_offset(rc.frame.start.offset())?,
            eval_offset(rc.frame.end.offset())?,
        ),
        ins: 0,
        newest: None,
        start: None,
        cur: None,
        end: None,
        sreg: 0,
        creg: 0,
        ereg: 0,
        peer: 0,
        rs: Reg::None,
        re: Reg::None,
        out: &mut sink,
    };
    sim.run(rows.len())
}

/// A frame offset's value (a constant expression, evaluated
/// against no row).
fn eval_offset(e: Option<&RExpr>) -> Result<Option<OwnedValue>, SchemaKind> {
    let Some(e) = e else {
        return Ok(None);
    };
    let empty = crate::query::RowData::empty();
    match crate::query::eval::<()>(e, &crate::query::Ctx::of(&empty)) {
        Ok(v) => Ok(Some(v)),
        Err(crate::error::Error::Schema { kind }) => Err(kind),
        Err(_) => Err(SchemaKind::FrameStartOffset),
    }
}

/// The rows of one partition, in window order.
struct Part<'a> {
    /// Input row (unit) of each partition position.
    rows: &'a [usize],
    inputs: &'a [AVec<AVec<OwnedValue>>],
    /// The class's ORDER BY values per position.
    keys: &'a [&'a [OwnedValue]],
    order: &'a [SortOrder],
    /// Each position's peer group, and each group's bounds.
    gno: &'a [usize],
    gbounds: &'a [(usize, usize)],
}

impl Part<'_> {
    /// Call `i`'s children values at partition position `pos`.
    fn kids(&self, pos: usize, i: usize) -> &[OwnedValue] {
        self.rows
            .get(pos)
            .and_then(|&u| self.inputs.get(u))
            .and_then(|r| r.as_slice().get(i))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn peers(&self, a: usize, b: usize) -> bool {
        let (Some(x), Some(y)) = (self.keys.get(a), self.keys.get(b)) else {
            return false;
        };
        crate::query::order_keys_cmp(x, y, self.order.iter().copied()) == core::cmp::Ordering::Equal
    }

    /// The peer group of `pos`: (number, first, one past last).
    fn group(&self, pos: usize) -> (usize, usize, usize) {
        let g = self.gno.get(pos).copied().unwrap_or(0);
        let (a, b) = self.gbounds.get(g).copied().unwrap_or((pos, pos + 1));
        (g, a, b)
    }
}

/// One event of the frame program.
#[derive(Clone, Copy)]
enum Ev {
    /// A partition's first row: every accumulator restarts.
    Init,
    /// The row at this position enters the aggregate.
    Step(usize),
    /// The row at this position leaves it.
    Inverse(usize),
    /// The row at `pos` is returned while `ins` rows of the
    /// partition are buffered.
    Return { pos: usize, ins: usize },
}

/// A countdown / offset register of the frame program.
#[derive(Clone)]
enum Reg {
    None,
    /// ROWS / GROUPS: the integer countdown.
    Count(i64),
    /// RANGE: the offset value.
    Val(OwnedValue),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Return,
    Inverse,
    Step,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cd {
    No,
    Start,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Ge,
    Gt,
    Le,
    Lt,
}

impl Cmp {
    fn holds(self, o: core::cmp::Ordering) -> bool {
        use core::cmp::Ordering::*;
        match self {
            Cmp::Ge => o != Less,
            Cmp::Gt => o == Greater,
            Cmp::Le => o != Greater,
            Cmp::Lt => o == Less,
        }
    }
}

/// The pin's frame program for one partition (a transcription of
/// `sqlite3WindowCodeStep` / `windowCodeOp` /
/// `windowCodeRangeTest`). Cursor positions are partition
/// positions; `None` is a cursor that ran off the buffer.
struct Sim<'a> {
    frame: &'a WinFrame,
    order: &'a [SortOrder],
    keys: &'a [&'a [OwnedValue]],
    /// The start / end offsets' values (None: no offset).
    offsets: (Option<OwnedValue>, Option<OwnedValue>),
    /// Rows buffered.
    ins: usize,
    /// The row just inserted while the input loop runs (the pin's
    /// `regRowid`; None in the flush).
    newest: Option<usize>,
    start: Option<usize>,
    cur: Option<usize>,
    end: Option<usize>,
    /// The peer-value registers: the row whose ORDER BY values
    /// each holds.
    sreg: usize,
    creg: usize,
    ereg: usize,
    peer: usize,
    rs: Reg,
    re: Reg,
    out: &'a mut dyn FnMut(Ev) -> Result<(), SchemaKind>,
}

impl Sim<'_> {
    fn unit(&self) -> FrameUnit {
        self.frame.unit
    }
    fn sk(&self) -> u8 {
        self.frame.start.kind()
    }
    fn ek(&self) -> u8 {
        self.frame.end.kind()
    }

    fn peers(&self, a: usize, b: usize) -> bool {
        let (Some(x), Some(y)) = (self.keys.get(a), self.keys.get(b)) else {
            return false;
        };
        crate::query::order_keys_cmp(x, y, self.order.iter().copied()) == core::cmp::Ordering::Equal
    }

    /// Check and load the offsets (the partition's first row).
    fn load(&mut self) -> Result<(), SchemaKind> {
        let range = self.unit() == FrameUnit::Range;
        self.rs = match &self.offsets.0 {
            None => Reg::None,
            Some(v) => check_offset(v, range, SchemaKind::FrameStartOffset)?,
        };
        self.re = match &self.offsets.1 {
            None => Reg::None,
            Some(v) => check_offset(v, range, SchemaKind::FrameEndOffset)?,
        };
        Ok(())
    }

    fn run(&mut self, n: usize) -> Result<(), SchemaKind> {
        const UP: u8 = 0;
        const P: u8 = 1;
        const F: u8 = 3;
        const UF: u8 = 4;
        let range = self.unit() == FrameUnit::Range;
        let rows = self.unit() == FrameUnit::Rows;
        let has_rs = self.frame.start.offset().is_some();
        let has_re = self.frame.end.offset().is_some();
        for k in 0..n {
            self.ins += 1;
            self.newest = Some(k);
            if self.ins == 1 {
                // The partition's first row (or the first row after
                // the empty-frame path emptied the buffer).
                (self.out)(Ev::Init)?;
                self.load()?;
                if !range && self.sk() == self.ek() && has_rs {
                    let (Reg::Count(s), Reg::Count(e)) = (&self.rs, &self.re) else {
                        return Err(SchemaKind::FrameStartOffset);
                    };
                    let nonempty = if self.sk() == F { e >= s } else { e <= s };
                    if !nonempty {
                        // Every frame is empty: return the row from
                        // a fresh accumulator and drop it.
                        (self.out)(Ev::Return { pos: k, ins: 1 })?;
                        self.ins = 0;
                        continue;
                    }
                }
                if self.sk() == F
                    && !range
                    && has_re
                    && let (Reg::Count(s), Reg::Count(e)) = (&self.rs, &self.re)
                {
                    self.rs = Reg::Count(e.saturating_sub(*s));
                }
                if self.sk() != UP {
                    self.start = Some(k);
                }
                self.cur = Some(k);
                self.end = Some(k);
                if !rows {
                    self.peer = k;
                    self.sreg = k;
                    self.creg = k;
                    self.ereg = k;
                }
                continue;
            }
            if !rows {
                if self.peers(k, self.peer) {
                    continue;
                }
                self.peer = k;
            }
            if self.sk() == F {
                self.op(Op::Step, Cd::No, false)?;
                if self.ek() != UF {
                    if range {
                        while !self.range_test(Cmp::Ge, self.cur, Cd::End, self.end) {
                            self.op(Op::Inverse, Cd::Start, false)?;
                            self.op(Op::Return, Cd::No, false)?;
                        }
                    } else {
                        self.op(Op::Return, Cd::End, false)?;
                        self.op(Op::Inverse, Cd::Start, false)?;
                    }
                }
            } else if self.ek() == P {
                let rps = self.sk() == P && range;
                self.op(Op::Step, Cd::End, false)?;
                if rps {
                    self.op(Op::Inverse, Cd::Start, false)?;
                }
                self.op(Op::Return, Cd::No, false)?;
                if !rps {
                    self.op(Op::Inverse, Cd::Start, false)?;
                }
            } else {
                self.op(Op::Step, Cd::No, false)?;
                if self.ek() != UF {
                    if range {
                        loop {
                            if has_re && self.range_test(Cmp::Ge, self.cur, Cd::End, self.end) {
                                break;
                            }
                            self.op(Op::Return, Cd::No, false)?;
                            self.op(Op::Inverse, self.cd_start(), false)?;
                            if !has_re {
                                break;
                            }
                        }
                    } else if !(has_re && self.ifpos(Cd::End)) {
                        self.op(Op::Return, Cd::No, false)?;
                        self.op(Op::Inverse, self.cd_start(), false)?;
                    }
                }
            }
        }
        // The flush.
        self.newest = None;
        if self.ins == 0 {
            return Ok(());
        }
        if self.ek() == P {
            let rps = self.sk() == P && range;
            self.op(Op::Step, Cd::End, false)?;
            if rps {
                self.op(Op::Inverse, Cd::Start, false)?;
            }
            self.op(Op::Return, Cd::No, false)?;
        } else if self.sk() == F {
            self.op(Op::Step, Cd::No, false)?;
            let mut to_tail = false;
            loop {
                if range {
                    if self.op(Op::Inverse, Cd::Start, true)? {
                        to_tail = true;
                        break;
                    }
                    if self.op(Op::Return, Cd::No, true)? {
                        break;
                    }
                } else if self.ek() == UF {
                    if self.op(Op::Return, Cd::Start, true)? {
                        break;
                    }
                    if self.op(Op::Inverse, Cd::No, true)? {
                        to_tail = true;
                        break;
                    }
                } else {
                    if self.op(Op::Return, Cd::End, true)? {
                        break;
                    }
                    if self.op(Op::Inverse, Cd::Start, true)? {
                        to_tail = true;
                        break;
                    }
                }
            }
            if to_tail {
                while !self.op(Op::Return, Cd::No, true)? {}
            }
        } else {
            self.op(Op::Step, Cd::No, false)?;
            loop {
                if self.op(Op::Return, Cd::No, true)? {
                    break;
                }
                self.op(Op::Inverse, self.cd_start(), false)?;
            }
        }
        Ok(())
    }

    /// The start countdown when the frame has a start offset.
    fn cd_start(&self) -> Cd {
        if self.frame.start.offset().is_some() {
            Cd::Start
        } else {
            Cd::No
        }
    }

    /// `OP_IfPos`: a positive countdown is decremented and the
    /// operation skipped.
    fn ifpos(&mut self, cd: Cd) -> bool {
        let r = match cd {
            Cd::Start => &mut self.rs,
            Cd::End => &mut self.re,
            Cd::No => return false,
        };
        match r {
            Reg::Count(c) if *c > 0 => {
                *c -= 1;
                true
            }
            _ => false,
        }
    }

    /// `windowCodeOp`: one operation (a peer group of rows for
    /// RANGE / GROUPS), with its countdown or RANGE test. Returns
    /// whether its cursor ran off the buffer when `eof_jump`.
    fn op(&mut self, op: Op, cd: Cd, eof_jump: bool) -> Result<bool, SchemaKind> {
        if op == Op::Inverse && self.sk() == 0 {
            return Ok(false);
        }
        let range = self.unit() == FrameUnit::Range;
        let peer = self.unit() != FrameUnit::Rows;
        let range_cd = cd != Cd::No && range;
        if cd != Cd::No && !range && self.ifpos(cd) {
            return Ok(false);
        }
        loop {
            if range_cd {
                let done = match op {
                    Op::Inverse if self.sk() == 3 => {
                        self.range_test(Cmp::Le, self.cur, cd, self.start)
                    }
                    Op::Inverse => self.range_test(Cmp::Ge, self.start, cd, self.cur),
                    Op::Step => self.range_test(Cmp::Gt, self.end, cd, self.cur),
                    Op::Return => false,
                };
                if done {
                    return Ok(false);
                }
            }
            loop {
                // RANGE with both bounds on one side: the start may
                // not pass the end, nor the end the newest row.
                if self.sk() == self.ek() && cd != Cd::No && range {
                    match op {
                        Op::Inverse => {
                            if let (Some(s), Some(e)) = (self.start, self.end)
                                && s >= e
                            {
                                return Ok(false);
                            }
                        }
                        Op::Step => {
                            if let (Some(w), Some(e)) = (self.newest, self.end)
                                && e >= w
                            {
                                return Ok(false);
                            }
                        }
                        Op::Return => {}
                    }
                }
                let csr = match op {
                    Op::Return => self.cur,
                    Op::Inverse => self.start,
                    Op::Step => self.end,
                };
                let Some(pos) = csr else {
                    return Ok(eof_jump);
                };
                (self.out)(match op {
                    Op::Return => Ev::Return { pos, ins: self.ins },
                    Op::Inverse => Ev::Inverse(pos),
                    Op::Step => Ev::Step(pos),
                })?;
                let next = if pos + 1 < self.ins {
                    Some(pos + 1)
                } else {
                    None
                };
                match op {
                    Op::Return => self.cur = next,
                    Op::Inverse => self.start = next,
                    Op::Step => self.end = next,
                }
                let Some(nx) = next else {
                    return Ok(eof_jump);
                };
                if peer {
                    let reg = match op {
                        Op::Return => &mut self.creg,
                        Op::Inverse => &mut self.sreg,
                        Op::Step => &mut self.ereg,
                    };
                    let r = *reg;
                    if self.peers(nx, r) {
                        continue;
                    }
                    match op {
                        Op::Return => self.creg = nx,
                        Op::Inverse => self.sreg = nx,
                        Op::Step => self.ereg = nx,
                    }
                }
                break;
            }
            if !range_cd {
                return Ok(false);
            }
        }
    }

    /// `windowCodeRangeTest`: whether `c1.key (+/-) offset op
    /// c2.key` holds (`+` and `op` as written for ASC; DESC
    /// subtracts and flips the comparison), with the pin's NULL
    /// rules, its "a non-numeric key takes no arithmetic" rule and
    /// the ORDER BY term's collation.
    fn range_test(&self, op: Cmp, c1: Option<usize>, cd: Cd, c2: Option<usize>) -> bool {
        let Some(o) = self.order.first().copied() else {
            return false;
        };
        let key = |c: Option<usize>| -> OwnedValue {
            c.and_then(|p| self.keys.get(p))
                .and_then(|k| k.first())
                .cloned()
                .unwrap_or(OwnedValue::Null)
        };
        let reg = match cd {
            Cd::Start => &self.rs,
            Cd::End => &self.re,
            Cd::No => &Reg::None,
        };
        let v = match reg {
            Reg::Val(v) => v.clone(),
            Reg::Count(c) => OwnedValue::Integer(*c),
            Reg::None => OwnedValue::Null,
        };
        let (a, b) = (key(c1), key(c2));
        let (op, sub) = if o.desc {
            let f = match op {
                Cmp::Ge => Cmp::Le,
                Cmp::Gt => Cmp::Lt,
                Cmp::Le => Cmp::Ge,
                Cmp::Lt => Cmp::Gt,
            };
            (f, true)
        } else {
            (op, false)
        };
        let null = |x: &OwnedValue| matches!(x, OwnedValue::Null);
        // NULLs last for ASC (first for DESC): the pin tests them
        // apart.
        if o.nulls_first == o.desc {
            if null(&a) {
                return match op {
                    Cmp::Ge => true,
                    Cmp::Gt => !null(&b),
                    Cmp::Le => null(&b),
                    Cmp::Lt => false,
                };
            }
            if null(&b) {
                return matches!(op, Cmp::Le | Cmp::Lt);
            }
        }
        let mut a = a;
        if !matches!(a, OwnedValue::Text(_) | OwnedValue::Blob(_)) {
            if ((op == Cmp::Ge && !sub) || (op == Cmp::Le && sub))
                && !null(&a)
                && !null(&b)
                && op.holds(compare_values(&a.as_value(), &b.as_value()))
            {
                return true;
            }
            let bop = if sub { BinOp::Sub } else { BinOp::Add };
            a = crate::query::arith_values::<()>(bop, a, v).unwrap_or(OwnedValue::Null);
        }
        op.holds(compare_coll(&a.as_value(), &b.as_value(), o.coll))
    }
}

/// The pin's `windowCheckValue` for a frame offset: ROWS / GROUPS
/// take a non-negative integer (`OP_MustBeInt`), RANGE a
/// non-negative number (numeric text counts; the value itself is
/// kept for the arithmetic).
fn check_offset(v: &OwnedValue, range: bool, err: SchemaKind) -> Result<Reg, SchemaKind> {
    let num = apply_affinity(v.clone(), Affinity::Numeric);
    if !range {
        return match num {
            OwnedValue::Integer(i) if i >= 0 => Ok(Reg::Count(i)),
            _ => Err(err),
        };
    }
    let ok = match num {
        OwnedValue::Integer(i) => i >= 0,
        OwnedValue::Real(r) => r >= 0.0,
        _ => false,
    };
    if ok {
        Ok(Reg::Val(v.clone()))
    } else {
        Err(err)
    }
}

// ---------------------------------------------------------------
// The functions.

/// One call's state while its class runs a partition.
struct Member<'a> {
    call: &'a WinCall,
    kids: &'a [RExpr],
    /// The call's index in `calls` (its input column).
    i: usize,
    steps: usize,
    invs: usize,
    count: i64,
    sum: SumCtx,
    gc: Concat,
    ntile: i64,
    /// min / max over a frame from UNBOUNDED PRECEDING (never
    /// inverted): the running extremum, the first of equal values
    /// kept (the pin's `minmaxStep`).
    best: Option<OwnedValue>,
}

impl<'a> Member<'a> {
    fn new(call: &'a WinCall, kids: &'a [RExpr], i: usize) -> Member<'a> {
        Member {
            call,
            kids,
            i,
            steps: 0,
            invs: 0,
            count: 0,
            sum: SumCtx::default(),
            gc: Concat::default(),
            ntile: 0,
            best: None,
        }
    }

    /// Whether the row at `pos` passes the call's FILTER.
    fn passes(&self, part: &Part<'_>, pos: usize) -> bool {
        if !self.call.filter {
            return true;
        }
        part.kids(pos, self.i)
            .get(self.call.nargs)
            .is_some_and(|f| truthy(f) == Some(true))
    }

    fn arg(&self, part: &Part<'_>, pos: usize, j: usize) -> OwnedValue {
        part.kids(pos, self.i)
            .get(j)
            .cloned()
            .unwrap_or(OwnedValue::Null)
    }

    fn event(
        &mut self,
        ev: Ev,
        part: &Part<'_>,
        full: bool,
        res: &mut AVec<AVec<OwnedValue>>,
    ) -> Result<(), SchemaKind> {
        match ev {
            Ev::Init => {
                self.steps = 0;
                self.invs = 0;
                self.count = 0;
                self.sum = SumCtx::default();
                self.gc = Concat::default();
                self.ntile = 0;
                self.best = None;
            }
            Ev::Step(pos) => {
                self.steps += 1;
                if !full {
                    self.step(part, pos, false)?;
                }
            }
            Ev::Inverse(pos) => {
                self.invs += 1;
                if !full {
                    self.step(part, pos, true)?;
                }
            }
            Ev::Return { pos, ins } => {
                let v = if full {
                    self.full_scan(part, pos)?
                } else {
                    self.value(part, pos, ins)?
                };
                if let Some(slot) = part
                    .rows
                    .get(pos)
                    .and_then(|&u| res.as_mut_slice().get_mut(u))
                    .and_then(|r| r.as_mut_slice().get_mut(self.call.slot))
                {
                    *slot = v;
                }
            }
        }
        Ok(())
    }

    /// The aggregate step (`inv`: the inverse) of the row at `pos`.
    fn step(&mut self, part: &Part<'_>, pos: usize, inv: bool) -> Result<(), SchemaKind> {
        if self.call.func == WinFn::Ntile && !inv && self.steps == 1 {
            self.ntile = crate::query::int_value(&self.arg(part, pos, 0));
            if self.ntile <= 0 {
                return Err(SchemaKind::NtileArgument);
            }
        }
        let WinFn::Agg(kind) = self.call.func else {
            return Ok(());
        };
        if !self.passes(part, pos) {
            return Ok(());
        }
        let x = self.arg(part, pos, 0);
        let null = matches!(x, OwnedValue::Null);
        match kind {
            AggKind::Count => {
                if self.call.star || !null {
                    self.count += if inv { -1 } else { 1 };
                }
            }
            AggKind::Sum | AggKind::Total | AggKind::Avg => {
                if inv {
                    self.sum.inverse(&x);
                } else {
                    self.sum.step(&x);
                }
            }
            AggKind::GroupConcat => {
                let sep = if self.call.nargs > 1 {
                    Some(crate::query::to_text(&self.arg(part, pos, 1)))
                } else {
                    None
                };
                if inv {
                    self.gc.inverse(&x);
                } else {
                    self.gc.step(&x, sep);
                }
            }
            AggKind::Median
            | AggKind::Percentile
            | AggKind::PercentileCont
            | AggKind::PercentileDisc => {
                // The pin checks each row as it is stepped.
                if !inv {
                    self.fold(part, &[pos])?;
                }
            }
            AggKind::Min | AggKind::Max => {
                if !inv && !null && self.call.frame.start.kind() == 0 {
                    let coll = crate::query::args_coll(self.kids.get(..1).unwrap_or(&[]));
                    let better = match &self.best {
                        None => true,
                        Some(b) => {
                            let o = compare_coll(&x.as_value(), &b.as_value(), coll);
                            if kind == AggKind::Max {
                                o == core::cmp::Ordering::Greater
                            } else {
                                o == core::cmp::Ordering::Less
                            }
                        }
                    };
                    if better {
                        self.best = Some(x);
                    }
                }
            }
        }
        Ok(())
    }

    /// A fresh aggregate over the rows at `poss` (those passing the
    /// FILTER).
    fn fold(&self, part: &Part<'_>, poss: &[usize]) -> Result<OwnedValue, SchemaKind> {
        let WinFn::Agg(kind) = self.call.func else {
            return Ok(OwnedValue::Null);
        };
        let args = self.kids.get(..self.call.nargs).unwrap_or(&[]);
        let mut owned = AVec::new();
        owned.extend_from_slice(args);
        let node = RExpr::Agg {
            kind,
            distinct: false,
            star: self.call.star,
            args: owned,
            lvl: 0,
            inert: false,
            alias: false,
            ext: None,
        };
        let Some(p) = crate::query::agg_parts(&node) else {
            return Ok(OwnedValue::Null);
        };
        let mut rows: AVec<AVec<OwnedValue>> = AVec::new();
        for &pos in poss {
            if !self.passes(part, pos) {
                continue;
            }
            let mut r = AVec::new();
            for j in 0..self.call.nargs {
                r.push(self.arg(part, pos, j));
            }
            rows.push(r);
        }
        crate::agg::fold::<()>(&p, rows).map_err(|e| match e {
            crate::error::Error::Schema { kind } => kind,
            _ => SchemaKind::IntegerOverflow,
        })
    }

    /// The extremum of the frame's non-NULL values, the last of
    /// equal ones (the pin's ephemeral index for a frame that does
    /// not start UNBOUNDED PRECEDING).
    fn extremum(&self, part: &Part<'_>, lo: usize, hi: usize) -> OwnedValue {
        let max = self.call.func == WinFn::Agg(AggKind::Max);
        let coll = crate::query::args_coll(self.kids.get(..1).unwrap_or(&[]));
        let mut best: Option<OwnedValue> = None;
        for pos in lo..hi {
            if !self.passes(part, pos) {
                continue;
            }
            let v = self.arg(part, pos, 0);
            if matches!(v, OwnedValue::Null) {
                continue;
            }
            let take = match &best {
                None => true,
                Some(b) => {
                    let o = compare_coll(&v.as_value(), &b.as_value(), coll);
                    let o = if max { o } else { o.reverse() };
                    match o {
                        core::cmp::Ordering::Greater => true,
                        core::cmp::Ordering::Equal => true,
                        core::cmp::Ordering::Less => false,
                    }
                }
            };
            if take {
                best = Some(v);
            }
        }
        best.unwrap_or(OwnedValue::Null)
    }

    /// The call's value for the row at `pos`, `ins` rows buffered
    /// (the incremental engine).
    fn value(&self, part: &Part<'_>, pos: usize, ins: usize) -> Result<OwnedValue, SchemaKind> {
        let n = part.rows.len();
        let (lo, hi) = (self.invs, self.steps);
        let unbounded = self.call.frame.start.kind() == 0;
        Ok(match self.call.func {
            WinFn::Agg(kind) => match kind {
                AggKind::Count => OwnedValue::Integer(self.count),
                AggKind::Sum | AggKind::Total | AggKind::Avg => self.sum.finish(kind)?,
                AggKind::GroupConcat => self.gc.value(),
                AggKind::Min | AggKind::Max if unbounded => {
                    self.best.clone().unwrap_or(OwnedValue::Null)
                }
                AggKind::Min | AggKind::Max => self.extremum(part, lo, hi),
                _ => {
                    let mut poss: AVec<usize> = AVec::new();
                    for p in lo..hi {
                        poss.push(p);
                    }
                    self.fold(part, poss.as_slice())?
                }
            },
            WinFn::RowNumber => int(pos + 1),
            WinFn::Rank => int(part.group(pos).1 + 1),
            WinFn::DenseRank => int(part.group(pos).0 + 1),
            WinFn::PercentRank => {
                if n > 1 {
                    OwnedValue::Real(part.group(pos).1 as f64 / (n - 1) as f64)
                } else {
                    OwnedValue::Real(0.0)
                }
            }
            WinFn::CumeDist => OwnedValue::Real(part.group(pos).2 as f64 / n as f64),
            WinFn::Ntile => ntile(self.ntile, n, pos),
            WinFn::Lag | WinFn::Lead => {
                let rowid = OwnedValue::Integer(i64::try_from(pos).unwrap_or(i64::MAX) + 1);
                let off = if self.call.nargs >= 2 {
                    self.arg(part, pos, 1)
                } else {
                    OwnedValue::Integer(1)
                };
                let op = if self.call.func == WinFn::Lead {
                    BinOp::Add
                } else {
                    BinOp::Sub
                };
                let t =
                    crate::query::arith_values::<()>(op, rowid, off).unwrap_or(OwnedValue::Null);
                let target = match apply_affinity(t, Affinity::Numeric) {
                    OwnedValue::Integer(i) if i >= 1 => usize::try_from(i - 1).ok(),
                    _ => None,
                };
                match target {
                    Some(t) if t < ins => self.arg(part, t, 0),
                    _ if self.call.nargs >= 3 => self.arg(part, pos, 2),
                    _ => OwnedValue::Null,
                }
            }
            WinFn::FirstValue => {
                if lo < hi {
                    self.arg(part, lo, 0)
                } else {
                    OwnedValue::Null
                }
            }
            WinFn::LastValue => {
                if hi > lo {
                    self.arg(part, hi - 1, 0)
                } else {
                    OwnedValue::Null
                }
            }
            WinFn::NthValue => {
                let k = match apply_affinity(self.arg(part, pos, 1), Affinity::Numeric) {
                    OwnedValue::Integer(i) if i > 0 => i,
                    _ => return Err(SchemaKind::NthValueArgument),
                };
                let at = usize::try_from(k)
                    .ok()
                    .and_then(|k| k.checked_add(lo))
                    .filter(|&x| x <= hi);
                match at {
                    Some(x) => self.arg(part, x - 1, 0),
                    None => OwnedValue::Null,
                }
            }
        })
    }

    /// The pin's full scan (a frame with EXCLUDE): a fresh
    /// aggregate over the frame's rows, in order, minus the
    /// excluded ones.
    fn full_scan(&self, part: &Part<'_>, pos: usize) -> Result<OwnedValue, SchemaKind> {
        let ex = self.call.frame.exclude;
        let mut poss: AVec<usize> = AVec::new();
        for p in self.invs..self.steps {
            let skip = match ex {
                FrameExclude::None | FrameExclude::NoOthers => false,
                FrameExclude::CurrentRow => p == pos,
                FrameExclude::Group => part.peers(p, pos),
                FrameExclude::Ties => p != pos && part.peers(p, pos),
            };
            if !skip {
                poss.push(p);
            }
        }
        Ok(match self.call.func {
            WinFn::Agg(_) => self.fold(part, poss.as_slice())?,
            WinFn::FirstValue => match poss.as_slice().first() {
                Some(&p) => self.arg(part, p, 0),
                None => OwnedValue::Null,
            },
            WinFn::LastValue => match poss.as_slice().last() {
                Some(&p) => self.arg(part, p, 0),
                None => OwnedValue::Null,
            },
            WinFn::NthValue => {
                let mut got = OwnedValue::Null;
                for (i, &p) in poss.iter().enumerate() {
                    // `nth_valueStepFunc`: N (the current row's) is
                    // checked as each row is stepped.
                    let k = match apply_affinity(self.arg(part, pos, 1), Affinity::Numeric) {
                        OwnedValue::Integer(k) if k > 0 => k,
                        _ => return Err(SchemaKind::NthValueArgument),
                    };
                    if usize::try_from(k).ok() == Some(i + 1) {
                        got = self.arg(part, p, 0);
                    }
                }
                got
            }
            _ => self.value(part, pos, part.rows.len())?,
        })
    }
}

fn int(v: usize) -> OwnedValue {
    OwnedValue::Integer(i64::try_from(v).unwrap_or(i64::MAX))
}

/// The pin's `ntileValueFunc`: `n` rows, `p` buckets, row `pos`.
fn ntile(p: i64, n: usize, pos: usize) -> OwnedValue {
    if p <= 0 {
        return OwnedValue::Null;
    }
    let total = i64::try_from(n).unwrap_or(i64::MAX);
    let row = i64::try_from(pos).unwrap_or(i64::MAX);
    let size = total / p;
    if size == 0 {
        return OwnedValue::Integer(row.saturating_add(1));
    }
    let large = total - p.saturating_mul(size);
    let small = large.saturating_mul(size + 1);
    OwnedValue::Integer(if row < small {
        1 + row / (size + 1)
    } else {
        1 + large + (row - small) / size
    })
}

/// `group_concat` / `string_agg` as the pin's window function
/// keeps it (`groupConcatStep` / `groupConcatInverse`): the text,
/// the count of values, the first value's separator length and,
/// once separator lengths vary, the length of each separator, so
/// an inverse removes the leading value and the separator after
/// it by length.
#[derive(Default)]
struct Concat {
    buf: AVec<u8>,
    /// The pin's `mxAlloc != 0`: something was appended since the
    /// text last emptied.
    started: bool,
    n: i64,
    first_sep: usize,
    seps: Option<AVec<usize>>,
}

impl Concat {
    /// `sep`: None for the one-argument form (a `,`), else the
    /// row's separator text (None: NULL).
    fn step(&mut self, v: &OwnedValue, sep: Option<Option<AVec<u8>>>) {
        let Some(t) = crate::query::to_text(v) else {
            return;
        };
        let first = !self.started;
        self.started = true;
        match sep {
            None => {
                if first {
                    self.first_sep = 1;
                } else {
                    self.buf.push(b',');
                }
            }
            Some(s) if !first => {
                let ns = match &s {
                    Some(s) => {
                        self.buf.extend_from_slice(s.as_slice());
                        s.len()
                    }
                    None => 0,
                };
                if ns != self.first_sep || self.seps.is_some() {
                    let n = usize::try_from(self.n).unwrap_or(0);
                    let mut l = match self.seps.take() {
                        Some(l) => l,
                        None => {
                            let mut l = AVec::new();
                            for _ in 0..n.saturating_sub(1) {
                                l.push(self.first_sep);
                            }
                            l
                        }
                    };
                    while l.len() < n {
                        l.push(0);
                    }
                    l.truncate(n);
                    if let Some(last) = l.as_mut_slice().last_mut() {
                        *last = ns;
                    }
                    self.seps = Some(l);
                }
            }
            Some(s) => self.first_sep = s.map_or(0, |s| s.len()),
        }
        self.n += 1;
        self.buf.extend_from_slice(t.as_slice());
    }

    fn inverse(&mut self, v: &OwnedValue) {
        let Some(t) = crate::query::to_text(v) else {
            return;
        };
        let mut nvs = t.len();
        self.n -= 1;
        match &mut self.seps {
            Some(l) => {
                if self.n > 0 {
                    nvs += l.as_slice().first().copied().unwrap_or(0);
                    l.remove_front(1);
                }
            }
            None => nvs += self.first_sep,
        }
        if nvs >= self.buf.len() {
            self.buf.clear();
        } else {
            self.buf.remove_front(nvs);
        }
        if self.buf.is_empty() {
            self.started = false;
            self.seps = None;
        }
    }

    fn value(&self) -> OwnedValue {
        if self.buf.is_empty() {
            if self.n > 0 {
                OwnedValue::Text(AVec::new())
            } else {
                OwnedValue::Null
            }
        } else {
            OwnedValue::Text(self.buf.clone())
        }
    }
}

// ---------------------------------------------------------------
// Resolution.

/// Resolve a call with an OVER clause (v0.16, `window`) in the
/// current SELECT core: the pin's checks (DISTINCT, an aggregate
/// ORDER BY, a function that is no window function, the argument
/// counts, a call where none may stand, FILTER on a built-in), the
/// named-window lookup and inheritance, the RANGE offset rule, then
/// the children — resolved with window calls disallowed (the pin
/// clears `NC_AllowWin` for them; aggregates stay allowed, owned by
/// this core).
#[allow(clippy::too_many_arguments)]
pub fn resolve_call<E>(
    name: &[u8],
    distinct: bool,
    star: bool,
    args: &[crate::sql::Expr],
    clause: &crate::sql::ast::FuncClause,
    over: &crate::sql::ast::Over,
    arena: &crate::arena::Arena,
    cur: &crate::query::ScopeGroup,
    enclosing: &[crate::query::ScopeGroup],
    planner: crate::query::SubPlanner<'_, E>,
) -> crate::error::Result<RExpr, E> {
    use crate::error::Error;
    use crate::sql::ast::{FrameBound, Over};
    let err = |kind| Err(Error::Schema { kind });
    // Parse-time refusals on the pin.
    if distinct {
        return err(SchemaKind::WindowDistinct);
    }
    if !clause.order_by.is_empty() {
        return err(SchemaKind::WindowClauseMisused);
    }
    // The function and its argument count.
    let n = args.len();
    let (func, lo, hi) = match WinFn::builtin(name) {
        Some(b) => b,
        None => match crate::query::agg_kind(name) {
            Some(AggKind::Min | AggKind::Max) if n > 1 => {
                return err(SchemaKind::NotWindowFunction);
            }
            Some(k) => {
                let (lo, hi) = match k {
                    AggKind::GroupConcat if crate::sql::eq_ci(name, b"string_agg") => (2, 2),
                    AggKind::GroupConcat => (1, 2),
                    AggKind::Count => (0, 1),
                    AggKind::Percentile | AggKind::PercentileCont | AggKind::PercentileDisc => {
                        (2, 2)
                    }
                    _ => (1, 1),
                };
                (WinFn::Agg(k), lo, hi)
            }
            None => {
                return err(if crate::query::is_scalar_name(name) {
                    SchemaKind::NotWindowFunction
                } else {
                    SchemaKind::UnknownFunction
                });
            }
        },
    };
    if !cur.win.ok.get() {
        return err(SchemaKind::MisusedWindow);
    }
    // `f(*)` is `f()` on the pin (its parser drops the star), so
    // the zero-argument built-ins and count accept it and every
    // other function has the wrong argument count.
    let n = if star { 0 } else { n };
    if n < lo || n > hi {
        return err(SchemaKind::WrongArgumentCount);
    }
    if clause.filter.is_some() && !matches!(func, WinFn::Agg(_)) {
        return err(SchemaKind::WindowClauseMisused);
    }
    // The window.
    let def = match over {
        Over::Named(w) => {
            let wn = crate::sql::handle_bytes(arena, *w);
            match cur
                .win
                .defs
                .iter()
                .rev()
                .find(|(d, _)| crate::sql::eq_ci(crate::sql::handle_bytes(arena, *d), wn))
            {
                Some((_, d)) => d.clone(),
                None => return err(SchemaKind::NoSuchWindow),
            }
        }
        Over::Spec(w) => {
            let mut d = (**w).clone();
            if d.base.is_some() {
                crate::sql::ast::chain_window(&mut d, cur.win.defs.as_slice(), arena)
                    .map_err(|kind| Error::Schema { kind })?;
            }
            d
        }
    };
    if let Some(f) = &def.frame
        && f.unit == FrameUnit::Range
        && (matches!(f.start, FrameBound::Preceding(_) | FrameBound::Following(_))
            || matches!(f.end, FrameBound::Preceding(_) | FrameBound::Following(_)))
        && def.order_by.len() != 1
    {
        return err(SchemaKind::RangeOffsetOrder);
    }
    // The children, with window calls disallowed.
    let was = cur.win.ok.replace(false);
    let r = resolve_kids::<E>(args, clause, &def, arena, cur, enclosing, planner);
    cur.win.ok.set(was);
    let (kids, order) = r?;
    let frame = match &def.frame {
        None => None,
        Some(f) => {
            let bound = |b: &FrameBound| -> crate::error::Result<Bound, E> {
                Ok(match b {
                    FrameBound::UnboundedPreceding => Bound::UnboundedPreceding,
                    FrameBound::CurrentRow => Bound::CurrentRow,
                    FrameBound::UnboundedFollowing => Bound::UnboundedFollowing,
                    FrameBound::Preceding(e) => {
                        Bound::Preceding(crate::query::resolve::<E>(e, arena, &[], &[])?)
                    }
                    FrameBound::Following(e) => {
                        Bound::Following(crate::query::resolve::<E>(e, arena, &[], &[])?)
                    }
                })
            };
            Some(WinFrame {
                unit: f.unit,
                start: bound(&f.start)?,
                end: bound(&f.end)?,
                exclude: f.exclude,
            })
        }
    };
    let star = star || (func == WinFn::Agg(AggKind::Count) && n == 0);
    let mut call = WinCall::new(
        func,
        star,
        n,
        clause.filter.is_some(),
        def.partition.len(),
        order,
        frame,
    );
    call.slot = cur.win.next.get();
    cur.win.next.set(call.slot.saturating_add(1));
    Ok(RExpr::Win(alloc::boxed::Box::new(call), kids))
}

/// A window call's children (arguments, FILTER, PARTITION BY,
/// ORDER BY) and its ORDER BY directions with their collations.
fn resolve_kids<E>(
    args: &[crate::sql::Expr],
    clause: &crate::sql::ast::FuncClause,
    def: &crate::sql::ast::WindowDef,
    arena: &crate::arena::Arena,
    cur: &crate::query::ScopeGroup,
    enclosing: &[crate::query::ScopeGroup],
    planner: crate::query::SubPlanner<'_, E>,
) -> crate::error::Result<(AVec<RExpr>, AVec<SortOrder>), E> {
    use crate::query::resolve_scoped;
    let mut kids = AVec::new();
    for a in args {
        kids.push(resolve_scoped::<E>(
            a,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?);
    }
    if let Some(f) = &clause.filter {
        kids.push(resolve_scoped::<E>(
            f,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?);
    }
    for p in def.partition.iter() {
        kids.push(resolve_scoped::<E>(
            p,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?);
    }
    let mut order = AVec::new();
    for (o, dir) in def.order_by.iter() {
        let r = resolve_scoped::<E>(o, arena, cur, enclosing, &mut *planner)?;
        let mut dir = *dir;
        dir.coll = crate::query::expr_coll(&r).unwrap_or_default();
        order.push(dir);
        kids.push(r);
    }
    Ok((kids, order))
}

/// `e` with every window call's children dropped (v0.16, `window`):
/// the template the executor clones and `fill`s for each output row
/// once the windows are computed.
pub fn strip(e: &RExpr) -> RExpr {
    let mut out = e.clone();
    strip_in(&mut out);
    out
}

fn strip_in(e: &mut RExpr) {
    if let RExpr::Win(_, k) = e {
        k.clear();
        return;
    }
    each_child_mut(e, &mut strip_in);
}

/// One input row's children values for every call (`calls` order),
/// evaluated by `ev`.
pub fn inputs<E>(
    calls: &[&RExpr],
    ev: &mut dyn FnMut(&RExpr) -> Result<OwnedValue, E>,
) -> Result<AVec<AVec<OwnedValue>>, E> {
    let mut out = AVec::with_capacity(calls.len());
    for c in calls {
        let mut v = AVec::new();
        for k in children(c) {
            v.push(ev(k)?);
        }
        out.push(v);
    }
    Ok(out)
}
