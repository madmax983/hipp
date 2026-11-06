//! The join planner's session side (v0.17, `join_order`; the
//! planning itself is `crate::xjoin`).
//!
//! A SELECT core whose FROM clause holds two or more plain tables
//! (rowid or WITHOUT ROWID, of the main database; no RIGHT / FULL
//! join, no window or merge key to deliver) is planned as one join:
//! `plan_join` builds each source's model and terms, asks
//! `xjoin::plan` for the loops in the pin's order, and then lays the
//! core out in that order — the sources permuted, every expression
//! of the core (and of the subqueries inside it) renumbered to the
//! new slots (`query::remap_plan`), the scope's offsets moved — so
//! the executors run the chosen order unchanged. An inner join's ON
//! terms are evaluated where their sources are available (on the
//! latest of them, else with the WHERE clause); a LEFT JOIN keeps its
//! ON clause, and a LEFT JOIN the WHERE clause makes inner (the
//! pin's `sqlite3ExprImpliesNonNullRow` simplification) becomes an
//! inner join. Each source then gets its loop's access path: an
//! index or rowid loop as `xplan` lowers it, or an automatic index
//! (`xplan::AutoIdx`), which the executors build once per statement
//! by sorting the table's rows on the index key (`auto_rows`) and
//! probe by binary search, after the Bloom filter the pin also
//! builds (`bloom_*`).

use super::*;
use crate::query::{RefRange, expr_refs_range};
use crate::xjoin::{JQuery, JSrc};
use crate::xplan::{Src, TOp, Table, Term};

/// The Bloom filter's size in bytes (the pin's `OP_Blob 10000`).
const BLOOM_BYTES: usize = 10_000;

/// The pin's `filterHash` of one key: integers (and integral reals)
/// by value, every string alike, every blob alike, NULL as 0.
fn bloom_hash(vals: &[OwnedValue]) -> u64 {
    let mut h: u64 = 0;
    for v in vals.iter() {
        let x: u64 = match v {
            OwnedValue::Integer(i) => *i as u64,
            OwnedValue::Real(r) => *r as i64 as u64,
            OwnedValue::Text(_) => 4093 + 2,
            OwnedValue::Blob(_) => 4093 + 16,
            OwnedValue::Null => 0,
        };
        h = h.wrapping_add(x);
    }
    h
}

/// A Bloom filter over the key columns of `rows` (v0.17).
pub(super) fn bloom_build(
    rows: &[(AVec<OwnedValue>, Option<i64>)],
    ai: &crate::xplan::AutoIdx,
) -> AVec<u8> {
    let mut bits: AVec<u8> = AVec::new();
    for _ in 0..BLOOM_BYTES {
        bits.push(0);
    }
    let mut key: AVec<OwnedValue> = AVec::new();
    for (vals, _) in rows.iter() {
        key.clear();
        for k in ai.keys.iter() {
            key.push(
                vals.as_slice()
                    .get(k.col)
                    .cloned()
                    .unwrap_or(OwnedValue::Null),
            );
        }
        let h = bloom_hash(key.as_slice()) % (BLOOM_BYTES as u64 * 8);
        if let Some(b) = bits.as_mut_slice().get_mut((h / 8) as usize) {
            *b |= 1 << (h % 8);
        }
    }
    bits
}

/// Whether probe `key` may find a row (`OP_Filter`).
pub(super) fn bloom_may(bits: &[u8], key: &[OwnedValue]) -> bool {
    if bits.is_empty() {
        return true;
    }
    let h = bloom_hash(key) % (BLOOM_BYTES as u64 * 8);
    bits.get((h / 8) as usize)
        .is_none_or(|b| b & (1 << (h % 8)) != 0)
}

/// The automatic index's row order: its key columns under their
/// collations, then the other columns it holds (BINARY), then the
/// rowid.
fn auto_cmp(
    a: &(AVec<OwnedValue>, Option<i64>),
    b: &(AVec<OwnedValue>, Option<i64>),
    ai: &crate::xplan::AutoIdx,
    enc: crate::record::TextEnc,
) -> core::cmp::Ordering {
    let get = |r: &(AVec<OwnedValue>, Option<i64>), c: usize| {
        r.0.as_slice().get(c).cloned().unwrap_or(OwnedValue::Null)
    };
    for k in ai.keys.iter() {
        let o = crate::record::compare_coll_in(
            &get(a, k.col).as_value(),
            &get(b, k.col).as_value(),
            k.coll,
            enc,
        );
        if o != core::cmp::Ordering::Equal {
            return o;
        }
    }
    for c in ai.extra.iter() {
        let o = crate::record::compare_coll_in(
            &get(a, *c).as_value(),
            &get(b, *c).as_value(),
            crate::record::Coll::Binary,
            enc,
        );
        if o != core::cmp::Ordering::Equal {
            return o;
        }
    }
    a.1.cmp(&b.1)
}

/// Sort a table's rows into automatic-index order (v0.17).
pub(super) fn auto_sort(
    rows: &mut [(AVec<OwnedValue>, Option<i64>)],
    ai: &crate::xplan::AutoIdx,
    enc: crate::record::TextEnc,
) {
    rows.sort_by(|a, b| auto_cmp(a, b, ai, enc));
}

/// The span of `rows` (in automatic-index order) whose key equals
/// `probe` (converted values, one per key column): binary searched.
pub(super) fn auto_span(
    rows: &[(AVec<OwnedValue>, Option<i64>)],
    ai: &crate::xplan::AutoIdx,
    probe: &[OwnedValue],
    enc: crate::record::TextEnc,
) -> (usize, usize) {
    let cmp = |r: &(AVec<OwnedValue>, Option<i64>)| {
        for (k, p) in ai.keys.iter().zip(probe.iter()) {
            let v =
                r.0.as_slice()
                    .get(k.col)
                    .cloned()
                    .unwrap_or(OwnedValue::Null);
            let o = crate::record::compare_coll_in(&v.as_value(), &p.as_value(), k.coll, enc);
            if o != core::cmp::Ordering::Equal {
                return o;
            }
        }
        core::cmp::Ordering::Equal
    };
    let lo = rows.partition_point(|r| cmp(r) == core::cmp::Ordering::Less);
    let hi = rows.partition_point(|r| cmp(r) != core::cmp::Ordering::Greater);
    (lo, hi.max(lo))
}

impl<'c, S: Storage> Statement<'c, S> {
    /// An automatic index's probe for the row `row`: each key's value,
    /// converted by the comparison's affinity; `None` when a NULL
    /// meets `=` (no row compares equal to it).
    pub(super) async fn auto_probe(
        &mut self,
        ai: &crate::xplan::AutoIdx,
        row: &RowData,
    ) -> Result<Option<AVec<OwnedValue>>, S::Error> {
        let mut out = AVec::new();
        for k in ai.keys.iter() {
            let v = crate::query::probe_value(self.eval_row_sub(&k.val, row).await?, k.aff);
            if matches!(v, OwnedValue::Null) && !k.is {
                return Ok(None);
            }
            out.push(v);
        }
        Ok(Some(out))
    }

    /// The rows of source `idx` (`rows`, in automatic-index order) an
    /// automatic-index probe of `row` finds: the Bloom filter (built
    /// at the first probe into `bloom`) first, then the key's span.
    pub(super) async fn auto_lookup(
        &mut self,
        ai: &crate::xplan::AutoIdx,
        rows: &AVec<(AVec<OwnedValue>, Option<i64>)>,
        idx: usize,
        bloom: &mut AVec<Option<AVec<u8>>>,
        row: &RowData,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let mut out = AVec::new();
        let Some(p) = self.auto_probe(ai, row).await? else {
            return Ok(out);
        };
        if ai.bloom {
            while bloom.len() <= idx {
                bloom.push(None);
            }
            if let Some(slot) = bloom.as_mut_slice().get_mut(idx) {
                let bits = slot.get_or_insert_with(|| bloom_build(rows.as_slice(), ai));
                if !bloom_may(bits.as_slice(), p.as_slice()) {
                    return Ok(out);
                }
            }
        }
        let (lo, hi) = auto_span(
            rows.as_slice(),
            ai,
            p.as_slice(),
            self.conn.pager.text_enc(),
        );
        for r in rows.as_slice().get(lo..hi).unwrap_or(&[]).iter() {
            out.push(r.clone());
        }
        Ok(out)
    }

    /// The Bloom filter of a lookup loop (v0.17; the pin's
    /// `sqlite3ConstructBloomFilter`): the key of every table row.
    /// Empty (admitting every probe) for an index expression key.
    pub(super) async fn bloom_rows(
        &mut self,
        sp: &SourcePlan,
        keys: &[crate::xplan::BKey],
    ) -> Result<AVec<u8>, S::Error> {
        use crate::xplan::BKey;
        if keys.contains(&BKey::Expr) {
            return Ok(AVec::new());
        }
        let rows = self.scan_source(sp).await?;
        let mut bits: AVec<u8> = AVec::new();
        for _ in 0..BLOOM_BYTES {
            bits.push(0);
        }
        let mut key: AVec<OwnedValue> = AVec::new();
        for (vals, rowid) in rows.iter() {
            key.clear();
            for k in keys.iter() {
                key.push(match k {
                    BKey::Col(c) => vals.as_slice().get(*c).cloned().unwrap_or(OwnedValue::Null),
                    _ => rowid.map_or(OwnedValue::Null, OwnedValue::Integer),
                });
            }
            let h = bloom_hash(key.as_slice()) % (BLOOM_BYTES as u64 * 8);
            if let Some(b) = bits.as_mut_slice().get_mut((h / 8) as usize) {
                *b |= 1 << (h % 8);
            }
        }
        Ok(bits)
    }

    /// Whether the lookup `a` (with a Bloom filter `bits`) may find a
    /// row for `row` (`OP_Filter`): its equality probes' key is in the
    /// filter. An IN or subquery probe, or a rowid probe that is no
    /// number, is not tested.
    pub(super) async fn bloom_pass(
        &mut self,
        a: &crate::xplan::IdxAccess,
        bits: &[u8],
        row: &RowData,
    ) -> Result<bool, S::Error> {
        let Some(keys) = a.bloom.as_ref() else {
            return Ok(true);
        };
        if bits.is_empty() {
            return Ok(true);
        }
        let mut key: AVec<OwnedValue> = AVec::new();
        for (k, p) in keys.iter().zip(a.eq.iter()) {
            let ([e], None, false) = (p.vals.as_slice(), &p.sub, p.is_in) else {
                return Ok(true);
            };
            let v = crate::query::probe_value(self.eval_row_sub(e, row).await?, p.aff);
            if *k == crate::xplan::BKey::Rowid
                && !matches!(v, OwnedValue::Integer(_) | OwnedValue::Real(_))
            {
                return Ok(true);
            }
            key.push(v);
        }
        Ok(bloom_may(bits, key.as_slice()))
    }

