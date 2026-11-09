//! Index-aware access planning (v0.16, `expr_index`).
//!
//! hipp's executor materialises each FROM source before joining it,
//! so planning here only decides *which rows a source reads, in which
//! order*: the full table in rowid order (no plan), or the rows an
//! index (or the rowid B-tree) finds for the query's equality, IN and
//! range terms, in that index's order. The executor still evaluates
//! every WHERE / ON term on every row it reads, so an access path only
//! has to return a superset of the matching rows: correctness never
//! depends on the plan, only the row order and the work done do.
//!
//! The order is observable (rows of a query without ORDER BY, ties of
//! a stable ORDER BY sort, LIMIT, the first row of a scalar subquery,
//! an UPDATE OR FAIL's partial work), so the access path chosen is the
//! one the pin's planner chooses where that choice follows from the
//! schema and the query alone. Without `sqlite_stat1` data the pin's
//! choice is a deterministic cost comparison; this module reproduces
//! the parts of it that decide a single table's loop
//! (`whereLoopAddBtree`, `whereLoopAddBtreeIndex`,
//! `whereLoopOutputAdjust`, `whereRangeScanEst`, `whereSortingCost`,
//! the one-table `wherePathSolver`), in the pin's `LogEst` integer
//! arithmetic:
//!
//! - candidates in the pin's order: the full table scan, the rowid
//!   lookups and ranges, then for each index (newest first, the pin's
//!   `pIndex` list) its full scan (when it covers the query and is
//!   narrower than the table, may help the ORDER BY, or is a partial
//!   index the WHERE implies) and its equality-prefix / range loops;
//! - default row estimates (`sqlite3DefaultRowEst`: a table of
//!   1,048,576 rows, ten per value of an index's first column, ...,
//!   one per key of a UNIQUE index, half the table in a partial
//!   index), row widths from the declared types (`szEst`);
//! - the cheapest loop wins, ties going to the earlier candidate; with
//!   an ORDER BY (or GROUP BY) the sorting cost a loop leaves is added
//!   (`whereSortingCost`), a loop that delivers the order is scanned
//!   forward or backward as the order needs.
//!
//! Index usability follows the pin: a term `key op value` where `key`
//! is an index column (or the same expression as an index expression
//! column), `value` reads no column of this source, the comparison's
//! collation is the index column's, and its affinity allows the index
//! (`query::index_probe_aff`). `x IN (list)`, `x BETWEEN a AND b`,
//! `x IS NULL`, `x IS v` and an OR of equalities on one column (the
//! pin's OR-to-IN rewrite) are terms too. A partial index is a
//! candidate only when each AND term of its WHERE is implied by a
//! WHERE term (the same expression, an OR containing it, or `x IS NOT
//! NULL` after a comparison of x — `sqlite3ExprImpliesExpr`).
//!
//! With the `planner` leaf (v0.17) a join of two or more sources
//! is planned as a whole by `crate::xjoin` (the pin's join order,
//! automatic indexes, Bloom filters), from the candidate loops this
//! module builds, and `sqlite_stat1` is read back: its row estimates
//! replace the defaults (`IdxModel::est`, `Table::rows`).
//!
//! With the `planner` leaf (v0.17) `crate::xmulti` adds the
//! MULTI-INDEX OR loop, the min() / max() order, skip-scans (with
//! `sqlite_stat1`), the IN operator's statistics rule and
//! `whereLoopInsert` for one table's loops.
//!
//! What is not modelled (the order may then differ from the pin's,
//! never the rows): without `planner`, join reordering (sources run
//! in FROM order, each with its own best loop), automatic indexes and
//! every `sqlite_stat1` estimate; without `planner`, the
//! MULTI-INDEX OR plan and skip-scans; in every build the LIKE prefix
//! optimisation and `sqlite_stat4`.

use crate::arena::AVec;
use crate::query::{RExpr, RefRange, binary_coll, expr_coll, expr_refs_range, same_expr};
use crate::record::{Affinity, Coll, KeyOrder};
use crate::sql::{BinOp, SortOrder};

// ------------------------------------------------------- LogEst

