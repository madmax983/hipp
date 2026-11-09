//! The pin's query flattener and its neighbours (v0.17, `planner`).
//!
//! The view / CTE expansion (`session::cte`) marks each FROM
//! subquery the pin's `sqlite3FlattenSubquery` merges into its reader
//! (`FromSource::flat`: its numbered restrictions, the ones hipp's
//! flattener follows); the planner plans such a body without access
//! paths or join order (`Connection::flat_raw`) and, once the
//! reader's own clauses are resolved, `flatten_core` splices it in:
//! the body's sources take the subquery's place in the FROM clause
//! (the first one its join and ON clause), its WHERE clause joins the
//! reader's (or, on a LEFT JOIN's right side, the ON clause), every
//! reference to one of its columns becomes the column's expression
//! (under a LEFT JOIN guarded by the right side's NULL row, the pin's
//! `TK_IF_NULL_ROW`), its LIMIT and an ORDER BY the reader keeps move
//! out, and every slot past it is renumbered — so the merged tables
//! join the join planner's order as ordinary sources and EXPLAIN
//! QUERY PLAN shows their loops. A subquery the splice cannot take
//! (`splice` says why) is planned again as usual.
//!
//! Then, for the subqueries that stay co-routines or
//! materialisations, the result columns the reader never reads are
//! not computed (`prune`; the pin's column-usage omission, so an
//! error in such a column is never raised), and an `EXISTS
//! (SELECT ... FROM t WHERE ...)` term of the WHERE clause becomes a
//! semi-join loop over `t` (`exists_join`; the pin's `existsToJoin`,
//! `SEARCH t EXISTS`).

use super::*;
use crate::query::{RExpr, SourceDesc, SubPlan, remap_expr};

/// What `flatten_core` rewrites of one SELECT core.
pub(super) struct Parts<'a> {
    pub sources: &'a mut AVec<SourcePlan>,
    pub cur: &'a mut crate::query::ScopeGroup,
    pub items: &'a mut AVec<RExpr>,
    pub filter: &'a mut Option<RExpr>,
    pub group_by: &'a mut AVec<RExpr>,
    pub having: &'a mut Option<RExpr>,
    pub order_by: &'a mut AVec<(RExpr, crate::sql::SortOrder)>,
    pub limit: &'a mut Option<RExpr>,
    pub result_names: &'a [AVec<u8>],
    pub result_named: &'a [bool],
    pub has_agg: bool,
    /// The core is a body planned raw for its reader's splice
    /// (`Connection::flat_raw`): its sources may join the reader's,
    /// so their shared materializations are settled there.
    pub raw: bool,
}

/// Where a merged subquery's column `c` waits for its expression
/// while the reader is renumbered (`SENT + c`; no real slot is that
/// large).
const SENT: usize = usize::MAX / 4;

/// Whether `e` calls a window function (`window`).
fn has_win(e: &RExpr) -> bool {
    #[cfg(feature = "window")]
    return crate::window::has_win(e);
    #[cfg(not(feature = "window"))]
    {
        let _ = e;
        false
    }
}

/// A virtual table or table-valued function, whose `derived` plan is
/// only its arguments' placeholder.
fn tvf_source(sp: &SourcePlan) -> bool {
    #[cfg(feature = "vtab")]
    if sp.vt.is_some() {
        return true;
    }
    #[cfg(any(feature = "json", feature = "vtab"))]
    if sp.tvf != 0 {
        return true;
    }
    let _ = sp;
    false
}

/// A FROM subquery the splice may merge: a plain core (no compound,
/// VALUES, aggregate, DISTINCT, OFFSET, window function or recursion,
/// no shared materialization) over tables and FROM subqueries.
fn mergeable(dp: &SubPlan) -> bool {
    dp.compound.is_empty()
        && dp.values.is_empty()
        && !dp.has_agg
        && dp.group_by.is_empty()
        && dp.having.is_none()
        && !dp.distinct
        && dp.offset.is_none()
        && dp.rec.is_none()
        && dp.mat == 0
        && !dp.sources.is_empty()
        && !dp
            .items
            .iter()
            .any(|e| has_win(e) || crate::query::has_agg(e))
        && dp
            .sources
            .iter()
            .all(|s| !tvf_source(s) && s.rec == 0 && !s.kind.pads_left() && s.access.is_none())
}

/// The NULL-row guard of a LEFT JOIN's merged column (the pin's
/// `TK_IF_NULL_ROW`): NULL while source `src` is the NULL row, else
/// `x`, evaluated only then.
fn if_null_row(src: usize, x: RExpr) -> RExpr {
    RExpr::Case {
        base: None,
        whens: {
            let mut w = AVec::new();
            w.push((
                RExpr::IsNull(Box::new(RExpr::Rowid(src)), false),
                RExpr::Literal(OwnedValue::Null),
            ));
            w
        },
        otherwise: Some(Box::new(x)),
    }
}

/// Replace every waiting column (`SENT + c`) of `e` by `xs[c]`.
/// False when an expression holding a subquery would land inside a
/// nested subquery (its own slots would collide there; the splice is
/// then given up).
fn fill_expr(e: &mut RExpr, xs: &[RExpr], nested: bool) -> bool {
    let f = |x: &mut RExpr| fill_expr(x, xs, nested);
    match e {
        RExpr::Column(slot, ..) => {
            if let Some(c) = slot.checked_sub(SENT) {
                let Some(x) = xs.get(c) else {
                    return false;
                };
                if nested && crate::query::has_sub(x) {
                    return false;
                }
                *e = x.clone();
            }
            true
        }
        RExpr::SubScalar(p) | RExpr::SubExists(p) => fill_plan(p, xs),
        RExpr::InSub(x, p, _) => f(x) && fill_plan(p, xs),
        RExpr::Literal(_) | RExpr::AffLiteral(..) | RExpr::Raise(_) | RExpr::Rowid(_) => true,
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) && f(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => f(a),
        RExpr::InList(x, l, ..) => f(x) && l.as_mut_slice().iter_mut().all(f),
        RExpr::Between(x, a, b, _) => f(x) && f(a) && f(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.iter_mut().chain(otherwise.iter_mut()).all(|b| f(b))
                && whens.as_mut_slice().iter_mut().all(|(w, t)| f(w) && f(t))
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
            args.as_mut_slice().iter_mut().all(f)
        }
    }
}

