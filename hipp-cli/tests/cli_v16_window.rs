//! hipp-cli v0.16 differentials: window functions (feature
//! `window`) must render byte-identically to the pinned oracle —
//! the built-in window functions (REAL percent_rank / cume_dist
//! included), every aggregate over ROWS / RANGE / GROUPS frames
//! with EXCLUDE, the REAL sums the pin's step / inverse order
//! makes, named windows, the output order of nested window classes,
//! windows over groups, in subqueries, views and CTEs, CREATE TABLE
//! ... AS SELECT over window calls (its `.schema`), and bound
//! parameters in frame offsets. Each batch runs one-shot and over
//! stdin, and the hipp-written file must pass the oracle's
//! integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16win-{tag}"));
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

const SETUP: &str = "\
CREATE TABLE t(a, b, c, n TEXT COLLATE NOCASE, r REAL);
INSERT INTO t VALUES (1,'x',5,'A',0.1),(2,'y',3,'a',0.2),(1,'z',4,'B',0.3),(2,'w',1,NULL,1e16),(1,'q',2,'b',-1e16),(0,'p',6,'c',2.5),(NULL,'o',7,'C',NULL);
";

/// The built-in window functions.
const BUILTIN: &str = "\
SELECT b, a, row_number() OVER (ORDER BY a), rank() OVER (ORDER BY a), dense_rank() OVER (ORDER BY a) FROM t;
SELECT a, percent_rank() OVER w, cume_dist() OVER w, ntile(3) OVER w FROM t WINDOW w AS (ORDER BY a);
SELECT c, lag(c) OVER (ORDER BY c), lead(c, 2, -1) OVER (ORDER BY c), lag(b, 1, 'none') OVER (PARTITION BY a ORDER BY c) FROM t;
SELECT c, first_value(b) OVER w, last_value(b) OVER w, nth_value(b, 2) OVER w FROM t WINDOW w AS (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING);
SELECT n, row_number() OVER (PARTITION BY n ORDER BY c DESC), rank() OVER (ORDER BY n NULLS LAST) FROM t;
";

/// Aggregates over frames, with the pin's REAL rendering.
const FRAMES: &str = "\
SELECT c, sum(r) OVER (ORDER BY c ROWS 1 PRECEDING), total(r) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING), avg(r) OVER (ORDER BY c) FROM t;
SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING), sum(c + 0.5) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t;
SELECT a, c, sum(c) OVER (ORDER BY a RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING), sum(c) OVER (ORDER BY a GROUPS 1 PRECEDING EXCLUDE GROUP), count(*) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t;
SELECT c, group_concat(b) OVER (ORDER BY c ROWS 2 PRECEDING), string_agg(n, '/') OVER (ORDER BY c ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING), min(n) OVER (ORDER BY c ROWS 1 PRECEDING), max(n) OVER (ORDER BY c) FROM t;
SELECT a, median(r) OVER (PARTITION BY a), percentile(c, 25) OVER (ORDER BY c ROWS 2 PRECEDING), sum(c) FILTER (WHERE c > 2) OVER (ORDER BY c) FROM t;
SELECT r, sum(r) OVER (ORDER BY r RANGE BETWEEN 0.5 PRECEDING AND 0.5 FOLLOWING), count(*) OVER (ORDER BY r DESC RANGE BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) FROM t;
";

