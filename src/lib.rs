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

#[allow(unsafe_code)] // the bounded arena exception (SPEC §6)
pub mod arena;
pub mod btree;
pub mod error;
pub mod pager;
pub mod record;
pub mod schema;
pub mod session;
pub mod sql;
#[allow(unsafe_code)] // no-op waker construction for block_on only
pub mod storage;

pub use error::{Error, SchemaKind, Scope};
pub use record::{OwnedValue, Value};
pub use session::{ArenaStats, Config, Connection, Statement, Step};
pub use storage::{MemStorage, Storage};

#[cfg(feature = "std")]
pub use storage::FileStorage;
