//! The pin's other index strategies (v0.17, `multi_index`).
//!
//! `xplan` chooses one loop per source from its index and rowid
//! loops; `xjoin` orders a join's sources. This module adds the loop
//! kinds and planner rules of the pin's `where.c` that those leave
//! out:
//!
//! - **The MULTI-INDEX OR plan** (`whereLoopAddOr`, `WHERE_MULTI_OR`):
//!   a WHERE term `x1 OR x2 OR ...` whose every branch has an index
//!   or rowid loop on the source is a loop of its own, costed as the
//!   sum of each branch's cheapest loop (`whereOrInsert`'s three
//!   cheapest per set of prerequisites), plus one. When chosen, each
//!   branch is planned again on its own with the WHERE clause's other
//!   indexable terms (`xN AND w`, as the pin's code generator does)
//!   and read in turn; a row is returned at the first branch whose
//!   condition it satisfies (the pin's RowSet keeps the rowids found
//!   so far), so the rows come in the pin's order, each once.
//! - **whereLoopInsert / whereLoopAdjustCost for a single table**:
//!   the candidate loops of one source pass through `xjoin::insert`
//!   as a join's do, so `b = 2 AND c = 1` over an index on `(b, c)`
//!   uses both columns.
//!
//! The min() / max() optimisation, skip-scans and the IN operator's
//! statistics rules live where the planner meets them (`xplan`'s
//! `add_index`, the SELECT planner in `session::xindex`).

use crate::arena::AVec;
use crate::query::RExpr;
use crate::sql::BinOp;
use crate::xjoin::JLoop;
use crate::xplan::{
    IdxAccess, IdxModel, Loop, Query, Src, Table, Term, best_access, candidates, conj_truth,
    conjuncts, log_est_add, order_sat, skip_collate_likely, strip, terms,
};

/// One branch of an OR term as the planner reads it for one source.
#[derive(Debug, Clone)]
pub struct OrBranch {
    /// The pin's `INDEX n` (the branch's place in the OR clause's term
    /// list: a commuted copy follows the written branches).
    pub n: u32,
    /// The branch as written.
    pub cond: RExpr,
    /// Its AND terms (the branch itself when it is one comparison).
    pub conj: AVec<RExpr>,
    pub truth: AVec<i32>,
    /// Its usable terms on the source, the sources each one's value
    /// reads, and the sources each AND term reads (`xplan::JMask`).
    pub ts: AVec<Term>,
    pub tmask: AVec<u64>,
    pub cmask: AVec<u64>,
}

/// An OR term of the WHERE clause that may drive a MULTI-INDEX OR
/// loop of one source.
#[derive(Debug, Clone)]
pub struct OrInfo {
    /// The term's place in the WHERE clause (`Query::conj`).
    pub ci: usize,
    pub branches: AVec<OrBranch>,
}

/// One branch of a chosen MULTI-INDEX OR loop: its condition and the
/// path that reads its rows (`None`: the whole table).
#[derive(Debug, Clone)]
pub struct OrSub {
    pub n: u32,
    pub cond: RExpr,
    pub acc: Option<IdxAccess>,
}

/// A chosen MULTI-INDEX OR loop (`IdxAccess::or`).
#[derive(Debug, Clone)]
pub struct OrAccess {
    /// The source's OR term (`OrInfo`) while the caller still has to
    /// plan the branches.
    pub pending: usize,
    pub subs: AVec<OrSub>,
    /// Where the source's columns and rowid sit in the query row.
    pub base: usize,
    pub ncols: usize,
    pub src: usize,
}

impl OrAccess {
    /// A loop chosen by `xplan::best_access` whose branches are yet to
    /// be planned.
    pub(crate) fn pending(k: usize) -> OrAccess {
        OrAccess {
            pending: k,
            subs: AVec::new(),
            base: 0,
            ncols: 0,
            src: 0,
        }
    }
}

