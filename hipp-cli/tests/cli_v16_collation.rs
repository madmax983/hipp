//! hipp-cli v0.16 differentials: collating sequences and pattern
//! matching must render byte-identically to the pinned oracle —
//! NOCASE / RTRIM column collations in comparisons, ORDER BY,
//! GROUP BY, DISTINCT, aggregates and compounds (REAL, NULL and
//! blob rendering mixed in), NOCASE / RTRIM indexes and UNIQUE
//! constraints under OR IGNORE / OR REPLACE / upsert, and under
//! `v16` the COLLATE operator, GLOB, LIKE ... ESCAPE and the
//! like() / glob() functions. Each batch runs one-shot and over
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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16co-{tag}-{}", std::process::id()));
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

const COLUMNS: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT COLLATE NOCASE, r TEXT COLLATE RTRIM, b, x REAL);
INSERT INTO t VALUES (1, 'abc', 'x  ', 'ABC', 1.5), (2, 'B', 'y', 'b', 2), (3, 'a', NULL, x'41', NULL),
  (4, 'ABC', 'x', 'abc', 0.25), (5, NULL, 'Y', NULL, -1), (6, 'b', 'y ', 'B', 1e10);
SELECT id, n = b, b = n, n = 'ABC', r = 'x', n < 'b', n IN ('abc', 'b'), 'ABC' IN (n), x FROM t;
SELECT n, id, x FROM t ORDER BY n, id DESC;
SELECT n, count(*), group_concat(id), sum(x) FROM t GROUP BY n;
SELECT r, count(*), max(x) FROM t GROUP BY r ORDER BY r DESC;
SELECT DISTINCT n FROM t;
SELECT min(n), max(n), min(r), max(r), count(DISTINCT n), group_concat(DISTINCT r) FROM t;
SELECT max(n), id, x FROM t;
SELECT n FROM t UNION SELECT b FROM t;
SELECT b FROM t UNION SELECT n FROM t ORDER BY 1 DESC;
SELECT n FROM t INTERSECT SELECT 'A' UNION SELECT 'c';
SELECT n, x FROM t EXCEPT SELECT 'b', 2.0;
SELECT nullif(n, 'ABC'), max(n, 'Z'), min(1, 1.0), CASE n WHEN 'ABC' THEN 'hit' END FROM t;
SELECT c FROM (SELECT n AS c FROM t) WHERE c = 'B';
SELECT id FROM t WHERE n IN (SELECT b FROM t WHERE id > 3);
";

const INDEXES: &str = "\
CREATE TABLE u(a TEXT UNIQUE COLLATE NOCASE, b COLLATE RTRIM, c, UNIQUE (b, c COLLATE NOCASE));
CREATE INDEX ua ON u(a DESC);
CREATE INDEX uc ON u(c COLLATE NOCASE, b);
INSERT INTO u VALUES ('x', 'p', 1.5), ('Y', 'q ', 'z'), ('w', 'q', 'Z ');
INSERT OR IGNORE INTO u VALUES ('X', 'r', 2), ('v', 'q  ', 'Z');
INSERT OR REPLACE INTO u VALUES ('y', 's', 3.25);
INSERT INTO u VALUES ('W', 't', 4) ON CONFLICT (a) DO UPDATE SET c = excluded.c * 2;
SELECT * FROM u;
SELECT a, b FROM u WHERE a = 'Y';
SELECT * FROM u WHERE b = 'q';
SELECT a FROM u ORDER BY a;
UPDATE OR REPLACE u SET a = 'W' WHERE a = 'x';
DELETE FROM u WHERE c = 'z';
SELECT rowid, * FROM u;
";

const V16: &str = "\
SELECT 'a' = 'A' COLLATE NOCASE, 'a ' = 'a' COLLATE RTRIM, 'a' COLLATE BINARY = 'A' COLLATE NOCASE;
CREATE TABLE w(a, b TEXT COLLATE NOCASE);
INSERT INTO w VALUES ('B', 'b'), ('a', 'A'), ('b', 'B'), ('A', 'a'), (2.5, 'x'), (x'61', NULL);
SELECT a FROM w ORDER BY a COLLATE NOCASE, rowid;
SELECT a COLLATE NOCASE, count(*) FROM w GROUP BY 1;
SELECT DISTINCT a COLLATE NOCASE FROM w;
SELECT b FROM w UNION SELECT a FROM w ORDER BY 1 COLLATE BINARY;
SELECT a, a GLOB '[a-b]*', a LIKE 'b', a LIKE 'B%' ESCAPE '%', glob('*5', a), like('_', a) FROM w;
SELECT 'a%c' LIKE 'a\\%c' ESCAPE '\\', 'abc' NOT GLOB '?[^b]?', like('a|_', 'a_', '|'), glob('é*', 'éa');
SELECT 'x' LIKE NULL ESCAPE 'a', like('x', 'x', NULL), 1.5 GLOB '1.?';
";

#[test]
fn cli_v16_column_collations_byte_identical() {
    byte_identical("columns", COLUMNS);
}

#[test]
fn cli_v16_collated_indexes_byte_identical() {
    byte_identical("indexes", INDEXES);
}

#[test]
fn cli_v16_collate_operator_and_patterns_byte_identical() {
    byte_identical("v16", V16);
}

/// A NOCASE-indexed table hipp wrote dumps and replays into the
/// oracle with identical rows and a clean integrity check.
#[test]
fn cli_v16_dump_replays_collated_schema() {
    let h = temp_db("dump-coll-h");
    run(&hipp_bin(), &h, INDEXES);
    let dump = run(&hipp_bin(), &h, ".dump");
    let dump = String::from_utf8_lossy(&dump).into_owned();
    let o = temp_db("dump-coll-o");
    run(&oracle(), &o, &dump);
    let q = "SELECT * FROM u; SELECT a FROM u ORDER BY a;";
    assert_eq!(run(&oracle(), &o, q), run(&hipp_bin(), &h, q));
    assert_eq!(run(&oracle(), &o, "PRAGMA integrity_check;"), b"ok\n");
}
