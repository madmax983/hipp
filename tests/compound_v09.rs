//! v0.9 differentials vs the pinned oracle (SPEC §2 phases,
//! feature `v09`): compound SELECTs (UNION / UNION ALL /
//! INTERSECT / EXCEPT between SELECT and VALUES cores) and
//! FROM-clause derived tables. Every compound rule below was
//! probed against the pin before implementation:
//!
//! - all four operators share ONE precedence and associate to
//!   the left (`1 UNION 2 INTERSECT 2` is `{2}`, not `{1,2}`);
//! - compound row equality is the RAW storage comparison with
//!   NULLs equal — NO affinity is applied, not even the
//!   leftmost column's (`1 UNION '1'` keeps both rows; a TEXT
//!   column does not convert an integer operand);
//! - dedup ops emit in sorter order (ascending, raw, by every
//!   column); UNION ALL concatenates in operand order;
//! - the dedup survivor rule: each side dedups first-wins,
//!   then a UNION right row replaces an equal left row
//!   (`1 UNION 1.0` is the real; `1.0 UNION 1` the integer);
//! - a compound's ORDER BY accepts only output ordinals or
//!   result-column names (from ANY constituent core, qualified
//!   names included); expressions and constants fail prepare;
//! - cores carry no ORDER BY / LIMIT of their own, operands
//!   are never parenthesised, and a trailing ORDER BY / LIMIT
//!   after a trailing VALUES core is a syntax error;
//! - derived tables take an optional alias but NO column-name
//!   list, expose no rowid, cannot see sibling sources (no
//!   LATERAL) but can correlate with enclosing queries.
//!
//! Read queries compare row-for-row against the oracle CLI
//! over the same file; writes go to twin files (hipp vs
//! oracle) with oracle-side verdicts.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Result<(), S::Error>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
    Ok(())
}

async fn exec_err<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Error<S::Error>
where
    S::Error: core::fmt::Debug,
{
    match db.prepare(sql).await {
        Err(e) => e,
        Ok(mut st) => st.step().await.expect_err("step must fail"),
    }
}

/// Step a statement to completion, collecting every row it
/// yields.
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

fn integrity(path: &std::path::Path) {
    assert_eq!(
        common::cli(path, "PRAGMA integrity_check;").trim(),
        "ok",
        "oracle version: {}",
        common::oracle_version()
    );
}

fn oracle_rows(path: &std::path::Path, sql: &str) -> Vec<String> {
    let out = common::cli(path, sql);
    out.lines().map(str::to_string).collect()
}

