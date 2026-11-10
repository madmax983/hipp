//! The cursors of unit `vm_rest` (ADR 0005 Stage 2): sources the
//! driver reads, and window functions.
//!
//! - **Host cursors.** A virtual table's cursor (xFilter / xNext /
//!   xColumn / xRowid: generate_series, an R*Tree search, the rows a
//!   module computes at its filter) and a WITHOUT ROWID table's scan
//!   are today's async readers (`session::vtab`, `btree::ScanCursor`
//!   over `crate::wr`). The VM keeps their state here, opaque to it
//!   (the readers are the driver's: `session::drive`), with the row
//!   the last driver-awaited `HostNext` gave, which `Load` appends to
//!   the joined row.
//! - **Window sets.** `window.rs` computes a core's windows over its
//!   whole input (`window::compute`): the set keeps the input rows (a
//!   query without aggregates) and each unit's row or group and its
//!   calls' children's values, then hands out the units in window
//!   order with their values (`EVAL_WIN` fills the result columns'
//!   templates with them, `window::fill`).

use crate::arena::AVec;
#[cfg(feature = "window")]
use crate::query::RowData;
use crate::record::OwnedValue;

/// A source the driver reads (`Cursor::Host`).
#[derive(Default)]
pub(crate) struct HostCur {
    /// The reader's state (the driver's type: a virtual table's
    /// cursor, a table scan), none before `HostOpen`.
    pub(crate) state: Option<alloc::boxed::Box<dyn core::any::Any>>,
    /// The current row's values and rowid.
    pub(crate) row: AVec<OwnedValue>,
    pub(crate) rowid: Option<i64>,
    /// Rows the cursor gave since it opened (`Config::max_vtab_rows`).
    #[cfg_attr(not(feature = "vtab"), allow(dead_code))]
    pub(crate) count: usize,
}

/// One window unit: an input row (`at` indexes `WinSet::rows`) or a
/// group (`at` indexes the group set, `rep` its bare-column row), and
/// each call's children's values.
#[cfg(feature = "window")]
pub(crate) struct WinUnit {
    pub(crate) at: usize,
    pub(crate) rep: Option<RowData>,
    pub(crate) ins: AVec<AVec<OwnedValue>>,
}

/// A query's window units (`Cursor::Win`).
#[cfg(feature = "window")]
#[derive(Default)]
pub(crate) struct WinSet {
    /// The `P4::Win` entry it was opened with.
    pub(crate) info: u32,
    /// The kept input rows (a query without aggregates).
    pub(crate) rows: AVec<RowData>,
    /// The next row `WinLoad` gives.
    pub(crate) pos: usize,
    pub(crate) units: AVec<WinUnit>,
    /// The units in window order, and each one's call values.
    pub(crate) order: AVec<usize>,
    pub(crate) vals: AVec<AVec<OwnedValue>>,
    /// The next unit `WinNext` gives.
    pub(crate) next: usize,
}
