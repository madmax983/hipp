//! `json_each` / `json_tree` (and `jsonb_each` / `jsonb_tree`) as
//! FROM sources (v0.16, leaf feature `json`).
//!
//! A call plans as a source of the pin's ten columns (`key value
//! type atom id parent fullkey path`, then the hidden `json` and
//! `root`) with a rowid (the row number from 0). Its arguments
//! resolve against the whole FROM clause (`Connection::plan_tvf_args`,
//! from `plan_on`): one that reads only enclosing queries is
//! evaluated once per execution of the query, one that reads a
//! source to its left (`json_each(t.a)`, the common correlated form)
//! once per row of the sources before it, inside the join. The rows
//! themselves come from `json::tvf_rows`.

use super::*;
#[cfg(feature = "json")]
use crate::query::{SourceDesc, SubPlan};
use crate::query::{TVF_LATERAL, and_terms};

impl<S: Storage> Connection<S> {
    /// Plan FROM source `s`, a call of the JSON table-valued
    /// function `kind` (`json::tvf_kind`): its description and a
    /// placeholder plan (the arguments are resolved later, by
    /// `plan_tvf_args`). Returns its width.
    #[cfg(feature = "json")]
    pub(super) fn plan_json_tvf(
        &self,
        s: &crate::sql::FromSource,
        kind: u8,
        arena: &Arena,
        descs: &mut AVec<SourceDesc>,
        plans: &mut AVec<SourcePlan>,
    ) -> Result<usize, S::Error> {
        // A bare `json_each` takes its arguments from equality
        // constraints on the hidden columns on the pin; hipp does
        // not plan those.
        let Some(args) = s.args.as_ref() else {
            return Err(Error::Unsupported);
        };
        // The pin's "too many arguments on json_each() - max 2".
        if args.len() > 2 {
            return Err(Error::Schema {
                kind: SchemaKind::WrongArgumentCount,
            });
        }
        let mut cols = AVec::new();
        let mut names = AVec::new();
        for n in crate::json::TVF_COLUMNS {
            cols.push((copy_vec(n), crate::record::Affinity::Blob));
            names.push(copy_vec(n));
        }
        let qual = copy_vec(handle_bytes(arena, s.alias.unwrap_or(s.table)));
        let mut desc = SourceDesc::new(qual, cols, None, true);
        desc.kind = s.kind;
        desc.json_tvf = kind;
        // An eponymous table of main's (v0.16 review fix: `main.j.value`
        // was "no such column").
        desc.db = 1;
        descs.push(desc);
        let w = crate::json::TVF_COLUMNS.len();
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        let mut result_affs = AVec::new();
        let mut result_named = AVec::new();
        for _ in 0..w {
            affs.push(crate::record::Affinity::Blob);
            defaults.push(OwnedValue::Null);
            result_affs.push(None);
            result_named.push(false);
        }
        let holder = SubPlan {
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
            result_names: names,
            result_affs,
            result_named,
            id: 0,
            uncorrelated: false,
            scalar_col: 0,
            rec: None,
            mat: 0,
            #[cfg(feature = "explain")]
            sel: 0,
            #[cfg(feature = "flatten")]
            view_names: AVec::new(),
        };
        plans.push(SourcePlan {
            #[cfg(feature = "explain")]
            eqp: Default::default(),
            root: 0,
            kind: s.kind,
            on: None,
            rowid_alias: None,
            ncols: w,
            affs,
            defaults,
            derived: Some(Box::new(holder)),
            gcols: None,
            rec: 0,
            tvf: kind,
            tvf_dead: AVec::new(),
            #[cfg(feature = "vtab")]
            vt: None,
            #[cfg(feature = "expr_index")]
            access: None,
            #[cfg(feature = "expr_index")]
            hint: crate::query::Hint::Any,
            #[cfg(feature = "v16")]
            temp: false,
            #[cfg(feature = "attach")]
            ext: 0,
            #[cfg(feature = "flatten")]
            exists: false,
        });
        Ok(w)
    }

