//! The pin's join planner (v0.17, `join_order`).
//!
//! `xplan` decides one source's loop; this module orders the
//! sources of a join and picks each one's loop together, as the
//! pin's `where.c` does for a FROM clause of two or more tables:
//!
//! - **Loops with prerequisites** (`whereLoopAddAll`,
//!   `whereLoopAddBtree`): every candidate loop of a source
//!   (`xplan::candidates`: the full scan, the rowid and index
//!   equality / range loops) carries the sources its probes read as
//!   its prerequisites, and its `nOut` is lowered only by the WHERE
//!   terms those prerequisites make computable
//!   (`whereLoopOutputAdjust`). A LEFT JOIN's right side and every
//!   source after an explicit CROSS JOIN also require all the sources
//!   before them in FROM order; a LEFT JOIN probes with its own ON
//!   terms only.
//! - **Automatic indexes**: for each WHERE term `col = value` on a
//!   column no index starts with (`termCanDriveIndex`,
//!   `columnIsGoodIndexCandidate`), a loop with the pin's setup cost
//!   (`N log N` plus 28), 20 rows per lookup and `log N + 20` per
//!   lookup, usable only where the outer loops run it more than
//!   once.
//! - **The path solver** (`wherePathSolver`): the `mxChoice` best
//!   partial paths per step (1, 5, then 12 or 18 with the star-query
//!   heuristic of `computeMxChoice`), costs in `LogEst`, the first
//!   pass ignoring the ORDER BY, the interstage heuristic, then a
//!   second pass charging the sort a path leaves
//!   (`wherePathSatisfiesOrderBy`, `whereSortingCost`).
//!
//! - **Virtual tables** (`whereLoopAddVirtual`): a virtual table or
//!   JSON table-valued function contributes one loop per usable
//!   constraint set its xBestIndex accepts (all usable, each distinct
//!   set of other sources, none), costed from its estimatedCost and
//!   estimatedRows, needing the sources its used constraints read, so
//!   a MATCH or a table-valued argument reading a table listed later
//!   in FROM runs after that table.
//! - **Bloom filters** (`whereCheckIfBloomFilterIsUseful`, the
//!   pull-down of `sqlite3ConstructBloomFilter`): with `sqlite_stat1`
//!   rows, a lookup run more often than its table has rows.
//!
//! - **MULTI-INDEX OR loops** (v0.17, `multi_index`; `whereLoopAddOr`,
//!   `crate::xmulti`): after a source's b-tree loops, one per cost
//!   entry of each OR term every branch of which has a loop.
//!
//! What is not modelled (the order or access path may then differ
//! from the pin's, never the rows): without `multi_index`, the
//! MULTI-INDEX OR plan of b-tree tables, skip-scans and the `IN`
//! statistics rule of `whereLoopAddBtreeIndex`; `sqlite_stat4`,
//! likelihood() truth probabilities and the LIKE length adjustment,
//! the EXISTS-to-join rewrite and the `ORDER BY ... LIMIT`
//! ordered-inner-loop case; STATUS.md lists the classes the
//! generated corpus still meets.

use crate::arena::AVec;
use crate::query::RExpr;
use crate::record::{Affinity, KeyOrder};
use crate::sql::SortOrder;
use crate::xplan::{
    IdxKey, JMask, Loop, Query, Src, TOp, Table, Term, candidates, est_log, log_est_add, sort_cost,
};

/// One FROM source as the join planner sees it.
pub struct JSrc<'x> {
    pub s: Src,
    pub tab: Table<'x>,
    /// Its usable terms (`xplan::terms` over `conj` minus the terms
    /// another source's LEFT JOIN owns) and the sources each one's
    /// value reads.
    pub ts: AVec<Term>,
    pub tmask: AVec<u64>,
    /// Per term: found through an equivalence (`a = b AND b op v`
    /// probes `a`), which no automatic index uses.
    pub derived: AVec<bool>,
    /// Per table column: read by the statement (`colUsed`; the rowid
    /// alias is the rowid, not a column).
    pub used: AVec<bool>,
    /// Per table column: its affinity, and whether no index starts
    /// with it (`columnIsGoodIndexCandidate`).
    pub affs: AVec<Affinity>,
    pub good: AVec<bool>,
    /// The right side of a LEFT JOIN (not made inner by the WHERE).
    pub left: bool,
    /// An explicit `CROSS JOIN`.
    pub cross: bool,
    /// The sources a semi-join source must follow (v0.17, `flatten`:
    /// an `EXISTS` turned into a join loop runs after every source its
    /// terms read, so its first row is the one the EXISTS finds).
    #[cfg(feature = "flatten")]
    pub after: u64,
    /// A semi-join source: each of its loops yields at most one row
    /// per combination before it (v0.17, `flatten`).
    #[cfg(feature = "flatten")]
    pub semi: bool,
    /// Automatic indexes are allowed (no INDEXED BY / NOT INDEXED).
    pub auto_ok: bool,
    /// The table's root page (the star-query heuristic's self-join).
    pub root: u32,
    /// The table has `sqlite_stat1` rows (`TF_HasStat1`).
    pub stat: bool,
    /// A virtual table or table-valued function (`whereLoopAddVirtual`):
    /// its loops as `(prerequisites, rRun, nOut)`, one per usable
    /// constraint set its xBestIndex accepted; it has no b-tree loops.
    pub vt: Option<AVec<(u64, i32, i32)>>,
    /// Its OR terms that may drive a MULTI-INDEX OR loop (v0.17,
    /// `multi_index`; `xmulti::or_infos`).
    #[cfg(feature = "multi_index")]
    pub ors: AVec<crate::xmulti::OrInfo>,
}

