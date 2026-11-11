//! SELECT code generation for the legacy scan plan (`Plan::Scan`,
//! the planner's single-table fast path), unit `vm_core` (ADR 0005
//! Stage 2): a full scan or one rowid lookup, its result columns read
//! straight from the row.
//!
//! A general query plan (`Plan::Query`) is compiled by `join::query`
//! alone. `vm_core` had a second generator here for a query plan over
//! one rowid table or none; `join::query` covers every shape it did
//! (the same loops, WHERE, ORDER BY, LIMIT / OFFSET and rowid access,
//! built for today's executor's rules), so unit `vm_shrink` removed
//! it: one copy of those rules, and 2.6-3.8 KB less in the flip probe.

use super::super::Program;
use super::super::build::Builder;
use super::super::cursor::TableInfo;
use super::super::op::{Code, P4};
use super::expr::cursor_ref;
use crate::arena::AVec;
use crate::record::{Affinity, OwnedValue};

/// A legacy scan plan's access path (`session::Access`).
pub(crate) enum ScanAccess {
    Full,
    Rowid(i64),
    Empty,
}

/// The program of a legacy scan plan (`Plan::Scan`): `out` lists the
/// result columns (`None`: the rowid), `aff` their affinities.
pub(crate) fn scan(
    root: u32,
    rowid_alias: Option<usize>,
    ncols: usize,
    out: &[Option<usize>],
    aff: &[Affinity],
    defaults: &[OwnedValue],
    access: ScanAccess,
) -> Option<Program> {
    let mut b = Builder::new();
    let end = b.label();
    if let ScanAccess::Empty = access {
        b.here(end);
        b.emit2(Code::Halt, 0, 0);
        return b.finish();
    }
    // The decode forces REAL only for the result columns that are
    // REAL (the legacy projection's rule).
    let mut affs = AVec::new();
    for _ in 0..ncols {
        affs.push(Affinity::Blob);
    }
    for (o, a) in out.iter().zip(aff.iter()) {
        if let Some(slot) = o.and_then(|i| affs.as_mut_slice().get_mut(i)) {
            *slot = *a;
        }
    }
    let mut defs = AVec::new();
    defs.extend_from_slice(defaults);
    let c = b.cursor();
    let info = b.p4(P4::Table(alloc::boxed::Box::new(TableInfo {
        root,
        ncols,
        rowid_alias,
        affs,
        defaults: defs,
        gcols: None,
    })));
    b.emit_p4(Code::OpenRead, c, info);
    let n = out.len();
    let regs = b.regs(n);
    let top = b.label();
    let one = match access {
        ScanAccess::Rowid(r) => {
            let k = b.reg();
            let v = b.p4(P4::Value(OwnedValue::Integer(r)));
            b.hoist(Code::Value, k, v);
            b.jump3(Code::SeekRowid, c, end, k);
            true
        }
        _ => {
            b.jump_to(Code::Rewind, c, end);
            false
        }
    };
    b.here(top);
    for (i, o) in out.iter().enumerate() {
        let r = regs.saturating_add(i as u32);
        match o {
            Some(col) => b.emit3(Code::Column, c, *col as u32, r),
            None => b.emit2(Code::Rowid, c, r),
        }
    }
    b.emit(Code::ResultRow, regs, n as u32, cursor_ref(Some(c)), 0, 0);
    if !one {
        b.jump_to(Code::Next, c, top);
    }
    b.here(end);
    b.emit2(Code::Halt, 0, 0);
    b.finish()
}