/// The pin's `sqlite3LogEst`: 10 * log2(x), as an integer.
pub fn log_est(x: u64) -> i32 {
    const A: [i32; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    let mut x = x;
    let mut y: i32 = 40;
    if x < 8 {
        if x < 2 {
            return 0;
        }
        while x < 8 {
            y -= 10;
            x <<= 1;
        }
    } else {
        while x > 255 {
            y += 40;
            x >>= 4;
        }
        while x > 15 {
            y += 10;
            x >>= 1;
        }
    }
    A.get((x & 7) as usize).copied().unwrap_or(0) + y - 10
}

/// The pin's `sqlite3LogEstAdd`: the LogEst of the sum.
pub fn log_est_add(a: i32, b: i32) -> i32 {
    const X: [i32; 32] = [
        10, 10, 9, 9, 8, 8, 7, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2,
        2, 2,
    ];
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    let d = hi - lo;
    if d > 49 {
        hi
    } else if d > 31 {
        hi + 1
    } else {
        hi + X.get(d as usize).copied().unwrap_or(0)
    }
}

/// The pin's `sqlite3LogEstFromDouble` (v0.17, `planner`: a
/// virtual table's estimatedCost).
#[cfg(feature = "planner")]
pub fn log_est_from_double(x: f64) -> i32 {
    if x.is_nan() || x <= 1.0 {
        return 0;
    }
    if x <= 2_000_000_000.0 {
        return log_est(x as u64);
    }
    let e = ((x.to_bits() >> 52) & 0x7ff) as i32 - 1022;
    e * 10
}

/// The pin's `estLog`: the LogEst of log(N) for a LogEst N.
pub(crate) fn est_log(n: i32) -> i32 {
    if n <= 10 { 0 } else { log_est(n as u64) - 33 }
}

/// The pin's default table size (`nRowLogEst`, 1,048,576 rows).
pub(crate) const ROWS: i32 = 200;

/// A column's size estimate (the pin's `Column.szEst`, from
/// `sqlite3AffinityType`): 1 for numeric types and no type, 5 for
/// TEXT / BLOB / CLOB, `k/4 + 1` for `CHAR(k)` / `BLOB(k)`.
pub fn sz_est(decl: &[u8], aff: Affinity) -> u32 {
    let decl = decl.trim_ascii();
    if decl.is_empty() || !matches!(aff, Affinity::Text | Affinity::Blob) {
        return 1;
    }
    // The text after the last "CHAR" (or after "BLOB" when a `(`
    // follows), where the pin reads the size digits.
    let lower: AVec<u8> = {
        let mut v = AVec::new();
        for b in decl {
            v.push(b.to_ascii_lowercase());
        }
        v
    };
    let l = lower.as_slice();
    let mut z: Option<usize> = None;
    let mut i = 0usize;
    while i + 4 <= l.len() {
        let w = l.get(i..i + 4).unwrap_or(&[]);
        if w == b"char" || (w == b"blob" && l.get(i + 4) == Some(&b'(')) {
            z = Some(i + 4);
        }
        i += 1;
    }
    let v: u32 = match z {
        None => 16,
        Some(at) => {
            let rest = l.get(at..).unwrap_or(&[]);
            match rest.iter().position(|b| b.is_ascii_digit()) {
                None => 0,
                Some(p) => {
                    let mut n: u32 = 0;
                    let mut ok = true;
                    for b in rest
                        .get(p..)
                        .unwrap_or(&[])
                        .iter()
                        .take_while(|b| b.is_ascii_digit())
                    {
                        match n
                            .checked_mul(10)
                            .and_then(|n| n.checked_add(u32::from(b - b'0')))
                        {
                            Some(m) if m <= i32::MAX as u32 => n = m,
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok { n } else { 0 }
                }
            }
        }
    };
    (v / 4 + 1).min(255)
}

// --------------------------------------------------------- terms

/// What a term constrains its key to.
#[derive(Debug, Clone)]
pub enum TOp {
    Eq,
    /// `IS` (a NULL value finds NULL keys).
    Is,
    In(AVec<RExpr>),
    /// `x IN (SELECT ...)` (v0.16 review fix): the subquery node with
    /// a NULL left operand, evaluated (once, when uncorrelated) into
    /// the probe values.
    InSub(RExpr),
    Lt,
    Le,
    Gt,
    Ge,
}

/// One usable term of a source: `key op val`.
#[derive(Debug, Clone)]
pub struct Term {
    /// The WHERE / ON conjunct it comes from.
    pub parent: usize,
    /// The constrained side, top-level COLLATE removed.
    pub key: RExpr,
    pub op: TOp,
    /// The value side (`Literal(Null)` for `x IS NULL`; unused for IN).
    pub val: RExpr,
    pub coll: Coll,
    pub aff: Option<Affinity>,
    /// The value reads an earlier source of the same query.
    pub lateral: bool,
    /// The pin's `x IS NULL` cost adjustment applies.
    pub isnull: bool,
    /// The pin's virtual `x > NULL` term for `x IS NOT NULL`
    /// (`TERM_VNULL`): a range for costing; it bounds nothing (the
    /// scan skips NULL keys, the WHERE term still filters).
    pub vnull: bool,
    /// The pin's commuted copy of a comparison whose left operand is
    /// a column too (`TERM_VIRTUAL`, v0.17 `planner`): it sits
    /// after every original term in the WHERE clause's term list.
    #[cfg(feature = "planner")]
    pub virt: bool,
    /// The pin's `truthProb` (v0.17 review fix): 1 when none is given,
    /// else the LogEst of a likely() / unlikely() / likelihood()
    /// probability (<= 0), which replaces the default estimates.
    /// Virtual terms (commuted copies, BETWEEN halves, `x > NULL`,
    /// OR-to-IN) inherit their parent's (`markTermAsChild`).
    #[cfg(feature = "planner")]
    pub truth: i32,
}

impl Term {
    pub(crate) fn is_eq(&self) -> bool {
        matches!(self.op, TOp::Eq | TOp::Is | TOp::In(_) | TOp::InSub(_))
    }
    fn lower(&self) -> bool {
        matches!(self.op, TOp::Gt | TOp::Ge)
    }
    fn upper(&self) -> bool {
        matches!(self.op, TOp::Lt | TOp::Le)
    }
}

/// The source being planned: where its columns sit in the query row.
#[derive(Debug, Clone, Copy)]
pub struct Src {
    pub base: usize,
    pub ncols: usize,
    pub src: usize,
    pub rowid_alias: Option<usize>,
}

impl Src {
    /// Whether `e` reads any column or the rowid of this source.
    pub fn reads(&self, e: &RExpr) -> bool {
        expr_refs_range(
            e,
            &RefRange {
                w: (self.base, self.base + self.ncols),
                s: (self.src, self.src + 1),
            },
        )
    }

    /// Whether WHERE clause `e` is false or NULL whenever this
    /// source's columns are all NULL — the pin's
    /// `sqlite3ExprImpliesNonNullRow` (v0.16 review fix): it turns a
    /// LEFT JOIN to this source into an inner join, so the WHERE may
    /// imply the source's `INDEXED BY` partial index. As on the pin,
    /// a top-level `IS NOT NULL` counts only where the AND chain's
    /// left operands are tried.
    pub fn null_rejects(&self, e: &RExpr) -> bool {
        // (`sqlite3ExprSkipCollateAndLikely` first: v0.17 review fix.)
        let mut p = skip_collate_likely(e);
        if let RExpr::IsNull(a, true) = p {
            return self.non_null_row(a);
        }
        while let RExpr::Binary(BinOp::And, l, r) = p {
            if self.null_rejects(l) {
                return true;
            }
            p = r;
        }
        self.non_null_row(p)
    }

    /// The pin's `impliesNotNullRow` walk under `null_rejects`.
    fn non_null_row(&self, e: &RExpr) -> bool {
        match e {
            RExpr::Column(slot, ..) => (self.base..self.base + self.ncols).contains(slot),
            RExpr::Rowid(s) => *s == self.src,
            RExpr::Binary(BinOp::And | BinOp::Or, a, b) => {
                self.non_null_row(a) && self.non_null_row(b)
            }
            RExpr::Binary(BinOp::Is | BinOp::IsNot, ..) => false,
            RExpr::Binary(_, a, b) => self.non_null_row(a) || self.non_null_row(b),
            RExpr::InList(a, l, ..) if !l.is_empty() => self.non_null_row(a),
            RExpr::Neg(a)
            | RExpr::Pos(a)
            | RExpr::Not(a)
            | RExpr::Cast(a, _)
            | RExpr::Collate(a, _)
            | RExpr::Between(a, ..) => self.non_null_row(a),
            _ => false,
        }
    }

    /// Whether `e` is this source's rowid (or its alias column).
    pub fn is_rowid(&self, e: &RExpr) -> bool {
        match e {
            RExpr::Rowid(s) => *s == self.src,
            RExpr::Column(slot, ..) => self.rowid_alias.is_some_and(|a| *slot == self.base + a),
            _ => false,
        }
    }

    /// Whether `e` is column `c` of this source.
    pub fn is_col(&self, e: &RExpr, c: usize) -> bool {
        matches!(e, RExpr::Column(slot, ..) if *slot == self.base + c)
    }
}

/// Whether `e` holds an explicit COLLATE anywhere outside its
/// subqueries (the pin's `EP_Collate`, which the parser propagates
/// up through operators and function arguments; v0.17 review fix).
/// The pin analyses an OR term holding one neither as an IN nor as
/// a MULTI-INDEX OR (probed: `a=2 OR b='x' COLLATE BINARY` scans).
pub(crate) fn has_collate(e: &RExpr) -> bool {
    let f = has_collate;
    match e {
        RExpr::Collate(..) => true,
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => false,
        RExpr::InSub(x, ..) => f(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) || f(b),
        RExpr::Neg(a) | RExpr::Pos(a) | RExpr::Not(a) | RExpr::IsNull(a, _) | RExpr::Cast(a, _) => {
            f(a)
        }
        RExpr::InList(x, l, ..) => f(x) || l.iter().any(f),
        RExpr::Between(x, a, b, _) => f(x) || f(a) || f(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_some_and(f)
                || whens.iter().any(|(w, t)| f(w) || f(t))
                || otherwise.as_deref().is_some_and(f)
        }
        RExpr::Scalar(_, a) | RExpr::Win(_, a) | RExpr::Agg { args: a, .. } => a.iter().any(f),
    }
}

/// A one-value IN list on a constant, `x IN (v)`, which the pin's
/// parser rewrites as `x = +v` (v0.17 review fix): its `(x, v)`.
pub(crate) fn one_in(e: &RExpr) -> Option<(&RExpr, &RExpr)> {
    match e {
        RExpr::InList(x, items, false, false) => match items.as_slice() {
            [v] if crate::query::is_constant(v) => Some((x, v)),
            _ => None,
        },
        _ => None,
    }
}

/// `e` with top-level COLLATE and likely() / unlikely() /
/// likelihood() wrappers removed (the pin's
/// `sqlite3ExprSkipCollateAndLikely`): neither changes whether a
/// WHERE term is true.
pub fn skip_collate_likely(e: &RExpr) -> &RExpr {
    let mut p = e;
    loop {
        match p {
            RExpr::Collate(a, _) => p = a,
            RExpr::Scalar(crate::query::ScalarFn::Likely, a) => match a.as_slice().first() {
                Some(x) => p = x,
                None => return p,
            },
            _ => return p,
        }
    }
}

/// Split a condition into its AND terms, each with its top-level
/// COLLATE / likely() wrappers removed, as the pin's
/// `sqlite3WhereSplit` and `whereClauseInsert` do (v0.17 review fix:
/// `likely(b.x = a.y)` is still a join constraint).
pub fn conjuncts<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    let mut stack: AVec<&'x RExpr> = AVec::new();
    stack.push(e);
    while let Some(x) = stack.pop() {
        #[cfg(feature = "planner")]
        let x = skip_collate_likely(x);
        match x {
            RExpr::Binary(BinOp::And, a, b) => {
                stack.push(b);
                stack.push(a);
            }
            x => out.push(x),
        }
    }
}

/// Per AND term of `e`, in `conjuncts` order: its truth probability
/// (the pin's `truthProb`: `LogEst(P * 2^27) - 270` for a term that
/// is a likely() / unlikely() / likelihood() call, else 1, "none
/// given") and whether a COLLATE or likely() wrapper was removed
/// above it (the pin's constant propagation does not look inside
/// one). v0.17 review fix.
#[cfg(feature = "planner")]
pub fn conjuncts_meta(e: &RExpr, out: &mut AVec<(i32, bool)>) {
    let mut stack: AVec<(&RExpr, bool)> = AVec::new();
    stack.push((e, false));
    while let Some((x, w)) = stack.pop() {
        let y = skip_collate_likely(x);
        let w = w || !core::ptr::eq(x, y);
        match y {
            RExpr::Binary(BinOp::And, a, b) => {
                stack.push((b, w));
                stack.push((a, w));
            }
            _ => out.push((truth_prob(x), w)),
        }
    }
}

/// The truth probability of WHERE term `e` as written (see
/// `conjuncts_meta`).
#[cfg(feature = "planner")]
fn truth_prob(e: &RExpr) -> i32 {
    if let RExpr::Scalar(crate::query::ScalarFn::Likely, a) = e
        && let Some(RExpr::Literal(crate::record::OwnedValue::Real(p))) = a.as_slice().get(1)
    {
        // (`exprProbability`: `(int)(P * 134217728.0)`; P is in
        // 0.0 ..= 1.0, checked at resolution.)
        let x = *p * 134_217_728.0;
        let n = if x >= 1.0 { x as u64 } else { 0 };
        return log_est(n) - 270;
    }
    1
}

/// The truth probabilities of `e`'s AND terms (`conjuncts_meta`).
#[cfg(feature = "planner")]
pub fn conj_truth(e: &RExpr, out: &mut AVec<i32>) {
    let mut m: AVec<(i32, bool)> = AVec::new();
    conjuncts_meta(e, &mut m);
    for (t, _) in m.iter() {
        out.push(*t);
    }
}

/// Split a condition into its AND terms as written.
#[cfg(feature = "planner")]
fn and_terms<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    let mut stack: AVec<&'x RExpr> = AVec::new();
    stack.push(e);
    while let Some(x) = stack.pop() {
        match x {
            RExpr::Binary(BinOp::And, a, b) => {
                stack.push(b);
                stack.push(a);
            }
            _ => out.push(x),
        }
    }
}

pub(crate) fn strip(e: &RExpr) -> &RExpr {
    match e {
        RExpr::Collate(x, _) => strip(x),
        _ => e,
    }
}

fn flip(op: BinOp) -> Option<TOp> {
    Some(match op {
        BinOp::Eq => TOp::Eq,
        BinOp::Is => TOp::Is,
        BinOp::Lt => TOp::Gt,
        BinOp::Le => TOp::Ge,
        BinOp::Gt => TOp::Lt,
        BinOp::Ge => TOp::Le,
        _ => return None,
    })
}

fn same_op(op: BinOp) -> Option<TOp> {
    Some(match op {
        BinOp::Eq => TOp::Eq,
        BinOp::Is => TOp::Is,
        BinOp::Lt => TOp::Lt,
        BinOp::Le => TOp::Le,
        BinOp::Gt => TOp::Gt,
        BinOp::Ge => TOp::Ge,
        _ => return None,
    })
}

/// The usable terms of one source among `conj` (v0.16). `avail`
/// says whether a value expression may probe this source: `None`
/// when it reads this source or one not yet joined, `Some(lateral)`
/// otherwise.
/// `truth` holds each conjunct's truth probability (`conj_truth`;
/// missing entries have none).
pub fn terms(
    conj: &[&RExpr],
    truth: &[i32],
    s: &Src,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
) -> AVec<Term> {
    let mut out: AVec<Term> = AVec::new();
    for (ci, c) in conj.iter().enumerate() {
        let tp = truth.get(ci).copied().unwrap_or(1);
        add_terms(ci, c, tp, s, avail, &mut out);
    }
    out
}

fn add_terms(
    ci: usize,
    c: &RExpr,
    tp: i32,
    s: &Src,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
    out: &mut AVec<Term>,
) {
    #[cfg(not(feature = "planner"))]
    let _ = tp;
    // `coll`: the comparison's collation as written (a commuted copy
    // keeps it: the pin's `EP_Commuted`; v0.17 fix).
    let cmp = |op: TOp, l: &RExpr, r: &RExpr, coll: Coll, out: &mut AVec<Term>, virt: bool| {
        #[cfg(not(feature = "planner"))]
        let _ = virt;
        let key = strip(l);
        if !s.reads(key) {
            return;
        }
        let Some(lateral) = avail(r) else { return };
        let Some(aff) = crate::query::index_probe_aff(l, r) else {
            return;
        };
        let isnull =
            matches!(op, TOp::Is) && matches!(r, RExpr::Literal(crate::record::OwnedValue::Null));
        out.push(Term {
            parent: ci,
            key: key.clone(),
            op,
            val: r.clone(),
            coll,
            aff,
            lateral,
            isnull,
            vnull: false,
            #[cfg(feature = "planner")]
            virt,
            #[cfg(feature = "planner")]
            truth: tp,
        });
    };
    match c {
        RExpr::Binary(op, l, r) => {
            let coll = binary_coll(l, r);
            if let Some(o) = same_op(*op) {
                cmp(o, l, r, coll, out, false);
            }
            if let Some(o) = flip(*op) {
                cmp(
                    o,
                    r,
                    l,
                    coll,
                    out,
                    matches!(strip(l), RExpr::Column(..) | RExpr::Rowid(_)),
                );
            }
            // (An OR term holding an explicit COLLATE is not analysed
            // as an OR at all: v0.17 review fix.)
            let or = *op == BinOp::Or && !has_collate(c);
            if or && let Some(t) = or_in(ci, c, tp, s, avail) {
                out.push(t);
            }
            // The pin's `whereCombineDisjuncts` (v0.17, `planner`):
            // `x < 5 OR x = 5` is also the virtual term `x <= 5`.
            #[cfg(feature = "planner")]
            if or {
                for x in crate::xmulti::combined(c).iter() {
                    let from = out.len();
                    add_terms(ci, x, 1, s, avail, out);
                    for t in out.as_mut_slice().get_mut(from..).unwrap_or(&mut []) {
                        t.virt = true;
                    }
                }
            }
        }
        // The pin's two virtual halves, each analysed in turn: `x >=
        // a` and its commuted copy `a <= x`, then `x <= b` and `b >=
        // x` (v0.17 review fix: a column bound probes its own table).
        RExpr::Between(x, a, b, false) => {
            cmp(TOp::Ge, x, a, binary_coll(x, a), out, true);
            cmp(TOp::Le, a, x, binary_coll(x, a), out, true);
            cmp(TOp::Le, x, b, binary_coll(x, b), out, true);
            cmp(TOp::Ge, b, x, binary_coll(x, b), out, true);
        }
        RExpr::InList(x, items, false, false) => {
            let key = strip(x);
            if !s.reads(key) || items.is_empty() {
                return;
            }
            // `x IN (v)` on a constant is the pin's `x = +v` (v0.17
            // review fix): an equality, compared as the IN compares.
            if let Some((_, v)) = one_in(c) {
                let Some(lateral) = avail(v) else { return };
                out.push(Term {
                    parent: ci,
                    key: key.clone(),
                    op: TOp::Eq,
                    val: v.clone(),
                    coll: expr_coll(x).unwrap_or_default(),
                    aff: crate::query::index_in_aff(x),
                    lateral,
                    isnull: false,
                    vnull: false,
                    #[cfg(feature = "planner")]
                    virt: false,
                    #[cfg(feature = "planner")]
                    truth: tp,
                });
                return;
            }
            let mut lateral = false;
            for i in items.iter() {
                match avail(i) {
                    Some(l) => lateral |= l,
                    None => return,
                }
            }
            out.push(Term {
                parent: ci,
                key: key.clone(),
                op: TOp::In(items.clone()),
                val: RExpr::Literal(crate::record::OwnedValue::Null),
                coll: expr_coll(x).unwrap_or_default(),
                aff: crate::query::index_in_aff(x),
                lateral,
                isnull: false,
                vnull: false,
                #[cfg(feature = "planner")]
                virt: false,
                #[cfg(feature = "planner")]
                truth: tp,
            });
        }
        // `x IN (SELECT ...)` (v0.16 review fix): probed with the
        // subquery's values, as the pin's ephemeral IN table.
        RExpr::InSub(x, plan, false) => {
            let key = strip(x);
            if plan.width() != 1
                || matches!(key, RExpr::Scalar(crate::query::ScalarFn::Vector, _))
                || !s.reads(key)
            {
                return;
            }
            let probe = RExpr::InSub(
                alloc::boxed::Box::new(RExpr::Literal(crate::record::OwnedValue::Null)),
                plan.clone(),
                false,
            );
            let Some(lateral) = avail(&probe) else { return };
            let rhs = RExpr::AffLiteral(
                crate::record::OwnedValue::Null,
                crate::query::result_affinity(plan),
                None,
            );
            let Some(aff) = crate::query::index_probe_aff(x, &rhs) else {
                return;
            };
            // The probe keeps its left operand for EXPLAIN QUERY PLAN
            // (the IN operator's index; v0.17 review fix); nothing
            // evaluates it.
            let probe = if crate::query::has_sub(x) {
                probe
            } else {
                RExpr::InSub(x.clone(), plan.clone(), false)
            };
            out.push(Term {
                parent: ci,
                key: key.clone(),
                op: TOp::InSub(probe),
                val: RExpr::Literal(crate::record::OwnedValue::Null),
                coll: crate::query::in_sub_coll(x, plan),
                aff,
                lateral,
                isnull: false,
                vnull: false,
                #[cfg(feature = "planner")]
                virt: false,
                #[cfg(feature = "planner")]
                truth: tp,
            });
        }
        RExpr::IsNull(x, false) => {
            let key = strip(x);
            if !s.reads(key) {
                return;
            }
            out.push(Term {
                parent: ci,
                key: key.clone(),
                op: TOp::Is,
                val: RExpr::Literal(crate::record::OwnedValue::Null),
                coll: expr_coll(x).unwrap_or_default(),
                aff: None,
                lateral: false,
                isnull: true,
                vnull: false,
                #[cfg(feature = "planner")]
                virt: false,
                #[cfg(feature = "planner")]
                truth: tp,
            });
        }
        // `x IS NOT NULL` on a column (not the rowid): the pin adds
        // a virtual `x > NULL` range term.
        RExpr::IsNull(x, true) => {
            let key = strip(x);
            if !matches!(key, RExpr::Column(..)) || !s.reads(key) || s.is_rowid(key) {
                return;
            }
            out.push(Term {
                parent: ci,
                key: key.clone(),
                op: TOp::Gt,
                val: RExpr::Literal(crate::record::OwnedValue::Null),
                coll: expr_coll(x).unwrap_or_default(),
                aff: None,
                lateral: false,
                isnull: false,
                vnull: true,
                #[cfg(feature = "planner")]
                virt: true,
                #[cfg(feature = "planner")]
                truth: tp,
            });
        }
        _ => {}
    }
}

/// The pin's OR-to-IN rewrite: `x = a OR x = b OR ...` on one key,
/// every value available, is `x IN (a, b, ...)`.
pub(crate) fn or_in(
    ci: usize,
    c: &RExpr,
    tp: i32,
    s: &Src,
    avail: &dyn Fn(&RExpr) -> Option<bool>,
) -> Option<Term> {
    #[cfg(not(feature = "planner"))]
    let _ = tp;
    let mut leaves: AVec<&RExpr> = AVec::new();
    let mut stack: AVec<&RExpr> = AVec::new();
    stack.push(c);
    while let Some(x) = stack.pop() {
        match x {
            RExpr::Binary(BinOp::Or, a, b) => {
                stack.push(b);
                stack.push(a);
            }
            _ => leaves.push(x),
        }
    }
    let mut key: Option<(&RExpr, &RExpr)> = None;
    let mut items = AVec::new();
    let mut lateral = false;
    for l in leaves.iter() {
        // (A branch's likely() wrapper is the pin's `whereSplit`'s to
        // skip; a one-value IN is its parser's `x = +v`, whose value
        // has no affinity: v0.17 review fixes.)
        let l = skip_collate_likely(l);
        let (k, v, plus) = match (l, one_in(l)) {
            (_, Some((x, v))) => (x, v, true),
            (RExpr::Binary(BinOp::Eq, a, b), None) => {
                if s.reads(strip(a)) {
                    (&**a, &**b, false)
                } else {
                    (&**b, &**a, false)
                }
            }
            _ => return None,
        };
        let kk = strip(k);
        if !s.reads(kk) {
            return None;
        }
        // The pin's `okToChngToIN`: a value with an affinity of its
        // own must have the key's (v0.17 fix: `x.c0 = y.c1 OR x.c0 =
        // y.c2` probed a TEXT key with REAL values, losing rows).
        let aff = |e: &RExpr| match strip(e) {
            RExpr::Column(_, a, _) | RExpr::Cast(_, a) | RExpr::AffLiteral(_, Some(a), _) => {
                Some(*a)
            }
            RExpr::Rowid(_) => Some(Affinity::Integer),
            _ => None,
        };
        if !plus && aff(v).is_some_and(|a| Some(a) != aff(kk)) {
            return None;
        }
        match key {
            None => key = Some((kk, k)),
            Some((p, _)) if same_expr(p, kk) => {}
            _ => return None,
        }
        lateral |= avail(v)?;
        items.push(v.clone());
    }
    let (kk, k) = key?;
    Some(Term {
        parent: ci,
        key: kk.clone(),
        op: TOp::In(items),
        val: RExpr::Literal(crate::record::OwnedValue::Null),
        coll: expr_coll(k).unwrap_or_default(),
        aff: crate::query::index_in_aff(k),
        lateral,
        isnull: false,
        vnull: false,
        #[cfg(feature = "planner")]
        virt: true,
        #[cfg(feature = "planner")]
        truth: tp,
    })
}

// ------------------------------------------------------- indexes

/// One key column of an index model.
#[derive(Debug, Clone)]
pub enum IdxKey {
    Col(usize),
    Rowid,
    /// An expression over the source (slots already shifted).
    Expr(RExpr),
}

/// An index as the planner sees it.
#[derive(Debug, Clone)]
pub struct IdxModel {
    pub root: u32,
    pub unique: bool,
    pub keys: AVec<IdxKey>,
    pub desc: AVec<KeyOrder>,
    /// A partial index's WHERE (shifted onto the source).
    pub pred: Option<RExpr>,
    /// `szIdxRow` (LogEst).
    pub width: i32,
    /// The columns a WITHOUT ROWID table's secondary index holds
    /// after its key, in entry order (v0.16: the table's key
    /// columns it lacks), with their orders; empty otherwise.
    pub suffix: AVec<(IdxKey, KeyOrder)>,
    /// The entries hold every column (a WITHOUT ROWID table's
    /// clustered index, v0.16).
    pub covers_all: bool,
    /// `suffix` counts toward covering (v0.16: not for a UNIQUE
    /// constraint's index of a WITHOUT ROWID table, whose
    /// not-indexed columns the pin computed before the key columns
    /// were appended).
    pub suffix_covers: bool,
    /// `aiRowLogEst` from `sqlite_stat1` (v0.17, `planner`): the
    /// index's rows, then the rows per distinct prefix of 1, 2, ...
    /// key columns; `None` for the pin's defaults.
    #[cfg(feature = "planner")]
    pub est: Option<AVec<i32>>,
    /// The SELECT reads the table's columns the index lacks only
    /// inside expressions the index holds (the pin's
    /// `whereIsCoveringIndex`, `WHERE_EXPRIDX`; v0.17 review fix): it
    /// counts as covering for the plan and EXPLAIN QUERY PLAN, while
    /// hipp still reads the table row for the expressions' values.
    pub xcover: bool,
    /// The index has `sqlite_stat1` rows (v0.17, `planner`; the
    /// pin's `hasStat1`): `est` may hold defaults otherwise.
    #[cfg(feature = "planner")]
    pub stat1: bool,
    /// Its `sqlite_stat1` text says `unordered` (v0.17 review fix,
    /// `planner`; the pin's `bUnordered`): no range constraint,
    /// no ORDER BY and no covering full scan through it.
    #[cfg(feature = "planner")]
    pub unordered: bool,
    /// ... or `noskipscan` (`noSkipScan`): no skip-scan.
    #[cfg(feature = "planner")]
    pub noskip: bool,
}

/// `m.unordered` (false without `planner`).
fn unordered(m: &IdxModel) -> bool {
    #[cfg(feature = "planner")]
    return m.unordered;
    #[cfg(not(feature = "planner"))]
    {
        let _ = m;
        false
    }
}

impl IdxModel {
    /// Whether `e` holds one of this index's key expressions outside
    /// every aggregate call's arguments (v0.17 review fix): in an
    /// aggregate query the pin reads such a result column or ORDER BY
    /// term from the table, so the index does not cover (probed:
    /// `SELECT a+1, max(a+1) FROM t` over an index on `a+1`).
    #[cfg(feature = "planner")]
    pub fn bare_expr(&self, e: &RExpr) -> bool {
        if self
            .keys
            .iter()
            .any(|k| matches!(k, IdxKey::Expr(x) if same_expr(e, x)))
        {
            return true;
        }
        let f = |x: &RExpr| self.bare_expr(x);
        match e {
            RExpr::Agg { .. }
            | RExpr::Column(..)
            | RExpr::Rowid(_)
            | RExpr::Literal(_)
            | RExpr::AffLiteral(..)
            | RExpr::Raise(_)
            | RExpr::SubScalar(_)
            | RExpr::SubExists(_) => false,
            RExpr::InSub(x, ..) => f(x),
            RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) || f(b),
            RExpr::Neg(a)
            | RExpr::Pos(a)
            | RExpr::Not(a)
            | RExpr::IsNull(a, _)
            | RExpr::Cast(a, _)
            | RExpr::Collate(a, _) => f(a),
            RExpr::InList(x, l, ..) => f(x) || l.iter().any(f),
            RExpr::Between(x, a, b, _) => f(x) || f(a) || f(b),
            RExpr::Case {
                base,
                whens,
                otherwise,
            } => {
                base.as_deref().is_some_and(f)
                    || whens.iter().any(|(w, t)| f(w) || f(t))
                    || otherwise.as_deref().is_some_and(f)
            }
            RExpr::Scalar(_, a) | RExpr::Win(_, a) => a.iter().any(f),
        }
    }

    /// Whether `e` (an expression of the statement) reads a column of
    /// source `s` that this index holds neither as a key column nor
    /// inside one of its key expressions (`whereIsCoveringIndex`'s
    /// walk; anything it does not look into counts when it reads the
    /// source).
    #[cfg(feature = "planner")]
    pub fn reads_uncovered(&self, e: &RExpr, s: &Src) -> bool {
        if self
            .keys
            .iter()
            .any(|k| matches!(k, IdxKey::Expr(x) if same_expr(e, x)))
        {
            return false;
        }
        let kids: AVec<&RExpr> = match e {
            RExpr::Column(slot, ..) => {
                let Some(c) = slot.checked_sub(s.base).filter(|c| *c < s.ncols) else {
                    return false;
                };
                return s.rowid_alias != Some(c)
                    && !self
                        .keys
                        .iter()
                        .any(|k| matches!(k, IdxKey::Col(x) if *x == c));
            }
            RExpr::Rowid(_) | RExpr::Literal(_) | RExpr::AffLiteral(..) => return false,
            RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => [&**a, &**b].into_iter().collect(),
            RExpr::Neg(a)
            | RExpr::Pos(a)
            | RExpr::Not(a)
            | RExpr::IsNull(a, _)
            | RExpr::Cast(a, _)
            | RExpr::Collate(a, _) => [&**a].into_iter().collect(),
            RExpr::InList(x, l, ..) => core::iter::once(&**x).chain(l.iter()).collect(),
            RExpr::Between(x, a, b, _) => [&**x, &**a, &**b].into_iter().collect(),
            RExpr::Case {
                base,
                whens,
                otherwise,
            } => base
                .iter()
                .chain(otherwise.iter())
                .map(|b| &**b)
                .chain(whens.iter().flat_map(|(w, t)| [w, t]))
                .collect(),
            // (An aggregate's arguments too: `min(a+1)` over an index
            // on `a+1` is covered; v0.17 review fix.)
            RExpr::Scalar(_, args) | RExpr::Agg { args, .. } => args.iter().collect(),
            _ => return s.reads(e),
        };
        kids.iter().any(|k| self.reads_uncovered(k, s))
    }
    pub(crate) fn key_matches(&self, k: usize, s: &Src, t: &Term) -> bool {
        // `x IS NULL` finds NULL keys under any collation (the pin
        // checks no collation for WO_ISNULL).
        let coll_ok = t.isnull
            || self
                .desc
                .as_slice()
                .get(k)
                .is_some_and(|o| o.coll == t.coll);
        coll_ok
            && match self.keys.as_slice().get(k) {
                Some(IdxKey::Col(c)) => s.is_col(&t.key, *c),
                Some(IdxKey::Rowid) => s.is_rowid(&t.key),
                Some(IdxKey::Expr(e)) => same_expr(&t.key, e),
                None => false,
            }
    }

    /// Whether every column the query reads is a key column.
    pub fn covers(&self, needed: &[bool]) -> bool {
        self.xcover || self.covers_plain(needed)
    }

    /// `covers` by the entries' own columns: the rows can be built from
    /// the entries alone.
    pub fn covers_plain(&self, needed: &[bool]) -> bool {
        self.covers_all
            || needed.iter().enumerate().all(|(c, n)| {
                !*n || self
                    .keys
                    .iter()
                    .chain(
                        self.suffix
                            .iter()
                            .filter(|_| self.suffix_covers)
                            .map(|(k, _)| k),
                    )
                    .any(|k| matches!(k, IdxKey::Col(x) if *x == c))
            })
    }

    /// Whether an ORDER BY term might be delivered by this index (the
    /// pin's `indexMightHelpWithOrderBy`).
    pub(crate) fn might_order(&self, s: &Src, order: &[(RExpr, SortOrder)]) -> bool {
        !unordered(self)
            && order.iter().any(|(e, _)| {
                let e = strip(e);
                if s.is_rowid(e) {
                    return true;
                }
                self.keys.iter().any(|k| match k {
                    IdxKey::Col(c) => s.is_col(e, *c),
                    IdxKey::Rowid => false,
                    IdxKey::Expr(x) => same_expr(e, x),
                })
            })
    }
}

