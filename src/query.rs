//! Resolved expressions and their evaluation (v0.4, SPEC §2).
//!
//! The parser produces `sql::Expr` trees over names; the planner
//! resolves them against the FROM sources into `RExpr` trees over
//! flat row slots, validating function names and column references
//! once, at prepare time. Evaluation then runs over materialised
//! rows (`RowData`) with SQLite's type-coercion rules:
//!
//! - arithmetic converts text operands by *prefix* numeric parse
//!   (`'12abc' + 0` is 12, `'abc' + 0` is 0); integer `/` truncates
//!   and division/modulo by zero yield NULL, never an error;
//! - comparisons follow `record::compare_values`, with SQLite's
//!   comparison-affinity adjustment (§4.2 of the datatype doc):
//!   each operand carries an affinity class derived from its
//!   expression shape — a column reference or CAST carries its
//!   column/cast affinity, a rowid reference is NUMERIC-class,
//!   anything else has no affinity. If exactly one side is
//!   NUMERIC-class, NUMERIC affinity is applied to the other
//!   side's value (whatever its class); else if one side is
//!   TEXT-class and the other has no affinity, TEXT affinity is
//!   applied to the no-affinity side; otherwise neither value is
//!   converted. (v0.8: the pre-v0.8 evaluator only adjusted when
//!   exactly one operand was a column reference, so a NUMERIC
//!   column compared against a TEXT column silently compared
//!   raw storage classes — a wrong answer, probed against the
//!   pinned oracle and fixed here. A BLOB-affinity column is
//!   its own class, *not* "no affinity": TEXT vs BLOB converts
//!   nothing, also oracle-probed.)
//! - `=`/`!=`/`<`/... with a NULL operand yield NULL; `IS`/`IS NOT`
//!   are the NULL-safe forms; AND/OR/NOT are three-valued.
//!
//! Aggregates are `RExpr::Agg` nodes, evaluated only in a group
//! context; bare (non-aggregate) subexpressions in an aggregate
//! query read the group's representative row (SQLite's rule).

use alloc::boxed::Box;

use crate::arena::{AVec, Arena};
use crate::error::{Error, Result, SchemaKind};
use crate::record::{
    Affinity, Coll, OwnedValue, apply_affinity, compare_coll, compare_values, render_int,
    render_real,
};
use crate::sql::{BinOp, Expr, Literal, SortOrder, eq_ci, handle_bytes};

/// One materialised row: flat column values across all FROM
/// sources, plus each source's rowid (None for a LEFT JOIN pad).
#[derive(Debug, Clone)]
pub struct RowData {
    pub vals: AVec<OwnedValue>,
    pub rowids: AVec<Option<i64>>,
}

impl RowData {
    pub fn empty() -> RowData {
        RowData {
            vals: AVec::new(),
            rowids: AVec::new(),
        }
    }
}

/// Aggregate function kinds (v0.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Avg,
    Total,
    Min,
    Max,
    GroupConcat,
}

/// Scalar function kinds (v0.4). `MinN`/`MaxN` are the multi-arg
/// scalar forms of min/max.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFn {
    Abs,
    Length,
    Upper,
    Lower,
    Coalesce,
    Nullif,
    Typeof,
    Substr,
    Round,
    Hex,
    Trim,
    Ltrim,
    Rtrim,
    Replace,
    Instr,
    Sign,
    Iif,
    MinN,
    MaxN,
    /// `like(pattern, subject[, escape])` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Like,
    /// `glob(pattern, subject)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Glob,
}

/// A resolved expression: column references are flat slot indices
/// into `RowData::vals`; `Rowid(src)` reads `RowData::rowids[src]`.
#[derive(Debug, Clone)]
pub enum RExpr {
    Literal(OwnedValue),
    /// A computed subquery result value carrying the result
    /// column's affinity (v0.8): evaluation yields the value
    /// unchanged, but comparisons treat it as an operand of
    /// this affinity class — a scalar / IN subquery whose
    /// single result item is a direct column reference (or
    /// CAST, or rowid) compares with that column's affinity
    /// in SQLite, which the plain `Literal` substitution of
    /// v0.6 silently dropped (oracle-probed: `'002' = (SELECT
    /// int_col ...)` is true there). Produced only by
    /// `substitute`; never by the resolver.
    AffLiteral(OwnedValue, Affinity),
    /// A column slot with its comparison affinity and its
    /// collating sequence (v0.16: the column's `COLLATE`; a
    /// derived table's column carries its subquery item's;
    /// `None` for a register reference — an upsert's
    /// `excluded.<col>` — which has none, so the other
    /// operand's collation decides a comparison).
    Column(usize, Affinity, Option<Coll>),
    Rowid(usize),
    Binary(BinOp, Box<RExpr>, Box<RExpr>),
    Neg(Box<RExpr>),
    /// Unary plus (v0.16): the operand's value, with no
    /// comparison affinity (the operand's collation passes).
    Pos(Box<RExpr>),
    Not(Box<RExpr>),
    IsNull(Box<RExpr>, bool),
    /// `x [NOT] IN (list)`: (operand, items, negated, from_sub).
    /// `from_sub` marks the literal list an `InSub` substitutes
    /// (v0.16): its items compare by the binary rule (the pin's
    /// comparison affinity for `IN (SELECT ...)`), while a
    /// written list converts by the left operand's affinity
    /// alone.
    InList(Box<RExpr>, AVec<RExpr>, bool, bool),
    /// A scalar subquery (v0.6): never evaluated directly — the
    /// executor computes it per outer row and substitutes the
    /// result as a `Literal` before `eval` runs (see
    /// `substitute`). Evaluating one is `Error::Unsupported`.
    SubScalar(Box<SubPlan>),
    /// An EXISTS subquery (v0.6): substituted like `SubScalar`.
    SubExists(Box<SubPlan>),
    /// `x [NOT] IN (SELECT ...)` (v0.6): substituted to an
    /// `InList` of literals before evaluation.
    InSub(Box<RExpr>, Box<SubPlan>, bool),
    Between(Box<RExpr>, Box<RExpr>, Box<RExpr>, bool),
    Like(Box<RExpr>, Box<RExpr>, bool),
    Case {
        base: Option<Box<RExpr>>,
        whens: AVec<(RExpr, RExpr)>,
        otherwise: Option<Box<RExpr>>,
    },
    Scalar(ScalarFn, AVec<RExpr>),
    Cast(Box<RExpr>, Affinity),
    /// `expr COLLATE name` (v0.16): evaluates to the operand;
    /// comparisons it takes part in use this collation (the
    /// pin's `EP_Collate`: an explicit collation anywhere in an
    /// operand wins over column collations). Also produced by
    /// `substitute` to carry an IN subquery's comparison
    /// collation onto its left operand.
    Collate(Box<RExpr>, Coll),
    Agg {
        kind: AggKind,
        distinct: bool,
        star: bool,
        args: AVec<RExpr>,
    },
    /// ORDER BY reference to an output column (0-based).
    OutRef(usize),
}

/// One FROM source for resolution: qualifier (alias if given, else
/// the table name), column names + affinities in table order, and
/// the rowid-alias column index if any.
#[derive(Debug, Clone)]
pub struct SourceDesc {
    pub qualifier: AVec<u8>,
    pub columns: AVec<(AVec<u8>, Affinity)>,
    pub rowid_alias: Option<usize>,
    /// Whether an unqualified / qualified `rowid` reference may
    /// resolve to this source's rowid (v0.9): true for real
    /// tables, false for derived tables — the pin answers
    /// "no such column: rowid" for `SELECT rowid FROM
    /// (SELECT ...) s` (a derived column literally NAMED rowid
    /// still resolves, as an ordinary column).
    pub has_rowid: bool,
    /// How this source joins the sources on its left (v0.16):
    /// unqualified references to a USING column pick a side by
    /// it (see `resolve_column_in_group`).
    pub kind: crate::sql::JoinKind,
    /// This source's USING column names (v0.16; NATURAL joins
    /// fill it with the common columns): empty without one.
    pub using: AVec<AVec<u8>>,
    /// Per-column collating sequences (v0.16), parallel to
    /// `columns`; a missing entry (the default: empty) is
    /// BINARY.
    pub colls: AVec<Coll>,
    /// The source's columns are register references (v0.16:
    /// an upsert's `excluded` row): no affinity and no
    /// collation in comparisons, as on the pin.
    pub registers: bool,
}

impl SourceDesc {
    /// A source joined as an inner join with no USING columns
    /// (the planner fills `kind` / `using` for joined sources).
    pub fn new(
        qualifier: AVec<u8>,
        columns: AVec<(AVec<u8>, Affinity)>,
        rowid_alias: Option<usize>,
        has_rowid: bool,
    ) -> SourceDesc {
        SourceDesc {
            qualifier,
            columns,
            rowid_alias,
            has_rowid,
            kind: crate::sql::JoinKind::Inner,
            using: AVec::new(),
            colls: AVec::new(),
            registers: false,
        }
    }

    /// Whether `name` is one of this source's USING columns.
    pub fn uses(&self, name: &[u8]) -> bool {
        self.using.iter().any(|u| eq_ci(u.as_slice(), name))
    }

    /// The resolved reference to this source's column `ci`
    /// (source `si` of `group`): the rowid for the rowid-alias
    /// column, else the flat slot.
    pub fn column_ref(&self, group: &ScopeGroup, si: usize, ci: usize) -> RExpr {
        let r = if Some(ci) == self.rowid_alias {
            RExpr::Rowid(group.first_src + si)
        } else {
            let aff = self
                .columns
                .as_slice()
                .get(ci)
                .map_or(Affinity::Blob, |c| c.1);
            RExpr::Column(
                group.offsets.as_slice().get(si).copied().unwrap_or(0) + ci,
                aff,
                if self.registers {
                    None
                } else {
                    Some(self.colls.as_slice().get(ci).copied().unwrap_or_default())
                },
            )
        };
        // A register reads as `+value`: no comparison affinity.
        if self.registers {
            RExpr::Pos(Box::new(r))
        } else {
            r
        }
    }

    /// The index of this source's column named `name`, if any.
    pub fn column_index(&self, name: &[u8]) -> Option<usize> {
        self.columns
            .iter()
            .position(|(c, _)| eq_ci(c.as_slice(), name))
    }
}

/// One level of name-resolution scope (v0.6): a query's own FROM
/// sources, with absolute slot offsets into the (possibly
/// combined outer ++ inner) evaluation row and the absolute
/// index of its first source in that row's `rowids`. Subquery
/// planning stacks these: the subquery's own group is resolved
/// first, then enclosing groups nearest-first — SQLite's
/// shadowing rule.
#[derive(Debug, Clone)]
pub struct ScopeGroup {
    pub sources: AVec<SourceDesc>,
    pub offsets: AVec<usize>,
    pub first_src: usize,
    /// Result-column aliases visible in this scope (v0.16,
    /// `v16`): `(alias, resolved item)`. Filled after the
    /// result list is resolved, so WHERE / ON / GROUP BY /
    /// HAVING / ORDER BY expressions — and subqueries inside
    /// them — may name an alias; an unqualified name that no
    /// source of this group has resolves to the first alias
    /// spelled the same, before any enclosing scope (the pin's
    /// `lookupName` order). Empty while the result list itself
    /// resolves: there an alias is "no such column".
    pub aliases: AVec<(AVec<u8>, RExpr)>,
}

impl ScopeGroup {
    /// A scope group with no aliases.
    pub fn new(sources: AVec<SourceDesc>, offsets: AVec<usize>, first_src: usize) -> ScopeGroup {
        ScopeGroup {
            sources,
            offsets,
            first_src,
            aliases: AVec::new(),
        }
    }

    /// One past the last absolute column slot / rowid source
    /// this group occupies (v0.16). A subquery planned inside
    /// these scopes numbers its own slots from the largest end
    /// over them — the same as summing widths when the groups
    /// tile the row from slot 0, and still right for a group
    /// that starts later (an UPDATE ... FROM clause's own scope
    /// sits after the target's columns).
    pub fn end(&self) -> (usize, usize) {
        let slots = match (
            self.offsets.as_slice().last(),
            self.sources.as_slice().last(),
        ) {
            (Some(o), Some(s)) => o + s.columns.len(),
            _ => 0,
        };
        (slots, self.first_src + self.sources.len())
    }

    /// The `end` over a stack of scope groups (v0.16).
    pub fn stack_end(groups: &[ScopeGroup]) -> (usize, usize) {
        groups.iter().fold((0, 0), |(w, s), g| {
            let (gw, gs) = g.end();
            (w.max(gw), s.max(gs))
        })
    }

    /// Total flat column slots across this group's sources.
    pub fn width(&self) -> usize {
        self.sources.iter().map(|s| s.columns.len()).sum()
    }
}

/// One FROM source of a planned subquery (v0.6): the executor's
/// view (root page + read rules), mirroring the session's query
/// `SourcePlan` but self-contained so a `SubPlan` can nest.
#[derive(Debug, Clone)]
pub struct SubSource {
    pub root: u32,
    pub kind: crate::sql::JoinKind,
    pub on: Option<RExpr>,
    pub rowid_alias: Option<usize>,
    pub ncols: usize,
    pub affs: AVec<Affinity>,
    pub defaults: AVec<OwnedValue>,
    /// A derived table (v0.9): when `Some`, the source's rows
    /// are this subplan's result (executed against the outer
    /// row at materialisation time; `root` is unused). The
    /// subplan was planned against the enclosing scopes only —
    /// SQLite has no LATERAL, so a FROM subquery cannot see its
    /// sibling sources (pin-verified: `FROM t, (SELECT t.a) s`
    /// is "no such column").
    pub derived: Option<Box<SubPlan>>,
    /// The table's generated columns (v0.16): VIRTUAL ones are
    /// absent from the record and computed as each row is read.
    pub gcols: Option<Box<crate::rules::GenCols>>,
}

/// A planned subquery (v0.6): a full SELECT planned against the
/// combined scope (enclosing sources ++ its own). Its
/// expressions index the combined evaluation row, so executing
/// it for one outer row is: scan its own sources, join them onto
/// the outer row, project. Nested subqueries nest as further
/// `RExpr` subquery nodes inside these expressions.
#[derive(Debug, Clone)]
pub struct SubPlan {
    pub sources: AVec<SubSource>,
    pub items: AVec<RExpr>,
    pub filter: Option<RExpr>,
    pub group_by: AVec<RExpr>,
    pub having: Option<RExpr>,
    pub order_by: AVec<(RExpr, SortOrder)>,
    pub distinct: bool,
    /// LIMIT / OFFSET (v0.16: resolved expressions — planned
    /// against an empty scope, so never correlated — evaluated
    /// once per execution with the pin's `MustBeInt` rule; see
    /// `limit_int`).
    pub limit: Option<RExpr>,
    pub offset: Option<RExpr>,
    pub has_agg: bool,
    /// Compound tail (v0.9): `(op, core plan)` pairs applied
    /// left to right after this plan's own core. Each tail plan
    /// is a single core (its own `compound` empty). When this
    /// is non-empty, `order_by` is empty and `comp_order`
    /// carries the compound's ORDER BY instead; `limit` /
    /// `offset` apply to the combined result.
    pub compound: AVec<(crate::sql::CompoundOp, Box<SubPlan>)>,
    /// A VALUES core (v0.9): resolved rows to produce in place
    /// of a source scan (sources/items are empty). Each row was
    /// resolved against the combined (enclosing ++ own) scope,
    /// so evaluating a row for an outer row yields its values.
    pub values: AVec<AVec<RExpr>>,
    /// Compound ORDER BY (v0.9): `(output column, descending)`
    /// pairs, resolved at plan time from the statement's ORDER
    /// BY terms — an output ordinal or a column name some
    /// constituent core matches (the session's `CompKeys`).
    pub comp_order: AVec<(usize, SortOrder)>,
    /// Result column names (v0.9), parallel to the core's
    /// output width: alias if written, else the column name
    /// for a direct column reference, else the expression's
    /// source text; a VALUES core's are `column1`, `column2`,
    /// ... (all pin-verified). Consumed by derived-table
    /// planning.
    pub result_names: AVec<AVec<u8>>,
    /// Result column comparison affinities (v0.9), parallel to
    /// `result_names`: `Some` when the result item is a direct
    /// column reference, CAST, or rowid (the affinity it
    /// carries into an enclosing comparison / a derived
    /// table's column), else `None`.
    pub result_affs: AVec<Option<Affinity>>,
    /// Whether each result item was written with an `AS` alias
    /// (v0.16), parallel to `result_names`: CREATE TABLE ... AS
    /// SELECT names an unaliased column reference by the
    /// table's own column name, not its spelling here.
    pub result_named: AVec<bool>,
    /// Plan-node identity (v0.13): unique across every
    /// `SubPlan` planned in this process, assigned by the
    /// session planner. Clones keep the id, which is what
    /// makes a plan node recognisable across the executor's
    /// per-execution plan clones. `0` is never assigned.
    pub id: usize,
    /// Uncorrelated at plan time (v0.13): no expression in
    /// this plan — its own cores, nested subqueries, derived
    /// sources, VALUES rows, compound tails — references a
    /// column slot or rowid source belonging to any enclosing
    /// scope (see `plan_is_uncorrelated`). The executor may
    /// execute such a plan once per statement and cache the
    /// result; the pinned oracle fixes an uncorrelated
    /// subquery's value for the whole statement (probed:
    /// `UPDATE t SET a = (SELECT sum(a) FROM t)` yields the
    /// pre-statement sum on every row).
    pub uncorrelated: bool,
}

