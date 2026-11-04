//! hipp-cli v0.16 differentials: bound parameters must render
//! byte-identically to the pinned oracle shell. The shell binds
//! every parameter from its `.parameter set` table by name (a
//! nameless `?` as `?N`) and NULL when there is no such key; the
//! stand-in oracle used locally cannot bind at all, so each batch
//! runs in hipp-cli with its parameters (unbound, or set with
//! `.parameter set`) and in the oracle with the same values
//! written as literals of the same storage class. Also: `-0.0`
//! renders as the pin's printf does (`0.0`). The hipp-written file
//! must pass the oracle's integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16pa-{tag}"));
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
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `hipp_batch` (parameters, maybe `.parameter` lines) in
/// hipp-cli over stdin against `oracle_batch` (literals) in the
/// oracle: byte-identical output, a clean hipp file.
fn same(tag: &str, hipp_batch: &str, oracle_batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run_stdin(&hipp_bin(), &a, hipp_batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b, oracle_batch))
    );
    assert!(!got.is_empty());
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Unbound parameters are NULL, one-shot and over stdin.
#[test]
fn cli_v16_unbound_parameters_are_null() {
    let hipp = "\
CREATE TABLE t(a INTEGER, b TEXT, c);
INSERT INTO t VALUES (?, :b, ?3), (1, 'x', 2.5);
INSERT INTO t SELECT ?1 + 1, @b || 'y', coalesce($c, 'dflt');
SELECT a, b, c, typeof(a), typeof(b), typeof(c) FROM t;
SELECT ?, :a, ?1 || 'x', coalesce(@b, 'dflt'), typeof($c), ?2 IS NULL, #z;
SELECT count(*) FROM t WHERE a = ? OR a IS ?;
UPDATE t SET c = ? WHERE a = 1;
SELECT * FROM t LIMIT coalesce(?, 5);
";
    let ora = "\
CREATE TABLE t(a INTEGER, b TEXT, c);
INSERT INTO t VALUES (NULL, NULL, NULL), (1, 'x', 2.5);
INSERT INTO t SELECT NULL + 1, NULL || 'y', coalesce(NULL, 'dflt');
SELECT a, b, c, typeof(a), typeof(b), typeof(c) FROM t;
SELECT NULL, NULL, NULL || 'x', coalesce(NULL, 'dflt'), typeof(NULL), NULL IS NULL, NULL;
SELECT count(*) FROM t WHERE a = NULL OR a IS NULL;
UPDATE t SET c = NULL WHERE a = 1;
SELECT * FROM t LIMIT coalesce(NULL, 5);
";
    same("unbound", hipp, ora);
    let a = temp_db("unbound-oneshot-hipp");
    let b = temp_db("unbound-oneshot-ora");
    let one = "SELECT ?, :a || 'x', typeof(?1); SELECT -0.0, 0.0 * -1, 1.5;";
    let one_ora = "SELECT NULL, NULL || 'x', typeof(NULL); SELECT -0.0, 0.0 * -1, 1.5;";
    assert_eq!(run(&hipp_bin(), &a, one), run(&oracle(), &b, one_ora));
}