    /// Resolve the arguments of FROM source `i`, a JSON
    /// table-valued function, against the FROM clause's scope (the
    /// pin lets them read any source to the left); they read no
    /// source at or after it (hipp does not reorder the join to
    /// make that possible) and hold no aggregate.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_tvf_args(
        &self,
        s: &crate::sql::FromSource,
        i: usize,
        arena: &Arena,
        cur: &crate::query::ScopeGroup,
        enclosing: &[crate::query::ScopeGroup],
        plan: &mut SourcePlan,
        planner: crate::query::SubPlanner<'_, S::Error>,
    ) -> Result<(), S::Error> {
        use crate::query::{RefRange, expr_refs_range, has_agg, resolve_scoped};
        let n = cur.sources.len();
        let end = cur.offsets.as_slice().last().copied().unwrap_or(0)
            + cur.sources.as_slice().last().map_or(0, |s| s.columns.len());
        let first = cur.offsets.as_slice().first().copied().unwrap_or(end);
        let here = cur.offsets.as_slice().get(i).copied().unwrap_or(end);
        #[cfg(not(feature = "join_order"))]
        let later = RefRange {
            w: (here, end),
            s: (cur.first_src + i, cur.first_src + n),
        };
        let left = RefRange {
            w: (first, here),
            s: (cur.first_src, cur.first_src + i),
        };
        let mut row = AVec::new();
        let mut lateral = false;
        // v0.17 (`join_order`): an argument may read a source after
        // it, which the join planner then runs first (the pin's
        // reordering; `check_tvf_order` refuses what stays after).
        #[cfg(feature = "join_order")]
        let (later, after) = (
            RefRange {
                w: (
                    here,
                    cur.offsets.as_slice().get(i + 1).copied().unwrap_or(end),
                ),
                s: (cur.first_src + i, cur.first_src + i + 1),
            },
            RefRange {
                w: (
                    cur.offsets.as_slice().get(i + 1).copied().unwrap_or(end),
                    end,
                ),
                s: (cur.first_src + i + 1, cur.first_src + n),
            },
        );
        for a in s.args.iter().flat_map(|a| a.iter()) {
            let r = resolve_scoped::<S::Error>(a, arena, cur, enclosing, &mut *planner)?;
            if has_agg(&r) || expr_refs_range(&r, &later) {
                return Err(Error::Unsupported);
            }
            lateral |= expr_refs_range(&r, &left);
            #[cfg(feature = "join_order")]
            {
                lateral |= expr_refs_range(&r, &after);
            }
            row.push(r);
        }
        // A lateral call on the right of a RIGHT / FULL join would
        // need its rows before the join runs.
        if lateral && s.kind.pads_left() {
            return Err(Error::Unsupported);
        }
        if lateral {
            plan.tvf |= TVF_LATERAL;
        }
        if let Some(d) = plan.derived.as_deref_mut() {
            d.values.clear();
            d.values.push(row);
        }
        Ok(())
    }
}

