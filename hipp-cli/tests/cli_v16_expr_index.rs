//! hipp-cli v0.16 differentials: expression indexes, partial indexes
//! and index-aware planning (leaf feature `expr_index`) must render
//! byte-identically to the pinned oracle — the `sqlite_schema`
//! texts, PRAGMA index_list / index_info /
//! index_xinfo (cid -2, empty name), the rows written through the
//! indexes, and the row order of queries the pin answers through an
//! index (ranges, IN lists, covering scans, ORDER BY ties, GROUP BY
//! over a DESC index, LIMIT), plus a generated corpus of random
//! schemas, indexes and queries. Each batch runs one-shot and over
//! stdin, and the hipp-written file must pass the oracle's integrity
//! check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16xi-{tag}"));
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

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run(&oracle(), &b, batch)),
        "{tag}"
    );
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, batch)),
        "{tag} (stdin)"
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Definitions, introspection and writes through the indexes.
const SCHEMA: &str = r#"
CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d AS (b || '!') VIRTUAL);
CREATE INDEX e1 ON t(lower(b) COLLATE NOCASE DESC, c);
CREATE UNIQUE INDEX p1 ON t(c) WHERE c > 0;
CREATE INDEX e2 ON t(d, a + 1) WHERE b IS NOT NULL;
CREATE INDEX e3 ON t((b), +c, 1);
INSERT INTO t(a, b, c) VALUES (1, 'X', 1), (2, 'y', -1), (3, NULL, 2), (4, 'kk', -1), (5, 'Abc', 0.5);
INSERT OR IGNORE INTO t(b, c) VALUES ('dup', 2);
INSERT OR REPLACE INTO t(b, c) VALUES ('rep', 1);
UPDATE t SET b = upper(b) WHERE c < 0;
INSERT INTO t(b, c) VALUES ('up', 2) ON CONFLICT (c) WHERE c > 0 DO UPDATE SET b = 'hit';
DELETE FROM t WHERE a = 4;
SELECT name, sql FROM sqlite_schema ORDER BY name;
PRAGMA index_list(t);
PRAGMA index_xinfo(e1);
PRAGMA index_info(e2);
PRAGMA index_xinfo(e3);
SELECT * FROM t;
SELECT a, d FROM t WHERE lower(b) = 'hit' COLLATE NOCASE;
SELECT a FROM t ORDER BY lower(b) COLLATE NOCASE DESC, c;
REINDEX;
PRAGMA integrity_check;
ALTER TABLE t RENAME COLUMN c TO cc;
SELECT sql FROM sqlite_schema WHERE type = 'index' ORDER BY name;
"#;

/// Row order through the indexes, as the pin's planner picks them.
const ORDER: &str = r#"
CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);
INSERT INTO t VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c'), (4, 3, 'd'), (5, NULL, 'e'), (6, 'x', 'f'), (7, 1.5, 'g');
CREATE INDEX ib ON t(b);
SELECT a FROM t WHERE b > 2;
SELECT a FROM t WHERE b > 2 LIMIT 2;
SELECT b FROM t;
SELECT DISTINCT b FROM t;
SELECT a FROM t WHERE b IN (9, 3, 'x');
SELECT a FROM t WHERE b = 3 OR b = 9;
SELECT a FROM t ORDER BY b DESC;
SELECT a, (SELECT a FROM t AS u WHERE u.b > t.b) FROM t;
CREATE TABLE d(k, v);
CREATE INDEX dk ON d(k DESC);
INSERT INTO d VALUES (1, 'a'), (2, 'b'), (3, 'c'), (2, 'bb'), (1, 'aa');
SELECT v FROM d WHERE k IN (1, 3, 2);
SELECT k, group_concat(v) FROM d GROUP BY k;
SELECT v FROM d ORDER BY k LIMIT 3;
CREATE TABLE x(a, b, c);
CREATE INDEX xe ON x(a * 10 + b);
CREATE INDEX xp ON x(c) WHERE b > 0;
INSERT INTO x VALUES (1, 2, 'z'), (0, 12, 'y'), (3, 4, 'x'), (0, -1, 'w'), (1, 1, 'v');
SELECT * FROM x WHERE a * 10 + b > 10;
SELECT c FROM x WHERE b > 0;
CREATE TABLE u(k INTEGER PRIMARY KEY, v UNIQUE, w);
INSERT INTO u VALUES (1, 3, 1), (2, 2, 2), (3, 1, 3);
CREATE INDEX uw ON u(w);
UPDATE u SET v = v + 10 WHERE w > 1;
SELECT * FROM u;
"#;

