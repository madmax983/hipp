//! hipp-cli v0.17 differentials for the ATTACH parity unit (leaf
//! `attach`): string-literal schema qualifiers, the integrity check
//! over every database, attached views read beside other databases'
//! tables, TEMP writes reading main and attached tables, and writes of
//! an attached table reading same-named tables elsewhere must render
//! byte-identically to the pinned oracle (one-shot and over stdin, in
//! twin directories); every database file hipp writes must pass the
//! oracle's integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17att-{tag}"));
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
fn string_qualifiers_and_views() {
    byte_identical(
        "strq",
        "ATTACH 'a.db' AS aux; CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT); \
         INSERT INTO m VALUES (1, 'p'), (2, 'q'), (3, 'r'); \
         CREATE TABLE 'aux'.t(a INTEGER PRIMARY KEY, b REAL); \
         INSERT INTO 'aux'.'t' VALUES (1, 1.5), (2, 2e20), (5, -0.25); \
         SELECT 'aux'.t.a, \"aux\".'t'.b FROM 'aux'.t; PRAGMA 'aux'.table_info('t'); \
         CREATE VIEW v AS SELECT 'main' AS w; CREATE VIEW aux.v AS SELECT a AS w, b FROM t WHERE a < 5; \
         SELECT * FROM aux.v, m WHERE w = x; SELECT * FROM v, aux.v; \
         SELECT x, (SELECT b FROM aux.v WHERE w = x) FROM m; \
         INSERT INTO m(y) SELECT b FROM aux.v; SELECT * FROM m;",
    );
}

#[test]
fn temp_and_attached_writes_reading_elsewhere() {
    byte_identical(
        "writes",
        "ATTACH 'a.db' AS aux; CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT); \
         INSERT INTO m VALUES (1, 'p'), (2, 'q'); \
         CREATE TABLE aux.m(x, y); INSERT INTO aux.m VALUES (1, 'a1'), (2, 'a2'); \
         CREATE TEMP TABLE tt(k, v); INSERT INTO tt VALUES (1, NULL), (2, NULL), (3, NULL); \
         UPDATE tt SET v = (SELECT y FROM m WHERE x = k) || (SELECT y FROM aux.m WHERE x = k); \
         SELECT * FROM tt; DELETE FROM tt WHERE k NOT IN (SELECT x FROM main.m); \
         INSERT INTO tt VALUES ((SELECT count(*) FROM aux.m), 1.0) RETURNING *; \
         UPDATE aux.m SET y = (SELECT y FROM m WHERE m.x = aux.m.x) || '/' || aux.m.y; \
         UPDATE aux.m SET y = main.m.y || '!' FROM main.m WHERE main.m.x = aux.m.x AND aux.m.x = 2; \
         SELECT * FROM aux.m; SELECT changes(), total_changes();",
    );
}

#[test]
fn integrity_check_every_database() {
    let corrupt = |d: &Path| {
        let out = Command::new(oracle())
            .arg("-cmd")
            .arg(".dbconfig defensive off")
            .arg(d.join("a.db"))
            .arg(
                "CREATE TABLE t(x, y); INSERT INTO t VALUES (NULL, 1), (2, 2); \
                 CREATE INDEX ti ON t(y); PRAGMA writable_schema = ON; \
                 UPDATE sqlite_schema SET sql = 'CREATE TABLE t(x NOT NULL, y CHECK(y > 1))' WHERE name = 't'; \
                 UPDATE sqlite_schema SET sql = 'CREATE INDEX ti ON t(x)' WHERE name = 'ti';",
            )
            .output()
            .expect("oracle");
        assert!(out.status.success());
    };
    let batch = "CREATE TABLE q(x); ATTACH 'a.db' AS aux; ATTACH ':memory:' AS mm; \
                 CREATE TABLE mm.k(z NOT NULL); INSERT INTO mm.k VALUES (1); \
                 PRAGMA integrity_check; PRAGMA integrity_check(2); PRAGMA quick_check; \
                 PRAGMA aux.integrity_check; PRAGMA main.integrity_check; PRAGMA mm.quick_check; \
                 DETACH aux; PRAGMA integrity_check;";
    let a = temp_dir("ic-hipp");
    let b = temp_dir("ic-ora");
    corrupt(&a);
    corrupt(&b);
    assert_eq!(run(&hipp_bin(), &a, batch), run(&oracle(), &b, batch));
    let a = temp_dir("ic-hipp-in");
    let b = temp_dir("ic-ora-in");
    corrupt(&a);
    corrupt(&b);
    let input = batch.replace("; ", ";\n") + "\n";
    assert_eq!(
        run_stdin(&hipp_bin(), &a, &input),
        run_stdin(&oracle(), &b, &input)
    );
}

