//! v0.16 savepoints and statement-level rollback, differentially
//! vs the pinned oracle (SQLite 3.54.1, live). Probed on the pin
//! before implementation; the matrix the tests encode:
//!
//! - `SAVEPOINT name` outside a transaction starts one; RELEASE of
//!   that outermost savepoint (directly or through an older name)
//!   commits; inside BEGIN it only closes. `RELEASE [SAVEPOINT]
//!   name` closes the newest savepoint of that name (ASCII case
//!   ignored, duplicates allowed) and every newer one. `ROLLBACK
//!   [TRANSACTION] TO [SAVEPOINT] name` undoes everything since it
//!   opened and keeps it open. COMMIT / END / ROLLBACK end them
//!   all. Unknown names are "no such savepoint"; BEGIN inside a
//!   savepoint transaction is an error; `release savepoint;`,
//!   `savepoint 123`, `savepoint main.x` are syntax errors; names
//!   may be keywords (`begin`, `end`, `main`), strings or "".
//! - A statement that fails inside a transaction undoes only
//!   itself (also half-way through a multi-row write); the
//!   transaction and its savepoints stay open. OR FAIL keeps the
//!   rows written before the failure.
//! - ROLLBACK TO restores schema changes (CREATE / DROP / ALTER /
//!   CREATE INDEX), freelist reuse, header values (`PRAGMA
//!   user_version`), and leaves changes() / total_changes() /
//!   last_insert_rowid() alone.
//!
//! Every script runs statement by statement on both engines,
//! continuing past errors (the pin's shell reading stdin); rows
//! and the set of failing statements must agree, and the oracle
//! must read hipp's final file as it reads its own (integrity
//! check and `.dump`).

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
    let h = common::temp_db(&format!("v16sp-{tag}-h"));
    let o = common::temp_db(&format!("v16sp-{tag}-o"));
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
fn savepoint_nesting() {
    same(
        "nesting",
        &lines(
            "CREATE TABLE t(a PRIMARY KEY, b)
            SAVEPOINT a
            INSERT INTO t VALUES (1, 'x')
            SELECT 'in', count(*) FROM t
            SAVEPOINT b
            INSERT INTO t VALUES (2, 'y')
            ROLLBACK TO b
            SELECT * FROM t
            INSERT INTO t VALUES (3, 'z')
            RELEASE a
            SELECT * FROM t
            RELEASE a
            ROLLBACK TO nosuch
            BEGIN
            SAVEPOINT c
            INSERT INTO t VALUES (4, 'w')
            RELEASE c
            ROLLBACK
            SELECT * FROM t
            SAVEPOINT d
            INSERT INTO t VALUES (5, 'v')
            ROLLBACK TO d
            SELECT count(*) FROM t
            ROLLBACK TRANSACTION TO SAVEPOINT d
            ROLLBACK TRANSACTION TO d
            ROLLBACK TO SAVEPOINT d
            RELEASE SAVEPOINT d
            SELECT * FROM t
            SAVEPOINT e
            SAVEPOINT f
            INSERT INTO t VALUES (6, 'u')
            ROLLBACK TO e
            RELEASE f
            RELEASE e
            SAVEPOINT g
            INSERT INTO t VALUES (7, 'g')
            COMMIT
            RELEASE g
            SAVEPOINT h
            INSERT INTO t VALUES (8, 'h')
            ROLLBACK
            ROLLBACK TO h
            SAVEPOINT i
            END
            SELECT * FROM t
            SAVEPOINT x
            BEGIN
            RELEASE x
            BEGIN
            SAVEPOINT y
            RELEASE y
            SELECT 'still in BEGIN'
            INSERT INTO t VALUES (9, 'i')
            COMMIT
            SELECT * FROM t ORDER BY a",
        ),
    );
}

#[test]
fn savepoint_names() {
    same(
        "names",
        &lines(
            "CREATE TABLE t(a)
            SAVEPOINT \"Quoted\"
            INSERT INTO t VALUES (1)
            RELEASE quoted
            SAVEPOINT a
            SAVEPOINT b
            SAVEPOINT A
            INSERT INTO t VALUES (2)
            RELEASE a
            SELECT count(*) FROM t
            RELEASE a
            RELEASE b
            SAVEPOINT begin
            RELEASE begin
            SAVEPOINT end
            RELEASE end
            SAVEPOINT main
            RELEASE main
            SAVEPOINT 'str'
            RELEASE str
            SAVEPOINT [br]
            RELEASE \"br\"
            SAVEPOINT savepoint
            RELEASE savepoint savepoint
            SAVEPOINT \"\"
            RELEASE \"\"
            SAVEPOINT \"a b\"
            RELEASE \"A B\"
            RELEASE savepoint
            SAVEPOINT 123
            SAVEPOINT main.x
            SAVEPOINT
            RELEASE
            ROLLBACK TO
            SELECT * FROM t",
        ),
    );
}