/// `e`'s OR operands, nested ORs flattened, each with its COLLATE /
/// likely() wrappers removed (`sqlite3WhereSplit` on TK_OR), with the
/// wrapped operand kept for its truth probability.
fn disjuncts<'x>(e: &'x RExpr, out: &mut AVec<(&'x RExpr, &'x RExpr)>) {
    let mut stack: AVec<&'x RExpr> = AVec::new();
    stack.push(e);
    while let Some(x) = stack.pop() {
        let y = skip_collate_likely(x);
        match y {
            RExpr::Binary(BinOp::Or, a, b) => {
                stack.push(b);
                stack.push(a);
            }
            _ => out.push((y, x)),
        }
    }
}

fn is_col(e: &RExpr) -> bool {
    matches!(strip(e), RExpr::Column(..) | RExpr::Rowid(_))
}

/// A branch the pin analyses as one term (`WO_SINGLE`): a comparison
/// with a column on either side, `col IN (...)`, `col IS NULL`.
/// Returns the column operand (after the pin's in-place commute) and
/// whether a commuted copy exists (a column on both sides).
fn single(d: &RExpr) -> Option<(&RExpr, Option<&RExpr>)> {
    match d {
        RExpr::Binary(
            BinOp::Eq | BinOp::Is | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge,
            l,
            r,
        ) => {
            if is_col(l) {
                Some((strip(l), is_col(r).then(|| strip(r))))
            } else if is_col(r) {
                Some((strip(r), None))
            } else {
                None
            }
        }
        RExpr::InList(x, _, false, _) | RExpr::InSub(x, _, false) if is_col(x) => {
            Some((strip(x), None))
        }
        RExpr::IsNull(x, false) if is_col(x) => Some((strip(x), None)),
        _ => None,
    }
}

/// The OR terms of `conj` that may drive a MULTI-INDEX OR loop of
/// source `s` (the pin's `exprAnalyzeOrTerm` "case 3": every branch
/// has a term on the source; not an OR the pin rewrites to IN).
/// `ok` filters the terms the source may use (a LEFT JOIN's own ON
/// terms); `avail` is `xplan::terms`'; `cmask_of` / `tmask_of` give
/// the sources an expression / a term's value reads.
#[allow(clippy::too_many_arguments)]
pub fn or_infos(
    conj: &[&RExpr],
    wtruth: &[i32],
    s: &Src,
    ok: &dyn Fn(usize) -> bool,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
    cmask_of: &dyn Fn(&RExpr) -> u64,
    tmask_of: &dyn Fn(&Term) -> u64,
) -> AVec<OrInfo> {
    let mut out = AVec::new();
    'terms: for (ci, c) in conj.iter().enumerate() {
        let c = strip(c);
        if !matches!(c, RExpr::Binary(BinOp::Or, ..)) || !ok(ci) || !s.reads(c) {
            continue;
        }
        // (An OR the pin also rewrites as an IN keeps its MULTI-INDEX
        // OR loops, as probed: the two compete on cost.)
        let mut ds: AVec<(&RExpr, &RExpr)> = AVec::new();
        disjuncts(c, &mut ds);
        let nd = ds.len();
        let copyable: AVec<bool> = ds
            .iter()
            .map(|(d, _)| single(d).is_some_and(|(_, r)| r.is_some()))
            .collect();
        let mut branches = AVec::new();
        for (k, (d, wrapped)) in ds.iter().enumerate() {
            let mut parts: AVec<&RExpr> = AVec::new();
            let mut truth: AVec<i32> = AVec::new();
            let n = match single(d) {
                Some((col, copy)) => {
                    parts.push(d);
                    conj_truth(wrapped, &mut truth);
                    if s.reads(col) {
                        (k + 1) as u32
                    } else if copy.is_some_and(|r| s.reads(r)) {
                        let later = copyable
                            .as_slice()
                            .get(k + 1..)
                            .map_or(0, |x| x.iter().filter(|b| **b).count());
                        (nd + 1 + later) as u32
                    } else {
                        // Not indexable for this source.
                        continue 'terms;
                    }
                }
                None => {
                    conjuncts(d, &mut parts);
                    conj_truth(d, &mut truth);
                    (k + 1) as u32
                }
            };
            // The branch's loops may also use the WHERE clause's own
            // terms (the pin's `whereScanNext` reads the OR clause's
            // `pOuter`), though only the branch's adjust its rows.
            let np = parts.len();
            let mut all: AVec<&RExpr> = parts.clone();
            let mut tall: AVec<i32> = truth.clone();
            while tall.len() < np {
                tall.push(1);
            }
            for (i, w) in conj.iter().enumerate() {
                all.push(w);
                tall.push(wtruth.get(i).copied().unwrap_or(1));
            }
            let ts: AVec<Term> = terms(all.as_slice(), tall.as_slice(), s, avail)
                .iter()
                .filter(|t| t.parent < np || ok(t.parent - np))
                .cloned()
                .collect();
            let tmask: AVec<u64> = ts.iter().map(tmask_of).collect();
            let cmask: AVec<u64> = parts.iter().map(|p| cmask_of(p)).collect();
            branches.push(OrBranch {
                n,
                cond: (*d).clone(),
                conj: parts.iter().map(|p| (*p).clone()).collect(),
                truth,
                ts,
                tmask,
                cmask,
            });
        }
        // The pin's code generator reads the OR clause's term list in
        // order: written branches, then the commuted copies.
        branches.as_mut_slice().sort_by_key(|b: &OrBranch| b.n);
        out.push(OrInfo { ci, branches });
    }
    out
}

