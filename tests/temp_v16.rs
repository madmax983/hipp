//! v0.16 TEMP tables, views and indexes, differentially vs the
//! pinned oracle (SQLite 3.54.1, live). Probed on the pin before
//! implementation; the matrix the tests encode:
//!
//! - `CREATE TEMP | TEMPORARY TABLE / VIEW`, `CREATE TABLE temp.t`
//!   and every `temp.`-qualified name use the connection's temp
//!   database; an unqualified name means the temp object of that
//!   name first, then main's (DROP TABLE drops the temp one);
//!   `sqlite_temp_schema` / `sqlite_temp_master` / `temp.sqlite_master`
//!   list it, with the pin's text and root pages (2, 3, ...) — no
//!   TEMP keyword, no qualifier — and its own `sqlite_sequence`;
//!   `PRAGMA temp.x`, `PRAGMA table_info(temp_table)`, `table_list`
//!   and `database_list` (`1|temp|` once it exists) show it.
//! - CREATE INDEX follows its table (`temp.i ON main_table` is an
//!   error); a view without TEMP is a main view even over temp
//!   tables; `CREATE TEMP TABLE main.t` is an error.
//! - The temp schema commits and rolls back with the main one:
//!   BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO,
//!   also for a temp database first used inside the transaction.
//! - Nothing of it reaches the database file, and it is gone when
//!   the connection closes.
//!
//! hipp's temp database is a second, private in-memory connection:
//! a statement that names objects of both databases is refused
//! (`Unsupported`, `refused_when_both_databases_meet`).

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};

