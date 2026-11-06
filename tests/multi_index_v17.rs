//! v0.17 index strategies (leaf `multi_index`), differentially vs the
//! pinned oracle (SQLite 3.54.1).
//!
//! The pin's `where.c` has loop kinds hipp's planner lacked in 0.16:
//! the MULTI-INDEX OR (each branch of an OR read through its own
//! index, rows in the pin's branch order, each once), the min() /
//! max() optimisation (a lone min(x) / max(x) read in x's order,
//! stopping at the first row: its bare columns are that row's), the
//! skip-scan of an index's leading column (only with `sqlite_stat1`
//! rows that justify it, `ANY(a) AND b=?`), the IN operator's
//! statistics rule, and whereLoopInsert's cost adjustment for one
//! table (`b = 2 AND c = 1` over an index on (b, c) uses both). hipp
//! now plans them the same way (`src/xmulti.rs`, `src/xplan.rs`). The
//! corpus here:
//!
//! - a generated corpus (hand-rolled PRNG) of single-table and join
//!   queries over random schemas (rowid, INTEGER PRIMARY KEY and
//!   WITHOUT ROWID tables, one- to three-column indexes, with and
//!   without the pin's ANALYZE) with OR terms (of comparisons, IN
//!   lists, IN subqueries, IS NULL, BETWEEN and AND groups), IN
//!   lists and subqueries, and min() / max() with and without bare
//!   columns; rows compared in order, plans as text. No row set may
//!   differ; at most 1 in 200 orders and 2 in 100 plans (1 and 53 of
//!   4,000 queries measured at 400 seeds; STATUS.md lists the
//!   classes);
//! - a curated corpus of every form, with data;
//! - regressions for the bugs found on the way (the OR-to-IN rewrite
//!   ignoring the values' affinity, a lost row; the row-value
//!   comparison read as an OR of branches; the DISTINCT table of an
//!   aggregate query; the IN operator's Bloom filter);
//! - performance: a two-branch OR over two indexed columns of a
//!   20,000-row table is far faster than the full scan it replaces.

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type HErr = Error<FileError>;

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> Vec<u8> {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    t.as_slice().to_vec()
}

/// One statement's rows in the CLI's list rendering.
async fn collect(db: &mut Connection<FileStorage>, sql: &str) -> Result<Vec<u8>, HErr> {
    let mut q = db.prepare(sql).await?;
    let mut out: Vec<u8> = Vec::new();
    while q.step().await? == Step::Row {
        for i in 0..q.column_count() {
            if i > 0 {
                out.push(b'|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => out.extend_from_slice(v.to_string().as_bytes()),
                Some(Value::Real(f)) => out.extend_from_slice(&fmt_real(f)),
                Some(Value::Text(b)) | Some(Value::Blob(b)) => out.extend_from_slice(b),
            }
        }
        out.push(b'\n');
    }
    Ok(out)
}

/// The pin shell's `eqp_render` of (id, parent, detail) rows.
fn render(rows: &[(i64, i64, String)]) -> String {
    fn level(rows: &[(i64, i64, String)], parent: i64, prefix: &mut String, out: &mut String) {
        let kids: Vec<&(i64, i64, String)> = rows.iter().filter(|r| r.1 == parent).collect();
        for (i, r) in kids.iter().enumerate() {
            let more = i + 1 < kids.len();
            out.push_str(prefix);
            out.push_str(if more { "|--" } else { "`--" });
            out.push_str(&r.2);
            out.push('\n');
            let n = prefix.len();
            prefix.push_str(if more { "|  " } else { "   " });
            level(rows, r.0, prefix, out);
            prefix.truncate(n);
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("QUERY PLAN\n");
    level(rows, 0, &mut String::new(), &mut out);
    out
}

async fn eqp_of(db: &mut Connection<FileStorage>, sql: &str) -> Result<String, String> {
    let mut st = db
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let mut rows = Vec::new();
    while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
        let id = st.column_i64(0).expect("id");
        let parent = st.column_i64(1).expect("parent");
        let d = String::from_utf8(st.column_bytes(3).expect("detail").to_vec()).expect("utf8");
        rows.push((id, parent, d));
    }
    Ok(render(&rows))
}

/// One query's answer: its rows (or the error) and its plan.
type Answer = (Result<Vec<u8>, String>, Result<String, String>);

fn hipp_answers(path: &Path, qs: &[String]) -> Vec<Answer> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut out = Vec::new();
        for q in qs {
            let rows = collect(&mut db, q).await.map_err(|e| format!("{e:?}"));
            let plan = eqp_of(&mut db, q).await;
            out.push((rows, plan));
        }
        out
    })
}

