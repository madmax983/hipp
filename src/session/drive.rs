//! The bytecode VM's driver (ADR 0005 Stage 2, hidden `vm`
//! feature): `Statement::step` runs a SELECT's program here. It is
//! the only code of the VM that knows the storage type: the program
//! runs synchronously against `PagerCore`, and the driver awaits the
//! one I/O it asks for (`Pager::vm_service`) and runs the same
//! opcode again.

use super::*;
use crate::vm::exec::Vm;
use crate::vm::{Exec, Program, codegen, op};

impl<S: Storage> Statement<'_, S> {
    /// The statement's program, compiled from its plan: a SELECT
    /// the code generator covers, else one `Host` instruction.
    fn vm_program(&self) -> Program {
        #[cfg(feature = "without_rowid")]
        let wr = |root: u32| self.conn.pager.wr.table_of(root).is_some();
        #[cfg(not(feature = "without_rowid"))]
        let wr = |_: u32| false;
        let prog = match &self.plan {
            Plan::Query(qp) => codegen::select::query(qp, &wr).or_else(|| {
                let utf8 = self.conn.pager.text_enc() == crate::record::TextEnc::Utf8;
                codegen::join::query(qp, &wr, utf8)
            }),
            Plan::Scan {
                root,
                rowid_alias,
                ncols,
                out,
                aff,
                defaults,
                access,
            } => {
                let access = match access {
                    Access::FullScan => Some(codegen::select::ScanAccess::Full),
                    Access::Rowid(r) => Some(codegen::select::ScanAccess::Rowid(*r)),
                    Access::Empty => Some(codegen::select::ScanAccess::Empty),
                    #[cfg(all(feature = "indexes", not(feature = "expr_index")))]
                    Access::Index { .. } => None,
                };
                let cols: AVec<Option<usize>> = out
                    .iter()
                    .map(|&i| if i == ROWID_COL { None } else { Some(i) })
                    .collect();
                match access {
                    Some(a) if !wr(*root) => codegen::select::scan(
                        *root,
                        *rowid_alias,
                        *ncols,
                        cols.as_slice(),
                        aff.as_slice(),
                        defaults.as_slice(),
                        a,
                    ),
                    _ => None,
                }
            }
            _ => None,
        };
        prog.unwrap_or_else(|| Program::host(op::HOST_QUERY))
    }

    /// Whether this statement's SELECT runs on the bytecode VM (ADR
    /// 0005 Stage 2, hidden `vm` feature; a test hook): its program,
    /// compiled now if the statement has not stepped yet, holds no
    /// `Host` hand-off. A statement with parameters compiles again
    /// for each execution's plan, so ask after its first step. Asking
    /// changes nothing the statement does: under `Config::vm_strict`
    /// a statement that falls back still fails with `VmFallback`.
    #[doc(hidden)]
    pub fn runs_on_vm(&mut self) -> bool {
        if !matches!(self.plan, Plan::Query(_) | Plan::Scan { .. }) {
            return false;
        }
        if self.vm.is_none() {
            self.vm = Some(Box::new(Vm::new(self.vm_program())));
        }
        self.vm.as_deref().is_some_and(Vm::on_vm)
    }

    /// One step of a SELECT on the VM: `None` hands it to today's
    /// executor (a `Host` program, or a statement of a trigger
    /// program, which keeps that executor's materialising order).
    pub(super) async fn vm_step(&mut self) -> Option<Result<Step, S::Error>> {
        // PRAGMA and EXPLAIN QUERY PLAN serve their rows through
        // `step_query` too.
        if !matches!(self.plan, Plan::Query(_) | Plan::Scan { .. }) || self.conn.cfg.vm_off {
            return None;
        }
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        if self.conn.in_program() {
            return None;
        }
        if self.vm.is_none() {
            self.vm = Some(Box::new(Vm::new(self.vm_program())));
        }
        // Checked on every step, not only when the program is
        // compiled, so that `runs_on_vm` (which may compile it first)
        // never turns strict mode off.
        if self.conn.cfg.vm_strict && !self.vm.as_deref().is_some_and(Vm::on_vm) {
            return Some(Err(Error::VmFallback));
        }
        loop {
            let vm = self.vm.as_deref_mut()?;
            let r = {
                let mut core = self.conn.pager.vm_core();
                vm.run(&mut core, &self.conn.env)
            };
            // Subquery-plan executions, counted as today's executor
            // counts them (`subquery_executions`).
            #[cfg(feature = "sql")]
            {
                self.conn.sub_execs = self.conn.sub_execs.saturating_add(vm.take_execs());
            }
            match r {
                Ok(Exec::Row) => {
                    self.current_rowid = vm.take_row(&mut self.current);
                    self.phase = Phase::Running;
                    return Some(Ok(Step::Row));
                }
                Ok(Exec::Done) => {
                    self.phase = Phase::Done;
                    self.current_rowid = None;
                    return Some(Ok(Step::Done));
                }
                Ok(Exec::Pending(io)) => {
                    if let Err(e) = self.conn.pager.vm_service(io).await {
                        self.vm_fail();
                        return Some(Err(e));
                    }
                }
                Ok(Exec::Host(_)) => return None,
                Err(e) => {
                    self.vm_fail();
                    return Some(Err(e.map_storage(|()| Error::Unsupported)));
                }
            }
        }
    }

    /// After an error the statement starts over when stepped again,
    /// as today's executors do.
    fn vm_fail(&mut self) {
        if let Some(vm) = self.vm.as_deref_mut() {
            vm.reset();
        }
        self.phase = Phase::Fresh;
        self.current_rowid = None;
    }
}
