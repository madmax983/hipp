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
    // This test runs from target/<profile>/deps/crash-<hash>; the
    // example lands in target/<profile>/examples/. Deriving the path
    // from the running executable honours CARGO_TARGET_DIR and the
    // build profile instead of assuming ./target/debug.
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile> directory")
        .to_path_buf();
    let p = profile_dir
        .join("examples")
        .join(format!("crash_writer{}", std::env::consts::EXE_SUFFIX));
    if !p.exists() {
        // `cargo test --test crash` alone does not build examples;
        // build the harness binary on first use. `CARGO` is the cargo
        // that is running this test, and the environment (toolchain,
        // RUSTFLAGS, e.g. coverage instrumentation) is inherited, so
        // the helper is built exactly like the test itself.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let target_dir = profile_dir.parent().expect("target directory");
        let mut cmd = std::process::Command::new(cargo);
        cmd.args(["build", "-j2", "--example", "crash_writer", "--target-dir"])
            .arg(target_dir);
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") | None => {}
            Some(profile) => {
                cmd.args(["--profile", profile]);
            }
        }
        let out = cmd
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
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-crash");
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

// ---------------------------------------------------------------
// v0.16: crash injection across whole transactions — savepoints,
// statement savepoints, VACUUM, and the two journal fixes found on
// the way (a spilled page this transaction allocated was journaled
// when it changed again, and replay stops at such a record; a
// freelist leaf freed and reused in one transaction was not
// journaled). A crash at any op must leave the file byte-exactly
// as before or as after the outermost transaction.
// ---------------------------------------------------------------

/// Run every statement to completion, ignoring errors (a dead
/// device fails them all from the kill on).
async fn run_all<S: Storage>(db: &mut Connection<S>, sqls: &[String]) {
    for sql in sqls {
        if let Ok(mut st) = db.prepare(sql).await {
            while let Ok(Step::Row) = st.step().await {}
        }
    }
}

/// A recursive-CTE row source of `n` rows numbered from `from`
/// with `width`-byte texts.
#[cfg(feature = "v16")]
fn rows_sql(table: &str, from: i64, n: i64, width: usize) -> String {
    format!(
        "INSERT INTO {table} WITH RECURSIVE c(i) AS (SELECT {from} UNION ALL \
         SELECT i + 1 FROM c WHERE i < {}) SELECT i, printf('%0{width}d', i) FROM c",
        from + n - 1
    )
}

/// Every crash point of `txn` (run after `setup`, under `cfg`)
/// recovers to the pre- or the post-image, bytes included, and
/// the recovered file scans cleanly.
fn crash_matrix(tag: &str, cfg: Config, setup: &[String], txn: &[String]) {
    block_on(async {
        let mem_pre = MemStorage::new();
        {
            let mut db = Connection::open(mem_pre.clone(), cfg.clone())
                .await
                .expect("open");
            run_all(&mut db, setup).await;
        }
        let pre_bytes = mem_pre.snapshot();
        let mem_post = MemStorage::new();
        mem_post.set_snapshot(pre_bytes.clone());
        {
            let mut db = Connection::open(mem_post.clone(), cfg.clone())
                .await
                .expect("open");
            run_all(&mut db, txn).await;
        }
        assert!(mem_post.journal_snapshot().is_none(), "{tag}: committed");
        let post_bytes = mem_post.snapshot();
        assert_ne!(
            pre_bytes, post_bytes,
            "{tag}: the transaction changes the file"
        );
        let total_ops = {
            let mem = MemStorage::new();
            mem.set_snapshot(pre_bytes.clone());
            let cs = crash_storage(mem);
            let mut db = Connection::open(cs.clone(), cfg.clone())
                .await
                .expect("open");
            let before = cs.ops();
            run_all(&mut db, txn).await;
            drop(db);
            cs.ops() - before
        };
        assert!(total_ops >= 4, "{tag}: ops {total_ops}");
        let mut saw_pre = false;
        for k in 0..total_ops {
            let mem = MemStorage::new();
            mem.set_snapshot(pre_bytes.clone());
            let cs = crash_storage(mem.clone());
            {
                let mut db = Connection::open(cs.clone(), cfg.clone())
                    .await
                    .expect("open");
                cs.st.borrow_mut().kill_at = Some(cs.ops() + k);
                run_all(&mut db, txn).await;
            }
            let mut db = Connection::open(mem.clone(), cfg.clone())
                .await
                .unwrap_or_else(|e| panic!("{tag} k={k}: reopen failed: {e:?}"));
            let mut q = db.prepare("SELECT count(*) FROM t").await.expect("prep");
            assert_eq!(q.step().await.expect("scan"), Step::Row, "{tag} k={k}");
            drop(q);
            drop(db);
            let bytes = mem.snapshot();
            if bytes == pre_bytes {
                assert!(
                    mem.journal_snapshot().is_none(),
                    "{tag} k={k}: journal consumed"
                );
                saw_pre = true;
            } else if bytes != post_bytes {
                panic!("{tag} k={k}/{total_ops}: torn file");
            }
        }
        assert!(saw_pre, "{tag}: a crash before the commit point restores");
    });
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[cfg(feature = "v16")]
#[test]
fn v16_crash_savepoint_transaction() {
    let setup = strs(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
        "INSERT INTO t VALUES (1, 'one')",
    ]);
    // SAVEPOINT outside BEGIN: releasing it commits.
    let txn = strs(&[
        "SAVEPOINT a",
        "INSERT INTO t VALUES (2, 'two')",
        "SAVEPOINT b",
        "INSERT INTO t VALUES (3, 'three')",
        "CREATE TABLE u (v)",
        "ROLLBACK TO b",
        "INSERT INTO t VALUES (4, 'four')",
        "INSERT INTO t VALUES (4, 'dup')",
        "RELEASE a",
    ]);
    crash_matrix("savepoints", Config::default(), &setup, &txn);
    // Inside BEGIN, with a statement that fails half-way.
    let txn = strs(&[
        "BEGIN",
        "SAVEPOINT a",
        "INSERT INTO t VALUES (2, 'two')",
        "INSERT INTO t VALUES (3, 'x'), (1, 'dup')",
        "RELEASE a",
        "SAVEPOINT c",
        "UPDATE t SET x = 'changed'",
        "ROLLBACK TO c",
        "COMMIT",
    ]);
    crash_matrix("savepoints-begin", Config::default(), &setup, &txn);
}

