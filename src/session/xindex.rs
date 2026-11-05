//! Expression and partial indexes (v0.16, `expr_index`).
//!
//! A catalog index whose stored text has expression key items or a
//! `WHERE` carries those texts (`schema::IndexExt`). Whenever the
//! index is maintained (every write path, CREATE INDEX, REINDEX,
//! `integrity_check`) the texts are parsed again and resolved
//! against the table's row — slot = column index, rowid source 0,
//! the rule machinery's shape (`rules::eval_row`) — into an
//! `IdxExt`. A row's entry is then its key values computed from the
//! row (`ext_key`), or no entry at all when the partial index's
//! `WHERE` is not true for it (NULL excludes, as on the pin).
//!
//! The pin's definition rules, checked when the texts are resolved
//! (so CREATE INDEX refuses what the pin refuses, and a file whose
//! index hipp cannot compute refuses writes instead of leaving the
//! index stale):
//!
//! - key expressions: no subquery, no aggregate or window call, no
//!   non-deterministic function, no qualified column (the pin's
//!   `"." operator prohibited`), no `rowid`;
//! - the WHERE: no subquery, aggregate or non-deterministic
//!   function; qualified columns and `rowid` are allowed;
//! - parameters are refused by the parser (stored texts).
//!
//! An expression column's collation is its item's top-level
//! `COLLATE`, else BINARY (the pin's `sqlite3CreateIndex`), even
//! when the expression reads a NOCASE column.
//!
//! The module also holds the session side of index-aware planning
//! (`crate::xplan` decides): the planner's view of a table and its
//! indexes (`table_models`), the per-core and UPDATE / DELETE access
//! choices (`plan_accesses`, `plan_write_access`), and the executor
//! that reads a source through its access path (`access_rows`,
//! `access_rowids`): equality / IN probes evaluated against the row
//! so far, converted by the comparison's affinity, walked in the
//! key's order (backwards when the plan says so), ranges bounded
//! under the key column's collation, each found row fetched by rowid.

use super::*;
use crate::query::{ScopeGroup, SourceDesc, resolve_scoped};

/// A resolved expression / partial index definition (v0.16,
/// `expr_index`), shared by the plans that maintain the index.
#[derive(Debug, Clone)]
pub(super) struct IdxExt {
    /// Per key column: the expression of an expression column
    /// (top-level `COLLATE` removed), `None` for a column item.
    pub exprs: AVec<Option<RExpr>>,
    /// The partial index's WHERE.
    pub pred: Option<RExpr>,
    /// The table's column affinities (the row the expressions read
    /// forces REAL columns, as a scan does).
    pub affs: AVec<crate::record::Affinity>,
    pub rowid_alias: Option<usize>,
}

/// Whether `e` reads a column qualified by any name.
fn qualified(e: &Expr) -> bool {
    crate::rules::expr_any(e, &mut |x| matches!(x, Expr::Column { table: Some(_), .. }))
}

impl<S: Storage> Connection<S> {
    /// Resolve an index's expression texts and WHERE against table
    /// `t` (v0.16, `expr_index`), with the pin's definition rules.
    /// `exprs` is parallel to the key columns (`None` for a column
    /// item); the resolved trees live in `arena`'s parse of them.
    pub(super) fn compile_ext(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        exprs: &[Option<&[u8]>],
        filter: Option<&[u8]>,
    ) -> Result<IdxExt, S::Error> {
        let ca = &self.catalog_arena;
        let lim = self.cfg.limits();
        let mut descs = AVec::new();
        let mut affs = AVec::new();
        for c in t.columns.iter() {
            descs.push((copy_vec(handle_bytes(ca, c.name)), c.affinity));
            affs.push(c.affinity);
        }
        let colls = table_colls(t);
        let mut offsets = AVec::new();
        offsets.push(0);
        // A partial index's WHERE ignores a three-part name's schema
        // part, as on the pin (v0.16 review fix: `temp.t.a` on a main
        // table, or `bogus.t.a`, was refused); a key expression may
        // not qualify a column at all (`QualifiedIndexColumn`).
        let scope = |qualifier: &[u8], rowid: bool| {
            let mut srcs = AVec::new();
            let mut src = SourceDesc::new(copy_vec(qualifier), descs.clone(), t.rowid_alias, rowid);
            src.colls = colls.clone();
            src.db = crate::query::DB_ANY;
            srcs.push(src);
            ScopeGroup::new(srcs, offsets.clone(), 0)
        };
        let check = |r: &RExpr| -> Result<(), S::Error> {
            if crate::query::has_sub(r) {
                return Err(Error::Schema {
                    kind: SchemaKind::SubqueryInIndex,
                });
            }
            if crate::query::has_agg(r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedAggregate,
                });
            }
            #[cfg(feature = "window")]
            if crate::window::has_win(r) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedWindow,
                });
            }
            if crate::query::any_scalar(r, &crate::query::ScalarFn::nondeterministic) {
                return Err(Error::Schema {
                    kind: SchemaKind::NonDeterministic,
                });
            }
            Ok(())
        };
        let mut refuse = |_: &crate::sql::SelectStmt,
                          _: &ScopeGroup,
                          _: &[ScopeGroup]|
         -> Result<crate::query::SubPlan, S::Error> {
            Err(Error::Schema {
                kind: SchemaKind::SubqueryInIndex,
            })
        };
        // The pin resolves the WHERE first (`sqlite3ResolveSelfReference`
        // with NC_PartIdx), then each key item.
        let pred = match filter {
            None => None,
            Some(text) => {
                let owned = copy_vec(text);
                let e =
                    crate::sql::parser::parse_expr_text::<S::Error>(owned.as_slice(), arena, lim)?;
                let cur = scope(handle_bytes(ca, t.name), t.has_rowid());
                let r = resolve_scoped::<S::Error>(&e, arena, &cur, &[], &mut refuse)?;
                check(&r)?;
                Some(r)
            }
        };
        let mut out = AVec::new();
        for x in exprs.iter() {
            let Some(text) = x else {
                out.push(None);
                continue;
            };
            let owned = copy_vec(text);
            let e = crate::sql::parser::parse_expr_text::<S::Error>(owned.as_slice(), arena, lim)?;
            if qualified(&e) {
                return Err(Error::Schema {
                    kind: SchemaKind::QualifiedIndexColumn,
                });
            }
            let cur = scope(b"", false);
            let mut r = resolve_scoped::<S::Error>(&e, arena, &cur, &[], &mut refuse)?;
            check(&r)?;
            if let RExpr::Collate(inner, _) = r {
                r = *inner;
            }
            out.push(Some(r));
        }
        Ok(IdxExt {
            exprs: out,
            pred,
            affs,
            rowid_alias: t.rowid_alias,
        })
    }

    /// The resolved definition of catalog index `ix` on table `t`,
    /// or `None` for an index on plain columns (v0.16).
    pub(super) fn index_ext(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        ix: &crate::schema::IndexDef,
    ) -> Result<Option<alloc::rc::Rc<IdxExt>>, S::Error> {
        let Some(x) = &ix.ext else {
            return Ok(None);
        };
        let ca = &self.catalog_arena;
        let mut exprs: AVec<Option<&[u8]>> = AVec::new();
        for e in x.exprs.iter() {
            exprs.push(e.map(|h| handle_bytes(ca, h)));
        }
        let filter = x.filter.map(|h| handle_bytes(ca, h));
        Ok(Some(alloc::rc::Rc::new(self.compile_ext(
            arena,
            t,
            exprs.as_slice(),
            filter,
        )?)))
    }
}