/// One case line: statements separated by `; `, then the main schema.
fn split(c: &str) -> Vec<String> {
    let mut v: Vec<String> = c
        .split("; ")
        .map(|s| s.trim_end_matches(';').to_string())
        .collect();
    v.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
    v
}

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
    let h = common::temp_db(&format!("v16tm-{tag}-h"));
    let o = common::temp_db(&format!("v16tm-{tag}-o"));
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
fn temp_schema_matches_the_pin() {
    for (i, c) in [
        "CREATE TEMP TABLE t(a); INSERT INTO t VALUES(1); SELECT * FROM t; SELECT * FROM temp.t; SELECT type,name,tbl_name,rootpage,sql FROM sqlite_temp_schema; SELECT * FROM temp.sqlite_master; SELECT count(*) FROM sqlite_schema",
        "CREATE TEMP TABLE t(a); SELECT * FROM main.t",
        "CREATE TEMPORARY TABLE t(a UNIQUE, b); CREATE INDEX i ON t(b); CREATE TEMP VIEW v AS SELECT * FROM t; SELECT type,name,tbl_name,rootpage,sql FROM sqlite_temp_master",
        "CREATE TABLE t(a); INSERT INTO t VALUES('main'); CREATE TEMP TABLE t(a); INSERT INTO t VALUES('temp'); SELECT * FROM t; SELECT * FROM main.t; SELECT * FROM temp.t",
        "CREATE TABLE temp.t(a); SELECT sql FROM sqlite_temp_schema",
        "CREATE TABLE main.t(a); CREATE TEMP TABLE main.u(a)",
        "CREATE TEMP TABLE temp.u(a); SELECT sql FROM temp.sqlite_schema",
        "CREATE TABLE m(a); CREATE VIEW v AS SELECT * FROM temp.x",
        "CREATE TEMP TABLE x(a); CREATE VIEW v AS SELECT * FROM x; SELECT type, name FROM sqlite_temp_schema; SELECT type,name FROM sqlite_schema",
        "CREATE TEMP TABLE x(a); CREATE TABLE m(a); CREATE INDEX i ON x(a); CREATE INDEX temp.j ON x(a); CREATE INDEX main.k ON x(a); SELECT name FROM sqlite_temp_schema",
        "CREATE TABLE m(a); CREATE INDEX temp.j ON m(a)",
        "CREATE TEMP TABLE x(a); DROP TABLE x; SELECT count(*) FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO x VALUES(NULL); SELECT * FROM temp.sqlite_sequence; SELECT name FROM temp.sqlite_schema",
        "CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); BEGIN; INSERT INTO x VALUES(2); ROLLBACK; SELECT * FROM x",
        "BEGIN; CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); ROLLBACK; SELECT * FROM x",
        "CREATE TEMP TABLE x(a); SAVEPOINT s; INSERT INTO x VALUES(1); ROLLBACK TO s; RELEASE s; SELECT count(*) FROM x",
        "SAVEPOINT s; CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); ROLLBACK TO s; RELEASE s; SELECT count(*) FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); SELECT last_insert_rowid(), changes(), total_changes()",
        "CREATE TABLE m(a); INSERT INTO m VALUES(1),(2),(3); CREATE TEMP TABLE x(a); INSERT INTO x VALUES(9); SELECT last_insert_rowid(), changes(), total_changes(); SELECT a FROM x WHERE a > last_insert_rowid()",
        "CREATE TEMP TABLE x(a); CREATE TEMP TABLE x(b)",
        "CREATE TABLE x(a); CREATE TEMP TABLE x(b); DROP TABLE x; SELECT name FROM sqlite_schema; SELECT name FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); CREATE INDEX x ON x(a)",
        "CREATE TABLE x(a); CREATE TEMP VIEW x AS SELECT 1; SELECT * FROM x",
        "CREATE TEMP TABLE x(a); PRAGMA temp.table_info(x); PRAGMA table_info(x)",
        "CREATE TEMP TABLE x(a); ALTER TABLE x RENAME TO y; SELECT name, sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); PRAGMA temp.page_count; PRAGMA temp.schema_version; PRAGMA temp.page_size",
        "CREATE TEMP TABLE IF NOT EXISTS x(a); CREATE TEMP TABLE IF NOT EXISTS x(a); SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE sqlite_x(a)",
        "CREATE TEMP TABLE x(a, b); INSERT INTO x VALUES(1,2),(3,4); UPDATE x SET b = b * 10 WHERE a > 1; DELETE FROM x WHERE a = 1; SELECT * FROM x; SELECT changes()",
        "CREATE TEMP TABLE x(a); WITH c AS (SELECT 5) INSERT INTO x SELECT * FROM c; SELECT * FROM x",
        "CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1) RETURNING a, rowid",
        "CREATE TEMP TABLE x(a); CREATE TEMP TABLE y(b); INSERT INTO x VALUES(1); INSERT INTO y VALUES(1); SELECT * FROM x JOIN y ON a = b; SELECT (SELECT count(*) FROM y) FROM x",
        "CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); DROP TABLE temp.x; SELECT * FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); CREATE INDEX ix ON x(a); DROP INDEX ix; SELECT name FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); CREATE TEMP VIEW v AS SELECT a * 2 AS d FROM x; INSERT INTO x VALUES(4); SELECT * FROM v; DROP VIEW v; SELECT name FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); ALTER TABLE x ADD COLUMN b DEFAULT 3; INSERT INTO x(a) VALUES(1); SELECT * FROM x; ALTER TABLE x DROP COLUMN a; SELECT * FROM x; SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a); INSERT INTO x VALUES(1); SELECT * FROM x; SELECT name FROM sqlite_schema",
        "CREATE TEMP TABLE x(a); SELECT seq, name FROM pragma_database_list; PRAGMA table_list",
        "SELECT seq, name FROM pragma_database_list; SELECT * FROM sqlite_temp_master; SELECT seq, name FROM pragma_database_list",
        "CREATE TABLE m(a, b); CREATE TEMP TABLE m(c); INSERT INTO m VALUES(1); SELECT * FROM m; SELECT * FROM main.m; PRAGMA table_info(m); PRAGMA main.table_info(m); DROP TABLE m; SELECT * FROM m; PRAGMA table_info(m)",
        "CREATE TEMP TABLE x(a TEXT, b INTEGER PRIMARY KEY, c REAL UNIQUE COLLATE NOCASE, CHECK (a <> 'bad')); INSERT INTO x VALUES ('1', NULL, 2), ('bad', NULL, 3); INSERT INTO x VALUES (5, NULL, '2'); INSERT OR REPLACE INTO x VALUES ('r', 1, 9); SELECT *, typeof(a), typeof(c) FROM x",
        "CREATE TEMP TABLE x(a); CREATE TEMP TABLE y(a); INSERT INTO x VALUES (1), (2); INSERT INTO y SELECT a * 10 FROM x; WITH q AS (SELECT a FROM y) SELECT * FROM q UNION SELECT a FROM x ORDER BY 1",
        "CREATE TEMP VIEW tv AS SELECT 1 AS one; SELECT * FROM tv; SELECT type, name FROM sqlite_temp_schema; CREATE VIEW tv AS SELECT 2; SELECT * FROM tv",
        "CREATE TEMP TABLE x(a); BEGIN; INSERT INTO x VALUES (1); SAVEPOINT a; INSERT INTO x VALUES (2); RELEASE a; COMMIT; SELECT * FROM x; BEGIN; DELETE FROM x; END; SELECT count(*) FROM x",
        "CREATE TABLE m(a); BEGIN; INSERT INTO m VALUES (1); SAVEPOINT s; CREATE TEMP TABLE x(a); INSERT INTO x VALUES (1); ROLLBACK TO s; SELECT count(*) FROM sqlite_temp_schema; COMMIT; SELECT * FROM m",
        "SAVEPOINT outer; CREATE TEMP TABLE x(a); INSERT INTO x VALUES (7); RELEASE outer; SELECT * FROM x; ROLLBACK",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("c{i}"), &split(c));
    }
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