/// Whether the WHERE terms `conj` imply a partial index's `pred`
/// (each AND term of it; the pin's `whereUsablePartialIndex` with
/// `sqlite3ExprImpliesExpr`).
pub fn implies(conj: &[&RExpr], pred: &RExpr) -> bool {
    let mut parts = AVec::new();
    #[cfg(feature = "planner")]
    and_terms(pred, &mut parts);
    #[cfg(not(feature = "planner"))]
    conjuncts(pred, &mut parts);
    parts.iter().all(|p| conj.iter().any(|c| implies1(c, p)))
}

fn implies1(c: &RExpr, p: &RExpr) -> bool {
    if implies2(c, p) {
        return true;
    }
    // The pin's `exprAnalyze` commutes a comparison whose right
    // operand is a column (`0 < b` is the term `b > 0`; when both
    // operands are columns it adds the commuted copy): either form
    // may imply the WHERE (v0.16 review fix).
    if let RExpr::Binary(op, l, r) = c
        && matches!(
            op,
            BinOp::Eq | BinOp::Is | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
        && matches!(strip(r), RExpr::Column(..) | RExpr::Rowid(_))
    {
        let fop = match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::Le => BinOp::Ge,
            BinOp::Gt => BinOp::Lt,
            BinOp::Ge => BinOp::Le,
            o => *o,
        };
        let commuted = RExpr::Binary(fop, r.clone(), l.clone());
        return implies2(&commuted, p);
    }
    false
}

