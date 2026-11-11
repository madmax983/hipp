//! v0.17 review fix (wave 15, `sync_readers` review): table-valued
//! functions in statements that write or read TEMP tables,
//! differentially vs the pinned oracle (SQLite 3.54.1, live).
//!
//! hipp's temp database is a second, private connection. Before the
//! fix it knew no table-valued function main resolves, so
//! - a TEMP write (INSERT ... SELECT, UPDATE / DELETE with a
//!   subquery, INSERT VALUES((subquery)), CREATE TEMP TABLE ... AS,
//!   a CTE feeding the INSERT) or a query over temp tables reading
//!   `generate_series` failed (`UnknownFunction`), and one reading
//!   `dbstat` failed (`UnknownTable`) — the pin runs them all;
//! - one reading a pragma function that names a main table
//!   (`pragma_table_info('m')`) silently read nothing.
//!
//! Now the temp database computes `generate_series` itself (it reads
//! no file), and `dbstat` / pragma functions are main's (the
//! statement runs on main, reading the temp tables there, or main
//! feeds the temp write) — but a pragma function whose lone argument
//! names a temp object stays the temp database's (the pin looks in
//! temp first). A temp UPDATE / DELETE (or INSERT VALUES) reading
//! main stays refused (`Unsupported`, as for main tables:
//! `refusals_stay_refusals`), never a wrong answer.

#![cfg(feature = "sql")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};

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

/// The pin's shell over stdin, one statement per line: stdout rows,
/// and the lines its errors name.
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