/// A JSON table-valued source whose arguments read a source after it
/// (v0.17, `join_order`): the join planner puts the sources they read
/// first where it may; where it may not (a CROSS JOIN keeps the call
/// before them) the pin offers xBestIndex no constraint for such an
/// argument, so the call runs without it — no JSON argument, no rows;
/// no root argument, the root `$` — and the argument's
/// `hidden column = value` term stays a WHERE term (review fix: it was
/// refused). `filter` takes those terms. A source the planner does not
/// order (`ordered` false: a RIGHT / FULL join, ...) is still refused.
#[cfg(all(feature = "join_order", feature = "json"))]
pub(super) fn fix_tvf_order<E>(
    sources: &mut [SourcePlan],
    cur: &crate::query::ScopeGroup,
    filter: &mut Option<RExpr>,
    ordered: bool,
) -> Result<(), Error<E>>
where
    Error<E>: Sized,
{
    use crate::query::{RefRange, expr_refs_range};
    let n = sources.len();
    let end =
        cur.offsets.as_slice().last().copied().unwrap_or(0) + sources.last().map_or(0, |s| s.ncols);
    for (i, sp) in sources.iter_mut().enumerate() {
        #[cfg(feature = "vtab")]
        if sp.tvf & crate::query::TVF_VTAB != 0 || sp.vt.is_some() {
            continue;
        }
        if sp.tvf == 0 {
            continue;
        }
        let here = cur.offsets.as_slice().get(i).copied().unwrap_or(end);
        let later = RefRange {
            w: (here, end),
            s: (cur.first_src + i, cur.first_src + n),
        };
        let Some(args) = sp
            .derived
            .as_deref_mut()
            .and_then(|d| d.values.as_mut_slice().first_mut())
        else {
            continue;
        };
        let reads: AVec<bool> = args.iter().map(|a| expr_refs_range(a, &later)).collect();
        if !reads.iter().any(|r| *r) {
            continue;
        }
        if !ordered {
            return Err(Error::Unsupported);
        }
        // (`json` and `root` are the hidden columns 8 and 9.)
        let keep = if reads.as_slice().first().copied().unwrap_or(false) {
            0
        } else {
            1
        };
        let mut k = args.len();
        while k > keep {
            k -= 1;
            let Some(a) = args.pop() else { break };
            if !reads.as_slice().get(k).copied().unwrap_or(false) && k > 0 {
                // (Dropped with the JSON argument: no rows to test.)
                continue;
            }
            let col = RExpr::Column(
                here + 8 + k,
                crate::record::Affinity::Blob,
                Some(crate::record::Coll::default()),
            );
            let term = RExpr::Binary(crate::sql::BinOp::Eq, Box::new(col), Box::new(a));
            *filter = Some(match filter.take() {
                None => term,
                Some(f) => and_expr(Some(f), term),
            });
        }
    }
    Ok(())
}

/// Fill each lateral JSON table-valued source's `tvf_dead`
/// (v0.16, `json`): the slots of the core's sources before it
/// (`offsets`, the first slot of each source) that its arguments
/// read and nothing else does — no result item, WHERE, GROUP BY,
/// HAVING or ORDER BY term (`rest`), no ON clause, no other
/// source's plan, no index probe. The join clears them once the
/// call has run: without that each of a document's rows carried
/// the whole document along with the row of the table it came
/// from (O(rows × size) memory for `FROM t, json_each(t.j)`).
pub(super) fn plan_tvf_dead(sources: &mut [SourcePlan], offsets: &[usize], rest: &[&RExpr]) {
    use crate::query::{RefRange, expr_refs_range, plan_refs_range};
    let first = offsets.first().copied().unwrap_or(0);
    for k in 0..sources.len() {
        let Some(sp) = sources.get(k) else {
            continue;
        };
        if sp.tvf & TVF_LATERAL == 0 {
            continue;
        }
        let here = offsets.get(k).copied().unwrap_or(first);
        let mut dead = AVec::new();
        for slot in first..here {
            let r = RefRange {
                w: (slot, slot + 1),
                s: (0, 0),
            };
            let args = sp
                .derived
                .as_deref()
                .and_then(|d| d.values.as_slice().first());
            if !args.is_some_and(|a| a.iter().any(|e| expr_refs_range(e, &r))) {
                continue;
            }
            let elsewhere = rest.iter().any(|e| expr_refs_range(e, &r))
                || sources.iter().enumerate().any(|(j, s)| {
                    #[cfg(feature = "expr_index")]
                    if s.access.as_deref().is_some_and(|a| a.lateral) {
                        return true;
                    }
                    s.on.as_ref().is_some_and(|e| expr_refs_range(e, &r))
                        || (j != k && s.derived.as_deref().is_some_and(|d| plan_refs_range(d, &r)))
                });
            if !elsewhere {
                dead.push(here - slot);
            }
        }
        if let Some(sp) = sources.get_mut(k) {
            sp.tvf_dead = dead;
        }
    }
}

