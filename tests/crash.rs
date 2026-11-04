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

// ---------------------------------------------------------------
// v0.16 (`wal`): crash injection in WAL mode — frame appends (with
// the cache spilling into the log, savepoints and statement
// rollback), the commit frame, checkpoints of every kind (the
// automatic one too), log restarts, `journal_size_limit`
// truncation, mode switches and the checkpoint at close. A crash at
// any op must recover the last committed state: the state after the
// statements that completed, or after the one in flight. The
// recovered file and log also pass the pin's integrity check and
// read the same there.
// ---------------------------------------------------------------

/// The table's contents and hipp's integrity check, as one line.
#[cfg(feature = "wal")]
async fn wal_content<S: Storage>(db: &mut Connection<S>) -> String
where
    S::Error: core::fmt::Debug,
{
    let mut out = String::new();
    for sql in [
        "SELECT count(*), group_concat(id || ':' || length(x)) FROM (SELECT * FROM t ORDER BY id)",
        "PRAGMA integrity_check",
    ] {
        let mut q = db.prepare(sql).await.expect("prep");
        while q.step().await.expect("step") == Step::Row {
            for i in 0..q.column_count() {
                if i > 0 {
                    out.push('|');
                }
                match q.column_value(i) {
                    Some(hipp::Value::Integer(v)) => out.push_str(&v.to_string()),
                    Some(hipp::Value::Text(b)) => out.push_str(&String::from_utf8_lossy(b)),
                    _ => {}
                }
            }
            out.push('\n');
        }
    }
    out
}

/// Write a MemStorage's database and log to `path` (+ `-wal`).
#[cfg(feature = "wal")]
fn mem_to_files(mem: &MemStorage, path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::remove_file(&wal);
    std::fs::write(path, mem.snapshot()).expect("db");
    if let Some(w) = mem.wal_snapshot() {
        std::fs::write(&wal, w).expect("wal");
    }
}

#[cfg(feature = "wal")]
fn wal_crash_matrix(tag: &str, cfg: Config, setup: &[String], steps: &[String], close: bool) {
    block_on(async {
        let pre = MemStorage::new();
        {
            let mut db = Connection::open(pre.clone(), cfg.clone())
                .await
                .expect("open");
            run_all(&mut db, setup).await;
        }
        let (pre_db, pre_wal) = (pre.snapshot(), pre.wal_snapshot());
        assert!(pre_db.get(18) == Some(&2), "{tag}: setup enters WAL mode");
        let fresh = || {
            let m = MemStorage::new();
            m.set_snapshot(pre_db.clone());
            m.set_wal(pre_wal.clone());
            m
        };
        // The committed state after each prefix of the steps: what a
        // new connection reads from a copy of the files (inside an
        // explicit transaction, the state before it).
        async fn committed(mem: &MemStorage, cfg: &Config) -> String {
            let m = MemStorage::new();
            m.set_snapshot(mem.snapshot());
            m.set_wal(mem.wal_snapshot());
            m.set_journal(mem.journal_snapshot());
            let mut db = Connection::open(m, cfg.clone()).await.expect("open copy");
            wal_content(&mut db).await
        }
        let mut states = Vec::new();
        {
            let mem = fresh();
            let mut db = Connection::open(mem.clone(), cfg.clone())
                .await
                .expect("open");
            states.push(committed(&mem, &cfg).await);
            for s in steps {
                run_all(&mut db, std::slice::from_ref(s)).await;
                states.push(committed(&mem, &cfg).await);
            }
            if close {
                db.close().await.expect("close");
                assert!(mem.wal_snapshot().is_none(), "{tag}: close removes the log");
            }
        }
        assert_ne!(states.first(), states.last(), "{tag}: the steps change t");
        let total_ops = {
            let cs = crash_storage(fresh());
            let mut db = Connection::open(cs.clone(), cfg.clone())
                .await
                .expect("open");
            let before = cs.ops();
            run_all(&mut db, steps).await;
            if close {
                db.close().await.expect("close");
            }
            cs.ops() - before
        };
        assert!(total_ops >= 8, "{tag}: ops {total_ops}");
        let file = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("hipp-walcrash-{tag}.db"));
        let mut seen = std::collections::BTreeSet::new();
        for k in 0..total_ops {
            let mem = fresh();
            let cs = crash_storage(mem.clone());
            let mut done = steps.len();
            {
                let mut db = Connection::open(cs.clone(), cfg.clone())
                    .await
                    .expect("open");
                cs.st.borrow_mut().kill_at = Some(cs.ops() + k);
                for (i, s) in steps.iter().enumerate() {
                    run_all(&mut db, std::slice::from_ref(s)).await;
                    if cs.st.borrow().dead {
                        done = i;
                        break;
                    }
                }
                if close && done == steps.len() {
                    let _ = db.close().await;
                }
            }
            let mut db = Connection::open(mem.clone(), cfg.clone())
                .await
                .unwrap_or_else(|e| panic!("{tag} k={k}: reopen failed: {e:?}"));
            let got = wal_content(&mut db).await;
            drop(db);
            let ok = [done, done + 1]
                .iter()
                .any(|&i| states.get(i) == Some(&got));
            assert!(
                ok,
                "{tag} k={k}/{total_ops}: recovered {got:?}, not the state after step {done} or {}",
                done + 1
            );
            assert!(got.ends_with("ok\n"), "{tag} k={k}: hipp integrity {got}");
            seen.insert(got.clone());
            // The pin reads the same files the same way.
            mem_to_files(&mem, &file);
            assert_eq!(
                common::cli(&file, "PRAGMA integrity_check;").trim(),
                "ok",
                "{tag} k={k}: the pin's integrity check"
            );
            let q = "SELECT count(*), group_concat(id || ':' || length(x)) FROM (SELECT * FROM t ORDER BY id);";
            assert_eq!(
                common::cli(&file, q),
                got.lines()
                    .next()
                    .map(|l| format!("{l}\n"))
                    .unwrap_or_default(),
                "{tag} k={k}: the pin's read"
            );
        }
        assert!(
            seen.len() >= 2,
            "{tag}: crashes recover more than one state"
        );
    });
}

