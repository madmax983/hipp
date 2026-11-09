//! SQL frontend (SPEC §5.6): lexer, parser, AST.

pub mod ast;
#[cfg(feature = "params")]
pub mod bind;
pub mod lexer;
pub mod parser;
#[cfg(feature = "triggers")]
pub mod trig;

pub use ast::{
    Ast, BinOp, ColumnDef, CompoundOp, ConflictAction, ConstraintSet, Expr, FromSource,
    IndexColumnDef, JoinKind, Literal, SelectItem, SelectList, SelectStmt, SortOrder, Upsert,
    UpsertAction,
};
#[cfg(feature = "sql")]
pub use ast::{Cte, CteUse, IndexHint, M10d, RecMark, With};
#[cfg(feature = "query")]
pub use parser::parse_expr_text;
#[cfg(feature = "params")]
pub use parser::parse_params;
pub use parser::{eq_ci, handle_bytes, parse};
