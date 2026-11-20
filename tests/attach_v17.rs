//! v0.17 ATTACH parity (leaf `attach`), differentially vs the pinned
//! oracle (SQLite 3.54.1, live). Probed on the pin before
//! implementation; the matrix the tests encode:
//!
//! - A string literal names a schema, table or column where the pin's
//!   grammar takes `nm` (`'aux'.t`, `aux.'t'`, `'aux'.'t'.c`, `INSERT
//!   INTO 'aux'.t`, `PRAGMA 'aux'.x`, `CREATE TABLE 'aux'.z`, `FROM 't'`).
//! - An unqualified `PRAGMA integrity_check` / `quick_check` checks
//!   main, temp, then every attached database in order under one
//!   error limit, `ok` only when none reports anything; `PRAGMA
//!   x.integrity_check` checks x, `integrity_check(t)` the first `t`
//!   the search order finds. An unqualified `foreign_key_check` checks
//!   main only (its table argument follows the search order).
//! - A view of an attached database reads that database's tables
//!   wherever the statement runs (beside main's tables, under a join,
//!   in subqueries, CTEs, compounds, INSERT ... SELECT into main); a
//!   main view of the same name does not shadow `aux.v`.
//! - A TEMP UPDATE / DELETE / INSERT VALUES (RETURNING, upsert, UPDATE
//!   ... FROM) reads main and attached tables, with or without ATTACH.
//! - A write of an attached table reads a same-named table of another
//!   database by an unqualified name (`UPDATE aux.m SET y = (SELECT y
//!   FROM m)`: main's `m`), its own by `aux.m.x`, and another's by a
//!   three-part name (`main.m.y` with `FROM main.m`); `UPDATE aux.m ...
//!   FROM main.m` is no repeat of the target.
//!
//! Every script runs on both engines in twin directories (attached
//! files named `{d}/x.db`), continuing past errors; rows, failing
//! statements, and the pin's integrity check and `.dump` of every
//! database file must agree.

#![cfg(feature = "attach")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};
use std::path::{Path, PathBuf};