/// Whether term `t` is the OR-to-IN rewrite of an OR term of `conj`.
pub fn or_in_term(t: &Term, conj: &[&RExpr]) -> bool {
    matches!(t.op, crate::xplan::TOp::In(_))
        && conj
            .get(t.parent)
            .is_some_and(|c| matches!(strip(c), RExpr::Binary(BinOp::Or, ..)))
}

/// The column the pin's OR-to-IN rewrite (`exprAnalyzeOrTerm` "case
/// 1") makes OR term `c` an IN on, if any: every branch `K = value`
/// (written, or as the commuted copy of a comparison of two columns)
/// on one column K of one of `srcs`, values with no other affinity
/// than K's; the first branch's left column is tried first, then the
/// first term on another table.
pub fn in_key<'x>(c: &'x RExpr, srcs: &[Src]) -> Option<&'x RExpr> {
    let mut ds: AVec<(&RExpr, &RExpr)> = AVec::new();
    disjuncts(c, &mut ds);
    // The OR clause's terms: each branch (in-place commuted when only
    // its right side is a column), then the copies, last first.
    let mut ts: AVec<(&RExpr, &RExpr)> = AVec::new();
    let mut copies: AVec<(&RExpr, &RExpr)> = AVec::new();
    for (d, _) in ds.iter() {
        let RExpr::Binary(BinOp::Eq, l, r) = d else {
            return None;
        };
        if is_col(l) {
            ts.push((strip(l), &**r));
            if is_col(r) {
                copies.push((strip(r), &**l));
            }
        } else if is_col(r) {
            ts.push((strip(r), &**l));
        } else {
            return None;
        }
    }
    while let Some(x) = copies.pop() {
        ts.push(x);
    }
    let src_of = |e: &RExpr| srcs.iter().position(|s| s.reads(e));
    // The tables every branch has a term on.
    let mut chng: AVec<usize> = AVec::new();
    for (i, _) in srcs.iter().enumerate() {
        let on = ds.iter().all(|(d, _)| match d {
            RExpr::Binary(_, l, r) => {
                (is_col(l) && src_of(strip(l)) == Some(i))
                    || (is_col(r) && src_of(strip(r)) == Some(i))
            }
            _ => false,
        });
        if on {
            chng.push(i);
        }
    }
    let aff = |e: &RExpr| match strip(e) {
        RExpr::Column(_, a, _) | RExpr::Cast(_, a) | RExpr::AffLiteral(_, Some(a), _) => Some(*a),
        RExpr::Rowid(_) => Some(crate::record::Affinity::Integer),
        _ => None,
    };
    let mut prev: Option<usize> = None;
    for _ in 0..2 {
        let cand = ts.iter().find(|(k, _)| {
            src_of(k).is_some_and(|t| chng.as_slice().contains(&t) && Some(t) != prev)
        });
        let Some((k, _)) = cand else { break };
        let t = src_of(k);
        let ok = ts.iter().filter(|(x, _)| src_of(x) == t).all(|(x, v)| {
            crate::query::same_expr(x, k) && aff(v).is_none_or(|a| Some(a) == aff(x))
        });
        if ok {
            return Some(k);
        }
        prev = t;
    }
    None
}

