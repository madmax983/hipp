//! Driving hipp from smol.
//!
//! ```text
//! cargo run -p hipp-runtimes --example smol
//! ```
//!
//! - [`UnblockFile`] implements hipp's `Storage` over `std::fs`, running
//!   each blocking call on smol's thread pool with `smol::unblock`, so
//!   file I/O never stalls the executor. That is the usual smol way to
//!   wrap blocking I/O.
//! - A `Connection` is `!Send`, so it lives on a `LocalExecutor`. Tasks
//!   on that executor share it through `smol::lock::Mutex` (an async
//!   mutex, held across a statement's awaits).
//! - Other work keeps running while the database waits on the disk: a
//!   ticker task counts how often it ran during the inserts.

use hipp::storage::{Sidecar, SidecarFile, Storage};
use hipp::{Config, Connection};
use hipp_runtimes::{exec, rows, sibling, sidecar_suffix, temp_db};
use smol::lock::Mutex;
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// One file of a database (main file or sidecar). The `std::fs::File`
/// is shared with the blocking pool, hence `Arc<std::sync::Mutex<..>>`.
/// The mutex is only ever locked inside `unblock` closures, never
/// across an await.
struct UnblockFile {
    path: PathBuf,
    file: Option<Arc<std::sync::Mutex<File>>>,
    existed: bool,
}

type Io<T> = std::io::Result<T>;

fn open_rw(path: &PathBuf) -> Io<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

impl UnblockFile {
    async fn open(path: PathBuf) -> Io<UnblockFile> {
        let p = path.clone();
        let file = smol::unblock(move || open_rw(&p)).await?;
        Ok(UnblockFile {
            path,
            file: Some(Arc::new(std::sync::Mutex::new(file))),
            existed: true,
        })
    }

    /// Run `op` on the open file on smol's blocking pool, creating the
    /// file first if this is a sidecar's first write.
    async fn with_file<T: Send + 'static>(
        &mut self,
        op: impl FnOnce(&mut File) -> Io<T> + Send + 'static,
    ) -> Io<T> {
        if self.file.is_none() {
            let p = self.path.clone();
            let f = smol::unblock(move || open_rw(&p)).await?;
            self.file = Some(Arc::new(std::sync::Mutex::new(f)));
        }
        let file = self.file.clone().expect("just opened");
        smol::unblock(move || op(&mut file.lock().expect("file lock"))).await
    }

    async fn len_(&self) -> Io<u64> {
        let p = self.path.clone();
        smol::unblock(move || match std::fs::metadata(&p) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        })
        .await
    }

    async fn read_at_(&self, offset: u64, buf: &mut [u8]) -> Io<()> {
        let Some(file) = self.file.clone() else {
            return if buf.is_empty() {
                Ok(())
            } else {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            };
        };
        // The closure must own what it touches, so it reads into a
        // buffer of its own, copied into hipp's afterwards.
        let n = buf.len();
        let data = smol::unblock(move || {
            let mut f = file.lock().expect("file lock");
            let mut v = vec![0; n];
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut v)?;
            Ok::<_, std::io::Error>(v)
        })
        .await?;
        buf.copy_from_slice(&data);
        Ok(())
    }

    async fn write_at_(&mut self, offset: u64, buf: &[u8]) -> Io<()> {
        let data = buf.to_vec();
        self.with_file(move |f| {
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(&data)
        })
        .await
    }

    async fn set_len_(&mut self, len: u64) -> Io<()> {
        self.with_file(move |f| f.set_len(len)).await
    }

    async fn sync_(&mut self) -> Io<()> {
        match self.file.clone() {
            Some(file) => smol::unblock(move || file.lock().expect("file lock").sync_all()).await,
            None => Ok(()),
        }
    }
}

impl Storage for UnblockFile {
    type Error = std::io::Error;
    type Sidecar = UnblockFile;

