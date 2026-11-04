//! hipp-cli v0.16 differentials: column constraints and table
//! definitions must render byte-identically to the pinned oracle
//! — CHECK constraints under OR IGNORE / upsert / UPDATE ... FROM,
//! expression and signed DEFAULTs (REAL and text conversions
//! included), VIRTUAL / STORED generated columns with REAL and
//! TEXT affinities, indexes on generated columns, and under
//! `v16` STRICT tables, INSERT ... DEFAULT VALUES and CREATE
//! TABLE ... AS SELECT. Each batch runs one-shot and over stdin,
//! and the hipp-written file must pass the oracle's integrity
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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16td-{tag}"));
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

const CHECKS: &str = "\
CREATE TABLE t(a REAL CHECK(a > 0), b TEXT CHECK(length(b) < 5), CONSTRAINT both CHECK(a < 100 OR b IS NULL));
INSERT INTO t VALUES (1.5, 'x'), (2, NULL), ('7', 'abc');
INSERT OR IGNORE INTO t VALUES (-1, 'x'), (3.25, 'toolong'), (500, 'y'), (500, NULL), (0.5, 'ok');
SELECT rowid, a, typeof(a), b FROM t;
UPDATE OR IGNORE t SET a = a * 40;
SELECT rowid, a, b FROM t;
CREATE TABLE k(id PRIMARY KEY, v CHECK(v < 10));
INSERT INTO k VALUES (1, 1), (2, 2);
INSERT OR IGNORE INTO k VALUES (1, 50) ON CONFLICT(id) DO UPDATE SET v = 3;
INSERT INTO k VALUES (2, 5) ON CONFLICT(id) DO UPDATE SET v = excluded.v + 0.5;
CREATE TABLE s(x);
INSERT INTO s VALUES (5), (50), (6.5);
UPDATE k SET v = s.x FROM s;
SELECT * FROM k;
";

const DEFAULTS: &str = "\
CREATE TABLE d(a DEFAULT (1+2), b DEFAULT -1.5, c DEFAULT +7, d DEFAULT TRUE, e DEFAULT 'x',
  f DEFAULT x'41', g DEFAULT hello, h INTEGER DEFAULT '7', i DEFAULT -'3', j REAL DEFAULT (10 / 4),
  k TEXT DEFAULT (2.0 * 3), l DEFAULT (upper('q') || 'r'), m DEFAULT -'2.5', z);
INSERT INTO d(z) VALUES (1), (2);
INSERT INTO d(a, z) VALUES (NULL, 3);
SELECT * FROM d;
SELECT typeof(a), typeof(b), typeof(h), typeof(i), typeof(j), typeof(k), typeof(m) FROM d;
CREATE TABLE n(a NOT NULL DEFAULT (0.5 * 3), b);
INSERT OR REPLACE INTO n VALUES (NULL, 1);
SELECT * FROM n;
ALTER TABLE n ADD COLUMN c REAL DEFAULT -'4';
ALTER TABLE n ADD COLUMN v AS (a * 2);
SELECT * FROM n;
";

const GENERATED: &str = "\
CREATE TABLE g(a, b AS (a * 2), c REAL GENERATED ALWAYS AS (a + 0.5) STORED, d TEXT AS (a || '!'),
  e INT AS (b + c) VIRTUAL, f);
CREATE INDEX gb ON g(b);
CREATE INDEX ge ON g(e DESC, d);
INSERT INTO g(a, f) VALUES (1, 'one'), (2.5, 'two'), (NULL, 'nil'), ('3', 'str');
INSERT INTO g VALUES (4, 'four');
SELECT * FROM g;
SELECT a, b FROM g WHERE b = 5.0;
SELECT d, e FROM g ORDER BY e DESC;
UPDATE g SET a = a + 1 WHERE b > 4;
SELECT rowid, * FROM g;
DELETE FROM g WHERE e < 4;
INSERT INTO g(a) VALUES (10) RETURNING *;
SELECT g.*, b + e FROM g ORDER BY rowid;
";