/// Review fixes: IN (SELECT ...) probes in the subquery's key order,
/// NOT BETWEEN / NOT IN against a partial index, a commuted `0 < b`,
/// and NULLS FIRST / LAST over a DESC index (the pin's two-pass scan).
const REVIEW: &str = r#"
CREATE TABLE t(a, b REAL);
CREATE INDEX i ON t(a);
INSERT INTO t VALUES (1, 1), (2, 2.5), ('1', NULL), ('a', 4), (3, 5), (1.5, 6);
CREATE TABLE s(x);
INSERT INTO s VALUES (3), ('a'), (1), (2), ('1'), (NULL);
SELECT rowid, a, b FROM t WHERE a IN (SELECT x FROM s);
SELECT a, b FROM t WHERE rowid IN (SELECT x FROM s);
DELETE FROM t WHERE a IN (SELECT x FROM s WHERE x = 3) RETURNING b;
CREATE TABLE p(a, b, c);
CREATE INDEX p2 ON p(b DESC) WHERE a IS NOT NULL;
CREATE INDEX p3 ON p(c) WHERE b > 0;
INSERT INTO p VALUES (1, 1, 1), (1, 2, 2), (2, NULL, 3), (NULL, 1, 4), (3, 3, 5), (2, 2, 6), (1.5, 'x', 7), ('a', x'01', 8), (NULL, NULL, 9);
SELECT c FROM p WHERE a NOT BETWEEN 1 AND 2;
SELECT c FROM p WHERE a NOT IN (SELECT 1);
SELECT c FROM p WHERE c > 2 AND 0 < b;
CREATE TABLE v(a, b, c);
CREATE INDEX v1 ON v(a DESC, b);
INSERT INTO v SELECT a, b, c FROM p;
SELECT a, b, c FROM v ORDER BY a NULLS LAST;
SELECT a, b, c FROM v ORDER BY a DESC NULLS FIRST;
"#;

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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

fn val(r: &mut Rng) -> String {
    match r.below(10) {
        0 => "NULL".into(),
        1..=5 => (r.below(13) as i64 - 3).to_string(),
        6 => format!("{}.5", r.below(13) as i64 - 3),
        _ => format!("'{}'", r.pick(&["a", "B", "b", "c", "A", "10", "2"])),
    }
}

/// Random tables, indexes (plain, expression, partial), data and
/// single-table queries, in one batch per table.
fn corpus(seed: u64, n: usize) -> String {
    let mut r = Rng(seed);
    let cols = ["a", "b", "c", "d"];
    let mut out = String::new();
    for i in 0..n {
        let mut defs = Vec::new();
        for c in cols {
            defs.push(format!(
                "{c} {}{}",
                r.pick(&["", "INTEGER", "TEXT", "REAL", "NUMERIC"]),
                r.pick(&["", "", " COLLATE NOCASE"])
            ));
        }
        out.push_str(&format!("CREATE TABLE t{i}({});\n", defs.join(", ")));
        for k in 0..1 + r.below(3) {
            let c = r.pick(&cols);
            let item = match r.below(6) {
                0 => format!("{c} + 1"),
                1 => format!("lower({c})"),
                2 => format!("{c} DESC"),
                _ => c.to_string(),
            };
            let w = if r.below(4) == 0 {
                format!(" WHERE {} IS NOT NULL", r.pick(&cols))
            } else {
                String::new()
            };
            out.push_str(&format!("CREATE INDEX t{i}i{k} ON t{i}({item}){w};\n"));
        }
        for _ in 0..12 {
            out.push_str(&format!(
                "INSERT INTO t{i} VALUES ({}, {}, {}, {});\n",
                val(&mut r),
                val(&mut r),
                val(&mut r),
                val(&mut r)
            ));
        }
        for _ in 0..4 {
            let c = r.pick(&cols);
            let w = match r.below(5) {
                0 => format!("{c} > {}", val(&mut r)),
                1 => format!("{c} = {}", val(&mut r)),
                2 => format!("{c} IN ({}, {})", val(&mut r), val(&mut r)),
                3 => format!("{c} + 1 > {}", val(&mut r)),
                _ => "1".into(),
            };
            let ob = match r.below(3) {
                0 => format!(" ORDER BY {}{}", r.pick(&cols), r.pick(&["", " DESC"])),
                _ => String::new(),
            };
            out.push_str(&format!(
                "SELECT {} FROM t{i} WHERE {w}{ob};\n",
                r.pick(&["*", "a", "b, c", "rowid, d"])
            ));
        }
        out.push_str(&format!(
            "UPDATE t{i} SET a = a + 1 WHERE b > 0;\nDELETE FROM t{i} WHERE c < 1;\nSELECT * FROM t{i};\n"
        ));
    }
    out
}

#[test]
fn cli_v16_expr_index_schema_byte_identical() {
    byte_identical("schema", SCHEMA);
}

#[test]
fn cli_v16_expr_index_order_byte_identical() {
    byte_identical("order", ORDER);
}

#[test]
fn cli_v16_expr_index_corpus_byte_identical() {
    byte_identical("corpus", &corpus(0x5eed_0016_e1d0_0001, 40));
}

#[test]
fn cli_v16_expr_index_review_byte_identical() {
    byte_identical("review", REVIEW);
}