fn implies2(c: &RExpr, p: &RExpr) -> bool {
    if same_expr(c, p) {
        return true;
    }
    match p {
        RExpr::Binary(BinOp::Or, a, b) => implies1(c, a) || implies1(c, b),
        RExpr::IsNull(x, true) => not_null(c, x),
        _ => false,
    }
}

/// The pin's `exprImpliesNotNull`: `p` is true (or, `seen_not`,
/// any non-NULL value) only when `x` is not NULL. Iterative over
/// the operand chain, so no recursion on SQL-derived depth.
fn not_null(c: &RExpr, x: &RExpr) -> bool {
    let mut stack: AVec<(&RExpr, bool)> = AVec::new();
    stack.push((c, false));
    while let Some((p, seen_not)) = stack.pop() {
        if same_expr(p, x) {
            if !matches!(x, RExpr::Literal(crate::record::OwnedValue::Null)) {
                return true;
            }
            continue;
        }
        match p {
            RExpr::InList(l, ..) => stack.push((l, true)),
            // `NOT IN (SELECT ...)` / `NOT BETWEEN` are the pin's
            // TK_NOT over the operator, so `seen_not` holds there
            // (v0.16 review fix: the negation was ignored).
            RExpr::InSub(l, _, neg) => {
                if !seen_not && !neg {
                    stack.push((l, true));
                }
            }
            RExpr::Between(l, a, b, neg) => {
                if !seen_not && !neg {
                    stack.push((l, true));
                    stack.push((b, true));
                    stack.push((a, true));
                }
            }
            RExpr::Binary(op, l, r) => {
                let sn = match op {
                    BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
                    | BinOp::Add
                    | BinOp::Sub
                    | BinOp::BitOr
                    | BinOp::Shl
                    | BinOp::Shr
                    | BinOp::Concat => true,
                    BinOp::Mul | BinOp::Mod | BinOp::BitAnd | BinOp::Div => seen_not,
                    _ => continue,
                };
                stack.push((l, sn));
                stack.push((r, sn));
            }
            RExpr::Collate(l, _) | RExpr::Pos(l) | RExpr::Neg(l) => stack.push((l, seen_not)),
            RExpr::Not(l) => stack.push((l, true)),
            RExpr::Scalar(crate::query::ScalarFn::BitNot, args) => {
                if let Some(l) = args.as_slice().first() {
                    stack.push((l, true));
                }
            }
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------- loops

/// One candidate loop.
#[derive(Debug, Clone)]
pub(crate) struct Loop {
    /// None: the table's rowid B-tree.
    pub(crate) ix: Option<usize>,
    /// One term per equality key column (or the rowid).
    pub(crate) eq: AVec<usize>,
    pub(crate) lo: Option<usize>,
    pub(crate) hi: Option<usize>,
    pub(crate) run: i32,
    pub(crate) out: i32,
    /// The ORDER BY terms the loop delivers in order (all of them
    /// when equal to the term count), and whether it must scan
    /// backwards for them.
    pub(crate) sorted: usize,
    pub(crate) reverse: bool,
    /// The pin's `WHERE_BIGNULL_SORT`: an ORDER BY term wants its
    /// NULLs where the index does not put them (`Some(nulls_first)`);
    /// the loop moves the NULL-keyed rows to that end.
    pub(crate) bignull: Option<bool>,
    /// A MULTI-INDEX OR loop (v0.17, `planner`; the pin's
    /// `WHERE_MULTI_OR`): the OR term's place in the source's list of
    /// OR terms (`xmulti::OrInfo`). Its other fields are empty.
    #[cfg(feature = "planner")]
    pub(crate) or: Option<usize>,
    /// The leading index columns a skip-scan steps over (v0.17,
    /// `planner`; the pin's `nSkip`): each is an `eq` slot with
    /// no term (`usize::MAX`).
    #[cfg(feature = "planner")]
    pub(crate) skip: usize,
}

/// What the planner needs about the query around the source.
pub struct Query<'x> {
    /// The source's WHERE / ON terms (usable or not).
    pub conj: &'x [&'x RExpr],
    /// Per conjunct: its truth probability (`conj_truth`; missing
    /// entries have none).
    pub truth: &'x [i32],
    /// The ORDER BY (or GROUP BY) the loop may deliver; empty when
    /// the source is joined with others or the query has none.
    pub order: &'x [(RExpr, SortOrder)],
    /// The order is a GROUP BY or DISTINCT one (direction-free).
    pub group: bool,
    /// ... a GROUP BY one: a loop delivering it emits the groups in
    /// its own order (the pin then skips its group sorter).
    pub group_by: bool,
    /// ... the ORDER BY too (the pin's `orderByGrp`, v0.17 review
    /// fix; `order` then holds its terms and directions): a loop
    /// delivering the GROUP BY walks the index the way that also
    /// delivers the ORDER BY, when one does (`WHERE_SORTBYGROUP`).
    pub obgrp: bool,
    /// Result columns (the sorting cost's width term).
    pub ncols_out: usize,
    /// Per table column: read by the query. `None`: all of them.
    pub needed: Option<&'x [bool]>,
    /// Full index scans for covering are allowed (not for UPDATE /
    /// DELETE: the pin wants a one-pass loop there).
    pub cover_scan: bool,
    /// A constant LIMIT, as a LogEst.
    pub limit: Option<i32>,
    pub distinct: bool,
    /// A DISTINCT's result columns when `order` is an ORDER BY
    /// (v0.16, `explain`): whether the chosen loop also brings equal
    /// rows together is what EXPLAIN QUERY PLAN reports.
    #[cfg(feature = "explain")]
    pub dset: &'x [(RExpr, SortOrder)],
    /// The join around the source (v0.17, `planner`): which
    /// sources each conjunct and term reads, so a loop's nOut is
    /// adjusted only by the terms its prerequisites make
    /// computable (`whereLoopOutputAdjust`'s `notAllowed`).
    #[cfg(feature = "planner")]
    pub jm: Option<&'x JMask<'x>>,
}

/// `Query::jm` (v0.17, `planner`).
#[cfg(feature = "planner")]
pub struct JMask<'x> {
    /// Per `Query::conj` entry: the sources it reads.
    pub cmask: &'x [u64],
    /// Per usable term: the sources its value reads.
    pub tmask: &'x [u64],
    /// This source's bit.
    pub me: u64,
    /// The FROM-order prerequisites (CROSS / LEFT JOIN).
    pub pre: u64,
}

/// The table being planned.
pub struct Table<'x> {
    /// `szTabRow`.
    pub width: i32,
    pub indexes: &'x [IdxModel],
    /// A WITHOUT ROWID table (v0.16): no rowid loops; its clustered
    /// index (`covers_all`) is the table, and every index's full
    /// scan is a candidate (the pin's `!HasRowid` rule).
    pub wr: bool,
    /// The source's `INDEXED BY` / `NOT INDEXED` (v0.16): only the
    /// named index (its full scan always a candidate, as the pin's
    /// `isIndexedBy`), or only the table itself.
    pub hint: crate::query::Hint,
    /// Per table column: never NULL (NOT NULL, the INTEGER PRIMARY
    /// KEY, a WITHOUT ROWID table's PRIMARY KEY columns). The pin
    /// offers no `x IS NULL` term to such a key column
    /// (`indexColumnNotNull`; v0.16 fix).
    pub notnull: &'x [bool],
    /// `nRowLogEst` (v0.17, `planner`): from `sqlite_stat1`, else
    /// the pin's default 200 (1,048,576 rows).
    #[cfg(feature = "planner")]
    pub rows: i32,
}

