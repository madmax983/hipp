//! hipp-cli v0.9 differentials: compound SELECTs, VALUES
//! cores and derived tables must produce byte-identical
//! output through the hipp CLI and the pinned oracle CLI,
//! and hipp's `.dump` after v0.9 writes must replay into the
//! oracle cleanly.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v09-{tag}-{}", std::process::id()));
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
CREATE TABLE t (a INTEGER, b TEXT);
INSERT INTO t VALUES (1,'one'),(2,'two'),(10,'ten');
CREATE TABLE u (c INTEGER, d TEXT);
INSERT INTO u VALUES (2,'zwei'),(3,'drei'),(7,'sieben');
SELECT a FROM t UNION SELECT c FROM u;
SELECT a FROM t UNION ALL SELECT c FROM u ORDER BY 1 DESC LIMIT 3;
SELECT 1 UNION SELECT 2 INTERSECT SELECT 2;
SELECT 1 UNION SELECT '1';
SELECT a, b FROM t EXCEPT SELECT c, d FROM u WHERE c = 2;
VALUES (3),(1),(2);
SELECT * FROM (SELECT a, b FROM t UNION SELECT c, d FROM u) s ORDER BY s.b;
SELECT * FROM (VALUES (1,2),(3,4)) v ORDER BY column1;
CREATE TABLE d (a INTEGER, b TEXT);
INSERT INTO d SELECT a, b FROM t UNION SELECT c, d FROM u;
INSERT INTO d SELECT * FROM (SELECT a, b FROM d) s;
UPDATE t SET b = 'x' FROM (SELECT c FROM u) s WHERE t.a = s.c;
SELECT count(*) FROM d;
SELECT * FROM t ORDER BY a;
";

#[test]
fn v09_corpus_byte_identical() {
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
fn dump_after_v09_writes_replays() {
    let a = temp_db("dump-a");
    let _ = run(&hipp_bin(), &a, CORPUS);
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let replayed = run(&oracle(), &fresh, &dump);
    let _ = replayed;
    // The replayed file must answer the same compound query
    // the same way through both CLIs.
    let q = "SELECT a FROM t UNION SELECT c FROM u ORDER BY 1;";
    assert_eq!(run(&hipp_bin(), &a, q), run(&oracle(), &fresh, q));
}
