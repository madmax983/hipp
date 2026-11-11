//! The bytecode VM's driver (ADR 0005 Stage 2, hidden `vm`
//! feature): `Statement::step` runs a SELECT's program here. It is
//! the only code of the VM that knows the storage type: the program
//! runs synchronously against `PagerCore`, and the driver awaits the
//! one I/O it asks for (`Pager::vm_service`) and runs the same
//! opcode again.

use super::*;
use crate::vm::exec::Vm;
use crate::vm::{Exec, Program, codegen, op};

#[cfg(any(feature = "json", feature = "fts5", feature = "vtab"))]
impl<S: Storage> Connection<S> {
    /// Whether a trigger or foreign-key action program is running.
    fn in_program_any(&self) -> bool {
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        {
            self.in_program()
        }
        #[cfg(not(any(feature = "triggers", feature = "foreign_keys")))]
        {
            false
        }
    }
}

impl<S: Storage> Statement<'_, S> {
    /// The statement's program, compiled from its plan: a SELECT
    /// the code generator covers, else one `Host` instruction.
    pub(super) fn vm_program(&self) -> Program {
        #[cfg(feature = "without_rowid")]
        let wr = |root: u32| self.conn.pager.wr.table_of(root).is_some();
        #[cfg(not(feature = "without_rowid"))]
        let wr = |_: u32| false;
        let prog = match &self.plan {
            Plan::Query(qp) => {
                let utf8 = self.conn.pager.text_enc() == crate::record::TextEnc::Utf8;
                codegen::join::query(qp, &wr, utf8)
            }
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

    /// One step of a SELECT on the VM (a statement of a trigger or
    /// foreign-key action program too): `None` hands it to today's
    /// executor (a `Host` program).
    pub(super) async fn vm_step(&mut self) -> Option<Result<Step, S::Error>> {
        // PRAGMA and EXPLAIN QUERY PLAN serve their rows through
        // `step_query` too.
        if !matches!(self.plan, Plan::Query(_) | Plan::Scan { .. }) || self.conn.cfg.vm_off {
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
        // The side tables' entries the program's rows may point into
        // stay until it ends (`Stream::marks`): a statement left
        // part-way gives them back at the next statement's first step
        // (`reclaim_stream`).
        #[cfg(any(feature = "json", feature = "fts5"))]
        if self.vm.as_deref().is_some_and(Vm::on_vm) {
            let vm = self.vm.as_deref_mut()?;
            let m = vm.side_begin(&self.conn.env);
            if self.phase == Phase::Fresh && !self.conn.in_program_any() {
                self.conn.stream_left = Some(m);
            }
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
                    self.vm_side_end();
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
                #[cfg(feature = "sql")]
                // (Boxed: the readers' futures stay out of the step
                // future.)
                Ok(Exec::Call) => match Box::pin(self.vm_call()).await {
                    Ok(()) => {}
                    Err(Error::Schema { kind })
                        if self.vm.as_deref_mut().is_some_and(|v| v.call_trap(kind)) => {}
                    Err(e) => {
                        self.vm_fail();
                        return Some(Err(e));
                    }
                },
                Err(e) => {
                    self.vm_fail();
                    return Some(Err(e.map_storage(|()| Error::Unsupported)));
                }
            }
        }
    }

    /// The program ended: the side-table entries its rows held go.
    fn vm_side_end(&mut self) {
        #[cfg(any(feature = "json", feature = "fts5"))]
        if let Some(vm) = self.vm.as_deref_mut() {
            let m = vm.side_begin(&self.conn.env);
            crate::vm::exec::side_drop(&self.conn.env, m);
            if !self.conn.in_program_any() {
                self.conn.stream_left = None;
            }
        }
    }

    /// Whether a query is running on the VM with rows still to come
    /// (`Statement::step` then keeps the side tables' entries).
    #[cfg(any(feature = "json", feature = "fts5"))]
    pub(super) fn vm_running(&self) -> bool {
        self.phase == Phase::Running && self.vm.as_deref().is_some_and(Vm::on_vm)
    }

    /// After an error the statement starts over when stepped again,
    /// as today's executors do.
    fn vm_fail(&mut self) {
        self.vm_side_end();
        if let Some(vm) = self.vm.as_deref_mut() {
            vm.reset();
        }
        self.phase = Phase::Fresh;
        self.current_rowid = None;
    }
}

#[cfg(feature = "sql")]
impl<S: Storage> Statement<'_, S> {
    /// One driver-awaited instruction (`Exec::Call`): today's async
    /// source reader it names, run over the joined row so far, its
    /// result stored in the VM, which then goes on. Its inputs are
    /// copied out of the VM first and its result stored after the
    /// reader completed, so a step future dropped while the reader
    /// waits leaves the VM as it was: the next step runs the
    /// instruction again (a WITHOUT ROWID scan and an R*Tree search
    /// move a copy of their cursor, `vm_host_next`).
    async fn vm_call(&mut self) -> Result<(), S::Error> {
        use crate::vm::op::{self as vop, Code};
        let (op, qp, d, base, src, drain) = {
            let vm = self.vm.as_deref().ok_or(Error::Unsupported)?;
            let op = vm.call_op().ok_or(Error::Unsupported)?;
            let info = vm.src_info(op.p4).ok_or(Error::Unsupported)?;
            let qp = vm.plan_at(info.plan).ok_or(Error::Unsupported)?;
            (op, qp, info.d, info.base, info.src, info.drain)
        };
        let _ = drain;
        let sp = qp.sources.as_slice().get(d).ok_or(Error::Unsupported)?;
        if op.code == Code::HostNext {
            return Box::pin(self.vm_host_next(op, sp)).await;
        }
        let (before, need) = {
            let vm = self.vm.as_deref().ok_or(Error::Unsupported)?;
            (vm.acc_before(base, src), vm.bound(op.p3))
        };
        let _ = need;
        let mut padded = before.clone();
        pad_sources(&mut padded, qp.sources.as_slice().get(d..).unwrap_or(&[]));
        #[cfg(any(feature = "json", feature = "vtab"))]
        let lateral = sp.tvf & crate::query::TVF_LATERAL != 0;
        #[cfg(not(any(feature = "json", feature = "vtab")))]
        let lateral = false;
        // A lateral table-valued source's pre-check (`lateral_ok`): no
        // rows, and no LEFT JOIN pad.
        #[cfg(any(feature = "json", feature = "vtab"))]
        if lateral
            && !self
                .lateral_ok(qp.sources.as_slice(), d, qp.filter.as_ref(), &before)
                .await?
        {
            if let Some(vm) = self.vm.as_deref_mut() {
                vm.call_done(true);
            }
            return Ok(());
        }
        if op.code == Code::HostRows {
            // One await for every row-list reader (unit `vm_shrink`):
            // the reader's future is boxed and awaited through one
            // state of this future.
            type Rows<E> = Result<AVec<(AVec<OwnedValue>, Option<i64>)>, E>;
            let fut: core::pin::Pin<Box<dyn core::future::Future<Output = Rows<S::Error>> + '_>> =
                match op.p5 {
                    #[cfg(any(feature = "json", feature = "vtab"))]
                    vop::HOST_TVF => Box::pin(self.json_tvf_rows(sp, &padded, need)),
                    #[cfg(feature = "expr_index")]
                    vop::HOST_ACCESS => {
                        let a = sp.access.as_deref().ok_or(Error::Unsupported)?;
                        Box::pin(self.access_rows(sp, a, &padded))
                    }
                    #[cfg(feature = "planner")]
                    vop::HOST_AUTO => {
                        let ai = sp
                            .access
                            .as_deref()
                            .and_then(|a| a.auto.as_deref())
                            .ok_or(Error::Unsupported)?;
                        Box::pin(self.auto_rows(sp, ai))
                    }
                    vop::HOST_SCAN if sp.temp => Box::pin(self.scan_temp(sp)),
                    vop::HOST_SCAN => Box::pin(self.scan_source(sp)),
                    _ => return Err(Error::Unsupported),
                };
            let rows = fut.await?;
            let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
            if !vm.fill(op.p1, rows) {
                return Err(Error::Unsupported);
            }
        } else {
            let state: Box<dyn core::any::Any> = match op.p5 {
                #[cfg(feature = "vtab")]
                vop::HOST_VT => {
                    let vt = sp.vt.as_deref().ok_or(Error::Unsupported)?;
                    let mut cur = self.vtab_open(sp, vt, &padded, need).await?;
                    // A core today's executor does not stream reads a
                    // generate_series cursor whole, under the reader's
                    // bound and `Config::max_vtab_rows` (`vtab_rows_of`;
                    // review fix: rows kept by a materialised CTE, a
                    // compound, an IN list or a derived table were not
                    // bounded).
                    if (drain || self.conn.in_program_any())
                        && let super::vtab::VtCur::Series(c) = &mut cur
                    {
                        let rows =
                            crate::vtab::drain(c, sp.ncols, need, self.conn.cfg.max_vtab_rows)
                                .map_err(|kind| Error::Schema { kind })?;
                        cur = super::vtab::VtCur::Rows(rows, 0);
                    }
                    Box::new(cur)
                }
                vop::HOST_SCAN => Box::new(ScanCursor::new(sp.root)),
                _ => return Err(Error::Unsupported),
            };
            let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
            vm.host(op.p1, true).ok_or(Error::Unsupported)?.state = Some(state);
        }
        let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
        // The values only the call read (`SourcePlan::tvf_dead`):
        // cleared for the rows it makes, put back when its loop ends.
        #[cfg(any(feature = "json", feature = "vtab"))]
        if lateral {
            let mut slots = AVec::new();
            for back in sp.tvf_dead.iter() {
                if let Some(slot) = base.checked_sub(*back) {
                    slots.push(slot);
                }
            }
            vm.clear_dead(op.p1, slots.as_slice());
        }
        let _ = lateral;
        vm.call_done(false);
        Ok(())
    }

    /// `HostNext`: the host cursor's next row (`vtab_next`, or the
    /// scan's `next_row_into` and `decode_source_row`).
    async fn vm_host_next(
        &mut self,
        op: crate::vm::op::Op,
        sp: &SourcePlan,
    ) -> Result<(), S::Error> {
        let max = {
            #[cfg(feature = "vtab")]
            {
                self.conn.cfg.max_vtab_rows
            }
            #[cfg(not(feature = "vtab"))]
            {
                usize::MAX
            }
        };
        let capped = {
            let vm = self.vm.as_deref().ok_or(Error::Unsupported)?;
            match op.p3 {
                0 => false,
                1 => true,
                r => vm.unlimited(r.saturating_sub(2)),
            }
        };
        let mut row = AVec::new();
        // An R*Tree search moves a copy of its cursor across its page
        // reads.
        #[cfg(feature = "rtree")]
        {
            let copy = {
                let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
                let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
                match h
                    .state
                    .as_mut()
                    .and_then(|s| s.downcast_mut::<super::vtab::VtCur>())
                {
                    Some(super::vtab::VtCur::Rtree(c)) => Some((**c).clone()),
                    _ => None,
                }
            };
            if let Some(mut c) = copy {
                let r = self.rtree_next(&mut c).await?;
                let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
                let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
                if let Some(super::vtab::VtCur::Rtree(slot)) = h
                    .state
                    .as_mut()
                    .and_then(|s| s.downcast_mut::<super::vtab::VtCur>())
                {
                    **slot = c;
                }
                h.row.clear();
                h.rowid = None;
                if let Some((vals, rowid)) = r {
                    h.row = vals;
                    h.rowid = rowid;
                    vm.call_done(op.p5 != 1);
                } else {
                    vm.call_done(op.p5 == 1);
                }
                return Ok(());
            }
        }
        // A WITHOUT ROWID table's scan: its cursor moved as a copy too
        // (its clustered-tree cursor changes state between page reads).
        let scan = {
            let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
            let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
            h.state
                .as_ref()
                .and_then(|s| s.downcast_ref::<ScanCursor>())
                .cloned()
        };
        let got = match scan {
            Some(mut c) => {
                let mut buf = AVec::new();
                let r = c.next_row_into(&mut self.conn.pager, &mut buf).await?;
                if let Some(rowid) = r {
                    decode_source_row::<S::Error>(sp, buf.as_slice(), rowid, &mut row)?;
                }
                let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
                let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
                h.state = Some(Box::new(c));
                r.map(Some)
            }
            None => {
                let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
                let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
                let state = h.state.as_mut().ok_or(Error::Unsupported)?;
                #[cfg(feature = "vtab")]
                {
                    let vt = state
                        .downcast_mut::<super::vtab::VtCur>()
                        .ok_or(Error::Unsupported)?;
                    if capped && matches!(vt, super::vtab::VtCur::Series(_)) {
                        h.count = h.count.saturating_add(1);
                        if h.count > max {
                            return Err(Error::Schema {
                                kind: SchemaKind::VtabTooManyRows,
                            });
                        }
                    }
                    vt_next_sync::<S::Error>(vt, sp.ncols, &mut row)?
                }
                #[cfg(not(feature = "vtab"))]
                {
                    let _ = (state, capped, max);
                    return Err(Error::Unsupported);
                }
            }
        };
        let vm = self.vm.as_deref_mut().ok_or(Error::Unsupported)?;
        let h = vm.host(op.p1, false).ok_or(Error::Unsupported)?;
        match got {
            Some(rowid) => {
                h.row = row;
                h.rowid = rowid;
                vm.call_done(op.p5 != 1);
            }
            None => {
                h.row.clear();
                h.rowid = None;
                vm.call_done(op.p5 == 1);
            }
        }
        Ok(())
    }
}