/// `fill_expr` over an access path's probes and bounds.
fn fill_access(a: &mut crate::xplan::IdxAccess, xs: &[RExpr]) -> bool {
    let f = |x: &mut RExpr| fill_expr(x, xs, true);
    for p in a.eq.as_mut_slice() {
        if !p.vals.as_mut_slice().iter_mut().all(f) || !p.sub.iter_mut().all(f) {
            return false;
        }
    }
    for b in a.lo.iter_mut().chain(a.hi.iter_mut()) {
        if !f(&mut b.e) {
            return false;
        }
    }
    if let Some(x) = &mut a.auto
        && !x.keys.as_mut_slice().iter_mut().all(|k| f(&mut k.val))
    {
        return false;
    }
    if let Some(o) = &mut a.or {
        for sub in o.subs.as_mut_slice() {
            if !f(&mut sub.cond) || !sub.acc.iter_mut().all(|x| fill_access(x, xs)) {
                return false;
            }
        }
    }
    if let Some(x) = &mut a.first
        && !f(&mut x.0)
    {
        return false;
    }
    true
}

/// `fill_expr` over every part of a nested plan that reads the
/// combined row (as `query::remap_plan` walks it).
fn fill_plan(p: &mut SubPlan, xs: &[RExpr]) -> bool {
    let f = |x: &mut RExpr| fill_expr(x, xs, true);
    for s in p.sources.as_mut_slice() {
        if !s.on.iter_mut().all(f) || !s.derived.iter_mut().all(|d| fill_plan(d, xs)) {
            return false;
        }
        if let Some(a) = &mut s.access
            && !fill_access(a, xs)
        {
            return false;
        }
        #[cfg(feature = "fts5")]
        if let Some(vt) = &mut s.vt {
            for (_, _, args, _) in vt.ors.as_mut_slice() {
                if !args.as_mut_slice().iter_mut().all(f) {
                    return false;
                }
            }
        }
    }
    p.items.as_mut_slice().iter_mut().all(f)
        && p.filter.iter_mut().all(f)
        && p.group_by.as_mut_slice().iter_mut().all(f)
        && p.having.iter_mut().all(f)
        && p.order_by.as_mut_slice().iter_mut().all(|(e, _)| f(e))
        && p.values
            .as_mut_slice()
            .iter_mut()
            .all(|r| r.as_mut_slice().iter_mut().all(f))
        && p.compound
            .as_mut_slice()
            .iter_mut()
            .all(|(_, t)| fill_plan(t, xs))
}

/// The AND terms of `e`, in order.
pub(super) fn and_terms(e: RExpr, out: &mut AVec<RExpr>) {
    match e {
        RExpr::Binary(crate::sql::BinOp::And, a, b) => {
            and_terms(*a, out);
            and_terms(*b, out);
        }
        e => out.push(e),
    }
}

/// The terms joined by AND again, left-deep.
fn and_all(terms: AVec<RExpr>) -> Option<RExpr> {
    let mut out: Option<RExpr> = None;
    for t in terms {
        out = Some(and_expr(out, t));
    }
    out
}

/// `a AND b`, either side possibly absent.
fn and_opt(a: Option<RExpr>, b: Option<RExpr>) -> Option<RExpr> {
    match (a, b) {
        (Some(a), Some(b)) => Some(and_expr(Some(a), b)),
        (a, b) => a.or(b),
    }
}

