//! hipp-cli v0.16 depth limits: the CLI runs the pin's limits
//! (1000-deep expressions, a 2500-symbol parser stack) on its
//! large-stack thread, so a batch of statements on both sides of
//! every boundary prints byte-identical output to the pinned oracle
//! — the accepted statement's rows, nothing for the refused one
//! (whose error goes to stderr on both), and the statements after
//! it. Errors are compared by presence only: hipp's messages are
//! not the pin's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Run `input` over stdin against `:memory:`; (stdout, number of
/// error lines on stderr). Neither engine stops at an error there.
fn run_stdin(bin: &Path, input: &str) -> (String, usize) {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    let err = String::from_utf8_lossy(&out.stderr);
    let errors = err
        .lines()
        .filter(|l| l.starts_with("Error") || l.starts_with("Parse error"))
        .count();
    (String::from_utf8(out.stdout).expect("utf8 stdout"), errors)
}

fn rep(s: &str, n: usize) -> String {
    s.repeat(n)
}

/// A statement of nesting depth `n`.
type Gen = fn(usize) -> String;

/// Each statement at the pin's largest accepted depth and one past
/// it, separated by marker rows.
fn boundary_batch() -> (String, usize) {
    let shapes: Vec<(Gen, usize)> = vec![
        (
            |n| format!("SELECT {}a{} FROM t", rep("(", n), rep(")", n)),
            2493,
        ),
        (|n| format!("SELECT {}1", rep("+", n)), 2494),
        (|n| format!("SELECT {}a FROM t", rep("- ", n)), 999),
        (|n| format!("SELECT a{} FROM t", rep("+a", n)), 999),
        (
            |n| format!("SELECT {}a{} FROM t", rep("abs(", n), rep(")", n)),
            831,
        ),
        (
            |n| {
                format!(
                    "SELECT {}a{} FROM t",
                    rep("CASE WHEN a THEN ", n),
                    rep(" END", n)
                )
            },
            498,
        ),
        (
            |n| format!("SELECT {}1{}", rep("(SELECT ", n), rep(")", n)),
            43,
        ),
        (
            |n| {
                format!(
                    "SELECT * FROM {}(SELECT 1){}",
                    rep("(SELECT * FROM ", n),
                    rep(")", n)
                )
            },
            414,
        ),
        (
            |n| format!("SELECT 1 UNION SELECT {}'x'", rep("NOT ", n)),
            998,
        ),
        (
            |n| format!("INSERT INTO t VALUES (1, 2, {}1)", rep("+", n)),
            2489,
        ),
        (|n| format!("UPDATE t SET a = {}a", rep("- ", n)), 999),
        (
            |n| format!("SELECT count(*){}", rep(" UNION ALL SELECT 1", n - 1)),
            500,
        ),
        (|n| format!("SELECT max({})", vec!["1"; n].join(",")), 1000),
        (
            |n| format!("SELECT count(*) FROM {}", vec!["one"; n].join(",")),
            64,
        ),
        (
            |n| format!("SELECT a FROM t ORDER BY 2 NOTNULL{}", rep(", a", n - 1)),
            2000,
        ),
    ];
    // `one` keeps its one row: a 64-way join of a growing table
    // would count 2^64 rows.
    let mut sql = String::from(
        "CREATE TABLE t(a, b, c);\nINSERT INTO t VALUES (1, 2, 3);\nCREATE TABLE one(x);\nINSERT INTO one VALUES (1);\n",
    );
    let mut refused = 0;
    for (i, (f, max)) in shapes.iter().enumerate() {
        sql.push_str(&format!(
            "SELECT 'shape {i}';\n{};\n{};\n",
            f(*max),
            f(max + 1)
        ));
        refused += 1;
    }
    sql.push_str("SELECT 'end', count(*) FROM t;\n");
    (sql, refused)
}

#[test]
fn boundaries_byte_identical_over_stdin() {
    let (sql, refused) = boundary_batch();
    let (want, pin_errors) = run_stdin(&oracle(), &sql);
    let (got, hipp_errors) = run_stdin(&hipp_bin(), &sql);
    assert_eq!(got, want);
    assert_eq!(
        pin_errors, refused,
        "the pin refuses each statement past its limit"
    );
    assert_eq!(hipp_errors, refused);
}

/// The ordinal fold (`ORDER BY 2 NOTNULL` is ORDER BY 2 on the pin)
/// and the count limits, rendered.
#[test]
fn ordinals_and_counts_byte_identical() {
    let cols = (0..2001)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "CREATE TABLE t(a, b);
INSERT INTO t VALUES (3, 'x'), (1, 'y'), (2, 'z');
SELECT a FROM t ORDER BY 2 NOTNULL;
SELECT b, count(*) FROM t GROUP BY 3 NOT NULL;
SELECT a FROM t ORDER BY 1 ISNULL;
SELECT a, b FROM t ORDER BY 'x' IS NOT NULL DESC;
CREATE TABLE wide({cols});
SELECT count(*) FROM sqlite_schema;
SELECT {} FROM t LIMIT 1;
SELECT 'after';
",
        vec!["a"; 2000].join(",")
    );
    let (want, _) = run_stdin(&oracle(), &sql);
    let (got, _) = run_stdin(&hipp_bin(), &sql);
    assert_eq!(got, want);
}