/// The file is never written for the temp database, and the temp
/// database is gone when the connection closes.
#[test]
fn never_in_the_file_and_gone_on_close() {
    let p = common::temp_db("v16tm-file");
    common::cli(&p, "CREATE TABLE m(a); INSERT INTO m VALUES (1);");
    let before = std::fs::read(&p).expect("read");
    block_on(async {
        let mut db = open(&p).await;
        for sql in [
            "CREATE TEMP TABLE x(a PRIMARY KEY, b)",
            "CREATE INDEX temp.xi ON x(b)",
            "CREATE TEMP VIEW tv AS SELECT a FROM x",
            "INSERT INTO x VALUES (1, 2), (3, 4)",
            "BEGIN",
            "UPDATE x SET b = b + 1",
            "DELETE FROM x WHERE a = 1",
            "COMMIT",
            "ALTER TABLE x ADD COLUMN c",
            "CREATE TEMP TABLE y(id INTEGER PRIMARY KEY AUTOINCREMENT)",
            "INSERT INTO y DEFAULT VALUES",
        ] {
            let mut q = db.prepare(sql).await.expect(sql);
            while q.step().await.expect(sql) == Step::Row {}
        }
        let t = db.temp_db().expect("temp database");
        let names: Vec<&[u8]> = t.tables().iter().map(|m| m.name).collect();
        assert_eq!(names, [&b"x"[..], b"y", b"sqlite_sequence"]);
        assert_eq!(db.last_insert_rowid(), 1);
        assert_eq!(db.tables().len(), 1, "main still has m only");
    });
    assert_eq!(std::fs::read(&p).expect("read"), before, "file untouched");
    let mut journal = p.clone().into_os_string();
    journal.push("-journal");
    assert!(!std::path::Path::new(&journal).exists());
    block_on(async {
        let mut db = open(&p).await;
        assert!(db.temp_db().is_none());
        let r = db.prepare("SELECT * FROM x").await;
        assert!(r.is_err(), "the temp table went with its connection");
        let mut q = db
            .prepare("SELECT count(*) FROM sqlite_temp_schema")
            .await
            .expect("prep");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        assert_eq!(q.column_i64(0), Some(0));
    });
}