impl SubPlan {
    /// The plan's output width: a VALUES core's row length,
    /// else the item count.
    pub fn width(&self) -> usize {
        match self.values.as_slice().first() {
            Some(r) => r.len(),
            None => self.items.len(),
        }
    }
}

/// Whether expression `e` references any column slot below
/// `width` or any rowid source below `srcs` (v0.13). Slot and
/// source indices in resolved expressions are absolute
/// positions in the combined (enclosing ++ own) evaluation
/// row, so a reference below the threshold is exactly a
/// reference bound to an enclosing scope. Nested subquery
/// plans are walked with the same absolute threshold: their
/// interiors index the same combined row.
pub fn expr_refs_below(e: &RExpr, width: usize, srcs: usize) -> bool {
    expr_refs_range(e, &RefRange::below(width, srcs))
}

/// A band of the combined evaluation row (v0.16): column slots
/// `w.0..w.1` and rowid sources `s.0..s.1`.
#[derive(Debug, Clone, Copy)]
pub struct RefRange {
    pub w: (usize, usize),
    pub s: (usize, usize),
}

impl RefRange {
    /// Every slot below `width` and source below `srcs`.
    pub fn below(width: usize, srcs: usize) -> RefRange {
        RefRange {
            w: (0, width),
            s: (0, srcs),
        }
    }
    fn slot(&self, i: usize) -> bool {
        self.w.0 <= i && i < self.w.1
    }
    fn src(&self, i: usize) -> bool {
        self.s.0 <= i && i < self.s.1
    }
}

/// Whether expression `e` references a column slot or rowid
/// source inside band `r` (v0.16 generalisation of
/// `expr_refs_below`; the ON-clause forward-reference check
/// asks it for the band of the sources joined later). Nested
/// subquery plans are walked with the same absolute band:
/// their own sources lie past every band of the query that
/// contains them.
pub fn expr_refs_range(e: &RExpr, r: &RefRange) -> bool {
    let f = |x: &RExpr| expr_refs_range(x, r);
    match e {
        RExpr::Column(slot, ..) => r.slot(*slot),
        RExpr::Rowid(src) => r.src(*src),
        RExpr::SubScalar(p) | RExpr::SubExists(p) => plan_refs_range(p, r),
        RExpr::InSub(x, p, _) => f(x) || plan_refs_range(p, r),
        RExpr::Literal(_) | RExpr::AffLiteral(..) | RExpr::OutRef(_) => false,
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
            base.as_ref().is_some_and(|b| f(b))
                || whens.iter().any(|(w, t)| f(w) || f(t))
                || otherwise.as_ref().is_some_and(|o| f(o))
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } => args.iter().any(f),
    }
}

/// Whether any part of plan `p` references a column slot
/// below `width` or a rowid source below `srcs` (v0.13):
/// its cores' items / filter / grouping / having / ordering,
/// its sources' ON expressions and derived subplans, its
/// VALUES rows, and its compound tails — recursively, with
/// the same absolute threshold throughout. (LIMIT / OFFSET
/// are planned against an empty scope of their own and can
/// reference nothing outside it, so they are not walked.)
pub fn plan_refs_below(p: &SubPlan, width: usize, srcs: usize) -> bool {
    plan_refs_range(p, &RefRange::below(width, srcs))
}

/// `plan_refs_below` over an arbitrary band (v0.16).
pub fn plan_refs_range(p: &SubPlan, r: &RefRange) -> bool {
    let f = |x: &RExpr| expr_refs_range(x, r);
    p.sources.iter().any(|s| {
        s.on.as_ref().is_some_and(f) || s.derived.as_ref().is_some_and(|d| plan_refs_range(d, r))
    }) || p.items.iter().any(f)
        || p.filter.as_ref().is_some_and(f)
        || p.group_by.iter().any(f)
        || p.having.as_ref().is_some_and(f)
        || p.order_by.iter().any(|(e, _)| f(e))
        || p.values.iter().any(|row| row.iter().any(f))
        || p.compound.iter().any(|(_, t)| plan_refs_range(t, r))
}

/// Whether plan `p`, planned against enclosing scopes that
/// occupy the first `outer_width` column slots and the first
/// `outer_srcs` rowid sources of the combined evaluation row,
/// is uncorrelated (v0.13): nothing in it is bound to an
/// enclosing scope, so its result is fixed for the whole
/// statement execution and may be computed once and cached.
pub fn plan_is_uncorrelated(p: &SubPlan, outer_width: usize, outer_srcs: usize) -> bool {
    !plan_refs_below(p, outer_width, outer_srcs)
}

/// The computed value of one subquery node for one outer row
/// (v0.6): a scalar / EXISTS result, or the first-column values
/// of an IN subquery's rows. The `Option<Affinity>` is the
/// result column's affinity when the subquery's single result
/// item is a direct column reference / CAST / rowid (v0.8 —
/// see `RExpr::AffLiteral`); EXISTS results and non-column
/// result items carry `None`.
#[derive(Debug, Clone)]
pub enum SubValue {
    One(OwnedValue, Option<Affinity>),
    Many(AVec<OwnedValue>, Option<Affinity>),
}

/// The affinity a subquery's single result item carries into
/// an enclosing comparison (v0.8): a direct column reference,
/// a rowid reference, or a CAST keeps its affinity; every
/// other expression shape has none.
pub fn result_affinity(plan: &SubPlan) -> Option<Affinity> {
    match plan.items.as_slice().first() {
        Some(r) => item_result_affinity(r),
        None => None,
    }
}

/// The comparison affinity one resolved result item carries
/// (v0.9): the per-item form of `result_affinity`, used for
/// derived-table column affinities and compound metadata. A
/// VALUES core's items carry none.
pub fn item_result_affinity(r: &RExpr) -> Option<Affinity> {
    match r {
        RExpr::Column(_, aff, _) | RExpr::Cast(_, aff) => Some(*aff),
        RExpr::Collate(x, _) => item_result_affinity(x),
        RExpr::Rowid(_) => Some(Affinity::Integer),
        _ => None,
    }
}