/// One row's entry key in an expression / partial index (v0.16,
/// `expr_index`): `rec` is the row in record form (generated
/// columns computed), `rowid` its rowid. `None` when the partial
/// index's WHERE is not true for the row (it has no entry).
pub(super) fn ext_key<E>(
    cols: &[IdxColPlan],
    x: &IdxExt,
    rec: &[OwnedValue],
    rowid: i64,
    env: Option<&crate::query::Env>,
) -> Result<Option<AVec<OwnedValue>>, E> {
    let rd = crate::rules::eval_row(x.rowid_alias, x.affs.as_slice(), rec, rowid);
    let ctx = match env {
        Some(env) => Ctx::of(&rd).with(env),
        None => Ctx::of(&rd),
    };
    // Index expressions are "pure" for the date and time functions.
    #[cfg(feature = "datetime")]
    let ctx = ctx.pure();
    if let Some(p) = &x.pred
        && !crate::query::cond::<E>(p, &ctx)?
    {
        return Ok(None);
    }
    let mut key = AVec::new();
    for (i, c) in cols.iter().enumerate() {
        key.push(match x.exprs.as_slice().get(i) {
            Some(Some(e)) => crate::query::eval::<E>(e, &ctx)?,
            _ => match c.col {
                Some(i) if Some(i) != x.rowid_alias => {
                    rec.get(i).cloned().unwrap_or(OwnedValue::Null)
                }
                _ => OwnedValue::Integer(rowid),
            },
        });
    }
    Ok(Some(key))
}

impl<S: Storage> Connection<S> {
    /// The expression or partial index an upsert's conflict target
    /// names (v0.16, `expr_index`; the pin's
    /// `sqlite3UpsertAnalyzeTarget`): the newest UNIQUE index whose
    /// key columns are exactly the target's items (in any order,
    /// an item's `COLLATE` matching the key column's), a partial
    /// one only when the target's WHERE is the same expression as
    /// the index's. `None` when the target is the rowid, or the
    /// first such index is a plain one (the keyset rule then
    /// arbitrates), or nothing matches.
    pub(super) fn upsert_ext_target(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        tname: &[u8],
        upsert: &crate::sql::Upsert,
        pred: Option<&RExpr>,
    ) -> Result<Option<u32>, S::Error> {
        let (sources, offsets) = self.single_source(t, tname);
        let cur = ScopeGroup::new(sources, offsets, 0);
        let mut planner = self.sub_planner(arena);
        let mut items: AVec<(RExpr, Option<crate::record::Coll>)> = AVec::new();
        for (i, n) in upsert.target.iter().enumerate() {
            let coll = upsert.target_colls.as_slice().get(i).copied().flatten();
            let r = match upsert.target_exprs.as_slice().get(i) {
                Some(Some(e)) => resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?,
                _ => resolve_scoped::<S::Error>(
                    &Expr::Column {
                        table: None,
                        name: *n,
                        schema: None,
                    },
                    arena,
                    &cur,
                    &[],
                    &mut planner,
                )?,
            };
            items.push((r, coll));
        }
        if let [(r, _)] = items.as_slice()
            && (matches!(r, RExpr::Rowid(_))
                || matches!(r, RExpr::Column(k, ..) if Some(*k) == t.rowid_alias))
        {
            return Ok(None);
        }
        let ca = &self.catalog_arena;
        let mut ixs: AVec<&crate::schema::IndexDef> = AVec::new();
        for ix in self.catalog.indexes_for(ca, tname) {
            ixs.push(ix);
        }
        for ix in ixs.iter().rev() {
            if !ix.unique || ix.opaque || ix.columns.len() != items.len() {
                continue;
            }
            let x = self.index_ext(arena, t, ix)?;
            if ix.partial() {
                let same = match (pred, x.as_ref().and_then(|x| x.pred.as_ref())) {
                    (Some(a), Some(b)) => crate::query::same_expr(a, b),
                    _ => false,
                };
                if !same {
                    continue;
                }
            }
            let all = ix.columns.iter().enumerate().all(|(ii, c)| {
                let ie = x
                    .as_ref()
                    .and_then(|x| x.exprs.as_slice().get(ii))
                    .and_then(|e| e.as_ref());
                items.iter().any(|(r, coll)| {
                    coll.is_none_or(|k| k == c.coll)
                        && match ie {
                            Some(e) => crate::query::same_expr(r, e),
                            None => matches!(r, RExpr::Column(k, ..) if Some(*k) == c.col),
                        }
                })
            });
            if all {
                return Ok(if ix.plain() { None } else { Some(ix.root) });
            }
        }
        Ok(None)
    }
}

// ------------------------------------------------------- executor

/// One table row of source `sp` as a query reads it (v0.16): decoded
/// with defaults materialised, VIRTUAL columns computed, the rowid
/// alias reading the rowid and REAL columns forced (§5.5).
pub(super) fn source_row<E>(
    sp: &SourcePlan,
    rowid: i64,
    payload: &[u8],
) -> Result<AVec<OwnedValue>, E> {
    let virt = sp.gcols.as_ref().map(|g| g.virt.as_slice());
    let mut rec = crate::rules::decode_rec::<E>(payload, sp.ncols, sp.defaults.as_slice(), virt)?;
    if let Some(g) = &sp.gcols {
        g.fill::<E>(rec.as_mut_slice(), rowid, true)?;
    }
    Ok(crate::rules::eval_row(sp.rowid_alias, sp.affs.as_slice(), rec.as_slice(), rowid).vals)
}

/// A range bound on the rowid (v0.16).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rb {
    /// No rowid satisfies the bound.
    Empty,
    /// Every rowid may.
    All,
    /// Rowids from (a lower bound) or up to (an upper) this one.
    At(i64),
}