#[cfg(feature = "wal")]
fn wal_setup() -> Vec<String> {
    strs(&[
        "PRAGMA journal_mode = WAL",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
        "CREATE INDEX tx ON t(x)",
        "INSERT INTO t VALUES (1, 'one')",
    ])
}

/// Frame appends, a transaction with savepoints and a failing
/// statement, checkpoints of each kind, restarts, and close.
#[cfg(feature = "wal")]
#[test]
fn v16_crash_wal_frames_and_checkpoints() {
    let steps = vec![
        "PRAGMA wal_autocheckpoint = 0".to_string(),
        "INSERT INTO t VALUES (2, 'two')".to_string(),
        rows_sql("t", 10, 40, 300),
        "BEGIN".to_string(),
        "UPDATE t SET x = x || 'u' WHERE id % 2 = 0".to_string(),
        "SAVEPOINT s".to_string(),
        "DELETE FROM t WHERE id > 30".to_string(),
        "ROLLBACK TO s".to_string(),
        "INSERT INTO t VALUES (3, 'x'), (1, 'dup')".to_string(),
        "INSERT INTO t VALUES (4, 'four')".to_string(),
        "COMMIT".to_string(),
        "PRAGMA wal_checkpoint(PASSIVE)".to_string(),
        "INSERT INTO t VALUES (5, 'five')".to_string(),
        "PRAGMA journal_size_limit = 0".to_string(),
        "PRAGMA wal_checkpoint(FULL)".to_string(),
        "DELETE FROM t WHERE id % 3 = 0".to_string(),
        "PRAGMA wal_checkpoint(TRUNCATE)".to_string(),
        "INSERT INTO t VALUES (6, printf('%05000d', 6))".to_string(),
    ];
    wal_crash_matrix("frames", Config::default(), &wal_setup(), &steps, true);
}

/// The automatic checkpoint, under a cache small enough that
/// transactions spill pages into the log and rewrite them in place.
#[cfg(feature = "wal")]
#[test]
fn v16_crash_wal_autocheckpoint_and_spills() {
    let steps = vec![
        "PRAGMA wal_autocheckpoint = 6".to_string(),
        "BEGIN".to_string(),
        rows_sql("t", 10, 90, 200),
        "SAVEPOINT s".to_string(),
        "UPDATE t SET x = x || 'more'".to_string(),
        "ROLLBACK TO s".to_string(),
        "UPDATE t SET x = 'v' || x WHERE id % 3 = 0".to_string(),
        "COMMIT".to_string(),
        "INSERT INTO t VALUES (2, 'two')".to_string(),
        "INSERT INTO t VALUES (3, 'three')".to_string(),
        "DELETE FROM t WHERE id % 2 = 1".to_string(),
    ];
    wal_crash_matrix(
        "autockpt",
        Config::default().cache_pages(4),
        &wal_setup(),
        &steps,
        false,
    );
}

