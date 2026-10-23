//! hipp-cli v0.4 differentials vs the pinned oracle CLI: joins,
//! aggregates, expressions, ALTER TABLE ADD COLUMN, and the
//! view-aware dot-commands (.schema with the `/* v(cols) */`
//! comment, .tables, .dump replaying views).

use std::path::PathBuf;
use std::process::{Command, Output};

fn hipp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_hipp")
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin).args(args).output().expect("binary runs")
}

fn oracle_run(db: &PathBuf, sql: &str) -> Output {
    run(oracle().to_str().unwrap(), &[db.to_str().unwrap(), sql])
}

fn run_stdin(bin: &str, db: &str, script: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    child.wait_with_output().expect("wait")
}

/// One-shot output for a v0.4 query corpus is byte-identical.
const CORPUS: &str = "\
CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT, k INTEGER);
CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT, k INTEGER);
INSERT INTO a VALUES (1, 'a1', 10), (2, 'a2', 20), (3, 'a3', 30);
INSERT INTO b VALUES (1, 'b1', 10), (2, 'b2', 99);
SELECT a.x, b.y FROM a JOIN b ON a.k = b.k ORDER BY a.id;
SELECT a.x, b.y FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id;
SELECT k, count(*), sum(id) FROM a GROUP BY k ORDER BY k;
SELECT count(*), sum(k), avg(k), min(x), max(x) FROM a;
SELECT x || '-' || k, abs(k - 25), upper(x) FROM a ORDER BY id;
SELECT id FROM a WHERE k > 15 AND x LIKE 'a%' ORDER BY id DESC LIMIT 2;
ALTER TABLE a ADD COLUMN w REAL DEFAULT 0.5;
SELECT id, w FROM a ORDER BY id;
INSERT INTO a VALUES (4, 'a4', 40, 2.5);
SELECT id, w FROM a WHERE w > 1;
";

#[test]
fn v04_oneshot_byte_identical_to_oracle() {
    let hdb = temp_db("v04-oneshot-h");
    let odb = temp_db("v04-oneshot-o");
    let hout = run(hipp_bin(), &[hdb.to_str().unwrap(), CORPUS]);
    let oout = oracle_run(&odb, CORPUS);
    assert!(hout.status.success(), "hipp: {}", String::from_utf8_lossy(&hout.stderr));
    assert!(oout.status.success(), "oracle: {}", String::from_utf8_lossy(&oout.stderr));
    assert_eq!(
        String::from_utf8_lossy(&hout.stdout),
        String::from_utf8_lossy(&oout.stdout),
        "stdout must be byte-identical"
    );
}

/// .schema with a view (incl. the oracle's column-comment line),
/// .tables listing the view, and .dump replaying the view into a
/// fresh oracle database.
#[test]
fn schema_tables_dump_with_views() {
    let script = "\
CREATE TABLE t (a INTEGER, b TEXT);
INSERT INTO t VALUES (1, 'x'), (2, 'y');
CREATE INDEX ix ON t(a);
CREATE VIEW v AS SELECT a, b FROM t WHERE a > 0;
CREATE VIEW w(n) AS SELECT count(*) FROM t;
.schema
";
    let hdb = temp_db("v04-schema-h");
    let hout = run_stdin(hipp_bin(), hdb.to_str().unwrap(), script);
    assert!(hout.status.success(), "hipp: {}", String::from_utf8_lossy(&hout.stderr));
    let odb = temp_db("v04-schema-o");
    let oout = run_stdin(oracle().to_str().unwrap(), odb.to_str().unwrap(), script);
    assert!(oout.status.success(), "oracle: {}", String::from_utf8_lossy(&oout.stderr));
    assert_eq!(
        String::from_utf8_lossy(&hout.stdout),
        String::from_utf8_lossy(&oout.stdout),
        ".schema must be byte-identical (view comment included)"
    );

    // .tables: hipp prints one name per line (documented v0.2
    // divergence from the oracle's grid) — compare as a set.
    let hout = run(hipp_bin(), &[hdb.to_str().unwrap(), ".tables"]);
    let names: Vec<&str> = std::str::from_utf8(&hout.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(names, vec!["t", "v", "w"]);

    // .dump from hipp, replayed into the oracle: the view comes
    // back with identical schema text.
    let hout = run(hipp_bin(), &[hdb.to_str().unwrap(), ".dump"]);
    assert!(hout.status.success());
    let dump = String::from_utf8_lossy(&hout.stdout).to_string();
    let rdb = temp_db("v04-dump-replay");
    let rout = oracle_run(&rdb, &dump);
    assert!(
        rout.status.success(),
        "oracle replay: {}",
        String::from_utf8_lossy(&rout.stderr)
    );
    let check = oracle_run(
        &rdb,
        "SELECT type, name, sql FROM sqlite_schema ORDER BY rowid;",
    );
    let orig = oracle_run(
        &odb,
        "SELECT type, name, sql FROM sqlite_schema ORDER BY rowid;",
    );
    assert_eq!(
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&orig.stdout),
        "replayed schema must match"
    );
}
