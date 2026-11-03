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
    /// `group_concat(X[, SEP])`, and (v0.16, `v16`)
    /// `string_agg(X, SEP)`.
    GroupConcat,
    /// `median(Y)` (v0.16, `v16`): `percentile(Y, 50)`.
    #[cfg(feature = "v16")]
    Median,
    /// `percentile(Y, P)`, P in 0 ..= 100 (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Percentile,
    /// `percentile_cont(Y, P)`, P in 0 ..= 1 (v0.16, `v16`).
    #[cfg(feature = "v16")]
    PercentileCont,
    /// `percentile_disc(Y, P)`, P in 0 ..= 1 (v0.16, `v16`): the
    /// value at or below the interpolation point, no blending.
    #[cfg(feature = "v16")]
    PercentileDisc,
    /// `json_group_array` / `json_group_object` and their `jsonb_`
    /// forms (v0.16, `json`): the arguments are wrapped in the
    /// per-row JSON text (`json::JsonAgg::frag_fn`), which the fold
    /// joins.
    #[cfg(feature = "json")]
    Json(crate::json::JsonAgg),
}

/// How an aggregate's `args` divide when it was written with an
/// aggregate ORDER BY and / or FILTER clause (v0.16, `v16`): the
/// first `nargs` are its arguments, the next `order.len()` its
/// ORDER BY keys (`order` holds their directions and
/// collations), and with `filter` the last one is its FILTER
/// expression. Keeping them in `args` lets every walker (column
/// shifts, subquery collection and substitution, aggregate
/// ownership, nested-aggregate misuse) see them unchanged.
#[cfg(feature = "v16")]
#[derive(Debug, Clone)]
pub struct AggExt {
    pub nargs: usize,
    pub order: AVec<SortOrder>,
    pub filter: bool,
    /// The pin's "ORDER BY is the argument" form of a DISTINCT
    /// aggregate (`group_concat(DISTINCT x ORDER BY x)`): the
    /// values are deduplicated by the sort itself, the *last* of
    /// equal keys surviving, instead of by the first-seen DISTINCT
    /// test.
    pub unique: bool,
}

/// One aggregate node, taken apart (`agg_parts`): its kind and
/// flags, its own arguments, and (v0.16, `v16`) its ORDER BY
/// keys with their directions and its FILTER expression. Each
/// group row contributes the values of `inputs()` (arguments,
/// then keys) when the filter passes.
#[derive(Debug, Clone, Copy)]
pub struct AggParts<'a> {
    pub kind: AggKind,
    pub distinct: bool,
    pub star: bool,
    pub args: &'a [RExpr],
    inputs: &'a [RExpr],
    pub filter: Option<&'a RExpr>,
    pub order: &'a [SortOrder],
    pub unique: bool,
}

impl<'a> AggParts<'a> {
    /// The expressions each stepped row evaluates: the arguments,
    /// then the ORDER BY keys.
    pub fn inputs(&self) -> &'a [RExpr] {
        self.inputs
    }
}

/// Take an aggregate node apart (None for any other node).
pub fn agg_parts(e: &RExpr) -> Option<AggParts<'_>> {
    let RExpr::Agg {
        kind,
        distinct,
        star,
        args,
        #[cfg(feature = "v16")]
        ext,
        ..
    } = e
    else {
        return None;
    };
    let all = args.as_slice();
    #[cfg(feature = "v16")]
    if let Some(x) = ext {
        let n = x.nargs.min(all.len());
        let k = n.saturating_add(x.order.len()).min(all.len());
        return Some(AggParts {
            kind: *kind,
            distinct: *distinct,
            star: *star,
            args: all.get(..n).unwrap_or(&[]),
            inputs: all.get(..k).unwrap_or(&[]),
            filter: if x.filter { all.get(k) } else { None },
            order: x.order.as_slice(),
            unique: x.unique,
        });
    }
    Some(AggParts {
        kind: *kind,
        distinct: *distinct,
        star: *star,
        args: all,
        inputs: all,
        filter: None,
        order: &[],
        unique: false,
    })
}

/// One group row's contribution to an aggregate (`agg_parts`):
/// None when its FILTER does not pass (NULL or false: the row is
/// not stepped, and nothing else of it is evaluated), else the
/// values of `inputs()`. `ev` evaluates an expression against
/// the row.
pub fn agg_row<E>(
    p: &AggParts<'_>,
    ev: &mut dyn FnMut(&RExpr) -> Result<OwnedValue, E>,
) -> Result<Option<AVec<OwnedValue>>, E> {
    if let Some(f) = p.filter
        && truthy(&ev(f)?) != Some(true)
    {
        return Ok(None);
    }
    let mut vs = AVec::new();
    for a in p.inputs() {
        vs.push(ev(a)?);
    }
    Ok(Some(vs))
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
    /// `~x` (v0.16, `v16`): bitwise NOT of the operand's
    /// integer value.
    #[cfg(feature = "v16")]
    BitNot,
    /// A row value's elements (v0.16, `v16`): only ever the left
    /// operand or an item of an `InList` (a row-value IN test),
    /// which evaluates it element by element; on its own it is
    /// `SchemaKind::RowValueMisused`.
    #[cfg(feature = "v16")]
    Vector,
    /// `printf()` / `format()` (v0.16, `v16`): `func::printf`.
    #[cfg(feature = "v16")]
    Printf,
    /// `quote(X)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Quote,
    /// `char(X, ...)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Char,
    /// `unicode(X)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Unicode,
    /// `zeroblob(N)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Zeroblob,
    /// `randomblob(N)` (v0.16, `v16`): reads the connection's
    /// random stream (`Env::prng`).
    #[cfg(feature = "v16")]
    Randomblob,
    /// `random()` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Random,
    /// `last_insert_rowid()` (v0.16, `v16`): `Env::last_rowid`.
    #[cfg(feature = "v16")]
    LastInsertRowid,
    /// `changes()` (v0.16, `v16`): `Env::changes`.
    #[cfg(feature = "v16")]
    Changes,
    /// `total_changes()` (v0.16, `v16`): `Env::total_changes`.
    #[cfg(feature = "v16")]
    TotalChanges,
    /// `likely(X)` / `unlikely(X)` / `likelihood(X, P)` (v0.16,
    /// `v16`): X itself (P is a planner hint, checked at
    /// resolution).
    #[cfg(feature = "v16")]
    Likely,
    /// `concat(X, ...)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Concat,
    /// `concat_ws(SEP, X, ...)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    ConcatWs,
    /// `octet_length(X)` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    OctetLength,
    /// `unhex(X[, Y])` (v0.16, `v16`).
    #[cfg(feature = "v16")]
    Unhex,
    /// `sqlite_version()` (v0.16, `v16`): the claim version,
    /// `'3.54.1'`.
    #[cfg(feature = "v16")]
    SqliteVersion,
    /// The math functions (v0.16, leaf feature `math`):
    /// `crate::math::eval`.
    #[cfg(feature = "math")]
    Math(crate::math::MathFn),
    /// The JSON functions (v0.16, leaf feature `json`):
    /// `crate::json::eval`, over arguments evaluated with their
    /// JSON subtype (`eval_json_arg`).
    #[cfg(feature = "json")]
    Json(crate::json::JsonFn),
    /// `RAISE(kind[, message])` (v0.16, `triggers`): evaluating it
    /// records the message (`Env::raise_msg`) and fails with the
    /// kind's error (`SchemaKind::RaiseIgnore`, ...), which the
    /// trigger engine acts on.
    #[cfg(feature = "triggers")]
    Raise(crate::sql::ast::RaiseKind),
}

impl ScalarFn {
    /// Whether the pin counts the function non-deterministic
    /// (v0.16): refused in generated columns.
    #[cfg(feature = "v16")]
    pub fn nondeterministic(self) -> bool {
        matches!(
            self,
            ScalarFn::Random
                | ScalarFn::Randomblob
                | ScalarFn::LastInsertRowid
                | ScalarFn::Changes
                | ScalarFn::TotalChanges
                | ScalarFn::SqliteVersion
        )
    }
}

/// Whether any function call in `e` (outside subquery plans)
/// satisfies `f` (v0.16).
#[cfg(feature = "v16")]
pub fn any_scalar(e: &RExpr, f: &dyn Fn(ScalarFn) -> bool) -> bool {
    let any = |x: &RExpr| any_scalar(x, f);
    match e {
        RExpr::Scalar(g, args) => f(*g) || args.iter().any(any),
        RExpr::Win(_, args) => args.iter().any(any),
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_) => false,
        RExpr::InSub(x, ..) => any(x),
        RExpr::Agg { args, .. } => args.iter().any(any),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => any(a) || any(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => any(a),
        RExpr::InList(x, l, ..) => any(x) || l.iter().any(any),
        RExpr::Between(x, a, b, _) => any(x) || any(a) || any(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_some_and(any)
                || whens.iter().any(|(w, t)| any(w) || any(t))
                || otherwise.as_deref().is_some_and(any)
        }
    }
}

/// A window call's description (v0.16, `window`; see
/// `RExpr::Win`).
#[cfg(feature = "window")]
pub type WinBox = Box<crate::window::WinCall>;
/// Uninhabited without `window`: `RExpr::Win` cannot be built,
/// and the walkers' arms for it compile away.
#[cfg(not(feature = "window"))]
pub type WinBox = core::convert::Infallible;

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
    ///
    /// v0.16 (`triggers`): a trigger program's `NEW.col` /
    /// `OLD.col` is one too — no affinity (`None`, the pin's
    /// `TK_TRIGGER` carries none for a column) and the column's
    /// collating sequence, which a comparison takes as it takes a
    /// column's; the rowid carries INTEGER affinity and no
    /// collation. Every other producer gives an affinity and no
    /// collation.
    AffLiteral(OwnedValue, Option<Affinity>, Option<Coll>),
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
        /// Which query the aggregate belongs to (v0.16 fix,
        /// ungated): 0 is the query whose expression holds it;
        /// `n` is the query `n` subquery levels out. The pin
        /// gives an aggregate to the innermost query whose FROM
        /// sources its arguments reference (`SELECT (SELECT
        /// max(t.a) FROM u) FROM t` aggregates over t, one row);
        /// arguments referencing no column keep it local. The
        /// owner folds it per group and substitutes the value
        /// into the subquery before executing it (see
        /// `visit_aggs`, `session::eval_group_sub`).
        lvl: u8,
        /// An enclosing query's aggregate in the result list or
        /// ORDER BY of an EXISTS subquery (v0.16 review fix,
        /// `mark_exists_inert`): the pin never evaluates those
        /// terms, so the aggregate is no misuse where its owner
        /// cannot aggregate (`WHERE EXISTS (SELECT max(t.a) FROM
        /// u)`) and does not make the EXISTS subquery an
        /// aggregate one. It still makes its owner an aggregate
        /// query from the owner's result list (`SELECT
        /// EXISTS(SELECT max(t.a) FROM u) FROM t` is one row) —
        /// see `has_result_agg`. Evaluates as NULL.
        inert: bool,
        /// Copied in from an enclosing query's result-column
        /// alias (v0.16 review fix, `alias_from_outer`): the pin
        /// substitutes an alias without re-resolving it, so such
        /// an aggregate is no misuse in a subquery's WHERE
        /// (`HAVING EXISTS (SELECT 1 FROM t x WHERE x.b = s)`
        /// over `sum(b) AS s`), where a written one is.
        alias: bool,
        /// The ORDER BY / FILTER layout of `args` (v0.16, `v16`);
        /// None when the call had neither clause.
        #[cfg(feature = "v16")]
        ext: Option<Box<AggExt>>,
    },
    /// A subquery whose execution failed with an evaluation error
    /// (v0.16 review fix): `substitute` puts it where the
    /// subquery's value would go, and evaluating it raises
    /// `Error::Schema { kind }`. The pin only runs a subquery when
    /// evaluation reaches it, so `coalesce(1, (SELECT
    /// abs(-9223372036854775808)))` is 1 there; the executor
    /// computes subqueries before evaluating, and this keeps
    /// their errors for the moment (if any) they are read.
    /// Produced only by `substitute`; never by the resolver.
    Raise(SchemaKind),
    /// A window function call (v0.16, `window`): what the call is
    /// (function, frame, the layout of the children) and its
    /// children — arguments, FILTER, PARTITION BY and ORDER BY
    /// terms, all evaluated per input row of the window (a row,
    /// or a group of an aggregate query). Every walker sees the
    /// children, so subqueries, correlation and aggregates inside
    /// them behave as anywhere else. Never evaluated directly:
    /// the executor computes the windows over the whole input and
    /// puts each call's value in its place (`window::fill`)
    /// before the result columns evaluate. Without `window` the
    /// variant cannot be built (`WinBox` is uninhabited).
    Win(WinBox, AVec<RExpr>),
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
    /// The source is the main schema table read without an alias
    /// (v0.16): `sqlite_master` and `sqlite_schema` both qualify
    /// its columns, as on the pin.
    pub schema_table: bool,
    /// The table an ALTER TABLE is renaming (v0.16, `v16`): every
    /// reference resolved to this source is recorded in the shared
    /// trace, with whether the source was read under an alias.
    #[cfg(feature = "v16")]
    pub trace: Option<(alloc::rc::Rc<core::cell::RefCell<AlterTrace>>, bool)>,
    /// A `json_each` / `json_tree` source (v0.16, `json`): its
    /// `json` and `root` columns are hidden from `*`, and its
    /// `value` column carries the JSON subtype for an array or
    /// object (`json::Op::EachValue`).
    #[cfg(feature = "json")]
    pub json_tvf: u8,
}

/// What an ALTER TABLE learns from planning a view (v0.16, `v16`):
/// the name tokens that resolved to the table being altered. Each
/// position is an `ABytes::pos` of the view's parsed text — a token
/// handle — so the rename can edit exactly those tokens, as the
/// pin's `renameColumnFunc` / `renameTableFunc` edit theirs.
#[cfg(feature = "v16")]
#[derive(Debug, Default)]
pub struct AlterTrace {
    /// The altered table's name.
    pub table: AVec<u8>,
    /// FROM-clause name tokens that read the table.
    pub from: AVec<u32>,
    /// Column references resolved to the table.
    pub hits: AVec<TraceHit>,
}

/// One reference to the altered table's column (v0.16, `v16`).
#[cfg(feature = "v16")]
#[derive(Debug, Clone, Copy)]
pub struct TraceHit {
    /// The column-name token (`None` for `t.rowid` and `t.*`).
    pub name: Option<u32>,
    /// The column's index in the table (`usize::MAX` without one).
    pub col: usize,
    /// The qualifier token, when the source was read under the
    /// table's own name (an alias is never the table's token).
    pub qual: Option<u32>,
}

/// Record a resolved column reference in the trace of the source it
/// resolved to (v0.16, `v16`; see `AlterTrace`): the first scope, in
/// resolution order, holding a source that matches is the one the
/// reference read; a scope whose result alias answered is not.
#[cfg(feature = "v16")]
fn trace_column(
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    table: Option<crate::arena::ABytes>,
    name: crate::arena::ABytes,
    arena: &Arena,
) {
    let q = table.map(|h| handle_bytes(arena, h));
    let n = handle_bytes(arena, name);
    for g in core::iter::once(cur).chain(enclosing.iter()) {
        let mut any = false;
        for src in g.sources.iter() {
            if let Some(q) = q
                && !src.named(q)
            {
                continue;
            }
            // `rowid` names the INTEGER PRIMARY KEY column when
            // there is one (the pin renames it with that column).
            let mut ci = src.column_index(n);
            let rowid = ci.is_none() && is_rowid_name(n) && src.has_rowid;
            if rowid {
                ci = src.rowid_alias;
            }
            if ci.is_none() && !(rowid && q.is_some()) {
                continue;
            }
            any = true;
            if let Some((tr, aliased)) = &src.trace {
                tr.borrow_mut().hits.push(TraceHit {
                    name: ci.map(|_| name.pos),
                    col: ci.unwrap_or(usize::MAX),
                    qual: table.filter(|_| !*aliased).map(|h| h.pos),
                });
            }
        }
        if any || (q.is_none() && g.aliases.iter().any(|(a, _)| eq_ci(a.as_slice(), n))) {
            return;
        }
    }
}

/// Record `q.*` over the altered table (v0.16, `v16`).
#[cfg(feature = "v16")]
pub fn trace_star(cur: &ScopeGroup, q: crate::arena::ABytes, arena: &Arena) {
    let qn = handle_bytes(arena, q);
    for src in cur.sources.iter() {
        if src.named(qn)
            && let Some((tr, false)) = &src.trace
        {
            tr.borrow_mut().hits.push(TraceHit {
                name: None,
                col: usize::MAX,
                qual: Some(q.pos),
            });
        }
    }
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
            schema_table: false,
            #[cfg(feature = "v16")]
            trace: None,
            #[cfg(feature = "json")]
            json_tvf: 0,
        }
    }

    /// Whether qualifier `q` names this source.
    pub fn named(&self, q: &[u8]) -> bool {
        eq_ci(self.qualifier.as_slice(), q)
            || (self.schema_table && (eq_ci(q, b"sqlite_master") || eq_ci(q, b"sqlite_schema")))
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
        // A `json_each` key / value / atom decodes from JSONB when
        // read, and a value keeps the JSON subtype of a container
        // (v0.16, `json`; `json::Op::EachValue`, which reads the
        // `type` column too).
        #[cfg(feature = "json")]
        if self.json_tvf != 0
            && matches!(ci, 0 | 1 | 3)
            && let RExpr::Column(slot, ..) = r
        {
            let base = slot.saturating_sub(ci);
            let mut a = AVec::new();
            a.push(r);
            let op = if ci == 0 {
                crate::json::Op::EachKey
            } else {
                a.push(RExpr::Column(
                    base + 2,
                    Affinity::Blob,
                    Some(Coll::default()),
                ));
                crate::json::Op::EachValue
            };
            return RExpr::Scalar(
                ScalarFn::Json(crate::json::JsonFn {
                    op,
                    blob: self.json_tvf & 4 != 0,
                }),
                a,
            );
        }
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
    /// Whether an aggregate in a subquery that references this
    /// group's columns may belong to it (v0.16 review fix). False
    /// for a RETURNING list's row and an upsert's `excluded`
    /// row: the pin's owner search never stops at those, so
    /// `RETURNING (SELECT max(t.a) FROM u)` aggregates inside
    /// the subquery (9 for a one-row insert of a = 9).
    pub agg_owner: bool,
    /// The window state of this group's SELECT core (v0.16,
    /// `window`).
    #[cfg(feature = "window")]
    pub win: WinScope,
}

