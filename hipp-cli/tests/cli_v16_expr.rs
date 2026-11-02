//! hipp-cli v0.16 differentials: expression operators, literals
//! and row values must render byte-identically to the pinned
//! oracle — precedence and the truth tests (ungated), hex and
//! digit-separated literals with REAL results, the aggregate
//! scoping fix (an enclosing query's aggregate inside a
//! subquery), and under `v16` the bitwise operators, IS
//! DISTINCT FROM, postfix NOT NULL, row-value comparisons / IN /
//! BETWEEN and UPDATE ... SET (a, b) = .... Each batch runs
//! one-shot and over stdin, and the hipp-written file must pass
//! the oracle's integrity check.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16ex-{tag}-{}", std::process::id()));
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

/// Ungated: precedence, truth tests, literals, aggregate scope.
const CORE: &str = "\
SELECT 3 = 2 < 3, 0 = 1 > 2, 1 ISNULL + 1, 2 = 1 ISNULL + 1, 1 IN (1) + 1, 2 BETWEEN 1 = 1 AND 3;
SELECT 2 IS TRUE, 0.5 IS TRUE, 'a' IS FALSE, NULL IS NOT TRUE, NULL IS NOT FALSE;
SELECT 0x10, 0xffffffffffffffff, -0x7fffffffffffffff, 0x8000000000000000, 0x00000000000000001;
SELECT 0-0x8000000000000000, 9223372036854775807 + 1, - -9223372036854775808, 9223372036854775808;
SELECT -9223372036854775808, typeof(-9223372036854775808), 1_000, 1_0.5, 1e1_0, .5_1, 0x1_0;
SELECT '0x10' + 0, CAST('0x10' AS REAL), 0x10 * 1.5, 1_0.25 / 4, 9_223_372_036_854_775_808;
CREATE TABLE t(a, z REAL); INSERT INTO t VALUES (1, 1.5), (2, 2.25), (3, NULL), (0xff, 1e300);
CREATE TABLE u(b); INSERT INTO u VALUES (5), (6);
SELECT (SELECT max(t.a) FROM u) FROM t;
SELECT (SELECT max(t.a) + b FROM u), (SELECT avg(t.z) FROM u), (SELECT total(t.z) * b FROM u) FROM t;
SELECT a, (SELECT max(t.a) FROM u) FROM t GROUP BY a;
SELECT a, (SELECT count(t.a) FROM u), count(*) FROM t;
SELECT (SELECT count(*) FROM u WHERE b < max(t.a)) FROM t;
";

/// `v16`: bitwise operators, DISTINCT FROM, NOT NULL, row values.
const V16: &str = "\
SELECT 1|2, 5&3, 1<<2, 8>>1, ~5, 1<<63, 1<<64, -1>>64, 8>>-1, 1<<-1, -8>>1, ~-1;
SELECT 3.9|4, -3.9|0, '12abc'|1, 1e19|0, -1e19|0, ~5.7, ~'abc', NULL|1, 1 + 1 << 1 + 1, 1|2&3;
SELECT 1 IS DISTINCT FROM 2, NULL IS NOT DISTINCT FROM NULL, 1 NOT NULL, NULL NOT NULL, 1 NOT NULL + 1;
SELECT (1,2)=(1,2), (1,NULL)=(1,2), (1,2)<(1,3), (1,NULL)<=(1,NULL), (1,2) IS (1,2), (1.5,'a') > (1.5,'B');
SELECT (1,2) IN ((1,2),(3,4)), (1,NULL) IN (VALUES (1,2)), (2,3) BETWEEN (1,1) AND (3,3);
SELECT (SELECT 1.5, 2) = (1.5, 2), CASE (1,2) WHEN (1,2) THEN 0.5 END;
CREATE TABLE r(a, b, c); INSERT INTO r VALUES (1, 2.5, 'x'), (2, NULL, 'y'), (3, 4, 'z');
SELECT a FROM r WHERE (a, b) > (1, 3);
SELECT a, (a, c) IN (SELECT a, c FROM r WHERE b NOT NULL) FROM r;
UPDATE r SET (b, c) = (c, b) WHERE a = 1;
UPDATE r SET (a, b) = (SELECT max(a) * 10, 0.125 FROM r) WHERE a = 2;
SELECT a, b, c, a << 1, ~a FROM r ORDER BY a;
";

#[test]
fn cli_v16_expr_core_byte_identical() {
    byte_identical("core", CORE);
}

#[test]
fn cli_v16_expr_rows_and_bits_byte_identical() {
    byte_identical("v16", V16);
}
