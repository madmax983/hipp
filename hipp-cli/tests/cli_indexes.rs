//! v0.3 CLI surface: `.schema` prints explicit indexes (never
//! implicit autoindexes) and `.dump` replays them — byte-checked
//! against the pinned oracle shell's `.schema`.

use std::path::PathBuf;
use std::process::Command;

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if p.exists() {
        return p;
    }
    PathBuf::from("sqlite3")
}

fn hipp_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hipp"))
}

fn temp_db(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("hipp-cli-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn oracle_sql(db: &PathBuf, sql: &str) -> String {
    let out = Command::new(oracle())
        .arg(db)
        .arg(sql)
        .output()
        .expect("oracle runs");
    assert!(
        out.status.success(),
        "oracle: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn hipp_run(db: &PathBuf, input: &str) -> String {
    let out = Command::new(hipp_cli())
        .arg(db)
        .arg(input)
        .output()
        .expect("hipp cli runs");
    assert!(
        out.status.success(),
        "hipp: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn schema_with_indexes_matches_oracle() {
    let db = temp_db("schema-idx");
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER); \
                 CREATE INDEX ix_b ON t(b); CREATE UNIQUE INDEX ux_ab ON t(a, b); \
                 INSERT INTO t VALUES (1,'one',10),(2,'two',20);";
    assert_eq!(oracle_sql(&db, setup), "");
    let oracle_schema = oracle_sql(&db, ".schema");
    let hipp_schema = hipp_run(&db, ".schema");
    assert_eq!(
        hipp_schema, oracle_schema,
        "oracle .schema:\n{oracle_schema}\nhipp .schema:\n{hipp_schema}"
    );
    // Implicit index (from a's UNIQUE) appears in neither.
    assert!(!hipp_schema.contains("sqlite_autoindex"));
}

#[test]
fn dump_round_trips_indexes() {
    let db = temp_db("dump-idx");
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER); \
                 CREATE INDEX ix_b ON t(b); \
                 INSERT INTO t VALUES (1,'one',10),(2,'two',20),(3,'three',10);";
    hipp_run(&db, setup);
    let dump = hipp_run(&db, ".dump");
    assert!(dump.contains("CREATE INDEX ix_b ON t(b);"), "dump:\n{dump}");
    // Replay the dump into a fresh oracle database.
    let db2 = temp_db("dump-idx-replay");
    assert_eq!(
        oracle_sql(
            &db2,
            &dump
                .replace("BEGIN TRANSACTION;", "")
                .replace("COMMIT;", "")
        ),
        ""
    );
    assert_eq!(oracle_sql(&db2, "PRAGMA integrity_check;").trim(), "ok");
    assert_eq!(
        oracle_sql(&db2, "SELECT id,a,b FROM t ORDER BY id;"),
        "1|one|10\n2|two|20\n3|three|10\n"
    );
    assert_eq!(
        oracle_sql(&db2, "SELECT name FROM sqlite_schema WHERE type='index';"),
        "ix_b\n"
    );
}
