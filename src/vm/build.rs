//! The program builder (ADR 0005 §"Program representation"): register
//! and cursor allocation, labels whose jumps are fixed up at
//! `finish`, and constants hoisted into the `Init` prologue.

use super::Program;
use super::op::{Code, Op, P4};
use crate::arena::AVec;

/// A jump target, placed with `Builder::here` and resolved at
/// `Builder::finish`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Label(usize);

/// Builds one `Program`. A number that does not fit its operand (more
/// than 65,535 cursors or `p1` registers, or more than 2^32
/// instructions) makes `finish` decline, and the statement then runs
/// on today's executor.
pub(crate) struct Builder {
    ops: AVec<Op>,
    p4: AVec<P4>,
    nreg: u32,
    ncur: u16,
    labels: AVec<Option<u32>>,
    /// (instruction, label) pairs whose `p2` is the label's address.
    fixups: AVec<(usize, usize)>,
    /// The constant loads of the `Init` prologue.
    consts: AVec<Op>,
    bad: bool,
}

impl Builder {
    /// An empty program: instruction 0 is `Init`.
    pub(crate) fn new() -> Builder {
        let mut ops = AVec::new();
        ops.push(Op::new(Code::Init, 0, 0, 0, 0, 0));
        Builder {
            ops,
            p4: AVec::new(),
            nreg: 0,
            ncur: 0,
            labels: AVec::new(),
            fixups: AVec::new(),
            consts: AVec::new(),
            bad: false,
        }
    }

    /// A fresh register.
    pub(crate) fn reg(&mut self) -> u32 {
        self.regs(1)
    }

    /// `n` consecutive fresh registers; the first one.
    pub(crate) fn regs(&mut self, n: usize) -> u32 {
        let first = self.nreg;
        match u32::try_from(n).ok().and_then(|n| self.nreg.checked_add(n)) {
            Some(x) => self.nreg = x,
            None => self.bad = true,
        }
        first
    }

    /// A fresh cursor.
    pub(crate) fn cursor(&mut self) -> u32 {
        let c = self.ncur;
        match self.ncur.checked_add(1) {
            Some(x) => self.ncur = x,
            None => self.bad = true,
        }
        u32::from(c)
    }

    /// A new, unplaced label.
    pub(crate) fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len().saturating_sub(1))
    }

    /// Place `l` at the next instruction.
    pub(crate) fn here(&mut self, l: Label) {
        let at = self.addr();
        if let Some(slot) = self.labels.as_mut_slice().get_mut(l.0) {
            *slot = Some(at);
        }
    }

    fn addr(&mut self) -> u32 {
        match u32::try_from(self.ops.len()) {
            Ok(a) => a,
            Err(_) => {
                self.bad = true;
                0
            }
        }
    }

    /// Add a side-table entry; its index.
    pub(crate) fn p4(&mut self, v: P4) -> u32 {
        self.p4.push(v);
        match u32::try_from(self.p4.len().saturating_sub(1)) {
            Ok(i) => i,
            Err(_) => {
                self.bad = true;
                0
            }
        }
    }

    fn make(&mut self, code: Code, p1: u32, p2: u32, p3: u32, p4: u32, p5: u8) -> Op {
        let p1 = match u16::try_from(p1) {
            Ok(x) => x,
            Err(_) => {
                self.bad = true;
                0
            }
        };
        Op::new(code, p1, p2, p3, p4, p5)
    }

    /// Append an instruction.
    pub(crate) fn emit(&mut self, code: Code, p1: u32, p2: u32, p3: u32, p4: u32, p5: u8) {
        let op = self.make(code, p1, p2, p3, p4, p5);
        self.ops.push(op);
    }

    /// Append an instruction whose `p2` jumps to `to`.
    pub(crate) fn jump(&mut self, code: Code, p1: u32, to: Label, p3: u32, p4: u32, p5: u8) {
        let at = self.ops.len();
        self.emit(code, p1, 0, p3, p4, p5);
        self.fixups.push((at, to.0));
    }

    /// Append a constant load to the `Init` prologue: it runs once,
    /// before instruction 1.
    pub(crate) fn hoist(&mut self, code: Code, p2: u32, p4: u32) {
        let op = self.make(code, 0, p2, 0, p4, 0);
        self.consts.push(op);
    }

    /// The finished program: the prologue appended (its constant
    /// loads, then a jump back to instruction 1), `Init` pointed at
    /// it and every label resolved. `None` when a number overflowed
    /// or a label was never placed.
    pub(crate) fn finish(mut self) -> Option<Program> {
        let start = self.addr();
        for op in core::mem::take(&mut self.consts) {
            self.ops.push(op);
        }
        self.emit(Code::Goto, 0, 1, 0, 0, 0);
        if self.bad {
            return None;
        }
        if let Some(init) = self.ops.as_mut_slice().first_mut() {
            init.p2 = start;
        }
        for (at, l) in self.fixups.iter() {
            let to = (*self.labels.as_slice().get(*l)?)?;
            self.ops.as_mut_slice().get_mut(*at)?.p2 = to;
        }
        Some(Program {
            ops: self.ops,
            p4: self.p4,
            nreg: self.nreg as usize,
            ncur: usize::from(self.ncur),
        })
    }
}