impl<S: Storage> Statement<'_, S> {
    /// The rows of the lateral JSON table-valued source `idx` for
    /// the combination `acc` of the sources before it. The pin
    /// tests the WHERE terms (and this join's ON terms) that read
    /// only those sources before it runs the function, so a row
    /// they reject never reaches it (`FROM t, json_each(t.j) WHERE
    /// json_valid(t.j)` skips t's malformed rows): None when one
    /// fails (the combination yields nothing).
    pub(super) async fn lateral_rows(
        &mut self,
        sources: &[SourcePlan],
        idx: usize,
        filter: Option<&RExpr>,
        acc: &RowData,
    ) -> Result<Option<AVec<(AVec<OwnedValue>, Option<i64>)>>, S::Error> {
        let Some(sp) = sources.get(idx) else {
            return Ok(Some(AVec::new()));
        };
        if !self.lateral_ok(sources, idx, filter, acc).await? {
            return Ok(None);
        }
        let mut padded = acc.clone();
        pad_sources(&mut padded, sources.get(idx..).unwrap_or(&[]));
        self.json_tvf_rows(sp, &padded, None).await.map(Some)
    }

    /// Whether the WHERE terms (and this join's ON terms) that read
    /// only the sources before lateral source `idx` hold for their
    /// combination `acc` (`lateral_rows`; v0.17: the streamed join
    /// opens the source's cursor only then).
    pub(super) async fn lateral_ok(
        &mut self,
        sources: &[SourcePlan],
        idx: usize,
        filter: Option<&RExpr>,
        acc: &RowData,
    ) -> Result<bool, S::Error> {
        use crate::query::{RefRange, expr_refs_range};
        let Some(sp) = sources.get(idx) else {
            return Ok(true);
        };
        let rest = sources.get(idx..).unwrap_or(&[]);
        let w = acc.vals.len();
        let n = acc.rowids.len();
        let later = RefRange {
            w: (w, w + rest.iter().map(|s| s.ncols).sum::<usize>()),
            s: (n, n + rest.len()),
        };
        let mut padded = acc.clone();
        pad_sources(&mut padded, rest);
        let mut terms = AVec::new();
        if let Some(f) = filter {
            and_terms(f, &mut terms);
        }
        for t in terms.iter() {
            if !expr_refs_range(t, &later) && !self.cond_row_sub(t, &padded).await? {
                return Ok(false);
            }
        }
        // An inner join's ON is a WHERE term on the pin; a LEFT
        // JOIN's runs the function and tests its ON per row.
        let mut on = AVec::new();
        if let Some(o) = &sp.on
            && !sp.kind.pads_right()
        {
            and_terms(o, &mut on);
        }
        for t in on.iter() {
            if !expr_refs_range(t, &later) && !self.cond_row_sub(t, &padded).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The rows of JSON table-valued source `sp` for the row `row`
    /// (the enclosing queries' values, then the sources before it,
    /// the rest NULL-padded): its arguments evaluated, then
    /// `json::tvf_rows` (at most `need` rows: a reader that stops
    /// early, as a scalar subquery's first row).
    pub(super) async fn json_tvf_rows(
        &mut self,
        sp: &SourcePlan,
        row: &RowData,
        need: Option<usize>,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        // A virtual table's rows (v0.16, `vtab`).
        #[cfg(feature = "vtab")]
        if let Some(vt) = sp.vt.as_deref() {
            return self.vtab_rows(sp, vt, row, need).await;
        }
        let mut args = AVec::new();
        if let Some(d) = sp.derived.as_deref()
            && let Some(r) = d.values.as_slice().first()
        {
            for a in r.iter() {
                args.push(self.eval_row_sub(a, row).await?);
            }
        }
        // A UTF-16 database's document or path BLOB decodes from
        // UTF-16 (v0.16, `utf16`).
        #[cfg(all(feature = "utf16", feature = "json"))]
        for (i, v) in args.as_mut_slice().iter_mut().enumerate() {
            crate::query::json_arg16(v, |i| i < 2, i, self.conn.pager.text_enc());
        }
        #[cfg(not(feature = "json"))]
        {
            let _ = need;
            Ok(AVec::new())
        }
        #[cfg(feature = "json")]
        crate::json::tvf_rows(
            sp.tvf & !TVF_LATERAL,
            args.as_slice(),
            need,
            &self.conn.env.json_docs,
        )
        .map_err(|kind| Error::Schema { kind })
    }
}
