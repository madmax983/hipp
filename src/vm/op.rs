//! Instructions and their side table (ADR 0005 §"Program
//! representation", §"Instruction set").
//!
//! Every opcode this unit adds sits in the ADR table's "always" or
//! `query` rows, which `vm` implies, except the rowid-range ones
//! (`Last`, `Prev`, `SeekGE`, `SeekLE`, `RowidBound`, `Affinity`,
//! `IsNull`, `Gt`, `Lt`): only the `expr_index` access planner produces the
//! plans that need them, so they carry its gate.

use crate::arena::AVec;
use crate::query::RExpr;
#[cfg(feature = "expr_index")]
use crate::record::Affinity;
use crate::record::OwnedValue;

/// One instruction: SQLite's p1-p5 shape in a fixed 16 bytes. `p1`
/// is 16 bits (a cursor or register number; the builder declines a
/// program that needs more), `p2` is usually a jump target, `p3` a
/// register, `p4` an index into `Program::p4` (or a small constant),
/// `p5` a flag byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Op {
    pub(crate) code: Code,
    pub(crate) p5: u8,
    pub(crate) p1: u16,
    pub(crate) p2: u32,
    pub(crate) p3: u32,
    pub(crate) p4: u32,
}

impl Op {
    pub(crate) fn new(code: Code, p1: u16, p2: u32, p3: u32, p4: u32, p5: u8) -> Op {
        Op {
            code,
            p5,
            p1,
            p2,
            p3,
            p4,
        }
    }
}