/// Leaving and re-entering WAL mode, and VACUUM in WAL mode.
#[cfg(feature = "wal")]
#[test]
fn v16_crash_wal_mode_switches_and_vacuum() {
    let steps = vec![
        "PRAGMA wal_autocheckpoint = 0".to_string(),
        rows_sql("t", 10, 30, 400),
        "PRAGMA journal_mode = DELETE".to_string(),
        "INSERT INTO t VALUES (2, 'two')".to_string(),
        "PRAGMA journal_mode = WAL".to_string(),
        "DELETE FROM t WHERE id % 2 = 0".to_string(),
        "VACUUM".to_string(),
        "INSERT INTO t VALUES (3, 'three')".to_string(),
    ];
    wal_crash_matrix("switch", Config::default(), &wal_setup(), &steps, true);
}

/// The cross-tool case in WAL mode: a hipp process killed at every
/// write boundary of a WAL workload (frames, a checkpoint, a log
/// restart, the checkpoint and log removal at close) leaves files
/// real SQLite recovers to a committed state that passes its
/// integrity check, and hipp reads them the same way.
#[cfg(feature = "wal")]
#[test]
fn v16_cross_tool_real_sqlite_recovers_hipp_wal() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-crash-wal");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let base = dir.join("base.db");
    let (code, err) = run_writer(&base, "wal_setup", None, false);
    assert_eq!(code, 0, "{err}");
    let side = |p: &std::path::Path, s: &str| {
        let mut o = p.as_os_str().to_owned();
        o.push(s);
        std::path::PathBuf::from(o)
    };
    assert!(side(&base, "-wal").exists(), "setup leaves its log");
    let copy = |to: &std::path::Path| {
        std::fs::copy(&base, to).expect("copy");
        std::fs::copy(side(&base, "-wal"), side(to, "-wal")).expect("copy log");
    };
    let probe = dir.join("probe.db");
    copy(&probe);
    let (code, err) = run_writer(&probe, "wal_txn", None, true);
    assert_eq!(code, 0, "count run: {err}");
    assert!(!side(&probe, "-wal").exists(), "close removes the log");
    assert_eq!(
        common::cli(&probe, "SELECT group_concat(id) FROM t;"),
        "1,2,3,4\n"
    );
    let total: usize = err
        .lines()
        .find_map(|l| l.strip_prefix("OPS="))
        .and_then(|v| v.parse().ok())
        .expect("OPS line");
    let states = ["1", "1,2", "1,2,3", "1,2,3,4"];
    for k in 0..total {
        let f = dir.join(format!("w{k}.db"));
        copy(&f);
        let (code, _) = run_writer(&f, "wal_txn", Some(k), false);
        assert_eq!(code, 42, "k={k}: writer should die at the boundary");
        // hipp's view first (on a copy: the pin's close checkpoints).
        let g = dir.join(format!("w{k}-h.db"));
        std::fs::copy(&f, &g).expect("copy");
        if side(&f, "-wal").exists() {
            std::fs::copy(side(&f, "-wal"), side(&g, "-wal")).expect("copy log");
        }
        let hipp_ids = block_on(async {
            let fs = hipp::FileStorage::open(&g).expect("fs");
            let mut db = Connection::open(fs, Config::default())
                .await
                .expect("hipp open");
            let mut ids = Vec::new();
            for id in scan_ids(&mut db).await {
                ids.push(id.to_string());
            }
            ids.join(",")
        });
        assert_eq!(
            common::cli(&f, "PRAGMA integrity_check;").trim(),
            "ok",
            "k={k}"
        );
        let ids = common::cli(&f, "SELECT group_concat(id) FROM t;");
        let ids = ids.trim();
        assert!(states.contains(&ids), "k={k}: ids {ids}");
        assert_eq!(hipp_ids, ids, "k={k}: hipp and the pin recover alike");
    }
}

// ---------------------------------------------------------------
// v0.16 (`attach`): crash injection across a commit of several
// databases. A storage over a shared in-memory file system (every
// database, journal and the super-journal are named files) dies at
// op k; reopening every database recovers them all to the pre- or
// all to the post-image, bytes included — and the same crashed
// files, written to disk, are recovered alike by the real pin.
// ---------------------------------------------------------------

#[cfg(feature = "attach")]
mod multi {
    use super::*;
    use hipp::storage::{AttachTarget, MemSidecar};
    use std::collections::BTreeMap;

    /// The shared file system: database files (each a `MemStorage`,
    /// its journal its sidecar) and stand-alone files (super-journals,
    /// each the journal slot of its own `MemStorage`).
    #[derive(Default)]
    pub(super) struct Fs {
        pub(super) dbs: BTreeMap<Vec<u8>, MemStorage>,
        pub(super) files: BTreeMap<Vec<u8>, MemStorage>,
    }

    #[derive(Clone)]
    pub(super) struct FsStorage {
        name: Vec<u8>,
        mem: MemStorage,
        fs: Rc<RefCell<Fs>>,
        st: Rc<RefCell<CrashState>>,
    }

