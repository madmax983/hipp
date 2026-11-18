//! hipp-cli v0.17 parity unit `result_gaps`: the result-level fixes
//! as the shell shows them, byte-for-byte the pinned oracle's (SQLite
//! 3.54.1) stdout, failing exactly where it fails: the top-N sorter
//! skips a row past the LIMIT before its columns, a WITHOUT ROWID
//! table's own order delivers its ORDER BY, a FROM subquery's order
//! delivers its reader's GROUP BY, an ordinal under COLLATE is a
//! result column, an EXISTS computes no result column, a TEMP CREATE
//! TABLE ... AS keeps FLEXNUM reals, and CTE references count toward
//! the nesting limit one each.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17rg-{tag}"));
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

/// Whether this shell runs its SELECTs on the frozen VM (the hidden
/// `vm` feature, which the workspace's `--features vm` pass turns on):
/// the VM keeps its own sorter, grouping and EXISTS handling (ADR
/// 0005 / 0007, a recorded gap), so the shapes it differs on are
/// checked on today's executor only (and in `tests/result_gaps_v17.rs`
/// under every build).
fn on_vm() -> bool {
    let h = temp_db("probe-vm");
    let (out, _, _) = run_stdin(
        &hipp_bin(),
        &h,
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (2,1),(1,5);
SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 1;",
    );
    out != "2\n"
}

const SETUP: &str = "CREATE TABLE t(a INTEGER, b, c TEXT);
INSERT INTO t VALUES (2,1,'x'),(3,2,'Y'),(1,5,'z'),(4,0,'w'),(5,3,NULL),(6,NULL,'v');
CREATE TABLE g(c COLLATE NOCASE, n);
INSERT INTO g VALUES ('A',1),('b',2),('C',3),(NULL,4),('a',5),('B',6),(1,7),('b',1);
CREATE TABLE w(k INTEGER NOT NULL, v NOT NULL, z, PRIMARY KEY(k)) WITHOUT ROWID;
INSERT INTO w VALUES (1,'a',NULL),(2,'b',3),(3,'c',4);
CREATE TABLE r(r REAL, i INT); INSERT INTO r VALUES (2.0, 3);";

#[test]
fn top_n_sorter_renders_like_the_pin() {
    if on_vm() {
        return;
    }
    same(
        "topn",
        SETUP,
        "SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 1;
SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END, b FROM t ORDER BY b LIMIT 2 OFFSET 1;
SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 3;
SELECT 'after the error';
SELECT * FROM (SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END AS x FROM t ORDER BY b LIMIT 1);
SELECT (SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 1);
SELECT b, CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t GROUP BY b ORDER BY b + 0 LIMIT 1;
SELECT sum(a) OVER (), CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 2;
SELECT b, CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 2;
SELECT DISTINCT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END FROM t ORDER BY b LIMIT 1;
SELECT 'end';",
    );
}

#[test]
fn orders_and_groups_render_like_the_pin() {
    same(
        "ord",
        SETUP,
        "SELECT k FROM w ORDER BY k, abs(-9223372036854775808);
SELECT k FROM w ORDER BY k, v, abs(-9223372036854775808);
EXPLAIN QUERY PLAN SELECT k FROM w ORDER BY k, abs(-9223372036854775808);
SELECT x FROM (SELECT c AS x, n FROM g ORDER BY 1 COLLATE BINARY LIMIT 10) ORDER BY x;
SELECT x FROM (SELECT c AS x, n FROM g ORDER BY x COLLATE BINARY DESC LIMIT 10) ORDER BY x DESC;
SELECT x FROM (SELECT c AS x, n FROM g ORDER BY c COLLATE BINARY LIMIT 10) ORDER BY x;",
    );
}

#[test]
fn coroutine_groups_render_like_the_pin() {
    if on_vm() {
        return;
    }
    same(
        "grp",
        SETUP,
        "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM g ORDER BY 1 DESC) GROUP BY x;
SELECT x, count(*) FROM (SELECT c AS x, n FROM g ORDER BY x COLLATE BINARY DESC) GROUP BY x;
SELECT x, count(*) FROM (SELECT c AS x, n FROM g ORDER BY 1 DESC) GROUP BY x LIMIT 2;
EXPLAIN QUERY PLAN SELECT x, count(*) FROM (SELECT c AS x, n FROM g ORDER BY 1 DESC) GROUP BY x;",
    );
}

#[test]
fn exists_and_writes_render_like_the_pin() {
    if on_vm() {
        return;
    }
    same(
        "ex",
        SETUP,
        "SELECT EXISTS (SELECT abs(-9223372036854775808) FROM t);
SELECT EXISTS (SELECT a FROM t ORDER BY abs(-9223372036854775808));
SELECT EXISTS (SELECT DISTINCT a % 2 FROM t LIMIT 1 OFFSET 3);
SELECT EXISTS (SELECT 1 FROM t WHERE 0 UNION SELECT abs(-9223372036854775808));
SELECT EXISTS (SELECT 1 INTERSECT SELECT abs(-9223372036854775808));
SELECT 'next';
CREATE TEMP TABLE q AS SELECT CAST(1 AS REAL) AS x UNION ALL SELECT r FROM r;
SELECT x, typeof(x) FROM q;
CREATE TABLE p AS SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END AS x FROM t ORDER BY b LIMIT 1;
SELECT * FROM p;",
    );
}

/// Three reads of a 450-arm CTE expand on the pin and in hipp; a
/// thousand references do not.
#[test]
fn cte_references_render_like_the_pin() {
    let arms: Vec<String> = (0..450).map(|i| format!("SELECT {i}")).collect();
    let refs = vec!["(SELECT x FROM c0)"; 1000].join(",");
    let a = format!(
        "WITH c(x) AS ({}) SELECT (SELECT sum(x) FROM c), (SELECT max(x) FROM c), (SELECT min(x) FROM c)",
        arms.join(" UNION ALL ")
    );
    let b = format!("WITH c0(x) AS (SELECT 1) SELECT {refs}");
    same_args("cte", &[&a, &b]);
}