/// Whether OR-to-IN term `t` (`or_in_term`) of OR term `c` is the IN
/// the pin makes (`in_key`).
pub fn in_kept(t: &Term, c: &RExpr, srcs: &[Src]) -> bool {
    in_key(c, srcs).is_some_and(|k| crate::query::same_expr(k, &t.key))
}

/// The pin's `whereOrInsert` (`N_OR_COST` 3): keep the cheapest
/// `(prereq, rRun, nOut)` per set of prerequisites.
fn or_insert(set: &mut AVec<(u64, i32, i32)>, prereq: u64, run: i32, out: i32) {
    let mut at: Option<usize> = None;
    for (i, p) in set.iter().enumerate() {
        if run <= p.1 && (prereq & p.0) == prereq {
            at = Some(i);
            break;
        }
        if p.1 <= run && (p.0 & prereq) == p.0 {
            return;
        }
    }
    let i = match at {
        Some(i) => i,
        None if set.len() < 3 => {
            set.push((prereq, run, out));
            return;
        }
        None => {
            let mut j = 0;
            for (k, p) in set.iter().enumerate() {
                if set.as_slice().get(j).is_some_and(|q| q.1 > p.1) {
                    j = k;
                }
            }
            if set.as_slice().get(j).is_some_and(|q| q.1 <= run) {
                return;
            }
            j
        }
    };
    if let Some(p) = set.as_mut_slice().get_mut(i) {
        p.0 = prereq;
        p.1 = run;
        if p.2 > out {
            p.2 = out;
        }
    }
}

fn nterms(l: &Loop) -> usize {
    l.eq.len() + usize::from(l.lo.is_some()) + usize::from(l.hi.is_some())
}

/// `whereLoopCheaperProperSubset(x, y)` for loops whose terms come
/// from different WHERE clauses (an OR branch's against the main
/// one's): only a loop with no term shares all of its terms.
fn foreign_subset(x: &Loop, x_io: bool, y: &Loop, y_io: bool) -> bool {
    if x.run > y.run && x.out > y.out {
        return false;
    }
    if x.eq.len() < y.eq.len() && x.ix == y.ix && x.skip == 0 && y.skip == 0 {
        return true;
    }
    let (nx, ny) = (nterms(x), nterms(y));
    if nx >= ny || nx > 0 {
        return false;
    }
    !(x_io && !y_io)
}

fn covers(m: Option<&IdxModel>, q: &Query<'_>) -> bool {
    match (m, q.needed) {
        (Some(m), Some(n)) => m.covers(n),
        (Some(m), None) => m.covers_all,
        (None, _) => false,
    }
}

