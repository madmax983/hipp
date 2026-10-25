//! hipp-cli v0.6 differentials: subquery scripts (scalar, IN,
//! EXISTS — including the write side) must produce
//! byte-identical output through the hipp CLI and the pinned
//! oracle CLI, and hipp's `.dump` after subquery-driven writes
//! must replay into the oracle cleanly.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v06-{tag}-{}", std::process::id()));
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

const CORPUS: &str = "\
CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT, budget INTEGER);
CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, dept_id INTEGER, salary INTEGER);
INSERT INTO dept VALUES (1,'eng',1000),(2,'ops',400),(3,'empty',50);
INSERT INTO emp VALUES (1,'amy',1,300),(2,'bob',1,200),(3,'cy',2,150),(4,'dee',NULL,100);
SELECT d.name, (SELECT count(*) FROM emp WHERE dept_id = d.id) FROM dept d ORDER BY d.id;
SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE budget >= 400) ORDER BY id;
SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE dept_id = dept.id);
SELECT (SELECT max(salary) FROM emp), (SELECT name FROM dept WHERE id = 99);
UPDATE emp SET salary = salary + 10 WHERE dept_id IN (SELECT id FROM dept WHERE budget >= 400);
SELECT * FROM emp ORDER BY id;
DELETE FROM emp WHERE EXISTS (SELECT 1 FROM dept WHERE dept.id = emp.dept_id AND dept.budget < 500);
SELECT * FROM emp ORDER BY id;
";

#[test]
fn subquery_corpus_byte_identical() {
    let a = temp_db("oneshot-a");
    let b = temp_db("oneshot-b");
    assert_eq!(
        run(&hipp_bin(), &a, CORPUS),
        run(&oracle(), &b, CORPUS),
        "one-shot corpus diverged"
    );
    let a2 = temp_db("stdin-a");
    let b2 = temp_db("stdin-b");
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, CORPUS),
        run_stdin(&oracle(), &b2, CORPUS),
        "stdin corpus diverged"
    );
}

#[test]
fn dump_after_subquery_writes_replays() {
    let a = temp_db("dump-a");
    let out = run(&hipp_bin(), &a, CORPUS);
    let _ = out;
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let replayed = run(&oracle(), &fresh, &dump);
    let _ = replayed;
    let check = "SELECT * FROM dept ORDER BY id; SELECT * FROM emp ORDER BY id;";
    assert_eq!(run(&oracle(), &fresh, check), run(&oracle(), &a, check));
}