/// Run one SQL batch through the oracle CLI; returns
/// (success, stdout lines) without asserting, for statements
/// expected to fail on both engines.
fn oracle_raw(path: &std::path::Path, sql: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

fn oracle_db(tag: &str, setup: &str) -> std::path::PathBuf {
    let path = common::temp_db(tag);
    common::cli(&path, setup);
    path
}

/// Assert hipp and the oracle agree on `query` over the file
/// at `path` — row order included.
async fn diff_query(path: &std::path::Path, query: &str) {
    let want = oracle_rows(path, query);
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

/// Assert BOTH engines refuse `query` over the file at `path`.
async fn diff_refuse(path: &std::path::Path, query: &str) {
    let (ok, _) = oracle_raw(path, query);
    assert!(!ok, "oracle accepted: {query}");
    let fs = FileStorage::open(path).expect("fs open");
    let mut db = Connection::open(fs, Config::default()).await.expect("open");
    let _ = exec_err(&mut db, query).await;
}

/// A pair of twin databases with identical setup.
struct Twins {
    hipp_path: std::path::PathBuf,
    oracle_path: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp_path = common::temp_db(&format!("{tag}-h"));
    let oracle_path = common::temp_db(&format!("{tag}-o"));
    common::cli(&oracle_path, setup);
    block_on(async {
        let fs = FileStorage::open(&hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                exec(&mut db, stmt).await.unwrap();
            }
        }
    });
    Twins {
        hipp_path,
        oracle_path,
    }
}

impl Twins {
    /// Apply one write statement to both twins; it must
    /// succeed on both.
    async fn write(&self, sql: &str) {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        exec(&mut db, sql).await.unwrap();
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(ok, "oracle rejected: {sql}");
    }

    /// Apply one write statement that must fail on both
    /// engines, leaving both twins untouched.
    async fn write_err(&self, sql: &str) {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let _ = exec_err(&mut db, sql).await;
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(!ok, "oracle accepted: {sql}");
    }

    /// Run one row-producing statement on both twins and
    /// compare the rows.
    async fn rows(&self, sql: &str) -> Vec<String> {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let got = collect(&mut db, sql).await;
        let want = oracle_rows(&self.oracle_path, sql);
        assert_eq!(got, want, "statement: {sql}");
        got
    }

    /// Final verdict: oracle reads the hipp file clean and
    /// both twins agree on the full contents per `dump_sql`.
    fn finish(&self, dump_sql: &str) {
        integrity(&self.hipp_path);
        assert_eq!(
            oracle_rows(&self.hipp_path, dump_sql),
            oracle_rows(&self.oracle_path, dump_sql),
            "twin contents diverged"
        );
    }
}

const SETUP: &str = "CREATE TABLE t (a INTEGER, b TEXT);
INSERT INTO t VALUES (1,'one'),(2,'two'),(10,'ten');
CREATE TABLE u (c INTEGER, d TEXT);
INSERT INTO u VALUES (2,'zwei'),(3,'drei'),(7,'sieben');
CREATE TABLE tt (x TEXT);
INSERT INTO tt VALUES ('1'),('2'),('10');
CREATE TABLE n (v);
INSERT INTO n VALUES (1),(1.0),('1');";

// ---------------- compound operators ----------------

#[test]
fn compound_operators_and_precedence() {
    block_on(async {
        let path = oracle_db("v09-ops", SETUP);
        for q in [
            "SELECT a FROM t UNION SELECT c FROM u",
            "SELECT a FROM t UNION ALL SELECT c FROM u",
            "SELECT a FROM t INTERSECT SELECT c FROM u",
            "SELECT a FROM t EXCEPT SELECT c FROM u",
            "SELECT c FROM u EXCEPT SELECT a FROM t",
            // All four operators share one precedence, left
            // associative (pin-verified): the INTERSECT-first
            // reading of the first query would give {1,2}.
            "SELECT 1 UNION SELECT 2 INTERSECT SELECT 2",
            "SELECT 1 INTERSECT SELECT 1 UNION SELECT 2",
            "SELECT 1 EXCEPT SELECT 1 UNION SELECT 1",
            "SELECT 1 UNION ALL SELECT 1 INTERSECT SELECT 1",
            "SELECT 5 INTERSECT SELECT 5 UNION ALL SELECT 5",
            "SELECT 1 UNION SELECT 1 UNION ALL SELECT 1",
            // Three-way chains and mixed widths of ops.
            "SELECT a FROM t UNION SELECT c FROM u UNION SELECT 100",
            "SELECT a FROM t UNION ALL SELECT c FROM u EXCEPT SELECT 2",
            "SELECT a, b FROM t UNION ALL SELECT c, d FROM u",
            "SELECT a, b FROM t UNION SELECT c, d FROM u",
            "SELECT a, b FROM t EXCEPT SELECT c, d FROM u WHERE c = 2",
            // Cores with their own WHERE / GROUP BY / DISTINCT.
            "SELECT a FROM t WHERE a > 1 UNION SELECT c FROM u WHERE c < 7",
            "SELECT a, count(*) FROM t GROUP BY a UNION ALL SELECT c, 1 FROM u",
            "SELECT DISTINCT a FROM t UNION SELECT c FROM u",
            "SELECT a FROM t UNION SELECT a FROM t",
            // No-FROM cores.
            "SELECT 1, 'x' UNION SELECT 2, 'y'",
            "SELECT 1 WHERE 0 UNION SELECT 2",
        ] {
            diff_query(&path, q).await;
        }
    });
}

#[test]
fn compound_equality_is_raw_no_affinity() {
    block_on(async {
        let path = oracle_db("v09-eq", SETUP);
        for q in [
            // Integer 1 and text '1' are distinct compound rows.
            "SELECT 1 UNION SELECT '1'",
            "SELECT '1' UNION SELECT 1",
            "SELECT x FROM tt UNION SELECT 1",
            "SELECT 1 UNION SELECT x FROM tt",
            "SELECT CAST(1 AS TEXT) UNION SELECT 1",
            "SELECT 'a' UNION SELECT x'61'",
            // Numeric equality crosses int/real; the survivor
            // is the right side's row (pin-verified).
            "SELECT 1 UNION SELECT 1.0",
            "SELECT 1.0 UNION SELECT 1",
            "SELECT 1 UNION SELECT 1.0 UNION SELECT 1",
            "SELECT v FROM n UNION SELECT 2",
            "SELECT v FROM n UNION SELECT 1.0",
            "SELECT 0 UNION SELECT column1 FROM (VALUES (1),(1.0))",
            // INTERSECT / EXCEPT keep the left side's rows;
            // within a side, first occurrence wins.
            "SELECT 1 INTERSECT SELECT 1.0",
            "SELECT 1.0 INTERSECT SELECT 1",
            "SELECT column1 FROM (VALUES (1.0),(1)) INTERSECT SELECT 1",
            "SELECT column1 FROM (VALUES (1),(1.0)) EXCEPT SELECT 2",
            // NULLs are equal for compound purposes.
            "SELECT NULL UNION SELECT NULL",
            "SELECT NULL INTERSECT SELECT NULL",
            "SELECT NULL EXCEPT SELECT NULL",
            "SELECT NULL EXCEPT SELECT 1",
            "SELECT 1 EXCEPT SELECT NULL",
            "SELECT a FROM t UNION SELECT NULL",
            // Affinity of the leftmost core does not convert
            // the right (and vice versa) for EXCEPT either.
            "SELECT '1' EXCEPT SELECT a FROM t WHERE a = 1",
            "SELECT a FROM t WHERE a = 1 EXCEPT SELECT '1'",
            // Compound ORDER BY compares raw too: the integer
            // sorts before the text regardless of column type.
            "SELECT x FROM tt WHERE x = '10' UNION ALL SELECT 2 ORDER BY 1",
            "SELECT 2 UNION ALL SELECT '10' ORDER BY 1",
        ] {
            diff_query(&path, q).await;
        }
    });
}

#[test]
fn compound_order_by_rules() {
    block_on(async {
        let path = oracle_db("v09-ord", SETUP);
        for q in [
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY 1",
            "SELECT a FROM t UNION ALL SELECT c FROM u ORDER BY 1 DESC",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY 1 LIMIT 2",
            "SELECT a FROM t UNION ALL SELECT c FROM u ORDER BY 1 LIMIT 2 OFFSET 1",
            // Names resolve against result columns of ANY
            // core: leftmost alias, leftmost column name,
            // right-core column name, right-core alias,
            // qualified names from either core, expression
            // alias.
            "SELECT a AS z FROM t UNION SELECT c FROM u ORDER BY z",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY a",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY c",
            "SELECT a FROM t UNION SELECT c AS w FROM u ORDER BY w",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY t.a",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY u.c",
            "SELECT a + 1 AS s FROM t UNION SELECT c FROM u ORDER BY s",
            "SELECT a, b FROM t UNION SELECT c, d FROM u ORDER BY 2 DESC, 1",
            "SELECT a, b FROM t UNION ALL SELECT c, d FROM u ORDER BY b",
            // ORDER BY of a compound inside a derived table
            // (the outer statement's own ORDER BY).
            "SELECT * FROM (SELECT a, b FROM t UNION SELECT c, d FROM u) s ORDER BY s.b",
            // Scalar subquery over a compound.
            "SELECT (SELECT a FROM t UNION SELECT c FROM u ORDER BY 1 DESC LIMIT 1)",
        ] {
            diff_query(&path, q).await;
        }
        // Refused on both: expressions, constants, unknown
        // names, bad qualifiers, out-of-range ordinals.
        for q in [
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY a + 0",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY 'x'",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY nosuch",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY x.a",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY 3",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY b",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY d",
        ] {
            diff_refuse(&path, q).await;
        }
    });
}

#[test]
fn compound_shape_refusals() {
    block_on(async {
        let path = oracle_db("v09-shape", SETUP);
        for q in [
            // Column-count mismatch, either direction.
            "SELECT a, b FROM t UNION SELECT c FROM u",
            "SELECT 1 UNION SELECT 1, 2",
            "SELECT a FROM t UNION VALUES (1, 2)",
            // A core's own ORDER BY / LIMIT before the operator.
            "SELECT a FROM t ORDER BY 1 UNION SELECT 2",
            "SELECT a FROM t LIMIT 2 UNION SELECT 99",
            // Parenthesised operands are not SQLite grammar.
            "SELECT 1 UNION (SELECT 2)",
            "(SELECT a FROM t LIMIT 2) UNION SELECT 99",
        ] {
            diff_refuse(&path, q).await;
        }
    });
}

// ---------------- VALUES cores ----------------

#[test]
fn values_cores() {
    block_on(async {
        let path = oracle_db("v09-values", SETUP);
        for q in [
            "VALUES (2),(1)",
            "VALUES (1, 'x'),(2, 'y')",
            "VALUES (1 + 1), ((SELECT max(a) FROM t))",
            "VALUES (1) UNION SELECT 2",
            "SELECT 1 UNION VALUES (2)",
            "VALUES (2) UNION ALL VALUES (1)",
            "VALUES (9) EXCEPT SELECT a FROM t",
            // A tail is fine when the LAST core is a SELECT.
            "VALUES (1) UNION SELECT 2 ORDER BY 1",
            "VALUES (1) UNION SELECT 2 LIMIT 1",
            // VALUES as subquery bodies.
            "SELECT 1 WHERE 1 IN (VALUES (1),(2))",
            "SELECT 1 WHERE 3 NOT IN (VALUES (1),(2))",
            "SELECT (VALUES (5))",
            "SELECT * FROM t WHERE a IN (VALUES (1),(10))",
            "SELECT * FROM (VALUES (1,2),(3,4)) v",
            "SELECT * FROM (VALUES (2),(1)) v ORDER BY column1",
            "SELECT column2 FROM (VALUES (1,'a'),(2,'b')) v WHERE column1 = 2",
        ] {
            diff_query(&path, q).await;
        }
        for q in [
            // Width mismatch inside VALUES and vs compounds.
            "VALUES (1),(2,3)",
            "VALUES (1,2) UNION SELECT 1",
            // A tail after a trailing VALUES core is a syntax
            // error on the pin.
            "VALUES (2),(1) ORDER BY 1",
            "VALUES (1) LIMIT 1",
            "SELECT 1 UNION VALUES (2) ORDER BY 1",
            "SELECT 1 UNION VALUES (2) LIMIT 1",
        ] {
            diff_refuse(&path, q).await;
        }
    });
}

// ---------------- derived tables ----------------

#[test]
fn derived_tables() {
    block_on(async {
        let path = oracle_db("v09-derived", SETUP);
        for q in [
            "SELECT * FROM (SELECT 1 AS x)",
            "SELECT * FROM (SELECT 1 AS x) s",
            "SELECT * FROM (SELECT a, b FROM t) s",
            "SELECT * FROM (SELECT a, b FROM t) s WHERE s.a = 2",
            "SELECT * FROM (SELECT a FROM t) WHERE a = 2",
            "SELECT s.a, u.d FROM (SELECT a FROM t) s JOIN u ON s.a = u.c",
            "SELECT * FROM (SELECT a FROM t) s1 JOIN (SELECT c FROM u) s2 ON s1.a = s2.c",
            "SELECT * FROM ((SELECT 1 AS x)) s",
            // Compound and aggregate and limited interiors.
            "SELECT * FROM (SELECT a FROM t UNION SELECT c FROM u) s",
            "SELECT * FROM (SELECT a, count(*) AS k FROM t GROUP BY a) s WHERE k = 1",
            "SELECT * FROM (SELECT a FROM t ORDER BY a DESC LIMIT 2) s",
            "SELECT * FROM (VALUES (1,2),(3,4)) AS v",
            // Expression column naming (the pin names it by
            // its source text).
            "SELECT \"1+1\" FROM (SELECT 1+1) s",
            // A derived column named rowid is a plain column.
            "SELECT rowid FROM (SELECT 5 AS rowid) s",
            // Derived inside a subquery's FROM, and nested.
            "SELECT * FROM t WHERE a IN (SELECT v FROM (SELECT c AS v FROM u) s)",
            "SELECT * FROM (SELECT * FROM (SELECT a FROM t) inner_s) outer_s",
            // Correlation with an ENCLOSING query is legal
            // (the EXISTS body sees the outer t).
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM (SELECT t.a AS v) s)",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM (SELECT t.a AS v) s WHERE s.v = 2)",
            // Derived table over a JOIN interior.
            "SELECT * FROM (SELECT t.a, u.d FROM t JOIN u ON t.a = u.c) s",
        ] {
            diff_query(&path, q).await;
        }
        for q in [
            // No rowid on a derived table.
            "SELECT rowid FROM (SELECT 1 AS x) s",
            "SELECT s.rowid FROM (SELECT 1 AS x) s",
            // No LATERAL: sibling sources are invisible.
            "SELECT * FROM t, (SELECT t.a AS v) s",
            "SELECT * FROM (SELECT a FROM t) s JOIN (SELECT s.a AS v) s2 ON 1 = 1",
            // No column-name list after a derived alias.
            "SELECT * FROM (SELECT 1 AS x) AS s (y)",
            "SELECT * FROM (VALUES (1,2)) AS v (a)",
        ] {
            diff_refuse(&path, q).await;
        }
    });
}