/// The cost entries of a MULTI-INDEX OR loop of source `s` for OR term
/// `info` (`whereLoopAddOr`): per branch the `(prereq, rRun, nOut)` of
/// its loops that use a term, adjusted against the source's loops in
/// `main` (`whereLoopAdjustCost`, as `whereLoopInsert` does before it
/// keeps only the costs), then summed across the branches. Empty when
/// a branch has no such loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn or_costs(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    info: &OrInfo,
    main: &[JLoop],
    src: usize,
    me: u64,
    pre: u64,
) -> AVec<(u64, i32, i32)> {
    let mut sum: AVec<(u64, i32, i32)> = AVec::new();
    let mut once = true;
    let all: AVec<bool> = (0..s.ncols).map(|_| true).collect();
    for b in info.branches.iter() {
        let conj: AVec<&RExpr> = b.conj.iter().collect();
        let jm = q.jm.map(|j| crate::xplan::JMask {
            cmask: b.cmask.as_slice(),
            tmask: b.tmask.as_slice(),
            me: j.me,
            pre: j.pre,
        });
        let qb = Query {
            conj: conj.as_slice(),
            truth: b.truth.as_slice(),
            order: &[],
            group: false,
            group_by: false,
            ncols_out: q.ncols_out,
            needed: or_needed(q, tab, all.as_slice()),
            cover_scan: q.cover_scan,
            limit: None,
            distinct: false,
            #[cfg(feature = "explain")]
            dset: &[],
            jm: jm.as_ref(),
        };
        let mut cur: AVec<(u64, i32, i32)> = AVec::new();
        for l in candidates(&qb, s, tab, b.ts.as_slice()).iter() {
            if nterms(l) == 0 {
                continue;
            }
            let mut t = l.clone();
            if let Some(m) = t.ix.and_then(|x| tab.indexes.get(x)) {
                let io = covers(Some(m), &qb);
                for p in main.iter() {
                    if p.src != src || p.auto || p.vt || p.l.ix.is_none() || p.l.or.is_some() {
                        continue;
                    }
                    if foreign_subset(&p.l, p.idx_only, &t, io) {
                        t.run = t.run.min(p.l.run);
                        t.out = t.out.min(p.l.out - 1);
                    } else if foreign_subset(&t, io, &p.l, p.idx_only) {
                        t.run = t.run.max(p.l.run);
                        t.out = t.out.max(p.l.out + 1);
                    }
                }
            }
            let mut p = pre;
            for x in t.eq.iter().chain(t.lo.iter()).chain(t.hi.iter()) {
                p |= b.tmask.as_slice().get(*x).copied().unwrap_or(0);
            }
            or_insert(&mut cur, p & !me, t.run, t.out);
        }
        if cur.is_empty() {
            return AVec::new();
        }
        if once {
            sum = cur;
            once = false;
        } else {
            let prev = core::mem::take(&mut sum);
            for a in prev.iter() {
                for c in cur.iter() {
                    or_insert(
                        &mut sum,
                        a.0 | c.0,
                        log_est_add(a.1, c.1),
                        log_est_add(a.2, c.2),
                    );
                }
            }
        }
    }
    sum
}

/// The pin's `iSortIdx` class of a loop of source `s` (see
/// `xjoin::all_loops`).
pub(crate) fn sort_class(q: &Query<'_>, s: &Src, tab: &Table<'_>, ix: Option<usize>) -> usize {
    let rowid_orders = !tab.wr && q.order.iter().any(|(e, _)| s.is_rowid(strip(e)));
    let first = usize::from(!tab.wr);
    match ix.and_then(|x| tab.indexes.get(x).map(|m| (x, m))) {
        None => usize::from(rowid_orders),
        Some((x, m)) => {
            if !q.order.is_empty() && m.might_order(s, q.order) {
                x + 1 + first
            } else {
                0
            }
        }
    }
}

/// One source's loops through the pin's `whereLoopInsert` (with
/// `whereLoopAdjustCost`), in candidate order; `lateral` gives a loop
/// reading an earlier source of the query a prerequisite.
fn insert_all(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    loops: &[Loop],
    lateral: bool,
) -> AVec<JLoop> {
    let mut list: AVec<JLoop> = AVec::new();
    for l in loops.iter() {
        let m = l.ix.and_then(|x| tab.indexes.get(x));
        let lat =
            l.eq.iter()
                .chain(l.lo.iter())
                .chain(l.hi.iter())
                .any(|t| ts.get(*t).is_some_and(|t| t.lateral));
        let jl = JLoop {
            src: 0,
            l: l.clone(),
            prereq: u64::from(lateral && lat),
            setup: 0,
            auto: false,
            width: m.map(|m| m.width),
            sort_idx: sort_class(q, s, tab, l.ix),
            idx_only: covers(m, q),
            vt: false,
        };
        crate::xjoin::insert(&mut list, ts, jl);
    }
    list
}

/// The cheapest loop of `list` as an access path (`xplan::choose`'s
/// last step).
fn pick(q: &Query<'_>, s: &Src, tab: &Table<'_>, ts: &[Term], list: &[JLoop]) -> Option<IdxAccess> {
    let mut loops: AVec<Loop> = AVec::new();
    for jl in list.iter() {
        let mut l = jl.l.clone();
        let (sorted, reverse, bignull) = order_sat(q, s, tab, ts, &l, false);
        l.sorted = sorted;
        l.reverse = reverse;
        l.bignull = bignull;
        loops.push(l);
    }
    best_access(q, s, tab, ts, loops.as_slice())
}