/// Merge FROM subquery `i` (marked `flat`, planned raw) into the
/// core; the number of sources it brought, or `None` when the splice
/// cannot take it (nothing is changed then).
fn splice(p: &mut Parts<'_>, i: usize, base: (usize, usize)) -> Option<usize> {
    let sp = p.sources.as_slice().get(i)?;
    let dp = sp.derived.as_deref()?;
    if tvf_source(sp) || !mergeable(dp) {
        return None;
    }
    // hipp's own limits: no virtual table or table-valued function
    // beside it (their constraints were planned for the old layout),
    // and no subquery LIMIT under a reader with one.
    if p.sources.iter().any(tvf_source) || (dp.limit.is_some() && p.limit.is_some()) {
        return None;
    }
    let kind = sp.kind;
    let left = kind == crate::sql::JoinKind::Left;
    if left && dp.sources.len() != 1 {
        return None;
    }
    let n = p.sources.len();
    let o_k = p.cur.offsets.as_slice().get(i).copied()?;
    let nk = sp.ncols;
    let w1: usize = dp.sources.iter().map(|s| s.ncols).sum();
    let n1 = dp.sources.len();
    let wo: usize = p.sources.iter().map(|s| s.ncols).sum();
    let w_new = wo.checked_sub(nk)?.checked_add(w1)?;
    let n_new = n.checked_sub(1)?.checked_add(n1)?;
    let (b0, b1) = base;
    // Band ends (slot numbers are small; saturating keeps the
    // arithmetic total).
    let (s_sub, s_new) = (b1.saturating_add(n1), b1.saturating_add(n_new));
    let (w_sub, w_end) = (b0.saturating_add(w1), b0.saturating_add(w_new));
    let (s_here, w_here) = (b1.saturating_add(i), o_k.saturating_add(nk));
    // The body's slots: its own sources move to the subquery's place,
    // its nested subqueries' own slots past the merged core.
    let ms = |v: usize, src: bool| -> usize {
        if src {
            if v < b1 {
                v
            } else if v < s_sub {
                v.saturating_add(i)
            } else {
                (v - s_sub).saturating_add(s_new)
            }
        } else if v < b0 {
            v
        } else if v < w_sub {
            (v - b0).saturating_add(o_k)
        } else {
            (v - w_sub).saturating_add(w_end)
        }
    };
    // The reader's: the subquery's columns wait at `SENT + c`, the
    // slots past it move by the body's extra width.
    let mo = |v: usize, src: bool| -> usize {
        if src {
            if v <= s_here {
                v
            } else {
                (v - 1).saturating_add(n1)
            }
        } else if v < o_k {
            v
        } else if v < w_here {
            SENT.saturating_add(v - o_k)
        } else {
            (v - nk).saturating_add(w1)
        }
    };
    let mut dp = dp.clone();
    // The column expressions, in the merged layout.
    let mut xs: AVec<RExpr> = AVec::new();
    for e in dp.items.as_mut_slice() {
        remap_expr(e, &ms);
        let mut x = e.clone();
        let is_col = match &x {
            RExpr::Column(s, ..) => (o_k..o_k.saturating_add(w1)).contains(s),
            RExpr::Rowid(s) => *s == s_here,
            _ => false,
        };
        if left && !is_col {
            x = if_null_row(s_here, x);
        }
        xs.push(x);
    }
    let mut sub_filter = dp.filter.take();
    if let Some(f) = sub_filter.as_mut() {
        remap_expr(f, &ms);
    }
    let mut sub_order = core::mem::take(&mut dp.order_by);
    for (e, _) in sub_order.as_mut_slice() {
        remap_expr(e, &ms);
    }
    let mut body = core::mem::take(&mut dp.sources);
    for s in body.as_mut_slice() {
        if let Some(on) = s.on.as_mut() {
            remap_expr(on, &ms);
        }
    }
    // The reader, renumbered and filled in, on copies.
    let mut items = p.items.clone();
    let mut filter = p.filter.clone();
    let mut group_by = p.group_by.clone();
    let mut having = p.having.clone();
    let mut order_by = p.order_by.clone();
    let mut ons: AVec<Option<RExpr>> = AVec::new();
    for s in p.sources.iter() {
        ons.push(s.on.clone());
    }
    let mut ok = true;
    {
        let mut one = |e: &mut RExpr| {
            remap_expr(e, &mo);
            ok &= fill_expr(e, xs.as_slice(), false);
        };
        for e in items.as_mut_slice() {
            one(e);
        }
        for e in filter.iter_mut() {
            one(e);
        }
        for e in group_by.as_mut_slice() {
            one(e);
        }
        for e in having.iter_mut() {
            one(e);
        }
        for (e, _) in order_by.as_mut_slice() {
            one(e);
        }
        for e in ons.as_mut_slice().iter_mut().flatten() {
            one(e);
        }
    }
    if !ok {
        return None;
    }
    // The subquery's ORDER BY survives only under a lone reader
    // without its own (the pin drops one that orders nothing; the
    // marking refused the rest).
    if !sub_order.is_empty() && order_by.is_empty() && n == 1 && !p.has_agg {
        order_by = sub_order;
    }
    // Commit.
    let mut own_on = None;
    for (j, (s, on)) in p.sources.as_mut_slice().iter_mut().zip(ons).enumerate() {
        if j == i {
            own_on = on;
        } else {
            s.on = on;
        }
    }
    if let Some(first) = body.as_mut_slice().first_mut() {
        first.kind = kind;
        first.on = if left {
            and_opt(sub_filter.take(), own_on)
        } else {
            own_on
        };
    }
    *p.items = items;
    *p.filter = and_opt(sub_filter, filter);
    *p.group_by = group_by;
    *p.having = having;
    *p.order_by = order_by;
    if dp.limit.is_some() {
        *p.limit = dp.limit.take();
    }
    let mut descs: AVec<SourceDesc> = AVec::new();
    for s in body.iter() {
        let mut cols = AVec::new();
        for a in s.affs.iter() {
            cols.push((AVec::new(), *a));
        }
        descs.push(SourceDesc::new(AVec::new(), cols, s.rowid_alias, true));
    }
    p.sources.remove_at(i);
    for (j, s) in body.into_iter().enumerate() {
        p.sources.insert_at(i + j, s);
    }
    p.cur.sources.remove_at(i);
    for (j, d) in descs.into_iter().enumerate() {
        p.cur.sources.insert_at(i + j, d);
    }
    let mut offs = AVec::new();
    let mut at = b0;
    for s in p.sources.as_mut_slice() {
        offs.push(at);
        #[cfg(feature = "explain")]
        {
            s.eqp.base = at;
        }
        at += s.ncols;
    }
    p.cur.offsets = offs;
    Some(n1)
}