fn fresh_dir(tag: &str) -> PathBuf {
    let p = common::temp_db(&format!("v17att-{tag}"));
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

/// Item 7: string-literal names (`nm ::= STRING`).
#[test]
fn string_literal_names() {
    each(
        "sq",
        &[
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE m(x INTEGER PRIMARY KEY, y)
             CREATE TABLE 'aux'.t(a INTEGER PRIMARY KEY, b)
             INSERT INTO m VALUES (1, 'p'), (2, 'q')
             INSERT INTO 'aux'.t VALUES (1, 'p'), (2, 'q')
             INSERT INTO aux.'t' VALUES (3, 'r')
             INSERT INTO 'aux'.'t'(b) VALUES ('s')
             SELECT 1, * FROM 'aux'.t
             SELECT 2, 'aux'.t.a, 'aux'.'t'.b, \"aux\".t.a FROM aux.t
             SELECT 3, * FROM 'main'.m, 'aux'.'t' WHERE 'main'.m.x = 'aux'.t.a
             SELECT 4, * FROM 'temp'.sqlite_master
             UPDATE 'aux'.t SET b = upper(b) WHERE a > 2
             DELETE FROM 'aux'.t WHERE a = 1
             SELECT 5, * FROM aux.t
             PRAGMA 'aux'.table_info('t')
             PRAGMA 'aux'.user_version
             CREATE INDEX 'aux'.tb ON t(b)
             SELECT 6, name FROM 'aux'.sqlite_master ORDER BY 1
             DROP INDEX 'aux'.tb
             DROP TABLE 'aux'.'t'
             SELECT 7, count(*) FROM 'aux'.sqlite_master
             SELECT * FROM 'nosuch'.t",
            "CREATE TABLE 't'('a' INT, \"b\")
             INSERT INTO 't'('a', b) VALUES (1, 2)
             SELECT * FROM 't'
             SELECT * FROM t AS 'x' WHERE 'x'.a = 1
             SELECT 't'.a, t.'b' FROM t
             SELECT 'main'.'t'.a FROM 'main'.'t'
             DELETE FROM 'main'.'t' WHERE 'a' = 1
             SELECT count(*) FROM t",
        ],
    );
}

/// Item 3: TEMP writes reading other databases (with and without
/// ATTACH).
#[test]
fn temp_writes_read_other_databases() {
    each(
        "tw",
        &[
            "CREATE TABLE m(x INTEGER PRIMARY KEY, y)
             INSERT INTO m VALUES (1, 'p'), (3, 'z')
             CREATE TEMP TABLE tt(k, v)
             INSERT INTO tt VALUES (1, 0), (2, 0), (3, 0)
             UPDATE tt SET v = (SELECT y FROM m WHERE x = tt.k)
             SELECT * FROM tt
             DELETE FROM tt WHERE k IN (SELECT x FROM main.m)
             SELECT * FROM tt
             INSERT INTO tt VALUES ((SELECT count(*) FROM m), 9)
             SELECT * FROM tt
             UPDATE tt SET v = m.y FROM m WHERE m.x = tt.k + 1
             SELECT * FROM tt
             UPDATE tt SET v = main.m.y FROM main.m WHERE main.m.x = 1 RETURNING *
             SELECT changes(), total_changes()
             INSERT INTO temp.tt VALUES ((SELECT max(x) FROM m), 'r') RETURNING k, v
             SELECT last_insert_rowid()",
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT)
             INSERT INTO m VALUES (1, 'p'), (2, 'q')
             CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE)
             INSERT INTO aux.t VALUES (1, 'P'), (2, 'Q'), (5, 'Z')
             CREATE TEMP TABLE tt(k UNIQUE, v)
             INSERT INTO tt VALUES (1, NULL), (2, NULL), (5, NULL)
             UPDATE tt SET v = (SELECT b FROM aux.t WHERE a = k) || (SELECT y FROM m WHERE x = k)
             SELECT * FROM tt
             UPDATE temp.tt SET v = aux.t.b FROM aux.t WHERE aux.t.a = temp.tt.k
             SELECT * FROM tt
             DELETE FROM tt WHERE k NOT IN (SELECT a FROM t WHERE b = 'q' OR b = 'p')
             SELECT * FROM tt
             INSERT INTO tt VALUES ((SELECT max(a) FROM aux.t), (SELECT group_concat(y) FROM m)) RETURNING k, v
             INSERT INTO tt(k) VALUES ((SELECT min(a) FROM aux.t)) ON CONFLICT DO NOTHING
             INSERT INTO tt(k) VALUES ((SELECT min(a) FROM aux.t)) ON CONFLICT(k) DO UPDATE SET v = (SELECT b FROM t WHERE a = 2)
             INSERT INTO tt VALUES ((SELECT min(a) FROM aux.t), 0)
             SELECT * FROM tt
             SELECT changes(), total_changes(), last_insert_rowid()
             BEGIN
             UPDATE tt SET v = (SELECT count(*) FROM aux.t)
             ROLLBACK
             SELECT * FROM tt
             BEGIN
             DELETE FROM tt WHERE k IN (SELECT x FROM m)
             SAVEPOINT s
             UPDATE tt SET v = 'sp' WHERE k > (SELECT min(a) FROM aux.t)
             ROLLBACK TO s
             COMMIT
             SELECT * FROM tt
             DETACH aux
             UPDATE tt SET v = (SELECT b FROM aux.t)
             SELECT * FROM tt",
        ],
    );
}

