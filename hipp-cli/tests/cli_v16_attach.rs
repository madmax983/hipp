//! hipp-cli v0.16 differentials for ATTACH / DETACH (leaf `attach`):
//! `.databases`, `.schema` and `.tables` over attached databases,
//! `PRAGMA database_list`, schema-qualified statements and statements
//! across databases must render byte-identically to the pinned oracle.
//! Each batch runs one-shot and over stdin, in twin directories (the
//! attached files are named relative to them, and the directory is
//! written back as `{d}` in the output); every database file hipp
//! writes must pass the oracle's integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16att-{tag}"));
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

/// A stdin script with dot-commands, byte-compared.
fn script_identical(tag: &str, input: &str) {
    let a = temp_dir(&format!("{tag}-hipp"));
    let b = temp_dir(&format!("{tag}-ora"));
    let got = run_stdin(&hipp_bin(), &a, input);
    let want = run_stdin(&oracle(), &b, input);
    assert_eq!(got, want, "{tag}: output differs:\n{input}");
    integrity_ok(&a);
}

#[test]
fn database_list_and_qualified_statements() {
    byte_identical(
        "list",
        "ATTACH 'a.db' AS aux; ATTACH ':memory:' AS m; ATTACH '' AS e; \
         PRAGMA database_list; CREATE TABLE aux.t(a, b REAL); \
         INSERT INTO aux.t VALUES (1, 2.5), (2, 1e20); SELECT * FROM t; \
         SELECT seq, name FROM pragma_database_list; DETACH m; \
         SELECT seq, name, file = '' FROM pragma_database_list;",
    );
    byte_identical(
        "cross",
        "ATTACH 'a.db' AS a; ATTACH 'b.db' AS b; \
         CREATE TABLE m(k INTEGER PRIMARY KEY, v TEXT); \
         CREATE TABLE a.n(k INTEGER PRIMARY KEY AUTOINCREMENT, w TEXT UNIQUE, z DEFAULT 7.5); \
         CREATE TABLE b.o(k, w, PRIMARY KEY(k)) WITHOUT ROWID; \
         INSERT INTO m VALUES (1, 'x'), (2, 'y'), (3, 'z'); \
         INSERT INTO a.n(w) SELECT v FROM m; INSERT INTO b.o SELECT k, w FROM a.n WHERE k > 1; \
         SELECT m.v, n.z, o.w FROM m LEFT JOIN a.n ON n.k = m.k LEFT JOIN b.o ON o.k = m.k ORDER BY 1; \
         SELECT v FROM m UNION SELECT w FROM a.n UNION SELECT w FROM b.o ORDER BY 1; \
         UPDATE m SET v = (SELECT w || '!' FROM b.o WHERE o.k = m.k) WHERE k > 1; \
         SELECT * FROM m; SELECT * FROM a.sqlite_sequence; \
         BEGIN; INSERT INTO a.n(w) VALUES ('q'); INSERT INTO m(v) VALUES ('q'); COMMIT; \
         SELECT count(*) FROM a.n; SELECT count(*) FROM m; \
         PRAGMA a.table_info(n); PRAGMA b.index_list(o); \
         SELECT schema, name, type FROM pragma_table_list ORDER BY 1, 2;",
    );
}

#[test]
fn dot_commands_over_attached_databases() {
    script_identical(
        "dots",
        "ATTACH 'q.db' AS \"w x\";\n\
         CREATE TABLE \"w x\".t(a);\n\
         CREATE VIEW \"w x\".v AS SELECT a FROM t;\n\
         CREATE INDEX \"w x\".i ON t(a);\n\
         CREATE TEMP TABLE tt(b);\n\
         CREATE TABLE m(a);\n\
         .schema\n\
         .schema t\n\
         .databases\n\
         ATTACH ':memory:' AS mm;\n\
         ATTACH '' AS ee;\n\
         CREATE TABLE mm.k(z);\n\
         .databases\n\
         .schema k\n\
         DETACH \"w x\";\n\
         .databases\n",
    );
}
