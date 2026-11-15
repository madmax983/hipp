//! v0.17 parity unit `gaps_eval`: evaluator and statement-level
//! divergences the v0.16 units recorded as honest gaps,
//! differentially vs the pinned oracle (SQLite 3.54.1), probed live
//! on the pin before the fixes:
//!
//! - `hex(NULL)` is `''` (hipp returned NULL).
//! - `substr` of an empty BLOB is NULL whatever its other arguments
//!   (the pin's `substrFunc` gets no pointer from
//!   `sqlite3_value_blob` for zero bytes); an empty text is `''`.
//! - A core over no table never computes its ORDER BY terms (its one
//!   row satisfies them: `SELECT 1 ORDER BY abs(-9223372036854775808)`
//!   is 1), DISTINCT and nested cores included; an aggregate's GROUP
//!   BY still sorts.
//! - LIMIT 0 ends a query before anything else is computed: its
//!   OFFSET, result columns, WHERE, ORDER BY, the cores of a compound,
//!   a window's input.
//! - The WHERE clause's constant propagation (`propagateConstants`)
//!   covers the rowid and its INTEGER PRIMARY KEY alias, constants
//!   other than literals (`c = 1+4`, `c = abs(-5)`), one-item IN lists
//!   (the pin's parser makes `x IN (c)` into `x = +c`) and every node
//!   kind (IN, BETWEEN, LIKE, CASE, COLLATE, unary plus): a term that
//!   becomes constant is computed before the loop, in order, so
//!   `WHERE a = 5 AND a < 5 AND <failing constant>` is no row.
//! - AND / OR simplify as `sqlite3ExprSimplifiedAndOr` does: an
//!   operand neutral to the operator leaves the other, so `x AND (0 OR
//!   0)` is 0 without computing `x`.
//! - An empty statement (`;`, `;;`, blanks and comments) prepares to
//!   a statement that does nothing; `EXPLAIN QUERY PLAN` of one is a
//!   syntax error.
//!
//! Each query runs on hipp and on the pin over copies of one file and
//! must give the same rows in the same order and the same success (an
//! error on both counts as agreement). With the hidden `vm` feature
//! the corpus also runs on the VM (falling back where it must) and on
//! today's executor (`Config::vm_off`).
#![cfg(feature = "sql")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, MemStorage, Step};
use std::path::{Path, PathBuf};

/// One row as the pin's shell prints it in list mode.
fn render<S: hipp::storage::Storage>(q: &hipp::Statement<'_, S>) -> String {
    let mut line = String::new();
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut out = hipp::arena::AVec::new();
                hipp::record::render_real(f, &mut out);
                line.push_str(&String::from_utf8_lossy(out.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
    line
}

/// What one engine made of one query.
#[derive(Debug, PartialEq)]
struct Run {
    ok: bool,
    rows: Vec<String>,
}

/// `sql` (one statement) on hipp under `cfg`.
fn hipp_run(path: &Path, cfg: Config, sql: &str) -> Run {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        let Ok(mut q) = db.prepare(sql).await else {
            return Run {
                ok: false,
                rows: Vec::new(),
            };
        };
        let mut rows = Vec::new();
        loop {
            match q.step().await {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => return Run { ok: true, rows },
                Err(_) => return Run { ok: false, rows },
            }
        }
    })
}

/// The pin's run of `sql` (`-bail`: it stops at the first error).
fn pin_run(path: &Path, sql: &str) -> Run {
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    Run {
        ok: out.status.success() && out.stderr.is_empty(),
        rows: String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    }
}

/// A file the pin builds with `setup`.
fn pin_file(tag: &str, setup: &str) -> PathBuf {
    let base = common::temp_db(&format!("eg17-{tag}-base"));
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(&base)
        .arg(setup)
        .output()
        .expect("oracle runs");
    assert!(
        out.status.success(),
        "setup failed: {setup}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    base
}

fn copy_of(base: &Path, tag: &str) -> PathBuf {
    let p = common::temp_db(tag);
    std::fs::copy(base, &p).expect("copy");
    p
}

/// The engines a query runs on: hipp's default; with `vm`, the VM
/// (non-strict: it falls back where it must) and today's executor.
fn engines() -> Vec<(&'static str, Config)> {
    #[cfg(feature = "vm")]
    let dflt = Config::default().vm_strict(false);
    #[cfg(not(feature = "vm"))]
    let dflt = Config::default();
    #[allow(unused_mut)]
    let mut v = vec![("default", dflt)];
    #[cfg(feature = "vm")]
    {
        v.push(("vm", Config::default().vm_strict(false)));
        v.push(("executor", Config::default().vm_off(true)));
    }
    v
}

/// Every difference between hipp (each engine) and the pin over `qs`,
/// each query on a fresh copy of the file `setup` builds.
fn diffs(setup: &str, tag: &str, qs: &[String]) -> Vec<String> {
    let base = pin_file(tag, setup);
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let o = copy_of(&base, &format!("eg17-{tag}{i}-o"));
        let want = pin_run(&o, q);
        for (name, cfg) in engines() {
            let h = copy_of(&base, &format!("eg17-{tag}{i}-{name}"));
            let got = hipp_run(&h, cfg, q);
            if got != want {
                bad.push(format!("[{name}] {q}\n  pin : {want:?}\n  hipp: {got:?}"));
            }
        }
    }
    bad
}

