//! v0.13 uncorrelated-subquery hoisting, differentially vs
//! the pinned oracle (SPEC §2): an uncorrelated scalar / IN /
//! EXISTS subquery (including inside aggregate arguments) is
//! executed once per statement and its result cached; the
//! pinned oracle fixes such a subquery's value for the whole
//! statement (probed before implementation — see the write
//! snapshot tests). Results must be identical to the oracle
//! everywhere the v0.12 engine was already right, and on the
//! UPDATE ... SET snapshot case the v0.12 engine was wrong
//! (6, 11, 20 vs the pin's 6, 6, 6). Execution-count tests
//! assert the win directly through
//! `Connection::subquery_executions`: O(rows) drops to O(1)
//! for uncorrelated nodes, and correlated nodes keep their
//! per-row count exactly.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

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

async fn diff_query(path: &std::path::Path, query: &str) {
    let oracle_out = common::cli(path, query);
    let want: Vec<String> = oracle_out.lines().map(|s| s.to_string()).collect();
    let fs = hipp::FileStorage::open(path).expect("fs open");
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
INSERT INTO dept VALUES (1,'eng',1000),(2,'ops',400),(3,'empty',50);
INSERT INTO emp VALUES (1,'amy',1,300),(2,'bob',1,200),(3,'cy',2,150),(4,'dee',NULL,100),(5,'eli',2,150);";

// ---------------- read-side identity ----------------

#[test]
fn hoisted_reads_differential() {
    block_on(async {
        let path = oracle_db("v13-reads", SETUP);
        for q in [
            // Uncorrelated scalar in the SELECT list / WHERE,
            // repeated over every outer row.
            "SELECT name, (SELECT max(salary) FROM emp) FROM emp ORDER BY id",
            "SELECT name FROM emp WHERE salary >= (SELECT avg(salary) FROM emp) ORDER BY id",
            // Empty scalar result caches as NULL; aggregate
            // over an empty set caches its one (NULL / 0) row.
            "SELECT (SELECT name FROM dept WHERE id = 99), id FROM emp ORDER BY id",
            "SELECT (SELECT sum(salary) FROM emp WHERE id > 100), (SELECT count(*) FROM emp WHERE id > 100) FROM dept ORDER BY id",
            // Multi-row scalar: the first row wins, with and
            // without ORDER BY / LIMIT steering.
            "SELECT (SELECT salary FROM emp ORDER BY id) FROM dept ORDER BY id",
            "SELECT (SELECT name FROM emp ORDER BY salary DESC LIMIT 1) FROM dept ORDER BY id",
            // IN: duplicates, empties, NULL members.
            "SELECT name FROM emp WHERE dept_id IN (SELECT dept_id FROM emp) ORDER BY id",
            "SELECT name FROM emp WHERE id IN (SELECT id FROM dept WHERE id > 90) ORDER BY id",
            "SELECT name FROM emp WHERE id NOT IN (SELECT dept_id FROM emp WHERE dept_id IS NOT NULL) ORDER BY id",
            // EXISTS, true and false, in list and filter.
            "SELECT EXISTS (SELECT 1 FROM emp WHERE salary > 1000), id FROM dept ORDER BY id",
            "SELECT name FROM dept WHERE EXISTS (SELECT 1 FROM emp WHERE salary >= 300) ORDER BY id",
            "SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE salary >= 300) ORDER BY id",
            // Deterministic expressions inside the subquery
            // (arithmetic, functions, CASE over its own data).
            "SELECT (SELECT max(salary) * 2 + 1 FROM emp) FROM dept ORDER BY id",
            "SELECT (SELECT coalesce(min(budget), -1) FROM dept WHERE id > 50) FROM emp ORDER BY id",
            "SELECT (SELECT CASE WHEN count(*) > 4 THEN 'many' ELSE 'few' END FROM emp) FROM dept ORDER BY id",
            // Nested: uncorrelated inside uncorrelated, and an
            // uncorrelated node inside a correlated plan (its
            // value is fixed by the outermost row only through
            // the enclosing plan's single... per-row path --
            // either way the results must match the pin).
            "SELECT (SELECT (SELECT max(budget) FROM dept)) FROM emp ORDER BY id",
            "SELECT d.name, (SELECT count(*) FROM emp e WHERE e.dept_id = d.id AND e.salary > (SELECT avg(salary) FROM emp)) FROM dept d ORDER BY d.id",
            // Uncorrelated in HAVING / GROUP BY / ORDER BY.
            "SELECT dept_id FROM emp GROUP BY dept_id HAVING count(*) >= (SELECT count(*) FROM dept) ORDER BY dept_id",
            "SELECT name FROM emp ORDER BY (SELECT max(budget) FROM dept) DESC, id",
            // Uncorrelated subquery over a FROM derived table
            // and a compound core.
            "SELECT (SELECT count(*) FROM (SELECT id FROM dept UNION ALL SELECT id FROM dept)) FROM emp ORDER BY id",
            "SELECT name FROM emp WHERE id IN (SELECT id FROM dept UNION SELECT dept_id FROM emp) ORDER BY id",
            // Correlated controls: results unchanged, per-row.
            "SELECT d.name, (SELECT count(*) FROM emp e WHERE e.dept_id = d.id) FROM dept d ORDER BY d.id",
            "SELECT name FROM emp e WHERE salary > (SELECT avg(salary) FROM emp WHERE dept_id = e.dept_id) ORDER BY id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

#[test]
fn hoisted_agg_arg_differential() {
    block_on(async {
        let path = oracle_db("v13-aggarg", SETUP);
        for q in [
            // Uncorrelated subquery inside aggregate arguments:
            // one value per statement, folded per group row.
            "SELECT sum((SELECT max(salary) FROM emp)) FROM emp",
            "SELECT dept_id, sum((SELECT max(budget) FROM dept)) FROM emp GROUP BY dept_id ORDER BY dept_id",
            "SELECT dept_id, count((SELECT id FROM dept WHERE id = 2)) FROM emp GROUP BY dept_id ORDER BY dept_id",
            "SELECT sum((SELECT salary FROM emp ORDER BY id LIMIT 1)) + count(*) FROM emp",
            // Correlated agg-arg control (per group row).
            "SELECT dept_id, sum((SELECT budget FROM dept WHERE dept.id = emp.dept_id)) FROM emp GROUP BY dept_id ORDER BY dept_id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- write-side snapshot semantics ----------------

/// Apply `writes` via hipp to one oracle-built copy and via
/// the oracle to a twin; final dumps must agree (the pattern
/// of tests/subqueries.rs, over a two-table setup of its own).
async fn diff_write(tag: &str, setup: &str, writes: &[&str], checks: &[&str]) {
    let a = oracle_db(&format!("{tag}-hipp"), setup);
    let b = oracle_db(&format!("{tag}-ora"), setup);
    {
        let fs = hipp::FileStorage::open(&a).expect("fs open");
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

const WSETUP: &str = "CREATE TABLE t (a INTEGER);
CREATE TABLE u (x INTEGER);
INSERT INTO t VALUES (1),(2),(3),(4);
INSERT INTO u VALUES (100),(200);";

#[test]
fn hoisted_write_snapshot_differential() {
    block_on(async {
        // The milestone's root find: the pin evaluates an
        // uncorrelated subquery once per statement, so an
        // UPDATE whose SET subquery reads the written table
        // sees the pre-statement snapshot on every row. The
        // v0.12 engine re-evaluated per row and produced
        // 6, 11, 20 here; the pin (and now hipp) produce
        // 10, 10, 10, 10 (sum of 1+2+3+4).
        diff_write(
            "v13-upd-sum",
            WSETUP,
            &["UPDATE t SET a = (SELECT sum(a) FROM t)"],
            &["SELECT a FROM t ORDER BY rowid"],
        )
        .await;
        // Same snapshot rule through a WHERE + SET pair of
        // distinct uncorrelated nodes over the written table.
        diff_write(
            "v13-upd-pair",
            WSETUP,
            &["UPDATE t SET a = (SELECT max(a) FROM t) WHERE a < (SELECT avg(a) FROM t)"],
            &["SELECT a FROM t ORDER BY rowid"],
        )
        .await;
        // DELETE against the running minimum: only the row
        // equal to the statement-start minimum is deleted.
        diff_write(
            "v13-del-min",
            WSETUP,
            &["DELETE FROM t WHERE a = (SELECT min(a) FROM t)"],
            &["SELECT a FROM t ORDER BY a"],
        )
        .await;
        // Multi-row INSERT VALUES: every row's subquery is
        // the statement-start maximum, not a climbing one.
        diff_write(
            "v13-ins-values",
            WSETUP,
            &["INSERT INTO t(a) VALUES ((SELECT max(a) FROM t) + 1), ((SELECT max(a) FROM t) + 1), ((SELECT max(a) FROM t) + 1)"],
            &["SELECT a FROM t ORDER BY rowid"],
        )
        .await;
        // INSERT ... SELECT whose expression subquery reads
        // the target table.
        diff_write(
            "v13-ins-select",
            WSETUP,
            &["INSERT INTO u SELECT (SELECT max(x) FROM u) + 1 FROM t"],
            &["SELECT x FROM u ORDER BY rowid"],
        )
        .await;
        // Uncorrelated subquery over a table the statement
        // does NOT write: ordinary cross-table case.
        diff_write(
            "v13-upd-cross",
            WSETUP,
            &["UPDATE t SET a = (SELECT max(x) FROM u) WHERE a IN (SELECT x - 99 FROM u)"],
            &["SELECT a FROM t ORDER BY rowid"],
        )
        .await;
    });
}

// ---------------- the win, counted ----------------

#[test]
fn execution_counts_drop_to_one() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (a INTEGER, g INTEGER)")
            .await
            .unwrap();
        exec(&mut db, "CREATE TABLE u (x INTEGER)").await.unwrap();
        // 200 outer rows, built in a handful of statements.
        for base in (0..200).step_by(4) {
            exec(
                &mut db,
                &format!(
                    "INSERT INTO t VALUES ({}, {}), ({}, {}), ({}, {}), ({}, {})",
                    base,
                    base % 4,
                    base + 1,
                    (base + 1) % 4,
                    base + 2,
                    (base + 2) % 4,
                    base + 3,
                    (base + 3) % 4
                ),
            )
            .await
            .unwrap();
        }
        exec(&mut db, "INSERT INTO u VALUES (10), (20), (30)")
            .await
            .unwrap();

        // Snapshot the cumulative counter around one statement
        // execution (the statement is dropped by collect()).
        macro_rules! count_during {
            ($sql:expr) => {{
                let before = db.subquery_executions();
                let _ = collect(&mut db, $sql).await;
                db.subquery_executions() - before
            }};
        }

        // Scalar / IN / EXISTS uncorrelated nodes: 1 execution
        // per statement, not 200.
        assert_eq!(
            count_during!("SELECT a FROM t WHERE a < (SELECT max(x) FROM u)"),
            1
        );
        assert_eq!(count_during!("SELECT (SELECT count(*) FROM u) FROM t"), 1);
        assert_eq!(
            count_during!("SELECT a FROM t WHERE a IN (SELECT x FROM u)"),
            1
        );
        assert_eq!(
            count_during!("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x > 5)"),
            1
        );
        // Two distinct uncorrelated nodes: exactly 2.
        assert_eq!(
            count_during!("SELECT (SELECT max(x) FROM u) + (SELECT min(x) FROM u) FROM t"),
            2
        );
        // Inside aggregate arguments, plain and grouped: 1.
        assert_eq!(
            count_during!("SELECT sum((SELECT max(x) FROM u)) FROM t"),
            1
        );
        assert_eq!(
            count_during!("SELECT g, sum((SELECT max(x) FROM u)) FROM t GROUP BY g ORDER BY g"),
            1
        );
        // The cache is per statement execution: running the
        // same statement again executes the subquery again.
        assert_eq!(
            count_during!("SELECT a FROM t WHERE a < (SELECT max(x) FROM u)"),
            1
        );
        // Nested: both plans are uncorrelated (the inner
        // node references only its own u), so each executes
        // exactly once for the whole statement -- the inner
        // node's plan id survives the outer plan's executor
        // clone, so the cache finds it across the outer
        // plan's per-row filter evaluations.
        assert_eq!(
            count_during!(
                "SELECT (SELECT count(*) FROM u WHERE u.x <= (SELECT max(x) FROM u)) FROM t"
            ),
            2
        );
        // Correlated control: still one execution per outer
        // row (200), bit-for-bit the pre-v0.13 cost.
        assert_eq!(
            count_during!("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.a)"),
            200
        );
        // A write statement over 200 rows: 1.
        let before = db.subquery_executions();
        exec(&mut db, "UPDATE t SET a = a + (SELECT max(x) FROM u)")
            .await
            .unwrap();
        assert_eq!(db.subquery_executions() - before, 1);
    });
}

// ---------------- hipp-built database ----------------

#[test]
fn hoist_on_hipp_built_db() {
    block_on(async {
        let path = common::temp_db("v13-built");
        {
            let fs = hipp::FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            for s in [
                "CREATE TABLE t (a INTEGER)",
                "INSERT INTO t VALUES (1),(2),(3)",
                "UPDATE t SET a = (SELECT sum(a) FROM t)",
            ] {
                exec(&mut db, s).await.expect("hipp build");
            }
        }
        // The UPDATE above must have produced the pin's
        // snapshot result (6 on every row) in hipp's own file.
        assert_eq!(
            common::cli(&path, "SELECT a FROM t ORDER BY rowid"),
            "6\n6\n6\n"
        );
        diff_query(
            &path,
            "SELECT a, (SELECT count(*) FROM t) FROM t ORDER BY a",
        )
        .await;
    });
}