/// A bound value as a rowid range end (v0.16): numbers round
/// outward, so the range is a superset of the matching rowids.
fn rowid_bound(v: &OwnedValue, lower: bool, incl: bool) -> Rb {
    match v {
        OwnedValue::Integer(i) => {
            let r = if incl {
                Some(*i)
            } else if lower {
                i.checked_add(1)
            } else {
                i.checked_sub(1)
            };
            r.map_or(Rb::Empty, Rb::At)
        }
        OwnedValue::Real(f) => {
            if f.is_nan() {
                return Rb::All;
            }
            let t = *f as i64;
            Rb::At(if lower {
                if (t as f64) > *f {
                    t.saturating_sub(1)
                } else {
                    t
                }
            } else if (t as f64) < *f {
                t.saturating_add(1)
            } else {
                t
            })
        }
        // Text and blobs sort above every number.
        OwnedValue::Text(_) | OwnedValue::Blob(_) => {
            if lower {
                Rb::Empty
            } else {
                Rb::All
            }
        }
        OwnedValue::Null => Rb::Empty,
    }
}

/// Range bounds on the rowid as `(from, to)` rowids (`None`: open);
/// `None` when no rowid satisfies them.
fn rowid_span(
    lo: &Option<(OwnedValue, bool)>,
    hi: &Option<(OwnedValue, bool)>,
) -> Option<(Option<i64>, Option<i64>)> {
    let rb = |b: &Option<(OwnedValue, bool)>, lower: bool| match b {
        None => Rb::All,
        Some((v, incl)) => rowid_bound(v, lower, *incl),
    };
    let at = |b: Rb| match b {
        Rb::At(x) => Some(Some(x)),
        Rb::All => Some(None),
        Rb::Empty => None,
    };
    Some((at(rb(lo, true))?, at(rb(hi, false))?))
}

