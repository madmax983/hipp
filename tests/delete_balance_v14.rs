//! v0.14 differentials: delete merge/redistribution.
//!
//! DELETE (and the delete side of UPDATE / REPLACE / upserts)
//! used to remove cells without rebalancing, leaving underflow
//! pages in place; the pin redistributes a window of up to
//! three siblings once a page's free space passes two thirds of
//! usable, merges when the window's cells fit fewer pages, and
//! collapses a one-child root. These tests pin the observable
//! file shape — `page_count` and `freelist_count` after EVERY
//! statement, plus final contents and `integrity_check` —
//! against the pinned 3.54.1 oracle.
//!
//! Both twins start from an ORACLE-built file (identical bytes):
//! hipp's insert-side split distribution is a separate,
//! pre-existing divergence (insert-only workloads at small page
//! sizes produce differently-filled leaves), so delete shape is
//! pinned in the direction that isolates it — hipp deleting on
//! a pin-shaped tree. The reverse direction (each engine builds
//! and deletes its own file) is asserted on contents and
//! integrity only.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str)
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
}

/// (page_count, freelist_count) read straight from the file:
/// page_count is the file length over the page size, and the
/// freelist count is header field 36.
fn shape_of(path: &std::path::Path) -> (u32, u32) {
    let bytes = std::fs::read(path).expect("read db file");
    let page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
    let page_count = (bytes.len() / page_size) as u32;
    let freelist = u32::from_be_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    (page_count, freelist)
}

fn oracle_trace(
    tag: &str,
    setup: &str,
    statements: &[String],
) -> (Vec<(u32, u32)>, std::path::PathBuf) {
    let path = common::temp_db(&format!("v14-{tag}-oracle"));
    common::cli(&path, setup);
    let mut trace = vec![shape_of(&path)];
    for s in statements {
        common::cli(&path, s);
        trace.push(shape_of(&path));
    }
    (trace, path)
}

fn hipp_trace(
    tag: &str,
    setup: &str,
    statements: &[String],
) -> (Vec<(u32, u32)>, std::path::PathBuf, Connection<FileStorage>) {
    let path = common::temp_db(&format!("v14-{tag}-hipp"));
    common::cli(&path, setup);
    let mut trace = vec![shape_of(&path)];
    let fs = FileStorage::open(&path).expect("fs");
    let db = block_on(async {
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for s in statements {
            exec(&mut db, s).await;
            trace.push(shape_of(&path));
        }
        db
    });
    (trace, path, db)
}

/// The full twin assertion: identical shape trace at every
/// step, identical final contents, oracle-clean integrity on
/// the hipp file.
fn assert_twins(tag: &str, setup: &str, table: &str, statements: Vec<String>) {
    let (otrace, opath) = oracle_trace(tag, setup, &statements);
    let (htrace, hpath, _db) = hipp_trace(tag, setup, &statements);
    assert_eq!(otrace.len(), htrace.len(), "{tag}: trace lengths differ");
    for (i, (o, h)) in otrace.iter().zip(htrace.iter()).enumerate() {
        assert_eq!(
            o,
            h,
            "{tag}: shape mismatch at step {i} (after {:?}): oracle (page_count, freelist) = {o:?}, hipp = {h:?}",
            if i == 0 { "setup" } else { &statements[i - 1] }
        );
    }
    let select = format!("SELECT * FROM {table} ORDER BY 1;");
    assert_eq!(
        common::cli(&opath, &select),
        common::cli(&hpath, &select),
        "{tag}: final contents differ"
    );
    assert_eq!(
        common::cli(&hpath, "PRAGMA integrity_check;").trim(),
        "ok",
        "{tag}: integrity_check on the hipp file"
    );
}

const TEXT100: &str = "PRAGMA page_size=512; CREATE TABLE t (x TEXT); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<100) INSERT INTO t SELECT printf('r%03dxxxxxx', n) FROM c;";

fn one_by_one(range: std::ops::RangeInclusive<i64>) -> Vec<String> {
    range
        .rev()
        .map(|r| format!("DELETE FROM t WHERE rowid={r};"))
        .collect()
}

