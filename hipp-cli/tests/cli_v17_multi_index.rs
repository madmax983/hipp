//! hipp-cli v0.17 index strategies (leaf `planner`): byte-for-byte
//! the pinned oracle's stdout for the MULTI-INDEX OR (rows in the
//! pin's branch order, each once; the `MULTI-INDEX OR` / `INDEX n`
//! trees with their IN subqueries and LEFT-JOIN loops), the min() /
//! max() lookups (their bare columns, `SEARCH t`), skip-scans after
//! ANALYZE (`ANY(a) AND b=?`), single-table cost ties and UPDATE /
//! DELETE through a MULTI-INDEX OR. (The plans come from EXPLAIN
//! QUERY PLAN statements: hipp's shell has no `.eqp`.)

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17mi-{tag}"));
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

const SETUP: &str = "CREATE TABLE t(a, b, c, d);
CREATE INDEX ta ON t(a);
CREATE INDEX tb ON t(b);
CREATE INDEX tbc ON t(b, c);
WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<50)
INSERT INTO t SELECT i%7, i%5, i%3, i FROM r;
INSERT INTO t VALUES (NULL, NULL, NULL, 99), ('x', X'41', 'z', 100);
CREATE TABLE u(x INTEGER PRIMARY KEY, y, z);
CREATE INDEX uy ON u(y);
INSERT INTO u SELECT d, a, b FROM t WHERE d < 30;
CREATE TABLE w(k INTEGER, j, v, PRIMARY KEY(k)) WITHOUT ROWID;
CREATE INDEX wj ON w(j);
INSERT INTO w SELECT d, d%4, a FROM t WHERE d < 20;";

/// MULTI-INDEX OR rows and trees on one table and in joins.
#[test]
fn multi_index_or_outputs() {
    same(
        "or",
        SETUP,
        "SELECT d FROM t WHERE a=1 OR b=2;
EXPLAIN QUERY PLAN SELECT d FROM t WHERE a=1 OR b=2;
SELECT d FROM t WHERE a IN (1,2) OR (b=2 AND c=1);
EXPLAIN QUERY PLAN SELECT d FROM t WHERE a IN (1,2) OR (b=2 AND c=1);
SELECT d FROM t WHERE a=1 OR b IN (SELECT y FROM u WHERE x < 9) ORDER BY d DESC LIMIT 4;
EXPLAIN QUERY PLAN SELECT d FROM t WHERE a=1 OR b IN (SELECT y FROM u WHERE x < 9) ORDER BY d DESC LIMIT 4;
SELECT * FROM u WHERE x=3 OR y=2;
EXPLAIN QUERY PLAN SELECT * FROM u WHERE x=3 OR y=2;
SELECT k, v FROM w WHERE k<4 OR j=1;
EXPLAIN QUERY PLAN SELECT k, v FROM w WHERE k<4 OR j=1;
SELECT t.d, u.x FROM u, t WHERE (t.a=u.y OR t.b=u.z) AND u.x < 4;
EXPLAIN QUERY PLAN SELECT t.d, u.x FROM u, t WHERE (t.a=u.y OR t.b=u.z) AND u.x < 4;
SELECT u.x, t.d FROM u LEFT JOIN t ON (t.a=u.y OR t.b=u.z) WHERE u.x < 3;
EXPLAIN QUERY PLAN SELECT u.x, t.d FROM u LEFT JOIN t ON (t.a=u.y OR t.b=u.z) WHERE u.x < 3;
SELECT count(*) FROM t WHERE a<2 OR a=2;
EXPLAIN QUERY PLAN SELECT count(*) FROM t WHERE a<2 OR a=2;",
    );
}

/// min() / max(): the value, the first row's bare columns, the plan.
#[test]
fn min_max_outputs() {
    same(
        "mm",
        SETUP,
        "SELECT max(a), d FROM t;
SELECT min(a), d FROM t;
SELECT max(b), d FROM t WHERE c=1;
SELECT min(b), d FROM t WHERE b IS NULL;
SELECT max(x), y FROM u;
SELECT min(k), v FROM w;
SELECT max(t.a), u.x FROM t, u WHERE u.y=t.c;
EXPLAIN QUERY PLAN SELECT max(a), d FROM t;
EXPLAIN QUERY PLAN SELECT min(a) FROM t;
EXPLAIN QUERY PLAN SELECT max(d) FROM t;
EXPLAIN QUERY PLAN SELECT max(x) FROM u;
EXPLAIN QUERY PLAN SELECT min(k) FROM w;
EXPLAIN QUERY PLAN SELECT max(DISTINCT b) FROM t WHERE b=2;
EXPLAIN QUERY PLAN SELECT DISTINCT max(a) FROM t;
EXPLAIN QUERY PLAN SELECT max(t.a), u.x FROM t, u WHERE u.y=t.c;",
    );
}

