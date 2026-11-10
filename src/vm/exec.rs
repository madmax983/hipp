//! The dispatch loop (ADR 0005 §Design): sync and monomorphic — it
//! reads pages through `PagerCore` and evaluates expressions with
//! `query::eval`, and returns to the driver for every row, the end,
//! and every page it does not have.

use super::cursor::{Cursor, Sorter, TableCursor};
use super::op::{ACC_ROWID, Code, EVAL_ACC, EVAL_COND, EVAL_CONSTS, EVAL_GROUP, P4, p4_at};
#[cfg(feature = "sql")]
use super::op::{EVAL_FALSE, EVAL_SUB};
use super::rows::{AggRun, Eph, Groups};
use super::{Exec, Program};
use crate::arena::AVec;
use crate::error::Error;
use crate::pager::{Held, PagerCore};
use crate::query::{Ctx, Env, RowData};
use crate::record::OwnedValue;

/// A program in execution: what `Statement` keeps between steps.
pub(crate) struct Vm {
    prog: Program,
    pc: usize,
    regs: AVec<OwnedValue>,
    curs: AVec<Cursor>,
    /// The pages the opcode at `pc` has read so far (copied when it
    /// misses, so its next run reads them whatever the driver
    /// evicts). Cleared when an opcode completes.
    pins: Held,
    /// The last result row and its rowid (`take_row`).
    out: AVec<OwnedValue>,
    out_rowid: Option<i64>,
    empty: RowData,
    /// The joined row so far: each source's values from its slot
    /// base, its rowid at its index (`Load` / `NullRow`).
    acc: RowData,
    /// Each subquery node's result (`SubStart` / `SubRow`).
    #[cfg(feature = "sql")]
    subs: AVec<crate::query::SubValue>,
    /// The subqueries running, innermost last: where a statement
    /// error inside one goes (`SubStart`).
    #[cfg(feature = "sql")]
    traps: AVec<Trap>,
    /// Joined rows `AccPush` saved.
    #[cfg(feature = "sql")]
    saved: AVec<RowData>,
    /// Subquery-plan executions since the driver last took them
    /// (`Connection::subquery_executions`).
    #[cfg(feature = "sql")]
    pub(crate) execs: usize,
    /// The joined-row values lateral table-valued calls cleared, by
    /// cursor (`Restore` puts them back).
    #[cfg(feature = "sql")]
    dead: AVec<(u16, usize, OwnedValue)>,
    /// The current window unit's call values (`WinNext`, `EVAL_WIN`).
    #[cfg(feature = "window")]
    win_vals: AVec<OwnedValue>,
    /// The side tables' lengths when the program started, and the
    /// lengths below which entries stay (`SidePin`).
    #[cfg(any(feature = "json", feature = "fts5"))]
    side: Option<((usize, usize), (usize, usize))>,
}

/// A running subquery's error trap: its slot, the instruction its
/// error continues at, and how many joined rows were saved when it
/// started (an error between an `AccPush` and its `AccPop` restores
/// the row).
#[cfg(feature = "sql")]
struct Trap {
    slot: u32,
    handler: u32,
    depth: usize,
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
            pins: Held::default(),
            out: AVec::new(),
            out_rowid: None,
            empty: RowData::empty(),
            acc: RowData::empty(),
            #[cfg(feature = "sql")]
            subs: AVec::new(),
            #[cfg(feature = "sql")]
            traps: AVec::new(),
            #[cfg(feature = "sql")]
            saved: AVec::new(),
            #[cfg(feature = "sql")]
            execs: 0,
            #[cfg(feature = "sql")]
            dead: AVec::new(),
            #[cfg(feature = "window")]
            win_vals: AVec::new(),
            #[cfg(any(feature = "json", feature = "fts5"))]
            side: None,
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
        self.acc = RowData::empty();
        #[cfg(feature = "sql")]
        {
            self.subs.clear();
            for _ in 0..self.prog.nsub {
                self.subs
                    .push(crate::query::SubValue::One(OwnedValue::Null, None));
            }
            self.traps.clear();
            self.saved.clear();
            self.dead.clear();
        }
        #[cfg(feature = "window")]
        self.win_vals.clear();
        #[cfg(any(feature = "json", feature = "fts5"))]
        {
            self.side = None;
        }
    }

    /// Whether the program runs on the VM (holds no `Host`).
    pub(crate) fn on_vm(&self) -> bool {
        self.prog.on_vm()
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
            let flow = match self.op(op, core, env) {
                Ok(f) => f,
                #[cfg(feature = "sql")]
                Err(Error::Schema { kind }) if !self.traps.is_empty() => {
                    self.trap(kind);
                    continue;
                }
                Err(e) => return Err(e),
            };
            match flow {
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

    /// A statement error inside the innermost running subquery: its
    /// slot becomes the error (raised where an expression reads it,
    /// today's `exec_sub_kind_deferred`) and the program goes on after
    /// it, with the joined row it started with.
    #[cfg(feature = "sql")]
    fn trap(&mut self, kind: crate::error::SchemaKind) {
        let Some(t) = self.traps.pop() else {
            return;
        };
        while self.saved.len() > t.depth {
            if let Some(r) = self.saved.pop() {
                self.acc = r;
            }
        }
        if let Some(s) = self.subs.as_mut_slice().get_mut(t.slot as usize) {
            *s = crate::query::SubValue::Fail(kind);
        }
        self.pins.clear();
        self.pc = t.handler as usize;
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
                // An expression with subquery nodes reads their slots'
                // values first (`query::substitute`, or with its
                // aggregates' folds `substitute_group`).
                #[cfg(feature = "sql")]
                let owned: Option<crate::query::RExpr> = match p4_at(&self.prog.p4, op.p4) {
                    Some(P4::SubExpr(x)) if op.p5 & EVAL_SUB != 0 => {
                        let mut vals = AVec::with_capacity(x.slots.len());
                        for s in x.slots.iter() {
                            vals.push(
                                self.subs
                                    .as_slice()
                                    .get(*s as usize)
                                    .cloned()
                                    .ok_or(Error::Unsupported)?,
                            );
                        }
                        let mut pos = 0;
                        Some(match &x.aggs {
                            None => crate::query::substitute(&x.e, vals.as_slice(), &mut pos),
                            Some(regs) => {
                                let mut av = AVec::with_capacity(regs.len());
                                for r in regs.iter() {
                                    av.push(reg(&self.regs, *r)?.clone());
                                }
                                let mut apos = 0;
                                crate::query::substitute_group(
                                    &x.e,
                                    vals.as_slice(),
                                    &mut pos,
                                    av.as_slice(),
                                    &mut apos,
                                )
                            }
                        })
                    }
                    _ => None,
                };
                #[cfg(feature = "sql")]
                let e: &crate::query::RExpr = match (&owned, p4_at(&self.prog.p4, op.p4)) {
                    (Some(e), _) => e,
                    (None, Some(P4::Expr(e))) => e,
                    _ => return Err(Error::Unsupported),
                };
                #[cfg(not(feature = "sql"))]
                let Some(P4::Expr(e)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                // A window template: its calls' values for the current
                // unit first (`window::fill`).
                #[cfg(feature = "window")]
                let filled: Option<crate::query::RExpr> = if op.p5 & super::op::EVAL_WIN != 0 {
                    let mut w = e.clone();
                    crate::window::fill(&mut w, self.win_vals.as_slice());
                    Some(w)
                } else {
                    None
                };
                #[cfg(feature = "window")]
                let e: &crate::query::RExpr = filled.as_ref().unwrap_or(e);
                let ctx = if op.p5 & EVAL_GROUP != 0 {
                    let g = match usize::from(op.p1)
                        .checked_sub(1)
                        .and_then(|c| self.curs.as_slice().get(c))
                    {
                        Some(Cursor::Groups(g)) => g,
                        _ => return Err(Error::Unsupported),
                    };
                    Ctx::group(g.rows(), &g.rep).with(env)
                } else if op.p5 & EVAL_ACC != 0 {
                    Ctx::of(&self.acc).with(env)
                } else {
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
                    Ctx::of(row).with(env)
                };
                #[cfg(feature = "sql")]
                let plain = matches!(p4_at(&self.prog.p4, op.p4), Some(P4::SubExpr(x)) if x.plain);
                #[cfg(feature = "sql")]
                if op.p5 & EVAL_SUB != 0 && op.p5 & 3 != 0 && !plain {
                    // A condition over subquery values: the value's
                    // truth (`cond_is_sub`'s leaf).
                    let v = crate::query::eval::<()>(e, &ctx)?;
                    let want = op.p5 & 3 == EVAL_COND;
                    let t = crate::query::truthy(&v) == Some(want);
                    *reg_mut(&mut self.regs, op.p2)? = OwnedValue::Integer(i64::from(t));
                    return Ok(Flow::Next);
                }
                let v = match op.p5 & 3 {
                    EVAL_COND => OwnedValue::Integer(i64::from(crate::query::cond::<()>(e, &ctx)?)),
                    #[cfg(feature = "sql")]
                    EVAL_FALSE => {
                        OwnedValue::Integer(i64::from(crate::query::cond_is::<()>(e, &ctx, false)?))
                    }
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
                let rowid = self.rowid_of(op.p3);
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
                self.out_rowid = self.rowid_of(op.p3);
                Flow::Yield(Exec::Row)
            }
            _ => return self.op_join(op, core, env),
        })
    }

    /// The rowid source `p` (cursor `p - 1`; 0: none; `ACC_ROWID`:
    /// the joined row's first source).
    fn rowid_of(&self, p: u32) -> Option<i64> {
        if p == ACC_ROWID {
            return self.acc.rowids.as_slice().first().copied().flatten();
        }
        rowid_of(&self.curs, p)
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
        Cursor::Eph(e) => e.cur().and_then(|r| r.1),
        #[cfg(feature = "sql")]
        Cursor::Host(h) => h.rowid,
        _ => None,
    }
}