/// A subquery node found in an expression, in substitution
/// traversal order (v0.6).
#[derive(Debug, Clone, Copy)]
pub enum SubKind<'x> {
    Scalar(&'x SubPlan),
    Exists(&'x SubPlan),
    In(&'x SubPlan),
}

pub(crate) fn is_rowid_name(b: &[u8]) -> bool {
    eq_ci(b, b"rowid") || eq_ci(b, b"oid") || eq_ci(b, b"_rowid_")
}

/// Try to resolve one column reference within a single scope
/// group (v0.6). Returns None when this group cannot answer the
/// reference at all (unqualified: no source has the column;
/// qualified: no source carries the qualifier), so the caller
/// may fall through to an enclosing scope. A group that *can*
/// answer — including with an ambiguity or a missing column on
/// a matched qualifier — answers definitively.
///
/// An unqualified name follows the pin's `lookupName` rules
/// (v0.16): real columns are matched across the group's
/// sources left to right; a later source that has the name
/// only as one of its USING (or NATURAL) columns does not make
/// it ambiguous — after an INNER / LEFT join the left-most
/// source keeps it, after a RIGHT join the right-most source
/// takes it, and a FULL join gathers every such side into
/// `coalesce(...)`. Only when no source has a real column so
/// named does `rowid` (or `oid` / `_rowid_`) bind to the one
/// source with a rowid — two such sources are ambiguous.
pub(crate) fn resolve_column_in_group<E>(
    group: &ScopeGroup,
    table: Option<&[u8]>,
    name: &[u8],
) -> Option<Result<RExpr, E>> {
    if let Some(q) = table {
        for (si, src) in group.sources.iter().enumerate() {
            if !eq_ci(src.qualifier.as_slice(), q) {
                continue;
            }
            if let Some(ci) = src.column_index(name) {
                return Some(Ok(src.column_ref(group, si, ci)));
            }
            if is_rowid_name(name) && src.has_rowid {
                return Some(Ok(RExpr::Rowid(group.first_src + si)));
            }
            // The qualifier matched but the column does not
            // exist: SQLite calls this "no such column" at
            // prepare time, in this scope.
            return Some(Err(Error::Schema {
                kind: SchemaKind::UnknownColumn,
            }));
        }
        return None;
    }
    let mut cnt = 0usize;
    let mut found: Option<RExpr> = None;
    let mut full: AVec<RExpr> = AVec::new();
    for (si, src) in group.sources.iter().enumerate() {
        let Some(ci) = src.column_index(name) else {
            continue;
        };
        if cnt > 0 {
            if !src.uses(name) {
                // A second real column not joined by USING:
                // ambiguous (cnt passes 1, the coalesce is off).
                full.clear();
            } else if !src.kind.pads_left() {
                // INNER / LEFT join: the left-most side stays.
                continue;
            } else if !src.kind.pads_right() {
                // RIGHT join: the right-most side takes over.
                cnt = 0;
                full.clear();
            } else if let Some(prev) = found.take() {
                // FULL join: coalesce over both sides.
                full.push(prev);
            }
        }
        cnt += 1;
        found = Some(src.column_ref(group, si, ci));
    }
    match (cnt, found) {
        (1, Some(e)) => Some(Ok(e)),
        (0, _) => {
            if !is_rowid_name(name) {
                return None;
            }
            let mut rowid: Option<RExpr> = None;
            for (si, src) in group.sources.iter().enumerate() {
                if src.has_rowid {
                    if rowid.is_some() {
                        return Some(Err(Error::Schema {
                            kind: SchemaKind::AmbiguousColumn,
                        }));
                    }
                    rowid = Some(RExpr::Rowid(group.first_src + si));
                }
            }
            rowid.map(Ok)
        }
        (n, Some(last)) if full.len() + 1 == n => {
            full.push(last);
            Some(Ok(RExpr::Scalar(ScalarFn::Coalesce, full)))
        }
        _ => Some(Err(Error::Schema {
            kind: SchemaKind::AmbiguousColumn,
        })),
    }
}

/// Resolve one column reference: the current query's sources
/// first, then enclosing scopes nearest-first (v0.6 subquery
/// shadowing). With no enclosing scopes this is exactly the
/// pre-v0.6 rule. v0.16: within each scope an unqualified name
/// no source has falls back to that scope's result-column
/// aliases (only filled under `v16`) before the next scope out.
fn resolve_column<E>(
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    table: Option<&[u8]>,
    name: &[u8],
) -> Result<RExpr, E> {
    #[cfg(not(feature = "v16"))]
    for g in core::iter::once(cur).chain(enclosing.iter()) {
        if let Some(r) = resolve_column_in_group::<E>(g, table, name) {
            return r;
        }
    }
    #[cfg(feature = "v16")]
    for (depth, g) in core::iter::once(cur).chain(enclosing.iter()).enumerate() {
        if let Some(r) = resolve_column_in_group::<E>(g, table, name) {
            return r;
        }
        if table.is_none()
            && let Some((_, e)) = g.aliases.iter().find(|(a, _)| eq_ci(a.as_slice(), name))
        {
            if depth == 0 {
                return Ok(e.clone());
            }
            return alias_from_outer(e, cur, enclosing, depth);
        }
    }
    Err(Error::Schema {
        kind: SchemaKind::UnknownColumn,
    })
}

/// An alias of an enclosing query, referenced from a subquery
/// `depth` scopes in (v0.16, `v16`). The alias's resolved item
/// is reused, rebased onto the current evaluation row: its
/// column refs into its own scope (and further out) keep their
/// slots, but any subquery inside it was planned with its own
/// rows numbered after its home scope, while here those rows
/// are appended after every scope of `cur` / `enclosing` — so
/// slots and rowid sources at or past the home stack's end
/// move by the difference (review regression: they read the
/// nested subquery's columns, silently wrong rows).
///
/// An alias whose item holds an aggregate (outside any nested
/// subquery) is refused: on the pin the aggregate still belongs
/// to the alias's own query (one value per outer group), which
/// hipp cannot evaluate inside a subquery — cloning it here
/// re-aggregated the subquery's rows (review regression).
#[cfg(feature = "v16")]
fn alias_from_outer<E>(
    e: &RExpr,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    depth: usize,
) -> Result<RExpr, E> {
    if has_agg(e) {
        return Err(Error::Unsupported);
    }
    let home = ScopeGroup::stack_end(enclosing.get(depth.saturating_sub(1)..).unwrap_or(&[]));
    let (cw, cs) = cur.end();
    let (ew, es) = ScopeGroup::stack_end(enclosing);
    let by = (
        cw.max(ew).saturating_sub(home.0),
        cs.max(es).saturating_sub(home.1),
    );
    let mut out = e.clone();
    shift_expr(&mut out, home, by);
    Ok(out)
}

/// Move every column slot at or past `at.0` by `by.0` and every
/// rowid source at or past `at.1` by `by.1`, through nested
/// subquery plans (v0.16, `alias_from_outer`).
#[cfg(feature = "v16")]
fn shift_expr(e: &mut RExpr, at: (usize, usize), by: (usize, usize)) {
    match e {
        RExpr::Column(slot, ..) => {
            if *slot >= at.0 {
                *slot = slot.saturating_add(by.0);
            }
        }
        RExpr::Rowid(src) => {
            if *src >= at.1 {
                *src = src.saturating_add(by.1);
            }
        }
        RExpr::SubScalar(p) | RExpr::SubExists(p) => shift_plan(p, at, by),
        RExpr::InSub(x, p, _) => {
            shift_expr(x, at, by);
            shift_plan(p, at, by);
        }
        RExpr::Literal(_) | RExpr::AffLiteral(..) | RExpr::OutRef(_) => {}
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            shift_expr(a, at, by);
            shift_expr(b, at, by);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => shift_expr(a, at, by),
        RExpr::InList(x, l, ..) => {
            shift_expr(x, at, by);
            for i in l.as_mut_slice() {
                shift_expr(i, at, by);
            }
        }
        RExpr::Between(x, a, b, _) => {
            shift_expr(x, at, by);
            shift_expr(a, at, by);
            shift_expr(b, at, by);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            for b in base.iter_mut().chain(otherwise.iter_mut()) {
                shift_expr(b, at, by);
            }
            for (w, t) in whens.as_mut_slice() {
                shift_expr(w, at, by);
                shift_expr(t, at, by);
            }
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } => {
            for a in args.as_mut_slice() {
                shift_expr(a, at, by);
            }
        }
    }
}

/// `shift_expr` over every part of a plan that indexes the
/// combined row (LIMIT / OFFSET run against an empty row of
/// their own and are left alone).
#[cfg(feature = "v16")]
fn shift_plan(p: &mut SubPlan, at: (usize, usize), by: (usize, usize)) {
    for s in p.sources.as_mut_slice() {
        if let Some(on) = &mut s.on {
            shift_expr(on, at, by);
        }
        if let Some(d) = &mut s.derived {
            shift_plan(d, at, by);
        }
    }
    let exprs = p
        .items
        .as_mut_slice()
        .iter_mut()
        .chain(p.filter.iter_mut())
        .chain(p.group_by.as_mut_slice().iter_mut())
        .chain(p.having.iter_mut())
        .chain(p.order_by.as_mut_slice().iter_mut().map(|(e, _)| e));
    for e in exprs {
        shift_expr(e, at, by);
    }
    for row in p.values.as_mut_slice() {
        for e in row.as_mut_slice() {
            shift_expr(e, at, by);
        }
    }
    for (_, t) in p.compound.as_mut_slice() {
        shift_plan(t, at, by);
    }
}

/// Expand `*` (`qual` None) or `q.*` over a scope group (v0.4;
/// USING-aware since v0.16, the pin's `selectExpander`) into
/// `(item, result name)` pairs; returns
/// whether any source matched (always true for `*` with
/// sources). For an unqualified `*`, a joined source's USING /
/// NATURAL columns are omitted — each shows once, at the
/// left-most source's position. Under an unqualified `*`, a
/// column of a source left of the FROM clause's last RIGHT /
/// FULL join that some later source names in USING expands to
/// the *unqualified* name, so it reads the joined value (the
/// right side after a RIGHT join, `coalesce(...)` after a FULL
/// one). `q.*` always reads `q`'s own columns (NULL on a row
/// another source contributed alone): CI against the 3.54.1 pin
/// (`SELECT a.* FROM a FULL JOIN b USING (id)` shows a NULL `id`
/// on the b-only rows); the 3.53 stand-in read the joined value
/// there too.
pub fn expand_star<E>(
    cur: &ScopeGroup,
    qual: Option<&[u8]>,
    out: &mut AVec<(RExpr, AVec<u8>)>,
) -> Result<bool, E> {
    let srcs = cur.sources.as_slice();
    let last_right = srcs.iter().rposition(|s| s.kind.pads_left());
    let mut hit = false;
    for (si, src) in srcs.iter().enumerate() {
        if let Some(q) = qual
            && !eq_ci(src.qualifier.as_slice(), q)
        {
            continue;
        }
        hit = true;
        for (ci, (cname, _)) in src.columns.iter().enumerate() {
            let n = cname.as_slice();
            if qual.is_none() && si > 0 && src.uses(n) {
                continue;
            }
            let joined = qual.is_none()
                && last_right.is_some_and(|r| si < r)
                && srcs
                    .get(si + 1..)
                    .is_some_and(|rest| rest.iter().any(|s| s.uses(n)));
            let e = if joined {
                match resolve_column_in_group::<E>(cur, None, n) {
                    Some(r) => r?,
                    None => src.column_ref(cur, si, ci),
                }
            } else {
                src.column_ref(cur, si, ci)
            };
            out.push((e, cname.clone()));
        }
    }
    Ok(hit)
}

fn literal_value(arena: &Arena, lit: Literal) -> OwnedValue {
    match lit {
        Literal::Null => OwnedValue::Null,
        Literal::Integer(i) => OwnedValue::Integer(i),
        Literal::Real(f) => OwnedValue::Real(f),
        Literal::Text(h) => {
            let mut v = AVec::new();
            v.extend_from_slice(handle_bytes(arena, h));
            OwnedValue::Text(v)
        }
        Literal::Blob(h) => {
            let mut v = AVec::new();
            v.extend_from_slice(handle_bytes(arena, h));
            OwnedValue::Blob(v)
        }
    }
}

/// The subquery planner callback (v0.6): given a subquery's
/// SELECT, the scope group of the query textually containing it,
/// and the enclosing scope groups (nearest first), produce its
/// plan. The session supplies the real planner; contexts that
/// cannot execute subqueries (constant folding) supply one that
/// fails with `Error::Unsupported`.
pub type SubPlanner<'p, E> =
    &'p mut dyn FnMut(&crate::sql::SelectStmt, &ScopeGroup, &[ScopeGroup]) -> Result<SubPlan, E>;

/// A scalar / EXISTS subquery reads at most its first row, and
/// the pin (`sqlite3CodeSubselect`) rewrites its `LIMIT X` to
/// `LIMIT (X <> 0)`, the `0` carrying NUMERIC affinity (v0.16,
/// `v16`): `LIMIT 2.5`, `LIMIT 'x'` or `LIMIT x'00'` become
/// `LIMIT 1` there instead of a datatype mismatch, `LIMIT '0'`
/// stays `LIMIT 0`, and a NULL limit still errors. OFFSET is
/// left alone, as are IN subqueries and derived tables.
///
/// Without a LIMIT of its own the subquery gets the pin's
/// implicit integer `LIMIT 1`, which is what lets a UNION
/// compound take the pin's first-row shortcut
/// (`session::union_first_row`; v0.16, CI against the pin).
#[cfg(feature = "v16")]
fn first_row_limit(mut plan: SubPlan) -> SubPlan {
    plan.limit = Some(match plan.limit.take() {
        Some(x) => RExpr::Binary(
            BinOp::Ne,
            Box::new(x),
            Box::new(RExpr::AffLiteral(OwnedValue::Integer(0), Affinity::Numeric)),
        ),
        None => RExpr::Literal(OwnedValue::Integer(1)),
    });
    plan
}

/// `first_row_limit` without `v16`: a LIMIT is an integer
/// literal there, for which the `X <> 0` rewrite changes
/// nothing; the implicit `LIMIT 1` still applies.
#[cfg(not(feature = "v16"))]
fn first_row_limit(mut plan: SubPlan) -> SubPlan {
    if plan.limit.is_none() {
        plan.limit = Some(RExpr::Literal(OwnedValue::Integer(1)));
    }
    plan
}

/// Resolve a parser `Expr` against the FROM sources, with no
/// enclosing scope and no subquery support (v0.6 contexts that
/// plan subqueries call `resolve_scoped` directly). Aggregate
/// functions resolve to `RExpr::Agg` wherever they appear; the
/// planner decides where aggregates are legal.
pub fn resolve<E>(
    expr: &Expr,
    arena: &Arena,
    sources: &[SourceDesc],
    offsets: &[usize],
) -> Result<RExpr, E> {
    let mut srcs = AVec::new();
    for s in sources {
        srcs.push(s.clone());
    }
    let mut offs = AVec::new();
    for o in offsets {
        offs.push(*o);
    }
    let cur = ScopeGroup::new(srcs, offs, 0);
    let mut planner = |_: &crate::sql::SelectStmt,
                       _: &ScopeGroup,
                       _: &[ScopeGroup]|
     -> Result<SubPlan, E> { Err(Error::Unsupported) };
    resolve_scoped::<E>(expr, arena, &cur, &[], &mut planner)
}

/// Resolve a parser `Expr` against the current scope group plus
/// enclosing groups (nearest first), planning subqueries through
/// `planner` (v0.6). Column resolution shadows outward: the
/// current group answers first.
pub fn resolve_scoped<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    Ok(match expr {
        Expr::Literal(l) => RExpr::Literal(literal_value(arena, *l)),
        // Only a stored schema text still holds a parameter here
        // (a prepared statement's are bound away before planning,
        // `session::bind_ast`); the pin reads one there as NULL.
        #[cfg(feature = "params")]
        Expr::Param(..) => RExpr::Literal(OwnedValue::Null),
        Expr::Column { table, name } => resolve_column::<E>(
            cur,
            enclosing,
            table.map(|h| handle_bytes(arena, h)),
            handle_bytes(arena, *name),
        )?,
        Expr::InSubquery(x, stmt, neg) => {
            let xr = resolve_scoped::<E>(x, arena, cur, enclosing, &mut *planner)?;
            let plan = planner(stmt, cur, enclosing)?;
            if plan.width() != 1 {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            RExpr::InSub(Box::new(xr), Box::new(plan), *neg)
        }
        Expr::ScalarSubquery(stmt) => {
            let plan = planner(stmt, cur, enclosing)?;
            if plan.width() != 1 {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            RExpr::SubScalar(Box::new(first_row_limit(plan)))
        }
        Expr::ExistsSubquery(stmt) => {
            RExpr::SubExists(Box::new(first_row_limit(planner(stmt, cur, enclosing)?)))
        }
        Expr::Binary(op, l, r) => RExpr::Binary(
            *op,
            Box::new(resolve_scoped::<E>(
                l,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            Box::new(resolve_scoped::<E>(
                r,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
        ),
        Expr::Neg(e) => RExpr::Neg(Box::new(resolve_scoped::<E>(
            e,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?)),
        Expr::Pos(e) => RExpr::Pos(Box::new(resolve_scoped::<E>(
            e,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?)),
        Expr::Not(e) => RExpr::Not(Box::new(resolve_scoped::<E>(
            e,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?)),
        Expr::IsNull(e, neg) => RExpr::IsNull(
            Box::new(resolve_scoped::<E>(
                e,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            *neg,
        ),
        Expr::InList(x, list, neg) => {
            let mut out = AVec::new();
            for e in list.iter() {
                out.push(resolve_scoped::<E>(
                    e,
                    arena,
                    cur,
                    enclosing,
                    &mut *planner,
                )?);
            }
            RExpr::InList(
                Box::new(resolve_scoped::<E>(
                    x,
                    arena,
                    cur,
                    enclosing,
                    &mut *planner,
                )?),
                out,
                *neg,
                false,
            )
        }
        Expr::Between(x, lo, hi, neg) => RExpr::Between(
            Box::new(resolve_scoped::<E>(
                x,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            Box::new(resolve_scoped::<E>(
                lo,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            Box::new(resolve_scoped::<E>(
                hi,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            *neg,
        ),
        Expr::Like(x, p, neg) => RExpr::Like(
            Box::new(resolve_scoped::<E>(
                x,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            Box::new(resolve_scoped::<E>(
                p,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            *neg,
        ),
        Expr::Case {
            base,
            whens,
            otherwise,
        } => {
            let mut ws = AVec::new();
            for (w, t) in whens.iter() {
                ws.push((
                    resolve_scoped::<E>(w, arena, cur, enclosing, &mut *planner)?,
                    resolve_scoped::<E>(t, arena, cur, enclosing, &mut *planner)?,
                ));
            }
            RExpr::Case {
                base: match base {
                    Some(b) => Some(Box::new(resolve_scoped::<E>(
                        b,
                        arena,
                        cur,
                        enclosing,
                        &mut *planner,
                    )?)),
                    None => None,
                },
                whens: ws,
                otherwise: match otherwise {
                    Some(o) => Some(Box::new(resolve_scoped::<E>(
                        o,
                        arena,
                        cur,
                        enclosing,
                        &mut *planner,
                    )?)),
                    None => None,
                },
            }
        }
        Expr::Cast(e, ty) => RExpr::Cast(
            Box::new(resolve_scoped::<E>(
                e,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            crate::record::affinity_of(handle_bytes(arena, *ty)),
        ),
        #[cfg(feature = "v16")]
        Expr::Collate(e, c) => RExpr::Collate(
            Box::new(resolve_scoped::<E>(
                e,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            *c,
        ),
        Expr::Func {
            name,
            distinct,
            star,
            args,
        } => resolve_func::<E>(
            handle_bytes(arena, *name),
            *distinct,
            *star,
            args.as_slice(),
            arena,
            cur,
            enclosing,
            planner,
        )?,
    })
}

fn resolve_args<E>(
    args: &[Expr],
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<AVec<RExpr>, E> {
    let mut out = AVec::new();
    for a in args.iter() {
        out.push(resolve_scoped::<E>(
            a,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn resolve_func<E>(
    name: &[u8],
    distinct: bool,
    star: bool,
    args: &[Expr],
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    let agg = if eq_ci(name, b"count") {
        Some(AggKind::Count)
    } else if eq_ci(name, b"sum") {
        Some(AggKind::Sum)
    } else if eq_ci(name, b"avg") {
        Some(AggKind::Avg)
    } else if eq_ci(name, b"total") {
        Some(AggKind::Total)
    } else if eq_ci(name, b"min") {
        Some(AggKind::Min)
    } else if eq_ci(name, b"max") {
        Some(AggKind::Max)
    } else if eq_ci(name, b"group_concat") {
        Some(AggKind::GroupConcat)
    } else {
        None
    };
    if let Some(kind) = agg {
        // min/max with several arguments are the scalar forms.
        if matches!(kind, AggKind::Min | AggKind::Max) && args.len() > 1 {
            let f = if kind == AggKind::Min {
                ScalarFn::MinN
            } else {
                ScalarFn::MaxN
            };
            return Ok(RExpr::Scalar(
                f,
                resolve_args::<E>(args, arena, cur, enclosing, &mut *planner)?,
            ));
        }
        let want = match kind {
            AggKind::GroupConcat => (1, 2),
            _ => (1, 1),
        };
        if star {
            if kind != AggKind::Count {
                return Err(Error::Unsupported);
            }
            return Ok(RExpr::Agg {
                kind,
                distinct: false,
                star: true,
                args: AVec::new(),
            });
        }
        if args.len() < want.0 || args.len() > want.1 {
            return Err(Error::Unsupported);
        }
        return Ok(RExpr::Agg {
            kind,
            distinct,
            star: false,
            args: resolve_args::<E>(args, arena, cur, enclosing, &mut *planner)?,
        });
    }
    if star || distinct {
        return Err(Error::Unsupported);
    }
    let f = if eq_ci(name, b"abs") {
        ScalarFn::Abs
    } else if eq_ci(name, b"length") {
        ScalarFn::Length
    } else if eq_ci(name, b"upper") {
        ScalarFn::Upper
    } else if eq_ci(name, b"lower") {
        ScalarFn::Lower
    } else if eq_ci(name, b"coalesce") || eq_ci(name, b"ifnull") {
        ScalarFn::Coalesce
    } else if eq_ci(name, b"nullif") {
        ScalarFn::Nullif
    } else if eq_ci(name, b"typeof") {
        ScalarFn::Typeof
    } else if eq_ci(name, b"substr") || eq_ci(name, b"substring") {
        ScalarFn::Substr
    } else if eq_ci(name, b"round") {
        ScalarFn::Round
    } else if eq_ci(name, b"hex") {
        ScalarFn::Hex
    } else if eq_ci(name, b"trim") {
        ScalarFn::Trim
    } else if eq_ci(name, b"ltrim") {
        ScalarFn::Ltrim
    } else if eq_ci(name, b"rtrim") {
        ScalarFn::Rtrim
    } else if eq_ci(name, b"replace") {
        ScalarFn::Replace
    } else if eq_ci(name, b"instr") {
        ScalarFn::Instr
    } else if eq_ci(name, b"sign") {
        ScalarFn::Sign
    } else if eq_ci(name, b"iif") {
        ScalarFn::Iif
    } else if cfg!(feature = "v16") && eq_ci(name, b"like") {
        #[cfg(feature = "v16")]
        {
            ScalarFn::Like
        }
        #[cfg(not(feature = "v16"))]
        return Err(Error::Unsupported);
    } else if cfg!(feature = "v16") && eq_ci(name, b"glob") {
        #[cfg(feature = "v16")]
        {
            ScalarFn::Glob
        }
        #[cfg(not(feature = "v16"))]
        return Err(Error::Unsupported);
    } else {
        return Err(Error::Unsupported);
    };
    let want = match f {
        ScalarFn::Coalesce => (1, usize::MAX),
        ScalarFn::Nullif => (2, 2),
        ScalarFn::Substr => (2, 3),
        ScalarFn::Round => (1, 2),
        ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => (1, 2),
        ScalarFn::Replace => (3, 3),
        ScalarFn::Instr => (2, 2),
        ScalarFn::Iif => (3, 3),
        #[cfg(feature = "v16")]
        ScalarFn::Like => (2, 3),
        #[cfg(feature = "v16")]
        ScalarFn::Glob => (2, 2),
        ScalarFn::MinN | ScalarFn::MaxN => (2, usize::MAX),
        _ => (1, 1),
    };
    if args.len() < want.0 || args.len() > want.1 {
        return Err(Error::Unsupported);
    }
    Ok(RExpr::Scalar(
        f,
        resolve_args::<E>(args, arena, cur, enclosing, &mut *planner)?,
    ))
}

/// Whether a resolved expression contains an aggregate node.
/// Aggregates inside a subquery belong to the subquery's own
/// grouping, never to the enclosing query, so subquery nodes
/// are not descended into (except an IN subquery's left
/// operand, which is an enclosing-scope expression).
pub fn has_agg(e: &RExpr) -> bool {
    match e {
        RExpr::Agg { .. } => true,
        RExpr::SubScalar(_) | RExpr::SubExists(_) => false,
        RExpr::InSub(x, _, _) => has_agg(x),
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => false,
        RExpr::Binary(_, a, b) => has_agg(a) || has_agg(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => has_agg(a),
        RExpr::InList(x, l, ..) => has_agg(x) || l.iter().any(has_agg),
        RExpr::Between(x, a, b, _) => has_agg(x) || has_agg(a) || has_agg(b),
        RExpr::Like(a, b, _) => has_agg(a) || has_agg(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_ref().is_some_and(|b| has_agg(b))
                || whens.iter().any(|(w, t)| has_agg(w) || has_agg(t))
                || otherwise.as_ref().is_some_and(|o| has_agg(o))
        }
        RExpr::Scalar(_, args) => args.iter().any(has_agg),
    }
}

/// Collect aggregate nodes (for the min/max bare-column rule).
/// Subquery interiors are a separate grouping scope and are
/// not descended into.
pub fn collect_aggs<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) => {}
        RExpr::InSub(x, _, _) => collect_aggs(x, out),
        RExpr::Agg { args, .. } => {
            out.push(e);
            for a in args.iter() {
                collect_aggs(a, out);
            }
        }
        RExpr::Binary(_, a, b) => {
            collect_aggs(a, out);
            collect_aggs(b, out);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => collect_aggs(a, out),
        RExpr::InList(x, l, ..) => {
            collect_aggs(x, out);
            for i in l.iter() {
                collect_aggs(i, out);
            }
        }
        RExpr::Between(x, a, b, _) => {
            collect_aggs(x, out);
            collect_aggs(a, out);
            collect_aggs(b, out);
        }
        RExpr::Like(a, b, _) => {
            collect_aggs(a, out);
            collect_aggs(b, out);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                collect_aggs(b, out);
            }
            for (w, t) in whens.iter() {
                collect_aggs(w, out);
                collect_aggs(t, out);
            }
            if let Some(o) = otherwise {
                collect_aggs(o, out);
            }
        }
        RExpr::Scalar(_, args) => {
            for a in args.iter() {
                collect_aggs(a, out);
            }
        }
        _ => {}
    }
}

/// Whether a resolved expression contains a subquery node in
/// the *current* scope (v0.6). Subquery interiors are not
/// descended into: their nodes belong to the subquery's own
/// execution. An IN subquery's left operand is current-scope.
pub fn has_sub(e: &RExpr) -> bool {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) | RExpr::InSub(..) => true,
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => false,
        RExpr::Binary(_, a, b) => has_sub(a) || has_sub(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => has_sub(a),
        RExpr::InList(x, l, ..) => has_sub(x) || l.iter().any(has_sub),
        RExpr::Between(x, a, b, _) => has_sub(x) || has_sub(a) || has_sub(b),
        RExpr::Like(a, b, _) => has_sub(a) || has_sub(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_ref().is_some_and(|b| has_sub(b))
                || whens.iter().any(|(w, t)| has_sub(w) || has_sub(t))
                || otherwise.as_ref().is_some_and(|o| has_sub(o))
        }
        RExpr::Scalar(_, args) => args.iter().any(has_sub),
        RExpr::Agg { args, .. } => args.iter().any(has_sub),
    }
}

/// Collect the current-scope subquery nodes of `e` in
/// substitution traversal order (v0.6): children before the
/// node itself where a node has current-scope children (only
/// `InSub` does). `substitute` consumes values in exactly this
/// order.
pub fn collect_subs<'x>(e: &'x RExpr, out: &mut AVec<SubKind<'x>>) {
    match e {
        RExpr::SubScalar(p) => out.push(SubKind::Scalar(p)),
        RExpr::SubExists(p) => out.push(SubKind::Exists(p)),
        RExpr::InSub(x, p, _) => {
            collect_subs(x, out);
            out.push(SubKind::In(p));
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => {}
        RExpr::Binary(_, a, b) => {
            collect_subs(a, out);
            collect_subs(b, out);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => collect_subs(a, out),
        RExpr::InList(x, l, ..) => {
            collect_subs(x, out);
            for i in l.iter() {
                collect_subs(i, out);
            }
        }
        RExpr::Between(x, a, b, _) => {
            collect_subs(x, out);
            collect_subs(a, out);
            collect_subs(b, out);
        }
        RExpr::Like(a, b, _) => {
            collect_subs(a, out);
            collect_subs(b, out);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                collect_subs(b, out);
            }
            for (w, t) in whens.iter() {
                collect_subs(w, out);
                collect_subs(t, out);
            }
            if let Some(o) = otherwise {
                collect_subs(o, out);
            }
        }
        RExpr::Scalar(_, args) => {
            for a in args.iter() {
                collect_subs(a, out);
            }
        }
        RExpr::Agg { args, .. } => {
            for a in args.iter() {
                collect_subs(a, out);
            }
        }
    }
}

/// Rebuild `e` with every current-scope subquery node replaced
/// by its computed value (v0.6): scalar / EXISTS results become
/// literals, an IN subquery becomes an IN list of literals.
/// The left operand of an IN subquery after substitution
/// (v0.16): `sx` carrying the membership test's collation —
/// the binary rule between the left operand and the
/// subquery's result item (its rightmost core's, as on the
/// pin) — as an explicit `Collate`, since the IN list it
/// becomes compares by its left operand's collation alone.
fn in_sub_lhs(x: &RExpr, plan: &SubPlan, sx: RExpr) -> RExpr {
    let core = plan.compound.as_slice().last().map_or(plan, |(_, t)| t);
    let item = core.items.as_slice().first().or_else(|| {
        core.values
            .as_slice()
            .first()
            .and_then(|r| r.as_slice().first())
    });
    let coll = match item {
        Some(i) => binary_coll(x, i),
        None => expr_coll(x).unwrap_or_default(),
    };
    RExpr::Collate(Box::new(sx), coll)
}

/// `values` is consumed in `collect_subs` order; the caller
/// guarantees the counts match.
pub fn substitute(e: &RExpr, values: &[SubValue], pos: &mut usize) -> RExpr {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) => {
            let v = match &values[*pos] {
                SubValue::One(v, aff) => match aff {
                    Some(a) => RExpr::AffLiteral(v.clone(), *a),
                    None => RExpr::Literal(v.clone()),
                },
                SubValue::Many(_, _) => RExpr::Literal(OwnedValue::Null),
            };
            *pos += 1;
            v
        }
        RExpr::InSub(x, plan, neg) => {
            let sx = in_sub_lhs(x, plan, substitute(x, values, pos));
            let wrap = |v: &OwnedValue, aff: &Option<Affinity>| match aff {
                Some(a) => RExpr::AffLiteral(v.clone(), *a),
                None => RExpr::Literal(v.clone()),
            };
            let list = match &values[*pos] {
                SubValue::Many(vs, aff) => {
                    let mut out = AVec::new();
                    for v in vs.iter() {
                        out.push(wrap(v, aff));
                    }
                    out
                }
                SubValue::One(v, aff) => {
                    let mut out = AVec::new();
                    out.push(wrap(v, aff));
                    out
                }
            };
            *pos += 1;
            RExpr::InList(Box::new(sx), list, *neg, true)
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => e.clone(),
        RExpr::Binary(op, a, b) => RExpr::Binary(
            *op,
            Box::new(substitute(a, values, pos)),
            Box::new(substitute(b, values, pos)),
        ),
        RExpr::Neg(a) => RExpr::Neg(Box::new(substitute(a, values, pos))),
        RExpr::Pos(a) => RExpr::Pos(Box::new(substitute(a, values, pos))),
        RExpr::Not(a) => RExpr::Not(Box::new(substitute(a, values, pos))),
        RExpr::IsNull(a, neg) => RExpr::IsNull(Box::new(substitute(a, values, pos)), *neg),
        RExpr::InList(x, l, neg, sub) => {
            let sx = substitute(x, values, pos);
            let mut out = AVec::new();
            for i in l.iter() {
                out.push(substitute(i, values, pos));
            }
            RExpr::InList(Box::new(sx), out, *neg, *sub)
        }
        RExpr::Between(x, a, b, neg) => RExpr::Between(
            Box::new(substitute(x, values, pos)),
            Box::new(substitute(a, values, pos)),
            Box::new(substitute(b, values, pos)),
            *neg,
        ),
        RExpr::Like(a, b, neg) => RExpr::Like(
            Box::new(substitute(a, values, pos)),
            Box::new(substitute(b, values, pos)),
            *neg,
        ),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            let sbase = base.as_ref().map(|b| Box::new(substitute(b, values, pos)));
            let mut ws = AVec::new();
            for (w, t) in whens.iter() {
                ws.push((substitute(w, values, pos), substitute(t, values, pos)));
            }
            let sother = otherwise
                .as_ref()
                .map(|o| Box::new(substitute(o, values, pos)));
            RExpr::Case {
                base: sbase,
                whens: ws,
                otherwise: sother,
            }
        }
        RExpr::Scalar(f, args) => {
            let mut out = AVec::new();
            for a in args.iter() {
                out.push(substitute(a, values, pos));
            }
            RExpr::Scalar(*f, out)
        }
        RExpr::Cast(a, aff) => RExpr::Cast(Box::new(substitute(a, values, pos)), *aff),
        RExpr::Collate(a, c) => RExpr::Collate(Box::new(substitute(a, values, pos)), *c),
        RExpr::Agg {
            kind,
            distinct,
            star,
            args,
        } => {
            let mut out = AVec::new();
            for a in args.iter() {
                out.push(substitute(a, values, pos));
            }
            RExpr::Agg {
                kind: *kind,
                distinct: *distinct,
                star: *star,
                args: out,
            }
        }
    }
}

/// Whether a subquery node occurs inside an aggregate's
/// arguments: such subqueries need per-group-row execution
/// (v0.11, behind the `v10` feature). Without that feature the
/// planners refuse them at plan time with
/// `Error::Unsupported` rather than mis-evaluating.
pub fn sub_in_agg_args(e: &RExpr) -> bool {
    match e {
        RExpr::Agg { args, .. } => args.iter().any(has_sub),
        RExpr::SubScalar(_) | RExpr::SubExists(_) => false,
        RExpr::InSub(x, _, _) => sub_in_agg_args(x),
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => false,
        RExpr::Binary(_, a, b) => sub_in_agg_args(a) || sub_in_agg_args(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => sub_in_agg_args(a),
        RExpr::InList(x, l, ..) => sub_in_agg_args(x) || l.iter().any(sub_in_agg_args),
        RExpr::Between(x, a, b, _) => {
            sub_in_agg_args(x) || sub_in_agg_args(a) || sub_in_agg_args(b)
        }
        RExpr::Like(a, b, _) => sub_in_agg_args(a) || sub_in_agg_args(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_ref().is_some_and(|b| sub_in_agg_args(b))
                || whens
                    .iter()
                    .any(|(w, t)| sub_in_agg_args(w) || sub_in_agg_args(t))
                || otherwise.as_ref().is_some_and(|o| sub_in_agg_args(o))
        }
        RExpr::Scalar(_, args) => args.iter().any(sub_in_agg_args),
    }
}

/// Collect the *outermost* aggregate nodes of `e` (v0.11),
/// in the traversal order `substitute_group` consumes them:
/// like `collect_aggs`, but an aggregate's own arguments are
/// not descended into (a nested aggregate there is a misuse
/// the evaluator refuses, not a second fold). Subquery
/// interiors are a separate grouping scope, as in
/// `collect_aggs`.
pub fn collect_outer_aggs<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) => {}
        RExpr::InSub(x, _, _) => collect_outer_aggs(x, out),
        RExpr::Agg { .. } => out.push(e),
        RExpr::Binary(_, a, b) => {
            collect_outer_aggs(a, out);
            collect_outer_aggs(b, out);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => collect_outer_aggs(a, out),
        RExpr::InList(x, l, ..) => {
            collect_outer_aggs(x, out);
            for i in l.iter() {
                collect_outer_aggs(i, out);
            }
        }
        RExpr::Between(x, a, b, _) => {
            collect_outer_aggs(x, out);
            collect_outer_aggs(a, out);
            collect_outer_aggs(b, out);
        }
        RExpr::Like(a, b, _) => {
            collect_outer_aggs(a, out);
            collect_outer_aggs(b, out);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                collect_outer_aggs(b, out);
            }
            for (w, t) in whens.iter() {
                collect_outer_aggs(w, out);
                collect_outer_aggs(t, out);
            }
            if let Some(o) = otherwise {
                collect_outer_aggs(o, out);
            }
        }
        RExpr::Scalar(_, args) => {
            for a in args.iter() {
                collect_outer_aggs(a, out);
            }
        }
        _ => {}
    }
}

/// Collect the current-scope subquery nodes of `e` that lie
/// OUTSIDE any aggregate's arguments (v0.11), in the order
/// `substitute_group` consumes them. Aggregate interiors are
/// skipped entirely: their subqueries are computed per group
/// row by the executor, never against the representative row.
pub fn collect_subs_outside_agg<'x>(e: &'x RExpr, out: &mut AVec<SubKind<'x>>) {
    match e {
        RExpr::SubScalar(p) => out.push(SubKind::Scalar(p)),
        RExpr::SubExists(p) => out.push(SubKind::Exists(p)),
        RExpr::InSub(x, p, _) => {
            collect_subs_outside_agg(x, out);
            out.push(SubKind::In(p));
        }
        RExpr::Agg { .. }
        | RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => {}
        RExpr::Binary(_, a, b) => {
            collect_subs_outside_agg(a, out);
            collect_subs_outside_agg(b, out);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => collect_subs_outside_agg(a, out),
        RExpr::InList(x, l, ..) => {
            collect_subs_outside_agg(x, out);
            for i in l.iter() {
                collect_subs_outside_agg(i, out);
            }
        }
        RExpr::Between(x, a, b, _) => {
            collect_subs_outside_agg(x, out);
            collect_subs_outside_agg(a, out);
            collect_subs_outside_agg(b, out);
        }
        RExpr::Like(a, b, _) => {
            collect_subs_outside_agg(a, out);
            collect_subs_outside_agg(b, out);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                collect_subs_outside_agg(b, out);
            }
            for (w, t) in whens.iter() {
                collect_subs_outside_agg(w, out);
                collect_subs_outside_agg(t, out);
            }
            if let Some(o) = otherwise {
                collect_subs_outside_agg(o, out);
            }
        }
        RExpr::Scalar(_, args) => {
            for a in args.iter() {
                collect_subs_outside_agg(a, out);
            }
        }
    }
}

/// Rebuild a group expression for synchronous evaluation
/// (v0.11): every outermost aggregate node is replaced by the
/// literal of its already-computed fold (consumed from
/// `agg_values` in `collect_outer_aggs` order), and every
/// subquery node outside an aggregate's arguments is replaced
/// by its representative-row value (consumed from `values` in
/// `collect_subs_outside_agg` order). Subqueries INSIDE an
/// aggregate were folded into that aggregate's value per
/// group row by the executor, so nothing of them remains.
/// The result contains no aggregate and no subquery node, so
/// bare columns read the representative row exactly as in a
/// plain group evaluation.
pub fn substitute_group(
    e: &RExpr,
    values: &[SubValue],
    pos: &mut usize,
    agg_values: &[OwnedValue],
    agg_pos: &mut usize,
) -> RExpr {
    match e {
        RExpr::Agg { .. } => {
            let v = agg_values[*agg_pos].clone();
            *agg_pos += 1;
            RExpr::Literal(v)
        }
        RExpr::SubScalar(_) | RExpr::SubExists(_) => {
            let v = match &values[*pos] {
                SubValue::One(v, aff) => match aff {
                    Some(a) => RExpr::AffLiteral(v.clone(), *a),
                    None => RExpr::Literal(v.clone()),
                },
                SubValue::Many(_, _) => RExpr::Literal(OwnedValue::Null),
            };
            *pos += 1;
            v
        }
        RExpr::InSub(x, plan, neg) => {
            let sx = in_sub_lhs(
                x,
                plan,
                substitute_group(x, values, pos, agg_values, agg_pos),
            );
            let wrap = |v: &OwnedValue, aff: &Option<Affinity>| match aff {
                Some(a) => RExpr::AffLiteral(v.clone(), *a),
                None => RExpr::Literal(v.clone()),
            };
            let list = match &values[*pos] {
                SubValue::Many(vs, aff) => {
                    let mut out = AVec::new();
                    for v in vs.iter() {
                        out.push(wrap(v, aff));
                    }
                    out
                }
                SubValue::One(v, aff) => {
                    let mut out = AVec::new();
                    out.push(wrap(v, aff));
                    out
                }
            };
            *pos += 1;
            RExpr::InList(Box::new(sx), list, *neg, true)
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_) => e.clone(),
        RExpr::Binary(op, a, b) => RExpr::Binary(
            *op,
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            Box::new(substitute_group(b, values, pos, agg_values, agg_pos)),
        ),
        RExpr::Neg(a) => RExpr::Neg(Box::new(substitute_group(
            a, values, pos, agg_values, agg_pos,
        ))),
        RExpr::Pos(a) => RExpr::Pos(Box::new(substitute_group(
            a, values, pos, agg_values, agg_pos,
        ))),
        RExpr::Not(a) => RExpr::Not(Box::new(substitute_group(
            a, values, pos, agg_values, agg_pos,
        ))),
        RExpr::IsNull(a, neg) => RExpr::IsNull(
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            *neg,
        ),
        RExpr::InList(x, l, neg, sub) => {
            let sx = substitute_group(x, values, pos, agg_values, agg_pos);
            let mut out = AVec::new();
            for i in l.iter() {
                out.push(substitute_group(i, values, pos, agg_values, agg_pos));
            }
            RExpr::InList(Box::new(sx), out, *neg, *sub)
        }
        RExpr::Between(x, a, b, neg) => RExpr::Between(
            Box::new(substitute_group(x, values, pos, agg_values, agg_pos)),
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            Box::new(substitute_group(b, values, pos, agg_values, agg_pos)),
            *neg,
        ),
        RExpr::Like(a, b, neg) => RExpr::Like(
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            Box::new(substitute_group(b, values, pos, agg_values, agg_pos)),
            *neg,
        ),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            let sbase = base
                .as_ref()
                .map(|b| Box::new(substitute_group(b, values, pos, agg_values, agg_pos)));
            let mut ws = AVec::new();
            for (w, t) in whens.iter() {
                ws.push((
                    substitute_group(w, values, pos, agg_values, agg_pos),
                    substitute_group(t, values, pos, agg_values, agg_pos),
                ));
            }
            let sother = otherwise
                .as_ref()
                .map(|o| Box::new(substitute_group(o, values, pos, agg_values, agg_pos)));
            RExpr::Case {
                base: sbase,
                whens: ws,
                otherwise: sother,
            }
        }
        RExpr::Scalar(f, args) => {
            let mut out = AVec::new();
            for a in args.iter() {
                out.push(substitute_group(a, values, pos, agg_values, agg_pos));
            }
            RExpr::Scalar(*f, out)
        }
        RExpr::Cast(a, aff) => RExpr::Cast(
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            *aff,
        ),
        RExpr::Collate(a, c) => RExpr::Collate(
            Box::new(substitute_group(a, values, pos, agg_values, agg_pos)),
            *c,
        ),
    }
}

// ---------------- value coercion ----------------

/// A numeric view of a value (SQLite arithmetic conversion).
#[derive(Debug, Clone, Copy)]
pub enum Num {
    Int(i64),
    Real(f64),
}

impl Num {
    pub fn as_f64(&self) -> f64 {
        match self {
            Num::Int(i) => *i as f64,
            Num::Real(r) => *r,
        }
    }
}

/// Parse the longest numeric prefix of `t` (SQLite's text→number
/// conversion for CAST and arithmetic): leading spaces skipped,
/// optional sign, digits with optional fraction/exponent. Returns
/// None when the prefix has no digits at all.
pub fn parse_num_prefix(t: &[u8]) -> Option<Num> {
    let s = core::str::from_utf8(t).ok()?;
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let mut digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    let mut is_real = false;
    if i < b.len() && b[i] == b'.' {
        is_real = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            is_real = true;
            i = j;
        }
    }
    let text = &s[start..i];
    if !is_real {
        if let Ok(v) = text.parse::<i64>() {
            return Some(Num::Int(v));
        }
        // Integer literal too large for i64: SQLite saturates for
        // CAST, and yields a real for arithmetic. Use real here;
        // the CAST-to-integer path clamps separately.
        return crate::sql::lexer::parse_real_signed(text.as_bytes()).map(Num::Real);
    }
    crate::sql::lexer::parse_real_signed(text.as_bytes()).map(Num::Real)
}

/// The number a value converts to for arithmetic: NULL stays NULL
/// (the caller checks), non-numeric text/blob convert to 0.
pub fn to_number(v: &OwnedValue) -> Option<Num> {
    match v {
        OwnedValue::Null => None,
        OwnedValue::Integer(i) => Some(Num::Int(*i)),
        OwnedValue::Real(r) => Some(Num::Real(*r)),
        OwnedValue::Text(t) => Some(parse_num_prefix(t.as_slice()).unwrap_or(Num::Int(0))),
        OwnedValue::Blob(t) => Some(parse_num_prefix(t.as_slice()).unwrap_or(Num::Int(0))),
    }
}

/// The pin's `sqlite3Atoi64` over text (v0.16): ASCII spaces,
/// an optional sign, then the leading digits only — a `.` or an
/// exponent ends the number (`'1e18'` is 1) — saturating at the
/// i64 bounds. What the pin's `sqlite3VdbeIntValue` reads from
/// a TEXT or BLOB: CAST(x AS INTEGER) and `%` operands.
pub fn atoi_prefix(t: &[u8]) -> i64 {
    let mut i = 0usize;
    while t
        .get(i)
        .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        i += 1;
    }
    let neg = t.get(i) == Some(&b'-');
    if matches!(t.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    // Accumulate the magnitude as a negative number, which holds
    // i64::MIN exactly; saturate on overflow.
    let mut v: i64 = 0;
    let mut over = false;
    while let Some(d) = t.get(i).filter(|c| c.is_ascii_digit()) {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_sub(i64::from(d - b'0')))
        {
            Some(x) => v = x,
            None => over = true,
        }
        i += 1;
    }
    match (neg, over) {
        (true, true) => i64::MIN,
        (false, true) => i64::MAX,
        (true, false) => v,
        (false, false) => v.checked_neg().unwrap_or(i64::MAX),
    }
}

/// The integer the pin's `sqlite3VdbeIntValue` reads from a
/// value (v0.16): reals truncate and clamp, text and blobs take
/// their integer prefix (`atoi_prefix`).
fn int_value(v: &OwnedValue) -> i64 {
    match v {
        OwnedValue::Null => 0,
        OwnedValue::Integer(i) => *i,
        OwnedValue::Real(r) => num_to_i64(Num::Real(*r)),
        OwnedValue::Text(t) | OwnedValue::Blob(t) => atoi_prefix(t.as_slice()),
    }
}

/// `round(r, n)` as the pin computes it (v0.16), for `n` in
/// 0..=30: a value beyond ±2^52 has no fraction and stays; with
/// `n` 0 it is `(double)(i64)(r ± 0.5)`; otherwise the pin prints
/// `%!.*f` (the decimal expansion rounded half away from zero at
/// `n` places, or at its 26th significant digit when that comes
/// first) and reads the text back. Here the expansion is exact:
/// `|r| * 10^n` as a 192-bit integer of 32-bit limbs.
fn round_real(r: f64, n: i64) -> f64 {
    const LIM: f64 = 4_503_599_627_370_496.0;
    if r.is_nan() || !(-LIM..=LIM).contains(&r) {
        return r;
    }
    if n <= 0 {
        return (if r < 0.0 { r - 0.5 } else { r + 0.5 }) as i64 as f64;
    }
    let bits = r.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if exp == 0 {
        (frac, -1074i64)
    } else {
        (frac | (1u64 << 52), exp - 1075)
    };
    // An integral value (e >= 0) prints and reads back unchanged.
    let Ok(k) = u32::try_from(-e) else {
        return r;
    };
    if k == 0 {
        return r;
    }
    // x = m * 10^n + 2^(k-1) < 2^154: six 32-bit limbs.
    if k >= 154 {
        // x < 2^153 <= 2^(k-1): rounds to 0.
        return if r < 0.0 { -0.0 } else { 0.0 };
    }
    let mut x = [m as u32, (m >> 32) as u32, 0, 0, 0, 0];
    for _ in 0..n {
        let mut carry = 0u64;
        for limb in x.iter_mut() {
            let t = u64::from(*limb) * 10 + carry;
            *limb = t as u32;
            carry = t >> 32;
        }
    }
    let mut carry = 1u64 << ((k - 1) % 32);
    for limb in x.iter_mut().skip(((k - 1) / 32) as usize) {
        let t = u64::from(*limb) + carry;
        *limb = t as u32;
        carry = t >> 32;
    }
    // D = x >> k, the value rounded half away from zero at n places.
    let (lo, hi) = ((k / 32) as usize, k % 32);
    let mut d = [0u32; 6];
    for (i, slot) in d.iter_mut().enumerate() {
        let a = u64::from(x.get(i + lo).copied().unwrap_or(0));
        let b = u64::from(x.get(i + lo + 1).copied().unwrap_or(0));
        *slot = (((b << 32) | a) >> hi) as u32;
    }
    // D's decimal digits, least significant first; more than 26
    // means the pin rounds at its 26th significant digit, which
    // reads back as `r` itself.
    let mut digits = [0u8; 27];
    let mut nd = 0usize;
    while d.iter().any(|l| *l != 0) || nd == 0 {
        let mut rem = 0u64;
        for limb in d.iter_mut().rev() {
            let cur = (rem << 32) | u64::from(*limb);
            *limb = (cur / 10) as u32;
            rem = cur % 10;
        }
        let Some(slot) = digits.get_mut(nd) else {
            return r;
        };
        *slot = b'0' + rem as u8;
        nd += 1;
    }
    // "[-]<digits>e-<n>", read back as the pin's AtoF reads it.
    let mut buf = [0u8; 32];
    let mut len = 0usize;
    let mut push = |c: u8| {
        if let Some(slot) = buf.get_mut(len) {
            *slot = c;
            len += 1;
        }
    };
    if r < 0.0 {
        push(b'-');
    }
    for c in digits.iter().take(nd).rev() {
        push(*c);
    }
    push(b'e');
    push(b'-');
    if n >= 10 {
        push(b'0' + (n / 10) as u8);
    }
    push(b'0' + (n % 10) as u8);
    buf.get(..len)
        .and_then(crate::sql::lexer::parse_real_signed)
        .unwrap_or(r)
}

/// Truncating conversion to i64 with SQLite's clamping at the
/// i64 bounds (CAST semantics for out-of-range reals).
pub fn num_to_i64(n: Num) -> i64 {
    match n {
        Num::Int(i) => i,
        Num::Real(r) => {
            const TWO_63: f64 = 9_223_372_036_854_775_808.0;
            if r.is_nan() {
                0
            } else if r >= TWO_63 {
                i64::MAX
            } else if r < -TWO_63 {
                i64::MIN
            } else {
                r as i64
            }
        }
    }
}

/// Render a value as text (SQLite's text conversion): used by
/// `||`, group_concat, and the text-valued scalar functions.
pub fn to_text(v: &OwnedValue) -> Option<AVec<u8>> {
    match v {
        OwnedValue::Null => None,
        OwnedValue::Integer(i) => {
            let mut t = AVec::new();
            render_int(*i, &mut t);
            Some(t)
        }
        OwnedValue::Real(r) => {
            let mut t = AVec::new();
            render_real(*r, &mut t);
            Some(t)
        }
        OwnedValue::Text(t) => Some(t.clone()),
        OwnedValue::Blob(t) => Some(t.clone()),
    }
}

/// Three-valued truth: None is NULL (unknown).
pub fn truthy(v: &OwnedValue) -> Option<bool> {
    match v {
        OwnedValue::Null => None,
        OwnedValue::Integer(i) => Some(*i != 0),
        OwnedValue::Real(r) => Some(*r != 0.0),
        OwnedValue::Text(t) | OwnedValue::Blob(t) => match parse_num_prefix(t.as_slice()) {
            Some(Num::Int(i)) => Some(i != 0),
            Some(Num::Real(r)) => Some(r != 0.0),
            None => Some(false),
        },
    }
}

fn bool_val(b: bool) -> OwnedValue {
    OwnedValue::Integer(if b { 1 } else { 0 })
}

/// CAST(x AS <affinity type>) — SQLite's dedicated conversion
/// rules (prefix parses for text, clamping for reals), which
/// differ from INSERT affinity's whole-string rule.
pub fn cast_value(v: &OwnedValue, aff: Affinity) -> OwnedValue {
    match aff {
        Affinity::Blob => match v {
            OwnedValue::Text(t) => OwnedValue::Blob(t.clone()),
            OwnedValue::Integer(_) | OwnedValue::Real(_) => match to_text(v) {
                Some(t) => OwnedValue::Blob(t),
                None => OwnedValue::Null,
            },
            other => other.clone(),
        },
        Affinity::Text => match v {
            OwnedValue::Blob(t) => OwnedValue::Text(t.clone()),
            OwnedValue::Integer(_) | OwnedValue::Real(_) => match to_text(v) {
                Some(t) => OwnedValue::Text(t),
                None => OwnedValue::Null,
            },
            other => other.clone(),
        },
        Affinity::Integer => match v {
            OwnedValue::Null => OwnedValue::Null,
            OwnedValue::Integer(i) => OwnedValue::Integer(*i),
            OwnedValue::Real(r) => OwnedValue::Integer(num_to_i64(Num::Real(*r))),
            // v0.16 fix: the integer prefix only, as the pin's
            // `sqlite3Atoi64` (CAST('1e18' AS INTEGER) is 1).
            OwnedValue::Text(t) | OwnedValue::Blob(t) => {
                OwnedValue::Integer(atoi_prefix(t.as_slice()))
            }
        },
        Affinity::Real => match v {
            OwnedValue::Null => OwnedValue::Null,
            OwnedValue::Integer(i) => OwnedValue::Real(*i as f64),
            OwnedValue::Real(r) => OwnedValue::Real(*r),
            OwnedValue::Text(t) | OwnedValue::Blob(t) => {
                OwnedValue::Real(match parse_num_prefix(t.as_slice()) {
                    Some(n) => n.as_f64(),
                    None => 0.0,
                })
            }
        },
        Affinity::Numeric => match v {
            OwnedValue::Null => OwnedValue::Null,
            OwnedValue::Integer(_) | OwnedValue::Real(_) => v.clone(),
            // The pin's `sqlite3VdbeMemNumerify` (v0.16 fix): an
            // integer-shaped prefix that fits i64 is that
            // INTEGER; any other number is an INTEGER only when it
            // is integral and |x| < 2^51 (`sqlite3RealSameAsInt`),
            // else a REAL — CAST('9223372036854775808' AS NUMERIC)
            // and CAST('1e18' AS NUMERIC) are REALs.
            OwnedValue::Text(t) | OwnedValue::Blob(t) => match parse_num_prefix(t.as_slice()) {
                Some(Num::Int(i)) => OwnedValue::Integer(i),
                Some(Num::Real(r)) => {
                    const TWO_51: f64 = 2_251_799_813_685_248.0;
                    let i = num_to_i64(Num::Real(r));
                    if (-TWO_51..TWO_51).contains(&r) && (i as f64) == r {
                        OwnedValue::Integer(i)
                    } else {
                        OwnedValue::Real(r)
                    }
                }
                None => OwnedValue::Integer(0),
            },
        },
    }
}

// ---------------- evaluation ----------------

/// Evaluation context: a single row, or an aggregate group (bare
/// columns read the representative row; `Agg` nodes fold the
/// group's rows).
pub enum Ctx<'x> {
    Row(&'x RowData),
    Group {
        rows: &'x [RowData],
        rep: &'x RowData,
    },
}

impl<'x> Ctx<'x> {
    fn row(&self) -> &'x RowData {
        match self {
            Ctx::Row(r) => r,
            Ctx::Group { rep, .. } => rep,
        }
    }
    /// A row context over the same representative/current row, for
    /// evaluating aggregate arguments per group row.
    fn as_row(r: &'x RowData) -> Ctx<'x> {
        Ctx::Row(r)
    }
}

/// The comparison-affinity class of one operand (v0.8; see the
/// module docs). `Blob` is a real class of its own — a column
/// declared without a type does *not* count as "no affinity"
/// for the TEXT rule (oracle-probed: `text_col = blob_col`
/// converts nothing, while `text_col = 1` converts the literal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpClass {
    Numeric,
    Text,
    Blob,
    None,
}

fn class_of_affinity(aff: Affinity) -> CmpClass {
    match aff {
        Affinity::Integer | Affinity::Real | Affinity::Numeric => CmpClass::Numeric,
        Affinity::Text => CmpClass::Text,
        Affinity::Blob => CmpClass::Blob,
    }
}

fn cmp_class(e: &RExpr) -> CmpClass {
    match e {
        RExpr::Column(_, aff, _) | RExpr::Cast(_, aff) | RExpr::AffLiteral(_, aff) => {
            class_of_affinity(*aff)
        }
        // COLLATE is transparent to affinity (the pin's EP_Skip).
        RExpr::Collate(x, _) => cmp_class(x),
        // A rowid reference behaves as an INTEGER-affinity
        // column (oracle-probed: `rowid = '1'` matches rowid 1).
        RExpr::Rowid(_) => CmpClass::Numeric,
        _ => CmpClass::None,
    }
}

/// SQLite's comparison-affinity adjustment for `=`-family
/// operators (v0.8, generalised from the v0.4 one-column rule):
/// NUMERIC-class on exactly one side applies NUMERIC affinity
/// to the other side's value; TEXT-class against no affinity
/// applies TEXT affinity to the no-affinity side; every other
/// pairing converts nothing.
fn adjust_cmp(l: &RExpr, r: &RExpr, lv: OwnedValue, rv: OwnedValue) -> (OwnedValue, OwnedValue) {
    let (lc, rc) = (cmp_class(l), cmp_class(r));
    if lc == CmpClass::Numeric && rc != CmpClass::Numeric {
        return (lv, apply_cmp_affinity(rv, Affinity::Numeric));
    }
    if rc == CmpClass::Numeric && lc != CmpClass::Numeric {
        return (apply_cmp_affinity(lv, Affinity::Numeric), rv);
    }
    if lc == CmpClass::Text && rc == CmpClass::None {
        return (lv, apply_cmp_affinity(rv, Affinity::Text));
    }
    if rc == CmpClass::Text && lc == CmpClass::None {
        return (apply_cmp_affinity(lv, Affinity::Text), rv);
    }
    (lv, rv)
}

/// The affinity conversion of a written `IN (list)` test
/// (v0.16): the left operand's comparison affinity applies to
/// both values; a left operand without one converts nothing.
fn adjust_in(x: &RExpr, xv: OwnedValue, iv: OwnedValue) -> (OwnedValue, OwnedValue) {
    let aff = match cmp_class(x) {
        CmpClass::Numeric => Affinity::Numeric,
        CmpClass::Text => Affinity::Text,
        CmpClass::Blob | CmpClass::None => return (xv, iv),
    };
    (apply_cmp_affinity(xv, aff), apply_cmp_affinity(iv, aff))
}

fn apply_cmp_affinity(v: OwnedValue, aff: Affinity) -> OwnedValue {
    match aff {
        Affinity::Integer | Affinity::Real | Affinity::Numeric => match &v {
            OwnedValue::Text(_) | OwnedValue::Blob(_) => apply_affinity(v, Affinity::Numeric),
            _ => v,
        },
        Affinity::Text => match &v {
            OwnedValue::Integer(_) | OwnedValue::Real(_) => apply_affinity(v, Affinity::Text),
            _ => v,
        },
        Affinity::Blob => v,
    }
}

/// NULL-safe equality used by IS, CASE's base form, DISTINCT and
/// GROUP BY key matching (no affinity adjustment; callers that
/// need it adjust first).
pub fn values_equal_is(a: &OwnedValue, b: &OwnedValue) -> bool {
    compare_values(&a.as_value(), &b.as_value()) == core::cmp::Ordering::Equal
}

/// `values_equal_is` under a collating sequence (v0.16).
pub fn values_equal_coll(a: &OwnedValue, b: &OwnedValue, c: Coll) -> bool {
    compare_coll(&a.as_value(), &b.as_value(), c) == core::cmp::Ordering::Equal
}

// ---------------- collating sequences (v0.16) ----------------

/// Whether an explicit `COLLATE` occurs in `e` outside any
/// subquery (the pin's `EP_Collate` flag, which every parent
/// node inherits from its operands and arguments).
pub fn has_explicit_coll(e: &RExpr) -> bool {
    let f = has_explicit_coll;
    match e {
        RExpr::Collate(..) => true,
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => false,
        RExpr::InSub(x, ..)
        | RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _) => f(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) || f(b),
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
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } => args.iter().any(f),
    }
}

/// The operand an explicit collation is inherited from (the
/// pin's `sqlite3ExprCollSeq` descent): the left operand when
/// it carries one, else the first argument / list item that
/// does, else the right operand. LIKE is the pin's
/// `like(pattern, subject)` call, so its pattern comes first.
fn explicit_child(e: &RExpr) -> Option<&RExpr> {
    let f = has_explicit_coll;
    let first = |l: &'_ [RExpr]| -> Option<usize> { l.iter().position(f) };
    match e {
        RExpr::InSub(x, ..)
        | RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _) => Some(x),
        RExpr::Binary(_, a, b) => Some(if f(a) { a } else { b }),
        RExpr::Like(x, p, _) => Some(if f(p) { p } else { x }),
        RExpr::InList(x, l, ..) => {
            if f(x) {
                Some(x)
            } else {
                first(l.as_slice()).and_then(|i| l.as_slice().get(i))
            }
        }
        RExpr::Between(x, a, b, _) => Some(if f(x) {
            x
        } else if f(a) {
            a
        } else {
            b
        }),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base.as_deref().filter(|b| f(b)) {
                return Some(b);
            }
            for (w, t) in whens.iter() {
                if f(w) {
                    return Some(w);
                }
                if f(t) {
                    return Some(t);
                }
            }
            otherwise.as_deref()
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } => {
            first(args.as_slice()).and_then(|i| args.as_slice().get(i))
        }
        _ => None,
    }
}

/// The collating sequence an expression carries (v0.16, the
/// pin's `sqlite3ExprCollSeq`): a column's own (BINARY when it
/// declares none — still a collation, so it beats a literal),
/// an explicit `COLLATE`'s, through CAST, else the one an
/// explicit `COLLATE` inside the expression lends it; `None`
/// for anything else (literals, functions, the rowid, scalar
/// subqueries).
pub fn expr_coll(e: &RExpr) -> Option<Coll> {
    let mut p = e;
    loop {
        match p {
            RExpr::Column(_, _, c) => return *c,
            RExpr::Collate(_, c) => return Some(*c),
            RExpr::Cast(x, _) | RExpr::Pos(x) => p = x,
            _ if has_explicit_coll(p) => p = explicit_child(p)?,
            _ => return None,
        }
    }
}

/// The collation of a binary comparison `l <op> r` (v0.16, the
/// pin's `sqlite3BinaryCompareCollSeq`): an explicit collation
/// on the left, else one on the right, else the left operand's
/// column collation, else the right's, else BINARY.
pub fn binary_coll(l: &RExpr, r: &RExpr) -> Coll {
    if has_explicit_coll(l) {
        expr_coll(l)
    } else if has_explicit_coll(r) {
        expr_coll(r)
    } else {
        expr_coll(l).or_else(|| expr_coll(r))
    }
    .unwrap_or_default()
}

/// The collation a collation-sensitive function or aggregate
/// uses (v0.16; the pin's `SQLITE_FUNC_NEEDCOLL` rule for
/// min / max / nullif): the first argument that has one.
pub fn args_coll(args: &[RExpr]) -> Coll {
    args.iter().find_map(expr_coll).unwrap_or_default()
}

/// Each expression's collation, BINARY when it has none
/// (v0.16): the per-term collations of GROUP BY, DISTINCT and
/// compound result columns.
pub fn exprs_colls(es: &[RExpr]) -> AVec<Coll> {
    let mut out = AVec::new();
    for e in es {
        out.push(expr_coll(e).unwrap_or_default());
    }
    out
}

/// Whether `e` is constant (v0.16): it reads no column, rowid,
/// subquery or aggregate (a VALUES core of constant rows is the
/// pin's co-routine; see `session::core_colls`).
pub fn is_constant(e: &RExpr) -> bool {
    match e {
        RExpr::Literal(_) | RExpr::AffLiteral(..) => true,
        RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::OutRef(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_)
        | RExpr::InSub(..)
        | RExpr::Agg { .. } => false,
        RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => is_constant(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => is_constant(a) && is_constant(b),
        RExpr::InList(x, l, ..) => is_constant(x) && l.iter().all(is_constant),
        RExpr::Between(x, a, b, _) => is_constant(x) && is_constant(a) && is_constant(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(is_constant)
                && whens.iter().all(|(w, t)| is_constant(w) && is_constant(t))
                && otherwise.as_deref().is_none_or(is_constant)
        }
        RExpr::Scalar(_, args) => args.iter().all(is_constant),
    }
}

/// The collation of an IN list's membership test (v0.16): the
/// left operand's alone (explicit `COLLATE`, else its column's),
/// never an item's. CI against the 3.54.1 pin: `'A' IN ('a'
/// COLLATE NOCASE)` is 0 there; the 3.53 stand-in rewrote a single
/// constant item to `x = +item` and gave 1 (the binary rule). That
/// a single item's column collation is ignored too is inferred.
fn in_list_coll(x: &RExpr) -> Coll {
    expr_coll(x).unwrap_or_default()
}

fn cmp_op<E>(op: BinOp, l: &RExpr, r: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let lv = eval::<E>(l, ctx)?;
    let rv = eval::<E>(r, ctx)?;
    let (lv, rv) = adjust_cmp(l, r, lv, rv);
    // v0.16: the comparison's collating sequence (column
    // COLLATE clauses used to be ignored).
    let coll = binary_coll(l, r);
    if matches!(op, BinOp::Is | BinOp::IsNot) {
        let eq = values_equal_coll(&lv, &rv, coll);
        return Ok(bool_val(if op == BinOp::Is { eq } else { !eq }));
    }
    if matches!(lv, OwnedValue::Null) || matches!(rv, OwnedValue::Null) {
        return Ok(OwnedValue::Null);
    }
    let ord = compare_coll(&lv.as_value(), &rv.as_value(), coll);
    use core::cmp::Ordering::*;
    let b = match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(bool_val(b))
}

fn arith<E>(op: BinOp, l: &RExpr, r: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let lv = eval::<E>(l, ctx)?;
    let rv = eval::<E>(r, ctx)?;
    if matches!(op, BinOp::Concat) {
        if matches!(lv, OwnedValue::Null) || matches!(rv, OwnedValue::Null) {
            return Ok(OwnedValue::Null);
        }
        // Always TEXT, blob operands included (the pin's
        // OP_Concat; v0.16 — hipp answered a BLOB).
        let mut out = to_text(&lv).unwrap_or_default();
        out.extend_from_slice(to_text(&rv).unwrap_or_default().as_slice());
        return Ok(OwnedValue::Text(out));
    }
    let (Some(a), Some(b)) = (to_number(&lv), to_number(&rv)) else {
        return Ok(OwnedValue::Null);
    };
    if matches!(op, BinOp::Mod) {
        // The pin's OP_Remainder (v0.16 fix): two INTEGER-classed
        // operands give an INTEGER; otherwise each operand's
        // integer value (`int_value`: a text operand's integer
        // *prefix*) is taken and the result is a REAL.
        if let (Num::Int(x), Num::Int(y)) = (a, b) {
            if y == 0 {
                return Ok(OwnedValue::Null);
            }
            return Ok(OwnedValue::Integer(x.wrapping_rem(y)));
        }
        let (x, y) = (int_value(&lv), int_value(&rv));
        if y == 0 {
            return Ok(OwnedValue::Null);
        }
        return Ok(OwnedValue::Real(x.wrapping_rem(y) as f64));
    }
    if matches!(op, BinOp::Div) {
        return Ok(match (a, b) {
            (Num::Int(x), Num::Int(y)) => {
                if y == 0 {
                    OwnedValue::Null
                } else if x == i64::MIN && y == -1 {
                    OwnedValue::Real(-(x as f64))
                } else {
                    OwnedValue::Integer(x / y)
                }
            }
            (x, y) => {
                let d = y.as_f64();
                if d == 0.0 {
                    OwnedValue::Null
                } else {
                    OwnedValue::Real(x.as_f64() / d)
                }
            }
        });
    }
    // Add / Sub / Mul.
    if let (Num::Int(x), Num::Int(y)) = (a, b) {
        let r = match op {
            BinOp::Add => x.checked_add(y),
            BinOp::Sub => x.checked_sub(y),
            BinOp::Mul => x.checked_mul(y),
            _ => unreachable!(),
        };
        if let Some(v) = r {
            return Ok(OwnedValue::Integer(v));
        }
        // Integer overflow: SQLite computes in real (its rule for
        // +,-,* overflow is an error for +,- in strict contexts;
        // the expression operators yield a real approximation —
        // recorded as a hipp divergence only for +,- overflow).
    }
    let (x, y) = (a.as_f64(), b.as_f64());
    Ok(OwnedValue::Real(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        _ => unreachable!(),
    }))
}

/// Decode text the way the pin's `sqlite3Utf8Read` walks a
/// LIKE / GLOB operand (v0.16): the value ends at its first
/// NUL byte (the pin reads it as a C string); a lead byte >=
/// 0xC0 takes every following continuation byte; overlong
/// forms, surrogates and U+FFFE / U+FFFF read as U+FFFD;
/// stray continuation and ASCII bytes read as themselves.
fn pattern_chars(b: &[u8]) -> AVec<u32> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let b = b.get(..end).unwrap_or(&[]);
    let mut out = AVec::new();
    let mut i = 0;
    while let Some(&c0) = b.get(i) {
        i += 1;
        let mut c = u32::from(c0);
        if c >= 0xc0 {
            // `sqlite3Utf8Trans1`: the lead byte's payload bits.
            c = match c0 {
                0xc0..=0xdf => c - 0xc0,
                0xe0..=0xef => c - 0xe0,
                0xf0..=0xf7 => c - 0xf0,
                0xf8..=0xfb => c - 0xf8,
                0xfc..=0xfd => c - 0xfc,
                _ => 0,
            };
            while let Some(&n) = b.get(i) {
                if n & 0xc0 != 0x80 {
                    break;
                }
                c = c.wrapping_shl(6) | u32::from(n & 0x3f);
                i += 1;
            }
            if c < 0x80 || (c & 0xFFFF_F800) == 0xD800 || (c & 0xFFFF_FFFE) == 0xFFFE {
                c = 0xFFFD;
            }
        }
        out.push(c);
    }
    out
}

/// The pin's `compareInfo` (v0.16): the wildcard characters
/// (0 = disabled), the escape / class character, and whether
/// ASCII letters match case-insensitively.
#[derive(Clone, Copy)]
struct PatInfo {
    all: u32,
    one: u32,
    /// LIKE's ESCAPE character, GLOB's `[` (0: none).
    other: u32,
    /// `other` opens a character class (GLOB) rather than
    /// escaping the next character (LIKE).
    set: bool,
    nocase: bool,
}

fn fold(c: u32) -> u32 {
    if (u32::from(b'A')..=u32::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// Whether the single-character pattern element at `pat[p]`
/// matches `ch`; `Some(len)` gives the element's length in
/// pattern characters. A lone trailing escape and an
/// unterminated class never match (the pin's NOMATCH).
fn element_match(pat: &[u32], p: usize, ch: Option<u32>, info: PatInfo) -> Option<usize> {
    let c = *pat.get(p)?;
    let ch = ch?;
    if info.other != 0 && c == info.other {
        if !info.set {
            let lit = *pat.get(p + 1)?;
            let ok = lit == ch || (info.nocase && lit < 0x80 && ch < 0x80 && fold(lit) == fold(ch));
            return ok.then_some(2);
        }
        // A GLOB class `[...]`: `^` inverts, a leading `]` is a
        // member, `a-z` is a range unless the `-` is last.
        let at = |q: usize| pat.get(q).copied();
        let mut q = p + 1;
        let (mut seen, mut invert, mut prior) = (false, false, 0u32);
        let mut c2 = at(q);
        q += 1;
        if c2 == Some(u32::from(b'^')) {
            invert = true;
            c2 = at(q);
            q += 1;
        }
        if c2 == Some(u32::from(b']')) {
            seen = ch == u32::from(b']');
            c2 = at(q);
            q += 1;
        }
        while let Some(x) = c2 {
            if x == u32::from(b']') {
                break;
            }
            if x == u32::from(b'-') && at(q).is_some_and(|n| n != u32::from(b']')) && prior > 0 {
                let hi = at(q).unwrap_or(0);
                q += 1;
                if ch >= prior && ch <= hi {
                    seen = true;
                }
                prior = 0;
            } else {
                if ch == x {
                    seen = true;
                }
                prior = x;
            }
            c2 = at(q);
            q += 1;
        }
        return (c2.is_some() && seen != invert).then_some(q - p);
    }
    if c == ch || (info.nocase && c < 0x80 && ch < 0x80 && fold(c) == fold(ch)) {
        return Some(1);
    }
    (info.one != 0 && c == info.one).then_some(1)
}

/// The pin's `patternCompare` (v0.16; v0.4 had a recursive
/// LIKE-only matcher that read invalid UTF-8 as empty and ran
/// in exponential time). Every pattern element but the
/// all-wildcard matches exactly one character, so the classic
/// single-backtrack-point scan decides exactly what the pin's
/// recursive search decides, iteratively and in O(n*m).
fn pattern_match(pat: &[u32], s: &[u32], info: PatInfo) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let mut back: Option<(usize, usize)> = None;
    loop {
        if let Some(&c) = pat.get(p) {
            if info.all != 0 && c == info.all {
                p += 1;
                back = Some((p, i));
                continue;
            }
            if let Some(n) = element_match(pat, p, s.get(i).copied(), info) {
                p += n;
                i += 1;
                continue;
            }
        } else if i >= s.len() {
            return true;
        }
        match back {
            Some((bp, bi)) if bi < s.len() => {
                back = Some((bp, bi + 1));
                p = bp;
                i = bi + 1;
            }
            _ => return false,
        }
    }
}

/// The pin's `likeFunc` (v0.16): `like(pattern, subject[,
/// escape])` / `glob(pattern, subject)`, and the LIKE / GLOB
/// operators built on them. A pattern over 50,000 bytes is
/// `PatternTooComplex`; a NULL escape gives NULL, one that is
/// not exactly one character `EscapeNotSingleChar` (both
/// checked before the NULL operands); a NULL pattern or subject
/// gives NULL. LIKE folds ASCII letters only (`'ä' LIKE 'Ä'`
/// is 0, as on the pin).
pub fn like_glob<E>(
    pat: &OwnedValue,
    subject: &OwnedValue,
    esc: Option<&OwnedValue>,
    glob: bool,
) -> Result<OwnedValue, E> {
    if to_text(pat).map_or(0, |t| t.len()) > 50_000 {
        return Err(Error::Schema {
            kind: SchemaKind::PatternTooComplex,
        });
    }
    let mut info = if glob {
        PatInfo {
            all: u32::from(b'*'),
            one: u32::from(b'?'),
            other: u32::from(b'['),
            set: true,
            nocase: false,
        }
    } else {
        PatInfo {
            all: u32::from(b'%'),
            one: u32::from(b'_'),
            other: 0,
            set: false,
            nocase: true,
        }
    };
    if let Some(e) = esc {
        let Some(t) = to_text(e) else {
            return Ok(OwnedValue::Null);
        };
        let chars = pattern_chars(t.as_slice());
        let [c] = chars.as_slice() else {
            return Err(Error::Schema {
                kind: SchemaKind::EscapeNotSingleChar,
            });
        };
        info.other = *c;
        if *c == info.all {
            info.all = 0;
        }
        if *c == info.one {
            info.one = 0;
        }
    }
    let (Some(p), Some(x)) = (to_text(pat), to_text(subject)) else {
        return Ok(OwnedValue::Null);
    };
    let (p, x) = (pattern_chars(p.as_slice()), pattern_chars(x.as_slice()));
    Ok(bool_val(pattern_match(p.as_slice(), x.as_slice(), info)))
}

/// Number of Unicode characters in a text value (blob length is
/// the byte length; SQLite's length() rule).
fn char_len(v: &OwnedValue) -> i64 {
    match v {
        OwnedValue::Text(t) => core::str::from_utf8(t.as_slice())
            .map(|s| s.chars().count())
            .unwrap_or(t.len()) as i64,
        OwnedValue::Blob(t) => t.len() as i64,
        OwnedValue::Integer(_) | OwnedValue::Real(_) => {
            to_text(v).map(|t| t.len()).unwrap_or(0) as i64
        }
        OwnedValue::Null => 0,
    }
}

/// Split text into (byte_range per char) for substr/instr.
fn char_ranges(t: &[u8]) -> AVec<(usize, usize)> {
    let mut out = AVec::new();
    let s = core::str::from_utf8(t).unwrap_or("");
    for (i, c) in s.char_indices() {
        out.push((i, i + c.len_utf8()));
    }
    out
}

fn eval_scalar<E>(f: ScalarFn, args: &[RExpr], ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let mut vs: AVec<OwnedValue> = AVec::new();
    for a in args {
        vs.push(eval::<E>(a, ctx)?);
    }
    let v = vs.as_slice();
    Ok(match f {
        ScalarFn::Coalesce => v
            .iter()
            .find(|x| !matches!(x, OwnedValue::Null))
            .cloned()
            .unwrap_or(OwnedValue::Null),
        ScalarFn::Nullif => {
            let (a, b) = (&v[0], &v[1]);
            if !matches!(a, OwnedValue::Null)
                && !matches!(b, OwnedValue::Null)
                && values_equal_coll(a, b, args_coll(args))
            {
                OwnedValue::Null
            } else {
                a.clone()
            }
        }
        ScalarFn::Typeof => {
            let name: &[u8] = match &v[0] {
                OwnedValue::Null => b"null",
                OwnedValue::Integer(_) => b"integer",
                OwnedValue::Real(_) => b"real",
                OwnedValue::Text(_) => b"text",
                OwnedValue::Blob(_) => b"blob",
            };
            let mut t = AVec::new();
            t.extend_from_slice(name);
            OwnedValue::Text(t)
        }
        // The pin's absFunc (v0.16): an INTEGER stays INTEGER —
        // i64::MIN is "integer overflow" —, a REAL stays REAL, and
        // TEXT / BLOB convert to REAL (`abs('5')` is 5.0).
        ScalarFn::Abs => match &v[0] {
            OwnedValue::Null => OwnedValue::Null,
            OwnedValue::Integer(i) => match i.checked_abs() {
                Some(a) => OwnedValue::Integer(a),
                None => {
                    return Err(Error::Schema {
                        kind: SchemaKind::IntegerOverflow,
                    });
                }
            },
            other => {
                let r = match to_number(other) {
                    Some(Num::Int(i)) => i as f64,
                    Some(Num::Real(r)) => r,
                    None => 0.0,
                };
                OwnedValue::Real(if r < 0.0 { -r } else { r })
            }
        },
        ScalarFn::Length => {
            if matches!(v[0], OwnedValue::Null) {
                OwnedValue::Null
            } else {
                OwnedValue::Integer(char_len(&v[0]))
            }
        }
        ScalarFn::Upper | ScalarFn::Lower => {
            let up = f == ScalarFn::Upper;
            match &v[0] {
                OwnedValue::Null => OwnedValue::Null,
                OwnedValue::Blob(b) => OwnedValue::Blob(b.clone()),
                _ => {
                    let Some(mut t) = to_text(&v[0]) else {
                        return Ok(OwnedValue::Null);
                    };
                    for b in t.as_mut_slice() {
                        if up {
                            b.make_ascii_uppercase();
                        } else {
                            b.make_ascii_lowercase();
                        }
                    }
                    OwnedValue::Text(t)
                }
            }
        }
        ScalarFn::Hex => {
            if matches!(v[0], OwnedValue::Null) {
                OwnedValue::Null
            } else {
                let bytes = to_text(&v[0]).unwrap_or_default();
                let mut out = AVec::new();
                for b in bytes.as_slice() {
                    let (hi, lo) = (b >> 4, b & 0xf);
                    out.push(if hi < 10 { b'0' + hi } else { b'A' + hi - 10 });
                    out.push(if lo < 10 { b'0' + lo } else { b'A' + lo - 10 });
                }
                OwnedValue::Text(out)
            }
        }
        ScalarFn::Sign => match to_number(&v[0]) {
            None => OwnedValue::Null,
            Some(n) => {
                let x = n.as_f64();
                OwnedValue::Integer(if x > 0.0 {
                    1
                } else if x < 0.0 {
                    -1
                } else {
                    0
                })
            }
        },
        ScalarFn::Round => {
            let Some(n) = to_number(&v[0]) else {
                return Ok(OwnedValue::Null);
            };
            // The digit count is read as the pin's
            // `sqlite3_value_int64` reads it (a text's integer
            // prefix: '1e1' is 1), then clamped to 0..=30.
            let digits = match v.get(1) {
                None => 0,
                Some(OwnedValue::Null) => return Ok(OwnedValue::Null),
                Some(d) => int_value(d).clamp(0, 30),
            };
            OwnedValue::Real(round_real(n.as_f64(), digits))
        }
        ScalarFn::Substr => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let Some(text) = to_text(&v[0]) else {
                return Ok(OwnedValue::Null);
            };
            // Start and length are read as the pin's
            // `sqlite3_value_int64` reads them: a text's integer
            // prefix ('1e1' is 1), a real truncated.
            let mut start = int_value(&v[1]);
            let ranges = char_ranges(text.as_slice());
            let n = ranges.len() as i64;
            // Two-argument form: the implicit length is i64::MAX
            // (pinned on 3.54.1 by a CI probe grid — e.g.
            // substr(x, -9223372036854775808) keeps all but the last
            // character, substr(x, -27) on 26 characters keeps all).
            let mut len: i64 = match v.get(2) {
                Some(m) => int_value(m),
                None => i64::MAX,
            };
            // SQLite's substr coordinate rules (verified
            // against the oracle, incl. negative lengths: a
            // negative length counts back from the start point),
            // saturating at the i64 bounds.
            if start < 0 {
                start = start.saturating_add(n);
                if start < 0 {
                    len = len.saturating_add(start).max(0);
                    start = 0;
                }
            } else if start > 0 {
                start -= 1;
            } else if len > 0 && v.len() > 2 {
                len -= 1;
            }
            if len < 0 {
                len = len.saturating_neg();
                start = start.saturating_sub(len);
                if start < 0 {
                    len = len.saturating_add(start).max(0);
                    start = 0;
                }
            }
            let end = start.saturating_add(len).min(n);
            let mut out = AVec::new();
            for (i, (bs, be)) in ranges.iter().enumerate() {
                let idx = i as i64;
                if idx >= start
                    && idx < end
                    && let Some(piece) = text.as_slice().get(*bs..*be)
                {
                    out.extend_from_slice(piece);
                }
            }
            match v[0] {
                OwnedValue::Blob(_) => OwnedValue::Blob(out),
                _ => OwnedValue::Text(out),
            }
        }
        ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let Some(text) = to_text(&v[0]) else {
                return Ok(OwnedValue::Null);
            };
            let chars: AVec<u8> = if v.len() > 1 {
                to_text(&v[1]).unwrap_or_default()
            } else {
                let mut c = AVec::new();
                c.push(b' ');
                c
            };
            let in_set = |b: u8| chars.as_slice().contains(&b);
            let bytes = text.as_slice();
            let (mut s, mut e) = (0usize, bytes.len());
            if f != ScalarFn::Rtrim {
                while s < e && in_set(bytes[s]) {
                    s += 1;
                }
            }
            if f != ScalarFn::Ltrim {
                while e > s && in_set(bytes[e - 1]) {
                    e -= 1;
                }
            }
            let mut out = AVec::new();
            out.extend_from_slice(&bytes[s..e]);
            OwnedValue::Text(out)
        }
        ScalarFn::Replace => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let (Some(x), Some(y), Some(z)) = (to_text(&v[0]), to_text(&v[1]), to_text(&v[2]))
            else {
                return Ok(OwnedValue::Null);
            };
            if y.is_empty() {
                return Ok(OwnedValue::Text(x));
            }
            let mut out = AVec::new();
            let (xb, yb) = (x.as_slice(), y.as_slice());
            let mut i = 0usize;
            while i < xb.len() {
                if i + yb.len() <= xb.len() && &xb[i..i + yb.len()] == yb {
                    out.extend_from_slice(z.as_slice());
                    i += yb.len();
                } else {
                    out.push(xb[i]);
                    i += 1;
                }
            }
            OwnedValue::Text(out)
        }
        ScalarFn::Instr => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let (Some(x), Some(y)) = (to_text(&v[0]), to_text(&v[1])) else {
                return Ok(OwnedValue::Null);
            };
            if y.is_empty() {
                return Ok(OwnedValue::Integer(1));
            }
            let (xb, yb) = (x.as_slice(), y.as_slice());
            let mut pos = 0i64;
            let mut i = 0usize;
            let mut found = 0i64;
            while i < xb.len() {
                if i + yb.len() <= xb.len() && &xb[i..i + yb.len()] == yb {
                    found = pos + 1;
                    break;
                }
                // Advance by one character.
                let step = utf8_len(xb[i]);
                i += step;
                pos += 1;
            }
            OwnedValue::Integer(found)
        }
        #[cfg(feature = "v16")]
        ScalarFn::Like | ScalarFn::Glob => {
            like_glob::<E>(&v[0], &v[1], v.get(2), f == ScalarFn::Glob)?
        }
        ScalarFn::Iif => {
            if truthy(&v[0]) == Some(true) {
                v[1].clone()
            } else {
                v[2].clone()
            }
        }
        ScalarFn::MinN | ScalarFn::MaxN => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                OwnedValue::Null
            } else {
                // v0.16: the first argument's collation decides,
                // and a tie keeps the earlier argument for max()
                // but takes the later one for min() — the pin's
                // `minmaxFunc` (`min(1, 1.0)` is 1.0).
                let coll = args_coll(args);
                let mut best = v[0].clone();
                for x in &v[1..] {
                    let ord = compare_coll(&x.as_value(), &best.as_value(), coll);
                    let take = if f == ScalarFn::MinN {
                        ord != core::cmp::Ordering::Greater
                    } else {
                        ord == core::cmp::Ordering::Greater
                    };
                    if take {
                        best = x.clone();
                    }
                }
                best
            }
        }
    })
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// The number a value contributes to sum/total/avg: like
/// `to_number`, except non-numeric text contributes a *real* 0.0
/// (SQLite: `sum('abc')` is 0.0, not integer 0).
fn sum_number(v: &OwnedValue) -> Num {
    match v {
        OwnedValue::Integer(i) => Num::Int(*i),
        OwnedValue::Real(r) => Num::Real(*r),
        OwnedValue::Text(t) | OwnedValue::Blob(t) => {
            parse_num_prefix(t.as_slice()).unwrap_or(Num::Real(0.0))
        }
        OwnedValue::Null => Num::Int(0),
    }
}

/// Fold one aggregate over a group's rows: evaluate the
/// arguments per row (synchronously) and fold the values.
fn eval_agg<E>(
    kind: AggKind,
    distinct: bool,
    star: bool,
    args: &[RExpr],
    rows: &[RowData],
) -> Result<OwnedValue, E> {
    let mut arg_rows: AVec<AVec<OwnedValue>> = AVec::new();
    for r in rows {
        let mut vs: AVec<OwnedValue> = AVec::new();
        if !star {
            for a in args {
                vs.push(eval::<E>(a, &Ctx::as_row(r))?);
            }
        }
        arg_rows.push(vs);
    }
    eval_agg_values::<E>(kind, distinct, star, arg_rows.as_slice(), args_coll(args))
}

/// Fold one aggregate over precomputed per-row argument
/// values (v0.11): `arg_rows` holds, per group row, the values
/// of the aggregate's arguments for that row (empty for
/// `count(*)`). This is the single folding implementation:
/// the synchronous evaluator reaches it through `eval_agg`,
/// and the subquery executor reaches it with argument values
/// whose subqueries were executed per group row.
pub fn eval_agg_values<E>(
    kind: AggKind,
    distinct: bool,
    star: bool,
    arg_rows: &[AVec<OwnedValue>],
    coll: Coll,
) -> Result<OwnedValue, E> {
    match kind {
        AggKind::Count => {
            if star {
                return Ok(OwnedValue::Integer(arg_rows.len() as i64));
            }
            let mut n = 0i64;
            let mut seen: AVec<OwnedValue> = AVec::new();
            for vs in arg_rows {
                let val = &vs.as_slice()[0];
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_coll(s, val, coll)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                n += 1;
            }
            Ok(OwnedValue::Integer(n))
        }
        AggKind::Sum | AggKind::Total | AggKind::Avg => {
            let mut int_acc: i64 = 0;
            let mut int_ok = true;
            let mut real_acc = 0.0f64;
            let mut count = 0i64;
            let mut seen: AVec<OwnedValue> = AVec::new();
            for vs in arg_rows {
                let val = &vs.as_slice()[0];
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_coll(s, val, coll)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                count += 1;
                match sum_number(val) {
                    Num::Int(i) => {
                        if int_ok {
                            match int_acc.checked_add(i) {
                                Some(s2) => int_acc = s2,
                                None => {
                                    int_ok = false;
                                    real_acc = int_acc as f64 + i as f64;
                                }
                            }
                        } else {
                            real_acc += i as f64;
                        }
                    }
                    Num::Real(x) => {
                        if int_ok {
                            int_ok = false;
                            real_acc = int_acc as f64 + x;
                        } else {
                            real_acc += x;
                        }
                    }
                }
            }
            if count == 0 {
                return Ok(match kind {
                    AggKind::Total => OwnedValue::Real(0.0),
                    _ => OwnedValue::Null,
                });
            }
            Ok(match kind {
                AggKind::Sum => {
                    if int_ok {
                        OwnedValue::Integer(int_acc)
                    } else {
                        OwnedValue::Real(real_acc)
                    }
                }
                AggKind::Total => OwnedValue::Real(if int_ok { int_acc as f64 } else { real_acc }),
                AggKind::Avg => OwnedValue::Real(
                    (if int_ok { int_acc as f64 } else { real_acc }) / count as f64,
                ),
                _ => unreachable!(),
            })
        }
        AggKind::Min | AggKind::Max => {
            let mut best: Option<OwnedValue> = None;
            let mut seen: AVec<OwnedValue> = AVec::new();
            for vs in arg_rows {
                let val = vs.as_slice()[0].clone();
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_coll(s, &val, coll)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                best = Some(match best {
                    None => val,
                    Some(b) => {
                        let ord = compare_coll(&val.as_value(), &b.as_value(), coll);
                        let take = if kind == AggKind::Min {
                            ord == core::cmp::Ordering::Less
                        } else {
                            ord == core::cmp::Ordering::Greater
                        };
                        if take { val } else { b }
                    }
                });
            }
            Ok(best.unwrap_or(OwnedValue::Null))
        }
        AggKind::GroupConcat => {
            let mut out = AVec::new();
            let mut any = false;
            let mut seen: AVec<OwnedValue> = AVec::new();
            for vs in arg_rows {
                let val = &vs.as_slice()[0];
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_coll(s, val, coll)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                let sep = if vs.len() > 1 {
                    match &vs.as_slice()[1] {
                        OwnedValue::Null => return Ok(OwnedValue::Null),
                        s => to_text(s).unwrap_or_default(),
                    }
                } else {
                    let mut c = AVec::new();
                    c.push(b',');
                    c
                };
                if any {
                    out.extend_from_slice(sep.as_slice());
                }
                out.extend_from_slice(to_text(val).unwrap_or_default().as_slice());
                any = true;
            }
            if !any {
                return Ok(OwnedValue::Null);
            }
            Ok(OwnedValue::Text(out))
        }
    }
}

/// Evaluate a resolved expression.
pub fn eval<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    Ok(match e {
        RExpr::Literal(v) => v.clone(),
        RExpr::AffLiteral(v, _) => v.clone(),
        RExpr::Column(i, ..) => ctx
            .row()
            .vals
            .as_slice()
            .get(*i)
            .cloned()
            .unwrap_or(OwnedValue::Null),
        RExpr::Rowid(s) => match ctx.row().rowids.as_slice().get(*s) {
            Some(Some(r)) => OwnedValue::Integer(*r),
            _ => OwnedValue::Null,
        },
        RExpr::OutRef(_) => return Err(Error::Unsupported),
        RExpr::Pos(x) => eval::<E>(x, ctx)?,
        RExpr::Neg(x) => match to_number(&eval::<E>(x, ctx)?) {
            None => OwnedValue::Null,
            Some(Num::Int(i)) => match i.checked_neg() {
                Some(n) => OwnedValue::Integer(n),
                None => OwnedValue::Real(-(i as f64)),
            },
            Some(Num::Real(r)) => OwnedValue::Real(-r),
        },
        RExpr::Not(x) => match truthy(&eval::<E>(x, ctx)?) {
            None => OwnedValue::Null,
            Some(b) => bool_val(!b),
        },
        RExpr::IsNull(x, neg) => {
            let is = matches!(eval::<E>(x, ctx)?, OwnedValue::Null);
            bool_val(is != *neg)
        }
        RExpr::Binary(op, l, r) => match op {
            BinOp::And => {
                let a = truthy(&eval::<E>(l, ctx)?);
                let b = truthy(&eval::<E>(r, ctx)?);
                match (a, b) {
                    (Some(false), _) | (_, Some(false)) => bool_val(false),
                    (Some(true), Some(true)) => bool_val(true),
                    _ => OwnedValue::Null,
                }
            }
            BinOp::Or => {
                let a = truthy(&eval::<E>(l, ctx)?);
                let b = truthy(&eval::<E>(r, ctx)?);
                match (a, b) {
                    (Some(true), _) | (_, Some(true)) => bool_val(true),
                    (Some(false), Some(false)) => bool_val(false),
                    _ => OwnedValue::Null,
                }
            }
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Is
            | BinOp::IsNot => cmp_op::<E>(*op, l, r, ctx)?,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Concat => {
                arith::<E>(*op, l, r, ctx)?
            }
        },
        // An empty set is FALSE for IN (TRUE for NOT IN) even
        // when the left operand is NULL (v0.16, pin-probed:
        // `NULL IN (SELECT a FROM empty)` is 0, not NULL — the
        // shape `x IN emptytable` exposed it).
        RExpr::InList(_, list, neg, _) if list.is_empty() => bool_val(*neg),
        RExpr::InList(x, list, neg, from_sub) => {
            let xv = eval::<E>(x, ctx)?;
            let mut any_null = matches!(xv, OwnedValue::Null);
            let mut hit = false;
            let coll = in_list_coll(x);
            for item in list.iter() {
                let iv = eval::<E>(item, ctx)?;
                // An IN (SELECT ...) substitution compares like
                // `=` (the item carries the subquery column's
                // affinity). A written list converts both sides
                // by the LEFT operand's affinity only, as the
                // pin's OP_Eq / ephemeral-index path does: an
                // operand without affinity converts nothing,
                // whatever the items declare (v0.16, probed:
                // with d no affinity, e INTEGER, f TEXT holding
                // '1'/1/'1', d IN (e) and 1 IN (f) are 0).
                let (a, b) = if *from_sub {
                    adjust_cmp(x, item, xv.clone(), iv)
                } else {
                    adjust_in(x, xv.clone(), iv)
                };
                if matches!(a, OwnedValue::Null) || matches!(b, OwnedValue::Null) {
                    any_null = true;
                    continue;
                }
                if compare_coll(&a.as_value(), &b.as_value(), coll) == core::cmp::Ordering::Equal {
                    hit = true;
                    break;
                }
            }
            if hit {
                bool_val(!*neg)
            } else if any_null {
                OwnedValue::Null
            } else {
                bool_val(*neg)
            }
        }
        RExpr::Between(x, lo, hi, neg) => {
            let ge = cmp_op::<E>(BinOp::Ge, x, lo, ctx)?;
            let le = cmp_op::<E>(BinOp::Le, x, hi, ctx)?;
            let r = match (truthy(&ge), truthy(&le)) {
                (Some(false), _) | (_, Some(false)) => bool_val(false),
                (Some(true), Some(true)) => bool_val(true),
                _ => OwnedValue::Null,
            };
            if *neg {
                match r {
                    OwnedValue::Integer(1) => bool_val(false),
                    OwnedValue::Integer(0) => bool_val(true),
                    other => other,
                }
            } else {
                r
            }
        }
        RExpr::Like(x, p, neg) => {
            let xv = eval::<E>(x, ctx)?;
            let pv = eval::<E>(p, ctx)?;
            match like_glob::<E>(&pv, &xv, None, false)? {
                OwnedValue::Integer(m) => bool_val((m != 0) != *neg),
                other => other,
            }
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            let mut out = OwnedValue::Null;
            let mut done = false;
            match base {
                Some(b) => {
                    let bv = eval::<E>(b, ctx)?;
                    for (w, t) in whens.iter() {
                        let wv = eval::<E>(w, ctx)?;
                        // The base form compares like `=`:
                        // comparison affinity applies between
                        // the base and each WHEN operand (v0.8;
                        // oracle-probed `CASE int_col WHEN
                        // text_col`). NULLIF is the exception —
                        // it compares binary, no affinity — and
                        // it does not come through here.
                        let (bv2, wv2) = adjust_cmp(b, w, bv.clone(), wv);
                        if !matches!(bv2, OwnedValue::Null)
                            && !matches!(wv2, OwnedValue::Null)
                            && values_equal_coll(&bv2, &wv2, binary_coll(b, w))
                        {
                            out = eval::<E>(t, ctx)?;
                            done = true;
                            break;
                        }
                    }
                }
                None => {
                    for (w, t) in whens.iter() {
                        if truthy(&eval::<E>(w, ctx)?) == Some(true) {
                            out = eval::<E>(t, ctx)?;
                            done = true;
                            break;
                        }
                    }
                }
            }
            if !done && let Some(o) = otherwise {
                out = eval::<E>(o, ctx)?;
            }
            out
        }
        RExpr::Scalar(f, args) => eval_scalar::<E>(*f, args.as_slice(), ctx)?,
        RExpr::Cast(x, aff) => cast_value(&eval::<E>(x, ctx)?, *aff),
        RExpr::Collate(x, _) => eval::<E>(x, ctx)?,
        RExpr::Agg {
            kind,
            distinct,
            star,
            args,
        } => match ctx {
            Ctx::Group { rows, .. } => {
                eval_agg::<E>(*kind, *distinct, *star, args.as_slice(), rows)?
            }
            Ctx::Row(_) => return Err(Error::Unsupported),
        },
        // Subquery nodes are computed by the executor and
        // substituted away before evaluation (v0.6); reaching
        // one here means a path forgot to substitute.
        RExpr::SubScalar(_) | RExpr::SubExists(_) | RExpr::InSub(..) => {
            return Err(Error::Unsupported);
        }
    })
}

/// Evaluate an expression that must be constant (INSERT VALUES):
/// resolve against zero sources (any column reference fails) and
/// evaluate over an empty row.
pub fn eval_const<E>(expr: &Expr, arena: &Arena) -> Result<OwnedValue, E> {
    let sources: [SourceDesc; 0] = [];
    let offsets: [usize; 0] = [];
    let r = resolve::<E>(expr, arena, &sources, &offsets)?;
    if has_agg(&r) {
        return Err(Error::Unsupported);
    }
    let empty = RowData::empty();
    eval::<E>(&r, &Ctx::Row(&empty))
}

/// Compare two ORDER BY key values under one term's direction
/// (v0.16): NULLs go first or last as the term says (SQLite's
/// default is NULL-smallest, i.e. first ascending and last
/// descending); any other pair compares by the storage-class
/// ordering, reversed for DESC.
pub fn order_cmp(a: &OwnedValue, b: &OwnedValue, o: SortOrder) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (matches!(a, OwnedValue::Null), matches!(b, OwnedValue::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if o.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if o.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let c = compare_coll(&a.as_value(), &b.as_value(), o.coll);
            if o.desc { c.reverse() } else { c }
        }
    }
}

/// Lexicographic ORDER BY comparison of two key rows (v0.16):
/// `order_cmp` per term, first difference wins.
pub fn order_keys_cmp(
    a: &[OwnedValue],
    b: &[OwnedValue],
    dirs: impl Iterator<Item = SortOrder>,
) -> core::cmp::Ordering {
    for ((x, y), o) in a.iter().zip(b.iter()).zip(dirs) {
        let c = order_cmp(x, y, o);
        if c != core::cmp::Ordering::Equal {
            return c;
        }
    }
    core::cmp::Ordering::Equal
}

/// A LIMIT / OFFSET value as the pin's `OP_MustBeInt` takes it
/// (v0.16): NUMERIC affinity first (`'2'`, `' 2 '`, `'2.0'`,
/// `2.0` become 2), then the value must be an integer — NULL,
/// a blob, non-numeric text, or a real with a fractional part
/// or outside the i64 range is a datatype mismatch.
pub fn limit_int<E>(v: OwnedValue) -> Result<i64, E> {
    match apply_affinity(v, Affinity::Numeric) {
        OwnedValue::Integer(i) => Ok(i),
        _ => Err(Error::Schema {
            kind: SchemaKind::DatatypeMismatch,
        }),
    }
}
