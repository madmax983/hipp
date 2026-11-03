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
use crate::query::{SourceDesc, SubPlan, TVF_LATERAL};

impl<S: Storage> Connection<S> {
    /// Plan FROM source `s`, a call of the JSON table-valued
    /// function `kind` (`json::tvf_kind`): its description and a
    /// placeholder plan (the arguments are resolved later, by
    /// `plan_tvf_args`). Returns its width.
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
        };
        plans.push(SourcePlan {
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
        for a in s.args.iter().flat_map(|a| a.iter()) {
            let r = resolve_scoped::<S::Error>(a, arena, cur, enclosing, &mut *planner)?;
            if has_agg(&r) || expr_refs_range(&r, &later) {
                return Err(Error::Unsupported);
            }
            lateral |= expr_refs_range(&r, &left);
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

/// The AND terms of `e`.
fn and_terms<'e>(e: &'e RExpr, out: &mut AVec<&'e RExpr>) {
    match e {
        RExpr::Binary(BinOp::And, l, r) => {
            and_terms(l, out);
            and_terms(r, out);
        }
        _ => out.push(e),
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
        use crate::query::{RefRange, expr_refs_range, truthy};
        let Some(sp) = sources.get(idx) else {
            return Ok(Some(AVec::new()));
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
            if !expr_refs_range(t, &later)
                && truthy(&self.eval_row_sub(t, &padded).await?) != Some(true)
            {
                return Ok(None);
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
            if !expr_refs_range(t, &later)
                && truthy(&self.eval_row_sub(t, &padded).await?) != Some(true)
            {
                return Ok(None);
            }
        }
        self.json_tvf_rows(sp, &padded, None).await.map(Some)
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
        let mut args = AVec::new();
        if let Some(d) = sp.derived.as_deref()
            && let Some(r) = d.values.as_slice().first()
        {
            for a in r.iter() {
                args.push(self.eval_row_sub(a, row).await?);
            }
        }
        crate::json::tvf_rows(sp.tvf & !TVF_LATERAL, args.as_slice(), need)
            .map_err(|kind| Error::Schema { kind })
    }
}