/// A probe value as an exact rowid (v0.16).
fn rowid_eq(v: &OwnedValue) -> Option<i64> {
    match v {
        OwnedValue::Integer(i) => Some(*i),
        OwnedValue::Real(f) if (*f as i64) as f64 == *f => Some(*f as i64),
        _ => None,
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    /// The rows source `sp` reads through its access path, against
    /// `row` (the enclosing row, or the combination so far for a
    /// lateral path), in the path's order (v0.16, `expr_index`).
    pub(super) async fn access_rows(
        &mut self,
        sp: &SourcePlan,
        acc: &crate::xplan::IdxAccess,
        row: &RowData,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let mut out = AVec::new();
        // The rowid B-tree walked over a range (v0.16 review fix):
        // its rows as the walk meets them, not re-sought by rowid.
        if acc.root == 0 && acc.eq.is_empty() {
            let (Some(lo), Some(hi)) = (
                self.bound_val(&acc.lo, row).await?,
                self.bound_val(&acc.hi, row).await?,
            ) else {
                return Ok(out);
            };
            let Some((lo_r, hi_r)) = rowid_span(&lo, &hi) else {
                return Ok(out);
            };
            let mut cur = match lo_r {
                Some(m) => btree::ScanCursor::from_rowid(sp.root, m),
                None => btree::ScanCursor::new(sp.root),
            };
            let mut buf = AVec::new();
            while let Some(r) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                if hi_r.is_some_and(|h| r > h) {
                    break;
                }
                out.push((source_row::<S::Error>(sp, r, buf.as_slice())?, Some(r)));
            }
            if acc.reverse {
                out.as_mut_slice().reverse();
            }
            return Ok(out);
        }
        // A covering index (v0.16 review fix): the rows come from its
        // entries — the key columns the query reads, every other
        // column NULL — with no table lookup.
        if let Some(cover) = &acc.cover {
            let mut ents: AVec<AVec<OwnedValue>> = AVec::new();
            let rowids = self.access_walk(sp.root, acc, row, Some(&mut ents)).await?;
            for (rowid, e) in rowids.iter().zip(ents.iter()) {
                let mut rec: AVec<OwnedValue> = AVec::new();
                for _ in 0..sp.ncols {
                    rec.push(OwnedValue::Null);
                }
                for (j, c) in cover.iter().enumerate() {
                    if let (Some(c), Some(v)) = (c, e.as_slice().get(j))
                        && let Some(slot) = rec.as_mut_slice().get_mut(*c)
                    {
                        *slot = v.clone();
                    }
                }
                let rd = crate::rules::eval_row(
                    sp.rowid_alias,
                    sp.affs.as_slice(),
                    rec.as_slice(),
                    *rowid,
                );
                out.push((rd.vals, Some(*rowid)));
            }
            return Ok(out);
        }
        let rowids = self.access_rowids(sp.root, acc, row).await?;
        for rowid in rowids.iter() {
            if let Some(p) = btree::seek_rowid(&mut self.conn.pager, sp.root, *rowid).await? {
                out.push((
                    source_row::<S::Error>(sp, *rowid, p.as_slice())?,
                    Some(*rowid),
                ));
            }
        }
        Ok(out)
    }

    /// One probe set's values, converted and (for IN) sorted and
    /// deduplicated under `coll`; NULLs dropped unless `IS`.
    async fn probe_vals(
        &mut self,
        p: &crate::xplan::Probe,
        key: crate::record::KeyOrder,
        reverse: bool,
        row: &RowData,
    ) -> Result<AVec<OwnedValue>, S::Error> {
        let coll = key.coll;
        let mut vals: AVec<OwnedValue> = AVec::new();
        for e in p.vals.iter() {
            let v = crate::query::probe_value(self.eval_row_sub(e, row).await?, p.aff);
            if matches!(v, OwnedValue::Null) && !p.is {
                continue;
            }
            vals.push(v);
        }
        // An IN subquery (v0.16 review fix): its values, computed
        // once per statement when uncorrelated (`sub_value_for`).
        if let Some(e) = &p.sub {
            let mut kinds: AVec<crate::query::SubKind<'_>> = AVec::new();
            crate::query::collect_subs(e, &mut kinds);
            for k in kinds.iter() {
                match self.sub_value_for(k, row).await? {
                    crate::query::SubValue::Many(vs, _) => {
                        for v in vs.iter() {
                            let v = crate::query::probe_value(v.clone(), p.aff);
                            if !matches!(v, OwnedValue::Null) {
                                vals.push(v);
                            }
                        }
                    }
                    crate::query::SubValue::One(v, _) => {
                        let v = crate::query::probe_value(v, p.aff);
                        if !matches!(v, OwnedValue::Null) {
                            vals.push(v);
                        }
                    }
                    crate::query::SubValue::Fail(kind) => return Err(Error::Schema { kind }),
                }
            }
        }
        if p.is_in {
            let enc = self.conn.pager.text_enc();
            vals.as_mut_slice().sort_by(|a, b| {
                crate::record::compare_coll_in(&a.as_value(), &b.as_value(), coll, enc)
            });
            let mut d: AVec<OwnedValue> = AVec::new();
            for v in vals.iter() {
                let dup = d.as_slice().last().is_some_and(|l| {
                    crate::record::compare_coll(&l.as_value(), &v.as_value(), coll)
                        == core::cmp::Ordering::Equal
                });
                if !dup {
                    d.push(v.clone());
                }
            }
            vals = d;
        }
        // The pin walks an IN list in its key column's order (a
        // DESC column: largest first), backwards for a reverse scan.
        if reverse != key.desc {
            vals.as_mut_slice().reverse();
        }
        Ok(vals)
    }

    /// A range bound's value, converted; `None` for NULL (no row
    /// compares with it).
    async fn bound_val(
        &mut self,
        b: &Option<crate::xplan::Bound>,
        row: &RowData,
    ) -> Result<Option<Option<(OwnedValue, bool)>>, S::Error> {
        let Some(b) = b else {
            return Ok(Some(None));
        };
        let v = crate::query::probe_value(self.eval_row_sub(&b.e, row).await?, b.aff);
        if matches!(v, OwnedValue::Null) {
            return Ok(None);
        }
        Ok(Some(Some((v, b.incl))))
    }

    /// The rowids an access path finds, in its order (v0.16).
    pub(super) async fn access_rowids(
        &mut self,
        table_root: u32,
        acc: &crate::xplan::IdxAccess,
        row: &RowData,
    ) -> Result<AVec<i64>, S::Error> {
        self.access_walk(table_root, acc, row, None).await
    }

    /// `access_rowids`, also keeping each index entry's values in
    /// `ents` (in step with the rowids) when given.
    async fn access_walk(
        &mut self,
        table_root: u32,
        acc: &crate::xplan::IdxAccess,
        row: &RowData,
        mut ents: Option<&mut AVec<AVec<OwnedValue>>>,
    ) -> Result<AVec<i64>, S::Error> {
        use crate::record::{compare_coll, compare_coll_in};
        use core::cmp::Ordering;
        let enc = self.conn.pager.text_enc();
        let mut out: AVec<i64> = AVec::new();
        let (Some(lo), Some(hi)) = (
            self.bound_val(&acc.lo, row).await?,
            self.bound_val(&acc.hi, row).await?,
        ) else {
            return Ok(out);
        };
        let nk = acc.desc.len();
        // The equality value lists, one per probed column.
        let mut lists: AVec<AVec<OwnedValue>> = AVec::new();
        for (j, p) in acc.eq.iter().enumerate() {
            let key = acc.desc.as_slice().get(j).copied().unwrap_or_default();
            let v = self.probe_vals(p, key, acc.reverse, row).await?;
            if v.is_empty() {
                return Ok(out);
            }
            lists.push(v);
        }
        // The rowid B-tree itself.
        if acc.root == 0 {
            if let Some(vals) = lists.as_slice().first() {
                for v in vals.iter() {
                    if let Some(r) = rowid_eq(v)
                        && btree::seek_rowid(&mut self.conn.pager, table_root, r)
                            .await?
                            .is_some()
                    {
                        out.push(r);
                    }
                }
                return Ok(out);
            }
            let Some((lo_r, hi_r)) = rowid_span(&lo, &hi) else {
                return Ok(out);
            };
            let mut cur = match lo_r {
                Some(m) => btree::ScanCursor::from_rowid(table_root, m),
                None => btree::ScanCursor::new(table_root),
            };
            let mut buf = AVec::new();
            while let Some(r) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                if hi_r.is_some_and(|h| r > h) {
                    break;
                }
                out.push(r);
            }
            if acc.reverse {
                out.as_mut_slice().reverse();
            }
            return Ok(out);
        }
        // An index: walk each combination of the equality values.
        let nprefix = lists.len().min(nk);
        let rowid_vals: Option<AVec<i64>> = lists.as_slice().get(nk).map(|vals| {
            let mut v = AVec::new();
            for x in vals.iter() {
                if let Some(r) = rowid_eq(x) {
                    v.push(r);
                }
            }
            v
        });
        let mut pos: AVec<usize> = AVec::new();
        for _ in 0..nprefix {
            pos.push(0);
        }
        loop {
            let mut prefix: AVec<OwnedValue> = AVec::new();
            for (j, p) in pos.iter().enumerate() {
                if let Some(v) = lists.as_slice().get(j).and_then(|l| l.as_slice().get(*p)) {
                    prefix.push(v.clone());
                }
            }
            // The range column, its direction and collation.
            let k = prefix.len();
            let ko = acc.desc.as_slice().get(k).copied().unwrap_or_default();
            let on_key = !acc.range_rowid && k < nk && (lo.is_some() || hi.is_some());
            let start = if on_key {
                if ko.desc { hi.as_ref() } else { lo.as_ref() }
            } else {
                None
            };
            let mut probe: AVec<btree::KeyPart> = AVec::new();
            for v in prefix.iter() {
                probe.push(btree::KeyPart::Val(v.clone()));
            }
            if let Some((v, _)) = start {
                probe.push(btree::KeyPart::Val(v.clone()));
            }
            while probe.len() < nk + 1 {
                probe.push(btree::KeyPart::Min);
            }
            let mut cur = if prefix.is_empty() && start.is_none() {
                btree::IndexCursor::new(acc.root, acc.desc.as_slice())
            } else {
                btree::IndexCursor::seek(
                    &mut self.conn.pager,
                    acc.root,
                    acc.desc.as_slice(),
                    probe.as_slice(),
                )
                .await?
            };
            let (lo_r, hi_r) = if acc.range_rowid {
                let f = |b: &Option<(OwnedValue, bool)>, lower: bool| match b {
                    None => Rb::All,
                    Some((v, incl)) => rowid_bound(v, lower, *incl),
                };
                (f(&lo, true), f(&hi, false))
            } else {
                (Rb::All, Rb::All)
            };
            let mut found: AVec<i64> = AVec::new();
            let ents_from = ents.as_deref().map_or(0, |es| es.len());
            let mut nulls: AVec<bool> = AVec::new();
            let none_left = lo_r == Rb::Empty || hi_r == Rb::Empty;
            loop {
                if none_left {
                    break;
                }
                let Some(e) = cur.next_entry(&mut self.conn.pager).await? else {
                    break;
                };
                let vals = e.values.as_slice();
                let same = prefix.iter().enumerate().all(|(j, p)| {
                    let c = acc
                        .desc
                        .as_slice()
                        .get(j)
                        .map(|o| o.coll)
                        .unwrap_or_default();
                    vals.get(j).is_some_and(|v| {
                        compare_coll(&v.as_value(), &p.as_value(), c) == Ordering::Equal
                    })
                });
                if !same {
                    break;
                }
                let rowid = e.rowid::<S::Error>()?;
                if on_key {
                    let Some(v) = vals.get(k) else { continue };
                    if matches!(v, OwnedValue::Null) {
                        if ko.desc {
                            break;
                        }
                        continue;
                    }
                    if let Some((l, incl)) = &lo {
                        let c = compare_coll_in(&v.as_value(), &l.as_value(), ko.coll, enc);
                        if c == Ordering::Less || (c == Ordering::Equal && !incl) {
                            if ko.desc {
                                break;
                            }
                            continue;
                        }
                    }
                    if let Some((h, incl)) = &hi {
                        let c = compare_coll_in(&v.as_value(), &h.as_value(), ko.coll, enc);
                        if c == Ordering::Greater || (c == Ordering::Equal && !incl) {
                            if ko.desc {
                                continue;
                            }
                            break;
                        }
                    }
                }
                if let Rb::At(l) = lo_r
                    && rowid < l
                {
                    continue;
                }
                if let Rb::At(h) = hi_r
                    && rowid > h
                {
                    break;
                }
                if let Some(rv) = &rowid_vals
                    && !rv.iter().any(|r| *r == rowid)
                {
                    continue;
                }
                found.push(rowid);
                if let Some((f, _)) = acc.bignull {
                    nulls.push(matches!(vals.get(f), Some(OwnedValue::Null)));
                }
                if let Some(es) = ents.as_deref_mut() {
                    es.push(e.values);
                }
            }
            if acc.reverse {
                found.as_mut_slice().reverse();
                nulls.as_mut_slice().reverse();
                if let Some(tail) = ents
                    .as_deref_mut()
                    .and_then(|es| es.as_mut_slice().get_mut(ents_from..))
                {
                    tail.reverse();
                }
            }
            // The pin's WHERE_BIGNULL_SORT: its second pass reads the
            // NULL keys, put where the ORDER BY wants them.
            if let Some((_, first)) = acc.bignull {
                let order = |want_null: bool| {
                    nulls
                        .iter()
                        .enumerate()
                        .filter(move |(_, n)| **n == want_null)
                        .map(|(i, _)| i)
                };
                let mut perm: AVec<usize> = AVec::new();
                for i in order(first).chain(order(!first)) {
                    perm.push(i);
                }
                let mut f2: AVec<i64> = AVec::new();
                for i in perm.iter() {
                    if let Some(r) = found.as_slice().get(*i) {
                        f2.push(*r);
                    }
                }
                found = f2;
                if let Some(es) = ents.as_deref_mut() {
                    let mut moved: AVec<AVec<OwnedValue>> = AVec::new();
                    for i in perm.iter() {
                        if let Some(e) = es.as_mut_slice().get_mut(ents_from + *i) {
                            moved.push(core::mem::take(e));
                        }
                    }
                    es.truncate(ents_from);
                    for e in moved.as_mut_slice() {
                        es.push(core::mem::take(e));
                    }
                }
            }
            out.extend_from_slice(found.as_slice());
            // The next combination (the last list varies fastest).
            let mut j = nprefix;
            loop {
                if j == 0 {
                    return Ok(out);
                }
                j -= 1;
                let len = lists.as_slice().get(j).map_or(0, |l| l.len());
                if let Some(p) = pos.as_mut_slice().get_mut(j) {
                    *p += 1;
                    if *p < len {
                        break;
                    }
                    *p = 0;
                }
            }
        }
    }
}

