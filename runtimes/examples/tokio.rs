//! Driving hipp from tokio.
//!
//! ```text
//! cargo run -p hipp-runtimes --example tokio
//! ```
//!
//! Three things to know:
//!
//! 1. **Storage can be truly async.** [`TokioFile`] below implements
//!    hipp's `Storage` over `tokio::fs`, so the database's file I/O
//!    runs on tokio's blocking pool and never stalls the executor.
//!    (hipp's own `FileStorage` does blocking `std::fs` calls, which is
//!    fine under `block_on` but blocks a tokio worker while it waits.)
//! 2. **A `Connection` is `!Send`.** It cannot move between worker
//!    threads, so `tokio::spawn` cannot take it. On one thread, use a
//!    current-thread runtime or a `LocalSet`, and share the connection
//!    between local tasks through an async mutex (part 1).
//! 3. **From a multi-threaded runtime** (axum, tonic, ...), give the
//!    connection to a thread of its own and send it requests over a
//!    channel. The handle is `Send + Clone`, so any task can use it
//!    (part 2).

use hipp::storage::{Sidecar, SidecarFile, Storage};
use hipp::{Config, Connection};
use hipp_runtimes::{exec, rows, sibling, sidecar_suffix, temp_db, try_rows};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};

// ---------------------------------------------------------------------
// An async Storage over tokio::fs
// ---------------------------------------------------------------------

/// One file of a database (the main file or a sidecar), on `tokio::fs`.
/// A sidecar is created lazily on its first write, as SQLite does, and
/// `delete` is idempotent.
struct TokioFile {
    path: PathBuf,
    /// `None` until a sidecar's first write, and after `delete`.
    /// `read_at` takes `&self`, so the file sits behind an async mutex.
    file: Mutex<Option<File>>,
    existed: bool,
}

impl TokioFile {
    async fn open(path: impl Into<PathBuf>) -> std::io::Result<TokioFile> {
        let path = path.into();
        let file = open_rw(&path).await?;
        Ok(TokioFile {
            path,
            file: Mutex::new(Some(file)),
            existed: true,
        })
    }

    /// The open file, creating it first if this is a sidecar's first
    /// write. Writes take `&mut self`, so no lock is needed.
    async fn ensure(&mut self) -> std::io::Result<&mut File> {
        let slot = self.file.get_mut();
        if slot.is_none() {
            *slot = Some(open_rw(&self.path).await?);
        }
        Ok(slot.as_mut().expect("just opened"))
    }