/// Run `stmts` on twin fresh files; rows, failing statements and the
/// final files (the pin's dump and integrity check) must agree.
fn same(tag: &str, stmts: &[String]) {
    let h = common::temp_db(&format!("v17tt-{tag}-h"));
    let o = common::temp_db(&format!("v17tt-{tag}-o"));
    let (hl, he) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ:\n{script}");
    assert_eq!(he, oe, "{tag}: failing statements differ:\n{script}");
    let hi = common::cli(&h, "PRAGMA integrity_check;");
    assert_eq!(hi.trim(), "ok", "{tag}: hipp's file\n{script}");
    assert_eq!(
        common::cli(&h, ".dump"),
        common::cli(&o, ".dump"),
        "{tag}: final files differ:\n{script}"
    );
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

/// The review's repros and their neighbours: `generate_series` in
/// every TEMP write shape, and in queries over temp tables.
#[cfg(feature = "vtab")]
#[test]
fn generate_series_feeds_temp_writes() {
    same(
        "series",
        &lines(
            "CREATE TABLE m(x, y);
             INSERT INTO m VALUES(1, 'a'), (3, 'c');
             CREATE TEMP TABLE tt(a, b);
             INSERT INTO tt VALUES(1, NULL), (2, 'x');
             DELETE FROM tt WHERE a IN (SELECT value FROM generate_series(2,2));
             SELECT * FROM tt;
             SELECT count(*) FROM generate_series(1,3);
             INSERT INTO tt SELECT value, 's' FROM generate_series(1,3);
             SELECT rowid, * FROM tt;
             UPDATE tt SET b = (SELECT max(value) FROM generate_series(1,a+1)) WHERE b = 's';
             SELECT * FROM tt;
             INSERT INTO tt VALUES((SELECT sum(value) FROM generate_series(1,4)), 'v');
             INSERT INTO tt SELECT value, 'w' FROM generate_series(7,9) WHERE value <> 8 ORDER BY value DESC;
             WITH c(v) AS (SELECT value FROM generate_series(10,11)) INSERT INTO tt SELECT v, 'cte' FROM c;
             WITH c(v) AS (SELECT value FROM generate_series(3,4)) UPDATE tt SET b = 'u' WHERE a IN (SELECT v FROM c);
             INSERT INTO tt(a) SELECT value FROM generate_series(20,21) WHERE 1 ON CONFLICT DO NOTHING;
             INSERT INTO tt(a) SELECT value FROM generate_series(30,31) RETURNING a, b;
             INSERT INTO tt(a) SELECT value FROM generate_series WHERE start = 40 AND stop = 41;
             INSERT INTO tt(a) SELECT value FROM generate_series(50) LIMIT 2;
             INSERT INTO tt(a) SELECT value FROM main.generate_series(60,60);
             SELECT rowid, * FROM tt ORDER BY rowid;
             SELECT a, value FROM tt, generate_series(1,2) WHERE a < 3 ORDER BY a, value;
             SELECT a, (SELECT count(*) FROM generate_series(1,a)) FROM tt WHERE a < 12 ORDER BY a;
             SELECT * FROM tt JOIN generate_series(1,4) s ON s.value = tt.a ORDER BY a;
             INSERT INTO tt SELECT value, y FROM generate_series(1,3) JOIN m ON x = value;
             SELECT * FROM tt WHERE b IN ('a','c');
             DELETE FROM tt WHERE NOT EXISTS (SELECT 1 FROM generate_series(1,9) WHERE value = a);
             SELECT count(*), sum(a) FROM tt;
             CREATE TEMP TABLE g AS SELECT value AS v, value * value AS sq FROM generate_series(1,4);
             SELECT * FROM g;
             SELECT typeof(v), typeof(sq) FROM g LIMIT 1;
             CREATE TEMP TABLE IF NOT EXISTS g AS SELECT value FROM generate_series(1,9);
             SELECT count(*) FROM g;
             CREATE TEMP TABLE e AS SELECT value FROM generate_series(5,1);
             SELECT count(*) FROM e;
             INSERT INTO tt(a) SELECT value FROM generate_series(NULL,3);
             INSERT INTO tt(a) SELECT value FROM generate_series(1,3,0);
             INSERT INTO tt(a) SELECT value FROM generate_series(1,2,3,4);
             SELECT count(*) FROM tt",
        ),
    );
}

/// TEMP views over `generate_series` / `dbstat` / pragma functions,
/// and temp tables shadowing the module's name.
#[cfg(feature = "vtab")]
#[test]
fn temp_views_and_shadowing() {
    same(
        "views",
        &lines(
            "CREATE TABLE m(x);
             INSERT INTO m VALUES(2);
             CREATE TEMP VIEW v AS SELECT value FROM generate_series(1,3);
             SELECT * FROM v;
             CREATE TEMP VIEW w AS SELECT name FROM dbstat ORDER BY name;
             SELECT * FROM w;
             CREATE TEMP VIEW u AS SELECT name FROM pragma_table_info('m');
             SELECT * FROM u;
             SELECT * FROM v JOIN m ON x = value;
             CREATE TEMP TABLE tt(a);
             INSERT INTO tt SELECT * FROM v;
             SELECT * FROM tt;
             SELECT * FROM tt, v ORDER BY 1, 2;
             CREATE TEMP VIEW z AS SELECT a FROM tt, generate_series(1,2);
             SELECT * FROM z ORDER BY a;
             CREATE TEMP TABLE generate_series(value);
             INSERT INTO generate_series VALUES(99);
             SELECT * FROM generate_series;
             INSERT INTO tt SELECT value FROM generate_series;
             SELECT a FROM tt WHERE a = 99;
             DROP TABLE temp.generate_series;
             SELECT value FROM generate_series(1,2);
             SELECT a, value FROM tt, json_each('[5]') WHERE a = 1",
        ),
    );
}

/// `dbstat` (main's file) into TEMP writes and beside temp tables.
#[cfg(feature = "vtab")]
#[test]
fn dbstat_feeds_temp_writes() {
    same(
        "dbstat",
        &lines(
            "CREATE TABLE m(x, y);
             INSERT INTO m VALUES(1, 2), (3, 4);
             CREATE INDEX mi ON m(y);
             CREATE TEMP TABLE tt(a, b);
             INSERT INTO tt SELECT name, pageno FROM dbstat ORDER BY name;
             SELECT * FROM tt;
             INSERT INTO tt SELECT count(*), 0 FROM dbstat;
             INSERT INTO tt(a) SELECT name FROM dbstat('main') WHERE name = 'mi';
             SELECT * FROM tt ORDER BY a;
             CREATE TEMP TABLE d AS SELECT name, pageno FROM dbstat ORDER BY name;
             SELECT * FROM d;
             SELECT d.name, s.pageno FROM d JOIN dbstat s ON s.name = d.name ORDER BY 1;
             SELECT (SELECT count(*) FROM dbstat) > 0, count(*) FROM d",
        ),
    );
    // The review's last repro: an empty main.
    same(
        "dbstat-empty",
        &lines(
            "CREATE TEMP TABLE tt(a);
             INSERT INTO tt SELECT count(*) FROM dbstat;
             SELECT * FROM tt",
        ),
    );
}

/// Pragma functions: one naming a main table is main's (was: read
/// nothing in a TEMP write); one naming a temp object stays the temp
/// database's (temp first, as on the pin).
#[test]
fn pragma_functions_with_temp_tables() {
    same(
        "pragma",
        &lines(
            "CREATE TABLE m(x, y);
             CREATE INDEX mi ON m(y);
             CREATE TEMP TABLE tt(a, b, c);
             INSERT INTO tt(a) SELECT name FROM pragma_table_info('m');
             SELECT a FROM tt;
             INSERT INTO tt(a) SELECT name FROM pragma_index_list('m');
             INSERT INTO tt(a) SELECT name FROM pragma_table_info('tt');
             SELECT a FROM tt;
             SELECT a, name FROM tt, pragma_table_info('m') WHERE a = 'x' ORDER BY 2;
             SELECT a, name FROM tt, pragma_table_info('tt') WHERE a = 'y' ORDER BY 2;
             CREATE TEMP TABLE p AS SELECT cid, name FROM pragma_table_info('m');
             SELECT * FROM p;
             UPDATE tt SET b = (SELECT count(*) FROM pragma_table_info('tt'));
             UPDATE tt SET c = (SELECT count(*) FROM pragma_index_list('tt'));
             DELETE FROM tt WHERE a NOT IN (SELECT name FROM pragma_table_info('tt'));
             SELECT * FROM tt;
             CREATE TEMP TABLE m(q);
             INSERT INTO tt(a) SELECT name FROM pragma_table_info('m');
             SELECT a FROM tt WHERE a = 'q';
             SELECT name FROM pragma_table_info('m')",
        ),
    );
}

/// What stays refused, never a wrong answer: a temp UPDATE / DELETE /
/// INSERT VALUES reading main through `dbstat` or a pragma function
/// naming a main table (as for main tables; STATUS.md's v0.16 TEMP
/// gaps). Each was a wrong error (`dbstat`) or read nothing (the
/// pragma function).
#[test]
fn refusals_stay_refusals() {
    let p = common::temp_db("v17tt-refused");
    common::cli(&p, "CREATE TABLE m(a); INSERT INTO m VALUES (1);");
    block_on(async {
        let mut db = open(&p).await;
        let mut q = db.prepare("CREATE TEMP TABLE x(a)").await.expect("prep");
        q.step().await.expect("create");
        drop(q);
        let mut refused = vec![
            "UPDATE x SET a = (SELECT count(*) FROM pragma_table_info('m'))",
            "DELETE FROM x WHERE a IN (SELECT cid FROM pragma_table_info('m'))",
            "INSERT INTO x VALUES((SELECT count(*) FROM pragma_table_info('m')))",
        ];
        if cfg!(feature = "vtab") {
            refused.extend([
                "UPDATE x SET a = (SELECT count(*) FROM dbstat)",
                "DELETE FROM x WHERE a IN (SELECT pageno FROM dbstat)",
            ]);
        }
        for sql in refused {
            let r = db.prepare(sql).await.map(|_| ());
            assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
        }
    });
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
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

/// Generated TEMP writes and queries over `generate_series` with
/// random bounds, steps (negative, zero, NULL) and filters.
#[cfg(feature = "vtab")]
#[test]
fn generated_series_corpus() {
    let mut rng = Rng(0x7e3b_51a9_04c2_d18f);
    for round in 0..6 {
        let mut s = vec![
            "CREATE TABLE m(x)".to_string(),
            "INSERT INTO m VALUES(1),(4),(9)".to_string(),
            "CREATE TEMP TABLE tt(a, b)".to_string(),
        ];
        for _ in 0..14 {
            let lo = rng.int(-5, 12);
            let hi = rng.int(-5, 15);
            let step = match rng.int(0, 5) {
                0 => "NULL".to_string(),
                1 => "0".to_string(),
                n => rng.int(-3, 3).max(n - 4).to_string(),
            };
            let k = rng.int(0, 9);
            s.push(match rng.int(0, 7) {
                0 => format!(
                    "INSERT INTO tt SELECT value, {k} FROM generate_series({lo},{hi},{step})"
                ),
                1 => format!(
                    "DELETE FROM tt WHERE a IN (SELECT value FROM generate_series({lo},{hi},{step}))"
                ),
                2 => format!(
                    "UPDATE tt SET b = (SELECT count(*) FROM generate_series({lo},a)) WHERE a % 3 = {}",
                    k % 3
                ),
                3 => format!(
                    "INSERT INTO tt VALUES((SELECT total(value) FROM generate_series({lo},{hi})), {k})"
                ),
                4 => format!(
                    "SELECT a, b, value FROM tt JOIN generate_series({lo},{hi}) ON value = a ORDER BY a, b, value"
                ),
                5 => format!(
                    "INSERT INTO tt SELECT value, x FROM generate_series({lo},{hi},{step}) JOIN m ON x = value"
                ),
                6 => format!(
                    "WITH c AS (SELECT value FROM generate_series({lo},{hi})) INSERT INTO tt SELECT value, -1 FROM c WHERE value % 2 = {}",
                    k % 2
                ),
                _ => "SELECT count(*), total(a), total(b) FROM tt".to_string(),
            });
        }
        s.push("SELECT rowid, a, b FROM tt ORDER BY rowid".to_string());
        same(&format!("gen{round}"), &s);
    }
}