/// The pin's output of `sql` on `path`, or its error.
fn pin(path: &Path, sql: &str) -> Result<String, String> {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn fresh(tag: &str, schema: &str) -> PathBuf {
    let path = common::temp_db(&format!("v17mi-{tag}"));
    common::cli(&path, schema);
    path
}

/// Every query of `qs` on a file with `schema`: hipp's rows, in
/// order, and its plan are the pin's (or both engines fail).
fn same(tag: &str, schema: &str, qs: &[&str]) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let got = hipp_answers(&path, &owned);
    let mut bad = Vec::new();
    for (q, (rows, plan)) in qs.iter().zip(got) {
        let p_rows = pin(&path, q);
        match (&rows, &p_rows) {
            (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin rows : {p_rows:?}\n  hipp rows: {:?}",
                rows.as_ref()
                    .map(|r| String::from_utf8_lossy(r).to_string())
            )),
        }
        let p_plan = pin(&path, &format!("EXPLAIN QUERY PLAN {q}"));
        match (&plan, &p_plan) {
            (Ok(h), Ok(p)) if h == p => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin plan : {p_plan:?}\n  hipp plan: {plan:?}"
            )),
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// `same` for the rows alone.
fn same_rows(tag: &str, schema: &str, qs: &[&str]) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    for (q, (rows, _)) in qs.iter().zip(hipp_answers(&path, &owned)) {
        let h = rows.map(|r| String::from_utf8_lossy(&r).to_string());
        assert_eq!(h.ok(), pin(&path, q).ok(), "{q}");
    }
}

// ----------------------------------------------------------- curated

const SCHEMA: &str = "CREATE TABLE t(a, b, c, d); \
     CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b); CREATE INDEX tbc ON t(b, c); \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<60) \
     INSERT INTO t SELECT i%7, i%5, i%3, i FROM r; \
     INSERT INTO t VALUES (NULL, NULL, NULL, 100), ('x', 'y', 'z', 101); \
     CREATE TABLE u(x INTEGER PRIMARY KEY, y, z); CREATE INDEX uy ON u(y); \
     INSERT INTO u SELECT d, a, b FROM t WHERE d < 40; \
     CREATE TABLE w(k INTEGER, j, v, PRIMARY KEY(k)) WITHOUT ROWID; \
     CREATE INDEX wj ON w(j); \
     INSERT INTO w SELECT d, d%4, a FROM t WHERE d < 30;";

/// MULTI-INDEX OR on one table: branches of comparisons, ranges, IN
/// lists and subqueries, IS NULL, BETWEEN and AND groups; the rowid
/// and WITHOUT ROWID keys; covering branches; the pushed-down WHERE
/// terms; ORDER BY, LIMIT, DISTINCT and aggregates over it.
#[test]
fn multi_index_or_single_table() {
    same(
        "or1",
        SCHEMA,
        &[
            "SELECT d FROM t WHERE a=1 OR b=2",
            "SELECT * FROM t WHERE a=1 OR b>3",
            "SELECT d FROM t WHERE a IN (1,2) OR b=2",
            "SELECT d FROM t WHERE a=1 OR a=3 OR b=2",
            "SELECT d FROM t WHERE (a=1 AND d>3) OR b=2",
            "SELECT d FROM t WHERE a=1 OR (b=2 AND c=1)",
            "SELECT d FROM t WHERE (a=1 AND b=2) OR (c=1 AND b=3)",
            "SELECT d FROM t WHERE a<1 OR a>5",
            "SELECT d FROM t WHERE a=1 OR a>5",
            "SELECT d FROM t WHERE a IS NULL OR b=2",
            "SELECT d FROM t WHERE a=1 OR b IN (SELECT y FROM u WHERE x < 9)",
            "SELECT d FROM t WHERE a=1 OR b BETWEEN 1 AND 2",
            "SELECT d FROM t WHERE (a=1 OR b=2) AND (c=1 OR d=2)",
            "SELECT d FROM t WHERE a=1 OR rowid=5",
            "SELECT d FROM t WHERE a=1 OR d+1=2",
            "SELECT d FROM t WHERE a=1 OR c=2",
            "SELECT * FROM t WHERE (a=1 OR b=2) AND c=1",
            "SELECT * FROM t WHERE (a=1 OR b=2) AND c>1",
            "SELECT * FROM t WHERE (a=1 OR b=2) LIMIT 3",
            "SELECT * FROM t WHERE (a=1 OR b=2) ORDER BY a",
            "SELECT DISTINCT a FROM t WHERE a=1 OR b=2",
            "SELECT count(*) FROM t WHERE a=1 OR b=2",
            "SELECT a, b FROM t WHERE a=1 OR b=2",
            "SELECT b FROM t WHERE b=1 OR b>3",
            "SELECT * FROM t WHERE a='x' OR b='y'",
            "SELECT * FROM u WHERE x=3 OR y=2",
            "SELECT * FROM u WHERE x<3 OR y=2",
            "SELECT * FROM w WHERE k<7 OR j IS NULL",
            "SELECT count(*) FROM w WHERE k<7 OR j=2",
            "SELECT k FROM w WHERE j=1 OR k=5",
            "SELECT d FROM t WHERE a<3 OR a=3",
            "SELECT d FROM t WHERE (a>=2 AND a<=4) OR (a>=2 AND a<=4)",
            "SELECT d FROM t INDEXED BY ta WHERE a=1 OR b=2",
            "SELECT d FROM t NOT INDEXED WHERE a=1 OR b=2",
        ],
    );
}

