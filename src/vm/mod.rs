//! The bytecode VM (ADR 0005 Stage 2, hidden feature `vm`).
//!
//! A register machine in SQLite's p1-p5 shape. A `Program` is a list
//! of 16-byte instructions (`op::Op`) plus a side table of operands
//! too large for them (`op::P4`): values, resolved expressions, table
//! layouts, sort keys. `Statement::step` is its driver: it runs the
//! program until it yields a row, finishes, needs a page, or hands
//! the statement back to today's executor (`Exec::Host`, the
//! migration seam for shapes the code generator does not cover yet).
//!
//! **Restart, not resume** (ADR 0005 §Design). Everything below the
//! driver is synchronous and monomorphic: it reads pages through
//! `PagerCore`, which never touches storage. An opcode that needs a
//! page not in the cache returns `Exec::Pending(io)` having changed
//! no register, no cursor position and not `pc`; the driver does the
//! I/O (`Pager::vm_service`) and runs the same opcode again. The pages
//! an opcode has read stay pinned until it completes, so it needs at
//! most "pages per opcode" frames (a cursor's descent from the root to
//! a leaf); fewer is `CacheExhausted`, as today. The VM state (pc,
//! registers, cursors) lives in the `Statement`, so dropping the step
//! future at its one `.await` is safe (SPEC §5.1.1).
//!
//! The one exception to "changes nothing before its last miss" is a
//! row's overflow chain, which a cursor copies page by page into a
//! scratch buffer that survives a miss (`cursor::Payload`): the copy
//! is not a position, a re-run produces the same bytes, and a chain
//! longer than the cache would otherwise never complete.
//!
//! Expressions run through the `Eval` opcode, which evaluates one
//! resolved expression of the side table with the tree-walking
//! `query::eval` against the row a cursor holds (see STATUS.md, "VM
//! (ADR 0005 Stage 2): vm_core", for that decision); comparisons and
//! jumps that drive loops are real opcodes.

pub(crate) mod build;
#[path = "gen/mod.rs"]
pub(crate) mod codegen;
pub(crate) mod cursor;
pub(crate) mod exec;
#[cfg(feature = "expr_index")]
pub(crate) mod index;
pub(crate) mod op;
pub(crate) mod rows;

pub(crate) use crate::pager::Io;

/// What one run of the program ended with (`exec::Vm::run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exec {
    /// A result row is ready (`exec::Vm::take_row`).
    Row,
    /// The program halted.
    Done,
    /// The opcode at `pc` needs this I/O first; it runs again after.
    Pending(Io),
    /// The statement runs on today's executor (`op::HOST_QUERY`).
    Host(u16),
}

/// A compiled statement: its instructions, their side table and the
/// register and cursor counts the VM allocates for it.
pub(crate) struct Program {
    pub(crate) ops: crate::arena::AVec<op::Op>,
    pub(crate) p4: crate::arena::AVec<op::P4>,
    pub(crate) nreg: usize,
    pub(crate) ncur: usize,
}

impl Program {
    /// The program of a statement the code generator does not cover:
    /// one `Host` instruction handing it to today's executor.
    pub(crate) fn host(kind: u16) -> Program {
        let mut ops = crate::arena::AVec::new();
        ops.push(op::Op::new(op::Code::Host, kind, 0, 0, 0, 0));
        Program {
            ops,
            p4: crate::arena::AVec::new(),
            nreg: 0,
            ncur: 0,
        }
    }

    /// Whether the program runs on the VM (holds no `Host`).
    pub(crate) fn on_vm(&self) -> bool {
        !self.ops.iter().any(|o| o.code == op::Code::Host)
    }
}
