//! Subqueries, compounds and CTEs, unit `vm_subqueries` (ADR 0005
//! Stage 2, `sql`). Today's executor (`eval_row_sub`, `cond_is_sub`,
//! `where_row_sub`, `eval_group_sub`, `exec_subplan_need`,
//! `run_query_compound`, `exec_recursive`) laid out as code:
//!
//! - **Subquery nodes.** Where today's executor computes an
//!   expression's subqueries (every node of a value, in `collect_subs`
//!   order; a condition's AND / OR / NOT operands one at a time, a
//!   subquery-free operand first; a WHERE clause's terms holding a
//!   correlated subquery after the others), the node's plan runs as
//!   code inline: its own sources are loops appended to the joined row
//!   after every slot of the query around it, its rows go to the
//!   node's slot (`SubRow`: a scalar's first row, EXISTS's first row,
//!   an IN set's rows), and the `Eval` that follows reads the slots
//!   (`query::substitute`). An uncorrelated node runs once per
//!   execution (`Once`, keyed by plan id and result column as today's
//!   `hoist` cache), every copy of it reading the one slot. A
//!   statement error inside a node's code becomes its value
//!   (`SubStart`'s trap: `SubValue::Fail`), raised only where an
//!   expression reads it.
//! - **Groups.** A subquery in a group's expression runs over the
//!   group's bare-column row (`AccGroup`); one inside an aggregate's
//!   arguments runs for every row of the group (`GroupRows`), the
//!   arguments folded (`AggFold`) and substituted
//!   (`query::substitute_group`).
//! - **FROM subqueries, views and CTEs** the flattener leaves are row
//!   lists their code fills the first time their loop opens
//!   (`join::G::derived_build`); a shared materialization once per
//!   execution; a first one the reader reads in order under its bound,
//!   and an endless recursive one under a growing bound.
//! - **Compounds.** UNION ALL chains without ORDER BY stream their
//!   cores' rows in order under the compound's LIMIT / OFFSET; every
//!   other compound computes each core into a row list and combines
//!   them with today's `run_compound` (`Compound`). VALUES cores are
//!   their rows.
//! - **Recursive CTEs**: the setup's rows into the queue (`QueueOpen`
//!   / `QueueAdd`: FIFO or the ORDER BY's priority, UNION dropping
//!   rows seen before), then each row taken out (`QueuePop`) is
//!   output unless the OFFSET skips it and fed to every recursive step
//!   as the one row of the recursive reference, until the LIMIT or the
//!   reader's bound.
//!
//! - **Shared code.** An uncorrelated node's code is a subroutine
//!   (`Gosub` / `Return`) every use over the same joined row calls; a
//!   correlated node's is repeated at each use, under the builder's
//!   instruction budget (`Builder::over`).
//! - **Outer aggregates.** The query's own aggregates inside its
//!   subqueries are folded over the group into slots that placeholder
//!   nodes read (`G::owned_aggs`, today's `subst_owned_aggs`); a min()
//!   / max() over a subquery picks the group's bare-column row by code
//!   (`G::rep_search`, today's `extremum_rep`).
//!
//! Falls back (the whole statement): a query's own aggregate whose
//! arguments hold a subquery or that a nested query uses as an access
//! path's bound, a program past the instruction budget, and every
//! shape the other generators do not cover.

use super::super::build::Builder;
use super::super::op::{
    Code, EVAL_ACC, EVAL_FALSE, EVAL_GROUP, EVAL_SUB, EVAL_VALUE, P4, SubExpr, SubInfo, SubKind,
};
use super::join::{Cx, G, Gx, Sink};
use crate::arena::AVec;
use crate::query::{RExpr, SubPlan};
use crate::session::QueryPlan;
use alloc::boxed::Box;
use alloc::rc::Rc;

impl G<'_, '_> {
    /// Code `e`'s value (`mode` `EVAL_VALUE`) or truth (`EVAL_COND` /
    /// `EVAL_FALSE`) into `dest` when it holds subquery nodes:
    /// `eval_row_sub` over the joined row, `eval_group_sub` over a
    /// group.
    pub(super) fn val_sub(&mut self, e: &RExpr, cx: Cx, dest: u32, mode: u8) {
        if self.val_sub_in(e, cx, dest, mode).is_none() {
            self.b.fail();
        }
    }

