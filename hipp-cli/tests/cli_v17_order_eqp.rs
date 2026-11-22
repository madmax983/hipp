//! hipp-cli v0.17 parity unit `order_eqp`: the ORDER BY / LIMIT fixes
//! and EXPLAIN QUERY PLAN lines as the shell shows them, byte-for-byte
//! the pinned oracle's (SQLite 3.54.1) stdout, failing exactly where it
//! fails: a signed `LIMIT +1` in a scalar / EXISTS subquery, such a
//! subquery planned for the LIMIT the pin leaves it, DISTINCT under a
//! partly delivered ORDER BY stopping at its LIMIT, an IN operator
//! reading its table's index in place of its subquery, a FROM
//! subquery's order with WHERE-fixed columns, and the IN operator's
//! Bloom filter lines, merged VALUES sorters and VALUES chains.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17oe-{tag}"));
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

/// Whether this shell runs its SELECTs on the frozen VM (the hidden
/// `vm` feature, which the workspace's `--features vm` pass turns on):
/// the VM keeps its own sorter, DISTINCT and IN handling (ADR 0005 /
/// 0007), so the row shapes it differs on are checked on today's
/// executor only (and in `tests/order_eqp_v17.rs` under every build).
fn on_vm() -> bool {
    let h = temp_db("probe-vm");
    let (out, _, _) = run_stdin(
        &hipp_bin(),
        &h,
        "CREATE TABLE t(a, c); CREATE INDEX tc ON t(c); INSERT INTO t VALUES (1,2);
SELECT 2 IN (SELECT c FROM t ORDER BY abs(-9223372036854775808));",
    );
    out != "1\n"
}

const SETUP: &str = "CREATE TABLE t(a, b TEXT, c INT, d COLLATE NOCASE, e);
CREATE INDEX tb ON t(b); CREATE INDEX tc ON t(c); CREATE INDEX td ON t(d);
CREATE UNIQUE INDEX te ON t(e);
INSERT INTO t VALUES (1,'1',1,'x',1),(2,'2',2,'y',2),(9,'3',0,'Y',3),(7,'4',0,'z',4);
CREATE TABLE p(a, b); CREATE INDEX pb ON p(b);
INSERT INTO p VALUES (1,0),(7,0),(1,0),(3,1),(5,1),(9,2),(4,3);
CREATE TABLE o(x, y); INSERT INTO o VALUES (1,1),(2,2),(5,3);
CREATE TABLE u(c INT, d TEXT, e); INSERT INTO u VALUES (3,'X',3),(2,'x',2),(3,'y',1);";

#[test]
fn limit_one_shortcut_renders_like_the_pin() {
    same(
        "lim1",
        SETUP,
        "SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT 1);
SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT +1);
SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT ++1);
SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT (1));
SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT 0x1);
SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t LIMIT +1);
SELECT a FROM t WHERE a IN (SELECT a FROM t UNION SELECT a FROM t LIMIT +1);
EXPLAIN QUERY PLAN SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT +1);
EXPLAIN QUERY PLAN SELECT (SELECT a FROM p ORDER BY b, a);
EXPLAIN QUERY PLAN SELECT (SELECT a FROM p ORDER BY b, a LIMIT 2);
EXPLAIN QUERY PLAN SELECT (SELECT a FROM p ORDER BY b, a LIMIT 1);
EXPLAIN QUERY PLAN SELECT EXISTS (SELECT a FROM p ORDER BY b, a LIMIT 2);",
    );
}

#[test]
fn distinct_and_in_index_rows_render_like_the_pin() {
    if on_vm() {
        return;
    }
    same(
        "rows",
        SETUP,
        "SELECT DISTINCT b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END FROM p WHERE b>=0 ORDER BY b, a LIMIT 2;
SELECT DISTINCT b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END FROM p WHERE b>=0 ORDER BY b, -a LIMIT 1 OFFSET 1;
SELECT DISTINCT b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END FROM p WHERE b>=0 ORDER BY b, 2 LIMIT 1;
SELECT 'after the error';
SELECT 2 IN (SELECT rowid FROM t ORDER BY abs(-9223372036854775808));
SELECT 2 IN (SELECT c FROM t ORDER BY abs(-9223372036854775808));
SELECT 'Y' IN (SELECT d FROM t ORDER BY abs(-9223372036854775808));
SELECT x FROM o WHERE x IN (SELECT e FROM t ORDER BY abs(-9223372036854775808));
SELECT 2 IN (SELECT a FROM t ORDER BY abs(-9223372036854775808));
SELECT x FROM o WHERE o.rowid IN (SELECT c FROM t ORDER BY abs(-9223372036854775808));
SELECT 'end';",
    );
}