    impl FsStorage {
        pub(super) fn open(
            fs: &Rc<RefCell<Fs>>,
            st: &Rc<RefCell<CrashState>>,
            name: &[u8],
        ) -> Self {
            let mem = fs
                .borrow_mut()
                .dbs
                .entry(name.to_vec())
                .or_default()
                .clone();
            FsStorage {
                name: name.to_vec(),
                mem,
                fs: fs.clone(),
                st: st.clone(),
            }
        }
    }

    impl Storage for FsStorage {
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
            gate_state(&self.st)?;
            self.mem
                .write_at(offset, buf)
                .await
                .map_err(CrashError::Inner)
        }
        async fn set_len(&mut self, len: u64) -> Result<(), CrashError> {
            gate_state(&self.st)?;
            self.mem.set_len(len).await.map_err(CrashError::Inner)
        }
        async fn sync(&mut self) -> Result<(), CrashError> {
            gate_state(&self.st)?;
            self.mem.sync().await.map_err(CrashError::Inner)
        }
        async fn sidecar(&mut self, kind: Sidecar) -> Result<CrashSidecar, CrashError> {
            Ok(CrashSidecar {
                inner: self.mem.sidecar(kind).await.map_err(CrashError::Inner)?,
                st: self.st.clone(),
            })
        }
        fn file_name(&self) -> &[u8] {
            &self.name
        }
        fn open_attached(&self, target: AttachTarget<'_>) -> Option<Self> {
            match target {
                AttachTarget::File(n) => {
                    // Relative to the main file's directory.
                    let dir = match self.name.iter().rposition(|&b| b == b'/') {
                        Some(i) => &self.name[..=i],
                        None => &[][..],
                    };
                    let mut full = dir.to_vec();
                    full.extend_from_slice(n);
                    Some(FsStorage::open(&self.fs, &self.st, &full))
                }
                _ => None,
            }
        }
        async fn open_named(&mut self, name: &[u8]) -> Result<Option<CrashSidecar>, CrashError> {
            // A database's journal, by its name.
            if let Some(db) = name.strip_suffix(b"-journal") {
                let mem = self.fs.borrow().dbs.get(db).cloned();
                if let Some(mut m) = mem {
                    let inner: MemSidecar = m
                        .sidecar(Sidecar::RollbackJournal)
                        .await
                        .map_err(CrashError::Inner)?;
                    return Ok(Some(CrashSidecar {
                        inner,
                        st: self.st.clone(),
                    }));
                }
            }
            let mut m = self
                .fs
                .borrow_mut()
                .files
                .entry(name.to_vec())
                .or_default()
                .clone();
            let inner = m
                .sidecar(Sidecar::RollbackJournal)
                .await
                .map_err(CrashError::Inner)?;
            Ok(Some(CrashSidecar {
                inner,
                st: self.st.clone(),
            }))
        }
    }

    /// Every file's bytes: databases with their journals, and the
    /// stand-alone files that exist.
    pub(super) type Image = (
        BTreeMap<Vec<u8>, (Vec<u8>, Option<Vec<u8>>)>,
        BTreeMap<Vec<u8>, Vec<u8>>,
    );

    pub(super) fn image(fs: &Rc<RefCell<Fs>>) -> Image {
        let fs = fs.borrow();
        let dbs = fs
            .dbs
            .iter()
            .map(|(k, m)| (k.clone(), (m.snapshot(), m.journal_snapshot())))
            .collect();
        let files = fs
            .files
            .iter()
            .filter_map(|(k, m)| m.journal_snapshot().map(|j| (k.clone(), j)))
            .collect();
        (dbs, files)
    }

    pub(super) fn restore(img: &Image) -> Rc<RefCell<Fs>> {
        let mut fs = Fs::default();
        for (k, (d, j)) in img.0.iter() {
            let m = MemStorage::new();
            m.set_snapshot(d.clone());
            m.set_journal(j.clone());
            fs.dbs.insert(k.clone(), m);
        }
        for (k, b) in img.1.iter() {
            let m = MemStorage::new();
            m.set_journal(Some(b.clone()));
            fs.files.insert(k.clone(), m);
        }
        Rc::new(RefCell::new(fs))
    }

    /// The rows of each database, as one text: `t` in main, `u` in
    /// `a`, `w` in `b`.
    pub(super) async fn contents(db: &mut Connection<FsStorage>) -> String {
        let mut out = String::new();
        for q in [
            "SELECT group_concat(id) FROM t",
            "SELECT group_concat(id) FROM a.u",
            "SELECT group_concat(id) FROM b.w",
        ] {
            let mut st = db.prepare(q).await.expect("prep");
            assert_eq!(st.step().await.expect("step"), Step::Row);
            if let Some(b) = st.column_bytes(0) {
                out.push_str(&String::from_utf8_lossy(b));
            }
            out.push(';');
        }
        out
    }

    pub(super) async fn open_all(
        fs: &Rc<RefCell<Fs>>,
        st: &Rc<RefCell<CrashState>>,
        root: &str,
    ) -> Connection<FsStorage> {
        let main = FsStorage::open(fs, st, format!("{root}/main.db").as_bytes());
        let mut db = Connection::open(main, Config::default())
            .await
            .expect("open main");
        run_all(
            &mut db,
            &strs(&["ATTACH 'a.db' AS a", "ATTACH 'b.db' AS b"]),
        )
        .await;
        db
    }
}