// -------------------------------------------------------- planning

/// What the access planner reads of one query core (v0.16).
pub(super) struct CoreInfo<'x> {
    pub items: &'x [RExpr],
    pub filter: Option<&'x RExpr>,
    pub group_by: &'x [RExpr],
    pub having: Option<&'x RExpr>,
    pub order_by: &'x [(RExpr, crate::sql::SortOrder)],
    pub has_agg: bool,
    pub distinct: bool,
    pub limit: Option<&'x RExpr>,
}

/// Whether any expression of the core reads column slot `slot`.
fn core_reads(info: &CoreInfo<'_>, sources: &[SourcePlan], slot: usize) -> bool {
    let r = crate::query::RefRange {
        w: (slot, slot.saturating_add(1)),
        s: (0, 0),
    };
    let f = |e: &RExpr| crate::query::expr_refs_range(e, &r);
    info.items.iter().any(f)
        || info.filter.is_some_and(f)
        || info.group_by.iter().any(f)
        || info.having.is_some_and(f)
        || info.order_by.iter().any(|(e, _)| f(e))
        || sources.iter().any(|s| {
            s.on.as_ref().is_some_and(f)
                // A table-valued source's arguments (v0.16 review
                // fix: they live in its placeholder plan).
                || s.derived
                    .as_ref()
                    .is_some_and(|d| crate::query::plan_refs_range(d, &r))
        })
}

