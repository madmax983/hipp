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
//!   columns; v0.17 review: COLLATE inside OR terms, one-value IN
//!   lists, an expression index's key, the IN operator's own index,
//!   NOCASE columns and `sqlite_stat1`'s `noskipscan` / `unordered` /
//!   `sz=`); rows compared in order, plans as text. No row set may
//!   differ; at most 1 in 400 orders and 3 in 200 plans (2 and 47 of
//!   4,000 queries measured at 400 seeds; a min() / max() whose bare
//!   columns come from another row of the same minimum counts as an
//!   order; STATUS.md lists the classes);
//! - a curated corpus of every form, with data;
//! - regressions for the bugs found on the way (the OR-to-IN rewrite
//!   ignoring the values' affinity, a lost row; the row-value
//!   comparison read as an OR of branches; the DISTINCT table of an
//!   aggregate query; the IN operator's Bloom filter; and the review's:
//!   COLLATE in an OR, the one-value IN, the expression OR-to-IN, the
//!   IN operator's index and Bloom collation, min() / max() SEARCH
//!   text and coverage, `sqlite_stat1` keywords, an equality's
//!   collation against the ORDER BY's);
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

/// Regressions for the review findings (v0.17 review): each a class
/// the pin plans otherwise.
#[test]
fn review_regressions() {
    // An explicit COLLATE anywhere in an OR term (outside a subquery):
    // the pin analyses it as no OR at all, neither a MULTI-INDEX OR
    // nor an IN; rows in rowid order.
    same(
        "rv1",
        "CREATE TABLE t(a, b, c); CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b); \
         CREATE INDEX tc ON t(c COLLATE NOCASE); CREATE TABLE r(k, j); \
         INSERT INTO t VALUES('x','y',1),('X','q',2),(1,'Y',3),('z','x',4),('y','X',5),('X','Y',6); \
         INSERT INTO r VALUES('x',1),('X',2),('Y',3);",
        &[
            "SELECT rowid FROM t WHERE a = 'X' OR b = 'x' COLLATE NOCASE",
            "SELECT rowid FROM t WHERE a=2 OR b='x' COLLATE BINARY",
            "SELECT rowid FROM t WHERE a=2 OR b COLLATE BINARY='x'",
            "SELECT rowid FROM t WHERE a=2 OR (b='x' COLLATE BINARY AND c=1)",
            "SELECT rowid FROM t WHERE a=2 OR b=lower('X' COLLATE BINARY)",
            "SELECT rowid FROM t WHERE a=2 OR b=1 OR c COLLATE NOCASE='x'",
            "SELECT rowid FROM t WHERE (a=2 OR b='x') COLLATE BINARY",
            "SELECT rowid FROM t WHERE a=2 OR b=(SELECT 'x' COLLATE BINARY)",
            "SELECT rowid FROM t WHERE (a=1 OR a=2) COLLATE NOCASE OR a=3",
            "SELECT a FROM t WHERE a='x' OR a='y' COLLATE BINARY",
            "SELECT a FROM t WHERE a='x' OR (a='y') COLLATE NOCASE",
            "SELECT a FROM t WHERE a<5 OR a=5 COLLATE BINARY",
            "SELECT a FROM t WHERE a='x' OR likely(a='y')",
            "SELECT a FROM t WHERE a='x' OR unlikely(a='y')",
            "SELECT t.rowid, r.rowid FROM r, t WHERE t.a=r.k OR t.b COLLATE NOCASE = r.k",
            "SELECT t.rowid, r.rowid FROM r, t WHERE t.a=r.k OR t.b = r.k",
        ],
    );
    // A one-value IN on a constant is the pin's `x = +v`: an equality
    // in the OR-to-IN rewrite and everywhere else.
    same(
        "rv2",
        "CREATE TABLE t(a TEXT, b INTEGER, c INTEGER, d BLOB); CREATE INDEX ti1 ON t(d, b DESC); \
         CREATE INDEX tbc ON t(b, c); \
         INSERT INTO t VALUES('a',1,1,''),('b',2,2,-2),('c',3,3,'-2'),('d',4,4,x''),('e',5,5,NULL), \
         ('f',6,6,-2.0),('g',3,1,7);",
        &[
            "SELECT rowid FROM t WHERE d='' OR d IN (-2)",
            "SELECT rowid FROM t WHERE d=1 OR d IN (-2)",
            "SELECT rowid FROM t WHERE d IN (1) OR d IN (-2)",
            "SELECT rowid FROM t WHERE d IN (1) OR d IN (-2, 7)",
            "SELECT rowid FROM t WHERE d IN (a) OR d IN (-2)",
            "SELECT rowid FROM t WHERE d IN (CAST(7 AS TEXT)) OR d=''",
            "SELECT rowid FROM t WHERE d IN (NULL) OR d=''",
            "SELECT rowid FROM t WHERE b IN (3) ORDER BY c",
            "SELECT rowid FROM t WHERE b IN (3) AND c>0 ORDER BY c DESC",
            "SELECT rowid FROM t WHERE rowid IN (3) OR rowid=5",
            "SELECT 'Y' IN ('y' COLLATE NOCASE), 1 IN (1.0), '1' IN (1)",
        ],
    );
    // An OR of equalities on an indexed expression: the pin's IN seek
    // on it; in a join, the MULTI-INDEX OR through the expression
    // index (its branches never covering by expressions).
    same(
        "rv3",
        "CREATE TABLE t(a, b); CREATE INDEX tex ON t(a*2); CREATE INDEX tb ON t(b); \
         CREATE TABLE r(k, j); \
         INSERT INTO t VALUES(7,1),(2,2),(7,3),(2,4),(NULL,5),('2',6); \
         INSERT INTO r VALUES(14,4),(4,1);",
        &[
            "SELECT rowid FROM t WHERE a*2=14 OR a*2=4",
            "SELECT rowid FROM t WHERE 14=a*2 OR 4=a*2",
            "SELECT rowid FROM t WHERE a*2=14 OR a*2 IN (4)",
            "SELECT t.rowid, r.rowid FROM r, t WHERE t.a*2=r.j OR t.a*2=r.k",
            "SELECT t.rowid, r.rowid FROM r, t WHERE t.a*2=r.j OR t.b=r.k",
            "SELECT t.rowid, r.rowid FROM r, t WHERE r.j=t.a*2 OR t.b=r.k",
            "SELECT t.rowid FROM r, t WHERE t.a*2=r.k OR t.a*2=5",
        ],
    );
    // The IN operator's Bloom filter only under BINARY comparisons; an
    // IN reading the right-hand table's rowids or index in place.
    same(
        "rv4",
        "CREATE TABLE t(a INTEGER, b TEXT COLLATE NOCASE, c, d TEXT); CREATE INDEX ta ON t(a); \
         CREATE TABLE u(a, e REAL, f TEXT COLLATE NOCASE, g TEXT, h INTEGER PRIMARY KEY, i UNIQUE); \
         CREATE INDEX ua ON u(a); CREATE INDEX uf ON u(f); CREATE INDEX ug ON u(g, e); \
         CREATE INDEX up ON u(e) WHERE e > 0; CREATE VIEW vu AS SELECT a FROM u; \
         CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID; \
         INSERT INTO t VALUES(1,'x',1,'x'),(2,'X',2,'y'),(3,'y',3,'1'); \
         INSERT INTO u VALUES(1,1.0,'X','x',5,7),(2,2.0,'y','1',6,8),('x',3,'z','q',7,9); \
         INSERT INTO w VALUES(1,'a'),('x','b');",
        &[
            "SELECT a FROM t WHERE t.b IN (SELECT a FROM u WHERE e>0)",
            "SELECT a FROM t WHERE t.c IN (SELECT f FROM u WHERE e>0)",
            "SELECT a FROM t WHERE t.c COLLATE NOCASE IN (SELECT a FROM u WHERE e>0)",
            "SELECT a FROM t WHERE t.c IN (SELECT a COLLATE NOCASE FROM u WHERE e>0)",
            "SELECT a FROM t WHERE t.b COLLATE BINARY IN (SELECT a FROM u WHERE e>0)",
            "SELECT a FROM t WHERE (t.c, t.b) IN (SELECT a, g FROM u WHERE e>0)",
            "SELECT a FROM t WHERE t.b IN (VALUES(1),(2))",
            "SELECT a FROM t WHERE t.c IN (SELECT a FROM u)",
            "SELECT a FROM t WHERE t.c NOT IN (SELECT a FROM u)",
            "SELECT a FROM t WHERE t.c IN (SELECT a FROM u ORDER BY a)",
            "SELECT a FROM t WHERE t.c IN (SELECT DISTINCT a FROM u)",
            "SELECT a FROM t WHERE t.c IN (SELECT a FROM u GROUP BY a)",
            "SELECT a FROM t WHERE t.c IN (SELECT a FROM vu)",
            "SELECT a FROM t WHERE t.c IN (SELECT h FROM u)",
            "SELECT a FROM t WHERE t.a IN (SELECT h FROM u)",
            "SELECT a FROM t WHERE t.a IN (SELECT i FROM u)",
            "SELECT a FROM t WHERE t.c IN (SELECT i FROM u)",
            "SELECT a FROM t WHERE t.d IN (SELECT g FROM u)",
            "SELECT a FROM t WHERE t.a IN (SELECT g FROM u)",
            "SELECT a FROM t WHERE t.b IN (SELECT f FROM u)",
            "SELECT a FROM t WHERE t.c IN (SELECT f FROM u)",
            "SELECT a FROM t WHERE (t.d, t.c) IN (SELECT g, e FROM u)",
            "SELECT a FROM t WHERE t.c IN (SELECT k FROM w)",
            "SELECT a FROM t WHERE t.c IN (SELECT v FROM w)",
            "SELECT a FROM t WHERE 5 IN (SELECT a FROM u)",
            "SELECT a, c IN (SELECT a FROM u) FROM t",
            "SELECT a FROM t WHERE c IN (SELECT a FROM u) AND a IN (SELECT a FROM u)",
        ],
    );
    // min() / max(): every loop prints SEARCH (FROM subqueries, CTEs,
    // virtual tables: max() offers generate_series its DESC order); an
    // index on the argument's expression covers; an aggregate query's
    // bare key expression does not.
    same(
        "rv5",
        "CREATE TABLE t(a); CREATE INDEX ta ON t(a); INSERT INTO t VALUES(1),(2),(NULL); \
         CREATE TABLE t2(a INTEGER, b); CREATE INDEX te ON t2(a+1); \
         INSERT INTO t2 VALUES(1,1),(2,2),(NULL,3);",
        &[
            "SELECT min(a) FROM (SELECT a FROM t ORDER BY a DESC LIMIT 3)",
            "WITH q AS MATERIALIZED (SELECT a FROM t) SELECT max(a) FROM q",
            "SELECT max(a) FROM (SELECT a FROM t UNION SELECT 5)",
            "SELECT max(value) FROM generate_series(1,5)",
            "SELECT min(value) FROM generate_series(1,5)",
            "SELECT max(value) FROM generate_series(1,5) WHERE value>2",
            "SELECT max(value+1) FROM generate_series(1,5)",
            "SELECT min(a+1) FROM t2",
            "SELECT max(a+1) FROM t2",
            "SELECT min(a+1), b FROM t2",
            "SELECT count(a+1) FROM t2",
            "SELECT a+1, max(a+1) FROM t2",
            "SELECT a+1 FROM t2 GROUP BY a+1",
            "SELECT a+1, count(*) FROM t2 GROUP BY a+1",
            "SELECT count(*) FROM t2 GROUP BY a+1",
            "SELECT count(*) FROM t2 GROUP BY a+1 ORDER BY a+1",
        ],
    );
    // A min() / max() over an IN probe folds every row, each value's
    // entries forward: the bare columns are the pin's.
    same(
        "rv6",
        "CREATE TABLE t0(c0 REAL, c1 INTEGER, c2 INTEGER); \
         CREATE INDEX t0i1 ON t0(c1, c2); CREATE INDEX t0i2 ON t0(c1); \
         INSERT INTO t0 VALUES(5,4,6),(0,'x',8),(7,'z',9),(6,'z',2),('y','z',3),(1,'z',NULL), \
         (6,'z',NULL),(2,7,4),(1,7,6),(9,7,NULL);",
        &[
            "SELECT max(c1), c2 FROM t0 WHERE c1 IN ('z', 7, 'x')",
            "SELECT min(c1), c2 FROM t0 WHERE c1 IN ('z', 7, 4)",
            "SELECT max(c1), c2 FROM t0 WHERE c1 IN (SELECT c1 FROM t0)",
            "SELECT max(c1), c2 FROM t0 WHERE c1 IN (SELECT c1 FROM t0 WHERE c1 < 8)",
            "SELECT min(c1), c2 FROM t0 WHERE c1 IN (SELECT c1 FROM t0 WHERE c1 < 5)",
        ],
    );
    // `sqlite_stat1`'s keywords: `noskipscan` (no skip-scan),
    // `unordered` (no range, ORDER BY or covering scan through the
    // index), `sz=N` (its row size).
    for kw in [
        "noskipscan",
        "unordered",
        "sz=200",
        "sz=1 noskipscan",
        "xnoskipscan",
    ] {
        same(
            &format!("rv7{}", kw.len()),
            &format!(
                "CREATE TABLE k(a INT, b INT, c TEXT, d); CREATE INDEX kab ON k(a, b); \
                 CREATE INDEX kc ON k(c); \
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300) \
                 INSERT INTO k SELECT i%3, i%50, 'c'||(i%17), i FROM n; ANALYZE; \
                 UPDATE sqlite_stat1 SET stat = stat || ' {kw}' WHERE idx = 'kab'; \
                 ANALYZE sqlite_schema;"
            ),
            &[
                "SELECT rowid, d FROM k WHERE b=7",
                "SELECT rowid, d FROM k WHERE a=1 ORDER BY a, b LIMIT 5",
                "SELECT rowid FROM k WHERE a>1 AND a<3",
                "SELECT a FROM k LIMIT 3",
                "SELECT a, b, rowid FROM k WHERE a>1 ORDER BY a LIMIT 4",
                "SELECT * FROM k WHERE c='c3' AND a=1",
            ],
        );
    }
    // An `=` term holds its column constant for an ORDER BY / GROUP BY
    // term of the same collation only (ungated: a NOCASE equality left
    // `Y` and `y` in rowid order).
    same(
        "rv8",
        "CREATE TABLE p(a TEXT, b); INSERT INTO p VALUES('y',1),('Y',2),('y',0); \
         CREATE TABLE t(a TEXT, b, c); CREATE INDEX tab ON t(a, b); INSERT INTO t SELECT a, b, 1 FROM p; \
         CREATE TABLE n(a TEXT COLLATE NOCASE, b); CREATE INDEX nab ON n(a, b); \
         INSERT INTO n SELECT * FROM p;",
        &[
            "SELECT * FROM p WHERE a = 'y' COLLATE NOCASE ORDER BY a",
            "SELECT * FROM p WHERE a = 'y' COLLATE NOCASE ORDER BY a, b",
            "SELECT * FROM t WHERE a = 'y' COLLATE NOCASE ORDER BY a",
            "SELECT * FROM t WHERE a = 'y' ORDER BY a COLLATE NOCASE",
            "SELECT * FROM n WHERE a = 'y' COLLATE BINARY ORDER BY a",
            "SELECT * FROM n WHERE a = 'y' ORDER BY a",
            "SELECT * FROM t WHERE a = 'y' COLLATE NOCASE GROUP BY a",
            "SELECT a, count(*) FROM p WHERE a = 'y' COLLATE NOCASE GROUP BY a",
        ],
    );
    // (An honest gap: after ANALYZE, an IN subquery on a small WITHOUT
    // ROWID table's PRIMARY KEY scans where the pin seeks; the rows
    // agree.)
    same_rows(
        "rv9",
        "CREATE TABLE t(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID; CREATE TABLE u(a, e); \
         INSERT INTO t VALUES(1,1,1),(2,2,2),(3,3,3),(4,4,4),(5,5,5); \
         INSERT INTO u VALUES(1,5),(2,6),(3,1),(4,0); ANALYZE;",
        &["SELECT * FROM t WHERE t.a IN (SELECT a FROM u WHERE e>4)"],
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
                let t = r.pick(&[
                    "INTEGER",
                    "TEXT",
                    "",
                    "REAL",
                    "NUMERIC",
                    "TEXT COLLATE NOCASE",
                ]);
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
        // An expression index (the OR-to-IN and MULTI-INDEX OR keys
        // of `c1*2`; v0.17 review).
        if r.chance(25) {
            out += &format!("CREATE INDEX {name}e ON {name}(c1*2);\n");
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
        // `sqlite_stat1`'s keywords (v0.17 review: `noskipscan`,
        // `unordered`, `sz=`).
        if r.chance(30) {
            let kw = r.pick(&["noskipscan", "unordered", "sz=40", "sz=1 noskipscan"]);
            out += &format!(
                "UPDATE sqlite_stat1 SET stat = stat || ' {kw}' WHERE idx IS NOT NULL \
                 AND rowid % 2 = {}; ANALYZE sqlite_schema;\n",
                r.n(2)
            );
        }
    }
    (out, tabs)
}

fn term(r: &mut Rng, al: &str, cols: &[String]) -> String {
    let c = format!("{al}{}", cols[r.n(cols.len() as u64) as usize]);
    let v = r.n(10);
    match r.n(18) {
        0 | 9 | 10 => format!("{c} = {v}"),
        1 => format!("{c} > {v}"),
        2 => format!("{c} < {v}"),
        3 => format!("{c} BETWEEN {v} AND {}", v + 3),
        4 => format!("{c} IN ({v}, {}, 'x')", v + 2),
        5 => format!("{c} IS NULL"),
        6 => format!("{c} >= {v} AND {c} <= {}", v + 2),
        7 => format!("{c} IN (SELECT c1 FROM t0 WHERE c1 < {v})"),
        8 => format!("{c} = '{}'", r.pick(&["x", "y", "z"])),
        // (v0.17 review: COLLATE inside an OR, the one-value IN, the
        // expression key, the IN operator's own index and collation.)
        12 => format!("{c} = '{}' COLLATE NOCASE", r.pick(&["X", "y"])),
        13 => format!("{c} COLLATE BINARY = {v}"),
        14 => format!("{c} IN ({v})"),
        15 => format!("{al}c1*2 = {}", 2 * v),
        16 => format!("{c} IN (SELECT c1 FROM t0)"),
        17 => format!("{c} COLLATE NOCASE IN (SELECT c2 FROM t0 WHERE c1 < {v})"),
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
                    // (A min() / max() query's bare columns come from the
                    // first row of the minimum its loop reaches: another
                    // plan, another row of the same minimum.)
                    let agg = |l: &[&str]| -> Vec<String> {
                        l.iter()
                            .map(|x| x.split('|').next().unwrap_or("").to_string())
                            .collect()
                    };
                    let bare = (q.starts_with("SELECT min(") || q.starts_with("SELECT max("))
                        && agg(&a) == agg(&b);
                    if a == b || q.contains(" LIMIT ") || bare {
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
    // (v0.17 review: 2 orders and 47 plans of 4,000 at 400 seeds, 1 and
    // 2 of 400 at the default 40, with the review's forms in the
    // corpus; the limits were 1 in 200 and 2 in 100.)
    assert!(
        order_bad.len() * 400 <= n,
        "{} of {n} row orders differ:\n{}",
        order_bad.len(),
        order_bad.join("\n")
    );
    assert!(
        plan_bad.len() * 200 <= n * 3,
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