/// UPDATE and DELETE read their rows through a MULTI-INDEX OR too
/// (collected first, so in rowid or key order).
#[test]
fn writes_use_multi_index_or() {
    let schema = "CREATE TABLE t(a,b,c); CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b); \
         INSERT INTO t VALUES(1,2,3),(2,1,4),(1,1,5),(3,3,6); \
         CREATE TABLE w(k INTEGER PRIMARY KEY, j, v); CREATE INDEX wj ON w(j); \
         INSERT INTO w VALUES (1,2,3),(2,2,3),(5,1,1); \
         CREATE TABLE x(k, j, v, PRIMARY KEY(k)) WITHOUT ROWID; CREATE INDEX xj ON x(j); \
         INSERT INTO x VALUES (1,2,3),(2,2,3),(5,1,1);";
    for (q, tab) in [
        ("DELETE FROM t WHERE a=1 OR b=1", "t"),
        ("UPDATE t SET c=c+1 WHERE a=1 OR b=1", "t"),
        ("UPDATE t SET c=c+1 WHERE a=1 OR b=1 RETURNING *", "t"),
        ("UPDATE t SET a=a+1 WHERE a=1 OR b=1 RETURNING *", "t"),
        ("DELETE FROM w WHERE k=1 OR j=1 RETURNING *", "w"),
        ("DELETE FROM x WHERE k=1 OR j=1 RETURNING *", "x"),
        ("UPDATE x SET v=9 WHERE k=2 OR j=2 RETURNING *", "x"),
    ] {
        // Each engine writes its own copy: the plan, the RETURNING
        // rows, then the table.
        let path = fresh("wr1", schema);
        let pin_path = fresh("wr2", schema);
        let (rows, plan) = hipp_answers(&path, &[q.to_string()]).remove(0);
        assert_eq!(
            plan.ok(),
            pin(&pin_path, &format!("EXPLAIN QUERY PLAN {q}")).ok(),
            "{q}"
        );
        let rows = rows.map(|r| String::from_utf8_lossy(&r).to_string());
        assert_eq!(rows.ok(), pin(&pin_path, q).ok(), "{q}");
        let sel = format!("SELECT * FROM {tab}");
        assert_eq!(pin(&path, &sel), pin(&pin_path, &sel), "{q}");
    }
}

/// MULTI-INDEX OR as a join loop: branches probing with an earlier
/// table's columns, the commuted copies (`INDEX 3`), LEFT JOIN, the
/// OR-to-IN rewrite competing with the OR loop.
#[test]
fn multi_index_or_in_joins() {
    let schema = "CREATE TABLE t(a INTEGER, b INTEGER, c INTEGER); CREATE INDEX tb ON t(b, a); \
         CREATE TABLE u(x INTEGER, y INTEGER); CREATE INDEX ux ON u(x); CREATE INDEX uy ON u(y); \
         CREATE TABLE v(p INTEGER, q INTEGER); CREATE INDEX vp ON v(p); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<30) \
         INSERT INTO t SELECT i%6, i%4, i FROM r; \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<25) \
         INSERT INTO u SELECT i%5, i%3 FROM r; \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<20) \
         INSERT INTO v SELECT i%7, i FROM r;";
    same(
        "or2",
        schema,
        &[
            "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.a = u.y",
            "SELECT t.c, u.rowid FROM u, t WHERE t.b = u.x OR t.b = u.y",
            "SELECT t.c, u.rowid FROM u, t WHERE t.c = u.x OR t.c = u.y",
            "SELECT t.c, u.rowid FROM u, t WHERE (t.c = u.x OR t.c = u.y) AND t.b=1",
            "SELECT t.c, v.q FROM t, v WHERE v.p = t.a OR v.p = t.b",
            "SELECT t.c, v.q FROM t, v WHERE t.a = v.p OR t.b = v.p",
            "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.a = u.x",
            "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.b = u.y",
            "SELECT t.c, u.rowid FROM u, t WHERE (t.a = u.x OR t.a = u.y) AND t.b = 3",
            "SELECT t.c, u.rowid FROM t LEFT JOIN u ON (u.x = t.a OR u.y = t.b)",
            "SELECT t.c, v.q FROM t JOIN v ON v.p = t.a WHERE t.b = 1 OR t.a = 2",
            "SELECT t.c, u.rowid FROM t, u WHERE u.rowid = t.c AND (u.x = 1 OR u.y = 2)",
            "SELECT max(u.y), t.c FROM t, u WHERE u.x = t.a OR u.y = t.b",
        ],
    );
}

