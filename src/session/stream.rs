//! Streamed query execution (v0.17, `sql`; virtual-table cursors
//! with leaf `vtab`): a top-level SELECT hands its rows to
//! `Statement::step` one at a time, as the pin's VDBE does, instead
//! of materialising its result in its first step.
//!
//! - **The join** is the nested loop of `join_sources`, unrolled
//!   into a resumable state machine (`Stream::depth`, one `Level`
//!   per FROM source): the first source is read through its cursor
//!   (a table's b-tree cursor, a virtual table's xFilter / xNext /
//!   xEof / xColumn / xRowid), each inner source is opened again for
//!   every combination of the sources before it — a virtual table's
//!   filter re-evaluated with the outer row's values, as the pin's
//!   xFilter — and a source whose rows hipp computes as a whole (a
//!   derived table, a TEMP table, an index lookup, a JSON
//!   table-valued function, a recursive CTE, an inner dbstat / fts5
//!   source reading nothing to its left) is computed the first time
//!   the join reaches it — an endless recursive CTE read first
//!   through a WHERE or a join under a LIMIT under a bound that
//!   doubles as the join reads it (`Stream::grow`). Rows come in
//!   `join_sources`' order; LEFT JOIN pads, the WHERE clause's
//!   constant terms first and the lateral pre-checks (`lateral_ok`)
//!   as there. RIGHT / FULL joins, window functions and compounds
//!   keep the materialising executor.
//! - **Side tables.** The JSON documents and full-text rows a row's
//!   values point into (`Env::json_docs`, `Env::fts_rows`) go when
//!   the loop that added them ends, or once the row is dealt with,
//!   unless a cached source or a kept row reads them
//!   (`Stream::pinned`): a streamed join's memory does not grow with
//!   its outer rows.
//! - **What follows the join.** A plain plan projects each row as
//!   the join makes it (OFFSET rows are skipped unprojected, LIMIT
//!   stops the join); DISTINCT keeps the rows seen so far and emits
//!   each new one; an aggregate without GROUP BY whose aggregates
//!   are count / sum / total / avg / min / max (no DISTINCT or
//!   subquery; the planner drops such a query's ORDER BY, as the pin)
//!   folds row by row in constant memory; the blocking
//!   operators — ORDER BY (except a table's own rowid order), GROUP
//!   BY, the other aggregates — buffer what they need, as the pin's
//!   sorters do, and finish through the materialising executor's
//!   projection.
//! - A row that fails part-way fails the statement after the rows
//!   before it were returned, as on the pin.

use super::*;
use crate::query::AggKind;

/// `(column values, rowid)` rows of one source.
type SrcRows = AVec<(AVec<OwnedValue>, Option<i64>)>;

/// Where a source's rows come from for the current combination of
/// the sources before it.
enum Src {
    /// Not open.
    Idle,
    /// A table's b-tree, read row by row (the first source).
    Scan(ScanCursor),
    /// `Level::cache`, replayed from the index given.
    Cached(usize),
    /// Rows computed for this combination, and the next one.
    #[cfg(any(feature = "json", feature = "vtab", feature = "expr_index"))]
    Rows(SrcRows, usize),
    /// A virtual table's cursor.
    #[cfg(feature = "vtab")]
    Vt(super::vtab::VtCur),
    /// `Level::cache` from the first index given to the second: an
    /// automatic index's span for this combination (v0.17,
    /// `planner`).
    #[cfg(feature = "planner")]
    Span(usize, usize),
}

/// One FROM source's loop.
struct Level {
    src: Src,
    /// The rows of a source computed once per execution.
    cache: Option<SrcRows>,
    /// A row of this combination passed the ON clause.
    any: bool,
    /// The LEFT JOIN pad was made (or must not be).
    padded: bool,
    /// Rows its cursor gave since it opened.
    count: usize,
    /// The side tables' lengths when it opened (`side_len`): what
    /// its loop added goes when it ends (`side_drop`).
    side: (usize, usize),
    /// The values a lateral table-valued function's call cleared
    /// (`SourcePlan::tvf_dead`), put back when its loop ends.
    #[cfg(any(feature = "json", feature = "vtab"))]
    saved: AVec<(usize, OwnedValue)>,
    /// An automatic index's Bloom filter (v0.17, `planner`),
    /// built with `cache`.
    #[cfg(feature = "planner")]
    bloom: AVec<u8>,
    /// A lookup loop's Bloom filter (`bloom`) is built.
    #[cfg(feature = "planner")]
    bloom_built: bool,
    /// A semi-join source's loop made a joined row (v0.17,
    /// `planner`): it ends at its next row.
    #[cfg(feature = "planner")]
    done: bool,
}

