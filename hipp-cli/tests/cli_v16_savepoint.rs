//! hipp-cli v0.16 differentials: savepoints and the maintenance
//! statements must print byte-identically to the pinned oracle —
//! rows read inside SAVEPOINT / ROLLBACK TO / RELEASE nests and
//! after statement-level rollbacks, PRAGMA values around VACUUM
//! (schema version, page size, freelist), `sqlite_stat1` rows from
//! ANALYZE, and REINDEX. Each batch runs one-shot and over stdin
//! (the shell continues past errors there); hipp-written files
//! must pass the oracle's integrity check.

use std::path::{Path, PathBuf};
use std::process::Command;

fn hipp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hipp"))
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16sp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> (bool, Vec<u8>) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (out.status.success(), out.stdout)
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("cli finishes").stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let (hok, got) = run(&hipp_bin(), &a, batch);
    let (ook, want) = run(&oracle(), &b, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{tag}"
    );
    assert_eq!(hok, ook, "{tag}");
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    if hok {
        assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;").1, b"ok\n");
    }
}

const SAVEPOINTS: &str = "\
CREATE TABLE t(a PRIMARY KEY, b);
SAVEPOINT a;
INSERT INTO t VALUES (1, 1.5), (2, 'two');
SAVEPOINT b;
UPDATE t SET b = b * 2;
SELECT * FROM t;
ROLLBACK TO b;
SELECT * FROM t;
INSERT INTO t VALUES (3, -0.25);
RELEASE a;
SELECT *, typeof(b) FROM t;
BEGIN;
SAVEPOINT \"x y\";
DELETE FROM t WHERE a > 1;
RELEASE \"X Y\";
SELECT count(*), changes(), total_changes() FROM t;
ROLLBACK;
SELECT a, b FROM t ORDER BY a DESC;
SAVEPOINT s;
PRAGMA user_version = 4;
PRAGMA user_version;
ROLLBACK TRANSACTION TO SAVEPOINT s;
PRAGMA user_version;
END;
SELECT last_insert_rowid();
";

const STATEMENT_ABORT: &str = "\
CREATE TABLE t(a PRIMARY KEY, b NOT NULL);
BEGIN;
INSERT INTO t VALUES (1, 'x');
SELECT 'started', count(*) FROM t;
INSERT INTO t VALUES (2, 'y'), (1, 'dup');
SELECT * FROM t;
INSERT INTO t VALUES (3, NULL);
INSERT INTO t VALUES (4, 'w');
COMMIT;
SELECT * FROM t;
";

const MAINT: &str = "\
CREATE TABLE t(a, b COLLATE NOCASE, c REAL);
CREATE INDEX tb ON t(b);
CREATE INDEX tac ON t(a, c DESC);
CREATE TABLE u(k INTEGER PRIMARY KEY, v UNIQUE);
CREATE VIEW vw AS SELECT a, c / 2 AS h FROM t;
INSERT INTO t VALUES (1, 'a', 0.5), (1, 'A', 1e20), (2, 'b', NULL), (3, 'c', -1.25), (3, 'C', 2.0);
INSERT INTO u VALUES (1, 'p'), (2, 'q'), (7, 'r');
DELETE FROM t WHERE c IS NULL;
PRAGMA schema_version;
VACUUM;
PRAGMA schema_version;
PRAGMA freelist_count;
SELECT rowid, * FROM t;
SELECT * FROM vw ORDER BY a, h;
REINDEX;
REINDEX nocase;
REINDEX main.tb;
ANALYZE;
SELECT * FROM sqlite_stat1 ORDER BY tbl, idx;
ANALYZE tac;
SELECT * FROM sqlite_stat1 ORDER BY tbl, idx;
PRAGMA page_size = 1024;
PRAGMA page_size;
VACUUM;
PRAGMA page_size;
SELECT type, name, tbl_name FROM sqlite_schema;
";

#[test]
fn savepoints_byte_identical() {
    byte_identical("savepoints", SAVEPOINTS);
}

#[test]
fn statement_abort_byte_identical() {
    // One-shot stops at the first error on both; stdin continues.
    byte_identical("stmt-abort", STATEMENT_ABORT);
}

#[test]
fn maintenance_byte_identical() {
    byte_identical("maint", MAINT);
}

/// Review regressions: OR ROLLBACK ends the transaction (the
/// later COMMIT fails on both), ROLLBACK TO keeps a new page size,
/// DROP clears sqlite_stat1, VACUUM INTO takes an expression.
const REVIEW_FIXES: &str = "\
SAVEPOINT a;
PRAGMA page_size = 1024;
ROLLBACK TO a;
CREATE TABLE t(a UNIQUE, b);
RELEASE a;
PRAGMA page_size;
INSERT INTO t VALUES (1, 'one');
BEGIN;
INSERT INTO t VALUES (9, 'nine');
INSERT OR ROLLBACK INTO t VALUES (1, 'dup');
SELECT * FROM t;
COMMIT;
CREATE INDEX tb ON t(b);
CREATE TABLE u(x);
INSERT INTO u VALUES (1), (2);
ANALYZE;
SELECT * FROM sqlite_stat1 ORDER BY tbl, idx;
DROP INDEX tb;
DROP TABLE u;
SELECT * FROM sqlite_stat1 ORDER BY tbl, idx;
VACUUM INTO '' || '';
SELECT count(*) FROM t;
";

#[test]
fn review_fixes_byte_identical() {
    byte_identical("review", REVIEW_FIXES);
}