impl<S: Storage> Connection<S> {
    /// The flattener and its neighbours over one SELECT core (see the
    /// module docs); the names a view gives the core's columns when a
    /// subquery was merged (`SubPlan::view_names`), else empty.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn flatten_core(
        &self,
        arena: &Arena,
        stmt: &SelectStmt,
        enclosing: &[crate::query::ScopeGroup],
        base: (usize, usize),
        mut p: Parts<'_>,
    ) -> Result<AVec<AVec<u8>>, S::Error> {
        let mut names = AVec::new();
        // The pin's LEFT JOIN simplification comes first (v0.17
        // review fix): before a body is merged, before a term is
        // pushed down, before an EXISTS body's WHERE joins the core's.
        let first_src = p.cur.first_src;
        super::join::simplify_left(
            p.sources.as_mut_slice(),
            p.cur.offsets.as_slice(),
            first_src,
            p.filter.as_ref(),
        );
        self.push_down(arena, stmt, enclosing, &mut p)?;
        if stmt.from.iter().any(|f| f.flat) {
            names = self.col_names_of(
                p.items.as_slice(),
                p.sources.as_slice(),
                p.result_names,
                p.result_named,
                p.items.len(),
                base,
            )?;
            // Each source's FROM clause entry (None: brought in by a
            // merge).
            let mut ast: AVec<Option<usize>> = AVec::new();
            let same = p.sources.len() == stmt.from.len();
            for k in 0..p.sources.len() {
                ast.push(same.then_some(k));
            }
            let mut merged = false;
            let mut i = 0;
            while i < p.sources.len() {
                let f = ast
                    .as_slice()
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|k| stmt.from.as_slice().get(k));
                let Some(f) = f.filter(|f| f.flat) else {
                    i += 1;
                    continue;
                };
                match splice(&mut p, i, base) {
                    Some(n1) => {
                        merged = true;
                        if let Some(x) = ast.as_mut_slice().get_mut(i) {
                            *x = None;
                        }
                        for _ in 1..n1 {
                            ast.insert_at(i + 1, None);
                        }
                        i += n1;
                    }
                    None => {
                        self.replan_flat(arena, f, enclosing, &mut p, i)?;
                        i += 1;
                    }
                }
            }
            // A body planned raw whose source the count lost track of:
            // planned again as usual.
            if !same {
                for (k, f) in stmt.from.iter().enumerate() {
                    if f.flat {
                        self.replan_flat(arena, f, enclosing, &mut p, k)?;
                    }
                }
            }
            if !merged {
                names.clear();
            }
            // The pin's `sqlite3WhereBegin` counts the sources after
            // the flattener merged them (v0.17 review fix: past 64 the
            // join's source masks overflowed).
            if p.sources.len() > 64 {
                return Err(Error::Schema {
                    kind: SchemaKind::TooManyTables,
                });
            }
        }
        prune(&mut p);
        exists_join(&mut p, base, stmt);
        Ok(names)
    }

    /// The pin's `pushDownWhereTerms`: a WHERE term (or a term of the
    /// subquery's own inner / LEFT JOIN ON clause) that reads only the
    /// columns of a FROM subquery that is not flattened, by
    /// deterministic expressions with no subquery, is copied into the
    /// subquery (each column replaced by its expression, into every
    /// arm of a UNION ALL), into its WHERE — its HAVING when it
    /// aggregates and the term reads more than its GROUP BY terms
    /// (`havingToWhere` moves the rest) — and the subquery is planned
    /// again, so its loops can use the term. Not into a LIMITed,
    /// recursive, windowed, MATERIALIZED or shared subquery, a compound
    /// other than UNION ALL, a VALUES body, or past a RIGHT / FULL
    /// join; the term stays in the reader too.
    fn push_down(
        &self,
        arena: &Arena,
        stmt: &SelectStmt,
        enclosing: &[crate::query::ScopeGroup],
        p: &mut Parts<'_>,
    ) -> Result<(), S::Error> {
        use crate::sql::JoinKind;
        if p.sources.len() != stmt.from.len()
            || stmt
                .from
                .iter()
                .any(|f| matches!(f.kind, JoinKind::Right | JoinKind::Full))
        {
            return Ok(());
        }
        let mut planner = self.sub_planner(arena);
        for (k, f) in stmt.from.iter().enumerate() {
            let Some(d) = f.derived.as_deref() else {
                continue;
            };
            if f.flat
                || f.cte
                    .as_ref()
                    .is_some_and(|c| c.mat != 0 || !matches!(c.rec, crate::sql::RecMark::None))
                || d.limit.is_some()
                || !d.values.is_empty()
                || !matches!(f.kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Left)
            {
                continue;
            }
            let (Some(sp), Some(&o_k)) =
                (p.sources.as_slice().get(k), p.cur.offsets.as_slice().get(k))
            else {
                continue;
            };
            let Some(dp) = sp.derived.as_deref() else {
                continue;
            };
            if tvf_source(sp) || dp.rec.is_some() || dp.mat != 0 {
                continue;
            }
            // Its arms: the head core, then a UNION ALL's others, each
            // as written and as planned.
            let mut arms: AVec<(&SelectStmt, &SubPlan)> = AVec::new();
            arms.push((d, dp));
            let mut ok = d.compound.len() == dp.compound.len();
            for ((op, c), (_, t)) in d.compound.iter().zip(dp.compound.iter()) {
                ok &= *op == crate::sql::CompoundOp::UnionAll;
                arms.push((c, t));
            }
            ok &= arms.iter().all(|(a, q)| {
                a.values.is_empty()
                    && a.items.iter().all(|it| matches!(it, SelectItem::Expr(..)))
                    && !q.items.iter().any(has_win)
            });
            if !ok {
                continue;
            }
            let nk = sp.ncols;
            // The candidate terms.
            let mut cand: AVec<&Expr> = AVec::new();
            fn conj<'e>(e: &'e Expr, out: &mut AVec<&'e Expr>) {
                match e {
                    Expr::Binary(crate::sql::BinOp::And, a, b) => {
                        conj(a, out);
                        conj(b, out);
                    }
                    e => out.push(e),
                }
            }
            // (A LEFT JOIN the WHERE made inner takes its terms too.)
            if sp.kind != JoinKind::Left
                && let Some(w) = &stmt.filter
            {
                conj(w, &mut cand);
            }
            if let Some(on) = &f.on {
                conj(on, &mut cand);
            }
            // Per arm: the terms for its WHERE and its HAVING.
            let mut adds: AVec<(AVec<Expr>, AVec<Expr>)> = AVec::new();
            for _ in arms.iter() {
                adds.push((AVec::new(), AVec::new()));
            }
            let mut any = false;
            for t in cand.iter() {
                let subq = crate::rules::expr_any(t, &mut |x| {
                    matches!(
                        x,
                        Expr::ScalarSubquery(_) | Expr::ExistsSubquery(_) | Expr::InSubquery(..)
                    )
                });
                if subq {
                    continue;
                }
                let Ok(r) = crate::query::resolve_scoped::<S::Error>(
                    t,
                    arena,
                    p.cur,
                    enclosing,
                    &mut planner,
                ) else {
                    continue;
                };
                let band = crate::query::RefRange {
                    w: (o_k, o_k.saturating_add(nk)),
                    s: (0, 0),
                };
                let everything = crate::query::RefRange {
                    w: (0, usize::MAX),
                    s: (0, usize::MAX),
                };
                let mut outside = r.clone();
                // Whether anything but the band is read.
                let other = {
                    let hit = core::cell::Cell::new(false);
                    remap_expr(&mut outside, &|v, src| {
                        if src || !(o_k..o_k.saturating_add(nk)).contains(&v) {
                            hit.set(true);
                        }
                        v
                    });
                    hit.get()
                };
                if other
                    || !crate::query::expr_refs_range(&r, &band)
                    || !crate::query::expr_refs_range(&r, &everything)
                    || crate::query::has_agg(&r)
                    || has_win(&r)
                    || crate::query::any_scalar(&r, &|f| f.nondeterministic())
                {
                    continue;
                }
                // The term in each arm's own words.
                let mut per: AVec<Expr> = AVec::new();
                let mut cols: AVec<usize> = AVec::new();
                for (a, q) in arms.iter() {
                    let mut x = (*t).clone();
                    let mut fine = true;
                    subst_cols(&mut x, &mut |e| {
                        let Ok(RExpr::Column(slot, ..)) = crate::query::resolve_scoped::<S::Error>(
                            e,
                            arena,
                            p.cur,
                            enclosing,
                            &mut planner,
                        ) else {
                            fine = false;
                            return None;
                        };
                        let c = slot.checked_sub(o_k).filter(|c| *c < nk);
                        match c.and_then(|c| a.items.as_slice().get(c).map(|it| (c, it))) {
                            Some((c, SelectItem::Expr(ie, ..))) => {
                                cols.push(c);
                                // The column's collating sequence is
                                // the leftmost arm's (the pin's
                                // `pCList`): an arm whose expression
                                // has another gets it explicitly.
                                let want = dp
                                    .items
                                    .as_slice()
                                    .get(c)
                                    .and_then(crate::query::expr_coll)
                                    .unwrap_or_default();
                                let have = q
                                    .items
                                    .as_slice()
                                    .get(c)
                                    .and_then(crate::query::expr_coll)
                                    .unwrap_or_default();
                                Some(if want == have {
                                    ie.clone()
                                } else {
                                    Expr::Collate(Box::new(ie.clone()), want)
                                })
                            }
                            _ => {
                                fine = false;
                                None
                            }
                        }
                    });
                    if !fine {
                        break;
                    }
                    per.push(x);
                }
                if per.len() != arms.len() {
                    continue;
                }
                // Across a compound's arms a column must compare alike
                // (its affinity is the compound's, not each arm's).
                let alike = cols.iter().all(|c| {
                    let aff = |q: &SubPlan| {
                        q.items
                            .as_slice()
                            .get(*c)
                            .map(crate::query::item_result_affinity)
                    };
                    arms.iter().all(|(_, q)| aff(q) == aff(dp))
                });
                if !alike {
                    continue;
                }
                for ((x, (a, q)), add) in per
                    .into_iter()
                    .zip(arms.iter())
                    .zip(adds.as_mut_slice().iter_mut())
                {
                    let agg = q.has_agg || !q.group_by.is_empty() || a.having.is_some();
                    let grouped = cols.iter().all(|c| {
                        q.items.as_slice().get(*c).is_some_and(|it| {
                            !crate::query::has_agg(it)
                                && q.group_by.iter().any(|g| crate::query::same_expr(g, it))
                        })
                    });
                    if agg && !grouped {
                        add.1.push(x);
                    } else {
                        add.0.push(x);
                    }
                }
                any = true;
            }
            if !any {
                continue;
            }
            let mut d2 = d.clone();
            let and = |a: Option<Expr>, b: Expr| -> Expr {
                match a {
                    None => b,
                    Some(a) => Expr::Binary(crate::sql::BinOp::And, Box::new(a), Box::new(b)),
                }
            };
            let mut it = adds.into_iter();
            if let Some((w, h)) = it.next() {
                for x in w {
                    d2.filter = Some(and(d2.filter.take(), x));
                }
                for x in h {
                    d2.having = Some(and(d2.having.take(), x));
                }
            }
            for ((_, c), (w, h)) in d2.compound.as_mut_slice().iter_mut().zip(it) {
                for x in w {
                    c.filter = Some(and(c.filter.take(), x));
                }
                for x in h {
                    c.having = Some(and(c.having.take(), x));
                }
            }
            let mut f2 = f.clone();
            f2.derived = Some(Box::new(d2));
            self.replan_flat(arena, &f2, enclosing, p, k)?;
        }
        Ok(())
    }

    /// A subquery marked `flat` that the splice gave up: planned again
    /// with its own access paths, in place of source `i` (its join and
    /// ON clause kept).
    fn replan_flat(
        &self,
        arena: &Arena,
        f: &crate::sql::FromSource,
        enclosing: &[crate::query::ScopeGroup],
        p: &mut Parts<'_>,
        i: usize,
    ) -> Result<(), S::Error> {
        let Some(d) = f.derived.as_deref() else {
            return Ok(());
        };
        let Some(old) = p.sources.as_mut_slice().get_mut(i) else {
            return Ok(());
        };
        if old.derived.is_none() {
            return Ok(());
        }
        let mut descs = AVec::new();
        let mut plans = AVec::new();
        self.plan_derived_source(f, d, arena, enclosing, &mut descs, &mut plans)?;
        if let Some(mut sp) = plans.pop() {
            sp.kind = old.kind;
            sp.on = old.on.take();
            #[cfg(feature = "explain")]
            {
                sp.eqp = core::mem::take(&mut old.eqp);
            }
            *old = sp;
        }
        Ok(())
    }
}