/// What the stream does with each joined row.
enum Mode {
    /// Project and return it.
    Plain,
    /// Project it; return it unless an equal row came before.
    Distinct(AVec<AVec<OwnedValue>>),
    /// Fold it into the aggregates.
    Agg(Box<AggRun>),
    /// Keep it for the grouping projection at the end.
    Buffer(AVec<RowData>),
    /// Project it and its ORDER BY keys for the sort at the end.
    Sort(AVec<Produced>),
}

/// A streamed aggregate query's state: each aggregate of the
/// result items (`collect_outer_aggs` order) with its accumulator,
/// and the bare columns' row (`pick_rep_from`'s rule).
struct AggRun {
    aggs: AVec<RExpr>,
    accs: AVec<Acc>,
    /// The lone min / max that picks the bare columns' row
    /// (`rep_minmax`): kind, argument, FILTER, collation.
    mm: Option<MinMax>,
    first: Option<RowData>,
    rep: Option<RowData>,
    best: Option<OwnedValue>,
}

/// `AggRun::mm`.
type MinMax = (AggKind, RExpr, Option<RExpr>, crate::record::Coll);

/// One streamed aggregate's accumulator (`agg::fold`'s rules for
/// these kinds, step by step).
enum Acc {
    Count(i64),
    Sum(crate::agg::SumCtx),
    Ext(Option<OwnedValue>),
}

/// A streamed top-level query in progress (`Statement::stream`).
pub(super) struct Stream {
    /// The statement's plan, shared (not copied) for this execution.
    qp: alloc::rc::Rc<QueryPlan>,
    levels: AVec<Level>,
    /// Where each source's values start in the joined row.
    bases: AVec<usize>,
    /// The result column each ORDER BY term is (`order_cols`).
    cols: AVec<Option<usize>>,
    /// The source whose loop runs next (`levels.len()`: the row is
    /// complete).
    depth: usize,
    acc: RowData,
    buf: AVec<u8>,
    mode: Mode,
    /// The row window still to apply: rows to skip, rows to return.
    skip: usize,
    take: usize,
    /// The statement's window, for the blocking modes' finish.
    window: (usize, usize),
    /// The bound on the first source's rows (`derived_need`).
    need0: Option<usize>,
    /// The first source's growing bound (0: none; `need0` then
    /// holds it): when the join has read all of the rows computed
    /// under it and there were that many, they are computed again
    /// under twice the bound and the join goes on after the rows it
    /// read.
    grow: usize,
    /// The plan holds a subquery (the async evaluators run).
    sub: bool,
    /// The mode keeps every row it is given (DISTINCT, GROUP BY, an
    /// ORDER BY without LIMIT): a generate_series cursor may give it
    /// at most `Config::max_vtab_rows` rows.
    capped: bool,
    /// The join is exhausted.
    ended: bool,
    /// The JSON documents and full-text rows held when the stream
    /// opened (`Statement::step` drops later ones at its end).
    #[cfg_attr(not(any(feature = "json", feature = "fts5")), allow(dead_code))]
    pub(super) marks: (usize, usize),
    /// The side tables' lengths below which entries stay until the
    /// stream ends: what a cached source's rows or a kept row (an
    /// aggregate's bare-column row, a buffered row) may point into.
    pinned: (usize, usize),
    /// The side tables' lengths when the current row was complete:
    /// what its WHERE and result columns added goes after it.
    row_side: (usize, usize),
}

/// Whether `p` folds row by row (`Acc`).
fn agg_streams(p: &crate::query::AggParts<'_>) -> bool {
    matches!(
        p.kind,
        AggKind::Count | AggKind::Sum | AggKind::Total | AggKind::Avg | AggKind::Min | AggKind::Max
    ) && !p.distinct
        && p.order.is_empty()
        && (p.star || p.args.len() == 1)
}

/// Whether ORDER BY `order` is source `sp`'s own row order: the
/// order of the index or rowid loop that reads it, when that loop
/// delivers every term; else its rowid (or INTEGER PRIMARY KEY),
/// ascending, over a table read in rowid order.
fn rowid_order(sp: &SourcePlan, order: &[(RExpr, crate::sql::SortOrder)]) -> bool {
    // An index (or rowid) loop that delivers every term (v0.16
    // `expr_index`'s access planner).
    #[cfg(feature = "expr_index")]
    if let Some(a) = sp.access.as_deref() {
        return !a.lateral && a.sorted >= order.len();
    }
    let [(e, o)] = order else {
        return false;
    };
    let plain = sp.derived.is_none() && !sp.temp;
    #[cfg(feature = "expr_index")]
    let plain = plain && sp.access.is_none();
    plain
        && !o.desc
        && match e {
            RExpr::Rowid(0) => true,
            RExpr::Column(slot, ..) => sp.rowid_alias.is_some_and(|a| a == *slot),
            _ => false,
        }
}