/// Whether WHERE term `c` is one the pin hands each OR branch's
/// sub-plan (`pAndExpr`): an indexable comparison, IN, IS NULL or OR
/// term (`eOperator & WO_ALL`) without a subquery.
pub(crate) fn pushed(c: &RExpr) -> bool {
    let c = strip(c);
    !crate::query::has_sub(c) && (matches!(c, RExpr::Binary(BinOp::Or, ..)) || single(c).is_some())
}

/// Choose a single table's access path with the pin's loop list
/// (v0.17, `multi_index`; `xplan::choose` with `whereLoopInsert` and
/// MULTI-INDEX OR loops). `ors` are the source's OR terms
/// (`or_infos`); `lateral` says loops reading an earlier source have
/// prerequisites (a FROM-order join), `avail` is `xplan::terms`'.
pub fn choose(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    ors: &[OrInfo],
    lateral: bool,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
) -> Option<IdxAccess> {
    let loops = candidates(q, s, tab, ts);
    let mut list = insert_all(q, s, tab, ts, loops.as_slice(), lateral);
    for (k, info) in ors.iter().enumerate() {
        let costs = or_costs(q, s, tab, info, list.as_slice(), 0, 0, 0);
        for (pre, run, out) in costs.iter() {
            let jl = JLoop {
                src: 0,
                l: Loop {
                    ix: None,
                    eq: AVec::new(),
                    lo: None,
                    hi: None,
                    run: run + 1,
                    out: *out,
                    sorted: 0,
                    reverse: false,
                    bignull: None,
                    skip: 0,
                    or: Some(k),
                },
                prereq: u64::from(lateral && *pre != 0),
                setup: 0,
                auto: false,
                width: None,
                sort_idx: 0,
                idx_only: false,
                vt: false,
            };
            crate::xjoin::insert(&mut list, ts, jl);
        }
    }
    let mut a = pick(q, s, tab, ts, list.as_slice())?;
    if let Some(p) = a.or.as_ref().map(|o| o.pending) {
        let info = ors.get(p)?;
        a.or = Some(alloc::boxed::Box::new(or_access(
            q,
            s,
            tab,
            info,
            avail,
            &|_| true,
        )));
    }
    Some(a)
}

/// The access path the pin's code generator plans for one OR branch:
/// the branch's terms and the WHERE clause's pushed terms (`w`) on
/// the table alone, with no ORDER BY and no OR loop of its own.
fn sub_plan(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    b: &OrBranch,
    w: &[usize],
    avail: &dyn Fn(&RExpr) -> Option<bool>,
) -> Option<IdxAccess> {
    let mut conj: AVec<&RExpr> = b.conj.iter().collect();
    let mut truth: AVec<i32> = b.truth.clone();
    while truth.len() < conj.len() {
        truth.push(1);
    }
    for i in w.iter() {
        if let Some(c) = q.conj.get(*i) {
            conj.push(c);
            truth.push(q.truth.get(*i).copied().unwrap_or(1));
        }
    }
    let ts = terms(conj.as_slice(), truth.as_slice(), s, avail);
    let all: AVec<bool> = (0..s.ncols).map(|_| true).collect();
    let qs = Query {
        conj: conj.as_slice(),
        truth: truth.as_slice(),
        order: &[],
        group: false,
        group_by: false,
        ncols_out: q.ncols_out,
        needed: or_needed(q, tab, all.as_slice()),
        cover_scan: q.cover_scan,
        limit: None,
        distinct: false,
        #[cfg(feature = "explain")]
        dset: &[],
        jm: None,
    };
    let loops = candidates(&qs, s, tab, ts.as_slice());
    let list = insert_all(&qs, s, tab, ts.as_slice(), loops.as_slice(), false);
    let mut a = pick(&qs, s, tab, ts.as_slice(), list.as_slice())?;
    finish(&mut a, tab, qs.needed);
    Some(a)
}

