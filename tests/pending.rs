//! A11: pending injection (SPEC §9.2, D2). Every A1-style flow
//! re-run against a Storage whose futures return `Pending` several
//! times at every I/O point — outcomes identical. Plus the
//! future-drop table: a step future dropped mid-commit leaves a
//! transaction that `resume_txn` finishes (post-image), that a
//! fresh open recovers (pre-image), and that blocks any second
//! driver with `OperationInFlight` in between.

use hipp::storage::{MemStorage, Sidecar, SidecarFile, Storage, block_on};
use hipp::{Config, Connection, Error, Step};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// A no-op waker for hand-polling (storage futures are polled in a
// busy loop by the tests; Pending just means "poll me again").
fn noop_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let w = noop_waker();
    let mut cx = Context::from_waker(&w);
    fut.poll(&mut cx)
}

fn drive<F: Future>(fut: F) -> F::Output {
    let mut fut = Box::pin(fut);
    loop {
        match poll_once(fut.as_mut()) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::hint::spin_loop(),
        }
    }
}

/// A future that returns Pending `times_left` times, then Ready.
struct Pend {
    left: usize,
}
impl Future for Pend {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.left == 0 {
            Poll::Ready(())
        } else {
            self.left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

type Log = Rc<RefCell<Vec<&'static str>>>;

#[derive(Clone)]
struct PendStorage {
    mem: MemStorage,
    per_op: usize,
    log: Log,
}

struct PendSidecar {
    inner: hipp::storage::MemSidecar,
    per_op: usize,
    log: Log,
}

impl PendStorage {
    fn new(mem: MemStorage, per_op: usize) -> PendStorage {
        PendStorage {
            mem,
            per_op,
            log: Rc::new(RefCell::new(Vec::new())),
        }
    }
    async fn pend(&self) {
        Pend { left: self.per_op }.await
    }
}

impl Storage for PendStorage {
    type Error = hipp::storage::MemError;
    type Sidecar = PendSidecar;

    async fn len(&self) -> Result<u64, Self::Error> {
        self.pend().await;
        self.mem.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.pend().await;
        self.mem.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error> {
        self.pend().await;
        self.log.borrow_mut().push("write");
        self.mem.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error> {
        self.pend().await;
        self.log.borrow_mut().push("set_len");
        self.mem.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), Self::Error> {
        self.pend().await;
        self.log.borrow_mut().push("sync");
        self.mem.sync().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<PendSidecar, Self::Error> {
        Ok(PendSidecar {
            inner: self.mem.sidecar(kind).await?,
            per_op: self.per_op,
            log: self.log.clone(),
        })
    }
}

impl SidecarFile for PendSidecar {
    type Error = hipp::storage::MemError;

    fn existed(&self) -> bool {
        self.inner.existed()
    }
    async fn len(&self) -> Result<u64, Self::Error> {
        Pend { left: self.per_op }.await;
        self.inner.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        Pend { left: self.per_op }.await;
        self.inner.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error> {
        Pend { left: self.per_op }.await;
        self.log.borrow_mut().push("jwrite");
        self.inner.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), Self::Error> {
        Pend { left: self.per_op }.await;
        self.log.borrow_mut().push("jset_len");
        self.inner.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), Self::Error> {
        Pend { left: self.per_op }.await;
        self.log.borrow_mut().push("jsync");
        self.inner.sync().await
    }
    async fn delete(&mut self) -> Result<(), Self::Error> {
        Pend { left: self.per_op }.await;
        self.log.borrow_mut().push("jdelete");
        self.inner.delete().await
    }
}

async fn script<S: Storage>(db: &mut Connection<S>) -> Vec<(i64, String)>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db
        .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .await
        .expect("prep create");
    assert_eq!(st.step().await.expect("create"), Step::Done);
    drop(st);
    for (id, name) in [(1, "one"), (2, "two"), (3, "three")] {
        let mut st = db
            .prepare(&format!("INSERT INTO t VALUES ({id}, '{name}')"))
            .await
            .expect("prep insert");
        assert_eq!(st.step().await.expect("insert"), Step::Done);
        drop(st);
    }
    let mut q = db.prepare("SELECT id, name FROM t").await.expect("prep select");
    let mut rows = Vec::new();
    while q.step().await.expect("step") == Step::Row {
        rows.push((
            q.column_i64(0).expect("id"),
            String::from_utf8_lossy(q.column_bytes(1).expect("name")).into_owned(),
        ));
    }
    rows
}

#[test]
fn a11_pending_outcomes_identical() {
    // Baseline (no pending).
    let base_mem = MemStorage::new();
    let base_rows = drive(async {
        let mut db = Connection::open(base_mem.clone(), Config::default())
            .await
            .expect("open");
        let rows = script(&mut db).await;
        rows
    });
    let base_bytes = base_mem.snapshot();

    for per_op in [1usize, 2, 5, 13] {
        let mem = MemStorage::new();
        let rows = drive(async {
            let ps = PendStorage::new(mem.clone(), per_op);
            let mut db = Connection::open(ps, Config::default()).await.expect("open");
            script(&mut db).await
        });
        assert_eq!(rows, base_rows, "per_op={per_op}");
        assert_eq!(mem.snapshot(), base_bytes, "per_op={per_op} bytes");
    }
}

/// Drive an INSERT's step future until the journal has synced but
/// no data page has been written yet: unambiguously mid-commit.
/// Returns the connection with the future (and statement) dropped.
async fn drop_mid_commit() -> (MemStorage, Connection<PendStorage>) {
    let mem = MemStorage::new();
    let ps = PendStorage::new(mem.clone(), 3);
    let log = ps.log.clone();
    let mut db = Connection::open(ps, Config::default()).await.expect("open");
    {
        let mut st = db
            .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)")
            .await
            .expect("prep");
        st.step().await.expect("create");
    }
    {
        let mut st = db
            .prepare("INSERT INTO t VALUES (1, 'one')")
            .await
            .expect("prep");
        st.step().await.expect("insert1");
    }
    log.borrow_mut().clear();
    let mut st = db
        .prepare("INSERT INTO t VALUES (2, 'two')")
        .await
        .expect("prep");
    let mut fut = Box::pin(st.step());
    let mut polls = 0usize;
    loop {
        let _ = poll_once(fut.as_mut());
        polls += 1;
        assert!(polls < 100_000, "step future never reached mid-commit");
        let l = log.borrow();
        let jsynced = l.iter().any(|e| *e == "jsync");
        let wrote_pages = l.iter().any(|e| *e == "write");
        if jsynced && !wrote_pages {
            break;
        }
    }
    // The future dies here, mid-commit. So does the statement.
    drop(fut);
    drop(st);
    (mem, db)
}

#[test]
fn a11_drop_mid_commit_resume_finishes() {
    block_on(async {
        let (mem, mut db) = drop_mid_commit().await;
        assert!(db.in_transaction(), "txn still open after future drop");
        // A second driver is refused while the commit is in flight.
        let r = db.prepare("SELECT id FROM t").await;
        assert!(
            matches!(r, Err(Error::OperationInFlight)),
            "second driver: {:?}",
            r.err()
        );
        // resume_txn finishes the commit: post-image.
        assert!(db.resume_txn().await.expect("resume"));
        assert!(!db.in_transaction());
        let mut q = db.prepare("SELECT id FROM t").await.expect("prep");
        let mut ids = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            ids.push(q.column_i64(0).expect("id"));
        }
        drop(q);
        assert_eq!(ids, vec![1, 2]);
        assert!(mem.journal_snapshot().is_none(), "journal deleted by commit");
    });
}

#[test]
fn a11_drop_mid_commit_reopen_recovers() {
    block_on(async {
        let (mem, db) = drop_mid_commit().await;
        // Connection dies too. Next open replays the hot journal:
        // pre-image (only row 1).
        drop(db);
        let mut db = Connection::open(mem.clone(), Config::default())
            .await
            .expect("reopen");
        let mut q = db.prepare("SELECT id FROM t").await.expect("prep");
        let mut ids = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            ids.push(q.column_i64(0).expect("id"));
        }
        assert_eq!(ids, vec![1]);
    });
}