    fn val_sub_in(&mut self, e: &RExpr, cx: Cx, dest: u32, mode: u8) -> Option<()> {
        let at = (self.width, self.nsrc);
        let (p1, flag, slots, aggs) = match cx {
            Cx::Acc => (0, EVAL_ACC, self.subs_of(e, at, false)?, None),
            Cx::Group(gc) => {
                // The query's own aggregates inside its subqueries
                // (`subst_owned_aggs`): folded first, then the
                // expression with each one's placeholder.
                if crate::query::has_owned_sub_agg(e) {
                    return self.owned_aggs(e, gc, dest, mode);
                }
                let aggs = if crate::query::sub_in_agg_args(e) {
                    let mut list: AVec<&RExpr> = AVec::new();
                    crate::query::collect_outer_aggs(e, &mut list);
                    let mut regs = AVec::new();
                    for a in list.iter() {
                        regs.push(self.agg_rows(a, gc)?);
                    }
                    Some(regs)
                } else {
                    None
                };
                self.b.emit(Code::AccGroup, gc, 0, 0, 0, 0);
                let slots = self.subs_of(e, at, aggs.is_some())?;
                (gc.saturating_add(1), EVAL_GROUP, slots, aggs)
            }
        };
        let k = self.b.p4(P4::SubExpr(Box::new(SubExpr {
            e: e.clone(),
            slots,
            aggs,
            plain: false,
        })));
        self.b
            .emit(Code::Eval, p1, dest, 0, k, mode | flag | EVAL_SUB);
        Some(())
    }

    /// A group expression holding this query's own aggregates inside
    /// its subqueries (`eval_group_sub`'s `subst_owned_aggs`): each
    /// one folded over the group, before anything else, into the slot
    /// of a placeholder that takes its place in the subquery plans;
    /// then the expression, whose subqueries read the placeholders'
    /// slots where today's executor read the folds as literals. An
    /// aggregate whose arguments or FILTER hold a subquery is
    /// refused today (`Unsupported`), so it falls back.
    fn owned_aggs(&mut self, e: &RExpr, gc: u32, dest: u32, mode: u8) -> Option<()> {
        let mut e2 = e.clone();
        let mut found: AVec<RExpr> = AVec::new();
        let base = self.b.preset.len();
        let _ = crate::query::walk_aggs_mut(&mut e2, 0, &mut |a, d| {
            if let RExpr::Agg {
                lvl, inert: false, ..
            } = a
                && *lvl == d
                && d > 0
            {
                let id = usize::MAX.saturating_sub(base.saturating_add(found.len()));
                found.push(core::mem::replace(
                    a,
                    RExpr::SubScalar(Box::new(placeholder(id))),
                ));
                return Ok(true);
            }
            Ok(false)
        });
        let mut slots = AVec::new();
        for a in found.iter() {
            let p = crate::query::agg_parts(a)?;
            if p.args.iter().any(crate::query::has_sub)
                || p.filter.is_some_and(crate::query::has_sub)
            {
                return None;
            }
            let r = self.agg_rows(a, gc)?;
            let s = self.b.slot();
            let info = self.b.p4(P4::Sub(Box::new(SubInfo {
                kind: SubKind::Scalar,
                aff: None,
                col: 0,
            })));
            let after = self.b.label();
            self.b.jump(Code::SubStart, s, after, 0, info, 0);
            self.b.emit(Code::SubRow, s, r, 1, info, 0);
            self.b.emit(Code::SubEnd, 0, 0, 0, 0, 0);
            self.b.here(after);
            slots.push(s);
        }
        for (i, s) in slots.iter().enumerate() {
            let id = usize::MAX.saturating_sub(base.saturating_add(i));
            self.b.preset.push((id, *s));
        }
        let r = self.val_sub_in(&e2, Cx::Group(gc), dest, mode);
        self.b.preset.truncate(base);
        r
    }

