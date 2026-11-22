//! hipp-cli v0.17 differentials for the `stmt_txn` unit (leaf
//! `attach`, the pragma tables with `vtab`): `pragma_integrity_check`
//! / `pragma_quick_check` as table-valued functions (argument forms,
//! hidden columns, plans), writes of attached and TEMP tables reading
//! other databases' views, and WITHOUT ROWID tables of other databases
//! searched by their keys, must render byte-identically to the pinned
//! oracle (one-shot and over stdin, in twin directories); every
//! database file hipp writes must pass the oracle's integrity check.

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

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17stx-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::canonicalize(&dir).expect("canonical dir")
}

fn run(bin: &Path, dir: &Path, sql: &str) -> String {
    let out = Command::new(bin)
        .arg("main.db")
        .arg(sql)
        .current_dir(dir)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace(dir.to_string_lossy().as_ref(), "{d}")
}

fn run_stdin(bin: &Path, dir: &Path, input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg("main.db")
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    String::from_utf8_lossy(&out.stdout).replace(dir.to_string_lossy().as_ref(), "{d}")
}

fn integrity_ok(dir: &Path) {
    for e in std::fs::read_dir(dir).expect("dir") {
        let p = e.expect("entry").path();
        if p.extension().is_some_and(|x| x == "db") {
            let out = Command::new(oracle())
                .arg(&p)
                .arg("PRAGMA integrity_check;")
                .output()
                .expect("oracle");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "ok\n",
                "{}",
                p.display()
            );
        }
    }
}

/// One batch of statements (no dot-commands), byte-compared one-shot
/// and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_dir(&format!("{tag}-hipp"));
    let b = temp_dir(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    let want = run(&oracle(), &b, batch);
    assert_eq!(got, want, "{tag}: one-shot output differs:\n{batch}");
    integrity_ok(&a);
    let a = temp_dir(&format!("{tag}-hipp-in"));
    let b = temp_dir(&format!("{tag}-ora-in"));
    let input = batch.replace("; ", ";\n") + "\n";
    let got = run_stdin(&hipp_bin(), &a, &input);
    let want = run_stdin(&oracle(), &b, &input);
    assert_eq!(got, want, "{tag}: stdin output differs:\n{batch}");
    integrity_ok(&a);
}

#[test]
fn integrity_check_tables() {
    let mut batch = String::from(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2); CREATE INDEX ti ON t(b); \
         ATTACH 'a.db' AS aux; CREATE TABLE aux.u(x PRIMARY KEY, y) WITHOUT ROWID; \
         INSERT INTO aux.u VALUES (1, 2); CREATE TEMP TABLE tt(q); ",
    );
    for q in [
        "SELECT * FROM pragma_integrity_check",
        "SELECT * FROM pragma_quick_check('t')",
        "SELECT * FROM pragma_quick_check('u')",
        "SELECT * FROM pragma_quick_check('tt')",
        "SELECT * FROM pragma_quick_check(NULL, 'aux')",
        "SELECT *, arg, schema FROM pragma_integrity_check('t', 'main')",
        "SELECT rowid, quick_check, typeof(arg) FROM pragma_quick_check(x'75')",
        "SELECT * FROM aux.pragma_quick_check",
        "SELECT n, quick_check FROM (SELECT 't' AS n UNION ALL SELECT 'u') JOIN pragma_quick_check(n)",
        "SELECT count(*) FROM t, pragma_quick_check",
        "SELECT * FROM pragma_quick_check WHERE arg = 'u'",
        "SELECT * FROM t WHERE (SELECT integrity_check FROM pragma_integrity_check) = 'ok'",
    ] {
        batch.push_str(&format!("EXPLAIN QUERY PLAN {q}; {q}; "));
    }
    batch.push_str("SELECT name FROM pragma_database_list; ");
    byte_identical("ictvf", &batch);
}

