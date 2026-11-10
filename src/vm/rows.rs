//! In-memory row lists, GROUP BY group sets and streamed aggregate
//! accumulators (ADR 0005 Stage 2, unit `vm_joins`; the ADR's
//! `Ephemeral` cursor kind and AggStep / AggFinal).
//!
//! They port today's executors' structures: a source's rows computed
//! as a whole (`session::stream`'s `Level::cache`, an access path's
//! rows, an automatic index's sorted rows and its Bloom filter, a
//! RIGHT / FULL source's rows with their match flags), DISTINCT's
//! rows seen so far, `project_qp_async`'s groups (formed in arrival
//! order under each GROUP BY term's collation, then sorted) and the
//! streamed fold (`stream::AggRun`) — so rows, their order, bare
//! columns and errors are exactly today's.

use crate::arena::AVec;
use crate::error::Error;
use crate::query::{AggKind, Ctx, Env, RExpr, RowData};
use crate::record::{Coll, OwnedValue, TextEnc};
use crate::session::QueryPlan;

/// A row of a row list: its values and rowid (the shape of
/// `session::stream`'s source rows, so the automatic-index helpers
/// read them as they are).
pub(crate) type ERow = (AVec<OwnedValue>, Option<i64>);

/// An in-memory row list, read from `at` up to `end`; each row has a
/// flag (a RIGHT / FULL source's "matched", a WHERE_BIGNULL_SORT
/// row's NULL key).
#[derive(Default)]
pub(crate) struct Eph {
    pub(crate) rows: AVec<ERow>,
    pub(crate) flags: AVec<bool>,
    pub(crate) at: usize,
    pub(crate) end: usize,
    /// Where the rows `EphReverse` / `EphNulls` reorder start.
    #[cfg_attr(not(feature = "expr_index"), allow(dead_code))]
    pub(crate) mark: usize,
    /// A Bloom filter over the rows' keys (empty: admits every key).
    #[cfg_attr(not(feature = "planner"), allow(dead_code))]
    pub(crate) bloom: AVec<u8>,
}

impl Eph {
    /// The current row.
    pub(crate) fn cur(&self) -> Option<&ERow> {
        if self.at < self.end {
            self.rows.as_slice().get(self.at)
        } else {
            None
        }
    }

    /// To the first row (`unmatched`: the first not flagged); false
    /// when there is none.
    pub(crate) fn rewind(&mut self, unmatched: bool) -> bool {
        self.end = self.rows.len();
        self.at = 0;
        if unmatched {
            self.skip_flagged();
        }
        self.at < self.end
    }

    /// To the next row; false past the last one.
    pub(crate) fn next(&mut self, unmatched: bool) -> bool {
        if self.at < self.end {
            self.at = self.at.saturating_add(1);
        }
        if unmatched {
            self.skip_flagged();
        }
        self.at < self.end
    }

    fn skip_flagged(&mut self) {
        while self.at < self.end && self.flags.as_slice().get(self.at).copied().unwrap_or(false) {
            self.at = self.at.saturating_add(1);
        }
    }