    /// `e` over `cx`'s row when its only subquery nodes are folded
    /// aggregates' placeholders (`owned_aggs`): their slots
    /// substituted, it evaluates as an expression without subqueries,
    /// as today's literal did (`mode` as for one).
    pub(super) fn val_preset(&mut self, e: &RExpr, cx: Cx, dest: u32, mode: u8) {
        let at = (self.width, self.nsrc);
        let Some(slots) = self.subs_of(e, at, false) else {
            self.b.fail();
            return;
        };
        let (p1, flag) = match cx {
            Cx::Acc => (0, EVAL_ACC),
            Cx::Group(c) => (c.saturating_add(1), EVAL_GROUP),
        };
        let k = self.b.p4(P4::SubExpr(Box::new(SubExpr {
            e: e.clone(),
            slots,
            aggs: None,
            plain: true,
        })));
        self.b
            .emit(Code::Eval, p1, dest, 0, k, mode | flag | EVAL_SUB);
    }

    /// One aggregate of a group whose arguments hold a subquery
    /// (`eval_agg_async`): for every row of the group, its FILTER and
    /// then its inputs over that row, folded; the register holding the
    /// fold.
    fn agg_rows(&mut self, a: &RExpr, gc: u32) -> Option<u32> {
        let p = crate::query::agg_parts(a)?;
        let list = self.b.cursor();
        self.b.emit(Code::EphOpen, list, 0, 0, 0, 0);
        let n = p.inputs().len();
        let regs = self.b.regs(n);
        let top = self.b.label();
        let next = self.b.label();
        let done = self.b.label();
        self.b.jump(Code::GroupRows, gc, done, 0, 0, 0);
        self.b.here(top);
        if let Some(f) = p.filter {
            self.test(f, Cx::Acc, next);
        }
        for (i, x) in p.inputs().iter().enumerate() {
            self.val(x, Cx::Acc, regs.saturating_add(i as u32));
        }
        self.b.emit(Code::EphPut, list, regs, n as u32, 0, 0);
        self.b.here(next);
        self.b.jump(Code::GroupRowNext, gc, top, 0, 0, 0);
        self.b.here(done);
        let dest = self.b.reg();
        let k = self.b.p4(P4::Expr(a.clone()));
        self.b.emit(Code::AggFold, list, dest, 0, k, 0);
        Some(dest)
    }

    /// A group's bare-column row picked by a min() / max() whose
    /// argument or FILTER holds a subquery (`extremum_rep`, before
    /// anything of the group is evaluated): for every row of the
    /// group, its FILTER and then its argument over that row, each a
    /// `RepStep`. (`Groups::next` started the search at the first
    /// row.)
    pub(super) fn rep_search(&mut self, gc: u32) {
        let qp = self.qp;
        if !super::super::rows::sub_rep(qp) {
            return;
        }
        let Some(m) = crate::session::rep_minmax(
            qp.items.as_slice(),
            qp.having.as_ref(),
            qp.order_by.as_slice(),
        ) else {
            return;
        };
        let mut c = AVec::new();
        c.push(crate::query::expr_coll(m.arg).unwrap_or_default());
        let colls = self.b.p4(P4::Colls(c));
        let max = u32::from(m.kind == crate::query::AggKind::Max);
        let v = self.b.reg();
        let top = self.b.label();
        let next = self.b.label();
        let done = self.b.label();
        self.b.jump(Code::GroupRows, gc, done, 0, 0, 0);
        self.b.here(top);
        if let Some(f) = m.filter {
            self.test(f, Cx::Acc, next);
        }
        self.val(m.arg, Cx::Acc, v);
        self.b.emit(Code::RepStep, gc, v, max, colls, 0);
        self.b.here(next);
        self.b.jump(Code::GroupRowNext, gc, top, 0, 0, 0);
        self.b.here(done);
    }

