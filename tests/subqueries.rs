//! v0.6 subqueries, differentially vs the pinned oracle
//! (SPEC §2 v0.6): scalar subqueries, IN (SELECT ...), EXISTS
//! across SELECT / UPDATE / DELETE — uncorrelated and
//! correlated — plus the plan-time rejection matrix. SELECT
//! results are compared row-for-row against the oracle CLI
//! over the same file; writes are applied by hipp to one copy
//! and by the oracle to a twin, then compared from the oracle
//! side.

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

/// Assert hipp and the oracle agree on `query` over the file at
/// `path` — row order included, so queries carry an ORDER BY
/// where determinism needs one.
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

fn oracle_db(tag: &str, setup: &str) -> std::path::PathBuf {
    let path = common::temp_db(tag);
    common::cli(&path, setup);
    path
}

const SETUP: &str = "CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT, budget INTEGER);
CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, dept_id INTEGER, salary INTEGER);
CREATE TABLE proj (id INTEGER PRIMARY KEY, title TEXT, lead_id INTEGER);
INSERT INTO dept VALUES (1,'eng',1000),(2,'ops',400),(3,'empty',50);
INSERT INTO emp VALUES (1,'amy',1,300),(2,'bob',1,200),(3,'cy',2,150),(4,'dee',NULL,100),(5,'eli',2,150);
INSERT INTO proj VALUES (1,'alpha',1),(2,'beta',3),(3,'gamma',NULL);";

// ---------------- scalar subqueries ----------------