/// A row's values and rowid, borrowed from its cursor.
type RowRef<'a> = (&'a [OwnedValue], Option<i64>);

/// The current row of cursor `c` (a table's, decoded, or a row
/// list's) and its rowid; a pending page leaves everything as it
/// was.
fn row_at<'a>(
    curs: &'a mut AVec<Cursor>,
    prog: &Program,
    core: &mut PagerCore<'_>,
    pins: &mut Held,
    c: u16,
) -> Result<core::result::Result<RowRef<'a>, crate::pager::Io>, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Table(t)) => {
            let info = table_info(prog, t.info)?;
            if let Err(io) = t.load(core, pins, info)? {
                return Ok(Err(io));
            }
            let rowid = t.rowid();
            Ok(Ok((t.row.vals.as_slice(), rowid)))
        }
        Some(Cursor::Eph(e)) => match e.cur() {
            Some((v, r)) => Ok(Ok((v.as_slice(), *r))),
            None => Err(Error::Unsupported),
        },
        #[cfg(feature = "sql")]
        Some(Cursor::Host(h)) => Ok(Ok((h.row.as_slice(), h.rowid))),
        _ => Err(Error::Unsupported),
    }
}

fn eph(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut Eph, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Eph(e)) => Ok(e),
        _ => Err(Error::Unsupported),
    }
}

fn groups(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut Groups, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Groups(g)) => Ok(g),
        _ => Err(Error::Unsupported),
    }
}

fn agg(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut AggRun, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Agg(a)) => Ok(a),
        _ => Err(Error::Unsupported),
    }
}

#[cfg(feature = "expr_index")]
fn index(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut super::index::IdxCursor, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Index(i)) => Ok(i),
        _ => Err(Error::Unsupported),
    }
}

/// The query plan a `P4::Plan` entry shares.
fn plan(prog: &Program, at: u32) -> Result<alloc::rc::Rc<crate::session::QueryPlan>, Error<()>> {
    match p4_at(&prog.p4, at) {
        Some(P4::Plan(p)) => Ok(alloc::rc::Rc::clone(p)),
        _ => Err(Error::Unsupported),
    }
}

/// `n` registers from `from`.
fn regs_at(regs: &AVec<OwnedValue>, from: u32, n: usize) -> Result<&[OwnedValue], Error<()>> {
    let from = from as usize;
    from.checked_add(n)
        .and_then(|end| regs.as_slice().get(from..end))
        .ok_or(Error::Unsupported)
}

/// The joined row cut back to source `src` (slot `base`).
fn cut(acc: &mut RowData, base: u32, src: u32) {
    acc.vals.truncate(base as usize);
    acc.rowids.truncate(src as usize);
    while acc.vals.len() < base as usize {
        acc.vals.push(OwnedValue::Null);
    }
    while acc.rowids.len() < src as usize {
        acc.rowids.push(None);
    }
}

/// The joined row's first `w` values and `n` rowids: the outer row a
/// subquery's aggregates and groups are opened under.
fn prefix_of(acc: &RowData, w: u32, n: u32) -> RowData {
    let mut r = RowData::empty();
    r.vals
        .extend_from_slice(acc.vals.as_slice().get(..w as usize).unwrap_or(&[]));
    r.rowids
        .extend_from_slice(acc.rowids.as_slice().get(..n as usize).unwrap_or(&[]));
    while r.vals.len() < w as usize {
        r.vals.push(OwnedValue::Null);
    }
    while r.rowids.len() < n as usize {
        r.rowids.push(None);
    }
    r
}

