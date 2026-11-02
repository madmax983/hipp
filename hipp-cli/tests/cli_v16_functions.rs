//! hipp-cli v0.16 differentials: the scalar function library must
//! render byte-identically to the pinned oracle — printf() /
//! format() over every conversion (REAL digits, padding, the
//! JSON and unistr forms, a NUL ending a text value in list
//! mode), quote() / char() / unicode() / concat() / unhex() and
//! friends, round()'s fitted digits, and the change counters /
//! last_insert_rowid() across a script. Each batch runs one-shot
//! and over stdin, and the hipp-written file must pass the
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

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16fn-{tag}-{}", std::process::id()));
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

/// One batch, byte-compared one-shot and over stdin (raw bytes:
/// a blob column renders unconverted, so not always UTF-8).
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(got, run(&oracle(), &b, batch));
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Ungated: round(), lazy coalesce / iif, the text-function fixes.
const CORE: &str = "\
SELECT round(2.675, 2), round(1.005, 2), round(0.285, 2), round(-2.5), round(166162.7745, 3), round(1e-300, 5);
SELECT round(188.30472979193155, 13), round(799.3565392695623, 25), round(0.49999999999999994), round(123.456, 30);
SELECT coalesce(1, abs(-9223372036854775808)), iif(0, abs(-9223372036854775808), 3.5);
SELECT instr(x'68c3a96c6c6f', x'6c'), upper(x'61'), typeof(lower(x'41')), length(CAST(x'610062' AS TEXT));
SELECT hex(substr(x'c3a9', 1, 1)), hex(trim(x'c3a9c3', 'é')), trim('xxaxx', 'x'), replace('abc', '', 'z');
";

/// `v16`: printf and the value functions.
const PRINTF: &str = "\
SELECT printf('%d %s %5.2f %x %%', 42, 'hi', 3.14159, 255), format('%05d', 42), printf('[%-8s|%8.3s]', 'abc', 'abcdef');
SELECT printf('%,d|%+d|% d|%#x|%#o|%r|%r|%r|%p', 1234567, 5, 5, 255, 8, 1, 22, 113, 255);
SELECT printf('%f|%e|%g|%G|%.0e|%.0e|%.2e|%#.0f|%!.17g|%!.26g', 3.14159, 3.14159, 1e-5, 1.5e-7, 45.0, 450.0, 12350.0, -0.4, 0.1, 0.1);
SELECT printf('%010.3f|%-10.2e|%+.3g|%,.2f|%f|%010f', -3.14159, 2.5, 1234.5, 1234567.891, 1e308*10, 1e308*10);
SELECT printf('%q|%Q|%w|%Q', 'it''s', 'it''s', 'a\"b', NULL), printf('%j|%J|%J', 'a\"b\\', 'x' || char(9), NULL);
SELECT printf('%#Q|%#q', 'a' || char(10) || 'b', 'c\\d'), printf('%c%c%c', 'h', 'é', 'y'), printf('%.3c|%5c|', 'x', 'é');
SELECT printf('a' || char(0) || 'b'), printf('%s|', 'x' || char(0) || 'y'), printf('%d%T%d', 1, 2), printf('%5%|%');
SELECT quote('it''s'), quote(1.5), quote(x'ab'), quote(NULL), quote(1e308*10), quote(0.1 + 0.2), quote(100.0);
SELECT char(72, 105, 233, 8364), unicode('€'), hex(zeroblob(3)), length(randomblob(7)), typeof(random());
SELECT concat('a', NULL, 2, 1.5), concat_ws('-', 'a', NULL, 'b'), octet_length('héllo'), hex(unhex('41 42', ' '));
SELECT likely(3), unlikely('u'), likelihood(4.5, 0.25), iif(0, 'a'), if(0, 1, 0, 2, 3), sqlite_version();
";

/// `v16`: the change counters across a script.
const COUNTERS: &str = "\
CREATE TABLE t(a PRIMARY KEY, b);
INSERT INTO t VALUES(1, 1.5), (2, 2.5), (3, 3.5);
SELECT changes(), total_changes(), last_insert_rowid();
INSERT OR REPLACE INTO t VALUES(1, 10), (4, 4);
SELECT changes(), total_changes(), last_insert_rowid();
INSERT INTO t VALUES(3, 0) ON CONFLICT(a) DO UPDATE SET b = b + 100;
UPDATE t SET b = b * 2 WHERE a > 1;
SELECT changes(), total_changes(), last_insert_rowid();
DELETE FROM t WHERE a = 4;
SELECT changes(), total_changes();
CREATE TABLE r(x);
INSERT INTO r VALUES(last_insert_rowid()), (last_insert_rowid()), (last_insert_rowid() * 0.5);
SELECT rowid, x FROM r;
INSERT INTO r VALUES (7) RETURNING last_insert_rowid(), changes(), total_changes();
SELECT changes(), total_changes(), last_insert_rowid();
";

/// Review fixes: overflow chains of large records, per-row
/// connection-state DEFAULTs, subqueries read only when reached,
/// `f(*)` / DISTINCT on scalar functions.
const REVIEW: &str = "\
CREATE TABLE big(b);
INSERT INTO big VALUES(zeroblob(69530)), (printf('%70000d', 1)), (randomblob(300000));
UPDATE big SET b = b || b WHERE rowid = 2;
SELECT rowid, length(b), typeof(b) FROM big;
CREATE TABLE d(a INTEGER PRIMARY KEY, b DEFAULT (last_insert_rowid()), c DEFAULT (random()));
INSERT INTO d(a) VALUES(11), (12), (13);
INSERT INTO d(a) SELECT a + 100 FROM d;
SELECT a, b, typeof(c) FROM d;
SELECT count(DISTINCT c) FROM d;
SELECT coalesce(1, (SELECT abs(-9223372036854775808))), iif(1, 2, (SELECT abs(-9223372036854775808)));
SELECT 0 AND (SELECT abs(-9223372036854775808)), 1 OR (SELECT abs(-9223372036854775808)), coalesce(1, 5 IN (SELECT abs(-9223372036854775808)));
SELECT typeof(random(*)), printf(*), abs(DISTINCT -1), upper(DISTINCT 'a'), printf(DISTINCT '%d-%d', 1, 2), changes(*), char(*);
SELECT length(printf('%*.*s', 3000000, 5, 'x')), length(substr(hex(zeroblob(400000)), 7, 500000)), hex(zeroblob(100000)) LIKE '%01';
";

#[test]
fn cli_v16_functions_review_byte_identical() {
    byte_identical("review", REVIEW);
}

#[test]
fn cli_v16_functions_core_byte_identical() {
    byte_identical("core", CORE);
}

#[test]
fn cli_v16_functions_printf_byte_identical() {
    byte_identical("printf", PRINTF);
}

#[test]
fn cli_v16_functions_counters_byte_identical() {
    byte_identical("counters", COUNTERS);
}
