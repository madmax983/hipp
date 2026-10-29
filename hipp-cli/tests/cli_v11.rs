//! hipp-cli v0.11 differentials: subqueries inside aggregate
//! arguments must produce byte-identical output through the
//! hipp CLI and the pinned oracle CLI (one-shot and over
//! stdin), and hipp's `.dump` after a v0.11-driven write must
//! replay into the oracle cleanly.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v11-{tag}-{}", std::process::id()));
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
CREATE TABLE t (a INTEGER, b INTEGER);
CREATE TABLE u (x INTEGER);
INSERT INTO t VALUES (1,10),(1,20),(2,30),(2,NULL);
INSERT INTO u VALUES (100),(200);
SELECT sum((SELECT 1)) FROM t;
SELECT sum((SELECT x FROM u LIMIT 1)) FROM t;
SELECT a, sum((SELECT b)) FROM t GROUP BY a ORDER BY a;
SELECT sum((SELECT b FROM t t2 WHERE t2.a = t.a LIMIT 1)) FROM t;
SELECT count((SELECT 1 WHERE a = 1)) FROM t;
SELECT sum(b + (SELECT 1)) FROM t;
SELECT min((SELECT b)), b FROM t;
SELECT max((SELECT b)), b, a FROM t;
SELECT sum(DISTINCT (SELECT a)) FROM t;
SELECT group_concat((SELECT b), (SELECT ';')) FROM t;
SELECT sum(CASE WHEN a = 1 THEN (SELECT 5) ELSE (SELECT 6) END) FROM t;
SELECT avg((SELECT b)) FROM t;
SELECT sum((SELECT b)) FROM t WHERE a = 999;
SELECT sum((SELECT sum(b) FROM t)) FROM t;
SELECT (SELECT sum((SELECT b)) FROM t);
CREATE TABLE dst (v INTEGER);
INSERT INTO dst SELECT sum((SELECT b)) FROM t;
INSERT INTO dst SELECT a FROM t GROUP BY a;
SELECT v FROM dst ORDER BY v;
";

#[test]
fn v11_corpus_byte_identical() {
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
fn dump_after_v11_writes_replays() {
    let a = temp_db("dump-a");
    let _ = run(&hipp_bin(), &a, CORPUS);
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let _ = run(&oracle(), &fresh, &dump);
    let q = "SELECT v FROM dst ORDER BY v;";
    assert_eq!(run(&hipp_bin(), &a, q), run(&oracle(), &fresh, q));
}