/// A statement naming objects of both databases runs on the pin;
/// hipp refuses it rather than reading the wrong one.
#[test]
fn refused_when_both_databases_meet() {
    let p = common::temp_db("v16tm-mixed");
    common::cli(&p, "CREATE TABLE m(a); INSERT INTO m VALUES (1);");
    block_on(async {
        let mut db = open(&p).await;
        let mut q = db.prepare("CREATE TEMP TABLE x(a)").await.expect("prep");
        q.step().await.expect("create");
        drop(q);
        for sql in [
            "SELECT * FROM m, x",
            "INSERT INTO x SELECT * FROM m",
            "INSERT INTO m SELECT * FROM x",
            "SELECT (SELECT count(*) FROM x) FROM m",
            "CREATE TEMP TABLE y AS SELECT * FROM m",
            "CREATE INDEX temp.mi ON m(a)",
            "SELECT * FROM main.m JOIN temp.x",
        ] {
            let r = db.prepare(sql).await.map(|_| ());
            assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
        }
    });
}

/// The pin's ALTER TABLE of a main table also rewrites the TEMP
/// views that read it; hipp refuses such an ALTER (so a view never
/// silently reads a different table later), and runs it once no
/// TEMP view reads the table.
#[test]
fn main_alter_under_temp_views_refused() {
    let p = common::temp_db("v16tm-alter-views");
    common::cli(
        &p,
        "CREATE TABLE m(q); CREATE TABLE n(r); INSERT INTO m VALUES (1);",
    );
    block_on(async {
        let mut db = open(&p).await;
        for sql in [
            "CREATE TEMP VIEW mv AS SELECT q FROM main.m",
            "CREATE TEMP VIEW mv2 AS SELECT * FROM mv",
        ] {
            let mut q = db.prepare(sql).await.expect(sql);
            q.step().await.expect(sql);
        }
        for sql in [
            "ALTER TABLE m RENAME TO m2",
            "ALTER TABLE m RENAME COLUMN q TO z",
            "ALTER TABLE m ADD COLUMN y",
        ] {
            let r = match db.prepare(sql).await {
                Ok(mut q) => q.step().await.map(|_| ()),
                Err(e) => Err(e),
            };
            if sql.contains("ADD") {
                r.expect("ADD COLUMN rewrites no view");
            } else {
                assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
            }
        }
        for sql in [
            "ALTER TABLE n RENAME TO n2",
            "DROP VIEW mv2",
            "DROP VIEW mv",
            "ALTER TABLE m RENAME TO m2",
        ] {
            let mut q = db.prepare(sql).await.expect(sql);
            q.step().await.expect(sql);
        }
    });
    assert_eq!(common::cli(&p, "SELECT * FROM m2;"), "1|\n");
}

/// Bound parameters reach a statement on the temp database.
#[cfg(feature = "params")]
#[test]
fn parameters_on_the_temp_database() {
    let p = common::temp_db("v16tm-params");
    block_on(async {
        let mut db = open(&p).await;
        for sql in ["CREATE TEMP TABLE x(a, b)"] {
            let mut q = db.prepare(sql).await.expect(sql);
            q.step().await.expect(sql);
        }
        let mut q = db
            .prepare("INSERT INTO x VALUES (?1, :b)")
            .await
            .expect("prep");
        assert_eq!(q.parameter_count(), 2);
        for i in 0..3 {
            q.bind_i64(1, i).expect("bind");
            q.bind_text(2, "t").expect("bind");
            q.step().await.expect("step");
            q.reset();
        }
        drop(q);
        let mut q = db
            .prepare("SELECT sum(a), group_concat(b, '') FROM x WHERE a >= ?")
            .await
            .expect("prep");
        q.bind_i64(1, 1).expect("bind");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        assert_eq!(q.column_i64(0), Some(3));
        assert_eq!(q.column_bytes(1), Some(&b"tt"[..]));
    });
}