impl Table<'_> {
    /// The table's row estimate (`nRowLogEst`).
    pub(crate) fn nrow(&self) -> i32 {
        #[cfg(feature = "planner")]
        return self.rows;
        #[cfg(not(feature = "planner"))]
        ROWS
    }
}

/// How an executor reads one probe set.
#[derive(Debug, Clone)]
pub struct Probe {
    pub vals: AVec<RExpr>,
    pub is_in: bool,
    /// An IN subquery whose values are the probe set (v0.16 review
    /// fix; `vals` is then empty).
    pub sub: Option<RExpr>,
    /// `IS` semantics: a NULL value finds NULL keys.
    pub is: bool,
    pub aff: Option<Affinity>,
}

/// A range bound.
#[derive(Debug, Clone)]
pub struct Bound {
    pub e: RExpr,
    pub incl: bool,
    pub aff: Option<Affinity>,
}

/// A source's access path (v0.16, `expr_index`): the B-tree to walk
/// and the probes to walk it with. Executed by `session::xindex`.
#[derive(Debug, Clone)]
pub struct IdxAccess {
    /// An index's root page; 0 for the table's own rowid B-tree.
    pub root: u32,
    pub desc: AVec<KeyOrder>,
    pub eq: AVec<Probe>,
    pub lo: Option<Bound>,
    pub hi: Option<Bound>,
    /// The range applies to the rowid after every key column.
    pub range_rowid: bool,
    pub reverse: bool,
    /// Some probe reads an earlier source of the query: the rows are
    /// found per combination of those sources' rows.
    pub lateral: bool,
    /// An UPDATE processes the rows in this order (the pin's one-pass
    /// loop); otherwise in rowid order.
    pub one_pass: bool,
    /// The rows arrive in GROUP BY order: the groups are emitted as
    /// they arrive (a DESC index column: largest first), not sorted.
    pub groups: bool,
    /// The index covers the query (v0.16 review fix): per entry
    /// field, the table column it holds. The rows are built from the
    /// entries, with no table lookup (set by the SELECT planner).
    pub cover: Option<AVec<Option<usize>>>,
    /// The pin's two-pass WHERE_BIGNULL_SORT scan (v0.16 review fix):
    /// `(key field, nulls_first)` — each probe's rows with a NULL in
    /// that key field move to the front (or back), in scan order.
    pub bignull: Option<(usize, bool)>,
    /// How many ORDER BY terms the loop's order delivers (v0.17: a
    /// lone source delivering them all streams its rows unsorted).
    pub sorted: usize,
    /// An automatic index (v0.17, `planner`): the rows are the
    /// table's, sorted on the index key once per statement and
    /// probed per combination of the sources before it.
    #[cfg(feature = "planner")]
    pub auto: Option<alloc::boxed::Box<AutoIdx>>,
    /// The pin's Bloom filter on the lookups (v0.17, `planner`):
    /// the key each table row contributes, per equality column; a
    /// probe whose key is not in the filter skips the lookup.
    #[cfg(feature = "planner")]
    pub bloom: Option<AVec<BKey>>,
    /// A MULTI-INDEX OR loop (v0.17, `planner`): the branches'
    /// own access paths, read in turn, each row once.
    #[cfg(feature = "planner")]
    pub or: Option<alloc::boxed::Box<crate::xmulti::OrAccess>>,
    /// A skip-scan (v0.17, `planner`): the leading key columns
    /// whose every value is stepped over (`ANY(a)`) before `eq`.
    #[cfg(feature = "planner")]
    pub skip: usize,
    /// The pin's min() / max() early-out (v0.17, `planner`): the
    /// statement needs only the first row of this path, in its order,
    /// that passes this condition (the WHERE clause; a `TRUE` literal
    /// when it has none).
    /// `(condition, base, src)`: where the source's columns and rowid
    /// sit in the query row the condition reads.
    #[cfg(feature = "planner")]
    pub first: Option<alloc::boxed::Box<(RExpr, usize, usize)>>,
    /// What EXPLAIN QUERY PLAN says of the loop (v0.16, `explain`):
    /// how many ORDER BY (or GROUP BY / DISTINCT) terms it delivers,
    /// whether its index covers the statement's columns, whether its
    /// lower bound is the pin's virtual `x > NULL` of an `x IS NOT
    /// NULL` term (printed `x>?`, though it bounds nothing), and
    /// whether it brings a DISTINCT's equal rows together
    /// (`Query::dset`).
    #[cfg(feature = "explain")]
    pub eqp: (usize, bool, bool, bool),
}

/// One key column of a Bloom filter (v0.17, `planner`): a table
/// column, the rowid, or an index expression (whose filter, never
/// built, admits every probe).
#[cfg(feature = "planner")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BKey {
    Col(usize),
    Rowid,
    Expr,
}

/// One key column of an automatic index (v0.17, `planner`).
#[derive(Debug, Clone)]
pub struct AutoKey {
    /// The table column.
    pub col: usize,
    /// The comparison's collation (the index column's).
    pub coll: Coll,
    /// The probe: the other side of the `col = value` term.
    pub val: RExpr,
    pub aff: Option<Affinity>,
    /// `IS`: a NULL probe finds NULL keys.
    pub is: bool,
}

/// An automatic index (v0.17, `planner`; the pin's
/// `constructAutomaticIndex`): the key columns the usable equality
/// terms constrain (in WHERE-clause order), then the other columns
/// the statement reads (BINARY, in column order), then the rowid.
/// Rows with equal keys come in that order.
#[derive(Debug, Clone)]
pub struct AutoIdx {
    pub keys: AVec<AutoKey>,
    pub extra: AVec<usize>,
    /// The WHERE clause constrains the table alone (`AUTOMATIC
    /// PARTIAL COVERING INDEX`): the index holds only the rows those
    /// terms admit (the executor still evaluates them).
    pub partial: bool,
    /// A Bloom filter of the keys is checked before each probe (every
    /// key collation BINARY, as on the pin).
    pub bloom: bool,
}

/// The pin's `whereSortingCost`.
pub(crate) fn sort_cost(q: &Query<'_>, n_row: i32, n_ob: usize, n_sorted: usize) -> i32 {
    let n_col = log_est(((q.ncols_out as u64) + 59) / 30);
    let mut cost = n_row + n_col;
    if n_sorted > 0 && n_ob > 0 {
        cost += log_est(((n_ob - n_sorted) * 100 / n_ob) as u64) - 66;
    }
    let mut n = n_row;
    if let Some(l) = q.limit {
        cost += 10;
        if n_sorted != 0 {
            cost += 6;
        }
        if l < n {
            n = l;
        }
    } else if q.distinct && n > 10 {
        n -= 10;
    }
    cost + est_log(n)
}

/// Whether term `t` is an IN list.
pub(crate) fn is_in(ts: &[Term], t: usize) -> bool {
    ts.get(t)
        .is_some_and(|t| matches!(t.op, TOp::In(_) | TOp::InSub(_)))
}

/// One column the loop delivers in order: the key, its direction.
enum Ord<'x> {
    Key(&'x IdxKey, KeyOrder),
    Rowid,
    /// Past a WITHOUT ROWID index's full key (v0.16): the rows are
    /// unique, every later term is tie-free.
    End,
}