    /// Condition `e` over the joined row (`cond_is_sub`: is it `want`?),
    /// jumping to `fail` unless so: AND / OR stop at a decisive
    /// operand, a subquery-free operand tested first, NOT flips the
    /// question, and a subquery is computed only when its operand is
    /// reached.
    pub(super) fn cond_sub(&mut self, e: &RExpr, want: bool, fail: super::super::build::Label) {
        use crate::sql::BinOp;
        if !real_sub(self.b, e) {
            let t = self.tmp;
            let mode = if want {
                super::super::op::EVAL_COND
            } else {
                EVAL_FALSE
            };
            if crate::query::has_sub(e) {
                self.val_preset(e, Cx::Acc, t, mode);
            } else {
                let k = self.b.p4(P4::Expr(e.clone()));
                self.b.emit(Code::Eval, 0, t, 0, k, mode | EVAL_ACC);
            }
            self.b.jump(Code::IfNot, t, fail, 0, 0, 0);
            return;
        }
        match e {
            RExpr::Binary(op @ (BinOp::And | BinOp::Or), l, r)
                if crate::query::int_lit(l).is_none() && crate::query::int_lit(r).is_none() =>
            {
                let (first, second) = if real_sub(self.b, l) && !real_sub(self.b, r) {
                    (&**r, &**l)
                } else {
                    (&**l, &**r)
                };
                let decisive = (*op == BinOp::Or) == want;
                if decisive {
                    let second_l = self.b.label();
                    let pass = self.b.label();
                    self.cond_sub(first, want, second_l);
                    self.b.jump(Code::Goto, 0, pass, 0, 0, 0);
                    self.b.here(second_l);
                    self.cond_sub(second, want, fail);
                    self.b.here(pass);
                } else {
                    self.cond_sub(first, want, fail);
                    self.cond_sub(second, want, fail);
                }
            }
            RExpr::Not(x) => self.cond_sub(x, !want, fail),
            _ => {
                let t = self.tmp;
                let mode = if want {
                    super::super::op::EVAL_COND
                } else {
                    EVAL_FALSE
                };
                self.val_sub(e, Cx::Acc, t, mode);
                self.b.jump(Code::IfNot, t, fail, 0, 0, 0);
            }
        }
    }

    /// The WHERE clause over the joined row (`where_row_sub`): its AND
    /// terms in order, those holding a correlated subquery (other than
    /// an `x IN (SELECT ...)` constraint) after the rest.
    pub(super) fn where_sub(&mut self, f: &RExpr, skip: super::super::build::Label) {
        if !real_sub(self.b, f) {
            self.test(f, Cx::Acc, skip);
            return;
        }
        let late = |t: &RExpr| match t {
            RExpr::InSub(x, _, false) => crate::query::has_corr_sub(x),
            _ => crate::query::has_corr_sub(t),
        };
        let mut terms: AVec<&RExpr> = AVec::new();
        crate::query::and_terms(f, &mut terms);
        for pass in [false, true] {
            for t in terms.iter() {
                if late(t) == pass {
                    self.cond_sub(t, true, skip);
                }
            }
        }
    }

    /// Code for each subquery node of `e` (`collect_subs` order, or
    /// `collect_subs_outside_agg`'s with `outside`), over the joined
    /// row's first `at` slots and sources: their slots.
    fn subs_of(&mut self, e: &RExpr, at: (usize, usize), outside: bool) -> Option<AVec<u32>> {
        subs_of(self.b, self.gx, e, at, outside)
    }

    /// A probe set from an IN subquery (`xindex::probe_vals`): the
    /// subquery node's code, then its values as the probe list `l`;
    /// `none` when it is empty.
    #[cfg(feature = "expr_index")]
    pub(super) fn probe_sub(
        &mut self,
        e: &RExpr,
        info: u32,
        l: u32,
        none: super::super::build::Label,
    ) {
        let at = (self.width, self.nsrc);
        let mut kinds: AVec<crate::query::SubKind<'_>> = AVec::new();
        crate::query::collect_subs(e, &mut kinds);
        let gx = self.gx;
        let ([k], Some(slot)) = (
            kinds.as_slice(),
            kinds
                .as_slice()
                .first()
                .and_then(|k| sub_node(self.b, gx, k, at)),
        ) else {
            self.b.fail();
            return;
        };
        let _ = k;
        self.b.jump(Code::ProbeSub, l, none, slot, info, 0);
    }
}

