//! Crash-injection harness for A6's cross-tool case: a hipp
//! process that dies hard (immediate `process::exit`, no cleanup)
//! at a chosen write boundary of an INSERT commit, leaving its
//! rollback journal behind for *real SQLite* to recover.
//!
//! Modes (env `MODE`):
//!
//! - `setup`: create the db + table + first row, committed.
//! - `insert`: insert the second row under the kill wrapper.
//! - `savepoint` (v0.16): a SAVEPOINT transaction under the kill
//!   wrapper whose ROLLBACK TO reads page images back from the
//!   journal file's savepoint area; it commits rows 2 and 4.
//!
//! - `wal_setup` (v0.16, `wal`): the db + table + first row in WAL
//!   mode, the connection dropped so its log stays.
//! - `wal_txn` (v0.16, `wal`): a WAL-mode workload under the kill
//!   wrapper — an insert, a transaction, a TRUNCATE checkpoint, an
//!   insert into the restarted log, then `Connection::close`
//!   (checkpoint, log removed); it commits rows 2, 3 and 4.
//!
//! Env `KILL_AT` = mutating-op index at which to die (default:
//! never). Env `COUNT=1` prints the total mutating-op count to
//! stderr after a successful insert.

use hipp::storage::{FileError, FileSidecar, FileStorage, Sidecar, SidecarFile, Storage};
use hipp::{Config, Connection, Step};
use std::cell::Cell;
use std::rc::Rc;

fn gate(n: &Rc<Cell<usize>>, kill: usize) {
    if n.get() == kill {
        std::process::exit(42);
    }
    n.set(n.get() + 1);
}

struct KillStorage {
    inner: FileStorage,
    n: Rc<Cell<usize>>,
    kill: usize,
}

struct KillSidecar {
    inner: FileSidecar,
    n: Rc<Cell<usize>>,
    kill: usize,
}

impl Storage for KillStorage {
    type Error = FileError;
    type Sidecar = KillSidecar;

    async fn len(&self) -> Result<u64, FileError> {
        self.inner.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
        self.inner.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.sync().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<KillSidecar, FileError> {
        Ok(KillSidecar {
            inner: self.inner.sidecar(kind).await?,
            n: self.n.clone(),
            kill: self.kill,
        })
    }
}

impl SidecarFile for KillSidecar {
    type Error = FileError;
    async fn len(&self) -> Result<u64, FileError> {
        self.inner.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
        self.inner.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.sync().await
    }
    fn existed(&self) -> bool {
        self.inner.existed()
    }
    async fn delete(&mut self) -> Result<(), FileError> {
        gate(&self.n, self.kill);
        self.inner.delete().await
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("db path arg");
    let mode = std::env::var("MODE").unwrap_or_else(|_| "setup".into());
    let kill: usize = std::env::var("KILL_AT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    hipp::storage::block_on(async {
        match mode.as_str() {
            "setup" => {
                let fs = FileStorage::open(&path).expect("fs");
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                for sql in [
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
                    "INSERT INTO t VALUES (1, 'one')",
                ] {
                    let mut st = db.prepare(sql).await.expect("prep");
                    assert_eq!(st.step().await.expect("step"), Step::Done);
                }
            }
            #[cfg(feature = "wal")]
            "wal_setup" => {
                let fs = FileStorage::open(&path).expect("fs");
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                for sql in [
                    "PRAGMA journal_mode = WAL",
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
                    "INSERT INTO t VALUES (1, 'one')",
                ] {
                    let mut st = db.prepare(sql).await.expect("prep");
                    while st.step().await.expect("step") == Step::Row {}
                }
            }
            #[cfg(feature = "wal")]
            "wal_txn" => {
                let n = Rc::new(Cell::new(0));
                let fs = KillStorage {
                    inner: FileStorage::open(&path).expect("fs"),
                    n: n.clone(),
                    kill,
                };
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                for sql in [
                    "PRAGMA wal_autocheckpoint = 0",
                    "INSERT INTO t VALUES (2, 'two')",
                    "BEGIN",
                    "INSERT INTO t VALUES (3, 'three')",
                    "UPDATE t SET x = x || '!'",
                    "COMMIT",
                    "PRAGMA wal_checkpoint(TRUNCATE)",
                    "INSERT INTO t VALUES (4, 'four')",
                ] {
                    let mut st = db.prepare(sql).await.expect("prep");
                    while st.step().await.expect("step") == Step::Row {}
                }
                db.close().await.expect("close");
                if std::env::var("COUNT").is_ok() {
                    eprintln!("OPS={}", n.get());
                }
            }
            "insert" | "savepoint" => {
                let n = Rc::new(Cell::new(0));
                let fs = KillStorage {
                    inner: FileStorage::open(&path).expect("fs"),
                    n: n.clone(),
                    kill,
                };
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                let script: &[&str] = if mode == "insert" {
                    &["INSERT INTO t VALUES (2, 'two')"]
                } else {
                    &[
                        "SAVEPOINT a",
                        "INSERT INTO t VALUES (2, 'two')",
                        "SAVEPOINT b",
                        "UPDATE t SET x = x || '!'",
                        "INSERT INTO t VALUES (3, 'three')",
                        "ROLLBACK TO b",
                        "INSERT INTO t VALUES (4, 'four')",
                        "RELEASE a",
                    ]
                };
                for sql in script {
                    let mut st = db.prepare(sql).await.expect("prep");
                    assert_eq!(st.step().await.expect("step"), Step::Done);
                }
                drop(db);
                if std::env::var("COUNT").is_ok() {
                    eprintln!("OPS={}", n.get());
                }
            }
            other => panic!("unknown MODE {other}"),
        }
    });
}