/// The columns an OR branch's loop must find: the statement's, or —
/// on a WITHOUT ROWID table, whose MULTI-INDEX OR reads each row
/// through its PRIMARY KEY — all of them (`all`; as probed, the pin
/// never covers a branch there).
fn or_needed<'x>(q: &Query<'x>, tab: &Table<'_>, all: &'x [bool]) -> Option<&'x [bool]> {
    if tab.wr { Some(all) } else { q.needed }
}

/// An access path's covering flags (the SELECT planner's last step):
/// EXPLAIN QUERY PLAN's COVERING, and a rowid table's rows built from
/// the entries of an index that holds every column read.
pub fn finish(a: &mut IdxAccess, tab: &Table<'_>, needed: Option<&[bool]>) {
    let Some(needed) = needed else { return };
    let Some(m) = tab.indexes.iter().find(|m| m.root == a.root && a.root != 0) else {
        return;
    };
    #[cfg(feature = "explain")]
    {
        a.eqp.1 = !m.covers_all && m.covers(needed);
    }
    if !tab.wr && m.covers_plain(needed) && a.bloom.is_none() {
        let mut map: AVec<Option<usize>> = AVec::new();
        for k in m.keys.iter() {
            map.push(match k {
                crate::xplan::IdxKey::Col(c) => Some(*c),
                _ => None,
            });
        }
        a.cover = Some(map);
    }
}

/// A chosen MULTI-INDEX OR loop's branches, each with the path the
/// pin's code generator gives it. `w_ok` says which WHERE terms the
/// branches may read (the pin's `TERM_CODED` / `notReady` rules: in
/// a join, those of the sources already joined and this one).
pub fn or_access(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    info: &OrInfo,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
    w_ok: &dyn Fn(usize) -> bool,
) -> OrAccess {
    let mut w: AVec<usize> = AVec::new();
    for (i, c) in q.conj.iter().enumerate() {
        if i != info.ci && s.reads(c) && pushed(c) && w_ok(i) {
            w.push(i);
        }
    }
    let mut subs = AVec::new();
    for b in info.branches.iter() {
        subs.push(OrSub {
            n: b.n,
            cond: b.cond.clone(),
            acc: sub_plan(q, s, tab, b, w.as_slice(), avail),
        });
    }
    OrAccess {
        pending: info.ci,
        subs,
        base: s.base,
        ncols: s.ncols,
        src: s.src,
    }
}

/// The pin's `minMaxQuery` (v0.17): a core without GROUP BY / HAVING
/// whose one aggregate call (in the result and the ORDER BY; repeats
/// count once) is `min(x)` / `max(x)` without FILTER. Its argument
/// and the order the loops should deliver it in: `x DESC` for max,
/// `x` (NULLs last when `nullable(x)`) for min.
pub fn minmax<'x>(
    items: &'x [RExpr],
    order_by: &'x [(RExpr, crate::sql::SortOrder)],
    grouped: bool,
    nullable: &dyn Fn(&RExpr) -> bool,
) -> Option<(&'x RExpr, crate::sql::SortOrder)> {
    use crate::query::{AggKind, agg_parts, collect_aggs, same_expr};
    if grouped {
        return None;
    }
    let mut aggs: AVec<&RExpr> = AVec::new();
    for it in items.iter() {
        collect_aggs(it, &mut aggs);
    }
    for (e, _) in order_by.iter() {
        collect_aggs(e, &mut aggs);
    }
    let first = *aggs.as_slice().first()?;
    if aggs.iter().any(|a| !same_expr(a, first)) {
        return None;
    }
    let p = agg_parts(first)?;
    if p.star || p.filter.is_some() || p.args.len() != 1 {
        return None;
    }
    let arg = p.args.first()?;
    let coll = crate::query::expr_coll(arg).unwrap_or_default();
    let o = match p.kind {
        AggKind::Max => crate::sql::SortOrder {
            desc: true,
            nulls_first: false,
            coll,
        },
        AggKind::Min => crate::sql::SortOrder {
            desc: false,
            nulls_first: !nullable(arg),
            coll,
        },
        _ => return None,
    };
    Some((arg, o))
}