/// `G::subs_of` for any generator.
fn subs_of(
    b: &mut Builder,
    gx: Gx<'_>,
    e: &RExpr,
    at: (usize, usize),
    outside: bool,
) -> Option<AVec<u32>> {
    let mut kinds: AVec<crate::query::SubKind<'_>> = AVec::new();
    if outside {
        crate::query::collect_subs_outside_agg(e, &mut kinds);
    } else {
        crate::query::collect_subs(e, &mut kinds);
    }
    let mut slots = AVec::new();
    for k in kinds.iter() {
        slots.push(sub_node(b, gx, k, at)?);
    }
    Some(slots)
}

/// A LIMIT / OFFSET holding a subquery (`limit_window`'s
/// `eval_row_sub` over the empty row): its value into `dest`, the
/// joined row saved around it.
pub(super) fn empty_scope_val(b: &mut Builder, e: &RExpr, gx: Gx<'_>, dest: u32) -> Option<()> {
    b.emit(Code::AccPush, 0, 0, 0, 0, 0);
    let slots = subs_of(b, gx, e, (0, 0), false)?;
    let k = b.p4(P4::SubExpr(Box::new(SubExpr {
        e: e.clone(),
        slots,
        aggs: None,
        plain: false,
    })));
    b.emit(Code::Eval, 0, dest, 0, k, EVAL_VALUE | EVAL_ACC | EVAL_SUB);
    b.emit(Code::AccPop, 0, 0, 0, 0, 0);
    Some(())
}

/// One subquery node's code (`sub_value_for`), its plan over the
/// joined row's first `at` slots and sources: its slot. An
/// uncorrelated node runs once per execution, and every copy of it
/// (the same plan id and result column) reads its one slot; its code
/// is a subroutine that a later copy over the same joined row calls
/// (an alias of a result column holding one, in ORDER BY, GROUP BY
/// or HAVING, is such a copy: repeated, nested aliases would
/// otherwise make the program grow exponentially). A placeholder of
/// an enclosing query's folded aggregate (`G::owned_aggs`) is its
/// slot, no code.
fn sub_node(
    b: &mut Builder,
    gx: Gx<'_>,
    k: &crate::query::SubKind<'_>,
    at: (usize, usize),
) -> Option<u32> {
    use crate::query::SubKind as K;
    let (plan, kind) = match k {
        K::Scalar(p) => (*p, SubKind::Scalar),
        K::Exists(p) => (*p, SubKind::Exists),
        K::In(p) => (*p, SubKind::In),
    };
    if let Some(s) = preset_slot(b, plan) {
        return Some(s);
    }
    if b.over() {
        return None;
    }
    let key = (plan.uncorrelated && plan.id != 0).then_some((plan.id, plan.scalar_col));
    let found = key.and_then(|k| b.hoisted.iter().find(|h| h.key == k));
    if let Some(h) = found
        && let Some((entry, ret, hat)) = h.code
        && hat == at
    {
        // The node's code again over the same row: call it.
        let (slot, once) = (h.slot, h.once);
        let done = b.label();
        b.jump(Code::Once, once, done, 0, 0, 0);
        b.jump(Code::Gosub, 0, entry, ret, 0, 0);
        b.here(done);
        return Some(slot);
    }
    let (slot, once, shared) = match found {
        Some(h) => (h.slot, Some(h.once), None),
        None => {
            let s = b.slot();
            match key {
                Some(k) => {
                    let o = b.reg();
                    let entry = b.label();
                    let ret = b.reg();
                    b.hoisted.push(crate::vm::build::Hoisted {
                        key: k,
                        slot: s,
                        once: o,
                        code: Some((entry, ret, at)),
                    });
                    (s, Some(o), Some((entry, ret)))
                }
                None => (s, None, None),
            }
        }
    };
    let done = b.label();
    if let Some(o) = once {
        b.jump(Code::Once, o, done, 0, 0, 0);
    }
    // Shared code: called here, placed after the call and jumped over.
    let ret_to = match shared {
        Some((entry, ret)) => {
            b.jump(Code::Gosub, 0, entry, ret, 0, 0);
            b.jump(Code::Goto, 0, done, 0, 0, 0);
            b.here(entry);
            let back = b.label();
            Some((back, ret))
        }
        None => None,
    };
    let after = match ret_to {
        Some((back, _)) => back,
        None => done,
    };
    let info = b.p4(P4::Sub(Box::new(SubInfo {
        kind,
        aff: crate::query::result_affinity(plan),
        col: plan.scalar_col,
    })));
    b.jump(Code::SubStart, slot, after, 0, info, 0);
    b.emit(Code::Count, 0, 0, 0, 0, 0);
    // A scalar subquery and EXISTS read one row (the pin's implicit
    // LIMIT 1); an IN set all of them.
    let need = match kind {
        SubKind::In => None,
        _ => {
            let r = b.reg();
            b.emit(Code::Integer, 0, r, 1, 0, 0);
            Some(r)
        }
    };
    subplan(b, plan, gx, at, need, Sink::Sub(slot, info))?;
    b.emit(Code::SubEnd, 0, 0, 0, 0, 0);
    if let Some((back, ret)) = ret_to {
        b.here(back);
        b.emit(Code::Return, 0, 0, ret, 0, 0);
    }
    b.here(done);
    Some(slot)
}

