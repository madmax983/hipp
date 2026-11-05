//! hipp-cli v0.16 EXPLAIN QUERY PLAN (leaf `explain`): byte-for-byte
//! the pinned oracle shell's stdout — its tree rendering (`QUERY
//! PLAN`, `|--`, `` `-- ``, `|  ` / blank indentation), nothing for a
//! statement without plan rows, plans between ordinary result rows,
//! lower-case and commented `EXPLAIN QUERY PLAN`, parameters bound from
//! `.parameter`, and a failing statement after it.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16eqp-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, bool) {
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
        out.status.success(),
    )
}

/// `script` on fresh twin files: identical stdout.
fn same(tag: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    // (stdout only: the shells' exit statuses after a failed
    // statement are not this suite's subject.)
    let a = run_stdin(&oracle(), &o, script).0;
    let b = run_stdin(&hipp_bin(), &h, script).0;
    assert_eq!(b, a, "script:\n{script}");
}

const SCHEMA: &str = "\
CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, d TEXT);
CREATE INDEX tb ON t(b);
CREATE INDEX tcd ON t(c, d);
CREATE TABLE u(x, y);
CREATE UNIQUE INDEX ux ON u(x);
CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO t VALUES (1, 2, 3, 'x'), (2, 5, 1, 'y');
INSERT INTO u VALUES (2, 9);
";

#[test]
fn plan_trees() {
    let script = format!(
        "{SCHEMA}\
EXPLAIN QUERY PLAN SELECT 1;
EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 1;
SELECT * FROM t ORDER BY a;
EXPLAIN QUERY PLAN SELECT * FROM t WHERE c = 1 AND d > 'x' ORDER BY b;
explain query plan select b from t where b in (1, 2);
/* c */ EXPLAIN -- x
QUERY PLAN SELECT * FROM t LEFT JOIN u ON u.x = t.b;
EXPLAIN QUERY PLAN SELECT (SELECT y FROM u WHERE u.x = t.b), * FROM t WHERE c IN (SELECT y FROM u);
EXPLAIN QUERY PLAN SELECT b FROM t UNION SELECT y FROM u UNION ALL SELECT 1;
EXPLAIN QUERY PLAN SELECT a FROM t UNION ALL SELECT x FROM u UNION ALL SELECT 3;
EXPLAIN QUERY PLAN WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT * FROM r;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT DISTINCT b FROM t) s, (SELECT DISTINCT y FROM u) r2;
EXPLAIN QUERY PLAN SELECT b, sum(c) OVER (ORDER BY b), rank() OVER (ORDER BY c) FROM t;
EXPLAIN QUERY PLAN SELECT * FROM w WHERE k > 3;
EXPLAIN QUERY PLAN SELECT * FROM generate_series(1, 9) ORDER BY value DESC;
EXPLAIN QUERY PLAN UPDATE t SET c = 1 WHERE b = 2;
EXPLAIN QUERY PLAN DELETE FROM t WHERE a > 1;
EXPLAIN QUERY PLAN INSERT INTO u SELECT b, c FROM t;
EXPLAIN QUERY PLAN INSERT INTO u VALUES (1, 2);
EXPLAIN QUERY PLAN CREATE TABLE z(q);
EXPLAIN QUERY PLAN VALUES (1), (2);
SELECT count(*) FROM u;
SELECT name FROM sqlite_schema WHERE name = 'z';
"
    );
    same("trees", &script);
}

#[test]
fn plan_with_parameters_and_errors() {
    let script = format!(
        "{SCHEMA}\
.parameter set :v 2
EXPLAIN QUERY PLAN SELECT * FROM t WHERE b = :v AND a > ?2;
EXPLAIN QUERY PLAN SELECT * FROM nosuch;
SELECT 'after';
EXPLAIN QUERY PLAN SELECT nosuch FROM t;
SELECT count(*) FROM t;
"
    );
    same("params", &script);
}
