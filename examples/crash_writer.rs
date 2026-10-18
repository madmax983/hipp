//! Crash-injection harness for A6's cross-tool case: a hipp
//! process that dies hard (immediate `process::exit`, no cleanup)
//! at a chosen write boundary of an INSERT commit, leaving its
//! rollback journal behind for *real SQLite* to recover.
//!
//! Modes (env `MODE`):
//!
//! - `setup`: create the db + table + first row, committed.
//! - `insert`: insert the second row under the kill wrapper.
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
            "insert" => {
                let n = Rc::new(Cell::new(0));
                let fs = KillStorage {
                    inner: FileStorage::open(&path).expect("fs"),
                    n: n.clone(),
                    kill,
                };
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                let mut st = db
                    .prepare("INSERT INTO t VALUES (2, 'two')")
                    .await
                    .expect("prep");
                assert_eq!(st.step().await.expect("step"), Step::Done);
                drop(st);
                drop(db);
                if std::env::var("COUNT").is_ok() {
                    eprintln!("OPS={}", n.get());
                }
            }
            other => panic!("unknown MODE {other}"),
        }
    });
}
