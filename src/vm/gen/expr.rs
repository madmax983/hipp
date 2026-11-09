//! Expressions (ADR 0005 Stage 2, rule 5): a resolved expression is
//! one `Eval` instruction over the side table, evaluated by the
//! tree-walking `query::eval` — the evaluator writes still need until
//! Stage 3, so a second one in opcodes would only add bytes. Its
//! evaluation order, laziness (CASE, coalesce, iif, AND / OR),
//! affinity and collation rules are therefore exactly today's. A
//! literal is loaded once, in the `Init` prologue.

use super::super::build::{Builder, Label};
use super::super::op::{Code, EVAL_COND, EVAL_VALUE, P4};
use crate::query::RExpr;

/// Whether `Eval` can evaluate `e` on its own: no subquery, aggregate,
/// window call or deferred subquery error (those need the executor
/// around the evaluator), and column references inside the `width`
/// slots of the one source (`src`: whether there is one, for rowid
/// references).
pub(crate) fn evaluable(e: &RExpr, width: usize, src: bool) -> bool {
    let f = |x: &RExpr| evaluable(x, width, src);
    match e {
        RExpr::Literal(_) | RExpr::AffLiteral(..) => true,
        RExpr::Column(slot, ..) => *slot < width,
        RExpr::Rowid(s) => src && *s == 0,
        RExpr::SubScalar(_)
        | RExpr::SubExists(_)
        | RExpr::InSub(..)
        | RExpr::Raise(_)
        | RExpr::Agg { .. }
        | RExpr::Win(..) => false,
        RExpr::Binary(_, a, b) | RExpr::Like(a, b, _) => f(a) && f(b),
        RExpr::Neg(a)
        | RExpr::Pos(a)
        | RExpr::Not(a)
        | RExpr::IsNull(a, _)
        | RExpr::Cast(a, _)
        | RExpr::Collate(a, _) => f(a),
        RExpr::InList(x, l, ..) => f(x) && l.iter().all(f),
        RExpr::Between(x, a, b, _) => f(x) && f(a) && f(b),
        RExpr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(f)
                && whens.iter().all(|(w, t)| f(w) && f(t))
                && otherwise.as_deref().is_none_or(f)
        }
        RExpr::Scalar(_, args) => args.iter().all(f),
    }
}

/// Code `e`'s value into register `dest`, reading the row of cursor
/// `cur` (`None`: no row). A literal is a prologue constant.
pub(crate) fn value(b: &mut Builder, e: &RExpr, cur: Option<u32>, dest: u32) {
    if let RExpr::Literal(v) = e {
        let k = b.p4(P4::Value(v.clone()));
        b.hoist(Code::Value, dest, k);
        return;
    }
    let k = b.p4(P4::Expr(e.clone()));
    b.emit(Code::Eval, cursor_ref(cur), dest, 0, k, EVAL_VALUE);
}

/// Code condition `e` (`query::cond`'s jump semantics: NULL is not
/// true) into scratch register `tmp`, then jump to `skip` unless it
/// holds.
pub(crate) fn cond(b: &mut Builder, e: &RExpr, cur: Option<u32>, tmp: u32, skip: Label) {
    let k = b.p4(P4::Expr(e.clone()));
    b.emit(Code::Eval, cursor_ref(cur), tmp, 0, k, EVAL_COND);
    b.jump(Code::IfNot, tmp, skip, 0, 0, 0);
}

/// An instruction's reference to cursor `cur` (`p1`/`p3`: cursor + 1,
/// 0 for none).
pub(crate) fn cursor_ref(cur: Option<u32>) -> u32 {
    cur.map_or(0, |c| c.saturating_add(1))
}