    /// Empty it.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.flags.clear();
        self.at = 0;
        self.end = 0;
        self.mark = 0;
    }

    pub(crate) fn push(&mut self, vals: AVec<OwnedValue>, rowid: Option<i64>, flag: bool) {
        self.rows.push((vals, rowid));
        self.flags.push(flag);
        self.end = self.rows.len();
    }

    /// Flag the current row (a RIGHT / FULL source's match).
    pub(crate) fn flag_current(&mut self) {
        if self.at < self.end
            && let Some(f) = self.flags.as_mut_slice().get_mut(self.at)
        {
            *f = true;
        }
    }

    /// The rows since the mark, reversed.
    #[cfg(feature = "expr_index")]
    pub(crate) fn reverse_tail(&mut self) {
        if let Some(t) = self.rows.as_mut_slice().get_mut(self.mark..) {
            t.reverse();
        }
        if let Some(t) = self.flags.as_mut_slice().get_mut(self.mark..) {
            t.reverse();
        }
    }

    /// The rows since the mark with the flagged ones first (or
    /// last), each part in its order.
    #[cfg(feature = "expr_index")]
    pub(crate) fn flagged_tail(&mut self, first: bool) {
        let mark = self.mark.min(self.rows.len());
        let mut tail: AVec<(ERow, bool)> = AVec::new();
        while self.rows.len() > mark {
            if let (Some(r), f) = (self.rows.pop(), self.flags.pop()) {
                tail.push((r, f.unwrap_or(false)));
            }
        }
        tail.as_mut_slice().reverse();
        let mut rest: AVec<(ERow, bool)> = AVec::new();
        for (r, f) in tail {
            if f == first {
                self.rows.push(r);
                self.flags.push(f);
            } else {
                rest.push((r, f));
            }
        }
        for (r, f) in rest {
            self.rows.push(r);
            self.flags.push(f);
        }
        self.end = self.rows.len();
    }

    /// Keep only the current row.
    #[cfg(feature = "planner")]
    pub(crate) fn keep_current(&mut self) {
        let at = self.at;
        let mut kept = None;
        for (i, r) in core::mem::take(&mut self.rows).into_iter().enumerate() {
            if i == at {
                kept = Some(r);
            }
        }
        self.flags.clear();
        if let Some(r) = kept {
            self.rows.push(r);
            self.flags.push(false);
        }
        self.at = 0;
        self.end = self.rows.len();
        self.mark = 0;
    }

    /// DISTINCT: whether `vals` equals a row seen before (under
    /// `colls`, NULLs equal, no affinity: `compound_rows_equal`);
    /// a new row is added.
    pub(crate) fn seen(&mut self, vals: &[OwnedValue], colls: &[Coll]) -> bool {
        let dup = self.rows.iter().any(|(r, _)| {
            r.len() == vals.len()
                && r.iter().zip(vals.iter()).enumerate().all(|(i, (x, y))| {
                    crate::query::values_equal_coll(x, y, colls.get(i).copied().unwrap_or_default())
                })
        });
        if !dup {
            let mut v = AVec::with_capacity(vals.len());
            v.extend_from_slice(vals);
            self.push(v, None, false);
        }
        dup
    }
}

/// The empty group's bare-column row: the outer row a subquery's
/// groups are formed under (`prefix`; none at the top level), then
/// NULLs for every source.
fn empty_rep(qp: &QueryPlan, prefix: &RowData) -> RowData {
    let mut r = prefix.clone();
    for sp in qp.sources.iter() {
        for _ in 0..sp.ncols {
            r.vals.push(OwnedValue::Null);
        }
        r.rowids.push(None);
    }
    r
}

/// Whether `qp`'s groups pick their bare-column row by a min() / max()
/// with a subquery in its argument or FILTER (`session::rep_minmax`,
/// `extremum_rep`): code searches it, `RepStep` by `RepStep`.
#[cfg(feature = "sql")]
pub(crate) fn sub_rep(qp: &QueryPlan) -> bool {
    crate::session::rep_minmax(
        qp.items.as_slice(),
        qp.having.as_ref(),
        qp.order_by.as_slice(),
    )
    .is_some_and(|m| crate::query::has_sub(m.arg) || m.filter.is_some_and(crate::query::has_sub))
}

/// A GROUP BY's groups (`project_qp_async`'s grouping): each group's
/// key (its first row's values) and rows, in arrival order; one group
/// of every row without a GROUP BY.
pub(crate) struct Groups {
    /// The `P4::Plan` entry it was opened with.
    pub(crate) info: u32,
    groups: AVec<(AVec<OwnedValue>, AVec<RowData>)>,
    gcolls: AVec<Coll>,
    gdesc: AVec<bool>,
    /// The next group `next` moves to.
    at: usize,
    /// The current group's index (`usize::MAX`: none yet).
    cur: usize,
    /// The current group's bare-column row (`session::pick_rep_from`).
    pub(crate) rep: RowData,
    /// The outer row of a subquery's groups (`empty_rep`).
    prefix: RowData,
    /// The current group's row `GroupRows` / `GroupRowNext` read.
    #[cfg_attr(not(feature = "sql"), allow(dead_code))]
    ri: usize,
    /// The bare-column row is searched by code (`RepStep`): the min()
    /// / max() that picks it has a subquery in its argument or FILTER
    /// (`session::extremum_rep`). `next` then starts the search at
    /// the group's first row, `best` its extremum so far.
    #[cfg(feature = "sql")]
    sub_rep: bool,
    #[cfg(feature = "sql")]
    best: Option<OwnedValue>,
}

impl Groups {
    pub(crate) fn new(info: u32, qp: &QueryPlan, prefix: RowData) -> Groups {
        let mut groups = AVec::new();
        if qp.group_by.is_empty() {
            groups.push((AVec::new(), AVec::new()));
        }
        Groups {
            info,
            groups,
            gcolls: crate::query::exprs_colls(qp.group_by.as_slice()),
            gdesc: crate::session::group_desc(
                qp.group_by.as_slice(),
                qp.items.as_slice(),
                qp.order_by.as_slice(),
            ),
            at: 0,
            cur: usize::MAX,
            rep: RowData::empty(),
            prefix,
            ri: 0,
            #[cfg(feature = "sql")]
            sub_rep: sub_rep(qp),
            #[cfg(feature = "sql")]
            best: None,
        }
    }