const V16: &str = "\
CREATE TABLE s(a INT, b REAL, c TEXT, d BLOB, e ANY) STRICT;
INSERT INTO s VALUES (1, 2, 3, x'41', 4.5), ('5', '6.5', 7.25, x'42', '08'), (1e2, 1, 'x', NULL, NULL);
SELECT a, typeof(a), b, typeof(b), c, typeof(c), d, e, typeof(e) FROM s;
CREATE TABLE dv(a DEFAULT 5, b, c INTEGER PRIMARY KEY, d AS (a * 1.5), e DEFAULT (2 * 3));
INSERT INTO dv DEFAULT VALUES;
INSERT INTO dv DEFAULT VALUES RETURNING c, d;
SELECT * FROM dv;
CREATE TABLE src(a INTEGER, b TEXT, c REAL, id INTEGER PRIMARY KEY);
INSERT INTO src VALUES (1, 'x', 1.5, 10), (2, 'y', 2.0, 20);
CREATE TABLE ct AS SELECT a, a + 1, b || '!' AS bb, c * 2, CAST(a AS TEXT) AS t, rowid, id, a, a FROM src;
SELECT * FROM ct;
SELECT typeof(\"a + 1\"), typeof(\"c * 2\"), typeof(t), \"a:1\", \"a:2\" FROM ct;
CREATE TABLE cu AS SELECT a FROM src UNION ALL SELECT b FROM src;
SELECT * FROM cu;
";

#[test]
fn cli_v16_check_constraints_byte_identical() {
    byte_identical("checks", CHECKS);
}

#[test]
fn cli_v16_defaults_byte_identical() {
    byte_identical("defaults", DEFAULTS);
}

#[test]
fn cli_v16_generated_columns_byte_identical() {
    byte_identical("generated", GENERATED);
}

#[test]
fn cli_v16_strict_default_values_ctas_byte_identical() {
    byte_identical("v16", V16);
}

/// `.dump` of a table with generated columns replays into the
/// oracle: only the non-generated columns are dumped (the pin's
/// shell does the same), and the replayed file recomputes them.
#[test]
fn cli_v16_dump_skips_generated_columns() {
    let h = temp_db("dump-gen-h");
    let setup = "CREATE TABLE g(a, b AS (a * 2), c TEXT, d AS (a || c) STORED); \
                 INSERT INTO g(a, c) VALUES (1, 'x'), (2.5, NULL), (NULL, 'z');";
    run(&hipp_bin(), &h, setup);
    let dump = run(&hipp_bin(), &h, ".dump");
    let dump = String::from_utf8_lossy(&dump).into_owned();
    let o = temp_db("dump-gen-o");
    run(&oracle(), &o, &dump);
    let q = "SELECT rowid, * FROM g;";
    assert_eq!(run(&oracle(), &o, q), run(&hipp_bin(), &h, q));
    assert_eq!(run(&oracle(), &o, "PRAGMA integrity_check;"), b"ok\n");
}

// ---------------- review fixes (second pass) ----------------

/// Second-pass regressions, byte-compared: unary `+` drops
/// affinity (`+a = '1'`) and names a CTAS column `"+a"`; an
/// unaliased column reference takes the table's spelling in CTAS
/// and view column lists; `||` over blobs is TEXT; REAL-to-TEXT
/// conversions use the pin's `%!.17g` layout; abs() of TEXT is
/// REAL; `-9223372036854775808` is an INTEGER through DEFAULT,
/// generated columns and ALTER; `main.` qualifiers stay out of the
/// stored schema text; a repeated PRIMARY KEY / UNIQUE set gets no
/// second autoindex; ALTER TABLE ADD COLUMN takes non-constant
/// defaults and STORED columns on an empty table.
const REVIEW: &str = "\
CREATE TABLE main.s(a INT, b TEXT, c TEXT COLLATE NOCASE, id INTEGER PRIMARY KEY);
INSERT INTO s VALUES (1, '1', 'X', 7), (2, '2.0', 'y', 8);
SELECT count(*) FROM s WHERE +a = '1';
SELECT count(*) FROM s WHERE a = '1';
SELECT +b = 1, b = 1, +c = 'x', '2' = +a FROM s;
CREATE TABLE ct AS SELECT +a, +b, (a), ((b)), A, S.B, ID, rowid, a AS A, c COLLATE BINARY FROM s;
SELECT typeof(\"+a\"), typeof(a) FROM ct;
CREATE VIEW v AS SELECT A, +b, S.B, a AS Q FROM s;
CREATE TABLE r(a, c, d AS (c || a) STORED, e DEFAULT (x'41' || x'42'), f TEXT, g AS (a || '') STORED);
INSERT INTO r(a, c, f) VALUES (3.5, x'01', 1e20), (1e-5, x'02', 1e16), (1e17, 'z', 0.0001);
SELECT typeof(d), hex(d), typeof(e), e, f, g FROM r;
SELECT 1e20 || '', 1e15 || '', 1.5e-7 || '', CAST(-2.5e30 AS TEXT), 1e100 || '';
SELECT abs('5'), typeof(abs('-2.5')), abs(x'35'), abs(-3), abs(-9223372036854775807);
CREATE TABLE m(a, b DEFAULT -9223372036854775808, c AS (-9223372036854775808),
  d CHECK(d IS NULL OR d = -9223372036854775808));
INSERT INTO m(a, d) VALUES (1, -9223372036854775808);
ALTER TABLE m ADD COLUMN e DEFAULT -9223372036854775808;
SELECT b, typeof(b), c, typeof(c), d, typeof(d), e, typeof(e), -(9223372036854775808) FROM m;
CREATE TABLE u(a PRIMARY KEY UNIQUE, b UNIQUE, UNIQUE(b), UNIQUE(a, b), UNIQUE(a) );
CREATE TABLE w(a, b, UNIQUE(a) UNIQUE(b));
CREATE UNIQUE INDEX main.wi ON w(a, b);
CREATE TABLE e(a);
ALTER TABLE e ADD COLUMN b DEFAULT (1 + 2);
ALTER TABLE e ADD COLUMN c NOT NULL;
ALTER TABLE e ADD COLUMN d AS (a * 2) STORED;
INSERT INTO e(a, c) VALUES (5, 'c');
SELECT * FROM e;
";

#[test]
fn cli_v16_review_fixes_byte_identical() {
    byte_identical("review", REVIEW);
    // The schema each engine wrote, read back by the oracle: the
    // CTAS text, the qualifier-free CREATE texts, the deduplicated
    // autoindexes; and the view's column list in `.schema`.
    let a = temp_db("review2-hipp");
    let b = temp_db("review2-ora");
    run(&hipp_bin(), &a, REVIEW);
    run(&oracle(), &b, REVIEW);
    let q = "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY name;";
    assert_eq!(run(&oracle(), &a, q), run(&oracle(), &b, q));
    assert_eq!(
        run(&hipp_bin(), &a, ".schema v"),
        run(&oracle(), &b, ".schema v")
    );
}

/// The CLI supplies the system clock: a CURRENT_* default no
/// longer fails with `NoClock`, and one statement's defaults
/// share one instant.
#[test]
fn cli_v16_clock_defaults_use_the_system_time() {
    let batch = "CREATE TABLE t(a, b DEFAULT CURRENT_TIMESTAMP, c DEFAULT CURRENT_DATE, \
                 d DEFAULT CURRENT_TIME); INSERT INTO t(a) VALUES (1), (2); \
                 SELECT length(b), substr(b, 1, 10) = c, substr(b, 12) = d, \
                 count(DISTINCT b) FROM t;";
    let a = temp_db("clock-hipp");
    let b = temp_db("clock-ora");
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(got, b"19|1|1|1\n");
    assert_eq!(got, run(&oracle(), &b, batch));
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}