/// Write a crashed multi-database image to real files under `dir`
/// (its names are absolute paths there).
#[cfg(feature = "attach")]
fn image_to_disk(img: &multi::Image) {
    for (k, (d, j)) in img.0.iter() {
        let p = std::path::PathBuf::from(String::from_utf8_lossy(k).to_string());
        std::fs::write(&p, d).expect("write db");
        let mut jp = p.clone().into_os_string();
        jp.push("-journal");
        match j {
            Some(j) => std::fs::write(&jp, j).expect("write journal"),
            None => {
                let _ = std::fs::remove_file(&jp);
            }
        }
    }
    for (k, b) in img.1.iter() {
        let p = std::path::PathBuf::from(String::from_utf8_lossy(k).to_string());
        std::fs::write(&p, b).expect("write file");
    }
}

#[cfg(feature = "attach")]
fn multi_crash_matrix(tag: &str, txn: &[&str]) {
    use multi::*;
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-crash-mj-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let root = dir.to_string_lossy().to_string();
    let txn = strs(txn);
    block_on(async {
        let st = Rc::new(RefCell::new(CrashState::default()));
        let fs = Rc::new(RefCell::new(Fs::default()));
        {
            let mut db = open_all(&fs, &st, &root).await;
            run_all(
                &mut db,
                &strs(&[
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)",
                    "INSERT INTO t VALUES (1, 'one')",
                    "CREATE TABLE a.u (id INTEGER PRIMARY KEY, x TEXT)",
                    "INSERT INTO a.u VALUES (1, 'one')",
                    "CREATE TABLE b.w (id INTEGER PRIMARY KEY, x TEXT)",
                    "INSERT INTO b.w VALUES (1, 'one')",
                ]),
            )
            .await;
        }
        let pre = image(&fs);
        let pre_rows = {
            let fs = restore(&pre);
            let mut db = open_all(&fs, &st, &root).await;
            contents(&mut db).await
        };
        // The post-image, and the commit's op count.
        let fs = restore(&pre);
        let total = {
            let mut db = open_all(&fs, &st, &root).await;
            let before = st.borrow().ops;
            run_all(&mut db, &txn).await;
            st.borrow().ops - before
        };
        let post = image(&fs);
        assert!(post.1.is_empty(), "{tag}: the super-journal is gone");
        assert!(
            post.0.values().all(|(_, j)| j.is_none()),
            "{tag}: every journal is gone"
        );
        let post_rows = {
            let fs = restore(&post);
            let mut db = open_all(&fs, &st, &root).await;
            contents(&mut db).await
        };
        assert_ne!(pre_rows, post_rows, "{tag}: the transaction changes rows");
        let (mut saw_pre, mut saw_post, mut saw_super) = (false, false, false);
        for k in 0..total {
            let fs = restore(&pre);
            {
                let mut db = open_all(&fs, &st, &root).await;
                let at = st.borrow().ops + k;
                st.borrow_mut().kill_at = Some(at);
                run_all(&mut db, &txn).await;
            }
            // The process dies here: nothing is cleaned up.
            st.borrow_mut().kill_at = None;
            st.borrow_mut().dead = false;
            let crashed = image(&fs);
            saw_super |= !crashed.1.is_empty();
            // hipp recovers every database to one side.
            let rows = {
                let fs = restore(&crashed);
                let mut db = open_all(&fs, &st, &root).await;
                let r = contents(&mut db).await;
                drop(db);
                let rec = image(&fs);
                if r == pre_rows {
                    assert_eq!(
                        rec.0.iter().map(|(k, v)| (k, &v.0)).collect::<Vec<_>>(),
                        pre.0.iter().map(|(k, v)| (k, &v.0)).collect::<Vec<_>>(),
                        "{tag} k={k}: pre-image bytes"
                    );
                } else {
                    assert_eq!(
                        rec.0.iter().map(|(k, v)| (k, &v.0)).collect::<Vec<_>>(),
                        post.0.iter().map(|(k, v)| (k, &v.0)).collect::<Vec<_>>(),
                        "{tag} k={k}: post-image bytes"
                    );
                }
                assert!(
                    rec.0.values().all(|(_, j)| j.is_none()),
                    "{tag} k={k}: journals consumed"
                );
                // (A super-journal no journal points at yet — a crash
                // while it was being written — stays behind, as the
                // pin leaves it.)
                r
            };
            if rows == pre_rows {
                saw_pre = true;
            } else if rows == post_rows {
                saw_post = true;
            } else {
                panic!("{tag} k={k}/{total}: torn: {rows} (pre {pre_rows}, post {post_rows})");
            }
            // The real pin recovers the same crashed files alike.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            image_to_disk(&crashed);
            let main = dir.join("main.db");
            let out = common::cli(
                &main,
                &format!(
                    "ATTACH '{root}/a.db' AS a; ATTACH '{root}/b.db' AS b; \
                     SELECT group_concat(id) FROM t; SELECT group_concat(id) FROM a.u; \
                     SELECT group_concat(id) FROM b.w; PRAGMA integrity_check; \
                     PRAGMA a.integrity_check; PRAGMA b.integrity_check;"
                ),
            );
            let ora: Vec<&str> = out.lines().collect();
            let ora_rows = format!(
                "{};{};{};",
                ora.first().unwrap_or(&""),
                ora.get(1).unwrap_or(&""),
                ora.get(2).unwrap_or(&"")
            );
            assert_eq!(ora_rows, rows, "{tag} k={k}: the pin recovers alike");
            assert_eq!(&ora[3..], &["ok", "ok", "ok"], "{tag} k={k}");
        }
        assert!(saw_pre && saw_post, "{tag}: both sides reached");
        assert!(saw_super, "{tag}: some crash left the super-journal");
    });
}

