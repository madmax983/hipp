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
//! - comparisons follow `record::compare_values`, with the
//!   documented comparison-affinity adjustment when one operand is
//!   a direct column reference (SQLite applies the column's
//!   affinity to the other operand);
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
    Affinity, OwnedValue, apply_affinity, compare_values, render_int, render_real,
};
use crate::sql::{BinOp, Expr, Literal, eq_ci, handle_bytes};

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
}

/// A resolved expression: column references are flat slot indices
/// into `RowData::vals`; `Rowid(src)` reads `RowData::rowids[src]`.
#[derive(Debug, Clone)]
pub enum RExpr {
    Literal(OwnedValue),
    Column(usize, Affinity),
    Rowid(usize),
    Binary(BinOp, Box<RExpr>, Box<RExpr>),
    Neg(Box<RExpr>),
    Not(Box<RExpr>),
    IsNull(Box<RExpr>, bool),
    InList(Box<RExpr>, AVec<RExpr>, bool),
    Between(Box<RExpr>, Box<RExpr>, Box<RExpr>, bool),
    Like(Box<RExpr>, Box<RExpr>, bool),
    Case {
        base: Option<Box<RExpr>>,
        whens: AVec<(RExpr, RExpr)>,
        otherwise: Option<Box<RExpr>>,
    },
    Scalar(ScalarFn, AVec<RExpr>),
    Cast(Box<RExpr>, Affinity),
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
pub struct SourceDesc {
    pub qualifier: AVec<u8>,
    pub columns: AVec<(AVec<u8>, Affinity)>,
    pub rowid_alias: Option<usize>,
}

fn is_rowid_name(b: &[u8]) -> bool {
    eq_ci(b, b"rowid") || eq_ci(b, b"oid") || eq_ci(b, b"_rowid_")
}

/// Resolve one column reference to an `RExpr`.
fn resolve_column<E>(
    sources: &[SourceDesc],
    offsets: &[usize],
    table: Option<&[u8]>,
    name: &[u8],
) -> Result<RExpr, E> {
    // Candidate: (flat slot or rowid source, is_rowid)
    let mut found: Option<RExpr> = None;
    let mut ambiguous = false;
    for (si, src) in sources.iter().enumerate() {
        if let Some(q) = table
            && !eq_ci(src.qualifier.as_slice(), q)
        {
            continue;
        }
        let mut cand: Option<RExpr> = None;
        for (ci, (cname, aff)) in src.columns.iter().enumerate() {
            if eq_ci(cname.as_slice(), name) {
                cand = Some(if Some(ci) == src.rowid_alias {
                    RExpr::Rowid(si)
                } else {
                    RExpr::Column(offsets[si] + ci, *aff)
                });
                break;
            }
        }
        if cand.is_none() && is_rowid_name(name) {
            cand = Some(RExpr::Rowid(si));
        }
        if let Some(c) = cand {
            if found.is_some() {
                ambiguous = true;
            } else {
                found = Some(c);
            }
        }
        if table.is_some() {
            break;
        }
    }
    if ambiguous {
        return Err(Error::Schema {
            kind: SchemaKind::AmbiguousColumn,
        });
    }
    if let Some(f) = found {
        return Ok(f);
    }
    if table.is_some() {
        // Qualified reference to a source that exists but lacks the
        // column, or to a source that does not exist: SQLite calls
        // both "no such column" at prepare time.
        return Err(Error::Schema {
            kind: SchemaKind::UnknownColumn,
        });
    }
    Err(Error::Schema {
        kind: SchemaKind::UnknownColumn,
    })
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

/// Resolve a parser `Expr` against the FROM sources. `offsets[i]`
/// is source i's first flat slot. Aggregate functions resolve to
/// `RExpr::Agg` wherever they appear; the planner decides where
/// aggregates are legal.
pub fn resolve<E>(
    expr: &Expr,
    arena: &Arena,
    sources: &[SourceDesc],
    offsets: &[usize],
) -> Result<RExpr, E> {
    Ok(match expr {
        Expr::Literal(l) => RExpr::Literal(literal_value(arena, *l)),
        Expr::Column { table, name } => resolve_column::<E>(
            sources,
            offsets,
            table.map(|h| handle_bytes(arena, h)),
            handle_bytes(arena, *name),
        )?,
        Expr::Binary(op, l, r) => RExpr::Binary(
            *op,
            Box::new(resolve::<E>(l, arena, sources, offsets)?),
            Box::new(resolve::<E>(r, arena, sources, offsets)?),
        ),
        Expr::Neg(e) => RExpr::Neg(Box::new(resolve::<E>(e, arena, sources, offsets)?)),
        Expr::Not(e) => RExpr::Not(Box::new(resolve::<E>(e, arena, sources, offsets)?)),
        Expr::IsNull(e, neg) => {
            RExpr::IsNull(Box::new(resolve::<E>(e, arena, sources, offsets)?), *neg)
        }
        Expr::InList(x, list, neg) => {
            let mut out = AVec::new();
            for e in list.iter() {
                out.push(resolve::<E>(e, arena, sources, offsets)?);
            }
            RExpr::InList(
                Box::new(resolve::<E>(x, arena, sources, offsets)?),
                out,
                *neg,
            )
        }
        Expr::Between(x, lo, hi, neg) => RExpr::Between(
            Box::new(resolve::<E>(x, arena, sources, offsets)?),
            Box::new(resolve::<E>(lo, arena, sources, offsets)?),
            Box::new(resolve::<E>(hi, arena, sources, offsets)?),
            *neg,
        ),
        Expr::Like(x, p, neg) => RExpr::Like(
            Box::new(resolve::<E>(x, arena, sources, offsets)?),
            Box::new(resolve::<E>(p, arena, sources, offsets)?),
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
                    resolve::<E>(w, arena, sources, offsets)?,
                    resolve::<E>(t, arena, sources, offsets)?,
                ));
            }
            RExpr::Case {
                base: match base {
                    Some(b) => Some(Box::new(resolve::<E>(b, arena, sources, offsets)?)),
                    None => None,
                },
                whens: ws,
                otherwise: match otherwise {
                    Some(o) => Some(Box::new(resolve::<E>(o, arena, sources, offsets)?)),
                    None => None,
                },
            }
        }
        Expr::Cast(e, ty) => RExpr::Cast(
            Box::new(resolve::<E>(e, arena, sources, offsets)?),
            crate::record::affinity_of(handle_bytes(arena, *ty)),
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
            sources,
            offsets,
        )?,
    })
}