    /// A source's rows in automatic-index order (built once per
    /// statement: the table scanned, then sorted).
    pub(super) async fn auto_rows(
        &mut self,
        sp: &SourcePlan,
        ai: &crate::xplan::AutoIdx,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let mut rows = self.scan_source(sp).await?;
        auto_sort(rows.as_mut_slice(), ai, self.conn.pager.text_enc());
        Ok(rows)
    }
}

/// One `col = constant` term of the pin's constant propagation
/// (`findConstInWhere`): the column's slot, the constant with the
/// column's affinity applied (`EP_FixedCol` codegen), the column's
/// affinity and collation, and where it was found (that occurrence
/// itself is not rewritten).
struct Fixed {
    slot: usize,
    val: OwnedValue,
    aff: crate::record::Affinity,
    coll: Option<crate::record::Coll>,
    at: usize,
}

/// A literal constant without affinity (`sqlite3ExprIsConstant` and
/// `sqlite3ExprAffinity(pValue) == 0`), as its value.
fn const_lit(e: &RExpr) -> Option<OwnedValue> {
    match e {
        RExpr::Literal(v) => Some(v.clone()),
        RExpr::Neg(x) => match &**x {
            RExpr::Literal(OwnedValue::Integer(i)) => i.checked_neg().map(OwnedValue::Integer),
            RExpr::Literal(OwnedValue::Real(r)) => Some(OwnedValue::Real(-*r)),
            _ => None,
        },
        _ => None,
    }
}

/// The pin's WHERE-clause constant propagation (`propagateConstants`)
/// as the planner sees it: each term of `conj` with the columns a
/// top-level `col = constant` term fixes replaced by that constant
/// (`RExpr::AffLiteral`, the column's affinity and collation kept),
/// so a join term `a = b` over a fixed `b` probes `a` with the
/// constant and no longer ties `a`'s table to `b`'s. Only the
/// planner reads the copies: the executors evaluate the clauses as
/// written. A LEFT JOIN's ON terms (`excl`) neither fix nor are
/// rewritten; a term under a COLLATE / likely() wrapper (`nofix`)
/// fixes nothing.
fn propagate(conj: &[&RExpr], excl: &[bool], nofix: &[bool], srcs: &[Src]) -> AVec<RExpr> {
    let mut fixed: AVec<Fixed> = AVec::new();
    // (Only a WHERE clause of two or more AND terms, ON clauses
    // included: `p->pWhere->op==TK_AND`.)
    let active = conj.len() >= 2;
    // `findConstInWhere` walks the AND tree right to left.
    for (ci, c) in conj.iter().enumerate().rev() {
        // (A term under a COLLATE or likely() wrapper fixes nothing:
        // `findConstInWhere` looks at `=` and AND nodes only.)
        if !active
            || excl.get(ci).copied().unwrap_or(true)
            || nofix.get(ci).copied().unwrap_or(true)
        {
            continue;
        }
        let RExpr::Binary(crate::sql::BinOp::Eq, l, r) = c else {
            continue;
        };
        if crate::query::binary_coll(l, r) != crate::record::Coll::Binary {
            continue;
        }
        for (col, val) in [(&**r, &**l), (&**l, &**r)] {
            let Some((nd, aff, coll)) = col_node(col, srcs) else {
                continue;
            };
            let Some(v) = const_lit(val) else { continue };
            if fixed.iter().any(|f| f.slot == nd) {
                continue;
            }
            let v = if matches!(aff, crate::record::Affinity::Blob) {
                v
            } else {
                crate::record::apply_affinity(v, aff)
            };
            fixed.push(Fixed {
                slot: nd,
                val: v,
                aff,
                coll,
                at: ci,
            });
        }
    }
    let mut out = AVec::new();
    let blob = fixed
        .iter()
        .any(|f| matches!(f.aff, crate::record::Affinity::Blob));
    for (ci, c) in conj.iter().enumerate() {
        let mut e = (*c).clone();
        if !fixed.is_empty() && !excl.get(ci).copied().unwrap_or(true) {
            rewrite(&mut e, fixed.as_slice(), srcs, blob, ci, true, false);
        }
        out.push(e);
    }
    out
}

/// `propagateConstantExprRewriteOne`: `e` becomes its constant when it
/// is a fixed column (not the fixing occurrence itself, and not a
/// BLOB-affinity column when `ignore_blob`).
fn rewrite_one(
    e: &mut RExpr,
    fixed: &[Fixed],
    srcs: &[Src],
    ignore_blob: bool,
    ci: usize,
    top: bool,
) -> bool {
    if !matches!(e, RExpr::Column(..) | RExpr::Rowid(_)) {
        return false;
    }
    let Some(nd) = col_node(e, srcs).map(|x| x.0) else {
        return true;
    };
    let Some(f) = fixed.iter().find(|f| f.slot == nd) else {
        return true;
    };
    if f.at == ci && top {
        return true;
    }
    if ignore_blob && matches!(f.aff, crate::record::Affinity::Blob) {
        return true;
    }
    *e = RExpr::AffLiteral(f.val.clone(), Some(f.aff), f.coll);
    true
}

/// `propagateConstantExprRewrite` over `e` (pre-order). `top`: `e`
/// is the term itself; `under_top`: `e` is one of its operands (the
/// fixing occurrence can only be one of those).
fn rewrite(
    e: &mut RExpr,
    fixed: &[Fixed],
    srcs: &[Src],
    blob: bool,
    ci: usize,
    top: bool,
    under_top: bool,
) {
    use crate::sql::BinOp;
    if blob
        && let RExpr::Binary(
            BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Is,
            l,
            r,
        ) = e
    {
        rewrite_one(l, fixed, srcs, false, ci, top);
        if crate::query::item_result_affinity(l) != Some(crate::record::Affinity::Text) {
            rewrite_one(r, fixed, srcs, false, ci, top);
        }
    }
    if rewrite_one(e, fixed, srcs, blob, ci, under_top) {
        return;
    }
    let mut kids: AVec<&mut RExpr> = AVec::new();
    match e {
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            kids.push(a);
            kids.push(b);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => kids.push(a),
        RExpr::InList(x, l, ..) => {
            kids.push(x);
            for i in l.as_mut_slice() {
                kids.push(i);
            }
        }
        RExpr::Between(x, a, b, _) => {
            kids.push(x);
            kids.push(a);
            kids.push(b);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            for b in base.iter_mut().chain(otherwise.iter_mut()) {
                kids.push(b);
            }
            for (w, t) in whens.as_mut_slice() {
                kids.push(w);
                kids.push(t);
            }
        }
        RExpr::Scalar(_, args) => {
            for a in args.as_mut_slice() {
                kids.push(a);
            }
        }
        _ => {}
    }
    for k in kids {
        rewrite(k, fixed, srcs, blob, ci, false, top);
    }
}

/// A rowid's node id in the equivalence classes (columns use their
/// slot).
const ROWID_NODE: usize = usize::MAX / 2;

/// A column (or rowid) of the core as a constant-propagation node,
/// with its affinity and collation.
fn col_node(
    e: &RExpr,
    srcs: &[Src],
) -> Option<(usize, crate::record::Affinity, Option<crate::record::Coll>)> {
    let nd = node(e, srcs)?;
    match e {
        RExpr::Column(_, aff, coll) => Some((nd, *aff, *coll)),
        _ => Some((nd, crate::record::Affinity::Integer, None)),
    }
}

/// The equivalence node of `e`: a column's slot, or a rowid (the
/// INTEGER PRIMARY KEY column is its table's rowid).
fn node(e: &RExpr, srcs: &[Src]) -> Option<usize> {
    match e {
        RExpr::Rowid(s) => srcs
            .iter()
            .any(|x| x.src == *s)
            .then_some(ROWID_NODE.saturating_add(*s)),
        RExpr::Column(slot, ..) => {
            let x = srcs
                .iter()
                .find(|x| *slot >= x.base && *slot < x.base + x.ncols)?;
            if x.rowid_alias == Some(*slot - x.base) {
                Some(ROWID_NODE.saturating_add(x.src))
            } else {
                Some(*slot)
            }
        }
        _ => None,
    }
}

/// The `x = y` terms between two columns that make them equivalent
/// (`termIsEquivalence`): `=` or `IS`, no LEFT JOIN ON term, no
/// explicit COLLATE, the same affinity (or both numeric) and
/// collation. Each as `(node x, node y, x's expression, y's)`.
fn equivalences(
    conj: &[&RExpr],
    on_of: &[Option<usize>],
    srcs: &[Src],
) -> AVec<(usize, usize, RExpr, RExpr)> {
    use crate::record::Affinity;
    let mut out = AVec::new();
    for (ci, c) in conj.iter().enumerate() {
        if on_of.get(ci).copied().flatten().is_some() {
            continue;
        }
        let RExpr::Binary(crate::sql::BinOp::Eq | crate::sql::BinOp::Is, l, r) = c else {
            continue;
        };
        let (Some(a), Some(b)) = (node(l, srcs), node(r, srcs)) else {
            continue;
        };
        if a == b {
            continue;
        }
        let aff = |e: &RExpr| crate::query::item_result_affinity(e).unwrap_or(Affinity::Blob);
        let coll = |e: &RExpr| match e {
            RExpr::Column(_, _, c) => c.unwrap_or_default(),
            _ => crate::record::Coll::Binary,
        };
        let num = |x: Affinity| matches!(x, Affinity::Integer | Affinity::Real | Affinity::Numeric);
        let (aa, ba) = (aff(l), aff(r));
        if aa != ba && !(num(aa) && num(ba)) {
            continue;
        }
        if coll(l) != coll(r) {
            continue;
        }
        out.push((a, b, (**l).clone(), (**r).clone()));
    }
    out
}

/// Node `x`'s equivalence chain (the pin keeps at most 11), `x`
/// first.
fn chain(eq: &[(usize, usize, RExpr, RExpr)], x: usize) -> AVec<usize> {
    let mut out: AVec<usize> = AVec::new();
    out.push(x);
    let mut k = 0usize;
    while k < out.len() && out.len() < 11 {
        let cur = out.as_slice().get(k).copied().unwrap_or(x);
        for (a, b, _, _) in eq.iter() {
            let other = if *a == cur {
                *b
            } else if *b == cur {
                *a
            } else {
                continue;
            };
            if !out.iter().any(|o| *o == other) && out.len() < 11 {
                out.push(other);
            }
        }
        k += 1;
    }
    out
}