#[cfg(feature = "attach")]
#[test]
fn v16_crash_multi_database_commit() {
    multi_crash_matrix(
        "two",
        &[
            "BEGIN",
            "INSERT INTO t VALUES (2, 'two')",
            "INSERT INTO a.u VALUES (2, 'two')",
            "COMMIT",
        ],
    );
}

#[cfg(feature = "attach")]
#[test]
fn v16_crash_three_databases_and_savepoints() {
    multi_crash_matrix(
        "three",
        &[
            "SAVEPOINT s",
            "INSERT INTO a.u VALUES (2, 'two')",
            "SAVEPOINT q",
            "INSERT INTO b.w SELECT id + 10, x FROM a.u",
            "ROLLBACK TO q",
            "INSERT INTO b.w VALUES (3, printf('%.3000c', 'w'))",
            "UPDATE t SET x = (SELECT group_concat(x) FROM a.u)",
            "RELEASE s",
        ],
    );
}

/// Only the attached databases are written (main's name still names
/// the super-journal).
#[cfg(feature = "attach")]
#[test]
fn v16_crash_attached_only_commit() {
    multi_crash_matrix(
        "att",
        &[
            "BEGIN",
            "INSERT INTO a.u VALUES (2, 'two')",
            "INSERT INTO b.w VALUES (2, 'two')",
            "COMMIT",
        ],
    );
}

