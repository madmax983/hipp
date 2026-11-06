//! hipp-cli v0.17 query flattening (leaf `flatten`): byte-for-byte
//! the pinned oracle's stdout for views, CTEs and FROM subqueries the
//! pin flattens into their readers (their rows in the pin's join
//! order, EXPLAIN QUERY PLAN trees without the co-routine), WHERE
//! terms pushed into the subqueries that stay, `SEARCH t EXISTS`
//! semi-join loops, the foreign-key child scans of a parent DELETE /
//! UPDATE, and errors in result columns the reader never reads, which
//! neither engine raises (hipp's shell prints an error in its own
//! words, so the cases here are those both engines answer).

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17fl-{tag}"));
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

const SETUP: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);
CREATE INDEX tb ON t(b);
CREATE TABLE u(x, y);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(1,2,3),(2,3,4),(3,4,5),(4,4,6),(5,NULL,7);
INSERT INTO u VALUES(1,10),(2,20),(5,50),(4,40),(NULL,0);
CREATE VIEW v AS SELECT a, b*2 AS bb FROM t WHERE c>3;
CREATE VIEW vagg AS SELECT b, count(*) n FROM t GROUP BY b;
CREATE VIEW vbad AS SELECT a, abs(a - 2 - 9223372036854775807) AS bad FROM t;";

#[test]
fn flattened_views_and_subqueries() {
    same(
        "flat",
        SETUP,
        "SELECT * FROM u, v WHERE v.a = u.x;
EXPLAIN QUERY PLAN SELECT * FROM u, v WHERE v.a = u.x;
SELECT * FROM u LEFT JOIN (SELECT a, 1 AS one FROM t) s ON s.a = u.x;
EXPLAIN QUERY PLAN SELECT * FROM u LEFT JOIN (SELECT a, 1 AS one FROM t) s ON s.a = u.x;
WITH c AS (SELECT a, b FROM t) SELECT * FROM c x, c y WHERE x.a = y.b;
EXPLAIN QUERY PLAN WITH c AS (SELECT a, b FROM t) SELECT * FROM c x, c y WHERE x.a = y.b;
SELECT * FROM (SELECT a, b FROM t ORDER BY b DESC LIMIT 2);
EXPLAIN QUERY PLAN SELECT * FROM (SELECT a, b FROM t ORDER BY b DESC LIMIT 2);
SELECT * FROM (SELECT a AS k, b FROM t) s JOIN u ON u.x = s.k;
",
    );
}

#[test]
fn push_down_and_exists_to_join() {
    same(
        "push",
        SETUP,
        "SELECT * FROM vagg WHERE b = 4;
EXPLAIN QUERY PLAN SELECT * FROM vagg WHERE b = 4;
SELECT * FROM (SELECT DISTINCT b FROM t) WHERE b > 2;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT DISTINCT b FROM t) WHERE b > 2;
SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x);
EXPLAIN QUERY PLAN SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x);
SELECT x FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b >= u.x) ORDER BY x DESC;
EXPLAIN QUERY PLAN SELECT * FROM u WHERE NOT EXISTS (SELECT 1 FROM t WHERE t.b = u.x);
",
    );
}

#[test]
fn foreign_key_child_scans() {
    same(
        "fk",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, k UNIQUE, z);
CREATE TABLE c1(pid REFERENCES p(id), v);
CREATE TABLE c2(pk REFERENCES p(k));
CREATE INDEX c2pk ON c2(pk);",
        "EXPLAIN QUERY PLAN DELETE FROM p WHERE id = 1;
PRAGMA foreign_keys = ON;
EXPLAIN QUERY PLAN DELETE FROM p WHERE id = 1;
EXPLAIN QUERY PLAN UPDATE p SET k = 5 WHERE id = 1;
EXPLAIN QUERY PLAN UPDATE p SET z = 5;
",
    );
}

#[test]
fn unused_columns_are_not_evaluated() {
    same(
        "unused",
        SETUP,
        "SELECT a FROM vbad;
SELECT count(*) FROM vbad;
SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t LIMIT 5) x, u WHERE x.a = u.x;
SELECT u.x, s.one FROM u LEFT JOIN (SELECT a, 1 AS one, abs(a - 9223372036854775807 - 2) AS z FROM t) s ON s.a = u.x;
",
    );
}

/// Review regressions (v0.17 flatten review), both engines answering:
/// a view read twice (CO-ROUTINE + MATERIALIZE once a copy loses a
/// column), a CTE read inside an IN (a MATERIALIZE), a MULTI-INDEX OR
/// under a kept subquery, an EXISTS beside a LEFT JOIN, an UPDATE ...
/// FROM over an unread overflowing column, and the foreign-key child
/// scans' affinity, collation and INSERT rules.
#[test]
fn review_regressions() {
    same(
        "review",
        "CREATE TABLE t1(a INTEGER PRIMARY KEY, b); INSERT INTO t1 VALUES(1,10),(2,20),(3,30);
CREATE VIEW vv AS SELECT a n, abs(-9223372036854775807-1) e FROM t1 LIMIT 3;
CREATE TABLE m1(a, c); CREATE TABLE m2(x, y);
CREATE INDEX m2x ON m2(x); CREATE INDEX m2y ON m2(y);
INSERT INTO m1 VALUES(1,'a'),(2,'b'); INSERT INTO m2 VALUES(1,'p'),(2,'q'),(3,'b');
CREATE TABLE t2(x INT, y, z); CREATE INDEX t2x ON t2(x);
INSERT INTO t2 VALUES(1,'p',100),(2,'q',200),(2,'r',300),(NULL,'s',400),(5,'t',NULL);
CREATE TABLE t4(a, b); INSERT INTO t4 VALUES(1,2);",
        "SELECT r.n FROM vv r, vv r2 WHERE r.n=r2.n;
EXPLAIN QUERY PLAN SELECT r.n FROM vv r, vv r2 WHERE r.n=r2.n;
WITH g(x) AS (SELECT b FROM t1 LIMIT 2) SELECT * FROM t1 WHERE b IN (SELECT x FROM g WHERE x>10);
EXPLAIN QUERY PLAN WITH g(x) AS (SELECT b FROM t1 LIMIT 2) SELECT * FROM t1 WHERE b IN (SELECT x FROM g WHERE x>10);
SELECT s.a, y FROM (SELECT a, c FROM m1 LIMIT 9) s, m2 WHERE m2.x=s.a OR m2.y=s.c;
SELECT * FROM t1 LEFT JOIN t2 ON x=a WHERE EXISTS (SELECT 1 FROM t2 w WHERE w.x=t2.x);
EXPLAIN QUERY PLAN SELECT * FROM t1 LEFT JOIN t2 ON x=a WHERE EXISTS (SELECT 1 FROM t2 w WHERE w.x=t2.x);
UPDATE t4 SET b=s.a FROM (SELECT a, abs(-9223372036854775807-1) e FROM t1 LIMIT 3) s WHERE s.a=t4.a;
SELECT * FROM t4;
PRAGMA foreign_keys = ON;
CREATE TABLE p(id INTEGER PRIMARY KEY, v);
CREATE TABLE c(k REFERENCES p, j); CREATE INDEX ck ON c(k);
CREATE TABLE ct(k TEXT REFERENCES p); CREATE INDEX ctk ON ct(k);
EXPLAIN QUERY PLAN DELETE FROM p WHERE id = 1;
EXPLAIN QUERY PLAN REPLACE INTO p VALUES(1, 2);
EXPLAIN QUERY PLAN INSERT INTO p VALUES(1, 2), (3, 4);
",
    );
}