/// Item 4: attached views read beside other databases' tables;
/// same-named tables and three-part names in writes of an attached
/// table.
#[test]
fn views_and_names_across_databases() {
    each(
        "vn",
        &[
            "ATTACH '{d}/a.db' AS aux
             ATTACH '{d}/b.db' AS bb
             CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT)
             INSERT INTO m VALUES (1, 'p'), (2, 'q'), (3, 'r')
             CREATE VIEW v AS SELECT 'main-v' AS w
             CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE)
             INSERT INTO aux.t VALUES (1, 'P'), (2, 'Q'), (5, 'Z')
             CREATE VIEW aux.v AS SELECT a AS w FROM t WHERE a < 5
             CREATE VIEW aux.v2 AS WITH c AS (SELECT w FROM v) SELECT w * 10 AS w2 FROM c
             CREATE VIEW aux.only AS SELECT b FROM t
             CREATE TABLE bb.t(a, b)
             INSERT INTO bb.t VALUES (1, 'bb1')
             SELECT 1, * FROM aux.v, m WHERE w = x
             SELECT 2, * FROM m, aux.v2 ORDER BY 2, 3
             SELECT 3, * FROM v
             SELECT 4, x, (SELECT count(*) FROM aux.v WHERE w <= x) FROM m
             SELECT 5, * FROM only, m WHERE b = y
             SELECT 6, * FROM m WHERE y IN (SELECT b FROM aux.only)
             SELECT 7, * FROM aux.v JOIN bb.t ON w = t.a
             SELECT 8, * FROM (SELECT * FROM aux.v2) AS s, m WHERE s.w2 = x * 10
             WITH q AS (SELECT * FROM aux.v) SELECT 9, * FROM q, m WHERE q.w = m.x
             SELECT 10, * FROM aux.v UNION ALL SELECT 10, x FROM m
             SELECT 11, * FROM v, aux.v
             INSERT INTO m(y) SELECT b FROM aux.only
             SELECT 12, * FROM m
             CREATE TABLE aux.m(x, y)
             INSERT INTO aux.m VALUES (1, 'aux1'), (2, 'aux2')
             UPDATE aux.m SET y = (SELECT y FROM m WHERE m.x = aux.m.x) || '/' || (SELECT count(*) FROM aux.m)
             SELECT 13, * FROM aux.m
             UPDATE aux.m SET y = main.m.y FROM main.m WHERE main.m.x = aux.m.x + 1
             SELECT 14, * FROM aux.m
             DELETE FROM aux.m WHERE x IN (SELECT x FROM m WHERE y = 'q')
             SELECT 15, * FROM aux.m
             INSERT INTO aux.m SELECT main.m.x, main.m.y FROM main.m WHERE main.m.x > 3
             SELECT 16, * FROM aux.m
             UPDATE aux.t SET b = bb.t.b FROM bb.t WHERE bb.t.a = aux.t.a
             SELECT 17, * FROM aux.t
             UPDATE aux.m SET y = 'x' FROM aux.m
             SELECT 18, changes(), total_changes()",
        ],
    );
}

/// The pin's view of `stmts`' rows and failing statements (no file
/// check: some tests read deliberately corrupt files).
fn same_rows(tag: &str, prep: &dyn Fn(&Path), stmts: &[String]) {
    let h = fresh_dir(&format!("{tag}-h"));
    let o = fresh_dir(&format!("{tag}-o"));
    prep(&h);
    prep(&o);
    let (hl, he) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    // A value with newlines is several of the shell's lines.
    let hl: Vec<String> = hl
        .iter()
        .flat_map(|l| l.split('\n'))
        .map(str::to_string)
        .collect();
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ:\n{script}");
    assert_eq!(he, oe, "{tag}: failing statements differ:\n{script}");
}