/// The min() / max() optimisation: the plan (SEARCH, the index or
/// rowid order), and the bare columns of the first row in it — NULLs
/// last for min(), the reverse scan for max(), an IN or a range on
/// the argument, a WHERE the rows must pass, all-NULL arguments.
#[test]
fn min_max_lookups() {
    let schema = "CREATE TABLE t(a, b, c, d); CREATE INDEX ta ON t(a); \
         CREATE INDEX tbd ON t(b DESC); \
         INSERT INTO t VALUES (NULL,NULL,1,1),(3,3,1,2),(1,1,1,3),(3,3,1,4),(1,1,1,5), \
         (NULL,NULL,1,6),('x','x',1,7),('x','x',1,8),(X'41',X'41',1,9),(X'41',X'41',1,10); \
         CREATE TABLE n(a, d); CREATE INDEX na ON n(a); \
         INSERT INTO n VALUES (NULL,1),(NULL,2),(NULL,3); \
         CREATE TABLE m(a, d); INSERT INTO m VALUES (NULL,1),(NULL,2),(NULL,3); \
         CREATE TABLE k(a, b, d); CREATE INDEX kb ON k(b, a); \
         INSERT INTO k VALUES (NULL,1,1),(NULL,1,2),(5,1,3),(5,1,4),(NULL,1,5),(2,2,6),(NULL,2,7); \
         CREATE TABLE kd(a, d); CREATE INDEX kdd ON kd(a DESC); \
         INSERT INTO kd VALUES (NULL,1),(NULL,2),(4,3),(4,4),(NULL,5); \
         CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID; \
         INSERT INTO w VALUES (1,1),(2,2),(3,3),(5,4); \
         CREATE TABLE e(a, b);";
    same(
        "mm",
        schema,
        &[
            "SELECT max(a), d FROM t",
            "SELECT min(a), d FROM t",
            "SELECT max(b), d FROM t",
            "SELECT min(b), d FROM t",
            "SELECT max(a), d FROM t WHERE c=1",
            "SELECT max(a), d FROM t WHERE d>3",
            "SELECT min(a) FROM t WHERE a IS NULL",
            "SELECT max(a) FROM t WHERE a < 'x'",
            "SELECT max(a) FROM t WHERE a IN (1,3)",
            "SELECT max(a) FROM t WHERE a>2",
            "SELECT min(a) FROM t WHERE a>=1 AND a<'z'",
            "SELECT min(rowid), d FROM t",
            "SELECT max(rowid), d FROM t",
            "SELECT max(a)+1 FROM t",
            "SELECT max(a)+max(a) FROM t",
            "SELECT max(abs(a)) FROM t",
            "SELECT max(+a) FROM t",
            "SELECT max(a COLLATE nocase) FROM t",
            "SELECT max(a), min(a) FROM t",
            "SELECT max(a), count(*) FROM t",
            "SELECT max(a) FROM t GROUP BY c",
            "SELECT max(a) FROM t HAVING max(a)>1",
            "SELECT max(a) FILTER (WHERE d>1) FROM t",
            "SELECT max(DISTINCT a) FROM t",
            "SELECT DISTINCT max(a) FROM t",
            "SELECT max(a) FROM t LIMIT 1",
            "SELECT max(a) FROM t ORDER BY 1",
            "SELECT (SELECT max(a) FROM t)",
            "SELECT max(a) FROM t AS q",
            "SELECT min(a) FROM t INDEXED BY tbd",
            "SELECT min(a) FROM t NOT INDEXED",
            "SELECT min(a), d FROM n",
            "SELECT max(a), d FROM n",
            "SELECT min(a), d FROM m",
            "SELECT max(a), d FROM m",
            "SELECT min(a), d FROM n WHERE d>1",
            "SELECT max(a), d FROM n WHERE d>1",
            "SELECT min(a), d FROM k WHERE b=1",
            "SELECT max(a), d FROM k WHERE b=1",
            "SELECT min(a), d FROM k WHERE b=2 AND a IS NULL",
            "SELECT min(a), d FROM k WHERE b IN (1,2)",
            "SELECT max(a), d FROM k WHERE b IN (1,2)",
            "SELECT max(DISTINCT a) FROM k WHERE b=1",
            "SELECT min(a), d FROM kd",
            "SELECT max(a), d FROM kd",
            "SELECT min(a), d FROM kd WHERE d<3",
            "SELECT max(a), d FROM kd WHERE d<3",
            "SELECT min(k) FROM w",
            "SELECT max(v) FROM w",
            "SELECT max(k) FROM w WHERE k>1",
            "SELECT max(a) FROM e",
            "SELECT max(t.a), e.b FROM t, e",
            "SELECT max(t.a) FROM t LEFT JOIN e ON 1",
            "SELECT max(t.a), k.d FROM t, k WHERE k.b=t.c",
            "SELECT min(k.a), t.d FROM t, k WHERE k.b=t.c",
        ],
    );
}

