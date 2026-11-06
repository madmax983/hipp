//! hipp-cli v0.17 streamed rows: byte-for-byte the pinned oracle's
//! stdout when a statement fails part-way — the shell prints the rows
//! the statement returned before its error (then reports the error on
//! stderr and goes on with the next statement in a script read from
//! stdin), over plain scans, joins, virtual tables, JSON table-valued
//! functions and LIMIT / OFFSET.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17st-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

/// A script on stdin: stdout, and whether anything reached stderr.
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
        !out.stderr.is_empty(),
    )
}

/// One-shot: stdout (the shell stops at the first error).
fn run_once(bin: &Path, db: &Path, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// `script` on fresh twin files, over stdin and one-shot: identical
/// stdout; `errs` says whether the script must report an error.
fn same(tag: &str, script: &str, errs: bool) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let (a, ae) = run_stdin(&oracle(), &o, script);
    let (b, be) = run_stdin(&hipp_bin(), &h, script);
    assert_eq!(b, a, "script:\n{script}");
    assert_eq!(ae, errs, "pin stderr: {script}");
    assert_eq!(be, errs, "hipp stderr: {script}");
    let h = temp_db(&format!("{tag}-h1"));
    let o = temp_db(&format!("{tag}-o1"));
    assert_eq!(
        run_once(&hipp_bin(), &h, script),
        run_once(&oracle(), &o, script),
        "one-shot:\n{script}"
    );
}

const SETUP: &str = "\
CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT);
INSERT INTO t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, 30, 'z'), (4, 40, 'w');
CREATE TABLE u(k, v);
INSERT INTO u VALUES (1, 'p'), (2, 'q'), (3, 'r');
";

#[test]
fn rows_before_an_error_then_the_next_statement() {
    let script = format!(
        "{SETUP}\
SELECT a, json(CASE WHEN a = 3 THEN 'x' ELSE a END) FROM t;
SELECT 'after';
SELECT t.a, u.v FROM t, u WHERE json(CASE WHEN t.a = 2 AND u.k = 2 THEN 'x' ELSE 1 END);
SELECT a FROM t WHERE abs(CASE WHEN a = 4 THEN -9223372036854775807 - 1 ELSE a END) > 0;
SELECT DISTINCT b / 20, json(CASE WHEN a = 4 THEN 'x' ELSE 1 END) FROM t;
SELECT a, json(CASE WHEN a = 3 THEN 'x' ELSE 1 END) FROM t ORDER BY a;
SELECT a, json(CASE WHEN a = 3 THEN 'x' ELSE 1 END) FROM t ORDER BY c;
SELECT 'end';
"
    );
    same("scan", &script, true);
}

#[test]
fn virtual_tables_and_table_valued_functions() {
    let script = format!(
        "{SETUP}\
SELECT value, json(CASE WHEN value = 4 THEN 'x' ELSE value END) FROM generate_series(1, 9);
SELECT t.a, s.value FROM t, generate_series(1, t.a) s WHERE json(CASE WHEN t.a = 3 AND s.value = 2 THEN 'x' ELSE 1 END);
SELECT t.a, j.value FROM t, json_each(CASE WHEN t.a = 3 THEN '[1,' ELSE '[7,8]' END) j;
CREATE VIRTUAL TABLE r USING rtree(id, x0, x1);
INSERT INTO r SELECT value, value, value + 1 FROM generate_series(1, 30);
SELECT id, x1 FROM r WHERE x0 > 5 AND json(CASE WHEN id = 9 THEN 'x' ELSE 1 END);
CREATE VIRTUAL TABLE f USING fts5(body);
INSERT INTO f(rowid, body) SELECT value, 'w' || (value % 3) FROM generate_series(1, 12);
SELECT rowid, body FROM f WHERE f MATCH 'w1' AND json(CASE WHEN rowid = 7 THEN 'x' ELSE 1 END);
SELECT 'end';
"
    );
    same("vtab", &script, true);
}

#[test]
fn limit_and_offset() {
    let script = format!(
        "{SETUP}\
SELECT value FROM generate_series(1) LIMIT 3;
SELECT value FROM generate_series(1) WHERE value % 5 = 0 LIMIT 2 OFFSET 3;
SELECT a.value, b.value FROM generate_series(1) a, generate_series(1, 2) b LIMIT 5;
SELECT json(CASE WHEN a <= 2 THEN 'x' ELSE a END) FROM t LIMIT 2 OFFSET 2;
SELECT a, json(CASE WHEN a = 3 THEN 'x' ELSE 1 END) FROM t LIMIT 2;
SELECT count(*), sum(value) FROM generate_series(1, 300000);
"
    );
    same("limit", &script, false);
}

/// A script on stdin: stdout and the exit status.
fn run_stdin_status(bin: &Path, db: &Path, input: &str) -> (String, Option<i32>) {
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
        out.status.code(),
    )
}

#[test]
fn a_failed_statement_sets_the_exit_status() {
    // Regression (review): a stdin script goes on after a failing
    // statement (rows before the error printed), and the pin's shell
    // then exits 1 — at the end of the script, or at `.exit`. hipp-cli
    // exited 0, and ran the statements after the failing one on its
    // input line.
    for (tag, script) in [
        // The rest of the failing input chunk is skipped, then the
        // script goes on.
        ("fail", "SELECT abs(-9223372036854775808); SELECT 1;\n"),
        (
            "chunks",
            "SELECT 1;\nSELECT abs(-9223372036854775808); SELECT 2;\nSELECT 3; SELECT json(1\n); SELECT 4;\nSELECT 5;\n",
        ),
        (
            "part-way",
            "SELECT value, abs(CASE WHEN value = 3 THEN -9223372036854775807 - 1 ELSE value END) FROM generate_series(1, 5);\nSELECT 'next';\n",
        ),
        ("exit", "SELECT abs(-9223372036854775808);\n.exit\n"),
        ("quit", "SELECT json('x');\n.quit\n"),
        ("dot", ".nosuchcommand\nSELECT 1;\n"),
        ("parse", "SELEC 1;\nSELECT 2;\n"),
        ("trailing", "SELECT 1;\nSELECT abs(-9223372036854775808)"),
        ("ok", "SELECT 1;\nSELECT 1 / 0;\n"),
        ("ok-exit", "SELECT 1;\n.exit\n"),
    ] {
        let h = temp_db(&format!("status-{tag}-h"));
        let o = temp_db(&format!("status-{tag}-o"));
        let pin = run_stdin_status(&oracle(), &o, script);
        let ours = run_stdin_status(&hipp_bin(), &h, script);
        assert_eq!(ours, pin, "script:\n{script}");
    }
}
