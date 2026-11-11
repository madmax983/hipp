//! SELECT code generation, unit `vm_joins` (ADR 0005 Stage 2): the
//! general query plan — any number of FROM sources joined in the
//! planner's order (inner, LEFT, RIGHT, FULL, cross, NATURAL / USING:
//! the plan's ON clauses and join kinds), each read by its access path
//! (a table scan, a rowid lookup or range, an index walk, IN lists,
//! covering and expression / partial indexes, MULTI-INDEX OR, skip-
//! scans, the min() / max() early-out, automatic indexes and Bloom
//! filters), then aggregates (the streamed fold or GROUP BY groups,
//! HAVING), DISTINCT, ORDER BY and LIMIT / OFFSET.
//!
//! The program is the nested loop of the streamed executor
//! (`session::stream`, `join_sources`) laid out as code, in its order:
//!
//! - **Loops.** Source `d`'s loop opens for each combination of the
//!   sources before it; its row is appended to the joined row (the
//!   VM's accumulator, `Load`), its ON clause decides whether the
//!   loop goes on inwards; a LEFT / FULL source that matched no row
//!   adds one NULL row (`NullRow`) once its loop is over; a semi-join
//!   source's loop ends once a joined row passed the WHERE clause.
//!   The complete row passes the WHERE clause (its constant terms were
//!   tested once, first) into the output.
//! - **Rows computed as a whole.** A source read through an index (or
//!   any access path other than a rowid lookup or range, which read the
//!   table directly) has its rows computed into a row list as today's
//!   `access_rows` computes them: once per execution when its probes
//!   read no earlier source, per combination otherwise; an automatic
//!   index's rows are the table's, sorted once, probed by binary
//!   search after the Bloom filter. A RIGHT / FULL source's rows are a
//!   row list too, whose match flags drive the second pass after the
//!   main loop: each unmatched row with NULLs for the sources before
//!   it, joined onward (`join_sources`).
//! - **Output.** Each row is returned as it is made (OFFSET rows are
//!   skipped unprojected, LIMIT stops the program), or checked against
//!   the DISTINCT rows so far, or sorted (unless the one source's
//!   order delivers the ORDER BY), or folded into the aggregates — the
//!   streamed fold (count / sum / total / avg / min / max) or the
//!   groups of `project_qp_async`, each group emitted as it is
//!   finalized (HAVING first) in GROUP BY order.
//!
//! - **Subqueries, compounds, CTEs** (unit `vm_subqueries`, `sql`;
//!   `gen/sub.rs`): an expression's subquery nodes are code run where
//!   today's executor computes them, writing a result slot the `Eval`
//!   reads; a FROM subquery, view or CTE that is not flattened is a row
//!   list its code fills the first time its loop opens; a subquery's
//!   own sources are further loops appended to the joined row after
//!   every slot of the query around it (the planner's absolute slots).
//!
//! Every other shape (windows, virtual and table-valued sources,
//! WITHOUT ROWID, TEMP and attached tables, `sqlite_schema`, the
//! shapes `gen/sub.rs` lists) returns `None`: the whole statement runs
//! on today's executor.

use super::super::Program;
use super::super::build::{Builder, Label};
use super::super::cursor::{SortInfo, TableInfo};
use super::super::op::{ACC_ROWID, Code, EVAL_ACC, EVAL_COND, EVAL_GROUP, EVAL_VALUE, P4};
use crate::arena::AVec;
use crate::query::{RExpr, SubSource};
use crate::session::QueryPlan;
use alloc::rc::Rc;

/// Where an expression reads its row: the joined row, or the group
/// of group set `.0` (aggregates fold its rows).
#[derive(Clone, Copy)]
pub(super) enum Cx {
    Acc,
    Group(u32),
}

/// What the generators know of the database: whether a root page is
/// a WITHOUT ROWID table's, and whether its text is UTF-8 (the
/// streamed aggregate fold's condition).
#[derive(Clone, Copy)]
pub(super) struct Gx<'w> {
    pub(super) wr: &'w dyn Fn(u32) -> bool,
    pub(super) utf8: bool,
}

/// Where a query's result rows go.
#[derive(Clone, Copy)]
pub(super) enum Sink {
    /// The statement's result (`ResultRow`).
    Result,
    /// A row list: a FROM subquery's rows, a compound core's.
    #[cfg(feature = "sql")]
    List(u32),
    /// A subquery node's slot (`SubRow`, `P4::Sub` entry `.1`).
    #[cfg(feature = "sql")]
    Sub(u32, u32),
    /// A recursive CTE's queue (`QueueAdd`).
    #[cfg(feature = "sql")]
    Queue(u32),
}

/// Whether `Eval` can evaluate `e` over a joined row of `width`
/// slots and `nsrc` sources: no window call or deferred error, and
/// (`agg`) aggregates of this query only, whose arguments read the
/// row. Subquery nodes are code of their own (`sql`): their plans are
/// checked as they are generated, and an expression of the query's
/// own aggregates inside one falls back.
pub(super) fn evaluable(e: &RExpr, width: usize, nsrc: usize, agg: bool) -> bool {
    evaluable_w(e, width, nsrc, agg, false)
}

/// `evaluable`, also allowing (`win`) window calls whose children are
/// evaluable (a window query's result columns and ORDER BY terms,
/// unit `vm_rest`).
pub(super) fn evaluable_w(e: &RExpr, width: usize, nsrc: usize, agg: bool, win: bool) -> bool {
    // A chain of unary operators (a run of signs, NOTs, CASTs) is
    // walked in a loop: the expression may be as deep as the
    // parser's limits allow, and the step path's stack is budgeted.
    let mut e = e;
    while let RExpr::Neg(a)
    | RExpr::Pos(a)
    | RExpr::Not(a)
    | RExpr::IsNull(a, _)
    | RExpr::Cast(a, _)
    | RExpr::Collate(a, _) = e
    {
        e = a;
    }
    let f = |x: &RExpr| evaluable_w(x, width, nsrc, agg, win);
    match e {
        RExpr::Literal(_) | RExpr::AffLiteral(..) => true,
        RExpr::Column(slot, ..) => *slot < width,
        RExpr::Rowid(s) => *s < nsrc,
        #[cfg(feature = "sql")]
        RExpr::SubScalar(_) | RExpr::SubExists(_) => true,
        #[cfg(feature = "sql")]
        RExpr::InSub(x, ..) => f(x),
        #[cfg(not(feature = "sql"))]
        RExpr::SubScalar(_) | RExpr::SubExists(_) | RExpr::InSub(..) => false,
        RExpr::Raise(_) => false,
        RExpr::Win(_, k) => win && k.iter().all(|x| evaluable_w(x, width, nsrc, agg, false)),
        // An enclosing query's aggregate in an EXISTS subquery's
        // result list is never evaluated there: NULL (`inert`).
        RExpr::Agg { inert: true, .. } => true,
        RExpr::Agg { args, lvl, .. } => {
            agg && *lvl == 0 && args.iter().all(|a| evaluable(a, width, nsrc, false))
        }
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) && f(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => f(a),
        RExpr::InList(x, l, ..) => f(x) && l.iter().all(f),
        RExpr::Between(x, a, b, _) => f(x) && f(a) && f(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(f)
                && whens.iter().all(|(w, t)| f(w) && f(t))
                && otherwise.as_deref().is_none_or(f)
        }
        RExpr::Scalar(_, args) => args.iter().all(f),
    }
}

/// Whether any of `es` holds a window call (outside its subqueries),
/// walked with a heap stack, not recursion: an expression may be as
/// deep as the parser's limits allow and the step path's native stack
/// is budgeted (`window::calls` recurses).
#[cfg(feature = "window")]
fn any_win<'x>(es: impl Iterator<Item = &'x RExpr>) -> bool {
    let mut stack: AVec<&'x RExpr> = es.collect();
    while let Some(e) = stack.pop() {
        if matches!(e, RExpr::Win(..)) {
            return true;
        }
        crate::window::each_child(e, &mut |c| stack.push(c));
    }
    false
}

/// Whether source `sp` is a table the VM reads: a rowid table of the
/// main database, `sqlite_schema` included (no derived table, TEMP,
/// attached, WITHOUT ROWID, virtual or table-valued source, nor
/// recursive reference: those the driver reads, `host_kind`).
fn table_source(sp: &SubSource, wr: &dyn Fn(u32) -> bool) -> bool {
    let ok = sp.derived.is_none() && sp.root >= 1 && sp.affs.len() == sp.ncols && !wr(sp.root);
    #[cfg(feature = "sql")]
    let ok = ok && sp.rec == 0 && !sp.temp;
    #[cfg(any(feature = "json", feature = "vtab"))]
    let ok = ok && sp.tvf == 0;
    #[cfg(feature = "vtab")]
    let ok = ok && sp.vt.is_none();
    #[cfg(feature = "attach")]
    let ok = ok && sp.ext == 0;
    ok
}

/// A FROM subquery, view or CTE the VM computes (`sql`): a derived
/// source that is no table-valued function or virtual table.
#[cfg(feature = "sql")]
fn derived_source(sp: &SubSource) -> Option<&crate::query::SubPlan> {
    #[cfg(any(feature = "json", feature = "vtab"))]
    if sp.tvf != 0 {
        return None;
    }
    #[cfg(feature = "vtab")]
    if sp.vt.is_some() {
        return None;
    }
    if sp.rec != 0 || sp.temp {
        return None;
    }
    sp.derived.as_deref()
}

#[cfg(feature = "expr_index")]
type Bounds<'p> = [Option<&'p crate::xplan::Bound>; 2];

/// How a source's loop reads its rows.
#[derive(Clone, Copy)]
enum Kind<'p> {
    /// The table in rowid order.
    Scan,
    /// One row by rowid: the probe and its affinity, after the
    /// bounds' NULL check.
    #[cfg(feature = "expr_index")]
    Eq(&'p RExpr, Option<crate::record::Affinity>, Bounds<'p>),
    /// A rowid range, ascending or (`.1`) descending.
    #[cfg(feature = "expr_index")]
    Range(Bounds<'p>, bool),
    /// A row list computed once per execution (an access path whose
    /// probes read no earlier source, or a RIGHT / FULL source).
    Mat,
    /// A row list computed per combination of the earlier sources.
    #[cfg(feature = "expr_index")]
    Lateral,
    /// An automatic index: the table's rows sorted once, probed per
    /// combination.
    #[cfg(feature = "planner")]
    Auto(&'p crate::xplan::AutoIdx, bool),
    /// A FROM subquery's rows: a row list its code fills the first
    /// time the loop opens (once per execution of this query).
    #[cfg(feature = "sql")]
    Derived(&'p crate::query::SubPlan),
    /// A recursive CTE's recursive reference: the one row its queue
    /// last gave up, in row list `.0`.
    #[cfg(feature = "sql")]
    Rec,
    /// A row list the driver fills (`HostRows`, mode `.0`): once per
    /// execution, or (`.1`, a lateral source) per combination of the
    /// sources before it.
    #[cfg(feature = "sql")]
    Host(u8, bool),
    /// A host cursor the driver opens and moves row by row
    /// (`HostOpen` mode `.0` / `HostNext`): a virtual table's xFilter
    /// for each combination, a WITHOUT ROWID table's scan.
    #[cfg(feature = "sql")]
    #[cfg_attr(
        not(any(feature = "vtab", feature = "without_rowid")),
        allow(dead_code)
    )]
    HostCur(u8),
    #[cfg(not(feature = "expr_index"))]
    #[allow(dead_code)]
    Never(core::marker::PhantomData<&'p ()>),
}

/// One source's loop: its cursors and state registers.
struct Lv<'p> {
    kind: Kind<'p>,
    /// The table cursor.
    t: u32,
    /// The row list (`Mat`, `Lateral`, `Auto`, `Derived`, `Rec`).
    e: u32,
    /// Set once the row list is built (`Mat`, `Auto`, `Derived`).
    built: u32,
    /// A row of this combination passed the ON clause.
    any: u32,
    /// The LEFT JOIN pad row is the current one.
    pad: u32,
    /// A semi-join loop made its joined row.
    done: u32,
    /// The lateral Bloom filter (a row list holding it) and its
    /// "built" register.
    #[cfg(feature = "planner")]
    bloom: Option<(u32, u32)>,
}

/// What the program does with each output row.
#[derive(Clone, Copy)]
enum Out {
    Plain,
    /// The row list of the rows seen so far.
    Distinct(u32),
    /// The sorter, and the DISTINCT row list if any.
    Sort(u32, Option<u32>),
}