/// Skip-scans need `sqlite_stat1` rows saying the leading column
/// repeats (18 rows or more per value); without them the index is
/// scanned or the table is.
#[test]
fn skip_scans_with_statistics() {
    let base = "CREATE TABLE t(a, b, c, d); CREATE INDEX tab ON t(a, b); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<2000) \
         INSERT INTO t SELECT i%3, i, i%50, i FROM r;";
    let qs = [
        "SELECT * FROM t WHERE b=5",
        "SELECT d FROM t WHERE b>5 AND b<9",
        "SELECT a, b FROM t WHERE b=5",
        "SELECT * FROM t WHERE b IN (5, 6)",
        "SELECT * FROM t WHERE a=1 AND b IN (4, 7, 10)",
        "SELECT * FROM t WHERE c=5",
        "SELECT * FROM t WHERE b=5 ORDER BY a DESC",
        "SELECT a FROM t WHERE b<10 ORDER BY a",
        "SELECT a, b FROM t WHERE b BETWEEN 5 AND 30 ORDER BY a DESC",
        "SELECT * FROM t WHERE b=5 OR b=7",
        "SELECT count(*) FROM t WHERE b>1990",
    ];
    same("skip0", base, &qs);
    same("skip1", &format!("{base} ANALYZE;"), &qs);
    // Statistics below the threshold: no skip-scan.
    same(
        "skip2",
        &format!(
            "{base} ANALYZE; DELETE FROM sqlite_stat1; \
             INSERT INTO sqlite_stat1 VALUES('t', 'tab', '2000 17 1');"
        ),
        &qs,
    );
}

/// The IN operator with statistics (an IN the index should not probe
/// is a filter), IN lists and subqueries on an index and the rowid,
/// their order, and the IN operator's Bloom filter.
#[test]
fn in_operator_plans() {
    let schema = "CREATE TABLE t(a, b, c, d); CREATE INDEX tab ON t(a, b); CREATE INDEX tc ON t(c); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<300) \
         INSERT INTO t SELECT i%2, i%150, i%10, i FROM r; \
         CREATE TABLE u(x, y); INSERT INTO u VALUES (1,2),(3,4),(5,6),(7,8);";
    let qs = [
        "SELECT d FROM t WHERE a=1 AND b IN (4, 7, 10, 3, 2)",
        "SELECT d FROM t WHERE a IN (0, 1) AND b IN (4, 7, 10)",
        "SELECT d FROM t WHERE c IN (1, 2) ORDER BY c DESC",
        "SELECT d FROM t WHERE rowid IN (5, 3, 9)",
        "SELECT d FROM t WHERE c IN (SELECT x FROM u)",
        "SELECT d FROM t WHERE d IN (SELECT x FROM u)",
        "SELECT d FROM t WHERE d NOT IN (SELECT x FROM u)",
        "SELECT d IN (SELECT x FROM u) FROM t WHERE c = 1",
        "SELECT d FROM t WHERE d IN (SELECT x FROM u WHERE y = t.c)",
        "SELECT d FROM t WHERE c IN (SELECT x FROM u) AND d IN (SELECT y FROM u)",
        "SELECT * FROM u WHERE x IN (SELECT c FROM t WHERE d IN (SELECT y FROM u))",
    ];
    same("in0", schema, &qs);
    same("in1", &format!("{schema} ANALYZE;"), &qs);
}

/// whereLoopInsert / whereLoopAdjustCost for one table: a loop with
/// more equality columns that costs the same as its prefix wins.
#[test]
fn single_table_cost_ties() {
    for schema in [
        "CREATE TABLE t(a,b,c); CREATE INDEX i ON t(b,c); CREATE INDEX j ON t(c);",
        "CREATE TABLE t(a,b,c); CREATE INDEX j ON t(c); CREATE INDEX i ON t(b,c);",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c); CREATE INDEX i ON t(b,c); CREATE INDEX j ON t(b);",
        "CREATE TABLE t(a,b,c); CREATE INDEX i ON t(b,c); CREATE INDEX j ON t(b);",
        "CREATE TABLE t(a,b,c); CREATE INDEX j ON t(b); CREATE INDEX i ON t(b,c);",
    ] {
        let s = format!(
            "{schema} WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<40) \
             INSERT INTO t SELECT n, n%4, n%3 FROM r;"
        );
        same(
            "ties",
            &s,
            &[
                "SELECT * FROM t WHERE b = 2 AND c = 1",
                "SELECT a FROM t WHERE b = 2 AND c = 1",
                "SELECT b FROM t WHERE b = 2 AND c > 1",
                "SELECT * FROM t WHERE b IN (1,2) AND c = 1",
            ],
        );
    }
}