/// How many leading ORDER BY terms a loop delivers, and whether
/// backwards (the pin's `wherePathSatisfiesOrderBy`, simplified:
/// equality-constrained columns and WHERE `col = value` terms are
/// constants; an IN column ends the delivered order).
pub(crate) fn order_sat(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    l: &Loop,
    in_eq: bool,
) -> (usize, bool, Option<bool>) {
    let n = q.order.len();
    if n == 0 {
        return (0, false, None);
    }
    // A MULTI-INDEX OR loop delivers no order (the pin's
    // `wherePathSatisfiesOrderBy` meets no index; v0.17).
    #[cfg(feature = "planner")]
    if l.or.is_some() {
        return (0, false, None);
    }
    // A loop that finds at most one row delivers any order.
    let one = |ix: Option<&IdxModel>| {
        l.lo.is_none()
            && l.hi.is_none()
            && l.eq.iter().all(|t| !is_in(ts, *t))
            && match ix {
                None => !l.eq.is_empty(),
                // v0.16 fix: `IS` / `IS NULL` on a UNIQUE index may find
                // many rows (NULLs repeat); the pin's WHERE_ONEROW needs
                // `=` there.
                Some(m) => {
                    m.unique
                        && l.eq.len() >= m.keys.len()
                        && l.eq.iter().all(|t| {
                            ts.get(*t)
                                .is_some_and(|t| matches!(t.op, TOp::Eq) && !t.isnull)
                        })
                }
            }
    };
    let ix = l.ix.and_then(|i| tab.indexes.get(i));
    if one(ix) {
        return (n, false, None);
    }
    if ix.is_some_and(unordered) {
        return (0, false, None);
    }
    // The delivered columns: the key columns (a single-value
    // equality column is constant; an IN column is walked in key
    // order), then the rowid.
    let mut cols: AVec<(Ord<'_>, bool)> = AVec::new();
    match ix {
        None => {
            // A rowid IN list is probed in sorted order (v0.16 fix,
            // as the pin's: `rowid IN (...) ORDER BY rowid` sorts
            // nothing).
            if l.eq.iter().all(|t| is_in(ts, *t)) {
                cols.push((Ord::Rowid, false));
            }
        }
        Some(m) => {
            for (k, key) in m.keys.iter().enumerate() {
                // (A skip-scan's columns, its equality ones too, are
                // walked in key order: the pin's `nSkip` rule.)
                let constant = skip_of(l) == 0
                    && l.eq
                        .as_slice()
                        .get(k)
                        .is_some_and(|t| in_eq || !is_in(ts, *t));
                cols.push((
                    Ord::Key(key, m.desc.as_slice().get(k).copied().unwrap_or_default()),
                    constant,
                ));
            }
            if tab.wr {
                for (key, o) in m.suffix.iter() {
                    cols.push((Ord::Key(key, *o), false));
                }
                cols.push((Ord::End, false));
            } else {
                cols.push((Ord::Rowid, false));
            }
        }
    }
    // Constant keys: the loop's equality columns, and any WHERE
    // `key = value` term with a static value.
    // (A column's `=` / `IS` term holds it constant only under the
    // ORDER BY term's collation: the pin's `wherePathSatisfiesOrderBy`
    // compares them; v0.17 review fix: `a = 'y' COLLATE NOCASE ORDER BY
    // a` left `Y` and `y` in rowid order.)
    let constant = |e: &RExpr, o: &SortOrder| {
        ts.iter().any(|t| {
            matches!(t.op, TOp::Eq | TOp::Is)
                && !t.lateral
                && same_expr(&t.key, e)
                && (t.isnull || s.is_rowid(e) || t.coll == o.coll)
        })
    };
    // A GROUP BY or DISTINCT takes its terms in any order: each key
    // column the loop delivers may satisfy any term not yet
    // satisfied (the pin's `wherePathSatisfiesOrderBy` without
    // `bOnce`; v0.16 fix: `GROUP BY a, b` over an index on (b, a)
    // was sorted, the groups in another order than the pin's).
    if q.group {
        let mut done: AVec<bool> = AVec::new();
        for (e, o) in q.order.iter() {
            done.push(constant(strip(e), o));
        }
        for (c, k) in cols.iter() {
            if *k {
                continue;
            }
            let hit = |e: &RExpr, o: &SortOrder| match c {
                Ord::End => false,
                Ord::Rowid => s.is_rowid(e),
                Ord::Key(k, ko) => {
                    ko.coll == o.coll
                        && match k {
                            IdxKey::Col(x) => s.is_col(e, *x),
                            IdxKey::Rowid => s.is_rowid(e),
                            IdxKey::Expr(x) => same_expr(e, x),
                        }
                }
            };
            if matches!(c, Ord::End) {
                return (n, false, None);
            }
            let Some(i) = q.order.iter().enumerate().position(|(i, (e, o))| {
                !done.as_slice().get(i).copied().unwrap_or(true) && hit(strip(e), o)
            }) else {
                break;
            };
            if let Some(d) = done.as_mut_slice().get_mut(i) {
                *d = true;
            }
            // Past the rowid every later term is tie-free.
            if matches!(c, Ord::Rowid) {
                return (n, false, None);
            }
        }
        let sat = done.iter().take_while(|d| **d).count();
        return (sat, false, None);
    }
    let mut next = 0usize;
    let mut rev: Option<bool> = None;
    let mut sat = 0usize;
    let mut bignull = None;
    for (e, o) in q.order.iter() {
        let e0 = strip(e);
        if constant(e0, o) {
            sat += 1;
            continue;
        }
        // NULLs must come where the index puts them — except on the
        // first column after the equality ones, which the pin's
        // WHERE_BIGNULL_SORT loop reads in two passes (v0.16 review
        // fix: hipp sorted instead, ties in another order).
        let big = !q.group && o.nulls_first == o.desc;
        while cols.as_slice().get(next).is_some_and(|(_, k)| *k) {
            next += 1;
        }
        let Some((c, _)) = cols.as_slice().get(next) else {
            break;
        };
        if matches!(c, Ord::End) {
            sat = n;
            break;
        }
        let (hit, kdesc) = match c {
            Ord::End => (false, false),
            Ord::Rowid => (s.is_rowid(e0), false),
            Ord::Key(k, ko) => (
                ko.coll == o.coll
                    && match k {
                        IdxKey::Col(x) => s.is_col(e0, *x),
                        IdxKey::Rowid => s.is_rowid(e0),
                        IdxKey::Expr(x) => same_expr(e0, x),
                    },
                ko.desc,
            ),
        };
        if !hit {
            break;
        }
        let r = if q.group { false } else { o.desc != kdesc };
        match rev {
            None => rev = Some(r),
            Some(p) if p != r => break,
            _ => {}
        }
        if big {
            if next != l.eq.len() {
                break;
            }
            bignull = Some(o.nulls_first);
        }
        sat += 1;
        let was_rowid = matches!(c, Ord::Rowid);
        next += 1;
        // Past the rowid every later term is a tie-free suffix.
        if was_rowid {
            sat = n;
            break;
        }
    }
    (sat.min(n), rev.unwrap_or(false), bignull)
}

/// The pin's `whereLoopOutputAdjust` for one loop.
fn adjust(q: &Query<'_>, s: &Src, ts: &[Term], used: &[usize], n_row: i32, out: i32) -> i32 {
    let mut out = out;
    let mut reduce = 0;
    // In a join (v0.17, `planner`): only the terms the loop's
    // prerequisites and its own table make computable.
    #[cfg(feature = "planner")]
    let allowed = q.jm.map(|j| {
        let mut p = j.pre | j.me;
        for t in used.iter() {
            p |= j.tmask.get(*t).copied().unwrap_or(0);
        }
        p
    });
    for (ci, c) in q.conj.iter().enumerate() {
        #[cfg(feature = "planner")]
        if let (Some(a), Some(j)) = (allowed, q.jm) {
            let m = j.cmask.get(ci).copied().unwrap_or(0);
            if m & j.me == 0 || m & !a != 0 {
                continue;
            }
        } else if !s.reads(c) {
            continue;
        }
        #[cfg(not(feature = "planner"))]
        if !s.reads(c) {
            continue;
        }
        if used
            .iter()
            .any(|t| ts.get(*t).is_some_and(|t| t.parent == ci))
        {
            continue;
        }
        // A likelihood() probability replaces the heuristics.
        #[cfg(feature = "planner")]
        if let Some(tp) = q.truth.get(ci).copied().filter(|t| *t <= 0) {
            out += tp;
            continue;
        }
        out -= 1;
        // In a join (v0.17, `planner`), heuristic 2 applies to any
        // `x = expr` term whose `x` might be indexed (a column, or a
        // constant-propagated one), as the pin's `eOperator` says —
        // usable by an index here or not.
        #[cfg(feature = "planner")]
        if q.jm.is_some() {
            if let RExpr::Binary(BinOp::Eq | BinOp::Is, l, r) = c {
                let ind = |e: &RExpr| {
                    matches!(
                        strip(e),
                        RExpr::Column(..) | RExpr::Rowid(_) | RExpr::AffLiteral(..)
                    )
                };
                let other = if s.reads(l) && s.reads(r) {
                    None
                } else if ind(l) {
                    Some(r)
                } else if ind(r) {
                    Some(l)
                } else {
                    None
                };
                if let Some(v) = other {
                    reduce = reduce.max(match strip(v) {
                        RExpr::Literal(crate::record::OwnedValue::Integer(i))
                            if (-1..=1).contains(i) =>
                        {
                            10
                        }
                        _ => 20,
                    });
                }
            }
            continue;
        }
        if let RExpr::Binary(BinOp::Eq | BinOp::Is, _, r) = c
            && ts
                .iter()
                .any(|t| t.parent == ci && matches!(t.op, TOp::Eq | TOp::Is))
        {
            let k = match strip(r) {
                RExpr::Literal(crate::record::OwnedValue::Integer(i)) if (-1..=1).contains(i) => 10,
                _ => 20,
            };
            reduce = reduce.max(k);
        }
    }
    out.min(n_row - reduce)
}

/// Choose a source's access path (v0.16, `expr_index`): `None` keeps
/// the full table scan in rowid order. `ts` are the source's usable
/// terms (`terms`).
pub fn choose(q: &Query<'_>, s: &Src, tab: &Table<'_>, ts: &[Term]) -> Option<IdxAccess> {
    let mut loops = candidates(q, s, tab, ts);
    for l in loops.as_mut_slice() {
        let (sorted, reverse, bignull) = order_sat(q, s, tab, ts, l, false);
        l.sorted = sorted;
        l.reverse = reverse;
        l.bignull = bignull;
    }
    best_access(q, s, tab, ts, loops.as_slice())
}

/// The pin's `aiRowLogEst` of index `m` in a table of `rows`
/// (LogEst): `sqlite_stat1`'s (v0.17, `planner`), else
/// `sqlite3DefaultRowEst` — the table (less half for a partial
/// index), ten rows per value of the first column, then 9, 8, 7, 6
/// (LogEst 33 32 30 28 26, then 23), one per key of a UNIQUE index;
/// and one row per rowid after the key columns.
pub(crate) fn row_est(m: &IdxModel, rows: i32) -> AVec<i32> {
    let nk = m.keys.len();
    let mut a: AVec<i32> = AVec::new();
    #[cfg(feature = "planner")]
    if let Some(e) = &m.est {
        for k in 0..=nk {
            a.push(e.as_slice().get(k).copied().unwrap_or(0));
        }
        a.push(0);
        return a;
    }
    a.push(if m.pred.is_some() { rows - 10 } else { rows });
    const VALS: [i32; 5] = [33, 32, 30, 28, 26];
    for k in 0..nk {
        a.push(VALS.get(k).copied().unwrap_or(23));
    }
    if m.unique
        && let Some(last) = a.as_mut_slice().last_mut()
    {
        *last = 0;
    }
    // The rowid after the key columns: one row each.
    a.push(0);
    a
}

/// Every candidate loop of one source, in the pin's order
/// (`whereLoopAddBtree`): the full table scan and the rowid loops,
/// then per index its full scan and equality / range loops.
pub(crate) fn candidates(q: &Query<'_>, s: &Src, tab: &Table<'_>, ts: &[Term]) -> AVec<Loop> {
    let mut loops: AVec<Loop> = AVec::new();
    let order_rowid = !q.order.is_empty();
    use crate::query::Hint;
    // The index `INDEXED BY` names (v0.16): the only candidate.
    let forced = match tab.hint {
        Hint::By(r) => Some(r),
        _ => None,
    };
    // The full table scan, and the rowid "index": one row per key
    // (a WITHOUT ROWID table has neither: its clustered index is
    // among the indexes).
    let rows = tab.nrow();
    if !tab.wr && forced.is_none() {
        loops.push(Loop {
            ix: None,
            eq: AVec::new(),
            lo: None,
            hi: None,
            run: rows + 16,
            out: adjust(q, s, ts, &[], rows, rows),
            sorted: 0,
            reverse: false,
            bignull: None,
            #[cfg(feature = "planner")]
            or: None,
            #[cfg(feature = "planner")]
            skip: 0,
        });
        let a_rowid = [rows, 0];
        add_index(q, s, tab, ts, None, &a_rowid, tab.width, true, &mut loops);
    }
    let _ = order_rowid;
    for (i, m) in tab.indexes.iter().enumerate() {
        // `NOT INDEXED`: no index (the pin ignores it on a WITHOUT
        // ROWID table, whose indexes are all it has); `INDEXED BY`:
        // that one.
        if (tab.hint == Hint::Not && !tab.wr) || forced.is_some_and(|r| r != m.root) {
            continue;
        }
        if let Some(p) = &m.pred
            && !implies(q.conj, p)
        {
            continue;
        }
        let a = row_est(m, rows);
        let covering = q.needed.is_some_and(|n| m.covers(n));
        let rsize = a.as_slice()[0];
        // The full index scan.
        let orders = !q.order.is_empty() && m.might_order(s, q.order);
        if orders
            || m.pred.is_some()
            || tab.wr
            || forced.is_some()
            || (q.cover_scan && covering && m.width < tab.width && !unordered(m))
        {
            let mut run = rsize + 1 + (15 * m.width) / tab.width.max(1);
            if !covering {
                #[cfg(feature = "planner")]
                let lookups = rsize + 16 + lookup_cut(q, s, m, tab.wr);
                #[cfg(not(feature = "planner"))]
                let lookups = rsize + 16;
                run = log_est_add(run, lookups);
            }
            loops.push(Loop {
                ix: Some(i),
                eq: AVec::new(),
                lo: None,
                hi: None,
                run,
                out: adjust(q, s, ts, &[], rsize, rsize),
                sorted: 0,
                reverse: false,
                bignull: None,
                #[cfg(feature = "planner")]
                or: None,
                #[cfg(feature = "planner")]
                skip: 0,
            });
        }
        // A WITHOUT ROWID table's UPDATE / DELETE (v0.16): the pin
        // costs a secondary index's lookups as not covering (probed:
        // `UPDATE t SET d = 1 WHERE a > 5` takes the PRIMARY KEY over
        // an index on `a` alone), its full scans as covering the
        // WHERE's columns.
        let covering_lookup = covering && (q.cover_scan || !tab.wr || m.covers_all);
        add_index(
            q,
            s,
            tab,
            ts,
            Some(i),
            a.as_slice(),
            m.width,
            covering_lookup,
            &mut loops,
        );
    }
    if tab.wr {
        loops = insert_loops(q, s, tab, ts, loops);
    }
    loops
}

/// The cheapest of `loops` (their ORDER BY delivery computed) as an
/// access path; `None` for the table's own order.
pub(crate) fn best_access(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    loops: &[Loop],
) -> Option<IdxAccess> {
    // Pass 1: the cheapest loop; its rows estimate the sort.
    let mut best = 0usize;
    let mut bk: Option<(i32, i32)> = None;
    for (i, l) in loops.iter().enumerate() {
        if bk.is_none_or(|b| (l.run, l.out) < b) {
            best = i;
            bk = Some((l.run, l.out));
        }
    }
    if !q.order.is_empty() {
        let n_row = bk.map_or(tab.nrow(), |b| b.1);
        let n_ob = q.order.len();
        let cost = |l: &Loop| {
            if l.sorted >= n_ob {
                l.run
            } else {
                log_est_add(l.run, sort_cost(q, n_row, n_ob, l.sorted)) + 3
            }
        };
        // The pin's `wherePathSolver` order: (cost, rows, unsorted
        // cost), then — probed, among loops that deliver the order —
        // the index with fewer key columns; the earlier candidate
        // keeps a tie.
        let width = |l: &Loop| match l.ix.and_then(|i| tab.indexes.get(i)) {
            Some(m) if l.sorted >= n_ob => m.keys.len(),
            _ => 0,
        };
        let mut bk2: Option<(i32, i32, i32, usize)> = None;
        for (i, l) in loops.iter().enumerate() {
            let k = (cost(l), l.out, l.run, width(l));
            if bk2.is_none_or(|b| k < b) {
                best = i;
                bk2 = Some(k);
            }
        }
    }
    let l = loops.get(best)?.clone();
    // A MULTI-INDEX OR loop (v0.17, `planner`): its branches'
    // paths are chosen by the caller (`xmulti::choose`).
    #[cfg(feature = "planner")]
    if let Some(k) = l.or {
        let mut a = lower(s, tab, ts, &l, false);
        a.or = Some(alloc::boxed::Box::new(crate::xmulti::OrAccess::pending(k)));
        return Some(a);
    }
    let mut reverse = l.reverse && l.sorted > 0;
    // The pin's "ordered inner loop": an ORDER BY the loop does not
    // deliver, but would with its IN columns held constant, still
    // sets the scan direction (each IN value's rows come in order,
    // the IN list walked the same way); the rows are then sorted.
    if l.sorted == 0
        && !q.order.is_empty()
        && !q.group
        && !(l.ix.is_none() && !l.eq.is_empty())
        && l.eq.iter().any(|t| is_in(ts, *t))
    {
        let (sat, rev, _) = order_sat(q, s, tab, ts, &l, true);
        if sat >= q.order.len() {
            reverse = rev;
        }
    }
    // The table's own order: the rowid B-tree, or a WITHOUT ROWID
    // table's clustered index, read forward in full.
    let natural = match l.ix.and_then(|i| tab.indexes.get(i)) {
        None => true,
        Some(m) => m.covers_all,
    };
    // (A table read in its own order that delivers the GROUP BY —
    // in any column order, e.g. `GROUP BY c, id` over the rowid —
    // still says so: the groups are emitted as they come, as the
    // pin's; v0.17 review fix.)
    let groups = q.group_by && l.sorted >= q.order.len();
    // The pin's `WHERE_SORTBYGROUP` check (v0.17 review fix): the
    // GROUP BY loop delivers the ORDER BY it repeats when read in
    // the ORDER BY's directions; `sorted` 0 says it does not.
    let mut sorted = l.sorted;
    if groups && q.obgrp {
        let o = Query {
            group: false,
            group_by: false,
            ..*q
        };
        let (k, rev, _) = order_sat(&o, s, tab, ts, &l, false);
        if k >= q.order.len() {
            reverse = rev;
        } else {
            sorted = 0;
        }
    }
    if natural && l.eq.is_empty() && l.lo.is_none() && l.hi.is_none() && !reverse && !groups {
        return None;
    }
    let mut a = lower(s, tab, ts, &l, reverse);
    a.groups = groups;
    a.sorted = sorted;
    #[cfg(feature = "explain")]
    if !q.dset.is_empty() {
        let d = Query {
            conj: q.conj,
            truth: q.truth,
            order: q.dset,
            group: true,
            group_by: false,
            obgrp: false,
            ncols_out: q.ncols_out,
            needed: q.needed,
            cover_scan: q.cover_scan,
            limit: q.limit,
            distinct: q.distinct,
            dset: &[],
            #[cfg(feature = "planner")]
            jm: None,
        };
        a.eqp.3 = order_sat(&d, s, tab, ts, &l, false).0 >= q.dset.len();
    }
    // The NULL-keyed rows' column: the first after the equality ones
    // (never the rowid, which is not NULL).
    if l.ix.is_some() && l.eq.len() < a.desc.len() {
        a.bignull = l.bignull.map(|f| (l.eq.len(), f));
    }
    Some(a)
}

/// What a table read in its own order delivers (v0.16, `explain`):
/// how many of `q.order` (and whether `q.dset`'s DISTINCT rows come
/// together) — the rowid B-tree, or a WITHOUT ROWID table's clustered
/// index, read forward in full, the loop `choose` leaves as no access
/// path.
#[cfg(feature = "explain")]
pub fn natural_sat(q: &Query<'_>, s: &Src, tab: &Table<'_>, ts: &[Term]) -> (usize, bool) {
    let l = Loop {
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
        sorted: 0,
        reverse: false,
        bignull: None,
        #[cfg(feature = "planner")]
        or: None,
        #[cfg(feature = "planner")]
        skip: 0,
    };
    let (sat, rev, _) = order_sat(q, s, tab, ts, &l, false);
    let d = Query {
        conj: q.conj,
        truth: q.truth,
        order: q.dset,
        group: true,
        group_by: false,
        obgrp: false,
        ncols_out: q.ncols_out,
        needed: q.needed,
        cover_scan: q.cover_scan,
        limit: q.limit,
        distinct: q.distinct,
        dset: &[],
        #[cfg(feature = "planner")]
        jm: None,
    };
    let dis = !q.dset.is_empty() && order_sat(&d, s, tab, ts, &l, false).0 >= q.dset.len();
    (if rev { 0 } else { sat }, dis)
}

/// The pin's `whereLoopInsert` over a WITHOUT ROWID table's loops,
/// in the order they were made (v0.16): loops of one "sort index"
/// class (the index's position when it might deliver the ORDER BY,
/// else 0 — the pin's `iSortIdx`) compete; a loop no cheaper (`rRun`)
/// and no smaller (`nOut`) than an earlier one of its class, with no
/// fewer prerequisites, is dropped, and one at least as good as an
/// earlier one takes that loop's place in the list (dropping the
/// later ones it also beats). The list order then breaks the path
/// solver's ties. (Rowid tables keep the plain candidate order the
/// expression-index unit validated.)
fn insert_loops(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    loops: AVec<Loop>,
) -> AVec<Loop> {
    let class = |l: &Loop| -> usize {
        match l.ix.and_then(|i| tab.indexes.get(i).map(|m| (i, m))) {
            Some((i, m)) if !q.order.is_empty() && m.might_order(s, q.order) => i + 1,
            _ => 0,
        }
    };
    let lateral = |l: &Loop| {
        l.eq.iter()
            .chain(l.lo.iter())
            .chain(l.hi.iter())
            .any(|t| ts.get(*t).is_some_and(|t| t.lateral))
    };
    // `a`'s prerequisites are a subset of `b`'s.
    let subset = |a: &Loop, b: &Loop| !lateral(a) || lateral(b);
    let mut out: AVec<Loop> = AVec::new();
    for t in loops.iter() {
        let ct = class(t);
        let mut discard = false;
        let mut at: Option<usize> = None;
        for (i, p) in out.iter().enumerate() {
            if class(p) != ct {
                continue;
            }
            if subset(p, t) && p.run <= t.run && p.out <= t.out {
                discard = true;
                break;
            }
            if subset(t, p) && p.run >= t.run && p.out >= t.out {
                at = Some(i);
                break;
            }
        }
        if discard {
            continue;
        }
        match at {
            None => out.push(t.clone()),
            Some(i) => {
                // Later loops the template also beats go too.
                let mut kept: AVec<Loop> = AVec::new();
                for (j, p) in out.iter().enumerate() {
                    if j == i {
                        kept.push(t.clone());
                    } else if j > i
                        && class(p) == ct
                        && subset(t, p)
                        && p.run >= t.run
                        && p.out >= t.out
                    {
                        continue;
                    } else {
                        kept.push(p.clone());
                    }
                }
                out = kept;
            }
        }
    }
    out
}

/// The equality / range loops of one index (the rowid B-tree when
/// `ix` is `None`), in the pin's order (`whereLoopAddBtreeIndex`).
#[allow(clippy::too_many_arguments)]
fn add_index(
    q: &Query<'_>,
    s: &Src,
    tab: &Table<'_>,
    ts: &[Term],
    ix: Option<usize>,
    a: &[i32],
    width: i32,
    covering: bool,
    loops: &mut AVec<Loop>,
) {
    let rsize = a.first().copied().unwrap_or(ROWS);
    let rlog = est_log(rsize);
    let ipk = ix.is_none();
    #[cfg(feature = "planner")]
    let from = loops.len();
    let ncol = match ix.and_then(|i| tab.indexes.get(i)) {
        Some(m) => m.keys.len() + 1,
        None => 1,
    };
    let matches = |k: usize, t: &Term| -> bool {
        if t.isnull
            && let Some(Some(IdxKey::Col(c))) = ix
                .and_then(|i| tab.indexes.get(i))
                .map(|m| m.keys.as_slice().get(k))
            && tab.notnull.get(*c).copied().unwrap_or(false)
        {
            return false;
        }
        match ix.and_then(|i| tab.indexes.get(i)) {
            // (A rowid is never NULL: the pin offers no `IS NULL`
            // term to it.)
            None => k == 0 && !t.isnull && s.is_rowid(&t.key),
            Some(m) => {
                if k == m.keys.len() {
                    !t.isnull && s.is_rowid(&t.key)
                } else {
                    m.key_matches(k, s, t)
                }
            }
        }
    };
    let cost = |out: i32| -> i32 {
        let idx = out + 1 + (15 * width) / tab.width.max(1);
        let mut run = log_est_add(rlog, idx);
        if !ipk && !covering {
            run = log_est_add(run, out + 16);
        }
        run
    };
    // With the `planner` leaf every loop, a join's or one table's,
    // passes through the pin's `whereLoopInsert` (`xjoin`), so a
    // deeper loop also pays the IN iterations of the columns before
    // it (`rRun += nInMul`); without it the IN is charged at its own
    // level only, which is what `whereLoopAdjustCost` makes of it for
    // one table.
    #[cfg(feature = "planner")]
    let join_in = |m: i32| m;
    #[cfg(not(feature = "planner"))]
    let join_in = |_: i32| 0;
    // An explicit stack instead of recursion: (equality terms,
    // nInMul, nOut so far).
    let mut work: AVec<(AVec<usize>, i32, i32)> = AVec::new();
    work.push((AVec::new(), 0, rsize));
    while let Some((eq, in_mul, saved)) = work.pop() {
        let k = eq.len();
        if k >= ncol {
            continue;
        }
        let mut deeper: AVec<(AVec<usize>, i32, i32)> = AVec::new();
        for (ti, t) in ts.iter().enumerate() {
            if !matches(k, t) {
                continue;
            }
            // (An `unordered` index takes no range: v0.17 review fix.)
            if !t.is_eq() && ix.and_then(|i| tab.indexes.get(i)).is_some_and(unordered) {
                continue;
            }
            let used_with = |extra: &[usize]| {
                let mut u = eq.clone();
                u.extend_from_slice(extra);
                u
            };
            if t.is_eq() {
                let n_in = match &t.op {
                    TOp::In(items) => log_est(items.len() as u64),
                    // The pin's tuning: a subquery returns 25 rows.
                    TOp::InSub(_) => 46,
                    _ => 0,
                };
                // With `sqlite_stat1` rows (v0.17, `planner`): an IN
                // whose K values cost more than scanning the M rows the
                // columns before it match (`M*log(K) < K*log(N)`, with a
                // margin of 10 for the IN) is not probed, unless it can
                // still seek-scan (the first IN of the loop).
                #[cfg(feature = "planner")]
                if matches!(t.op, TOp::In(_) | TOp::InSub(_))
                    && ix.and_then(|i| tab.indexes.get(i)).is_some_and(|m| m.stat1)
                    && rlog >= 10
                {
                    let m_rows = a.get(k).copied().unwrap_or(rsize);
                    let x = m_rows + est_log(n_in) + 10 - (n_in + rlog);
                    if x < 0 && in_mul >= 2 {
                        continue;
                    }
                }
                let cur = a.get(k).copied().unwrap_or(rsize);
                let nxt = a.get(k + 1).copied().unwrap_or(0);
                // A likelihood() probability on a table column's
                // term replaces the index's estimate.
                let col_key = match ix.and_then(|i| tab.indexes.get(i)) {
                    Some(m) => matches!(
                        m.keys.as_slice().get(k),
                        Some(IdxKey::Col(c)) if s.rowid_alias != Some(*c)
                    ),
                    None => false,
                };
                let tp = truth_of(t);
                let mut out = if tp <= 0 && col_key {
                    saved + tp - n_in
                } else {
                    saved + (nxt - cur)
                };
                if t.isnull && !(tp <= 0 && col_key) {
                    out += 10;
                }
                // The pin charges an IN's iterations at its own level only
                // (probed: `b IN (16 values) AND c = ?` still prefers an
                // index on (b, c) to one on c).
                let run = cost(out) + n_in + join_in(in_mul);
                let used = used_with(&[ti]);
                let o2 = adjust(q, s, ts, used.as_slice(), rsize, out + in_mul + n_in);
                loops.push(Loop {
                    ix,
                    eq: used.clone(),
                    lo: None,
                    hi: None,
                    run,
                    out: o2,
                    sorted: 0,
                    reverse: false,
                    bignull: None,
                    #[cfg(feature = "planner")]
                    or: None,
                    #[cfg(feature = "planner")]
                    skip: 0,
                });
                deeper.push((used, in_mul + n_in, out));
            } else if t.lower() {
                let lo_out = range_est(saved, Some(t), None);
                let used = used_with(&[ti]);
                loops.push(Loop {
                    ix,
                    eq: eq.clone(),
                    lo: Some(ti),
                    hi: None,
                    run: cost(lo_out) + join_in(in_mul),
                    out: adjust(q, s, ts, used.as_slice(), rsize, lo_out + in_mul),
                    sorted: 0,
                    reverse: false,
                    bignull: None,
                    #[cfg(feature = "planner")]
                    or: None,
                    #[cfg(feature = "planner")]
                    skip: 0,
                });
                // ... and with each upper bound on the same column.
                for (ui, u) in ts.iter().enumerate() {
                    if !u.upper() || !matches(k, u) {
                        continue;
                    }
                    let both = range_est(saved, Some(t), Some(u));
                    let used = used_with(&[ti, ui]);
                    loops.push(Loop {
                        ix,
                        eq: eq.clone(),
                        lo: Some(ti),
                        hi: Some(ui),
                        run: cost(both) + join_in(in_mul),
                        out: adjust(q, s, ts, used.as_slice(), rsize, both + in_mul),
                        sorted: 0,
                        reverse: false,
                        bignull: None,
                        #[cfg(feature = "planner")]
                        or: None,
                        #[cfg(feature = "planner")]
                        skip: 0,
                    });
                }
            } else if t.upper() {
                let hi_out = range_est(saved, None, Some(t));
                let used = used_with(&[ti]);
                loops.push(Loop {
                    ix,
                    eq: eq.clone(),
                    lo: None,
                    hi: Some(ti),
                    run: cost(hi_out) + join_in(in_mul),
                    out: adjust(q, s, ts, used.as_slice(), rsize, hi_out + in_mul),
                    sorted: 0,
                    reverse: false,
                    bignull: None,
                    #[cfg(feature = "planner")]
                    or: None,
                    #[cfg(feature = "planner")]
                    skip: 0,
                });
            }
        }
        // A skip-scan (v0.17, `planner`; the end of the pin's
        // `whereLoopAddBtreeIndex`): no term for the leading columns
        // so far, and `sqlite_stat1` says each value of the next one
        // repeats at least 18 times (LogEst 42): step over its values,
        // `nIter` more lookups.
        #[cfg(feature = "planner")]
        if let Some(m) = ix.and_then(|i| tab.indexes.get(i))
            && m.stat1
            && !m.noskip
            && eq.iter().all(|t| *t == usize::MAX)
            && k + 1 < m.keys.len()
            && a.get(k + 1).copied().unwrap_or(0) >= 42
        {
            let n_iter = a.get(k).copied().unwrap_or(rsize) - a.get(k + 1).copied().unwrap_or(0);
            let mut e2 = eq.clone();
            e2.push(usize::MAX);
            deeper.push((e2, in_mul + n_iter + 5, saved - n_iter));
        }
        // Recurse in term order: the stack pops the last pushed
        // first, so push in reverse.
        while let Some(d) = deeper.pop() {
            work.push(d);
        }
    }
    // A skip-scan's loops: how many leading columns they step over.
    #[cfg(feature = "planner")]
    for l in loops.as_mut_slice().get_mut(from..).unwrap_or(&mut []) {
        l.skip = l.eq.iter().take_while(|t| **t == usize::MAX).count();
    }
}

/// The pin's `whereRangeScanEst` without statistics (`lo` / `hi`:
/// whether that bound exists; a `TERM_VNULL` bound narrows nothing).
/// A term's truth probability (`Term::truth`; 1, none, below
/// `planner`).
fn truth_of(t: &Term) -> i32 {
    #[cfg(feature = "planner")]
    return t.truth;
    #[cfg(not(feature = "planner"))]
    {
        let _ = t;
        1
    }
}

fn range_est(out: i32, lo: Option<&Term>, hi: Option<&Term>) -> i32 {
    // `whereRangeAdjust`: a likelihood() probability, else 20 per
    // bound (none for a `TERM_VNULL` one).
    let adj = |n: i32, t: Option<&Term>| match t {
        Some(t) if truth_of(t) <= 0 => n + truth_of(t),
        Some(t) if !t.vnull => n - 20,
        _ => n,
    };
    let mut n = adj(adj(out, lo), hi);
    if lo.is_some_and(|t| truth_of(t) > 0) && hi.is_some_and(|t| truth_of(t) > 0) {
        n -= 20;
    }
    let o = out - i32::from(lo.is_some()) - i32::from(hi.is_some());
    let n = n.max(10);
    n.min(o)
}

/// The pin's `nLookup` reduction for a non-covering full index scan
/// (v0.17, `planner`; `whereLoopAddBtree`): each WHERE clause term,
/// in term-list order up to the first the index does not cover
/// (`sqlite3ExprCoveredByIndex`: every column of this source it reads
/// is in the index), saves one lookup, twenty for an `=` / `IS`; the
/// commuted, BETWEEN, `IS NOT NULL` and OR-to-IN virtual terms follow
/// the originals in the list.
#[cfg(feature = "planner")]
fn lookup_cut(q: &Query<'_>, s: &Src, m: &IdxModel, wr: bool) -> i32 {
    use crate::query::{RefRange, expr_refs_range};
    let held = |c: usize| {
        m.keys
            .iter()
            .chain(m.suffix.iter().map(|(k, _)| k))
            .any(|k| matches!(k, IdxKey::Col(x) if *x == c))
            || (!wr && s.rowid_alias == Some(c))
    };
    let covered = |e: &RExpr| {
        (0..s.ncols).all(|c| {
            held(c)
                || !expr_refs_range(
                    e,
                    &RefRange {
                        w: (s.base + c, s.base + c + 1),
                        s: (0, 0),
                    },
                )
        })
    };
    let col = |e: &RExpr| matches!(strip(e), RExpr::Column(..) | RExpr::Rowid(_));
    let mut cut = 0;
    let mut virt = 0;
    for c in q.conj.iter() {
        if !covered(c) {
            return cut;
        }
        let c = strip(c);
        match c {
            RExpr::Binary(BinOp::Eq | BinOp::Is, l, r) if col(l) || col(r) => {
                cut -= 20;
                if col(l) && col(r) {
                    virt -= 20;
                }
            }
            RExpr::Binary(BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, l, r) => {
                cut -= 1;
                if col(l) && col(r) {
                    virt -= 1;
                }
            }
            RExpr::Between(x, _, _, false) => {
                cut -= 1;
                if col(x) {
                    virt -= 2;
                }
            }
            RExpr::IsNull(x, true) => {
                cut -= 1;
                if matches!(strip(x), RExpr::Column(..)) {
                    virt -= 1;
                }
            }
            _ => cut -= 1,
        }
    }
    cut + virt
}

/// The leading index columns loop `l` skip-scans (0 below
/// `planner`).
pub(crate) fn skip_of(l: &Loop) -> usize {
    #[cfg(feature = "planner")]
    return l.skip;
    #[cfg(not(feature = "planner"))]
    {
        let _ = l;
        0
    }
}

/// A chosen loop as the executor's access path.
pub(crate) fn lower(s: &Src, tab: &Table<'_>, ts: &[Term], l: &Loop, reverse: bool) -> IdxAccess {
    let _ = s;
    let (root, desc, nkeys) = match l.ix.and_then(|i| tab.indexes.get(i)) {
        Some(m) => (m.root, m.desc.clone(), m.keys.len()),
        None => (0, AVec::new(), 0),
    };
    let mut lateral = false;
    let mut eq = AVec::new();
    for t in l.eq.iter().filter_map(|i| ts.get(*i)) {
        lateral |= t.lateral;
        let (vals, is_in, sub) = match &t.op {
            TOp::In(items) => (items.clone(), true, None),
            TOp::InSub(e) => (AVec::new(), true, Some(e.clone())),
            _ => {
                let mut v = AVec::new();
                v.push(t.val.clone());
                (v, false, None)
            }
        };
        eq.push(Probe {
            vals,
            is_in,
            sub,
            is: matches!(t.op, TOp::Is),
            aff: t.aff,
        });
    }
    let bound = |i: Option<usize>, incl_op: fn(&TOp) -> bool| -> Option<Bound> {
        let t = ts.get(i?)?;
        if t.vnull {
            return None;
        }
        Some(Bound {
            e: t.val.clone(),
            incl: incl_op(&t.op),
            aff: t.aff,
        })
    };
    let lo = bound(l.lo, |o| matches!(o, TOp::Ge));
    let hi = bound(l.hi, |o| matches!(o, TOp::Le));
    for i in [l.lo, l.hi].into_iter().flatten() {
        if let Some(t) = ts.get(i) {
            lateral |= t.lateral;
        }
    }
    IdxAccess {
        root,
        desc,
        range_rowid: !tab.wr && root != 0 && eq.len() + skip_of(l) == nkeys,
        eq,
        lo,
        hi,
        reverse,
        lateral,
        one_pass: false,
        groups: false,
        cover: None,
        bignull: None,
        sorted: l.sorted,
        #[cfg(feature = "planner")]
        auto: None,
        #[cfg(feature = "planner")]
        bloom: None,
        #[cfg(feature = "planner")]
        or: None,
        #[cfg(feature = "planner")]
        skip: l.skip,
        #[cfg(feature = "planner")]
        first: None,
        #[cfg(feature = "explain")]
        eqp: (
            l.sorted,
            false,
            l.lo.and_then(|i| ts.get(i)).is_some_and(|t| t.vnull),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_est_matches_pin() {
        assert_eq!(log_est(0), 0);
        assert_eq!(log_est(1), 0);
        assert_eq!(log_est(2), 10);
        assert_eq!(log_est(8), 30);
        assert_eq!(log_est(10), 33);
        assert_eq!(log_est(25), 46);
        assert_eq!(log_est(1000), 99);
        assert_eq!(log_est(1048576), 200);
        assert_eq!(est_log(200), 43);
        assert_eq!(log_est_add(43, 16), 45);
    }

    #[test]
    fn sz_est_matches_pin() {
        assert_eq!(sz_est(b"", Affinity::Blob), 1);
        assert_eq!(sz_est(b"INTEGER", Affinity::Integer), 1);
        assert_eq!(sz_est(b"TEXT", Affinity::Text), 5);
        assert_eq!(sz_est(b"BLOB", Affinity::Blob), 5);
        assert_eq!(sz_est(b"VARCHAR(10)", Affinity::Text), 3);
        assert_eq!(sz_est(b"varchar(100)", Affinity::Text), 26);
    }
}