/// The columns of the co-routines and materialisations of the core
/// that nothing reads become NULL (the pin computes only the result
/// columns its reader uses, unless the subquery is a CTE's reference
/// or correlated, DISTINCT, an aggregate, windowed, MATERIALIZED, a
/// compound other than UNION ALL, or its ORDER BY names the column).
fn prune(p: &mut Parts<'_>) {
    let offs = p.cur.offsets.clone();
    let (items, filter, group_by, having, order_by) = (
        &mut *p.items,
        &mut *p.filter,
        &mut *p.group_by,
        &mut *p.having,
        &mut *p.order_by,
    );
    prune_sources(
        p.sources.as_mut_slice(),
        offs.as_slice(),
        !p.raw,
        &mut |mark: &dyn Fn(usize, bool) -> usize| {
            for e in items.as_mut_slice() {
                remap_expr(e, mark);
            }
            for e in filter.iter_mut() {
                remap_expr(e, mark);
            }
            for e in group_by.as_mut_slice() {
                remap_expr(e, mark);
            }
            for e in having.iter_mut() {
                remap_expr(e, mark);
            }
            for (e, _) in order_by.as_mut_slice() {
                remap_expr(e, mark);
            }
        },
    );
}

/// A slot marker `prune_sources` hands its reader walk.
pub(super) type Mark<'m> = &'m dyn Fn(usize, bool) -> usize;

