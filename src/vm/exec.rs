//! The dispatch loop (ADR 0005 §Design): sync and monomorphic — it
//! reads pages through `PagerCore` and evaluates expressions with
//! `query::eval`, and returns to the driver for every row, the end,
//! and every page it does not have.

use super::cursor::{Cursor, Sorter, TableCursor};
use super::op::{Code, EVAL_COND, EVAL_CONSTS, P4, p4_at};
use super::{Exec, Program};
use crate::arena::AVec;
use crate::error::Error;
use crate::pager::PagerCore;
use crate::query::{Ctx, Env, RowData};
use crate::record::OwnedValue;

/// A program in execution: what `Statement` keeps between steps.
pub(crate) struct Vm {
    prog: Program,
    pc: usize,
    regs: AVec<OwnedValue>,
    curs: AVec<Cursor>,
    /// The pages the opcode at `pc` has read so far (the driver keeps
    /// them in the cache while it services a miss). Cleared when an
    /// opcode completes.
    pins: AVec<u32>,
    /// The last result row and its rowid (`take_row`).
    out: AVec<OwnedValue>,
    out_rowid: Option<i64>,
    empty: RowData,
}

/// The outcome of one opcode: go on at `pc + 1`, jump, or leave the
/// loop with `pc` unchanged (`Stay`) or advanced (`Yield`).
enum Flow {
    Next,
    Jump(u32),
    Stay(Exec),
    Yield(Exec),
}

fn reg_mut(regs: &mut AVec<OwnedValue>, r: u32) -> Result<&mut OwnedValue, Error<()>> {
    regs.as_mut_slice()
        .get_mut(r as usize)
        .ok_or(Error::Unsupported)
}

fn reg(regs: &AVec<OwnedValue>, r: u32) -> Result<&OwnedValue, Error<()>> {
    regs.as_slice().get(r as usize).ok_or(Error::Unsupported)
}

fn int_of(v: &OwnedValue) -> Option<i64> {
    match v {
        OwnedValue::Integer(i) => Some(*i),
        _ => None,
    }
}

/// The table cursor `c` (and the table it reads).
fn table(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut TableCursor, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Table(t)) => Ok(t),
        _ => Err(Error::Unsupported),
    }
}

fn sorter(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut Sorter, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Sorter(s)) => Ok(s),
        _ => Err(Error::Unsupported),
    }
}

impl Vm {
    pub(crate) fn new(prog: Program) -> Vm {
        let mut vm = Vm {
            prog,
            pc: 0,
            regs: AVec::new(),
            curs: AVec::new(),
            pins: AVec::new(),
            out: AVec::new(),
            out_rowid: None,
            empty: RowData::empty(),
        };
        vm.reset();
        vm
    }

    /// Back to the start: the next run executes the program afresh
    /// (after an error, as today's executors start over).
    pub(crate) fn reset(&mut self) {
        self.pc = 0;
        self.regs.clear();
        for _ in 0..self.prog.nreg {
            self.regs.push(OwnedValue::Null);
        }
        self.curs.clear();
        for _ in 0..self.prog.ncur {
            self.curs.push(Cursor::Closed);
        }
        self.pins.clear();
        self.out.clear();
        self.out_rowid = None;
    }

    /// Whether the program runs on the VM (holds no `Host`).
    pub(crate) fn on_vm(&self) -> bool {
        self.prog.on_vm()
    }

    /// The pages the pending opcode keeps (`Pager::vm_service`).
    pub(crate) fn pins(&self) -> &[u32] {
        self.pins.as_slice()
    }

    /// The row `Exec::Row` announced, swapped into `into`, and its
    /// rowid.
    pub(crate) fn take_row(&mut self, into: &mut AVec<OwnedValue>) -> Option<i64> {
        core::mem::swap(into, &mut self.out);
        self.out_rowid
    }