#[test]
fn scalar_subqueries_differential() {
    block_on(async {
        let path = oracle_db("v06-scalar", SETUP);
        for q in [
            // Uncorrelated, in the SELECT list and in WHERE.
            "SELECT name, (SELECT name FROM dept WHERE id = 1) FROM emp ORDER BY id",
            "SELECT name FROM emp WHERE salary = (SELECT max(salary) FROM emp)",
            "SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY id",
            // No-FROM subquery and constant folding around one.
            "SELECT (SELECT 41 + 1)",
            "SELECT (SELECT count(*) FROM emp) + (SELECT count(*) FROM dept)",
            // Empty subquery yields NULL; aggregate over an
            // empty set still yields one (NULL / count) row.
            "SELECT (SELECT name FROM dept WHERE id = 99)",
            "SELECT (SELECT sum(salary) FROM emp WHERE id > 100)",
            "SELECT (SELECT count(*) FROM emp WHERE id > 100)",
            // Multi-row scalar: the first row wins (SQLite's
            // rule), with ORDER BY / LIMIT steering which.
            "SELECT (SELECT name FROM emp ORDER BY salary DESC LIMIT 1)",
            "SELECT (SELECT name FROM emp ORDER BY salary LIMIT 1)",
            "SELECT (SELECT salary FROM emp ORDER BY id)",
            // Scalar in arithmetic, CASE, and function arguments.
            "SELECT (SELECT budget FROM dept WHERE id = 2) / 2",
            "SELECT CASE WHEN (SELECT count(*) FROM proj) > 2 THEN 'many' ELSE 'few' END",
            "SELECT coalesce((SELECT name FROM dept WHERE id = 42), 'none')",
            // Correlated scalar in the SELECT list.
            "SELECT d.name, (SELECT count(*) FROM emp e WHERE e.dept_id = d.id) FROM dept d ORDER BY d.id",
            "SELECT d.name, (SELECT max(salary) FROM emp WHERE dept_id = d.id) FROM dept d ORDER BY d.id",
            // Correlated scalar in WHERE.
            "SELECT name FROM emp e WHERE salary > (SELECT avg(salary) FROM emp WHERE dept_id = e.dept_id) ORDER BY id",
            // Nested subqueries (scalar inside scalar, IN
            // inside scalar).
            "SELECT (SELECT (SELECT name FROM dept WHERE id = 2))",
            "SELECT (SELECT count(*) FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE budget > 300))",
            "SELECT name FROM dept WHERE id = (SELECT dept_id FROM emp WHERE id = (SELECT lead_id FROM proj WHERE id = 1))",
            // Correlated subquery in an aggregate query's items:
            // the outer context is the group's representative
            // row (here the grouping key itself).
            "SELECT dept_id, (SELECT name FROM dept WHERE id = emp.dept_id) FROM emp GROUP BY dept_id ORDER BY dept_id",
            // Subquery in HAVING, correlated and not.
            "SELECT dept_id FROM emp GROUP BY dept_id HAVING count(*) >= (SELECT count(*) FROM proj) ORDER BY dept_id",
            "SELECT dept_id FROM emp GROUP BY dept_id HAVING sum(salary) > (SELECT budget FROM dept WHERE id = emp.dept_id) ORDER BY dept_id",
            // Subquery in ORDER BY and in a GROUP BY key.
            "SELECT name FROM emp ORDER BY (SELECT budget FROM dept WHERE id = emp.dept_id) DESC, id",
            "SELECT (SELECT name FROM dept WHERE id = dept_id), count(*) FROM emp GROUP BY dept_id ORDER BY 1",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- IN (SELECT ...) ----------------

#[test]
fn in_subqueries_differential() {
    block_on(async {
        let path = oracle_db("v06-in", SETUP);
        for q in [
            "SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE budget >= 400) ORDER BY id",
            "SELECT name FROM emp WHERE dept_id NOT IN (SELECT id FROM dept WHERE budget >= 400) ORDER BY id",
            "SELECT name FROM dept WHERE id IN (SELECT lead_id FROM proj) ORDER BY id",
            // IN over an empty subquery matches nothing; NOT IN
            // over it matches everything (non-NULL side).
            "SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE id > 90) ORDER BY id",
            "SELECT name FROM emp WHERE id NOT IN (SELECT lead_id FROM proj WHERE id > 90) ORDER BY id",
            // Duplicates in the subquery result change nothing.
            "SELECT name FROM emp WHERE dept_id IN (SELECT dept_id FROM emp) ORDER BY id",
            // Correlated IN.
            "SELECT title FROM proj p WHERE EXISTS (SELECT 1 FROM emp e WHERE e.id = p.lead_id) ORDER BY p.id",
            "SELECT d.name FROM dept d WHERE d.id IN (SELECT dept_id FROM emp WHERE salary > 175) ORDER BY d.id",
            // Expression on the left, subquery with its own
            // WHERE / ORDER BY / LIMIT.
            "SELECT name FROM emp WHERE salary + 50 IN (SELECT budget / 2 FROM dept) ORDER BY id",
            "SELECT name FROM emp WHERE id IN (SELECT lead_id FROM proj ORDER BY id DESC LIMIT 2) ORDER BY id",
            // IN inside a larger boolean expression.
            "SELECT name FROM emp WHERE (dept_id IN (SELECT id FROM dept WHERE name = 'eng') AND salary >= 200) OR id = 4 ORDER BY id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

#[test]
fn in_subquery_null_semantics() {
    block_on(async {
        // proj.lead_id contains a NULL: x IN (… NULL …) still
        // matches real hits, while NOT IN yields no rows at all
        // (three-valued logic) — the classic SQLite trap.
        let path = oracle_db("v06-innull", SETUP);
        for q in [
            "SELECT id FROM emp WHERE id IN (SELECT lead_id FROM proj) ORDER BY id",
            "SELECT id FROM emp WHERE id NOT IN (SELECT lead_id FROM proj) ORDER BY id",
            "SELECT id FROM emp WHERE id NOT IN (SELECT lead_id FROM proj WHERE lead_id IS NOT NULL) ORDER BY id",
            // A NULL left side never matches, in either form.
            "SELECT id FROM emp WHERE dept_id IN (SELECT id FROM dept) ORDER BY id",
            "SELECT id FROM emp WHERE dept_id NOT IN (SELECT id FROM dept) ORDER BY id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- EXISTS ----------------

#[test]
fn exists_subqueries_differential() {
    block_on(async {
        let path = oracle_db("v06-exists", SETUP);
        for q in [
            "SELECT name FROM dept WHERE EXISTS (SELECT 1 FROM emp WHERE dept_id = dept.id) ORDER BY id",
            "SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE dept_id = dept.id) ORDER BY id",
            "SELECT EXISTS (SELECT 1 FROM emp)",
            "SELECT EXISTS (SELECT 1 FROM emp WHERE id > 100)",
            "SELECT NOT EXISTS (SELECT 1 FROM emp WHERE id > 100)",
            // LIMIT 0 empties even an EXISTS subquery.
            "SELECT EXISTS (SELECT 1 FROM emp LIMIT 0)",
            // EXISTS in the SELECT list, correlated per row.
            "SELECT d.name, EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id AND e.salary >= 300) FROM dept d ORDER BY d.id",
            // EXISTS over an aggregate subquery with HAVING.
            "SELECT name FROM dept d WHERE EXISTS (SELECT dept_id FROM emp WHERE dept_id = d.id GROUP BY dept_id HAVING count(*) > 1) ORDER BY id",
            // Double correlation: the innermost query reaches
            // the outermost row through two scope levels.
            "SELECT d.name FROM dept d WHERE EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id AND EXISTS (SELECT 1 FROM proj p WHERE p.lead_id = e.id)) ORDER BY d.id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- correlation and shadowing ----------------

#[test]
fn correlated_shadowing_differential() {
    block_on(async {
        let path = oracle_db(
            "v06-shadow",
            "CREATE TABLE t (a INTEGER, b TEXT);
             CREATE TABLE u (a INTEGER, b TEXT);
             INSERT INTO t VALUES (1,'t1'),(2,'t2'),(3,'t3');
             INSERT INTO u VALUES (2,'u2'),(3,'u3'),(4,'u4');",
        );
        for q in [
            // Unqualified `a` inside the subquery is u.a (the
            // inner source shadows the outer one).
            "SELECT a FROM t WHERE a IN (SELECT a FROM u) ORDER BY a",
            "SELECT a, (SELECT b FROM u WHERE a = t.a) FROM t ORDER BY a",
            // Qualified references reach the outer alias.
            "SELECT x.a FROM t x WHERE EXISTS (SELECT 1 FROM u WHERE u.a = x.a) ORDER BY x.a",
            // Self-correlation through two aliases of one table.
            "SELECT a FROM t t1 WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.a > t1.a) ORDER BY a",
            "SELECT a FROM t t1 WHERE a = (SELECT max(a) FROM t t2 WHERE t2.b = t1.b) ORDER BY a",
            // Correlated subquery in a JOIN's ON clause.
            "SELECT t.a, u.b FROM t LEFT JOIN u ON u.a = t.a AND EXISTS (SELECT 1 FROM u u2 WHERE u2.a = t.a AND u2.b LIKE 'u%') ORDER BY t.a",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- subqueries on the write side ----------------

/// Apply `writes` via hipp to one oracle-built copy and via
/// the oracle to a twin; the final table dumps (read by the
/// oracle) must agree.
async fn diff_write(tag: &str, writes: &[&str], checks: &[&str]) {
    let a = oracle_db(&format!("{tag}-hipp"), SETUP);
    let b = oracle_db(&format!("{tag}-ora"), SETUP);
    {
        let fs = FileStorage::open(&a).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for w in writes {
            exec(&mut db, w).await.expect("hipp write");
        }
    }
    for w in writes {
        common::cli(&b, w);
    }
    for c in checks {
        assert_eq!(
            common::cli(&a, c),
            common::cli(&b, c),
            "write check: {c} (writes: {writes:?})"
        );
        assert_eq!(
            common::cli(&a, "PRAGMA integrity_check;"),
            "ok\n",
            "integrity after {writes:?}"
        );
    }
}

#[test]
fn subquery_writes_differential() {
    block_on(async {
        diff_write(
            "v06-upd-in",
            &["UPDATE emp SET salary = salary + 10 WHERE dept_id IN (SELECT id FROM dept WHERE budget >= 400)"],
            &["SELECT * FROM emp ORDER BY id"],
        )
        .await;
        diff_write(
            "v06-upd-set-scalar",
            &["UPDATE dept SET budget = (SELECT coalesce(sum(salary), 0) FROM emp WHERE dept_id = dept.id)"],
            &["SELECT * FROM dept ORDER BY id"],
        )
        .await;
        diff_write(
            "v06-upd-set-uncorr",
            &["UPDATE emp SET salary = (SELECT max(salary) FROM emp) WHERE id = 3"],
            &["SELECT * FROM emp ORDER BY id"],
        )
        .await;
        diff_write(
            "v06-del-exists",
            &["DELETE FROM emp WHERE EXISTS (SELECT 1 FROM dept WHERE dept.id = emp.dept_id AND dept.budget < 500)"],
            &["SELECT * FROM emp ORDER BY id"],
        )
        .await;
        diff_write(
            "v06-del-in",
            &["DELETE FROM proj WHERE lead_id NOT IN (SELECT id FROM emp WHERE salary >= 200)"],
            &["SELECT * FROM proj ORDER BY id"],
        )
        .await;
        diff_write(
            "v06-upd-where-scalar",
            &["UPDATE proj SET title = title || '!' WHERE lead_id = (SELECT id FROM emp WHERE name = 'amy')"],
            &["SELECT * FROM proj ORDER BY id"],
        )
        .await;
    });
}

// ---------------- hipp-built database ----------------

#[test]
fn subqueries_on_hipp_built_db() {
    block_on(async {
        let path = common::temp_db("v06-built");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            for s in [
                "CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT, budget INTEGER)",
                "CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, dept_id INTEGER, salary INTEGER)",
                "INSERT INTO dept VALUES (1,'eng',1000),(2,'ops',400)",
                "INSERT INTO emp VALUES (1,'amy',1,300),(2,'bob',1,200),(3,'cy',2,150)",
            ] {
                exec(&mut db, s).await.expect("hipp build");
            }
        }
        for q in [
            "SELECT d.name, (SELECT count(*) FROM emp WHERE dept_id = d.id) FROM dept d ORDER BY d.id",
            "SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE budget > 500) ORDER BY id",
            "SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE dept_id = dept.id) ORDER BY id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- rejections ----------------

#[test]
fn subquery_rejections() {
    block_on(async {
        let path = oracle_db("v06-reject", SETUP);
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        // A scalar / IN subquery must return exactly one
        // column (the oracle: "sub-select returns 2 columns").
        let r = db
            .prepare("SELECT (SELECT id, name FROM dept WHERE id = 1) FROM emp")
            .await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: hipp::SchemaKind::ColumnCountMismatch
                })
            ),
            "scalar width: {:?}",
            r.err()
        );
        assert!(
            db.prepare("SELECT id FROM emp WHERE id IN (SELECT id, name FROM dept)")
                .await
                .is_err()
        );
        // A subquery inside an aggregate's arguments is
        // refused at plan time (named v0.6 boundary).
        assert!(
            db.prepare("SELECT sum((SELECT max(salary) FROM emp)) FROM emp")
                .await
                .is_err()
        );
        // Subqueries in INSERT VALUES were refused in v0.6
        // (constant folding has no executor); v0.8 implements
        // them (see tests/insert_select_v08.rs), so the prepare
        // now succeeds — execute it and check the row landed.
        {
            let mut st = db
                .prepare("INSERT INTO dept VALUES ((SELECT max(id) FROM dept) + 1, 'x', 1)")
                .await
                .expect("v0.8: VALUES subquery prepares");
            assert_eq!(st.step().await.expect("step"), hipp::Step::Done);
        }
        assert_eq!(
            collect(&mut db, "SELECT id, name, budget FROM dept ORDER BY id").await,
            vec![
                "1|eng|1000".to_string(),
                "2|ops|400".to_string(),
                "3|empty|50".to_string(),
                "4|x|1".to_string(),
            ]
        );
        // Unknown names inside a subquery fail at prepare,
        // correlated or not.
        assert!(
            db.prepare("SELECT id FROM emp WHERE id IN (SELECT nope FROM dept)")
                .await
                .is_err()
        );
        assert!(
            db.prepare("SELECT id FROM emp WHERE id IN (SELECT id FROM nosuch)")
                .await
                .is_err()
        );
        assert!(
            db.prepare("SELECT (SELECT budget FROM dept WHERE dept.id = emp.nope) FROM emp")
                .await
                .is_err()
        );
        // Selecting from a view inside a subquery is refused
        // (views are inert).
        exec(&mut db, "CREATE VIEW v_dept AS SELECT id, name FROM dept")
            .await
            .expect("create view");
        assert!(
            db.prepare("SELECT id FROM emp WHERE id IN (SELECT id FROM v_dept)")
                .await
                .is_err()
        );
    });
}