impl<S: Storage> Connection<S> {
    /// The planner's view of table `t` read as source `s` (v0.16):
    /// its width and its usable indexes, newest first (the pin's
    /// `pIndex` order). An index hipp cannot compute is left out.
    pub(super) fn table_models(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        s: &crate::xplan::Src,
    ) -> (i32, AVec<crate::xplan::IdxModel>) {
        use crate::xplan::{IdxKey, IdxModel, log_est, sz_est};
        let ca = &self.catalog_arena;
        let mut szs: AVec<u32> = AVec::new();
        for c in t.columns.iter() {
            szs.push(sz_est(handle_bytes(ca, c.decl), c.affinity));
        }
        let wt: u32 = szs.iter().sum::<u32>() + u32::from(t.rowid_alias.is_none());
        let width = log_est(u64::from(wt) * 4);
        let mut ixs: AVec<&crate::schema::IndexDef> = AVec::new();
        for ix in self.catalog.indexes_for(ca, handle_bytes(ca, t.name)) {
            ixs.push(ix);
        }
        let mut out = AVec::new();
        for ix in ixs.iter().rev() {
            if ix.opaque {
                continue;
            }
            let Ok(ext) = self.index_ext(arena, t, ix) else {
                continue;
            };
            let shift = |e: &RExpr| {
                let mut e = e.clone();
                crate::query::shift_expr(&mut e, (0, 0), (s.base, s.src));
                e
            };
            let mut keys = AVec::new();
            let mut desc = AVec::new();
            // The rowid's width; a WITHOUT ROWID table's entries end
            // with columns instead (below).
            let mut wi: u32 = u32::from(t.has_rowid());
            for (k, c) in ix.columns.iter().enumerate() {
                let e = ext
                    .as_ref()
                    .and_then(|x| x.exprs.as_slice().get(k))
                    .and_then(|e| e.as_ref());
                keys.push(match (e, c.col) {
                    (Some(e), _) => {
                        wi += 1;
                        IdxKey::Expr(shift(e))
                    }
                    (None, Some(col)) => {
                        wi += szs.as_slice().get(col).copied().unwrap_or(1);
                        IdxKey::Col(col)
                    }
                    (None, None) => {
                        wi += 1;
                        IdxKey::Rowid
                    }
                });
                desc.push(crate::record::KeyOrder {
                    desc: c.desc,
                    coll: c.coll,
                });
            }
            // A WITHOUT ROWID table (v0.16): the clustered index holds
            // every stored column, a secondary index the key columns
            // it lacks (`crate::wr`).
            #[allow(unused_mut)]
            let mut suffix = AVec::new();
            #[allow(unused_mut)]
            let mut covers_all = false;
            // A UNIQUE constraint's index (see `IdxModel::suffix_covers`).
            let auto = ix.origin == crate::schema::IndexOrigin::Auto;
            #[cfg(feature = "without_rowid")]
            if !t.has_rowid() {
                if ix.clustered {
                    covers_all = true;
                    for (k, c) in t.columns.iter().enumerate() {
                        let virt = c.gen_expr.is_some() && !c.stored;
                        if !virt && !ix.columns.iter().any(|kc| kc.col == Some(k)) {
                            wi += szs.as_slice().get(k).copied().unwrap_or(1);
                        }
                    }
                } else if let Some(pk) = self
                    .catalog
                    .indexes_for(ca, handle_bytes(ca, t.name))
                    .find(|i| i.clustered)
                {
                    for pc in pk.columns.iter() {
                        let held = ix.columns.iter().enumerate().any(|(k, kc)| {
                            !matches!(keys.as_slice().get(k), Some(IdxKey::Expr(_)))
                                && kc.col == pc.col
                                && kc.coll == pc.coll
                        });
                        if held {
                            continue;
                        }
                        if let Some(c) = pc.col {
                            wi += szs.as_slice().get(c).copied().unwrap_or(1);
                            suffix.push((
                                IdxKey::Col(c),
                                crate::record::KeyOrder {
                                    desc: pc.desc && !auto,
                                    coll: pc.coll,
                                },
                            ));
                        }
                    }
                }
            }
            out.push(IdxModel {
                root: ix.root,
                unique: ix.unique,
                keys,
                desc,
                pred: ext.as_ref().and_then(|x| x.pred.as_ref()).map(shift),
                width: log_est(u64::from(wi) * 4),
                suffix,
                covers_all,
                suffix_covers: !auto,
            });
        }
        (width, out)
    }