/// Regressions for the bugs found on the way.
#[test]
fn regressions() {
    // The OR-to-IN rewrite probed a TEXT key with REAL values (rows
    // were lost; ungated: every build with `expr_index`).
    same(
        "reg1",
        "CREATE TABLE t0(c0 TEXT, c1 INTEGER, PRIMARY KEY(c0)) WITHOUT ROWID; \
         CREATE INDEX t0i0 ON t0(c0 DESC, c1); \
         CREATE TABLE t1(c0 INTEGER PRIMARY KEY, c1 REAL, c2 INTEGER); \
         CREATE INDEX t1i0 ON t1(c2 DESC, c1); \
         INSERT INTO t0 VALUES ('1',1),('2',2),('3',3),('9',4),('x',5); \
         INSERT INTO t1 VALUES (1,1.0,2),(2,2,3),(3,9,1),(4,NULL,9),(5,3,3);",
        &["SELECT x.c0, y.c2 FROM t0 x, t1 y ON (y.c1 = x.c0 OR y.c2 = x.c0)"],
    );
    // A row-value comparison is no OR of branches (its rows; the
    // pin's vector range on the key is not modelled).
    same_rows(
        "reg2",
        "CREATE TABLE c(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID; \
         INSERT INTO c VALUES (1,2,'q'),(2,1,'r'),(3,3,'s');",
        &[
            "SELECT * FROM c WHERE (a, b) > (1, 1)",
            "SELECT * FROM c WHERE (a, b) <= (2, 1)",
            "SELECT (1, 2) < (1, 3), (2, 1) >= (2, 1)",
        ],
    );
    // An aggregate query's DISTINCT table is shown (EXPLAIN QUERY
    // PLAN), and an uncorrelated IN subquery's Bloom filter.
    same(
        "reg3",
        "CREATE TABLE t(a, b); CREATE INDEX tb ON t(b); CREATE TABLE u(x, y); \
         INSERT INTO t VALUES (1,2),(2,3); INSERT INTO u VALUES (1,1);",
        &[
            "SELECT DISTINCT count(*) FROM t",
            "SELECT DISTINCT sum(a) FROM t",
            "SELECT * FROM t WHERE a IN (SELECT x FROM u)",
            "SELECT * FROM t WHERE b IN (SELECT x FROM u)",
            "SELECT * FROM t WHERE a IN (SELECT x FROM u WHERE y = t.b)",
            "SELECT a IN (SELECT x FROM u) FROM t",
        ],
    );
}

// ------------------------------------------------------------ corpus

/// A small xorshift PRNG (tests/fuzz_smoke.rs style).
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
    fn n(&mut self, k: u64) -> u64 {
        self.next() % k.max(1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.n(100) < pct
    }
    fn pick<'a>(&mut self, l: &[&'a str]) -> &'a str {
        l[self.n(l.len() as u64) as usize]
    }
}

struct Tab {
    name: String,
    cols: Vec<String>,
}

fn gen_schema(r: &mut Rng) -> (String, Vec<Tab>) {
    let mut out = String::new();
    let mut tabs = Vec::new();
    let nt = 1 + r.n(3) as usize;
    for ti in 0..nt {
        let n = 3 + r.n(3) as usize;
        let cols: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
        let ipk = r.chance(40);
        let wr = !ipk && r.chance(15);
        let mut defs = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            if i == 0 && ipk {
                defs.push(format!("{c} INTEGER PRIMARY KEY"));
            } else {
                let t = r.pick(&["INTEGER", "TEXT", "", "REAL", "NUMERIC"]);
                defs.push(format!("{c} {t}").trim().to_string());
            }
        }
        if wr {
            defs.push("PRIMARY KEY(c0)".into());
        }
        let name = format!("t{ti}");
        out += &format!(
            "CREATE TABLE {name}({}){};\n",
            defs.join(", "),
            if wr { " WITHOUT ROWID" } else { "" }
        );
        for k in 0..r.n(5) {
            let m = 1 + r.n(3) as usize;
            let mut ic: Vec<String> = Vec::new();
            for _ in 0..m {
                let c = &cols[if ipk && r.chance(80) {
                    1 + r.n(n as u64 - 1)
                } else {
                    r.n(n as u64)
                } as usize];
                if !ic.iter().any(|x| x.split(' ').next() == Some(c.as_str())) {
                    ic.push(format!("{c}{}", if r.chance(15) { " DESC" } else { "" }));
                }
            }
            let u = if r.chance(10) { "UNIQUE " } else { "" };
            out += &format!(
                "CREATE {u}INDEX IF NOT EXISTS {name}i{k} ON {name}({});\n",
                ic.join(", ")
            );
        }
        for j in 0..r.n(41) {
            let mut vals = Vec::new();
            for i in 0..n {
                if i == 0 && (ipk || wr) {
                    vals.push((j + 1).to_string());
                    continue;
                }
                let x = r.n(100);
                vals.push(if x < 12 {
                    "NULL".into()
                } else if x < 25 {
                    format!("'{}'", r.pick(&["x", "y", "z"]))
                } else {
                    r.n(10).to_string()
                });
            }
            out += &format!("INSERT OR IGNORE INTO {name} VALUES({});\n", vals.join(","));
        }
        tabs.push(Tab { name, cols });
    }
    if r.chance(30) {
        out += "ANALYZE;\n";
    }
    (out, tabs)
}

fn term(r: &mut Rng, al: &str, cols: &[String]) -> String {
    let c = format!("{al}{}", cols[r.n(cols.len() as u64) as usize]);
    let v = r.n(10);
    match r.n(12) {
        0 | 9 | 10 => format!("{c} = {v}"),
        1 => format!("{c} > {v}"),
        2 => format!("{c} < {v}"),
        3 => format!("{c} BETWEEN {v} AND {}", v + 3),
        4 => format!("{c} IN ({v}, {}, 'x')", v + 2),
        5 => format!("{c} IS NULL"),
        6 => format!("{c} >= {v} AND {c} <= {}", v + 2),
        7 => format!("{c} IN (SELECT c1 FROM t0 WHERE c1 < {v})"),
        8 => format!("{c} = '{}'", r.pick(&["x", "y", "z"])),
        _ => format!("{c} <= {v}"),
    }
}

