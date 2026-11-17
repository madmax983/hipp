//! hipp-cli v0.17 parity unit `values_chain`: the fixes as the
//! shell shows them, byte-for-byte the pinned oracle's (SQLite 3.54.1)
//! stdout, and failing exactly where it fails: a VALUES chain's rows
//! filtered with their own affinity (a WHERE term pushed into each
//! chained row, a later chain's wrapper with its column's, a chain of
//! one-row selects throughout untouched) and its EXPLAIN QUERY PLAN
//! lines; a trigger body's VALUES read as a chain; an unknown
//! collation refused only where the pin looks it up; an enclosing
//! aggregate in a dropped ORDER BY or an EXISTS result list stepped;
//! a nested window ORDER BY inside a dropped term; view column types
//! through a compound subquery.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17vc-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

/// A script on stdin: stdout, whether stderr had anything, and
/// whether the shell exited 0.
fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, bool, bool) {
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
        !out.stderr.is_empty(),
        out.status.success(),
    )
}

/// The SQL as the one-shot argument: stdout, whether stderr had
/// anything, and whether the shell exited 0.
fn run_arg(bin: &Path, db: &Path, sql: &str) -> (String, bool, bool) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        !out.stderr.is_empty(),
        out.status.success(),
    )
}

/// `setup` then `script` on stdin, each engine on its own fresh
/// file: the same stdout, the same error presence and exit status.
fn same(tag: &str, setup: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let input = format!("{setup}\n{script}");
    let want = run_stdin(&oracle(), &o, &input);
    let got = run_stdin(&hipp_bin(), &h, &input);
    assert_eq!(got, want, "script:\n{input}");
}

/// Each one-shot argument on a fresh file: as the pin.
fn same_args(tag: &str, args: &[&str]) {
    for (i, sql) in args.iter().enumerate() {
        let h = temp_db(&format!("{tag}{i}-h"));
        let o = temp_db(&format!("{tag}{i}-o"));
        let want = run_arg(&oracle(), &o, sql);
        let got = run_arg(&hipp_bin(), &h, sql);
        assert_eq!(got, want, "argument: {sql:?}");
    }
}

const SETUP: &str = "CREATE TABLE t(r REAL, s TEXT, i INTEGER);
INSERT INTO t VALUES (3, '3', 3), (1.5, 'x', 2);
CREATE TABLE u(a INTEGER, b TEXT);
INSERT INTO u VALUES (1, 'x'), (2, 'Y'), (-9223372036854775807, 'z');";

#[test]
fn values_chains_render_like_the_pin() {
    same(
        "ch",
        SETUP,
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x = '3';
WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (3), (CAST(4 AS REAL))) SELECT x, typeof(x) FROM q WHERE x = '3';
WITH w AS (SELECT 1) SELECT column1 FROM (VALUES (3), (CAST(4 AS REAL))) WHERE column1 = '3';
SELECT column1, typeof(column1) FROM (VALUES (3), ((SELECT 3)), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3';
SELECT (SELECT group_concat(r) FROM (VALUES (t.r), (2))) FROM t;
SELECT (SELECT group_concat(column1) FROM (VALUES (t.r))) FROM t;
SELECT 'after';
EXPLAIN QUERY PLAN WITH q(x) AS (VALUES (3), (4)) SELECT x FROM q;
EXPLAIN QUERY PLAN WITH w AS (SELECT 1) SELECT * FROM (VALUES (3), (4));
EXPLAIN QUERY PLAN WITH w AS (SELECT 1) VALUES (1),(2) UNION ALL VALUES (3),(4);
EXPLAIN QUERY PLAN VALUES (1),((SELECT 3)),(4),(5);
EXPLAIN QUERY PLAN SELECT 0 UNION ALL VALUES (1),((SELECT 3)),(4);",
    );
}

#[test]
fn trigger_values_render_like_the_pin() {
    same(
        "tr",
        SETUP,
        "CREATE TABLE lg(x, ty);
CREATE TABLE g(k);
CREATE TRIGGER tr AFTER INSERT ON g BEGIN
  INSERT INTO lg SELECT column1, typeof(column1) FROM (VALUES (3), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3';
  INSERT INTO lg SELECT x, typeof(x) FROM (SELECT s AS x FROM t UNION ALL VALUES (2), (CAST(3 AS REAL)));
END;
INSERT INTO g VALUES (1);
SELECT * FROM lg;",
    );
}

#[test]
fn unknown_collations_render_like_the_pin() {
    same(
        "co",
        SETUP,
        "SELECT 1 COLLATE bogus;
SELECT s COLLATE bogus || '!' FROM t;
SELECT count(s COLLATE bogus), group_concat(s COLLATE bogus) FROM t;
SELECT 1 FROM t WHERE 0 AND s COLLATE bogus = 'x';
SELECT 's' IN ('s' COLLATE bogus, 't');
SELECT max(s, 'q' COLLATE bogus) FROM t;
SELECT 1 ORDER BY (SELECT s FROM t ORDER BY s COLLATE bogus);
SELECT s FROM t WHERE s COLLATE bogus = 'x';
SELECT 'after an error';
SELECT s FROM t ORDER BY s COLLATE bogus;
SELECT DISTINCT s COLLATE bogus FROM t;
SELECT * FROM (SELECT s COLLATE bogus FROM t);
CREATE TABLE ck(y CHECK (y COLLATE bogus = 'a'));
INSERT INTO ck VALUES ('a');
CREATE INDEX ib ON t(s COLLATE bogus);
SELECT 'done';",
    );
    same_args(
        "coa",
        &[
            "SELECT 1 COLLATE bogus",
            "SELECT 1 COLLATE bogus = 1",
            "VALUES (1 COLLATE bogus), (2)",
            "SELECT 1 COLLATE bogus UNION ALL SELECT 2",
            "SELECT 1 COLLATE bogus UNION SELECT 2",
        ],
    );
}

#[test]
fn inert_aggregates_and_nested_windows_render_like_the_pin() {
    same(
        "ag",
        SETUP,
        "SELECT (SELECT 1 ORDER BY sum(u.a)) FROM u;
SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u;
SELECT 'next';
SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3))) FROM u;
SELECT a FROM u WHERE EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3)));
SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY (1,2));
SELECT count(*) FROM u ORDER BY (SELECT row_number() OVER () ORDER BY (1,2));
SELECT row_number() OVER () ORDER BY (1,2);
SELECT 'done';",
    );
}

#[test]
fn view_types_render_like_the_pin() {
    same(
        "vt",
        "CREATE TABLE t(a INTEGER, b TEXT); CREATE TABLE w(p DATETIME, r DOUBLE PRECISION);",
        "CREATE VIEW v1 AS SELECT * FROM (SELECT a FROM t UNION ALL SELECT 2.5);
CREATE VIEW v2 AS SELECT * FROM (SELECT p, r FROM w UNION ALL SELECT 1, 1);
CREATE VIEW v3 AS WITH c AS (SELECT b FROM t UNION ALL SELECT 1) SELECT * FROM c;
PRAGMA table_info(v1);
PRAGMA table_info(v2);
SELECT name, type FROM pragma_table_info('v3');",
    );
}
