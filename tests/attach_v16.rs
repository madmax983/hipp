//! v0.16 ATTACH / DETACH and statements across several databases
//! (leaf `attach`), differentially vs the pinned oracle (SQLite
//! 3.54.1, live). Probed on the pin before implementation; the
//! matrix the tests encode:
//!
//! - `ATTACH [DATABASE] expr AS expr [KEY expr]`: a lone identifier
//!   operand is its own text; NULL / `''` is a private temporary
//!   database, `':memory:'` an in-memory one; the limit (10) is
//!   checked first, then the name (`main`, `temp` and attached names,
//!   any case: "database X is already in use"), then the open ("unable
//!   to open database") and the encoding ("attached databases must
//!   use the same text encoding as main database"). An attach inside
//!   a transaction joins it.
//! - `DETACH [DATABASE] expr`: "no such database" (also `temp` before
//!   the TEMP database exists), "cannot detach database main / temp",
//!   "database X is locked" once the open transaction has read or
//!   written it.
//! - Names: `aux.t` is aux's; an unqualified name is temp's, then
//!   main's, then each attached database's in attach order; `aux.t.c`
//!   three-part names, `aux.sqlite_schema`, `PRAGMA aux.x`, VACUUM /
//!   REINDEX / ANALYZE / ALTER / DROP / CREATE in aux; views and
//!   triggers of one database cannot name another's objects; a
//!   table-valued pragma's qualifier is ignored.
//! - Statements reading several databases (joins, subqueries,
//!   compounds, INSERT ... SELECT both ways, UPDATE ... FROM, CREATE
//!   TABLE ... AS) and transactions spanning them (BEGIN / COMMIT /
//!   ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO).
//!
//! Every script runs on both engines in twin directories (attached
//! files named `{d}/x.db`, the directory written back as `{d}` in the
//! output), continuing past errors; rows, failing statements, and the
//! pin's integrity check and `.dump` of every database file must agree.

#![cfg(feature = "attach")]

mod common;

use hipp::storage::{AttachTarget, MemStorage, block_on};
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};
use std::path::{Path, PathBuf};

fn fresh_dir(tag: &str) -> PathBuf {
    let p = common::temp_db(&format!("v16att-{tag}"));
    p.parent().expect("dir").to_path_buf()
}