/// What the joined rows feed.
#[derive(Clone, Copy)]
enum Fold {
    /// The output, row by row.
    Rows,
    /// The streamed aggregate accumulator.
    Agg(u32),
    /// The GROUP BY group set.
    Groups(u32),
}

/// The generator of one query core (a SELECT without compound): the
/// statement's own, or a subquery's, whose slots and sources follow
/// those of the query around it (`w0`, `n0`).
pub(super) struct G<'p, 'b> {
    pub(super) b: &'b mut Builder,
    pub(super) qp: &'p QueryPlan,
    pub(super) gx: Gx<'p>,
    plan: u32,
    bases: AVec<usize>,
    lv: AVec<Lv<'p>>,
    pub(super) end: Label,
    pub(super) lim: Option<u32>,
    pub(super) off: Option<u32>,
    pub(super) tmp: u32,
    /// The output row's rowid source (`ACC_ROWID` for one source).
    rowid: u32,
    fold: Fold,
    /// The joined row's slots and sources before this core's own.
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    w0: usize,
    n0: usize,
    /// Every slot and source of the joined row this core makes.
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    pub(super) width: usize,
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    pub(super) nsrc: usize,
    pub(super) sink: Sink,
    /// The rows the reader takes: a register (−1: all; its copy
    /// `cnt` counts them down).
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    pub(super) need: Option<u32>,
    pub(super) cnt: Option<u32>,
    /// A growing first FROM subquery (`session::streams_first`): its
    /// bound and the read position registers.
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    grow: Option<(u32, u32)>,
    /// The bound of a first FROM subquery the reader reads in order
    /// (`session::derived_need`; −1: none).
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    dneed: Option<u32>,
    /// The result columns and ORDER BY terms the output computes:
    /// the plan's, or a window query's templates (`window::strip`).
    items: &'p [RExpr],
    order: &'p [(RExpr, crate::sql::SortOrder)],
    /// A window query's set (unit `vm_rest`).
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    win: Option<u32>,
    /// Whether today's executor streams this core (`stream_plan`):
    /// the statement's own core, not a compound's, with no RIGHT /
    /// FULL join, recursive CTE reference or window call. Every other
    /// core is today's materialising executor's, which reads a
    /// generate_series cursor whole (`SrcInfo::drain`).
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    streamed: bool,
    /// One core of a compound (its LIMIT / OFFSET the compound's).
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    arm: bool,
    /// `Eval` flags every expression the output computes takes
    /// (`EVAL_WIN` while a window query's units are output).
    pub(super) wflag: u8,
    /// The statement's own core reads no table: its one row satisfies
    /// the ORDER BY, whose terms are then never computed (the pin's
    /// loop over no table, a subquery anywhere in the statement
    /// notwithstanding). Today's executor computes them (and fails
    /// where a term does). Unit `vm_core`'s separate single-table
    /// generator carried this rule, for subquery-free statements only,
    /// until `vm_shrink`; a DISTINCT core and a nested query's own
    /// ORDER BY still compute their terms (a recorded gap).
    free_order: bool,
}

/// The program of a general query plan, or `None` when it holds a
/// shape the generators do not cover. `wr`: whether a root page is a
/// WITHOUT ROWID table's; `utf8`: the database's text encoding is
/// UTF-8 (the streamed aggregate fold's condition).
pub(crate) fn query(qp: &Rc<QueryPlan>, wr: &dyn Fn(u32) -> bool, utf8: bool) -> Option<Program> {
    let mut b = Builder::new();
    let gx = Gx { wr, utf8 };
    select(&mut b, qp, gx, (0, 0), None, Sink::Result)?;
    b.emit2(Code::Halt, 0, 0);
    b.finish()
}

/// Code for one execution of `qp` (a core, or a compound / VALUES
/// one under `sql`) over the joined row's first `at` slots and
/// sources, its rows into `sink`, at most `need` of them.
pub(super) fn select(
    b: &mut Builder,
    qp: &Rc<QueryPlan>,
    gx: Gx<'_>,
    at: (usize, usize),
    need: Option<u32>,
    sink: Sink,
) -> Option<()> {
    #[cfg(feature = "sql")]
    if !qp.compound.is_empty() || !qp.values.is_empty() {
        return super::sub::compound(b, qp, gx, at, need, sink);
    }
    let end = b.label();
    let cnt = need.map(|n| {
        let c = b.reg();
        b.emit2(Code::Copy, n, c);
        c
    });
    core(b, qp, gx, at, None, need, cnt, sink, end)?;
    b.here(end);
    Some(())
}

/// One core's code: `qp`'s sources joined (its compound tails, if any,
/// are not this core's), its rows into `sink`; `end` follows. `win`:
/// the LIMIT / OFFSET registers of a compound around it (else the
/// core's own LIMIT / OFFSET apply). `need`: the register holding the
/// rows the reader takes (−1: all), which bounds a first FROM
/// subquery; `cnt`: the counter of rows still to give, which ends the
/// core at 0.
#[allow(clippy::too_many_arguments)]
pub(super) fn core(
    b: &mut Builder,
    qp: &Rc<QueryPlan>,
    gx: Gx<'_>,
    at: (usize, usize),
    win: Option<(Option<u32>, Option<u32>)>,
    need: Option<u32>,
    cnt: Option<u32>,
    sink: Sink,
    end: Label,
) -> Option<()> {
    let (w0, n0) = at;
    let nsrc = n0.saturating_add(qp.sources.len());
    let width: usize = w0.saturating_add(qp.sources.iter().map(|s| s.ncols).sum::<usize>());
    let grouped = qp.has_agg || !qp.group_by.is_empty();
    // A window query (unit `vm_rest`): its calls, and the templates
    // its output computes with their values (`window::strip`).
    #[cfg(feature = "window")]
    let windowed = any_win(qp.items.iter().chain(qp.order_by.iter().map(|(e, _)| e)));
    #[cfg(feature = "window")]
    let wins = if windowed {
        crate::window::calls(qp.items.as_slice(), qp.order_by.as_slice())
    } else {
        AVec::new()
    };
    #[cfg(not(feature = "window"))]
    let windowed = false;
    #[cfg(feature = "window")]
    let tpl_items: AVec<RExpr> = if windowed {
        qp.items.iter().map(crate::window::strip).collect()
    } else {
        AVec::new()
    };
    #[cfg(feature = "window")]
    let tpl_order: AVec<(RExpr, crate::sql::SortOrder)> = if windowed {
        qp.order_by
            .iter()
            .map(|(e, o)| (crate::window::strip(e), *o))
            .collect()
    } else {
        AVec::new()
    };
    let row_ok = |e: &RExpr| evaluable(e, width, nsrc, false);
    let out_ok = |e: &RExpr| evaluable_w(e, width, nsrc, grouped, windowed);
    if !qp.items.iter().all(out_ok)
        || !qp.order_by.iter().all(|(e, _)| out_ok(e))
        || !qp.having.as_ref().is_none_or(out_ok)
        || !qp.group_by.iter().all(row_ok)
        || !qp.filter.as_ref().is_none_or(row_ok)
        || (win.is_none()
            && ![&qp.limit, &qp.offset]
                .iter()
                .all(|l| l.as_ref().is_none_or(|e| evaluable(e, 0, 0, false))))
    {
        return None;
    }
    let plan = b.p4(P4::Plan(Rc::clone(qp)));
    let mut bases = AVec::new();
    let mut lv = AVec::new();
    let mut w = w0;
    for (d, sp) in qp.sources.iter().enumerate() {
        if !sp.on.as_ref().is_none_or(row_ok) {
            return None;
        }
        bases.push(w);
        w = w.saturating_add(sp.ncols);
        let kind = kind_of(b, sp, d, width, nsrc, gx)?;
        if table_backed(&kind) && !table_source(sp, gx.wr) {
            return None;
        }
        let t = b.cursor();
        let needs_list = !matches!(kind, Kind::Scan) && !direct(&kind);
        #[cfg(feature = "sql")]
        let rec_list = match kind {
            Kind::Rec => b
                .recs
                .iter()
                .rev()
                .find(|(id, _)| *id == sp.rec)
                .map(|r| r.1),
            _ => None,
        };
        #[cfg(not(feature = "sql"))]
        let rec_list: Option<u32> = None;
        let e = match rec_list {
            Some(l) => l,
            None if needs_list => b.cursor(),
            None => u32::MAX,
        };
        #[cfg(feature = "planner")]
        let bloom = match sp.access.as_deref() {
            Some(a) if a.lateral && a.auto.is_none() && a.bloom.is_some() => {
                Some((b.cursor(), b.reg()))
            }
            _ => None,
        };
        lv.push(Lv {
            kind,
            t,
            e,
            built: b.reg(),
            any: b.reg(),
            pad: b.reg(),
            done: b.reg(),
            #[cfg(feature = "planner")]
            bloom,
        });
    }
    let tmp = b.reg();
    // The statement's own core (not a subquery's or a compound's).
    let streamed_top = matches!(sink, Sink::Result) && w0 == 0 && n0 == 0 && win.is_none();
    let streamed = streamed_top && !windowed && !qp.sources.iter().any(|s| s.kind.pads_left());
    #[cfg(feature = "sql")]
    let streamed = streamed && !qp.sources.iter().any(|s| s.rec != 0);
    let mut g = G {
        b,
        qp,
        gx,
        plan,
        bases,
        lv,
        end,
        lim: None,
        off: None,
        tmp,
        rowid: if nsrc == 1 { ACC_ROWID } else { 0 },
        fold: Fold::Rows,
        w0,
        n0,
        width,
        nsrc,
        sink,
        need,
        cnt,
        grow: None,
        dneed: None,
        items: qp.items.as_slice(),
        order: qp.order_by.as_slice(),
        win: None,
        streamed,
        arm: win.is_some(),
        wflag: 0,
        free_order: streamed_top
            && qp.sources.is_empty()
            && !qp.distinct
            && !grouped
            && qp.order_by.iter().all(|(e, _)| evaluable(e, 0, 0, false)),
    };
    match win {
        Some((l, o)) => {
            g.lim = l;
            g.off = o;
        }
        None => g.limits()?,
    }
    #[cfg(feature = "window")]
    if windowed {
        g.items = tpl_items.as_slice();
        g.order = tpl_order.as_slice();
        let mut calls = AVec::with_capacity(wins.len());
        for w in wins.iter() {
            calls.push((*w).clone());
        }
        let k = g.b.p4(P4::Win(calls));
        let c = g.b.cursor();
        g.b.emit_p4(Code::WinOpen, c, k);
        g.win = Some(c);
    }
    // The output, the sorter, the accumulators.
    let out = g.out_mode(grouped);
    g.fold = if grouped {
        let c = g.b.cursor();
        let (pw, pn) = (w0 as u32, n0 as u32);
        if !windowed && super::super::rows::folds(qp, gx.utf8) {
            g.b.emit(Code::AggOpen, c, pw, pn, plan, 0);
            Fold::Agg(c)
        } else {
            g.b.emit(Code::GroupOpen, c, pw, pn, plan, 0);
            Fold::Groups(c)
        }
    } else {
        Fold::Rows
    };
    // Table cursors, and every row list built anew in this execution.
    for i in 0..g.lv.len() {
        let (t, built, root) = match (g.lv.as_slice().get(i), qp.sources.as_slice().get(i)) {
            (Some(l), Some(sp)) => (l.t, l.built, sp),
            _ => return None,
        };
        g.b.emit2(Code::Integer, 0, built);
        #[cfg(feature = "planner")]
        if let Some((_, bb)) = g.lvl(i).and_then(|l| l.bloom) {
            g.b.emit2(Code::Integer, 0, bb);
        }
        if table_source(root, gx.wr) {
            let info = g.b.p4(P4::Table(alloc::boxed::Box::new(table_info(root))));
            g.b.emit_p4(Code::OpenRead, t, info);
        }
    }
    #[cfg(feature = "sql")]
    g.grow_setup();
    let after = g.b.label();
    // The WHERE clause's constant terms, once, first: false, and no
    // row joins (an aggregate still has its empty group).
    if qp.filter.is_some() || qp.sources.iter().any(|s| s.on.is_some()) {
        g.b.emit(Code::Consts, 0, tmp, 0, plan, 0);
        g.b.jump_to(Code::IfNot, tmp, after);
    }
    let main_done = g.b.label();
    g.from(0, main_done, out)?;
    g.b.here(main_done);
    // Each RIGHT / FULL source's unmatched rows, NULLs on the left.
    for i in 0..qp.sources.len() {
        if qp
            .sources
            .as_slice()
            .get(i)
            .is_some_and(|s| s.kind.pads_left())
        {
            g.second_pass(i, out)?;
        }
    }
    g.b.here(after);
    g.finish_agg(out);
    #[cfg(feature = "window")]
    if windowed {
        g.window_out(out);
    }
    g.sorted_out(out);
    if g.b.failed() {
        return None;
    }
    Some(())
}

