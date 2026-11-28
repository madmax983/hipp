//! hipp: a small `no_std` database engine that reads and writes SQLite
//! database files (pinned claim version: the SQLite 3.54.1 file format and
//! SQL behaviour).
//!
//! ```
//! use hipp::storage::block_on;
//! use hipp::{Config, Connection, MemStorage, Step};
//!
//! block_on(async {
//!     let mut db = Connection::open(MemStorage::new(), Config::default()).await?;
//!     for sql in [
//!         "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
//!         "INSERT INTO t (name) VALUES ('ada'), ('grace')",
//!     ] {
//!         let mut st = db.prepare(sql).await?;
//!         while st.step().await? == Step::Row {}
//!     }
//!     let mut q = db.prepare("SELECT id, name FROM t WHERE rowid = 2").await?;
//!     assert_eq!(q.step().await?, Step::Row);
//!     assert_eq!(q.column_i64(0), Some(2));
//!     assert_eq!(q.column_bytes(1), Some(&b"grace"[..]));
//!     assert_eq!(q.step().await?, Step::Done);
//!     Ok::<(), hipp::Error<hipp::storage::MemError>>(())
//! })
//! .expect("query");
//! ```
//!
//! - [`Connection`] opens a database on any [`Storage`] ([`MemStorage`],
//!   `FileStorage` with feature `std`, or your own) and prepares one
//!   statement at a time. [`Statement::step`] yields [`Step::Row`] until
//!   [`Step::Done`].
//! - [`Config`] sets the page cache and the statement / transaction /
//!   catalog arena limits. Every failure is a named [`Error`].
//! - The build profile (cargo features `full`, `max`, `xl`; none for
//!   `tiny`) decides which SQL is available. A smaller build refuses SQL and
//!   files it cannot handle, and never misreads them.
//!
//! The usage guide is `docs/GUIDE.md` and the per-profile reference is
//! `docs/profiles/` in the repository. hipp is one-process, one-connection
//! per database: there is no multi-process locking (SPEC §5.3, RISKS R4).

#![no_std]
#![deny(unsafe_code)]
// The flip probe (`ci/flip_probe.sh`, build-only) cuts today's read
// executor off, leaving its helpers without callers; the Stage 3
// probe (`--cfg hipp_s3_probe`, `ci/flip_probe.sh --s3`) also cuts the
// write executors, the trigger and foreign-key runners and the VM's
// async source readers.
#![cfg_attr(
    all(feature = "vm", any(hipp_flip_probe, hipp_s3_probe)),
    allow(dead_code, unused)
)]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "query")]
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
#[cfg(feature = "sql")]
pub mod func;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "math")]
pub mod math;
pub mod pager;
#[cfg(feature = "query")]
pub mod query;
pub mod record;
#[cfg(feature = "rtree")]
pub mod rtree;
#[cfg(feature = "query")]
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
#[cfg(feature = "vm")]
pub(crate) mod vm;
#[cfg(feature = "vtab")]
pub mod vtab;
#[cfg(feature = "wal")]
pub mod wal;
#[cfg(feature = "window")]
pub mod window;
#[cfg(feature = "without_rowid")]
pub mod wr;
#[cfg(feature = "planner")]
pub mod xjoin;
#[cfg(feature = "planner")]
pub mod xmulti;
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
