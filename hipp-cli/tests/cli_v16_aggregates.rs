//! hipp-cli v0.16 differentials: aggregate completion must render
//! byte-identically to the pinned oracle — string_agg() and the
//! aggregate ORDER BY (DISTINCT interplay included), FILTER on
//! every aggregate (bare columns too), the percentile family's
//! REAL results, the compensated REAL sums and text addends of
//! sum() / total() / avg(), group_concat() separators (NULL ones,
//! blobs, control characters), and CREATE TABLE ... AS SELECT
//! over these clauses (the pin's column names), plus the review
//! fixes (min / max ORDER BY ownership, `count(ORDER BY ...)`, NaN
//! results as NULL, derived-table LIMITs, HAVING terms moved into
//! WHERE). Each batch runs
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
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16agg-{tag}"));
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
CREATE TABLE t(a, b, c TEXT COLLATE NOCASE, r REAL, g);
INSERT INTO t VALUES (1,'x','A',1.5,1),(2,'y','a',2,1),(3,'z','B',NULL,2),(NULL,'w',NULL,0.1,2),(2,'v','b',0.2,3),(7,'u','c',-3,3),(x'0102','t','C',0.3,3);
";

/// `sql`: string_agg() and the aggregate ORDER BY.
const ORDER: &str = "\
SELECT string_agg(b, ', ' ORDER BY a DESC, b), group_concat(b ORDER BY c, r DESC NULLS FIRST) FROM t;
SELECT g, group_concat(b, '' ORDER BY b), string_agg(c, '/' ORDER BY c COLLATE BINARY) FROM t GROUP BY g ORDER BY g;
SELECT group_concat(DISTINCT c ORDER BY c), group_concat(DISTINCT c ORDER BY b DESC), count(DISTINCT c ORDER BY c) FROM t;
SELECT hex(group_concat(a ORDER BY r)), group_concat(a, '|' ORDER BY r) FROM t;
SELECT sum(r ORDER BY r DESC), total(r ORDER BY b), avg(r ORDER BY g, b) FROM t;
";

/// `sql`: FILTER, and its effect on the bare columns.
const FILTER: &str = "\
SELECT count(*) FILTER (WHERE a > 1), sum(a) FILTER (WHERE g = 3), avg(r) FILTER (WHERE r > 0), group_concat(b) FILTER (WHERE c = 'a') FROM t;
SELECT g, count() FILTER (WHERE r IS NOT NULL), max(r) FILTER (WHERE b < 'x'), b FROM t GROUP BY g ORDER BY g;
SELECT min(a) FILTER (WHERE a > 99), b, count(DISTINCT c) FILTER (WHERE c <> 'c') FROM t;
SELECT g FROM t GROUP BY g HAVING count(*) FILTER (WHERE r > 0.15) > 1 ORDER BY sum(r) FILTER (WHERE r < 1) DESC;
SELECT (SELECT count(*) FILTER (WHERE t.g > 1) FROM t u), (SELECT string_agg(u.b, '' ORDER BY u.b) FILTER (WHERE u.g = t.g) FROM t u) FROM t ORDER BY 2;
";

/// `sql`: the percentile family.
const PERCENTILE: &str = "\
SELECT median(r), percentile(r, 25), percentile(r, 99.9), percentile_cont(r, 0.4), percentile_disc(r, 0.4) FROM t;
SELECT g, median(r), percentile(r, '50'), percentile_disc(r, 1) FROM t GROUP BY g ORDER BY g;
SELECT median(x), percentile(x, 33.3) FROM (SELECT 3 x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 0.5 UNION ALL SELECT 9007199254740993);
SELECT median(r) FILTER (WHERE r > 0), typeof(median(r)), median(r) FILTER (WHERE 0) IS NULL FROM t;
";