/// Whether a loop reads a table-valued or virtual source the driver
/// reads.
#[cfg(feature = "sql")]
fn tvf_kind(k: &Kind<'_>) -> bool {
    match k {
        #[cfg(any(feature = "json", feature = "vtab"))]
        Kind::Host(super::super::op::HOST_TVF, _) => true,
        #[cfg(feature = "vtab")]
        Kind::HostCur(super::super::op::HOST_VT) => true,
        _ => false,
    }
}

/// Whether a loop reads a cursor directly (no row list): the table,
/// or a host cursor.
fn direct(k: &Kind<'_>) -> bool {
    match k {
        Kind::Scan => true,
        #[cfg(feature = "expr_index")]
        Kind::Eq(..) | Kind::Range(..) => true,
        #[cfg(feature = "sql")]
        Kind::HostCur(_) => true,
        _ => false,
    }
}

/// Whether a loop's rows come from the source's table through the
/// VM's own table cursor (a scan, a rowid lookup or range, an access
/// path, an automatic index, a RIGHT / FULL table source).
fn table_backed(k: &Kind<'_>) -> bool {
    match k {
        #[cfg(feature = "sql")]
        Kind::Derived(_) | Kind::Rec | Kind::Host(..) | Kind::HostCur(_) => false,
        #[cfg(feature = "planner")]
        Kind::Auto(_, host) => !host,
        _ => true,
    }
}

/// A FROM source's table layout (`P4::Table`).
fn table_info(sp: &SubSource) -> TableInfo {
    TableInfo {
        root: sp.root,
        ncols: sp.ncols,
        rowid_alias: sp.rowid_alias,
        affs: sp.affs.clone(),
        defaults: sp.defaults.clone(),
        gcols: sp.gcols.clone(),
    }
}

/// How source `d`'s loop reads it, when the VM covers its access
/// path: every probe evaluable over the sources before it (a
/// non-lateral one over none); a FROM subquery's rows; a recursive
/// reference's row.
fn kind_of<'p>(
    b: &Builder,
    sp: &'p SubSource,
    d: usize,
    width: usize,
    nsrc: usize,
    gx: Gx<'_>,
) -> Option<Kind<'p>> {
    let _ = (d, width, nsrc, b, gx);
    #[cfg(feature = "sql")]
    if let Some(k) = host_kind(sp, d, gx, width, nsrc) {
        // (A folded aggregate's placeholder the driver's readers would
        // evaluate falls back: v0.17 review fix.)
        if super::sub::host_reads_preset(b, sp) {
            return None;
        }
        return k;
    }
    #[cfg(feature = "sql")]
    if sp.rec != 0 {
        return b
            .recs
            .iter()
            .any(|(id, _)| *id == sp.rec)
            .then_some(Kind::Rec);
    }
    #[cfg(feature = "sql")]
    if sp.derived.is_some() {
        // (No planner gives a FROM subquery an access path; one would
        // be today's executor's.)
        #[cfg(feature = "expr_index")]
        if sp.access.is_some() {
            return None;
        }
        return derived_source(sp).map(Kind::Derived);
    }
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        if sp.kind.pads_left() || !access_ok(a, width, nsrc) {
            return None;
        }
        #[cfg(feature = "planner")]
        if let Some(ai) = a.auto.as_deref() {
            return Some(Kind::Auto(ai, false));
        }
        let plain = a.root == 0 && a.cover.is_none() && a.bignull.is_none() && a.eq.len() <= 1;
        #[cfg(feature = "planner")]
        let plain = plain && a.or.is_none() && a.first.is_none() && a.skip == 0;
        if plain {
            let bounds = [a.lo.as_ref(), a.hi.as_ref()];
            match a.eq.as_slice().first() {
                Some(p) => {
                    if let ([e], false, None) = (p.vals.as_slice(), p.is_in, &p.sub) {
                        return Some(Kind::Eq(e, p.aff, bounds));
                    }
                }
                None => return Some(Kind::Range(bounds, a.reverse)),
            }
        }
        return Some(if a.lateral { Kind::Lateral } else { Kind::Mat });
    }
    if sp.kind.pads_left() {
        return Some(Kind::Mat);
    }
    Some(Kind::Scan)
}

/// How the driver reads source `sp` when the VM's own cursors do not
/// (unit `vm_rest`), as the streamed executor reads it
/// (`level_open`): a table-valued function's rows (once, or per
/// combination when its arguments read a source to its left); a
/// virtual table's cursor (xFilter per combination, except a module
/// computing its rows at its filter that reads nothing to its left,
/// computed once for an inner source); a TEMP or attached table's rows
/// once; a WITHOUT ROWID table's scan (row by row for the first
/// source, else its rows once) or the rows its access path finds.
/// `None`: a source the VM reads itself; `Some(None)`: a shape no
/// generator covers.
#[cfg(feature = "sql")]
fn host_kind<'p>(
    sp: &'p SubSource,
    d: usize,
    gx: Gx<'_>,
    width: usize,
    nsrc: usize,
) -> Option<Option<Kind<'p>>> {
    use super::super::op::HOST_SCAN;
    let _ = (d, gx, width, nsrc);
    // The arguments evaluate on the row before the source.
    #[cfg(any(feature = "json", feature = "vtab"))]
    if sp.tvf != 0 {
        let lateral = sp.tvf & crate::query::TVF_LATERAL != 0;
        #[cfg(feature = "vtab")]
        if let Some(vt) = sp.vt.as_deref() {
            use crate::vtab::Module;
            #[cfg(feature = "rtree")]
            let streamed = matches!(vt.tab.module, Module::Series | Module::Rtree { .. });
            #[cfg(not(feature = "rtree"))]
            let streamed = matches!(vt.tab.module, Module::Series);
            return Some(Some(
                if sp.kind.pads_left() || (!lateral && d > 0 && !streamed) {
                    Kind::Host(super::super::op::HOST_TVF, false)
                } else {
                    Kind::HostCur(super::super::op::HOST_VT)
                },
            ));
        }
        return Some(Some(Kind::Host(
            super::super::op::HOST_TVF,
            lateral && !sp.kind.pads_left(),
        )));
    }
    if sp.derived.is_some() || sp.rec != 0 {
        return None;
    }
    if sp.temp {
        return Some(Some(Kind::Host(HOST_SCAN, false)));
    }
    #[cfg(feature = "without_rowid")]
    if (gx.wr)(sp.root) {
        #[cfg(feature = "expr_index")]
        if let Some(a) = sp.access.as_deref() {
            if !access_ok(a, width, nsrc) {
                return Some(None);
            }
            #[cfg(feature = "planner")]
            if let Some(ai) = a.auto.as_deref() {
                return Some(Some(Kind::Auto(ai, true)));
            }
            return Some(Some(Kind::Host(
                super::super::op::HOST_ACCESS,
                a.lateral && !sp.kind.pads_left(),
            )));
        }
        return Some(Some(if d == 0 && !sp.kind.pads_left() {
            Kind::HostCur(HOST_SCAN)
        } else {
            Kind::Host(HOST_SCAN, false)
        }));
    }
    None
}

/// Whether every expression of access path `a` is evaluable (an
/// IN-subquery probe set under `sql`; the MULTI-INDEX OR branches'
/// conditions, which the earlier branches test as plain conditions,
/// hold no subquery).
#[cfg(feature = "expr_index")]
fn access_ok(a: &crate::xplan::IdxAccess, width: usize, nsrc: usize) -> bool {
    let ok = |e: &RExpr| evaluable(e, width, nsrc, false);
    #[cfg(feature = "sql")]
    let sub_ok = |p: &crate::xplan::Probe| p.sub.as_ref().is_none_or(ok);
    #[cfg(not(feature = "sql"))]
    let sub_ok = |p: &crate::xplan::Probe| p.sub.is_none();
    let probes = a.eq.iter().all(|p| sub_ok(p) && p.vals.iter().all(ok));
    let bounds = [&a.lo, &a.hi]
        .iter()
        .all(|b| b.as_ref().is_none_or(|b| ok(&b.e)));
    #[cfg(feature = "planner")]
    {
        let or = a.or.as_deref().is_none_or(|o| {
            o.subs.iter().all(|s| {
                ok(&s.cond)
                    && s.acc
                        .as_ref()
                        .is_none_or(|x| x.or.is_none() && access_ok(x, width, nsrc))
            })
        });
        let first = a.first.as_deref().is_none_or(|f| ok(&f.0));
        let auto = a
            .auto
            .as_deref()
            .is_none_or(|ai| ai.keys.iter().all(|k| ok(&k.val)));
        probes && bounds && or && first && auto
    }
    #[cfg(not(feature = "planner"))]
    {
        probes && bounds
    }
}