    /// Choose each table source's access path in one query core
    /// (v0.16, `expr_index`; see `xplan`). `offsets` / `first_src`
    /// place the core's sources in the query row, after the
    /// enclosing scopes' `outer` slots and rowid sources.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_accesses(
        &self,
        arena: &Arena,
        sources: &mut [SourcePlan],
        offsets: &[usize],
        first_src: usize,
        outer: (usize, usize),
        info: &CoreInfo<'_>,
    ) -> Result<(), S::Error> {
        use crate::query::{RefRange, expr_refs_range};
        use crate::xplan::{Query, Src, Table, choose, conjuncts, log_est, terms};
        let mut wconj: AVec<&RExpr> = AVec::new();
        if let Some(f) = info.filter {
            conjuncts(f, &mut wconj);
        }
        // `INDEXED BY` a partial index the WHERE (and the source's
        // ON) does not imply: the pin's "no query solution".
        for (i, sp) in sources.iter().enumerate() {
            if sp.hint == crate::query::Hint::Any || sp.temp {
                continue;
            }
            let (Some(t), Some(&base)) = (
                self.catalog.tables.iter().find(|t| t.root == sp.root),
                offsets.get(i),
            ) else {
                continue;
            };
            let s = Src {
                base,
                ncols: sp.ncols,
                src: first_src + i,
                rowid_alias: sp.rowid_alias,
            };
            let mut conj: AVec<&RExpr> = AVec::new();
            if i == 0 || !sp.kind.pads_right() {
                conj.extend_from_slice(wconj.as_slice());
            } else if info.filter.is_some_and(|f| s.null_rejects(f)) {
                // A WHERE term no NULL-extended row passes makes the
                // LEFT JOIN an inner join, as the pin simplifies it
                // (v0.16 review fix: "no query solution").
                conj.extend_from_slice(wconj.as_slice());
            }
            if let Some(on) = &sp.on {
                conjuncts(on, &mut conj);
            }
            self.hint_solvable(arena, t, sp.hint, &s, conj.as_slice())?;
        }
        // RIGHT / FULL joins emit each right row's no-match: every
        // source keeps its whole table.
        if sources.iter().any(|s| s.kind.pads_left()) {
            return Ok(());
        }
        let n = sources.len();
        let core_end = match (offsets.last(), sources.last()) {
            (Some(o), Some(s)) => o + s.ncols,
            _ => return Ok(()),
        };
        let mut plans: AVec<(usize, crate::xplan::IdxAccess)> = AVec::new();
        #[cfg(feature = "explain")]
        let mut naturals: AVec<(usize, (usize, bool))> = AVec::new();
        for i in 0..n {
            let Some(sp) = sources.get(i) else { break };
            // (A TEMP table's root is the temp database's page.)
            if sp.derived.is_some() || sp.root <= 1 || sp.rec != 0 || sp.temp {
                continue;
            }
            #[cfg(any(feature = "json", feature = "vtab"))]
            if sp.tvf != 0 {
                continue;
            }
            let Some(t) = self.catalog.tables.iter().find(|t| t.root == sp.root) else {
                continue;
            };
            let Some(&base) = offsets.get(i) else { break };
            let s = Src {
                base,
                ncols: sp.ncols,
                src: first_src + i,
                rowid_alias: sp.rowid_alias,
            };
            let mut conj: AVec<&RExpr> = AVec::new();
            if i == 0 || !sp.kind.pads_right() {
                conj.extend_from_slice(wconj.as_slice());
            }
            if let Some(on) = &sp.on {
                conjuncts(on, &mut conj);
            }
            let later = RefRange {
                w: (base, core_end),
                s: (s.src, first_src + n),
            };
            let earlier = RefRange {
                w: (outer.0, base),
                s: (outer.1, s.src),
            };
            let avail = |e: &RExpr| -> Option<bool> {
                if expr_refs_range(e, &later)
                    || crate::query::any_scalar(e, &crate::query::ScalarFn::nondeterministic)
                {
                    return None;
                }
                Some(expr_refs_range(e, &earlier))
            };
            let ts = terms(conj.as_slice(), &s, &avail);
            // The ORDER BY (or GROUP BY / DISTINCT) the loop may
            // deliver: a lone source only.
            let mut order: AVec<(RExpr, crate::sql::SortOrder)> = AVec::new();
            let mut group = false;
            let mut group_by = false;
            if n == 1 {
                if !info.group_by.is_empty() {
                    group = true;
                    group_by = true;
                    for e in info.group_by.iter() {
                        order.push((
                            e.clone(),
                            crate::sql::SortOrder {
                                desc: false,
                                nulls_first: true,
                                coll: crate::query::expr_coll(e).unwrap_or_default(),
                            },
                        ));
                    }
                } else if !info.has_agg && !info.order_by.is_empty() {
                    order.extend_from_slice(info.order_by);
                } else if !info.has_agg
                    && info.distinct
                    // A DISTINCT over the rowid is redundant (the pin's
                    // `isDistinctRedundant`): no order to deliver.
                    && !info.items.iter().any(|e| s.is_rowid(e))
                {
                    group = true;
                    for e in info.items.iter() {
                        order.push((
                            e.clone(),
                            crate::sql::SortOrder {
                                desc: false,
                                nulls_first: true,
                                coll: crate::query::expr_coll(e).unwrap_or_default(),
                            },
                        ));
                    }
                }
            }
            let mut needed: AVec<bool> = AVec::new();
            for c in 0..sp.ncols {
                needed.push(core_reads(info, sources, base + c));
            }
            let limit = match info.limit {
                Some(RExpr::Literal(OwnedValue::Integer(k))) if *k > 0 => Some(log_est(*k as u64)),
                _ => None,
            };
            #[cfg(feature = "explain")]
            let mut dset: AVec<(RExpr, crate::sql::SortOrder)> = AVec::new();
            #[cfg(feature = "explain")]
            if n == 1 && info.distinct && !info.has_agg && !info.order_by.is_empty() {
                for e in info.items.iter() {
                    dset.push((
                        e.clone(),
                        crate::sql::SortOrder {
                            desc: false,
                            nulls_first: true,
                            coll: crate::query::expr_coll(e).unwrap_or_default(),
                        },
                    ));
                }
            }
            let q = Query {
                conj: conj.as_slice(),
                order: order.as_slice(),
                group,
                group_by,
                ncols_out: info.items.len(),
                needed: Some(needed.as_slice()),
                cover_scan: true,
                limit,
                distinct: info.distinct,
                #[cfg(feature = "explain")]
                dset: dset.as_slice(),
            };
            let (width, models) = self.table_models(arena, t, &s);
            let notnull = self.not_null_cols(t);
            let tab = Table {
                width,
                indexes: models.as_slice(),
                wr: !t.has_rowid(),
                hint: sp.hint,
                notnull: notnull.as_slice(),
            };
            let chosen = choose(&q, &s, &tab, ts.as_slice());
            #[cfg(feature = "explain")]
            if chosen.is_none() {
                naturals.push((i, crate::xplan::natural_sat(&q, &s, &tab, ts.as_slice())));
            }
            if let Some(mut a) = chosen {
                // EXPLAIN QUERY PLAN's COVERING (v0.16, `explain`).
                #[cfg(feature = "explain")]
                {
                    a.eqp.1 = models
                        .iter()
                        .find(|m| m.root == a.root)
                        .is_some_and(|m| !m.covers_all && m.covers(needed.as_slice()));
                }
                // A covering index of a rowid table: the rows come
                // from its entries (v0.16 review fix).
                if t.has_rowid()
                    && a.root != 0
                    && let Some(m) = models.iter().find(|m| m.root == a.root)
                    && m.covers(needed.as_slice())
                {
                    let mut map: AVec<Option<usize>> = AVec::new();
                    for k in m.keys.iter() {
                        map.push(match k {
                            crate::xplan::IdxKey::Col(c) => Some(*c),
                            _ => None,
                        });
                    }
                    a.cover = Some(map);
                }
                plans.push((i, a));
            }
        }
        for (i, a) in plans.iter() {
            if let Some(sp) = sources.get_mut(*i) {
                sp.access = Some(Box::new(a.clone()));
            }
        }
        #[cfg(feature = "explain")]
        for (i, (n, d)) in naturals.iter() {
            if let Some(sp) = sources.get_mut(*i) {
                sp.eqp.natural = *n;
                sp.eqp.nat_distinct = *d;
            }
        }
        Ok(())
    }

    /// Per column of `t`: never NULL (`xplan::Table::notnull`).
    fn not_null_cols(&self, t: &crate::schema::TableDef) -> AVec<bool> {
        #[cfg(feature = "without_rowid")]
        let ca = &self.catalog_arena;
        #[cfg(feature = "without_rowid")]
        let pk = self
            .catalog
            .indexes_for(ca, handle_bytes(ca, t.name))
            .find(|i| i.clustered);
        #[cfg(not(feature = "without_rowid"))]
        let pk: Option<&crate::schema::IndexDef> = None;
        let mut out = AVec::new();
        for (i, c) in t.columns.iter().enumerate() {
            out.push(
                c.not_null
                    || t.rowid_alias == Some(i)
                    || pk.is_some_and(|p| p.columns.iter().any(|k| k.col == Some(i))),
            );
        }
        out
    }

    /// Whether an `INDEXED BY` index can serve table `t` read as
    /// source `s` under the conjuncts `conj` (v0.16): a partial
    /// index only when they imply its WHERE (`NoQuerySolution`
    /// otherwise, as on the pin); an index hipp cannot compute is
    /// refused (`Unsupported`).
    pub(super) fn hint_solvable(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        hint: crate::query::Hint,
        s: &crate::xplan::Src,
        conj: &[&RExpr],
    ) -> Result<(), S::Error> {
        let crate::query::Hint::By(root) = hint else {
            return Ok(());
        };
        let (_, models) = self.table_models(arena, t, s);
        match models.iter().find(|m| m.root == root) {
            None => Err(Error::Unsupported),
            Some(m) => match &m.pred {
                Some(p) if !crate::xplan::implies(conj, p) => Err(Error::Schema {
                    kind: SchemaKind::NoQuerySolution,
                }),
                _ => Ok(()),
            },
        }
    }
}

/// What an UPDATE's access planning needs (v0.16).
pub(super) struct UpdateInfo<'x> {
    pub sets: &'x [(Target, RExpr)],
    pub returning: bool,
    pub from: bool,
    /// The UPDATE may delete REPLACE victims: OR REPLACE, or a
    /// constraint declared `ON CONFLICT REPLACE` (v0.16).
    pub replaces: bool,
}