/// The other direction (v0.16, `attach`): the real pin killed (by
/// `strace`'s fault injection) on entry to each write, sync, truncate
/// and unlink of a multi-database commit leaves journals and a
/// super-journal that hipp recovers exactly as the pin itself does —
/// all databases to one side. Skipped where `strace` cannot run.
#[cfg(feature = "attach")]
#[test]
fn v16_cross_tool_hipp_recovers_pin_super_journal() {
    use std::process::{Command, Stdio};
    if Command::new("strace")
        .arg("-V")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("strace not available: skipped");
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-crash-pin-mj");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let main = dir.join("main.db");
    let a = dir.join("a.db");
    let setup = format!(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, x); INSERT INTO t VALUES (1, 'one'); \
         ATTACH '{}' AS a; CREATE TABLE a.u(id INTEGER PRIMARY KEY, x); \
         INSERT INTO a.u VALUES (1, 'one');",
        a.display()
    );
    common::cli(&main, &setup);
    let pre = (
        std::fs::read(&main).expect("main"),
        std::fs::read(&a).expect("a"),
    );
    let txn = format!(
        "ATTACH '{}' AS a;\nBEGIN;\nINSERT INTO t VALUES (2, 'two');\n\
         INSERT INTO a.u VALUES (2, printf('%.5000c', 'u'));\nCOMMIT;\n",
        a.display()
    );
    let reset = || {
        for e in std::fs::read_dir(&dir).expect("dir") {
            let _ = std::fs::remove_file(e.expect("entry").path());
        }
        std::fs::write(&main, &pre.0).expect("main");
        std::fs::write(&a, &pre.1).expect("a");
    };
    let snapshot = || -> Vec<(std::path::PathBuf, Vec<u8>)> {
        std::fs::read_dir(&dir)
            .expect("dir")
            .map(|e| {
                let p = e.expect("entry").path();
                let b = std::fs::read(&p).expect("read");
                (p, b)
            })
            .collect()
    };
    let restore = |files: &[(std::path::PathBuf, Vec<u8>)]| {
        for e in std::fs::read_dir(&dir).expect("dir") {
            let _ = std::fs::remove_file(e.expect("entry").path());
        }
        for (p, b) in files {
            std::fs::write(p, b).expect("write");
        }
    };
    let (mut saw_pre, mut saw_post, mut saw_super) = (false, false, false);
    for sys in ["pwrite64", "fsync", "ftruncate", "unlink"] {
        for n in 1.. {
            reset();
            let mut child = Command::new("strace")
                .args(["-f", "-qq", "-o", "/dev/null", "-e"])
                .arg(format!("inject={sys}:signal=KILL:when={n}"))
                .arg(common::oracle())
                .arg(&main)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("strace runs");
            {
                use std::io::Write;
                let _ = child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(txn.as_bytes());
            }
            let status = child.wait().expect("wait");
            if status.success() {
                break; // no n-th such call: this syscall is exhausted
            }
            let crashed = snapshot();
            saw_super |= crashed
                .iter()
                .any(|(p, _)| p.to_string_lossy().contains("-mj"));
            // hipp recovers in place (the names inside are absolute).
            let hipp_rows = block_on(async {
                let fs = hipp::FileStorage::open(&main).expect("fs");
                let mut db = Connection::open(fs, Config::default())
                    .await
                    .expect("hipp open");
                let mut out = String::new();
                for q in [
                    format!("ATTACH '{}' AS a", a.display()),
                    "SELECT group_concat(id) FROM t".to_string(),
                    "SELECT group_concat(id) FROM a.u".to_string(),
                ] {
                    let mut st = db.prepare(&q).await.expect("prep");
                    while st.step().await.expect("step") == Step::Row {
                        out.push_str(&String::from_utf8_lossy(
                            st.column_bytes(0).unwrap_or_default(),
                        ));
                        out.push(';');
                    }
                }
                out
            });
            assert_eq!(
                common::cli(
                    &main,
                    &format!(
                        "ATTACH '{}' AS a; PRAGMA integrity_check; PRAGMA a.integrity_check;",
                        a.display()
                    )
                ),
                "ok\nok\n",
                "{sys}#{n}: hipp's recovery"
            );
            // The pin recovers the same crashed files.
            restore(&crashed);
            let out = common::cli(
                &main,
                &format!(
                    "ATTACH '{}' AS a; SELECT group_concat(id) FROM t; \
                     SELECT group_concat(id) FROM a.u;",
                    a.display()
                ),
            );
            let pin_rows: String = out.lines().map(|l| format!("{l};")).collect();
            assert_eq!(
                hipp_rows, pin_rows,
                "{sys}#{n}: hipp and the pin recover alike"
            );
            match hipp_rows.as_str() {
                "1;1;" => saw_pre = true,
                "1,2;1,2;" => saw_post = true,
                r => panic!("{sys}#{n}: torn recovery {r}"),
            }
        }
    }
    assert!(
        saw_pre && saw_post && saw_super,
        "both sides and a super-journal seen"
    );
}

// ---------------------------------------------------------------
// v0.16 (`autovacuum`): crash injection while an auto-vacuum file
// commits — FULL's page moves into free pages and the truncation
// (`autoVacuumCommit`), a root moved by DROP / CREATE, and PRAGMA
// incremental_vacuum's steps. Every kill point recovers (in hipp)
// to the pre- or post-image byte for byte; the same crashed file
// and journal, handed to the pin, recover to a state its integrity
// check passes with the pre- or the post-transaction rows.
// ---------------------------------------------------------------

