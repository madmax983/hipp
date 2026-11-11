//! Expressions (ADR 0005 Stage 2, rule 5): a resolved expression is
//! one `Eval` instruction over the side table, evaluated by the
//! tree-walking `query::eval` — the evaluator writes still need until
//! Stage 3, so a second one in opcodes would only add bytes. Its
//! evaluation order, laziness (CASE, coalesce, iif, AND / OR),
//! affinity and collation rules are therefore exactly today's. A
//! literal is loaded once, in the `Init` prologue.

use super::super::build::Builder;
use super::super::op::{Code, EVAL_VALUE, P4};
use crate::query::RExpr;

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

/// An instruction's reference to cursor `cur` (`p1`/`p3`: cursor + 1,
/// 0 for none).
pub(crate) fn cursor_ref(cur: Option<u32>) -> u32 {
    cur.map_or(0, |c| c.saturating_add(1))
}
