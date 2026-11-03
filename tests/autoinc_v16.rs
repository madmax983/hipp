//! v0.16 AUTOINCREMENT, differentially vs the pinned oracle
//! (SQLite 3.54.1, live). Probed on the pin before implementation;
//! the matrix the tests encode:
//!
//! - CREATE TABLE with `INTEGER PRIMARY KEY AUTOINCREMENT` (or
//!   `PRIMARY KEY (a [DESC] AUTOINCREMENT)`) creates
//!   `sqlite_sequence` once, as `CREATE TABLE
//!   sqlite_sequence(name,seq)`, its schema row right after the
//!   first such table's automatic indexes. AUTOINCREMENT on any
//!   other column (`INT PRIMARY KEY`, `INTEGER PRIMARY KEY DESC`)
//!   is an error.
//! - An INSERT reads the table's row (the first whose `name` is the
//!   table's name, byte for byte; `seq` as CAST AS INTEGER, NULL or
//!   no row reading 0); a new rowid is one past both the largest
//!   rowid and that counter; every rowid the statement inserts or
//!   generates raises it — an ignored row, an upsert's DO UPDATE and
//!   a replaced row included; when the statement completes, the row
//!   is written (a new row when there was none, even for an INSERT
//!   of no rows). An INSERT that fails part way (OR FAIL) writes
//!   nothing; i64::MAX handed out or present is "database or disk
//!   is full". UPDATE never touches the sequence.
//! - DROP TABLE deletes the table's rows of that name; `sqlite_*`
//!   tables may not be dropped (but `sqlite_stat*`), indexed or
//!   altered; the sequence follows ALTER TABLE RENAME.
//!
//! Without `v16` an AUTOINCREMENT table is refused at CREATE and
//! read-only (tests/table_defs_v16.rs).

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut t = hipp::arena::AVec::new();
                hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
                line.push_str(&String::from_utf8_lossy(t.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
}

/// hipp: every statement in order, rows printed, errors noted by
/// 1-based statement number and skipped past.
fn hipp_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        let mut errs = Vec::new();
        for (i, sql) in stmts.iter().enumerate() {
            let mut q = match db.prepare(sql).await {
                Ok(q) => q,
                Err(_) => {
                    errs.push(i + 1);
                    continue;
                }
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        lines.push(l);
                    }
                    Ok(Step::Done) => break,
                    Err(_) => {
                        errs.push(i + 1);
                        break;
                    }
                }
            }
        }
        (lines, errs)
    })
}

/// The pin's shell over stdin, one statement per line: stdout
/// rows, and the lines its errors name.
fn oracle_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    let mut input = String::new();
    for s in stmts {
        assert!(!s.contains('\n'), "one statement per line: {s}");
        input.push_str(s);
        input.push_str(";\n");
    }
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut errs = Vec::new();
    for l in String::from_utf8_lossy(&out.stderr).lines() {
        if let Some(i) = l.find(" near line ") {
            let rest = &l[i + 11..];
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = n.parse() {
                errs.push(n);
            }
        }
    }
    (lines, errs)
}

/// The oracle's view of a database file: integrity check and dump.
fn oracle_view(path: &std::path::Path) -> (String, String) {
    (
        common::cli(path, "PRAGMA integrity_check;"),
        common::cli(path, ".dump"),
    )
}

/// Run `stmts` on twin fresh files; rows, failing statements and
/// final files must agree.
fn same(tag: &str, stmts: &[String]) {
    let h = common::temp_db(&format!("v16ai-{tag}-h"));
    let o = common::temp_db(&format!("v16ai-{tag}-o"));
    let (hl, he) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "rows differ:\n{script}");
    assert_eq!(he, oe, "failing statements differ:\n{script}");
    let (hi, hd) = oracle_view(&h);
    let (oi, od) = oracle_view(&o);
    assert_eq!(hi.trim(), "ok", "{tag}: hipp's file\n{script}");
    assert_eq!(oi.trim(), "ok");
    assert_eq!(hd, od, "final files differ:\n{script}");
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