/// The statement around the join.
pub struct JQuery<'x> {
    /// The WHERE clause's terms: the WHERE's AND terms, then each
    /// source's ON terms in FROM order (the pin's `WhereClause`).
    pub conj: &'x [&'x RExpr],
    /// Per term: its truth probability (`xplan::conj_truth`).
    pub truth: &'x [i32],
    /// Per term: the sources it reads; whether it reads nothing else
    /// (no enclosing query, subquery or non-deterministic call: the
    /// pin's table-constant test); and the LEFT JOIN source whose ON
    /// clause it is.
    pub cmask: &'x [u64],
    pub pure: &'x [bool],
    pub on_of: &'x [Option<usize>],
    /// The ORDER BY / GROUP BY / DISTINCT the join may deliver.
    pub order: &'x [(RExpr, SortOrder)],
    pub group: bool,
    pub group_by: bool,
    /// `order` is the DISTINCT result set (`WHERE_DISTINCTBY`).
    pub distinct_by: bool,
    pub distinct: bool,
    pub ncols_out: usize,
    pub limit: Option<i32>,
    /// Per ORDER BY term: the sources it reads, and whether it is a
    /// constant.
    pub omask: &'x [u64],
    pub oconst: &'x [bool],
}

/// One candidate loop of the solver.
#[derive(Debug, Clone)]
pub(crate) struct JLoop {
    pub src: usize,
    pub l: Loop,
    pub prereq: u64,
    /// `rSetup`: an automatic index's construction.
    pub setup: i32,
    /// An automatic-index loop (`l.eq` holds its driving term).
    pub auto: bool,
    /// The index's `szIdxRow` (`whereLoopIsNoBetter`), for an index.
    pub width: Option<i32>,
    /// `iSortIdx`: the index's place in the table's index list when
    /// it might deliver the ORDER BY, else 0 (`whereLoopFindLesser`
    /// compares only loops of one class).
    pub sort_idx: usize,
    /// The index covers the statement (`WHERE_IDX_ONLY`).
    pub idx_only: bool,
    /// A virtual table's loop (`WHERE_VIRTUALTABLE`).
    pub vt: bool,
}

/// The chosen plan: per position, the loop (its source in
/// `JLoop::src`), which loops scan backwards, how many ORDER BY terms
/// the path delivers and whether it brings a DISTINCT's equal rows
/// together.
pub(crate) struct JPlan {
    pub loops: AVec<JLoop>,
    pub rev: u64,
    pub ob_sat: usize,
    #[cfg_attr(not(feature = "explain"), allow(dead_code))]
    pub distinct_ordered: bool,
    /// Per position: the loop's index or rowid lookups go through a
    /// Bloom filter (`whereCheckIfBloomFilterIsUseful`), and the
    /// position whose filter construction also builds (and EXPLAIN
    /// QUERY PLAN prints) it — itself, or an earlier one by the pin's
    /// Bloom filter pull-down (`sqlite3ConstructBloomFilter`).
    pub bloom_at: AVec<Option<usize>>,
}

/// Whether loop `jl` of source `i` is self-culling (`WHERE_SELFCULL`,
/// `whereLoopOutputAdjust`): a WHERE term it does not use reads its
/// table alone, and is a comparison or the table is no LEFT JOIN's
/// right side.
fn self_cull(js: &JSrc<'_>, jq: &JQuery<'_>, i: usize, jl: &JLoop) -> bool {
    use crate::sql::BinOp;
    let me = 1u64 << i;
    let used = |ci: usize| {
        lterms(jl)
            .iter()
            .any(|t| js.ts.as_slice().get(*t).is_some_and(|t| t.parent == ci))
    };
    let col = |e: &RExpr| matches!(strip(e), RExpr::Column(..) | RExpr::Rowid(_));
    jq.conj.iter().enumerate().any(|(ci, c)| {
        if jq.cmask.get(ci).copied() != Some(me) || used(ci) {
            return false;
        }
        let cmp = match strip(c) {
            RExpr::Binary(BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, l, r) => {
                col(l) || col(r)
            }
            RExpr::InList(x, _, false, _) => col(x),
            _ => false,
        };
        cmp || !js.left
    })
}

