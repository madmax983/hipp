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
    /// (instruction, label) pairs whose `p3` is the label's address.
    fixups3: AVec<(usize, usize)>,
    /// The constant loads of the `Init` prologue.
    consts: AVec<Op>,
    bad: bool,
    /// Subquery result slots (`Code::SubStart`).
    #[cfg(feature = "sql")]
    nsub: u32,
    /// The program's uncorrelated subquery nodes, by (plan id, result
    /// column): their slot and "computed" register, so every copy of
    /// a node reads one result per execution (today's `hoist` cache).
    ///
    /// The node's code is a subroutine (`Gosub` / `Return`) its first
    /// use places: its entry, return-address register and the joined
    /// row's slots and sources it was generated over, which a later
    /// use over the same row calls instead of repeating the code.
    #[cfg(feature = "sql")]
    pub(crate) hoisted: AVec<Hoisted>,
    /// The enclosing queries' aggregates folded into slots while a
    /// group expression's subqueries are generated (`G::owned_aggs`,
    /// today's `subst_owned_aggs`): the id of the placeholder plan
    /// that stands for each, and its slot.
    #[cfg(feature = "sql")]
    pub(crate) preset: AVec<(usize, u32)>,
    /// The shared materializations (`SubPlan::mat`): their row list
    /// and flag registers (built, complete).
    #[cfg(feature = "sql")]
    pub(crate) mats: AVec<(u32, u32, u32)>,
    /// The recursive CTEs being generated: each one's id and the row
    /// list holding the row its queue last gave up.
    #[cfg(feature = "sql")]
    pub(crate) recs: AVec<(u32, u32)>,
}

/// One uncorrelated subquery node of a program (`Builder::hoisted`):
/// its key (plan id, result column), slot, "computed" register, and
/// its shared code's entry, return-address register and joined-row
/// position.
#[cfg(feature = "sql")]
pub(crate) struct Hoisted {
    pub(crate) key: (usize, usize),
    pub(crate) slot: u32,
    pub(crate) once: u32,
    pub(crate) code: Option<(Label, u32, (usize, usize))>,
}

/// The most instructions (and side-table entries) code generation
/// makes before it gives up: a statement whose program would be
/// bigger runs on today's executor, and the generator stops as soon
/// as it is past this, so its time and memory stay bounded (the
/// planner's alias copies can make a short statement's plan grow
/// exponentially with its nesting).
#[cfg(feature = "sql")]
const MAX_OPS: usize = 1 << 16;

impl Builder {
    /// Whether the program is already declined or past `MAX_OPS`:
    /// generating more of it is wasted work.
    #[cfg(feature = "sql")]
    pub(crate) fn over(&mut self) -> bool {
        if self.ops.len() > MAX_OPS || self.p4.len() > MAX_OPS {
            self.bad = true;
        }
        self.bad
    }

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
            fixups3: AVec::new(),
            consts: AVec::new(),
            bad: false,
            #[cfg(feature = "sql")]
            nsub: 0,
            #[cfg(feature = "sql")]
            hoisted: AVec::new(),
            #[cfg(feature = "sql")]
            preset: AVec::new(),
            #[cfg(feature = "sql")]
            mats: AVec::new(),
            #[cfg(feature = "sql")]
            recs: AVec::new(),
        }
    }

    /// Mark the program as one the generator does not cover: `finish`
    /// declines it, and the statement runs on today's executor whole.
    #[cfg(feature = "sql")]
    pub(crate) fn fail(&mut self) {
        self.bad = true;
    }

    /// Whether the program was marked as not covered.
    pub(crate) fn failed(&self) -> bool {
        self.bad
    }

    /// A fresh subquery result slot.
    #[cfg(feature = "sql")]
    pub(crate) fn slot(&mut self) -> u32 {
        let s = self.nsub;
        match self.nsub.checked_add(1) {
            Some(x) => self.nsub = x,
            None => self.bad = true,
        }
        s
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

    /// Append an instruction whose `p2` jumps to `to` and whose `p3`
    /// jumps to `to3`.
    #[cfg(feature = "expr_index")]
    pub(crate) fn jump2(&mut self, code: Code, p1: u32, to: Label, to3: Label, p4: u32) {
        let at = self.ops.len();
        self.emit(code, p1, 0, 0, p4, 0);
        self.fixups.push((at, to.0));
        self.fixups3.push((at, to3.0));
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
        for (at, l) in self.fixups3.iter() {
            let to = (*self.labels.as_slice().get(*l)?)?;
            self.ops.as_mut_slice().get_mut(*at)?.p3 = to;
        }
        Some(Program {
            ops: self.ops,
            p4: self.p4,
            nreg: self.nreg as usize,
            ncur: usize::from(self.ncur),
            #[cfg(feature = "sql")]
            nsub: self.nsub as usize,
        })
    }
}