    async fn len_(&self) -> std::io::Result<u64> {
        match tokio::fs::metadata(&self.path).await {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    async fn read_at_(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut guard = self.file.lock().await;
        match guard.as_mut() {
            Some(f) => {
                f.seek(SeekFrom::Start(offset)).await?;
                f.read_exact(buf).await?;
                Ok(())
            }
            None if buf.is_empty() => Ok(()),
            None => Err(std::io::ErrorKind::UnexpectedEof.into()),
        }
    }

    async fn write_at_(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        let f = self.ensure().await?;
        f.seek(SeekFrom::Start(offset)).await?;
        f.write_all(buf).await?;
        // tokio buffers writes; flush waits for this one to land, so
        // its error (if any) is reported here, not on a later call.
        f.flush().await
    }

    async fn set_len_(&mut self, len: u64) -> std::io::Result<()> {
        self.ensure().await?.set_len(len).await
    }

    async fn sync_(&mut self) -> std::io::Result<()> {
        match self.file.get_mut() {
            Some(f) => f.sync_all().await,
            None => Ok(()),
        }
    }
}

async fn open_rw(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .await
}

impl Storage for TokioFile {
    type Error = std::io::Error;
    type Sidecar = TokioFile;

    async fn len(&self) -> std::io::Result<u64> {
        self.len_().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.read_at_(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.write_at_(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len_(len).await
    }
    async fn sync(&mut self) -> std::io::Result<()> {
        self.sync_().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> std::io::Result<TokioFile> {
        let path = sibling(&self.path, sidecar_suffix(kind));
        let existed = tokio::fs::try_exists(&path).await?;
        // An existing sidecar (a hot journal) is opened now so it can
        // be read; a new one is created on its first write.
        let file = if existed {
            Some(open_rw(&path).await?)
        } else {
            None
        };
        Ok(TokioFile {
            path,
            file: Mutex::new(file),
            existed,
        })
    }
}

impl SidecarFile for TokioFile {
    type Error = std::io::Error;

    fn existed(&self) -> bool {
        self.existed
    }
    async fn len(&self) -> std::io::Result<u64> {
        self.len_().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.read_at_(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.write_at_(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len_(len).await
    }
    async fn sync(&mut self) -> std::io::Result<()> {
        self.sync_().await
    }
    async fn delete(&mut self) -> std::io::Result<()> {
        *self.file.get_mut() = None;
        match tokio::fs::remove_file(&self.path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------
// Part 1: one thread, several tasks, one connection
// ---------------------------------------------------------------------

/// The connection shared between the tasks of one thread. An *async*
/// mutex, because a task holds it across the awaits of a statement.
type Shared = Rc<Mutex<Connection<TokioFile>>>;

async fn part1(path: &Path) {
    println!("== Part 1: current-thread runtime, LocalSet, async file I/O ==");
    let storage = TokioFile::open(path).await.expect("open file");
    let mut db = Connection::open(storage, Config::default())
        .await
        .expect("open database");
    exec(
        &mut db,
        "CREATE TABLE readings (id INTEGER PRIMARY KEY, sensor TEXT, v REAL)",
    )
    .await;
    let db: Shared = Rc::new(Mutex::new(db));

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Two writers and a reader, interleaving on one thread.
            let mut tasks = Vec::new();
            for sensor in ["north", "south"] {
                let db = db.clone();
                tasks.push(tokio::task::spawn_local(async move {
                    for i in 0..5 {
                        let sql =
                            format!("INSERT INTO readings (sensor, v) VALUES ('{sensor}', {i}.5)");
                        // Lock, run one statement, unlock: the other
                        // tasks run between statements.
                        exec(&mut *db.lock().await, &sql).await;
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }
                }));
            }
            let reader = {
                let db = db.clone();
                tokio::task::spawn_local(async move {
                    loop {
                        let n = rows(&mut *db.lock().await, "SELECT count(*) FROM readings").await;
                        println!("reader sees {} rows", n[0]);
                        if n[0] == "10" {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
                    }
                })
            };
            for t in tasks {
                t.await.expect("writer");
            }
            reader.await.expect("reader");
        })
        .await;

    let db = Rc::try_unwrap(db).ok().expect("tasks done").into_inner();
    let mut db = db;
    let per_sensor = rows(
        &mut db,
        "SELECT sensor, count(*), sum(v) FROM readings GROUP BY sensor ORDER BY sensor",
    )
    .await;
    println!("{per_sensor:?}");
    assert_eq!(per_sensor, ["north|5|12.5", "south|5|12.5"]);
    db.close().await.expect("close");
}

// ---------------------------------------------------------------------
// Part 2: a multi-threaded runtime talking to a database thread
// ---------------------------------------------------------------------

/// A request to the database thread: one SQL statement and where to
/// send its rows.
struct Request {
    sql: String,
    reply: oneshot::Sender<Result<Vec<String>, String>>,
}

/// A `Send + Clone` handle to the database thread. Any tokio task can
/// hold one.
#[derive(Clone)]
struct Db {
    tx: mpsc::Sender<Request>,
}

impl Db {
    /// Start a thread that owns the connection, with a current-thread
    /// runtime of its own (so `TokioFile` has a reactor and blocking
    /// pool there).
    fn spawn(path: PathBuf) -> (Db, std::thread::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<Request>(64);
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("db runtime");
            rt.block_on(async move {
                let storage = TokioFile::open(&path).await.expect("open file");
                let mut db = Connection::open(storage, Config::default())
                    .await
                    .expect("open database");
                // Requests run one at a time, in arrival order.
                while let Some(req) = rx.recv().await {
                    let result = try_rows(&mut db, &req.sql).await;
                    let _ = req.reply.send(result);
                }
                // Every handle is dropped: close cleanly.
                db.close().await.expect("close");
            });
        });
        (Db { tx }, thread)
    }

    async fn query(&self, sql: impl Into<String>) -> Result<Vec<String>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request {
                sql: sql.into(),
                reply,
            })
            .await
            .map_err(|_| "database thread is gone".to_string())?;
        rx.await
            .map_err(|_| "database thread dropped the request".to_string())?
    }
}

async fn part2(path: PathBuf) {
    println!("\n== Part 2: multi-threaded runtime, a database thread, a Send handle ==");
    let (db, thread) = Db::spawn(path);
    db.query("CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, worker INTEGER, n INTEGER)")
        .await
        .expect("create");

    // Sixteen tasks on the worker pool, each writing and reading.
    let mut handles = Vec::new();
    for worker in 0..16 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            for n in 0..10 {
                db.query(format!(
                    "INSERT INTO jobs (worker, n) VALUES ({worker}, {n})"
                ))
                .await
                .expect("insert");
            }
            db.query(format!("SELECT count(*) FROM jobs WHERE worker = {worker}"))
                .await
                .expect("count")
        }));
    }
    for h in handles {
        assert_eq!(h.await.expect("task"), ["10"]);
    }
    let total = db
        .query("SELECT count(*), sum(n) FROM jobs")
        .await
        .expect("total");
    println!("16 tasks x 10 inserts: {total:?}");
    assert_eq!(total, ["160|720"]);

    // Errors travel back over the channel too.
    let err = db
        .query("SELECT * FROM nope")
        .await
        .expect_err("no such table");
    println!("error from the database thread: {err}");
    assert!(err.contains("UnknownTable"));

    // Data from part 1 is in the same file.
    assert_eq!(
        db.query("SELECT count(*) FROM readings")
            .await
            .expect("readings"),
        ["10"]
    );

    drop(db); // the last handle: the thread closes the database and exits
    thread.join().expect("db thread");
}

fn main() {
    let path = temp_db("tokio");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(part1(&path));

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(part2(path.clone()));

    // Every commit deleted its journal, and the file is an ordinary
    // SQLite database: hipp's own FileStorage reads it back intact.
    assert!(!sibling(&path, "-journal").exists());
    hipp::storage::block_on(async {
        let storage = hipp::FileStorage::open(&path).expect("open");
        let mut db = Connection::open(storage, Config::default())
            .await
            .expect("reopen");
        assert_eq!(rows(&mut db, "PRAGMA integrity_check").await, ["ok"]);
        assert_eq!(rows(&mut db, "SELECT count(*) FROM jobs").await, ["160"]);
    });
    println!("reopened with FileStorage: integrity_check ok");
    hipp_runtimes::remove_db(&path);
    println!("\ntokio: ok");
}