/// A flag register's truth (`p3`: 1 + the register; 0: false).
fn flag_of(regs: &AVec<OwnedValue>, p3: u32) -> bool {
    p3.checked_sub(1)
        .and_then(|r| regs.as_slice().get(r as usize))
        .is_some_and(|v| crate::query::truthy(v) == Some(true))
}

impl Vm {
    /// The opcodes of unit `vm_joins`: joins, row lists, aggregates,
    /// index cursors and the access walk.
    fn op_join(
        &mut self,
        op: super::op::Op,
        core: &mut PagerCore<'_>,
        env: &Env,
    ) -> Result<Flow, Error<()>> {
        #[cfg(feature = "expr_index")]
        macro_rules! step {
            ($e:expr, $jump_if:expr) => {
                match $e? {
                    Err(io) => Flow::Stay(Exec::Pending(io)),
                    Ok(b) if b == $jump_if => Flow::Jump(op.p2),
                    Ok(_) => Flow::Next,
                }
            };
        }
        let jump_if = |b: bool| if b { Flow::Jump(op.p2) } else { Flow::Next };
        Ok(match op.code {
            Code::Integer => {
                *reg_mut(&mut self.regs, op.p2)? = OwnedValue::Integer(i64::from(op.p3 as i32));
                Flow::Next
            }
            Code::Once => {
                let r = reg_mut(&mut self.regs, u32::from(op.p1))?;
                if matches!(r, OwnedValue::Integer(1)) {
                    Flow::Jump(op.p2)
                } else {
                    *r = OwnedValue::Integer(1);
                    Flow::Next
                }
            }
            Code::Load => {
                let (vals, rowid) =
                    match row_at(&mut self.curs, &self.prog, core, &mut self.pins, op.p1)? {
                        Err(io) => return Ok(Flow::Stay(Exec::Pending(io))),
                        Ok(r) => r,
                    };
                cut(&mut self.acc, op.p2, op.p3);
                self.acc.vals.extend_from_slice(vals);
                self.acc.rowids.push(rowid);
                Flow::Next
            }
            Code::NullRow => {
                cut(&mut self.acc, op.p2, op.p3);
                for _ in 0..op.p1 {
                    self.acc.vals.push(OwnedValue::Null);
                }
                self.acc.rowids.push(None);
                Flow::Next
            }
            Code::Consts => {
                let qp = plan(&self.prog, op.p4)?;
                let ctx = Ctx::of(&self.empty).with(env);
                let ok = crate::query::join_constants_hold::<()>(
                    qp.filter.as_ref(),
                    qp.sources.as_slice(),
                    &ctx,
                )?;
                *reg_mut(&mut self.regs, op.p2)? = OwnedValue::Integer(i64::from(ok));
                Flow::Next
            }
            Code::EphOpen => {
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                match slot {
                    Cursor::Eph(e) => e.clear(),
                    _ => *slot = Cursor::Eph(alloc::boxed::Box::default()),
                }
                Flow::Next
            }
            Code::EphRewind => jump_if(!eph(&mut self.curs, op.p1)?.rewind(op.p5 == 1)),
            Code::EphNext => jump_if(eph(&mut self.curs, op.p1)?.next(op.p5 == 1)),
            Code::EphAdd => {
                let flag = flag_of(&self.regs, op.p3);
                let (vals, rowid) = match row_at(
                    &mut self.curs,
                    &self.prog,
                    core,
                    &mut self.pins,
                    op.p2 as u16,
                )? {
                    Err(io) => return Ok(Flow::Stay(Exec::Pending(io))),
                    Ok((v, r)) => {
                        let mut c = AVec::with_capacity(v.len());
                        c.extend_from_slice(v);
                        (c, r)
                    }
                };
                eph(&mut self.curs, op.p1)?.push(vals, rowid, flag);
                Flow::Next
            }
            Code::EphMatch => {
                eph(&mut self.curs, op.p1)?.flag_current();
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::EphMark => {
                let e = eph(&mut self.curs, op.p1)?;
                e.mark = e.rows.len();
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::EphReverse => {
                eph(&mut self.curs, op.p1)?.reverse_tail();
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::EphNulls => {
                eph(&mut self.curs, op.p1)?.flagged_tail(op.p5 == 1);
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::EphKeep => {
                eph(&mut self.curs, op.p1)?.keep_current();
                Flow::Next
            }
            Code::Distinct => {
                let Some(P4::Colls(colls)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let vals = regs_at(&self.regs, op.p3, colls.len())?;
                let e = match self.curs.as_mut_slice().get_mut(usize::from(op.p1)) {
                    Some(Cursor::Eph(e)) => e,
                    _ => return Err(Error::Unsupported),
                };
                jump_if(e.seen(vals, colls.as_slice()))
            }
            Code::AggOpen => {
                let qp = plan(&self.prog, op.p4)?;
                let prefix = prefix_of(&self.acc, op.p2, op.p3);
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Agg(alloc::boxed::Box::new(AggRun::new(&qp, prefix)));
                Flow::Next
            }
            Code::AggStep => {
                let a = match self.curs.as_mut_slice().get_mut(usize::from(op.p1)) {
                    Some(Cursor::Agg(a)) => a,
                    _ => return Err(Error::Unsupported),
                };
                let kept = a.step(&self.acc, env)?;
                // A kept row (the bare columns') pins what it points
                // into (`stream::agg_step`).
                #[cfg(any(feature = "json", feature = "fts5"))]
                if kept {
                    let start = self.side_start(env).0;
                    self.side = Some((start, side_len(env)));
                }
                let _ = kept;
                Flow::Next
            }
            Code::AggFinal => {
                let qp = plan(&self.prog, op.p4)?;
                let n = qp.items.len();
                let from = op.p2 as usize;
                let a = agg(&mut self.curs, op.p1)?;
                let dest = from
                    .checked_add(n)
                    .and_then(|end| self.regs.as_mut_slice().get_mut(from..end))
                    .ok_or(Error::Unsupported)?;
                a.finish(&qp, env, dest)?;
                Flow::Next
            }
            Code::GroupOpen => {
                let qp = plan(&self.prog, op.p4)?;
                let prefix = prefix_of(&self.acc, op.p2, op.p3);
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Groups(alloc::boxed::Box::new(Groups::new(op.p4, &qp, prefix)));
                Flow::Next
            }
            Code::GroupAdd => {
                let g = groups(&mut self.curs, op.p1)?;
                let qp = plan(&self.prog, g.info)?;
                let key = regs_at(&self.regs, op.p3, qp.group_by.len())?;
                let g = match self.curs.as_mut_slice().get_mut(usize::from(op.p1)) {
                    Some(Cursor::Groups(g)) => g,
                    _ => return Err(Error::Unsupported),
                };
                g.add(&qp, &self.acc, key);
                Flow::Next
            }
            Code::GroupSort => {
                let g = groups(&mut self.curs, op.p1)?;
                let qp = plan(&self.prog, g.info)?;
                g.sort(&qp, core.text_enc());
                Flow::Next
            }
            Code::GroupNext => {
                let g = groups(&mut self.curs, op.p1)?;
                let qp = plan(&self.prog, g.info)?;
                jump_if(!g.next(&qp, env)?)
            }
            #[cfg(feature = "expr_index")]
            Code::IdxOpen => {
                let Some(P4::Index(ii)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let c = super::index::IdxCursor::new(op.p4, ii);
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Index(alloc::boxed::Box::new(c));
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::IdxRewind => step!(
                index(&mut self.curs, op.p1)?.rewind(core, &mut self.pins),
                false
            ),
            #[cfg(feature = "expr_index")]
            Code::IdxSeek => {
                let i = index(&mut self.curs, op.p1)?;
                let Some(P4::Index(ii)) = p4_at(&self.prog.p4, i.info) else {
                    return Err(Error::Unsupported);
                };
                let vals = regs_at(&self.regs, op.p3, op.p4 as usize)?;
                let probe = super::index::probe_key(vals, ii.desc.len());
                let i = index(&mut self.curs, op.p1)?;
                step!(
                    i.seek(core, &mut self.pins, probe.as_slice(), ii.desc.as_slice()),
                    false
                )
            }
            #[cfg(feature = "expr_index")]
            Code::IdxNext => step!(
                index(&mut self.curs, op.p1)?.next(core, &mut self.pins),
                true
            ),
            #[cfg(feature = "expr_index")]
            Code::IdxTest => {
                let Some(P4::Walk(w)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let enc = core.text_enc();
                let curs = self.curs.as_slice();
                let ent = match curs.get(usize::from(op.p1)) {
                    Some(Cursor::Index(i)) if i.valid => i.entry.as_slice(),
                    _ => return Err(Error::Unsupported),
                };
                let rowids = match w.rowids {
                    None => None,
                    Some(c) => match curs.get(c as usize) {
                        Some(Cursor::Eph(e)) => Some(e),
                        _ => return Err(Error::Unsupported),
                    },
                };
                let mut rv: AVec<OwnedValue> = AVec::new();
                if let Some(e) = rowids {
                    for (v, _) in e.rows.iter() {
                        if let Some(x) = v.as_slice().first() {
                            rv.push(x.clone());
                        }
                    }
                }
                let got = super::index::walk_test(
                    w,
                    ent,
                    self.regs.as_slice(),
                    rowids.map(|_| rv.as_slice()),
                    enc,
                )?;
                match got {
                    super::index::WalkStep::Stop => Flow::Jump(op.p2),
                    super::index::WalkStep::Skip => Flow::Jump(op.p3),
                    super::index::WalkStep::Found(null) => {
                        let fr = w.flag;
                        *reg_mut(&mut self.regs, fr)? = OwnedValue::Integer(i64::from(null));
                        Flow::Next
                    }
                }
            }
            #[cfg(feature = "expr_index")]
            Code::IdxRowid => {
                let r = index(&mut self.curs, op.p1)?.rowid()?;
                *reg_mut(&mut self.regs, op.p2)? = OwnedValue::Integer(r);
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::IdxCover => {
                let Some(P4::Cover(c)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let flag = flag_of(&self.regs, op.p3);
                let i = index(&mut self.curs, op.p2 as u16)?;
                let rowid = i.rowid()?;
                let row = super::index::cover_row(c, i.entry.as_slice(), rowid);
                eph(&mut self.curs, op.p1)?.push(row, Some(rowid), flag);
                Flow::Next
            }
            #[cfg(feature = "expr_index")]
            Code::ProbeList => {
                let Some(P4::Probe(p)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let vals = regs_at(&self.regs, op.p3, p.n)?;
                let list = super::index::probe_list(p, vals, core.text_enc());
                let e = eph(&mut self.curs, op.p1)?;
                e.clear();
                for v in list {
                    let mut row = AVec::with_capacity(1);
                    row.push(v);
                    e.push(row, None, false);
                }
                e.rewind(false);
                jump_if(e.rows.is_empty())
            }
            #[cfg(feature = "expr_index")]
            Code::ListValue => {
                let v = eph(&mut self.curs, op.p1)?
                    .cur()
                    .and_then(|r| r.0.as_slice().first().cloned())
                    .unwrap_or(OwnedValue::Null);
                *reg_mut(&mut self.regs, op.p2)? = v;
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::ListValues => {
                let mut vals = AVec::new();
                if let Some(r) = eph(&mut self.curs, op.p1)?.cur() {
                    vals.extend_from_slice(r.0.as_slice());
                }
                for j in 0..op.p3 {
                    let v = vals
                        .as_slice()
                        .get(j as usize)
                        .cloned()
                        .unwrap_or(OwnedValue::Null);
                    *reg_mut(&mut self.regs, op.p2.saturating_add(j))? = v;
                }
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::SkipAdd => {
                let Some(P4::Index(ii)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let n = op.p3 as usize;
                let mut vals: AVec<OwnedValue> = AVec::new();
                match self.curs.as_slice().get(op.p2 as usize) {
                    Some(Cursor::Index(i)) if i.valid => {
                        vals.extend_from_slice(i.entry.as_slice().get(..n).unwrap_or(&[]));
                    }
                    _ => return Err(Error::Unsupported),
                }
                let e = eph(&mut self.curs, op.p1)?;
                let last = e.rows.as_slice().last().map(|r| r.0.as_slice());
                if super::index::new_prefix(last, vals.as_slice(), n, ii.desc.as_slice()) {
                    e.push(vals, None, false);
                }
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::AutoSort => {
                let Some(P4::Auto(ai)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let enc = core.text_enc();
                let e = eph(&mut self.curs, op.p1)?;
                crate::session::auto_sort(e.rows.as_mut_slice(), ai, enc);
                if ai.bloom {
                    e.bloom = crate::session::bloom_build(e.rows.as_slice(), ai);
                }
                e.end = e.rows.len();
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::AutoSpan => {
                let Some(P4::Auto(ai)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let probe = regs_at(&self.regs, op.p3, ai.keys.len())?;
                let enc = core.text_enc();
                let e = match self.curs.as_mut_slice().get_mut(usize::from(op.p1)) {
                    Some(Cursor::Eph(e)) => e,
                    _ => return Err(Error::Unsupported),
                };
                let (lo, hi) = if crate::session::bloom_may(e.bloom.as_slice(), probe) {
                    crate::session::auto_span(e.rows.as_slice(), ai, probe, enc)
                } else {
                    (0, 0)
                };
                e.at = lo;
                e.end = hi;
                jump_if(lo >= hi)
            }
            #[cfg(feature = "planner")]
            Code::BloomAdd => {
                let Some(P4::BKeys(keys)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                if op.p5 == 1 {
                    let e = eph(&mut self.curs, op.p1)?;
                    e.bloom.clear();
                    for _ in 0..crate::session::BLOOM_BYTES {
                        e.bloom.push(0);
                    }
                    return Ok(Flow::Next);
                }
                let (vals, rowid) = match row_at(
                    &mut self.curs,
                    &self.prog,
                    core,
                    &mut self.pins,
                    op.p2 as u16,
                )? {
                    Err(io) => return Ok(Flow::Stay(Exec::Pending(io))),
                    Ok(r) => r,
                };
                let mut key: AVec<OwnedValue> = AVec::new();
                for k in keys.iter() {
                    key.push(match k {
                        crate::xplan::BKey::Col(c) => {
                            vals.get(*c).cloned().unwrap_or(OwnedValue::Null)
                        }
                        _ => rowid.map_or(OwnedValue::Null, OwnedValue::Integer),
                    });
                }
                let bits = crate::session::BLOOM_BYTES as u64 * 8;
                let h = crate::session::bloom_hash(key.as_slice()) % bits;
                let e = eph(&mut self.curs, op.p1)?;
                if let Some(b) = e.bloom.as_mut_slice().get_mut((h / 8) as usize) {
                    *b |= 1 << (h % 8);
                }
                Flow::Next
            }
            #[cfg(feature = "planner")]
            Code::BloomTest => {
                let Some(P4::BKeys(keys)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let probe = regs_at(&self.regs, op.p3, keys.len())?;
                let skip = keys.iter().zip(probe.iter()).any(|(k, v)| {
                    *k == crate::xplan::BKey::Rowid
                        && !matches!(v, OwnedValue::Integer(_) | OwnedValue::Real(_))
                });
                let e = match self.curs.as_slice().get(usize::from(op.p1)) {
                    Some(Cursor::Eph(e)) => e,
                    _ => return Err(Error::Unsupported),
                };
                jump_if(!skip && !crate::session::bloom_may(e.bloom.as_slice(), probe))
            }
            #[cfg(feature = "sql")]
            _ => return self.op_sub(op, core, env),
            #[cfg(not(feature = "sql"))]
            _ => return Err(Error::Unsupported),
        })
    }
}

/// An integer register's value (−1 for anything else: no bound).
#[cfg(feature = "sql")]
fn bound_of(regs: &AVec<OwnedValue>, r: u32) -> i64 {
    match regs.as_slice().get(r as usize) {
        Some(OwnedValue::Integer(i)) => *i,
        _ => -1,
    }
}

/// Register `p` (1 + the register; 0: none) as a row count bound:
/// `None` for no bound (absent or negative).
#[cfg(feature = "sql")]
fn opt_bound(regs: &AVec<OwnedValue>, p: u32) -> Option<i64> {
    let r = p.checked_sub(1)?;
    let v = bound_of(regs, r);
    (v >= 0).then_some(v)
}

#[cfg(feature = "sql")]
impl Vm {
    /// The opcodes of unit `vm_subqueries`: subquery slots and traps,
    /// the saved joined rows, aggregates over a group's rows, shared
    /// and growing row lists, compounds and the recursive queue.
    fn op_sub(
        &mut self,
        op: super::op::Op,
        core: &mut PagerCore<'_>,
        env: &Env,
    ) -> Result<Flow, Error<()>> {
        use super::op::SubKind;
        use crate::query::SubValue;
        let jump_if = |b: bool| if b { Flow::Jump(op.p2) } else { Flow::Next };
        let enc = core.text_enc();
        Ok(match op.code {
            Code::AccGroup => {
                self.acc = groups(&mut self.curs, op.p1)?.rep.clone();
                Flow::Next
            }
            Code::AccPush => {
                self.saved.push(self.acc.clone());
                Flow::Next
            }
            Code::AccPop => {
                self.acc = self.saved.pop().ok_or(Error::Unsupported)?;
                Flow::Next
            }
            Code::GroupRows | Code::GroupRowNext => {
                let first = op.code == Code::GroupRows;
                let row = groups(&mut self.curs, op.p1)?.row(first).cloned();
                let got = row.is_some();
                if let Some(r) = row {
                    self.acc = r;
                }
                jump_if(got != first)
            }
            Code::SubStart => {
                let Some(P4::Sub(info)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let v = match info.kind {
                    SubKind::Scalar => SubValue::One(OwnedValue::Null, info.aff),
                    SubKind::Exists => SubValue::One(OwnedValue::Integer(0), None),
                    SubKind::In => SubValue::Many(AVec::new(), info.aff),
                };
                *self
                    .subs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)? = v;
                self.traps.push(Trap {
                    slot: u32::from(op.p1),
                    handler: op.p2,
                    depth: self.saved.len(),
                });
                Flow::Next
            }
            Code::SubRow => {
                let Some(P4::Sub(info)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let vals = regs_at(&self.regs, op.p2, op.p3 as usize)?;
                let slot = self
                    .subs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                match info.kind {
                    SubKind::Scalar => {
                        let v = vals.get(info.col).cloned().unwrap_or(OwnedValue::Null);
                        *slot = SubValue::One(v, info.aff);
                    }
                    SubKind::Exists => *slot = SubValue::One(OwnedValue::Integer(1), None),
                    SubKind::In => {
                        if let SubValue::Many(vs, _) = slot {
                            vs.extend_from_slice(vals);
                        }
                    }
                }
                Flow::Next
            }
            Code::SubEnd => {
                self.traps.pop();
                Flow::Next
            }
            Code::Count => {
                self.execs = self.execs.saturating_add(1);
                Flow::Next
            }
            Code::EphPut => {
                let mut row = AVec::with_capacity(op.p3 as usize);
                row.extend_from_slice(regs_at(&self.regs, op.p2, op.p3 as usize)?);
                eph(&mut self.curs, op.p1)?.push(row, None, false);
                Flow::Next
            }
            Code::EphRow => {
                let mut vals = AVec::new();
                if let Some(r) = eph(&mut self.curs, op.p1)?.cur() {
                    vals.extend_from_slice(r.0.as_slice());
                }
                for j in 0..op.p3 {
                    let v = vals
                        .as_slice()
                        .get(j as usize)
                        .cloned()
                        .unwrap_or(OwnedValue::Null);
                    *reg_mut(&mut self.regs, op.p2.saturating_add(j))? = v;
                }
                Flow::Next
            }
            Code::EphCopy => {
                let mut rows = AVec::new();
                for (v, r) in eph(&mut self.curs, op.p2 as u16)?.rows.iter() {
                    rows.push((v.clone(), *r));
                }
                let e = eph(&mut self.curs, op.p1)?;
                e.clear();
                for (v, r) in rows {
                    e.push(v, r, false);
                }
                Flow::Next
            }
            Code::EphSeek => {
                let at = usize::try_from(bound_of(&self.regs, op.p3)).unwrap_or(0);
                let e = eph(&mut self.curs, op.p1)?;
                e.end = e.rows.len();
                e.at = at;
                jump_if(e.at >= e.end)
            }
            Code::Grow => {
                let n = bound_of(&self.regs, op.p3);
                let len = eph(&mut self.curs, op.p1)?.rows.len();
                let len = i64::try_from(len).unwrap_or(i64::MAX);
                if n >= 0 && len >= n {
                    *reg_mut(&mut self.regs, op.p4)? = OwnedValue::Integer(len);
                    *reg_mut(&mut self.regs, op.p3)? = OwnedValue::Integer(n.saturating_mul(2));
                    Flow::Next
                } else {
                    Flow::Jump(op.p2)
                }
            }
            Code::MatTest => {
                let need = bound_of(&self.regs, op.p3);
                let built = crate::query::truthy(reg(&self.regs, op.p4)?) == Some(true);
                let complete =
                    crate::query::truthy(reg(&self.regs, op.p4.saturating_add(1))?) == Some(true);
                let len = match self.curs.as_slice().get(usize::from(op.p1)) {
                    Some(Cursor::Eph(e)) => e.rows.len(),
                    _ => 0,
                };
                let len = i64::try_from(len).unwrap_or(i64::MAX);
                jump_if(built && (complete || (need >= 0 && len >= need)))
            }
            Code::MatDone => {
                let ask = bound_of(&self.regs, op.p3);
                let len = eph(&mut self.curs, op.p1)?.rows.len();
                let len = i64::try_from(len).unwrap_or(i64::MAX);
                *reg_mut(&mut self.regs, op.p4)? = OwnedValue::Integer(1);
                *reg_mut(&mut self.regs, op.p4.saturating_add(1))? =
                    OwnedValue::Integer(i64::from(ask < 0 || len < ask));
                Flow::Next
            }
            Code::Need => {
                let take = opt_bound(&self.regs, op.p2);
                let off = opt_bound(&self.regs, op.p3).unwrap_or(0);
                let take = match (take, opt_bound(&self.regs, op.p4)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let v = take.map_or(-1, |t| off.saturating_add(t));
                *reg_mut(&mut self.regs, u32::from(op.p1))? = OwnedValue::Integer(v);
                Flow::Next
            }
            Code::AggFold => {
                let Some(P4::Expr(a)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let p = crate::query::agg_parts(a).ok_or(Error::Unsupported)?;
                let mut rows = AVec::new();
                for (v, _) in eph(&mut self.curs, op.p1)?.rows.iter() {
                    rows.push(v.clone());
                }
                let v = crate::agg::fold::<()>(&p, rows, enc)?;
                *reg_mut(&mut self.regs, op.p2)? = v;
                Flow::Next
            }
            Code::Compound => {
                let Some(P4::Comp(c)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let rows_of = |curs: &mut AVec<Cursor>, k: u32| -> Result<_, Error<()>> {
                    let mut out: AVec<AVec<OwnedValue>> = AVec::new();
                    let c =
                        u16::try_from(op.p2.saturating_add(k)).map_err(|_| Error::Unsupported)?;
                    for (v, _) in eph(curs, c)?.rows.iter() {
                        out.push(v.clone());
                    }
                    Ok(out)
                };
                let head = rows_of(&mut self.curs, 0)?;
                let mut tails = AVec::new();
                for (k, o) in c.ops.iter().enumerate() {
                    tails.push((*o, rows_of(&mut self.curs, (k as u32).saturating_add(1))?));
                }
                let rows = if c.first_row {
                    crate::session::first_core_row(head, tails.as_slice())
                } else {
                    crate::session::run_compound(
                        head,
                        tails.as_slice(),
                        c.cores.as_slice(),
                        c.order.as_slice(),
                        enc,
                    )
                };
                let e = eph(&mut self.curs, op.p1)?;
                e.clear();
                for r in rows {
                    e.push(r, None, false);
                }
                Flow::Next
            }
            Code::QueueOpen => {
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Queue(alloc::boxed::Box::new(super::rows::Queue::new(op.p4)));
                Flow::Next
            }
            Code::QueueAdd | Code::QueuePop => {
                let q = match self.curs.as_mut_slice().get_mut(usize::from(op.p1)) {
                    Some(Cursor::Queue(q)) => q,
                    _ => return Err(Error::Unsupported),
                };
                let Some(P4::Queue(info)) = p4_at(&self.prog.p4, q.info) else {
                    return Err(Error::Unsupported);
                };
                if op.code == Code::QueueAdd {
                    let vals = regs_at(&self.regs, op.p2, op.p3 as usize)?;
                    q.add(info, vals, enc);
                    Flow::Next
                } else {
                    match q.pop(info, enc) {
                        None => Flow::Jump(op.p2),
                        Some(row) => {
                            let e = eph(&mut self.curs, op.p3 as u16)?;
                            e.clear();
                            e.push(row, None, false);
                            e.rewind(false);
                            Flow::Next
                        }
                    }
                }
            }
            #[cfg(feature = "expr_index")]
            Code::ProbeSub => {
                let Some(P4::Probe(p)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let mut vals: AVec<OwnedValue> = AVec::new();
                let raw: &[OwnedValue] = match self.subs.as_slice().get(op.p3 as usize) {
                    Some(SubValue::Many(vs, _)) => vs.as_slice(),
                    Some(SubValue::One(v, _)) => core::slice::from_ref(v),
                    Some(SubValue::Fail(kind)) => return Err(Error::Schema { kind: *kind }),
                    None => return Err(Error::Unsupported),
                };
                for v in raw.iter() {
                    let v = crate::query::probe_value(v.clone(), p.aff);
                    if !matches!(v, OwnedValue::Null) {
                        vals.push(v);
                    }
                }
                let list = super::index::probe_list(p, vals.as_slice(), enc);
                let e = eph(&mut self.curs, op.p1)?;
                e.clear();
                for v in list {
                    let mut row = AVec::with_capacity(1);
                    row.push(v);
                    e.push(row, None, false);
                }
                e.rewind(false);
                jump_if(e.rows.is_empty())
            }
            Code::Gosub => {
                let back = i64::try_from(self.pc.saturating_add(1)).unwrap_or(i64::MAX);
                *reg_mut(&mut self.regs, op.p3)? = OwnedValue::Integer(back);
                Flow::Jump(op.p2)
            }
            Code::Return => {
                let to = int_of(reg(&self.regs, op.p3)?).ok_or(Error::Unsupported)?;
                Flow::Jump(u32::try_from(to).map_err(|_| Error::Unsupported)?)
            }
            Code::RepStep => {
                let Some(P4::Colls(c)) = p4_at(&self.prog.p4, op.p4) else {
                    return Err(Error::Unsupported);
                };
                let coll = c.as_slice().first().copied().unwrap_or_default();
                let kind = if op.p3 == 1 {
                    crate::query::AggKind::Max
                } else {
                    crate::query::AggKind::Min
                };
                let v = reg(&self.regs, op.p2)?.clone();
                groups(&mut self.curs, op.p1)?.rep_step(kind, coll, v, enc);
                Flow::Next
            }
            _ => return self.op_rest(op, core, env),
        })
    }

    /// Subquery-plan executions counted since the last call.
    pub(crate) fn take_execs(&mut self) -> usize {
        core::mem::take(&mut self.execs)
    }
}

/// The window set `c`.
#[cfg(feature = "window")]
fn winset(curs: &mut AVec<Cursor>, c: u16) -> Result<&mut super::rest::WinSet, Error<()>> {
    match curs.as_mut_slice().get_mut(usize::from(c)) {
        Some(Cursor::Win(w)) => Ok(w),
        _ => Err(Error::Unsupported),
    }
}

/// The side tables' lengths (`Env::json_docs`, `Env::fts_rows`).
#[cfg(any(feature = "json", feature = "fts5"))]
pub(crate) fn side_len(env: &Env) -> (usize, usize) {
    #[cfg(feature = "json")]
    let j = env.json_docs.try_borrow().map_or(0, |d| d.len());
    #[cfg(not(feature = "json"))]
    let j = 0;
    #[cfg(feature = "fts5")]
    let f = env.fts_rows.try_borrow().map_or(0, |d| d.len());
    #[cfg(not(feature = "fts5"))]
    let f = 0;
    (j, f)
}

/// Drop the side-table entries past `to`.
#[cfg(any(feature = "json", feature = "fts5"))]
pub(crate) fn side_drop(env: &Env, to: (usize, usize)) {
    #[cfg(feature = "json")]
    if let Ok(mut d) = env.json_docs.try_borrow_mut() {
        d.truncate(to.0);
    }
    #[cfg(feature = "fts5")]
    if let Ok(mut d) = env.fts_rows.try_borrow_mut() {
        d.truncate(to.1);
    }
    let _ = to;
}

#[cfg(feature = "sql")]
impl Vm {
    /// The opcodes of unit `vm_rest`: driver-awaited sources, the
    /// side tables and window functions.
    #[allow(unused_variables)]
    fn op_rest(
        &mut self,
        op: super::op::Op,
        core: &mut PagerCore<'_>,
        env: &Env,
    ) -> Result<Flow, Error<()>> {
        Ok(match op.code {
            Code::HostRows | Code::HostOpen | Code::HostNext => Flow::Stay(Exec::Call),
            Code::Restore => {
                let mut i = 0;
                while let Some((c, slot, _)) = self.dead.as_slice().get(i) {
                    if *c != op.p1 {
                        i += 1;
                        continue;
                    }
                    let slot = *slot;
                    if let Some((_, _, v)) = self.dead.remove_at(i)
                        && let Some(x) = self.acc.vals.as_mut_slice().get_mut(slot)
                    {
                        *x = v;
                    }
                }
                Flow::Next
            }
            #[cfg(any(feature = "json", feature = "fts5"))]
            Code::SideMark => {
                let (j, f) = side_len(env);
                *reg_mut(&mut self.regs, u32::from(op.p1))? = OwnedValue::Integer(j as i64);
                *reg_mut(&mut self.regs, u32::from(op.p1).saturating_add(1))? =
                    OwnedValue::Integer(f as i64);
                Flow::Next
            }
            #[cfg(any(feature = "json", feature = "fts5"))]
            Code::SideDrop => {
                let at = |r: u32| -> Result<usize, Error<()>> {
                    Ok(int_of(reg(&self.regs, r)?)
                        .and_then(|i| usize::try_from(i).ok())
                        .unwrap_or(usize::MAX))
                };
                let to = (
                    at(u32::from(op.p1))?,
                    at(u32::from(op.p1).saturating_add(1))?,
                );
                let pin = self.side_start(env).1;
                side_drop(env, (to.0.max(pin.0), to.1.max(pin.1)));
                Flow::Next
            }
            #[cfg(any(feature = "json", feature = "fts5"))]
            Code::SidePin => {
                let now = side_len(env);
                let start = self.side_start(env).0;
                self.side = Some((start, now));
                Flow::Next
            }
            #[cfg(feature = "window")]
            Code::WinOpen => {
                let slot = self
                    .curs
                    .as_mut_slice()
                    .get_mut(usize::from(op.p1))
                    .ok_or(Error::Unsupported)?;
                *slot = Cursor::Win(alloc::boxed::Box::new(super::rest::WinSet {
                    info: op.p4,
                    ..Default::default()
                }));
                Flow::Next
            }
            #[cfg(feature = "window")]
            Code::WinRow => {
                let row = self.acc.clone();
                winset(&mut self.curs, op.p1)?.rows.push(row);
                Flow::Next
            }
            #[cfg(feature = "window")]
            Code::WinLoad => {
                let w = winset(&mut self.curs, op.p1)?;
                if op.p5 == 1 {
                    w.pos = 0;
                }
                match w.rows.as_slice().get(w.pos) {
                    None => Flow::Jump(op.p2),
                    Some(r) => {
                        self.acc = r.clone();
                        w.pos = w.pos.saturating_add(1);
                        Flow::Next
                    }
                }
            }
            #[cfg(feature = "window")]
            Code::WinAdd => {
                let info = winset(&mut self.curs, op.p1)?.info;
                let Some(P4::Win(calls)) = p4_at(&self.prog.p4, info) else {
                    return Err(Error::Unsupported);
                };
                let mut ins = AVec::with_capacity(calls.len());
                let mut r = op.p2;
                for c in calls.iter() {
                    let n = crate::window::children(c).len();
                    let mut v = AVec::with_capacity(n);
                    v.extend_from_slice(regs_at(&self.regs, r, n)?);
                    r = r.saturating_add(n as u32);
                    ins.push(v);
                }
                let (at, rep) = match op.p3.checked_sub(1) {
                    Some(gc) => {
                        let g = groups(&mut self.curs, gc as u16)?;
                        (g.current(), Some(g.rep.clone()))
                    }
                    None => {
                        let w = winset(&mut self.curs, op.p1)?;
                        (w.pos.saturating_sub(1), None)
                    }
                };
                winset(&mut self.curs, op.p1)?
                    .units
                    .push(super::rest::WinUnit { at, rep, ins });
                Flow::Next
            }
            #[cfg(feature = "window")]
            Code::WinCompute => {
                let enc = core.text_enc();
                let info = winset(&mut self.curs, op.p1)?.info;
                let Some(P4::Win(calls)) = p4_at(&self.prog.p4, info) else {
                    return Err(Error::Unsupported);
                };
                let refs: AVec<&crate::query::RExpr> = calls.iter().collect();
                let w = winset(&mut self.curs, op.p1)?;
                let mut ins = AVec::with_capacity(w.units.len());
                for u in w.units.as_mut_slice() {
                    ins.push(core::mem::take(&mut u.ins));
                }
                let (order, vals) = crate::window::compute(refs.as_slice(), ins.as_slice(), enc)
                    .map_err(|kind| Error::Schema { kind })?;
                w.order = order;
                w.vals = vals;
                w.next = 0;
                Flow::Next
            }
            #[cfg(feature = "window")]
            Code::WinNext => {
                let w = winset(&mut self.curs, op.p1)?;
                if op.p5 == 1 {
                    w.next = 0;
                }
                let Some(&u) = w.order.as_slice().get(w.next) else {
                    return Ok(Flow::Jump(op.p2));
                };
                w.next = w.next.saturating_add(1);
                self.win_vals = w.vals.as_slice().get(u).cloned().unwrap_or_default();
                let unit = w.units.as_slice().get(u).ok_or(Error::Unsupported)?;
                match &unit.rep {
                    Some(rep) => {
                        let (at, rep) = (unit.at, rep.clone());
                        let gc = op.p3.checked_sub(1).ok_or(Error::Unsupported)?;
                        groups(&mut self.curs, gc as u16)?.seek(at, rep);
                    }
                    None => {
                        self.acc = w
                            .rows
                            .as_slice()
                            .get(unit.at)
                            .cloned()
                            .unwrap_or_else(RowData::empty);
                    }
                }
                Flow::Next
            }
            _ => return Err(Error::Unsupported),
        })
    }

    /// The side tables' start and pinned lengths, the start taken now
    /// on the first call.
    #[cfg(any(feature = "json", feature = "fts5"))]
    fn side_start(&mut self, env: &Env) -> ((usize, usize), (usize, usize)) {
        *self.side.get_or_insert_with(|| {
            let now = side_len(env);
            (now, now)
        })
    }

    /// The side tables' lengths when the program started (what its
    /// rows may point into is past them), taken now when it starts.
    #[cfg(any(feature = "json", feature = "fts5"))]
    pub(crate) fn side_begin(&mut self, env: &Env) -> (usize, usize) {
        self.side_start(env).0
    }

    // ---- the driver's side of driver-awaited opcodes ----

    /// The driver-awaited instruction at `pc` (`Exec::Call`).
    #[cfg(feature = "sql")]
    pub(crate) fn call_op(&self) -> Option<super::op::Op> {
        self.prog.ops.as_slice().get(self.pc).copied()
    }

    /// The source `P4::Src` entry `at` names.
    #[cfg(feature = "sql")]
    pub(crate) fn src_info(&self, at: u32) -> Option<&super::op::SrcInfo> {
        match p4_at(&self.prog.p4, at) {
            Some(P4::Src(s)) => Some(s),
            _ => None,
        }
    }

    /// The query plan of `P4::Plan` entry `at`.
    #[cfg(feature = "sql")]
    pub(crate) fn plan_at(&self, at: u32) -> Option<alloc::rc::Rc<crate::session::QueryPlan>> {
        plan(&self.prog, at).ok()
    }

    /// The joined row cut back to source `src` (its values to slot
    /// `base`): the row a source's arguments and pre-checks read.
    #[cfg(feature = "sql")]
    pub(crate) fn acc_before(&self, base: usize, src: usize) -> RowData {
        prefix_of(&self.acc, base as u32, src as u32)
    }

    /// The bound in register `p3 - 1` (`p3` 0, or a negative value:
    /// none).
    #[cfg(feature = "sql")]
    pub(crate) fn bound(&self, p3: u32) -> Option<usize> {
        let r = p3.checked_sub(1)?;
        let v = int_of(reg(&self.regs, r).ok()?)?;
        usize::try_from(v).ok()
    }

    /// Whether the LIMIT capping register `r` holds a negative value
    /// (no LIMIT in effect).
    #[cfg(feature = "sql")]
    pub(crate) fn unlimited(&self, r: u32) -> bool {
        reg(&self.regs, r)
            .ok()
            .and_then(int_of)
            .is_none_or(|v| v < 0)
    }

    /// Row list `c` = `rows`.
    #[cfg(feature = "sql")]
    pub(crate) fn fill(&mut self, c: u16, rows: AVec<(AVec<OwnedValue>, Option<i64>)>) -> bool {
        let Ok(e) = eph(&mut self.curs, c) else {
            return false;
        };
        e.clear();
        for (v, r) in rows {
            e.push(v, r, false);
        }
        true
    }

    /// Host cursor `c`, opened anew when `fresh`.
    #[cfg(feature = "sql")]
    pub(crate) fn host(&mut self, c: u16, fresh: bool) -> Option<&mut super::rest::HostCur> {
        let slot = self.curs.as_mut_slice().get_mut(usize::from(c))?;
        if fresh || !matches!(slot, Cursor::Host(_)) {
            *slot = Cursor::Host(alloc::boxed::Box::default());
        }
        match slot {
            Cursor::Host(h) => Some(h),
            _ => None,
        }
    }

    /// Clear the joined-row slots `slots` (a lateral table-valued
    /// call's arguments nothing else reads), kept for cursor `c`'s
    /// `Restore`.
    #[cfg(any(feature = "json", feature = "vtab"))]
    pub(crate) fn clear_dead(&mut self, c: u16, slots: &[usize]) {
        for &slot in slots {
            if let Some(v) = self.acc.vals.as_mut_slice().get_mut(slot) {
                let v = core::mem::replace(v, OwnedValue::Null);
                self.dead.push((c, slot, v));
            }
        }
    }

    /// The driver-awaited instruction completed: on to the next one,
    /// or to its `p2`.
    #[cfg(feature = "sql")]
    pub(crate) fn call_done(&mut self, jump: bool) {
        let to = match self.call_op() {
            Some(op) if jump => op.p2 as usize,
            _ => self.pc.saturating_add(1),
        };
        self.pc = to;
        self.pins.clear();
    }

    /// A driver-awaited instruction failed with the statement error
    /// `kind`: inside a running subquery it becomes that subquery's
    /// value and the program goes on (true); otherwise the statement
    /// fails (false).
    #[cfg(feature = "sql")]
    pub(crate) fn call_trap(&mut self, kind: crate::error::SchemaKind) -> bool {
        if self.traps.is_empty() {
            return false;
        }
        self.trap(kind);
        true
    }
}