/// The pin's corner cases around the temp schema, each probed on
/// 3.54.1: RENAME COLUMN of a TEMP table edits its TEMP views twice
/// (the second pass reads the edited text against the old columns,
/// so it fails unless every edited name is a bare quoted `"new"`,
/// which then reads as a string); REINDEX / ANALYZE of temp objects
/// (`ANALYZE temp` makes `temp.sqlite_stat1`, which then hides
/// main's); `main.sqlite_temp_schema` is no table.
#[test]
fn pin_corner_cases() {
    for (i, script) in [
        "CREATE TEMP TABLE x(a, b);
         CREATE TEMP VIEW v AS SELECT a FROM x;
         ALTER TABLE x RENAME COLUMN a TO z;
         SELECT sql FROM sqlite_temp_schema;
         ALTER TABLE x RENAME COLUMN b TO c;
         SELECT sql FROM sqlite_temp_schema;
         ALTER TABLE x RENAME COLUMN a TO A;
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a, b);
         CREATE TEMP VIEW v AS SELECT x.a FROM x;
         ALTER TABLE x RENAME COLUMN a TO z;
         ALTER TABLE x RENAME COLUMN a TO \"z\";
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a, b);
         CREATE TEMP VIEW v AS SELECT b FROM x WHERE a > 0 GROUP BY a ORDER BY a;
         ALTER TABLE x RENAME COLUMN a TO c;
         ALTER TABLE x RENAME COLUMN a TO \"c\";
         SELECT sql FROM sqlite_temp_schema;
         ALTER TABLE x RENAME COLUMN c TO 'd';
         ALTER TABLE x RENAME COLUMN d TO [e];
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a, b);
         CREATE TEMP VIEW v AS SELECT * FROM x;
         CREATE INDEX xi ON x(a);
         ALTER TABLE x RENAME COLUMN a TO z;
         ALTER TABLE x RENAME TO y;
         ALTER TABLE y DROP COLUMN b;
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TEMP TABLE x(a);
         CREATE INDEX i ON x(a);
         INSERT INTO x VALUES (1), (1), (2);
         REINDEX x;
         REINDEX temp.x;
         REINDEX i;
         REINDEX temp.i;
         REINDEX temp.nope;
         REINDEX temp;
         ANALYZE x;
         SELECT * FROM sqlite_stat1;
         SELECT name FROM sqlite_temp_schema",
        "CREATE TABLE m(a);
         CREATE INDEX mi ON m(a);
         INSERT INTO m VALUES (1);
         ANALYZE;
         CREATE TEMP TABLE x(a);
         INSERT INTO x VALUES (1), (2);
         CREATE INDEX i ON x(a);
         ANALYZE temp;
         SELECT * FROM sqlite_stat1;
         SELECT * FROM main.sqlite_stat1;
         SELECT * FROM temp.sqlite_stat1;
         ANALYZE temp.nope;
         ANALYZE temp.i;
         SELECT * FROM sqlite_stat1",
        "ANALYZE temp;
         SELECT name FROM sqlite_temp_schema;
         SELECT seq, name FROM pragma_database_list",
        "CREATE TEMP TABLE x(a);
         CREATE INDEX i ON x(a);
         INSERT INTO x VALUES (1);
         ANALYZE;
         SELECT * FROM temp.sqlite_stat1",
        "SELECT * FROM main.sqlite_temp_schema;
         SELECT * FROM main.sqlite_temp_master;
         SELECT count(*) FROM temp.sqlite_temp_schema;
         CREATE TEMP TABLE x(a);
         SELECT * FROM main.x;
         SELECT * FROM main.sqlite_temp_schema",
    ]
    .iter()
    .enumerate()
    {
        let mut stmts = lines(script);
        stmts.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
        same(&format!("q{i}"), &stmts);
    }
}