#[test]
fn v14_tail_deletes_redistribute_then_merge() {
    assert_twins("tail", TEXT100, "t", one_by_one(61..=100));
}

#[test]
fn v14_middle_deletes() {
    assert_twins("middle", TEXT100, "t", one_by_one(32..=62));
}

#[test]
fn v14_scattered_deletes() {
    let mut stmts: Vec<String> = (1..=100)
        .filter(|r| r % 2 == 1)
        .map(|r| format!("DELETE FROM t WHERE rowid={r};"))
        .collect();
    stmts.extend(
        (1..=100)
            .filter(|r| r % 2 == 0)
            .map(|r| format!("DELETE FROM t WHERE rowid={r};")),
    );
    assert_twins("scatter", TEXT100, "t", stmts);
}

#[test]
fn v14_delete_all_collapses_and_reinsert_reuses_freelist() {
    let mut stmts = one_by_one(1..=100);
    // Small reinsert: fits the surviving root leaf, drawn from
    // the freelist identically in both engines.
    for n in 1..=20 {
        stmts.push(format!("INSERT INTO t VALUES ('r{n:03}xxxxxx');"));
    }
    stmts.extend(one_by_one(1..=20));
    assert_twins("collapse", TEXT100, "t", stmts);
}

#[test]
fn v14_batch_deletes() {
    let stmts: Vec<String> = vec![
        "DELETE FROM t WHERE rowid > 90;".into(),
        "DELETE FROM t WHERE rowid > 50;".into(),
        "DELETE FROM t WHERE rowid BETWEEN 10 AND 30;".into(),
        "DELETE FROM t;".into(),
    ];
    assert_twins("batch", TEXT100, "t", stmts);
}

#[test]
fn v14_overflow_row_deletes() {
    let setup = "PRAGMA page_size=512; CREATE TABLE t (x TEXT); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<60) INSERT INTO t SELECT printf('r%03d', n) || zeroblob(900) FROM c;";
    assert_twins("overflow", setup, "t", one_by_one(21..=60));
}

#[test]
fn v14_ps4096_tail_deletes() {
    let setup = "PRAGMA page_size=4096; CREATE TABLE t (x TEXT); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<600) INSERT INTO t SELECT printf('r%03dxxxxxx', n) FROM c;";
    assert_twins("ps4096", setup, "t", one_by_one(301..=600));
}

#[test]
fn v14_deep_tree_interior_cascade() {
    // Depth-3 table tree: 3000 rows at 512-byte pages. Tail
    // deletes cascade merges up through two interior levels
    // and shrink the tree height; the second range hollows out
    // the middle.
    let setup = "PRAGMA page_size=512; CREATE TABLE t (x INTEGER); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3000) INSERT INTO t SELECT n FROM c;";
    let mut stmts = one_by_one(2001..=3000);
    stmts.extend(one_by_one(1001..=1500));
    assert_twins("deep", setup, "t", stmts);
}

#[test]
fn v14_index_deletes_balance() {
    let setup = "PRAGMA page_size=512; CREATE TABLE t (x TEXT); CREATE INDEX ix ON t(x); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<100) INSERT INTO t SELECT printf('r%03dxxxxxx', n) FROM c;";
    assert_twins("index", setup, "t", one_by_one(61..=100));
}

#[test]
fn v14_deep_index_deletes() {
    // Depth-3 index (400 entries at 512-byte pages): deleting
    // every second row removes leaf entries and
    // interior-resident entries (predecessor replacement) and
    // merges index pages on both levels.
    let setup = "PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER); CREATE INDEX ix_k ON t(k); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<400) INSERT INTO t SELECT n, printf('key-%04d', n), n FROM c;";
    let stmts: Vec<String> = (2..=400)
        .step_by(2)
        .map(|n| format!("DELETE FROM t WHERE id={n};"))
        .collect();
    assert_twins("deep-index", setup, "t", stmts);
}

