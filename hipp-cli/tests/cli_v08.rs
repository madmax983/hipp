//! hipp-cli v0.8 differentials: cross-affinity comparisons,
//! INSERT INTO ... SELECT and VALUES subqueries must produce
//! byte-identical output through the hipp CLI and the pinned
//! oracle CLI, and hipp's `.dump` after v0.8 writes must
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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v08-{tag}-{}", std::process::id()));
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
CREATE TABLE m (i INTEGER, x TEXT, n NUMERIC, b BLOB);
INSERT INTO m VALUES (1,'1',1,1),(2,'002','002','2'),(10,'10',10,10);
SELECT i = x, n = x, b = i, x = b FROM m ORDER BY i;
SELECT i FROM m WHERE i = x ORDER BY i;
CREATE TABLE d (a INTEGER PRIMARY KEY, b TEXT, c INTEGER DEFAULT 99);
INSERT INTO d SELECT i, x, n FROM m WHERE i <= 2 RETURNING a, b;
INSERT INTO d SELECT a + 100, b, c FROM d;
INSERT INTO d SELECT 7, 'seven', 7 ON CONFLICT DO NOTHING RETURNING a;
INSERT INTO m VALUES ((SELECT max(i) FROM m) + 1, '11', 11, 11) RETURNING i, x;
SELECT count(*), sum(i) FROM m;
SELECT CASE i WHEN x THEN 'y' ELSE 'n' END FROM m ORDER BY i;
";

#[test]
fn v08_corpus_byte_identical() {
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
fn dump_after_v08_writes_replays() {
    let a = temp_db("dump-a");
    let out = run(&hipp_bin(), &a, CORPUS);
    let _ = out;
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let replayed = run(&oracle(), &fresh, &dump);
    let _ = replayed;
    let check = "SELECT * FROM m ORDER BY i; SELECT * FROM d ORDER BY a;";
    assert_eq!(run(&oracle(), &fresh, check), run(&oracle(), &a, check));
}