/// The expression of node `x` as an equivalence term holds it.
fn equiv_key(eq: &[(usize, usize, RExpr, RExpr)], x: usize) -> Option<RExpr> {
    for (a, b, ea, eb) in eq.iter() {
        if *a == x {
            return Some(ea.clone());
        }
        if *b == x {
            return Some(eb.clone());
        }
    }
    None
}

/// The sources `e` reads, as a mask over the core's sources.
fn src_mask(e: &RExpr, bands: &[RefRange]) -> u64 {
    let mut m = 0u64;
    for (i, b) in bands.iter().enumerate() {
        if expr_refs_range(e, b) {
            m |= 1u64 << i;
        }
    }
    m
}

/// The sources a term's value reads.
fn term_mask(t: &Term, bands: &[RefRange]) -> u64 {
    match &t.op {
        TOp::In(items) => items.iter().fold(0, |m, e| m | src_mask(e, bands)),
        TOp::InSub(e) => src_mask(e, bands),
        _ => src_mask(&t.val, bands),
    }
}

/// Whether FROM source `sp` is one the join planner orders: a table
/// of the main database, a virtual table, or a JSON table-valued
/// function (v0.17); no RIGHT / FULL join.
pub(super) fn join_source_ok(sp: &SourcePlan) -> bool {
    if sp.kind.pads_left() || sp.rec != 0 || sp.temp {
        return false;
    }
    #[cfg(feature = "attach")]
    if sp.ext != 0 {
        return false;
    }
    if virtual_source(sp) {
        return true;
    }
    #[cfg(any(feature = "json", feature = "vtab"))]
    if sp.tvf != 0 {
        return false;
    }
    sp.derived.is_none() && sp.root > 1
}

/// A virtual-table or JSON table-valued source (v0.17).
fn virtual_source(sp: &SourcePlan) -> bool {
    #[cfg(not(any(feature = "json", feature = "vtab")))]
    let _ = sp;
    #[cfg(feature = "vtab")]
    if sp.vt.is_some() {
        return true;
    }
    #[cfg(feature = "json")]
    {
        #[cfg(feature = "vtab")]
        let vt = sp.tvf & crate::query::TVF_VTAB != 0;
        #[cfg(not(feature = "vtab"))]
        let vt = false;
        if sp.tvf != 0 && !vt && sp.derived.is_some() {
            return true;
        }
    }
    false
}

/// Whether the join planner takes core `sources` (two or more, each
/// `join_source_ok`).
pub(super) fn join_eligible(sources: &[SourcePlan]) -> bool {
    (2..=64).contains(&sources.len()) && sources.iter().all(join_source_ok)
}

impl<S: Storage> Connection<S> {
    /// The loops of virtual-table (or JSON table-valued) source `i`
    /// (v0.17; the pin's `whereLoopAddVirtual`): its xBestIndex asked
    /// with every constraint usable, then with those of each distinct
    /// set of other sources, then with none; each plan accepted is a
    /// loop `(the sources its used constraints read, rRun from the
    /// estimatedCost, nOut from the estimatedRows)`. Constraints that
    /// read a source past a later LEFT / CROSS JOIN (`unusable`) are
    /// not offered.
    #[cfg(any(feature = "json", feature = "vtab"))]
    #[allow(clippy::too_many_arguments)]
    fn vt_loops(
        &self,
        sp: &SourcePlan,
        i: usize,
        s: &Src,
        conj: &[&RExpr],
        on_of: &[Option<usize>],
        bands: &[RefRange],
        unusable: u64,
    ) -> AVec<(u64, i32, i32)> {
        use crate::xplan::{log_est, log_est_from_double};
        let me = 1u64 << i;
        let mut out: AVec<(u64, i32, i32)> = AVec::new();
        #[cfg(not(feature = "vtab"))]
        let _ = (conj, on_of, s);
        #[cfg(feature = "vtab")]
        let col_of = |e: &RExpr| -> Option<i32> {
            match e {
                RExpr::Column(slot, ..) if *slot >= s.base && *slot < s.base + s.ncols => {
                    i32::try_from(*slot - s.base).ok()
                }
                RExpr::Rowid(src) if *src == s.src => Some(-1),
                _ => None,
            }
        };
        let args: &[RExpr] = sp
            .derived
            .as_deref()
            .and_then(|d| d.values.as_slice().first())
            .map_or(&[][..], |r| r.as_slice());
        // (constraint column, operator, sources its value reads)
        #[cfg(feature = "vtab")]
        let mut cands: AVec<(crate::vtab::Cons, u64)> = AVec::new();
        #[cfg(feature = "vtab")]
        if let Some(vt) = sp.vt.as_deref() {
            use crate::vtab::{Cons, Op};
            let left = sp.kind == crate::sql::JoinKind::Left;
            let mut virt: AVec<(Cons, u64)> = AVec::new();
            let push = |col: i32, op: Op, v: &RExpr, to: &mut AVec<(Cons, u64)>| {
                if expr_refs_range(
                    v,
                    bands.get(i).unwrap_or(&RefRange {
                        w: (0, 0),
                        s: (0, 0),
                    }),
                ) {
                    return;
                }
                let d = src_mask(v, bands);
                if d & unusable != 0 {
                    return;
                }
                to.push((
                    Cons {
                        col,
                        op,
                        usable: true,
                    },
                    d,
                ));
            };
            for (ci, c) in conj.iter().enumerate() {
                let owner = on_of.get(ci).copied().flatten();
                if (left && owner != Some(i)) || (!left && owner.is_some()) {
                    continue;
                }
                match c {
                    RExpr::Binary(op, l, r) => {
                        let o = match op {
                            crate::sql::BinOp::Eq => Op::Eq,
                            crate::sql::BinOp::Lt => Op::Lt,
                            crate::sql::BinOp::Le => Op::Le,
                            crate::sql::BinOp::Gt => Op::Gt,
                            crate::sql::BinOp::Ge => Op::Ge,
                            crate::sql::BinOp::Is => Op::Is,
                            _ => continue,
                        };
                        let flip = |o: Op| match o {
                            Op::Lt => Op::Gt,
                            Op::Le => Op::Ge,
                            Op::Gt => Op::Lt,
                            Op::Ge => Op::Le,
                            x => x,
                        };
                        if let Some(k) = col_of(l)
                            && col_of(r).is_none()
                        {
                            push(k, o, r, &mut cands);
                        } else if let Some(k) = col_of(r)
                            && col_of(l).is_none()
                        {
                            push(k, flip(o), l, &mut cands);
                        }
                    }
                    #[cfg(feature = "fts5")]
                    RExpr::Scalar(crate::query::ScalarFn::Match, a) => {
                        if let (Some(q), Some(x)) = (a.as_slice().first(), a.as_slice().get(1))
                            && let Some(k) = col_of(x)
                            && col_of(q).is_none()
                        {
                            push(k, Op::Match, q, &mut cands);
                        }
                    }
                    RExpr::Between(x, lo, hi, false) if !left => {
                        if let Some(k) = col_of(x) {
                            push(k, Op::Ge, lo, &mut virt);
                            push(k, Op::Le, hi, &mut virt);
                        }
                    }
                    _ => {}
                }
            }
            let hidden = vt.tab.hidden_cols();
            for (k, a) in args.iter().enumerate().take(vt.nargs) {
                let col = hidden.as_slice().get(k).copied().unwrap_or(0);
                push(i32::try_from(col).unwrap_or(0), Op::Eq, a, &mut cands);
            }
            for c in virt.iter() {
                cands.push(*c);
            }
            let ask = |mask: u64| -> Option<(u64, i32, i32)> {
                let mut cons: AVec<Cons> = AVec::new();
                for (c, d) in cands.iter() {
                    let mut c = *c;
                    c.usable = *d & !mask == 0;
                    cons.push(c);
                }
                let mut info = crate::vtab::IndexInfo::new(cons, AVec::new());
                // (The pin's starting estimates.)
                info.cost = 5e98;
                info.rows = 25;
                crate::vtab::Vtab::best_index(&vt.tab.module, &vt.tab, &mut info).ok()?;
                let mut pre = 0u64;
                for (u, (_, d)) in info.usage.iter().zip(cands.iter()) {
                    if u.argv > 0 {
                        pre |= *d;
                    }
                }
                let rows = u64::try_from(info.rows.max(0)).unwrap_or(0);
                Some((pre, log_est_from_double(info.cost), log_est(rows)))
            };
            vt_masks(&mut out, cands.iter().map(|c| c.1), &ask);
            // An OR of full-text constraints (`whereLoopAddOr`, leaf
            // `fts5`): one plan per branch, their costs and rows
            // summed, needing what the branches read.
            #[cfg(feature = "fts5")]
            if vt.tab.module == crate::vtab::Module::Fts5
                && !cands.iter().any(|c| c.0.op == Op::Match)
                && !left
            {
                for (ci, c) in conj.iter().enumerate() {
                    if on_of.get(ci).copied().flatten().is_some()
                        || !matches!(c, RExpr::Binary(crate::sql::BinOp::Or, ..))
                    {
                        continue;
                    }
                    let mut alts: AVec<&RExpr> = AVec::new();
                    let mut stack: AVec<&RExpr> = AVec::new();
                    stack.push(c);
                    while let Some(e) = stack.pop() {
                        match e {
                            RExpr::Binary(crate::sql::BinOp::Or, l, r) => {
                                stack.push(r);
                                stack.push(l);
                            }
                            _ => alts.push(e),
                        }
                    }
                    let mut total: Option<(i32, i32)> = None;
                    let mut ok = true;
                    for a in alts.iter() {
                        let mut bc: AVec<(Cons, u64)> = AVec::new();
                        match a {
                            RExpr::Scalar(crate::query::ScalarFn::Match, x) => {
                                if let (Some(q), Some(col)) =
                                    (x.as_slice().first(), x.as_slice().get(1))
                                    && let Some(k) = col_of(col)
                                {
                                    push(k, Op::Match, q, &mut bc);
                                }
                            }
                            RExpr::Binary(crate::sql::BinOp::Eq, l, r) => {
                                if let Some(k) = col_of(l).filter(|k| *k < 0) {
                                    push(k, Op::Eq, r, &mut bc);
                                }
                            }
                            _ => {}
                        }
                        let [(con, _)] = bc.as_slice() else {
                            ok = false;
                            break;
                        };
                        let mut cons = AVec::new();
                        cons.push(*con);
                        let mut info = crate::vtab::IndexInfo::new(cons, AVec::new());
                        info.cost = 5e98;
                        info.rows = 25;
                        if crate::vtab::Vtab::best_index(&vt.tab.module, &vt.tab, &mut info)
                            .is_err()
                        {
                            ok = false;
                            break;
                        }
                        let rows = u64::try_from(info.rows.max(0)).unwrap_or(0);
                        let (r, o) = (log_est_from_double(info.cost), log_est(rows));
                        total = Some(match total {
                            None => (r, o),
                            Some((a, b)) => (
                                crate::xplan::log_est_add(a, r),
                                crate::xplan::log_est_add(b, o),
                            ),
                        });
                    }
                    let deps = src_mask(c, bands) & !me;
                    if ok
                        && deps & unusable == 0
                        && let Some((r, o)) = total
                    {
                        out.push((deps, r, o));
                    }
                }
            }
            return out;
        }
        // A JSON table-valued function: `json` and `root` are its
        // inputs (`jsonEachBestIndex`: an unusable one rejects the
        // plan; with the JSON argument the cost is 1).
        // (An argument reading a source the call may not follow — past
        // a CROSS JOIN — is not offered at all; without the JSON
        // argument the call has no input: review fix, it was refused.)
        let deps: AVec<u64> = {
            let mut v = AVec::new();
            for a in args.iter().take(2) {
                let d = src_mask(a, bands) & !me;
                if d & unusable != 0 {
                    if v.is_empty() {
                        out.push((0, log_est_from_double(5e98), log_est(25)));
                        return out;
                    }
                    break;
                }
                v.push(d);
            }
            v
        };
        let ask = |mask: u64| -> Option<(u64, i32, i32)> {
            let mut pre = 0u64;
            for d in deps.iter() {
                if d & !mask != 0 {
                    return None;
                }
                pre |= *d;
            }
            if deps.is_empty() {
                return Some((0, log_est_from_double(5e98), log_est(25)));
            }
            Some((pre, 0, log_est(25)))
        };
        vt_masks(&mut out, deps.iter().copied(), &ask);
        out
    }