/// Named windows, nested classes, groups, subqueries, views, CTEs.
const SHAPES: &str = "\
SELECT c, sum(c) OVER w, sum(c) OVER (w ROWS 1 PRECEDING) FROM t WINDOW w AS (ORDER BY c);
SELECT b, sum(a) OVER z FROM t WINDOW w AS (PARTITION BY a), y AS (w), z AS (y ORDER BY c);
SELECT b, a, c, rank() OVER (ORDER BY b), row_number() OVER (ORDER BY a DESC), row_number() OVER (ORDER BY c) FROM t;
SELECT a, count(*), sum(count(*)) OVER (ORDER BY a), rank() OVER (ORDER BY count(*) DESC) FROM t GROUP BY a;
SELECT DISTINCT a, count(*) OVER (PARTITION BY a) FROM t;
SELECT a, (SELECT count(*) OVER () FROM t u WHERE u.a = t.a LIMIT 1) FROM t;
SELECT * FROM (SELECT a, row_number() OVER (ORDER BY c) rn FROM t) WHERE rn > 3;
WITH q AS (SELECT a, c FROM t) SELECT a, c, lag(c) OVER (PARTITION BY a ORDER BY c) FROM q;
CREATE VIEW v AS SELECT a, b, rank() OVER (ORDER BY c) r FROM t;
SELECT * FROM v WHERE r < 4 ORDER BY r;
SELECT a, b FROM t ORDER BY row_number() OVER (ORDER BY c DESC) LIMIT 3;
VALUES (row_number() OVER (), sum(5) OVER (), lag(3) OVER ()), (2, 2, 2);
";

/// CREATE TABLE ... AS SELECT over window calls, and bound
/// parameters in frame offsets (stdin only: dot commands).
const CTAS_PARAMS: &str = "\
CREATE TABLE u AS SELECT a, row_number() OVER (ORDER BY c) rn, sum(r) OVER (ORDER BY c) s, cume_dist() OVER () cd FROM t;
SELECT * FROM u;
.schema u
.parameter set ?1 1
.parameter set :n 2
SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN ?1 PRECEDING AND CURRENT ROW), nth_value(c, :n) OVER (ORDER BY c) FROM t;
";

/// Review regressions: ORDER BY naming window columns, windows
/// over groups under an outer ORDER BY, `f(...) over` aliases,
/// `f(*)` built-ins.
const ORDERING: &str = "\
SELECT b, count(*) OVER (PARTITION BY a) c, row_number() OVER (ORDER BY n) FROM t ORDER BY c;
SELECT b, row_number() OVER (ORDER BY a) ra, row_number() OVER (ORDER BY n) rn, sum(r) OVER (ORDER BY a ROWS 1 PRECEDING) s FROM t ORDER BY ra, 3;
SELECT b, row_number() OVER (ORDER BY a) ra, row_number() OVER (ORDER BY n) rn FROM t ORDER BY -ra;
SELECT a, row_number() OVER (), sum(sum(r)) OVER (ROWS UNBOUNDED PRECEDING) FROM t GROUP BY a ORDER BY a DESC;
SELECT a, first_value(a) OVER (ORDER BY count(*) DESC), avg(r) FROM t GROUP BY a ORDER BY 3;
SELECT count(*) over FROM t;
SELECT abs(1) over UNION SELECT 2.5;
SELECT row_number(*) OVER (ORDER BY c), cume_dist(*) OVER (ORDER BY a) FROM t;
";

fn batch(body: &str) -> String {
    format!("{SETUP}{body}")
}

#[test]
fn cli_v16_window_builtin_byte_identical() {
    byte_identical("builtin", &batch(BUILTIN));
}

#[test]
fn cli_v16_window_frames_byte_identical() {
    byte_identical("frames", &batch(FRAMES));
}

#[test]
fn cli_v16_window_shapes_byte_identical() {
    byte_identical("shapes", &batch(SHAPES));
}

#[test]
fn cli_v16_window_ctas_params_stdin_byte_identical() {
    let a = temp_db("ctas-hipp");
    let b = temp_db("ctas-ora");
    let input = batch(CTAS_PARAMS);
    let got = run_stdin(&hipp_bin(), &a, &input);
    assert_eq!(got, run_stdin(&oracle(), &b, &input));
    assert!(!got.is_empty());
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

#[test]
fn cli_v16_window_ordering_byte_identical() {
    byte_identical("ordering", &batch(ORDERING));
}
