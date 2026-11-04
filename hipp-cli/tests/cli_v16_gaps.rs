//! hipp-cli v0.16 differentials for the honest-gap sweep: constraint
//! level `ON CONFLICT` clauses, `INDEXED BY` / `NOT INDEXED`,
//! three-part column names and statements that read or write both
//! the main and the TEMP database must render byte-identically to
//! the pinned oracle — rows, REAL rendering, the stored constraint
//! texts, and the order rows come back in under a forced
//! index. Each batch runs one-shot and over stdin, and the
//! hipp-written file must pass the oracle's integrity check.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16gap-{tag}"));
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

/// Constraint-level clauses resolving without an error.
const CONFLICTS: &str = r#"
CREATE TABLE t(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, a NOT NULL ON CONFLICT REPLACE DEFAULT 1.5, b UNIQUE ON CONFLICT IGNORE, c, UNIQUE(c) ON CONFLICT REPLACE, CHECK (a > 0) ON CONFLICT IGNORE);
INSERT INTO t VALUES (1, NULL, 'x', 10), (2, 2.25, 'y', 20), (3, 3, 'x', 30);
INSERT INTO t VALUES (1, 7, 'z', 40), (9, NULL, 'w', 20);
UPDATE t SET b = 'w' WHERE id = 1;
UPDATE t SET a = NULL WHERE id = 2;
SELECT * FROM t ORDER BY id;
SELECT changes(), total_changes();
SELECT sql FROM sqlite_schema WHERE name = 't';
CREATE TABLE u(k TEXT PRIMARY KEY ON CONFLICT IGNORE, v NOT NULL ON CONFLICT IGNORE) WITHOUT ROWID;
INSERT INTO u VALUES ('a', 1), ('a', 2), ('b', NULL), ('c', 3);
SELECT * FROM u;
CREATE TABLE log(x);
CREATE TABLE s(x);
CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO u VALUES (new.x, NULL); INSERT INTO log VALUES (new.x); END;
INSERT INTO s VALUES ('d'), ('e');
SELECT * FROM u;
SELECT * FROM log;
"#;

/// Forced and forbidden indexes: the rows come in the access path's
/// order.
const INDEXED: &str = r#"
CREATE TABLE t(a, b REAL, c TEXT);
CREATE INDEX ia ON t(a DESC);
CREATE INDEX ib ON t(b, a);
CREATE INDEX ic ON t(c) WHERE c > 'm';
INSERT INTO t VALUES (3, 1.5, 'z'), (1, 0.1, 'n'), (2, 2.0, 'a'), (5, -1e10, 'q'), (4, NULL, NULL);
SELECT * FROM t INDEXED BY ia;
SELECT * FROM t INDEXED BY ib;
SELECT * FROM t INDEXED BY ib WHERE b > 0;
SELECT * FROM t INDEXED BY ic WHERE c > 'm';
SELECT * FROM t NOT INDEXED WHERE a > 1;
SELECT * FROM t AS x INDEXED BY ia WHERE x.a < 4;
UPDATE t INDEXED BY ia SET c = c || '!' WHERE a > 2;
DELETE FROM t NOT INDEXED WHERE a = 1;
SELECT * FROM t INDEXED BY ia;
"#;

/// Three-part names everywhere a column may be named.
const THREE_PART: &str = r#"
CREATE TABLE t(a, b, CHECK (main.t.a > 0));
CREATE INDEX i ON t(b) WHERE main.t.b > 0;
INSERT INTO t VALUES (1, 2.5), (2, -1), (3, 4);
SELECT main.t.a, main.t.b FROM t WHERE main.t.a > 1 ORDER BY main.t.b;
SELECT main.x.a FROM t AS x ORDER BY 1;
SELECT main.t.a, count(*) FROM t GROUP BY main.t.a HAVING main.t.a < 3;
SELECT (SELECT max(main.t.b) FROM t AS u WHERE u.a < main.t.a) FROM t ORDER BY 1;
UPDATE t SET b = main.t.b * 2 WHERE main.t.a = 3;
SELECT * FROM t INDEXED BY i WHERE b > 0;
CREATE VIEW v AS SELECT main.t.a AS z FROM t;
SELECT * FROM v ORDER BY z;
SELECT type, name, sql FROM sqlite_schema ORDER BY name;
"#;