#[test]
fn creation_and_schema_order() {
    same(
        "create",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE, c UNIQUE)
            SELECT rowid, type, name, tbl_name, rootpage, sql FROM sqlite_schema
            CREATE TABLE u(x integer primary key autoincrement)
            CREATE TABLE v(a INTEGER, b, PRIMARY KEY (a DESC AUTOINCREMENT))
            CREATE TABLE w(a INTEGER, b, PRIMARY KEY (a AUTOINCREMENT) ON CONFLICT REPLACE)
            SELECT rowid, type, name, tbl_name, rootpage, sql FROM sqlite_schema
            SELECT * FROM sqlite_sequence
            PRAGMA table_info(sqlite_sequence)
            CREATE TABLE bad1(a INT PRIMARY KEY AUTOINCREMENT)
            CREATE TABLE bad2(a INTEGER PRIMARY KEY DESC AUTOINCREMENT)
            CREATE TABLE bad3(a INTEGER AUTOINCREMENT)
            CREATE TABLE bad4(a, b, PRIMARY KEY (a, b AUTOINCREMENT))
            CREATE TABLE bad5(a TEXT PRIMARY KEY AUTOINCREMENT)
            CREATE TABLE sqlite_sequence(name, seq)
            DROP TABLE sqlite_sequence
            CREATE INDEX i ON sqlite_sequence(name)
            ALTER TABLE sqlite_sequence ADD COLUMN x
            ALTER TABLE sqlite_sequence RENAME TO s
            DROP TABLE t
            DROP TABLE u
            DROP TABLE v
            DROP TABLE w
            SELECT rowid, type, name FROM sqlite_schema
            CREATE TABLE again(a INTEGER PRIMARY KEY AUTOINCREMENT)
            INSERT INTO again DEFAULT VALUES
            SELECT rowid, type, name FROM sqlite_schema
            SELECT * FROM sqlite_sequence",
        ),
    );
}

#[test]
fn counter_rules() {
    same(
        "rules",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE)
            INSERT INTO t VALUES (-5, 'neg')
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) VALUES ('x')
            INSERT INTO t(b) VALUES ('x')
            SELECT rowid, * FROM sqlite_sequence
            INSERT OR IGNORE INTO t(b) VALUES ('x')
            SELECT rowid, * FROM sqlite_sequence
            INSERT OR REPLACE INTO t(b) VALUES ('x')
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) VALUES ('x') ON CONFLICT(b) DO UPDATE SET b = 'y'
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) VALUES ('y') ON CONFLICT DO NOTHING
            SELECT * FROM t
            UPDATE t SET a = 100 WHERE b = 'y'
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) VALUES ('z')
            SELECT * FROM t
            SELECT rowid, * FROM sqlite_sequence
            INSERT OR IGNORE INTO t VALUES (500, 'z')
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(rowid, b) VALUES (50, 'r')
            INSERT INTO t VALUES ('70', 'text rowid')
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) SELECT 'sel' || value FROM (SELECT 1 AS value UNION ALL SELECT 2)
            SELECT * FROM t ORDER BY a
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t(b) SELECT 1 WHERE 0
            SELECT rowid, * FROM sqlite_sequence
            INSERT INTO t VALUES (NULL, 'ret') RETURNING a, b
            SELECT last_insert_rowid(), changes()
            INSERT OR FAIL INTO t(b) VALUES ('f1'), ('x'), ('f2')
            SELECT * FROM t WHERE b LIKE 'f%'
            SELECT rowid, * FROM sqlite_sequence",
        ),
    );
}

#[test]
fn sequence_rows_edited_by_hand() {
    same(
        "edited",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)
            CREATE TABLE \"T2\"(a INTEGER PRIMARY KEY AUTOINCREMENT)
            INSERT INTO t2 VALUES (NULL)
            INSERT INTO t VALUES (NULL, 1)
            SELECT rowid, * FROM sqlite_sequence
            DELETE FROM sqlite_sequence WHERE name = 't'
            INSERT INTO t VALUES (NULL, 2)
            SELECT rowid, * FROM sqlite_sequence
            UPDATE sqlite_sequence SET seq = 1000 WHERE name = 't'
            INSERT INTO t VALUES (NULL, 3)
            UPDATE sqlite_sequence SET seq = 'abc' WHERE name = 't'
            INSERT INTO t(b) SELECT 1 WHERE 0
            SELECT rowid, *, typeof(seq) FROM sqlite_sequence
            INSERT INTO t VALUES (NULL, 4)
            UPDATE sqlite_sequence SET seq = NULL WHERE name = 't'
            INSERT INTO t VALUES (NULL, 5)
            UPDATE sqlite_sequence SET seq = -50 WHERE name = 't'
            INSERT INTO t VALUES (NULL, 6)
            UPDATE sqlite_sequence SET seq = 2500.7 WHERE name = 't'
            INSERT INTO t VALUES (NULL, 7)
            UPDATE sqlite_sequence SET seq = '  3000x' WHERE name = 't'
            INSERT INTO t VALUES (NULL, 8)
            UPDATE sqlite_sequence SET seq = x'3132' WHERE name = 't'
            INSERT INTO t VALUES (NULL, 9)
            SELECT rowid, *, typeof(seq) FROM sqlite_sequence
            INSERT INTO sqlite_sequence VALUES ('t', 5000)
            INSERT INTO t VALUES (NULL, 10)
            SELECT rowid, *, typeof(seq) FROM sqlite_sequence
            UPDATE sqlite_sequence SET name = 'T' WHERE seq = 5000
            INSERT INTO t VALUES (NULL, 11)
            SELECT rowid, * FROM sqlite_sequence
            DROP TABLE t2
            SELECT rowid, * FROM sqlite_sequence
            DROP TABLE t
            SELECT rowid, * FROM sqlite_sequence
            SELECT * FROM sqlite_schema",
        ),
    );
}