fn render<S: hipp::Storage>(q: &hipp::Statement<'_, S>, line: &mut String) {
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

/// Run `stmts` on a connection (`{d}` standing for `dir`): rows, and
/// the 1-based numbers of the statements that failed.
async fn run_on<S: hipp::Storage>(
    db: &mut Connection<S>,
    dir: &str,
    stmts: &[String],
) -> (Vec<String>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut errs = Vec::new();
    for (i, sql) in stmts.iter().enumerate() {
        let sql = sql.replace("{d}", dir);
        let mut q = match db.prepare(&sql).await {
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
                    lines.push(if dir.is_empty() {
                        l
                    } else {
                        l.replace(dir, "{d}")
                    });
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
}

fn hipp_run(dir: &Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    block_on(async {
        let fs = FileStorage::open(dir.join("main.db")).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let d = dir.to_string_lossy().to_string();
        let r = run_on(&mut db, &d, stmts).await;
        db.close().await.expect("close");
        r
    })
}

/// The pin's shell over stdin, one statement per line: stdout rows,
/// and the lines its errors name.
fn oracle_run(dir: &Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    use std::io::Write;
    let d = dir.to_string_lossy().to_string();
    let mut child = std::process::Command::new(common::oracle())
        .arg(dir.join("main.db"))
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    let mut input = String::new();
    for s in stmts {
        assert!(!s.contains('\n'), "one statement per line: {s}");
        input.push_str(&s.replace("{d}", &d));
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
        .map(|l| l.replace(&d, "{d}"))
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
    errs.dedup();
    (lines, errs)
}

/// The pin's view of every database file in `dir`: name, integrity
/// check and `.dump`.
fn files_view(dir: &Path) -> Vec<(String, String, String)> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "db"))
        .collect();
    names.sort();
    names
        .iter()
        .map(|p| {
            (
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                common::cli(p, "PRAGMA integrity_check;"),
                common::cli(p, ".dump"),
            )
        })
        .collect()
}

/// Run `stmts` on twin fresh directories: rows, failing statements
/// and every database file must agree.
fn same(tag: &str, stmts: &[String]) {
    let h = fresh_dir(&format!("{tag}-h"));
    let o = fresh_dir(&format!("{tag}-o"));
    let (hl, he) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ:\n{script}");
    assert_eq!(he, oe, "{tag}: failing statements differ:\n{script}");
    let hv = files_view(&h);
    let ov = files_view(&o);
    for (name, integrity, _) in hv.iter() {
        assert_eq!(integrity.trim(), "ok", "{tag}: hipp's {name}\n{script}");
    }
    assert_eq!(hv, ov, "{tag}: final files differ:\n{script}");
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

fn each(tag: &str, cases: &[&str]) {
    for (i, c) in cases.iter().enumerate() {
        same(&format!("{tag}{i}"), &lines(c));
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

#[test]
fn attach_and_detach() {
    each(
        "ad",
        &[
            "ATTACH '{d}/a.db' AS aux
             PRAGMA database_list
             CREATE TABLE aux.t(a, b)
             INSERT INTO aux.t VALUES (1, 2)
             SELECT * FROM t
             SELECT * FROM aux.t
             ATTACH '{d}/b.db' AS aux
             ATTACH '{d}/b.db' AS AUX
             ATTACH '{d}/b.db' AS main
             ATTACH '{d}/b.db' AS temp
             ATTACH '{d}/b.db' AS 'Main'
             DETACH main
             DETACH temp
             DETACH nope
             DETACH DATABASE aux
             SELECT * FROM aux.t
             ATTACH DATABASE '{d}/a.db' AS aux
             SELECT * FROM aux.t
             PRAGMA database_list",
            "ATTACH ':memory:' AS m
             ATTACH '' AS e
             ATTACH NULL AS n
             PRAGMA database_list
             CREATE TABLE m.t(x)
             INSERT INTO m.t VALUES (1)
             SELECT * FROM m.t
             CREATE TABLE e.t(y)
             INSERT INTO e.t SELECT x + 1 FROM m.t
             SELECT * FROM e.t
             PRAGMA m.journal_mode
             PRAGMA e.journal_mode
             DETACH m
             ATTACH ':memory:' AS m
             SELECT * FROM m.t
             SELECT seq, name FROM pragma_database_list",
            "ATTACH '{d}/a' || '.db' AS 'x' || 'y'
             ATTACH ('{d}/' || 'b.db') AS (SELECT 'q')
             ATTACH '{d}/c.db' AS \"quoted\"
             ATTACH '{d}/d.db' AS 7
             ATTACH '{d}/e.db' AS 'w x'
             ATTACH '{d}/f.db' AS ''
             ATTACH '{d}/g.db' AS ''
             ATTACH '{d}/h.db' AS hh KEY 'secret'
             ATTACH '{d}/i.db' AS c || 'd'
             SELECT seq, name FROM pragma_database_list
             DETACH 'xy'
             DETACH \"Q\"
             DETACH [quoted]
             DETACH 7
             DETACH ''
             SELECT seq, name FROM pragma_database_list",
            "CREATE TEMP TABLE tt(x)
             DETACH temp
             DETACH TEMP
             DETACH 'main'
             ATTACH '{d}/a1.db' AS a1
             ATTACH '{d}/a2.db' AS a2
             ATTACH '{d}/a3.db' AS a3
             ATTACH '{d}/a4.db' AS a4
             ATTACH '{d}/a5.db' AS a5
             ATTACH '{d}/a6.db' AS a6
             ATTACH '{d}/a7.db' AS a7
             ATTACH '{d}/a8.db' AS a8
             ATTACH '{d}/a9.db' AS a9
             ATTACH '{d}/a10.db' AS a10
             ATTACH '{d}/a11.db' AS a11
             ATTACH '{d}/a11.db' AS a1
             ATTACH '{d}/a11.db' AS ''
             PRAGMA database_list
             DETACH a3
             ATTACH '{d}/a11.db' AS a11
             SELECT seq, name FROM pragma_database_list",
            "ATTACH '/nonexistent-dir/x.db' AS bad
             ATTACH '{d}/nodir/x.db' AS bad
             SELECT count(*) FROM pragma_database_list",
            "CREATE TABLE m(x)
             ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(y)
             BEGIN
             INSERT INTO m VALUES (1)
             DETACH a
             SELECT * FROM a.t
             DETACH a
             ATTACH '{d}/b.db' AS b
             DETACH b
             COMMIT
             DETACH a
             BEGIN
             INSERT INTO a.t VALUES (1)
             DETACH a
             ROLLBACK
             DETACH a
             PRAGMA database_list",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x)
             BEGIN
             ATTACH '{d}/b.db' AS b
             CREATE TABLE b.u(y)
             INSERT INTO a.t VALUES (1)
             INSERT INTO b.u VALUES (2)
             ROLLBACK
             SELECT count(*) FROM b.sqlite_schema
             SELECT count(*) FROM a.t
             SAVEPOINT s
             INSERT INTO a.t VALUES (3)
             ATTACH '{d}/c.db' AS c
             CREATE TABLE c.w(z)
             SAVEPOINT q
             INSERT INTO c.w VALUES (4)
             ROLLBACK TO q
             INSERT INTO c.w VALUES (5)
             RELEASE s
             SELECT * FROM a.t
             SELECT * FROM c.w
             DETACH c",
        ],
    );
}

#[test]
fn name_resolution() {
    each(
        "nr",
        &[
            "ATTACH '{d}/a.db' AS a
             ATTACH '{d}/b.db' AS b
             CREATE TABLE a.t(x)
             INSERT INTO a.t VALUES ('a')
             CREATE TABLE b.t(x)
             INSERT INTO b.t VALUES ('b')
             CREATE TABLE b.u(y)
             INSERT INTO b.u VALUES ('bu')
             SELECT * FROM t
             SELECT * FROM u
             CREATE TABLE t(x)
             INSERT INTO t VALUES ('main')
             SELECT * FROM t
             CREATE TEMP TABLE t(x)
             INSERT INTO t VALUES ('temp')
             SELECT * FROM t
             SELECT * FROM main.t
             SELECT * FROM a.t
             INSERT INTO u VALUES ('bu2')
             SELECT * FROM b.u
             DROP TABLE t
             SELECT * FROM t
             DROP TABLE t
             SELECT * FROM t
             DROP TABLE u
             SELECT name FROM b.sqlite_master",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x)
             SELECT * FROM nosuch.t
             CREATE TABLE nosuch.t(x)
             INSERT INTO nosuch.t VALUES (1)
             DROP TABLE nosuch.t
             DROP TABLE IF EXISTS nosuch.t
             DROP INDEX IF EXISTS nosuch.i
             DROP VIEW IF EXISTS nosuch.v
             CREATE INDEX nosuch.i ON t(x)
             UPDATE nosuch.t SET x = 1
             PRAGMA nosuch.table_info(t)
             VACUUM nosuch
             SELECT nosuch.t.x FROM a.t
             SELECT main.t.x FROM a.t
             SELECT a.t.x FROM t
             SELECT * FROM a.nosuch",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE m(k, v)
             INSERT INTO m VALUES (1, 'a'), (2, 'b')
             CREATE TABLE a.t(k, w)
             INSERT INTO a.t VALUES (1, 'A'), (3, 'C')
             SELECT a.t.k, a.t.w FROM a.t
             SELECT a.t.w, main.m.v FROM a.t, main.m WHERE a.t.k = main.m.k
             SELECT k FROM m WHERE k IN a.t
             SELECT k FROM a.t WHERE k NOT IN m
             SELECT * FROM a.t AS q WHERE q.k = 1
             SELECT a.q.w FROM a.t AS q
             SELECT a.t.w FROM a.t AS q
             SELECT * FROM a.t t1 JOIN a.t t2 USING (k)
             SELECT * FROM m NATURAL JOIN a.t
             SELECT * FROM m, a.t ORDER BY m.k, t.k
             SELECT t.* FROM m, a.t ORDER BY 1
             SELECT x.k FROM m AS x, a.t AS x
             SELECT k FROM m, a.t",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x)
             SELECT name FROM a.sqlite_schema
             SELECT name FROM a.sqlite_master
             SELECT name FROM a.sqlite_temp_master
             SELECT type, name, tbl_name, sql FROM a.sqlite_schema
             CREATE INDEX a.i ON t(x)
             CREATE UNIQUE INDEX IF NOT EXISTS a.i2 ON t(x)
             CREATE TABLE IF NOT EXISTS a.u(y)
             CREATE TABLE IF NOT EXISTS a.u(y)
             CREATE TABLE a.\"w w\"(y)
             CREATE INDEX a.i3 ON a.t(x)
             CREATE INDEX i4 ON t(x)
             SELECT name, sql FROM a.sqlite_schema ORDER BY name",
        ],
    );
}

#[test]
fn schema_objects_in_attached_databases() {
    each(
        "so",
        &[
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.n(k INTEGER PRIMARY KEY AUTOINCREMENT, w TEXT UNIQUE, z DEFAULT 7)
             INSERT INTO a.n(w) VALUES ('x'), ('y')
             SELECT * FROM a.sqlite_sequence
             CREATE TABLE a.o(k, w, PRIMARY KEY(k)) WITHOUT ROWID
             INSERT INTO a.o VALUES (2, 'two'), (1, 'one')
             SELECT * FROM a.o
             CREATE TABLE a.s(a INTEGER, b TEXT) STRICT
             INSERT INTO a.s VALUES ('x', 1)
             CREATE TABLE a.g(a, b AS (a * 2) STORED, c AS (a + 1))
             INSERT INTO a.g(a) VALUES (5)
             SELECT * FROM a.g
             CREATE VIEW a.v AS SELECT w FROM n
             CREATE VIEW a.v2 AS SELECT * FROM a.n
             SELECT * FROM a.v
             SELECT * FROM a.v2
             ALTER TABLE a.n RENAME COLUMN w TO ww
             ALTER TABLE a.n ADD COLUMN q DEFAULT 'qq'
             ALTER TABLE n RENAME TO nn
             SELECT * FROM a.v
             SELECT name, sql FROM a.sqlite_schema ORDER BY name
             ALTER TABLE a.nn DROP COLUMN q
             DROP VIEW a.v2
             DROP VIEW v
             SELECT name FROM a.sqlite_schema ORDER BY name",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x)
             CREATE TABLE a.log(m)
             CREATE TABLE t2(x)
             CREATE TRIGGER a.tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.x); END
             INSERT INTO a.t VALUES (1)
             SELECT * FROM a.log
             CREATE TRIGGER a.tr2 AFTER INSERT ON t BEGIN INSERT INTO a.log VALUES (new.x); END
             CREATE TRIGGER a.tr3 AFTER INSERT ON t BEGIN INSERT INTO main.t2 VALUES (new.x); END
             CREATE TRIGGER a.tr4 AFTER INSERT ON t BEGIN INSERT INTO t2 VALUES (new.x); END
             INSERT INTO a.t VALUES (2)
             DROP TRIGGER a.tr4
             CREATE TRIGGER tr5 AFTER INSERT ON a.t BEGIN SELECT 1; END
             CREATE TRIGGER a.tr6 AFTER INSERT ON a.t BEGIN SELECT 1; END
             CREATE TRIGGER a.tr9 AFTER INSERT ON t BEGIN SELECT * FROM main.t2; END
             CREATE TRIGGER a.tr10 AFTER DELETE ON t BEGIN UPDATE log SET m = m WHERE m IN (SELECT x FROM main.t2); END
             CREATE TRIGGER nosuch.tr AFTER INSERT ON t BEGIN SELECT 1; END
             INSERT INTO a.t VALUES (3)
             SELECT * FROM a.log
             SELECT name, sql FROM a.sqlite_master WHERE type = 'trigger' ORDER BY name
             DROP TRIGGER tr
             DROP TRIGGER IF EXISTS nosuch.tr
             SELECT name FROM a.sqlite_master WHERE type = 'trigger'",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x)
             CREATE VIEW a.v2 AS SELECT * FROM main.t
             CREATE TABLE b(y)
             CREATE VIEW v3 AS SELECT * FROM a.t
             CREATE VIEW v4 AS SELECT * FROM t
             SELECT * FROM v4
             CREATE TEMP VIEW tv AS SELECT * FROM a.t, b
             SELECT * FROM tv
             INSERT INTO a.t VALUES (1)
             INSERT INTO b VALUES (2)
             SELECT * FROM tv",
            "ATTACH '{d}/a.db' AS a
             PRAGMA foreign_keys = ON
             CREATE TABLE a.p(id INTEGER PRIMARY KEY)
             CREATE TABLE a.c(pid REFERENCES p(id) ON DELETE CASCADE)
             INSERT INTO a.c VALUES (5)
             INSERT INTO a.p VALUES (5)
             INSERT INTO a.c VALUES (5)
             SELECT * FROM a.c
             DELETE FROM a.p
             SELECT count(*) FROM a.c
             PRAGMA a.foreign_keys
             PRAGMA a.foreign_keys = OFF
             PRAGMA foreign_keys
             INSERT INTO a.c VALUES (9)
             PRAGMA a.foreign_key_check",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(k INTEGER PRIMARY KEY, v)
             INSERT INTO a.t VALUES (1, 'x'), (2, 'y'), (3, 'z')
             DELETE FROM a.t WHERE k = 2
             VACUUM a
             SELECT * FROM a.t
             VACUUM a INTO '{d}/copy.db'
             ATTACH '{d}/copy.db' AS c
             SELECT * FROM c.t
             CREATE INDEX a.ti ON t(v)
             REINDEX a.ti
             REINDEX a.t
             ANALYZE a
             SELECT * FROM a.sqlite_stat1
             ANALYZE a.t
             PRAGMA a.integrity_check
             PRAGMA a.quick_check
             PRAGMA c.integrity_check",
        ],
    );
}

#[test]
fn schema_scoped_pragmas() {
    each(
        "pr",
        &[
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(x INTEGER PRIMARY KEY, y TEXT UNIQUE)
             PRAGMA a.table_info(t)
             PRAGMA table_info(t)
             PRAGMA a.table_xinfo(t)
             PRAGMA a.user_version = 7
             PRAGMA a.user_version
             PRAGMA user_version
             PRAGMA a.application_id = 5
             PRAGMA a.application_id
             PRAGMA a.page_size
             PRAGMA a.page_count
             PRAGMA a.freelist_count
             PRAGMA a.schema_version
             PRAGMA a.journal_mode
             PRAGMA journal_mode
             PRAGMA a.integrity_check
             PRAGMA a.index_list(t)
             PRAGMA index_list(t)
             PRAGMA a.index_info(sqlite_autoindex_t_1)
             PRAGMA a.encoding
             PRAGMA a.auto_vacuum
             PRAGMA a.synchronous
             PRAGMA nosuch.user_version
             PRAGMA nosuch.foreign_keys
             PRAGMA a.database_list
             PRAGMA table_list
             PRAGMA a.table_list
             PRAGMA main.table_list",
            "ATTACH '{d}/a.db' AS a
             ATTACH '{d}/b.db' AS b
             CREATE TABLE a.t(x)
             CREATE TABLE b.t(y)
             CREATE TABLE main.t(z)
             SELECT 'aq', * FROM a.pragma_table_info('t')
             SELECT 'mq', * FROM main.pragma_table_info('t')
             SELECT 'bs', * FROM pragma_table_info('t', 'b')
             SELECT 'as', * FROM pragma_table_info('t', 'a')
             SELECT 'bl', * FROM b.pragma_table_list
             SELECT 'bl2', * FROM pragma_table_list('t')
             SELECT * FROM pragma_table_info('t', 'nosuch')
             DROP TABLE main.t
             PRAGMA table_info(t)
             SELECT * FROM pragma_table_info('t')
             CREATE INDEX b.bi ON t(y)
             SELECT * FROM pragma_index_list('t', 'b')
             SELECT * FROM pragma_index_list('t')
             PRAGMA index_info(bi)",
            "ATTACH '{d}/a.db' AS a
             PRAGMA a.page_size = 1024
             CREATE TABLE a.t(x)
             PRAGMA a.page_size
             PRAGMA page_size
             PRAGMA a.journal_mode = WAL
             PRAGMA a.journal_mode
             PRAGMA journal_mode
             INSERT INTO a.t VALUES (1)
             BEGIN
             INSERT INTO a.t VALUES (2)
             CREATE TABLE m(x)
             INSERT INTO m SELECT * FROM a.t
             COMMIT
             SELECT * FROM m
             PRAGMA a.wal_checkpoint(TRUNCATE)
             PRAGMA a.journal_mode = DELETE
             SELECT * FROM a.t",
        ],
    );
}

#[test]
fn statements_across_databases() {
    each(
        "x",
        &[
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE m(k INTEGER PRIMARY KEY, v)
             INSERT INTO m VALUES (1, 'one'), (2, 'two'), (3, 'three')
             CREATE TABLE a.n(k INTEGER PRIMARY KEY, w)
             INSERT INTO a.n SELECT k, upper(v) FROM main.m WHERE k < 3
             SELECT * FROM a.n
             INSERT INTO m(v) SELECT w FROM a.n
             SELECT * FROM m
             SELECT m.v, n.w FROM m JOIN a.n USING (k)
             SELECT * FROM m WHERE k IN (SELECT k FROM a.n)
             SELECT (SELECT w FROM a.n WHERE n.k = m.k) FROM m
             UPDATE a.n SET w = (SELECT v FROM m WHERE m.k = n.k)
             SELECT * FROM a.n
             UPDATE m SET v = n.w FROM a.n WHERE n.k = m.k + 1
             SELECT * FROM m
             UPDATE a.n SET w = m.v FROM m WHERE m.k = n.k
             SELECT * FROM a.n
             DELETE FROM a.n WHERE k IN (SELECT k FROM m WHERE v = 'one')
             SELECT * FROM a.n
             DELETE FROM m WHERE k NOT IN (SELECT k FROM a.n)
             SELECT * FROM m
             CREATE TABLE a.c AS SELECT * FROM m
             SELECT sql FROM a.sqlite_master WHERE name = 'c'
             CREATE TABLE c2 AS SELECT k, w FROM a.n
             SELECT sql FROM sqlite_master WHERE name = 'c2'
             INSERT INTO a.n VALUES (9, 'x') RETURNING *
             INSERT INTO a.n VALUES (9, 'y') ON CONFLICT(k) DO UPDATE SET w = (SELECT count(*) FROM m) RETURNING *
             WITH c AS (SELECT * FROM a.n) SELECT * FROM c
             SELECT * FROM a.n UNION SELECT k, v FROM m ORDER BY 1, 2
             SELECT k FROM a.n INTERSECT SELECT k FROM m
             SELECT count(*), sum(k), group_concat(w) FROM a.n",
            "ATTACH '{d}/a.db' AS a
             ATTACH '{d}/b.db' AS b
             CREATE TABLE m(k INTEGER PRIMARY KEY, v TEXT)
             CREATE TABLE a.n(k INTEGER PRIMARY KEY AUTOINCREMENT, w TEXT UNIQUE, z DEFAULT 7)
             CREATE TABLE b.o(k, w, PRIMARY KEY(k)) WITHOUT ROWID
             INSERT INTO m VALUES (1, 'x'), (2, 'y'), (3, 'z')
             INSERT INTO a.n(w) SELECT v FROM m
             INSERT INTO b.o SELECT k, w FROM a.n WHERE k > 1
             SELECT * FROM b.o
             SELECT m.v, n.z, o.w FROM m LEFT JOIN a.n ON n.k = m.k LEFT JOIN b.o ON o.k = m.k ORDER BY 1
             SELECT v FROM m UNION SELECT w FROM a.n UNION SELECT w FROM b.o ORDER BY 1
             WITH c AS (SELECT k FROM a.n) SELECT * FROM c JOIN m USING (k)
             SELECT (SELECT count(*) FROM b.o WHERE o.k <= m.k) FROM m
             INSERT OR REPLACE INTO a.n(k, w) SELECT k + 10, v || 'r' FROM m RETURNING k, w
             UPDATE a.n SET z = z + 1 WHERE w IN (SELECT v FROM m)
             UPDATE m SET v = (SELECT w FROM b.o WHERE o.k = m.k) WHERE k > 1
             SELECT * FROM a.n ORDER BY k
             SELECT * FROM m
             SELECT rowid, * FROM b.o
             SELECT o.rowid FROM b.o, m
             CREATE TEMP TABLE tt AS SELECT * FROM b.o
             INSERT INTO temp.tt SELECT k + 10, w FROM a.n ORDER BY k
             SELECT * FROM tt
             UPDATE a.n SET w = (SELECT max(w) FROM tt) WHERE k = 1
             SELECT * FROM a.n WHERE k > (SELECT min(k) FROM temp.tt) ORDER BY k
             SELECT * FROM tt, b.o WHERE tt.k = o.k",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE m(k, v)
             CREATE TABLE a.t(k, v)
             CREATE VIEW a.v AS SELECT k * 2 AS kk FROM t
             INSERT INTO a.t VALUES (1, 'p')
             INSERT INTO m VALUES (1, 'q')
             SELECT * FROM a.v
             SELECT * FROM v
             SELECT * FROM a.t, (SELECT * FROM m) AS d WHERE d.k = t.k
             SELECT * FROM (SELECT * FROM a.t) AS d, m",
        ],
    );
}

#[test]
fn transactions_across_databases() {
    each(
        "tx",
        &[
            "CREATE TABLE m(x)
             ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(y UNIQUE ON CONFLICT ROLLBACK)
             BEGIN
             INSERT INTO m VALUES (1)
             INSERT INTO a.t VALUES (1)
             SAVEPOINT s1
             INSERT INTO m VALUES (2)
             INSERT INTO a.t VALUES (2)
             ROLLBACK TO s1
             INSERT INTO a.t VALUES (3)
             RELEASE s1
             DETACH a
             COMMIT
             SELECT * FROM m
             SELECT * FROM a.t
             BEGIN
             INSERT INTO m VALUES (4)
             INSERT INTO a.t VALUES (4)
             ROLLBACK
             SELECT * FROM m
             SELECT * FROM a.t
             BEGIN
             INSERT INTO m VALUES (5)
             INSERT INTO a.t VALUES (1)
             INSERT INTO m VALUES (6)
             COMMIT
             SELECT * FROM m
             SELECT * FROM a.t",
            "CREATE TABLE m(x UNIQUE)
             ATTACH '{d}/a.db' AS a
             CREATE TABLE a.t(y UNIQUE)
             BEGIN
             INSERT INTO a.t VALUES (1)
             INSERT OR ROLLBACK INTO m VALUES (1), (1)
             COMMIT
             SELECT count(*) FROM a.t
             BEGIN
             INSERT INTO m VALUES (2)
             INSERT OR FAIL INTO a.t VALUES (2), (2)
             COMMIT
             SELECT * FROM m
             SELECT * FROM a.t
             SAVEPOINT x
             INSERT INTO a.t VALUES (3)
             RELEASE x
             SELECT * FROM a.t
             BEGIN IMMEDIATE
             INSERT INTO a.t VALUES (10)
             END
             SELECT * FROM a.t
             COMMIT
             ROLLBACK",
            "ATTACH '{d}/a.db' AS a
             ATTACH '{d}/b.db' AS b
             CREATE TABLE a.t(x)
             CREATE TABLE b.t(x)
             BEGIN
             INSERT INTO a.t VALUES (1)
             INSERT INTO b.t SELECT x + 1 FROM a.t
             COMMIT
             SELECT * FROM a.t
             SELECT * FROM b.t
             BEGIN
             SELECT * FROM b.t
             DETACH b
             DETACH a
             ROLLBACK
             DETACH b
             SELECT seq, name FROM pragma_database_list",
            "ATTACH '{d}/a.db' AS a
             CREATE TABLE a.p(id INTEGER PRIMARY KEY)
             CREATE TABLE a.c(pid REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)
             CREATE TABLE m(x)
             PRAGMA foreign_keys = ON
             BEGIN
             INSERT INTO m VALUES (1)
             INSERT INTO a.c VALUES (7)
             COMMIT
             SELECT count(*) FROM m
             INSERT INTO a.p VALUES (7)
             COMMIT
             SELECT * FROM m
             SELECT * FROM a.c",
        ],
    );
}

#[cfg(feature = "utf16")]
#[test]
fn encodings_must_match() {
    each(
        "enc",
        &[
            "ATTACH '{d}/u.db' AS u
             PRAGMA u.encoding = 'UTF-16le'
             CREATE TABLE u.t(x)
             PRAGMA u.encoding
             PRAGMA encoding
             DETACH u
             SELECT 1",
            "PRAGMA encoding = 'UTF-16le'
             CREATE TABLE m(x)
             ATTACH '{d}/n.db' AS n
             PRAGMA n.encoding
             CREATE TABLE n.t(x)
             INSERT INTO n.t VALUES ('héllo')
             SELECT x, length(x), hex(x) FROM n.t
             PRAGMA n.encoding
             INSERT INTO m SELECT x || '!' FROM n.t
             SELECT hex(x) FROM m",
        ],
    );
    // A non-empty database of another encoding is refused.
    let h = fresh_dir("enc-mix-h");
    let o = fresh_dir("enc-mix-o");
    for d in [&h, &o] {
        common::cli(
            &d.join("u16.db"),
            "PRAGMA encoding = 'UTF-16be'; CREATE TABLE q(x); INSERT INTO q VALUES ('hi');",
        );
    }
    let stmts = lines(
        "ATTACH '{d}/u16.db' AS u
         SELECT * FROM u.q
         PRAGMA database_list
         PRAGMA encoding = 'UTF-16be'
         ATTACH '{d}/u16.db' AS u
         SELECT * FROM u.q",
    );
    assert_eq!(hipp_run(&h, &stmts), oracle_run(&o, &stmts));
}

/// Files move between the engines both ways: a hipp-written set of
/// attached databases is read by the pin, and a pin-written set by
/// hipp.
#[test]
fn files_interoperate() {
    let h = fresh_dir("interop-h");
    let write = lines(
        "ATTACH '{d}/a.db' AS a
         CREATE TABLE m(k INTEGER PRIMARY KEY, v)
         CREATE TABLE a.t(k INTEGER PRIMARY KEY, w, UNIQUE (w))
         CREATE INDEX a.tw ON t(w, k)
         BEGIN
         INSERT INTO m WITH RECURSIVE c(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM c WHERE value < 300) SELECT value, printf('%.200c', 'm') FROM c
         INSERT INTO a.t SELECT k, v || k FROM m
         COMMIT
         DELETE FROM a.t WHERE k % 3 = 0",
    );
    let (_, errs) = hipp_run(&h, &write);
    assert!(errs.is_empty(), "{errs:?}");
    for f in ["main.db", "a.db"] {
        assert_eq!(common::cli(&h.join(f), "PRAGMA integrity_check;"), "ok\n");
    }
    let read = format!(
        "ATTACH '{}' AS a; SELECT count(*), sum(k) FROM a.t; SELECT count(*) FROM m JOIN a.t USING (k); SELECT w FROM a.t WHERE w > 'mmmm' ORDER BY w LIMIT 1;",
        h.join("a.db").display()
    );
    let pin_view = common::cli(&h.join("main.db"), &read);
    let hipp_view = {
        let stmts = lines(
            &read
                .replace("; ", "\n")
                .replace(h.to_string_lossy().as_ref(), "{d}"),
        );
        hipp_run(&h, &stmts).0.join("\n") + "\n"
    };
    assert_eq!(hipp_view, pin_view);
    // The pin writes a set; hipp reads and writes it; the pin checks.
    let o = fresh_dir("interop-o");
    common::cli(
        &o.join("main.db"),
        &format!(
            "ATTACH '{}' AS b; CREATE TABLE b.t(k PRIMARY KEY, v) WITHOUT ROWID; \
             CREATE TABLE m(x); BEGIN; INSERT INTO m VALUES (1); \
             INSERT INTO b.t VALUES (1, 'one'), (2, 'two'); COMMIT;",
            o.join("b.db").display()
        ),
    );
    let (rows, errs) = hipp_run(
        &o,
        &lines(
            "ATTACH '{d}/b.db' AS b
             SELECT * FROM b.t
             BEGIN
             INSERT INTO b.t SELECT x + 10, 'ten' FROM m
             UPDATE m SET x = (SELECT count(*) FROM b.t)
             COMMIT
             SELECT * FROM m",
        ),
    );
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(rows, ["1|one", "2|two", "3"]);
    assert_eq!(
        common::cli(
            &o.join("main.db"),
            &format!(
                "ATTACH '{}' AS b; SELECT * FROM b.t; PRAGMA b.integrity_check; PRAGMA integrity_check;",
                o.join("b.db").display()
            )
        ),
        "1|one\n2|two\n11|ten\nok\nok\n"
    );
}

/// The storage hook's files, by name.
type Registry = std::rc::Rc<std::cell::RefCell<Vec<(Vec<u8>, MemStorage)>>>;

/// A `no_std` caller supplies storage for attached names through the
/// connection's hook (here a registry of `MemStorage`s that outlive
/// their attachment); without one, `MemStorage` reaches `:memory:`
/// and temporary databases but no file.
#[test]
fn storage_hook() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let r = db
            .prepare("ATTACH 'x.db' AS x")
            .await
            .expect("prep")
            .step()
            .await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: SchemaKind::CannotOpenDatabase
                })
            ),
            "{r:?}"
        );
        let mut q = db.prepare("ATTACH ':memory:' AS m").await.expect("prep");
        q.step().await.expect("memory database");
        drop(q);
        let registry: Registry = Default::default();
        let reg = registry.clone();
        db.set_attach_hook(move |t| match t {
            AttachTarget::File(n) => {
                let mut r = reg.borrow_mut();
                if let Some((_, m)) = r.iter().find(|(k, _)| k.as_slice() == n) {
                    return Some(m.clone());
                }
                let m = MemStorage::new();
                r.push((n.to_vec(), m.clone()));
                Some(m)
            }
            AttachTarget::Memory => Some(MemStorage::memory_db()),
            AttachTarget::Temporary => Some(MemStorage::new()),
        });
        let d = String::new();
        let (rows, errs) = run_on(
            &mut db,
            &d,
            &lines(
                "ATTACH 'x.db' AS x
                 CREATE TABLE x.t(a)
                 INSERT INTO x.t VALUES (1), (2)
                 CREATE TABLE m.u(b)
                 BEGIN
                 INSERT INTO m.u SELECT a * 10 FROM x.t
                 INSERT INTO x.t SELECT b FROM m.u
                 COMMIT
                 DETACH x
                 SELECT * FROM x.t
                 ATTACH 'x.db' AS y
                 SELECT * FROM y.t
                 SELECT name FROM pragma_database_list",
            ),
        )
        .await;
        assert_eq!(errs, [10]);
        assert_eq!(rows, ["1", "2", "10", "20", "main", "m", "y"]);
        let names: Vec<&[u8]> = db.attached_names().collect();
        assert_eq!(names, [&b"m"[..], b"y"]);
        assert_eq!(registry.borrow().len(), 1);
    });
}

