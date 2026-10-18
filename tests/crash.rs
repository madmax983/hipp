//! A6: crash injection at every write boundary of an INSERT
//! commit (SPEC §9.2). Two halves:
//!
//! 1. In-process: a storage wrapper that "dies" (all further
//!    mutating ops fail) at op k, for every k. The connection is
//!    dropped with no cleanup; reopening must recover to exactly
//!    the pre-image or the post-image — file bytes included.
//! 2. Cross-tool: a hipp process killed mid-commit (exit(42), the
//!    `crash_writer` example) leaves a journal that REAL SQLite
//!    rolls back; oracle integrity + row count must agree with
//!    hipp's own reopened view.

mod common;

use hipp::storage::{MemError, MemStorage, Sidecar, SidecarFile, Storage, block_on};
use hipp::{Config, Connection, Step};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug)]
enum CrashError {
    Dead,
    Inner(MemError),
}

#[derive(Default)]
struct CrashState {
    ops: usize,
    kill_at: Option<usize>,
    dead: bool,
}

#[derive(Clone)]
struct CrashStorage {
    mem: MemStorage,
    st: Rc<RefCell<CrashState>>,
}

struct CrashSidecar {
    inner: hipp::storage::MemSidecar,
    st: Rc<RefCell<CrashState>>,
}

fn gate_state(st: &Rc<RefCell<CrashState>>) -> Result<(), CrashError> {
    let mut st = st.borrow_mut();
    if st.dead {
        return Err(CrashError::Dead);
    }
    if st.kill_at == Some(st.ops) {
        st.dead = true;
        return Err(CrashError::Dead);
    }
    st.ops += 1;
    Ok(())
}

impl CrashStorage {
    fn gate(&self) -> Result<(), CrashError> {
        gate_state(&self.st)
    }
    fn ops(&self) -> usize {
        self.st.borrow().ops
    }
}

impl Storage for CrashStorage {
    type Error = CrashError;
    type Sidecar = CrashSidecar;

    async fn len(&self) -> Result<u64, CrashError> {
        self.mem.len().await.map_err(CrashError::Inner)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), CrashError> {
        self.mem
            .read_at(offset, buf)
            .await
            .map_err(CrashError::Inner)
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), CrashError> {
        self.gate()?;
        self.mem
            .write_at(offset, buf)
            .await
            .map_err(CrashError::Inner)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), CrashError> {
        self.gate()?;
        self.mem.set_len(len).await.map_err(CrashError::Inner)
    }
    async fn sync(&mut self) -> Result<(), CrashError> {
        self.gate()?;
        self.mem.sync().await.map_err(CrashError::Inner)
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<CrashSidecar, CrashError> {
        Ok(CrashSidecar {
            inner: self.mem.sidecar(kind).await.map_err(CrashError::Inner)?,
            st: self.st.clone(),
        })
    }
}

impl SidecarFile for CrashSidecar {
    type Error = CrashError;
    async fn len(&self) -> Result<u64, CrashError> {
        self.inner.len().await.map_err(CrashError::Inner)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), CrashError> {
        self.inner
            .read_at(offset, buf)
            .await
            .map_err(CrashError::Inner)
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), CrashError> {
        gate_state(&self.st)?;
        self.inner
            .write_at(offset, buf)
            .await
            .map_err(CrashError::Inner)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), CrashError> {
        gate_state(&self.st)?;
        self.inner.set_len(len).await.map_err(CrashError::Inner)
    }
    async fn sync(&mut self) -> Result<(), CrashError> {
        gate_state(&self.st)?;
        self.inner.sync().await.map_err(CrashError::Inner)
    }
    fn existed(&self) -> bool {
        self.inner.existed()
    }
    async fn delete(&mut self) -> Result<(), CrashError> {
        gate_state(&self.st)?;
        self.inner.delete().await.map_err(CrashError::Inner)
    }
}

fn crash_storage(mem: MemStorage) -> CrashStorage {
    CrashStorage {
        mem,
        st: Rc::new(RefCell::new(CrashState::default())),
    }
}

async fn setup_db(mem: MemStorage) {
    let mut db = Connection::open(mem, Config::default())
        .await
        .expect("open");
    for sql in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
        "INSERT INTO t VALUES (1, 'one')",
    ] {
        let mut st = db.prepare(sql).await.expect("prep");
        assert_eq!(st.step().await.expect("step"), Step::Done);
        drop(st);
    }
}

async fn scan_ids<S: Storage>(db: &mut Connection<S>) -> Vec<i64>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare("SELECT id FROM t").await.expect("prep");
    let mut out = Vec::new();
    while q.step().await.expect("step") == Step::Row {
        out.push(q.column_i64(0).expect("id"));
    }
    out
}

