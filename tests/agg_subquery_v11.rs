//! v0.11 subqueries inside aggregate arguments,
//! differentially vs the pinned oracle (SQLite 3.54.1).
//! Semantics probed on the pin before implementation; the
//! rules the tests encode:
//!
//! - An aggregate argument containing a subquery is evaluated
//!   PER GROUP ROW: the subquery executes against each row of
//!   the group in turn (correlated references bind that row,
//!   including a no-FROM subquery reading the row's own
//!   columns), and the aggregate folds the per-row values.
//!   An uncorrelated subquery therefore contributes the same
//!   value once per row.
//! - A subquery that yields no row for a group row
//!   contributes NULL for that row (aggregates skip NULLs, so
//!   `count((SELECT 1 WHERE ...))` counts only matching rows).
//! - Subqueries compose everywhere inside an argument:
//!   arithmetic mixing plain columns and subqueries, CASE,
//!   IN / EXISTS forms, nested aggregates inside the
//!   subquery, and compound subquery bodies (first row wins).
//! - DISTINCT folds over the per-row values; group_concat's
//!   separator argument is likewise per-row.
//! - A lone min/max over a subquery argument keeps the
//!   bare-column rule: bare columns read the extremum row
//!   (by the per-row subquery value), exactly as for a plain
//!   argument.
//! - Subqueries OUTSIDE the aggregate in the same expression
//!   still resolve against the group's representative row.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Result<(), S::Error>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
    Ok(())
}

async fn collect<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Vec<String>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await.expect("prepare");
    let mut rows = Vec::new();
    while q.step().await.expect("step") == Step::Row {
        let mut line = String::new();
        for i in 0..q.column_count() {
            if i > 0 {
                line.push('|');
            }
            match q.column_value(i) {
                Some(hipp::Value::Null) | None => {}
                Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
                Some(hipp::Value::Real(f)) => {
                    let s = format!("{f}");
                    line.push_str(&s);
                    if !s.contains('.') {
                        line.push_str(".0");
                    }
                }
                Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                    line.push_str(&String::from_utf8_lossy(b));
                }
            }
        }
        rows.push(line);
    }
    rows
}

/// Assert hipp and the oracle agree on `query` over the file
/// at `path`, row order included.
async fn diff_query(path: &std::path::Path, query: &str) {
    let oracle_out = common::cli(path, query);
    let want: Vec<String> = oracle_out.lines().map(|s| s.to_string()).collect();
    let fs = FileStorage::open(path).expect("fs open");
    let mut db = Connection::open(fs, Config::default()).await.expect("open");
    let got = collect(&mut db, query).await;
    assert_eq!(
        got,
        want,
        "query: {query}\noracle: {}",
        common::oracle_version()
    );
}

async fn diff_queries(path: &std::path::Path, queries: &[&str]) {
    for q in queries {
        diff_query(path, q).await;
    }
}

fn oracle_db(tag: &str, setup: &str) -> std::path::PathBuf {
    let path = common::temp_db(tag);
    common::cli(&path, setup);
    path
}

const SETUP: &str = "CREATE TABLE t (a INTEGER, b INTEGER);
CREATE TABLE u (x INTEGER);
INSERT INTO t VALUES (1,10),(1,20),(2,30),(2,NULL);
INSERT INTO u VALUES (100),(200);";

