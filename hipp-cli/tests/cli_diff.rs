//! hipp-cli differential tests vs the pinned SQLite 3.54.1
//! oracle CLI (SPEC §9.4, D1/D4): the CLI's first job is being a
//! differential-testing tool, so its list-format output must be
//! byte-identical to the oracle's for the shared SQL subset.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn hipp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_hipp")
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .output()
        .expect("binary runs")
}

fn run_stdin(bin: &str, db: &str, script: &str) -> Output {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    child.wait_with_output().expect("wait")
}

/// The shared-subset corpus: DDL, multi-row VALUES, scans,
/// WHERE rowid= hit/miss, explicit transactions (commit and
/// rollback visibility), value rendering across all types.
const CORPUS: &str = "\
CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB);
INSERT INTO t VALUES (1, 'one', 1.5, x'00ff'), (2, 'two', 0.1, NULL), (3, 'it''s', 100.0, x''), (4, '', -0.25, x'deadbeef'), (5, 'five', 3.14159265358979, NULL), (6, 'six', 123456789.25, x'00'), (7, 'seven', 0.00001, NULL), (8, 'eight', 2.0, NULL);
SELECT id, name, v, b FROM t;
SELECT name, v FROM t WHERE rowid = 3;
SELECT id FROM t WHERE id = 999;
SELECT id FROM t WHERE id = '5';
BEGIN;
INSERT INTO t VALUES (9, 'nine', 9.75, NULL), (10, 'ten', NULL, NULL);
COMMIT;
SELECT id, name FROM t WHERE rowid = 10;
BEGIN;
INSERT INTO t VALUES (11, 'gone', 0.0, NULL);
ROLLBACK;
SELECT id FROM t WHERE rowid = 11;
SELECT id, name, v FROM t;
INSERT INTO t VALUES (20, 'ctrl', NULL, x'0001090a0d1b1f7f80ff');
SELECT b FROM t WHERE rowid = 20;
";

#[test]
fn oneshot_output_byte_identical_to_oracle() {
    let hdb = temp_db("oneshot-h");
    let odb = temp_db("oneshot-o");
    let hout = run(hipp_bin(), &[hdb.to_str().unwrap(), CORPUS]);
    let oout = run(
        oracle().to_str().unwrap(),
        &[odb.to_str().unwrap(), CORPUS],
    );
    assert!(
        hout.status.success(),
        "hipp failed: {}",
        String::from_utf8_lossy(&hout.stderr)
    );
    assert!(
        oout.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&oout.stderr)
    );
    assert_eq!(
        hout.stdout,
        oout.stdout,
        "hipp:\n{}\noracle:\n{}",
        String::from_utf8_lossy(&hout.stdout),
        String::from_utf8_lossy(&oout.stdout)
    );
}

#[test]
fn stdin_batch_output_byte_identical_to_oracle() {
    let hdb = temp_db("stdin-h");
    let odb = temp_db("stdin-o");
    let hout = run_stdin(hipp_bin(), hdb.to_str().unwrap(), CORPUS);
    let oout = run_stdin(oracle().to_str().unwrap(), odb.to_str().unwrap(), CORPUS);
    assert!(
        hout.status.success(),
        "hipp failed: {}",
        String::from_utf8_lossy(&hout.stderr)
    );
    assert!(oout.status.success());
    assert_eq!(hout.stdout, oout.stdout);
}

#[test]
fn dot_tables_and_schema() {
    let db = temp_db("dots");
    let setup = run(
        hipp_bin(),
        &[
            db.to_str().unwrap(),
            "CREATE TABLE alpha (id INTEGER PRIMARY KEY, x TEXT); CREATE TABLE beta (v REAL);",
        ],
    );
    assert!(setup.status.success());
    let tables = run(hipp_bin(), &[db.to_str().unwrap(), ".tables"]);
    assert_eq!(String::from_utf8_lossy(&tables.stdout), "alpha\nbeta\n");
    let schema = run(hipp_bin(), &[db.to_str().unwrap(), ".schema"]);
    assert_eq!(
        String::from_utf8_lossy(&schema.stdout),
        "CREATE TABLE alpha (id INTEGER PRIMARY KEY, x TEXT);\nCREATE TABLE beta (v REAL);\n"
    );
    let one = run(hipp_bin(), &[db.to_str().unwrap(), ".schema beta"]);
    assert_eq!(
        String::from_utf8_lossy(&one.stdout),
        "CREATE TABLE beta (v REAL);\n"
    );
}