#[test]
fn a6_every_commit_boundary() {
    block_on(async {
        // Pre-image and post-image, byte-exact.
        let mem_pre = MemStorage::new();
        setup_db(mem_pre.clone()).await;
        let pre_bytes = mem_pre.snapshot();

        let mem_post = MemStorage::new();
        setup_db(mem_post.clone()).await;
        {
            let mut db = Connection::open(mem_post.clone(), Config::default())
                .await
                .expect("open");
            let mut st = db
                .prepare("INSERT INTO t VALUES (2, 'two')")
                .await
                .expect("prep");
            assert_eq!(st.step().await.expect("step"), Step::Done);
        }
        let post_bytes = mem_post.snapshot();
        assert_ne!(pre_bytes, post_bytes);

        // Measure the insert's mutating-op count.
        let total_ops = {
            let mem = MemStorage::new();
            mem.set_snapshot(pre_bytes.clone());
            let cs = crash_storage(mem);
            let mut db = Connection::open(cs.clone(), Config::default())
                .await
                .expect("open");
            let before = cs.ops();
            let mut st = db
                .prepare("INSERT INTO t VALUES (2, 'two')")
                .await
                .expect("prep");
            assert_eq!(st.step().await.expect("step"), Step::Done);
            drop(st);
            drop(db);
            cs.ops() - before
        };
        assert!(
            total_ops >= 8,
            "commit should have several ops, got {total_ops}"
        );

        for k in 0..total_ops {
            let mem = MemStorage::new();
            mem.set_snapshot(pre_bytes.clone());
            let cs = crash_storage(mem.clone());
            cs.st.borrow_mut().kill_at = Some(k);
            let mut db = Connection::open(cs.clone(), Config::default())
                .await
                .expect("open");
            let mut st = db
                .prepare("INSERT INTO t VALUES (2, 'two')")
                .await
                .expect("prep");
            let r = st.step().await;
            assert!(r.is_err(), "k={k}: insert should fail once dead");
            // Process "dies": drop everything, no cleanup.
            drop(st);
            drop(db);
            // Reopen on the same bytes (journal included) → recover.
            let mut db = Connection::open(mem.clone(), Config::default())
                .await
                .unwrap_or_else(|e| panic!("k={k}: reopen failed: {e:?}"));
            let ids = scan_ids(&mut db).await;
            drop(db);
            let bytes = mem.snapshot();
            if ids == vec![1] {
                assert_eq!(bytes, pre_bytes, "k={k}: pre-image bytes");
                assert!(mem.journal_snapshot().is_none(), "k={k}: journal consumed");
            } else if ids == vec![1, 2] {
                assert_eq!(bytes, post_bytes, "k={k}: post-image bytes");
            } else {
                panic!("k={k}: torn state, ids={ids:?}");
            }
        }
    });
}

fn crash_writer_bin() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/examples/crash_writer");
    if !p.exists() {
        // `cargo test --test crash` alone does not build examples;
        // build the harness binary on first use.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let out = std::process::Command::new(cargo)
            .args(["build", "-j2", "--example", "crash_writer"])
            .env("RUSTUP_TOOLCHAIN", "stable")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo build example");
        assert!(
            out.status.success(),
            "building crash_writer: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    p
}

fn run_writer(
    path: &std::path::Path,
    mode: &str,
    kill: Option<usize>,
    count: bool,
) -> (i32, String) {
    let mut cmd = std::process::Command::new(crash_writer_bin());
    cmd.arg(path).env("MODE", mode);
    if let Some(k) = kill {
        cmd.env("KILL_AT", k.to_string());
    }
    if count {
        cmd.env("COUNT", "1");
    }
    let out = cmd.output().expect("spawn crash_writer");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a6_cross_tool_real_sqlite_recovers_hipp_journal() {
    let dir = std::env::temp_dir().join(format!("hipp-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let base = dir.join("base.db");
    let (code, _) = run_writer(&base, "setup", None, false);
    assert_eq!(code, 0);

    // Count the insert's mutating ops.
    let probe = dir.join("probe.db");
    std::fs::copy(&base, &probe).expect("copy");
    let (code, err) = run_writer(&probe, "insert", None, true);
    assert_eq!(code, 0, "count run: {err}");
    let total: usize = err
        .lines()
        .find_map(|l| l.strip_prefix("OPS="))
        .and_then(|v| v.parse().ok())
        .expect("OPS line");

    for k in 0..total {
        let f = dir.join(format!("k{k}.db"));
        std::fs::copy(&base, &f).expect("copy");
        let (code, _) = run_writer(&f, "insert", Some(k), false);
        assert_eq!(code, 42, "k={k}: writer should die at the boundary");
        // Real SQLite opens the file: its recovery consumes hipp's
        // journal. Integrity must hold; the row count is pre or post.
        assert_eq!(
            common::cli(&f, "PRAGMA integrity_check;").trim(),
            "ok",
            "k={k}"
        );
        let count = common::cli(&f, "SELECT count(*) FROM t;")
            .trim()
            .to_string();
        assert!(count == "1" || count == "2", "k={k}: count {count}");
        // hipp agrees with the oracle's post-recovery view.
        block_on(async {
            let fs = hipp::FileStorage::open(&f).expect("fs");
            let mut db = Connection::open(fs, Config::default())
                .await
                .expect("hipp open");
            let ids = scan_ids(&mut db).await;
            assert_eq!(ids.len().to_string(), count, "k={k}");
        });
    }
}