/// Spills under a small cache, with pages the transaction
/// allocated changed again after their spill, inside savepoints
/// rolled back to across the spills.
#[cfg(feature = "v16")]
#[test]
fn v16_crash_spill_rechange_and_savepoints() {
    let cfg = Config::default().cache_pages(4);
    let setup = strs(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
        "INSERT INTO t VALUES (1, 'one')",
    ]);
    let txn = vec![
        "BEGIN".to_string(),
        rows_sql("t", 2, 120, 150),
        "SAVEPOINT s".to_string(),
        "UPDATE t SET x = x || 'more'".to_string(),
        rows_sql("t", 200, 60, 150),
        "ROLLBACK TO s".to_string(),
        "UPDATE t SET x = 'v' || x WHERE id % 3 = 0".to_string(),
        "COMMIT".to_string(),
    ];
    crash_matrix("spill", cfg, &setup, &txn);
}

/// Pages freed and reused in one transaction.
#[cfg(feature = "v16")]
#[test]
fn v16_crash_freelist_reuse() {
    let setup = vec![
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)".to_string(),
        rows_sql("t", 1, 150, 120),
    ];
    let txn = vec![
        "BEGIN".to_string(),
        "DELETE FROM t WHERE id > 20".to_string(),
        rows_sql("t", 1000, 150, 100),
        "COMMIT".to_string(),
    ];
    crash_matrix("freelist", Config::default(), &setup, &txn);
}

#[cfg(feature = "v16")]
#[test]
fn v16_crash_vacuum() {
    let setup = vec![
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)".to_string(),
        "CREATE INDEX tx ON t(x)".to_string(),
        rows_sql("t", 1, 120, 100),
        "DELETE FROM t WHERE id % 2 = 0".to_string(),
    ];
    crash_matrix("vacuum", Config::default(), &setup, &strs(&["VACUUM"]));
    // A page-size change through VACUUM.
    let mut txn = strs(&["PRAGMA page_size = 1024", "VACUUM"]);
    crash_matrix("vacuum-1024", Config::default(), &setup, &txn);
    txn[0] = "PRAGMA page_size = 16384".to_string();
    crash_matrix("vacuum-16384", Config::default(), &setup, &txn);
}

/// The cross-tool case for a savepoint transaction (v0.16): copies
/// of pages the transaction had already changed live past the
/// largest rollback journal it can write, behind a zero record, so
/// real SQLite's recovery stops before them at every kill point.
#[cfg(feature = "v16")]
#[test]
fn v16_cross_tool_real_sqlite_recovers_savepoint_journal() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-crash-sp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let base = dir.join("base.db");
    let (code, _) = run_writer(&base, "setup", None, false);
    assert_eq!(code, 0);
    let probe = dir.join("probe.db");
    std::fs::copy(&base, &probe).expect("copy");
    let (code, err) = run_writer(&probe, "savepoint", None, true);
    assert_eq!(code, 0, "count run: {err}");
    assert_eq!(common::cli(&probe, "SELECT id FROM t;"), "1\n2\n4\n");
    let total: usize = err
        .lines()
        .find_map(|l| l.strip_prefix("OPS="))
        .and_then(|v| v.parse().ok())
        .expect("OPS line");
    for k in 0..total {
        let f = dir.join(format!("sp{k}.db"));
        std::fs::copy(&base, &f).expect("copy");
        let (code, _) = run_writer(&f, "savepoint", Some(k), false);
        assert_eq!(code, 42, "k={k}: writer should die at the boundary");
        assert_eq!(
            common::cli(&f, "PRAGMA integrity_check;").trim(),
            "ok",
            "k={k}"
        );
        let ids = common::cli(&f, "SELECT group_concat(id) FROM t;");
        assert!(ids == "1\n" || ids == "1,2,4\n", "k={k}: ids {ids}");
    }
}

/// A WITHOUT ROWID table's clustered tree and secondary index
/// through the journal (v0.16, `without_rowid`): a crash at every
/// write boundary of a transaction that inserts (splitting the
/// trees), moves rows by key and deletes recovers to the pre- or
/// post-image.
#[cfg(feature = "without_rowid")]
#[test]
fn v16_crash_without_rowid() {
    let setup = strs(&[
        "CREATE TABLE t (id TEXT PRIMARY KEY, x TEXT) WITHOUT ROWID",
        "CREATE INDEX tx ON t(x)",
        "INSERT INTO t VALUES ('k1', 'one')",
    ]);
    let txn = vec![
        "BEGIN".to_string(),
        "INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 2 UNION ALL SELECT i + 1 FROM c \
         WHERE i < 150) SELECT printf('k%04d', i), printf('%0120d', i) FROM c"
            .to_string(),
        "UPDATE t SET id = id || 'm' WHERE x LIKE '%5'".to_string(),
        "DELETE FROM t WHERE x LIKE '%3'".to_string(),
        "COMMIT".to_string(),
    ];
    crash_matrix("without-rowid", Config::default(), &setup, &txn);
}