/// A file whose table breaks its (rewritten) NOT NULL and CHECK
/// constraints and whose index misses a row, made by the pin.
fn corrupt(path: &Path) {
    let out = std::process::Command::new(common::oracle())
        .arg("-cmd")
        .arg(".dbconfig defensive off")
        .arg(path)
        .arg(
            "CREATE TABLE t(x, y); INSERT INTO t VALUES (NULL, 1), (2, 2), (3, NULL); \
             CREATE INDEX ti ON t(y); PRAGMA writable_schema = ON; \
             UPDATE sqlite_schema SET sql = 'CREATE TABLE t(x NOT NULL, y CHECK(y > 1))' WHERE name = 't'; \
             UPDATE sqlite_schema SET sql = 'CREATE INDEX ti ON t(x)' WHERE name = 'ti';",
        )
        .output()
        .expect("oracle runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Item 6: the integrity check over every database.
#[test]
fn integrity_check_every_database() {
    let prep = |d: &Path| {
        corrupt(&d.join("a.db"));
        corrupt(&d.join("b.db"));
        common::cli(
            &d.join("main.db"),
            "CREATE TABLE q(x NOT NULL); INSERT INTO q VALUES (1);",
        );
    };
    let script = lines(
        "PRAGMA integrity_check
         ATTACH '{d}/a.db' AS aux
         PRAGMA integrity_check
         ATTACH '{d}/b.db' AS bb
         CREATE TEMP TABLE tt(z NOT NULL)
         INSERT INTO tt VALUES (1)
         PRAGMA integrity_check
         PRAGMA integrity_check(1)
         PRAGMA integrity_check(2)
         PRAGMA integrity_check(4)
         PRAGMA integrity_check(5)
         PRAGMA integrity_check(-3)
         PRAGMA quick_check
         PRAGMA quick_check(3)
         PRAGMA integrity_check(t)
         PRAGMA integrity_check('q')
         PRAGMA aux.integrity_check
         PRAGMA bb.quick_check(2)
         PRAGMA main.integrity_check
         PRAGMA temp.integrity_check
         BEGIN
         PRAGMA integrity_check
         DETACH bb
         COMMIT
         DETACH bb
         PRAGMA integrity_check
         DETACH aux
         PRAGMA integrity_check",
    );
    same_rows("ic", &prep, &script);
    // foreign_key_check: unqualified, main only.
    let fk = lines(
        "PRAGMA foreign_keys = OFF
         ATTACH '{d}/a.db' AS aux
         CREATE TABLE p(id INTEGER PRIMARY KEY)
         CREATE TABLE c(x REFERENCES p(id))
         INSERT INTO c VALUES (5)
         CREATE TABLE aux.p(id INTEGER PRIMARY KEY)
         CREATE TABLE aux.c2(x REFERENCES p(id))
         INSERT INTO aux.c2 VALUES (7)
         CREATE TEMP TABLE tp(id INTEGER PRIMARY KEY)
         CREATE TEMP TABLE tc(x REFERENCES tp(id))
         INSERT INTO tc VALUES (9)
         PRAGMA foreign_key_check
         PRAGMA foreign_key_check(c2)
         PRAGMA foreign_key_check(tc)
         PRAGMA aux.foreign_key_check
         PRAGMA temp.foreign_key_check
         PRAGMA integrity_check
         PRAGMA quick_check",
    );
    same("fk", &fk);
}

/// Regressions found while building the unit.
#[test]
fn regressions() {
    each(
        "rg",
        &[
            // `aux.v` read main's view `v` when main had one (beside a
            // main table, so the statement ran on main).
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE m(x)
             INSERT INTO m VALUES (1)
             CREATE VIEW v AS SELECT 'main' AS s
             CREATE TABLE aux.t(y)
             INSERT INTO aux.t VALUES ('aux')
             CREATE VIEW aux.v AS SELECT y AS s FROM t
             SELECT * FROM aux.v, m
             SELECT * FROM v, m
             SELECT (SELECT s FROM aux.v) FROM m",
            // A string name the router's cut left without its dot.
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE aux.z(q)
             INSERT INTO aux.'z' VALUES (1)
             INSERT INTO 'aux'.'z' VALUES (2)
             SELECT 'aux'.'z'.q, \"aux\".z.q FROM 'aux'.z ORDER BY 1",
            // An attached database's own three-part name stays its own
            // beside a same-named table of main.
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE m(x, y)
             INSERT INTO m VALUES (1, 'main1'), (2, 'main2')
             CREATE TABLE aux.m(x, y)
             INSERT INTO aux.m VALUES (2, 'aux2'), (1, 'aux1')
             UPDATE aux.m SET y = (SELECT m.y FROM m WHERE m.x = aux.m.x)
             SELECT * FROM aux.m ORDER BY x
             UPDATE aux.m SET y = 'u' FROM main.m WHERE main.m.x = aux.m.x AND aux.m.x = 1
             SELECT * FROM aux.m ORDER BY x",
        ],
    );
}

/// Review fixes: a WITH clause lists its CTEs' names before the
/// write's target, which the router took as the target (the wrong
/// database's same-named table was written, or main's target looked
/// up in temp); a WITH-prefixed, upsert or RETURNING TEMP INSERT ...
/// SELECT reading main; CTE / trigger / SET / constraint column names
/// as strings.
#[test]
fn with_prefixed_writes() {
    each(
        "wf",
        &[
            // No ATTACH: main's write whose CTE reads temp.
            "CREATE TABLE m(x)
             INSERT INTO m VALUES (1), (2), (3)
             CREATE TEMP TABLE t(y)
             INSERT INTO t VALUES (2)
             CREATE TEMP TABLE m(x)
             INSERT INTO temp.m VALUES (2), (7)
             WITH c AS (SELECT y FROM temp.t) DELETE FROM main.m WHERE x IN (SELECT y FROM c)
             SELECT 'main', * FROM main.m
             SELECT 'temp', * FROM temp.m
             WITH c AS (SELECT y FROM temp.t) UPDATE main.m SET x = x + 100
             SELECT 'main', * FROM main.m
             SELECT 'temp', * FROM temp.m",
            "CREATE TABLE m(x PRIMARY KEY, y)
             INSERT INTO m VALUES (1, 'a'), (2, 'b'), (3, 'c')
             CREATE TEMP TABLE t(y)
             INSERT INTO t VALUES (2)
             WITH c AS (SELECT y FROM temp.t) UPDATE m SET y = 'w' WHERE x IN (SELECT y FROM c)
             SELECT * FROM m
             WITH c AS (SELECT y FROM temp.t) DELETE FROM m WHERE x IN (SELECT y FROM c)
             SELECT * FROM m
             WITH c AS (SELECT y FROM temp.t) INSERT INTO m VALUES ((SELECT y + 50 FROM c), 'v')
             SELECT * FROM m
             WITH c AS (SELECT x FROM m) UPDATE t SET y = (SELECT max(x) FROM c)
             SELECT * FROM t
             WITH c AS (SELECT x FROM m WHERE x = 52) DELETE FROM t WHERE y IN (SELECT x - 49 FROM c)
             SELECT count(*) FROM t
             WITH c AS (SELECT x FROM m) INSERT INTO t SELECT x + 500 FROM c
             INSERT INTO t SELECT x + 600 FROM m RETURNING y
             SELECT * FROM t ORDER BY y",
            // A same-named CTE never names the target.
            "CREATE TABLE m(x)
             INSERT INTO m VALUES (1), (2)
             CREATE TEMP TABLE t(y)
             WITH m AS (SELECT 1 AS x) DELETE FROM m WHERE x IN (SELECT x FROM m)
             SELECT * FROM m",
            // ATTACH: an attached table's write whose CTE reads temp.
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE aux.m(x)
             INSERT INTO aux.m VALUES (1), (2), (3)
             CREATE TEMP TABLE t(y)
             INSERT INTO t VALUES (2)
             CREATE TEMP TABLE m(x)
             INSERT INTO temp.m VALUES (2), (7)
             WITH c AS (SELECT y FROM temp.t) DELETE FROM aux.m WHERE x IN (SELECT y FROM c)
             SELECT 'aux', * FROM aux.m
             SELECT 'temp', * FROM temp.m
             WITH c AS (SELECT y FROM temp.t) UPDATE aux.m SET x = x + 100
             SELECT 'aux', * FROM aux.m
             SELECT 'temp', * FROM temp.m",
            // A CTE reading another database (pre-existing).
            "ATTACH '{d}/a.db' AS aux
             CREATE TABLE m(x)
             INSERT INTO m VALUES (1), (2), (3)
             CREATE TABLE aux.m(x)
             INSERT INTO aux.m VALUES (10), (2)
             CREATE TABLE aux.n(v)
             INSERT INTO aux.n VALUES (2)
             WITH c AS (SELECT v FROM aux.n) DELETE FROM main.m WHERE x IN (SELECT v FROM c)
             SELECT 'aux', * FROM aux.m
             SELECT 'main', * FROM main.m
             WITH c AS (SELECT v FROM aux.n) UPDATE m SET x = x + 100
             SELECT 'aux', * FROM aux.m
             SELECT 'main', * FROM main.m
             WITH c AS (SELECT x FROM main.m) DELETE FROM aux.m WHERE x IN (SELECT x - 99 FROM c)
             SELECT 'aux', * FROM aux.m
             WITH c AS (SELECT x FROM m) UPDATE aux.m SET x = x + (SELECT max(x) FROM c)
             SELECT 'aux', * FROM aux.m
             SELECT 'main', * FROM main.m",
            // String names in the remaining `nm` positions.
            "CREATE TABLE t(a PRIMARY KEY, b, UNIQUE('b'))
             INSERT INTO t VALUES (1, 2)
             INSERT INTO t VALUES (5, 2)
             UPDATE 't' SET 'b' = 5
             INSERT INTO t VALUES (1, 0) ON CONFLICT(a) DO UPDATE SET 'b' = 7
             CREATE TABLE p(id PRIMARY KEY)
             CREATE TABLE ch(x REFERENCES 'p'('id'), FOREIGN KEY('x') REFERENCES p('id'))
             CREATE TRIGGER 'tr' AFTER UPDATE OF 'b' ON 't' BEGIN INSERT INTO p VALUES (new.b); END
             UPDATE t SET b = 9
             SELECT * FROM p
             WITH 'c'('y') AS (SELECT 1) SELECT y FROM c
             WITH RECURSIVE 'r'('n') AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT * FROM r
             DROP TRIGGER 'tr'
             SELECT * FROM t",
        ],
    );
}

/// One random script of WITH-prefixed (and plain) writes over main,
/// temp and an attached database whose tables share names.
fn random_with_script(rng: &mut Rng) -> Vec<String> {
    let att = rng.int(10) < 7;
    let mut s = Vec::new();
    if att {
        s.push("ATTACH '{d}/a.db' AS aux".to_string());
    }
    let mut dbs = vec!["main", "temp"];
    for (sql, aux) in [
        (
            "CREATE TABLE m(x)|INSERT INTO m VALUES (1), (2), (3)",
            false,
        ),
        (
            "CREATE TABLE aux.m(x)|INSERT INTO aux.m VALUES (10), (2)",
            true,
        ),
        (
            "CREATE TEMP TABLE m(x)|INSERT INTO temp.m VALUES (2), (7)",
            false,
        ),
        (
            "CREATE TEMP TABLE t(y)|INSERT INTO t VALUES (2), (3)",
            false,
        ),
        (
            "CREATE TABLE aux.n(v)|INSERT INTO aux.n VALUES (2), (10)",
            true,
        ),
        ("CREATE TABLE k(v)|INSERT INTO k VALUES (3)", false),
    ] {
        if (aux && !att) || rng.int(100) >= 85 {
            continue;
        }
        s.extend(sql.split('|').map(str::to_string));
    }
    if att {
        dbs.push("aux");
    }
    let col = |n: &str| match n.rsplit('.').next() {
        Some("t") => "y",
        Some("n") | Some("k") => "v",
        _ => "x",
    };
    let srcs = [
        "temp.t", "t", "aux.n", "n", "main.m", "m", "temp.m", "aux.m", "k", "main.k",
    ];
    let tgts = [
        "main.m", "m", "temp.m", "aux.m", "temp.t", "t", "aux.n", "k",
    ];
    for _ in 0..4 {
        let src = rng.pick(&srcs);
        let tg = rng.pick(&tgts);
        let (tc, sc) = (col(tg), col(src));
        let body = match rng.int(7) {
            0 => format!("DELETE FROM {tg} WHERE {tc} IN (SELECT z FROM c)"),
            1 => format!("UPDATE {tg} SET {tc} = {tc} + 100 WHERE {tc} IN (SELECT z FROM c)"),
            2 => format!("UPDATE {tg} SET {tc} = (SELECT max(z) FROM c)"),
            3 => format!("INSERT INTO {tg} VALUES ((SELECT min(z) + 50 FROM c))"),
            4 => format!("DELETE FROM {tg} WHERE {tc} IN (SELECT z FROM c) RETURNING {tc}"),
            5 => format!("INSERT INTO {tg} SELECT z + 700 FROM c RETURNING {tc}"),
            _ => format!("INSERT INTO {tg} SELECT z + 500 FROM c"),
        };
        s.push(if rng.int(100) < 35 {
            body.replace("FROM c", &format!("FROM (SELECT {sc} AS z FROM {src})"))
        } else {
            format!("WITH c AS (SELECT {sc} AS z FROM {src}) {body}")
        });
        for d in dbs.iter() {
            for t in ["m", "t", "n", "k"] {
                s.push(format!("SELECT '{d}.{t}', * FROM {d}.{t} ORDER BY 2"));
            }
        }
    }
    s
}

#[test]
fn generated_with_writes() {
    let n: u64 = std::env::var("HIPP_ATTACH_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let mut rng = Rng(0x0217_c7e5_9a4d_6b13);
    for i in 0..n {
        let script = random_with_script(&mut rng);
        same(&format!("wgen{i}"), &script);
    }
}

/// Review fixes: `PRAGMA aux.integrity_check` over a B-tree-corrupt
/// attached file reports its rows before the error (they were
/// dropped); an unqualified check opens the temp database, as the
/// pin's `sqlite3CodeVerifySchema` does (`database_list` lists it).
#[test]
fn integrity_check_rows_and_temp() {
    let prep = |d: &Path| {
        let p = d.join("a.db");
        common::cli(
            &p,
            "CREATE TABLE t(a, b); CREATE INDEX i ON t(a); \
             WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 300) \
             INSERT INTO t SELECT n, printf('%.200c', 'x') FROM r;",
        );
        let mut b = std::fs::read(&p).expect("read");
        b[9 * 4096] = 0;
        b[6 * 4096 + 3] = 0x7f;
        b[6 * 4096 + 4] = 0xff;
        std::fs::write(&p, b).expect("write");
    };
    let script = lines(
        "ATTACH '{d}/a.db' AS aux
         PRAGMA aux.integrity_check
         PRAGMA aux.quick_check
         PRAGMA aux.quick_check(2)
         PRAGMA integrity_check
         SELECT name FROM pragma_database_list",
    );
    same_rows("icrows", &prep, &script);
    each(
        "ictemp",
        &[
            "PRAGMA integrity_check
             SELECT name FROM pragma_database_list
             ATTACH '{d}/a.db' AS a
             SELECT name FROM pragma_database_list",
            "PRAGMA main.quick_check
             SELECT name FROM pragma_database_list
             CREATE TABLE x(a)
             PRAGMA quick_check(x)
             SELECT name FROM pragma_database_list",
        ],
    );
}

/// What hipp still refuses cleanly where the pin answers (STATUS.md's
/// v0.17 honest gaps): a TEMP trigger on a main or attached table, one
/// writing a schema-qualified table, and an attached database's write
/// reading another database's view.
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
                 CREATE TABLE m(y)
                 CREATE VIEW mv AS SELECT y FROM m
                 CREATE TEMP TABLE tt(z)",
            ),
        )
        .await;
        assert!(errs.is_empty(), "{errs:?}");
        for sql in [
            "CREATE TEMP TRIGGER tg AFTER INSERT ON a.t BEGIN SELECT 1; END",
            "CREATE TEMP TRIGGER tm AFTER INSERT ON m BEGIN SELECT 1; END",
            "CREATE TEMP TRIGGER tq AFTER INSERT ON tt BEGIN INSERT INTO main.m VALUES (1); END",
            "INSERT INTO a.t SELECT y FROM mv",
        ] {
            let r = db.prepare(sql).await.map(|_| ());
            assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
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

/// One random script over three databases and TEMP: same-named
/// tables (`t` in main and `a`), views in `a`, TEMP writes reading
/// main and `a`, writes of `a` reading main by unqualified and
/// three-part names, string-literal qualifiers. Every query orders its
/// rows by a unique key.
fn random_script(rng: &mut Rng) -> Vec<String> {
    let mut s = vec![
        "ATTACH '{d}/a.db' AS a".to_string(),
        "CREATE TABLE t(k INTEGER PRIMARY KEY, v, w)".to_string(),
        "CREATE TABLE a.t(k INTEGER PRIMARY KEY, v, w)".to_string(),
        "CREATE TABLE a.u(k INTEGER PRIMARY KEY, v, w)".to_string(),
        "CREATE VIEW a.av AS SELECT k, v + 1 AS v, w FROM t".to_string(),
        "CREATE VIEW a.uv AS SELECT u.k, u.v, t.w FROM u JOIN t USING (k)".to_string(),
        "CREATE TEMP TABLE tt(k INTEGER PRIMARY KEY, v, w)".to_string(),
    ];
    for k in 0..6 {
        s.push(format!("INSERT INTO t VALUES ({k}, {}, 'm{k}')", k % 3));
        s.push(format!(
            "INSERT INTO a.t VALUES ({k}, {}, 'a{k}')",
            (k + 1) % 4
        ));
        s.push(format!("INSERT INTO a.u VALUES ({}, {k}, 'u{k}')", k * 2));
        s.push(format!("INSERT INTO tt VALUES ({k}, {k}, NULL)"));
    }
    let reads = [
        "t", "main.t", "a.t", "'a'.t", "a.u", "u", "a.av", "av", "uv", "tt", "temp.tt",
    ];
    // What a TEMP write reads: no view of another database (refused,
    // STATUS.md's gaps).
    let tables = ["t", "main.t", "a.t", "'a'.t", "a.u", "u", "tt", "temp.tt"];
    let writes_a = ["a.t", "a.u", "'a'.u"];
    let writes_t = ["tt", "temp.tt"];
    for _ in 0..20 + rng.int(20) {
        let r = rng.pick(&reads);
        let rt = rng.pick(&tables);
        let k = rng.int(8);
        let v = rng.int(4);
        let stmt = match rng.int(12) {
            0 => format!(
                "UPDATE {} SET w = (SELECT max(w) FROM {rt} WHERE v = {v}) WHERE k % 3 = {}",
                rng.pick(&writes_t),
                k % 3
            ),
            1 => format!(
                "DELETE FROM {} WHERE k IN (SELECT k FROM {rt} WHERE v = {v})",
                rng.pick(&writes_t)
            ),
            2 => format!(
                "INSERT OR REPLACE INTO {} VALUES ({k}, (SELECT count(*) FROM {rt}), (SELECT min(w) FROM {rt}))",
                rng.pick(&writes_t)
            ),
            3 => format!(
                "UPDATE {x} SET w = (SELECT max(w) FROM t WHERE t.k = {x}.k) WHERE v > {v}",
                x = rng.pick(&writes_a)
            ),
            4 => format!(
                "UPDATE a.t SET w = main.t.w || 'x' FROM main.t WHERE main.t.k = a.t.k AND main.t.v = {v}"
            ),
            5 => format!(
                "INSERT OR IGNORE INTO {} SELECT k + {k}, v, w FROM {r}",
                rng.pick(&writes_a)
            ),
            6 => format!(
                "SELECT p.k, p.v, q.w FROM {r} AS p JOIN {} AS q ON p.v = q.v ORDER BY p.k, q.k",
                rng.pick(&reads)
            ),
            7 => format!("SELECT * FROM {r} ORDER BY k"),
            8 => format!(
                "SELECT k, (SELECT count(*) FROM {r} AS q WHERE q.k <= p.k) FROM tt AS p ORDER BY k"
            ),
            9 => rng.pick(&["BEGIN", "SAVEPOINT s"]).to_string(),
            10 => rng
                .pick(&["COMMIT", "ROLLBACK", "ROLLBACK TO s", "RELEASE s"])
                .to_string(),
            _ => format!(
                "UPDATE tt SET w = q.w FROM {rt} AS q WHERE q.k = tt.k + {}",
                k % 2
            ),
        };
        s.push(stmt);
    }
    s.push("COMMIT".to_string());
    for t in ["main.t", "a.t", "a.u", "temp.tt"] {
        s.push(format!("SELECT * FROM {t} ORDER BY k"));
    }
    s.push("PRAGMA integrity_check".to_string());
    s
}

#[test]
fn generated_cross_database_scripts() {
    let n: u64 = std::env::var("HIPP_ATTACH_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let mut rng = Rng(0x5eed_0a77_17c4_93b1);
    for i in 0..n {
        let script = random_script(&mut rng);
        same(&format!("gen{i}"), &script);
    }
}
