//! hipp-cli v0.16 differentials for auto-vacuum files (leaf
//! `autovacuum`): batches on FULL and INCREMENTAL files — page and
//! freelist counts after each commit, root pages after CREATE / DROP
//! moved them, PRAGMA incremental_vacuum's empty rows, PRAGMA
//! auto_vacuum's values, VACUUM keeping or changing the mode — must
//! render byte-identically to the pinned oracle, one-shot and over
//! stdin, and the hipp-written file must pass the oracle's integrity
//! check. A pin-written auto-vacuum file is then written by hipp.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16av-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> Vec<u8> {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run(&oracle(), &b, batch)),
        "{tag}"
    );
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, batch)),
        "{tag} (stdin)"
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// A FULL file: every commit truncates; roots move on DROP.
const FULL: &str = r#"
PRAGMA auto_vacuum = FULL;
PRAGMA auto_vacuum;
CREATE TABLE t(a INTEGER PRIMARY KEY, b);
CREATE TABLE u(x UNIQUE);
CREATE INDEX tb ON u(x, x);
INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < 200) SELECT v, zeroblob(v % 9 * 100) || v FROM s;
INSERT INTO u VALUES (1), (2), (zeroblob(5000));
PRAGMA page_count;
DELETE FROM t WHERE a % 2 = 0;
PRAGMA page_count;
PRAGMA freelist_count;
SELECT name, rootpage FROM sqlite_schema;
DROP TABLE t;
SELECT name, rootpage FROM sqlite_schema;
PRAGMA page_count;
PRAGMA freelist_count;
SELECT count(*), sum(length(x)) FROM u;
PRAGMA integrity_check;
"#;

/// An INCREMENTAL file: free pages stay until incremental_vacuum,
/// which yields one empty row per page.
const INCREMENTAL: &str = r#"
PRAGMA auto_vacuum = INCREMENTAL;
CREATE TABLE t(a);
INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < 40) SELECT zeroblob(3000) FROM s;
DELETE FROM t WHERE rowid > 10;
PRAGMA page_count;
PRAGMA freelist_count;
PRAGMA incremental_vacuum(3);
SELECT 'after 3';
PRAGMA freelist_count;
PRAGMA incremental_vacuum = 2;
PRAGMA incremental_vacuum('x');
SELECT 'after all';
PRAGMA page_count;
PRAGMA freelist_count;
PRAGMA incremental_vacuum;
PRAGMA auto_vacuum;
PRAGMA auto_vacuum = 1;
PRAGMA auto_vacuum;
DELETE FROM t;
PRAGMA page_count;
PRAGMA freelist_count;
"#;

/// The pragma's values and VACUUM's mode changes.
const PRAGMAS: &str = r#"
PRAGMA auto_vacuum = 3;
PRAGMA auto_vacuum;
CREATE TABLE t(a, b);
INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < 100) SELECT v, zeroblob(v * 20) FROM s;
PRAGMA auto_vacuum = full;
PRAGMA auto_vacuum;
VACUUM;
PRAGMA auto_vacuum;
PRAGMA page_count;
PRAGMA auto_vacuum = incremental;
PRAGMA auto_vacuum;
DELETE FROM t WHERE a > 50;
PRAGMA freelist_count;
PRAGMA auto_vacuum = none;
PRAGMA auto_vacuum;
VACUUM;
PRAGMA auto_vacuum;
PRAGMA page_count;
SELECT count(*), sum(length(b)) FROM t;
"#;

#[test]
fn full_file() {
    byte_identical("full", FULL);
}

#[test]
fn incremental_file() {
    byte_identical("incremental", INCREMENTAL);
}

#[test]
fn pragma_and_vacuum() {
    byte_identical("pragmas", PRAGMAS);
}

/// A pin-written auto-vacuum file, written by hipp's shell and by
/// the pin's: the same output, both files clean.
#[test]
fn pin_file_written_by_hipp() {
    for mode in ["FULL", "INCREMENTAL"] {
        let a = temp_db(&format!("pinfile-{mode}-hipp"));
        let b = temp_db(&format!("pinfile-{mode}-ora"));
        let setup = format!(
            "PRAGMA page_size = 1024; PRAGMA auto_vacuum = {mode}; CREATE TABLE t(a, b); \
             CREATE INDEX tb ON t(b); INSERT INTO t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL \
             SELECT v + 1 FROM s WHERE v < 300) SELECT v, zeroblob(v % 40 * 30) || v FROM s; \
             CREATE TABLE u(c); DELETE FROM t WHERE a % 4 = 0;"
        );
        run(&oracle(), &b, &setup);
        std::fs::copy(&b, &a).expect("copy");
        let work = "DROP INDEX tb; PRAGMA incremental_vacuum(4); PRAGMA page_count; \
                    INSERT INTO u SELECT b FROM t WHERE a < 20; DELETE FROM t WHERE a % 3 = 0; \
                    CREATE INDEX ta ON t(a); SELECT name, rootpage FROM sqlite_schema; \
                    PRAGMA freelist_count; SELECT count(*), sum(length(b)) FROM t; PRAGMA integrity_check;";
        assert_eq!(
            String::from_utf8_lossy(&run(&hipp_bin(), &a, work)),
            String::from_utf8_lossy(&run(&oracle(), &b, work)),
            "{mode}"
        );
        assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
    }
}

/// An auto-vacuum file as an attached database, and the integrity
/// check inside an open transaction (v0.16 review fixes): one batch
/// per engine in its own directory (`{d}`), byte-identical.
const ATTACHED: &str = r#"
ATTACH '{d}/aux.db' AS aux;
PRAGMA aux.auto_vacuum = INCREMENTAL;
PRAGMA aux.auto_vacuum;
CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b);
INSERT INTO aux.t WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < 60) SELECT v, zeroblob(1500) FROM s;
CREATE TABLE aux.u(c);
DELETE FROM aux.t WHERE a > 20;
PRAGMA aux.freelist_count;
PRAGMA aux.incremental_vacuum(2);
SELECT 'after 2';
BEGIN;
DELETE FROM aux.t WHERE a > 10;
PRAGMA aux.integrity_check;
PRAGMA aux.incremental_vacuum;
PRAGMA aux.integrity_check;
COMMIT;
PRAGMA aux.page_count;
PRAGMA aux.freelist_count;
DROP TABLE aux.t;
SELECT name, rootpage FROM aux.sqlite_schema;
PRAGMA aux.page_count;
PRAGMA aux.auto_vacuum = FULL;
CREATE TABLE aux.v(d);
INSERT INTO aux.v SELECT zeroblob(4000) FROM aux.u UNION ALL SELECT zeroblob(9000);
DELETE FROM aux.v;
PRAGMA aux.page_count;
PRAGMA aux.freelist_count;
"#;

#[test]
fn attached_file() {
    let a = temp_db("attached-hipp");
    let b = temp_db("attached-ora");
    let batch = |db: &Path| {
        let d = db.parent().expect("dir").to_string_lossy().to_string();
        ATTACHED.replace("{d}", &d)
    };
    let got = run(&hipp_bin(), &a, &batch(&a));
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run(&oracle(), &b, &batch(&b))),
        "attached"
    );
    assert!(!got.is_empty());
    let aux = a.parent().expect("dir").join("aux.db");
    assert_eq!(run(&oracle(), &aux, "PRAGMA integrity_check;"), b"ok\n");
    let (a2, b2) = (
        temp_db("attached-hipp-stdin"),
        temp_db("attached-ora-stdin"),
    );
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, &batch(&a2))),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, &batch(&b2))),
        "attached (stdin)"
    );
}