impl<'p> G<'p, '_> {
    fn lvl(&self, d: usize) -> Option<&Lv<'p>> {
        self.lv.as_slice().get(d)
    }

    /// Source `d`'s index in the joined row.
    fn src(&self, d: usize) -> u32 {
        self.n0.saturating_add(d) as u32
    }

    fn base(&self, d: usize) -> u32 {
        self.bases.as_slice().get(d).copied().unwrap_or(0) as u32
    }

    /// Code `e`'s value into `dest`, reading `cx`'s row; a literal is a
    /// prologue constant.
    pub(super) fn val(&mut self, e: &RExpr, cx: Cx, dest: u32) {
        if let RExpr::Literal(v) = e {
            let k = self.b.p4(P4::Value(v.clone()));
            self.b.hoist(Code::Value, dest, k);
            return;
        }
        #[cfg(feature = "sql")]
        if crate::query::has_sub(e) {
            if super::sub::real_sub(self.b, e) {
                self.val_sub(e, cx, dest, EVAL_VALUE);
            } else {
                self.val_preset(e, cx, dest, EVAL_VALUE);
            }
            return;
        }
        let k = self.b.p4(P4::Expr(e.clone()));
        let (p1, flag) = match cx {
            Cx::Acc => (0, EVAL_ACC),
            Cx::Group(c) => (c.saturating_add(1), EVAL_GROUP),
        };
        self.b
            .emit(Code::Eval, p1, dest, 0, k, EVAL_VALUE | flag | self.wflag);
    }

    /// `val` for a register an opcode then converts in place
    /// (`Affinity`, `RowidBound`): a literal is loaded where it is
    /// used, not once in the prologue, so code that runs again (a
    /// loop opened per outer row) starts from the literal each time.
    #[cfg(feature = "expr_index")]
    fn val_mut(&mut self, e: &RExpr, cx: Cx, dest: u32) {
        if let RExpr::Literal(v) = e {
            let k = self.b.p4(P4::Value(v.clone()));
            self.b.emit(Code::Value, 0, dest, 0, k, 0);
            return;
        }
        self.val(e, cx, dest);
    }

    /// Code condition `e` (NULL is not true) and jump to `skip` unless
    /// it holds.
    pub(super) fn test(&mut self, e: &RExpr, cx: Cx, skip: Label) {
        // A condition holding a subquery (`cond_row_sub` /
        // `cond_group_sub`).
        #[cfg(feature = "sql")]
        if crate::query::has_sub(e) {
            match cx {
                _ if !super::sub::real_sub(self.b, e) => {
                    let t = self.tmp;
                    self.val_preset(e, cx, t, super::super::op::EVAL_COND);
                    self.b.jump_to(Code::IfNot, t, skip);
                }
                Cx::Acc => self.cond_sub(e, true, skip),
                Cx::Group(_) => {
                    let t = self.tmp;
                    self.val_sub(e, cx, t, EVAL_COND);
                    self.b.jump_to(Code::IfNot, t, skip);
                }
            }
            return;
        }
        let k = self.b.p4(P4::Expr(e.clone()));
        let (p1, flag) = match cx {
            Cx::Acc => (0, EVAL_ACC),
            Cx::Group(c) => (c.saturating_add(1), EVAL_GROUP),
        };
        let t = self.tmp;
        self.b
            .emit(Code::Eval, p1, t, 0, k, EVAL_COND | flag | self.wflag);
        self.b.jump_to(Code::IfNot, t, skip);
    }

    /// LIMIT, then OFFSET, before the loop (`computeLimitRegisters`:
    /// a LIMIT of 0 reads nothing, not even the OFFSET).
    fn limits(&mut self) -> Option<()> {
        let (l, o) = limits(
            self.b,
            self.qp.limit.as_ref(),
            self.qp.offset.as_ref(),
            self.gx,
            self.end,
        )?;
        self.lim = l;
        self.off = o;
        Some(())
    }

    /// One result row in registers `regs ..` (`n` values) into the
    /// sink, then the LIMIT and the reader's bound.
    pub(super) fn sink_row(&mut self, regs: u32, n: usize, rowid: u32) {
        emit_row(
            self.b, self.sink, regs, n, rowid, self.lim, self.cnt, self.end,
        );
    }

    /// The output mode (`stream_build`'s): sorted unless the one
    /// source's order (or, for groups, the GROUP BY order) delivers
    /// the ORDER BY; DISTINCT; plain.
    fn out_mode(&mut self, grouped: bool) -> Out {
        let qp = self.qp;
        let distinct = if qp.distinct {
            let e = self.b.cursor();
            self.b.emit2(Code::EphOpen, e, 0);
            Some(e)
        } else {
            None
        };
        let sorted = if self.win.is_some() {
            // (The units come in window order.)
            qp.order_by.is_empty()
        } else if grouped {
            crate::session::groups_sorted(
                qp.group_by.as_slice(),
                qp.order_by.as_slice(),
                qp.sources.as_slice(),
            )
        } else {
            qp.order_by.is_empty()
                || self.free_order
                || (!qp.distinct && matches!(qp.sources.as_slice(), [sp] if rowid_order(sp, qp)))
        };
        if !sorted {
            let s = self.b.cursor();
            let mut orders = AVec::new();
            for (_, o) in qp.order_by.iter() {
                orders.push(*o);
            }
            let k = self.b.p4(P4::Sorter(alloc::boxed::Box::new(SortInfo {
                orders,
                nout: qp.items.len(),
                window: if distinct.is_none() && !grouped {
                    self.lim
                } else {
                    None
                },
            })));
            self.b.emit_p4(Code::SorterOpen, s, k);
            return Out::Sort(s, distinct);
        }
        match distinct {
            Some(e) => Out::Distinct(e),
            None => Out::Plain,
        }
    }

    /// One output row from the values `cx` computes (`vals`: already
    /// in registers, the streamed fold's), `cont` the next row's code.
    fn emit(&mut self, out: Out, cx: Cx, vals: Option<u32>, cont: Label) {
        let (items_e, order_e) = (self.items, self.order);
        let n = items_e.len();
        let rowid = if matches!(cx, Cx::Group(_)) || vals.is_some() {
            0
        } else {
            self.rowid
        };
        let items = match vals {
            Some(r) => r,
            None => self.b.regs(n.saturating_add(order_e.len())),
        };
        let compute = |g: &mut G<'p, '_>| {
            if vals.is_none() {
                for (i, it) in items_e.iter().enumerate() {
                    g.val(it, cx, items.saturating_add(i as u32));
                }
            }
        };
        match out {
            Out::Plain => {
                if let Some(o) = self.off {
                    self.b.jump3(Code::IfPos, o, cont, 1);
                }
                compute(self);
                self.sink_row(items, n, rowid);
            }
            Out::Distinct(e) => {
                compute(self);
                let colls = self.b.p4(P4::Colls(crate::query::exprs_colls(
                    self.qp.items.as_slice(),
                )));
                self.b.jump(Code::Distinct, e, cont, items, colls, 0);
                if let Some(o) = self.off {
                    self.b.jump3(Code::IfPos, o, cont, 1);
                }
                self.sink_row(items, n, rowid);
            }
            Out::Sort(s, distinct) => {
                compute(self);
                for (j, (e, _)) in order_e.iter().enumerate() {
                    self.val(e, cx, items.saturating_add(n.saturating_add(j) as u32));
                }
                // (A window query's keys are its terms' values, as
                // today's `win_project_async` computes them.)
                let cols = if self.win.is_some() {
                    AVec::new()
                } else {
                    crate::session::order_cols(
                        self.qp.order_by.as_slice(),
                        self.qp.items.as_slice(),
                    )
                };
                for (j, col) in cols.iter().enumerate() {
                    if let Some(i) = col {
                        self.b.emit(
                            Code::Copy,
                            items.saturating_add(*i as u32),
                            items.saturating_add(n.saturating_add(j) as u32),
                            0,
                            0,
                            0,
                        );
                    }
                }
                if let Some(e) = distinct {
                    let colls = self.b.p4(P4::Colls(crate::query::exprs_colls(
                        self.qp.items.as_slice(),
                    )));
                    self.b.jump(Code::Distinct, e, cont, items, colls, 0);
                }
                self.b.emit3(Code::SorterInsert, s, items, rowid);
            }
        }
    }

    /// A complete joined row: the WHERE clause, the semi-join loops'
    /// end, then the output or the aggregates; `cont` is the next
    /// row's code.
    fn body(&mut self, out: Out, next: Label) {
        let qp = self.qp;
        // The side-table entries this row adds go once it is dealt
        // with (`Stream::row_side`), unless something keeps it.
        #[cfg(any(feature = "json", feature = "fts5"))]
        let side = self.track_side().then(|| {
            let r = self.b.regs(2);
            self.b.emit2(Code::SideMark, r, 0);
            (r, self.b.label())
        });
        #[cfg(any(feature = "json", feature = "fts5"))]
        let cont = side.map_or(next, |s| s.1);
        #[cfg(not(any(feature = "json", feature = "fts5")))]
        let cont = next;
        if let Some(f) = &qp.filter {
            #[cfg(feature = "sql")]
            self.where_sub(f, cont);
            #[cfg(not(feature = "sql"))]
            self.test(f, Cx::Acc, cont);
        }
        #[cfg(feature = "planner")]
        for (d, sp) in qp.sources.iter().enumerate() {
            if sp.exists
                && let Some(l) = self.lvl(d)
            {
                let r = l.done;
                self.b.emit3(Code::Integer, 0, r, 1);
            }
        }
        match self.fold {
            #[cfg(feature = "window")]
            Fold::Rows if self.win.is_some() => {
                let w = self.win.unwrap_or(0);
                self.b.emit2(Code::WinRow, w, 0);
            }
            Fold::Rows => self.emit(out, Cx::Acc, None, cont),
            Fold::Agg(a) => self.b.emit2(Code::AggStep, a, 0),
            Fold::Groups(gc) => {
                // The HAVING terms the pin moves into the WHERE clause
                // (constant or GROUP BY terms, `having_early`): tested
                // on each row, in turn, before it joins its group.
                if let Some(h) = &qp.having {
                    let mut early = AVec::new();
                    crate::session::having_early(h, qp.group_by.as_slice(), &mut early);
                    for c in early.iter() {
                        self.test(c, Cx::Acc, cont);
                    }
                }
                let k = self.b.regs(qp.group_by.len());
                for (i, e) in qp.group_by.iter().enumerate() {
                    self.val(e, Cx::Acc, k.saturating_add(i as u32));
                }
                self.b.emit3(Code::GroupAdd, gc, 0, k);
            }
        }
        #[cfg(any(feature = "json", feature = "fts5"))]
        if let Some((r, l)) = side {
            // A kept row (an aggregate's, a group's) pins what it
            // points into.
            // (The streamed fold pins a row it keeps itself, `AggStep`.)
            if matches!(self.fold, Fold::Groups(_)) || self.win.is_some() {
                self.b.emit2(Code::SidePin, 0, 0);
            }
            self.b.here(l);
            self.b.emit2(Code::SideDrop, r, 0);
        }
        self.b.jump_to(Code::Goto, 0, next);
    }

    /// The loops of sources `d ..` (then the body), for the joined row
    /// so far; `exit` follows when source `d`'s loop is over.
    fn from(&mut self, d: usize, exit: Label, out: Out) -> Option<()> {
        let qp = self.qp;
        let Some(sp) = qp.sources.as_slice().get(d) else {
            self.body(out, exit);
            return Some(());
        };
        let (t, e, any, pad, done) = {
            let l = self.lvl(d)?;
            (l.t, l.e, l.any, l.pad, l.done)
        };
        let base = self.base(d);
        let ncols = sp.ncols as u32;
        let top = self.b.label();
        let next = self.b.label();
        let pad_chk = self.b.label();
        let inner = self.b.label();
        let fin = self.b.label();
        self.b.emit2(Code::Integer, 0, any);
        self.b.emit2(Code::Integer, 0, pad);
        self.b.emit2(Code::Integer, 0, done);
        // Open: the first row, or `pad_chk` when there is none.
        // A driver-read table-valued or virtual source: the side-table
        // entries its loop adds go when it ends (`Stream::side_drop`).
        #[cfg(any(feature = "json", feature = "fts5"))]
        let side = if self.track_side() && tvf_kind(&self.lvl(d)?.kind) {
            let r = self.b.regs(2);
            self.b.emit2(Code::SideMark, r, 0);
            Some(r)
        } else {
            None
        };
        let adv = self.open(d, top, pad_chk, fin)?;
        self.b.here(top);
        if let Some((code, r)) = adv.stop {
            let tmp = self.tmp;
            self.b.emit2(Code::Rowid, t, tmp);
            self.b.jump3(code, tmp, pad_chk, r);
        }
        let src = if adv.list { e } else { t };
        self.b.emit(Code::Load, src, base, self.src(d), 0, 0);
        if let Some(on) = &sp.on {
            self.test(on, Cx::Acc, next);
        }
        self.b.emit3(Code::Integer, 0, any, 1);
        if sp.kind.pads_left() {
            self.b.emit2(Code::EphMatch, e, 0);
        }
        self.b.here(inner);
        self.from(d.saturating_add(1), next, out)?;
        self.b.here(next);
        self.b.jump_to(Code::If, pad, fin);
        #[cfg(feature = "planner")]
        if sp.exists {
            self.b.jump_to(Code::If, done, fin);
        }
        if let Some((code, c)) = adv.next {
            self.b.jump(code, c, top, adv.p3, adv.p4, 0);
        }
        // A growing first FROM subquery read to the end of its rows:
        // computed again under twice the bound, the loop going on
        // after the rows it read (`Stream::grow`).
        #[cfg(feature = "sql")]
        if d == 0
            && let (Some((n, pos)), Kind::Derived(p)) = (self.grow, &self.lvl(0)?.kind)
        {
            let p = *p;
            let nogrow = self.b.label();
            self.b.jump(Code::Grow, e, nogrow, n, pos, 0);
            self.derived_build(p, e, Some(n))?;
            self.b.jump3(Code::EphSeek, e, pad_chk, pos);
            self.b.jump_to(Code::Goto, 0, top);
            self.b.here(nogrow);
        }
        self.b.here(pad_chk);
        if sp.kind.pads_right() {
            self.b.jump_to(Code::If, any, fin);
            self.b.emit(Code::NullRow, ncols, base, self.src(d), 0, 0);
            self.b.emit3(Code::Integer, 0, pad, 1);
            self.b.jump_to(Code::Goto, 0, inner);
        }
        self.b.here(fin);
        // The values a lateral table-valued call cleared, back.
        #[cfg(any(feature = "json", feature = "vtab"))]
        if sp.tvf & crate::query::TVF_LATERAL != 0 && !sp.tvf_dead.is_empty() {
            let c = if direct(&self.lvl(d)?.kind) { t } else { e };
            self.b.emit2(Code::Restore, c, 0);
        }
        #[cfg(any(feature = "json", feature = "fts5"))]
        if let Some(r) = side {
            self.b.emit2(Code::SideDrop, r, 0);
        }
        self.b.jump_to(Code::Goto, 0, exit);
        Some(())
    }

    /// The unmatched rows of RIGHT / FULL source `i` after the main
    /// loop (`join_sources`): NULLs for the sources before it, the
    /// row, then the sources after it.
    fn second_pass(&mut self, i: usize, out: Out) -> Option<()> {
        let qp = self.qp;
        let e = self.lvl(i)?.e;
        self.build_once(i)?;
        let after = self.b.label();
        let top = self.b.label();
        let next = self.b.label();
        self.b.jump(Code::EphRewind, e, after, 0, 0, 1);
        self.b.here(top);
        for (j, sp) in qp.sources.iter().enumerate().take(i) {
            let base = self.base(j);
            self.b.emit3(
                Code::NullRow,
                sp.ncols as u32,
                base,
                self.n0.saturating_add(j) as u32,
            );
        }
        let base = self.base(i);
        self.b.emit(Code::Load, e, base, self.src(i), 0, 0);
        self.from(i.saturating_add(1), next, out)?;
        self.b.here(next);
        self.b.jump(Code::EphNext, e, top, 0, 0, 1);
        self.b.here(after);
        Some(())
    }

    /// The aggregates' output after the loops: the fold's one row, or
    /// each group in GROUP BY order (HAVING first).
    fn finish_agg(&mut self, out: Out) {
        let qp = self.qp;
        match self.fold {
            Fold::Rows => {}
            Fold::Agg(a) => {
                let n = qp.items.len();
                let vals = self.b.regs(n);
                let plan = self.plan;
                self.b.emit(Code::AggFinal, a, vals, 0, plan, 0);
                let end = self.end;
                self.emit(out, Cx::Acc, Some(vals), end);
            }
            Fold::Groups(gc) => {
                let top = self.b.label();
                let done = self.b.label();
                self.b.emit2(Code::GroupSort, gc, 0);
                self.b.here(top);
                self.b.jump_to(Code::GroupNext, gc, done);
                #[cfg(feature = "sql")]
                self.rep_search(gc);
                let cx = Cx::Group(gc);
                if let Some(h) = &qp.having {
                    // (Its moved terms were tested on the rows.) Every
                    // aggregate finalized, then the HAVING itself.
                    let mut aggs =
                        crate::session::group_aggs(qp.items.as_slice(), qp.order_by.as_slice());
                    crate::query::collect_outer_aggs(h, &mut aggs);
                    let scratch = self.b.reg();
                    for a in aggs.iter() {
                        self.val(a, cx, scratch);
                    }
                    self.test(h, cx, top);
                }
                #[cfg(feature = "window")]
                if let Some(w) = self.win {
                    self.win_unit(w, cx, gc.saturating_add(1));
                    self.b.jump_to(Code::Goto, 0, top);
                    self.b.here(done);
                    return;
                }
                self.emit(out, cx, None, top);
                self.b.jump_to(Code::Goto, 0, top);
                self.b.here(done);
            }
        }
    }

    /// The sorted rows inside the LIMIT / OFFSET window.
    fn sorted_out(&mut self, out: Out) {
        let Out::Sort(s, _) = out else {
            return;
        };
        let n = self.qp.items.len();
        let end = self.end;
        let regs = self.b.regs(n);
        let more = self.b.label();
        let skip = self.b.label();
        self.b.jump_to(Code::SorterSort, s, end);
        self.b.here(more);
        if let Some(o) = self.off {
            self.b.jump3(Code::IfPos, o, skip, 1);
        }
        self.b.emit2(Code::SorterData, s, regs);
        let rowid = s.saturating_add(1);
        self.sink_row(regs, n, rowid);
        self.b.here(skip);
        self.b.jump_to(Code::SorterNext, s, more);
    }
}