    /// One row of the code's bare-column search (`RepStep`,
    /// `session::extremum_rep`): `v` is the argument's value for the
    /// current group row; the new extremum makes that row the group's
    /// bare-column row.
    #[cfg(feature = "sql")]
    pub(crate) fn rep_step(&mut self, kind: AggKind, coll: Coll, v: OwnedValue, enc: TextEnc) {
        if crate::session::extremum_step(kind, coll, &mut self.best, v, enc)
            && let Some(r) = self.rows().get(self.ri)
        {
            self.rep = r.clone();
        }
    }

    /// The current group's row `ri` (`first`: its first row; else
    /// the next one), or none past its last.
    #[cfg(feature = "sql")]
    pub(crate) fn row(&mut self, first: bool) -> Option<&RowData> {
        self.ri = if first { 0 } else { self.ri.saturating_add(1) };
        self.rows().get(self.ri)
    }

    /// Add `row` to the group its GROUP BY values `key` name.
    pub(crate) fn add(&mut self, qp: &QueryPlan, row: &RowData, key: &[OwnedValue]) {
        if qp.group_by.is_empty() {
            if let Some(g) = self.groups.as_mut_slice().first_mut() {
                g.1.push(row.clone());
            }
            return;
        }
        let colls = self.gcolls.as_slice();
        let found = self.groups.iter().position(|(k, _)| {
            k.len() == key.len()
                && k.iter()
                    .zip(key.iter())
                    .zip(colls.iter())
                    .all(|((a, b), c)| crate::query::values_equal_coll(a, b, *c))
        });
        match found.and_then(|i| self.groups.as_mut_slice().get_mut(i)) {
            Some(g) => g.1.push(row.clone()),
            None => {
                let mut k = AVec::with_capacity(key.len());
                k.extend_from_slice(key);
                let mut rows = AVec::new();
                rows.push(row.clone());
                self.groups.push((k, rows));
            }
        }
    }

    /// The groups in GROUP BY order (unless the rows arrived in it).
    pub(crate) fn sort(&mut self, qp: &QueryPlan, enc: TextEnc) {
        if qp.group_by.is_empty() || crate::session::groups_arrive_ordered(qp.sources.as_slice()) {
            return;
        }
        crate::session::sort_groups(
            self.groups.as_mut_slice(),
            self.gcolls.as_slice(),
            self.gdesc.as_slice(),
            enc,
        );
    }

    /// To the next group, its bare-column row computed; false when
    /// there is none.
    pub(crate) fn next(&mut self, qp: &QueryPlan, env: &Env) -> Result<bool, Error<()>> {
        let Some((_, rows)) = self.groups.as_slice().get(self.at) else {
            self.cur = usize::MAX;
            return Ok(false);
        };
        #[cfg(feature = "sql")]
        if self.sub_rep {
            self.best = None;
            self.rep = match rows.as_slice().first() {
                Some(r) => r.clone(),
                None => empty_rep(qp, &self.prefix),
            };
            self.cur = self.at;
            self.at = self.at.saturating_add(1);
            return Ok(true);
        }
        self.rep = crate::session::pick_rep_from::<()>(
            env,
            qp.items.as_slice(),
            qp.having.as_ref(),
            qp.order_by.as_slice(),
            rows.as_slice(),
            &empty_rep(qp, &self.prefix),
        )?;
        self.cur = self.at;
        self.at = self.at.saturating_add(1);
        Ok(true)
    }

    /// Make group `i` current again with its bare-column row `rep` (a
    /// window unit's group, `WinNext`).
    #[cfg(feature = "window")]
    pub(crate) fn seek(&mut self, i: usize, rep: RowData) {
        self.cur = i;
        self.rep = rep;
    }

    /// The current group's index.
    #[cfg(feature = "window")]
    pub(crate) fn current(&self) -> usize {
        self.cur
    }

    /// The current group's rows.
    pub(crate) fn rows(&self) -> &[RowData] {
        self.groups
            .as_slice()
            .get(self.cur)
            .map_or(&[], |g| g.1.as_slice())
    }
}

/// One streamed aggregate's accumulator (`stream::Acc`).
enum Acc {
    Count(i64),
    Sum(crate::agg::SumCtx),
    Ext(Option<OwnedValue>),
}