fn corpus(setup: &str, tag: &str, qs: &[&str]) {
    let qs: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs(setup, tag, &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The overflowing constant every corpus uses as "a term that fails".
const BAD: &str = "abs(-9223372036854775808)";

fn with_bad(qs: &[&str]) -> Vec<String> {
    qs.iter().map(|q| q.replace("BAD", BAD)).collect()
}

fn corpus_bad(setup: &str, tag: &str, qs: &[&str]) {
    let qs = with_bad(qs);
    let bad = diffs(setup, tag, &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

const T: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT, d INTEGER, e REAL, f NUMERIC); \
    INSERT INTO t VALUES (1, 1, '1', 1, 1.0, 1), (5, x'', '', 5, 5.5, '5'), \
    (9, NULL, NULL, NULL, NULL, NULL), (12, 'abc', 'abc', 12, -1.5, 'x'), \
    (20, x'4142', '0102', -3, 0.0, 2.5);";

// ---------------------------------------------------------------- hex / substr

/// hex() and substr() over every storage class, empty values and
/// NULLs, as literals and as columns of every affinity.
#[test]
fn hex_and_substr_match_the_pin() {
    corpus(
        T,
        "hs",
        &[
            "SELECT quote(hex(NULL)), typeof(hex(NULL)), quote(hex(NULL) || 'x')",
            "SELECT quote(hex('')), quote(hex(x'')), hex(1), hex(-1.5), hex('ab'), hex(x'00ff')",
            "SELECT quote(hex(zeroblob(0))), quote(hex(zeroblob(2))), hex(CAST(NULL AS BLOB))",
            "SELECT a, quote(hex(b)), quote(hex(c)), quote(hex(d)), quote(hex(e)), quote(hex(f)) FROM t",
            "SELECT quote(substr(x'', 'NULL')), quote(substr(x'', 0)), quote(substr('', 'a')), quote(substr('', 0))",
            "SELECT quote(substr(x'', 1)), quote(substr(x'', 1, 1)), quote(substr(x'', 0, 0)), quote(substr(x'', -1, 5))",
            "SELECT quote(substr(x'', NULL)), quote(substr(x'', 1, NULL)), quote(substr(NULL, 1)), quote(substr('', NULL))",
            "SELECT quote(substring(x'', 2)), quote(substr(zeroblob(0), 1)), quote(substr(CAST('' AS BLOB), 1))",
            "SELECT quote(substr(x'01', 0)), quote(substr(x'01', 2)), quote(substr(x'01', 5)), quote(substr(x'0102', -1))",
            "SELECT quote(substr(x'00', 1, 0)), quote(substr(x'00', 2, 5)), quote(substr(x'0102', 0, 1))",
            "SELECT quote(substr('', 1, 1)), quote(substr('', -1)), quote(substr('', 1, -1)), quote(substr('abc', 5))",
            "SELECT quote(substr('abc', 0)), quote(substr('abc', -5, 2)), quote(substr('abc', 2, -1)), quote(substr('abc', 'x'))",
            "SELECT quote(substr(123, 2)), quote(substr(1.5, 1)), quote(substr(x'616263', 2, 1))",
            "SELECT a, quote(substr(b, 1)), quote(substr(c, 1)), quote(substr(d, 1)), quote(substr(e, 2)), quote(substr(f, 1, 1)) FROM t",
            "SELECT a, quote(substr(b, 0)), quote(substr(b, 2, 1)), quote(substr(b, -1)), quote(substr(b, 'z')) FROM t",
            "SELECT a FROM t WHERE substr(b, 1) IS NULL",
            "SELECT a FROM t WHERE hex(b) = '' ORDER BY a",
            "SELECT count(*), group_concat(quote(substr(b, 1)), ',') FROM t",
            "SELECT quote(substr(CAST(x'' AS TEXT), 1)), quote(substr(CAST('' AS BLOB), 1, 2))",
        ],
    );
}

// ------------------------------------------------- FROM-less ORDER BY / LIMIT

/// A FROM-less core's ORDER BY terms (never computed), its LIMIT 0
/// and OFFSET, at the top level and nested, plain, DISTINCT,
/// aggregate, compound and window shapes.
#[test]
fn from_less_order_by_and_limit_match_the_pin() {
    corpus_bad(
        T,
        "fl",
        &[
            "SELECT 1 ORDER BY BAD",
            "SELECT 1, 2 ORDER BY 2 DESC, BAD",
            "SELECT 1 ORDER BY 1, BAD",
            "SELECT 1 x ORDER BY x, BAD",
            "SELECT 1 ORDER BY BAD DESC",
            "SELECT 1 ORDER BY BAD LIMIT 1",
            "SELECT 1 ORDER BY (SELECT BAD)",
            "SELECT DISTINCT 1 ORDER BY BAD",
            "SELECT DISTINCT 1, 2 ORDER BY 2, BAD",
            "SELECT count(*) ORDER BY BAD",
            "SELECT max(1) ORDER BY BAD",
            "SELECT sum(1) HAVING 1 ORDER BY BAD",
            "SELECT 1 WHERE 1 ORDER BY BAD",
            "SELECT 1 WHERE 0 ORDER BY BAD",
            "SELECT (SELECT 1 ORDER BY BAD)",
            "SELECT * FROM (SELECT 1 ORDER BY BAD)",
            "SELECT EXISTS(SELECT 1 ORDER BY BAD)",
            "SELECT 1 IN (SELECT 1 ORDER BY BAD)",
            "WITH c AS (SELECT 1 ORDER BY BAD) SELECT * FROM c",
            "SELECT a FROM t WHERE a = (SELECT 5 ORDER BY BAD)",
            "SELECT 1 ORDER BY json('x')",
            "SELECT 1 ORDER BY randomblob(-1)",
            "SELECT 1 ORDER BY 2",
            "SELECT 1 ORDER BY 0",
            "SELECT 1 ORDER BY nosuch",
            "SELECT 1 ORDER BY count(*)",
            "SELECT 1 ORDER BY BAD + count(*)",
            "SELECT 1 x ORDER BY max(x)",
            "SELECT DISTINCT 1 ORDER BY count(*)",
            "SELECT 1 WHERE 0 ORDER BY count(*)",
            "SELECT 1 ORDER BY sum(max(1))",
            "SELECT 1 ORDER BY row_number() OVER ()",
            "SELECT 1 ORDER BY count(*) OVER ()",
            "SELECT 1 ORDER BY (SELECT count(*))",
            "SELECT a FROM t ORDER BY count(*)",
            "SELECT 1 FROM t ORDER BY max(a)",
            "SELECT 1 ORDER BY 1 COLLATE nocase, BAD",
            "SELECT 1, 'b' UNION ALL SELECT 2, 'a' ORDER BY 2",
            "SELECT 3 UNION SELECT 1 ORDER BY 1 DESC",
            "SELECT 1 UNION ALL SELECT 2 ORDER BY BAD",
            // Still computed: a GROUP BY's sort, and a table's ORDER BY.
            "SELECT 1 GROUP BY 1 ORDER BY BAD",
            "SELECT a FROM t ORDER BY BAD",
            "SELECT BAD ORDER BY 1",
            "SELECT 1 LIMIT 0 OFFSET BAD",
            "SELECT 1 LIMIT 1 OFFSET BAD",
            "SELECT 1 LIMIT BAD",
            "SELECT 1 LIMIT 0, BAD",
            "SELECT 1 LIMIT BAD, 0",
            "SELECT 1 LIMIT 0 OFFSET 'x'",
            "SELECT 1 LIMIT 'x' OFFSET 0",
            "SELECT 1 LIMIT 0 OFFSET NULL",
            "SELECT 1 LIMIT 0 OFFSET 1.5",
            "SELECT 1 LIMIT 0 OFFSET x'01'",
            "SELECT 1 LIMIT -1 OFFSET BAD",
            "SELECT 1 LIMIT 1 OFFSET 'x'",
            "SELECT 1 LIMIT 0 OFFSET -1",
            "SELECT 1 LIMIT 1-1 OFFSET BAD",
            "SELECT 1 LIMIT (SELECT 0) OFFSET BAD",
            "SELECT 1 LIMIT 0 OFFSET (SELECT BAD)",
            "SELECT BAD LIMIT 0",
            "SELECT 1 WHERE BAD LIMIT 0",
            "SELECT count(*) WHERE BAD LIMIT 0",
            "SELECT DISTINCT 1 LIMIT 0 OFFSET BAD",
            "SELECT count(*) LIMIT 0 OFFSET BAD",
            "SELECT 1 ORDER BY 1 LIMIT 0 OFFSET BAD",
            "SELECT (SELECT 1 LIMIT 0 OFFSET BAD)",
            "SELECT * FROM (SELECT 1 LIMIT 0 OFFSET BAD)",
            "SELECT * FROM (SELECT BAD LIMIT 0)",
            "SELECT 1 LIMIT 2 OFFSET 0",
            "SELECT 1 LIMIT 1 OFFSET 1",
            "SELECT BAD UNION ALL SELECT 2 LIMIT 0",
            "SELECT 1 UNION SELECT BAD LIMIT 0",
            "SELECT BAD UNION SELECT 2 LIMIT (SELECT 0)",
            "SELECT BAD INTERSECT SELECT 2 LIMIT 0",
            "SELECT BAD EXCEPT SELECT 2 LIMIT 0",
            "SELECT 2 EXCEPT SELECT BAD LIMIT 0",
            "SELECT 1 UNION ALL SELECT 2 LIMIT 0 OFFSET 'x'",
            "SELECT 1 UNION SELECT 2 LIMIT 0 OFFSET 'x'",
            "SELECT 1 UNION SELECT 2 ORDER BY 1 LIMIT 0 OFFSET 'x'",
            "SELECT 1 UNION ALL SELECT 2 LIMIT 1 OFFSET 1",
            "SELECT a FROM t UNION SELECT BAD LIMIT 0",
            "SELECT a FROM t UNION ALL SELECT BAD ORDER BY 1 LIMIT 0",
            "SELECT a FROM t WHERE a = BAD UNION ALL SELECT 1 LIMIT 0",
            "SELECT a FROM t LIMIT 0 OFFSET BAD",
            "SELECT a FROM t ORDER BY a LIMIT 0 OFFSET 'x'",
            "SELECT 1 FROM t LIMIT 0 OFFSET 'x'",
            "SELECT count(*) FROM t LIMIT 0 OFFSET 'x'",
            "SELECT a FROM t WHERE BAD LIMIT 0",
            "SELECT BAD FROM t LIMIT 0",
            "SELECT a, sum(a) OVER () FROM t WHERE BAD LIMIT 0",
            "SELECT a, row_number() OVER (ORDER BY BAD) FROM t LIMIT 0",
            "SELECT a FROM t WHERE a IN (SELECT BAD) LIMIT 0",
            "SELECT max(a) FROM t WHERE BAD LIMIT 0",
            "SELECT count(*) FROM t GROUP BY BAD LIMIT 0",
            "SELECT a FROM t ORDER BY BAD LIMIT 0",
            "SELECT (SELECT BAD) FROM t LIMIT 0",
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+BAD FROM c) SELECT * FROM c LIMIT 0",
            "WITH RECURSIVE c(n) AS (SELECT BAD UNION ALL SELECT n+1 FROM c LIMIT 0) SELECT * FROM c",
            "SELECT a FROM t LIMIT 2 OFFSET 1",
            "SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1",
        ],
    );
}

// ----------------------------------------------------- constant propagation

/// The WHERE clause's constant propagation over the rowid, its alias
/// and the other columns, next to a failing constant term.
#[test]
fn constant_propagation_matches_the_pin() {
    corpus_bad(
        T,
        "cp",
        &[
            "SELECT * FROM t WHERE a = 5 AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a = 6 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a > 5 AND BAD",
            "SELECT * FROM t WHERE a > 5 AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND BAD",
            "SELECT * FROM t WHERE a = 7 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a < 6 AND BAD",
            "SELECT * FROM t WHERE BAD AND a = 5 AND a < 5",
            "SELECT * FROM t WHERE rowid = 5 AND rowid < 5 AND BAD",
            "SELECT * FROM t WHERE rowid = 5 AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND rowid < 5 AND BAD",
            "SELECT * FROM t WHERE _rowid_ = 5 AND oid < 5 AND BAD",
            "SELECT count(*) FROM t WHERE a = 5 AND a < 5 AND BAD",
            "SELECT b FROM t WHERE a = 5 AND a < 5 AND BAD GROUP BY b",
            "SELECT * FROM t WHERE a = 5 AND a < 5 AND BAD ORDER BY b",
            "SELECT * FROM t WHERE a = '5' AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5.0 AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5.5 AND a < 6 AND BAD",
            "SELECT * FROM t WHERE a = x'05' AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = NULL AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = -5 AND a > 0 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a IN (1, 2) AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a IN (1) AND BAD",
            "SELECT * FROM t WHERE a IN (5) AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a NOT IN (5) AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a BETWEEN 6 AND 7 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a NOT BETWEEN 1 AND 7 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND CASE a WHEN 5 THEN 0 ELSE 1 END AND BAD",
            "SELECT * FROM t WHERE a = 5 AND coalesce(a, 1) = 3 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND +a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a COLLATE NOCASE < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 COLLATE NOCASE AND a < 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a LIKE '6' AND BAD",
            "SELECT * FROM t WHERE a = 5 AND (a = 6 OR a = 7) AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a = 5 AND BAD",
            "SELECT * FROM t WHERE a = 5 AND a < 5 AND (SELECT BAD)",
            "SELECT * FROM t WHERE a = 5 AND a < 5 AND BAD AND b = 1",
            "SELECT * FROM t, t AS s WHERE t.a = 5 AND t.a < 5 AND BAD",
            "SELECT * FROM t LEFT JOIN t AS s ON s.a = 5 AND s.a < 5 AND BAD",
            "SELECT (SELECT b FROM t WHERE a = 5 AND a < 5 AND BAD)",
            "SELECT * FROM (SELECT * FROM t WHERE a = 5 AND a < 5 AND BAD)",
            "SELECT * FROM t WHERE d = 5 AND d < 5 AND BAD",
            "SELECT * FROM t WHERE d = 5 AND d < 6 AND BAD",
            "SELECT * FROM t WHERE d = 1+4 AND d < 5 AND BAD",
            "SELECT * FROM t WHERE d = (1+4) AND d IN (1, 2) AND BAD",
            "SELECT * FROM t WHERE d = abs(-5) AND d < 0 AND BAD",
            "SELECT * FROM t WHERE d = abs(-5) AND d > 0 AND BAD",
            "SELECT * FROM t WHERE d = BAD AND d < 5",
            "SELECT * FROM t WHERE d = BAD AND (0 AND d)",
            "SELECT * FROM t WHERE d = BAD AND b = 1",
            "SELECT * FROM t WHERE d = 1/0 AND d IS NULL AND BAD",
            "SELECT * FROM t WHERE d = CAST(5 AS INT) AND d < 5 AND BAD",
            "SELECT * FROM t WHERE d = (SELECT 5) AND d < 5 AND BAD",
            "SELECT * FROM t WHERE d = random() AND d < 5 AND BAD",
            "SELECT * FROM t WHERE d = 5 AND c = d AND c < 5 AND BAD",
            "SELECT * FROM t WHERE b = 5 AND b < 5 AND BAD",
            "SELECT * FROM t WHERE b = 5 AND b + 0 < 5 AND BAD",
            "SELECT * FROM t WHERE b = 5 AND b IN (1) AND BAD",
            "SELECT * FROM t WHERE b = 5 AND b IN (1, 2) AND BAD",
            "SELECT * FROM t WHERE b = 5 AND b BETWEEN 6 AND 7 AND BAD",
            "SELECT * FROM t WHERE c = 5 AND c < 5 AND BAD",
            "SELECT * FROM t WHERE c = 5 AND c LIKE 'x' AND BAD",
            "SELECT * FROM t WHERE c = 5 AND 5 < c AND BAD",
            "SELECT * FROM t WHERE e = 5 AND e < 5 AND BAD",
            "SELECT * FROM t WHERE f = '5' AND f < 5 AND BAD",
            "SELECT * FROM t WHERE c IN ('a' COLLATE NOCASE) AND c < 'a' AND BAD",
            // The rows themselves (no failing term).
            "SELECT a FROM t WHERE a = 5 AND a IN (5)",
            "SELECT a FROM t WHERE d = 5 AND d BETWEEN 1 AND 7",
            "SELECT a FROM t WHERE c = 1 AND c IN ('1')",
            "SELECT a FROM t WHERE c IN (1) AND c = '1'",
            "SELECT a FROM t WHERE b IN (1) AND b < 2",
            "SELECT a FROM t WHERE f = 5 AND f LIKE '5'",
            "SELECT a FROM t WHERE a = '5' AND a <= 5",
            "SELECT a FROM t WHERE e = 5.5 AND e > 5",
            "SELECT a FROM t WHERE c = 'abc' AND upper(c) = 'ABC'",
            // The write side does not propagate (only the pin's SELECT does).
            "DELETE FROM t WHERE a = 5 AND a < 5 AND BAD",
            "UPDATE t SET b = 0 WHERE a = 5 AND a < 5 AND BAD",
        ],
    );
}

/// One-item IN lists compare as `x = +c` on the pin: the item's own
/// affinity and collation do not count, the left operand's do.
#[test]
fn one_item_in_lists_match_the_pin() {
    corpus(
        "CREATE TABLE w(t TEXT, i INTEGER, b, n NUMERIC); \
         INSERT INTO w VALUES('1', 1, '1', 1), ('1.0', 1, 1, '1.0'), ('a', 2, 'A', 'a');",
        "in",
        &[
            "SELECT t IN (CAST(1 AS INTEGER)), t = CAST(1 AS INTEGER), t IN (1), t IN (1, 2) FROM w",
            "SELECT i IN ('1'), i IN (CAST('1' AS TEXT)), i = CAST('1' AS TEXT), i IN (CAST('1' AS TEXT), 5) FROM w",
            "SELECT b IN (CAST(1 AS INTEGER)), b = CAST(1 AS INTEGER), b IN (CAST(1 AS INTEGER), 5) FROM w",
            "SELECT b IN ('a' COLLATE NOCASE), b = 'a' COLLATE NOCASE, b IN ('a' COLLATE NOCASE, 'z') FROM w",
            "SELECT 1 IN (t), 1 = t, 1 IN (t, 5) FROM w",
            "SELECT b NOT IN (1), b NOT IN (1, 7), NULL NOT IN (1), 1 NOT IN (NULL) FROM w",
            "SELECT t IN (i), t = i, t IN (i, 99) FROM w",
            "SELECT n IN (CAST('1.0' AS TEXT)), n = CAST('1.0' AS TEXT) FROM w",
            "SELECT t FROM w WHERE t IN (CAST(1 AS INTEGER))",
            "SELECT t FROM w WHERE i IN (1) AND t IN ('1')",
            "SELECT t FROM w WHERE b IN (b), b IN (b, 1)",
        ],
    );
}

// ----------------------------------------------------- AND / OR simplification

#[test]
fn and_or_simplification_matches_the_pin() {
    corpus_bad(
        T,
        "ao",
        &[
            "SELECT 1 AND 5, 5 AND 1, 0 OR 5, 5 OR 0, 2 AND 'x', 1 AND NULL, NULL OR 0, 'x' OR 0",
            "SELECT BAD AND 0",
            "SELECT BAD OR 1",
            "SELECT 0 AND BAD, 1 OR BAD",
            "SELECT (BAD AND 1) AND 0",
            "SELECT BAD AND (1 AND 0)",
            "SELECT BAD AND (0 OR 0)",
            "SELECT BAD OR (1 AND 1)",
            "SELECT BAD OR (0 OR 1)",
            "SELECT BAD AND (1 OR 0)",
            "SELECT BAD AND (0 AND abs(1))",
            "SELECT BAD AND ((1 AND 0) OR 0)",
            "SELECT BAD AND (1 AND (0 OR 0))",
            "SELECT (0 OR 0) AND BAD",
            "SELECT (1 AND 1) OR BAD",
            "SELECT BAD AND -0",
            "SELECT BAD AND 0.0",
            "SELECT BAD AND '0'",
            "SELECT BAD AND (0)",
            "SELECT BAD AND +0",
            "SELECT BAD AND NOT 1",
            "SELECT BAD AND FALSE, BAD OR TRUE",
            "SELECT BAD AND 0x0",
            "SELECT CASE WHEN BAD AND (0 OR 0) THEN 1 ELSE 2 END",
            "SELECT a FROM t WHERE BAD AND (0 OR 0)",
            "SELECT a FROM t WHERE a > 1 AND (BAD OR (1 AND 1))",
            "SELECT a, (BAD OR (0 OR 1)) FROM t",
            "SELECT a FROM t WHERE (BAD AND (1 AND 0)) IS NULL",
            "SELECT count(*) FROM t WHERE BAD OR (0 OR 1)",
            "SELECT a AND (0 OR 0), a OR (1 AND 1), (1 AND 1) AND a, (0 OR 0) OR a FROM t",
            "SELECT a, (a AND (1 OR 0)), (a OR (0 AND 1)) FROM t",
            "UPDATE t SET b = 9 WHERE BAD AND (0 OR 0)",
            "DELETE FROM t WHERE (1 AND 0) AND BAD",
        ],
    );
}

// ------------------------------------------------------------- generated

/// A small deterministic generator (xorshift).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Generated WHERE clauses over `t`: conjunctions of terms on the
/// rowid, its alias and columns of every affinity (equalities that
/// fix a column, comparisons, IN / BETWEEN / LIKE / CASE operands,
/// failing constants), compared with the pin.
#[test]
fn generated_where_clauses_match_the_pin() {
    let mut rng = Rng(0x5eed_e017_0000_0017);
    let cols = ["a", "rowid", "b", "c", "d", "e", "f"];
    let vals = [
        "5", "'5'", "5.0", "-5", "1+4", "abs(-5)", "NULL", "'abc'", "x'05'", "12", "BAD",
    ];
    let mut qs = Vec::new();
    for _ in 0..140 {
        let n = 2 + (rng.next() % 3) as usize;
        let mut terms = Vec::new();
        for _ in 0..n {
            let c = rng.pick(&cols);
            let v = rng.pick(&vals);
            let t = match rng.next() % 11 {
                0..=2 => format!("{c} = {v}"),
                3 => format!("{c} < {v}"),
                4 => format!("{c} >= {v}"),
                5 => format!("{c} IN ({v})"),
                6 => format!("{c} IN ({v}, 1)"),
                7 => format!("{c} BETWEEN {v} AND 9"),
                8 => format!("{c} LIKE '5'"),
                9 => "BAD".to_string(),
                _ => format!("CASE {c} WHEN {v} THEN 0 ELSE 1 END"),
            };
            terms.push(t);
        }
        let sel = rng.pick(&["*", "a", "count(*)", "a, quote(b)"]);
        qs.push(format!("SELECT {sel} FROM t WHERE {}", terms.join(" AND ")));
    }
    let qs: Vec<String> = qs.iter().map(|q| q.replace("BAD", BAD)).collect();
    let bad = diffs(T, "gw", &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// Generated FROM-less and single-table queries with ORDER BY,
/// DISTINCT, aggregates, LIMIT and OFFSET terms (some failing),
/// compared with the pin.
#[test]
fn generated_order_and_limit_match_the_pin() {
    let mut rng = Rng(0x0dd5_17a7_e017_0003);
    let items = ["1", "1, 2", "'x'", "count(*)", "max(1)", "BAD", "a", "a, b"];
    let orders = [
        "",
        "1",
        "BAD",
        "1, BAD",
        "1 DESC",
        "(SELECT BAD)",
        "json('x')",
        "2",
    ];
    let limits = [
        "",
        "LIMIT 0",
        "LIMIT 1",
        "LIMIT -1",
        "LIMIT 0 OFFSET BAD",
        "LIMIT 1 OFFSET BAD",
        "LIMIT 0 OFFSET 'x'",
        "LIMIT 2 OFFSET 1",
        "LIMIT (SELECT 0) OFFSET BAD",
        "LIMIT BAD",
    ];
    let mut qs = Vec::new();
    for _ in 0..160 {
        let it = rng.pick(&items);
        let from = it.contains('a') || rng.next().is_multiple_of(4);
        let distinct = if rng.next().is_multiple_of(3) {
            "DISTINCT "
        } else {
            ""
        };
        let mut q = format!("SELECT {distinct}{it}");
        if from {
            q.push_str(" FROM t");
        }
        if rng.next().is_multiple_of(4) {
            q.push_str(rng.pick(&[" WHERE 1", " WHERE 0", " WHERE BAD"]));
        }
        let o = rng.pick(&orders);
        // (Over a table, an ORDER BY led by the rowid is the pin's
        // scan order, so its later terms go uncomputed there: a
        // planner rule this unit does not take; STATUS.md.)
        let o = if from && o == "1, BAD" { "BAD" } else { o };
        if !o.is_empty() {
            q.push_str(" ORDER BY ");
            q.push_str(o);
        }
        let l = rng.pick(&limits);
        if !l.is_empty() {
            q.push(' ');
            q.push_str(l);
        }
        if rng.next().is_multiple_of(5) {
            q = format!("SELECT * FROM ({q})");
        } else if rng.next().is_multiple_of(6) && !it.contains(',') {
            q = format!("SELECT ({q})");
        }
        qs.push(q.replace("BAD", BAD));
    }
    let bad = diffs(T, "go", &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// Generated hex() / substr() calls over literals and columns of
/// every storage class, compared with the pin.
#[test]
fn generated_hex_substr_match_the_pin() {
    let mut rng = Rng(0x4e78_5b57_e017_0005);
    let xs = [
        "x''",
        "''",
        "NULL",
        "x'00'",
        "x'0102'",
        "'abc'",
        "'é€'",
        "123",
        "-1.5",
        "zeroblob(0)",
        "b",
        "c",
        "d",
        "e",
        "f",
    ];
    let ns = ["0", "1", "2", "-1", "-5", "NULL", "'x'", "'2'", "1.9", "5"];
    let mut qs = Vec::new();
    for _ in 0..80 {
        let mut cols = Vec::new();
        for _ in 0..4 {
            let x = rng.pick(&xs);
            cols.push(match rng.next() % 3 {
                0 => format!("quote(hex({x}))"),
                1 => format!("quote(substr({x}, {}))", rng.pick(&ns)),
                _ => format!("quote(substr({x}, {}, {}))", rng.pick(&ns), rng.pick(&ns)),
            });
        }
        qs.push(format!("SELECT a, {} FROM t", cols.join(", ")));
    }
    let bad = diffs(T, "gh", &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

// ------------------------------------------------------- empty statements

/// Each text prepared and stepped on a fresh in-memory connection:
/// the rows, or `None` when prepare or step failed.
fn mem_run(setup: &[&str], sql: &str) -> Option<Vec<String>> {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        for s in setup {
            let mut q = db.prepare(s).await.expect("setup prepare");
            while q.step().await.expect("setup step") == Step::Row {}
        }
        let mut q = db.prepare(sql).await.ok()?;
        let mut rows = Vec::new();
        loop {
            match q.step().await {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => return Some(rows),
                Err(_) => return None,
            }
        }
    })
}

/// An empty statement prepares and does nothing, as the pin's
/// prepare (no statement) and exec (skipped) treat it; a statement
/// followed by more `;` prepares as itself.
#[test]
fn empty_statements_do_nothing() {
    for sql in [
        "",
        ";",
        ";;",
        "  ;  ",
        " -- c",
        "/* x */ ;",
        "-- a\n;\n-- b",
    ] {
        assert_eq!(mem_run(&[], sql), Some(Vec::new()), "{sql:?}");
    }
    assert_eq!(
        mem_run(&[], "SELECT 1;;"),
        Some(vec!["1".to_string()]),
        "trailing semicolons"
    );
    assert_eq!(mem_run(&[], "SELECT 1; ; ;"), Some(vec!["1".to_string()]));
    // Not two statements: the rest is still refused.
    assert_eq!(mem_run(&[], "SELECT 1;; SELECT 2"), None);
    // EXPLAIN QUERY PLAN of nothing is a syntax error on the pin.
    assert_eq!(mem_run(&[], "EXPLAIN QUERY PLAN ;"), None);
    assert_eq!(mem_run(&[], "EXPLAIN QUERY PLAN"), None);
    assert_eq!(mem_run(&[], "EXPLAIN ;"), None);
    // It has no columns, and leaves the connection's counters and
    // its transaction alone.
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        for s in [
            "CREATE TABLE t(a INTEGER PRIMARY KEY)",
            "BEGIN",
            "INSERT INTO t VALUES (7), (8)",
        ] {
            let mut q = db.prepare(s).await.expect("prepare");
            assert_eq!(q.step().await.expect("step"), Step::Done);
        }
        {
            let mut q = db.prepare(";;").await.expect("prepare empty");
            assert_eq!(q.column_count(), 0);
            assert_eq!(q.step().await.expect("step"), Step::Done);
            assert_eq!(q.step().await.expect("step again"), Step::Done);
        }
        assert_eq!(db.changes(), 2);
        assert_eq!(db.total_changes(), 2);
        assert_eq!(db.last_insert_rowid(), 8);
        assert!(db.in_explicit_transaction());
        let mut q = db.prepare("ROLLBACK").await.expect("prepare");
        assert_eq!(q.step().await.expect("step"), Step::Done);
    });
}

/// Single statements with empty ones after them, against the pin's
/// CLI one-shot mode: the same output and success (scripts of several
/// statements are the CLI suite's, `cli_v17_eval_gaps.rs`).
#[test]
fn empty_statements_match_the_pin() {
    corpus(
        "CREATE TABLE z(a); INSERT INTO z VALUES (1);",
        "es",
        &[
            ";",
            ";;",
            "  ;  -- c",
            "/* x */ ;",
            "SELECT 1; ; ;",
            "SELECT a FROM z;;",
            "PRAGMA user_version; ;",
            "INSERT INTO z VALUES (2);;",
        ],
    );
}
