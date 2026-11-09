//! Code generation (ADR 0005 §"Program representation": codegen
//! consumes today's plans; the parser, binder and planners are
//! unchanged). Each generator returns `None` for a plan shape it does
//! not cover; the statement then runs on today's executor
//! (`Program::host`).

pub(crate) mod expr;
pub(crate) mod join;
pub(crate) mod select;
#[cfg(feature = "sql")]
pub(crate) mod sub;