/// UPDATE and REPLACE reach the same delete machinery through
/// the session layer (delete + insert per victim). The insert
/// side makes per-step shape non-comparable, so this workload
/// asserts contents and integrity on an oracle-built file
/// after update/replace/upsert churn.
#[test]
fn v14_update_replace_victims() {
    block_on(async {
        let path = common::temp_db("v14-update");
        common::cli(
            &path,
            "PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT UNIQUE, v INTEGER); CREATE INDEX ix_v ON t(v); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<300) INSERT INTO t SELECT n, printf('key-%04d', n), n FROM c;",
        );
        let fs = FileStorage::open(&path).expect("fs");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for sql in [
            "UPDATE t SET v = v + 1000 WHERE id % 2 = 0",
            "REPLACE INTO t (id, k, v) VALUES (5, 'key-0007', 777)",
            "INSERT INTO t (id, k, v) VALUES (9, 'key-0009', 999) ON CONFLICT(id) DO UPDATE SET v = excluded.v",
            "UPDATE t SET k = 'gone-' || id WHERE id > 250",
            "DELETE FROM t WHERE id > 200",
        ] {
            exec(&mut db, sql).await;
        }
        drop(db);
        let oracle_path = common::temp_db("v14-update-oracle");
        common::cli(
            &oracle_path,
            "PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT UNIQUE, v INTEGER); CREATE INDEX ix_v ON t(v); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<300) INSERT INTO t SELECT n, printf('key-%04d', n), n FROM c;",
        );
        for sql in [
            "UPDATE t SET v = v + 1000 WHERE id % 2 = 0",
            "REPLACE INTO t (id, k, v) VALUES (5, 'key-0007', 777)",
            "INSERT INTO t (id, k, v) VALUES (9, 'key-0009', 999) ON CONFLICT(id) DO UPDATE SET v = excluded.v",
            "UPDATE t SET k = 'gone-' || id WHERE id > 250",
            "DELETE FROM t WHERE id > 200",
        ] {
            common::cli(&oracle_path, sql);
        }
        assert_eq!(
            common::cli(&oracle_path, "SELECT * FROM t ORDER BY id;"),
            common::cli(&path, "SELECT * FROM t ORDER BY id;"),
            "update/replace churn: contents differ"
        );
        assert_eq!(
            common::cli(&path, "PRAGMA integrity_check;").trim(),
            "ok",
            "update/replace churn: integrity_check on the hipp file"
        );
    });
}

/// Reverse direction: hipp builds its own file and deletes;
/// the oracle does the same on its own file. Contents and
/// integrity must agree (shape is not asserted: the insert-side
/// split distribution divergence predates v0.14).
#[test]
fn v14_hipp_built_direction() {
    block_on(async {
        let path = common::temp_db("v14-own");
        let fs = FileStorage::open(&path).expect("fs");
        let mut db = Connection::open(fs, Config::default().page_size(512))
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (x TEXT)").await;
        let mut values = String::new();
        for n in 1..=200 {
            if n > 1 {
                values.push(',');
            }
            values.push_str(&format!("('r{n:03}xxxxxx')"));
        }
        exec(&mut db, &format!("INSERT INTO t VALUES {values}")).await;
        exec(&mut db, "DELETE FROM t WHERE rowid > 40").await;
        exec(&mut db, "DELETE FROM t WHERE rowid % 3 = 0").await;
        drop(db);
        let opath = common::temp_db("v14-own-oracle");
        common::cli(
            &opath,
            "PRAGMA page_size=512; CREATE TABLE t (x TEXT); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<200) INSERT INTO t SELECT printf('r%03dxxxxxx', n) FROM c; DELETE FROM t WHERE rowid > 40; DELETE FROM t WHERE rowid % 3 = 0;",
        );
        assert_eq!(
            common::cli(&opath, "SELECT * FROM t ORDER BY rowid;"),
            common::cli(&path, "SELECT * FROM t ORDER BY rowid;"),
            "hipp-built direction: contents differ"
        );
        assert_eq!(
            common::cli(&path, "PRAGMA integrity_check;").trim(),
            "ok",
            "hipp-built direction: integrity_check"
        );
    });
}
