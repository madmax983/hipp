//! hipp — a little `no_std` database that reads and writes SQLite
//! database files (SPEC rev. 0.3, v0.1 profile; pinned claim
//! version: SQLite 3.54.1 file format).
//!
//! Two processes must never open the same file for writing in v0.1:
//! multi-process locking is specified but not implemented (SPEC
//! §5.3, RISKS R4). One connection, one writer.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "v04")]
pub mod agg;
#[allow(unsafe_code)] // the bounded arena exception (SPEC §6)
pub mod arena;
#[cfg(feature = "autovacuum")]
pub mod autovac;
pub mod btree;
#[cfg(feature = "datetime")]
pub mod datetime;
pub mod error;
#[cfg(feature = "fts5")]
pub mod fts5;
#[cfg(feature = "v16")]
pub mod func;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "math")]
pub mod math;
pub mod pager;
#[cfg(feature = "v04")]
pub mod query;
pub mod record;
#[cfg(feature = "rtree")]
pub mod rtree;
#[cfg(feature = "v04")]
pub mod rules;
pub mod schema;
pub mod session;
pub mod sql;
#[allow(unsafe_code)] // no-op waker construction for block_on only
pub mod storage;
#[cfg(feature = "std")]
pub mod systime;
#[cfg(feature = "utf16")]
pub mod utf16;
#[cfg(feature = "vtab")]
pub mod vtab;
#[cfg(feature = "wal")]
pub mod wal;
#[cfg(feature = "window")]
pub mod window;
#[cfg(feature = "without_rowid")]
pub mod wr;
#[cfg(feature = "join_order")]
pub mod xjoin;
#[cfg(feature = "expr_index")]
pub mod xplan;

pub use error::{Error, SchemaKind, Scope};
pub use record::{OwnedValue, Value};
pub use session::{
    ArenaStats, Config, Connection, IndexMeta, Statement, Step, TableMeta, ViewMeta,
};
#[cfg(feature = "attach")]
pub use storage::AttachTarget;
pub use storage::{MemStorage, Storage};

#[cfg(feature = "std")]
pub use storage::FileStorage;
