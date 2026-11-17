//! hipp-cli v0.17 parity unit `order_offset`: the ORDER BY / OFFSET
//! fixes as the shell shows them, byte-for-byte the pinned oracle's
//! (SQLite 3.54.1) stdout, failing exactly where it fails: the OFFSET
//! skips rows before their result columns are computed (FROM-less
//! cores, subqueries, UNION ALL chains, views, CTEs, windows,
//! aggregates), an ORDER BY past the rowid or a UNIQUE NOT NULL key
//! never computes its later terms, a FROM subquery's ORDER BY the pin
//! omits is dropped, and a co-routine's own order delivers its
//! reader's ORDER BY whatever the reader's collation.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17oo-{tag}"));
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

const SETUP: &str = "CREATE TABLE t(c TEXT COLLATE NOCASE, n INT);
INSERT INTO t VALUES ('b',1),('A',2),('C',3),('a',4),(NULL,5);
CREATE TABLE w(a INT NOT NULL, b TEXT); CREATE UNIQUE INDEX wa ON w(a);
INSERT INTO w VALUES (3,'x'),(1,'y'),(2,'x'),(5,'z');
CREATE TABLE t2(x TEXT COLLATE NOCASE, n INT);
INSERT INTO t2 VALUES ('a',1),('B',5),('A',3),('b',2),('c',4),('C',0);";

#[test]
fn offsets_render_like_the_pin() {
    same(
        "off",
        SETUP,
        "SELECT abs(-9223372036854775808) LIMIT 1 OFFSET 1;
SELECT 'next';
SELECT abs(-9223372036854775808) UNION ALL SELECT 2 LIMIT 1 OFFSET 1;
SELECT * FROM (SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1);
WITH q AS (SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1) SELECT * FROM q;
SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c LIMIT 1 OFFSET 1;
SELECT n, abs(row_number() OVER ()-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1;
SELECT count(*), abs(-9223372036854775808) LIMIT 1 OFFSET 1;
SELECT DISTINCT abs(-9223372036854775808) LIMIT 1 OFFSET 1;
SELECT 'after the error';",
    );
}

#[test]
fn order_by_tails_render_like_the_pin() {
    same(
        "tail",
        SETUP,
        "SELECT n FROM t ORDER BY rowid, abs(-9223372036854775808);
SELECT a FROM w ORDER BY a, abs(-9223372036854775808);
SELECT a, b FROM w ORDER BY a DESC, abs(-9223372036854775808);
SELECT t.n, w.a FROM t, w ORDER BY t.rowid, w.a, abs(-9223372036854775808) LIMIT 3;
SELECT b FROM w ORDER BY b, abs(-9223372036854775808);
SELECT 'after the error';
EXPLAIN QUERY PLAN SELECT n FROM t ORDER BY rowid, random();
EXPLAIN QUERY PLAN SELECT a FROM w ORDER BY a, abs(-9223372036854775808);",
    );
}

#[test]
fn subquery_orders_render_like_the_pin() {
    same(
        "sub",
        SETUP,
        "WITH q(x) AS (SELECT 2.5 UNION SELECT c FROM t) SELECT x FROM q ORDER BY 1;
EXPLAIN QUERY PLAN WITH q(x) AS (SELECT 2.5 UNION SELECT c FROM t) SELECT x FROM q ORDER BY 1;
SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x COLLATE BINARY;
SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x;
EXPLAIN QUERY PLAN SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x;
SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY x;
SELECT sum(n), group_concat(x) FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY 1;
SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC), (SELECT 1);",
    );
}

#[test]
fn one_shot_arguments_like_the_pin() {
    same_args(
        "args",
        &[
            "SELECT abs(-9223372036854775808) LIMIT 1 OFFSET 1",
            "SELECT 1 UNION ALL SELECT abs(-9223372036854775808) LIMIT 1 OFFSET 1",
            "SELECT abs(-9223372036854775808) UNION ALL SELECT 2 LIMIT 1 OFFSET 1",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT 'b' UNION SELECT 'A' UNION SELECT 'C') ORDER BY 1",
        ],
    );
}