// ---------------- compounds/derived in writes ----------------

#[test]
fn insert_select_compound_and_derived() {
    block_on(async {
        let tw = twins("v09-ins", SETUP);
        tw.write("CREATE TABLE d (a INTEGER, b TEXT)").await;
        // Compound as an INSERT source, incl. a self-read
        // (snapshot semantics from v0.8 carry through).
        tw.write("INSERT INTO d SELECT a, b FROM t UNION SELECT c, d FROM u")
            .await;
        tw.write("INSERT INTO d SELECT a, b FROM d UNION ALL SELECT a, b FROM d")
            .await;
        // Derived and VALUES sources.
        tw.write("INSERT INTO d SELECT * FROM (SELECT c, d FROM u) s")
            .await;
        tw.write("INSERT INTO d VALUES (42, 'x')").await;
        // Upsert over a compound source: the compound
        // terminates the SELECT, so ON CONFLICT parses (the
        // v0.8 bare-FROM quirk does not apply).
        tw.write(
            "INSERT INTO d SELECT a, b FROM t UNION SELECT 7, 'sieben' \
             ON CONFLICT DO NOTHING",
        )
        .await;
        // A compound source whose cores disagree on width is
        // refused by both engines, writing nothing.
        tw.write_err("INSERT INTO d SELECT a FROM t UNION SELECT c, d FROM u")
            .await;
        let rows = tw.rows("SELECT count(*) FROM d").await;
        assert_eq!(rows.len(), 1);
        tw.finish("SELECT a, b FROM d ORDER BY a, b");
    });
}