/// A streamed aggregate query's state (`stream::AggRun`): each
/// aggregate of the result columns with its accumulator, and the
/// bare columns' row.
pub(crate) struct AggRun {
    aggs: AVec<RExpr>,
    accs: AVec<Acc>,
    /// The min / max that picks the bare columns' row
    /// (`session::rep_minmax`): kind, argument, FILTER, collation.
    mm: Option<(AggKind, RExpr, Option<RExpr>, Coll)>,
    first: Option<RowData>,
    rep: Option<RowData>,
    best: Option<OwnedValue>,
    /// The outer row of a subquery's aggregate (`empty_rep`).
    prefix: RowData,
}

/// Whether a query's aggregates fold row by row (`stream_build`'s
/// rule): a FROM clause (a query without one is materialised today),
/// count / sum / total / avg / min / max over one argument or `*`, no
/// DISTINCT, ORDER BY or GROUP BY, in a UTF-8 database.
pub(crate) fn folds(qp: &QueryPlan, utf8: bool) -> bool {
    let mut aggs: AVec<&RExpr> = AVec::new();
    for it in qp.items.iter() {
        crate::query::collect_outer_aggs(it, &mut aggs);
    }
    utf8 && !qp.sources.is_empty()
        && !qp.items.iter().any(crate::query::has_sub)
        && qp.group_by.is_empty()
        && qp.having.is_none()
        && qp.order_by.is_empty()
        && !qp.distinct
        && aggs.iter().all(|a| {
            crate::query::agg_parts(a).is_some_and(|p| {
                matches!(
                    p.kind,
                    AggKind::Count
                        | AggKind::Sum
                        | AggKind::Total
                        | AggKind::Avg
                        | AggKind::Min
                        | AggKind::Max
                ) && !p.distinct
                    && p.order.is_empty()
                    && (p.star || p.args.len() == 1)
            })
        })
}

impl AggRun {
    pub(crate) fn new(qp: &QueryPlan, prefix: RowData) -> AggRun {
        let mut aggs: AVec<&RExpr> = AVec::new();
        for it in qp.items.iter() {
            crate::query::collect_outer_aggs(it, &mut aggs);
        }
        let mm = crate::session::rep_minmax(qp.items.as_slice(), None, &[]).map(|m| {
            (
                m.kind,
                m.arg.clone(),
                m.filter.cloned(),
                crate::query::expr_coll(m.arg).unwrap_or_default(),
            )
        });
        let mut run = AggRun {
            aggs: AVec::new(),
            accs: AVec::new(),
            mm,
            first: None,
            rep: None,
            best: None,
            prefix,
        };
        for a in aggs.iter() {
            run.aggs.push((*a).clone());
            run.accs
                .push(match crate::query::agg_parts(a).map(|p| p.kind) {
                    Some(AggKind::Count) => Acc::Count(0),
                    Some(AggKind::Min | AggKind::Max) => Acc::Ext(None),
                    _ => Acc::Sum(Default::default()),
                });
        }
        run
    }

    /// Fold the joined row `row` (`stream::agg_step`); whether it kept
    /// the row (as the bare columns' row).
    pub(crate) fn step(&mut self, row: &RowData, env: &Env) -> Result<bool, Error<()>> {
        let enc = env.text_enc();
        let ctx = Ctx::of(row).with(env);
        for (a, st) in self.aggs.iter().zip(self.accs.as_mut_slice().iter_mut()) {
            let Some(p) = crate::query::agg_parts(a) else {
                continue;
            };
            if let Some(f) = p.filter
                && !crate::query::cond::<()>(f, &ctx)?
            {
                continue;
            }
            let v = match p.args.first() {
                Some(e) if !p.star => crate::query::eval::<()>(e, &ctx)?,
                _ => OwnedValue::Integer(1),
            };
            match st {
                Acc::Count(n) => {
                    if !matches!(v, OwnedValue::Null) {
                        *n = n.saturating_add(1);
                    }
                }
                Acc::Sum(c) => c.step(&v),
                Acc::Ext(best) => {
                    if matches!(v, OwnedValue::Null) {
                        continue;
                    }
                    let take = match best {
                        None => true,
                        Some(b) => {
                            let o = crate::record::compare_coll_in(
                                &v.as_value(),
                                &b.as_value(),
                                crate::query::args_coll(p.args),
                                enc,
                            );
                            if p.kind == AggKind::Min {
                                o == core::cmp::Ordering::Less
                            } else {
                                o == core::cmp::Ordering::Greater
                            }
                        }
                    };
                    if take {
                        *best = Some(v);
                    }
                }
            }
        }
        let mut kept = false;
        if self.first.is_none() {
            self.first = Some(row.clone());
            kept = true;
        }
        if let Some((kind, arg, filter, coll)) = &self.mm {
            let pass = match filter {
                Some(f) => crate::query::cond::<()>(f, &ctx)?,
                None => true,
            };
            if pass {
                let v = crate::query::eval::<()>(arg, &ctx)?;
                if crate::session::extremum_step(*kind, *coll, &mut self.best, v, enc) {
                    self.rep = Some(row.clone());
                    kept = true;
                }
            }
        }
        Ok(kept)
    }