#[test]
fn dot_dump_reimports_into_oracle() {
    let hdb = temp_db("dump-h");
    let setup = run(
        hipp_bin(),
        &[
            hdb.to_str().unwrap(),
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB); \
             INSERT INTO t VALUES (1, 'o''clock', 1.5, x'00ff'), (2, NULL, 3.14159265358979, x''), (3, 'three', NULL, NULL);",
        ],
    );
    assert!(setup.status.success());
    let dump = run(hipp_bin(), &[hdb.to_str().unwrap(), ".dump"]);
    assert!(dump.status.success());
    let dump_sql = String::from_utf8_lossy(&dump.stdout).into_owned();
    assert!(dump_sql.starts_with("BEGIN TRANSACTION;\n"), "{dump_sql}");
    assert!(dump_sql.ends_with("COMMIT;\n"), "{dump_sql}");

    // Feed hipp's dump to the oracle on a fresh file; the oracle's
    // rendering must equal hipp's rendering of the original.
    let odb = temp_db("dump-o");
    let imp = run(oracle().to_str().unwrap(), &[odb.to_str().unwrap(), &dump_sql]);
    assert!(
        imp.status.success(),
        "oracle rejected hipp dump: {}",
        String::from_utf8_lossy(&imp.stderr)
    );
    let q = "SELECT id, name, v, b FROM t;";
    let from_oracle = run(oracle().to_str().unwrap(), &[odb.to_str().unwrap(), q]);
    let from_hipp = run(hipp_bin(), &[hdb.to_str().unwrap(), q]);
    assert_eq!(from_oracle.stdout, from_hipp.stdout);
}

#[test]
fn dot_open_switches_database() {
    let a = temp_db("open-a");
    let b = temp_db("open-b");
    let script = format!(
        "CREATE TABLE ina (id INTEGER PRIMARY KEY);\nINSERT INTO ina VALUES (1);\n.open {}\nCREATE TABLE inb (id INTEGER PRIMARY KEY);\nINSERT INTO inb VALUES (2);\n.exit\n",
        b.display()
    );
    let out = run_stdin(hipp_bin(), a.to_str().unwrap(), &script);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ob = oracle();
    let ra = run(ob.to_str().unwrap(), &[a.to_str().unwrap(), "SELECT id FROM ina;"]);
    assert_eq!(String::from_utf8_lossy(&ra.stdout), "1\n");
    let rb = run(ob.to_str().unwrap(), &[b.to_str().unwrap(), "SELECT id FROM inb;"]);
    assert_eq!(String::from_utf8_lossy(&rb.stdout), "2\n");
}

#[test]
fn memory_database_oneshot() {
    let out = run(
        hipp_bin(),
        &[
            ":memory:",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT); INSERT INTO t VALUES (1, 'a'), (2, 'b'); SELECT x FROM t WHERE rowid = 2;",
        ],
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b\n");
}

#[test]
fn errors_exit_nonzero_with_stderr() {
    let db = temp_db("errs");
    for sql in [
        "SELECT id FROM missing;",
        "INSERT INTO missing VALUES (1);",
        "SELECT id FROM t WHERE x > 1;",
        "DROP TABLE t;",
    ] {
        let out = run(hipp_bin(), &[db.to_str().unwrap(), sql]);
        assert!(!out.status.success(), "{sql} unexpectedly succeeded");
        assert!(
            !out.stderr.is_empty(),
            "{sql} failed silently (no stderr)"
        );
    }
}