/// A generate_series cursor's or computed rows' next row (`vtab_next`
/// for the cursors that read no page): its rowid, or `None` at its
/// end.
#[cfg(feature = "vtab")]
fn vt_next_sync<E>(
    c: &mut super::vtab::VtCur,
    ncols: usize,
    out: &mut AVec<OwnedValue>,
) -> Result<Option<Option<i64>>, E> {
    use crate::vtab::VtCursor;
    Ok(match c {
        super::vtab::VtCur::Series(c) => {
            if c.eof() {
                return Ok(None);
            }
            for i in 0..ncols {
                out.push(c.column(i));
            }
            let r = Some(c.rowid());
            c.next();
            Some(r)
        }
        super::vtab::VtCur::Rows(rows, pos) => {
            let Some((vals, rowid)) = rows.as_slice().get(*pos) else {
                return Ok(None);
            };
            *pos += 1;
            out.extend_from_slice(vals.as_slice());
            Some(*rowid)
        }
        #[cfg(feature = "rtree")]
        super::vtab::VtCur::Rtree(_) => return Err(Error::Unsupported),
    })
}

/// Plain `EXPLAIN` (unit `vm_rest`; `explain` with the hidden `vm`).
#[cfg(feature = "explain")]
impl<S: Storage> Connection<S> {
    /// Prepare `EXPLAIN <statement>`: the statement from byte `at` of
    /// `sql` is prepared (never run) and its program listed in the
    /// pin's columns (`vm::op::listing`): a SELECT's compiled
    /// program, or for any other statement (and a SELECT the code
    /// generator does not cover) the one `Host` instruction that hands
    /// it to today's executor. Errors are the statement's, at their
    /// offsets in `sql`; a second EXPLAIN is the pin's syntax error.
    pub(super) async fn prepare_bytecode(
        &mut self,
        sql: &str,
        at: usize,
    ) -> Result<Statement<'_, S>, S::Error> {
        let rest = sql.get(at..).unwrap_or("");
        match eqp::query_plan_prefix::<S::Error>(rest.as_bytes()) {
            Ok(None) => {}
            Ok(Some(_)) | Err(Error::Parse { .. }) => {
                let skip = rest.len().saturating_sub(rest.trim_start().len());
                return Err(Error::Parse {
                    offset: at.saturating_add(skip),
                    kind: crate::error::ParseKind::UnexpectedToken,
                });
            }
            Err(e) => return Err(e),
        }
        let prep: Prep<'_, '_, S> = Box::pin(self.prepare(rest));
        let mut st = prep.await.map_err(|e| match e {
            Error::Parse { offset, kind } => Error::Parse {
                offset: offset.saturating_add(at),
                kind,
            },
            e => e,
        })?;
        let prog = match &st.plan {
            Plan::Query(_) | Plan::Scan { .. } => st.vm_program(),
            _ => Program::host(op::HOST_STMT),
        };
        st.plan = Plan::Bytecode(crate::vm::op::listing(&prog));
        #[cfg(feature = "params")]
        {
            st.binds.stale = false;
        }
        Ok(st)
    }
}

#[cfg(feature = "explain")]
impl<S: Storage> Statement<'_, S> {
    /// A plain EXPLAIN's rows, served as a query's.
    pub(super) async fn step_bytecode(&mut self) -> Result<Step, S::Error> {
        if !self.query_loaded {
            let Plan::Bytecode(rows) = &self.plan else {
                return Err(Error::Unsupported);
            };
            self.query_rows = rows.clone();
            self.query_rowids.clear();
            for _ in 0..self.query_rows.len() {
                self.query_rowids.push(None);
            }
            self.query_loaded = true;
            self.query_pos = 0;
        }
        self.step_query().await
    }
}