fn or_term(r: &mut Rng, al: &str, cols: &[String]) -> String {
    let n = 2 + usize::from(r.chance(25));
    let mut parts = Vec::new();
    for _ in 0..n {
        if r.chance(25) {
            parts.push(format!("({} AND {})", term(r, al, cols), term(r, al, cols)));
        } else {
            parts.push(term(r, al, cols));
        }
    }
    format!("({})", parts.join(" OR "))
}

fn gen_single(r: &mut Rng, tabs: &[Tab]) -> String {
    let t = &tabs[r.n(tabs.len() as u64) as usize];
    let col = |r: &mut Rng| t.cols[r.n(t.cols.len() as u64) as usize].clone();
    if r.chance(20) {
        let f = r.pick(&["min", "max"]);
        let arg = if r.chance(15) {
            "rowid".to_string()
        } else {
            col(r)
        };
        let bare = if r.chance(40) {
            format!(", {}", col(r))
        } else {
            String::new()
        };
        let mut q = format!("SELECT {f}({arg}){bare} FROM {}", t.name);
        let mut ts = Vec::new();
        if r.chance(50) {
            ts.push(term(r, "", &t.cols));
        }
        if r.chance(30) {
            ts.push(or_term(r, "", &t.cols));
        }
        if !ts.is_empty() {
            q += &format!(" WHERE {}", ts.join(" AND "));
        }
        return q;
    }
    let sel = match r.n(3) {
        0 => "*".to_string(),
        1 => "count(*)".to_string(),
        _ => {
            let mut cs: Vec<String> = Vec::new();
            for _ in 0..1 + r.n(3) {
                let c = col(r);
                if !cs.contains(&c) {
                    cs.push(c);
                }
            }
            cs.join(", ")
        }
    };
    let mut ts = vec![or_term(r, "", &t.cols)];
    for _ in 0..r.n(3) {
        if r.chance(50) {
            ts.push(term(r, "", &t.cols));
        } else {
            ts.push(or_term(r, "", &t.cols));
        }
    }
    let k = r.n(ts.len() as u64) as usize;
    ts.swap(0, k);
    let mut q = format!("SELECT {sel} FROM {} WHERE {}", t.name, ts.join(" AND "));
    if sel != "count(*)" && r.chance(30) {
        q += &format!(
            " ORDER BY {}{}",
            col(r),
            if r.chance(30) { " DESC" } else { "" }
        );
    }
    if r.chance(15) {
        q += &format!(" LIMIT {}", 1 + r.n(4));
    }
    q
}

fn gen_join(r: &mut Rng, tabs: &[Tab]) -> String {
    if tabs.len() < 2 {
        return gen_single(r, tabs);
    }
    let i = r.n(tabs.len() as u64) as usize;
    let mut j = r.n(tabs.len() as u64 - 1) as usize;
    if j >= i {
        j += 1;
    }
    let (t1, t2) = (&tabs[i], &tabs[j]);
    let c1 = |r: &mut Rng| t1.cols[r.n(t1.cols.len() as u64) as usize].clone();
    let c2 = |r: &mut Rng| t2.cols[r.n(t2.cols.len() as u64) as usize].clone();
    let mut conds = Vec::new();
    match r.n(4) {
        0 => conds.push(format!(
            "(y.{} = x.{} OR y.{} = x.{})",
            c2(r),
            c1(r),
            c2(r),
            c1(r)
        )),
        1 => {
            conds.push(format!("y.{} = x.{}", c2(r), c1(r)));
            conds.push(or_term(r, "x.", &t1.cols));
        }
        2 => {
            conds.push(or_term(r, "y.", &t2.cols));
            conds.push(format!("y.{} = x.{}", c2(r), c1(r)));
        }
        _ => conds.push(format!(
            "(y.{} = x.{} OR y.{} > {})",
            c2(r),
            c1(r),
            c2(r),
            r.n(10)
        )),
    }
    let mut sel = format!("x.{}, y.{}", c1(r), c2(r));
    if r.chance(20) {
        sel = format!("max(y.{}), x.{}", c2(r), c1(r));
    }
    match r.n(3) {
        0 => format!(
            "SELECT {sel} FROM {} x, {} y WHERE {}",
            t1.name,
            t2.name,
            conds.join(" AND ")
        ),
        k => {
            let on = conds.remove(0);
            let jt = if k == 1 { "JOIN" } else { "LEFT JOIN" };
            let mut q = format!("SELECT {sel} FROM {} x {jt} {} y ON {on}", t1.name, t2.name);
            if !conds.is_empty() {
                q += &format!(" WHERE {}", conds.join(" AND "));
            }
            q
        }
    }
}