    /// Plan a SELECT core's join in the pin's order (v0.17,
    /// `join_order`; see the module comment). False when the core is
    /// not such a join: the caller plans its sources one by one in
    /// FROM order (`plan_accesses`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_join(
        &self,
        arena: &Arena,
        sources: &mut AVec<SourcePlan>,
        cur: &mut crate::query::ScopeGroup,
        outer: (usize, usize),
        items: &mut AVec<RExpr>,
        filter: &mut Option<RExpr>,
        group_by: &mut AVec<RExpr>,
        having: &mut Option<RExpr>,
        order_by: &mut AVec<(RExpr, crate::sql::SortOrder)>,
        has_agg: bool,
        distinct: bool,
        limit: Option<&RExpr>,
    ) -> Result<bool, S::Error> {
        // (The planner's frame is large: a core that is no join, the
        // common case on the deep planning paths, does not pay it.)
        if !join_eligible(sources.as_slice()) {
            return Ok(false);
        }
        self.plan_join_body(
            arena, sources, cur, outer, items, filter, group_by, having, order_by, has_agg,
            distinct, limit,
        )
    }

    /// `plan_join` for a core `join_eligible` accepts.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn plan_join_body(
        &self,
        arena: &Arena,
        sources: &mut AVec<SourcePlan>,
        cur: &mut crate::query::ScopeGroup,
        outer: (usize, usize),
        items: &mut AVec<RExpr>,
        filter: &mut Option<RExpr>,
        group_by: &mut AVec<RExpr>,
        having: &mut Option<RExpr>,
        order_by: &mut AVec<(RExpr, crate::sql::SortOrder)>,
        has_agg: bool,
        distinct: bool,
        limit: Option<&RExpr>,
    ) -> Result<bool, S::Error> {
        use crate::xplan::{conjuncts, log_est, terms};
        let n = sources.len();
        // Per source: its table (None for a virtual one).
        let mut tabs: AVec<Option<&crate::schema::TableDef>> = AVec::new();
        for sp in sources.iter() {
            if virtual_source(sp) {
                tabs.push(None);
                continue;
            }
            let Some(t) = self.catalog.tables.iter().find(|t| t.root == sp.root) else {
                return Ok(false);
            };
            tabs.push(Some(t));
        }
        let offsets: AVec<usize> = cur.offsets.clone();
        let first_src = cur.first_src;
        // The sources whose columns may be NULL whatever their table
        // says (`EP_CanBeNull`: a LEFT JOIN's right side, as written).
        let mut can_null: AVec<bool> = AVec::new();
        for sp in sources.iter() {
            can_null.push(sp.kind == crate::sql::JoinKind::Left);
        }
        // INDEXED BY a partial index the terms do not imply.
        self.hint_checks(
            arena,
            sources.as_slice(),
            offsets.as_slice(),
            first_src,
            filter.as_ref(),
        )?;
        // A LEFT JOIN the WHERE clause rejects the NULL row of is an
        // inner join (the pin's simplification, in FROM order, over
        // the WHERE clause with every ON clause appended,
        // `sqlite3ExprImpliesNonNullRow`): the ON clause of a LEFT
        // JOIN not (yet) simplified proves nothing.
        for i in 1..n {
            let Some(&base) = offsets.as_slice().get(i) else {
                break;
            };
            let Some(sp) = sources.as_slice().get(i) else {
                break;
            };
            if sp.kind != crate::sql::JoinKind::Left {
                continue;
            }
            let s = Src {
                base,
                ncols: sp.ncols,
                src: first_src + i,
                rowid_alias: sp.rowid_alias,
            };
            let mut tree: Option<RExpr> = filter.clone();
            for x in sources.iter() {
                let Some(on) = &x.on else { continue };
                let chunk = if x.kind == crate::sql::JoinKind::Left {
                    RExpr::Literal(OwnedValue::Null)
                } else {
                    on.clone()
                };
                tree = Some(match tree {
                    None => chunk,
                    Some(t) => and_expr(Some(t), chunk),
                });
            }
            if tree.as_ref().is_some_and(|t| s.null_rejects(t))
                && let Some(sp) = sources.as_mut_slice().get_mut(i)
            {
                sp.kind = crate::sql::JoinKind::Inner;
            }
        }
        let mut bands: AVec<RefRange> = AVec::new();
        let mut srcs_s: AVec<Src> = AVec::new();
        for (i, sp) in sources.iter().enumerate() {
            let base = offsets.as_slice().get(i).copied().unwrap_or(0);
            bands.push(RefRange {
                w: (base, base + sp.ncols),
                s: (first_src + i, first_src + i + 1),
            });
            srcs_s.push(Src {
                base,
                ncols: sp.ncols,
                src: first_src + i,
                rowid_alias: sp.rowid_alias,
            });
        }
        // The WHERE clause's terms: the WHERE, then the ON clauses.
        let mut conj: AVec<&RExpr> = AVec::new();
        let mut on_of: AVec<Option<usize>> = AVec::new();
        // Per term: its truth probability, and whether a COLLATE /
        // likely() wrapper was removed above it.
        let mut meta: AVec<(i32, bool)> = AVec::new();
        if let Some(f) = filter.as_ref() {
            conjuncts(f, &mut conj);
            crate::xplan::conjuncts_meta(f, &mut meta);
        }
        while on_of.len() < conj.len() {
            on_of.push(None);
        }
        for (i, sp) in sources.iter().enumerate() {
            if let Some(on) = &sp.on {
                conjuncts(on, &mut conj);
                crate::xplan::conjuncts_meta(on, &mut meta);
                let left = sp.kind == crate::sql::JoinKind::Left;
                while on_of.len() < conj.len() {
                    on_of.push(if left { Some(i) } else { None });
                }
            }
        }
        // The planner reads the terms after the pin's constant
        // propagation.
        let mut excl: AVec<bool> = AVec::new();
        let mut nofix: AVec<bool> = AVec::new();
        let mut truth: AVec<i32> = AVec::new();
        for (ci, o) in on_of.iter().enumerate() {
            excl.push(o.is_some());
            let (tp, w) = meta.as_slice().get(ci).copied().unwrap_or((1, false));
            nofix.push(w);
            truth.push(tp);
        }
        let prop = propagate(
            conj.as_slice(),
            excl.as_slice(),
            nofix.as_slice(),
            srcs_s.as_slice(),
        );
        // `x IS NULL` / `x IS NOT NULL` on a column that cannot be
        // NULL is the constant 0 / 1 (the pin's resolver,
        // `sqlite3ExprCanBeNull`), unless the column is of a LEFT
        // JOIN's right side.
        let mut prop = prop;
        for e in prop.as_mut_slice().iter_mut() {
            let RExpr::IsNull(x, not) = e else { continue };
            let never = match &**x {
                RExpr::Rowid(s) => s
                    .checked_sub(first_src)
                    .is_some_and(|i| !can_null.as_slice().get(i).copied().unwrap_or(true)),
                RExpr::Column(slot, ..) => (0..n).any(|i| {
                    let base = offsets.as_slice().get(i).copied().unwrap_or(0);
                    let sp = sources.as_slice().get(i);
                    let nc = sp.map_or(0, |s| s.ncols);
                    *slot >= base
                        && *slot < base + nc
                        && !can_null.as_slice().get(i).copied().unwrap_or(true)
                        && (sp.is_some_and(|s| s.rowid_alias == Some(*slot - base))
                            || tabs.as_slice().get(i).copied().flatten().is_some_and(|t| {
                                self.not_null_cols(t)
                                    .as_slice()
                                    .get(*slot - base)
                                    .copied()
                                    .unwrap_or(false)
                            }))
                }),
                _ => false,
            };
            if never {
                *e = RExpr::Literal(OwnedValue::Integer(i64::from(*not)));
            }
        }
        let mut conj: AVec<&RExpr> = AVec::new();
        for e in prop.iter() {
            conj.push(e);
        }
        let enclosing = RefRange {
            w: (0, outer.0),
            s: (0, outer.1),
        };
        let nondet =
            |e: &RExpr| crate::query::any_scalar(e, &crate::query::ScalarFn::nondeterministic);
        let mut cmask: AVec<u64> = AVec::new();
        let mut pure: AVec<bool> = AVec::new();
        for c in conj.iter() {
            cmask.push(src_mask(c, bands.as_slice()));
            pure.push(!expr_refs_range(c, &enclosing) && !crate::query::has_sub(c) && !nondet(c));
        }
        // The order the join may deliver.
        let mut order: AVec<(RExpr, crate::sql::SortOrder)> = AVec::new();
        let mut group = false;
        let mut group_by_o = false;
        let mut distinct_by = false;
        let unsorted = |e: &RExpr| crate::sql::SortOrder {
            desc: false,
            nulls_first: true,
            coll: crate::query::expr_coll(e).unwrap_or_default(),
        };
        if !group_by.is_empty() {
            group = true;
            group_by_o = true;
            for e in group_by.iter() {
                order.push((e.clone(), unsorted(e)));
            }
        } else if !has_agg && !order_by.is_empty() {
            order.extend_from_slice(order_by.as_slice());
        } else if !has_agg && distinct {
            group = true;
            distinct_by = true;
            for e in items.iter() {
                order.push((e.clone(), unsorted(e)));
            }
        }
        // The pin's min() / max() optimisation (v0.17, `multi_index`):
        // the join reads the argument's order.
        #[cfg(feature = "multi_index")]
        let mm = has_agg && having.is_none() && {
            let nullable = |e: &RExpr| match crate::xplan::strip(e) {
                RExpr::Column(slot, ..) => !(0..n).any(|i| {
                    let base = offsets.as_slice().get(i).copied().unwrap_or(0);
                    let sp = sources.as_slice().get(i);
                    let nc = sp.map_or(0, |s| s.ncols);
                    *slot >= base
                        && *slot < base + nc
                        && !can_null.as_slice().get(i).copied().unwrap_or(true)
                        && (sp.is_some_and(|s| s.rowid_alias == Some(*slot - base))
                            || tabs.as_slice().get(i).copied().flatten().is_some_and(|t| {
                                self.not_null_cols(t)
                                    .as_slice()
                                    .get(*slot - base)
                                    .copied()
                                    .unwrap_or(false)
                            }))
                }),
                RExpr::Rowid(x) => x
                    .checked_sub(first_src)
                    .is_none_or(|i| can_null.as_slice().get(i).copied().unwrap_or(true)),
                RExpr::Literal(v) => matches!(v, OwnedValue::Null),
                _ => true,
            };
            match crate::xmulti::minmax(
                items.as_slice(),
                order_by.as_slice(),
                !group_by.is_empty(),
                &nullable,
            ) {
                Some((arg, o)) => {
                    order.clear();
                    order.push((arg.clone(), o));
                    true
                }
                None => false,
            }
        };
        let mut omask: AVec<u64> = AVec::new();
        let mut oconst: AVec<bool> = AVec::new();
        for (e, _) in order.iter() {
            omask.push(src_mask(e, bands.as_slice()));
            oconst.push(
                !expr_refs_range(e, &enclosing)
                    && !crate::query::has_sub(e)
                    && !nondet(e)
                    && src_mask(e, bands.as_slice()) == 0,
            );
        }
        let lim = match limit {
            Some(RExpr::Literal(OwnedValue::Integer(k))) if *k > 0 => Some(log_est(*k as u64)),
            _ => None,
        };
        let core_info = xindex::CoreInfo {
            items: items.as_slice(),
            filter: filter.as_ref(),
            group_by: group_by.as_slice(),
            having: having.as_ref(),
            order_by: order_by.as_slice(),
            has_agg,
            distinct,
            limit,
            also_reads: &[],
        };
        // Per source: its model, terms and columns.
        let mut models: AVec<(i32, AVec<crate::xplan::IdxModel>)> = AVec::new();
        let mut notnulls: AVec<AVec<bool>> = AVec::new();
        for (i, t) in tabs.iter().enumerate() {
            let Some(s) = srcs_s.as_slice().get(i) else {
                break;
            };
            match t {
                Some(t) => {
                    let (w, mut ms) = self.table_models(arena, t, s);
                    xindex::mark_xcover(ms.as_mut_slice(), s, &core_info, sources.as_slice());
                    models.push((w, ms));
                    notnulls.push(self.not_null_cols(t));
                }
                None => {
                    models.push((1, AVec::new()));
                    let mut nn = AVec::new();
                    for _ in 0..s.ncols {
                        nn.push(false);
                    }
                    notnulls.push(nn);
                }
            }
        }
        // The sources past a LEFT / CROSS JOIN after each one, which a
        // virtual table's constraints may not read (`mUnusable`).
        let mut unusable: AVec<u64> = AVec::new();
        for i in 0..n {
            let mut m = 0u64;
            for (j, x) in sources.iter().enumerate().skip(i + 1) {
                if m != 0
                    || matches!(
                        x.kind,
                        crate::sql::JoinKind::Left | crate::sql::JoinKind::Cross
                    )
                {
                    m |= 1u64 << j;
                }
            }
            unusable.push(m);
        }
        let mut jsrcs: AVec<JSrc<'_>> = AVec::new();
        let mut direct: AVec<(AVec<Term>, AVec<u64>)> = AVec::new();
        for (i, t) in tabs.iter().enumerate() {
            let (Some(sp), Some(s), Some((width, ms)), Some(nn)) = (
                sources.as_slice().get(i),
                srcs_s.as_slice().get(i).copied(),
                models.as_slice().get(i),
                notnulls.as_slice().get(i),
            ) else {
                break;
            };
            let Some(t) = t else {
                // A virtual table: no b-tree loops, only its own.
                #[cfg(not(any(feature = "json", feature = "vtab")))]
                let vl: AVec<(u64, i32, i32)> = AVec::new();
                #[cfg(any(feature = "json", feature = "vtab"))]
                let vl = self.vt_loops(
                    sp,
                    i,
                    &s,
                    conj.as_slice(),
                    on_of.as_slice(),
                    bands.as_slice(),
                    unusable.as_slice().get(i).copied().unwrap_or(0),
                );
                direct.push((AVec::new(), AVec::new()));
                let mut none: AVec<bool> = AVec::new();
                for _ in 0..sp.ncols {
                    none.push(false);
                }
                jsrcs.push(JSrc {
                    s,
                    tab: Table {
                        width: *width,
                        indexes: ms.as_slice(),
                        wr: false,
                        hint: sp.hint,
                        notnull: nn.as_slice(),
                        rows: crate::xplan::ROWS,
                    },
                    ts: AVec::new(),
                    tmask: AVec::new(),
                    derived: AVec::new(),
                    used: none.clone(),
                    affs: AVec::new(),
                    good: none,
                    left: sp.kind == crate::sql::JoinKind::Left,
                    cross: sp.kind == crate::sql::JoinKind::Cross,
                    auto_ok: false,
                    root: 0,
                    stat: false,
                    vt: Some(vl),
                    #[cfg(feature = "multi_index")]
                    ors: AVec::new(),
                });
                continue;
            };
            let me_band = bands.as_slice().get(i).copied().unwrap_or(RefRange {
                w: (0, 0),
                s: (0, 0),
            });
            let avail = |e: &RExpr| -> Option<bool> {
                if expr_refs_range(e, &me_band) || nondet(e) {
                    return None;
                }
                Some(src_mask(e, bands.as_slice()) != 0)
            };
            let left = sp.kind == crate::sql::JoinKind::Left;
            let mut ts: AVec<Term> = AVec::new();
            for t in terms(conj.as_slice(), truth.as_slice(), &s, &avail).iter() {
                let owner = on_of.as_slice().get(t.parent).copied().flatten();
                let ok = if left {
                    owner == Some(i)
                } else {
                    owner.is_none()
                };
                // (No IN term for an OR the pin does not rewrite: v0.17,
                // `multi_index`.)
                #[cfg(feature = "multi_index")]
                let ok = ok
                    && (!crate::xmulti::or_in_term(t, conj.as_slice())
                        || conj
                            .as_slice()
                            .get(t.parent)
                            .is_some_and(|c| crate::xmulti::in_kept(t, c, srcs_s.as_slice())));
                if ok {
                    ts.push(t.clone());
                }
            }
            // In the pin's WHERE clause term order: the terms as
            // written, then the virtual ones (commuted copies, BETWEEN
            // halves, `x>NULL`), which `sqlite3WhereExprAnalyze`
            // appends analysing the terms last to first; the loops are
            // built, and tie, in that order.
            {
                let nc = conj.len();
                let key = |t: &Term| {
                    if t.virt {
                        nc + nc.saturating_sub(t.parent + 1)
                    } else {
                        t.parent
                    }
                };
                ts.as_mut_slice().sort_by_key(key);
            }
            let mut tmask: AVec<u64> = AVec::new();
            for t in ts.iter() {
                tmask.push(term_mask(t, bands.as_slice()));
            }
            direct.push((ts.clone(), tmask.clone()));
            // Its OR terms (v0.17, `multi_index`), owned as its terms
            // are.
            #[cfg(feature = "multi_index")]
            let ors = {
                let ok = |ci: usize| {
                    let owner = on_of.as_slice().get(ci).copied().flatten();
                    if left {
                        owner == Some(i)
                    } else {
                        owner.is_none()
                    }
                };
                crate::xmulti::or_infos(
                    conj.as_slice(),
                    truth.as_slice(),
                    &s,
                    &ok,
                    &avail,
                    &|e: &RExpr| src_mask(e, bands.as_slice()),
                    &|t: &Term| term_mask(t, bands.as_slice()),
                )
            };
            let mut used: AVec<bool> = AVec::new();
            let mut affs: AVec<crate::record::Affinity> = AVec::new();
            let mut good: AVec<bool> = AVec::new();
            for c in 0..sp.ncols {
                used.push(
                    xindex::core_reads(&core_info, sources.as_slice(), s.base + c)
                        && sp.rowid_alias != Some(c),
                );
                affs.push(
                    sp.affs
                        .as_slice()
                        .get(c)
                        .copied()
                        .unwrap_or(crate::record::Affinity::Blob),
                );
                good.push(self.auto_candidate(t, c));
            }
            jsrcs.push(JSrc {
                s,
                tab: Table {
                    width: *width,
                    indexes: ms.as_slice(),
                    wr: !t.has_rowid(),
                    hint: sp.hint,
                    notnull: nn.as_slice(),
                    rows: self.table_rows(t),
                },
                ts,
                tmask,
                derived: AVec::new(),
                used,
                affs,
                good,
                left,
                cross: sp.kind == crate::sql::JoinKind::Cross,
                auto_ok: sp.hint == crate::query::Hint::Any,
                root: sp.root,
                stat: self.table_has_stat(t),
                vt: None,
                #[cfg(feature = "multi_index")]
                ors,
            });
        }
        if jsrcs.len() != n {
            return Ok(false);
        }
        // Terms on columns a `x = y` term makes equivalent (the pin's
        // `whereScan` equivalence classes): `y op value` probes `x`.
        let equiv = equivalences(conj.as_slice(), on_of.as_slice(), srcs_s.as_slice());
        for (i, js) in jsrcs.as_mut_slice().iter_mut().enumerate() {
            for _ in js.ts.iter() {
                js.derived.push(false);
            }
            if js.vt.is_some() {
                continue;
            }
            let me = 1u64 << i;
            let left = js.left;
            let mut xs: AVec<usize> = AVec::new();
            for c in 0..js.s.ncols {
                if js.s.rowid_alias != Some(c) {
                    xs.push(js.s.base + c);
                }
            }
            xs.push(ROWID_NODE.saturating_add(js.s.src));
            let ss = srcs_s.as_slice();
            for x in xs.iter() {
                let x = *x;
                for y in chain(equiv.as_slice(), x).iter().skip(1) {
                    let owner_of = |nd: usize| {
                        if nd >= ROWID_NODE {
                            ss.iter().position(|s| s.src == nd - ROWID_NODE)
                        } else {
                            ss.iter()
                                .position(|s| nd >= s.base && nd < s.base + s.ncols)
                        }
                    };
                    let Some(j) = owner_of(*y) else { continue };
                    let Some((dts, dmask)) = direct.as_slice().get(j) else {
                        continue;
                    };
                    let Some(key) = equiv_key(equiv.as_slice(), x) else {
                        continue;
                    };
                    for (t, m) in dts.iter().zip(dmask.iter()) {
                        if node(&t.key, ss) != Some(*y) || m & me != 0 {
                            continue;
                        }
                        // (Not the equivalence back to `x`.)
                        if matches!(t.op, TOp::Eq | TOp::Is) && node(&t.val, ss) == Some(x) {
                            continue;
                        }
                        // (A LEFT JOIN's ON terms never come through an
                        // equivalence, and a LEFT JOIN probes with those
                        // only.)
                        let owner = on_of.as_slice().get(t.parent).copied().flatten();
                        if left || owner.is_some() {
                            continue;
                        }
                        let mut d = t.clone();
                        d.key = key.clone();
                        d.lateral = *m != 0;
                        js.ts.push(d);
                        js.tmask.push(*m);
                        js.derived.push(true);
                    }
                }
            }
        }
        let jq = JQuery {
            conj: conj.as_slice(),
            truth: truth.as_slice(),
            cmask: cmask.as_slice(),
            pure: pure.as_slice(),
            on_of: on_of.as_slice(),
            order: order.as_slice(),
            group,
            group_by: group_by_o,
            distinct_by,
            distinct,
            ncols_out: items.len(),
            limit: lim,
            omask: omask.as_slice(),
            oconst: oconst.as_slice(),
        };
        let Some(plan) = crate::xjoin::plan(jsrcs.as_slice(), &jq) else {
            return Ok(false);
        };
        // The LEFT JOINs the pin's plan omits (EXPLAIN QUERY PLAN).
        #[cfg(feature = "explain")]
        let mut omit: AVec<bool> = AVec::new();
        #[cfg(feature = "explain")]
        {
            for _ in 0..n {
                omit.push(false);
            }
            let mut used_mask = 0u64;
            for e in items.iter().chain(order_by.iter().map(|(e, _)| e)) {
                used_mask |= src_mask(e, bands.as_slice());
            }
            if !has_agg {
                for jl in plan.loops.iter().skip(1).rev() {
                    let i = jl.src;
                    let Some(js) = jsrcs.as_slice().get(i) else {
                        continue;
                    };
                    if !js.left || !(distinct || crate::xjoin::one_row(js, jl)) {
                        continue;
                    }
                    let me = 1u64 << i;
                    if used_mask & me != 0 {
                        continue;
                    }
                    let only_own = conj.iter().enumerate().all(|(ci, _)| {
                        cmask.as_slice().get(ci).copied().unwrap_or(0) & me == 0
                            || on_of.as_slice().get(ci).copied().flatten() == Some(i)
                    });
                    if only_own && let Some(o) = omit.as_mut_slice().get_mut(i) {
                        *o = true;
                    }
                }
            }
        }
        // Each source's access path, in the old layout.
        let mut perm: AVec<usize> = AVec::new();
        let mut accs: AVec<Option<Box<crate::xplan::IdxAccess>>> = AVec::new();
        for _ in 0..n {
            accs.push(None);
        }
        let mut ready = 0u64;
        for (pos, jl) in plan.loops.iter().enumerate() {
            let i = jl.src;
            perm.push(i);
            let Some(js) = jsrcs.as_slice().get(i) else {
                continue;
            };
            let reverse = plan.rev & (1u64 << pos) != 0;
            // A MULTI-INDEX OR loop (v0.17, `multi_index`): each branch
            // planned on the table alone, the sources before it given.
            #[cfg(feature = "multi_index")]
            let or_acc = jl.l.or.and_then(|k| js.ors.as_slice().get(k)).map(|info| {
                let me = 1u64 << i;
                let me_band = bands.as_slice().get(i).copied().unwrap_or(RefRange {
                    w: (0, 0),
                    s: (0, 0),
                });
                let avail = |e: &RExpr| -> Option<bool> {
                    if expr_refs_range(e, &me_band) || nondet(e) {
                        return None;
                    }
                    let m = src_mask(e, bands.as_slice());
                    if m & !ready != 0 {
                        return None;
                    }
                    Some(m != 0)
                };
                let w_ok = |ci: usize| {
                    let owner = on_of.as_slice().get(ci).copied().flatten();
                    let own = if js.left {
                        owner == Some(i)
                    } else {
                        owner.is_none()
                    };
                    own && cmask.as_slice().get(ci).copied().unwrap_or(0) & !(ready | me) == 0
                };
                let mut needed: AVec<bool> = AVec::new();
                for u in js.used.iter() {
                    needed.push(*u);
                }
                let q = crate::xplan::Query {
                    conj: conj.as_slice(),
                    truth: truth.as_slice(),
                    order: &[],
                    group: false,
                    group_by: false,
                    ncols_out: items.len(),
                    needed: Some(needed.as_slice()),
                    cover_scan: true,
                    limit: None,
                    distinct: false,
                    #[cfg(feature = "explain")]
                    dset: &[],
                    jm: None,
                };
                let o = crate::xmulti::or_access(&q, &js.s, &js.tab, info, &avail, &w_ok);
                let lateral = o.subs.iter().any(|b| {
                    b.acc.as_ref().is_some_and(|a| a.lateral)
                        || src_mask(&b.cond, bands.as_slice()) & !me != 0
                });
                let mut a = crate::xplan::lower(&js.s, &js.tab, &[], &jl.l, false);
                a.lateral = lateral;
                a.or = Some(Box::new(o));
                a
            });
            #[cfg(not(feature = "multi_index"))]
            let or_acc: Option<crate::xplan::IdxAccess> = None;
            let mut acc = if let Some(a) = or_acc {
                Some(a)
            } else if jl.auto {
                let ai = crate::xjoin::auto_index(js, &jq, i, ready);
                let lateral = ai
                    .keys
                    .iter()
                    .any(|k| src_mask(&k.val, bands.as_slice()) != 0);
                let mut a = crate::xplan::lower(
                    &js.s,
                    &js.tab,
                    &[],
                    &crate::xplan::Loop {
                        ix: None,
                        eq: AVec::new(),
                        lo: None,
                        hi: None,
                        run: 0,
                        out: 0,
                        sorted: 0,
                        reverse: false,
                        bignull: None,
                        #[cfg(feature = "multi_index")]
                        or: None,
                        #[cfg(feature = "multi_index")]
                        skip: 0,
                    },
                    false,
                );
                a.lateral = lateral;
                a.auto = Some(Box::new(ai));
                Some(a)
            } else {
                let m = jl.l.ix.and_then(|x| js.tab.indexes.get(x));
                let natural = match m {
                    None => true,
                    Some(m) => m.covers_all,
                };
                // (The first loop of a join that delivers the GROUP
                // BY keeps an access path to say so, whatever it is:
                // the groups are emitted as they come; v0.17 review
                // fix.)
                let groups = pos == 0 && group_by_o && plan.ob_sat >= order.len();
                if natural
                    && jl.l.eq.is_empty()
                    && jl.l.lo.is_none()
                    && jl.l.hi.is_none()
                    && !reverse
                    && !groups
                {
                    None
                } else {
                    let mut a =
                        crate::xplan::lower(&js.s, &js.tab, js.ts.as_slice(), &jl.l, reverse);
                    if let Some(m) = m {
                        let mut needed: AVec<bool> = AVec::new();
                        for u in js.used.iter() {
                            needed.push(*u);
                        }
                        #[cfg(feature = "explain")]
                        {
                            a.eqp.1 = !m.covers_all && m.covers(needed.as_slice());
                        }
                        if js.tab.wr {
                            // (A WITHOUT ROWID table's rows come whole.)
                        } else if m.covers_plain(needed.as_slice()) {
                            let mut map: AVec<Option<usize>> = AVec::new();
                            for k in m.keys.iter() {
                                map.push(match k {
                                    crate::xplan::IdxKey::Col(c) => Some(*c),
                                    _ => None,
                                });
                            }
                            a.cover = Some(map);
                        }
                        if jl.l.eq.len() < a.desc.len() {
                            a.bignull = None;
                        }
                    }
                    Some(a)
                }
            };
            // A Bloom filter on the lookups: the rows come from the
            // table (IDX_ONLY is cleared), keyed on the equality
            // columns.
            if plan
                .bloom_at
                .as_slice()
                .get(pos)
                .copied()
                .flatten()
                .is_some()
                && let Some(a) = acc.as_mut()
            {
                use crate::xplan::{BKey, IdxKey};
                let mut keys: AVec<BKey> = AVec::new();
                match jl.l.ix.and_then(|x| js.tab.indexes.get(x)) {
                    None => keys.push(BKey::Rowid),
                    Some(m) => {
                        for k in m.keys.iter().take(jl.l.eq.len()) {
                            keys.push(match k {
                                IdxKey::Col(c) if js.s.rowid_alias == Some(*c) => BKey::Rowid,
                                IdxKey::Col(c) => BKey::Col(*c),
                                IdxKey::Rowid => BKey::Rowid,
                                IdxKey::Expr(_) => BKey::Expr,
                            });
                        }
                    }
                }
                a.bloom = Some(keys);
                a.cover = None;
                #[cfg(feature = "explain")]
                {
                    a.eqp.1 = false;
                }
            }
            if pos == 0
                && let Some(a) = acc.as_mut()
            {
                a.groups = group_by_o && plan.ob_sat >= order.len();
                #[cfg(feature = "explain")]
                {
                    a.eqp.0 = plan.ob_sat;
                    a.eqp.3 = plan.distinct_ordered;
                }
            }
            if let Some(slot) = accs.as_mut_slice().get_mut(i) {
                *slot = acc.map(Box::new);
            }
            ready |= 1u64 << i;
        }
        // The ON terms of inner joins move to the latest source they
        // read in the new order (or to the WHERE clause when that is a
        // LEFT JOIN's right side).
        let mut posn: AVec<usize> = AVec::new();
        for _ in 0..n {
            posn.push(0);
        }
        for (p, i) in perm.iter().enumerate() {
            if let Some(x) = posn.as_mut_slice().get_mut(*i) {
                *x = p;
            }
        }
        let mut new_on: AVec<Option<RExpr>> = AVec::new();
        for sp in sources.iter() {
            new_on.push(if sp.kind == crate::sql::JoinKind::Left {
                sp.on.clone()
            } else {
                None
            });
        }
        let mut to_where: Option<RExpr> = None;
        for (i, sp) in sources.iter().enumerate() {
            if sp.kind == crate::sql::JoinKind::Left {
                continue;
            }
            let Some(on) = &sp.on else { continue };
            let mut parts: AVec<&RExpr> = AVec::new();
            conjuncts(on, &mut parts);
            for c in parts.iter() {
                let m = src_mask(c, bands.as_slice()) | (1u64 << i);
                let mut target = i;
                for j in 0..n {
                    if m & (1u64 << j) != 0 && posn.as_slice().get(j) > posn.as_slice().get(target)
                    {
                        target = j;
                    }
                }
                let left = sources
                    .as_slice()
                    .get(target)
                    .is_some_and(|s| s.kind == crate::sql::JoinKind::Left);
                if left || crate::query::has_sub(c) {
                    to_where = Some(and_expr(to_where, (*c).clone()));
                } else if let Some(slot) = new_on.as_mut_slice().get_mut(target) {
                    *slot = Some(and_expr(slot.take(), (*c).clone()));
                }
            }
        }
        if let Some(w) = to_where {
            *filter = Some(match filter.take() {
                None => w,
                Some(f) => and_expr(Some(f), w),
            });
        }
        // The new layout.
        let mut new_base: AVec<usize> = AVec::new();
        for _ in 0..n {
            new_base.push(0);
        }
        let start = offsets.as_slice().first().copied().unwrap_or(0);
        let mut w = start;
        for i in perm.iter() {
            if let Some(b) = new_base.as_mut_slice().get_mut(*i) {
                *b = w;
            }
            w += sources.as_slice().get(*i).map_or(0, |s| s.ncols);
        }
        let old = offsets.clone();
        let ncols: AVec<usize> = {
            let mut v = AVec::new();
            for s in sources.iter() {
                v.push(s.ncols);
            }
            v
        };
        let posn2 = posn.clone();
        let map = move |x: usize, rowid: bool| -> usize {
            if rowid {
                if x >= first_src && x < first_src + n {
                    return first_src + posn2.as_slice().get(x - first_src).copied().unwrap_or(0);
                }
                return x;
            }
            for i in 0..n {
                let b = old.as_slice().get(i).copied().unwrap_or(0);
                let k = ncols.as_slice().get(i).copied().unwrap_or(0);
                if x >= b && x < b + k {
                    return new_base.as_slice().get(i).copied().unwrap_or(b) + (x - b);
                }
            }
            x
        };
        use crate::query::{remap_access, remap_expr};
        for e in items.as_mut_slice() {
            remap_expr(e, &map);
        }
        if let Some(f) = filter.as_mut() {
            remap_expr(f, &map);
        }
        for e in group_by.as_mut_slice() {
            remap_expr(e, &map);
        }
        if let Some(h) = having.as_mut() {
            remap_expr(h, &map);
        }
        for (e, _) in order_by.as_mut_slice() {
            remap_expr(e, &map);
        }
        for (_, e) in cur.aliases.as_mut_slice() {
            remap_expr(e, &map);
        }
        let mut old_srcs: AVec<Option<SourcePlan>> = {
            let mut v = AVec::new();
            for x in core::mem::take(sources) {
                v.push(Some(x));
            }
            v
        };
        let mut old_descs: AVec<Option<crate::query::SourceDesc>> = {
            let mut v = AVec::new();
            for x in core::mem::take(&mut cur.sources) {
                v.push(Some(x));
            }
            v
        };
        let mut new_offsets: AVec<usize> = AVec::new();
        for i in perm.iter() {
            let (Some(mut sp), Some(d)) = (
                old_srcs.as_mut_slice().get_mut(*i).and_then(Option::take),
                old_descs.as_mut_slice().get_mut(*i).and_then(Option::take),
            ) else {
                return Err(Error::Unsupported);
            };
            sp.on = new_on.as_mut_slice().get_mut(*i).and_then(Option::take);
            if let Some(on) = sp.on.as_mut() {
                remap_expr(on, &map);
            }
            // A virtual table's arguments (its placeholder plan's one
            // VALUES row) read the core too.
            if virtual_source(&sp)
                && let Some(d) = sp.derived.as_deref_mut()
            {
                for row in d.values.as_mut_slice() {
                    for e in row.as_mut_slice() {
                        remap_expr(e, &map);
                    }
                }
            }
            sp.access = accs.as_mut_slice().get_mut(*i).and_then(Option::take);
            if let Some(a) = sp.access.as_mut() {
                remap_access(a, &map);
            }
            let nb = map(offsets.as_slice().get(*i).copied().unwrap_or(0), false);
            #[cfg(feature = "explain")]
            {
                sp.eqp.base = nb;
                sp.eqp.omit = omit.as_slice().get(*i).copied().unwrap_or(false);
            }
            new_offsets.push(nb);
            sources.push(sp);
            cur.sources.push(d);
        }
        // The first loop reports what the join delivers (EXPLAIN
        // QUERY PLAN), whatever its access path.
        #[cfg(feature = "explain")]
        if let Some(sp) = sources.as_mut_slice().first_mut() {
            sp.eqp.natural = plan.ob_sat;
            sp.eqp.nat_distinct = plan.distinct_ordered;
            sp.eqp.joined = true;
        }
        #[cfg(all(feature = "multi_index", not(feature = "explain")))]
        let _ = mm;
        #[cfg(all(feature = "multi_index", feature = "explain"))]
        if mm {
            for sp in sources.as_mut_slice() {
                sp.eqp.minmax = 1 + u8::from(plan.ob_sat >= 1);
            }
        }
        #[cfg(feature = "explain")]
        for (q, at) in plan.bloom_at.iter().enumerate() {
            if let Some(p) = at
                && let Some(sp) = sources.as_mut_slice().get_mut(*p)
            {
                sp.eqp.bloom_lines |= 1u64 << q;
            }
        }
        cur.offsets = new_offsets;
        Ok(true)
    }

    /// Whether column `c` of `t` may key an automatic index (the
    /// pin's `columnIsGoodIndexCandidate`): no index starts with it,
    /// and none with statistics holds it where a prefix ending there
    /// matches more than 4 rows.
    fn auto_candidate(&self, t: &crate::schema::TableDef, c: usize) -> bool {
        let ca = &self.catalog_arena;
        for ix in self.catalog.indexes_for(ca, handle_bytes(ca, t.name)) {
            for (j, k) in ix.columns.iter().enumerate() {
                if k.col == Some(c) {
                    if j == 0 {
                        return false;
                    }
                    if let Some(est) = self.stat_est(t, ix)
                        && est.as_slice().get(j + 1).is_some_and(|v| *v > 20)
                    {
                        return false;
                    }
                    break;
                }
            }
        }
        true
    }
}