    async fn len(&self) -> Io<u64> {
        self.len_().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Io<()> {
        self.read_at_(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Io<()> {
        self.write_at_(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Io<()> {
        self.set_len_(len).await
    }
    async fn sync(&mut self) -> Io<()> {
        self.sync_().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Io<UnblockFile> {
        let path = sibling(&self.path, sidecar_suffix(kind));
        let p = path.clone();
        // An existing sidecar (a hot journal) opens now, to be read; a
        // new one is created on its first write.
        let file = smol::unblock(move || {
            if p.exists() {
                open_rw(&p).map(Some)
            } else {
                Ok(None)
            }
        })
        .await?;
        Ok(UnblockFile {
            path,
            existed: file.is_some(),
            file: file.map(|f| Arc::new(std::sync::Mutex::new(f))),
        })
    }
}

impl SidecarFile for UnblockFile {
    type Error = std::io::Error;

    fn existed(&self) -> bool {
        self.existed
    }
    async fn len(&self) -> Io<u64> {
        self.len_().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Io<()> {
        self.read_at_(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Io<()> {
        self.write_at_(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Io<()> {
        self.set_len_(len).await
    }
    async fn sync(&mut self) -> Io<()> {
        self.sync_().await
    }
    async fn delete(&mut self) -> Io<()> {
        self.file = None;
        let p = self.path.clone();
        smol::unblock(move || match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        })
        .await
    }
}

fn main() {
    let path = temp_db("smol");
    let ex = smol::LocalExecutor::new();
    // hipp's futures nest deeply. Boxing the program's future keeps the
    // executor's future type small (and within rustc's layout depth).
    smol::block_on(ex.run(Box::pin(demo(&ex, path.clone()))));

    assert!(!sibling(&path, "-journal").exists());
    // Reopen with hipp's FileStorage: an ordinary SQLite file.
    hipp::storage::block_on(async {
        let storage = hipp::FileStorage::open(&path).expect("open");
        let mut db = Connection::open(storage, Config::default())
            .await
            .expect("reopen");
        assert_eq!(rows(&mut db, "SELECT count(*) FROM log").await, ["60"]);
    });
    hipp_runtimes::remove_db(&path);
    println!("\nsmol: ok");
}

async fn demo(ex: &smol::LocalExecutor<'_>, path: PathBuf) {
    {
        let storage = UnblockFile::open(path).await.expect("open file");
        let mut db = Connection::open(storage, Config::default())
            .await
            .expect("open database");
        exec(
            &mut db,
            "CREATE TABLE log (id INTEGER PRIMARY KEY, worker INTEGER, msg TEXT)",
        )
        .await;
        let db = Rc::new(Mutex::new(db));

        // A ticker that runs whenever the executor is free.
        let ticks = Rc::new(Cell::new(0u32));
        let done = Rc::new(Cell::new(false));
        let ticker = {
            let (ticks, done) = (ticks.clone(), done.clone());
            ex.spawn(async move {
                while !done.get() {
                    ticks.set(ticks.get() + 1);
                    smol::Timer::after(std::time::Duration::from_micros(200)).await;
                }
            })
        };

        // Three writer tasks share the connection, each committing its
        // rows in one transaction.
        let mut writers = Vec::new();
        for worker in 0..3 {
            let db = db.clone();
            writers.push(ex.spawn(async move {
                let mut db = db.lock().await; // held for the whole transaction
                exec(&mut *db, "BEGIN").await;
                for i in 0..20 {
                    let sql =
                        format!("INSERT INTO log (worker, msg) VALUES ({worker}, 'event {i}')");
                    exec(&mut *db, &sql).await;
                }
                exec(&mut *db, "COMMIT").await;
            }));
        }
        for w in writers {
            w.await;
        }
        done.set(true);
        ticker.await;

        let mut db = db.lock().await;
        let per_worker = rows(&mut *db, "SELECT worker, count(*) FROM log GROUP BY worker").await;
        println!("rows per worker: {per_worker:?}");
        assert_eq!(per_worker, ["0|20", "1|20", "2|20"]);
        println!(
            "the ticker ran {} times while the commits were on the blocking pool",
            ticks.get()
        );
        assert!(ticks.get() > 0);
        assert_eq!(rows(&mut *db, "PRAGMA integrity_check").await, ["ok"]);
    }
}