/// `.parameter set` values of every storage class, rendered in
/// results, stored through column affinity, compared, and used in
/// LIMIT / compound / subquery positions; `.parameter unset` and
/// `.parameter clear` drop them back to NULL.
#[test]
fn cli_v16_parameter_set_byte_identical() {
    let hipp = "\
.parameter set :i 42
.parameter set :r 2.5
.parameter set :big 1e300
.parameter set :neg -7
.parameter set :t \"'it''s'\"
.parameter set :n5 \"'5'\"
.parameter set :b x'00ff41'
.parameter set :word hello
.parameter set :e 1+2
.parameter set :z NULL
.parameter set ?1 99
.parameter set @lim 2
CREATE TABLE t(i INTEGER, tx TEXT, n NUMERIC, r REAL, x);
INSERT INTO t VALUES (:i, :i, :i, :i, :i), (:r, :r, :r, :r, :r), (:t, :t, :t, :t, :t);
INSERT INTO t VALUES (:n5, :n5, :n5, :n5, :n5), (:b, :b, :b, :b, :b), (:z, :word, :e, :big, :neg);
SELECT typeof(i), i, typeof(tx), tx, typeof(n), n, typeof(r), r, typeof(x), x FROM t;
SELECT :i, :r, :big, :neg, :t, :b, :word, :e, :z, typeof(:e), typeof(:word);
SELECT ?, ?1 + 1, typeof(?);
SELECT count(*) FROM t WHERE i = :n5 OR tx = :i;
SELECT x FROM t WHERE x > :neg ORDER BY x LIMIT @lim;
SELECT :i UNION SELECT :r UNION SELECT :t ORDER BY 1;
SELECT (SELECT max(i) FROM t WHERE i < :i), :i IN (SELECT i FROM t);
.parameter unset :i
.parameter set :r 0.1
SELECT :i, :r, typeof(:i);
.parameter clear
SELECT :r, :t, ?1;
";
    let ora = "\
CREATE TABLE t(i INTEGER, tx TEXT, n NUMERIC, r REAL, x);
INSERT INTO t VALUES (42, 42, 42, 42, 42), (2.5, 2.5, 2.5, 2.5, 2.5), ('it''s', 'it''s', 'it''s', 'it''s', 'it''s');
INSERT INTO t VALUES ('5', '5', '5', '5', '5'), (x'00ff41', x'00ff41', x'00ff41', x'00ff41', x'00ff41'), (NULL, 'hello', 3, 1e300, -7);
SELECT typeof(i), i, typeof(tx), tx, typeof(n), n, typeof(r), r, typeof(x), x FROM t;
SELECT 42, 2.5, 1e300, -7, 'it''s', x'00ff41', 'hello', 3, NULL, typeof(3), typeof('hello');
SELECT 99, 99 + 1, typeof(NULL);
SELECT count(*) FROM t WHERE i = '5' OR tx = 42;
SELECT x FROM t WHERE x > -7 ORDER BY x LIMIT 2;
SELECT 42 UNION SELECT 2.5 UNION SELECT 'it''s' ORDER BY 1;
SELECT (SELECT max(i) FROM t WHERE i < 42), 42 IN (SELECT i FROM t);
SELECT NULL, 0.1, typeof(NULL);
SELECT NULL, NULL, NULL;
";
    same("set", hipp, ora);
}

/// Regressions from review: a compound ORDER BY parameter takes
/// the column of the leftmost core that holds it, rows sharing a
/// multi-row VALUES co-routine never match it, substr() / round()
/// read text integer arguments by their integer prefix, and a
/// REAL whose 16th significant digit is an exact 5 prints rounded
/// up (half away from zero, as the pin's printf), not to even.
#[test]
fn cli_v16_compound_order_and_integer_arguments() {
    let hipp = "\
.parameter set ?1 9
.parameter set :t \"'1e1'\"
SELECT ?1, 2 UNION SELECT 3, ?1 ORDER BY ?1;
SELECT 5, ?1 UNION ALL SELECT ?1, 2 ORDER BY ?1 DESC;
VALUES (CAST(1 AS INT), ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1;
SELECT substr('abcdefghijkl', :t), substr('abcdefghijkl', 2, :t), round(1.23456, :t);
SELECT round(2.675, 2), round(1.5, 1000000000000), round(1e300, 2), round(0.1, 30);
SELECT 5833858491548785.0, -6776496294506.125, 0.000123456789012345, 999999999999999.5;
";
    let ora = "\
SELECT 9, 2 UNION SELECT 3, 9 ORDER BY 1;
SELECT 5, 9 UNION ALL SELECT 9, 2 ORDER BY 2 DESC;
VALUES (CAST(1 AS INT), 9), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 2;
SELECT substr('abcdefghijkl', '1e1'), substr('abcdefghijkl', 2, '1e1'), round(1.23456, '1e1');
SELECT round(2.675, 2), round(1.5, 1000000000000), round(1e300, 2), round(0.1, 30);
SELECT 5833858491548785.0, -6776496294506.125, 0.000123456789012345, 999999999999999.5;
";
    let a = temp_db("ordint-hipp");
    let b = temp_db("ordint-ora");
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a, hipp)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b, ora))
    );
    // Rows that share a co-routine: the pin's "does not match any
    // column" error, in both.
    let q = "VALUES (1, ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1;";
    let qo = "VALUES (1, 9), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 'nomatch';";
    for (bin, sql) in [(hipp_bin(), q), (oracle(), qo)] {
        let out = Command::new(&bin)
            .arg(&a)
            .arg(sql)
            .output()
            .expect("cli runs");
        assert!(
            !out.status.success() || !out.stderr.is_empty(),
            "{}",
            bin.display()
        );
        assert!(out.stdout.is_empty(), "{}", bin.display());
    }
}