#[cfg(feature = "sql")]
impl G<'_, '_> {
    /// The bound a first FROM subquery is computed under
    /// (`session::derived_need`), or its growing bound
    /// (`session::streams_first`: an endless recursive CTE read
    /// through a WHERE or a join under a LIMIT): the rows the reader
    /// takes, OFFSET + the least of LIMIT and its own reader's bound.
    fn grow_setup(&mut self) {
        let qp = self.qp;
        // A FROM subquery, or a table-valued source the driver reads.
        let first = self.lvl(0).map(|l| l.kind);
        let derived = matches!(first, Some(Kind::Derived(_)));
        let tvf = first.is_some_and(|k| tvf_kind(&k) && !matches!(k, Kind::Host(_, true)));
        // Window calls read every input row: no bound passes into the
        // first source (today's `run_query_sub` / `exec_subplan`'s
        // `wins`; review fix: a LIMIT, or a scalar subquery's one row,
        // bounded a derived or table-valued source's rows).
        if (!derived && !tvf) || self.win.is_some() {
            return;
        }
        let plain = qp.group_by.is_empty() && !qp.has_agg && qp.order_by.is_empty() && !qp.distinct;
        let grow = crate::session::streams_first(qp.sources.as_slice(), plain)
            && (qp.filter.is_some() || qp.sources.len() > 1);
        let reduced = qp.filter.is_some() || !qp.group_by.is_empty() || qp.has_agg;
        let reordered = !qp.order_by.is_empty() || qp.distinct;
        // (A compound's cores read their first source whole, as today's
        // `exec_qp_core_values` and compound finish do: its LIMIT is
        // the compound's.)
        let bounded = qp.sources.len() == 1 && !reduced && !reordered && !self.arm;
        if !grow && !bounded {
            return;
        }
        let r = self.b.reg();
        let refp = |x: Option<u32>| x.map_or(0, |v| v.saturating_add(1));
        let (l, o, n) = (refp(self.lim), refp(self.off), refp(self.need));
        self.b.emit(Code::Need, r, l, o, n, 0);
        if grow {
            let pos = self.b.reg();
            self.b.emit2(Code::Integer, 0, pos);
            self.grow = Some((r, pos));
        } else {
            self.dneed = Some(r);
        }
    }

    /// Code that fills row list `e` with FROM subquery `p`'s rows for
    /// this execution (`materialise_sources_sub`), under the bound in
    /// register `need` (−1: none): its own code, over the joined row's
    /// slots before this core's (the joined row so far is saved
    /// around it); a shared materialization (`SubPlan::mat`) computed
    /// by its first reader and copied by every other.
    fn derived_build(
        &mut self,
        p: &crate::query::SubPlan,
        e: u32,
        need: Option<u32>,
    ) -> Option<()> {
        let at = (self.w0, self.n0);
        let gx = self.gx;
        self.b.emit2(Code::AccPush, 0, 0);
        if p.mat == 0 {
            self.b.emit2(Code::EphOpen, e, 0);
            self.b.emit2(Code::Count, 0, 0);
            super::sub::subplan(self.b, p, gx, at, need, Sink::List(e))?;
        } else {
            let (se, flags) = match self.b.mats.iter().find(|m| m.0 == p.mat) {
                Some(m) => (m.1, m.2),
                None => {
                    let se = self.b.cursor();
                    let flags = self.b.regs(2);
                    self.b.mats.push((p.mat, se, flags));
                    (se, flags)
                }
            };
            let none = self.b.reg();
            self.b.emit3(Code::Integer, 0, none, -1i32 as u32);
            let needr = need.unwrap_or(none);
            let ask = if p.rec.is_some() { need } else { None };
            let skip = self.b.label();
            self.b.jump(Code::MatTest, se, skip, needr, flags, 0);
            self.b.emit2(Code::EphOpen, se, 0);
            self.b.emit2(Code::Count, 0, 0);
            super::sub::subplan(self.b, p, gx, at, ask, Sink::List(se))?;
            self.b
                .emit(Code::MatDone, se, 0, ask.unwrap_or(none), flags, 0);
            self.b.here(skip);
            self.b.emit2(Code::EphOpen, e, 0);
            self.b.emit2(Code::EphCopy, e, se);
        }
        for c in p.real_cols.iter() {
            self.b.emit2(Code::EphReal, e, *c as u32);
        }
        self.b.emit2(Code::AccPop, 0, 0);
        Some(())
    }
}

/// How a loop opened by `G::open` advances.
struct Adv {
    /// The advance instruction and its cursor (`None`: one row), and
    /// its `p3`.
    next: Option<(Code, u32)>,
    p3: u32,
    p4: u32,
    /// The end test at the top of each row: the rowid past the
    /// bound register ends the loop.
    stop: Option<(Code, u32)>,
    /// The loop reads the row list.
    list: bool,
}

/// One result row in registers `regs ..` (`n` values, the rowid of
/// `rowid`'s source) into `sink`; then the LIMIT `lim` and the
/// reader's counter `cnt`, each ending the code at `end` once 0.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_row(
    b: &mut Builder,
    sink: Sink,
    regs: u32,
    n: usize,
    rowid: u32,
    lim: Option<u32>,
    cnt: Option<u32>,
    end: Label,
) {
    match sink {
        Sink::Result => b.emit3(Code::ResultRow, regs, n as u32, rowid),
        #[cfg(feature = "sql")]
        Sink::List(e) => b.emit3(Code::EphPut, e, regs, n as u32),
        #[cfg(feature = "sql")]
        Sink::Sub(slot, info) => b.emit(Code::SubRow, slot, regs, n as u32, info, 0),
        #[cfg(feature = "sql")]
        Sink::Queue(q) => b.emit3(Code::QueueAdd, q, regs, n as u32),
    }
    let _ = rowid;
    if let Some(l) = lim {
        b.jump_to(Code::DecrJumpZero, l, end);
    }
    if let Some(c) = cnt {
        b.jump_to(Code::DecrJumpZero, c, end);
    }
}

/// A VALUES core's rows (`sql`): each row's values over the joined
/// row's first `at` slots and sources, into `sink`, after the OFFSET
/// `off` and up to the LIMIT `lim` and the counter `cnt`.
#[cfg(feature = "sql")]
#[allow(clippy::too_many_arguments)]
pub(super) fn values_core(
    b: &mut Builder,
    qp: &QueryPlan,
    gx: Gx<'_>,
    at: (usize, usize),
    lim: Option<u32>,
    off: Option<u32>,
    cnt: Option<u32>,
    sink: Sink,
    end: Label,
) -> Option<()> {
    let tmp = b.reg();
    let mut g = G {
        b,
        qp,
        gx,
        plan: 0,
        bases: AVec::new(),
        lv: AVec::new(),
        end,
        lim,
        off,
        tmp,
        rowid: 0,
        fold: Fold::Rows,
        w0: at.0,
        n0: at.1,
        width: at.0,
        nsrc: at.1,
        sink,
        need: None,
        cnt,
        grow: None,
        dneed: None,
        items: qp.items.as_slice(),
        order: qp.order_by.as_slice(),
        win: None,
        streamed: false,
        arm: false,
        wflag: 0,
        free_order: false,
    };
    for r in qp.values.iter() {
        // A row's window calls run over that row alone, as the pin's
        // one-row SELECT core it is (`eval_values_row`, unit
        // `vm_rest`).
        #[cfg(feature = "window")]
        let windowed = any_win(r.iter());
        #[cfg(feature = "window")]
        let wins = if windowed {
            crate::window::calls(r.as_slice(), &[])
        } else {
            AVec::new()
        };
        #[cfg(not(feature = "window"))]
        let windowed = false;
        if !r
            .iter()
            .all(|e| evaluable_w(e, at.0, at.1, false, windowed))
        {
            return None;
        }
        let next = g.b.label();
        let regs = g.b.regs(r.len());
        #[cfg(feature = "window")]
        if windowed {
            let mut calls = AVec::with_capacity(wins.len());
            for w in wins.iter() {
                calls.push((*w).clone());
            }
            let n: usize = wins.iter().map(|c| crate::window::children(c).len()).sum();
            let k = g.b.p4(P4::Win(calls));
            let w = g.b.cursor();
            g.b.emit_p4(Code::WinOpen, w, k);
            g.b.emit2(Code::WinRow, w, 0);
            g.b.jump(Code::WinLoad, w, next, 0, 0, 1);
            let kids = g.b.regs(n);
            let mut j = 0u32;
            for c in wins.iter() {
                for x in crate::window::children(c) {
                    g.val(x, Cx::Acc, kids.saturating_add(j));
                    j = j.saturating_add(1);
                }
            }
            g.b.emit2(Code::WinAdd, w, kids);
            g.b.emit2(Code::WinCompute, w, 0);
            g.b.jump(Code::WinNext, w, next, 0, 0, 1);
            g.wflag = super::super::op::EVAL_WIN;
            for (i, e) in r.iter().enumerate() {
                let t = crate::window::strip(e);
                g.val(&t, Cx::Acc, regs.saturating_add(i as u32));
            }
            g.wflag = 0;
        }
        if !windowed {
            for (i, e) in r.iter().enumerate() {
                g.val(e, Cx::Acc, regs.saturating_add(i as u32));
            }
        }
        if let Some(o) = off {
            g.b.jump3(Code::IfPos, o, next, 1);
        }
        g.sink_row(regs, r.len(), 0);
        g.b.here(next);
    }
    (!g.b.failed()).then_some(())
}

/// LIMIT, then OFFSET, into registers (`computeLimitRegisters`: a
/// LIMIT of 0 jumps to `end` and reads nothing, not even the OFFSET).
/// They were planned against the empty row: a subquery in one runs
/// over an empty joined row (`sql`).
pub(super) fn limits(
    b: &mut Builder,
    limit: Option<&RExpr>,
    offset: Option<&RExpr>,
    gx: Gx<'_>,
    end: Label,
) -> Option<(Option<u32>, Option<u32>)> {
    let Some(l) = limit else {
        return Some((None, None));
    };
    let r = b.regs(2);
    scope_val(b, l, gx, r)?;
    b.emit2(Code::MustBeInt, r, 0);
    b.jump_to(Code::IfNot, r, end);
    let mut off = None;
    if let Some(o) = offset {
        let ro = r.saturating_add(1);
        scope_val(b, o, gx, ro)?;
        b.emit2(Code::MustBeInt, ro, 0);
        off = Some(ro);
    }
    Some((Some(r), off))
}