/// Review fixes, each compared with the pin: the connection-wide
/// pragmas (foreign_keys, case_sensitive_like,
/// ignore_check_constraints, recursive_triggers, busy_timeout) hold
/// on TEMP tables too and `PRAGMA temp.x` of one sets the
/// connection's; a quoted `sqlite_temp_schema` is the temp schema
/// table; `CREATE TEMP [UNIQUE] INDEX` is a syntax error; `temp.a`
/// naming a column of a table or alias called `temp` is no schema
/// qualifier.
#[test]
fn review_fixes() {
    for (i, script) in [
        "PRAGMA foreign_keys = ON;
         CREATE TEMP TABLE p(id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE c(x REFERENCES p(id));
         INSERT INTO c VALUES (5);
         SELECT * FROM c",
        "CREATE TEMP TABLE p(id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE c(x REFERENCES p(id));
         PRAGMA foreign_keys = ON;
         INSERT INTO c VALUES (5);
         PRAGMA foreign_keys = OFF;
         INSERT INTO c VALUES (6);
         SELECT * FROM c",
        "CREATE TEMP TABLE p(id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE c(x REFERENCES p(id));
         PRAGMA temp.foreign_keys = ON;
         PRAGMA foreign_keys;
         PRAGMA main.foreign_keys;
         PRAGMA temp.foreign_keys;
         INSERT INTO c VALUES (5);
         SELECT * FROM c",
        "PRAGMA case_sensitive_like = ON;
         CREATE TEMP TABLE t(a);
         INSERT INTO t VALUES ('ABC');
         SELECT count(*) FROM t WHERE a LIKE 'abc';
         PRAGMA case_sensitive_like = OFF;
         SELECT count(*) FROM t WHERE a LIKE 'abc';
         PRAGMA temp.case_sensitive_like = 1;
         SELECT count(*) FROM t WHERE a LIKE 'abc';
         SELECT 'a' LIKE 'A'",
        "CREATE TEMP TABLE k(v CHECK (v > 0));
         PRAGMA ignore_check_constraints = ON;
         INSERT INTO k VALUES (-1);
         PRAGMA ignore_check_constraints = OFF;
         INSERT INTO k VALUES (-2);
         PRAGMA temp.ignore_check_constraints = 1;
         PRAGMA ignore_check_constraints;
         INSERT INTO k VALUES (-3);
         SELECT * FROM k",
        "PRAGMA temp.recursive_triggers = 1;
         PRAGMA recursive_triggers;
         PRAGMA temp.busy_timeout = 5;
         PRAGMA busy_timeout;
         PRAGMA temp.busy_timeout",
        "CREATE TEMP TABLE t(a);
         SELECT count(*) FROM \"sqlite_temp_schema\";
         SELECT count(*) FROM [sqlite_temp_master];
         SELECT count(*) FROM `sqlite_temp_schema`;
         SELECT count(*) FROM \"SQLITE_TEMP_SCHEMA\";
         SELECT count(*) FROM temp.\"sqlite_temp_master\";
         SELECT name FROM \"sqlite_temp_schema\" AS s WHERE s.type = 'table';
         SELECT * FROM t WHERE a IN (SELECT 3 FROM \"sqlite_temp_master\");
         SELECT count(*) FROM main.\"sqlite_temp_schema\"",
        "CREATE TEMP TABLE t(a);
         CREATE TEMP INDEX i ON t(a);
         CREATE TEMP UNIQUE INDEX j ON t(a);
         CREATE TEMPORARY INDEX k ON t(a);
         SELECT name FROM sqlite_temp_schema;
         DROP INDEX i",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (7);
         SELECT temp.a FROM t AS temp;
         SELECT temp.a FROM t temp WHERE temp.a > 0;
         CREATE TABLE temp(a);
         INSERT INTO temp VALUES (1);
         SELECT temp.a FROM temp;
         SELECT temp.a FROM temp WHERE temp.a = 1 ORDER BY temp.a",
        "CREATE TEMP TABLE tt(a);
         INSERT INTO tt VALUES (3);
         SELECT temp.a FROM temp.tt;
         SELECT temp.a FROM tt AS temp;
         SELECT temp.a FROM temp.tt AS temp;
         UPDATE tt SET a = 4 WHERE a = 3;
         SELECT temp.a FROM temp.tt temp",
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (7);
         SELECT temp.* FROM t AS temp;
         CREATE TABLE x(a);
         INSERT INTO x VALUES (5);
         CREATE TEMP TABLE x(b);
         INSERT INTO x VALUES (1);
         SELECT temp.* FROM x AS temp;
         SELECT 1 IN temp.x;
         SELECT 5 IN main.x;
         SELECT 5 IN x;
         SELECT 1 IN x;
         SELECT x FROM (SELECT 1 AS x) WHERE 1 IN temp.x",
    ]
    .iter()
    .enumerate()
    {
        let mut stmts = lines(script);
        stmts.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
        same(&format!("r{i}"), &stmts);
    }
}

/// A TEMP view may read main tables (review fix: refused). hipp
/// runs a statement reading such views on main, the views written
/// in as common table expressions; one that also reads a temp table
/// stays refused.
#[test]
fn temp_views_over_main_tables() {
    for (i, script) in [
        "CREATE TEMP TABLE t(a);
         INSERT INTO t VALUES (1);
         CREATE VIEW temp.v AS SELECT * FROM temp.t WHERE t.a > 0;
         CREATE TEMP VIEW v4 AS SELECT name FROM \"sqlite_temp_master\" WHERE type = 'table';
         CREATE TEMP VIEW v5 AS SELECT name FROM sqlite_temp_schema WHERE type = 'view';
         SELECT * FROM v;
         SELECT * FROM temp.v;
         SELECT * FROM v4;
         SELECT * FROM v5;
         ALTER TABLE t RENAME TO u;
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TABLE m(a, b);
         INSERT INTO m VALUES (1, 2), (3, 4);
         CREATE TEMP VIEW tv AS SELECT * FROM m;
         SELECT * FROM tv;
         SELECT * FROM temp.tv;
         SELECT a FROM tv WHERE a > 1;
         SELECT tv.a FROM tv;
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TABLE m(a, b);
         INSERT INTO m VALUES (1, 2), (3, 4);
         CREATE TEMP VIEW tv AS SELECT * FROM m;
         CREATE TEMP VIEW tv2 AS SELECT a * 10 AS x FROM tv;
         CREATE TEMP VIEW tv3(p, q) AS SELECT b, a FROM main.m;
         SELECT * FROM tv2;
         SELECT * FROM tv3 ORDER BY p DESC;
         SELECT (SELECT count(*) FROM tv), (SELECT max(x) FROM tv2);
         SELECT * FROM m JOIN tv ON m.a = tv.a;
         SELECT * FROM tv2, tv3 WHERE x = q * 10;
         SELECT sql FROM sqlite_temp_schema",
        "CREATE TABLE m(a, b);
         INSERT INTO m VALUES (1, 2), (3, 4);
         CREATE TEMP VIEW tv AS SELECT * FROM m;
         CREATE TEMP VIEW tv2 AS SELECT a * 10 AS x FROM tv;
         INSERT INTO m SELECT a + 10, b FROM tv;
         SELECT count(*) FROM m;
         UPDATE m SET b = b + 1 WHERE a IN (SELECT a FROM tv WHERE a < 3);
         DELETE FROM m WHERE a IN (SELECT a FROM tv2 WHERE x > 100);
         SELECT * FROM m;
         WITH c AS (SELECT 1 AS k) SELECT * FROM c, tv WHERE k = 1;
         WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 2) SELECT * FROM r, tv ORDER BY 1, 2",
        "CREATE TABLE m(a);
         INSERT INTO m VALUES (1);
         CREATE TEMP VIEW tv AS SELECT * FROM m;
         DROP TABLE m;
         SELECT * FROM tv;
         CREATE TABLE m(a, b);
         INSERT INTO m VALUES (5, 6);
         SELECT * FROM tv;
         DROP VIEW tv;
         SELECT * FROM tv;
         SELECT name FROM sqlite_temp_schema",
        "CREATE TABLE m(a);
         CREATE TEMP VIEW tv AS SELECT * FROM nosuch;
         SELECT * FROM tv;
         CREATE TEMP VIEW tv4 AS SELECT * FROM m WHERE a > 0 /* trailing */;
         INSERT INTO m VALUES (1), (2);
         SELECT * FROM tv4;
         INSERT INTO tv4 VALUES (3);
         DELETE FROM tv4;
         SELECT count(*) FROM m",
        "CREATE TABLE tv(z);
         INSERT INTO tv VALUES ('main');
         CREATE TABLE m(a);
         INSERT INTO m VALUES (1);
         CREATE TEMP VIEW tv AS SELECT a AS z FROM m;
         SELECT * FROM tv;
         SELECT * FROM main.tv;
         SELECT * FROM temp.tv",
    ]
    .iter()
    .enumerate()
    {
        let mut stmts = lines(script);
        stmts.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
        same(&format!("tvm{i}"), &stmts);
    }
}

/// Random interleavings of main and temp statements — a main table,
/// a temp table, one name in both (the temp one shadows main's),
/// temp indexes and views, transactions and savepoints — compared
/// with the pin statement by statement (every statement names one
/// database only).
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
    let mut rng = Rng(0x7e3b_0016_a11e_c0de);
    let cases = std::env::var("HIPP_TEMP_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    for case in 0..cases {
        let mut stmts = vec![
            "CREATE TABLE m(a INTEGER PRIMARY KEY, b)".to_string(),
            "CREATE TABLE s(k, v)".to_string(),
            "INSERT INTO s VALUES ('main', 0)".to_string(),
        ];
        for _ in 0..40 {
            let v = rng.pick(20);
            let s = match rng.pick(24) {
                0 => {
                    "CREATE TEMP TABLE x(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE)".to_string()
                }
                1 => "CREATE TEMP TABLE IF NOT EXISTS s(k, v)".to_string(),
                2 => format!("INSERT INTO x(b) VALUES ({v})"),
                3 => format!("INSERT OR REPLACE INTO x(b) VALUES ({v})"),
                4 => format!(
                    "INSERT INTO temp.x VALUES (NULL, {v}) ON CONFLICT(b) DO UPDATE SET b = b + 100"
                ),
                5 => format!("INSERT INTO s VALUES ('t{v}', {v})"),
                6 => format!("INSERT INTO main.s VALUES ('m{v}', {v})"),
                7 => format!("INSERT INTO m(b) VALUES ({v})"),
                8 => format!("DELETE FROM x WHERE b % 3 = {}", rng.pick(3)),
                9 => format!("UPDATE s SET v = v + 1 WHERE v < {v}"),
                10 => "DROP TABLE s".to_string(),
                11 => "DROP TABLE IF EXISTS temp.x".to_string(),
                12 => "CREATE INDEX temp.xb ON x(b DESC)".to_string(),
                13 => "CREATE TEMP VIEW tv AS SELECT b * 2 AS d FROM x".to_string(),
                14 => "SELECT * FROM tv ORDER BY d".to_string(),
                15 => "BEGIN".to_string(),
                16 => "COMMIT".to_string(),
                17 => "ROLLBACK".to_string(),
                18 => "SAVEPOINT sp".to_string(),
                19 => "ROLLBACK TO sp".to_string(),
                20 => "RELEASE sp".to_string(),
                21 => "SELECT * FROM s ORDER BY k".to_string(),
                22 => "SELECT a, b FROM x ORDER BY a".to_string(),
                _ => "SELECT * FROM temp.sqlite_sequence".to_string(),
            };
            stmts.push(s);
            if rng.pick(5) == 0 {
                // No rootpage: hipp reuses freed pages in another
                // order than the pin (STATUS.md, honest gaps).
                stmts.push("SELECT type, name, sql FROM sqlite_temp_schema".to_string());
            }
        }
        stmts.push("SELECT * FROM main.s ORDER BY k".to_string());
        stmts.push("SELECT * FROM m".to_string());
        stmts.push("SELECT type, name, sql FROM sqlite_temp_master".to_string());
        stmts.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
        same(&format!("gen{case}"), &stmts);
    }
}