/// One branch's terms as the pin's `whereNthSubterm` lists them,
/// each as `(column side, operator, value side)`: the branch itself,
/// or an AND branch's terms with their BETWEEN halves and commuted
/// copies.
fn subterms<'x>(d: &'x RExpr, out: &mut AVec<(&'x RExpr, BinOp, &'x RExpr)>) {
    let flip = |o: BinOp| match o {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        o => o,
    };
    let mut parts: AVec<&RExpr> = AVec::new();
    conjuncts(d, &mut parts);
    let and = parts.len() > 1;
    for p in parts.iter() {
        match strip(p) {
            RExpr::Binary(
                o @ (BinOp::Eq | BinOp::Is | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge),
                l,
                r,
            ) => {
                if is_col(l) {
                    out.push((l, *o, r));
                    if and && is_col(r) {
                        out.push((r, flip(*o), l));
                    }
                } else if is_col(r) {
                    out.push((r, flip(*o), l));
                }
            }
            RExpr::Between(x, a, b, false) if and && is_col(x) => {
                out.push((x, BinOp::Ge, a));
                out.push((x, BinOp::Le, b));
            }
            _ => {}
        }
    }
}

/// The virtual terms the pin's `whereCombineDisjuncts` adds for a
/// two-branch OR term `c`: per pair of comparisons (one from each
/// branch) of the same column with the same value, whose operators
/// are all `= < <=` or all `= > >=`, the one comparison both imply
/// (`x < 5 OR x = 5` is `x <= 5`).
pub fn combined(c: &RExpr) -> AVec<RExpr> {
    let mut out = AVec::new();
    let mut ds: AVec<(&RExpr, &RExpr)> = AVec::new();
    disjuncts(c, &mut ds);
    let ([(d1, _), (d2, _)], false) = (
        ds.as_slice(),
        ds.iter()
            .any(|(d, _)| single(d).is_some_and(|(_, r)| r.is_some())),
    ) else {
        return out;
    };
    let (mut a, mut b) = (AVec::new(), AVec::new());
    subterms(d1, &mut a);
    subterms(d2, &mut b);
    let bit = |o: BinOp| -> u8 {
        match o {
            BinOp::Eq => 1,
            BinOp::Lt => 2,
            BinOp::Le => 4,
            BinOp::Gt => 8,
            BinOp::Ge => 16,
            _ => 0,
        }
    };
    for (l1, o1, r1) in a.iter() {
        for (l2, o2, r2) in b.iter() {
            let (b1, b2) = (bit(*o1), bit(*o2));
            if b1 == 0 || b2 == 0 {
                continue;
            }
            let e = b1 | b2;
            if e & !7 != 0 && e & !25 != 0 {
                continue;
            }
            if !crate::query::same_expr(l1, l2) || !crate::query::same_expr(r1, r2) {
                continue;
            }
            let op = if b1 == b2 {
                *o1
            } else if e & 6 != 0 {
                BinOp::Le
            } else {
                BinOp::Ge
            };
            out.push(RExpr::Binary(
                op,
                alloc::boxed::Box::new((*l1).clone()),
                alloc::boxed::Box::new((*r1).clone()),
            ));
        }
    }
    out
}

/// The access path of a table read in its own order (the rowid B-tree,
/// or a WITHOUT ROWID table's clustered index, forward in full), when
/// that order delivers `q`'s (a min() / max() early-out over it: the
/// WHERE may hold the argument constant, `x IS NULL`).
pub fn natural_access(q: &Query<'_>, s: &Src, tab: &Table<'_>, ts: &[Term]) -> Option<IdxAccess> {
    let mut l = Loop {
        ix: if tab.wr {
            tab.indexes.iter().position(|m| m.covers_all)
        } else {
            None
        },
        eq: AVec::new(),
        lo: None,
        hi: None,
        run: 0,
        out: 0,
        sorted: 1,
        reverse: false,
        bignull: None,
        or: None,
        skip: 0,
    };
    let (sat, rev, _) = order_sat(q, s, tab, ts, &l, false);
    if sat == 0 || rev {
        return None;
    }
    l.sorted = sat;
    Some(crate::xplan::lower(s, tab, ts, &l, false))
}