/// `e`'s value over the empty row into `dest` (a LIMIT or OFFSET).
fn scope_val(b: &mut Builder, e: &RExpr, gx: Gx<'_>, dest: u32) -> Option<()> {
    #[cfg(feature = "sql")]
    if crate::query::has_sub(e) {
        return super::sub::empty_scope_val(b, e, gx, dest);
    }
    let _ = gx;
    // A literal is loaded here, not in the prologue: the LIMIT / OFFSET
    // registers count down, and a subquery's run again.
    if let RExpr::Literal(v) = e {
        let k = b.p4(P4::Value(v.clone()));
        b.emit(Code::Value, 0, dest, 0, k, 0);
        return Some(());
    }
    super::expr::value(b, e, None, dest);
    Some(())
}

/// Whether ORDER BY is the one source's own row order (the streamed
/// executor's `rowid_order`).
fn rowid_order(sp: &SubSource, qp: &QueryPlan) -> bool {
    let order = qp.order_by.as_slice();
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        return !a.lateral && a.sorted >= order.len();
    }
    let [(e, o)] = order else {
        return false;
    };
    let plain = sp.derived.is_none();
    #[cfg(feature = "sql")]
    let plain = plain && !sp.temp;
    plain
        && !o.desc
        && match e {
            RExpr::Rowid(0) => true,
            RExpr::Column(slot, ..) => sp.rowid_alias == Some(*slot),
            _ => false,
        }
}

/// Where a walk's equality value for one key column comes from.
#[cfg(feature = "expr_index")]
#[derive(Clone, Copy)]
enum PSrc {
    /// A probe set's row list, walked in turn.
    List(u32),
    /// One value in a register (a skip-scan's prefix value).
    #[cfg(feature = "planner")]
    Reg(u32),
}

#[cfg(feature = "expr_index")]
impl PSrc {
    /// The row list, for a set of values.
    fn list(self) -> Option<u32> {
        match self {
            PSrc::List(l) => Some(l),
            #[cfg(feature = "planner")]
            PSrc::Reg(_) => None,
        }
    }
}