/// The pin recovers every kill point of `txn` (after `setup`) to the
/// pre- or post-transaction state, its integrity check passing (the
/// kill points before the journal's deletion roll back).
#[cfg(feature = "autovacuum")]
fn pin_recovers_autovacuum(tag: &str, setup: &[String], txn: &[String]) {
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-crash-av-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let q = "PRAGMA integrity_check; SELECT count(*), total(length(x)) FROM t;";
    block_on(async {
        let mem = MemStorage::new();
        {
            let mut db = Connection::open(mem.clone(), Config::default())
                .await
                .expect("open");
            run_all(&mut db, setup).await;
        }
        let pre = mem.snapshot();
        let write = |name: &str, bytes: &[u8], journal: Option<Vec<u8>>| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).expect("write");
            let j = dir.join(format!("{name}-journal"));
            let _ = std::fs::remove_file(&j);
            if let Some(jb) = journal {
                std::fs::write(&j, jb).expect("journal");
            }
            p
        };
        let pre_rows = common::cli(&write("pre.db", &pre, None), q);
        let post_mem = MemStorage::new();
        post_mem.set_snapshot(pre.clone());
        {
            let mut db = Connection::open(post_mem.clone(), Config::default())
                .await
                .expect("open");
            run_all(&mut db, txn).await;
        }
        let post_rows = common::cli(&write("post.db", &post_mem.snapshot(), None), q);
        assert!(
            pre_rows.starts_with("ok\n") && post_rows.starts_with("ok\n"),
            "{tag}"
        );
        let total = {
            let m = MemStorage::new();
            m.set_snapshot(pre.clone());
            let cs = crash_storage(m);
            let mut db = Connection::open(cs.clone(), Config::default())
                .await
                .expect("open");
            let before = cs.ops();
            run_all(&mut db, txn).await;
            drop(db);
            cs.ops() - before
        };
        // The commit point is the journal's deletion, the last op:
        // every kill before it rolls back.
        let mut saw_pre = false;
        for k in 0..total {
            let m = MemStorage::new();
            m.set_snapshot(pre.clone());
            let cs = crash_storage(m.clone());
            {
                let mut db = Connection::open(cs.clone(), Config::default())
                    .await
                    .expect("open");
                cs.st.borrow_mut().kill_at = Some(cs.ops() + k);
                run_all(&mut db, txn).await;
            }
            let f = write(&format!("k{k}.db"), &m.snapshot(), m.journal_snapshot());
            let rows = common::cli(&f, q);
            if rows == pre_rows {
                saw_pre = true;
            } else if rows != post_rows {
                panic!("{tag} k={k}/{total}: the pin recovered {rows:?}");
            }
        }
        assert!(saw_pre, "{tag}: a crash before the commit point restores");
    });
}

#[cfg(feature = "autovacuum")]
#[test]
fn v16_crash_autovacuum_full() {
    let setup = vec![
        "PRAGMA auto_vacuum = FULL".to_string(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)".to_string(),
        "CREATE INDEX tx ON t(x)".to_string(),
        "CREATE TABLE u (y)".to_string(),
        rows_sql("t", 1, 150, 120),
        "INSERT INTO u VALUES (zeroblob(9000))".to_string(),
    ];
    // Pages from the end moved into the free pages, then truncated.
    let txn = strs(&["DELETE FROM t WHERE id % 3 = 0"]);
    crash_matrix("av-full-delete", Config::default(), &setup, &txn);
    pin_recovers_autovacuum("full-delete", &setup, &txn);
    // A transaction that frees, reuses and frees again.
    let txn = vec![
        "BEGIN".to_string(),
        "DELETE FROM t WHERE id > 40".to_string(),
        rows_sql("t", 1000, 30, 100),
        "DELETE FROM u".to_string(),
        "COMMIT".to_string(),
    ];
    crash_matrix("av-full-txn", Config::default(), &setup, &txn);
    pin_recovers_autovacuum("full-txn", &setup, &txn);
    // DROP INDEX moves the largest root (u's) into its slot; CREATE
    // moves the page after the largest root out of the way.
    let txn = strs(&["DROP INDEX tx"]);
    crash_matrix("av-full-drop", Config::default(), &setup, &txn);
    pin_recovers_autovacuum("full-drop", &setup, &txn);
    let txn = strs(&["CREATE TABLE v (z)"]);
    crash_matrix("av-full-create", Config::default(), &setup, &txn);
}

#[cfg(feature = "autovacuum")]
#[test]
fn v16_crash_incremental_vacuum() {
    let setup = vec![
        "PRAGMA auto_vacuum = INCREMENTAL".to_string(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)".to_string(),
        "CREATE INDEX tx ON t(x)".to_string(),
        rows_sql("t", 1, 200, 120),
        "DELETE FROM t WHERE id % 2 = 0".to_string(),
    ];
    for (tag, txn) in [
        ("inc-7", strs(&["PRAGMA incremental_vacuum(7)"])),
        ("inc-all", strs(&["PRAGMA incremental_vacuum"])),
        (
            "inc-txn",
            vec![
                "BEGIN".to_string(),
                "PRAGMA incremental_vacuum(3)".to_string(),
                rows_sql("t", 1000, 20, 100),
                "PRAGMA incremental_vacuum(2)".to_string(),
                "COMMIT".to_string(),
            ],
        ),
    ] {
        crash_matrix(&format!("av-{tag}"), Config::default(), &setup, &txn);
        pin_recovers_autovacuum(tag, &setup, &txn);
    }
}