#[test]
fn full_at_the_largest_rowid() {
    same(
        "full",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)
            INSERT INTO t VALUES (NULL, 1)
            UPDATE sqlite_sequence SET seq = 9223372036854775807
            INSERT INTO t(b) VALUES (2)
            INSERT INTO t VALUES (9223372036854775807, 'max')
            INSERT INTO t(b) VALUES (3)
            UPDATE sqlite_sequence SET seq = 1e30
            INSERT INTO t(b) VALUES (4)
            DELETE FROM sqlite_sequence
            INSERT INTO t(b) VALUES (5)
            DELETE FROM t WHERE a > 1
            INSERT INTO t(b) VALUES (6)
            UPDATE sqlite_sequence SET seq = 9223372036854775806
            INSERT INTO t(b) VALUES (7)
            INSERT INTO t(b) VALUES (8)
            SELECT * FROM t
            SELECT * FROM sqlite_sequence",
        ),
    );
}

/// "database or disk is full" inside a transaction rolls the whole
/// transaction back on the pin (review fix: hipp failed only the
/// statement, so a later COMMIT kept the writes before it) —
/// single and multi-row VALUES, INSERT ... SELECT, OR IGNORE / OR
/// FAIL, RETURNING, a SAVEPOINT-opened transaction, a TEMP table.
#[test]
fn full_ends_the_transaction() {
    let prep = "CREATE TABLE a(id INTEGER PRIMARY KEY AUTOINCREMENT, b)
        CREATE TABLE m(x)
        INSERT INTO a VALUES (9223372036854775806, 0)";
    for (i, body) in [
        "BEGIN; INSERT INTO m VALUES (1); INSERT INTO a(b) VALUES (1); INSERT INTO a(b) VALUES (2); COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT INTO a(b) VALUES (1), (2); COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT INTO a(b) SELECT 5 UNION ALL SELECT 6; COMMIT",
        "SAVEPOINT s; INSERT INTO m VALUES (1); INSERT INTO a(b) VALUES (1), (2); RELEASE s",
        "BEGIN; INSERT INTO m VALUES (1); SAVEPOINT q; INSERT INTO m VALUES (2); INSERT INTO a(b) VALUES (1), (2); ROLLBACK TO q; COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT OR IGNORE INTO a(b) VALUES (1), (2); COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT OR FAIL INTO a(b) VALUES (1), (2); COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT INTO a(b) VALUES (1), (2) RETURNING id; COMMIT",
        "BEGIN; INSERT INTO m VALUES (1); INSERT INTO a VALUES (9223372036854775807, 1); INSERT INTO a(b) VALUES (3); COMMIT",
        "INSERT INTO a(b) VALUES (1), (2)",
        "CREATE TEMP TABLE ta(id INTEGER PRIMARY KEY AUTOINCREMENT, b); BEGIN; INSERT INTO m VALUES (1); INSERT INTO ta VALUES (9223372036854775807, 1); INSERT INTO ta(b) VALUES (3); COMMIT; SELECT * FROM ta",
        "BEGIN; INSERT INTO m VALUES (1); CREATE TEMP TABLE ta(id INTEGER PRIMARY KEY AUTOINCREMENT, b); INSERT INTO ta VALUES (9223372036854775807, 1); INSERT INTO ta(b) VALUES (3); COMMIT; SELECT * FROM ta",
        "CREATE TEMP TABLE ta(a); BEGIN; INSERT INTO ta VALUES (5); INSERT INTO a(b) VALUES (3), (4); COMMIT; SELECT * FROM ta",
    ]
    .iter()
    .enumerate()
    {
        let mut stmts = lines(prep);
        stmts.extend(body.split("; ").map(str::to_string));
        stmts.push("SELECT * FROM m".to_string());
        stmts.push("SELECT * FROM a".to_string());
        stmts.push("SELECT * FROM sqlite_sequence".to_string());
        same(&format!("full-txn{i}"), &stmts);
    }
}