/// The pin's `whereLoopAddVirtual` sequence of usable-source masks
/// over constraints reading `deps`: all usable; then, unless that
/// plan needs no other source, each distinct dependency set in
/// increasing order (not the first plan's again), then none at all
/// if no plan so far needed none. `ask` gives each accepted plan.
#[cfg(any(feature = "json", feature = "vtab"))]
fn vt_masks(
    out: &mut AVec<(u64, i32, i32)>,
    deps: impl Iterator<Item = u64> + Clone,
    ask: &dyn Fn(u64) -> Option<(u64, i32, i32)>,
) {
    let first = ask(u64::MAX);
    let best = first.map_or(0, |l| l.0);
    if let Some(l) = first {
        out.push(l);
    }
    if first.is_some() && best == 0 {
        return;
    }
    let mut seen_zero = false;
    let mut prev = 0u64;
    loop {
        let next = deps.clone().filter(|m| *m > prev).min().unwrap_or(u64::MAX);
        if next == u64::MAX {
            break;
        }
        prev = next;
        if next == best {
            continue;
        }
        if let Some(l) = ask(next) {
            if l.0 == 0 {
                seen_zero = true;
            }
            out.push(l);
        }
    }
    if !seen_zero && let Some(l) = ask(0) {
        out.push(l);
    }
}

