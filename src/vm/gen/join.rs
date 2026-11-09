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
//! Every other shape (subqueries anywhere, compounds, VALUES, derived
//! tables, views and CTEs that are not flattened, windows, virtual and
//! table-valued sources, WITHOUT ROWID, TEMP and attached tables,
//! `sqlite_schema`, an IN-subquery probe) returns `None`: the whole
//! statement runs on today's executor.

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
enum Cx {
    Acc,
    Group(u32),
}

/// Whether `Eval` can evaluate `e` over a joined row of `width`
/// slots and `nsrc` sources: no subquery, window call or deferred
/// error, and (`agg`) aggregates of this query only, whose arguments
/// read the row.
fn evaluable(e: &RExpr, width: usize, nsrc: usize, agg: bool) -> bool {
    let f = |x: &RExpr| evaluable(x, width, nsrc, agg);
    match e {
        RExpr::Literal(_) | RExpr::AffLiteral(..) => true,
        RExpr::Column(slot, ..) => *slot < width,
        RExpr::Rowid(s) => *s < nsrc,
        RExpr::SubScalar(_)
        | RExpr::SubExists(_)
        | RExpr::InSub(..)
        | RExpr::Raise(_)
        | RExpr::Win(..) => false,
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

/// Whether source `sp` is a table the VM reads: a rowid table of the
/// main database (no derived table, TEMP, attached, WITHOUT ROWID,
/// virtual or table-valued source, recursive reference, nor the
/// schema table).
fn table_source(sp: &SubSource, wr: &dyn Fn(u32) -> bool) -> bool {
    let ok = sp.derived.is_none() && sp.root > 1 && sp.affs.len() == sp.ncols && !wr(sp.root);
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

#[cfg(feature = "expr_index")]
type Bounds<'p> = [Option<&'p crate::xplan::Bound>; 2];

/// How a source's loop reads its rows.
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
    Auto(&'p crate::xplan::AutoIdx),
    #[cfg(not(feature = "expr_index"))]
    #[allow(dead_code)]
    Never(core::marker::PhantomData<&'p ()>),
}

/// One source's loop: its cursors and state registers.
struct Lv<'p> {
    kind: Kind<'p>,
    /// The table cursor.
    t: u32,
    /// The row list (`Mat`, `Lateral`, `Auto`).
    e: u32,
    /// Set once the row list is built (`Mat`, `Auto`).
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

struct G<'p> {
    b: Builder,
    qp: &'p QueryPlan,
    plan: u32,
    bases: AVec<usize>,
    lv: AVec<Lv<'p>>,
    end: Label,
    lim: Option<u32>,
    off: Option<u32>,
    tmp: u32,
    /// The output row's rowid source (`ACC_ROWID` for one source).
    rowid: u32,
    fold: Fold,
}

/// The program of a general query plan, or `None` when it holds a
/// shape this unit does not cover. `wr`: whether a root page is a
/// WITHOUT ROWID table's; `utf8`: the database's text encoding is
/// UTF-8 (the streamed aggregate fold's condition).
pub(crate) fn query(qp: &Rc<QueryPlan>, wr: &dyn Fn(u32) -> bool, utf8: bool) -> Option<Program> {
    if !qp.compound.is_empty() || !qp.values.is_empty() {
        return None;
    }
    let nsrc = qp.sources.len();
    let width: usize = qp.sources.iter().map(|s| s.ncols).sum();
    let grouped = qp.has_agg || !qp.group_by.is_empty();
    let row_ok = |e: &RExpr| evaluable(e, width, nsrc, false);
    let out_ok = |e: &RExpr| evaluable(e, width, nsrc, grouped);
    if !qp.items.iter().all(out_ok)
        || !qp.order_by.iter().all(|(e, _)| out_ok(e))
        || !qp.having.as_ref().is_none_or(out_ok)
        || !qp.group_by.iter().all(row_ok)
        || !qp.filter.as_ref().is_none_or(row_ok)
        || ![&qp.limit, &qp.offset]
            .iter()
            .all(|l| l.as_ref().is_none_or(|e| evaluable(e, 0, 0, false)))
    {
        return None;
    }
    let mut b = Builder::new();
    let plan = b.p4(P4::Plan(Rc::clone(qp)));
    let mut bases = AVec::new();
    let mut lv = AVec::new();
    let mut w = 0usize;
    for (d, sp) in qp.sources.iter().enumerate() {
        if !table_source(sp, wr) || !sp.on.as_ref().is_none_or(row_ok) {
            return None;
        }
        bases.push(w);
        w = w.saturating_add(sp.ncols);
        let kind = kind_of(sp, d, width, nsrc)?;
        let t = b.cursor();
        let needs_list = !matches!(kind, Kind::Scan) && !direct(&kind);
        let e = if needs_list { b.cursor() } else { u32::MAX };
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
    let end = b.label();
    let tmp = b.reg();
    let mut g = G {
        b,
        qp,
        plan,
        bases,
        lv,
        end,
        lim: None,
        off: None,
        tmp,
        rowid: if nsrc == 1 { ACC_ROWID } else { 0 },
        fold: Fold::Rows,
    };
    g.limits();
    // The output, the sorter, the accumulators.
    let out = g.out_mode(grouped);
    g.fold = if grouped {
        let c = g.b.cursor();
        if super::super::rows::folds(qp, utf8) {
            g.b.emit(Code::AggOpen, c, 0, 0, plan, 0);
            Fold::Agg(c)
        } else {
            g.b.emit(Code::GroupOpen, c, 0, 0, plan, 0);
            Fold::Groups(c)
        }
    } else {
        Fold::Rows
    };
    // Table cursors.
    for i in 0..g.lv.len() {
        let (t, root) = match (g.lv.as_slice().get(i), qp.sources.as_slice().get(i)) {
            (Some(l), Some(sp)) => (l.t, sp),
            _ => return None,
        };
        let info = g.b.p4(P4::Table(alloc::boxed::Box::new(table_info(root))));
        g.b.emit(Code::OpenRead, t, 0, 0, info, 0);
    }
    let after = g.b.label();
    // The WHERE clause's constant terms, once, first: false, and no
    // row joins (an aggregate still has its empty group).
    if qp.filter.is_some() || qp.sources.iter().any(|s| s.on.is_some()) {
        g.b.emit(Code::Consts, 0, tmp, 0, plan, 0);
        g.b.jump(Code::IfNot, tmp, after, 0, 0, 0);
    }
    let main_done = g.b.label();
    g.from(0, main_done, out)?;
    g.b.here(main_done);
    // Each RIGHT / FULL source's unmatched rows, NULLs on the left.
    for i in 0..nsrc {
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
    g.sorted_out(out);
    g.b.here(end);
    g.b.emit(Code::Halt, 0, 0, 0, 0, 0);
    g.b.finish()
}

/// Whether a loop reads the table directly (no row list).
fn direct(k: &Kind<'_>) -> bool {
    match k {
        Kind::Scan => true,
        #[cfg(feature = "expr_index")]
        Kind::Eq(..) | Kind::Range(..) => true,
        _ => false,
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
/// non-lateral one over none), no IN-subquery probe (unit
/// `vm_subqueries`).
fn kind_of(sp: &SubSource, d: usize, width: usize, nsrc: usize) -> Option<Kind<'_>> {
    let _ = (d, width, nsrc);
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        if sp.kind.pads_left() || !access_ok(a, width, nsrc) {
            return None;
        }
        #[cfg(feature = "planner")]
        if let Some(ai) = a.auto.as_deref() {
            return Some(Kind::Auto(ai));
        }
        let plain = a.root == 0 && a.cover.is_none() && a.bignull.is_none() && a.eq.len() <= 1;
        #[cfg(feature = "planner")]
        let plain = plain && a.or.is_none() && a.first.is_none() && a.skip == 0;
        if plain {
            let bounds = [a.lo.as_ref(), a.hi.as_ref()];
            match a.eq.as_slice().first() {
                Some(p) => {
                    if let ([e], false) = (p.vals.as_slice(), p.is_in) {
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

/// Whether every expression of access path `a` is evaluable and it
/// has no IN-subquery probe.
#[cfg(feature = "expr_index")]
fn access_ok(a: &crate::xplan::IdxAccess, width: usize, nsrc: usize) -> bool {
    let ok = |e: &RExpr| evaluable(e, width, nsrc, false);
    let probes =
        a.eq.iter()
            .all(|p| p.sub.is_none() && p.vals.iter().all(ok));
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

impl<'p> G<'p> {
    fn lvl(&self, d: usize) -> Option<&Lv<'p>> {
        self.lv.as_slice().get(d)
    }

    fn base(&self, d: usize) -> u32 {
        self.bases.as_slice().get(d).copied().unwrap_or(0) as u32
    }

    /// Code `e`'s value into `dest`, reading `cx`'s row; a literal is a
    /// prologue constant.
    fn val(&mut self, e: &RExpr, cx: Cx, dest: u32) {
        if let RExpr::Literal(v) = e {
            let k = self.b.p4(P4::Value(v.clone()));
            self.b.hoist(Code::Value, dest, k);
            return;
        }
        let k = self.b.p4(P4::Expr(e.clone()));
        let (p1, flag) = match cx {
            Cx::Acc => (0, EVAL_ACC),
            Cx::Group(c) => (c.saturating_add(1), EVAL_GROUP),
        };
        self.b.emit(Code::Eval, p1, dest, 0, k, EVAL_VALUE | flag);
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
    fn test(&mut self, e: &RExpr, cx: Cx, skip: Label) {
        let k = self.b.p4(P4::Expr(e.clone()));
        let (p1, flag) = match cx {
            Cx::Acc => (0, EVAL_ACC),
            Cx::Group(c) => (c.saturating_add(1), EVAL_GROUP),
        };
        let t = self.tmp;
        self.b.emit(Code::Eval, p1, t, 0, k, EVAL_COND | flag);
        self.b.jump(Code::IfNot, t, skip, 0, 0, 0);
    }

    /// LIMIT, then OFFSET, before the loop (`computeLimitRegisters`:
    /// a LIMIT of 0 reads nothing, not even the OFFSET).
    fn limits(&mut self) {
        let qp = self.qp;
        let end = self.end;
        if let Some(l) = &qp.limit {
            let r = self.b.regs(2);
            super::expr::value(&mut self.b, l, None, r);
            self.b.emit(Code::MustBeInt, r, 0, 0, 0, 0);
            self.b.jump(Code::IfNot, r, end, 0, 0, 0);
            if let Some(o) = &qp.offset {
                let ro = r.saturating_add(1);
                super::expr::value(&mut self.b, o, None, ro);
                self.b.emit(Code::MustBeInt, ro, 0, 0, 0, 0);
                self.off = Some(ro);
            }
            self.lim = Some(r);
        }
    }

    /// The output mode (`stream_build`'s): sorted unless the one
    /// source's order (or, for groups, the GROUP BY order) delivers
    /// the ORDER BY; DISTINCT; plain.
    fn out_mode(&mut self, grouped: bool) -> Out {
        let qp = self.qp;
        let distinct = if qp.distinct {
            let e = self.b.cursor();
            self.b.emit(Code::EphOpen, e, 0, 0, 0, 0);
            Some(e)
        } else {
            None
        };
        let sorted = if grouped {
            qp.order_by.is_empty() || group_order(qp)
        } else {
            qp.order_by.is_empty()
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
            self.b.emit(Code::SorterOpen, s, 0, 0, k, 0);
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
        let qp = self.qp;
        let n = qp.items.len();
        let end = self.end;
        let rowid = if matches!(cx, Cx::Group(_)) || vals.is_some() {
            0
        } else {
            self.rowid
        };
        let items = match vals {
            Some(r) => r,
            None => self.b.regs(n.saturating_add(qp.order_by.len())),
        };
        let compute = |g: &mut G<'p>| {
            if vals.is_none() {
                for (i, it) in qp.items.iter().enumerate() {
                    g.val(it, cx, items.saturating_add(i as u32));
                }
            }
        };
        match out {
            Out::Plain => {
                if let Some(o) = self.off {
                    self.b.jump(Code::IfPos, o, cont, 1, 0, 0);
                }
                compute(self);
                self.b.emit(Code::ResultRow, items, n as u32, rowid, 0, 0);
                if let Some(l) = self.lim {
                    self.b.jump(Code::DecrJumpZero, l, end, 0, 0, 0);
                }
            }
            Out::Distinct(e) => {
                compute(self);
                let colls = self
                    .b
                    .p4(P4::Colls(crate::query::exprs_colls(qp.items.as_slice())));
                self.b.jump(Code::Distinct, e, cont, items, colls, 0);
                if let Some(o) = self.off {
                    self.b.jump(Code::IfPos, o, cont, 1, 0, 0);
                }
                self.b.emit(Code::ResultRow, items, n as u32, rowid, 0, 0);
                if let Some(l) = self.lim {
                    self.b.jump(Code::DecrJumpZero, l, end, 0, 0, 0);
                }
            }
            Out::Sort(s, distinct) => {
                compute(self);
                for (j, (e, _)) in qp.order_by.iter().enumerate() {
                    self.val(e, cx, items.saturating_add(n.saturating_add(j) as u32));
                }
                let cols = crate::session::order_cols(qp.order_by.as_slice(), qp.items.as_slice());
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
                    let colls = self
                        .b
                        .p4(P4::Colls(crate::query::exprs_colls(qp.items.as_slice())));
                    self.b.jump(Code::Distinct, e, cont, items, colls, 0);
                }
                self.b.emit(Code::SorterInsert, s, items, rowid, 0, 0);
            }
        }
    }

    /// A complete joined row: the WHERE clause, the semi-join loops'
    /// end, then the output or the aggregates; `cont` is the next
    /// row's code.
    fn body(&mut self, out: Out, cont: Label) {
        let qp = self.qp;
        if let Some(f) = &qp.filter {
            self.test(f, Cx::Acc, cont);
        }
        #[cfg(feature = "planner")]
        for (d, sp) in qp.sources.iter().enumerate() {
            if sp.exists
                && let Some(l) = self.lvl(d)
            {
                let r = l.done;
                self.b.emit(Code::Integer, 0, r, 1, 0, 0);
            }
        }
        match self.fold {
            Fold::Rows => self.emit(out, Cx::Acc, None, cont),
            Fold::Agg(a) => self.b.emit(Code::AggStep, a, 0, 0, 0, 0),
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
                self.b.emit(Code::GroupAdd, gc, 0, k, 0, 0);
            }
        }
        self.b.jump(Code::Goto, 0, cont, 0, 0, 0);
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
        self.b.emit(Code::Integer, 0, any, 0, 0, 0);
        self.b.emit(Code::Integer, 0, pad, 0, 0, 0);
        self.b.emit(Code::Integer, 0, done, 0, 0, 0);
        // Open: the first row, or `pad_chk` when there is none.
        let adv = self.open(d, top, pad_chk)?;
        self.b.here(top);
        if let Some((code, r)) = adv.stop {
            let tmp = self.tmp;
            self.b.emit(Code::Rowid, t, tmp, 0, 0, 0);
            self.b.jump(code, tmp, pad_chk, r, 0, 0);
        }
        let src = if adv.list { e } else { t };
        self.b.emit(Code::Load, src, base, d as u32, 0, 0);
        if let Some(on) = &sp.on {
            self.test(on, Cx::Acc, next);
        }
        self.b.emit(Code::Integer, 0, any, 1, 0, 0);
        if sp.kind.pads_left() {
            self.b.emit(Code::EphMatch, e, 0, 0, 0, 0);
        }
        self.b.here(inner);
        self.from(d.saturating_add(1), next, out)?;
        self.b.here(next);
        self.b.jump(Code::If, pad, fin, 0, 0, 0);
        #[cfg(feature = "planner")]
        if sp.exists {
            self.b.jump(Code::If, done, fin, 0, 0, 0);
        }
        if let Some((code, c)) = adv.next {
            self.b.jump(code, c, top, 0, 0, 0);
        }
        self.b.here(pad_chk);
        if sp.kind.pads_right() {
            self.b.jump(Code::If, any, fin, 0, 0, 0);
            self.b.emit(Code::NullRow, ncols, base, d as u32, 0, 0);
            self.b.emit(Code::Integer, 0, pad, 1, 0, 0);
            self.b.jump(Code::Goto, 0, inner, 0, 0, 0);
        }
        self.b.here(fin);
        self.b.jump(Code::Goto, 0, exit, 0, 0, 0);
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
            self.b
                .emit(Code::NullRow, sp.ncols as u32, base, j as u32, 0, 0);
        }
        let base = self.base(i);
        self.b.emit(Code::Load, e, base, i as u32, 0, 0);
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
                self.b.emit(Code::GroupSort, gc, 0, 0, 0, 0);
                self.b.here(top);
                self.b.jump(Code::GroupNext, gc, done, 0, 0, 0);
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
                self.emit(out, cx, None, top);
                self.b.jump(Code::Goto, 0, top, 0, 0, 0);
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
        self.b.jump(Code::SorterSort, s, end, 0, 0, 0);
        self.b.here(more);
        if let Some(o) = self.off {
            self.b.jump(Code::IfPos, o, skip, 1, 0, 0);
        }
        self.b.emit(Code::SorterData, s, regs, 0, 0, 0);
        let rowid = s.saturating_add(1);
        self.b.emit(Code::ResultRow, regs, n as u32, rowid, 0, 0);
        if let Some(l) = self.lim {
            self.b.jump(Code::DecrJumpZero, l, end, 0, 0, 0);
        }
        self.b.here(skip);
        self.b.jump(Code::SorterNext, s, more, 0, 0, 0);
    }
}

/// How a loop opened by `G::open` advances.
struct Adv {
    /// The advance instruction and its cursor (`None`: one row).
    next: Option<(Code, u32)>,
    /// The end test at the top of each row: the rowid past the
    /// bound register ends the loop.
    stop: Option<(Code, u32)>,
    /// The loop reads the row list.
    list: bool,
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
    !o.desc
        && match e {
            RExpr::Rowid(0) => true,
            RExpr::Column(slot, ..) => sp.rowid_alias == Some(*slot),
            _ => false,
        }
}

/// Whether a grouped query's ORDER BY is its GROUP BY order (each
/// term the GROUP BY term in its position, with its collation and the
/// default NULLs placement; the groups sorted, not arriving in an
/// index's order): the groups are emitted as they are finalized, as
/// the pin's group sorter does.
fn group_order(qp: &QueryPlan) -> bool {
    !qp.group_by.is_empty()
        && qp.order_by.len() == qp.group_by.len()
        && !crate::session::groups_arrive_ordered(qp.sources.as_slice())
        && qp
            .order_by
            .iter()
            .zip(qp.group_by.iter())
            .all(|((o, so), g)| {
                crate::query::same_expr(o, g)
                    && so.nulls_first != so.desc
                    && so.coll == crate::query::expr_coll(g).unwrap_or_default()
            })
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

impl<'p> G<'p> {
    /// Open source `d`'s loop on its first row for the joined row so
    /// far (jumping to `none` when there is no row); how it advances.
    fn open(&mut self, d: usize, top: Label, none: Label) -> Option<Adv> {
        let _ = top;
        let (t, e) = {
            let l = self.lvl(d)?;
            (l.t, l.e)
        };
        #[cfg(feature = "planner")]
        self.bloom_check(d, none)?;
        let kind = match &self.lvl(d)?.kind {
            Kind::Scan => 0,
            #[cfg(feature = "expr_index")]
            Kind::Eq(..) => 1,
            #[cfg(feature = "expr_index")]
            Kind::Range(..) => 2,
            Kind::Mat => 3,
            #[cfg(feature = "expr_index")]
            Kind::Lateral => 4,
            #[cfg(feature = "planner")]
            Kind::Auto(_) => 5,
            #[cfg(not(feature = "expr_index"))]
            Kind::Never(_) => return None,
        };
        let list = Adv {
            next: Some((Code::EphNext, e)),
            stop: None,
            list: true,
        };
        Some(match kind {
            0 => {
                self.b.jump(Code::Rewind, t, none, 0, 0, 0);
                Adv {
                    next: Some((Code::Next, t)),
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
                    self.b.jump(Code::IsNull, r.0, none, 0, 0, 0);
                }
                let r = self.b.reg();
                self.val_mut(pe, Cx::Acc, r);
                let k = self.b.p4(P4::Aff(aff));
                self.b.emit(Code::Affinity, r, 0, 0, k, 0);
                self.b.jump(Code::SeekRowid, t, none, r, 0, 0);
                Adv {
                    next: None,
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
                        self.b.jump(Code::IsNull, r, whole, 0, 0, 0);
                        self.b.jump(seek, t, none, r, 0, 0);
                        self.b.jump(Code::Goto, 0, top, 0, 0, 0);
                        self.b.here(whole);
                        self.b.jump(first, t, none, 0, 0, 0);
                    }
                    None => self.b.jump(first, t, none, 0, 0, 0),
                }
                Adv {
                    next: Some((if reverse { Code::Prev } else { Code::Next }, t)),
                    stop: to.map(|r| (if reverse { Code::Lt } else { Code::Gt }, r)),
                    list: false,
                }
            }
            3 => {
                self.build_once(d)?;
                self.b.jump(Code::EphRewind, e, none, 0, 0, 0);
                list
            }
            #[cfg(feature = "expr_index")]
            4 => {
                self.b.emit(Code::EphOpen, e, 0, 0, 0, 0);
                self.build(d, e)?;
                self.b.jump(Code::EphRewind, e, none, 0, 0, 0);
                list
            }
            #[cfg(feature = "planner")]
            5 => {
                let Kind::Auto(ai) = self.lvl(d)?.kind else {
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
                    self.b.emit(Code::Affinity, r, 0, 0, a, 0);
                    if !k.is {
                        self.b.jump(Code::IsNull, r, none, 0, 0, 0);
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
        self.b.jump(Code::Once, built, skip, 0, 0, 0);
        self.b.emit(Code::EphOpen, e, 0, 0, 0, 0);
        self.build(d, e)?;
        self.b.here(skip);
        Some(())
    }

    /// Code that fills row list `e` with source `d`'s rows: its access
    /// path's, its automatic index's (sorted), or its table's.
    fn build(&mut self, d: usize, e: u32) -> Option<()> {
        let qp = self.qp;
        let sp = qp.sources.as_slice().get(d)?;
        #[cfg(feature = "planner")]
        if let Kind::Auto(ai) = self.lvl(d)?.kind {
            self.scan_into(d, e);
            let a = self.b.p4(P4::Auto(alloc::boxed::Box::new(ai.clone())));
            self.b.emit(Code::AutoSort, e, 0, 0, a, 0);
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
        self.b.jump(Code::Rewind, t, done, 0, 0, 0);
        self.b.here(top);
        self.b.emit(Code::EphAdd, e, t, 0, 0, 0);
        self.b.jump(Code::Next, t, top, 0, 0, 0);
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
        self.b.jump(Code::Once, built, skip, 0, 0, 0);
        self.b.emit(Code::EphOpen, bl, 0, 0, 0, 0);
        self.b.emit(Code::BloomAdd, bl, 0, 0, kp, 1);
        let top = self.b.label();
        let done = self.b.label();
        self.b.jump(Code::Rewind, t, done, 0, 0, 0);
        self.b.here(top);
        self.b.emit(Code::BloomAdd, bl, t, 0, kp, 0);
        self.b.jump(Code::Next, t, top, 0, 0, 0);
        self.b.here(done);
        self.b.here(skip);
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
            self.b.emit(Code::Affinity, r, 0, 0, k, 0);
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
                self.b.emit(Code::Affinity, r, 0, 0, k, 0);
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
            self.b.jump(Code::IsNull, r.0, fin, 0, 0, 0);
        }
        let mut ends = [None, None];
        for (i, (slot, r)) in ends.iter_mut().zip(regs).enumerate() {
            if let Some((r, incl)) = r {
                let flags = u8::from(i == 0) | (u8::from(incl) << 1);
                self.b.jump(Code::RowidBound, r, fin, 0, 0, flags);
                *slot = Some(r);
            }
        }
        self.b.emit(Code::EphMark, e, 0, 0, 0, 0);
        let top = self.b.label();
        let done = self.b.label();
        match ends[0] {
            Some(r) => {
                let whole = self.b.label();
                self.b.jump(Code::IsNull, r, whole, 0, 0, 0);
                self.b.jump(Code::SeekGE, t, done, r, 0, 0);
                self.b.jump(Code::Goto, 0, top, 0, 0, 0);
                self.b.here(whole);
                self.b.jump(Code::Rewind, t, done, 0, 0, 0);
            }
            None => self.b.jump(Code::Rewind, t, done, 0, 0, 0),
        }
        self.b.here(top);
        if let Some(h) = ends[1] {
            let tmp = self.tmp;
            self.b.emit(Code::Rowid, t, tmp, 0, 0, 0);
            self.b.jump(Code::Gt, tmp, done, h, 0, 0);
        }
        self.b.emit(Code::EphAdd, e, t, 0, 0, 0);
        self.b.jump(Code::Next, t, top, 0, 0, 0);
        self.b.here(done);
        if a.reverse {
            self.b.emit(Code::EphReverse, e, 0, 0, 0, 0);
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
        let regs = self.b.regs(p.vals.len());
        for (i, v) in p.vals.iter().enumerate() {
            self.val(v, Cx::Acc, regs.saturating_add(i as u32));
        }
        let l = self.b.cursor();
        self.b.emit(Code::EphOpen, l, 0, 0, 0, 0);
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
            self.b.jump(Code::IsNull, r.0, fin, 0, 0, 0);
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
            self.b.emit(Code::IdxOpen, i, 0, 0, ii, 0);
            self.b.emit(Code::EphOpen, pl, 0, 0, 0, 0);
            let top = self.b.label();
            let done = self.b.label();
            self.b.jump(Code::IdxRewind, i, done, 0, 0, 0);
            self.b.here(top);
            self.b.emit(Code::SkipAdd, pl, i, n, ii, 0);
            self.b.jump(Code::IdxNext, i, top, 0, 0, 0);
            self.b.here(done);
            if a.reverse {
                self.b.emit(Code::EphReverse, pl, 0, 0, 0, 0);
            }
            let ploop = self.b.label();
            let pnext = self.b.label();
            self.b.jump(Code::EphRewind, pl, fin, 0, 0, 0);
            self.b.here(ploop);
            let pv = self.b.regs(a.skip);
            self.b.emit(Code::ListValues, pl, pv, n, 0, 0);
            let bounds = self.bound_vals([a.lo.as_ref(), a.hi.as_ref()]);
            for r in bounds.iter().flatten() {
                self.b.jump(Code::IsNull, r.0, pnext, 0, 0, 0);
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
            self.b.jump(Code::EphNext, pl, ploop, 0, 0, 0);
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
            self.b.jump(Code::EphRewind, l, fin, 0, 0, 0);
            self.b.here(top);
            self.b.emit(Code::ListValue, l, r, 0, 0, 0);
            self.b.jump(Code::SeekRowid, t, next, r, 0, 0);
            self.b.emit(Code::EphAdd, e, t, 0, 0, 0);
            self.b.here(next);
            self.b.jump(Code::EphNext, l, top, 0, 0, 0);
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
        self.b.emit(Code::IdxOpen, i, 0, 0, ii, 0);
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
                    self.b.jump(Code::EphRewind, l, exit, 0, 0, 0);
                    self.b.here(top);
                    self.b.emit(Code::ListValue, l, dest, 0, 0, 0);
                    nexts.push((next, l));
                    // (`top` is where its EphNext returns.)
                    nexts.push((top, u32::MAX));
                    exit = next;
                }
                #[cfg(feature = "planner")]
                PSrc::Reg(r) => self.b.emit(Code::Copy, r, dest, 0, 0, 0),
            }
        }
        self.b.emit(Code::EphMark, e, 0, 0, 0, 0);
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
            None if nprefix == 0 => self.b.jump(Code::IdxRewind, i, walked, 0, 0, 0),
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
                self.b.emit(Code::IdxRowid, i, r, 0, 0, 0);
                self.b.jump(Code::SeekRowid, t, wnext, r, 0, 0);
                self.b.emit(Code::EphAdd, e, t, fl, 0, 0);
            }
        }
        self.b.here(wnext);
        self.b.jump(Code::IdxNext, i, wtop, 0, 0, 0);
        self.b.here(walked);
        if a.reverse {
            self.b.emit(Code::EphReverse, e, 0, 0, 0, 0);
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
            self.b.jump(Code::EphNext, *l, *top, 0, 0, 0);
        }
        self.b.jump(Code::Goto, 0, fin, 0, 0, 0);
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
            self.b.emit(Code::EphOpen, tk, 0, 0, 0, 0);
            match &sub.acc {
                Some(a) => self.access_rows(d, a, tk, false)?,
                None => {
                    let top = self.b.label();
                    let done = self.b.label();
                    self.b.jump(Code::Rewind, t, done, 0, 0, 0);
                    self.b.here(top);
                    self.b.emit(Code::EphAdd, tk, t, 0, 0, 0);
                    self.b.jump(Code::Next, t, top, 0, 0, 0);
                    self.b.here(done);
                }
            }
            let top = self.b.label();
            let next = self.b.label();
            let done = self.b.label();
            self.b.jump(Code::EphRewind, tk, done, 0, 0, 0);
            self.b.here(top);
            self.b
                .emit(Code::Load, tk, o.base as u32, o.src as u32, 0, 0);
            self.test(&sub.cond, Cx::Acc, next);
            for prev in o.subs.as_slice().get(..k).unwrap_or(&[]).iter() {
                let kk = self.b.p4(P4::Expr(prev.cond.clone()));
                let tmp = self.tmp;
                self.b.emit(Code::Eval, 0, tmp, 0, kk, EVAL_COND | EVAL_ACC);
                self.b.jump(Code::If, tmp, next, 0, 0, 0);
            }
            self.b.emit(Code::EphAdd, e, tk, 0, 0, 0);
            self.b.here(next);
            self.b.jump(Code::EphNext, tk, top, 0, 0, 0);
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
        self.b.jump(Code::EphRewind, e, none, 0, 0, 0);
        self.b.here(top);
        self.b.emit(Code::Load, e, f.1 as u32, f.2 as u32, 0, 0);
        self.test(&f.0, Cx::Acc, next);
        self.b.emit(Code::EphKeep, e, 0, 0, 0, 0);
        self.b.jump(Code::Goto, 0, done, 0, 0, 0);
        self.b.here(next);
        self.b.jump(Code::EphNext, e, top, 0, 0, 0);
        self.b.here(none);
        self.b.emit(Code::EphOpen, e, 0, 0, 0, 0);
        self.b.here(done);
        Some(())
    }
}