    /// The result columns (`stream::stream_finish`'s fold).
    pub(crate) fn finish(
        &self,
        qp: &QueryPlan,
        env: &Env,
        out: &mut [OwnedValue],
    ) -> Result<(), Error<()>> {
        let empty = empty_rep(qp, &self.prefix);
        let rep = self.rep.as_ref().or(self.first.as_ref()).unwrap_or(&empty);
        let mut vals = AVec::new();
        for (a, st) in self.aggs.iter().zip(self.accs.iter()) {
            let kind = crate::query::agg_parts(a).map_or(AggKind::Count, |p| p.kind);
            vals.push(match st {
                Acc::Count(n) => OwnedValue::Integer(*n),
                Acc::Sum(c) => c.finish(kind).map_err(|kind| Error::Schema { kind })?,
                Acc::Ext(b) => b.clone().unwrap_or(OwnedValue::Null),
            });
        }
        let mut at = 0usize;
        for (it, slot) in qp.items.iter().zip(out.iter_mut()) {
            let mut pos = 0;
            let rest = vals.as_slice().get(at..).unwrap_or(&[]);
            let mut used = 0;
            let e = crate::query::substitute_group(it, &[], &mut pos, rest, &mut used);
            at = at.saturating_add(used);
            *slot = crate::query::eval::<()>(&e, &Ctx::group(&[], rep).with(env))?;
        }
        Ok(())
    }
}

/// A recursive CTE's queue (`session::exec_recursive`): the pin's
/// FIFO, or with an ORDER BY a priority queue with ties in arrival
/// order (`session::RecQueue`, whose state this holds between
/// opcodes), and under UNION every row that entered it, sorted, so
/// an equal row is dropped.
#[cfg(feature = "sql")]
#[derive(Default)]
pub(crate) struct Queue {
    /// The `P4::Queue` entry it was opened with.
    pub(crate) info: u32,
    rows: AVec<(u64, AVec<OwnedValue>)>,
    head: usize,
    seq: u64,
    seen: AVec<AVec<OwnedValue>>,
}

#[cfg(feature = "sql")]
impl Queue {
    pub(crate) fn new(info: u32) -> Queue {
        Queue {
            info,
            ..Default::default()
        }
    }

    /// `f` over the queue as `session::RecQueue`.
    fn with<R>(
        &mut self,
        info: &super::op::QueueInfo,
        enc: TextEnc,
        f: impl FnOnce(&mut crate::session::RecQueue<'_>) -> R,
    ) -> R {
        let mut q = crate::session::RecQueue {
            order: info.order.as_slice(),
            enc,
            rows: core::mem::take(&mut self.rows),
            head: self.head,
            seq: self.seq,
        };
        let r = f(&mut q);
        self.rows = q.rows;
        self.head = q.head;
        self.seq = q.seq;
        r
    }

    /// Add `row` unless UNION saw an equal one (the compound's column
    /// collations, `exec_recursive`'s `admit`).
    pub(crate) fn add(&mut self, info: &super::op::QueueInfo, row: &[OwnedValue], enc: TextEnc) {
        if let Some(colls) = &info.union {
            let at = self.seen.as_slice().binary_search_by(|r| {
                crate::session::compound_key_cmp(r.as_slice(), row, colls.as_slice(), enc)
            });
            match at {
                Ok(_) => return,
                Err(at) => {
                    let mut v = AVec::with_capacity(row.len());
                    v.extend_from_slice(row);
                    self.seen.insert_at(at, v);
                }
            }
        }
        let mut v = AVec::with_capacity(row.len());
        v.extend_from_slice(row);
        self.with(info, enc, |q| q.push(v));
    }

    /// The next row out.
    pub(crate) fn pop(
        &mut self,
        info: &super::op::QueueInfo,
        enc: TextEnc,
    ) -> Option<AVec<OwnedValue>> {
        self.with(info, enc, |q| q.pop())
    }
}