/// The statistics the planner reads (v0.17, `join_order`), held as
/// the pin holds them on its schema objects: per table its
/// `nRowLogEst` and `TF_HasStat1`, per index its `aiRowLogEst` and
/// `hasStat1` (ASCII names, matched case-blind). They are set when
/// the schema is read and by ANALYZE (`sqlite3AnalysisLoad`), and
/// otherwise only follow the schema: a new table starts at 200, a new
/// index gets `sqlite3DefaultRowEst` from its table's estimate then,
/// a dropped object takes its numbers with it (review fix: a DROP
/// INDEX after ANALYZE leaves the table's estimate in place).
#[derive(Default)]
pub(super) struct StatState {
    tabs: AVec<(AVec<u8>, i32, bool)>,
    idxs: AVec<(AVec<u8>, AVec<i32>, bool)>,
}

/// `decodeIntArray` into `out` (LogEst): up to `out.len()` integers
/// of a `stat` text, each digits then an optional space; the counts
/// wrap as the pin's `tRowcnt` does; entries past the text's end keep
/// their values.
fn decode_ints(stat: &[u8], out: &mut [i32]) {
    let mut z = stat;
    for slot in out.iter_mut() {
        if z.is_empty() {
            break;
        }
        let mut v: u64 = 0;
        while let Some(c) = z.first().filter(|c| c.is_ascii_digit()) {
            v = v.wrapping_mul(10).wrapping_add(u64::from(c - b'0'));
            z = z.get(1..).unwrap_or(&[]);
        }
        *slot = crate::xplan::log_est(v);
        if z.first() == Some(&b' ') {
            z = z.get(1..).unwrap_or(&[]);
        }
    }
}