#[test]
fn uncorrelated_and_plain_mixes() {
    block_on(async {
        let path = oracle_db("v11-uncorr", SETUP);
        diff_queries(
            &path,
            &[
                // The uncorrelated value lands once per row.
                "SELECT sum((SELECT 1)) FROM t",
                "SELECT sum((SELECT x FROM u LIMIT 1)) FROM t",
                "SELECT count((SELECT 1)) FROM t",
                // A nested aggregate inside the subquery.
                "SELECT sum((SELECT max(x) FROM u)) FROM t",
                "SELECT sum((SELECT sum(b) FROM t)) FROM t",
                // A compound subquery body: first row wins.
                "SELECT sum((SELECT 1 UNION SELECT 2)) FROM t",
                // The whole fold family over a bare-column
                // subquery (a no-FROM subquery reads the row).
                "SELECT sum((SELECT b)) FROM t",
                "SELECT avg((SELECT b)) FROM t",
                "SELECT total((SELECT b)) FROM t",
                "SELECT min((SELECT b)) FROM t",
                "SELECT max((SELECT b)) FROM t",
                "SELECT group_concat((SELECT b)) FROM t",
                // Plain columns mixed with subqueries inside
                // and around the argument.
                "SELECT sum(b + (SELECT 1)) FROM t",
                "SELECT sum((SELECT b) + (SELECT a)) FROM t",
                "SELECT sum((SELECT b)) + (SELECT 10) FROM t",
                "SELECT sum((SELECT b)) + b FROM t",
            ],
        )
        .await;
    });
}

#[test]
fn correlated_per_group_row() {
    block_on(async {
        let path = oracle_db("v11-corr", SETUP);
        diff_queries(
            &path,
            &[
                // Correlation to the group row through a real
                // FROM clause (first matching row per row).
                "SELECT sum((SELECT b FROM t t2 WHERE t2.a = t.a LIMIT 1)) FROM t",
                "SELECT sum((SELECT x FROM u WHERE x = b)) FROM t",
                // A conditional subquery: rows it misses
                // contribute NULL and are skipped / uncounted.
                "SELECT count((SELECT 1 WHERE a = 1)) FROM t",
                "SELECT sum((SELECT 1 WHERE a = 1)) FROM t",
                // IN and EXISTS forms inside an argument.
                "SELECT sum(a IN (SELECT x FROM u)) FROM t",
                "SELECT sum(EXISTS(SELECT 1 FROM u WHERE x = a)) FROM t",
                "SELECT sum(EXISTS(SELECT 1 FROM u WHERE x = b)) FROM t",
                // CASE steering between two subqueries.
                "SELECT sum(CASE WHEN a = 1 THEN (SELECT 5) ELSE (SELECT 6) END) FROM t",
            ],
        )
        .await;
    });
}

#[test]
fn group_by_having_order_by() {
    block_on(async {
        let path = oracle_db("v11-groups", SETUP);
        diff_queries(
            &path,
            &[
                "SELECT a, sum((SELECT b)) FROM t GROUP BY a ORDER BY a",
                "SELECT a, group_concat((SELECT b)) FROM t GROUP BY a ORDER BY a",
                "SELECT a, count((SELECT 1 WHERE b > 15)) FROM t GROUP BY a ORDER BY a",
                // HAVING over an aggregate whose argument is a
                // subquery, and HAVING as a bare subquery next
                // to a plain aggregate.
                "SELECT sum((SELECT 1)) FROM t GROUP BY a HAVING sum((SELECT 1)) > 1",
                "SELECT a, sum(b) FROM t GROUP BY a HAVING (SELECT 1) ORDER BY a",
                "SELECT a, sum(b) FROM t GROUP BY a HAVING sum((SELECT b)) > 25 ORDER BY a",
                // ORDER BY over an aggregate-argument
                // expression.
                "SELECT sum((SELECT 1)) AS s FROM t ORDER BY sum((SELECT 2))",
                "SELECT a, sum((SELECT b)) AS s FROM t GROUP BY a ORDER BY sum((SELECT a)) DESC, a",
            ],
        )
        .await;
    });
}

#[test]
fn bare_column_representative() {
    block_on(async {
        let path = oracle_db("v11-rep", SETUP);
        diff_queries(
            &path,
            &[
                // Lone min/max: bare columns read the row
                // whose per-row subquery value is extremal.
                "SELECT min((SELECT b)), b FROM t",
                "SELECT max((SELECT b)), b FROM t",
                "SELECT max((SELECT b)), b, a FROM t",
                "SELECT a, min((SELECT b)), b FROM t GROUP BY a ORDER BY a",
                // A non-extremum aggregate keeps the first
                // row as representative.
                "SELECT sum((SELECT b)), b FROM t",
            ],
        )
        .await;
    });
}