#[test]
fn failed_statement_undoes_only_itself() {
    same(
        "stmt-abort",
        &lines(
            "CREATE TABLE t(a PRIMARY KEY, b NOT NULL, c CHECK (c IS NULL OR c > 0))
            CREATE TABLE s(x INTEGER) STRICT
            CREATE TABLE big(k INTEGER PRIMARY KEY, v)
            INSERT INTO t VALUES (1, 'one', NULL)
            BEGIN
            INSERT INTO t VALUES (2, 'two', 2)
            INSERT INTO t VALUES (3, 'three', 3), (4, NULL, 4)
            INSERT INTO t VALUES (5, 'five', -5)
            INSERT INTO t VALUES (1, 'dup', NULL)
            INSERT INTO s VALUES ('text')
            UPDATE t SET a = 9 WHERE a >= 1
            UPDATE t SET c = 1 - c WHERE a = 2
            SELECT * FROM t ORDER BY a
            INSERT OR FAIL INTO t VALUES (6, 'six', NULL), (7, 'seven', NULL), (1, 'dup', NULL), (8, 'eight', NULL)
            SELECT * FROM t ORDER BY a
            INSERT OR IGNORE INTO t VALUES (1, 'x', NULL), (10, 'ten', NULL)
            INSERT INTO big WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 400) SELECT i, printf('%0200d', i) FROM c
            INSERT INTO big WITH RECURSIVE c(i) AS (SELECT 1000 UNION ALL SELECT i + 1 FROM c WHERE i < 1400) SELECT i, zeroblob(300) FROM c UNION ALL SELECT 5, 'dup'
            SELECT count(*), sum(k) FROM big
            CREATE UNIQUE INDEX bu ON big(v)
            INSERT INTO big VALUES (2000, printf('%0200d', 7))
            CREATE UNIQUE INDEX bu2 ON big(v)
            SELECT count(*) FROM sqlite_schema
            CREATE TABLE t(z)
            COMMIT
            SELECT * FROM t ORDER BY a
            SELECT count(*), sum(k) FROM big",
        ),
    );
    same(
        "stmt-abort-in-savepoint",
        &lines(
            "CREATE TABLE t(a PRIMARY KEY)
            SAVEPOINT a
            INSERT INTO t VALUES (1)
            INSERT INTO t VALUES (1)
            SELECT 'after err'
            INSERT INTO t VALUES (2)
            RELEASE a
            SELECT * FROM t
            SAVEPOINT s
            INSERT INTO t VALUES (40)
            INSERT INTO t VALUES (41), (40)
            SELECT count(*) FROM t
            ROLLBACK TO s
            RELEASE s
            SELECT count(*) FROM t",
        ),
    );
}

#[test]
fn rollback_to_restores_schema_and_header() {
    same(
        "schema",
        &lines(
            "CREATE TABLE t(a, b)
            INSERT INTO t VALUES (1, 2), (3, 4)
            PRAGMA user_version = 3
            SAVEPOINT s
            CREATE TABLE u(x)
            INSERT INTO u VALUES (1)
            CREATE INDEX ta ON t(a)
            CREATE VIEW v AS SELECT * FROM t
            ALTER TABLE t ADD COLUMN c DEFAULT 7
            PRAGMA user_version = 9
            PRAGMA application_id = 11
            SELECT name FROM sqlite_schema ORDER BY name
            SELECT * FROM t
            ROLLBACK TO s
            SELECT name FROM sqlite_schema ORDER BY name
            SELECT * FROM t
            SELECT * FROM u
            PRAGMA user_version
            PRAGMA application_id
            DROP TABLE t
            SELECT name FROM sqlite_schema
            ROLLBACK TO s
            SELECT * FROM t
            CREATE TABLE w(y)
            RELEASE s
            SELECT name FROM sqlite_schema ORDER BY name
            PRAGMA user_version",
        ),
    );
}

#[test]
fn rollback_to_and_connection_counters() {
    same(
        "counters",
        &lines(
            "CREATE TABLE t(a PRIMARY KEY)
            INSERT INTO t VALUES (1), (2)
            SAVEPOINT d
            INSERT INTO t VALUES (10), (11)
            SELECT changes(), total_changes(), last_insert_rowid()
            ROLLBACK TO d
            SELECT changes(), total_changes(), last_insert_rowid()
            INSERT INTO t VALUES (12)
            SELECT changes(), total_changes(), last_insert_rowid()
            RELEASE d
            SELECT changes(), total_changes(), last_insert_rowid()
            SELECT rowid, a FROM t",
        ),
    );
}