impl<S: Storage> Connection<S> {
    /// After a schema read: the pin's statistics as at a fresh open
    /// (`sqlite3InitOne`: every table at 200, every index at its
    /// defaults, then `sqlite3AnalysisLoad`), or — `keep`, after this
    /// connection's own CREATE / DROP (the pin parses only the new
    /// object) — the statistics held so far, brought in step with the
    /// schema (`stat_follow`).
    pub(super) async fn load_stats(&mut self, keep: bool) -> Result<(), S::Error> {
        if keep {
            self.stat_follow();
            return Ok(());
        }
        self.stat = StatState::default();
        self.stat_follow();
        let root = self
            .catalog
            .find(&self.catalog_arena, b"sqlite_stat1")
            .filter(|t| t.has_rowid())
            .map(|t| t.root);
        self.analysis_load(root).await
    }

    /// Drop the statistics of objects the schema no longer has; give
    /// a new table the default 200 and a new index its defaults.
    fn stat_follow(&mut self) {
        let ca = &self.catalog_arena;
        let tabs = &self.catalog.tables;
        let idxs = &self.catalog.indexes;
        let st = &mut self.stat;
        let mut k = 0;
        while k < st.tabs.len() {
            let gone = st.tabs.as_slice().get(k).is_some_and(|(n, ..)| {
                !tabs
                    .iter()
                    .any(|t| eq_ci(handle_bytes(ca, t.name), n.as_slice()))
            });
            if gone {
                st.tabs.remove_at(k);
            } else {
                k += 1;
            }
        }
        let mut k = 0;
        while k < st.idxs.len() {
            let gone = st.idxs.as_slice().get(k).is_some_and(|(n, ..)| {
                !idxs
                    .iter()
                    .any(|i| eq_ci(handle_bytes(ca, i.name), n.as_slice()))
            });
            if gone {
                st.idxs.remove_at(k);
            } else {
                k += 1;
            }
        }
        for t in tabs.iter() {
            let n = handle_bytes(ca, t.name);
            if !st.tabs.iter().any(|(x, ..)| eq_ci(x.as_slice(), n)) {
                st.tabs.push((copy_vec(n), crate::xplan::ROWS, false));
            }
        }
        for i in idxs.iter() {
            let n = handle_bytes(ca, i.name);
            if !st.idxs.iter().any(|(x, ..)| eq_ci(x.as_slice(), n)) {
                let mut a: AVec<i32> = AVec::new();
                for _ in 0..=i.columns.len() {
                    a.push(0);
                }
                st.idxs.push((copy_vec(n), a, false));
                default_row_est(st, ca, i);
            }
        }
    }