/// The placeholder standing for an enclosing query's folded aggregate
/// (`G::owned_aggs`): a scalar subquery node of an empty plan, id `id`,
/// whose slot the fold fills.
fn placeholder(id: usize) -> SubPlan {
    SubPlan {
        sources: AVec::new(),
        items: AVec::new(),
        filter: None,
        group_by: AVec::new(),
        having: None,
        order_by: AVec::new(),
        distinct: false,
        limit: None,
        offset: None,
        has_agg: false,
        compound: AVec::new(),
        values: AVec::new(),
        comp_order: AVec::new(),
        result_names: AVec::new(),
        result_affs: AVec::new(),
        result_named: AVec::new(),
        id,
        uncorrelated: true,
        scalar_col: 0,
        rec: None,
        mat: 0,
        #[cfg(feature = "explain")]
        sel: 0,
        #[cfg(feature = "planner")]
        view_names: AVec::new(),
        #[cfg(feature = "planner")]
        is_cte: false,
        #[cfg(feature = "planner")]
        lazy: false,
        #[cfg(feature = "planner")]
        grp: 0,
    }
}

/// The slot of placeholder plan `p` (`G::owned_aggs`), if it is one.
fn preset_slot(b: &Builder, p: &SubPlan) -> Option<u32> {
    b.preset.iter().find(|(id, _)| *id == p.id).map(|(_, s)| *s)
}

/// Whether `e` holds a subquery node of its own that is not a folded
/// aggregate's placeholder (`G::owned_aggs`): what today's executor,
/// which substituted those as literals, would see as a subquery.
pub(super) fn real_sub(b: &Builder, e: &RExpr) -> bool {
    if !crate::query::has_sub(e) {
        return false;
    }
    if b.preset.is_empty() {
        return true;
    }
    let mut kinds: AVec<crate::query::SubKind<'_>> = AVec::new();
    crate::query::collect_subs(e, &mut kinds);
    kinds.iter().any(|k| {
        let p = match k {
            crate::query::SubKind::Scalar(p)
            | crate::query::SubKind::Exists(p)
            | crate::query::SubKind::In(p) => *p,
        };
        preset_slot(b, p).is_none()
    })
}

/// Code for one execution of subquery plan `p` over the joined row's
/// first `at` slots and sources (`exec_subplan_need`), its rows into
/// `sink`, at most `need` of them (a register; −1: all).
pub(super) fn subplan(
    b: &mut Builder,
    p: &SubPlan,
    gx: Gx<'_>,
    at: (usize, usize),
    need: Option<u32>,
    sink: Sink,
) -> Option<()> {
    if b.depth >= MAX_NEST || b.over() {
        b.fail();
        return None;
    }
    b.depth = b.depth.saturating_add(1);
    let r = match p.rec {
        Some(rec) => recursive(b, p, rec, gx, at, need, sink),
        None => {
            let qp = Rc::new(QueryPlan::of_sub(p));
            super::join::select(b, &qp, gx, at, need, sink)
        }
    };
    b.depth = b.depth.saturating_sub(1);
    r
}