/// The generated corpus (see the module comment). `HIPP_MI_SEEDS`
/// runs more seeds.
#[test]
fn generated_corpus_matches_pin() {
    let seeds: u64 = std::env::var("HIPP_MI_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let from: u64 = std::env::var("HIPP_MI_FROM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let (mut n, mut order_bad, mut plan_bad, mut hard) =
        (0usize, Vec::new(), Vec::new(), Vec::new());
    for seed in from..=seeds {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (schema, tabs) = gen_schema(&mut r);
        let path = fresh(&format!("gen{seed}"), &schema);
        let qs: Vec<String> = (0..10)
            .map(|_| {
                if r.chance(35) {
                    gen_join(&mut r, &tabs)
                } else {
                    gen_single(&mut r, &tabs)
                }
            })
            .collect();
        let got = hipp_answers(&path, &qs);
        for (q, (rows, plan)) in qs.iter().zip(got) {
            n += 1;
            let p_rows = pin(&path, q);
            match (&rows, &p_rows) {
                (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
                (Ok(h), Ok(p)) => {
                    let h = String::from_utf8_lossy(h).to_string();
                    let mut a: Vec<&str> = h.lines().collect();
                    let mut b: Vec<&str> = p.lines().collect();
                    a.sort_unstable();
                    b.sort_unstable();
                    if a == b || q.contains(" LIMIT ") {
                        order_bad.push(format!("seed {seed}: {q}"));
                    } else {
                        hard.push(format!(
                            "seed {seed}: {q}\n{schema}  pin : {p:?}\n  hipp: {h:?}"
                        ));
                    }
                }
                (Err(_), Err(_)) => {}
                _ => hard.push(format!(
                    "seed {seed}: {q}\n  pin : {p_rows:?}\n  hipp: {rows:?}"
                )),
            }
            let p_plan = pin(&path, &format!("EXPLAIN QUERY PLAN {q}"));
            if plan != p_plan && !(plan.is_err() && p_plan.is_err()) {
                plan_bad.push(format!(
                    "seed {seed}: {q}\n  pin : {p_plan:?}\n  hipp: {plan:?}"
                ));
            }
        }
    }
    eprintln!(
        "{n} queries: {} other rows, {} other orders, {} other plans",
        hard.len(),
        order_bad.len(),
        plan_bad.len()
    );
    assert!(hard.is_empty(), "rows differ:\n{}", hard.join("\n"));
    assert!(
        order_bad.len() * 200 <= n,
        "{} of {n} row orders differ:\n{}",
        order_bad.len(),
        order_bad.join("\n")
    );
    assert!(
        plan_bad.len() * 100 <= n * 2,
        "{} of {n} plans differ:\n{}",
        plan_bad.len(),
        plan_bad.join("\n")
    );
    if !order_bad.is_empty() || !plan_bad.is_empty() {
        eprintln!("{}\n{}", order_bad.join("\n"), plan_bad.join("\n"));
    }
}

// ------------------------------------------------------- performance

/// Best of three timings of `sql` on `db`.
fn best_of_3(db: &mut Connection<FileStorage>, sql: &str) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let n = block_on(collect(db, sql)).expect("query").len();
        assert!(n > 0);
        best = best.min(t.elapsed());
    }
    best
}

/// A two-branch OR over two indexed columns of a 20,000-row table:
/// the MULTI-INDEX OR reads ~40 rows through the two indexes where
/// the full scan reads them all; measured net of a one-row baseline,
/// best of three each.
#[test]
fn multi_index_or_is_faster() {
    let path = fresh(
        "perf",
        "CREATE TABLE t(a, b, c); CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<20000) \
         INSERT INTO t SELECT i%1000, i%997, i FROM r;",
    );
    block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let plan = eqp_of(&mut db, "SELECT count(*) FROM t WHERE a = 5 OR b = 7")
            .await
            .expect("plan");
        assert!(plan.contains("MULTI-INDEX OR"), "{plan}");
        let base = best_of_3(&mut db, "SELECT c FROM t WHERE rowid = 5");
        let or = best_of_3(&mut db, "SELECT count(*) FROM t WHERE a = 5 OR b = 7");
        let scan = best_of_3(
            &mut db,
            "SELECT count(*) FROM t NOT INDEXED WHERE a = 5 OR b = 7",
        );
        let (or_net, scan_net) = (or.saturating_sub(base), scan.saturating_sub(base));
        eprintln!("baseline {base:?}, MULTI-INDEX OR {or:?}, full scan {scan:?}");
        assert!(
            or_net * 5 < scan_net,
            "MULTI-INDEX OR {or_net:?} vs full scan {scan_net:?} (net of {base:?})"
        );
        assert_eq!(
            block_on(collect(
                &mut db,
                "SELECT count(*) FROM t WHERE a = 5 OR b = 7"
            ))
            .ok(),
            block_on(collect(
                &mut db,
                "SELECT count(*) FROM t NOT INDEXED WHERE a = 5 OR b = 7"
            ))
            .ok()
        );
    });
}