    /// The pin's `sqlite3AnalysisLoad` over the `sqlite_stat1` table
    /// rooted at `root` (none: no statistics): every `TF_HasStat1` /
    /// `hasStat1` cleared, each row applied as `analysisLoader` does,
    /// then the defaults for every index the rows did not reach. A
    /// table's `nRowLogEst` is not reset first, as on the pin.
    pub(super) async fn analysis_load(&mut self, root: Option<u32>) -> Result<(), S::Error> {
        for t in self.stat.tabs.as_mut_slice() {
            t.2 = false;
        }
        for i in self.stat.idxs.as_mut_slice() {
            i.2 = false;
        }
        if let Some(root) = root {
            // (`sqlite3_exec`'s text of each value.)
            let text = |v: Option<&crate::record::Value<'_>>| -> Option<AVec<u8>> {
                let o = match v? {
                    crate::record::Value::Null => return None,
                    crate::record::Value::Text(t) | crate::record::Value::Blob(t) => {
                        return Some(copy_vec(t));
                    }
                    crate::record::Value::Integer(i) => OwnedValue::Integer(*i),
                    crate::record::Value::Real(r) => OwnedValue::Real(*r),
                };
                match crate::query::cast_value(&o, crate::record::Affinity::Text) {
                    OwnedValue::Text(t) => Some(t),
                    _ => None,
                }
            };
            let mut cur = ScanCursor::new(root);
            while let Some((_, payload)) = cur.next_row(&mut self.pager).await? {
                let vals = crate::record::decode_record::<S::Error>(payload.as_slice(), 3)?;
                let v = vals.as_slice();
                let (Some(t), Some(st)) = (text(v.first()), text(v.get(2))) else {
                    continue;
                };
                let ix = text(v.get(1));
                self.stat_row(
                    t.as_slice(),
                    ix.as_ref().map(|x| x.as_slice()),
                    st.as_slice(),
                );
            }
        }
        let ca = &self.catalog_arena;
        for i in self.catalog.indexes.iter() {
            let n = handle_bytes(ca, i.name);
            if self
                .stat
                .idxs
                .iter()
                .any(|(x, _, has)| !*has && eq_ci(x.as_slice(), n))
            {
                default_row_est(&mut self.stat, ca, i);
            }
        }
        Ok(())
    }

    /// One `sqlite_stat1` row (`analysisLoader`): `idx` names any
    /// index of the schema (its table need not be `tbl`), or `tbl`
    /// itself for a WITHOUT ROWID table's PRIMARY KEY; otherwise the
    /// row is `tbl`'s own row count.
    fn stat_row(&mut self, tbl: &[u8], idx: Option<&[u8]>, stat: &[u8]) {
        let ca = &self.catalog_arena;
        // (`sqlite3FindTable` finds a view too: its numbers are never
        // read, but an index the row names still takes them.)
        let tab = self.catalog.find(ca, tbl);
        if tab.is_none() && self.catalog.find_view(ca, tbl).is_none() {
            return;
        }
        let ix = idx.and_then(|n| {
            if eq_ci(n, tbl) {
                #[cfg(feature = "without_rowid")]
                return tab.and_then(|t| {
                    self.catalog
                        .indexes_for(ca, handle_bytes(ca, t.name))
                        .find(|i| i.clustered)
                });
                #[cfg(not(feature = "without_rowid"))]
                return None;
            }
            self.catalog
                .indexes
                .iter()
                .find(|i| eq_ci(handle_bytes(ca, i.name), n))
        });
        let st = &mut self.stat;
        let tp = tab.and_then(|t| {
            let tn = handle_bytes(ca, t.name);
            st.tabs.iter().position(|(x, ..)| eq_ci(x.as_slice(), tn))
        });
        let tp = tp.unwrap_or(usize::MAX);
        match ix {
            Some(i) => {
                let n = handle_bytes(ca, i.name);
                let Some(e) = st
                    .idxs
                    .as_mut_slice()
                    .iter_mut()
                    .find(|(x, ..)| eq_ci(x.as_slice(), n))
                else {
                    return;
                };
                decode_ints(stat, e.1.as_mut_slice());
                e.2 = true;
                let first = e.1.as_slice().first().copied();
                if !i.partial()
                    && let (Some(t), Some(r)) = (st.tabs.as_mut_slice().get_mut(tp), first)
                {
                    t.1 = r;
                    t.2 = true;
                }
            }
            None => {
                if let Some(t) = st.tabs.as_mut_slice().get_mut(tp) {
                    let mut r = [t.1];
                    decode_ints(stat, &mut r);
                    t.1 = r[0];
                    t.2 = true;
                }
            }
        }
    }

    fn stat_tab(&self, t: &crate::schema::TableDef) -> Option<&(AVec<u8>, i32, bool)> {
        let n = handle_bytes(&self.catalog_arena, t.name);
        self.stat.tabs.iter().find(|(x, ..)| eq_ci(x.as_slice(), n))
    }

    /// Table `t`'s `nRowLogEst` (v0.17, `join_order`).
    pub(super) fn table_rows(&self, t: &crate::schema::TableDef) -> i32 {
        self.stat_tab(t).map_or(crate::xplan::ROWS, |e| e.1)
    }

    /// Whether table `t` has `sqlite_stat1` rows (`TF_HasStat1`).
    pub(super) fn table_has_stat(&self, t: &crate::schema::TableDef) -> bool {
        self.stat_tab(t).is_some_and(|e| e.2)
    }

    /// Index `ix`'s `aiRowLogEst` when `sqlite_stat1` gave it
    /// (`hasStat1`).
    pub(super) fn stat_est(
        &self,
        _t: &crate::schema::TableDef,
        ix: &crate::schema::IndexDef,
    ) -> Option<AVec<i32>> {
        let n = handle_bytes(&self.catalog_arena, ix.name);
        self.stat
            .idxs
            .iter()
            .find(|(x, _, has)| *has && eq_ci(x.as_slice(), n))
            .map(|e| e.1.clone())
    }

    /// Index `ix`'s `aiRowLogEst` (`IdxModel::est`).
    pub(super) fn index_stat(
        &self,
        _t: &crate::schema::TableDef,
        ix: &crate::schema::IndexDef,
    ) -> Option<AVec<i32>> {
        let n = handle_bytes(&self.catalog_arena, ix.name);
        self.stat
            .idxs
            .iter()
            .find(|(x, ..)| eq_ci(x.as_slice(), n))
            .map(|e| e.1.clone())
    }
}

/// The pin's `sqlite3DefaultRowEst` for index `i`: its table's
/// estimate (lifted to at least 1,000 rows, on the table too; less
/// half for a partial index), then ten rows per value of the first
/// column, 9, 8, 7, 6 (LogEst 33 32 30 28 26), then 23; one row per
/// key of a UNIQUE index.
fn default_row_est(st: &mut StatState, ca: &crate::arena::Arena, i: &crate::schema::IndexDef) {
    let tn = handle_bytes(ca, i.table);
    let mut x = crate::xplan::ROWS;
    if let Some(t) = st
        .tabs
        .as_mut_slice()
        .iter_mut()
        .find(|(n, ..)| eq_ci(n.as_slice(), tn))
    {
        if t.1 < 99 {
            t.1 = 99;
        }
        x = t.1;
    }
    if i.partial() {
        x -= 10;
    }
    let n = handle_bytes(ca, i.name);
    let Some(e) = st
        .idxs
        .as_mut_slice()
        .iter_mut()
        .find(|(m, ..)| eq_ci(m.as_slice(), n))
    else {
        return;
    };
    const VALS: [i32; 5] = [33, 32, 30, 28, 26];
    let nk = i.columns.len();
    let mut a: AVec<i32> = AVec::new();
    a.push(x);
    for k in 0..nk {
        a.push(VALS.get(k).copied().unwrap_or(23));
    }
    if i.unique
        && let Some(l) = a.as_mut_slice().last_mut()
    {
        *l = 0;
    }
    e.1 = a;
}