/// Main and TEMP in one statement.
const TEMP_MAIN: &str = r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES (1, 10, 'x'), (2, 20.5, 'y'), (3, 30, 'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES (1, 1.5, 'p'), (3, '7', 'q'), (4, NULL, 'r');
SELECT * FROM m, t WHERE m.id = t.id ORDER BY 1;
SELECT * FROM m LEFT JOIN t USING (id) ORDER BY m.id;
SELECT id, (SELECT count(*) FROM m WHERE m.id <= t.id) FROM t ORDER BY 1;
SELECT temp.t.d, main.m.b FROM t, m WHERE t.id = m.id ORDER BY 1;
INSERT INTO m(a, b) SELECT c, d FROM t;
INSERT INTO t(c, d) SELECT a, b FROM m WHERE id > 3;
CREATE TEMP TABLE u AS SELECT m.a, t.d FROM m JOIN t ON m.id = t.id;
CREATE TABLE n AS SELECT * FROM temp.t;
CREATE TEMP VIEW tv AS SELECT m.b, t.c FROM m JOIN t ON m.id = t.id;
SELECT * FROM m ORDER BY 1;
SELECT * FROM t ORDER BY 1;
SELECT * FROM u ORDER BY 1;
SELECT * FROM n ORDER BY 1;
SELECT * FROM tv ORDER BY 1;
SELECT sql FROM sqlite_temp_master ORDER BY name;
SELECT * FROM pragma_table_info('t');
SELECT * FROM pragma_table_info('u', 'temp');
"#;

#[test]
fn conflict_clauses() {
    byte_identical("conflicts", CONFLICTS);
}

#[test]
fn indexed_by() {
    byte_identical("indexed", INDEXED);
}

#[test]
fn three_part_names() {
    byte_identical("three-part", THREE_PART);
}

#[test]
fn temp_and_main() {
    byte_identical("temp-main", TEMP_MAIN);
}

/// Review fixes that answer: main views and triggers read main
/// only, `NOT INDEXED` on views and CTEs, a forced partial index
/// under a LEFT JOIN the WHERE makes inner, schema parts a CHECK
/// or partial index ignores, `main.t` beside `temp.t`, table-valued
/// functions as main's, and CREATE TABLE ... AS / temp INSERT over
/// TEMP views.
const REVIEW: &str = r#"
CREATE TABLE t(a, b REAL); INSERT INTO t VALUES (1, 0.5), (2, NULL), (3, 3.25);
CREATE VIEW v AS SELECT * FROM t;
CREATE TABLE l(x);
CREATE TRIGGER tr AFTER INSERT ON l BEGIN INSERT INTO l SELECT a + 100 FROM t WHERE new.x < 10 AND a = 1; END;
CREATE TEMP TABLE t(a, b REAL); INSERT INTO temp.t VALUES (99, 9.5), (1, 1.5);
SELECT * FROM v;
INSERT INTO l VALUES (1);
SELECT * FROM l;
SELECT * FROM v NOT INDEXED;
WITH c AS (SELECT 1.5 x) SELECT * FROM c NOT INDEXED;
CREATE INDEX main.tp ON t(a) WHERE b IS NOT NULL;
SELECT * FROM l LEFT JOIN main.t INDEXED BY tp ON main.t.a = l.x - 100 WHERE main.t.b > 0;
SELECT main.t.b, temp.t.b FROM main.t, temp.t WHERE main.t.a = temp.t.a;
SELECT * FROM main.t, temp.t WHERE main.t.a = temp.t.a;
SELECT main.p.name, main.p.type FROM pragma_table_info('l') p;
SELECT main.j.value FROM json_each('[1.5, 2]') j;
CREATE TABLE u(a CHECK (temp.u.a > 0), b, CHECK (bogus.u.b IS NOT 0));
CREATE INDEX ui ON u(a) WHERE temp.u.a > 1;
INSERT INTO u VALUES (2.5, 1), (1, 2);
SELECT * FROM u INDEXED BY ui WHERE a > 1;
CREATE TEMP VIEW tv AS SELECT a, b FROM main.t WHERE b IS NOT NULL;
CREATE TABLE n AS SELECT * FROM tv;
CREATE TEMP TABLE tn AS SELECT * FROM tv;
INSERT INTO temp.t SELECT * FROM tv;
SELECT * FROM n;
SELECT * FROM tn;
SELECT * FROM temp.t ORDER BY a;
SELECT sql FROM sqlite_schema ORDER BY name;
"#;

#[test]
fn review_fixes() {
    byte_identical("review", REVIEW);
}