fn resolve_args<E>(
    args: &[Expr],
    arena: &Arena,
    sources: &[SourceDesc],
    offsets: &[usize],
) -> Result<AVec<RExpr>, E> {
    let mut out = AVec::new();
    for a in args.iter() {
        out.push(resolve::<E>(a, arena, sources, offsets)?);
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
    sources: &[SourceDesc],
    offsets: &[usize],
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
                resolve_args::<E>(args, arena, sources, offsets)?,
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
            args: resolve_args::<E>(args, arena, sources, offsets)?,
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
        ScalarFn::MinN | ScalarFn::MaxN => (2, usize::MAX),
        _ => (1, 1),
    };
    if args.len() < want.0 || args.len() > want.1 {
        return Err(Error::Unsupported);
    }
    Ok(RExpr::Scalar(
        f,
        resolve_args::<E>(args, arena, sources, offsets)?,
    ))
}

/// Whether a resolved expression contains an aggregate node.
pub fn has_agg(e: &RExpr) -> bool {
    match e {
        RExpr::Agg { .. } => true,
        RExpr::Literal(_) | RExpr::Column(..) | RExpr::Rowid(_) | RExpr::OutRef(_) => false,
        RExpr::Binary(_, a, b) => has_agg(a) || has_agg(b),
        RExpr::Neg(a) | RExpr::Not(a) | RExpr::IsNull(a, _) | RExpr::Cast(a, _) => has_agg(a),
        RExpr::InList(x, l, _) => has_agg(x) || l.iter().any(has_agg),
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
pub fn collect_aggs<'x>(e: &'x RExpr, out: &mut AVec<&'x RExpr>) {
    match e {
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
        RExpr::Neg(a) | RExpr::Not(a) | RExpr::IsNull(a, _) | RExpr::Cast(a, _) => {
            collect_aggs(a, out)
        }
        RExpr::InList(x, l, _) => {
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
        return text.parse::<f64>().ok().map(Num::Real);
    }
    text.parse::<f64>().ok().map(Num::Real)
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
            OwnedValue::Text(t) | OwnedValue::Blob(t) => {
                OwnedValue::Integer(match parse_num_prefix(t.as_slice()) {
                    Some(n) => num_to_i64(n),
                    None => 0,
                })
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
            OwnedValue::Text(t) | OwnedValue::Blob(t) => match parse_num_prefix(t.as_slice()) {
                Some(Num::Int(i)) => OwnedValue::Integer(i),
                Some(Num::Real(r)) => {
                    let i = num_to_i64(Num::Real(r));
                    if (i as f64) == r {
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

fn col_affinity(e: &RExpr) -> Option<Affinity> {
    match e {
        RExpr::Column(_, aff) => Some(*aff),
        _ => None,
    }
}

/// SQLite's comparison-affinity adjustment for `=`-family
/// operators: when exactly one operand is a direct column
/// reference, the column's affinity is applied to the other
/// operand (numeric affinity converts text; TEXT affinity
/// converts numbers to text).
fn adjust_cmp(l: &RExpr, r: &RExpr, lv: OwnedValue, rv: OwnedValue) -> (OwnedValue, OwnedValue) {
    match (col_affinity(l), col_affinity(r)) {
        (Some(a), None) => (lv, apply_cmp_affinity(rv, a)),
        (None, Some(a)) => (apply_cmp_affinity(lv, a), rv),
        _ => (lv, rv),
    }
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

fn cmp_op<E>(op: BinOp, l: &RExpr, r: &RExpr, ctx: &Ctx<'_>) -> Result<OwnedValue, E> {
    let lv = eval::<E>(l, ctx)?;
    let rv = eval::<E>(r, ctx)?;
    let (lv, rv) = adjust_cmp(l, r, lv, rv);
    if matches!(op, BinOp::Is | BinOp::IsNot) {
        let eq = values_equal_is(&lv, &rv);
        return Ok(bool_val(if op == BinOp::Is { eq } else { !eq }));
    }
    if matches!(lv, OwnedValue::Null) || matches!(rv, OwnedValue::Null) {
        return Ok(OwnedValue::Null);
    }
    let ord = compare_values(&lv.as_value(), &rv.as_value());
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
        let blob = matches!(lv, OwnedValue::Blob(_)) || matches!(rv, OwnedValue::Blob(_));
        let mut out = to_text(&lv).unwrap_or_default();
        out.extend_from_slice(to_text(&rv).unwrap_or_default().as_slice());
        return Ok(if blob {
            OwnedValue::Blob(out)
        } else {
            OwnedValue::Text(out)
        });
    }
    let (Some(a), Some(b)) = (to_number(&lv), to_number(&rv)) else {
        return Ok(OwnedValue::Null);
    };
    if matches!(op, BinOp::Mod) {
        let (x, y) = (num_to_i64(a), num_to_i64(b));
        if y == 0 {
            return Ok(OwnedValue::Null);
        }
        return Ok(OwnedValue::Integer(x.wrapping_rem(y)));
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

/// SQLite LIKE matching: `%` any run, `_` exactly one character;
/// ASCII case-insensitive. Works on characters, not bytes, for
/// text values.
fn like_match(text: &[u8], pat: &[u8]) -> bool {
    // Decode both as char vectors (lossy on invalid UTF-8, which
    // SQLite would treat byte-wise; ASCII corpora agree).
    let t: AVec<char> = {
        let mut v = AVec::new();
        for c in core::str::from_utf8(text).unwrap_or("").chars() {
            v.push(c);
        }
        v
    };
    let p: AVec<char> = {
        let mut v = AVec::new();
        for c in core::str::from_utf8(pat).unwrap_or("").chars() {
            v.push(c);
        }
        v
    };
    fn inner(t: &[char], p: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '%' => {
                for k in 0..=t.len() {
                    if inner(&t[k..], &p[1..]) {
                        return true;
                    }
                }
                false
            }
            '_' => !t.is_empty() && inner(&t[1..], &p[1..]),
            c => !t.is_empty() && t[0].eq_ignore_ascii_case(&c) && inner(&t[1..], &p[1..]),
        }
    }
    inner(t.as_slice(), p.as_slice())
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
                && values_equal_is(a, b)
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
        ScalarFn::Abs => match to_number(&v[0]) {
            None => OwnedValue::Null,
            Some(Num::Int(i)) => match i.checked_abs() {
                Some(a) => OwnedValue::Integer(a),
                None => OwnedValue::Real(-(i as f64)),
            },
            Some(Num::Real(r)) => OwnedValue::Real(if r < 0.0 { -r } else { r }),
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
            let digits = if v.len() > 1 {
                match to_number(&v[1]) {
                    Some(m) => num_to_i64(m),
                    None => return Ok(OwnedValue::Null),
                }
            } else {
                0
            };
            let x = n.as_f64();
            // A negative digit count behaves as 0 in SQLite (its
            // scale factor loop simply never runs).
            let mut mult = 1.0f64;
            let mut d = digits;
            while d > 0 {
                mult *= 10.0;
                d -= 1;
            }
            let scaled = x * mult;
            let rounded = if scaled >= 0.0 {
                (scaled + 0.5) as i64 as f64
            } else {
                (scaled - 0.5) as i64 as f64
            };
            OwnedValue::Real(rounded / mult)
        }
        ScalarFn::Substr => {
            if v.iter().any(|x| matches!(x, OwnedValue::Null)) {
                return Ok(OwnedValue::Null);
            }
            let Some(text) = to_text(&v[0]) else {
                return Ok(OwnedValue::Null);
            };
            let Some(start_n) = to_number(&v[1]) else {
                return Ok(OwnedValue::Null);
            };
            let mut start = num_to_i64(start_n);
            let ranges = char_ranges(text.as_slice());
            let n = ranges.len() as i64;
            let mut len: i64 = if v.len() > 2 {
                match to_number(&v[2]) {
                    Some(m) => num_to_i64(m),
                    None => return Ok(OwnedValue::Null),
                }
            } else {
                n
            };
            // SQLite's substr coordinate rules (verified
            // against the oracle, incl. negative lengths: a
            // negative length counts back from the start point).
            if start < 0 {
                start += n;
                if start < 0 {
                    len += start;
                    if len < 0 {
                        len = 0;
                    }
                    start = 0;
                }
            } else if start > 0 {
                start -= 1;
            } else if len > 0 && v.len() > 2 {
                len -= 1;
            }
            if len < 0 {
                len = -len;
                start -= len;
                if start < 0 {
                    len += start;
                    if len < 0 {
                        len = 0;
                    }
                    start = 0;
                }
            }
            if start + len > n {
                len = n - start;
            }
            let mut out = AVec::new();
            let mut idx = 0i64;
            for (bs, be) in ranges.iter() {
                if idx >= start && idx < start + len {
                    out.extend_from_slice(&text.as_slice()[*bs..*be]);
                }
                idx += 1;
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
                let mut best = v[0].clone();
                for x in &v[1..] {
                    let ord = compare_values(&x.as_value(), &best.as_value());
                    let take = if f == ScalarFn::MinN {
                        ord == core::cmp::Ordering::Less
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

/// Fold one aggregate over a group's rows.
fn eval_agg<E>(
    kind: AggKind,
    distinct: bool,
    star: bool,
    args: &[RExpr],
    rows: &[RowData],
) -> Result<OwnedValue, E> {
    match kind {
        AggKind::Count => {
            if star {
                return Ok(OwnedValue::Integer(rows.len() as i64));
            }
            let mut n = 0i64;
            let mut seen: AVec<OwnedValue> = AVec::new();
            for r in rows {
                let val = eval::<E>(&args[0], &Ctx::as_row(r))?;
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_is(s, &val)) {
                        continue;
                    }
                    seen.push(val);
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
            for r in rows {
                let val = eval::<E>(&args[0], &Ctx::as_row(r))?;
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_is(s, &val)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                count += 1;
                match Some(sum_number(&val)) {
                    Some(Num::Int(i)) => {
                        if int_ok {
                            match int_acc.checked_add(i) {
                                Some(s) => int_acc = s,
                                None => {
                                    int_ok = false;
                                    real_acc = int_acc as f64 + i as f64;
                                }
                            }
                        } else {
                            real_acc += i as f64;
                        }
                    }
                    Some(Num::Real(x)) => {
                        if int_ok {
                            int_ok = false;
                            real_acc = int_acc as f64 + x;
                        } else {
                            real_acc += x;
                        }
                    }
                    None => {}
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
            for r in rows {
                let val = eval::<E>(&args[0], &Ctx::as_row(r))?;
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_is(s, &val)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                best = Some(match best {
                    None => val,
                    Some(b) => {
                        let ord = compare_values(&val.as_value(), &b.as_value());
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
            for r in rows {
                let val = eval::<E>(&args[0], &Ctx::as_row(r))?;
                if matches!(val, OwnedValue::Null) {
                    continue;
                }
                if distinct {
                    if seen.iter().any(|s| values_equal_is(s, &val)) {
                        continue;
                    }
                    seen.push(val.clone());
                }
                let sep = if args.len() > 1 {
                    match eval::<E>(&args[1], &Ctx::as_row(r))? {
                        OwnedValue::Null => return Ok(OwnedValue::Null),
                        s => to_text(&s).unwrap_or_default(),
                    }
                } else {
                    let mut c = AVec::new();
                    c.push(b',');
                    c
                };
                if any {
                    out.extend_from_slice(sep.as_slice());
                }
                out.extend_from_slice(to_text(&val).unwrap_or_default().as_slice());
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
        RExpr::Column(i, _) => ctx
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
        RExpr::InList(x, list, neg) => {
            let xv = eval::<E>(x, ctx)?;
            let mut any_null = matches!(xv, OwnedValue::Null);
            let mut hit = false;
            for item in list.iter() {
                let iv = eval::<E>(item, ctx)?;
                // Comparison-affinity adjustment mirrors `=`.
                let (a, b) = adjust_cmp(x, item, xv.clone(), iv);
                if matches!(a, OwnedValue::Null) || matches!(b, OwnedValue::Null) {
                    any_null = true;
                    continue;
                }
                if compare_values(&a.as_value(), &b.as_value()) == core::cmp::Ordering::Equal {
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
            if matches!(xv, OwnedValue::Null) || matches!(pv, OwnedValue::Null) {
                OwnedValue::Null
            } else {
                let xt = to_text(&xv).unwrap_or_default();
                let pt = to_text(&pv).unwrap_or_default();
                bool_val(like_match(xt.as_slice(), pt.as_slice()) != *neg)
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
                        if !matches!(bv, OwnedValue::Null)
                            && !matches!(wv, OwnedValue::Null)
                            && values_equal_is(&bv, &wv)
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
