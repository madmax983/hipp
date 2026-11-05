//! hipp-cli v0.16 EXPLAIN QUERY PLAN review fixes (leaf `explain`):
//! byte-for-byte the pinned oracle shell's stdout for the trees the
//! review found wrong — window and UNION LIMIT 1 rows next to their
//! plans, the write loops' COVERING, `f(ORDER BY)` sorters and their
//! place under GROUP BY, upsert subqueries, VALUES select numbers,
//! and the schema qualifiers of statements routed to the TEMP or an
//! attached database.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16eqpr-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// `script` on fresh twin files: identical stdout.
fn same(tag: &str, script: &str) {
    let a = run_stdin(&oracle(), &temp_db(&format!("{tag}-o")), script);
    let b = run_stdin(&hipp_bin(), &temp_db(&format!("{tag}-h")), script);
    assert_eq!(b, a, "script:\n{script}");
}

const SCHEMA: &str = "\
CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, d TEXT);
CREATE INDEX tb ON t(b);
CREATE INDEX tcd ON t(c, d);
CREATE TABLE u(x, y);
CREATE UNIQUE INDEX ux ON u(x);
CREATE TABLE e(p, q);
INSERT INTO t VALUES (1, 30, 3, 'x'), (2, 10, 1, 'y'), (3, 20, 2, 'z');
INSERT INTO u VALUES (2, 9);
INSERT INTO e VALUES (1, 2), (1, 3);
";

#[test]
fn review_trees() {
    let script = format!(
        "{SCHEMA}\
SELECT a, sum(a) OVER (ORDER BY b) FROM t ORDER BY c;
EXPLAIN QUERY PLAN SELECT a, sum(a) OVER (ORDER BY b) FROM t ORDER BY c;
SELECT a FROM t UNION SELECT 99 LIMIT 1;
SELECT (SELECT a FROM t UNION SELECT 99);
EXPLAIN QUERY PLAN SELECT (SELECT a FROM t UNION SELECT 99);
EXPLAIN QUERY PLAN UPDATE t SET b = 1 WHERE b = 2;
EXPLAIN QUERY PLAN UPDATE t SET d = 'q' WHERE c = 2;
EXPLAIN QUERY PLAN UPDATE t SET a = 5 WHERE b = 2;
EXPLAIN QUERY PLAN DELETE FROM u WHERE x = 2 RETURNING (SELECT count(*) FROM e);
EXPLAIN QUERY PLAN SELECT group_concat(c ORDER BY d) FROM t;
EXPLAIN QUERY PLAN SELECT b, string_agg(c, ',' ORDER BY d) FROM t GROUP BY b;
EXPLAIN QUERY PLAN SELECT p, count(DISTINCT q) FROM e GROUP BY p ORDER BY 2;
EXPLAIN QUERY PLAN INSERT INTO u VALUES (2, 3) ON CONFLICT(x) DO UPDATE SET y = (SELECT p FROM e) RETURNING (SELECT q FROM e);
EXPLAIN QUERY PLAN SELECT * FROM t WHERE b IN (VALUES (1));
EXPLAIN QUERY PLAN SELECT (VALUES (1), (2));
EXPLAIN QUERY PLAN SELECT count(*) FROM t AS z;
"
    );
    same("trees", &script);
}

#[test]
fn routed_qualifiers() {
    let script = "\
CREATE TABLE m(x);
CREATE TEMP TABLE tt(a, b);
CREATE INDEX temp.tti ON tt(a);
ATTACH ':memory:' AS aux;
CREATE TABLE aux.at(a, b);
EXPLAIN QUERY PLAN SELECT * FROM temp.tt WHERE a = 1;
EXPLAIN QUERY PLAN DELETE FROM TEMP.tt WHERE b = 1;
EXPLAIN QUERY PLAN SELECT * FROM temp.sqlite_master;
EXPLAIN QUERY PLAN SELECT * FROM sqlite_temp_master;
EXPLAIN QUERY PLAN SELECT count(*) FROM temp.tt;
EXPLAIN QUERY PLAN SELECT * FROM aux.at;
EXPLAIN QUERY PLAN SELECT * FROM main.m, temp.tt;
";
    same("routed", script);
}