impl<S: Storage> Connection<S> {
    /// An UPDATE's or DELETE's target access path (v0.16,
    /// `expr_index`): the WHERE terms on constants only. The rows
    /// are visited in rowid order (the pin collects them first)
    /// unless an UPDATE runs as the pin's one-pass loop — no
    /// RETURNING, UPDATE trigger, foreign keys, FROM, REPLACE, rowid
    /// change or WHERE subquery, and the loop's index not changed by
    /// the SET — which keeps the loop's order.
    pub(super) fn plan_write_access(
        &self,
        arena: &Arena,
        t: &crate::schema::TableDef,
        filter: Option<&RExpr>,
        update: Option<UpdateInfo<'_>>,
        hint: crate::query::Hint,
    ) -> Result<Option<Box<crate::xplan::IdxAccess>>, S::Error> {
        use crate::query::{RefRange, expr_refs_range};
        use crate::xplan::{IdxKey, Query, Src, Table, choose, conjuncts, terms};
        // A WITHOUT ROWID table is read through its cheapest index
        // even with no usable term (v0.16): the pin's one-pass loop
        // then walks that index.
        let wr = !t.has_rowid();
        let always = RExpr::Literal(OwnedValue::Integer(1));
        // A forced index is walked even with no WHERE.
        let forced = matches!(hint, crate::query::Hint::By(_));
        let f = match filter {
            Some(f) => f,
            None if wr || forced => &always,
            None => return Ok(None),
        };
        let ca = &self.catalog_arena;
        let _ = ca;
        let s = Src {
            base: 0,
            ncols: t.columns.len(),
            src: 0,
            rowid_alias: t.rowid_alias,
        };
        let mut conj: AVec<&RExpr> = AVec::new();
        conjuncts(f, &mut conj);
        let all = RefRange {
            w: (0, usize::MAX),
            s: (0, usize::MAX),
        };
        let avail = |e: &RExpr| -> Option<bool> {
            // An uncorrelated `IN (SELECT ...)` (v0.16 review fix):
            // its values are computed once, before the first write.
            if let RExpr::InSub(_, p, _) = e
                && p.uncorrelated
                && p.id != 0
                && !update.as_ref().is_some_and(|u| u.from)
            {
                return Some(false);
            }
            if expr_refs_range(e, &all)
                || crate::query::any_scalar(e, &crate::query::ScalarFn::nondeterministic)
            {
                return None;
            }
            Some(false)
        };
        self.hint_solvable(arena, t, hint, &s, conj.as_slice())?;
        let ts = terms(conj.as_slice(), &s, &avail);
        if ts.is_empty() && !wr && !forced {
            return Ok(None);
        }
        let one_pass = update.as_ref().is_some_and(|u| {
            #[cfg(feature = "triggers")]
            let trig = self
                .catalog
                .triggers_on(ca, handle_bytes(ca, t.name))
                .any(|tr| tr.event == crate::sql::ast::TrigEvent::Update);
            #[cfg(not(feature = "triggers"))]
            let trig = false;
            let replace = u.replaces;
            // Changing the key (the rowid, or — v0.16 — a WITHOUT
            // ROWID table's PRIMARY KEY column) is two-pass, as on the
            // pin (its `chngKey`).
            #[cfg(feature = "without_rowid")]
            let pk = self
                .catalog
                .indexes_for(ca, handle_bytes(ca, t.name))
                .find(|i| i.clustered);
            #[cfg(feature = "without_rowid")]
            let in_pk = |i: usize| pk.is_some_and(|p| p.columns.iter().any(|c| c.col == Some(i)));
            #[cfg(not(feature = "without_rowid"))]
            let in_pk = |_: usize| false;
            let rowid = u.sets.iter().any(|(tg, _)| match tg {
                Target::Rowid => true,
                Target::Col(i) => Some(*i) == t.rowid_alias || in_pk(*i),
            });
            !u.returning
                && !u.from
                && !trig
                && !self.prag.foreign_keys
                && !replace
                && !rowid
                && !crate::query::has_sub(f)
        });
        let mut needed: AVec<bool> = AVec::new();
        for c in 0..t.columns.len() {
            let r = RefRange {
                w: (c, c + 1),
                s: (0, 0),
            };
            needed.push(expr_refs_range(f, &r));
        }
        let q = Query {
            conj: conj.as_slice(),
            order: &[],
            group: false,
            group_by: false,
            ncols_out: 0,
            // The pin's `colUsed`: a WITHOUT ROWID table's loop
            // covers the WHERE's columns (the SET and RETURNING
            // expressions are not counted).
            needed: if one_pass && !wr {
                None
            } else {
                Some(needed.as_slice())
            },
            cover_scan: false,
            limit: None,
            distinct: false,
            #[cfg(feature = "explain")]
            dset: &[],
        };
        let (width, models) = self.table_models(arena, t, &s);
        let notnull = self.not_null_cols(t);
        let tab = Table {
            width,
            indexes: models.as_slice(),
            wr: !t.has_rowid(),
            hint,
            notnull: notnull.as_slice(),
        };
        let Some(mut acc) = choose(&q, &s, &tab, ts.as_slice()) else {
            return Ok(None);
        };
        // A loop whose index the SET changes is not one-pass.
        let changed = |e: &RExpr| {
            update.as_ref().is_some_and(|u| {
                u.sets.iter().any(|(tg, _)| match tg {
                    Target::Col(i) => expr_refs_range(
                        e,
                        &RefRange {
                            w: (*i, *i + 1),
                            s: (0, 0),
                        },
                    ),
                    Target::Rowid => true,
                })
            })
        };
        let modified = models.iter().find(|m| m.root == acc.root).is_some_and(|m| {
            m.pred.as_ref().is_some_and(changed)
                || m.keys.iter().any(|k| match k {
                    IdxKey::Col(c) => update.as_ref().is_some_and(|u| {
                        u.sets
                            .iter()
                            .any(|(tg, _)| matches!(tg, Target::Col(i) if i == c))
                    }),
                    IdxKey::Rowid => false,
                    IdxKey::Expr(e) => changed(e),
                })
        });
        acc.one_pass = one_pass && !modified;
        // EXPLAIN QUERY PLAN's COVERING (v0.16, `explain`): a two-pass
        // loop reads only the WHERE's columns, as the pin's colUsed.
        #[cfg(feature = "explain")]
        {
            acc.eqp.1 = models.iter().find(|m| m.root == acc.root).is_some_and(|m| {
                !m.covers_all && !(acc.one_pass && !wr) && m.covers(needed.as_slice())
            });
        }
        Ok(Some(Box::new(acc)))
    }
}