    /// Run from `pc` until a row, the end, a miss or a hand-off.
    pub(crate) fn run(&mut self, core: &mut PagerCore<'_>, env: &Env) -> Result<Exec, Error<()>> {
        loop {
            let Some(op) = self.prog.ops.as_slice().get(self.pc).copied() else {
                return Err(Error::Unsupported);
            };
            match self.op(op, core, env)? {
                Flow::Next => self.pc = self.pc.saturating_add(1),
                Flow::Jump(to) => self.pc = to as usize,
                Flow::Stay(e) => {
                    // A miss keeps the pages the opcode read; the end
                    // and a hand-off hold none.
                    if !matches!(e, Exec::Pending(_)) {
                        self.pins.clear();
                    }
                    return Ok(e);
                }
                Flow::Yield(e) => {
                    self.pc = self.pc.saturating_add(1);
                    self.pins.clear();
                    return Ok(e);
                }
            }
            self.pins.clear();
        }
    }

    /// Execute one instruction. A pending I/O returns `Stay(Pending)`
    /// before anything is written.
    fn op(
        &mut self,
        op: super::op::Op,
        core: &mut PagerCore<'_>,
        env: &Env,
    ) -> Result<Flow, Error<()>> {
        /// A cursor step's result as a flow: pending stays; `jump_if`
        /// decides whether the step's bool jumps.
        macro_rules! step {
            ($e:expr, $jump_if:expr) => {
                match $e? {
                    Err(io) => Flow::Stay(Exec::Pending(io)),
                    Ok(b) if b == $jump_if => Flow::Jump(op.p2),
                    Ok(_) => Flow::Next,
                }
            };
        }
        Ok(match op.code {
            Code::Init | Code::Goto => Flow::Jump(op.p2),
            Code::Halt => Flow::Stay(Exec::Done),
            Code::Host => Flow::Stay(Exec::Host(op.p1)),
            Code::If | Code::IfNot => {
                let t = crate::query::truthy(reg(&self.regs, u32::from(op.p1))?) == Some(true);
                if t == (op.code == Code::If) {
                    Flow::Jump(op.p2)
                } else {
                    Flow::Next
                }
            }
            Code::IfPos => {
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                match int_of(r) {
                    Some(i) if i > 0 => {
                        *r = OwnedValue::Integer(i.saturating_sub(i64::from(op.p3)));
                        Flow::Jump(op.p2)
                    }
                    _ => Flow::Next,
                }
            }
            Code::DecrJumpZero => {
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                let Some(i) = int_of(r) else {
                    return Err(Error::Unsupported);
                };
                let i = i.saturating_sub(1);
                *r = OwnedValue::Integer(i);
                if i == 0 {
                    Flow::Jump(op.p2)
                } else {
                    Flow::Next
                }
            }
            #[cfg(feature = "expr_index")]
            Code::IsNull => {
                if matches!(reg(&self.regs, u32::from(op.p1))?, OwnedValue::Null) {
                    Flow::Jump(op.p2)
                } else {
                    Flow::Next
                }
            }
            #[cfg(feature = "expr_index")]
            Code::Gt | Code::Lt => {
                let a = reg(&self.regs, u32::from(op.p1))?;
                let b = reg(&self.regs, op.p3)?;
                if matches!(a, OwnedValue::Null) || matches!(b, OwnedValue::Null) {
                    Flow::Next
                } else {
                    let o = crate::record::compare_values(&a.as_value(), &b.as_value());
                    let want = if op.code == Code::Gt {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Less
                    };
                    if o == want {
                        Flow::Jump(op.p2)
                    } else {
                        Flow::Next
                    }
                }
            }
            Code::Value => {
                let Some(P4::Value(v)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                *reg_mut(&mut self.regs, op.p2)? = v.clone();
                Flow::Next
            }
            Code::Copy => {
                let v = reg(&self.regs, u32::from(op.p1))?.clone();
                *reg_mut(&mut self.regs, op.p2)? = v;
                Flow::Next
            }
            Code::MustBeInt => {
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                let v = core::mem::replace(r, OwnedValue::Null);
                *r = OwnedValue::Integer(crate::query::limit_int::<()>(v)?);
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::Affinity => {
                let Some(P4::Aff(a)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let a = *a;
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                let v = core::mem::replace(r, OwnedValue::Null);
                *r = crate::query::probe_value(v, a);
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::RowidBound => {
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                let lower = op.p5 & 1 != 0;
                let incl = op.p5 & 2 != 0;
                match crate::session::rowid_end(r, lower, incl) {
                    None => Flow::Jump(op.p2),
                    Some(b) => {
                        *r = b.map_or(OwnedValue::Null, OwnedValue::Integer);
                        Flow::Next
                    }
                }
            }
            Code::Eval => {
                let Some(P4::Expr(e)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let row: &RowData = match usize::from(op.p1).checked_sub(1) {
                    None => &self.empty,
                    Some(c) => {
                        let t = match self.curs.as_mut_slice().get_mut(c) {
                            Some(Cursor::Table(t)) => t,
                            _ => return Err(Error::Unsupported),
                        };
                        let info = table_info(&self.prog, t.info)?;
                        if let Err(io) = t.load(core, &mut self.pins, info)? {
                            return Ok(Flow::Stay(Exec::Pending(io)));
                        }
                        &t.row
                    }
                };
                let ctx = Ctx::of(row).with(env);
                let v = match op.p5 {
                    EVAL_COND => OwnedValue::Integer(i64::from(crate::query::cond::<()>(e, &ctx)?)),
                    EVAL_CONSTS => OwnedValue::Integer(i64::from(
                        crate::query::join_constants_hold::<()>(Some(e), &[], &ctx)?,
                    )),
                    _ => crate::query::eval::<()>(e, &ctx)?,
                };
                *reg_mut(&mut self.regs, op.p2)? = v;
                Flow::Next
            }
            Code::OpenRead => {
                let Some(P4::Table(info)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Table(alloc::boxed::Box::new(TableCursor::new(info.root, op.p4)));
                Flow::Next
            }
            Code::Rewind => step!(
                table(&mut self.curs, op.p1)?.rewind(core, &mut self.pins),
                false
            ),
            Code::Next => step!(
                table(&mut self.curs, op.p1)?.next(core, &mut self.pins),
                true
            ),
            #[cfg(feature = "expr_index")]
            Code::Last => step!(
                table(&mut self.curs, op.p1)?.last(core, &mut self.pins),
                false
            ),
            #[cfg(feature = "expr_index")]
            Code::Prev => step!(
                table(&mut self.curs, op.p1)?.prev(core, &mut self.pins),
                true
            ),
            Code::SeekRowid => {
                let k = match reg(&self.regs, op.p3)? {
                    OwnedValue::Integer(i) => Some(*i),
                    OwnedValue::Real(f) if (*f as i64) as f64 == *f => Some(*f as i64),
                    _ => None,
                };
                match k {
                    None => Flow::Jump(op.p2),
                    Some(k) => step!(
                        table(&mut self.curs, op.p1)?.seek_rowid(core, &mut self.pins, k),
                        false
                    ),
                }
            }
            #[cfg(feature = "expr_index")]
            Code::SeekGE | Code::SeekLE => {
                let Some(k) = int_of(reg(&self.regs, op.p3)?) else {
                    return Err(Error::Unsupported);
                };
                let t = table(&mut self.curs, op.p1)?;
                if op.code == Code::SeekGE {
                    step!(t.seek_ge(core, &mut self.pins, k), false)
                } else {
                    step!(t.seek_le(core, &mut self.pins, k), false)
                }
            }
            Code::Column => {
                let t = table(&mut self.curs, op.p1)?;
                let info = table_info(&self.prog, t.info)?;
                if let Err(io) = t.load(core, &mut self.pins, info)? {
                    return Ok(Flow::Stay(Exec::Pending(io)));
                }
                let v = t
                    .row
                    .vals
                    .as_slice()
                    .get(op.p2 as usize)
                    .cloned()
                    .unwrap_or(OwnedValue::Null);
                *reg_mut(&mut self.regs, op.p3)? = v;
                Flow::Next
            }
            Code::Rowid => {
                let v = table(&mut self.curs, op.p1)?
                    .rowid()
                    .map_or(OwnedValue::Null, OwnedValue::Integer);
                *reg_mut(&mut self.regs, op.p2)? = v;
                Flow::Next
            }
            Code::SorterOpen => {
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Sorter(alloc::boxed::Box::new(Sorter::new(op.p4)));
                Flow::Next
            }
            Code::SorterInsert => {
                let s = sorter(&mut self.curs, op.p1)?;
                let info = sort_info(&self.prog, s.info)?;
                let n = info.nout.saturating_add(info.orders.len());
                let window = match info.window {
                    None => None,
                    Some(r) => {
                        let lim = int_of(reg(&self.regs, r)?).unwrap_or(-1);
                        let off = int_of(reg(&self.regs, r.saturating_add(1))?).unwrap_or(0);
                        if lim < 0 {
                            None
                        } else {
                            Some((
                                usize::try_from(off.max(0)).unwrap_or(usize::MAX),
                                usize::try_from(lim).unwrap_or(usize::MAX),
                            ))
                        }
                    }
                };
                let from = op.p2 as usize;
                let vals = from
                    .checked_add(n)
                    .and_then(|end| self.regs.as_slice().get(from..end))
                    .ok_or(Error::Unsupported)?;
                let rowid = rowid_of(&self.curs, op.p3);
                let enc = core.text_enc();
                sorter(&mut self.curs, op.p1)?.insert(info, vals, rowid, window, enc);
                Flow::Next
            }
            Code::SorterSort => {
                let enc = core.text_enc();
                let s = sorter(&mut self.curs, op.p1)?;
                let info = sort_info(&self.prog, s.info)?;
                if s.sort(info, enc) {
                    Flow::Next
                } else {
                    Flow::Jump(op.p2)
                }
            }
            Code::SorterData => {
                let s = sorter(&mut self.curs, op.p1)?;
                let n = sort_info(&self.prog, s.info)?.nout;
                let from = op.p2 as usize;
                let dest = from
                    .checked_add(n)
                    .and_then(|end| self.regs.as_mut_slice().get_mut(from..end))
                    .ok_or(Error::Unsupported)?;
                s.data(dest);
                Flow::Next
            }
            Code::SorterNext => {
                if sorter(&mut self.curs, op.p1)?.next() {
                    Flow::Jump(op.p2)
                } else {
                    Flow::Next
                }
            }
            Code::ResultRow => {
                let from = usize::from(op.p1);
                let vals = from
                    .checked_add(op.p2 as usize)
                    .and_then(|end| self.regs.as_slice().get(from..end))
                    .ok_or(Error::Unsupported)?;
                self.out.clear();
                self.out.extend_from_slice(vals);
                self.out_rowid = rowid_of(&self.curs, op.p3);
                Flow::Yield(Exec::Row)
            }
        })
    }
}

/// The table a cursor reads (its `OpenRead`'s `P4::Table`).
fn table_info(prog: &Program, at: u32) -> Result<&super::cursor::TableInfo, Error<()>> {
    match p4_at(&prog.p4, at) {
        Some(P4::Table(t)) => Ok(t),
        _ => Err(Error::Unsupported),
    }
}

/// The keys a sorter was opened with (its `SorterOpen`'s
/// `P4::Sorter`).
fn sort_info(prog: &Program, at: u32) -> Result<&super::cursor::SortInfo, Error<()>> {
    match p4_at(&prog.p4, at) {
        Some(P4::Sorter(s)) => Ok(s),
        _ => Err(Error::Unsupported),
    }
}

/// The rowid source `p` (cursor `p - 1`; 0: none).
fn rowid_of(curs: &AVec<Cursor>, p: u32) -> Option<i64> {
    let c = (p as usize).checked_sub(1)?;
    match curs.as_slice().get(c)? {
        Cursor::Table(t) => t.rowid(),
        Cursor::Sorter(s) => s.rowid,
        Cursor::Closed => None,
    }
}