#[test]
fn update_from_derived() {
    block_on(async {
        let tw = twins("v09-upd", SETUP);
        tw.write("UPDATE t SET b = 'x' FROM (SELECT c FROM u) s WHERE t.a = s.c")
            .await;
        tw.write(
            "UPDATE t SET b = 'y' FROM (SELECT c FROM u UNION SELECT 10) s \
             WHERE t.a = s.c",
        )
        .await;
        tw.write("UPDATE u SET d = 'z' FROM (SELECT a FROM t GROUP BY a) s WHERE u.c = s.a")
            .await;
        tw.finish("SELECT a, b FROM t ORDER BY a");
        tw.finish("SELECT c, d FROM u ORDER BY c");
    });
}

#[test]
fn compound_subqueries() {
    block_on(async {
        let path = oracle_db("v09-sub", SETUP);
        for q in [
            "SELECT * FROM t WHERE a IN (SELECT c FROM u UNION SELECT 10)",
            "SELECT * FROM t WHERE a IN (SELECT c FROM u EXCEPT SELECT 2)",
            "SELECT * FROM t WHERE EXISTS (SELECT c FROM u INTERSECT SELECT a FROM t)",
            "SELECT (SELECT c FROM u UNION SELECT 99 ORDER BY 1 LIMIT 1)",
            "SELECT (SELECT max(v) FROM (SELECT a AS v FROM t UNION SELECT c FROM u) s)",
            "SELECT a, (SELECT count(*) FROM (SELECT c FROM u UNION ALL SELECT a) s) FROM t ORDER BY a",
            // Correlated compound: the right core reads the
            // outer row.
            "SELECT * FROM t WHERE EXISTS (SELECT 1 UNION SELECT 1 WHERE t.a = 10)",
        ] {
            diff_query(&path, q).await;
        }
    });
}