/// Skip-scans after ANALYZE, single-table cost ties, and UPDATE /
/// DELETE through a MULTI-INDEX OR.
#[test]
fn skip_scan_ties_and_writes() {
    same(
        "ss",
        "CREATE TABLE s(a, b, c);
CREATE INDEX sab ON s(a, b);
WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<600)
INSERT INTO s SELECT i%3, i, i%7 FROM r;
CREATE TABLE p(a, b, c);
CREATE INDEX pbc ON p(b, c);
CREATE INDEX pc ON p(c);
INSERT INTO p VALUES (1,2,1),(2,2,1),(3,1,1),(4,2,2);",
        "SELECT * FROM s WHERE b=5 OR b=200;
EXPLAIN QUERY PLAN SELECT * FROM s WHERE b=5;
ANALYZE;
SELECT * FROM s WHERE b=5 OR b=200;
SELECT a, b FROM s WHERE b BETWEEN 10 AND 12 ORDER BY a DESC;
EXPLAIN QUERY PLAN SELECT * FROM s WHERE b=5;
EXPLAIN QUERY PLAN SELECT a, b FROM s WHERE b BETWEEN 10 AND 12 ORDER BY a DESC;
SELECT * FROM p WHERE b=2 AND c=1;
EXPLAIN QUERY PLAN SELECT * FROM p WHERE b=2 AND c=1;
EXPLAIN QUERY PLAN UPDATE p SET a=a+1 WHERE b=1 OR c=2;
UPDATE p SET a=a+1 WHERE b=1 OR c=2 RETURNING *;
EXPLAIN QUERY PLAN DELETE FROM p WHERE a=2 OR c=2;
DELETE FROM p WHERE b=1 OR c=2 RETURNING a;
SELECT * FROM p;",
    );
}

/// The review's classes (v0.17 review): a COLLATE inside an OR, the
/// one-value IN, an OR over an indexed expression, the IN operator's
/// own index and Bloom filter, min() / max() over a FROM subquery and
/// a virtual table, `sqlite_stat1`'s `noskipscan`, and an equality's
/// collation against the ORDER BY's.
#[test]
fn review_outputs() {
    same(
        "rv",
        "CREATE TABLE t(a, b); CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b);
CREATE INDEX tex ON t(a*2);
INSERT INTO t VALUES('x','y'),('X','q'),(1,'Y'),('z','x'),('y','X'),('X','Y'),(7,1),(2,2);
CREATE TABLE u(a TEXT COLLATE NOCASE, e REAL, h INTEGER PRIMARY KEY);
CREATE INDEX ua ON u(a);
INSERT INTO u VALUES('x',1,5),('Y',2,6);
CREATE TABLE k(a INT, b INT, d); CREATE INDEX kab ON k(a, b);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300)
INSERT INTO k SELECT i%3, i%50, i FROM n;
ANALYZE;
UPDATE sqlite_stat1 SET stat = stat || ' noskipscan' WHERE idx = 'kab';
ANALYZE sqlite_schema;",
        "SELECT rowid FROM t WHERE a = 'X' OR b = 'x' COLLATE NOCASE;
EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE a = 2 OR b = 'x' COLLATE BINARY;
SELECT rowid FROM t WHERE a = 2 OR b = 'x' COLLATE BINARY;
SELECT rowid FROM t WHERE a = 'z' OR a IN ('X');
EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE a = 'z' OR a IN ('X');
SELECT rowid FROM t WHERE a*2 = 14 OR a*2 = 4;
EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE a*2 = 14 OR a*2 = 4;
EXPLAIN QUERY PLAN SELECT * FROM t WHERE b IN (SELECT a FROM u WHERE e > 0);
EXPLAIN QUERY PLAN SELECT * FROM t WHERE a IN (SELECT a FROM u WHERE e > 0);
EXPLAIN QUERY PLAN SELECT * FROM t WHERE b IN (SELECT h FROM u);
EXPLAIN QUERY PLAN SELECT * FROM t WHERE b IN (SELECT a FROM u);
SELECT * FROM t WHERE b IN (SELECT a FROM u);
EXPLAIN QUERY PLAN SELECT max(a) FROM (SELECT a FROM t ORDER BY a DESC LIMIT 3);
EXPLAIN QUERY PLAN SELECT max(value) FROM generate_series(1, 5);
SELECT max(value) FROM generate_series(1, 5);
SELECT rowid, d FROM k WHERE b = 7;
EXPLAIN QUERY PLAN SELECT rowid, d FROM k WHERE b = 7;
SELECT a, rowid FROM t WHERE a = 'x' COLLATE NOCASE ORDER BY a;
EXPLAIN QUERY PLAN SELECT a, rowid FROM t WHERE a = 'x' COLLATE NOCASE ORDER BY a;",
    );
}