impl<'p> G<'p, '_> {
    /// Open source `d`'s loop on its first row for the joined row so
    /// far (jumping to `none` when there is no row); how it advances.
    fn open(&mut self, d: usize, top: Label, none: Label, nopad: Label) -> Option<Adv> {
        let _ = (top, nopad);
        let (t, e) = {
            let l = self.lvl(d)?;
            (l.t, l.e)
        };
        #[cfg(feature = "planner")]
        self.bloom_check(d, none)?;
        let kk = self.lvl(d)?.kind;
        let kind = match kk {
            Kind::Scan => 0,
            #[cfg(feature = "expr_index")]
            Kind::Eq(..) => 1,
            #[cfg(feature = "expr_index")]
            Kind::Range(..) => 2,
            Kind::Mat => 3,
            #[cfg(feature = "expr_index")]
            Kind::Lateral => 4,
            #[cfg(feature = "planner")]
            Kind::Auto(..) => 5,
            #[cfg(feature = "sql")]
            Kind::Derived(_) => 3,
            #[cfg(feature = "sql")]
            Kind::Rec => 6,
            #[cfg(feature = "sql")]
            Kind::Host(_, false) => 3,
            #[cfg(feature = "sql")]
            Kind::Host(mode, true) => {
                self.b.emit2(Code::EphOpen, e, 0);
                self.host_rows(d, e, mode, Some(nopad));
                self.b.jump_to(Code::EphRewind, e, none);
                return Some(Adv {
                    next: Some((Code::EphNext, e)),
                    p3: 0,
                    p4: 0,
                    stop: None,
                    list: true,
                });
            }
            #[cfg(feature = "sql")]
            Kind::HostCur(mode) => {
                let src = self.src_entry(d);
                let need = self.first_need(d);
                self.b.jump(Code::HostOpen, t, nopad, need, src, mode);
                let cap = self.vtab_cap();
                self.b.jump(Code::HostNext, t, none, cap, src, 1);
                return Some(Adv {
                    next: Some((Code::HostNext, t)),
                    p3: cap,
                    p4: src,
                    stop: None,
                    list: false,
                });
            }
            #[cfg(not(feature = "expr_index"))]
            Kind::Never(_) => return None,
        };
        let list = Adv {
            next: Some((Code::EphNext, e)),
            p3: 0,
            p4: 0,
            stop: None,
            list: true,
        };
        Some(match kind {
            0 => {
                self.b.jump_to(Code::Rewind, t, none);
                Adv {
                    next: Some((Code::Next, t)),
                    p3: 0,
                    p4: 0,
                    stop: None,
                    list: false,
                }
            }
            #[cfg(feature = "expr_index")]
            1 => {
                let Kind::Eq(pe, aff, bounds) = self.lvl(d)?.kind else {
                    return None;
                };
                // The bounds first (a NULL one finds no row), then the
                // one rowid (`xindex::access_walk`).
                let regs = self.bound_vals(bounds);
                for r in regs.into_iter().flatten() {
                    self.b.jump_to(Code::IsNull, r.0, none);
                }
                let r = self.b.reg();
                self.val_mut(pe, Cx::Acc, r);
                let k = self.b.p4(P4::Aff(aff));
                self.b.emit_p4(Code::Affinity, r, k);
                self.b.jump3(Code::SeekRowid, t, none, r);
                Adv {
                    next: None,
                    p3: 0,
                    p4: 0,
                    stop: None,
                    list: false,
                }
            }
            #[cfg(feature = "expr_index")]
            2 => {
                let Kind::Range(bounds, reverse) = self.lvl(d)?.kind else {
                    return None;
                };
                let [lo, hi] = self.rowid_ends(bounds, none);
                let (from, to) = if reverse { (hi, lo) } else { (lo, hi) };
                let (seek, first) = if reverse {
                    (Code::SeekLE, Code::Last)
                } else {
                    (Code::SeekGE, Code::Rewind)
                };
                match from {
                    Some(r) => {
                        let whole = self.b.label();
                        self.b.jump_to(Code::IsNull, r, whole);
                        self.b.jump3(seek, t, none, r);
                        self.b.jump_to(Code::Goto, 0, top);
                        self.b.here(whole);
                        self.b.jump_to(first, t, none);
                    }
                    None => self.b.jump_to(first, t, none),
                }
                Adv {
                    next: Some((if reverse { Code::Prev } else { Code::Next }, t)),
                    p3: 0,
                    p4: 0,
                    stop: to.map(|r| (if reverse { Code::Lt } else { Code::Gt }, r)),
                    list: false,
                }
            }
            3 => {
                self.build_once(d)?;
                self.b.jump_to(Code::EphRewind, e, none);
                list
            }
            #[cfg(feature = "sql")]
            6 => {
                self.b.jump_to(Code::EphRewind, e, none);
                list
            }
            #[cfg(feature = "expr_index")]
            4 => {
                self.b.emit2(Code::EphOpen, e, 0);
                self.build(d, e)?;
                self.b.jump_to(Code::EphRewind, e, none);
                list
            }
            #[cfg(feature = "planner")]
            5 => {
                let Kind::Auto(ai, _) = self.lvl(d)?.kind else {
                    return None;
                };
                self.build_once(d)?;
                // The probe: each key's value converted, a NULL meeting
                // `=` finding nothing (`join::auto_probe`).
                let regs = self.b.regs(ai.keys.len());
                for (i, k) in ai.keys.iter().enumerate() {
                    let r = regs.saturating_add(i as u32);
                    self.val_mut(&k.val, Cx::Acc, r);
                    let a = self.b.p4(P4::Aff(k.aff));
                    self.b.emit_p4(Code::Affinity, r, a);
                    if !k.is {
                        self.b.jump_to(Code::IsNull, r, none);
                    }
                }
                let a = self.b.p4(P4::Auto(alloc::boxed::Box::new(ai.clone())));
                self.b.jump(Code::AutoSpan, e, none, regs, a, 0);
                list
            }
            _ => return None,
        })
    }

    /// Build source `d`'s row list the first time its loop opens.
    fn build_once(&mut self, d: usize) -> Option<()> {
        let (e, built) = {
            let l = self.lvl(d)?;
            (l.e, l.built)
        };
        let skip = self.b.label();
        self.b.jump_to(Code::Once, built, skip);
        self.b.emit2(Code::EphOpen, e, 0);
        self.build(d, e)?;
        // Its rows stay for the execution: so does what they point
        // into.
        #[cfg(any(feature = "json", feature = "fts5"))]
        if self.track_side() {
            self.b.emit2(Code::SidePin, 0, 0);
        }
        self.b.here(skip);
        Some(())
    }

    /// Code that fills row list `e` with source `d`'s rows: its access
    /// path's, its automatic index's (sorted), or its table's.
    fn build(&mut self, d: usize, e: u32) -> Option<()> {
        let qp = self.qp;
        let sp = qp.sources.as_slice().get(d)?;
        #[cfg(feature = "sql")]
        match self.lvl(d)?.kind {
            Kind::Derived(p) => {
                let need = if d == 0 {
                    self.grow.map(|g| g.0).or(self.dneed)
                } else {
                    None
                };
                return self.derived_build(p, e, need);
            }
            Kind::Rec => return Some(()),
            Kind::Host(mode, _) => {
                self.host_rows(d, e, mode, None);
                return Some(());
            }
            _ => {}
        }
        #[cfg(feature = "planner")]
        if let Kind::Auto(ai, host) = self.lvl(d)?.kind {
            // A WITHOUT ROWID table's rows, sorted by the driver.
            if host {
                self.host_rows(d, e, super::super::op::HOST_AUTO, None);
                return Some(());
            }
            self.scan_into(d, e);
            let a = self.b.p4(P4::Auto(alloc::boxed::Box::new(ai.clone())));
            self.b.emit_p4(Code::AutoSort, e, a);
            return Some(());
        }
        #[cfg(feature = "expr_index")]
        if let Some(a) = sp.access.as_deref() {
            return self.access_rows(d, a, e, false);
        }
        let _ = sp;
        self.scan_into(d, e);
        Some(())
    }

    /// Every row of source `d`'s table, in rowid order, into `e`.
    fn scan_into(&mut self, d: usize, e: u32) {
        let Some(t) = self.lvl(d).map(|l| l.t) else {
            return;
        };
        let top = self.b.label();
        let done = self.b.label();
        self.b.jump_to(Code::Rewind, t, done);
        self.b.here(top);
        self.b.emit2(Code::EphAdd, e, t);
        self.b.jump_to(Code::Next, t, top);
        self.b.here(done);
    }

    /// A lateral lookup's Bloom filter (`join::bloom_rows` /
    /// `bloom_pass`): built from every table row at the first probe,
    /// then the probe's key tested; `none` when it is not in it.
    #[cfg(feature = "planner")]
    fn bloom_check(&mut self, d: usize, none: Label) -> Option<()> {
        let qp = self.qp;
        let Some((bl, built)) = self.lvl(d)?.bloom else {
            return Some(());
        };
        let a = qp.sources.as_slice().get(d)?.access.as_deref()?;
        let keys = a.bloom.as_ref()?;
        // An index expression key: the filter admits every probe.
        if keys.as_slice().contains(&crate::xplan::BKey::Expr) {
            return Some(());
        }
        let t = self.lvl(d)?.t;
        let kp = self.b.p4(P4::BKeys(keys.clone()));
        let skip = self.b.label();
        self.b.jump_to(Code::Once, built, skip);
        self.b.emit2(Code::EphOpen, bl, 0);
        self.b.emit(Code::BloomAdd, bl, 0, 0, kp, 1);
        let top = self.b.label();
        let done = self.b.label();
        // A WITHOUT ROWID table's rows come from the driver
        // (`join::bloom_rows` reads them with `scan_source`).
        #[cfg(feature = "sql")]
        if matches!(self.lvl(d)?.kind, Kind::Host(..)) {
            let l = self.b.cursor();
            self.b.emit2(Code::EphOpen, l, 0);
            self.host_rows(d, l, super::super::op::HOST_SCAN, None);
            self.b.jump_to(Code::EphRewind, l, done);
            self.b.here(top);
            self.b.emit(Code::BloomAdd, bl, l, 0, kp, 0);
            self.b.jump_to(Code::EphNext, l, top);
            self.b.here(done);
            self.b.here(skip);
            return self.bloom_probe(a, keys, bl, none);
        }
        self.b.jump_to(Code::Rewind, t, done);
        self.b.here(top);
        self.b.emit(Code::BloomAdd, bl, t, 0, kp, 0);
        self.b.jump_to(Code::Next, t, top);
        self.b.here(done);
        self.b.here(skip);
        self.bloom_probe(a, keys, bl, none)
    }

    /// A lateral lookup's probe of its Bloom filter `bl` (`none` when
    /// the key is not in it).
    #[cfg(feature = "planner")]
    fn bloom_probe(
        &mut self,
        a: &crate::xplan::IdxAccess,
        keys: &AVec<crate::xplan::BKey>,
        bl: u32,
        none: Label,
    ) -> Option<()> {
        // The probes, in order (`join::bloom_pass`); an IN or
        // multi-value probe stops the test (every earlier probe still
        // evaluated).
        let regs = self.b.regs(keys.len());
        for (i, (_, p)) in keys.iter().zip(a.eq.iter()).enumerate() {
            let ([v], None, false) = (p.vals.as_slice(), &p.sub, p.is_in) else {
                return Some(());
            };
            let r = regs.saturating_add(i as u32);
            self.val_mut(v, Cx::Acc, r);
            let k = self.b.p4(P4::Aff(p.aff));
            self.b.emit_p4(Code::Affinity, r, k);
        }
        // The key is as long as the shorter of the two lists, as
        // `bloom_pass` zips them.
        let n = keys.len().min(a.eq.len());
        let mut kt = AVec::with_capacity(n);
        kt.extend_from_slice(keys.as_slice().get(..n).unwrap_or(&[]));
        let kt = self.b.p4(P4::BKeys(kt));
        self.b.jump(Code::BloomTest, bl, none, regs, kt, 0);
        Some(())
    }

    /// The range bounds' values, converted by their comparisons'
    /// affinity (`xindex::bound_val`): each register and inclusive.
    #[cfg(feature = "expr_index")]
    fn bound_vals(&mut self, bounds: Bounds<'_>) -> [Option<(u32, bool)>; 2] {
        let mut regs = [None, None];
        for (slot, bd) in regs.iter_mut().zip(bounds) {
            if let Some(bd) = bd {
                let r = self.b.reg();
                self.val_mut(&bd.e, Cx::Acc, r);
                let k = self.b.p4(P4::Aff(bd.aff));
                self.b.emit_p4(Code::Affinity, r, k);
                *slot = Some((r, bd.incl));
            }
        }
        regs
    }

    /// The range bounds as rowid range ends (`xindex::rowid_span`):
    /// NULL for an open end; a bound no rowid satisfies jumps to
    /// `none`.
    #[cfg(feature = "expr_index")]
    fn rowid_ends(&mut self, bounds: Bounds<'_>, none: Label) -> [Option<u32>; 2] {
        let regs = self.bound_vals(bounds);
        let mut out = [None, None];
        for (i, (slot, r)) in out.iter_mut().zip(regs).enumerate() {
            if let Some((r, incl)) = r {
                let flags = u8::from(i == 0) | (u8::from(incl) << 1);
                self.b.jump(Code::RowidBound, r, none, 0, 0, flags);
                *slot = Some(r);
            }
        }
        out
    }

    /// Code that appends the rows access path `a` finds for source
    /// `d` to row list `e`, in its order (`xindex::access_rows`).
    #[cfg(feature = "expr_index")]
    fn access_rows(
        &mut self,
        d: usize,
        a: &'p crate::xplan::IdxAccess,
        e: u32,
        skip_first: bool,
    ) -> Option<()> {
        #[cfg(feature = "planner")]
        if let Some(o) = a.or.as_deref() {
            return self.or_rows(d, o, e);
        }
        #[cfg(feature = "planner")]
        if !skip_first && let Some(f) = a.first.as_deref() {
            self.access_rows(d, a, e, true)?;
            return self.first_row(e, f);
        }
        let _ = skip_first;
        if a.root == 0 && a.eq.is_empty() {
            return self.rowid_range_rows(d, a, e);
        }
        self.walk(d, a, e)
    }

    /// The rowid B-tree walked over a range: its rows as the walk
    /// meets them, reversed for a reverse walk.
    #[cfg(feature = "expr_index")]
    fn rowid_range_rows(&mut self, d: usize, a: &crate::xplan::IdxAccess, e: u32) -> Option<()> {
        let t = self.lvl(d)?.t;
        let fin = self.b.label();
        let regs = self.bound_vals([a.lo.as_ref(), a.hi.as_ref()]);
        for r in regs.iter().flatten() {
            self.b.jump_to(Code::IsNull, r.0, fin);
        }
        let mut ends = [None, None];
        for (i, (slot, r)) in ends.iter_mut().zip(regs).enumerate() {
            if let Some((r, incl)) = r {
                let flags = u8::from(i == 0) | (u8::from(incl) << 1);
                self.b.jump(Code::RowidBound, r, fin, 0, 0, flags);
                *slot = Some(r);
            }
        }
        self.b.emit2(Code::EphMark, e, 0);
        let top = self.b.label();
        let done = self.b.label();
        match ends[0] {
            Some(r) => {
                let whole = self.b.label();
                self.b.jump_to(Code::IsNull, r, whole);
                self.b.jump3(Code::SeekGE, t, done, r);
                self.b.jump_to(Code::Goto, 0, top);
                self.b.here(whole);
                self.b.jump_to(Code::Rewind, t, done);
            }
            None => self.b.jump_to(Code::Rewind, t, done),
        }
        self.b.here(top);
        if let Some(h) = ends[1] {
            let tmp = self.tmp;
            self.b.emit2(Code::Rowid, t, tmp);
            self.b.jump3(Code::Gt, tmp, done, h);
        }
        self.b.emit2(Code::EphAdd, e, t);
        self.b.jump_to(Code::Next, t, top);
        self.b.here(done);
        if a.reverse {
            self.b.emit2(Code::EphReverse, e, 0);
        }
        self.b.here(fin);
        Some(())
    }

    /// One probe set's values into a new row list (`probe_vals`):
    /// `none` when it is empty.
    #[cfg(feature = "expr_index")]
    fn probe_set(
        &mut self,
        p: &crate::xplan::Probe,
        key: crate::record::KeyOrder,
        reverse: bool,
        none: Label,
    ) -> u32 {
        // An IN subquery's set (`probe_vals`): its node's values.
        #[cfg(feature = "sql")]
        if let Some(e) = &p.sub {
            let l = self.b.cursor();
            self.b.emit2(Code::EphOpen, l, 0);
            let k = self.b.p4(P4::Probe(alloc::boxed::Box::new(
                super::super::index::ProbeInfo {
                    n: 0,
                    aff: p.aff,
                    is: p.is,
                    is_in: p.is_in,
                    key,
                    reverse,
                },
            )));
            self.probe_sub(e, k, l, none);
            return l;
        }
        let regs = self.b.regs(p.vals.len());
        for (i, v) in p.vals.iter().enumerate() {
            self.val(v, Cx::Acc, regs.saturating_add(i as u32));
        }
        let l = self.b.cursor();
        self.b.emit2(Code::EphOpen, l, 0);
        let k = self.b.p4(P4::Probe(alloc::boxed::Box::new(
            super::super::index::ProbeInfo {
                n: p.vals.len(),
                aff: p.aff,
                is: p.is,
                is_in: p.is_in,
                key,
                reverse,
            },
        )));
        self.b.jump(Code::ProbeList, l, none, regs, k, 0);
        l
    }

    /// An index (or rowid) walk over each combination of the equality
    /// values (`xindex::access_walk`), its rows into `e`.
    #[cfg(feature = "expr_index")]
    fn walk(&mut self, d: usize, a: &crate::xplan::IdxAccess, e: u32) -> Option<()> {
        let t = self.lvl(d)?.t;
        let fin = self.b.label();
        let bounds = self.bound_vals([a.lo.as_ref(), a.hi.as_ref()]);
        for r in bounds.iter().flatten() {
            self.b.jump_to(Code::IsNull, r.0, fin);
        }
        // A skip-scan: each distinct value of the skipped leading
        // columns, in key order, then the walk with them (its bounds
        // and probes evaluated again per prefix).
        #[cfg(feature = "planner")]
        if a.skip > 0 && a.root != 0 {
            let ii = self.b.p4(P4::Index(alloc::boxed::Box::new(
                super::super::index::IndexInfo {
                    root: a.root,
                    desc: a.desc.clone(),
                },
            )));
            let i = self.b.cursor();
            let pl = self.b.cursor();
            let n = a.skip as u32;
            self.b.emit_p4(Code::IdxOpen, i, ii);
            self.b.emit2(Code::EphOpen, pl, 0);
            let top = self.b.label();
            let done = self.b.label();
            self.b.jump_to(Code::IdxRewind, i, done);
            self.b.here(top);
            self.b.emit(Code::SkipAdd, pl, i, n, ii, 0);
            self.b.jump_to(Code::IdxNext, i, top);
            self.b.here(done);
            if a.reverse {
                self.b.emit2(Code::EphReverse, pl, 0);
            }
            let ploop = self.b.label();
            let pnext = self.b.label();
            self.b.jump_to(Code::EphRewind, pl, fin);
            self.b.here(ploop);
            let pv = self.b.regs(a.skip);
            self.b.emit3(Code::ListValues, pl, pv, n);
            let bounds = self.bound_vals([a.lo.as_ref(), a.hi.as_ref()]);
            for r in bounds.iter().flatten() {
                self.b.jump_to(Code::IsNull, r.0, pnext);
            }
            let mut lists: AVec<PSrc> = AVec::new();
            for j in 0..a.skip {
                lists.push(PSrc::Reg(pv.saturating_add(j as u32)));
            }
            for (j, p) in a.eq.iter().enumerate() {
                let key = a
                    .desc
                    .as_slice()
                    .get(a.skip.saturating_add(j))
                    .copied()
                    .unwrap_or_default();
                let l = self.probe_set(p, key, a.reverse, pnext);
                lists.push(PSrc::List(l));
            }
            self.combos(t, a, e, lists.as_slice(), bounds, pnext)?;
            self.b.here(pnext);
            self.b.jump_to(Code::EphNext, pl, ploop);
            self.b.here(fin);
            return Some(());
        }
        let mut lists: AVec<PSrc> = AVec::new();
        for (j, p) in a.eq.iter().enumerate() {
            let key = a.desc.as_slice().get(j).copied().unwrap_or_default();
            let l = self.probe_set(p, key, a.reverse, fin);
            lists.push(PSrc::List(l));
        }
        if a.root == 0 {
            // The rowid B-tree: each value of the first list that is an
            // exact rowid, in the list's order.
            let l = lists.as_slice().first()?.list()?;
            let top = self.b.label();
            let next = self.b.label();
            let r = self.b.reg();
            self.b.jump_to(Code::EphRewind, l, fin);
            self.b.here(top);
            self.b.emit2(Code::ListValue, l, r);
            self.b.jump3(Code::SeekRowid, t, next, r);
            self.b.emit2(Code::EphAdd, e, t);
            self.b.here(next);
            self.b.jump_to(Code::EphNext, l, top);
            self.b.here(fin);
            return Some(());
        }
        self.combos(t, a, e, lists.as_slice(), bounds, fin)?;
        self.b.here(fin);
        Some(())
    }

    /// The index walk for each combination of `lists` (the last
    /// varying fastest), its rows into `e`; then `fin`.
    #[cfg(feature = "expr_index")]
    fn combos(
        &mut self,
        t: u32,
        a: &crate::xplan::IdxAccess,
        e: u32,
        lists: &[PSrc],
        bounds: [Option<(u32, bool)>; 2],
        fin: Label,
    ) -> Option<()> {
        let nk = a.desc.len();
        let nprefix = lists.len().min(nk);
        let rowids = match lists.get(nk) {
            Some(PSrc::List(l)) => Some(*l),
            #[cfg(feature = "planner")]
            Some(PSrc::Reg(_)) => return None,
            None => None,
        };
        let ii = self.b.p4(P4::Index(alloc::boxed::Box::new(
            super::super::index::IndexInfo {
                root: a.root,
                desc: a.desc.clone(),
            },
        )));
        let i = self.b.cursor();
        self.b.emit_p4(Code::IdxOpen, i, ii);
        // The combination's values, then the range's start.
        let pr = self.b.regs(nprefix.saturating_add(1));
        let mut nexts: AVec<(Label, u32)> = AVec::new();
        let mut exit = fin;
        for (j, src) in lists.iter().take(nprefix).enumerate() {
            let dest = pr.saturating_add(j as u32);
            match *src {
                PSrc::List(l) => {
                    let top = self.b.label();
                    let next = self.b.label();
                    self.b.jump_to(Code::EphRewind, l, exit);
                    self.b.here(top);
                    self.b.emit2(Code::ListValue, l, dest);
                    nexts.push((next, l));
                    // (`top` is where its EphNext returns.)
                    nexts.push((top, u32::MAX));
                    exit = next;
                }
                #[cfg(feature = "planner")]
                PSrc::Reg(r) => self.b.emit2(Code::Copy, r, dest),
            }
        }
        self.b.emit2(Code::EphMark, e, 0);
        let k = nprefix;
        let ko = a.desc.as_slice().get(k).copied().unwrap_or_default();
        let [lo, hi] = bounds;
        let on_key = !a.range_rowid && k < nk && (lo.is_some() || hi.is_some());
        let start = if on_key {
            if ko.desc { hi } else { lo }
        } else {
            None
        };
        let walked = self.b.label();
        let wtop = self.b.label();
        let wnext = self.b.label();
        match start {
            None if nprefix == 0 => self.b.jump_to(Code::IdxRewind, i, walked),
            _ => {
                let mut n = nprefix as u32;
                if let Some((r, _)) = start {
                    self.b.emit(Code::Copy, r, pr.saturating_add(n), 0, 0, 0);
                    n = n.saturating_add(1);
                }
                self.b.jump(Code::IdxSeek, i, walked, pr, n, 0);
            }
        }
        self.b.here(wtop);
        let flag = self.b.reg();
        let w = self.b.p4(P4::Walk(alloc::boxed::Box::new(
            super::super::index::Walk {
                prefix: pr,
                nprefix,
                desc: a.desc.clone(),
                on_key,
                k,
                ko,
                lo,
                hi,
                range_rowid: a.range_rowid,
                rowids,
                bignull: a.bignull.map(|(f, _)| f),
                flag,
            },
        )));
        self.b.jump2(Code::IdxTest, i, walked, wnext, w);
        let fl = if a.bignull.is_some() {
            flag.saturating_add(1)
        } else {
            0
        };
        match &a.cover {
            Some(cover) => {
                let sp_info = self.cover_info(t, cover)?;
                let c = self.b.p4(P4::Cover(alloc::boxed::Box::new(sp_info)));
                self.b.emit(Code::IdxCover, e, i, fl, c, 0);
            }
            None => {
                let r = self.b.reg();
                self.b.emit2(Code::IdxRowid, i, r);
                self.b.jump3(Code::SeekRowid, t, wnext, r);
                self.b.emit3(Code::EphAdd, e, t, fl);
            }
        }
        self.b.here(wnext);
        self.b.jump_to(Code::IdxNext, i, wtop);
        self.b.here(walked);
        if a.reverse {
            self.b.emit2(Code::EphReverse, e, 0);
        }
        if let Some((_, first)) = a.bignull {
            self.b.emit(Code::EphNulls, e, 0, 0, 0, u8::from(first));
        }
        // The next combination: innermost list first.
        let mut pairs: AVec<(Label, Label, u32)> = AVec::new();
        let mut it = nexts.iter();
        while let (Some(&(next, l)), Some(&(top, _))) = (it.next(), it.next()) {
            pairs.push((next, top, l));
        }
        for (next, top, l) in pairs.iter().rev() {
            self.b.here(*next);
            self.b.jump_to(Code::EphNext, *l, *top);
        }
        self.b.jump_to(Code::Goto, 0, fin);
        Some(())
    }

    /// The covering-index row layout of the source whose table cursor
    /// is `t`.
    #[cfg(feature = "expr_index")]
    fn cover_info(
        &self,
        t: u32,
        cover: &AVec<Option<usize>>,
    ) -> Option<super::super::index::CoverInfo> {
        let d = self.lv.iter().position(|l| l.t == t)?;
        let sp = self.qp.sources.as_slice().get(d)?;
        Some(super::super::index::CoverInfo {
            cover: cover.clone(),
            ncols: sp.ncols,
            rowid_alias: sp.rowid_alias,
            affs: sp.affs.clone(),
        })
    }

    /// A MULTI-INDEX OR loop's rows (`xindex::or_rows`): per branch,
    /// the rows its path finds that satisfy its condition and no
    /// earlier branch's.
    #[cfg(feature = "planner")]
    fn or_rows(&mut self, d: usize, o: &'p crate::xmulti::OrAccess, e: u32) -> Option<()> {
        let t = self.lvl(d)?.t;
        for (k, sub) in o.subs.iter().enumerate() {
            let tk = self.b.cursor();
            self.b.emit2(Code::EphOpen, tk, 0);
            match &sub.acc {
                Some(a) => self.access_rows(d, a, tk, false)?,
                None => {
                    let top = self.b.label();
                    let done = self.b.label();
                    self.b.jump_to(Code::Rewind, t, done);
                    self.b.here(top);
                    self.b.emit2(Code::EphAdd, tk, t);
                    self.b.jump_to(Code::Next, t, top);
                    self.b.here(done);
                }
            }
            let top = self.b.label();
            let next = self.b.label();
            let done = self.b.label();
            self.b.jump_to(Code::EphRewind, tk, done);
            self.b.here(top);
            self.b.emit3(Code::Load, tk, o.base as u32, o.src as u32);
            // (Holding a subquery, a branch's condition is its value's
            // truth, `eval_row_sub`, as today.)
            #[cfg(feature = "sql")]
            if super::sub::real_sub(self.b, &sub.cond) {
                let tmp = self.tmp;
                self.val_sub(&sub.cond, Cx::Acc, tmp, EVAL_COND);
                self.b.jump_to(Code::IfNot, tmp, next);
            } else {
                self.test(&sub.cond, Cx::Acc, next);
            }
            #[cfg(not(feature = "sql"))]
            self.test(&sub.cond, Cx::Acc, next);
            for prev in o.subs.as_slice().get(..k).unwrap_or(&[]).iter() {
                let tmp = self.tmp;
                #[cfg(feature = "sql")]
                if crate::query::has_sub(&prev.cond) {
                    if super::sub::real_sub(self.b, &prev.cond) {
                        self.val_sub(&prev.cond, Cx::Acc, tmp, EVAL_COND);
                    } else {
                        self.val_preset(&prev.cond, Cx::Acc, tmp, EVAL_COND);
                    }
                    self.b.jump_to(Code::If, tmp, next);
                    continue;
                }
                let kk = self.b.p4(P4::Expr(prev.cond.clone()));
                self.b.emit(Code::Eval, 0, tmp, 0, kk, EVAL_COND | EVAL_ACC);
                self.b.jump_to(Code::If, tmp, next);
            }
            self.b.emit2(Code::EphAdd, e, tk);
            self.b.here(next);
            self.b.jump_to(Code::EphNext, tk, top);
            self.b.here(done);
        }
        Some(())
    }

    /// The min() / max() early-out: the first row of `e` that passes
    /// the condition, or none.
    #[cfg(feature = "planner")]
    fn first_row(&mut self, e: u32, f: &(RExpr, usize, usize)) -> Option<()> {
        let top = self.b.label();
        let next = self.b.label();
        let none = self.b.label();
        let done = self.b.label();
        self.b.jump_to(Code::EphRewind, e, none);
        self.b.here(top);
        self.b.emit3(Code::Load, e, f.1 as u32, f.2 as u32);
        self.test(&f.0, Cx::Acc, next);
        self.b.emit2(Code::EphKeep, e, 0);
        self.b.jump_to(Code::Goto, 0, done);
        self.b.here(next);
        self.b.jump_to(Code::EphNext, e, top);
        self.b.here(none);
        self.b.emit2(Code::EphOpen, e, 0);
        self.b.here(done);
        Some(())
    }
}