/// Review fixes: WITH-prefixed writes write their target's database
/// (not the CTE's), TEMP writes fed by WITH / RETURNING reading main,
/// string names in CTE / trigger / SET positions, and the temp
/// database the unqualified integrity check opens.
#[test]
fn with_prefixed_writes_and_string_names() {
    byte_identical(
        "with",
        "ATTACH 'a.db' AS aux; CREATE TABLE m(x); INSERT INTO m VALUES (1), (2), (3); \
         CREATE TABLE aux.m(x); INSERT INTO aux.m VALUES (10), (2); \
         CREATE TEMP TABLE t(y); INSERT INTO t VALUES (2); \
         CREATE TEMP TABLE m(x); INSERT INTO temp.m VALUES (2), (7); \
         WITH c AS (SELECT y FROM temp.t) DELETE FROM main.m WHERE x IN (SELECT y FROM c); \
         WITH c AS (SELECT y FROM temp.t) UPDATE aux.m SET x = x + 100; \
         WITH c AS (SELECT x FROM main.m) UPDATE t SET y = (SELECT max(x) FROM c); \
         WITH c AS (SELECT x FROM m) INSERT INTO t SELECT x + 500 FROM c; \
         INSERT INTO t SELECT x + 600 FROM main.m RETURNING y; \
         SELECT 'main', * FROM main.m; SELECT 'aux', * FROM aux.m; \
         SELECT 'temp', * FROM temp.m; SELECT 'temp', * FROM temp.t; \
         WITH 'c'('z') AS (SELECT 1) SELECT z FROM c; \
         CREATE TRIGGER 'aux'.'tr' AFTER UPDATE OF 'x' ON m BEGIN SELECT 1; END; \
         UPDATE 'aux'.'m' SET 'x' = 4 WHERE x > 100; SELECT * FROM aux.m; \
         PRAGMA integrity_check; PRAGMA database_list;",
    );
}

/// Review fix: `PRAGMA aux.integrity_check` over a B-tree-corrupt
/// attached file prints its report before the error.
#[test]
fn corrupt_attached_btree_report() {
    let corrupt = |d: &Path| {
        let p = d.join("a.db");
        let out = Command::new(oracle())
            .arg(&p)
            .arg(
                "CREATE TABLE t(a, b); CREATE INDEX i ON t(a); \
                 WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 300) \
                 INSERT INTO t SELECT n, printf('%.200c', 'x') FROM r;",
            )
            .output()
            .expect("oracle");
        assert!(out.status.success());
        let mut b = std::fs::read(&p).expect("read");
        b[9 * 4096] = 0;
        b[6 * 4096 + 3] = 0x7f;
        b[6 * 4096 + 4] = 0xff;
        std::fs::write(&p, b).expect("write");
    };
    let input = "ATTACH 'a.db' AS aux;\nPRAGMA aux.integrity_check;\nPRAGMA aux.quick_check(2);\n\
                 PRAGMA integrity_check;\nSELECT 'after';\n";
    let a = temp_dir("btree-hipp");
    let b = temp_dir("btree-ora");
    corrupt(&a);
    corrupt(&b);
    assert_eq!(
        run_stdin(&hipp_bin(), &a, input),
        run_stdin(&oracle(), &b, input)
    );
}
