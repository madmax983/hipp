//! hipp-cli v0.17 differentials for index and rowid access into
//! attached and TEMP tables (leaf `attach`): EXPLAIN QUERY PLAN trees
//! that search another database's table through its rowid, its
//! indexes, an automatic index or a MULTI-INDEX OR, joins driven from
//! the TEMP or attached table, and the rows of those statements, must
//! render byte-identically to the pinned oracle (one-shot and over
//! stdin, in twin directories); every database file hipp writes must
//! pass the oracle's integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17acc-{tag}"));
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

const SETUP: &str = "ATTACH 'a.db' AS aux; \
     CREATE TABLE m(x INTEGER PRIMARY KEY, y, z); \
     CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c); \
     CREATE INDEX aux.tb ON t(b); CREATE INDEX aux.tc ON t(c DESC) WHERE c > 0; \
     CREATE TEMP TABLE tt(k INTEGER PRIMARY KEY, v); CREATE INDEX temp.tv ON tt(v); \
     INSERT INTO m VALUES (1, 'a', 1), (2, 'B', 2), (3, 'c', 3), (4, NULL, 4); \
     INSERT INTO aux.t VALUES (1, 'A', 1), (2, 'b', 2), (3, 'C', -3), (4, NULL, 4); \
     INSERT INTO tt VALUES (1, 'a'), (2, 'b'), (3, 'A'); ";

#[test]
fn plans_into_other_databases() {
    let mut batch = SETUP.to_string();
    for q in [
        "SELECT (SELECT c FROM aux.t WHERE a = m.x) FROM m",
        "SELECT * FROM m, aux.t WHERE t.a = m.x",
        "SELECT m.x, t.a FROM m JOIN aux.t ON t.b = m.y",
        "SELECT * FROM m, aux.t WHERE t.c = m.z AND t.c > 0",
        "SELECT * FROM m, aux.t WHERE t.a BETWEEN m.x AND m.x + 1",
        "SELECT * FROM m, tt WHERE tt.k = m.x",
        "SELECT * FROM m, temp.tt WHERE tt.v = m.y",
        "SELECT * FROM tt, m WHERE m.x = tt.k AND tt.v = 'b'",
        "SELECT * FROM aux.t, tt WHERE t.b = tt.v",
        "SELECT * FROM m WHERE y IN (SELECT b FROM aux.t)",
        "SELECT * FROM m WHERE EXISTS (SELECT 1 FROM tt WHERE k = m.x)",
        "SELECT * FROM aux.t, m WHERE t.a = m.x OR t.b = m.y",
        "SELECT * FROM m LEFT JOIN aux.t ON t.a = m.x ORDER BY t.a DESC",
        "SELECT * FROM m, aux.t INDEXED BY tc WHERE t.c > 0 AND t.a = m.x",
    ] {
        batch.push_str(&format!("EXPLAIN QUERY PLAN {q}; {q}; "));
    }
    byte_identical("plans", &batch);
}

#[test]
fn writes_reading_other_databases_through_indexes() {
    byte_identical(
        "writes",
        &format!(
            "{SETUP}UPDATE m SET z = (SELECT c FROM aux.t WHERE a = m.x); \
             DELETE FROM m WHERE x IN (SELECT k FROM tt WHERE v = 'b'); \
             INSERT INTO m SELECT a + 10, b, c FROM aux.t WHERE a IN (SELECT k FROM tt); \
             SELECT * FROM m; SELECT changes(), total_changes();"
        ),
    );
}

/// Review fixes: a later LEFT JOIN's ON term proves no partial index
/// of an earlier source (rows were lost), and a generated column read
/// through an index is no COVERING INDEX, as on the pin.
#[test]
fn partial_on_terms_and_generated_cover() {
    let mut batch = String::from(
        "ATTACH 'a.db' AS aux; \
         CREATE TABLE aux.t(id, a); INSERT INTO aux.t VALUES (1, NULL), (2, 5), (3, NULL); \
         CREATE INDEX aux.ti ON t(id) WHERE a IS NOT NULL; \
         CREATE TABLE m(k); INSERT INTO m VALUES (1); \
         CREATE TEMP TABLE g(a INTEGER PRIMARY KEY, b, c AS (b * 2), d AS (b || 'x') STORED, e); \
         CREATE INDEX temp.ge ON g(e, d); INSERT INTO g(b, e) VALUES (1, 'dflt'), (2, 'dflt'), (3, 'z'); ",
    );
    for q in [
        "SELECT t.id FROM aux.t LEFT JOIN m ON t.a = 5",
        "SELECT count(*) FROM aux.t LEFT JOIN m ON t.a = 5",
        "SELECT t.id FROM m JOIN aux.t LEFT JOIN m m2 ON t.a = 5",
        "SELECT t.id, m.k FROM m LEFT JOIN aux.t ON t.a = 5",
        "SELECT a, d FROM g WHERE e = 'dflt' ORDER BY d DESC",
        "SELECT e FROM g WHERE e = 'dflt'",
        "SELECT count(*) FROM g WHERE d = '1x'",
    ] {
        batch.push_str(&format!("EXPLAIN QUERY PLAN {q}; {q}; "));
    }
    byte_identical("review", &batch);
}