#[test]
fn writes_reading_other_databases_views() {
    byte_identical(
        "views",
        "CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT COLLATE NOCASE, z); \
         INSERT INTO m VALUES (1, 'A', 1.5), (2, 'b', '7'), (3, 'C', NULL); \
         CREATE VIEW mv(k, label, zz) AS SELECT x, y, z * 2 FROM m WHERE x < 3; \
         CREATE VIEW mv2 AS SELECT k * 10 AS kk, label FROM mv; \
         ATTACH 'a.db' AS aux; \
         CREATE TABLE aux.t(a INTEGER PRIMARY KEY, c, d TEXT); \
         INSERT INTO aux.t VALUES (1, NULL, 'a'), (2, NULL, 'B'), (3, NULL, 'c'); \
         CREATE TABLE aux.base(p, q); INSERT INTO aux.base VALUES (1, 'one'), (2, 'two'); \
         CREATE VIEW aux.av AS SELECT p, upper(q) AS uq FROM base; \
         CREATE TEMP TABLE tt(k, v); INSERT INTO tt VALUES (1, 0), (2, 0), (5, 0); \
         CREATE TEMP VIEW tv AS SELECT t.a, t.d FROM aux.t; \
         UPDATE aux.t SET c = (SELECT zz FROM mv WHERE mv.k = t.a); SELECT * FROM aux.t; \
         UPDATE aux.t SET d = (SELECT label FROM main.mv WHERE k = t.a) WHERE a < 3; SELECT * FROM aux.t; \
         INSERT INTO aux.t(c, d) SELECT kk, label FROM mv2; SELECT * FROM aux.t; SELECT changes(); \
         UPDATE tt SET v = (SELECT uq FROM aux.av WHERE p = k); SELECT * FROM tt; \
         UPDATE tt SET v = (SELECT d FROM tv WHERE tv.a = tt.k); SELECT * FROM tt; \
         DELETE FROM aux.t WHERE a IN (SELECT k FROM mv); SELECT * FROM aux.t; \
         SELECT changes(), total_changes();",
    );
}

#[test]
fn without_rowid_tables_of_other_databases() {
    let mut batch = String::from(
        "CREATE TABLE m(x INTEGER PRIMARY KEY, y); INSERT INTO m VALUES (1, 10), (2, 20), (3, 30); \
         ATTACH 'a.db' AS aux; \
         CREATE TABLE aux.w(k, v, z, PRIMARY KEY(k, v)) WITHOUT ROWID; \
         INSERT INTO aux.w VALUES (1, 'a', 100), (1, 'b', 101), (2, 'a', 200), (3, 'c', 300); \
         CREATE INDEX aux.wz ON w(z); \
         CREATE TEMP TABLE tw(k PRIMARY KEY, v) WITHOUT ROWID; INSERT INTO tw VALUES (1, 't1'), (2, 't2'); ",
    );
    for q in [
        "SELECT * FROM m JOIN aux.w ON w.k = m.x",
        "SELECT (SELECT z FROM aux.w WHERE k = m.x AND v = 'a') FROM m",
        "SELECT * FROM aux.w, m WHERE w.z = m.y * 10",
        "SELECT * FROM m, tw WHERE tw.k = m.x",
        "SELECT tw.k, w.z FROM tw JOIN aux.w ON w.k = tw.k",
    ] {
        batch.push_str(&format!("EXPLAIN QUERY PLAN {q}; {q}; "));
    }
    batch.push_str(
        "UPDATE m SET y = (SELECT z FROM aux.w WHERE w.k = m.x AND w.v = 'a'); SELECT * FROM m;",
    );
    byte_identical("wr", &batch);
}

/// The headers of TEMP views the temp database cannot plan (over the
/// pragma check tables, over main's tables) are named from the view's
/// text, as the pin names them (review fix: `column1`, ...), and the
/// IN forms of the check tables' arguments (review fix: no rows).
#[test]
fn temp_view_headers_and_in_arguments() {
    let input = "CREATE TABLE t(a); INSERT INTO t VALUES (1);\n\
         CREATE TABLE u(c, d);\n\
         .headers on\n\
         CREATE TEMP VIEW tv AS SELECT * FROM pragma_quick_check;\n\
         CREATE TEMP VIEW tv2(c1) AS SELECT * FROM pragma_quick_check;\n\
         CREATE TEMP VIEW tv3 AS SELECT quick_check AS qc FROM pragma_quick_check;\n\
         CREATE TEMP VIEW tv4 AS SELECT quick_check, arg FROM pragma_quick_check('t');\n\
         CREATE TEMP VIEW tm AS SELECT a, a + 1 AS b FROM t;\n\
         CREATE TEMP VIEW tm2 AS SELECT * FROM tm;\n\
         SELECT * FROM tv;\n\
         SELECT * FROM tv2;\n\
         SELECT * FROM tv3;\n\
         SELECT * FROM tv4;\n\
         SELECT * FROM tv, tv3;\n\
         SELECT * FROM (SELECT * FROM tv);\n\
         SELECT * FROM tm;\n\
         SELECT * FROM tm2;\n\
         SELECT * FROM pragma_quick_check WHERE arg IN ('t', 'u');\n\
         SELECT arg, * FROM pragma_quick_check WHERE arg = 'u' OR arg = 't';\n\
         SELECT * FROM pragma_quick_check WHERE arg = 't' AND schema IN ('main', 'main');\n";
    let a = temp_dir("tvh-hipp");
    let b = temp_dir("tvh-ora");
    let got = run_stdin(&hipp_bin(), &a, input);
    let want = run_stdin(&oracle(), &b, input);
    assert_eq!(got, want);
    assert!(got.contains("quick_check|arg"), "{got}");
}