/// The opcodes. One byte (`repr(u8)`), so an `Op` stays 16 bytes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    // ---- control (ADR: always) ----
    /// Jump to `p2`: the constant prologue, which ends with a jump
    /// back to instruction 1.
    Init,
    /// Jump to `p2`.
    Goto,
    /// Stop: the statement is done (running again stays here).
    Halt,
    /// Hand the statement to today's executor, kind `p1`
    /// (`HOST_QUERY`): the migration seam (ADR 0005 §Design).
    Host,
    // ---- control+ (ADR: `query`) ----
    /// Jump to `p2` when register `p1` is true.
    If,
    /// Jump to `p2` unless register `p1` is true (false or NULL).
    IfNot,
    /// When register `p1` holds an integer above 0, subtract `p3`
    /// from it and jump to `p2` (OFFSET).
    IfPos,
    /// Decrement the integer in register `p1` (not below
    /// i64::MIN) and jump to `p2` when it becomes 0 (LIMIT; a
    /// negative limit never reaches 0).
    DecrJumpZero,
    /// Jump to `p2` when register `p1` is NULL.
    #[cfg(feature = "expr_index")]
    IsNull,
    /// Jump to `p2` when register `p1` > register `p3` (numbers
    /// compared as the pin does; NULL never jumps).
    #[cfg(feature = "expr_index")]
    Gt,
    /// Jump to `p2` when register `p1` < register `p3`.
    #[cfg(feature = "expr_index")]
    Lt,
    // ---- values (ADR: always / `query`) ----
    /// Register `p2` = the value `P4::Value` `p4` (the ADR's Integer
    /// / Int64 / Real / String8 / Blob: the side table carries the
    /// type).
    Value,
    /// Register `p2` = a copy of register `p1`.
    Copy,
    /// Register `p1` through the pin's `OP_MustBeInt` as LIMIT /
    /// OFFSET take it (`query::limit_int`): NUMERIC affinity, then an
    /// integer or "datatype mismatch".
    MustBeInt,
    /// Register `p1` converted for a comparison with a column of the
    /// affinity `P4::Aff` `p4` (`query::probe_value`).
    #[cfg(feature = "expr_index")]
    Affinity,
    // ---- expressions (ADR: `query`; rule 5's Eval) ----
    /// Register `p2` = `P4::Expr` `p4` evaluated by `query::eval`
    /// against the row of cursor `p1 - 1` (`p1` 0: the empty row).
    /// `p5`: `EVAL_VALUE`, `EVAL_COND` (`query::cond`'s jump
    /// semantics, 1 or 0) or `EVAL_CONSTS` (the WHERE clause's
    /// constant terms, `query::join_constants_hold`, 1 or 0).
    Eval,
    // ---- tables (ADR: always / `query`) ----
    /// Open cursor `p1` on the table `P4::Table` `p4`.
    OpenRead,
    /// Cursor `p1` to its first row; jump to `p2` when there is none.
    Rewind,
    /// Cursor `p1` to its next row; jump to `p2` when there is one.
    Next,
    /// Cursor `p1` to its last row; jump to `p2` when there is none.
    #[cfg(feature = "expr_index")]
    Last,
    /// Cursor `p1` to its previous row; jump to `p2` when there is
    /// one.
    #[cfg(feature = "expr_index")]
    Prev,
    /// Cursor `p1` to the row whose rowid is register `p3` (an
    /// integer, or a real with an integral value); jump to `p2` when
    /// there is none.
    SeekRowid,
    /// Cursor `p1` to the first row whose rowid is the integer in
    /// register `p3` or more; jump to `p2` when there is none.
    #[cfg(feature = "expr_index")]
    SeekGE,
    /// Cursor `p1` to the last row whose rowid is register `p3` or
    /// less; jump to `p2` when there is none.
    #[cfg(feature = "expr_index")]
    SeekLE,
    /// Register `p1` (a bound value) as a rowid range end
    /// (`session::xindex::rowid_bound`; `p5` bit 0: lower bound, bit
    /// 1: inclusive): an integer, NULL for an open end, or a jump to
    /// `p2` when no rowid satisfies it.
    #[cfg(feature = "expr_index")]
    RowidBound,
    /// Register `p3` = column `p2` of cursor `p1`'s row (decoded with
    /// the table's defaults, rowid alias, REAL forcing and generated
    /// columns).
    Column,
    /// Register `p2` = cursor `p1`'s rowid.
    Rowid,
    // ---- sorting (ADR: `query`) ----
    /// Open sorter `p1` with the keys `P4::Sorter` `p4`.
    SorterOpen,
    /// Add registers `p2 ..` (the result values, then the sort keys)
    /// to sorter `p1`, with the rowid of cursor `p3 - 1` (`p3` 0:
    /// none).
    SorterInsert,
    /// Sort sorter `p1` (stable); jump to `p2` when it is empty.
    SorterSort,
    /// Registers `p2 ..` = the sorter `p1` row's result values.
    SorterData,
    /// Sorter `p1` to its next row; jump to `p2` when there is one.
    SorterNext,
    // ---- results ----
    /// Registers `p1 .. p1 + p2` are a result row, with the rowid of
    /// cursor `p3 - 1` (`p3` 0: none).
    ResultRow,
}

/// `Code::Host` `p1`: a SELECT on today's executor.
pub(crate) const HOST_QUERY: u16 = 1;

/// `Code::Eval` `p5`: the expression's value.
pub(crate) const EVAL_VALUE: u8 = 0;
/// `Code::Eval` `p5`: the expression as a WHERE condition, 1 or 0.
pub(crate) const EVAL_COND: u8 = 1;
/// `Code::Eval` `p5`: whether the WHERE clause's constant terms hold.
pub(crate) const EVAL_CONSTS: u8 = 2;

/// One entry of a program's side table (`Op::p4`).
pub(crate) enum P4 {
    Value(OwnedValue),
    Expr(RExpr),
    /// A comparison affinity (`Code::Affinity`).
    #[cfg(feature = "expr_index")]
    Aff(Option<Affinity>),
    Table(alloc::boxed::Box<super::cursor::TableInfo>),
    Sorter(alloc::boxed::Box<super::cursor::SortInfo>),
}

/// The side table entries as a slice-indexed lookup that never
/// panics.
pub(crate) fn p4_at(p4: &AVec<P4>, i: u32) -> Option<&P4> {
    p4.as_slice().get(i as usize)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_instruction_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<super::Op>(), 16);
        assert_eq!(core::mem::size_of::<super::Code>(), 1);
    }
}