/// `prune` over FROM sources `sources` (their columns at `offs`), of
/// which `reads` walks every expression outside the sources
/// themselves — the SELECT core's clauses, or an UPDATE ... FROM's
/// SET expressions and WHERE clause (v0.17 review fix: there every
/// column was computed). Then the pin's reuse of one view's
/// materialization by its copies in one FROM clause
/// (`isSelfJoinView`) is settled: a copy that lost a column computes
/// its own rows (the pin marks it changed, `SF_PushDown`), the copies
/// left share one, and a lone one is materialized when a later copy
/// stands beside it (`fromClauseTermCanBeCoroutine`'s rule 5), else
/// planned on its own (`settle`; not for a body its reader merges).
/// A NOT MATERIALIZED CTE's reference the pin runs as a co-routine is
/// computed alone.
pub(super) fn prune_sources(
    sources: &mut [SourcePlan],
    offs: &[usize],
    settle: bool,
    reads: &mut dyn FnMut(Mark<'_>),
) {
    let n = sources.len();
    for i in 0..n {
        let (Some(sp), Some(&o)) = (sources.get(i), offs.get(i)) else {
            continue;
        };
        let Some(dp) = sp.derived.as_deref() else {
            continue;
        };
        if tvf_source(sp)
            || sp.rec != 0
            || dp.is_cte
            || !dp.uncorrelated
            || dp.distinct
            || dp.has_agg
            || !dp.group_by.is_empty()
            || dp.rec.is_some()
            || !dp.values.is_empty()
            || dp.items.iter().any(has_win)
            || dp.compound.iter().any(|(op, t)| {
                *op != crate::sql::CompoundOp::UnionAll
                    || !t.values.is_empty()
                    || t.distinct
                    || t.has_agg
                    || t.items.iter().any(has_win)
            })
        {
            continue;
        }
        let nk = sp.ncols;
        // Which of its columns any part of the core reads.
        let used = core::cell::RefCell::new({
            let mut v: AVec<bool> = AVec::new();
            for _ in 0..nk {
                v.push(false);
            }
            v
        });
        {
            let mark = |v: usize, src: bool| -> usize {
                if !src
                    && let Some(c) = v.checked_sub(o).filter(|c| *c < nk)
                    && let Ok(mut u) = used.try_borrow_mut()
                    && let Some(x) = u.as_mut_slice().get_mut(c)
                {
                    *x = true;
                }
                v
            };
            reads(&mark);
            for (j, s) in sources.iter_mut().enumerate() {
                if let Some(on) = s.on.as_mut() {
                    remap_expr(on, &mark);
                }
                if let Some(a) = s.access.as_mut() {
                    crate::query::remap_access(a, &mark);
                }
                // A table-valued function's arguments may read it; a
                // FROM subquery's own plan never does.
                if j != i
                    && tvf_source(s)
                    && let Some(d) = s.derived.as_deref_mut()
                {
                    crate::query::remap_plan(d, &mark);
                }
                #[cfg(feature = "fts5")]
                if let Some(vt) = &mut s.vt {
                    for (_, _, args, _) in vt.ors.as_mut_slice() {
                        for a in args.as_mut_slice() {
                            remap_expr(a, &mark);
                        }
                    }
                }
            }
        }
        let used = used.into_inner();
        let Some(dp) = sources.get_mut(i).and_then(|s| s.derived.as_deref_mut()) else {
            continue;
        };
        let mut changed = false;
        for c in 0..nk {
            if used.as_slice().get(c).copied().unwrap_or(true) {
                continue;
            }
            // A column its ORDER BY names is computed for the sort.
            let named = dp.comp_order.iter().any(|(j, _)| *j == c)
                || dp.items.as_slice().get(c).is_some_and(|it| {
                    dp.order_by
                        .iter()
                        .any(|(e, _)| crate::query::same_expr(e, it))
                });
            if named {
                continue;
            }
            if let Some(it) = dp.items.as_mut_slice().get_mut(c) {
                changed |= !matches!(it, RExpr::Literal(OwnedValue::Null));
                *it = RExpr::Literal(OwnedValue::Null);
            }
            for (_, t) in dp.compound.as_mut_slice() {
                if let Some(it) = t.items.as_mut_slice().get_mut(c) {
                    changed |= !matches!(it, RExpr::Literal(OwnedValue::Null));
                    *it = RExpr::Literal(OwnedValue::Null);
                }
            }
        }
        // A view's copy that lost a column computes its own rows.
        if changed && dp.grp != 0 {
            dp.mat = 0;
        }
    }
    if !settle {
        return;
    }
    // A NOT MATERIALIZED CTE's reference in a co-routine's place (no
    // later reference of this FROM clause shares it) is one.
    for i in 0..n {
        let lazy = match sources.get(i).and_then(|s| s.derived.as_deref()) {
            Some(d) if d.lazy && d.mat != 0 => d.mat,
            _ => continue,
        };
        let later = sources
            .iter()
            .skip(i + 1)
            .any(|s| s.derived.as_deref().is_some_and(|d| d.mat == lazy));
        if !later
            && coroutine_at(sources, i)
            && let Some(d) = sources.get_mut(i).and_then(|s| s.derived.as_deref_mut())
        {
            d.mat = 0;
        }
    }
    // The copies of each view group left sharing.
    for i in 0..n {
        let g = match sources.get(i).and_then(|s| s.derived.as_deref()) {
            Some(d) if d.grp != 0 && d.mat != 0 => d.grp,
            _ => continue,
        };
        let member = |s: &SourcePlan| s.derived.as_deref().is_some_and(|d| d.grp == g);
        let sharing = |s: &SourcePlan| {
            s.derived
                .as_deref()
                .is_some_and(|d| d.grp == g && d.mat != 0)
        };
        let before = sources.iter().take(i).any(sharing);
        if before {
            continue;
        }
        let others = sources.iter().skip(i + 1).any(sharing);
        let later = sources.iter().skip(i + 1).any(member);
        if !others
            && !later
            && let Some(d) = sources.get_mut(i).and_then(|s| s.derived.as_deref_mut())
        {
            d.mat = 0;
        }
    }
}

/// Whether source `i` of `sources` stands where the pin's
/// `fromClauseTermCanBeCoroutine` allows a co-routine: no RIGHT /
/// FULL join, and the first source, or one after plain tables of
/// inner joins only.
fn coroutine_at(sources: &[SourcePlan], i: usize) -> bool {
    !sources.iter().any(|s| s.kind.pads_left())
        && (i == 0
            || sources.iter().take(i + 1).enumerate().all(|(j, s)| {
                !matches!(
                    s.kind,
                    crate::sql::JoinKind::Left | crate::sql::JoinKind::Full
                ) && (j == i || s.derived.is_none())
            }))
}

/// Whether nested plan `q` of an `EXISTS` term is one the pin's
/// `existsToJoin` turns into a join loop: one table of main (no view,
/// CTE, subquery, virtual table or table-valued function), no
/// aggregate, compound, window function or LIMIT / OFFSET of its own
/// (only the implicit first-row LIMIT 1 every EXISTS gets).
fn exists_ok(q: &SubPlan) -> bool {
    let [s] = q.sources.as_slice() else {
        return false;
    };
    let implicit = matches!(&q.limit, Some(RExpr::Literal(OwnedValue::Integer(1))));
    implicit
        && q.offset.is_none()
        && !q.has_agg
        && q.group_by.is_empty()
        && q.having.is_none()
        && q.compound.is_empty()
        && q.values.is_empty()
        && q.rec.is_none()
        && !q.items.iter().any(has_win)
        && !q.filter.as_ref().is_some_and(crate::query::has_agg)
        && s.derived.is_none()
        && !tvf_source(s)
        && s.rec == 0
        && !s.temp
        && s.root > 1
        && s.kind == crate::sql::JoinKind::Inner
        && s.on.is_none()
        && ext_none(s)
}

/// The source is no attached database's.
fn ext_none(s: &SourcePlan) -> bool {
    #[cfg(feature = "attach")]
    if s.ext != 0 {
        return false;
    }
    let _ = s;
    true
}

/// The pin's `existsToJoin`: each `EXISTS (SELECT ... FROM t WHERE
/// w)` AND term of the WHERE clause becomes the literal 1, `t` a
/// semi-join source after the core's (`SubSource::exists`: its loop
/// stops at its first row that makes a joined row) and `w` a term of
/// the WHERE clause. Not under OR / NOT, nor from an ON clause; not
/// with a RIGHT / FULL join.
fn exists_join(p: &mut Parts<'_>, base: (usize, usize), stmt: &SelectStmt) {
    let Some(w) = stmt.filter.as_ref() else {
        return;
    };
    if p.sources.iter().any(|s| s.kind.pads_left()) {
        return;
    }
    // The WHERE clause's own terms are the last of the resolved
    // filter's (an inner join's hoisted ON terms come first).
    // Per AND term of the WHERE as written: an EXISTS over one table
    // (the pin rewrites it before flattening the subquery's own FROM
    // subqueries, so one over a view or FROM subquery stays).
    let mut ast: AVec<bool> = AVec::new();
    {
        fn walk(e: &Expr, out: &mut AVec<bool>) {
            match e {
                Expr::Binary(crate::sql::BinOp::And, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Expr::ExistsSubquery(q) => out.push(
                    q.compound.is_empty()
                        && matches!(q.from.as_slice(), [f] if f.derived.is_none()
                            && f.args.is_none()),
                ),
                // A row value's `=` / `IS` resolves to one term per
                // element (v0.17 review fix: the count lost its place).
                Expr::Binary(crate::sql::BinOp::Eq | crate::sql::BinOp::Is, l, r) => {
                    let n = match (&**l, &**r) {
                        (Expr::Vector(v), _) | (_, Expr::Vector(v)) => v.len().max(1),
                        _ => 1,
                    };
                    for _ in 0..n {
                        out.push(false);
                    }
                }
                _ => out.push(false),
            }
        }
        walk(w, &mut ast);
    }
    if !ast.iter().any(|x| *x) {
        return;
    }
    let ast_terms = ast.len();
    let Some(f) = p.filter.take() else {
        return;
    };
    let mut terms = AVec::new();
    and_terms(f, &mut terms);
    // (Fewer terms than written: the count cannot place them.)
    if terms.len() < ast_terms {
        *p.filter = and_all(terms);
        return;
    }
    let first_where = terms.len().saturating_sub(ast_terms);
    let mut added: AVec<RExpr> = AVec::new();
    for t in first_where..terms.len() {
        if p.sources.len() >= 64 {
            break;
        }
        let written = ast
            .as_slice()
            .get(t - first_where)
            .copied()
            .unwrap_or(false);
        let ok =
            written && matches!(terms.as_slice().get(t), Some(RExpr::SubExists(q)) if exists_ok(q));
        if !ok {
            continue;
        }
        let Some(slot) = terms.as_mut_slice().get_mut(t) else {
            continue;
        };
        let RExpr::SubExists(q) = core::mem::replace(slot, RExpr::Literal(OwnedValue::Integer(1)))
        else {
            continue;
        };
        let mut q = *q;
        let Some(mut src) = q.sources.pop() else {
            continue;
        };
        // The new source's slots are the subquery's own (past the
        // core); every other nested subquery's move past it.
        let w_now: usize = p.sources.iter().map(|s| s.ncols).sum();
        let at = (
            base.0.saturating_add(w_now),
            base.1.saturating_add(p.sources.len()),
        );
        let by = src.ncols;
        let shift = |v: usize, s: bool| -> usize {
            if s {
                if v >= at.1 { v.saturating_add(1) } else { v }
            } else if v >= at.0 {
                v.saturating_add(by)
            } else {
                v
            }
        };
        for e in p.items.as_mut_slice() {
            remap_expr(e, &shift);
        }
        for e in terms.as_mut_slice() {
            remap_expr(e, &shift);
        }
        for e in added.as_mut_slice() {
            remap_expr(e, &shift);
        }
        for e in p.group_by.as_mut_slice() {
            remap_expr(e, &shift);
        }
        for e in p.having.iter_mut() {
            remap_expr(e, &shift);
        }
        for (e, _) in p.order_by.as_mut_slice() {
            remap_expr(e, &shift);
        }
        for s in p.sources.as_mut_slice() {
            if let Some(on) = s.on.as_mut() {
                remap_expr(on, &shift);
            }
            if tvf_source(s)
                && let Some(d) = s.derived.as_deref_mut()
            {
                crate::query::remap_plan(d, &shift);
            }
        }
        if let Some(qf) = q.filter.take() {
            added.push(qf);
        }
        src.access = None;
        src.exists = true;
        #[cfg(feature = "explain")]
        {
            src.eqp.base = at.0;
            src.eqp.joined = false;
            src.eqp.omit = false;
            src.eqp.bloom_lines = 0;
        }
        let mut cols = AVec::new();
        for a in src.affs.iter() {
            cols.push((AVec::new(), *a));
        }
        p.cur.offsets.push(at.0);
        p.cur
            .sources
            .push(SourceDesc::new(AVec::new(), cols, src.rowid_alias, true));
        p.sources.push(src);
    }
    for a in added {
        terms.push(a);
    }
    *p.filter = and_all(terms);
}

/// Replace each column reference of `e` (outside subqueries, which
/// the caller has none of) for which `f` gives an expression.
fn subst_cols(e: &mut Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>) {
    if matches!(e, Expr::Column { .. }) {
        if let Some(x) = f(e) {
            *e = x;
        }
        return;
    }
    match e {
        Expr::Binary(_, a, c) | Expr::Like(a, c, _) => {
            subst_cols(a, f);
            subst_cols(c, f);
        }
        Expr::Neg(a)
        | Expr::Pos(a)
        | Expr::Not(a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::Collate(a, _)
        | Expr::BitNot(a)
        | Expr::RowItem(a, ..) => subst_cols(a, f),
        Expr::Vector(l) => {
            for x in l.as_mut_slice() {
                subst_cols(x, f);
            }
        }
        Expr::InList(a, l, _) => {
            subst_cols(a, f);
            for x in l.as_mut_slice() {
                subst_cols(x, f);
            }
        }
        Expr::Between(a, c, d, _) => {
            subst_cols(a, f);
            subst_cols(c, f);
            subst_cols(d, f);
        }
        Expr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(x) = base {
                subst_cols(x, f);
            }
            for (w, t) in whens.as_mut_slice() {
                subst_cols(w, f);
                subst_cols(t, f);
            }
            if let Some(x) = otherwise {
                subst_cols(x, f);
            }
        }
        Expr::Func { args, clause, .. } => {
            for x in args.as_mut_slice() {
                subst_cols(x, f);
            }
            if let Some(c) = clause {
                for (x, _) in c.order_by.as_mut_slice() {
                    subst_cols(x, f);
                }
                if let Some(x) = &mut c.filter {
                    subst_cols(x, f);
                }
            }
        }
        #[cfg(feature = "triggers")]
        Expr::Raise(_, Some(a), _) => subst_cols(a, f),
        _ => {}
    }
}