/// Ungated: sum() / total() / avg() and group_concat() fixes.
const SUMS: &str = "\
SELECT sum(x), total(x), avg(x) FROM (SELECT 0.1 x UNION ALL SELECT 0.2 UNION ALL SELECT 0.3);
SELECT sum(x), avg(x) FROM (SELECT 1e18 x UNION ALL SELECT 1 UNION ALL SELECT -1e18);
SELECT total(9223372036854775807), sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT 1.0);
SELECT sum(x), typeof(sum(x)) FROM (SELECT '  12abc' x UNION ALL SELECT x'31');
SELECT sum(x), typeof(sum(x)) FROM (SELECT ' 5 ' x UNION ALL SELECT '+7');
SELECT group_concat(x, s) FROM (SELECT 'a' x, '1' s UNION ALL SELECT 'b', NULL UNION ALL SELECT 'c', char(9));
SELECT group_concat(x, char(1)) FROM (SELECT 'a' x UNION ALL SELECT 'b');
";

/// `sql`: CREATE TABLE ... AS SELECT over the new clauses.
const CTAS: &str = "\
CREATE TABLE c AS SELECT group_concat(b ORDER BY a DESC), count(*) FILTER (WHERE a > 1), median(r) AS m FROM t;
SELECT * FROM c;
.schema c
";

/// Review fixes: min / max ORDER BY ownership, `count(ORDER BY
/// ...)`, NaN sums and arithmetic rendering (and stored) as NULL,
/// an outer LIMIT stopping a derived table's groups, HAVING terms
/// the pin moves into WHERE.
const REVIEW: &str = "\
CREATE TABLE big(k, v); INSERT INTO big VALUES (1,1),(2,9223372036854775807),(2,1),(3,5);
SELECT a, (SELECT max(t.a ORDER BY u.b) FROM t u) FROM t ORDER BY 1;
SELECT (SELECT max(1 ORDER BY t.a) FROM t u) FROM t;
SELECT count(ORDER BY b), count(ORDER BY nosuch) FILTER (WHERE a > 1), count(ORDER BY b DESC) FROM t;
SELECT sum(x), total(x), avg(x), typeof(sum(x)), count(x) FROM (SELECT 9e999 x UNION ALL SELECT -9e999);
SELECT 9e999 - 9e999, 0 * 9e999, 9e999 / -9e999, 9e999 + 9e999;
CREATE TABLE n AS SELECT total(x) s, 9e999 - 9e999 d FROM (SELECT 9e999 x UNION ALL SELECT -9e999);
SELECT s, d, typeof(s), typeof(d) FROM n;
SELECT k FROM (SELECT k, sum(v) s FROM big GROUP BY k) LIMIT 1;
SELECT * FROM (SELECT * FROM (SELECT k, sum(v) s FROM big GROUP BY k)) LIMIT 1;
SELECT k, sum(v) FROM big GROUP BY k HAVING k = 1 AND abs(k) = 1;
SELECT k, sum(v) FROM big GROUP BY k HAVING abs(k) = 3 AND 0;
";

fn batch(body: &str) -> String {
    format!("{SETUP}{body}")
}

#[test]
fn cli_v16_aggregates_order_byte_identical() {
    byte_identical("order", &batch(ORDER));
}

#[test]
fn cli_v16_aggregates_filter_byte_identical() {
    byte_identical("filter", &batch(FILTER));
}

#[test]
fn cli_v16_aggregates_percentile_byte_identical() {
    byte_identical("pct", &batch(PERCENTILE));
}

#[test]
fn cli_v16_aggregates_sums_byte_identical() {
    byte_identical("sums", &batch(SUMS));
}

#[test]
fn cli_v16_aggregates_ctas_stdin_byte_identical() {
    // `.schema` is a dot command: stdin only.
    let a = temp_db("ctas-hipp");
    let b = temp_db("ctas-ora");
    let input = batch(CTAS);
    let got = run_stdin(&hipp_bin(), &a, &input);
    assert_eq!(got, run_stdin(&oracle(), &b, &input));
    assert!(!got.is_empty());
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

#[test]
fn cli_v16_aggregates_review_byte_identical() {
    byte_identical("review", &batch(REVIEW));
}