/// The deepest nesting of subquery plans the generator follows (its
/// recursion's native stack); a deeper statement runs on today's
/// executor.
const MAX_NEST: u32 = 32;

/// A compound or VALUES query (`run_query_compound` /
/// `exec_subplan_inner`'s compound finish): a UNION ALL chain without
/// ORDER BY streams its cores in order under the compound's LIMIT /
/// OFFSET; any other compound computes each core into a row list,
/// combines them (`run_compound`, or the UNION-with-LIMIT-1
/// shortcut's first row) and gives the result's window.
pub(super) fn compound(
    b: &mut Builder,
    qp: &Rc<QueryPlan>,
    gx: Gx<'_>,
    at: (usize, usize),
    need: Option<u32>,
    sink: Sink,
) -> Option<()> {
    use crate::sql::CompoundOp;
    let end = b.label();
    let (lim, off) = super::join::limits(b, qp.limit.as_ref(), qp.offset.as_ref(), gx, end)?;
    let cnt = need.map(|n| {
        let c = b.reg();
        b.emit(Code::Copy, n, c, 0, 0, 0);
        c
    });
    let mut cores: AVec<Rc<QueryPlan>> = AVec::new();
    cores.push(Rc::clone(qp));
    for (_, t) in qp.compound.iter() {
        cores.push(Rc::new((**t).clone()));
    }
    let order = qp.comp_order();
    let stream = order.is_empty() && qp.compound.iter().all(|(o, _)| *o == CompoundOp::UnionAll);
    if stream {
        for c in cores.iter() {
            if c.values.is_empty() {
                super::join::core(b, c, gx, at, Some((lim, off)), None, cnt, sink, end)?;
            } else {
                super::join::values_core(b, c, gx, at, lim, off, cnt, sink, end)?;
            }
        }
        b.here(end);
        return Some(());
    }
    // Each core into its own row list (consecutive cursors), in order.
    let first = b.cursor();
    for _ in 1..cores.len() {
        b.cursor();
    }
    for (k, c) in cores.iter().enumerate() {
        let e = first.saturating_add(k as u32);
        b.emit(Code::EphOpen, e, 0, 0, 0, 0);
        let cend = b.label();
        if c.values.is_empty() {
            super::join::core(
                b,
                c,
                gx,
                at,
                Some((None, None)),
                None,
                None,
                Sink::List(e),
                cend,
            )?;
        } else {
            super::join::values_core(b, c, gx, at, None, None, None, Sink::List(e), cend)?;
        }
        b.here(cend);
    }
    let mut ops = AVec::new();
    let mut colls = AVec::new();
    for c in cores.iter() {
        colls.push(crate::session::core_colls(
            c.items.as_slice(),
            c.values.as_slice(),
        ));
    }
    for (o, _) in qp.compound.iter() {
        ops.push(*o);
    }
    let first_row = matches!(
        qp.limit,
        Some(RExpr::Literal(crate::record::OwnedValue::Integer(1)))
    ) && qp.offset.is_none()
        && order.is_empty()
        && ops.iter().all(|o| *o == CompoundOp::Union);
    let mut ord = AVec::new();
    ord.extend_from_slice(order);
    let k = b.p4(P4::Comp(Box::new(super::super::op::CompInfo {
        ops,
        cores: colls,
        order: ord,
        first_row,
    })));
    let r = b.cursor();
    b.emit(Code::EphOpen, r, 0, 0, 0, 0);
    b.emit(Code::Compound, r, first, cores.len() as u32, k, 0);
    let width = qp
        .values
        .as_slice()
        .first()
        .map_or(qp.items.len(), |v| v.len());
    window_rows(b, r, width, lim, off, cnt, sink, end);
    b.here(end);
    Some(())
}

