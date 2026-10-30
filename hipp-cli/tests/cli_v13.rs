//! hipp-cli v0.13 differentials: uncorrelated-subquery
//! hoisting must leave every output byte-identical to the
//! pinned oracle — including the write-side snapshot cases,
//! where the pin fixes an uncorrelated subquery's value for
//! the whole statement (an UPDATE whose SET subquery reads
//! the written table sees the pre-statement sum on all rows).

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v13-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// Read-side corpus: uncorrelated scalar / IN / EXISTS in
/// every position, plus correlated controls. One batch,
/// byte-compared one-shot and over stdin.
const READS: &str = "\
CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT, budget INTEGER);
CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, dept_id INTEGER, salary INTEGER);
INSERT INTO dept VALUES (1,'eng',1000),(2,'ops',400),(3,'empty',50);
INSERT INTO emp VALUES (1,'amy',1,300),(2,'bob',1,200),(3,'cy',2,150),(4,'dee',NULL,100),(5,'eli',2,150);
SELECT name, (SELECT max(salary) FROM emp) FROM emp ORDER BY id;
SELECT name FROM emp WHERE salary >= (SELECT avg(salary) FROM emp) ORDER BY id;
SELECT (SELECT name FROM dept WHERE id = 99), id FROM emp ORDER BY id;
SELECT name FROM emp WHERE dept_id IN (SELECT dept_id FROM emp) ORDER BY id;
SELECT EXISTS (SELECT 1 FROM emp WHERE salary > 1000), id FROM dept ORDER BY id;
SELECT sum((SELECT max(salary) FROM emp)) FROM emp;
SELECT d.name, (SELECT count(*) FROM emp e WHERE e.dept_id = d.id) FROM dept d ORDER BY d.id;
";

/// Write-side snapshot corpus: each statement's uncorrelated
/// subquery reads a table the statement itself writes.
const WRITES: &str = "\
CREATE TABLE t (a INTEGER);
INSERT INTO t VALUES (1),(2),(3),(4);
UPDATE t SET a = (SELECT sum(a) FROM t);
SELECT a FROM t ORDER BY rowid;
DELETE FROM t WHERE a = (SELECT min(a) FROM t);
SELECT a FROM t ORDER BY a;
INSERT INTO t(a) VALUES ((SELECT max(a) FROM t) + 1), ((SELECT max(a) FROM t) + 1);
SELECT a FROM t ORDER BY rowid;
";

#[test]
fn cli_v13_reads_byte_identical() {
    let a = temp_db("reads-hipp");
    let b = temp_db("reads-ora");
    assert_eq!(run(&hipp_bin(), &a, READS), run(&oracle(), &b, READS));
    let a2 = temp_db("reads-hipp-stdin");
    let b2 = temp_db("reads-ora-stdin");
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, READS),
        run_stdin(&oracle(), &b2, READS)
    );
}

#[test]
fn cli_v13_write_snapshot_byte_identical() {
    let a = temp_db("writes-hipp");
    let b = temp_db("writes-ora");
    assert_eq!(run(&hipp_bin(), &a, WRITES), run(&oracle(), &b, WRITES));
    // The files themselves agree afterwards, read by the oracle.
    assert_eq!(
        run(&oracle(), &a, "SELECT a FROM t ORDER BY rowid;"),
        run(&oracle(), &b, "SELECT a FROM t ORDER BY rowid;")
    );
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), "ok\n");
}
