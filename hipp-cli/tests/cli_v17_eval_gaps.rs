//! hipp-cli v0.17 parity unit `gaps_eval`: the evaluator and
//! statement-level fixes as the shell shows them, byte-for-byte the
//! pinned oracle's (SQLite 3.54.1) stdout, and failing exactly where
//! it fails: `hex(NULL)` is `''`, `substr` of an empty BLOB is NULL,
//! a FROM-less core never computes its ORDER BY terms, LIMIT 0 skips
//! its OFFSET and the whole query, the WHERE clause's constant
//! propagation covers the rowid, a neutral AND / OR operand is
//! simplified away, and empty statements (`;`, `;;`, comments) in
//! one-shot and stdin scripts are skipped.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17eg-{tag}"));
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

const SETUP: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT);
INSERT INTO t VALUES (1, 1, '1'), (5, x'', ''), (9, NULL, NULL), (12, 'abc', 'abc');";

#[test]
fn hex_and_substr_render_like_the_pin() {
    same(
        "hs",
        SETUP,
        "SELECT hex(NULL), typeof(hex(NULL)), quote(hex(NULL));
SELECT quote(substr(x'', 'NULL')), quote(substr(x'', 0)), quote(substr('', 'a')), quote(substr('', 0));
SELECT substr(x'', 1) IS NULL, substr('', 1) IS NULL, typeof(substr(zeroblob(0), 1));
SELECT a, hex(b), quote(substr(b, 1)), quote(substr(c, 1)) FROM t;
SELECT a FROM t WHERE substr(b, 1) IS NULL;",
    );
}

#[test]
fn from_less_order_by_and_limit_render_like_the_pin() {
    same(
        "fl",
        SETUP,
        "SELECT 1 ORDER BY abs(-9223372036854775808);
SELECT 1, 2 ORDER BY 2 DESC, abs(-9223372036854775808);
SELECT DISTINCT 1 ORDER BY abs(-9223372036854775808);
SELECT (SELECT 1 ORDER BY abs(-9223372036854775808));
SELECT * FROM (SELECT 1 ORDER BY abs(-9223372036854775808));
SELECT 1 ORDER BY count(*);
SELECT 1 LIMIT 0 OFFSET abs(-9223372036854775808);
SELECT 'after';
SELECT abs(-9223372036854775808) UNION ALL SELECT 2 LIMIT 0;
SELECT a FROM t LIMIT 0 OFFSET 'x';
SELECT (SELECT a FROM t LIMIT 0 OFFSET 'x');
SELECT 1 GROUP BY 1 ORDER BY abs(-9223372036854775808);
SELECT 'still running';",
    );
}

#[test]
fn constant_propagation_and_and_or_render_like_the_pin() {
    same(
        "cp",
        SETUP,
        "SELECT * FROM t WHERE a = 5 AND a < 5 AND abs(-9223372036854775808);
SELECT count(*) FROM t WHERE rowid = 5 AND a < 5 AND abs(-9223372036854775808);
SELECT * FROM t WHERE a = 5 AND a IN (1) AND abs(-9223372036854775808);
SELECT * FROM t WHERE a = 5 AND a < 6 AND abs(-9223372036854775808);
SELECT 'next';
SELECT abs(-9223372036854775808) AND (0 OR 0);
SELECT a FROM t WHERE abs(-9223372036854775808) AND (1 AND 0);
SELECT a, a AND (0 OR 0), a OR (1 AND 1) FROM t;",
    );
}

#[test]
fn empty_statements_in_stdin_scripts_like_the_pin() {
    same("e1", "", ";");
    same("e2", "", ";;");
    same("e3", "", "-- only a comment");
    same("e4", "", "/* x */ ;\n;\n");
    same(
        "e5",
        "CREATE TABLE z(a);",
        "INSERT INTO z VALUES(1);;INSERT INTO z VALUES(2);
;
;;
SELECT * FROM z;;
SELECT 1 /* a */ ; /* b */ ; SELECT 2;
BEGIN;;COMMIT;
INSERT INTO z VALUES(7); ; SELECT changes(), total_changes(), last_insert_rowid();",
    );
    // An error after an empty statement still stops that input chunk.
    same("e6", "", "SELECT 1;; SELEC 2;\nSELECT 3;");
}

#[test]
fn empty_statements_in_one_shot_scripts_like_the_pin() {
    same_args(
        "a",
        &[
            ";",
            ";;",
            "SELECT 1;;SELECT 2;",
            ";SELECT 1",
            "SELECT 1; ; ;",
            "  ;  -- c",
            "/* x */ ;",
            "CREATE TABLE t(a);;INSERT INTO t VALUES(1);;;SELECT * FROM t;",
            "BEGIN;;COMMIT;",
            "SELECT 1;;SELEC 2;",
            "EXPLAIN QUERY PLAN ;",
            "CREATE TEMP TABLE x(a);; INSERT INTO x VALUES(1);; SELECT * FROM x;;",
        ],
    );
}
