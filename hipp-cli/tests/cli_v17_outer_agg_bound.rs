//! hipp-cli v0.17 review fix (unit `vm_flip`, item (d)): an
//! enclosing query's aggregate that a nested aggregate query uses as
//! an index or rowid bound (`SELECT (SELECT count(*) FROM u WHERE u.k
//! <= max(t.a)) FROM t` over an index on `u.k`), and a repeated min()
//! / max() deciding the bare columns — byte-for-byte the pinned
//! oracle's stdout (hipp failed the
//! first with `MisusedAggregate`, and read the wrong bare-column row
//! for the second).

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
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17oab-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

/// A script on stdin: stdout and stderr.
fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, String) {
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
    let out = child.wait_with_output().expect("cli finishes");
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// `setup` then `script`, each engine on its own fresh file: stdout
/// identical and nothing on stderr.
fn same(tag: &str, setup: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let input = format!("{setup}\n{script}");
    let (a, ae) = run_stdin(&oracle(), &o, &input);
    let (b, be) = run_stdin(&hipp_bin(), &h, &input);
    assert_eq!(b, a, "script:\n{input}");
    assert!(ae.is_empty(), "pin stderr {ae}: {input}");
    assert!(be.is_empty(), "hipp stderr {be}: {input}");
}

const SETUP: &str = "CREATE TABLE t(a, b TEXT);
INSERT INTO t VALUES (1,'x'),(2,'y'),(3,NULL),(NULL,'z'),(2,'Y');
CREATE TABLE u(k INTEGER, v);
INSERT INTO u VALUES (1,10),(1,11),(2,20),(NULL,30),(5,50),(2.5,25);
CREATE TABLE w(id INTEGER PRIMARY KEY, p, q);
INSERT INTO w VALUES (1,1,'a'),(2,2,'A'),(3,2,'b'),(4,NULL,'B');
CREATE INDEX u_k ON u(k);
CREATE INDEX w_p ON w(p);";

const QUERIES: &str = "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)) FROM t;
SELECT (SELECT count(*) FROM w WHERE w.id <= max(t.a)) FROM t;
SELECT (SELECT sum(v) FROM u WHERE u.k = min(t.a)) FROM t;
SELECT count(*) FROM t HAVING (SELECT count(*) FROM u WHERE u.k <= max(t.a)) > 1;
SELECT b, (SELECT count(*) FROM u WHERE u.k = max(t.a)) FROM t GROUP BY b ORDER BY 1;
SELECT b, (SELECT avg(v) FROM u WHERE u.k BETWEEN min(t.a) AND max(t.a)) AS av FROM t GROUP BY b ORDER BY 1;
SELECT (SELECT group_concat(q) FROM w WHERE w.p IN (min(t.a), max(t.a))) FROM t;
SELECT max(a), min(a), max(a), a FROM t;
SELECT b, max(a), (SELECT min(t.a)), max(a), a FROM t GROUP BY b IS NULL ORDER BY 1;";

#[test]
fn outer_aggregate_bounds_render_like_the_pin() {
    same("list", SETUP, QUERIES);
}
