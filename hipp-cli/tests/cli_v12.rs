//! hipp-cli v0.12 differentials: real literals must parse to
//! the pinned oracle's exact bits (RISKS R5). The corpus
//! exposes the parse through comparison operators, whose
//! 0/1 output flips on a 1-ulp error and renders identically
//! in both CLIs, and through CAST / affinity conversions of
//! text. The second test stores literals in both engines'
//! files and compares the oracle's exact extraction
//! (`printf('%!.25e')`) across them.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v12-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// Every statement's output is a bit-sensitive fingerprint of
/// the parse: equality/inequality against a neighbour value
/// whose own parse is exact, or a stored value read back and
/// compared. All expected outputs are plain integers.
const CORPUS: &str = "\
CREATE TABLE r (v REAL);
INSERT INTO r VALUES (1.0), (0.9999999999999999), (1e308), (9.999999999999999e307), (4.9e-324), (0.0), (9007199254740992.0), (2.675), (123456789.123456789);
SELECT count(*) FROM r WHERE v = 1.0;
SELECT count(*) FROM r WHERE v = 0.9999999999999999;
SELECT count(*) FROM r WHERE v < 1.0 AND v > 0.9;
SELECT count(*) FROM r WHERE v = 1e308;
SELECT count(*) FROM r WHERE v > 9.999999999999999e307;
SELECT count(*) FROM r WHERE v = 4.9e-324;
SELECT count(*) FROM r WHERE v = 2.5e-324;
SELECT count(*) FROM r WHERE v = 2.47e-324;
SELECT count(*) FROM r WHERE v = 9007199254740993.0;
SELECT 0.9999999999999999 = 1.0;
SELECT 0.9999999999999999 < 1.0;
SELECT 1.0000000000000002 = 1.0;
SELECT 1.0000000000000002 > 1.0;
SELECT 9007199254740993.0 = 9007199254740992.0;
SELECT 18446744073709551617 = 18446744073709551616.0;
SELECT 18446744073709553664 = 18446744073709551616.0;
SELECT 18446744073709553665 = 18446744073709551616.0;
SELECT 1.7976931348623157e308 = 1.7976931348623158e308;
SELECT 1.7976931348623159e308 > 1.7976931348623157e308;
SELECT 2.2250738585072014e-308 > 2.2250738585072009e-308;
SELECT CAST('123456789.123456789' AS REAL) = 123456789.123456789;
SELECT CAST('0.9999999999999999' AS REAL) = 1.0;
SELECT CAST('2.4703282292062327209e-324' AS REAL) = 4.9e-324;
SELECT CAST('1e308' AS REAL) = 1e308;
SELECT v = 2.675 FROM r WHERE rowid = 8;
SELECT v = 123456789.123456789 FROM r WHERE rowid = 9;
";

#[test]
fn v12_corpus_byte_identical() {
    let a = run(&hipp_bin(), &temp_db("corpus-hipp"), CORPUS);
    let b = run(&oracle(), &temp_db("corpus-oracle"), CORPUS);
    assert_eq!(a, b, "one-shot corpus outputs differ");
    // Same corpus over stdin.
    let a2 = run_stdin(&hipp_bin(), &temp_db("corpus-hipp-stdin"), CORPUS);
    let b2 = run_stdin(&oracle(), &temp_db("corpus-oracle-stdin"), CORPUS);
    assert_eq!(a2, b2, "stdin corpus outputs differ");
}

#[test]
fn v12_stored_literals_extract_identically() {
    // hipp writes a file of parsed literals; the oracle's
    // exact extraction from it must equal its extraction
    // from its own file of the same literals.
    const LITS: &str = "0.1, 2.675, 1.0000000000000002, 0.9999999999999999, \
        123456789.123456789, 1e308, 9.999999999999999e307, \
        1.7976931348623157e308, 2.2250738585072014e-308, 4.9e-324, \
        2.5e-324, 0.30000000000000004, 9007199254740993.0, \
        18446744073709551616, 3.141592653589793238462643383279";
    let setup = format!(
        "CREATE TABLE r (v REAL); INSERT INTO r VALUES {}",
        LITS.split(", ")
            .map(|l| format!("({l})"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let hipp_db = temp_db("stored-hipp");
    let oracle_db = temp_db("stored-oracle");
    run(&hipp_bin(), &hipp_db, &setup);
    run(&oracle(), &oracle_db, &setup);
    assert_eq!(
        run(&oracle(), &hipp_db, "PRAGMA integrity_check;").trim(),
        "ok"
    );
    let q = "SELECT printf('%!.25e', v) FROM r ORDER BY rowid;";
    let from_hipp = run(&oracle(), &hipp_db, q);
    let from_oracle = run(&oracle(), &oracle_db, q);
    assert_eq!(from_hipp, from_oracle);
}