#[test]
fn plans_render_like_the_pin() {
    same(
        "eqp",
        SETUP,
        "EXPLAIN QUERY PLAN SELECT 2 IN (SELECT c FROM t ORDER BY abs(-9223372036854775808));
EXPLAIN QUERY PLAN SELECT DISTINCT b, a FROM p WHERE b>=0 ORDER BY b, a LIMIT 2;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT c, d FROM u LIMIT 5) WHERE c = 3 ORDER BY c;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT c, d FROM u LIMIT 5) WHERE c = 3 ORDER BY c, d;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT c, d FROM u ORDER BY d LIMIT 5) WHERE c IS NULL ORDER BY c, d;
SELECT * FROM (SELECT c, d FROM u ORDER BY d LIMIT 5) WHERE c = 3 ORDER BY c, d;
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (SELECT c FROM u ORDER BY c);
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (SELECT DISTINCT c FROM u);
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (SELECT c FROM u UNION ALL SELECT e FROM u);
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (SELECT c FROM u UNION SELECT e FROM u UNION ALL SELECT 1);
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (SELECT c FROM u EXCEPT SELECT e FROM u);
EXPLAIN QUERY PLAN SELECT x FROM o WHERE x IN (VALUES (1),(random() IS NOT NULL),(3));
EXPLAIN QUERY PLAN SELECT (VALUES (1),(random() IS NOT NULL));
EXPLAIN QUERY PLAN VALUES (1),(2) UNION VALUES (3),(4);
EXPLAIN QUERY PLAN SELECT c FROM u UNION VALUES (1),(2);
VALUES (1),(2) UNION VALUES (3),(1);",
    );
}

/// Review fixes: a row-value IN the pin runs as a vector IN loop (its
/// ORDER BY runs), one naming the rowid read from an index, a window
/// subquery's dropped ORDER BY, the DISTINCT-to-GROUP-BY rule's NULLS
/// placements, and a DISTINCT repeat that computes no ORDER BY key.
#[test]
fn review_fixes_render_like_the_pin() {
    let setup = "CREATE TABLE r(a INTEGER PRIMARY KEY, b, c TEXT, d); CREATE INDEX rb ON r(b);
CREATE UNIQUE INDEX rc ON r(c);
INSERT INTO r VALUES(1,1,'x',1),(2,1,'y',1),(3,1,'z',2),(4,2,'w',NULL),(8,3,'p',9),(9,3,'q',9),(10,3,'k',1);
CREATE TABLE q(x, y); INSERT INTO q VALUES(1,'x'),(2,'y'),(9,'q');
CREATE TABLE qq(x, y); CREATE INDEX qqy ON qq(y); INSERT INTO qq VALUES(1,'x'),(2,'y'),(9,'q');
CREATE TABLE v(c INT, d TEXT, e); CREATE INDEX vd ON v(d); INSERT INTO v VALUES(3,'a',1),(3,'b',2),(2,'A',3);";
    let plans = "EXPLAIN QUERY PLAN SELECT * FROM (SELECT c, d, e, row_number() OVER (ORDER BY d) r FROM v ORDER BY d LIMIT 6) WHERE c = 3 ORDER BY c, d;
EXPLAIN QUERY PLAN SELECT * FROM (SELECT c, d, e, row_number() OVER (ORDER BY d) r FROM v ORDER BY d LIMIT 6) ORDER BY d;
EXPLAIN QUERY PLAN SELECT DISTINCT b, a+1 FROM r WHERE b>=0 ORDER BY b NULLS LAST, 2 LIMIT 2;
EXPLAIN QUERY PLAN SELECT DISTINCT b, a+1 FROM r WHERE b>=0 ORDER BY b, 2 DESC LIMIT 2;
EXPLAIN QUERY PLAN SELECT x FROM q WHERE (x, y) IN (SELECT a, c FROM r ORDER BY abs(-9223372036854775808));";
    same("review-eqp", setup, plans);
    if on_vm() {
        return;
    }
    same(
        "review-rows",
        setup,
        "SELECT x FROM q WHERE (x, y) IN (SELECT a, c FROM r ORDER BY abs(-9223372036854775808));
SELECT x FROM q WHERE (y, x) IN (SELECT c, rowid FROM r ORDER BY abs(-9223372036854775808));
SELECT 'loop';
SELECT x FROM qq WHERE (y, x) IN (SELECT c, b FROM r ORDER BY abs(-9223372036854775808));
SELECT DISTINCT b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END FROM r WHERE b>=0 ORDER BY b NULLS LAST, 2 LIMIT 2;
SELECT DISTINCT b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END FROM r WHERE b>=0 ORDER BY b, 2 DESC NULLS FIRST LIMIT 2;
SELECT DISTINCT b FROM r WHERE b>=0 ORDER BY b, CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END LIMIT 3;
SELECT DISTINCT d FROM r ORDER BY CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END;
SELECT * FROM (SELECT c, d, e, row_number() OVER (ORDER BY d) r FROM v ORDER BY d LIMIT 6) WHERE c = 3 ORDER BY c, d;",
    );
}