/// Driver-read sources (unit `vm_rest`).
#[cfg(feature = "sql")]
impl G<'_, '_> {
    /// The `P4::Src` entry of source `d`.
    fn src_entry(&mut self, d: usize) -> u32 {
        let info = super::super::op::SrcInfo {
            plan: self.plan,
            d,
            base: self.base(d) as usize,
            src: self.src(d) as usize,
            drain: !self.streamed,
        };
        self.b.p4(P4::Src(alloc::boxed::Box::new(info)))
    }

    /// `p3` of a first source's driver-awaited read: 1 + the register
    /// holding the rows the reader needs (`session::derived_need`), 0
    /// for none.
    fn first_need(&self, d: usize) -> u32 {
        match self.dneed {
            Some(r) if d == 0 => r.saturating_add(1),
            _ => 0,
        }
    }

    /// Code that fills row list `e` with source `d`'s rows read by the
    /// driver (`mode`); a lateral source whose pre-check fails jumps
    /// to `nopad`.
    fn host_rows(&mut self, d: usize, e: u32, mode: u8, nopad: Option<Label>) {
        let src = self.src_entry(d);
        let need = self.first_need(d);
        match nopad {
            Some(l) => self.b.jump(Code::HostRows, e, l, need, src, mode),
            None => self.b.emit(Code::HostRows, e, 0, need, src, mode),
        }
    }

    /// `HostNext`'s `p3`: whether a generate_series cursor counts its
    /// rows against `Config::max_vtab_rows` — when this core keeps
    /// every row it is given (DISTINCT, groups, an ORDER BY without a
    /// LIMIT in effect: `Stream::capped`).
    fn vtab_cap(&self) -> u32 {
        let qp = self.qp;
        // (The streamed fold keeps none: `Stream::capped` is false
        // for it.)
        if qp.distinct || matches!(self.fold, Fold::Groups(_)) || self.win.is_some() {
            return 1;
        }
        let sorted =
            qp.order_by.is_empty() || matches!(qp.sources.as_slice(), [sp] if rowid_order(sp, qp));
        if sorted {
            return 0;
        }
        match self.lim {
            Some(r) => r.saturating_add(2),
            None => 1,
        }
    }

    /// Whether this core tracks the side tables' entries (the
    /// statement's own rows: `Stream`'s bookkeeping).
    #[cfg(any(feature = "json", feature = "fts5"))]
    fn track_side(&self) -> bool {
        matches!(self.sink, Sink::Result) && self.n0 == 0 && self.w0 == 0
    }
}

/// Window queries (unit `vm_rest`): today's `project_qp_async` window
/// path laid out as code.
#[cfg(feature = "window")]
impl G<'_, '_> {
    /// One window unit of set `w`: every call's children's values
    /// over `cx`'s row (in `window::calls` order), with its row or
    /// (`gp` > 0) group set `gp - 1`'s current group.
    fn win_unit(&mut self, w: u32, cx: Cx, gp: u32) {
        let calls = crate::window::calls(self.qp.items.as_slice(), self.qp.order_by.as_slice());
        let n: usize = calls.iter().map(|c| crate::window::children(c).len()).sum();
        let regs = self.b.regs(n);
        let mut i = 0u32;
        for c in calls.iter() {
            for k in crate::window::children(c) {
                self.val(k, cx, regs.saturating_add(i));
                i = i.saturating_add(1);
            }
        }
        self.b.emit3(Code::WinAdd, w, regs, gp);
    }

    /// The output of a window query, after its rows or groups: the
    /// units of a query without aggregates first (every kept row's
    /// children, in row order: the rows were all joined first, as
    /// today's executor joins them before it projects), then the
    /// windows computed, then each unit in window order through the
    /// output with the templates (`EVAL_WIN`).
    fn window_out(&mut self, out: Out) {
        let Some(w) = self.win else {
            return;
        };
        let gp = match self.fold {
            Fold::Groups(gc) => gc.saturating_add(1),
            _ => {
                let done = self.b.label();
                let top = self.b.label();
                self.b.jump(Code::WinLoad, w, done, 0, 0, 1);
                self.b.here(top);
                self.win_unit(w, Cx::Acc, 0);
                self.b.jump_to(Code::WinLoad, w, done);
                self.b.jump_to(Code::Goto, 0, top);
                self.b.here(done);
                0
            }
        };
        self.b.emit2(Code::WinCompute, w, 0);
        let done = self.b.label();
        let top = self.b.label();
        let next = self.b.label();
        self.b.jump(Code::WinNext, w, done, gp, 0, 1);
        self.b.here(top);
        let cx = match gp.checked_sub(1) {
            Some(gc) => Cx::Group(gc),
            None => Cx::Acc,
        };
        self.wflag = super::super::op::EVAL_WIN;
        self.emit(out, cx, None, next);
        self.wflag = 0;
        self.b.here(next);
        self.b.jump3(Code::WinNext, w, done, gp);
        self.b.jump_to(Code::Goto, 0, top);
        self.b.here(done);
    }
}