/// Large savepoints: B-tree splits, overflow pages, the freelist
/// and cache spills (16-page cache) across ROLLBACK TO.
#[test]
fn rollback_to_across_splits_and_freelist() {
    same(
        "pages",
        &lines(
            "CREATE TABLE t(k INTEGER PRIMARY KEY, v)
            CREATE INDEX tv ON t(v)
            INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 300) SELECT i, printf('%0300d', i) FROM c
            BEGIN
            DELETE FROM t WHERE k % 3 = 0
            SAVEPOINT a
            INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1000 UNION ALL SELECT i + 1 FROM c WHERE i < 1300) SELECT i, zeroblob(5000) || i FROM c
            UPDATE t SET v = v || 'x' WHERE k < 100
            SELECT count(*), sum(length(v)) FROM t
            PRAGMA freelist_count
            SAVEPOINT b
            DELETE FROM t WHERE k > 50
            DROP INDEX tv
            ROLLBACK TO b
            SELECT count(*), sum(length(v)) FROM t
            ROLLBACK TO a
            SELECT count(*), sum(length(v)) FROM t
            PRAGMA freelist_count
            PRAGMA page_count
            INSERT INTO t VALUES (5000, 'five thousand')
            RELEASE a
            COMMIT
            SELECT count(*), sum(length(v)) FROM t
            PRAGMA integrity_check",
        ),
    );
}

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
}

/// Random interleavings of writes and transaction / savepoint
/// statements (including invalid ones) over a table with an index.
#[test]
fn generated_savepoint_corpus() {
    let mut rng = Rng(0x5a7e_9017_0016_0001);
    for case in 0..24 {
        let mut stmts = vec![
            "CREATE TABLE t(k INTEGER PRIMARY KEY, a, b TEXT)".to_string(),
            "CREATE UNIQUE INDEX tb ON t(b)".to_string(),
        ];
        let names = ["a", "b", "c", "A"];
        for _ in 0..40 {
            let n = names[rng.below(names.len())];
            let k = rng.below(60);
            let s = match rng.below(14) {
                0 => format!("SAVEPOINT {n}"),
                1 => format!("RELEASE {n}"),
                2 => format!("ROLLBACK TO {n}"),
                3 => "BEGIN".to_string(),
                4 => "COMMIT".to_string(),
                5 => "ROLLBACK".to_string(),
                6 | 7 => format!(
                    "INSERT INTO t VALUES ({k}, {}, 'b{}')",
                    rng.below(5),
                    rng.below(40)
                ),
                8 => format!(
                    "INSERT INTO t SELECT k + {k}, a + 1, b || 'x' FROM t WHERE k < {}",
                    rng.below(30)
                ),
                9 => format!("UPDATE t SET a = a + 1, b = b || 'u' WHERE k > {k}"),
                10 => format!("DELETE FROM t WHERE a = {}", rng.below(5)),
                11 => format!(
                    "INSERT INTO t VALUES ({k}, zeroblob({}), 'z{k}')",
                    rng.below(3000)
                ),
                12 => "SELECT count(*), sum(k), sum(length(a)) FROM t".to_string(),
                _ => "SELECT changes(), total_changes()".to_string(),
            };
            stmts.push(s);
        }
        stmts.push("SELECT k, typeof(a), length(a), b FROM t ORDER BY k".to_string());
        same(&format!("gen{case}"), &stmts);
    }
}

/// The Connection API agrees with SQL: `in_explicit_transaction`
/// follows a savepoint transaction, and `rollback` ends it.
#[test]
fn connection_api_and_savepoints() {
    let path = common::temp_db("v16sp-api");
    block_on(async {
        let mut db = open(&path).await;
        for sql in [
            "CREATE TABLE t(a)",
            "SAVEPOINT s",
            "INSERT INTO t VALUES (1)",
        ] {
            let mut q = db.prepare(sql).await.expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Done);
        }
        assert!(db.in_explicit_transaction());
        assert!(db.in_transaction());
        db.rollback().await.expect("rollback");
        assert!(!db.in_explicit_transaction());
        let e = match db.prepare("RELEASE s").await {
            Ok(mut q) => q.step().await.err(),
            Err(e) => Some(e),
        };
        assert!(
            matches!(
                e,
                Some(hipp::Error::Schema {
                    kind: hipp::SchemaKind::NoSuchSavepoint
                })
            ),
            "{e:?}"
        );
        let mut q = db.prepare("SELECT count(*) FROM t").await.expect("prep");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        assert_eq!(q.column_i64(0), Some(0));
    });
}