/// Without the hook's help, each attached connection is the main
/// one's storage type: a file attached to a file-backed connection
/// is a `FileStorage`, and the same file twice is refused (hipp has
/// no shared page cache), as is a file that is main itself.
#[test]
fn same_file_twice_is_refused() {
    let d = fresh_dir("twice");
    block_on(async {
        let fs = FileStorage::open(d.join("main.db")).expect("fs");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let a = d.join("a.db");
        for (sql, ok) in [
            (format!("ATTACH '{}' AS a", a.display()), true),
            (format!("ATTACH '{}' AS b", a.display()), false),
            (
                format!("ATTACH '{}' AS c", d.join("main.db").display()),
                false,
            ),
        ] {
            let r = db.prepare(&sql).await.expect("prep").step().await;
            assert_eq!(r.is_ok(), ok, "{sql}: {r:?}");
        }
    });
}

/// A deterministic xorshift stream.
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
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
    fn int(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One random multi-database script: three databases (main, `a`,
/// `b`) with a table each, written and read across each other inside
/// and outside transactions and savepoints, with `b` detached and
/// re-attached on the way. Every query orders its rows by a unique key.
fn random_script(rng: &mut Rng) -> Vec<String> {
    let mut s = vec![
        "ATTACH '{d}/a.db' AS a".to_string(),
        "ATTACH '{d}/b.db' AS b".to_string(),
        "CREATE TABLE t(k INTEGER PRIMARY KEY, v, w)".to_string(),
        "CREATE TABLE a.t(k INTEGER PRIMARY KEY, v, w)".to_string(),
        "CREATE TABLE b.u(k INTEGER PRIMARY KEY, v UNIQUE, w)".to_string(),
    ];
    if rng.int(2) == 0 {
        s.push("CREATE INDEX a.tv ON t(v)".to_string());
    }
    let dbs = ["main.t", "a.t", "b.u"];
    let names = ["t", "a.t", "b.u", "main.t", "u"];
    for _ in 0..30 + rng.int(30) {
        let x = rng.pick(&dbs);
        let y = rng.pick(&dbs);
        let k = rng.int(20);
        let v = rng.int(8);
        let stmt = match rng.int(16) {
            0 => format!("INSERT OR IGNORE INTO {x} VALUES ({k}, {v}, 'w{k}')"),
            1 => format!("INSERT OR REPLACE INTO {x}(v, w) VALUES ({v}, 'r{v}')"),
            2 => format!("INSERT OR IGNORE INTO {x} SELECT k + {k}, v, w FROM {y} WHERE k < {v}"),
            3 => format!(
                "UPDATE {x} SET w = (SELECT max(w) FROM {y}) WHERE k % 3 = {}",
                k % 3
            ),
            4 => format!("DELETE FROM {x} WHERE k IN (SELECT k FROM {y} WHERE v = {v})"),
            5 => format!("UPDATE OR IGNORE {x} SET v = v + 1 WHERE k > {k}"),
            6 => format!(
                "SELECT p.k, p.v, q.w FROM {x} AS p JOIN {y} AS q ON p.v = q.v ORDER BY p.k, q.k"
            ),
            7 => format!(
                "SELECT count(*), sum(v), group_concat(k) FROM (SELECT k, v FROM {x} ORDER BY k)"
            ),
            8 => format!("SELECT k FROM {x} UNION SELECT k FROM {y} ORDER BY 1"),
            9 => format!(
                "SELECT k, (SELECT count(*) FROM {y} WHERE {y}.k <= p.k) FROM {x} AS p ORDER BY k"
            ),
            10 => rng
                .pick(&["BEGIN", "SAVEPOINT s", "SAVEPOINT r"])
                .to_string(),
            11 => rng
                .pick(&[
                    "COMMIT",
                    "ROLLBACK",
                    "RELEASE s",
                    "ROLLBACK TO s",
                    "RELEASE r",
                ])
                .to_string(),
            12 => rng
                .pick(&["DETACH b", "ATTACH '{d}/b.db' AS b", "DETACH a"])
                .to_string(),
            13 => format!("SELECT * FROM {} ORDER BY k", rng.pick(&names)),
            14 => format!("UPDATE {x} SET w = q.w FROM {y} AS q WHERE q.k = {x}.k + 1"),
            _ => format!(
                "INSERT INTO {x}(k, v, w) VALUES ({}, {v}, NULL) ON CONFLICT DO NOTHING",
                k + 20
            ),
        };
        s.push(stmt);
    }
    s.push("COMMIT".to_string());
    s.push("ATTACH '{d}/a.db' AS a".to_string());
    s.push("ATTACH '{d}/b.db' AS b".to_string());
    for t in ["main.t", "a.t", "b.u"] {
        s.push(format!("SELECT * FROM {t} ORDER BY k"));
    }
    s
}

#[test]
fn generated_multi_database_scripts() {
    let n: u64 = std::env::var("HIPP_ATTACH_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let mut rng = Rng(0x0a77_ac4d_b5e1_9c2f);
    for i in 0..n {
        let script = random_script(&mut rng);
        same(&format!("gen{i}"), &script);
    }
}

/// Regressions found while building the unit.
#[test]
fn regressions() {
    each(
        "rg",
        &[
            // A table-valued pragma under an attached database's
            // qualifier is main's function (the pin ignores it): it
            // was routed to the attached database and listed only its
            // tables.
            "ATTACH '{d}/b.db' AS b
             CREATE TABLE b.t(x)
             SELECT schema, name FROM b.pragma_table_list ORDER BY 1, 2",
            // A WITHOUT ROWID table read from another database was
            // refused.
            "ATTACH '{d}/b.db' AS b
             CREATE TABLE b.o(k PRIMARY KEY, w) WITHOUT ROWID
             INSERT INTO b.o VALUES (2, 'y'), (1, 'x')
             CREATE TABLE m(k)
             INSERT INTO m VALUES (1), (2)
             SELECT m.k, o.w FROM m JOIN b.o USING (k)
             SELECT o.rowid FROM b.o, m",
            // Connection-wide pragmas under an attached database's
            // name were "unknown database".
            "ATTACH '{d}/b.db' AS b
             PRAGMA b.foreign_keys = ON
             PRAGMA foreign_keys
             PRAGMA b.recursive_triggers
             PRAGMA b.encoding",
            // A RELEASE that committed several databases through the
            // super-journal left the attached ones in a transaction
            // (their savepoints were gone when RELEASE reached them),
            // so their later autocommit writes were never committed.
            "ATTACH '{d}/b.db' AS b
             CREATE TABLE t(k INTEGER PRIMARY KEY, v, w)
             CREATE TABLE b.u(k INTEGER PRIMARY KEY, v UNIQUE, w)
             SAVEPOINT r
             INSERT OR IGNORE INTO b.u VALUES (13, 1, 'w13')
             INSERT INTO main.t(k, v, w) VALUES (20, 3, NULL)
             RELEASE r
             INSERT INTO b.u(k, v, w) VALUES (38, 7, NULL)
             BEGIN
             INSERT INTO b.u VALUES (39, 8, NULL)
             INSERT INTO t VALUES (40, 9, NULL)
             COMMIT
             INSERT INTO b.u VALUES (41, 10, NULL)",
            // PRAGMA table_list of an attached database named itself
            // `main`.
            "ATTACH '{d}/b.db' AS b
             CREATE TABLE b.t(x)
             PRAGMA b.table_list
             PRAGMA table_list",
        ],
    );
}

/// What hipp refuses cleanly where the pin answers (STATUS.md's
/// honest gaps): a TEMP write other than INSERT ... SELECT reading an
/// attached table, an attached view read together with another
/// database's table, a TEMP trigger on an attached table.
#[test]
fn documented_refusals() {
    let d = fresh_dir("refusals");
    block_on(async {
        let fs = FileStorage::open(d.join("main.db")).expect("fs");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let dir = d.to_string_lossy().to_string();
        let (_, errs) = run_on(
            &mut db,
            &dir,
            &lines(
                "ATTACH '{d}/a.db' AS a
                 CREATE TABLE a.t(x)
                 CREATE VIEW a.v AS SELECT x FROM t
                 CREATE TABLE m(y)
                 CREATE TEMP TABLE tt(z)",
            ),
        )
        .await;
        assert!(errs.is_empty(), "{errs:?}");
        for sql in [
            "UPDATE temp.tt SET z = (SELECT max(x) FROM a.t)",
            "DELETE FROM tt WHERE z IN (SELECT x FROM a.t)",
            "SELECT * FROM a.v, m",
            "CREATE TEMP TRIGGER tg AFTER INSERT ON a.t BEGIN SELECT 1; END",
        ] {
            let r = db.prepare(sql).await.map(|_| ());
            assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
        }
    });
}