/// Row list `r`'s rows (`width` values each) into `sink` after the
/// OFFSET and up to the LIMIT and the counter (`apply_window`).
#[allow(clippy::too_many_arguments)]
fn window_rows(
    b: &mut Builder,
    r: u32,
    width: usize,
    lim: Option<u32>,
    off: Option<u32>,
    cnt: Option<u32>,
    sink: Sink,
    end: super::super::build::Label,
) {
    let top = b.label();
    let next = b.label();
    let regs = b.regs(width);
    b.jump(Code::EphRewind, r, end, 0, 0, 0);
    b.here(top);
    if let Some(o) = off {
        b.jump(Code::IfPos, o, next, 1, 0, 0);
    }
    b.emit(Code::EphRow, r, regs, width as u32, 0, 0);
    super::join::emit_row(b, sink, regs, width, 0, lim, cnt, end);
    b.here(next);
    b.jump(Code::EphNext, r, top, 0, 0, 0);
}

/// A recursive CTE (`exec_recursive`): its LIMIT / OFFSET first (a
/// LIMIT of 0 runs nothing), the setup's rows into the queue, then
/// each row taken out of it is output unless the OFFSET skips it —
/// the LIMIT or the reader's bound ending everything — and fed to
/// every recursive step as the recursive reference's one row.
fn recursive(
    b: &mut Builder,
    p: &SubPlan,
    rec: crate::query::RecInfo,
    gx: Gx<'_>,
    at: (usize, usize),
    need: Option<u32>,
    sink: Sink,
) -> Option<()> {
    let end = b.label();
    let (lim, off) = super::join::limits(b, p.limit.as_ref(), p.offset.as_ref(), gx, end)?;
    let cnt = need.map(|n| {
        let c = b.reg();
        b.emit(Code::Copy, n, c, 0, 0, 0);
        c
    });
    let mut setup = p.clone();
    setup.rec = None;
    setup.comp_order = AVec::new();
    setup.limit = None;
    setup.offset = None;
    setup.compound = AVec::new();
    let mut steps: AVec<&SubPlan> = AVec::new();
    let mut cores = AVec::new();
    cores.push(crate::session::core_colls(
        p.items.as_slice(),
        p.values.as_slice(),
    ));
    for (k, (op, t)) in p.compound.iter().enumerate() {
        cores.push(crate::session::core_colls(
            t.items.as_slice(),
            t.values.as_slice(),
        ));
        if k.saturating_add(1) < rec.split {
            setup.compound.push((*op, t.clone()));
        } else {
            steps.push(t);
        }
    }
    let width = p.width();
    let union = rec.union.then(|| {
        let mut colls = AVec::new();
        for i in 0..width {
            colls.push(crate::session::compound_col_coll(cores.iter(), i));
        }
        colls
    });
    let mut order = AVec::new();
    order.extend_from_slice(p.comp_order.as_slice());
    let qi = b.p4(P4::Queue(Box::new(super::super::op::QueueInfo {
        order,
        union,
    })));
    let q = b.cursor();
    b.emit(Code::QueueOpen, q, 0, 0, qi, 0);
    b.emit(Code::Count, 0, 0, 0, 0, 0);
    subplan(b, &setup, gx, at, None, Sink::Queue(q))?;
    let cur = b.cursor();
    b.emit(Code::EphOpen, cur, 0, 0, 0, 0);
    b.recs.push((rec.id, cur));
    let top = b.label();
    let feed = b.label();
    let regs = b.regs(width);
    b.here(top);
    b.jump(Code::QueuePop, q, end, cur, 0, 0);
    if let Some(o) = off {
        b.jump(Code::IfPos, o, feed, 1, 0, 0);
    }
    b.emit(Code::EphRow, cur, regs, width as u32, 0, 0);
    super::join::emit_row(b, sink, regs, width, 0, lim, cnt, end);
    b.here(feed);
    let mut r = Some(());
    for step in steps.iter() {
        let sq = Rc::new(QueryPlan::of_sub(step));
        let send = b.label();
        r = super::join::core(
            b,
            &sq,
            gx,
            at,
            Some((None, None)),
            None,
            None,
            Sink::Queue(q),
            send,
        );
        if r.is_none() {
            break;
        }
        b.here(send);
    }
    b.recs.pop();
    r?;
    b.jump(Code::Goto, 0, top, 0, 0, 0);
    b.here(end);
    Some(())
}