/// The pin's `whereCheckIfBloomFilterIsUseful` and Bloom filter
/// pull-down over the chosen path (two or more loops): a lookup loop
/// after the first whose table and every outer one have
/// `sqlite_stat1` rows, with an `=` key, self-culling, keyed on BINARY
/// columns, run more often than its table has rows.
fn blooms(srcs: &[JSrc<'_>], jq: &JQuery<'_>, loops: &[JLoop]) -> AVec<Option<usize>> {
    let mut on: AVec<bool> = AVec::new();
    let mut search: i32 = 0;
    let mut live = true;
    for (pos, jl) in loops.iter().enumerate() {
        let Some(js) = srcs.get(jl.src) else { break };
        live = live && js.stat;
        let bloomable = !jl.auto
            && crate::xplan::skip_of(&jl.l) == 0
            && match jl.l.ix.and_then(|x| js.tab.indexes.get(x)) {
                None => !jl.l.eq.is_empty(),
                Some(m) => (0..jl.l.eq.len()).all(|k| {
                    m.desc
                        .as_slice()
                        .get(k)
                        .is_some_and(|o| o.coll == crate::record::Coll::Binary)
                }),
            };
        on.push(
            live && pos >= 1
                && bloomable
                && column_eq(js.ts.as_slice(), jl)
                && self_cull(js, jq, jl.src, jl)
                && search > js.tab.nrow(),
        );
        search = search.saturating_add(jl.l.out);
    }
    let mut at: AVec<Option<usize>> = AVec::new();
    for _ in loops.iter() {
        at.push(None);
    }
    let mut ready_before = 0u64;
    for (pos, jl) in loops.iter().enumerate() {
        if on.as_slice().get(pos).copied().unwrap_or(false)
            && at.as_slice().get(pos).copied().flatten().is_none()
        {
            if let Some(x) = at.as_mut_slice().get_mut(pos) {
                *x = Some(pos);
            }
            for (q, ql) in loops.iter().enumerate().skip(pos + 1) {
                let Some(qs) = srcs.get(ql.src) else { break };
                if qs.left || ql.prereq & !ready_before != 0 {
                    continue;
                }
                let has_in = ql.l.eq.iter().any(|t| {
                    qs.ts
                        .as_slice()
                        .get(*t)
                        .is_some_and(|t| matches!(t.op, TOp::In(_) | TOp::InSub(_)))
                });
                if on.as_slice().get(q).copied().unwrap_or(false)
                    && !has_in
                    && let Some(x) = at.as_mut_slice().get_mut(q)
                    && x.is_none()
                {
                    *x = Some(pos);
                }
            }
        }
        ready_before |= 1u64 << jl.src;
    }
    at
}

#[derive(Clone)]
struct Path {
    mask: u64,
    rev: u64,
    nrow: i32,
    cost: i32,
    unsort: i32,
    ordered: i8,
    loops: AVec<usize>,
}

/// Whether term `t` of source `js` could drive an automatic index
/// (`termCanDriveIndex`): `col = value` or `col IS value` on a table
/// column (not the rowid) whose affinity the comparison allows, no
/// index starting with it.
fn can_drive(js: &JSrc<'_>, t: &Term) -> Option<usize> {
    if !matches!(t.op, TOp::Eq | TOp::Is) || t.isnull || t.vnull {
        return None;
    }
    let RExpr::Column(slot, ..) = &t.key else {
        return None;
    };
    let c = slot.checked_sub(js.s.base)?;
    if c >= js.s.ncols || js.s.rowid_alias == Some(c) {
        return None;
    }
    js.good
        .as_slice()
        .get(c)
        .copied()
        .unwrap_or(false)
        .then_some(c)
}

/// The position of term `ti` of source `js` in the pin's WHERE
/// clause term list: an original term where it was written; a
/// commuted (`TERM_VIRTUAL`) copy after them all, the copies in the
/// reverse of their originals' order (`sqlite3WhereExprAnalyze`
/// analyses the terms last to first, appending each copy).
fn wc_pos(js: &JSrc<'_>, ti: usize, nconj: usize) -> usize {
    match js.ts.as_slice().get(ti) {
        Some(t) if t.virt => nconj + nconj.saturating_sub(t.parent + 1),
        Some(t) => t.parent,
        None => usize::MAX,
    }
}

/// The FROM-order prerequisites of each source (`whereLoopAddAll`):
/// a LEFT JOIN's right side and an explicit CROSS JOIN need every
/// source before them (and so does each one after it in such a run);
/// a plain join resets them (no RIGHT JOIN reaches the planner).
pub fn from_prereqs(srcs: &[JSrc<'_>]) -> AVec<u64> {
    let mut out = AVec::new();
    let mut pre = 0u64;
    let mut prior = 0u64;
    for (i, js) in srcs.iter().enumerate() {
        if js.left || js.cross {
            pre |= prior;
        } else {
            pre = 0;
        }
        #[cfg(feature = "flatten")]
        out.push(pre | js.after);
        #[cfg(not(feature = "flatten"))]
        out.push(pre);
        prior |= 1u64 << i;
    }
    out
}

/// Every candidate loop of the join, source by source in FROM order
/// (the pin's `pLoops` list order).
pub(crate) fn all_loops(srcs: &[JSrc<'_>], jq: &JQuery<'_>, pre: &[u64]) -> AVec<JLoop> {
    let mut out = AVec::new();
    let mut sort_idx = 0usize;
    for (i, js) in srcs.iter().enumerate() {
        let me = 1u64 << i;
        let mpre = pre.get(i).copied().unwrap_or(0);
        // A virtual table's loops (`whereLoopAddVirtual`).
        if let Some(vl) = &js.vt {
            for (p, run, nout) in vl.iter() {
                let jl = JLoop {
                    src: i,
                    l: Loop {
                        ix: None,
                        eq: AVec::new(),
                        lo: None,
                        hi: None,
                        run: *run,
                        out: *nout,
                        sorted: 0,
                        reverse: false,
                        bignull: None,
                        #[cfg(feature = "multi_index")]
                        or: None,
                        #[cfg(feature = "multi_index")]
                        skip: 0,
                    },
                    prereq: (mpre | *p) & !me,
                    setup: 0,
                    auto: false,
                    width: None,
                    sort_idx: 0,
                    idx_only: false,
                    vt: true,
                };
                insert(&mut out, js.ts.as_slice(), jl);
            }
            continue;
        }
        let rsize = js.tab.nrow();
        // Automatic indexes first, in WHERE-clause term order.
        if js.auto_ok {
            let mut drive: AVec<(usize, usize)> = AVec::new();
            for (ti, t) in js.ts.iter().enumerate() {
                if can_drive(js, t).is_some()
                    && !js.derived.as_slice().get(ti).copied().unwrap_or(false)
                {
                    drive.push((wc_pos(js, ti, jq.conj.len()), ti));
                }
            }
            drive.as_mut_slice().sort_by_key(|d| d.0);
            let rlog = est_log(rsize);
            for (_, ti) in drive.iter() {
                let mut eq = AVec::new();
                eq.push(*ti);
                let jl = JLoop {
                    src: i,
                    l: Loop {
                        ix: None,
                        eq,
                        lo: None,
                        hi: None,
                        run: log_est_add(rlog, 43),
                        out: 43,
                        sorted: 0,
                        reverse: false,
                        bignull: None,
                        #[cfg(feature = "multi_index")]
                        or: None,
                        #[cfg(feature = "multi_index")]
                        skip: 0,
                    },
                    prereq: mpre | js.tmask.as_slice().get(*ti).copied().unwrap_or(0),
                    setup: (rlog + rsize + 28).max(0),
                    auto: true,
                    width: None,
                    sort_idx,
                    idx_only: false,
                    vt: false,
                };
                insert(&mut out, js.ts.as_slice(), jl);
            }
        }
        let jm = JMask {
            cmask: jq.cmask,
            tmask: js.tmask.as_slice(),
            me,
            pre: mpre,
        };
        let mut needed: AVec<bool> = AVec::new();
        for u in js.used.iter() {
            needed.push(*u);
        }
        let q = Query {
            conj: jq.conj,
            truth: jq.truth,
            order: jq.order,
            group: jq.group,
            group_by: jq.group_by,
            ncols_out: jq.ncols_out,
            needed: Some(needed.as_slice()),
            cover_scan: true,
            limit: jq.limit,
            distinct: jq.distinct,
            #[cfg(feature = "explain")]
            dset: &[],
            jm: Some(&jm),
        };
        // The rowid "index" first on a rowid table, then the indexes
        // in the pin's list order (`iSortIdx` counts from 1).
        let rowid_orders = !js.tab.wr
            && jq
                .order
                .iter()
                .any(|(e, _)| matches!(ob_col(js, e), Some(None)));
        let first = usize::from(!js.tab.wr);
        let class = |ix: Option<usize>| -> usize {
            match ix.and_then(|x| js.tab.indexes.get(x).map(|m| (x, m))) {
                None => usize::from(rowid_orders),
                Some((x, m)) => {
                    if !jq.order.is_empty() && m.might_order(&js.s, jq.order) {
                        x + 1 + first
                    } else {
                        0
                    }
                }
            }
        };
        for l in candidates(&q, &js.s, &js.tab, js.ts.as_slice()).iter() {
            let mut p = mpre;
            for t in l.eq.iter().chain(l.lo.iter()).chain(l.hi.iter()) {
                p |= js.tmask.as_slice().get(*t).copied().unwrap_or(0);
            }
            let m = l.ix.and_then(|x| js.tab.indexes.get(x));
            let jl = JLoop {
                src: i,
                l: l.clone(),
                prereq: p & !me,
                setup: 0,
                auto: false,
                width: m.map(|m| m.width),
                sort_idx: class(l.ix),
                idx_only: m.is_some_and(|m| m.covers(needed.as_slice())),
                vt: false,
            };
            insert(&mut out, js.ts.as_slice(), jl);
        }
        // Its MULTI-INDEX OR loops (v0.17, `multi_index`;
        // `whereLoopAddOr`, after the b-tree loops).
        #[cfg(feature = "multi_index")]
        for (k, info) in js.ors.iter().enumerate() {
            let costs =
                crate::xmulti::or_costs(&q, &js.s, &js.tab, info, out.as_slice(), i, me, mpre);
            for (p, run, nout) in costs.iter() {
                let jl = JLoop {
                    src: i,
                    l: Loop {
                        ix: None,
                        eq: AVec::new(),
                        lo: None,
                        hi: None,
                        run: run + 1,
                        out: *nout,
                        sorted: 0,
                        reverse: false,
                        bignull: None,
                        or: Some(k),
                        skip: 0,
                    },
                    prereq: (p | mpre) & !me,
                    setup: 0,
                    auto: false,
                    width: None,
                    sort_idx: 0,
                    idx_only: false,
                    vt: false,
                };
                insert(&mut out, js.ts.as_slice(), jl);
            }
        }
        // (The template's `iSortIdx` is left at the last index's for
        // the next table's automatic-index loops.)
        sort_idx = js
            .tab
            .indexes
            .len()
            .checked_sub(1)
            .map_or(usize::from(rowid_orders), |x| class(Some(x)));
    }
    out
}

/// The terms a loop uses (`aLTerm`), as indexes into its source's
/// terms.
fn lterms(l: &JLoop) -> AVec<usize> {
    let mut v = AVec::new();
    for t in l.l.eq.iter().chain(l.l.lo.iter()).chain(l.l.hi.iter()) {
        // (Not a skip-scan's empty slots.)
        if *t != usize::MAX {
            v.push(*t);
        }
    }
    v
}

/// The pin's `whereLoopCheaperProperSubset(x, y)`.
fn cheaper_subset(x: &JLoop, y: &JLoop) -> bool {
    if x.l.run > y.l.run && x.l.out > y.l.out {
        return false;
    }
    let (sx, sy) = (crate::xplan::skip_of(&x.l), crate::xplan::skip_of(&y.l));
    if x.l.eq.len() < y.l.eq.len() && x.l.ix == y.l.ix && sx == 0 && sy == 0 {
        return true;
    }
    let (tx, ty) = (lterms(x), lterms(y));
    if tx.len() >= ty.len() || sy > sx {
        return false;
    }
    if !tx.iter().all(|t| ty.iter().any(|u| u == t)) {
        return false;
    }
    !(x.idx_only && !y.idx_only)
}

/// Whether loop `l` reads through an index (`WHERE_INDEXED`).
fn indexed(l: &JLoop) -> bool {
    !l.auto && l.l.ix.is_some()
}

/// Whether loop `l` has an `=` / `IS` key term (`WHERE_COLUMN_EQ`).
fn column_eq(ts: &[Term], l: &JLoop) -> bool {
    l.l.eq.iter().any(|t| {
        ts.get(*t)
            .is_some_and(|t| matches!(t.op, TOp::Eq | TOp::Is) && !t.isnull)
    })
}

/// The pin's `whereLoopFindLesser` from `from`: `Err(())` to drop the
/// template, `Ok(Some(k))` to overwrite loop `k`, `Ok(None)` to append.
fn find_lesser(list: &[JLoop], from: usize, ts: &[Term], t: &JLoop) -> Result<Option<usize>, ()> {
    for (k, p) in list.iter().enumerate().skip(from) {
        if p.src != t.src || p.sort_idx != t.sort_idx {
            continue;
        }
        if p.auto
            && crate::xplan::skip_of(&t.l) == 0
            && indexed(t)
            && column_eq(ts, t)
            && (p.prereq & t.prereq) == t.prereq
        {
            return Ok(Some(k));
        }
        if (p.prereq & t.prereq) == p.prereq
            && p.setup <= t.setup
            && p.l.run <= t.l.run
            && p.l.out <= t.l.out
        {
            return Err(());
        }
        if (p.prereq & t.prereq) == t.prereq && p.l.run >= t.l.run && p.l.out >= t.l.out {
            return Ok(Some(k));
        }
    }
    Ok(None)
}

/// The pin's `whereLoopInsert`: the template's cost adjusted against
/// the loops it is a proper subset or superset of
/// (`whereLoopAdjustCost`), then dropped, put in place of the first
/// loop it beats (removing the later ones it also beats), or added.
pub(crate) fn insert(list: &mut AVec<JLoop>, ts: &[Term], mut t: JLoop) {
    if indexed(&t) {
        for p in list.iter() {
            if p.src != t.src || !indexed(p) {
                continue;
            }
            if cheaper_subset(p, &t) {
                t.l.run = t.l.run.min(p.l.run);
                t.l.out = t.l.out.min(p.l.out - 1);
            } else if cheaper_subset(&t, p) {
                t.l.run = t.l.run.max(p.l.run);
                t.l.out = t.l.out.max(p.l.out + 1);
            }
        }
    }
    match find_lesser(list.as_slice(), 0, ts, &t) {
        Err(()) => {}
        Ok(None) => list.push(t),
        Ok(Some(k)) => {
            let mut from = k + 1;
            while let Ok(Some(d)) = find_lesser(list.as_slice(), from, ts, &t) {
                list.remove_at(d);
                from = d;
            }
            if let Some(slot) = list.as_mut_slice().get_mut(k) {
                *slot = t;
            }
        }
    }
}

/// Whether loop `jl` finds at most one row (`WHERE_ONEROW`): an
/// equality on the rowid, or on every key column of a UNIQUE index
/// whose key is NOT NULL (or a one-column UNIQUE index probed with
/// `=`), with no IN before it.
pub(crate) fn one_row(js: &JSrc<'_>, jl: &JLoop) -> bool {
    if jl.auto
        || jl.l.lo.is_some()
        || jl.l.hi.is_some()
        || jl.l.eq.is_empty()
        || crate::xplan::skip_of(&jl.l) != 0
    {
        return false;
    }
    let op = |t: usize| js.ts.as_slice().get(t).map(|t| (&t.op, t.isnull));
    let no_in =
        jl.l.eq
            .iter()
            .all(|t| !matches!(op(*t), Some((TOp::In(_) | TOp::InSub(_), _))));
    if !no_in {
        return false;
    }
    match jl.l.ix.and_then(|i| js.tab.indexes.get(i)) {
        None => true,
        Some(m) => {
            if !m.unique || jl.l.eq.len() < m.keys.len() {
                return false;
            }
            let not_null = m.keys.iter().all(|k| match k {
                IdxKey::Col(c) => js.tab.notnull.get(*c).copied().unwrap_or(false),
                IdxKey::Rowid => true,
                IdxKey::Expr(_) => false,
            });
            let last_eq =
                jl.l.eq
                    .as_slice()
                    .last()
                    .is_some_and(|t| matches!(op(*t), Some((TOp::Eq, false))));
            not_null || (m.keys.len() == 1 && last_eq)
        }
    }
}

/// One ordered column of a loop: a table column, the rowid, or an
/// index expression; its direction and collation.
enum OCol<'x> {
    Col(usize),
    Rowid,
    Expr(&'x RExpr),
}

fn strip(e: &RExpr) -> &RExpr {
    match e {
        RExpr::Collate(x, _) => strip(x),
        RExpr::Scalar(crate::query::ScalarFn::Likely, a) => match a.as_slice().first() {
            Some(x) => strip(x),
            None => e,
        },
        _ => e,
    }
}

/// The ORDER BY term `e` as a column of source `js`: `Some(Some(c))`
/// for column `c`, `Some(None)` for the rowid.
fn ob_col(js: &JSrc<'_>, e: &RExpr) -> Option<Option<usize>> {
    match strip(e) {
        RExpr::Rowid(s) if *s == js.s.src => Some(None),
        RExpr::Column(slot, ..) => {
            let c = slot.checked_sub(js.s.base)?;
            if c >= js.s.ncols {
                return None;
            }
            if js.s.rowid_alias == Some(c) {
                Some(None)
            } else {
                Some(Some(c))
            }
        }
        _ => None,
    }
}

/// The pin's `wherePathSatisfiesOrderBy` over `path` + `last`: how
/// many ORDER BY terms the rows come in (`n` when all), or -1 when
/// that is not known until more loops are added; the loops to scan
/// backwards go into `rev`.
#[allow(clippy::too_many_arguments)]
fn path_order(
    srcs: &[JSrc<'_>],
    jq: &JQuery<'_>,
    loops: &[JLoop],
    path: &[usize],
    last: usize,
    group: bool,
    rev: &mut u64,
) -> i8 {
    let n = jq.order.len();
    if n == 0 || n > 63 {
        return 0;
    }
    let ob_done: u64 = (1u64 << n) - 1;
    let mut ob_sat: u64 = 0;
    let mut distinct = true;
    let mut od_mask: u64 = 0;
    let mut ready: u64 = 0;
    let nloop = path.len();
    let mut i_loop = 0usize;
    while distinct && ob_sat < ob_done && i_loop <= nloop {
        let li = if i_loop < nloop {
            path.get(i_loop).copied().unwrap_or(0)
        } else {
            last
        };
        if i_loop > 0 {
            let prev = if i_loop - 1 < nloop {
                path.get(i_loop - 1).copied().unwrap_or(0)
            } else {
                last
            };
            if let Some(p) = loops.get(prev) {
                ready |= 1u64 << p.src;
            }
        }
        let Some(jl) = loops.get(li) else { break };
        let Some(js) = srcs.get(jl.src) else { break };
        // ORDER BY columns held constant by a `col = value` / `col IS
        // NULL` term on the outer loops.
        for (i, (e, o)) in jq.order.iter().enumerate() {
            if ob_sat & (1u64 << i) != 0 {
                continue;
            }
            let Some(col) = ob_col(js, e) else { continue };
            let hit = js.ts.iter().enumerate().any(|(ti, t)| {
                if !matches!(t.op, TOp::Eq | TOp::Is) {
                    return false;
                }
                if js.tmask.as_slice().get(ti).copied().unwrap_or(0) & !ready != 0 {
                    return false;
                }
                let same = match col {
                    None => js.s.is_rowid(&t.key),
                    Some(c) => js.s.is_col(&t.key, c),
                };
                if !same {
                    return false;
                }
                if let Some(c) = col {
                    if t.coll != o.coll && !t.isnull {
                        return false;
                    }
                    // An affinity conversion would be required.
                    let kaff = js.affs.as_slice().get(c).copied();
                    if let RExpr::Column(_, a, _) = strip(&t.val)
                        && Some(*a) != kaff
                    {
                        return false;
                    }
                }
                true
            });
            if hit {
                ob_sat |= 1u64 << i;
            }
        }
        if jl.vt {
            // (A virtual table's rows come in no order hipp uses.)
            break;
        }
        if !one_row(js, jl) {
            if jl.auto {
                return 0;
            }
            // A MULTI-INDEX OR loop has no index order (v0.17).
            #[cfg(feature = "multi_index")]
            if jl.l.or.is_some() {
                return 0;
            }
            // The ordered columns: an index's key (then the rowid or a
            // WITHOUT ROWID table's key suffix), or the rowid alone.
            let mut cols: AVec<(OCol<'_>, KeyOrder)> = AVec::new();
            let mut nkey = 0usize;
            match jl.l.ix.and_then(|x| js.tab.indexes.get(x)) {
                None => {
                    distinct = false;
                    cols.push((OCol::Rowid, KeyOrder::default()));
                }
                Some(m) => {
                    nkey = m.keys.len();
                    distinct = m.unique;
                    for (k, key) in m.keys.iter().enumerate() {
                        let o = m.desc.as_slice().get(k).copied().unwrap_or_default();
                        cols.push((
                            match key {
                                IdxKey::Col(c) if js.s.rowid_alias == Some(*c) => OCol::Rowid,
                                IdxKey::Col(c) => OCol::Col(*c),
                                IdxKey::Rowid => OCol::Rowid,
                                IdxKey::Expr(x) => OCol::Expr(x),
                            },
                            o,
                        ));
                    }
                    if js.tab.wr {
                        for (key, o) in m.suffix.iter() {
                            if let IdxKey::Col(c) = key {
                                cols.push((OCol::Col(*c), *o));
                            }
                        }
                    } else {
                        cols.push((OCol::Rowid, KeyOrder::default()));
                    }
                }
            }
            let neq = jl.l.eq.len();
            let mut rev_set = false;
            let mut rv = false;
            let mut distinct_cols = false;
            let skips = crate::xplan::skip_of(&jl.l);
            for (j, (c, ko)) in cols.iter().enumerate() {
                if j < neq && skips == 0 {
                    let t =
                        jl.l.eq
                            .as_slice()
                            .get(j)
                            .and_then(|t| js.ts.as_slice().get(*t));
                    match t.map(|t| (&t.op, t.isnull)) {
                        Some((TOp::Eq, false)) => continue,
                        Some((TOp::Is, _)) | Some((TOp::Eq, true)) => {
                            distinct = false;
                            continue;
                        }
                        _ => {}
                    }
                }
                if distinct {
                    match c {
                        OCol::Col(x)
                            if j >= neq && !js.tab.notnull.get(*x).copied().unwrap_or(false) =>
                        {
                            distinct = false
                        }
                        OCol::Expr(_) => distinct = false,
                        _ => {}
                    }
                }
                let mut matched: Option<usize> = None;
                for (i, (e, o)) in jq.order.iter().enumerate() {
                    if ob_sat & (1u64 << i) != 0 {
                        continue;
                    }
                    let hit = match c {
                        OCol::Rowid => ob_col(js, e) == Some(None),
                        OCol::Col(x) => ob_col(js, e) == Some(Some(*x)) && o.coll == ko.coll,
                        OCol::Expr(x) => crate::query::same_expr(strip(e), x) && o.coll == ko.coll,
                    };
                    if hit {
                        matched = Some(i);
                    }
                    if hit || !group {
                        break;
                    }
                }
                let mut ok = matched.is_some();
                if let Some(i) = matched {
                    let Some(o) = jq.order.get(i).map(|x| x.1) else {
                        break;
                    };
                    if !jq.group_by {
                        let want = o.desc;
                        if rev_set {
                            if (rv ^ ko.desc) != want {
                                ok = false;
                            }
                        } else {
                            rv = ko.desc ^ want;
                            if rv {
                                *rev |= 1u64 << i_loop;
                            }
                            rev_set = true;
                        }
                    }
                    // NULLS where the index does not put them: only on
                    // the column after the equality ones.
                    if ok && !group && o.nulls_first == o.desc && j != neq {
                        ok = false;
                    }
                    if ok {
                        if matches!(c, OCol::Rowid) {
                            distinct_cols = true;
                        }
                        ob_sat |= 1u64 << i;
                    }
                }
                if !ok {
                    if j == 0 || j < nkey {
                        distinct = false;
                    }
                    break;
                }
            }
            if distinct_cols {
                distinct = true;
            }
        }
        if distinct {
            od_mask |= 1u64 << jl.src;
            for i in 0..n {
                if ob_sat & (1u64 << i) != 0 {
                    continue;
                }
                let m = jq.omask.get(i).copied().unwrap_or(u64::MAX);
                if m == 0 && !jq.oconst.get(i).copied().unwrap_or(false) {
                    continue;
                }
                if m & !od_mask == 0 {
                    ob_sat |= 1u64 << i;
                }
            }
        }
        i_loop += 1;
    }
    if ob_sat == ob_done {
        return n as i8;
    }
    if !distinct {
        let mut i = n.saturating_sub(1);
        while i > 0 {
            let m = (1u64 << i) - 1;
            if ob_sat & m == m {
                return i as i8;
            }
            i -= 1;
        }
        return 0;
    }
    -1
}

/// The pin's `computeMxChoice` for three or more sources: 18 for a
/// star query (raising the dimension tables' full-scan costs just
/// past the fact table's), else 12.
fn mx_choice(srcs: &[JSrc<'_>], loops: &mut [JLoop]) -> usize {
    let n = srcs.len();
    if n <= 1 {
        return 1;
    }
    if n == 2 {
        return 5;
    }
    let mut star = false;
    if n >= 4 {
        let mut start = 0usize;
        for f in 0..n {
            let m = 1u64 << f;
            let Some(fact) = srcs.get(f) else { break };
            if fact.left || fact.cross {
                if f + 3 > n {
                    break;
                }
                while loops.get(start).is_some_and(|l| l.src <= f) {
                    start += 1;
                }
            }
            let mut ndep = 0usize;
            let mut seen = 0u64;
            let mut selfj = 0u64;
            for l in loops.get(start..).unwrap_or(&[]).iter() {
                let Some(ls) = srcs.get(l.src) else { break };
                if ls.left || ls.cross {
                    break;
                }
                let lm = 1u64 << l.src;
                if l.prereq & m != 0 && lm & seen == 0 && lm & selfj == 0 {
                    if ls.root == fact.root {
                        selfj |= m;
                    } else {
                        ndep += 1;
                        seen |= lm;
                    }
                }
            }
            if ndep <= 2 {
                continue;
            }
            star = true;
            let mut mx = i32::MIN;
            for l in loops.get(start..).unwrap_or(&[]).iter() {
                if l.src < f {
                    continue;
                }
                if l.src > f {
                    break;
                }
                mx = mx.max(l.l.run);
            }
            let mx = mx.saturating_add(1);
            for l in loops.get_mut(start..).unwrap_or(&mut []).iter_mut() {
                if (1u64 << l.src) & seen == 0 {
                    continue;
                }
                #[cfg(feature = "multi_index")]
                let or = usize::from(l.l.or.is_some());
                #[cfg(not(feature = "multi_index"))]
                let or = 0;
                let nterm = l.l.eq.len()
                    + usize::from(l.l.lo.is_some())
                    + usize::from(l.l.hi.is_some())
                    + or;
                if nterm != 0 {
                    continue;
                }
                if l.l.run < mx {
                    l.l.run = mx;
                }
            }
        }
    }
    if star { 18 } else { 12 }
}

/// One run of the pin's `wherePathSolver`: `nrow_est` 0 ignores the
/// ORDER BY.
fn solve(
    srcs: &[JSrc<'_>],
    jq: &JQuery<'_>,
    loops: &[JLoop],
    disabled: &[bool],
    mx: usize,
    nrow_est: i32,
) -> Option<Path> {
    let nloop = srcs.len();
    let n_ob = if nrow_est == 0 { 0 } else { jq.order.len() };
    let mut sort_costs: AVec<i32> = AVec::new();
    for _ in 0..n_ob {
        sort_costs.push(0);
    }
    let mut from: AVec<Path> = AVec::new();
    from.push(Path {
        mask: 0,
        rev: 0,
        nrow: 0,
        cost: 0,
        unsort: 0,
        ordered: if n_ob > 0 { -1 } else { 0 },
        loops: AVec::new(),
    });
    let q = Query {
        conj: jq.conj,
        truth: jq.truth,
        order: jq.order,
        group: jq.group,
        group_by: jq.group_by,
        ncols_out: jq.ncols_out,
        needed: None,
        cover_scan: true,
        limit: jq.limit,
        distinct: jq.distinct,
        #[cfg(feature = "explain")]
        dset: &[],
        jm: None,
    };
    for i_loop in 0..nloop {
        let mut to: AVec<Path> = AVec::new();
        let mut mx_i = 0usize;
        let mut mx_cost = 0i32;
        let mut mx_unsort = 0i32;
        for pf in from.iter() {
            for (wi, w) in loops.iter().enumerate() {
                if disabled.get(wi).copied().unwrap_or(true) {
                    continue;
                }
                let self_bit = 1u64 << w.src;
                if w.prereq & !pf.mask != 0 || self_bit & pf.mask != 0 {
                    continue;
                }
                if w.auto && pf.nrow < 3 {
                    continue;
                }
                let mut unsort = w.l.run + pf.nrow;
                if w.setup != 0 {
                    unsort = log_est_add(w.setup, unsort);
                }
                unsort = log_est_add(unsort, pf.unsort);
                let n_out = pf.nrow + w.l.out;
                let mask_new = pf.mask | self_bit;
                let mut ordered = pf.ordered;
                let rev;
                if ordered < 0 {
                    let mut r = 0u64;
                    ordered =
                        path_order(srcs, jq, loops, pf.loops.as_slice(), wi, jq.group, &mut r);
                    rev = r;
                } else {
                    rev = pf.rev;
                }
                let cost;
                if ordered >= 0 && (ordered as usize) < n_ob {
                    let k = ordered as usize;
                    let sc = match sort_costs.as_slice().get(k).copied() {
                        Some(0) | None => {
                            let c = sort_cost(&q, nrow_est, n_ob, k);
                            if let Some(x) = sort_costs.as_mut_slice().get_mut(k) {
                                *x = c;
                            }
                            c
                        }
                        Some(c) => c,
                    };
                    cost = log_est_add(unsort, sc) + 3;
                } else {
                    cost = unsort;
                    unsort -= 2;
                }
                let last = i_loop + 1 == nloop;
                let mut jj = to
                    .iter()
                    .position(|p| p.mask == mask_new && ((p.ordered < 0) == (ordered < 0) || last));
                match jj {
                    None => {
                        if to.len() >= mx
                            && (cost > mx_cost || (cost == mx_cost && unsort >= mx_unsort))
                        {
                            continue;
                        }
                        if to.len() < mx {
                            to.push(Path {
                                mask: 0,
                                rev: 0,
                                nrow: 0,
                                cost: 0,
                                unsort: 0,
                                ordered: 0,
                                loops: AVec::new(),
                            });
                            jj = Some(to.len() - 1);
                        } else {
                            jj = Some(mx_i);
                        }
                    }
                    Some(j) => {
                        let Some(p) = to.as_slice().get(j) else {
                            continue;
                        };
                        let no_better = || {
                            let base = p.loops.as_slice().get(i_loop).and_then(|b| loops.get(*b));
                            match (w.width, base.and_then(|b| b.width)) {
                                (Some(a), Some(b)) => a >= b,
                                _ => true,
                            }
                        };
                        if p.cost < cost
                            || (p.cost == cost && p.nrow < n_out)
                            || (p.cost == cost && p.nrow == n_out && p.unsort < unsort)
                            || (p.cost == cost
                                && p.nrow == n_out
                                && p.unsort == unsort
                                && no_better())
                        {
                            continue;
                        }
                    }
                }
                let Some(j) = jj else { continue };
                let mut ls = pf.loops.clone();
                ls.push(wi);
                if let Some(p) = to.as_mut_slice().get_mut(j) {
                    *p = Path {
                        mask: mask_new,
                        rev,
                        nrow: n_out,
                        cost,
                        unsort,
                        ordered,
                        loops: ls,
                    };
                }
                if to.len() >= mx {
                    mx_i = 0;
                    let first = to.as_slice().first();
                    mx_cost = first.map_or(0, |p| p.cost);
                    // (The pin's `mxUnsort = aTo[0].nRow`.)
                    mx_unsort = first.map_or(0, |p| p.nrow);
                    for (k, p) in to.iter().enumerate().skip(1) {
                        if p.cost > mx_cost || (p.cost == mx_cost && p.unsort > mx_unsort) {
                            mx_cost = p.cost;
                            mx_unsort = p.unsort;
                            mx_i = k;
                        }
                    }
                }
            }
        }
        from = to;
    }
    from.as_slice().first().cloned()
}

/// Plan the join (two or more sources, at most 64, as the pin): the loops in
/// the order the pin runs them. `None` when no path exists (the pin's
/// "no query solution" cannot arise here: the full scans need only
/// the FROM-order prerequisites).
pub(crate) fn plan(srcs: &[JSrc<'_>], jq: &JQuery<'_>) -> Option<JPlan> {
    let n = srcs.len();
    if !(2..=64).contains(&n) {
        return None;
    }
    let pre = from_prereqs(srcs);
    let mut loops = all_loops(srcs, jq, pre.as_slice());
    // A semi-join loop yields at most one row per combination
    // before it (v0.17, `flatten`).
    #[cfg(feature = "flatten")]
    for l in loops.as_mut_slice() {
        if !l.auto && srcs.get(l.src).is_some_and(|s| s.semi) {
            l.l.out = l.l.out.min(0);
        }
    }
    let mx = mx_choice(srcs, loops.as_mut_slice());
    let mut disabled: AVec<bool> = AVec::new();
    for _ in loops.iter() {
        disabled.push(false);
    }
    let mut best = solve(srcs, jq, loops.as_slice(), disabled.as_slice(), mx, 0)?;
    if !jq.order.is_empty() {
        // The interstage heuristic: a table the first pass reads
        // through an equality is not swapped for one of its full
        // scans to deliver the order.
        for li in best.loops.iter() {
            let Some(l) = loops.as_slice().get(*li) else {
                break;
            };
            if l.auto || l.l.eq.is_empty() {
                break;
            }
            for (k, o) in loops.iter().enumerate() {
                let constrained =
                    o.auto || !o.l.eq.is_empty() || o.l.lo.is_some() || o.l.hi.is_some();
                if o.src == l.src
                    && !constrained
                    && let Some(d) = disabled.as_mut_slice().get_mut(k)
                {
                    *d = true;
                }
            }
        }
        let est = if best.nrow < 0 { 1 } else { best.nrow + 1 };
        best = solve(srcs, jq, loops.as_slice(), disabled.as_slice(), mx, est)?;
    }
    let mut out = AVec::new();
    for li in best.loops.iter() {
        out.push(loops.as_slice().get(*li)?.clone());
    }
    let mut distinct_ordered = false;
    let mut ob_sat = 0usize;
    if !jq.order.is_empty() {
        ob_sat = best.ordered.max(0) as usize;
        if jq.distinct_by && ob_sat >= jq.order.len() {
            distinct_ordered = true;
        }
    }
    let bloom_at = blooms(srcs, jq, out.as_slice());
    Some(JPlan {
        bloom_at,
        loops: out,
        rev: if jq.group { 0 } else { best.rev },
        ob_sat,
        distinct_ordered,
    })
}

/// The automatic index loop `jl` of source `js` at its place in the
/// path (`ready`: the sources before it), as the pin's
/// `constructAutomaticIndex` builds it: every usable `col = value`
/// term whose value the outer loops give, one per column, in
/// WHERE-clause order; the other columns the statement reads; the
/// partial form when a WHERE term constrains the table alone.
pub fn auto_index(js: &JSrc<'_>, jq: &JQuery<'_>, i: usize, ready: u64) -> crate::xplan::AutoIdx {
    let me = 1u64 << i;
    let mut drive: AVec<(usize, usize)> = AVec::new();
    for (ti, t) in js.ts.iter().enumerate() {
        if can_drive(js, t).is_some()
            && !js.derived.as_slice().get(ti).copied().unwrap_or(false)
            && js.tmask.as_slice().get(ti).copied().unwrap_or(0) & !ready == 0
        {
            drive.push((wc_pos(js, ti, jq.conj.len()), ti));
        }
    }
    drive.as_mut_slice().sort_by_key(|d| d.0);
    let mut keys: AVec<crate::xplan::AutoKey> = AVec::new();
    let mut taken: AVec<usize> = AVec::new();
    let mut bloom = true;
    for (_, ti) in drive.iter() {
        let Some(t) = js.ts.as_slice().get(*ti) else {
            continue;
        };
        let Some(c) = can_drive(js, t) else { continue };
        if taken.iter().any(|x| *x == c) {
            continue;
        }
        taken.push(c);
        if t.coll != crate::record::Coll::Binary {
            bloom = false;
        }
        keys.push(crate::xplan::AutoKey {
            col: c,
            coll: t.coll,
            val: t.val.clone(),
            aff: t.aff,
            is: matches!(t.op, TOp::Is),
        });
    }
    let mut extra = AVec::new();
    for (c, u) in js.used.iter().enumerate() {
        let pk = js.tab.wr && js.tab.notnull.get(c).copied().unwrap_or(false) && is_pk(js, c);
        if (*u || pk) && !taken.iter().any(|x| *x == c) && js.s.rowid_alias != Some(c) {
            extra.push(c);
        }
    }
    let partial = jq.conj.iter().enumerate().any(|(ci, c)| {
        let m = jq.cmask.get(ci).copied().unwrap_or(0);
        let pure = jq.pure.get(ci).copied().unwrap_or(false);
        let owner_ok = match jq.on_of.get(ci).copied().flatten() {
            Some(o) => js.left && o == i,
            None => !js.left,
        };
        m & !me == 0 && pure && owner_ok && !crate::query::has_sub(c)
    });
    crate::xplan::AutoIdx {
        keys,
        extra,
        partial,
        bloom,
    }
}

/// Whether column `c` is in a WITHOUT ROWID table's PRIMARY KEY.
fn is_pk(js: &JSrc<'_>, c: usize) -> bool {
    js.tab.indexes.iter().any(|m| {
        m.covers_all
            && m.keys
                .iter()
                .any(|k| matches!(k, IdxKey::Col(x) if *x == c))
    })
}