/// What window calls a SELECT core's resolution may make (v0.16,
/// `window`): its WINDOW clause, whether calls are allowed where
/// resolution stands (its result list and ORDER BY, outside any
/// aggregate's or window call's own clauses — the pin's
/// `NC_AllowWin`), and the next call's slot.
#[cfg(feature = "window")]
#[derive(Debug, Clone, Default)]
pub struct WinScope {
    pub defs: AVec<(crate::arena::ABytes, crate::sql::ast::WindowDef)>,
    pub ok: core::cell::Cell<bool>,
    pub next: core::cell::Cell<usize>,
}

impl ScopeGroup {
    /// A scope group with no aliases.
    pub fn new(sources: AVec<SourceDesc>, offsets: AVec<usize>, first_src: usize) -> ScopeGroup {
        ScopeGroup {
            sources,
            offsets,
            first_src,
            aliases: AVec::new(),
            agg_owner: true,
            #[cfg(feature = "window")]
            win: WinScope::default(),
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
    /// A recursive CTE's recursive reference (v0.16, `v16`): when
    /// non-zero, the source's one row is the row the CTE's queue
    /// last gave up (`RecInfo::id`); `root` is unused.
    #[cfg(feature = "v16")]
    pub rec: u32,
    /// A `json_each` / `json_tree` source (v0.16, `json`): its kind
    /// (`json::tvf_kind`, with `TVF_LATERAL` when its arguments
    /// read a source to its left), 0 for any other source. Its
    /// `derived` plan is a placeholder whose one VALUES row holds
    /// the resolved arguments, so every plan walker (correlation,
    /// shifts, aggregates, subqueries) sees them.
    #[cfg(feature = "json")]
    pub tvf: u8,
}

/// `SubSource::tvf`: the arguments read a FROM source to the left,
/// so the rows are computed per row of the sources before it.
#[cfg(feature = "json")]
pub const TVF_LATERAL: u8 = 0x80;

/// A recursive common table expression's shape (v0.16, `v16`),
/// carried by its `SubPlan`: compound tails from index `split - 1`
/// on are the recursive SELECTs, the plan's head and the tails
/// before them the setup. `comp_order` / `limit` / `offset` are
/// the CTE's own ORDER BY (the queue order) and row window.
#[cfg(feature = "v16")]
#[derive(Debug, Clone, Copy)]
pub struct RecInfo {
    pub split: usize,
    /// UNION (rows seen before are dropped) rather than UNION ALL.
    pub union: bool,
    /// The id the recursive references read (`SubSource::rec`).
    pub id: u32,
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
    /// The result column a scalar subquery yields (v0.16): 0,
    /// except for the parts of a multi-column scalar subquery
    /// used as a row value (`(SELECT a, b) = (1, 2)`, `SET (a,
    /// b) = (SELECT ...)`), where part `i` is a copy of the plan
    /// yielding column `i` of the first row.
    pub scalar_col: usize,
    /// A recursive CTE (v0.16, `v16`; see `RecInfo`).
    #[cfg(feature = "v16")]
    pub rec: Option<RecInfo>,
    /// The shared materialization this derived table's rows come
    /// from (v0.16 review fix, `v16`; `CteUse::mat`): non-zero when
    /// every derived table with this id reads one result, computed
    /// once per statement execution. Set only on an uncorrelated
    /// plan.
    #[cfg(feature = "v16")]
    pub mat: u32,
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
        RExpr::Literal(_) | RExpr::AffLiteral(..) | RExpr::Raise(_) => false,
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
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
            args.iter().any(f)
        }
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
    !plan_refs_below(p, outer_width, outer_srcs) && !plan_has_foreign_agg(p)
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
    /// The subquery failed with this evaluation error (v0.16
    /// review fix): raised only if evaluation reads it
    /// (`RExpr::Raise`).
    Fail(SchemaKind),
}

/// The affinity a subquery's single result item carries into
/// an enclosing comparison (v0.8): a direct column reference,
/// a rowid reference, or a CAST keeps its affinity; every
/// other expression shape has none.
pub fn result_affinity(plan: &SubPlan) -> Option<Affinity> {
    sub_col_affinity(plan, plan.scalar_col)
}

/// The comparison affinity column `j` of a subquery used as an
/// expression carries (a scalar subquery's value, an IN set):
/// the pin's `sqlite3ExprAffinity` of `pSelect->pEList`, i.e.
/// of the RIGHTMOST compound core (v0.16 review fix: hipp read
/// the leftmost, so `'1' IN (SELECT 1 UNION SELECT i FROM c)`
/// over INTEGER i was 0). A one-row VALUES core is its row; a
/// longer one is the pin's `sqlite3MultiValues` — a co-routine
/// with no affinity when every row is constant and the first
/// has none, else the UNION ALL chain, whose rightmost core is
/// the last row (the rule `core_colls` follows for collations).
pub fn sub_col_affinity(plan: &SubPlan, j: usize) -> Option<Affinity> {
    let core: &SubPlan = plan.compound.as_slice().last().map_or(plan, |(_, t)| t);
    let rows = core.values.as_slice();
    let Some(last) = rows.last() else {
        return core.items.as_slice().get(j).and_then(item_result_affinity);
    };
    let first = rows.first().map_or(&[][..], |r| r.as_slice());
    let coroutine = rows.len() > 1
        && rows.iter().all(|r| r.iter().all(is_constant))
        && first.iter().all(|e| item_result_affinity(e).is_none());
    if coroutine {
        return None;
    }
    last.as_slice().get(j).and_then(item_result_affinity)
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
        // A scalar subquery carries its own result's affinity
        // (v0.16 review fix: `(SELECT (SELECT i FROM c)) = '1'`
        // over INTEGER i was 0).
        RExpr::SubScalar(p) => result_affinity(p),
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
    // A qualified reference counts only the sources of that name
    // (v0.16 fix, ungated: `SELECT t.a FROM t, t` read the first t
    // where the pin reports "ambiguous column name"); a USING
    // column shared by two of them resolves as unqualified.
    let mut cnt = 0usize;
    let mut found: Option<RExpr> = None;
    let mut full: AVec<RExpr> = AVec::new();
    for (si, src) in group.sources.iter().enumerate() {
        if let Some(q) = table
            && !src.named(q)
        {
            continue;
        }
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
    if let Some(q) = table
        && cnt == 0
    {
        // The qualifier matched no column: its rowid, or the
        // enclosing scopes — the pin's `lookupName` goes on there
        // (v0.16 fix: `SELECT (SELECT max(t.a) FROM u AS t) FROM t`
        // reads the outer t.a; hipp answered "no such column").
        // Two such sources with a rowid are ambiguous (v0.16
        // review fix: `SELECT t.rowid FROM t, t` read the first).
        if is_rowid_name(name) {
            let mut rowid: Option<RExpr> = None;
            for (si, src) in group.sources.iter().enumerate() {
                if src.named(q) && src.has_rowid {
                    if rowid.is_some() {
                        return Some(Err(Error::Schema {
                            kind: SchemaKind::AmbiguousColumn,
                        }));
                    }
                    rowid = Some(RExpr::Rowid(group.first_src + si));
                }
            }
            if let Some(r) = rowid {
                return Some(Ok(r));
            }
        }
        return None;
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
            // An alias of a window call outside the result list /
            // ORDER BY (or from a subquery): the pin's "misuse of
            // aliased window function" (v0.16, `window`).
            #[cfg(feature = "window")]
            if (depth > 0 || !cur.win.ok.get()) && crate::window::has_win(e) {
                return Err(Error::Schema {
                    kind: SchemaKind::MisusedWindow,
                });
            }
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
/// An alias whose item holds an aggregate still names the
/// alias's own query's aggregate (one value per outer group):
/// every aggregate the item's query (or a query further out)
/// owns has its owner level raised by `depth`, so the owner
/// folds it per group and substitutes the value into this
/// subquery (`subst_owned_aggs`) — `HAVING (SELECT s) > 0`
/// over `sum(a) AS s` (v0.16 review fix: refused, and before
/// that the subquery re-aggregated its own rows). Where the
/// owner cannot aggregate (WHERE, GROUP BY) the planner then
/// reports the pin's misuse.
#[cfg(feature = "v16")]
fn alias_from_outer<E>(
    e: &RExpr,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    depth: usize,
) -> Result<RExpr, E> {
    let home = ScopeGroup::stack_end(enclosing.get(depth.saturating_sub(1)..).unwrap_or(&[]));
    let (cw, cs) = cur.end();
    let (ew, es) = ScopeGroup::stack_end(enclosing);
    let by = (
        cw.max(ew).saturating_sub(home.0),
        cs.max(es).saturating_sub(home.1),
    );
    let mut out = e.clone();
    shift_expr(&mut out, home, by);
    let up = u8::try_from(depth).map_err(|_| Error::Unsupported)?;
    walk_aggs_mut::<E>(&mut out, 0, &mut |a, dd| {
        if let RExpr::Agg { lvl, alias, .. } = a
            && *lvl >= dd
        {
            *lvl = lvl.checked_add(up).ok_or(Error::Unsupported)?;
            *alias = true;
        }
        Ok(false)
    })?;
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
        RExpr::Literal(_) | RExpr::AffLiteral(..) | RExpr::Raise(_) => {}
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
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
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
            && !src.named(q)
        {
            continue;
        }
        hit = true;
        for (ci, (cname, _)) in src.columns.iter().enumerate() {
            let n = cname.as_slice();
            if qual.is_none() && si > 0 && src.uses(n) {
                continue;
            }
            // A table-valued function's hidden columns (v0.16).
            #[cfg(feature = "json")]
            if src.json_tvf != 0 && ci >= crate::json::TVF_VISIBLE {
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
                // With several sources the pin expands `*` to
                // `q.col` references, so two sources of one name
                // sharing a column make it ambiguous (v0.16 fix,
                // ungated: `SELECT * FROM t, t` answered).
                if srcs.len() > 1
                    && !src.qualifier.is_empty()
                    && let Some(Err(e)) =
                        resolve_column_in_group::<E>(cur, Some(src.qualifier.as_slice()), n)
                {
                    return Err(e);
                }
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
            Box::new(RExpr::AffLiteral(
                OwnedValue::Integer(0),
                Some(Affinity::Numeric),
                None,
            )),
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
        Expr::HexTooBig(offset) => {
            return Err(Error::Parse {
                offset: *offset,
                kind: crate::error::ParseKind::HexLiteralTooBig,
            });
        }
        // Only a stored schema text still holds a parameter here
        // (a prepared statement's are bound away before planning,
        // `session::bind_ast`); the pin reads one there as NULL.
        #[cfg(feature = "params")]
        Expr::Param(..) => RExpr::Literal(OwnedValue::Null),
        Expr::Column { table, name } => resolve_col_ref::<E>(*table, *name, arena, cur, enclosing)?,
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        Expr::TrigValue(lit, aff, coll) => {
            RExpr::AffLiteral(literal_value(arena, *lit), *aff, *coll)
        }
        // Row-value comparisons (resolved out of line).
        #[cfg(feature = "v16")]
        Expr::Binary(op, l, r) if is_cmp_op(*op) && (is_row(l) || is_row(r)) => {
            resolve_other::<E>(expr, arena, cur, enclosing, planner)?
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
        Expr::Cast(e, ty) => RExpr::Cast(
            Box::new(resolve_scoped::<E>(
                e,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?),
            crate::record::cast_affinity(handle_bytes(arena, *ty)),
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
        // Everything else (subqueries, row values, lists, CASE,
        // calls) is resolved out of line: this function is on the
        // stack once per level of the expression tree, so its own
        // frame is kept to the common unary / binary nodes (v0.16:
        // the deepest tree the parser admits must resolve on a
        // small stack — see `Config::max_expr_depth`).
        _ => resolve_other::<E>(expr, arena, cur, enclosing, planner)?,
    })
}

/// A column reference (out of line: see `resolve_scoped`).
#[inline(never)]
fn resolve_col_ref<E>(
    table: Option<crate::arena::ABytes>,
    name: crate::arena::ABytes,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
) -> Result<RExpr, E> {
    let r = resolve_column::<E>(
        cur,
        enclosing,
        table.map(|h| handle_bytes(arena, h)),
        handle_bytes(arena, name),
    )?;
    #[cfg(feature = "v16")]
    if core::iter::once(cur)
        .chain(enclosing.iter())
        .any(|g| g.sources.iter().any(|s| s.trace.is_some()))
    {
        trace_column(cur, enclosing, table, name, arena);
    }
    Ok(r)
}

/// The nodes `resolve_scoped` leaves out of its own frame.
#[inline(never)]
fn resolve_other<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    let p = planner;
    Ok(match expr {
        #[cfg(feature = "v16")]
        Expr::InSubquery(x, ..) if is_row(x) => {
            res_in_sub_row::<E>(expr, arena, cur, enclosing, p)?
        }
        Expr::InSubquery(..) => res_in_sub::<E>(expr, arena, cur, enclosing, p)?,
        Expr::ScalarSubquery(_) => res_scalar_sub::<E>(expr, arena, cur, enclosing, p)?,
        #[cfg(feature = "v16")]
        Expr::BitNot(_) => res_bit_not::<E>(expr, arena, cur, enclosing, p)?,
        // A row value anywhere but a comparison operand.
        #[cfg(feature = "v16")]
        Expr::Vector(_) => return Err(row_misused()),
        #[cfg(feature = "v16")]
        Expr::RowItem(..) => res_row_item::<E>(expr, arena, cur, enclosing, p)?,
        #[cfg(feature = "v16")]
        Expr::Binary(op, l, r) if is_cmp_op(*op) && (is_row(l) || is_row(r)) => {
            res_row_cmp::<E>(expr, arena, cur, enclosing, p)?
        }
        #[cfg(feature = "v16")]
        Expr::Between(x, lo, hi, _) if is_row(x) || is_row(lo) || is_row(hi) => {
            res_row_between::<E>(expr, arena, cur, enclosing, p)?
        }
        #[cfg(feature = "v16")]
        Expr::InList(x, ..) if matches!(**x, Expr::Vector(_)) => {
            res_row_in_list::<E>(expr, arena, cur, enclosing, p)?
        }
        #[cfg(feature = "v16")]
        Expr::Case {
            base: Some(b),
            whens,
            ..
        } if is_row(b) || whens.iter().any(|(w, _)| is_row(w)) => {
            res_row_case::<E>(expr, arena, cur, enclosing, p)?
        }
        Expr::ExistsSubquery(_) => res_exists::<E>(expr, arena, cur, enclosing, p)?,
        Expr::InList(..) => res_in_list::<E>(expr, arena, cur, enclosing, p)?,
        Expr::Between(..) => res_between::<E>(expr, arena, cur, enclosing, p)?,
        Expr::Like(..) => res_like::<E>(expr, arena, cur, enclosing, p)?,
        Expr::Case { .. } => res_case::<E>(expr, arena, cur, enclosing, p)?,
        Expr::Func {
            name,
            distinct,
            star,
            args,
            #[cfg(feature = "v16")]
            clause,
        } => resolve_func::<E>(
            handle_bytes(arena, *name),
            *distinct,
            *star,
            args.as_slice(),
            #[cfg(feature = "v16")]
            clause.as_deref(),
            arena,
            cur,
            enclosing,
            p,
        )?,
        // `RAISE(...)` (v0.16, `triggers`): a call whose evaluation
        // fails the trigger program (`eval_lazy`).
        #[cfg(feature = "triggers")]
        Expr::Raise(kind, msg, in_trigger) => {
            if !*in_trigger {
                return Err(Error::Schema {
                    kind: SchemaKind::RaiseOutsideTrigger,
                });
            }
            let mut args = AVec::new();
            if let Some(m) = msg {
                args.push(resolve_scoped::<E>(m, arena, cur, enclosing, p)?);
            }
            RExpr::Scalar(ScalarFn::Raise(*kind), args)
        }
        // Handled by `resolve_scoped` itself.
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `in_sub_row` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_in_sub_row<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::InSubquery(x, stmt, neg) if is_row(x) => {
            let xp = resolve_row::<E>(x, arena, cur, enclosing, &mut *planner)?;
            let plan = planner(stmt, cur, enclosing)?;
            if plan.width() != xp.len() {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            RExpr::InSub(
                Box::new(RExpr::Scalar(ScalarFn::Vector, xp)),
                Box::new(plan),
                *neg,
            )
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `in_sub` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_in_sub<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
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
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `scalar_sub` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_scalar_sub<E>(
    expr: &Expr,
    _arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::ScalarSubquery(stmt) => {
            let plan = planner(stmt, cur, enclosing)?;
            if plan.width() != 1 {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            RExpr::SubScalar(Box::new(first_row_limit(plan)))
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `bit_not` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_bit_not<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::BitNot(e) => {
            let mut args = AVec::new();
            args.push(resolve_scoped::<E>(
                e,
                arena,
                cur,
                enclosing,
                &mut *planner,
            )?);
            RExpr::Scalar(ScalarFn::BitNot, args)
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `row_item` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_row_item<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::RowItem(e, i, n) => {
            let parts = resolve_row::<E>(e, arena, cur, enclosing, &mut *planner)?;
            if parts.len() != usize::from(*n) {
                return Err(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                });
            }
            parts
                .as_slice()
                .get(usize::from(*i))
                .cloned()
                .ok_or(Error::Schema {
                    kind: SchemaKind::ColumnCountMismatch,
                })?
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `row_cmp` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_row_cmp<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::Binary(op, l, r) if is_cmp_op(*op) && (is_row(l) || is_row(r)) => {
            let lp = resolve_row::<E>(l, arena, cur, enclosing, &mut *planner)?;
            let rp = resolve_row::<E>(r, arena, cur, enclosing, &mut *planner)?;
            row_cmp::<E>(*op, lp.as_slice(), rp.as_slice())?
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `row_between` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_row_between<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::Between(x, lo, hi, neg) if is_row(x) || is_row(lo) || is_row(hi) => {
            let xp = resolve_row::<E>(x, arena, cur, enclosing, &mut *planner)?;
            let lp = resolve_row::<E>(lo, arena, cur, enclosing, &mut *planner)?;
            let hp = resolve_row::<E>(hi, arena, cur, enclosing, &mut *planner)?;
            let b = RExpr::Binary(
                BinOp::And,
                Box::new(row_cmp::<E>(BinOp::Ge, xp.as_slice(), lp.as_slice())?),
                Box::new(row_cmp::<E>(BinOp::Le, xp.as_slice(), hp.as_slice())?),
            );
            if *neg { RExpr::Not(Box::new(b)) } else { b }
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `row_in_list` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_row_in_list<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::InList(x, list, neg) if matches!(**x, Expr::Vector(_)) => {
            let xp = resolve_row::<E>(x, arena, cur, enclosing, &mut *planner)?;
            let mut items = AVec::new();
            for e in list.iter() {
                // Each item must itself be a row value of the
                // same size (the pin's VALUES rewrite: "IN(...)
                // element has N terms - expected M").
                let Expr::Vector(_) = e else {
                    return Err(row_misused());
                };
                let ip = resolve_row::<E>(e, arena, cur, enclosing, &mut *planner)?;
                if ip.len() != xp.len() {
                    return Err(row_misused());
                }
                items.push(RExpr::Scalar(ScalarFn::Vector, ip));
            }
            RExpr::InList(
                Box::new(RExpr::Scalar(ScalarFn::Vector, xp)),
                items,
                *neg,
                false,
            )
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `row_case` node (out of line, v0.16: see
/// `resolve_scoped`).
#[cfg(feature = "v16")]
#[inline(never)]
fn res_row_case<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::Case {
            base: Some(b),
            whens,
            otherwise,
        } if is_row(b) || whens.iter().any(|(w, _)| is_row(w)) => {
            // `CASE (a, b) WHEN (x, y) ...` compares like `=`:
            // the searched CASE of the row comparisons.
            let bp = resolve_row::<E>(b, arena, cur, enclosing, &mut *planner)?;
            let mut ws = AVec::new();
            for (w, t) in whens.iter() {
                let wp = resolve_row::<E>(w, arena, cur, enclosing, &mut *planner)?;
                ws.push((
                    row_cmp::<E>(BinOp::Eq, bp.as_slice(), wp.as_slice())?,
                    resolve_scoped::<E>(t, arena, cur, enclosing, &mut *planner)?,
                ));
            }
            RExpr::Case {
                base: None,
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
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `exists` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_exists<E>(
    expr: &Expr,
    _arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
        Expr::ExistsSubquery(stmt) => {
            let mut plan = planner(stmt, cur, enclosing)?;
            mark_exists_inert(&mut plan);
            RExpr::SubExists(Box::new(first_row_limit(plan)))
        }
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `in_list` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_in_list<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
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
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `between` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_between<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
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
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `like` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_like<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
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
        _ => return Err(Error::Unsupported),
    })
}

/// `resolve_other`'s `case` node (out of line, v0.16: see
/// `resolve_scoped`).
#[inline(never)]
fn res_case<E>(
    expr: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    #[allow(unreachable_patterns)]
    Ok(match expr {
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
        _ => return Err(Error::Unsupported),
    })
}

/// The pin's "row value misused" (v0.16).
#[cfg(feature = "v16")]
fn row_misused<E>() -> Error<E> {
    Error::Schema {
        kind: SchemaKind::RowValueMisused,
    }
}

/// `=`, `<>`, `<`, `<=`, `>`, `>=`, `IS`, `IS NOT`: the operators
/// that compare row values (v0.16).
#[cfg(feature = "v16")]
fn is_cmp_op(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Is
            | BinOp::IsNot
    )
}

/// Whether `e` may be a row value (v0.16): a written `(a, b,
/// ...)` or a scalar subquery (whose width only planning
/// tells).
#[cfg(feature = "v16")]
fn is_row(e: &Expr) -> bool {
    matches!(e, Expr::Vector(_) | Expr::ScalarSubquery(_))
}

/// Resolve an operand that may be a row value (v0.16): its
/// elements — one for a scalar. A written row value's elements
/// must be scalars; a scalar subquery of n > 1 columns gives n
/// copies of its plan, part `i` yielding column `i` of the
/// first row.
#[cfg(feature = "v16")]
fn resolve_row<E>(
    e: &Expr,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<AVec<RExpr>, E> {
    let mut out = AVec::new();
    match e {
        Expr::Vector(items) => {
            for x in items.iter() {
                out.push(resolve_scoped::<E>(
                    x,
                    arena,
                    cur,
                    enclosing,
                    &mut *planner,
                )?);
            }
        }
        Expr::ScalarSubquery(stmt) => {
            let plan = first_row_limit(planner(stmt, cur, enclosing)?);
            for i in 0..plan.width() {
                let mut p = plan.clone();
                p.scalar_col = i;
                out.push(RExpr::SubScalar(Box::new(p)));
            }
        }
        _ => out.push(resolve_scoped::<E>(
            e,
            arena,
            cur,
            enclosing,
            &mut *planner,
        )?),
    }
    Ok(out)
}

/// A comparison of two resolved operands as the pin's vector
/// comparison (v0.16, `codeVectorCompare`), written with scalar
/// operators so affinity and collation stay per element: `=` /
/// `IS` are the AND of the element tests, `<>` / `IS NOT` their
/// OR, and `<` (`<=`, `>`, `>=`) is lexicographic — `a1 < b1 OR
/// (a1 = b1 AND (rest))`, the last element taking the operator
/// itself. Three-valued logic gives the pin's NULL answers. Two
/// scalars compare directly; operands of different sizes are
/// "row value misused".
#[cfg(feature = "v16")]
fn row_cmp<E>(op: BinOp, l: &[RExpr], r: &[RExpr]) -> Result<RExpr, E> {
    if l.len() != r.len() {
        return Err(row_misused());
    }
    let bin = |op: BinOp, a: RExpr, b: RExpr| RExpr::Binary(op, Box::new(a), Box::new(b));
    let pair = |i: usize, op: BinOp| match (l.get(i), r.get(i)) {
        (Some(a), Some(b)) => Ok(bin(op, a.clone(), b.clone())),
        _ => Err(row_misused()),
    };
    let n = l.len();
    let Some(last) = n.checked_sub(1) else {
        return Err(row_misused());
    };
    let (join, elem) = match op {
        BinOp::Eq | BinOp::Is => (BinOp::And, op),
        BinOp::Ne => (BinOp::Or, BinOp::Ne),
        BinOp::IsNot => (BinOp::Or, BinOp::IsNot),
        _ => {
            let strict = if matches!(op, BinOp::Lt | BinOp::Le) {
                BinOp::Lt
            } else {
                BinOp::Gt
            };
            let mut acc = pair(last, op)?;
            for i in (0..last).rev() {
                acc = bin(
                    BinOp::Or,
                    pair(i, strict)?,
                    bin(BinOp::And, pair(i, BinOp::Eq)?, acc),
                );
            }
            return Ok(acc);
        }
    };
    let mut acc = pair(0, elem)?;
    for i in 1..n {
        acc = bin(join, acc, pair(i, elem)?);
    }
    Ok(acc)
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

/// The scalar function a name calls (case-insensitive), or None
/// for a name the pin does not define (or this build leaves out).
/// Whether `name` is a scalar function (v0.16, `window`: OVER on
/// one is the pin's "may not be used as a window function").
#[cfg(feature = "window")]
pub(crate) fn is_scalar_name(name: &[u8]) -> bool {
    scalar_fn(name).is_some()
}

fn scalar_fn(name: &[u8]) -> Option<ScalarFn> {
    const BASE: &[(&[u8], ScalarFn)] = &[
        (b"abs", ScalarFn::Abs),
        (b"length", ScalarFn::Length),
        (b"upper", ScalarFn::Upper),
        (b"lower", ScalarFn::Lower),
        (b"coalesce", ScalarFn::Coalesce),
        (b"ifnull", ScalarFn::Coalesce),
        (b"nullif", ScalarFn::Nullif),
        (b"typeof", ScalarFn::Typeof),
        (b"substr", ScalarFn::Substr),
        (b"substring", ScalarFn::Substr),
        (b"round", ScalarFn::Round),
        (b"hex", ScalarFn::Hex),
        (b"trim", ScalarFn::Trim),
        (b"ltrim", ScalarFn::Ltrim),
        (b"rtrim", ScalarFn::Rtrim),
        (b"replace", ScalarFn::Replace),
        (b"instr", ScalarFn::Instr),
        (b"sign", ScalarFn::Sign),
        (b"iif", ScalarFn::Iif),
    ];
    #[cfg(feature = "v16")]
    const V16: &[(&[u8], ScalarFn)] = &[
        (b"if", ScalarFn::Iif),
        (b"like", ScalarFn::Like),
        (b"glob", ScalarFn::Glob),
        (b"printf", ScalarFn::Printf),
        (b"format", ScalarFn::Printf),
        (b"quote", ScalarFn::Quote),
        (b"char", ScalarFn::Char),
        (b"unicode", ScalarFn::Unicode),
        (b"zeroblob", ScalarFn::Zeroblob),
        (b"randomblob", ScalarFn::Randomblob),
        (b"random", ScalarFn::Random),
        (b"last_insert_rowid", ScalarFn::LastInsertRowid),
        (b"changes", ScalarFn::Changes),
        (b"total_changes", ScalarFn::TotalChanges),
        (b"likely", ScalarFn::Likely),
        (b"unlikely", ScalarFn::Likely),
        (b"likelihood", ScalarFn::Likely),
        (b"concat", ScalarFn::Concat),
        (b"concat_ws", ScalarFn::ConcatWs),
        (b"octet_length", ScalarFn::OctetLength),
        (b"unhex", ScalarFn::Unhex),
        (b"sqlite_version", ScalarFn::SqliteVersion),
    ];
    #[cfg(not(feature = "v16"))]
    const V16: &[(&[u8], ScalarFn)] = &[];
    let f = BASE
        .iter()
        .chain(V16.iter())
        .find(|(n, _)| eq_ci(name, n))
        .map(|(_, f)| *f);
    #[cfg(feature = "json")]
    if f.is_none()
        && let Some(j) = crate::json::JsonFn::lookup(name)
    {
        return Some(ScalarFn::Json(j));
    }
    #[cfg(feature = "math")]
    if f.is_none() {
        return crate::math::MathFn::lookup(name).map(ScalarFn::Math);
    }
    f
}

/// The argument counts a scalar function takes, as on the pin.
fn arity(f: ScalarFn) -> (usize, usize) {
    match f {
        // The pin's coalesce() takes two or more arguments (the
        // name-dependent `ifnull` form is exactly two; see
        // `resolve_func`). v0.16 review fix, ungated: one argument
        // used to run.
        ScalarFn::Coalesce => (2, usize::MAX),
        ScalarFn::Nullif | ScalarFn::Instr => (2, 2),
        ScalarFn::Substr => (2, 3),
        ScalarFn::Round | ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => (1, 2),
        ScalarFn::Replace => (3, 3),
        // The pin's iif() is a CASE of (condition, value) pairs
        // with an optional ELSE (3.48+): two or more arguments.
        #[cfg(feature = "v16")]
        ScalarFn::Iif => (2, usize::MAX),
        #[cfg(not(feature = "v16"))]
        ScalarFn::Iif => (3, 3),
        #[cfg(feature = "v16")]
        ScalarFn::Like => (2, 3),
        #[cfg(feature = "v16")]
        ScalarFn::Glob => (2, 2),
        ScalarFn::MinN | ScalarFn::MaxN => (2, usize::MAX),
        #[cfg(feature = "v16")]
        ScalarFn::Printf | ScalarFn::Char => (0, usize::MAX),
        #[cfg(feature = "v16")]
        ScalarFn::Random
        | ScalarFn::LastInsertRowid
        | ScalarFn::Changes
        | ScalarFn::TotalChanges
        | ScalarFn::SqliteVersion => (0, 0),
        #[cfg(feature = "v16")]
        ScalarFn::Likely | ScalarFn::Unhex => (1, 2),
        #[cfg(feature = "v16")]
        ScalarFn::Concat => (1, usize::MAX),
        #[cfg(feature = "v16")]
        ScalarFn::ConcatWs => (2, usize::MAX),
        #[cfg(feature = "math")]
        ScalarFn::Math(m) => m.arity(),
        #[cfg(feature = "json")]
        ScalarFn::Json(j) => j.arity(),
        _ => (1, 1),
    }
}

/// Without `window` a call resolves as `resolve_call_body` does.
#[cfg(not(feature = "window"))]
use resolve_call_body as resolve_func;

/// A function call (v0.16, `window`: a call with OVER is a window
/// call, a built-in window function without one the pin's misuse,
/// and no window call may sit inside an aggregate's arguments,
/// ORDER BY or FILTER).
#[cfg(feature = "window")]
#[allow(clippy::too_many_arguments)]
fn resolve_func<E>(
    name: &[u8],
    distinct: bool,
    star: bool,
    args: &[Expr],
    #[cfg(feature = "v16")] clause: Option<&crate::sql::ast::FuncClause>,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    {
        if let Some(c) = clause
            && let Some(over) = &c.over
        {
            return crate::window::resolve_call::<E>(
                name, distinct, star, args, c, over, arena, cur, enclosing, planner,
            );
        }
        if crate::window::WinFn::builtin(name).is_some() {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedWindow,
            });
        }
        let aggregate = agg_kind(name)
            .is_some_and(|k| !(matches!(k, AggKind::Min | AggKind::Max) && args.len() > 1));
        if aggregate {
            let was = cur.win.ok.replace(false);
            let r = resolve_call_body::<E>(
                name, distinct, star, args, clause, arena, cur, enclosing, planner,
            );
            cur.win.ok.set(was);
            return r;
        }
    }
    resolve_call_body::<E>(
        name, distinct, star, args, clause, arena, cur, enclosing, planner,
    )
}

/// The aggregate a function name calls, if any.
#[inline(always)]
pub(crate) fn agg_kind(name: &[u8]) -> Option<AggKind> {
    const AGGS: &[(&[u8], AggKind)] = &[
        (b"count", AggKind::Count),
        (b"sum", AggKind::Sum),
        (b"avg", AggKind::Avg),
        (b"total", AggKind::Total),
        (b"min", AggKind::Min),
        (b"max", AggKind::Max),
        (b"group_concat", AggKind::GroupConcat),
        #[cfg(feature = "v16")]
        (b"string_agg", AggKind::GroupConcat),
        #[cfg(feature = "v16")]
        (b"median", AggKind::Median),
        #[cfg(feature = "v16")]
        (b"percentile", AggKind::Percentile),
        #[cfg(feature = "v16")]
        (b"percentile_cont", AggKind::PercentileCont),
        #[cfg(feature = "v16")]
        (b"percentile_disc", AggKind::PercentileDisc),
    ];
    let k = AGGS.iter().find(|(n, _)| eq_ci(name, n)).map(|(_, k)| *k);
    #[cfg(feature = "json")]
    if k.is_none() {
        return crate::json::JsonAgg::lookup(name).map(AggKind::Json);
    }
    k
}

/// A JSON aggregate's argument layout (v0.16, `json`): its own
/// arguments (the first `nargs` of `all`; the ORDER BY keys and
/// FILTER follow) become the per-row JSON text the fold joins —
/// `Frag(x)` for an array (the value itself first under DISTINCT,
/// which tests it), `ObjFrag(label, value)` for an object. Returns
/// the new argument count.
#[cfg(feature = "json")]
pub(crate) fn json_agg_args(
    a: crate::json::JsonAgg,
    distinct: bool,
    nargs: usize,
    all: &mut AVec<RExpr>,
) -> usize {
    let n = nargs.min(all.len());
    let mut own: AVec<RExpr> = AVec::new();
    own.extend_from_slice(all.as_slice().get(..n).unwrap_or(&[]));
    let rest: AVec<RExpr> = {
        let mut r = AVec::new();
        r.extend_from_slice(all.as_slice().get(n..).unwrap_or(&[]));
        r
    };
    let mut out: AVec<RExpr> = AVec::new();
    if distinct && !a.object {
        out.extend_from_slice(own.as_slice());
    }
    out.push(RExpr::Scalar(ScalarFn::Json(a.frag_fn()), own));
    let k = out.len();
    out.extend_from_slice(rest.as_slice());
    *all = out;
    k
}

/// `resolve_func` without the window rules.
#[allow(clippy::too_many_arguments)]
fn resolve_call_body<E>(
    name: &[u8],
    distinct: bool,
    star: bool,
    args: &[Expr],
    #[cfg(feature = "v16")] clause: Option<&crate::sql::ast::FuncClause>,
    arena: &Arena,
    cur: &ScopeGroup,
    enclosing: &[ScopeGroup],
    planner: SubPlanner<'_, E>,
) -> Result<RExpr, E> {
    let agg = agg_kind(name);
    // The aggregate clauses on anything but an aggregate (v0.16,
    // `v16`): the pin refuses them at prepare. min / max with
    // several arguments are the scalar forms.
    #[cfg(feature = "v16")]
    if let Some(c) = clause
        && (agg.is_none() || (matches!(agg, Some(AggKind::Min | AggKind::Max)) && args.len() > 1))
    {
        return Err(Error::Schema {
            kind: if c.order_by.is_empty() {
                SchemaKind::FilterNonAggregate
            } else {
                SchemaKind::OrderByNonAggregate
            },
        });
    }
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
            AggKind::GroupConcat => {
                #[cfg(feature = "v16")]
                if eq_ci(name, b"string_agg") {
                    (2, 2)
                } else {
                    (1, 2)
                }
                #[cfg(not(feature = "v16"))]
                (1, 2)
            }
            // `count()` is `count(*)` on the pin (v0.16, `v16`).
            #[cfg(feature = "v16")]
            AggKind::Count => (0, 1),
            #[cfg(feature = "v16")]
            AggKind::Percentile | AggKind::PercentileCont | AggKind::PercentileDisc => (2, 2),
            #[cfg(feature = "json")]
            AggKind::Json(j) => (j.nargs(), j.nargs()),
            _ => (1, 1),
        };
        if star && kind != AggKind::Count {
            return Err(Error::Unsupported);
        }
        if !star && (args.len() < want.0 || args.len() > want.1) {
            return Err(Error::Schema {
                kind: SchemaKind::WrongArgumentCount,
            });
        }
        // The pin's "DISTINCT aggregates must have exactly one
        // argument" (v0.16, ungated: `group_concat(DISTINCT x,
        // ',')` used to run), checked after the argument count.
        if distinct && args.len() != 1 {
            return Err(Error::Schema {
                kind: SchemaKind::DistinctAggregateArgs,
            });
        }
        let star = star || args.is_empty();
        let all = resolve_args::<E>(args, arena, cur, enclosing, &mut *planner)?;
        #[cfg(feature = "v16")]
        let mut all = all;
        // min / max take no ORDER BY on the pin, but only its code
        // generator ignores it: the keys are resolved (so their
        // errors stand) and every resolver walk — ownership and
        // the nested-aggregate misuse check — still reads them
        // (review fix: `(SELECT max(o.x ORDER BY i.y) FROM i)`
        // became the outer query's aggregate, and `max(x ORDER BY
        // count(*))` ran). They are held here, outside the node.
        #[cfg(feature = "v16")]
        let mut ignored: AVec<RExpr> = AVec::new();
        #[cfg(feature = "v16")]
        let ext = match clause {
            None => None,
            Some(c) => {
                let nargs = all.len();
                let minmax = matches!(kind, AggKind::Min | AggKind::Max);
                let mut order = AVec::new();
                for (k, o) in c.order_by.iter() {
                    let r = resolve_scoped::<E>(k, arena, cur, enclosing, &mut *planner)?;
                    if minmax {
                        ignored.push(r);
                        continue;
                    }
                    let mut o = *o;
                    o.coll = expr_coll(&r).unwrap_or_default();
                    order.push(o);
                    all.push(r);
                }
                let unique = distinct
                    && nargs == 1
                    && order.len() == 1
                    && matches!((all.as_slice().first(), all.as_slice().get(1)), (Some(a), Some(b)) if same_expr(a, b));
                let filter = match &c.filter {
                    Some(f) => {
                        all.push(resolve_scoped::<E>(
                            f,
                            arena,
                            cur,
                            enclosing,
                            &mut *planner,
                        )?);
                        true
                    }
                    None => false,
                };
                if order.is_empty() && !filter {
                    None
                } else {
                    Some(Box::new(AggExt {
                        nargs,
                        order,
                        filter,
                        unique,
                    }))
                }
            }
        };
        // An aggregate directly inside another's arguments (and,
        // v0.16, its ORDER BY or FILTER) is the pin's "misuse of
        // aggregate function" whichever query owns either (its
        // resolver clears NC_AllowAgg while it walks an
        // aggregate's arguments): `sum(max(a))`, and `(SELECT
        // sum(c + max(t.a)) FROM u)`, where sum belongs to u and
        // max to t (review fix: the latter returned a value).
        #[cfg(feature = "v16")]
        let ignored = ignored.as_slice();
        #[cfg(not(feature = "v16"))]
        let ignored: &[RExpr] = &[];
        if all.iter().chain(ignored.iter()).any(has_direct_agg) {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedAggregate,
            });
        }
        // The owner reads every part: an aggregate whose FILTER or
        // ORDER BY (min / max's ignored one included) names an
        // inner source belongs to the inner query, as on the pin.
        let lvl = agg_level(all.as_slice(), ignored, cur, enclosing);
        #[cfg(feature = "json")]
        let mut ext = ext;
        #[cfg(feature = "json")]
        if let AggKind::Json(j) = kind {
            let nargs = ext.as_ref().map_or(all.len(), |x| x.nargs);
            let k = json_agg_args(j, distinct, nargs, &mut all);
            if let Some(x) = ext.as_mut() {
                x.nargs = k;
            }
        }
        return Ok(RExpr::Agg {
            kind,
            distinct,
            star,
            args: all,
            lvl,
            inert: false,
            alias: false,
            #[cfg(feature = "v16")]
            ext,
        });
    }
    let Some(f) = scalar_fn(name) else {
        return Err(Error::Schema {
            kind: SchemaKind::UnknownFunction,
        });
    };
    #[cfg(feature = "v16")]
    let want = if f == ScalarFn::Likely {
        let n = if eq_ci(name, b"likelihood") { 2 } else { 1 };
        (n, n)
    } else {
        arity(f)
    };
    #[cfg(not(feature = "v16"))]
    let want = arity(f);
    // `ifnull` is coalesce with exactly two arguments on the pin
    // (v0.16 review fix: `ifnull(1)` / `ifnull(NULL, NULL, 5)`
    // used to run).
    let want = if f == ScalarFn::Coalesce && eq_ci(name, b"ifnull") {
        (2, 2)
    } else {
        want
    };
    // `f(*)` is `f()` for a scalar function and DISTINCT is
    // ignored, as on the pin (v0.16, `v16`): `random(*)`,
    // `printf(*)` (NULL), `abs(DISTINCT -1)` (1). The argument
    // count is checked as for the plain call (`abs(*)` errors).
    #[cfg(not(feature = "v16"))]
    if star || distinct {
        return Err(Error::Unsupported);
    }
    if args.len() < want.0 || args.len() > want.1 {
        return Err(Error::Schema {
            kind: SchemaKind::WrongArgumentCount,
        });
    }
    // likelihood(X, P): P must be a REAL literal in 0.0 ..= 1.0
    // (the pin's `TK_FLOAT` test; `-0.0` and integers fail).
    #[cfg(feature = "v16")]
    if f == ScalarFn::Likely
        && args.len() == 2
        && !matches!(args.get(1), Some(Expr::Literal(Literal::Real(r)))
            if (0.0..=1.0).contains(r) && !r.is_sign_negative())
    {
        return Err(Error::Schema {
            kind: SchemaKind::LikelihoodNotConstant,
        });
    }
    Ok(RExpr::Scalar(
        f,
        resolve_args::<E>(args, arena, cur, enclosing, &mut *planner)?,
    ))
}

/// Whether two resolved expressions are the same expression in
/// the pin's `sqlite3ExprCompare` sense (v0.16, `v16`; used for
/// the DISTINCT aggregate whose ORDER BY is its argument): the
/// same column (whatever qualifier named it), literal, operator,
/// function or COLLATE, recursively. Anything else — CASE,
/// subqueries, lists — compares unequal (the general form).
/// Also decides whether an ORDER BY names the GROUP BY terms
/// (`session::group_cap`).
pub fn same_expr(a: &RExpr, b: &RExpr) -> bool {
    let all = |x: &[RExpr], y: &[RExpr]| {
        x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same_expr(p, q))
    };
    match (a, b) {
        (RExpr::Literal(x), RExpr::Literal(y)) => match (x, y) {
            (OwnedValue::Null, OwnedValue::Null) => true,
            (OwnedValue::Integer(p), OwnedValue::Integer(q)) => p == q,
            (OwnedValue::Real(p), OwnedValue::Real(q)) => p.to_bits() == q.to_bits(),
            (OwnedValue::Text(p), OwnedValue::Text(q))
            | (OwnedValue::Blob(p), OwnedValue::Blob(q)) => p.as_slice() == q.as_slice(),
            _ => false,
        },
        (RExpr::Column(i, _, c), RExpr::Column(j, _, d)) => i == j && c == d,
        (RExpr::Rowid(i), RExpr::Rowid(j)) => i == j,
        (RExpr::Binary(o, x1, y1), RExpr::Binary(p, x2, y2)) => {
            o == p && same_expr(x1, x2) && same_expr(y1, y2)
        }
        (RExpr::Neg(x), RExpr::Neg(y))
        | (RExpr::Pos(x), RExpr::Pos(y))
        | (RExpr::Not(x), RExpr::Not(y)) => same_expr(x, y),
        (RExpr::IsNull(x, n), RExpr::IsNull(y, m)) => n == m && same_expr(x, y),
        (RExpr::Cast(x, f), RExpr::Cast(y, g)) => f == g && same_expr(x, y),
        (RExpr::Collate(x, c), RExpr::Collate(y, d)) => c == d && same_expr(x, y),
        (RExpr::Scalar(f, x), RExpr::Scalar(g, y)) => f == g && all(x.as_slice(), y.as_slice()),
        // Aggregates (v0.16 review fix, `window`): two windows over
        // groups ordered or partitioned by the same aggregate
        // (`OVER (ORDER BY sum(v))`) are the same window.
        #[cfg(feature = "window")]
        (
            RExpr::Agg {
                kind: k1,
                distinct: d1,
                star: s1,
                args: a1,
                lvl: l1,
                inert: i1,
                ext: e1,
                ..
            },
            RExpr::Agg {
                kind: k2,
                distinct: d2,
                star: s2,
                args: a2,
                lvl: l2,
                inert: i2,
                ext: e2,
                ..
            },
        ) => {
            k1 == k2
                && d1 == d2
                && s1 == s2
                && l1 == l2
                && i1 == i2
                && match (e1, e2) {
                    (None, None) => true,
                    (Some(x), Some(y)) => {
                        x.nargs == y.nargs
                            && x.filter == y.filter
                            && x.unique == y.unique
                            && x.order.as_slice() == y.order.as_slice()
                    }
                    _ => false,
                }
                && all(a1.as_slice(), a2.as_slice())
        }
        _ => false,
    }
}

/// The owner level of an aggregate with resolved arguments
/// `args` and `extra` (parts walked but not kept: min / max's
/// ORDER BY keys) (v0.16, the pin's `sqlite3ReferencesSrcList`
/// walk):
/// 0 when they reference the current query's sources or no
/// column at all, else 1 + the index (nearest first) of the
/// nearest enclosing scope they reference. References past the
/// current query's end belong to subqueries nested in the
/// arguments and do not count, nor do references to a group
/// that cannot own aggregates (`ScopeGroup::agg_owner`).
fn agg_level(args: &[RExpr], extra: &[RExpr], cur: &ScopeGroup, enclosing: &[ScopeGroup]) -> u8 {
    let band = |g: &ScopeGroup| {
        let (we, se) = g.end();
        RefRange {
            w: (g.offsets.as_slice().first().copied().unwrap_or(we), we),
            s: (g.first_src, se),
        }
    };
    let refs = |g: &ScopeGroup| {
        let r = band(g);
        args.iter()
            .chain(extra.iter())
            .any(|a| expr_refs_range(a, &r))
    };
    if refs(cur) {
        return 0;
    }
    for (k, g) in enclosing.iter().enumerate() {
        if g.agg_owner && refs(g) {
            return u8::try_from(k.saturating_add(1)).unwrap_or(u8::MAX);
        }
    }
    0
}

/// Visit, in pre-order, every aggregate node in `e` that
/// belongs to the query `d` subquery levels out from `e`'s own
/// (v0.16): at depth 0 the expression's own aggregates, and
/// inside nested subquery plans (depth + 1 per level; derived
/// tables and compound cores share their plan's depth) those
/// whose `lvl` reaches back to it. `f` gets the node and the
/// depth it was found at.
pub fn visit_aggs<'x>(e: &'x RExpr, d: u8, f: &mut dyn FnMut(&'x RExpr, u8)) {
    visit_all_aggs(e, d, &mut |a, dd| {
        if matches!(a, RExpr::Agg { lvl, .. } if *lvl == dd) {
            f(a, dd);
        }
    });
}

/// Whether plan `p` holds an aggregate that an enclosing query
/// owns, other than an inert one (v0.16 review fix): its value
/// changes per owner group, so the plan is correlated even with
/// no column reference (`HAVING (SELECT s) > 1` over `count(*)
/// AS s` was cached as uncorrelated, one count for every group).
pub fn plan_has_foreign_agg(p: &SubPlan) -> bool {
    let mut any = false;
    visit_plan_all_aggs(p, 0, &mut |a, d| {
        any |= matches!(a, RExpr::Agg { lvl, inert: false, .. } if *lvl > d);
    });
    any
}

/// `visit_aggs` without the owner filter: every aggregate node,
/// with the subquery depth it was found at (v0.16).
fn visit_all_aggs<'x>(e: &'x RExpr, d: u8, f: &mut dyn FnMut(&'x RExpr, u8)) {
    let visit_aggs = visit_all_aggs;
    let visit_plan_aggs = visit_plan_all_aggs;
    match e {
        RExpr::Agg { args, .. } => {
            f(e, d);
            for a in args.iter() {
                visit_aggs(a, d, f);
            }
        }
        RExpr::SubScalar(p) | RExpr::SubExists(p) => visit_plan_aggs(p, d.saturating_add(1), f),
        RExpr::InSub(x, p, _) => {
            visit_aggs(x, d, f);
            visit_plan_aggs(p, d.saturating_add(1), f);
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => {}
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            visit_aggs(a, d, f);
            visit_aggs(b, d, f);
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => visit_aggs(a, d, f),
        RExpr::InList(x, l, ..) => {
            visit_aggs(x, d, f);
            for i in l.iter() {
                visit_aggs(i, d, f);
            }
        }
        RExpr::Between(x, a, b, _) => {
            visit_aggs(x, d, f);
            visit_aggs(a, d, f);
            visit_aggs(b, d, f);
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                visit_aggs(b, d, f);
            }
            for (w, t) in whens.iter() {
                visit_aggs(w, d, f);
                visit_aggs(t, d, f);
            }
            if let Some(o) = otherwise {
                visit_aggs(o, d, f);
            }
        }
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => {
            for a in args.iter() {
                visit_aggs(a, d, f);
            }
        }
    }
}

/// `visit_all_aggs` over every expression of a plan (v0.16).
fn visit_plan_all_aggs<'x>(p: &'x SubPlan, d: u8, f: &mut dyn FnMut(&'x RExpr, u8)) {
    let visit_aggs = visit_all_aggs;
    let visit_plan_aggs = visit_plan_all_aggs;
    for s in p.sources.iter() {
        if let Some(on) = &s.on {
            visit_aggs(on, d, f);
        }
        if let Some(dp) = &s.derived {
            visit_plan_aggs(dp, d, f);
        }
    }
    for e in p.items.iter() {
        visit_aggs(e, d, f);
    }
    if let Some(e) = &p.filter {
        visit_aggs(e, d, f);
    }
    for e in p.group_by.iter() {
        visit_aggs(e, d, f);
    }
    if let Some(e) = &p.having {
        visit_aggs(e, d, f);
    }
    for (e, _) in p.order_by.iter() {
        visit_aggs(e, d, f);
    }
    for row in p.values.iter() {
        for e in row.iter() {
            visit_aggs(e, d, f);
        }
    }
    for (_, t) in p.compound.iter() {
        visit_plan_aggs(t, d, f);
    }
}

/// Whether a resolved expression holds an aggregate of its own
/// query: one directly in it, or (v0.16) one inside a nested
/// subquery whose arguments reference only this query's columns
/// (`visit_aggs`). Aggregates a subquery owns are not this
/// query's, and neither are the inert ones of an EXISTS
/// subquery's result list (see `RExpr::Agg::inert`).
pub fn has_agg(e: &RExpr) -> bool {
    let mut any = false;
    visit_aggs(e, 0, &mut |a, _| any |= !is_inert(a));
    any
}

/// `has_agg` counting inert aggregates too (v0.16 review fix):
/// whether a result-list term makes its query an aggregate
/// query. On the pin `SELECT EXISTS(SELECT max(t.a) FROM u)
/// FROM t` aggregates t to one row, though the EXISTS never
/// evaluates max(t.a).
pub fn has_result_agg(e: &RExpr) -> bool {
    let mut any = false;
    visit_aggs(e, 0, &mut |_, _| any = true);
    any
}

/// Whether `e` is an inert aggregate node (`RExpr::Agg::inert`).
fn is_inert(e: &RExpr) -> bool {
    matches!(e, RExpr::Agg { inert: true, .. })
}

/// Mark the enclosing queries' aggregates in an EXISTS
/// subquery's result list and ORDER BY inert (v0.16 review
/// fix; the pin never evaluates those terms of an EXISTS, so
/// `WHERE EXISTS (SELECT max(t.a) FROM u)` runs, and is true
/// when u has rows — the EXISTS is not an aggregate query of
/// its own). Only a plan whose compound operators are all
/// UNION / UNION ALL qualifies (every core of it, which keeps
/// its emptiness): under INTERSECT / EXCEPT the values decide
/// the result and the pin reports the misuse.
pub fn mark_exists_inert(p: &mut SubPlan) {
    if !p.compound.iter().all(|(op, _)| {
        matches!(
            op,
            crate::sql::CompoundOp::Union | crate::sql::CompoundOp::UnionAll
        )
    }) {
        return;
    }
    fn mark(e: &mut RExpr, d: u8) {
        let _ = walk_aggs_mut::<()>(e, d, &mut |a, dd| {
            if let RExpr::Agg { lvl, inert, .. } = a
                && *lvl > dd
            {
                *inert = true;
            }
            Ok(false)
        });
    }
    fn core(p: &mut SubPlan) {
        for e in p.items.as_mut_slice() {
            mark(e, 0);
        }
        for (e, _) in p.order_by.as_mut_slice() {
            mark(e, 0);
        }
    }
    core(p);
    for (_, t) in p.compound.as_mut_slice() {
        core(t);
    }
}

/// Whether a WHERE expression `e` holds an aggregate the pin
/// refuses there (v0.16): any of this query's own (directly or
/// inside a subquery), and — when this query is not an
/// aggregate query (`allow_foreign` false) — any enclosing
/// query's directly in it ("misuse of aggregate function": a
/// non-aggregate query's WHERE resolves with aggregates
/// disallowed).
pub fn where_agg(e: &RExpr, allow_foreign: bool) -> bool {
    has_agg(e)
        || (!allow_foreign && direct_agg(e, &|a| !matches!(a, RExpr::Agg { alias: true, .. })))
}

/// Whether `e` holds an aggregate node of any query outside a
/// nested subquery (v0.16).
fn has_direct_agg(e: &RExpr) -> bool {
    direct_agg(e, &|_| true)
}

/// Whether `e` holds an aggregate node satisfying `p` outside
/// any nested subquery (v0.16).
fn direct_agg(e: &RExpr, p: &dyn Fn(&RExpr) -> bool) -> bool {
    let f = |x: &RExpr| direct_agg(x, p);
    match e {
        RExpr::Agg { args, .. } => p(e) || args.iter().any(f),
        RExpr::SubScalar(_) | RExpr::SubExists(_) => false,
        RExpr::InSub(x, _, _) => f(x),
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => false,
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
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => args.iter().any(f),
    }
}

/// Whether `e` holds an aggregate of an enclosing query inside a
/// nested subquery that this query owns (v0.16): its group
/// evaluation must substitute it before executing the subquery.
pub fn has_owned_sub_agg(e: &RExpr) -> bool {
    let mut any = false;
    visit_aggs(e, 0, &mut |a, d| any |= d > 0 && !is_inert(a));
    any
}

/// Whether an aggregate's arguments hold another aggregate of
/// the same query (v0.16; the pin's "misuse of aggregate
/// function": `sum(max(a))`, `sum((SELECT max(t.a) FROM u))`).
pub fn has_nested_agg(e: &RExpr) -> bool {
    let mut bad = false;
    visit_aggs(e, 0, &mut |a, d| {
        if d == 0
            && let RExpr::Agg { args, .. } = a
        {
            bad |= args.iter().any(has_agg);
        }
    });
    bad
}

/// Collect this query's aggregate nodes (for the min/max
/// bare-column rule), in `visit_aggs` order: its own, and
/// (v0.16) its aggregates inside nested subqueries — the pin
/// sets its min/max flag on the owning query.
pub fn collect_aggs<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    visit_aggs(e, 0, &mut |a, _| out.push(a));
}

/// Replace every aggregate of the current query inside nested
/// subquery plans of `e` (depth >= 1, `visit_aggs`) by the
/// value `fold` gives it (v0.16): the owner's group evaluation,
/// before it executes those subqueries. Inert aggregates stay
/// (they evaluate as NULL, and nothing reads them).
pub fn subst_owned_aggs<E>(
    e: &mut RExpr,
    d: u8,
    fold: &mut dyn FnMut(&RExpr) -> Result<OwnedValue, E>,
) -> Result<(), E> {
    walk_aggs_mut::<E>(e, d, &mut |a, dd| {
        if let RExpr::Agg {
            lvl, inert: false, ..
        } = a
            && *lvl == dd
            && dd > 0
        {
            let v = fold(a)?;
            *a = RExpr::Literal(v);
            return Ok(true);
        }
        Ok(false)
    })
}

/// Walk every node of `e` (at subquery depth `d`, + 1 per
/// nested plan as in `visit_aggs`), calling `f` on each
/// aggregate node first; `f` returning true means it replaced
/// the node, which is then not descended into (v0.16).
fn walk_aggs_mut<E>(
    e: &mut RExpr,
    d: u8,
    f: &mut dyn FnMut(&mut RExpr, u8) -> Result<bool, E>,
) -> Result<(), E> {
    let fold = f;
    if matches!(e, RExpr::Agg { .. }) && fold(e, d)? {
        return Ok(());
    }
    match e {
        RExpr::SubScalar(p) | RExpr::SubExists(p) => {
            walk_plan_aggs_mut(p, d.saturating_add(1), fold)
        }
        RExpr::InSub(x, p, _) => {
            walk_aggs_mut(x, d, fold)?;
            walk_plan_aggs_mut(p, d.saturating_add(1), fold)
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => Ok(()),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => {
            walk_aggs_mut(a, d, fold)?;
            walk_aggs_mut(b, d, fold)
        }
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => walk_aggs_mut(a, d, fold),
        RExpr::InList(x, l, ..) => {
            walk_aggs_mut(x, d, fold)?;
            for i in l.as_mut_slice() {
                walk_aggs_mut(i, d, fold)?;
            }
            Ok(())
        }
        RExpr::Between(x, a, b, _) => {
            walk_aggs_mut(x, d, fold)?;
            walk_aggs_mut(a, d, fold)?;
            walk_aggs_mut(b, d, fold)
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                walk_aggs_mut(b, d, fold)?;
            }
            for (w, t) in whens.as_mut_slice() {
                walk_aggs_mut(w, d, fold)?;
                walk_aggs_mut(t, d, fold)?;
            }
            if let Some(o) = otherwise {
                walk_aggs_mut(o, d, fold)?;
            }
            Ok(())
        }
        RExpr::Scalar(_, args) | RExpr::Agg { args, .. } | RExpr::Win(_, args) => {
            for a in args.as_mut_slice() {
                walk_aggs_mut(a, d, fold)?;
            }
            Ok(())
        }
    }
}

/// `walk_aggs_mut` over every expression of a plan (v0.16).
fn walk_plan_aggs_mut<E>(
    p: &mut SubPlan,
    d: u8,
    fold: &mut dyn FnMut(&mut RExpr, u8) -> Result<bool, E>,
) -> Result<(), E> {
    for s in p.sources.as_mut_slice() {
        if let Some(on) = &mut s.on {
            walk_aggs_mut(on, d, fold)?;
        }
        if let Some(dp) = &mut s.derived {
            walk_plan_aggs_mut(dp, d, fold)?;
        }
    }
    for e in p.items.as_mut_slice() {
        walk_aggs_mut(e, d, fold)?;
    }
    if let Some(e) = &mut p.filter {
        walk_aggs_mut(e, d, fold)?;
    }
    for e in p.group_by.as_mut_slice() {
        walk_aggs_mut(e, d, fold)?;
    }
    if let Some(e) = &mut p.having {
        walk_aggs_mut(e, d, fold)?;
    }
    for (e, _) in p.order_by.as_mut_slice() {
        walk_aggs_mut(e, d, fold)?;
    }
    for row in p.values.as_mut_slice() {
        for e in row.as_mut_slice() {
            walk_aggs_mut(e, d, fold)?;
        }
    }
    for (_, t) in p.compound.as_mut_slice() {
        walk_plan_aggs_mut(t, d, fold)?;
    }
    Ok(())
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
        | RExpr::Raise(_) => false,
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
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => args.iter().any(has_sub),
        RExpr::Agg { args, .. } => args.iter().any(has_sub),
    }
}

/// Collect the current-scope subquery nodes of `e` in
/// substitution traversal order (v0.6): children before the
/// node itself where a node has current-scope children (only
/// `InSub` does). `substitute` consumes values in exactly this
/// order.
/// The ids of the subquery plans in `e`, at any depth, whose FROM
/// clause (of any compound arm) names the table whose root page is
/// `root` directly (v0.16 review fix, `triggers` / `foreign_keys`):
/// the pin evaluates such a RETURNING subquery for every row, and
/// any other uncorrelated one once — even one that reads the table
/// through a nested subquery, a derived table, a view or a CTE
/// (probed). See `Statement::eval_returning`.
#[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
pub fn subs_reading(e: &RExpr, root: u32, out: &mut AVec<usize>) {
    let mut subs = AVec::new();
    collect_subs(e, &mut subs);
    for k in subs.iter() {
        let p = match k {
            SubKind::Scalar(p) | SubKind::Exists(p) | SubKind::In(p) => *p,
        };
        plan_subs_reading(p, root, out);
    }
}

/// `subs_reading` over one plan and the plans inside it.
#[cfg(all(feature = "v07", any(feature = "triggers", feature = "foreign_keys")))]
fn plan_subs_reading(p: &SubPlan, root: u32, out: &mut AVec<usize>) {
    let mut direct = false;
    let mut cores: AVec<&SubPlan> = AVec::new();
    cores.push(p);
    for (_, c) in p.compound.iter() {
        cores.push(c);
    }
    for c in cores.iter() {
        for s in c.sources.iter() {
            match &s.derived {
                Some(d) => plan_subs_reading(d, root, out),
                #[cfg(feature = "v16")]
                None if s.rec != 0 => {}
                None => direct |= s.root == root,
            }
            if let Some(on) = &s.on {
                subs_reading(on, root, out);
            }
        }
        for e in c.items.iter().chain(c.group_by.iter()) {
            subs_reading(e, root, out);
        }
        for e in c.filter.iter().chain(c.having.iter()) {
            subs_reading(e, root, out);
        }
        for (e, _) in c.order_by.iter() {
            subs_reading(e, root, out);
        }
        for r in c.values.iter() {
            for e in r.iter() {
                subs_reading(e, root, out);
            }
        }
    }
    for e in p.limit.iter().chain(p.offset.iter()) {
        subs_reading(e, root, out);
    }
    if direct {
        out.push(p.id);
    }
}

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
        | RExpr::Raise(_) => {}
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
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => {
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

/// An IN subquery node after substitution: an `InList` (marked
/// as substituted) of the subquery's values, with the left
/// operand `sx` (the substituted `x`) carrying the membership
/// collation (`in_sub_lhs`). A row-value left operand (v0.16,
/// `v16`) gets one collation per element and the row-major
/// values regrouped into rows of its size, each element
/// carrying its result column's affinity.
fn in_sub_list(x: &RExpr, plan: &SubPlan, sx: RExpr, value: &SubValue, neg: bool) -> RExpr {
    if let SubValue::Fail(k) = value {
        return RExpr::Raise(*k);
    }
    let wrap = |v: &OwnedValue, aff: Option<Affinity>| match aff {
        Some(a) => RExpr::AffLiteral(v.clone(), Some(a), None),
        None => RExpr::Literal(v.clone()),
    };
    #[cfg(feature = "v16")]
    if let (RExpr::Scalar(ScalarFn::Vector, xs), RExpr::Scalar(ScalarFn::Vector, sxs)) = (x, &sx) {
        let core = plan.compound.as_slice().last().map_or(plan, |(_, t)| t);
        let n = xs.len().max(1);
        let mut lhs = AVec::new();
        let mut affs = AVec::new();
        for (j, (xe, se)) in xs.iter().zip(sxs.iter()).enumerate() {
            let item = core.items.as_slice().get(j).or_else(|| {
                core.values
                    .as_slice()
                    .first()
                    .and_then(|r| r.as_slice().get(j))
            });
            let coll = match item {
                Some(i) => binary_coll(xe, i),
                None => expr_coll(xe).unwrap_or_default(),
            };
            lhs.push(RExpr::Collate(Box::new(se.clone()), coll));
            affs.push(sub_col_affinity(plan, j));
        }
        let vals: &[OwnedValue] = match value {
            SubValue::Many(vs, _) => vs.as_slice(),
            SubValue::One(..) | SubValue::Fail(_) => &[],
        };
        let mut rows = AVec::new();
        for chunk in vals.chunks(n) {
            let mut row = AVec::new();
            for (j, v) in chunk.iter().enumerate() {
                row.push(wrap(v, affs.as_slice().get(j).copied().flatten()));
            }
            rows.push(RExpr::Scalar(ScalarFn::Vector, row));
        }
        return RExpr::InList(
            Box::new(RExpr::Scalar(ScalarFn::Vector, lhs)),
            rows,
            neg,
            true,
        );
    }
    let sx = in_sub_lhs(x, plan, sx);
    let mut list = AVec::new();
    match value {
        SubValue::Many(vs, aff) => {
            for v in vs.iter() {
                list.push(wrap(v, *aff));
            }
        }
        SubValue::One(v, aff) => list.push(wrap(v, *aff)),
        SubValue::Fail(_) => {}
    }
    RExpr::InList(Box::new(sx), list, neg, true)
}

/// `values` is consumed in `collect_subs` order; the caller
/// guarantees the counts match.
pub fn substitute(e: &RExpr, values: &[SubValue], pos: &mut usize) -> RExpr {
    match e {
        RExpr::SubScalar(_) | RExpr::SubExists(_) => {
            let v = match &values[*pos] {
                SubValue::One(v, aff) => match aff {
                    Some(a) => RExpr::AffLiteral(v.clone(), Some(*a), None),
                    None => RExpr::Literal(v.clone()),
                },
                SubValue::Many(_, _) => RExpr::Literal(OwnedValue::Null),
                SubValue::Fail(k) => RExpr::Raise(*k),
            };
            *pos += 1;
            v
        }
        RExpr::InSub(x, plan, neg) => {
            let sx = substitute(x, values, pos);
            let v = &values[*pos];
            *pos += 1;
            in_sub_list(x, plan, sx, v, *neg)
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => e.clone(),
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
        #[cfg(feature = "window")]
        RExpr::Win(c, args) => {
            let mut out = AVec::new();
            for a in args.iter() {
                out.push(substitute(a, values, pos));
            }
            RExpr::Win(c.clone(), out)
        }
        #[cfg(not(feature = "window"))]
        RExpr::Win(c, _) => match *c {},
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
            lvl,
            inert,
            alias,
            #[cfg(feature = "v16")]
            ext,
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
                lvl: *lvl,
                inert: *inert,
                alias: *alias,
                #[cfg(feature = "v16")]
                ext: ext.clone(),
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
        | RExpr::Raise(_) => false,
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
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => args.iter().any(sub_in_agg_args),
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
        | RExpr::Raise(_) => {}
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
        RExpr::Scalar(_, args) | RExpr::Win(_, args) => {
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
                    Some(a) => RExpr::AffLiteral(v.clone(), Some(*a), None),
                    None => RExpr::Literal(v.clone()),
                },
                SubValue::Many(_, _) => RExpr::Literal(OwnedValue::Null),
                SubValue::Fail(k) => RExpr::Raise(*k),
            };
            *pos += 1;
            v
        }
        RExpr::InSub(x, plan, neg) => {
            let sx = substitute_group(x, values, pos, agg_values, agg_pos);
            let v = &values[*pos];
            *pos += 1;
            in_sub_list(x, plan, sx, v, *neg)
        }
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::Raise(_) => e.clone(),
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
        #[cfg(feature = "window")]
        RExpr::Win(c, args) => {
            let mut out = AVec::new();
            for a in args.iter() {
                out.push(substitute_group(a, values, pos, agg_values, agg_pos));
            }
            RExpr::Win(c.clone(), out)
        }
        #[cfg(not(feature = "window"))]
        RExpr::Win(c, _) => match *c {},
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
///
/// Like the pin's `sqlite3AtoF`, this reads bytes: what follows the
/// prefix is never looked at (`x'38ff' + 0` is 8 — it used to be 0
/// because the whole buffer had to be UTF-8), and only the ASCII
/// spaces `sqlite3Isspace` knows are skipped (v0.16 fix, ungated).
pub fn parse_num_prefix(b: &[u8]) -> Option<Num> {
    let mut i = 0usize;
    while i < b.len() && matches!(b[i], b' ' | b'\t'..=b'\r') {
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
    // Sign, digits, '.' and exponent only: always ASCII.
    let text = core::str::from_utf8(b.get(start..i)?).ok()?;
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
pub(crate) fn int_value(v: &OwnedValue) -> i64 {
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
/// `%!.*f` from its own digits and reads the text back
/// (`record::round_digits`).
fn round_real(r: f64, n: i64) -> f64 {
    const LIM: f64 = 4_503_599_627_370_496.0;
    if r.is_nan() || !(-LIM..=LIM).contains(&r) {
        return r;
    }
    if n <= 0 {
        return (if r < 0.0 { r - 0.5 } else { r + 0.5 }) as i64 as f64;
    }
    crate::record::round_digits(r, n)
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

/// Connection state the evaluator reads (v0.16, `v16`): the
/// pin's `last_insert_rowid()`, `changes()`, `total_changes()`
/// and the random stream behind `random()` / `randomblob()`. One
/// per connection; the cells are updated from `&self` paths (a
/// statement's writes, the random draws), never through global
/// state. Without `v16` it is empty.
#[derive(Debug)]
pub struct Env {
    #[cfg(feature = "v16")]
    pub last_rowid: core::cell::Cell<i64>,
    #[cfg(feature = "v16")]
    pub changes: core::cell::Cell<i64>,
    #[cfg(feature = "v16")]
    pub total_changes: core::cell::Cell<i64>,
    #[cfg(feature = "v16")]
    pub prng: crate::func::Prng,
    /// Rows the running INSERT / UPDATE / DELETE has changed so
    /// far (moved to `changes` when it succeeds).
    #[cfg(feature = "v16")]
    pub pending: core::cell::Cell<i64>,
    /// `PRAGMA case_sensitive_like` (v0.16): LIKE and `like()`
    /// compare ASCII letters case-sensitively while set.
    #[cfg(feature = "v16")]
    pub case_like: core::cell::Cell<bool>,
    /// The message of the last `RAISE(...)` evaluated (v0.16,
    /// `triggers`; `Connection::raise_message`).
    #[cfg(feature = "triggers")]
    pub raise_msg: core::cell::RefCell<AVec<u8>>,
}

impl Env {
    /// A fresh connection's state: no rows inserted or changed,
    /// the random stream seeded from `seed` (`Config::nonce_seed`).
    pub fn new(seed: u64) -> Env {
        let _ = seed;
        Env {
            #[cfg(feature = "v16")]
            last_rowid: core::cell::Cell::new(0),
            #[cfg(feature = "v16")]
            changes: core::cell::Cell::new(0),
            #[cfg(feature = "v16")]
            total_changes: core::cell::Cell::new(0),
            #[cfg(feature = "v16")]
            prng: crate::func::Prng::new(seed),
            #[cfg(feature = "v16")]
            pending: core::cell::Cell::new(0),
            #[cfg(feature = "v16")]
            case_like: core::cell::Cell::new(false),
            #[cfg(feature = "triggers")]
            raise_msg: core::cell::RefCell::new(AVec::new()),
        }
    }

    /// One row inserted with `rowid` (an INSERT, the REPLACE
    /// verb, an upsert's insert): counted, and the new
    /// `last_insert_rowid()`.
    #[cfg(feature = "v16")]
    pub fn note_insert(&self, rowid: i64) {
        self.last_rowid.set(rowid);
        self.note_change();
    }

    /// One row updated (UPDATE, upsert DO UPDATE) or deleted
    /// (DELETE). A REPLACE conflict's deletions are not counted,
    /// as on the pin.
    #[cfg(feature = "v16")]
    pub fn note_change(&self) {
        self.pending.set(self.pending.get().saturating_add(1));
    }

    /// The end of an INSERT / UPDATE / DELETE: on success its
    /// count becomes `changes()` and joins `total_changes()`; a
    /// failed statement leaves `changes()` 0.
    #[cfg(feature = "v16")]
    pub fn finish(&self, ok: bool) {
        let n = if ok { self.pending.get() } else { 0 };
        self.changes.set(n);
        self.total_changes
            .set(self.total_changes.get().saturating_add(n));
        self.pending.set(0);
    }
}

/// Evaluation context: a single row, or an aggregate group (bare
/// columns read the representative row; `Agg` nodes fold the
/// group's rows), plus the connection state (`Env`) the
/// connection-state functions read — absent where an expression
/// is evaluated outside any connection (they then refuse with
/// `Unsupported`).
#[derive(Clone, Copy)]
pub struct Ctx<'x> {
    rows: Option<&'x [RowData]>,
    rep: &'x RowData,
    env: Option<&'x Env>,
    /// LIKE compares case-sensitively without the connection
    /// state at hand (v0.16: a generated column's computation).
    #[cfg(feature = "v16")]
    like_cs: bool,
}

impl<'x> Ctx<'x> {
    /// A single-row context.
    pub fn of(r: &'x RowData) -> Ctx<'x> {
        Ctx {
            rows: None,
            rep: r,
            env: None,
            #[cfg(feature = "v16")]
            like_cs: false,
        }
    }
    /// The same context with `PRAGMA case_sensitive_like` set to
    /// `on` (v0.16).
    #[cfg(feature = "v16")]
    pub fn case_like(mut self, on: bool) -> Ctx<'x> {
        self.like_cs = on;
        self
    }
    /// An aggregate group's context: `rows` folds, `rep` supplies
    /// the bare columns.
    pub fn group(rows: &'x [RowData], rep: &'x RowData) -> Ctx<'x> {
        Ctx {
            rows: Some(rows),
            rep,
            env: None,
            #[cfg(feature = "v16")]
            like_cs: false,
        }
    }
    /// The same context reading the connection state `env`.
    pub fn with(mut self, env: &'x Env) -> Ctx<'x> {
        self.env = Some(env);
        self
    }
    fn row(&self) -> &'x RowData {
        self.rep
    }
    /// A row context over one of the group's rows, for evaluating
    /// aggregate arguments per group row (same connection state).
    fn as_row(&self, r: &'x RowData) -> Ctx<'x> {
        Ctx {
            rows: None,
            rep: r,
            env: self.env,
            #[cfg(feature = "v16")]
            like_cs: self.like_cs,
        }
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
        RExpr::Column(_, aff, _) | RExpr::Cast(_, aff) | RExpr::AffLiteral(_, Some(aff), _) => {
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
///
/// The pin's `OP_Eq` family applies the chosen affinity to BOTH
/// operands (NUMERIC to a string, TEXT to a number when either
/// operand is a string), which for a value read from a column
/// of that affinity changes nothing — but a compound subquery's
/// value may come from a core whose column has another affinity
/// (v0.16 review fix: `(SELECT 1 UNION SELECT t FROM c) = 1`
/// over TEXT t is 1 on the pin, hipp gave 0).
fn adjust_cmp(l: &RExpr, r: &RExpr, lv: OwnedValue, rv: OwnedValue) -> (OwnedValue, OwnedValue) {
    adjust_classes(cmp_class(l), cmp_class(r), lv, rv)
}

/// `adjust_cmp` over the operands' comparison classes.
#[inline(always)]
fn adjust_classes(
    lc: CmpClass,
    rc: CmpClass,
    lv: OwnedValue,
    rv: OwnedValue,
) -> (OwnedValue, OwnedValue) {
    let aff = if lc == CmpClass::Numeric || rc == CmpClass::Numeric {
        Affinity::Numeric
    } else if (lc == CmpClass::Text && rc == CmpClass::None)
        || (rc == CmpClass::Text && lc == CmpClass::None)
    {
        if !matches!(lv, OwnedValue::Text(_)) && !matches!(rv, OwnedValue::Text(_)) {
            return (lv, rv);
        }
        Affinity::Text
    } else {
        return (lv, rv);
    };
    (apply_cmp_affinity(lv, aff), apply_cmp_affinity(rv, aff))
}

/// `l = r` as a comparison of two values whose operands carry the
/// comparison affinities `la` / `ra` (`None`: no affinity), under
/// `coll` (v0.16, `foreign_keys`: the pin's foreign-key scans and
/// action programs compare a parent value with a child column so).
/// NULL equals nothing.
#[cfg(feature = "foreign_keys")]
pub(crate) fn fk_eq(
    l: &OwnedValue,
    la: Option<Affinity>,
    r: &OwnedValue,
    ra: Option<Affinity>,
    coll: Coll,
) -> bool {
    if matches!(l, OwnedValue::Null) || matches!(r, OwnedValue::Null) {
        return false;
    }
    let lc = la.map_or(CmpClass::None, class_of_affinity);
    let rc = ra.map_or(CmpClass::None, class_of_affinity);
    let (lv, rv) = adjust_classes(lc, rc, l.clone(), r.clone());
    compare_coll(&lv.as_value(), &rv.as_value(), coll) == core::cmp::Ordering::Equal
}

/// The affinity conversion of an IN over a subquery's set
/// (v0.16 review fix): the set takes each column's comparison
/// affinity from the left operand and the subquery's
/// (`exprINAffinity`) and applies it to BOTH sides, as column
/// affinity does — the left operand when it probes, every
/// set value as it is stored — so TEXT stringifies a number
/// even against a number (`1 IN (VALUES (1.0), (t))` over TEXT
/// t is 0 on the pin). A row-value IN list is such a set too:
/// the pin rewrites `(a, b) IN ((x, y), ...)` as an IN over
/// `VALUES`, so `(1, 1) IN ((x, y), (a, b))` with INTEGER x, y
/// and TEXT a, b converts the literals and x, y to TEXT, and
/// the first row matches (only the left operand was converted,
/// a silent 0).
fn adjust_set(l: &RExpr, r: &RExpr, lv: OwnedValue, rv: OwnedValue) -> (OwnedValue, OwnedValue) {
    let aff = match (cmp_class(l), cmp_class(r)) {
        (CmpClass::None, CmpClass::None) => return (lv, rv),
        (CmpClass::Numeric, _) | (_, CmpClass::Numeric) => Affinity::Numeric,
        (CmpClass::None, CmpClass::Text) | (CmpClass::Text, CmpClass::None) => Affinity::Text,
        // Two affinities, neither numeric — or BLOB against
        // none: the pin compares as stored.
        _ => return (lv, rv),
    };
    (apply_cmp_affinity(lv, aff), apply_cmp_affinity(rv, aff))
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
        | RExpr::Raise(_)
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
        RExpr::Scalar(_, args) => args.iter().any(f),
        RExpr::Win(..) => false,
        RExpr::Agg { .. } => agg_parts(e).is_some_and(|p| p.args.iter().any(f)),
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
        RExpr::Scalar(_, args) => first(args.as_slice()).and_then(|i| args.as_slice().get(i)),
        RExpr::Agg { .. } => {
            let args = agg_parts(e).map(|p| p.args).unwrap_or(&[]);
            first(args).and_then(|i| args.get(i))
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
            #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
            RExpr::AffLiteral(_, _, Some(c)) => return Some(*c),
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
        | RExpr::Raise(_)
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
        // RAISE() is never folded (v0.16, `triggers`): it fails only
        // where evaluation reaches it.
        #[cfg(feature = "triggers")]
        RExpr::Scalar(ScalarFn::Raise(_), _) => false,
        RExpr::Scalar(_, args) => args.iter().all(is_constant),
        RExpr::Win(..) => false,
    }
}

/// An integer literal's truth (`None` for any other expression).
/// An AND over a false one is false too, an OR over a true one true
/// (the pin's simplified operator is such a literal again).
fn int_lit(e: &RExpr) -> Option<bool> {
    match e {
        RExpr::Literal(OwnedValue::Integer(i)) => Some(*i != 0),
        RExpr::Binary(BinOp::And, a, b)
            if int_lit(a) == Some(false) || int_lit(b) == Some(false) =>
        {
            Some(false)
        }
        RExpr::Binary(BinOp::Or, a, b) if int_lit(a) == Some(true) || int_lit(b) == Some(true) => {
            Some(true)
        }
        _ => None,
    }
}

/// Whether `e` holds a failed subquery's deferred error
/// (`RExpr::Raise`, v0.16 review fix). The pin codes an AND /
/// OR whose operand has a subquery as a short-circuit jump, so
/// `0 AND (SELECT abs(-9223372036854775808))` is 0 there; without
/// a subquery both operands run (`0 AND abs(b)` raises).
fn has_raise(e: &RExpr) -> bool {
    match e {
        RExpr::Raise(_) => true,
        RExpr::Literal(_)
        | RExpr::AffLiteral(..)
        | RExpr::Column(..)
        | RExpr::Rowid(_)
        | RExpr::SubScalar(_)
        | RExpr::SubExists(_)
        | RExpr::InSub(..) => false,
        RExpr::Agg { args, .. } | RExpr::Scalar(_, args) | RExpr::Win(_, args) => {
            args.iter().any(has_raise)
        }
        RExpr::Neg(x)
        | RExpr::Pos(x)
        | RExpr::Not(x)
        | RExpr::IsNull(x, _)
        | RExpr::Cast(x, _)
        | RExpr::Collate(x, _) => has_raise(x),
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => has_raise(a) || has_raise(b),
        RExpr::InList(x, l, ..) => has_raise(x) || l.iter().any(has_raise),
        RExpr::Between(x, a, b, _) => has_raise(x) || has_raise(a) || has_raise(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_some_and(has_raise)
                || whens.iter().any(|(w, t)| has_raise(w) || has_raise(t))
                || otherwise.as_deref().is_some_and(has_raise)
        }
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
    cmp_values::<E>(op, l, r, lv, rv)
}

/// `cmp_op` over its operands' values (out of line: `cmp_op` is
/// on the stack once per level of a comparison chain).
#[inline(never)]
fn cmp_values<E>(
    op: BinOp,
    l: &RExpr,
    r: &RExpr,
    lv: OwnedValue,
    rv: OwnedValue,
) -> Result<OwnedValue, E> {
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
        _ => return Err(Error::Unsupported),
    };
    Ok(bool_val(b))
}

fn arith<E>(op: BinOp, l: &RExpr, r: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let lv = eval::<E>(l, ctx)?;
    let rv = eval::<E>(r, ctx)?;
    arith_values::<E>(op, lv, rv)
}

/// `arith` over its operands' values (out of line: `arith` is on
/// the stack once per level of an arithmetic chain).
#[inline(never)]
pub(crate) fn arith_values<E>(op: BinOp, lv: OwnedValue, rv: OwnedValue) -> Result<OwnedValue, E> {
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
                    real_value(x.as_f64() / d)
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
            _ => return Err(Error::Unsupported),
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
    Ok(real_value(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        _ => return Err(Error::Unsupported),
    }))
}

/// A REAL result as the pin stores it: a NaN (`inf - inf`,
/// `0 * inf`, `inf / inf`) is NULL (`sqlite3VdbeMemSetDouble`;
/// v0.16 review fix: hipp answered a NaN REAL, which rendered
/// as 0.0 and was written to the file).
pub fn real_value(r: f64) -> OwnedValue {
    if r.is_nan() {
        OwnedValue::Null
    } else {
        OwnedValue::Real(r)
    }
}

/// Decode text the way the pin's `sqlite3Utf8Read` walks a
/// LIKE / GLOB operand (v0.16): the value ends at its first
/// NUL byte (the pin reads it as a C string); a lead byte >=
/// 0xC0 takes every following continuation byte; overlong
/// forms, surrogates and U+FFFE / U+FFFF read as U+FFFD;
/// stray continuation and ASCII bytes read as themselves.
fn pattern_chars(b: &[u8]) -> AVec<u32> {
    let mut out = AVec::new();
    let mut i = 0;
    while let Some((c, n)) = pattern_char_at(b, i) {
        out.push(c);
        i = n;
    }
    out
}

/// One character of `pattern_chars` at byte `i`: the code point
/// and the next character's byte offset; None at the end (or
/// the first NUL). The LIKE / GLOB subject is walked with this
/// directly rather than decoded into a list (v0.16 review fix:
/// four bytes per character of a subject up to the 1e9-byte
/// limit).
fn pattern_char_at(b: &[u8], i: usize) -> Option<(u32, usize)> {
    let c0 = *b.get(i)?;
    if c0 == 0 {
        return None;
    }
    let mut i = i + 1;
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
    Some((c, i))
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
fn pattern_match(pat: &[u32], s: &[u8], info: PatInfo) -> bool {
    // `i` is a byte offset into the subject, which is decoded one
    // character at a time (`pattern_char_at`).
    let (mut p, mut i) = (0usize, 0usize);
    let mut back: Option<(usize, usize)> = None;
    loop {
        let cur = pattern_char_at(s, i);
        if let Some(&c) = pat.get(p) {
            if info.all != 0 && c == info.all {
                p += 1;
                back = Some((p, i));
                continue;
            }
            if let Some(n) = element_match(pat, p, cur.map(|(c, _)| c), info) {
                p += n;
                i = cur.map_or(i, |(_, next)| next);
                continue;
            }
        } else if cur.is_none() {
            return true;
        }
        match back.and_then(|(bp, bi)| pattern_char_at(s, bi).map(|(_, next)| (bp, next))) {
            Some((bp, bn)) => {
                back = Some((bp, bn));
                p = bp;
                i = bn;
            }
            None => return false,
        }
    }
}

/// Whether `PRAGMA case_sensitive_like` is on for this
/// evaluation (v0.16; off without the connection state).
fn case_like(ctx: &Ctx<'_>) -> bool {
    #[cfg(feature = "v16")]
    {
        ctx.like_cs || ctx.env.is_some_and(|e| e.case_like.get())
    }
    #[cfg(not(feature = "v16"))]
    {
        let _ = ctx;
        false
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
    case_sensitive: bool,
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
            nocase: !case_sensitive,
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
    let p = pattern_chars(p.as_slice());
    Ok(bool_val(pattern_match(p.as_slice(), x.as_slice(), info)))
}

/// The characters of a text as the pin's C functions walk it
/// (v0.16 fix): up to the first NUL, a lead byte of 0xc0 or more
/// taking the continuation bytes after it (`SQLITE_SKIP_UTF8`),
/// any other byte one character on its own — invalid UTF-8
/// included (v0.15 read invalid UTF-8 as no characters, or as
/// bytes, and counted past a NUL).
///
/// An iterator, not a list: `length()` / `substr()` of a large
/// text walk it once without allocating (v0.16 review fix — a
/// collected list took 16 bytes per character, so
/// `length(printf('%*.*s', 999999999, 5, 'x'))` was killed for
/// memory where the pin answers).
fn char_ranges(t: &[u8]) -> CharRanges<'_> {
    CharRanges { t, i: 0 }
}

/// The byte ranges `char_ranges` yields.
struct CharRanges<'a> {
    t: &'a [u8],
    i: usize,
}

impl Iterator for CharRanges<'_> {
    type Item = (usize, usize);
    fn next(&mut self) -> Option<(usize, usize)> {
        let i = self.i;
        let c = *self.t.get(i)?;
        if c == 0 {
            return None;
        }
        let mut j = i + 1;
        if c >= 0xc0 {
            while self.t.get(j).is_some_and(|b| (*b & 0xc0) == 0x80) {
                j += 1;
            }
        }
        self.i = j;
        Some((i, j))
    }
}

/// `length(X)` (v0.16 fix): a BLOB's bytes; a TEXT's characters
/// up to its first NUL (`char_ranges`); a number's text form's
/// bytes.
fn char_len(v: &OwnedValue) -> i64 {
    match v {
        OwnedValue::Text(t) => char_ranges(t.as_slice()).count() as i64,
        OwnedValue::Blob(t) => t.len() as i64,
        OwnedValue::Integer(_) | OwnedValue::Real(_) => {
            to_text(v).map(|t| t.len()).unwrap_or(0) as i64
        }
        OwnedValue::Null => 0,
    }
}

/// The text a C function sees for a value (`sqlite3_value_text`):
/// its bytes up to the first NUL.
fn c_text(t: &[u8]) -> &[u8] {
    match t.iter().position(|b| *b == 0) {
        Some(i) => t.get(..i).unwrap_or(t),
        None => t,
    }
}

/// The pin's `instrFunc` (v0.16 fix): two BLOBs search bytes;
/// otherwise both sides as text, counting characters (a lead
/// byte's continuation bytes are skipped). An empty needle is at
/// 1; not found is 0.
fn instr_value(x: &OwnedValue, y: &OwnedValue) -> OwnedValue {
    if matches!(x, OwnedValue::Null) || matches!(y, OwnedValue::Null) {
        return OwnedValue::Null;
    }
    let is_text = !(matches!(x, OwnedValue::Blob(_)) && matches!(y, OwnedValue::Blob(_)));
    let (Some(h), Some(n)) = (to_text(x), to_text(y)) else {
        return OwnedValue::Null;
    };
    let (h, n) = (h.as_slice(), n.as_slice());
    if n.is_empty() {
        return OwnedValue::Integer(1);
    }
    let mut pos = 1i64;
    let mut i = 0usize;
    while n.len() <= h.len().saturating_sub(i) {
        if h.get(i..i + n.len()) == Some(n) {
            return OwnedValue::Integer(pos);
        }
        pos += 1;
        i += 1;
        while is_text && h.get(i).is_some_and(|b| (*b & 0xc0) == 0x80) {
            i += 1;
        }
    }
    OwnedValue::Integer(0)
}

/// The pin's `trimFunc` (v0.16 fix): the trim set is the second
/// argument's characters up to its first NUL (one space when
/// absent; nothing to trim when empty), each matched as its whole
/// byte sequence against either end of the first argument's
/// bytes. Always TEXT.
fn trim_value(f: ScalarFn, v: &[OwnedValue]) -> OwnedValue {
    if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
        return OwnedValue::Null;
    }
    let Some(text) = v.first().and_then(to_text) else {
        return OwnedValue::Null;
    };
    let set: AVec<u8> = match v.get(1) {
        Some(c) => to_text(c).unwrap_or_default(),
        None => {
            let mut c = AVec::new();
            c.push(b' ');
            c
        }
    };
    let set = c_text(set.as_slice());
    let mut z = text.as_slice();
    let hit = |z: &[u8], front: bool| -> Option<usize> {
        char_ranges(set).find_map(|(a, b)| {
            let ch = set.get(a..b)?;
            let ok = if front {
                z.starts_with(ch)
            } else {
                z.ends_with(ch)
            };
            ok.then_some(ch.len())
        })
    };
    if !set.is_empty() {
        if f != ScalarFn::Rtrim {
            while let Some(k) = hit(z, true).filter(|_| !z.is_empty()) {
                z = z.get(k..).unwrap_or(&[]);
            }
        }
        if f != ScalarFn::Ltrim {
            while let Some(k) = hit(z, false).filter(|_| !z.is_empty()) {
                z = z.get(..z.len() - k).unwrap_or(&[]);
            }
        }
    }
    let mut out = AVec::new();
    out.extend_from_slice(z);
    OwnedValue::Text(out)
}

/// Evaluate the arguments a function call needs only lazily
/// (v0.16 fix): `coalesce` / `ifnull` stop at the first non-NULL,
/// `iif` / `if` evaluate only the chosen branch, `likely` & co.
/// are their first argument — the pin compiles them inline and
/// never evaluates the rest (`coalesce(1, abs(<i64 min>))` is 1
/// there; hipp raised "integer overflow").
fn eval_lazy<E>(f: ScalarFn, args: &[RExpr], ctx: &Ctx<'_>) -> Option<Result<OwnedValue, E>> {
    match f {
        #[cfg(feature = "triggers")]
        ScalarFn::Raise(k) => Some(eval_raise::<E>(k, args, ctx)),
        ScalarFn::Coalesce => {
            for a in args {
                match eval::<E>(a, ctx) {
                    Ok(OwnedValue::Null) => {}
                    other => return Some(other),
                }
            }
            Some(Ok(OwnedValue::Null))
        }
        ScalarFn::Iif => {
            let mut i = 0usize;
            while let (Some(c), Some(t)) = (args.get(i), args.get(i + 1)) {
                match cond_case::<E>(c, ctx) {
                    Err(e) => return Some(Err(e)),
                    Ok(true) => return Some(eval::<E>(t, ctx)),
                    Ok(false) => {}
                }
                i += 2;
            }
            Some(match args.get(i) {
                Some(o) => eval::<E>(o, ctx),
                None => Ok(OwnedValue::Null),
            })
        }
        #[cfg(feature = "v16")]
        ScalarFn::Likely => args.first().map(|a| eval::<E>(a, ctx)),
        #[cfg(feature = "v16")]
        ScalarFn::Random
        | ScalarFn::Randomblob
        | ScalarFn::LastInsertRowid
        | ScalarFn::Changes
        | ScalarFn::TotalChanges => {
            let Some(env) = ctx.env else {
                return Some(Err(Error::Unsupported));
            };
            Some(match f {
                ScalarFn::Random => Ok(OwnedValue::Integer(env.prng.random())),
                ScalarFn::Randomblob => match args.first().map(|a| eval::<E>(a, ctx)) {
                    Some(Ok(n)) => env.prng.blob::<E>(&n),
                    Some(Err(e)) => Err(e),
                    None => Ok(OwnedValue::Null),
                },
                ScalarFn::LastInsertRowid => Ok(OwnedValue::Integer(env.last_rowid.get())),
                ScalarFn::Changes => Ok(OwnedValue::Integer(env.changes.get())),
                _ => Ok(OwnedValue::Integer(env.total_changes.get())),
            })
        }
        _ => None,
    }
}

fn eval_scalar<E>(f: ScalarFn, args: &[RExpr], ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    #[cfg(feature = "json")]
    if let ScalarFn::Json(j) = f {
        return eval_json_call::<E>(j, args, ctx).map(|r| r.0);
    }
    if let Some(r) = eval_lazy::<E>(f, args, ctx) {
        return r;
    }
    let mut vs: AVec<OwnedValue> = AVec::new();
    for a in args {
        vs.push(eval::<E>(a, ctx)?);
    }
    let v = vs.as_slice();
    #[cfg(feature = "math")]
    if let ScalarFn::Math(m) = f {
        return Ok(crate::math::eval(m, v));
    }
    let Some(v0) = v.first() else {
        // The argument-free calls.
        #[cfg(feature = "v16")]
        match f {
            ScalarFn::Printf => return Ok(OwnedValue::Null),
            ScalarFn::Char => return Ok(OwnedValue::Text(AVec::new())),
            ScalarFn::SqliteVersion => {
                let mut t = AVec::new();
                t.extend_from_slice(b"3.54.1");
                return Ok(OwnedValue::Text(t));
            }
            _ => {}
        }
        return Err(Error::Unsupported);
    };
    let v1 = v.get(1);
    Ok(match f {
        #[cfg(feature = "v16")]
        ScalarFn::BitNot => match v0 {
            OwnedValue::Null => OwnedValue::Null,
            x => OwnedValue::Integer(!int_value(x)),
        },
        #[cfg(feature = "v16")]
        ScalarFn::Vector => return Err(row_misused()),
        #[cfg(feature = "v16")]
        ScalarFn::Printf => crate::func::printf::<E>(v)?,
        #[cfg(feature = "v16")]
        ScalarFn::Quote => crate::func::quote::<E>(v0)?,
        #[cfg(feature = "v16")]
        ScalarFn::Char => crate::func::char_fn::<E>(v)?,
        #[cfg(feature = "v16")]
        ScalarFn::Unicode => crate::func::unicode(v0),
        #[cfg(feature = "v16")]
        ScalarFn::Zeroblob => crate::func::zeroblob::<E>(v0)?,
        #[cfg(feature = "v16")]
        ScalarFn::Concat => crate::func::concat::<E>(None, v)?,
        #[cfg(feature = "v16")]
        ScalarFn::ConcatWs => crate::func::concat::<E>(Some(v0), v.get(1..).unwrap_or(&[]))?,
        #[cfg(feature = "v16")]
        ScalarFn::OctetLength => crate::func::octet_length(v0),
        #[cfg(feature = "v16")]
        ScalarFn::Unhex => crate::func::unhex(v0, v1),
        ScalarFn::Nullif => {
            let b = v1.unwrap_or(&OwnedValue::Null);
            if !matches!(v0, OwnedValue::Null)
                && !matches!(b, OwnedValue::Null)
                && values_equal_coll(v0, b, args_coll(args))
            {
                OwnedValue::Null
            } else {
                v0.clone()
            }
        }
        ScalarFn::Typeof => {
            let name: &[u8] = match v0 {
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
        ScalarFn::Abs => match v0 {
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
            if matches!(v0, OwnedValue::Null) {
                OwnedValue::Null
            } else {
                OwnedValue::Integer(char_len(v0))
            }
        }
        // ASCII case folding over the value's text bytes (all of
        // them); a BLOB comes back as TEXT, as on the pin (v0.16
        // fix: hipp returned the BLOB unchanged).
        ScalarFn::Upper | ScalarFn::Lower => {
            let up = f == ScalarFn::Upper;
            let Some(mut t) = to_text(v0) else {
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
        ScalarFn::Hex => {
            if matches!(v0, OwnedValue::Null) {
                OwnedValue::Null
            } else {
                let bytes = to_text(v0).unwrap_or_default();
                let mut out = AVec::new();
                for b in bytes.as_slice() {
                    let (hi, lo) = (b >> 4, b & 0xf);
                    out.push(if hi < 10 { b'0' + hi } else { b'A' + hi - 10 });
                    out.push(if lo < 10 { b'0' + lo } else { b'A' + lo - 10 });
                }
                OwnedValue::Text(out)
            }
        }
        ScalarFn::Sign => match to_number(v0) {
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
            let Some(n) = to_number(v0) else {
                return Ok(OwnedValue::Null);
            };
            // The digit count is read as the pin's
            // `sqlite3_value_int64` reads it (a text's integer
            // prefix: '1e1' is 1), then clamped to 0..=30.
            let digits = match v1 {
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
            let Some(text) = to_text(v0) else {
                return Ok(OwnedValue::Null);
            };
            let blob = matches!(v0, OwnedValue::Blob(_));
            // Start and length are read as the pin's
            // `sqlite3_value_int64` reads them: a text's integer
            // prefix ('1e1' is 1), a real truncated.
            let mut start = v1.map_or(0, int_value);
            // A BLOB's units are bytes; a text's are its
            // characters up to the first NUL (v0.16 fix). Counted
            // and walked without a per-unit list (v0.16 review
            // fix: memory stays the size of the text).
            let tb = text.as_slice();
            let n = if blob {
                tb.len() as i64
            } else {
                char_ranges(tb).count() as i64
            };
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
            // The byte span of units start..end (0 <= start).
            let span = if start >= end {
                None
            } else if blob {
                Some((start as usize, end as usize))
            } else {
                let mut first = None;
                let mut last = 0usize;
                for (idx, (bs, be)) in char_ranges(tb).enumerate() {
                    let idx = idx as i64;
                    if idx >= end {
                        break;
                    }
                    if idx >= start {
                        first.get_or_insert(bs);
                        last = be;
                    }
                }
                first.map(|f| (f, last))
            };
            let mut out = AVec::new();
            if let Some(piece) = span.and_then(|(a, b)| tb.get(a..b)) {
                out.extend_from_slice(piece);
            }
            if blob {
                OwnedValue::Blob(out)
            } else {
                OwnedValue::Text(out)
            }
        }
        ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => trim_value(f, v),
        ScalarFn::Replace => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let (Some(x), Some(y), Some(z)) = (
                to_text(v0),
                v1.and_then(to_text),
                v.get(2).and_then(to_text),
            ) else {
                return Ok(OwnedValue::Null);
            };
            // The pin tests the pattern's first byte for NUL: an
            // empty or NUL-led pattern leaves X as it is (TEXT).
            if matches!(y.as_slice().first(), None | Some(0)) {
                return Ok(OwnedValue::Text(x));
            }
            let mut out = AVec::new();
            let (xb, yb) = (x.as_slice(), y.as_slice());
            let mut i = 0usize;
            while let Some(&c) = xb.get(i) {
                if xb.get(i..i + yb.len()) == Some(yb) {
                    out.extend_from_slice(z.as_slice());
                    i += yb.len();
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            OwnedValue::Text(out)
        }
        ScalarFn::Instr => instr_value(v0, v1.unwrap_or(&OwnedValue::Null)),
        #[cfg(feature = "v16")]
        ScalarFn::Like | ScalarFn::Glob => like_glob::<E>(
            v0,
            v1.unwrap_or(&OwnedValue::Null),
            v.get(2),
            f == ScalarFn::Glob,
            case_like(ctx),
        )?,
        ScalarFn::MinN | ScalarFn::MaxN => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                OwnedValue::Null
            } else {
                // v0.16: the first argument's collation decides,
                // and a tie keeps the earlier argument for max()
                // but takes the later one for min() — the pin's
                // `minmaxFunc` (`min(1, 1.0)` is 1.0).
                let coll = args_coll(args);
                let mut best = v0.clone();
                for x in v.get(1..).unwrap_or(&[]) {
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
        // Handled lazily (`eval_lazy`) or argument-free above.
        _ => return Err(Error::Unsupported),
    })
}

/// A JSON function's call (v0.16, `json`): its arguments with
/// their JSON subtype (`eval_json_arg`), then `json::eval`.
#[cfg(feature = "json")]
#[inline(never)]
fn eval_json_call<E>(
    f: crate::json::JsonFn,
    args: &[RExpr],
    ctx: &Ctx<'_>,
) -> Result<(OwnedValue, bool), E> {
    let mut vs: AVec<(OwnedValue, bool)> = AVec::with_capacity(args.len());
    for a in args {
        vs.push(eval_json_arg::<E>(a, ctx)?);
    }
    crate::json::eval(f, vs.as_slice()).map_err(|kind| Error::Schema { kind })
}

/// An expression's value and whether it carries the JSON subtype
/// (v0.16, `json`; the pin's `sqlite3_value_subtype`): a JSON
/// function's text result does, and so does a value passed on
/// unchanged by `+x`, COLLATE, CAST to TEXT, CASE, coalesce /
/// ifnull, iif, nullif, the multi-argument min / max and likely /
/// unlikely / likelihood, as on the pin; so does a JSON
/// aggregate's text result. Anything else — a column (but a
/// `json_each` value, see `json::Op::EachValue`), a subquery, any
/// other function, a value read back from a table — does not.
#[cfg(feature = "json")]
pub(crate) fn eval_json_arg<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<(OwnedValue, bool), E> {
    let plain = |x: &RExpr| -> Result<(OwnedValue, bool), E> { Ok((eval::<E>(x, ctx)?, false)) };
    match e {
        RExpr::Scalar(ScalarFn::Json(f), args) => eval_json_call::<E>(*f, args.as_slice(), ctx),
        RExpr::Pos(x) | RExpr::Collate(x, _) => eval_json_arg::<E>(x, ctx),
        RExpr::Cast(x, Affinity::Text) => {
            let (v, sub) = eval_json_arg::<E>(x, ctx)?;
            Ok(match v {
                OwnedValue::Text(_) => (v, sub),
                other => (cast_value(&other, Affinity::Text), false),
            })
        }
        RExpr::Scalar(ScalarFn::Likely, args) => match args.as_slice().first() {
            Some(a) => eval_json_arg::<E>(a, ctx),
            None => Ok((OwnedValue::Null, false)),
        },
        RExpr::Scalar(ScalarFn::Coalesce, args) => {
            let args = args.as_slice();
            for a in args {
                let r = eval_json_arg::<E>(a, ctx)?;
                if !matches!(r.0, OwnedValue::Null) {
                    return Ok(r);
                }
            }
            Ok((OwnedValue::Null, false))
        }
        RExpr::Scalar(ScalarFn::Iif, args) => {
            let args = args.as_slice();
            let mut i = 0usize;
            while let (Some(c), Some(t)) = (args.get(i), args.get(i + 1)) {
                if cond_case::<E>(c, ctx)? {
                    return eval_json_arg::<E>(t, ctx);
                }
                i += 2;
            }
            match args.get(i) {
                Some(o) => eval_json_arg::<E>(o, ctx),
                None => Ok((OwnedValue::Null, false)),
            }
        }
        RExpr::Scalar(ScalarFn::Nullif, args) => {
            let args = args.as_slice();
            let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
                return plain(e);
            };
            let (va, sa) = eval_json_arg::<E>(a, ctx)?;
            let vb = eval::<E>(b, ctx)?;
            if !matches!(va, OwnedValue::Null)
                && !matches!(vb, OwnedValue::Null)
                && values_equal_coll(&va, &vb, args_coll(args))
            {
                return Ok((OwnedValue::Null, false));
            }
            Ok((va, sa))
        }
        RExpr::Scalar(f @ (ScalarFn::MinN | ScalarFn::MaxN), args) => {
            let args = args.as_slice();
            let mut vs: AVec<(OwnedValue, bool)> = AVec::with_capacity(args.len());
            for a in args {
                vs.push(eval_json_arg::<E>(a, ctx)?);
            }
            if vs.iter().any(|x| matches!(x.0, OwnedValue::Null)) {
                return Ok((OwnedValue::Null, false));
            }
            let coll = args_coll(args);
            let mut best = 0usize;
            for (k, x) in vs.iter().enumerate().skip(1) {
                let Some(b) = vs.as_slice().get(best) else {
                    break;
                };
                let ord = compare_coll(&x.0.as_value(), &b.0.as_value(), coll);
                let take = if *f == ScalarFn::MinN {
                    ord != core::cmp::Ordering::Greater
                } else {
                    ord == core::cmp::Ordering::Greater
                };
                if take {
                    best = k;
                }
            }
            Ok(vs
                .as_slice()
                .get(best)
                .cloned()
                .unwrap_or((OwnedValue::Null, false)))
        }
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            match base {
                Some(b) => {
                    let bv = eval::<E>(b, ctx)?;
                    for (w, t) in whens.iter() {
                        let wv = eval::<E>(w, ctx)?;
                        let (bv2, wv2) = adjust_cmp(b, w, bv.clone(), wv);
                        if !matches!(bv2, OwnedValue::Null)
                            && !matches!(wv2, OwnedValue::Null)
                            && values_equal_coll(&bv2, &wv2, binary_coll(b, w))
                        {
                            return eval_json_arg::<E>(t, ctx);
                        }
                    }
                }
                None => {
                    for (w, t) in whens.iter() {
                        if cond_case::<E>(w, ctx)? {
                            return eval_json_arg::<E>(t, ctx);
                        }
                    }
                }
            }
            match otherwise {
                Some(o) => eval_json_arg::<E>(o, ctx),
                None => Ok((OwnedValue::Null, false)),
            }
        }
        RExpr::Agg {
            kind: AggKind::Json(a),
            ..
        } if !a.blob => {
            let v = eval::<E>(e, ctx)?;
            let sub = matches!(v, OwnedValue::Text(_));
            Ok((v, sub))
        }
        // An aggregate min / max is one of its argument's values,
        // subtype and all (when the argument is always JSON).
        RExpr::Agg {
            kind: AggKind::Min | AggKind::Max,
            args,
            ..
        } if matches!(args.as_slice().first(), Some(RExpr::Scalar(ScalarFn::Json(f), _)) if f.always_json()) =>
        {
            let v = eval::<E>(e, ctx)?;
            let sub = matches!(v, OwnedValue::Text(_));
            Ok((v, sub))
        }
        _ => plain(e),
    }
}

/// Fold one aggregate over a group's rows: evaluate its inputs
/// per row (synchronously; `agg_row` applies its FILTER) and fold
/// the values (`agg::fold`).
fn eval_agg<E>(e: &RExpr, rows: &[RowData], ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let Some(p) = agg_parts(e) else {
        return Err(Error::Unsupported);
    };
    let mut arg_rows: AVec<AVec<OwnedValue>> = AVec::new();
    for r in rows {
        let rc = ctx.as_row(r);
        if let Some(vs) = agg_row::<E>(&p, &mut |x| eval::<E>(x, &rc))? {
            arg_rows.push(vs);
        }
    }
    crate::agg::fold::<E>(&p, arg_rows)
}

/// Evaluate a resolved expression.
/// `&`, `|`, `<<`, `>>` on two integers (v0.16): the pin's
/// `OP_BitAnd` / `OP_BitOr` / `OP_ShiftLeft` / `OP_ShiftRight`.
/// A negative count shifts the other way; a count of 64 or more
/// leaves 0, or -1 for a negative value shifted right.
#[cfg(feature = "v16")]
fn bit_op(op: BinOp, a: i64, b: i64) -> i64 {
    let mut left = match op {
        BinOp::BitAnd => return a & b,
        BinOp::BitOr => return a | b,
        BinOp::Shl => true,
        _ => false,
    };
    if b == 0 {
        return a;
    }
    let mut n = b;
    if n < 0 {
        left = !left;
        n = if n > -64 { -n } else { 64 };
    }
    if n >= 64 {
        return if a >= 0 || left { 0 } else { -1 };
    }
    // 0 < n < 64 here.
    let n = n as u32;
    if left {
        ((a as u64) << n) as i64
    } else {
        a >> n
    }
}

/// A row-value IN test (v0.16): `x` and every item are
/// `Vector`s of the same size (checked at plan time). An item
/// matches when every element compares equal, each element
/// pair under the comparison affinity of `x`'s element and the
/// LAST item's element — the pin rewrites a row-value IN list
/// as `IN (VALUES ...)`, whose column affinity is its rightmost
/// row's, and an `IN (SELECT ...)` substitutes items that all
/// carry the subquery column's affinity — and the collation of
/// `x`'s element against that same element. The result is 1 on
/// a match, else NULL if some item had a NULL element test and
/// no unequal one, else 0 (negated for NOT IN).
#[cfg(feature = "v16")]
fn in_rows<E>(x: &RExpr, list: &[RExpr], neg: bool, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    fn parts(e: &RExpr) -> &[RExpr] {
        match e {
            RExpr::Scalar(ScalarFn::Vector, p) => p.as_slice(),
            _ => &[],
        }
    }
    let xs = parts(x);
    let last = list.last().map(parts).unwrap_or(&[]);
    let mut xv = AVec::new();
    for e in xs {
        xv.push(eval::<E>(e, ctx)?);
    }
    let mut any_null = false;
    for item in list {
        let mut row_null = false;
        let mut row_false = false;
        for (j, ie) in parts(item).iter().enumerate() {
            let (Some(xe), Some(src), Some(v)) = (xs.get(j), last.get(j), xv.as_slice().get(j))
            else {
                return Err(row_misused());
            };
            let iv = eval::<E>(ie, ctx)?;
            let (a, b) = adjust_set(xe, src, v.clone(), iv);
            if matches!(a, OwnedValue::Null) || matches!(b, OwnedValue::Null) {
                row_null = true;
            } else if compare_coll(&a.as_value(), &b.as_value(), binary_coll(xe, src))
                != core::cmp::Ordering::Equal
            {
                row_false = true;
                break;
            }
        }
        if !row_false {
            if !row_null {
                return Ok(bool_val(!neg));
            }
            any_null = true;
        }
    }
    Ok(if any_null {
        OwnedValue::Null
    } else {
        bool_val(neg)
    })
}

/// `RAISE(kind[, message])` (v0.16, `triggers`): the message as
/// text (NULL is empty) is kept in the connection's `Env`, and the
/// kind's error ends the trigger program.
#[cfg(feature = "triggers")]
fn eval_raise<E>(
    k: crate::sql::ast::RaiseKind,
    args: &[RExpr],
    ctx: &Ctx<'_>,
) -> Result<OwnedValue, E> {
    use crate::sql::ast::RaiseKind;
    let mut msg = AVec::new();
    if let Some(a) = args.first() {
        match eval::<E>(a, ctx)? {
            OwnedValue::Null => {}
            v => {
                if let OwnedValue::Text(t) = cast_value(&v, Affinity::Text) {
                    msg = t;
                }
            }
        }
    }
    if let Some(env) = ctx.env {
        *env.raise_msg.borrow_mut() = msg;
    }
    Err(Error::Schema {
        kind: match k {
            RaiseKind::Ignore => SchemaKind::RaiseIgnore,
            RaiseKind::Rollback => SchemaKind::RaiseRollback,
            RaiseKind::Abort => SchemaKind::RaiseAbort,
            RaiseKind::Fail => SchemaKind::RaiseFail,
        },
    })
}

pub fn eval<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    Ok(match e {
        RExpr::Literal(v) => v.clone(),
        RExpr::AffLiteral(v, ..) => v.clone(),
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
        RExpr::Win(..) => return Err(Error::Unsupported),
        RExpr::Raise(kind) => return Err(Error::Schema { kind: *kind }),
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
            // An integer literal operand decides AND / OR without the
            // other (v0.16 fix, ungated: the pin simplifies `x AND 0`
            // to 0 and `x OR 1` to 1, so `0 AND abs(-9223372036854775808)`
            // is 0 there, not "integer overflow").
            BinOp::And if int_lit(l) == Some(false) || int_lit(r) == Some(false) => bool_val(false),
            BinOp::Or if int_lit(l) == Some(true) || int_lit(r) == Some(true) => bool_val(true),
            BinOp::And => {
                let a = truthy(&eval::<E>(l, ctx)?);
                if a == Some(false) && has_raise(r) {
                    return Ok(bool_val(false));
                }
                let b = truthy(&eval::<E>(r, ctx)?);
                match (a, b) {
                    (Some(false), _) | (_, Some(false)) => bool_val(false),
                    (Some(true), Some(true)) => bool_val(true),
                    _ => OwnedValue::Null,
                }
            }
            BinOp::Or => {
                let a = truthy(&eval::<E>(l, ctx)?);
                if a == Some(true) && has_raise(r) {
                    return Ok(bool_val(true));
                }
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
            #[cfg(feature = "v16")]
            BinOp::BitAnd | BinOp::BitOr | BinOp::Shl | BinOp::Shr => eval_other::<E>(e, ctx)?,
        },
        RExpr::Cast(x, aff) => cast_value(&eval::<E>(x, ctx)?, *aff),
        RExpr::Collate(x, _) => eval::<E>(x, ctx)?,
        // Everything else out of line: this function is on the
        // stack once per level of the expression tree, so its own
        // frame is kept to the common nodes (v0.16: see
        // `Config::max_expr_depth`).
        _ => eval_other::<E>(e, ctx)?,
    })
}

/// `x [NOT] IN (list)` (out of line: see `eval`).
#[inline(never)]
fn eval_in_list<E>(
    x: &RExpr,
    list: &[RExpr],
    neg: &bool,
    from_sub: &bool,
    ctx: &Ctx<'_>,
) -> Result<OwnedValue, E> {
    Ok({
        let xv = eval::<E>(x, ctx)?;
        let mut any_null = matches!(xv, OwnedValue::Null);
        let mut hit = false;
        let coll = in_list_coll(x);
        for item in list.iter() {
            let iv = eval::<E>(item, ctx)?;
            // An IN (SELECT ...) substitution converts both
            // sides by the set's affinity (the item carries
            // the subquery column's; `adjust_set`). A written list converts both sides
            // by the LEFT operand's affinity only, as the
            // pin's OP_Eq / ephemeral-index path does: an
            // operand without affinity converts nothing,
            // whatever the items declare (v0.16, probed:
            // with d no affinity, e INTEGER, f TEXT holding
            // '1'/1/'1', d IN (e) and 1 IN (f) are 0).
            let (a, b) = if *from_sub {
                adjust_set(x, item, xv.clone(), iv)
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
    })
}

/// A CASE expression (out of line: see `eval`).
#[inline(never)]
fn eval_case<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let RExpr::Case {
        base,
        whens,
        otherwise,
    } = e
    else {
        return Err(Error::Unsupported);
    };
    Ok({
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
                    if cond_case::<E>(w, ctx)? {
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
    })
}

/// Whether condition `e` holds (v0.16 fix, ungated): its value is
/// true, computed as the pin's jump code computes a WHERE / ON /
/// FILTER term or a CASE WHEN / iif condition (`sqlite3ExprIfTrue`)
/// — AND stops at an operand that is not true, OR at one that is,
/// and the other operand is never evaluated (so its errors never
/// surface). The same answer as `truthy(eval(e)) == Some(true)`
/// whenever both operands evaluate. An integer-literal operand
/// keeps `eval`'s simplification (`x AND 0` evaluates nothing).
///
/// A WHERE / ON / FILTER term (`factored`) also evaluates a skipped
/// operand that is a constant: the pin computes constant terms once
/// before its loop, so their errors surface whatever the other
/// operand says (`WHERE x = 1 AND abs(-9223372036854775808) > 0`
/// fails); a CASE WHEN / iif condition factors nothing.
pub fn cond<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<bool, E> {
    cond_in::<E>(e, ctx, true)
}

/// `cond` for a CASE WHEN / iif condition.
fn cond_case<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<bool, E> {
    cond_in::<E>(e, ctx, false)
}

fn cond_in<E>(e: &RExpr, ctx: &Ctx<'_>, factored: bool) -> Result<bool, E> {
    match e {
        RExpr::Binary(op @ (BinOp::And | BinOp::Or), l, r)
            if int_lit(l).is_none() && int_lit(r).is_none() =>
        {
            let a = cond_in::<E>(l, ctx, factored)?;
            if a == (*op == BinOp::Or) {
                if factored {
                    skipped_constants::<E>(r, ctx)?;
                }
                return Ok(a);
            }
            cond_in::<E>(r, ctx, factored)
        }
        _ => Ok(truthy(&eval::<E>(e, ctx)?) == Some(true)),
    }
}

/// The constant terms of an AND / OR operand `cond` skips, computed
/// for their errors (see `cond`).
fn skipped_constants<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<(), E> {
    match e {
        RExpr::Binary(BinOp::And | BinOp::Or, l, r) => {
            skipped_constants::<E>(l, ctx)?;
            skipped_constants::<E>(r, ctx)
        }
        _ if factorable(e) => eval::<E>(e, ctx).map(|_| ()),
        _ => Ok(()),
    }
}

/// A constant the pin computes once per statement: no column, no
/// subquery, nothing non-deterministic.
fn factorable(e: &RExpr) -> bool {
    #[cfg(feature = "v16")]
    if any_scalar(e, &|f| f.nondeterministic()) {
        return false;
    }
    is_constant(e)
}

/// The nodes `eval` leaves out of its own frame.
#[inline(never)]
fn eval_other<E>(e: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    Ok(match e {
        #[cfg(feature = "v16")]
        RExpr::Binary(op @ (BinOp::BitAnd | BinOp::BitOr | BinOp::Shl | BinOp::Shr), l, r) => {
            let lv = eval::<E>(l, ctx)?;
            let rv = eval::<E>(r, ctx)?;
            if matches!(lv, OwnedValue::Null) || matches!(rv, OwnedValue::Null) {
                OwnedValue::Null
            } else {
                OwnedValue::Integer(bit_op(*op, int_value(&lv), int_value(&rv)))
            }
        }
        // An empty set is FALSE for IN (TRUE for NOT IN) even
        // when the left operand is NULL (v0.16, pin-probed:
        // `NULL IN (SELECT a FROM empty)` is 0, not NULL — the
        // shape `x IN emptytable` exposed it).
        RExpr::InList(_, list, neg, _) if list.is_empty() => bool_val(*neg),
        #[cfg(feature = "v16")]
        RExpr::InList(x, list, neg, _) if matches!(**x, RExpr::Scalar(ScalarFn::Vector, _)) => {
            in_rows::<E>(x, list.as_slice(), *neg, ctx)?
        }
        RExpr::InList(x, list, neg, from_sub) => {
            eval_in_list::<E>(x, list.as_slice(), neg, from_sub, ctx)?
        }
        RExpr::Case { .. } => eval_case::<E>(e, ctx)?,
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
            match like_glob::<E>(&pv, &xv, None, false, case_like(ctx))? {
                OwnedValue::Integer(m) => bool_val((m != 0) != *neg),
                other => other,
            }
        }
        RExpr::Scalar(f, args) => eval_scalar::<E>(*f, args.as_slice(), ctx)?,
        // An EXISTS result-list term nobody reads.
        RExpr::Agg { inert: true, .. } => OwnedValue::Null,
        // An aggregate of an enclosing query that its owner did
        // not substitute: one in a clause that cannot aggregate.
        RExpr::Agg { lvl: 1.., .. } => {
            return Err(Error::Schema {
                kind: SchemaKind::MisusedAggregate,
            });
        }
        RExpr::Agg { .. } => match ctx.rows {
            Some(rows) => eval_agg::<E>(e, rows, ctx)?,
            None => return Err(Error::Unsupported),
        },
        // Subquery nodes are computed by the executor and
        // substituted away before evaluation (v0.6); reaching
        // one here means a path forgot to substitute.
        RExpr::SubScalar(_) | RExpr::SubExists(_) | RExpr::InSub(..) => {
            return Err(Error::Unsupported);
        }
        _ => return Err(Error::Unsupported),
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
    eval::<E>(&r, &Ctx::of(&empty))
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