#[test]
fn transactions_and_savepoints() {
    same(
        "txn",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE)
            BEGIN
            INSERT INTO t(b) VALUES (1), (2)
            SAVEPOINT s
            INSERT INTO t(b) VALUES (3)
            SELECT * FROM sqlite_sequence
            ROLLBACK TO s
            SELECT * FROM sqlite_sequence
            INSERT INTO t(b) VALUES (2)
            SELECT * FROM sqlite_sequence
            INSERT INTO t(b) VALUES (4)
            COMMIT
            SELECT * FROM sqlite_sequence
            BEGIN
            INSERT INTO t(b) VALUES (5)
            ROLLBACK
            SELECT * FROM sqlite_sequence
            INSERT INTO t(b) VALUES (6)
            SELECT * FROM t
            SELECT * FROM sqlite_sequence",
        ),
    );
}

/// Files cross engines: the pin inserts into hipp's AUTOINCREMENT
/// tables and hipp into the pin's, each continuing the other's
/// sequence.
#[test]
fn files_interoperate() {
    let p = common::temp_db("v16ai-interop");
    let hipp = |sql: &str| {
        let (_, errs) = hipp_run(&p, &lines(sql));
        assert!(errs.is_empty(), "hipp failed {errs:?}: {sql}");
    };
    hipp(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)
        INSERT INTO t(b) VALUES ('h1'), ('h2')
        DELETE FROM t WHERE b = 'h2'",
    );
    common::cli(
        &p,
        "INSERT INTO t(b) VALUES ('o1'); CREATE TABLE u(x INTEGER PRIMARY KEY AUTOINCREMENT, y); INSERT INTO u VALUES (40, 'o');",
    );
    hipp(
        "INSERT INTO t(b) VALUES ('h3')
        INSERT INTO u(y) VALUES ('h')
        DELETE FROM u",
    );
    common::cli(&p, "INSERT INTO u(y) VALUES ('o2');");
    assert_eq!(
        common::cli(
            &p,
            "SELECT * FROM t; SELECT * FROM u; SELECT rowid, * FROM sqlite_sequence; PRAGMA integrity_check;"
        ),
        "1|h1\n3|o1\n4|h3\n42|o2\n1|t|4\n2|u|42\nok\n"
    );
}

/// A generated corpus: random interleavings of inserts (explicit,
/// generated, NULL, conflicting, OR IGNORE / REPLACE / upsert),
/// deletes, updates and edits of `sqlite_sequence` on two tables.
#[test]
fn generated_corpus() {
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
        fn pick(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    let mut rng = Rng(0x5eed_a170_c0de_0016);
    for case in 0..12 {
        let mut stmts = vec![
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE, c)".to_string(),
            "CREATE TABLE u(a INTEGER PRIMARY KEY AUTOINCREMENT, b)".to_string(),
        ];
        for _ in 0..30 {
            let tbl = if rng.pick(3) == 0 { "u" } else { "t" };
            let v = rng.pick(12);
            let s = match rng.pick(11) {
                0 => format!("INSERT INTO {tbl}(b) VALUES ({v})"),
                1 => {
                    let r = match rng.pick(40) {
                        0 => "NULL".to_string(),
                        n => (n as i64 - 5).to_string(),
                    };
                    if tbl == "t" {
                        format!("INSERT INTO t VALUES ({r}, {v}, NULL)")
                    } else {
                        format!("INSERT INTO u VALUES ({r}, {v})")
                    }
                }
                2 => format!("INSERT OR IGNORE INTO {tbl}(b) VALUES ({v})"),
                3 => format!("INSERT OR REPLACE INTO {tbl}(b) VALUES ({v})"),
                4 if tbl == "t" => {
                    format!(
                        "INSERT INTO t(b) VALUES ({v}) ON CONFLICT(b) DO UPDATE SET c = c || 'u'"
                    )
                }
                5 => format!("DELETE FROM {tbl} WHERE a % 3 = {}", rng.pick(3)),
                6 => format!("UPDATE {tbl} SET a = a + 100 WHERE a = (SELECT max(a) FROM {tbl})"),
                7 => format!(
                    "UPDATE sqlite_sequence SET seq = {} WHERE name = '{tbl}'",
                    rng.pick(60)
                ),
                8 => format!("DELETE FROM sqlite_sequence WHERE name = '{tbl}'"),
                9 => format!("INSERT INTO {tbl}(b) SELECT b + 20 FROM {tbl} WHERE a < 4"),
                _ => format!("SELECT * FROM {tbl} ORDER BY a"),
            };
            stmts.push(s);
            if rng.pick(4) == 0 {
                stmts.push("SELECT name, seq FROM sqlite_sequence ORDER BY name".to_string());
            }
        }
        stmts.push("SELECT * FROM t ORDER BY a".to_string());
        stmts.push("SELECT * FROM u ORDER BY a".to_string());
        stmts.push("SELECT rowid, * FROM sqlite_sequence".to_string());
        same(&format!("gen{case}"), &stmts);
    }
}