impl<S: Storage> Statement<'_, S> {
    /// The stream of the statement's query plan, when it streams
    /// (see the module comment); None for the materialising
    /// executor. Evaluates LIMIT / OFFSET and the WHERE clause's
    /// constant terms, as the materialising executor does first.
    pub(super) async fn stream_open(&mut self) -> Result<Option<Box<Stream>>, S::Error> {
        let Some(qp) = self.stream_plan() else {
            return Ok(None);
        };
        let window = self.limit_window(&qp.limit, &qp.offset).await?;
        self.stream_build(qp, window)
    }

    /// The statement's query plan when it streams (sync part of
    /// `stream_open`, ADR 0005 Stage 1).
    #[inline(never)]
    fn stream_plan(&self) -> Option<alloc::rc::Rc<QueryPlan>> {
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        if self.conn.in_program() {
            return None;
        }
        let Plan::Query(qp) = &self.plan else {
            return None;
        };
        if qp.sources.is_empty()
            || !qp.compound.is_empty()
            || !qp.values.is_empty()
            || qp.sources.iter().any(|s| s.kind.pads_left() || s.rec != 0)
        {
            return None;
        }
        #[cfg(feature = "window")]
        if !crate::window::calls(qp.items.as_slice(), qp.order_by.as_slice()).is_empty() {
            return None;
        }
        Some(alloc::rc::Rc::clone(qp))
    }

    /// Set up the stream of `qp` over the row window `window` (sync
    /// part of `stream_open`, ADR 0005 Stage 1).
    #[inline(never)]
    fn stream_build(
        &mut self,
        qp: alloc::rc::Rc<QueryPlan>,
        window: (usize, usize),
    ) -> Result<Option<Box<Stream>>, S::Error> {
        // An endless recursive CTE read first through a WHERE or a
        // join under a LIMIT: its rows are computed under a bound
        // that doubles whenever the join has read them all
        // (`Stream::grow`; the materialising executor's growing
        // bound, the pin's co-routine stopped at the LIMIT).
        let grow = if streams_first(
            qp.sources.as_slice(),
            qp.group_by.is_empty() && !qp.has_agg && qp.order_by.is_empty() && !qp.distinct,
        ) && (qp.filter.is_some() || qp.sources.len() > 1)
            && window.1 != usize::MAX
        {
            window.0.saturating_add(window.1).max(1)
        } else {
            0
        };
        let need0 = if grow != 0 {
            Some(grow)
        } else {
            derived_need(
                qp.sources.as_slice(),
                qp.filter.is_some() || !qp.group_by.is_empty() || qp.has_agg,
                !qp.order_by.is_empty() || qp.distinct,
                window,
            )
        };
        let sub = query_plan_has_sub(&qp);
        let utf8 = self.conn.pager.text_enc() == crate::record::TextEnc::Utf8;
        let mode = if qp.has_agg || !qp.group_by.is_empty() {
            let mut aggs: AVec<&RExpr> = AVec::new();
            for it in qp.items.iter() {
                crate::query::collect_outer_aggs(it, &mut aggs);
            }
            let fold = utf8
                && qp.group_by.is_empty()
                && qp.having.is_none()
                && qp.order_by.is_empty()
                && !qp.distinct
                && !qp.items.iter().any(crate::query::has_sub)
                && aggs
                    .iter()
                    .all(|a| crate::query::agg_parts(a).is_some_and(|p| agg_streams(&p)));
            if fold {
                let mm = rep_minmax(qp.items.as_slice(), None, &[]).map(|m| {
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
                Mode::Agg(Box::new(run))
            } else {
                Mode::Buffer(AVec::new())
            }
        } else if !qp.order_by.is_empty()
            && !(qp.sources.len() == 1
                && !qp.distinct
                && qp
                    .sources
                    .as_slice()
                    .first()
                    .is_some_and(|s| rowid_order(s, qp.order_by.as_slice())))
        {
            Mode::Sort(AVec::new())
        } else if qp.distinct {
            Mode::Distinct(AVec::new())
        } else {
            Mode::Plain
        };
        let mut levels = AVec::new();
        let mut bases = AVec::new();
        let mut w = 0usize;
        for sp in qp.sources.iter() {
            bases.push(w);
            w = w.saturating_add(sp.ncols);
            levels.push(Level {
                src: Src::Idle,
                cache: None,
                any: false,
                padded: false,
                count: 0,
                side: (0, 0),
                #[cfg(any(feature = "json", feature = "vtab"))]
                saved: AVec::new(),
                #[cfg(feature = "planner")]
                bloom: AVec::new(),
                #[cfg(feature = "planner")]
                bloom_built: false,
                #[cfg(feature = "planner")]
                done: false,
            });
        }
        #[cfg(feature = "json")]
        let jm = self.conn.env.json_docs.try_borrow().map_or(0, |d| d.len());
        #[cfg(not(feature = "json"))]
        let jm = 0;
        #[cfg(feature = "fts5")]
        let fm = self.conn.env.fts_rows.try_borrow().map_or(0, |d| d.len());
        #[cfg(not(feature = "fts5"))]
        let fm = 0;
        // LIMIT 0 reads nothing; constant WHERE terms that fail join
        // nothing (an aggregate still has its empty group).
        let ended = window.1 == 0
            || !crate::query::join_constants_hold::<S::Error>(
                qp.filter.as_ref(),
                qp.sources.as_slice(),
                &Ctx::of(&RowData::empty()).with(&self.conn.env),
            )?;
        let capped = match mode {
            Mode::Distinct(_) | Mode::Buffer(_) => true,
            Mode::Sort(_) => qp.distinct || window.1 == usize::MAX,
            _ => false,
        };
        self.conn.stream_left = Some((jm, fm));
        Ok(Some(Box::new(Stream {
            cols: order_cols(qp.order_by.as_slice(), qp.items.as_slice()),
            qp,
            levels,
            bases,
            depth: 0,
            acc: RowData::empty(),
            buf: AVec::new(),
            mode,
            skip: window.0,
            take: window.1,
            window,
            need0,
            grow,
            sub,
            capped,
            ended,
            marks: (jm, fm),
            pinned: (jm, fm),
            row_side: (jm, fm),
        })))
    }

    /// The stream's next row into `current` / `current_rowid`
    /// (true), or false once it has no more — a blocking mode's rows
    /// are then in `query_rows`.
    pub(super) async fn stream_step(&mut self, s: &mut Stream) -> Result<bool, S::Error> {
        loop {
            if matches!(s.mode, Mode::Plain | Mode::Distinct(_)) && s.take == 0 {
                return Ok(false);
            }
            if !self.join_next(s).await? {
                self.stream_finish(s).await?;
                return Ok(false);
            }
            let rowid = if s.qp.sources.len() == 1 {
                s.acc.rowids.as_slice().first().copied().flatten()
            } else {
                None
            };
            match &mut s.mode {
                Mode::Plain => {
                    if s.skip > 0 {
                        s.skip -= 1;
                        self.side_drop(s.row_side, s.pinned);
                        continue;
                    }
                    let vals = self.stream_items(s.qp.items.as_slice(), &s.acc).await?;
                    self.side_drop(s.row_side, s.pinned);
                    self.current = vals;
                    self.current_rowid = rowid;
                    s.take -= 1;
                    return Ok(true);
                }
                Mode::Distinct(seen) => {
                    let vals = self.stream_items(s.qp.items.as_slice(), &s.acc).await?;
                    self.side_drop(s.row_side, s.pinned);
                    let colls = crate::query::exprs_colls(s.qp.items.as_slice());
                    if seen.iter().any(|k| {
                        k.len() == vals.len()
                            && compound_rows_equal(k.as_slice(), vals.as_slice(), colls.as_slice())
                    }) {
                        continue;
                    }
                    seen.push(vals.clone());
                    if s.skip > 0 {
                        s.skip -= 1;
                        continue;
                    }
                    self.current = vals;
                    self.current_rowid = rowid;
                    s.take -= 1;
                    return Ok(true);
                }
                Mode::Agg(run) => {
                    if self.agg_step(run, &s.acc)? {
                        s.pinned = self.side_len();
                    } else {
                        self.side_drop(s.row_side, s.pinned);
                    }
                }
                Mode::Buffer(rows) => {
                    rows.push(s.acc.clone());
                    s.pinned = self.side_len();
                }
                Mode::Sort(out) => {
                    let vals = self.stream_items(s.qp.items.as_slice(), &s.acc).await?;
                    let mut keys = AVec::new();
                    for (e, _) in s.qp.order_by.iter() {
                        keys.push(self.eval_row_sub(e, &s.acc).await?);
                    }
                    shown_keys(keys.as_mut_slice(), s.cols.as_slice(), vals.as_slice());
                    self.side_drop(s.row_side, s.pinned);
                    out.push(Produced {
                        out: vals,
                        keys,
                        rowid,
                    });
                    // An ORDER BY with a LIMIT keeps the rows it may
                    // still return (the pin's sorter does): sorted
                    // stably, so the earliest of equal rows stay.
                    let k = s.window.0.saturating_add(s.window.1);
                    if !s.capped && out.len() >= k.saturating_mul(2).saturating_add(64) {
                        let enc = self.conn.pager.text_enc();
                        crate::arena::sort_by(out.as_mut_slice(), &mut |x, y| {
                            crate::query::order_keys_cmp(
                                x.keys.as_slice(),
                                y.keys.as_slice(),
                                s.qp.order_by.iter().map(|(_, o)| *o),
                                enc,
                            )
                        });
                        out.truncate(k);
                    }
                }
            }
        }
    }

    /// The result items for the joined row `row`.
    async fn stream_items(
        &mut self,
        items: &[RExpr],
        row: &RowData,
    ) -> Result<AVec<OwnedValue>, S::Error> {
        let mut vals = AVec::new();
        for it in items.iter() {
            vals.push(self.eval_row_sub(it, row).await?);
        }
        Ok(vals)
    }

    /// The join's next complete row that passes the WHERE clause,
    /// in `s.acc`; false once every combination was tried.
    async fn join_next(&mut self, s: &mut Stream) -> Result<bool, S::Error> {
        let n = s.levels.len();
        loop {
            if s.ended {
                return Ok(false);
            }
            if s.depth >= n {
                // Back to the innermost loop for its next row.
                s.depth = n.saturating_sub(1);
                s.row_side = self.side_len();
                let ok = match &s.qp.filter {
                    None => true,
                    Some(f) if s.sub => self.where_row_sub(f, &s.acc).await?,
                    Some(f) => {
                        crate::query::cond::<S::Error>(f, &Ctx::of(&s.acc).with(&self.conn.env))?
                    }
                };
                if ok {
                    // Every semi-join loop has its row (v0.17,
                    // `planner`).
                    #[cfg(feature = "planner")]
                    for (lv, sp) in s.levels.as_mut_slice().iter_mut().zip(s.qp.sources.iter()) {
                        if sp.exists {
                            lv.done = true;
                        }
                    }
                    return Ok(true);
                }
                self.side_drop(s.row_side, s.pinned);
                continue;
            }
            let d = s.depth;
            let base = s.bases.as_slice().get(d).copied().unwrap_or(0);
            // A growing first source (`Stream::grow`) the join has
            // read to the end of: its rows again under twice the
            // bound, the join going on after those it read.
            let mut resume = None;
            if d == 0
                && s.grow != 0
                && let Some(lv) = s.levels.as_mut_slice().first_mut()
                && let (Src::Cached(p), Some(r)) = (&lv.src, &lv.cache)
                && *p >= r.len()
                && r.len() >= s.grow
            {
                resume = Some(*p);
                lv.src = Src::Idle;
                lv.cache = None;
                s.grow = s.grow.saturating_mul(2);
                s.need0 = Some(s.grow);
            }
            if matches!(s.levels.as_slice().get(d).map(|l| &l.src), Some(Src::Idle)) {
                let cached = s
                    .levels
                    .as_slice()
                    .get(d)
                    .is_some_and(|l| l.cache.is_some());
                self.level_open(s, d).await?;
                // A source's rows computed once stay for the stream.
                if !cached
                    && s.levels
                        .as_slice()
                        .get(d)
                        .is_some_and(|l| l.cache.is_some())
                {
                    s.pinned = self.side_len();
                }
            }
            if let Some(p) = resume
                && let Some(lv) = s.levels.as_mut_slice().first_mut()
            {
                lv.src = Src::Cached(p);
            }
            s.acc.vals.truncate(base);
            s.acc.rowids.truncate(d);
            let got = self.level_row(s, d).await?;
            let Some(sp) = s.qp.sources.as_slice().get(d) else {
                return Ok(false);
            };
            #[cfg(feature = "planner")]
            let got = got && !s.levels.as_slice().get(d).is_some_and(|l| l.done);
            if got {
                let on_ok = match &sp.on {
                    None => true,
                    // A subquery in the ON finds its slots after every
                    // source (`join_step`).
                    Some(on) if crate::query::has_sub(on) => {
                        let mut padded = s.acc.clone();
                        pad_sources(
                            &mut padded,
                            s.qp.sources.as_slice().get(d + 1..).unwrap_or(&[]),
                        );
                        self.cond_row_sub(on, &padded).await?
                    }
                    Some(on) => {
                        crate::query::cond::<S::Error>(on, &Ctx::of(&s.acc).with(&self.conn.env))?
                    }
                };
                if on_ok {
                    if let Some(l) = s.levels.as_mut_slice().get_mut(d) {
                        l.any = true;
                    }
                    s.depth = d + 1;
                }
                continue;
            }
            let Some(lv) = s.levels.as_mut_slice().get_mut(d) else {
                return Ok(false);
            };
            if sp.kind.pads_right() && !lv.any && !lv.padded {
                lv.padded = true;
                pad_sources(&mut s.acc, core::slice::from_ref(sp));
                s.depth = d + 1;
                continue;
            }
            lv.src = Src::Idle;
            // What its loop added (a lateral call's documents, the
            // full-text rows of an inner filter) goes with it.
            self.side_drop(lv.side, s.pinned);
            #[cfg(any(feature = "json", feature = "vtab"))]
            for (slot, v) in core::mem::take(&mut lv.saved) {
                if let Some(x) = s.acc.vals.as_mut_slice().get_mut(slot) {
                    *x = v;
                }
            }
            if d == 0 {
                s.ended = true;
                return Ok(false);
            }
            s.depth = d - 1;
        }
    }

    /// The lengths of the side tables a row's values point into
    /// (`Env::json_docs`, `Env::fts_rows`).
    #[inline(never)]
    fn side_len(&self) -> (usize, usize) {
        #[cfg(feature = "json")]
        let j = self.conn.env.json_docs.try_borrow().map_or(0, |d| d.len());
        #[cfg(not(feature = "json"))]
        let j = 0;
        #[cfg(feature = "fts5")]
        let f = self.conn.env.fts_rows.try_borrow().map_or(0, |d| d.len());
        #[cfg(not(feature = "fts5"))]
        let f = 0;
        (j, f)
    }

    /// Drop the side-table entries past `to` (but none below `pin`):
    /// what a loop that ended, or a row that was dealt with, added
    /// and nothing kept reads — so a streamed join's memory does not
    /// grow with its outer rows.
    #[inline(never)]
    fn side_drop(&self, to: (usize, usize), pin: (usize, usize)) {
        #[cfg(feature = "json")]
        if let Ok(mut d) = self.conn.env.json_docs.try_borrow_mut() {
            d.truncate(to.0.max(pin.0));
        }
        #[cfg(feature = "fts5")]
        if let Ok(mut d) = self.conn.env.fts_rows.try_borrow_mut() {
            d.truncate(to.1.max(pin.1));
        }
        let _ = (to, pin);
    }

    /// Open source `d`'s loop for the combination in `s.acc`.
    async fn level_open(&mut self, s: &mut Stream, d: usize) -> Result<(), S::Error> {
        let side = self.side_len();
        let Stream {
            qp, levels, acc, ..
        } = s;
        let (Some(sp), Some(lv)) = (
            qp.sources.as_slice().get(d),
            levels.as_mut_slice().get_mut(d),
        ) else {
            return Ok(());
        };
        lv.any = false;
        lv.padded = false;
        lv.count = 0;
        #[cfg(feature = "planner")]
        {
            lv.done = false;
        }
        lv.side = side;
        let need = if d == 0 { s.need0 } else { None };
        // The row a source's arguments or probes evaluate on: the
        // sources before it, then NULLs for it and the rest.
        let mut padded = acc.clone();
        pad_sources(&mut padded, qp.sources.as_slice().get(d..).unwrap_or(&[]));
        #[cfg(any(feature = "json", feature = "vtab"))]
        if sp.tvf != 0 {
            let lateral = sp.tvf & crate::query::TVF_LATERAL != 0;
            if lateral
                && !self
                    .lateral_ok(qp.sources.as_slice(), d, qp.filter.as_ref(), acc)
                    .await?
            {
                // No rows, and no LEFT JOIN pad (`join_step`).
                lv.padded = true;
                lv.src = Src::Rows(AVec::new(), 0);
                return Ok(());
            }
            #[cfg(feature = "vtab")]
            if let Some(vt) = sp.vt.as_deref() {
                // A module whose rows are computed at its filter, not
                // reading the sources to its left: the same rows for
                // every outer row, computed once.
                if !lateral && d > 0 && super::vtab::filters_whole(vt) {
                    if lv.cache.is_none() {
                        lv.cache = Some(self.vtab_whole(sp, vt, &padded).await?);
                    }
                    lv.src = Src::Cached(0);
                } else {
                    lv.src = Src::Vt(self.vtab_open(sp, vt, &padded, need).await?);
                }
            } else if lateral {
                lv.src = Src::Rows(self.json_tvf_rows(sp, &padded, None).await?, 0);
            } else {
                if lv.cache.is_none() {
                    lv.cache = Some(self.json_tvf_rows(sp, &padded, need).await?);
                }
                lv.src = Src::Cached(0);
            }
            #[cfg(not(feature = "vtab"))]
            if lateral {
                lv.src = Src::Rows(self.json_tvf_rows(sp, &padded, None).await?, 0);
            } else {
                if lv.cache.is_none() {
                    lv.cache = Some(self.json_tvf_rows(sp, &padded, need).await?);
                }
                lv.src = Src::Cached(0);
            }
            if lateral {
                // The left values only the call read: cleared for the
                // rows it makes, put back when its loop ends.
                let here = acc.vals.len();
                for back in sp.tvf_dead.iter() {
                    if let Some(slot) = here.checked_sub(*back)
                        && let Some(v) = acc.vals.as_mut_slice().get_mut(slot)
                    {
                        lv.saved
                            .push((slot, core::mem::replace(v, OwnedValue::Null)));
                    }
                }
            }
            return Ok(());
        }
        // An automatic index (v0.17, `planner`): the table's rows
        // sorted on its key once, then this combination's span.
        #[cfg(feature = "planner")]
        if let Some(ai) = sp.access.as_deref().and_then(|a| a.auto.as_deref()) {
            if lv.cache.is_none() {
                let rows = alloc::boxed::Box::pin(self.auto_rows(sp, ai)).await?;
                if ai.bloom {
                    lv.bloom = super::join::bloom_build(rows.as_slice(), ai);
                }
                lv.cache = Some(rows);
            }
            let span = match alloc::boxed::Box::pin(self.auto_probe(ai, &padded)).await? {
                Some(p) if super::join::bloom_may(lv.bloom.as_slice(), p.as_slice()) => {
                    match &lv.cache {
                        Some(r) => super::join::auto_span(
                            r.as_slice(),
                            ai,
                            p.as_slice(),
                            self.conn.pager.text_enc(),
                        ),
                        None => (0, 0),
                    }
                }
                _ => (0, 0),
            };
            lv.src = Src::Span(span.0, span.1);
            return Ok(());
        }
        #[cfg(feature = "expr_index")]
        if let Some(a) = sp.access.as_deref()
            && a.lateral
        {
            // v0.17 (`planner`): the Bloom filter first, built at
            // the first probe.
            #[cfg(feature = "planner")]
            if let Some(keys) = &a.bloom {
                if !lv.bloom_built {
                    lv.bloom = alloc::boxed::Box::pin(self.bloom_rows(sp, keys.as_slice())).await?;
                    lv.bloom_built = true;
                }
                let bits = core::mem::take(&mut lv.bloom);
                let pass =
                    alloc::boxed::Box::pin(self.bloom_pass(a, bits.as_slice(), &padded)).await;
                lv.bloom = bits;
                if !pass? {
                    lv.src = Src::Rows(AVec::new(), 0);
                    return Ok(());
                }
            }
            lv.src = Src::Rows(self.access_rows(sp, a, &padded).await?, 0);
            return Ok(());
        }
        if lv.cache.is_none() {
            let rows = if let Some(dp) = sp.derived.as_deref() {
                let inner = if dp.mat != 0 {
                    self.shared_rows(dp, &RowData::empty(), need).await?
                } else {
                    self.exec_subplan_need(dp, &RowData::empty(), need).await?
                };
                let mut rows = AVec::new();
                for r in inner {
                    rows.push((r, None));
                }
                Some(rows)
            } else if sp.temp {
                Some(self.scan_temp(sp).await?)
            } else {
                #[cfg(feature = "expr_index")]
                let rows = match sp.access.as_deref() {
                    Some(a) => Some(self.access_rows(sp, a, &padded).await?),
                    None if d == 0 => None,
                    None => Some(self.scan_source(sp).await?),
                };
                #[cfg(not(feature = "expr_index"))]
                let rows = if d == 0 {
                    None
                } else {
                    Some(self.scan_source(sp).await?)
                };
                rows
            };
            match rows {
                Some(r) => lv.cache = Some(r),
                None => {
                    // The first source's table, row by row.
                    lv.src = Src::Scan(ScanCursor::new(sp.root));
                    return Ok(());
                }
            }
        }
        lv.src = Src::Cached(0);
        Ok(())
    }

    /// Source `d`'s next row appended to `s.acc`; false at its end.
    async fn level_row(&mut self, s: &mut Stream, d: usize) -> Result<bool, S::Error> {
        let Stream {
            qp,
            levels,
            acc,
            buf,
            capped,
            ..
        } = s;
        let _ = capped;
        let (Some(sp), Some(lv)) = (
            qp.sources.as_slice().get(d),
            levels.as_mut_slice().get_mut(d),
        ) else {
            return Ok(false);
        };
        let (rows, pos) = match &mut lv.src {
            Src::Idle => return Ok(false),
            Src::Scan(c) => {
                let Some(rowid) = c.next_row_into(&mut self.conn.pager, buf).await? else {
                    return Ok(false);
                };
                decode_source_row::<S::Error>(sp, buf.as_slice(), rowid, &mut acc.vals)?;
                acc.rowids.push(Some(rowid));
                return Ok(true);
            }
            #[cfg(feature = "vtab")]
            Src::Vt(c) => {
                if *capped && matches!(c, super::vtab::VtCur::Series(_)) {
                    lv.count = lv.count.saturating_add(1);
                    if lv.count > self.conn.cfg.max_vtab_rows {
                        return Err(Error::Schema {
                            kind: SchemaKind::VtabTooManyRows,
                        });
                    }
                }
                return self.vtab_next(c, sp.ncols, acc).await;
            }
            Src::Cached(pos) => match &lv.cache {
                Some(r) => (r, pos),
                None => return Ok(false),
            },
            #[cfg(feature = "planner")]
            Src::Span(pos, end) => {
                if *pos >= *end {
                    return Ok(false);
                }
                match &lv.cache {
                    Some(r) => (r, pos),
                    None => return Ok(false),
                }
            }
            #[cfg(any(feature = "json", feature = "vtab", feature = "expr_index"))]
            Src::Rows(r, pos) => (&*r, pos),
        };
        let Some((vals, rowid)) = rows.as_slice().get(*pos) else {
            return Ok(false);
        };
        *pos += 1;
        for v in vals.iter() {
            acc.vals.push(v.clone());
        }
        acc.rowids.push(*rowid);
        Ok(true)
    }

    /// Fold the joined row `row` into the aggregates.
    /// True when it kept the row (as the bare columns' row).
    fn agg_step(&self, run: &mut AggRun, row: &RowData) -> Result<bool, S::Error> {
        let env = &self.conn.env;
        let enc = self.conn.pager.text_enc();
        let ctx = Ctx::of(row).with(env);
        for (a, st) in run.aggs.iter().zip(run.accs.as_mut_slice().iter_mut()) {
            let Some(p) = crate::query::agg_parts(a) else {
                continue;
            };
            if let Some(f) = p.filter
                && !crate::query::cond::<S::Error>(f, &ctx)?
            {
                continue;
            }
            let v = match p.args.first() {
                Some(e) if !p.star => crate::query::eval::<S::Error>(e, &ctx)?,
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
        // The bare columns' row: the first, or the lone min / max
        // aggregate's extremum row (`pick_rep_from`).
        let mut kept = false;
        if run.first.is_none() {
            run.first = Some(row.clone());
            kept = true;
        }
        if let Some((kind, arg, filter, coll)) = &run.mm {
            let pass = match filter {
                Some(f) => crate::query::cond::<S::Error>(f, &ctx)?,
                None => true,
            };
            if pass {
                let v = crate::query::eval::<S::Error>(arg, &ctx)?;
                if extremum_step(*kind, *coll, &mut run.best, v, enc) {
                    run.rep = Some(row.clone());
                    kept = true;
                }
            }
        }
        Ok(kept)
    }

    /// The end of the join: a blocking mode's rows into
    /// `query_rows`.
    async fn stream_finish(&mut self, s: &mut Stream) -> Result<(), S::Error> {
        self.query_rows.clear();
        self.query_rowids.clear();
        if s.window.1 == 0 {
            return Ok(());
        }
        let produced = match &mut s.mode {
            Mode::Plain | Mode::Distinct(_) => return Ok(()),
            Mode::Agg(run) => {
                let empty = empty_rep(s.qp.sources.as_slice());
                let rep = run.rep.as_ref().or(run.first.as_ref()).unwrap_or(&empty);
                let mut vals = AVec::new();
                for (a, st) in run.aggs.iter().zip(run.accs.iter()) {
                    let kind = crate::query::agg_parts(a).map_or(AggKind::Count, |p| p.kind);
                    vals.push(match st {
                        Acc::Count(n) => OwnedValue::Integer(*n),
                        Acc::Sum(c) => c.finish(kind).map_err(|kind| Error::Schema { kind })?,
                        Acc::Ext(b) => b.clone().unwrap_or(OwnedValue::Null),
                    });
                }
                let mut out = AVec::new();
                let mut at = 0usize;
                for it in s.qp.items.iter() {
                    let mut pos = 0;
                    let rest = vals.as_slice().get(at..).unwrap_or(&[]);
                    let mut used = 0;
                    let e = crate::query::substitute_group(it, &[], &mut pos, rest, &mut used);
                    at += used;
                    out.push(crate::query::eval::<S::Error>(
                        &e,
                        &Ctx::group(&[], rep).with(&self.conn.env),
                    )?);
                }
                let mut p = AVec::new();
                p.push(Produced {
                    out,
                    keys: AVec::new(),
                    rowid: None,
                });
                p
            }
            Mode::Buffer(rows) => {
                let cap = group_cap(
                    s.qp.group_by.as_slice(),
                    s.qp.order_by.as_slice(),
                    s.qp.distinct,
                    s.window,
                );
                self.project_qp_async(&s.qp, rows.as_slice(), cap).await?
            }
            Mode::Sort(out) => core::mem::take(out),
        };
        self.finish_produced(&s.qp, produced, s.window);
        Ok(())
    }
}

/// The empty group's bare-column row: NULLs for every source.
fn empty_rep(sources: &[SourcePlan]) -> RowData {
    let mut r = RowData::empty();
    pad_sources(&mut r, sources);
    r
}

impl<S: Storage> Statement<'_, S> {
    /// Drop a stream left unfinished (a reset, or the statement
    /// dropped part-way): the JSON documents and full-text rows its
    /// rows held go with it.
    #[cfg(feature = "params")]
    pub(super) fn end_stream(&mut self) {
        if self.stream.take().is_some() {
            self.reclaim_stream();
        }
    }

    /// Drop what an unfinished stream's rows held (`end_stream`; at
    /// the first step of the next top-level statement, the
    /// statement that left it being gone).
    pub(super) fn reclaim_stream(&mut self) {
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        if self.conn.in_program() {
            return;
        }
        let Some(m) = self.conn.stream_left.take() else {
            return;
        };
        #[cfg(feature = "json")]
        if let Ok(mut d) = self.conn.env.json_docs.try_borrow_mut() {
            d.truncate(m.0);
        }
        #[cfg(feature = "fts5")]
        if let Ok(mut d) = self.conn.env.fts_rows.try_borrow_mut() {
            d.truncate(m.1);
        }
        let _ = m;
    }
}
