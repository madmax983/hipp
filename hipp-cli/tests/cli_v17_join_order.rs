//! hipp-cli v0.17 join ordering (leaf `planner`): byte-for-byte
//! the pinned oracle's stdout for joins the pin reorders — the rows of
//! a query without ORDER BY in the pin's order and EXPLAIN QUERY PLAN
//! statements' trees (SCAN / SEARCH order, AUTOMATIC [PARTIAL]
//! COVERING INDEX, BLOOM FILTER ON) as the shell renders them, after
//! the pin's ANALYZE and after hipp's, and MATCH / table-valued
//! arguments reading a table listed later in FROM. (hipp's shell has
//! no `.eqp`, so the plans come from EXPLAIN QUERY PLAN statements.)

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17jo-{tag}"));
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

const SETUP: &str = "CREATE TABLE a(x INTEGER PRIMARY KEY, y, z TEXT);
CREATE TABLE b(p, q INT, r);
CREATE INDEX bq ON b(q);
CREATE TABLE c(s, t, u NOT NULL);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<20)
INSERT INTO a SELECT i, i%7, 'z'||(i%4) FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<30)
INSERT INTO b SELECT i%9, i%11, NULLIF(i%5, 0) FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<15)
INSERT INTO c SELECT i%6, i%13, i FROM n;";

#[test]
fn reordered_rows_and_plans() {
    same(
        "rows",
        SETUP,
        "SELECT * FROM a, b WHERE a.x = b.p;
SELECT a.z, c.u FROM a JOIN c ON c.s = a.y;
SELECT a.z, c.u FROM a JOIN c ON c.s = a.y WHERE c.u > 3;
SELECT * FROM c LEFT JOIN a ON a.x = c.s;
SELECT * FROM b CROSS JOIN a WHERE a.x = b.q;
EXPLAIN QUERY PLAN SELECT * FROM a, b WHERE a.x = b.p;
EXPLAIN QUERY PLAN SELECT a.z, c.u FROM a JOIN c ON c.s = a.y WHERE c.u > 3;
EXPLAIN QUERY PLAN SELECT * FROM a, b, c WHERE b.q = a.x AND c.t = b.p;
EXPLAIN QUERY PLAN SELECT b.q, count(*) FROM a, b WHERE b.q = a.x GROUP BY b.q;
SELECT b.q, count(*) FROM a, b WHERE b.q = a.x GROUP BY b.q;
SELECT a.x, b.r, c.u FROM c, b, a WHERE b.q = a.x AND c.t = a.y;
EXPLAIN QUERY PLAN SELECT a.x, b.r, c.u FROM c, b, a WHERE b.q = a.x AND c.t = a.y;
SELECT a.x, b.r FROM b, a WHERE b.q = a.x LIMIT 4;",
    );
}

#[test]
fn statistics_from_either_engine() {
    // The pin's ANALYZE in the setup; then the same plans.
    same(
        "stat-pin",
        &format!("{SETUP}\nCREATE INDEX cs ON c(s);\nANALYZE;"),
        "EXPLAIN QUERY PLAN SELECT * FROM b, c WHERE c.s = b.p AND c.t > 2;
SELECT * FROM b, c WHERE c.s = b.p AND c.t > 2;
EXPLAIN QUERY PLAN SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1;
SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1;",
    );
    // Each engine's own ANALYZE, read back in the same session.
    same(
        "stat-own",
        SETUP,
        "ANALYZE;
SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY 1, 2;
EXPLAIN QUERY PLAN SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1;
SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1;",
    );
}

#[test]
fn virtual_tables_read_later_tables() {
    same(
        "vtab",
        "CREATE TABLE t(n INT, j TEXT);
INSERT INTO t VALUES(2,'[1,2]'),(3,'[5]');
CREATE VIRTUAL TABLE f USING fts5(x);
INSERT INTO f VALUES('a b'),('b c'),('c d');
CREATE TABLE m(q TEXT);
INSERT INTO m VALUES('b'),('c');",
        "SELECT x, q FROM f, m WHERE f MATCH m.q;
EXPLAIN QUERY PLAN SELECT x, q FROM f, m WHERE f MATCH m.q;
SELECT value, n FROM generate_series(1, t.n), t;
EXPLAIN QUERY PLAN SELECT value, n FROM generate_series(1, t.n), t;
SELECT key, value, n FROM json_each(t.j), t;
EXPLAIN QUERY PLAN SELECT key, value, n FROM json_each(t.j), t;",
    );
}

/// The review fixes, as the shell prints them: groups in loop order,
/// likely()-wrapped join terms, UPDATE ... FROM's final table, the
/// statistics held after DROP INDEX, a table-valued argument past a
/// CROSS JOIN.
#[test]
fn review_fix_outputs() {
    same(
        "review",
        "CREATE TABLE t1(id INTEGER PRIMARY KEY, a, b); CREATE INDEX t1ba ON t1(b, a);
INSERT INTO t1 VALUES(1,1,2),(2,2,1),(3,1,1),(4,2,2),(5,3,0);
CREATE TABLE t0(id INTEGER PRIMARY KEY, v); INSERT INTO t0 VALUES(1,1),(2,2),(3,3);
CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v);
CREATE INDEX bax ON b(ax);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300)
INSERT INTO a SELECT i, i%6, i%40 FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300)
INSERT INTO b SELECT i, i%150, i*7%11 FROM n;
CREATE TABLE t(n INT, j TEXT); INSERT INTO t VALUES(2,'[1,2]'),(3,'[5]');",
        "SELECT t1.a, t1.b, count(*) FROM t1, t0 WHERE t0.id = t1.a GROUP BY t1.a, t1.b;
EXPLAIN QUERY PLAN SELECT t1.a, t1.b, count(*) FROM t1, t0 WHERE t0.id = t1.a GROUP BY t1.a, t1.b;
EXPLAIN QUERY PLAN SELECT a.id, b.id FROM b, a WHERE likely(a.id = b.ax);
SELECT a.id, b.id FROM a, b WHERE likely(b.ax = a.y) LIMIT 6;
EXPLAIN QUERY PLAN SELECT a.id, b.id FROM a LEFT JOIN b ON b.ax = a.x WHERE b.v IS DISTINCT FROM NULL;
ANALYZE;
DROP INDEX bax;
EXPLAIN QUERY PLAN SELECT a.id, b.id FROM a, b WHERE a.y = b.ax;
SELECT a.id, b.id FROM a, b WHERE a.y = b.ax LIMIT 6;
EXPLAIN QUERY PLAN UPDATE a SET y = b.v FROM b WHERE b.ax = a.x;
UPDATE a SET y = b.v FROM b WHERE b.ax = a.x;
SELECT * FROM a WHERE id < 9;
SELECT count(*) FROM json_each(t.j) CROSS JOIN t;
EXPLAIN QUERY PLAN SELECT * FROM json_each(t.j) CROSS JOIN t;",
    );
}