#[test]
fn distinct_and_separators() {
    block_on(async {
        let path = oracle_db("v11-distinct", SETUP);
        diff_queries(
            &path,
            &[
                "SELECT sum(DISTINCT (SELECT a)) FROM t",
                "SELECT count(DISTINCT (SELECT b)) FROM t",
                "SELECT sum(DISTINCT (SELECT b % 2)) FROM t",
                // group_concat's separator is a per-row
                // argument too, subqueries included.
                "SELECT group_concat((SELECT b), (SELECT ';')) FROM t",
                "SELECT group_concat((SELECT b), (SELECT a)) FROM t",
            ],
        )
        .await;
    });
}

#[test]
fn empty_and_null_groups() {
    block_on(async {
        let path = oracle_db("v11-empty", SETUP);
        diff_queries(
            &path,
            &[
                "SELECT sum((SELECT b)) FROM t WHERE a = 999",
                "SELECT count(*), sum((SELECT 1)) FROM t WHERE 0",
                "SELECT count((SELECT NULL)) FROM t",
                "SELECT sum((SELECT b)) FROM t WHERE 0",
                // Every row's subquery misses: the fold sees
                // only NULLs.
                "SELECT sum((SELECT x FROM u WHERE x = 999)) FROM t",
                "SELECT count((SELECT x FROM u WHERE x = 999)) FROM t",
                "SELECT total((SELECT x FROM u WHERE x = 999)) FROM t",
            ],
        )
        .await;
    });
}

#[test]
fn nested_positions() {
    block_on(async {
        let path = oracle_db("v11-nested", SETUP);
        diff_queries(
            &path,
            &[
                // An aggregate-argument subquery inside a
                // scalar subquery's own grouping.
                "SELECT (SELECT sum((SELECT b)) FROM t)",
                "SELECT (SELECT sum((SELECT b)) FROM t GROUP BY a ORDER BY a LIMIT 1)",
                // Inside a derived table's grouping.
                "SELECT * FROM (SELECT a, sum((SELECT b)) AS s FROM t GROUP BY a) ORDER BY a",
                // Inside the cores of a compound statement.
                "SELECT sum((SELECT b)) FROM t UNION ALL SELECT sum((SELECT a)) FROM t",
            ],
        )
        .await;
        // As an INSERT ... SELECT source: hipp writes one
        // file, the oracle a twin; the landed rows must agree
        // read back through the oracle.
        let twin = oracle_db("v11-nested-twin", SETUP);
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        exec(&mut db, "CREATE TABLE dst (v INTEGER)")
            .await
            .expect("hipp ddl");
        exec(&mut db, "INSERT INTO dst SELECT sum((SELECT b)) FROM t")
            .await
            .expect("hipp insert-select");
        exec(&mut db, "INSERT INTO dst SELECT a FROM t GROUP BY a")
            .await
            .expect("hipp insert-select 2");
        drop(db);
        common::cli(
            &twin,
            "CREATE TABLE dst (v INTEGER); INSERT INTO dst SELECT sum((SELECT b)) FROM t; INSERT INTO dst SELECT a FROM t GROUP BY a;",
        );
        let read = "SELECT v FROM dst ORDER BY v";
        assert_eq!(common::cli(&path, read), common::cli(&twin, read));
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
    });
}

#[test]
fn nested_aggregate_misuse_still_refused() {
    block_on(async {
        let path = oracle_db("v11-misuse", SETUP);
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        // A direct aggregate inside an aggregate's argument
        // is a misuse on the pin ("misuse of aggregate
        // function"); hipp refuses it with its typed
        // Unsupported error rather than evaluating it.
        let mut st = db
            .prepare("SELECT sum(sum(b)) FROM t")
            .await
            .expect("prepare");
        assert!(st.step().await.is_err(), "nested aggregate must fail");
        // The same shape one subquery down is legal (the
        // inner aggregate belongs to the subquery's grouping).
        assert_eq!(
            collect(&mut db, "SELECT sum((SELECT sum(b) FROM t)) FROM t").await,
            vec!["240".to_string()]
        );
    });
}
