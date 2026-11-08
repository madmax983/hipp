//! v0.17 query flattening, WHERE push-down, unused subquery columns,
//! EXISTS-to-join and foreign-key child scans (leaf `planner`),
//! differentially vs the pinned oracle (SQLite 3.54.1).
//!
//! - curated plans: views, CTEs and FROM subqueries flattened into
//!   their reader (joins reordered through them, the LEFT JOIN's
//!   right side, the subquery's LIMIT and ORDER BY moving out, nested
//!   views), the restrictions that keep a co-routine or a
//!   materialisation (aggregate, DISTINCT, LIMIT under a join,
//!   MATERIALIZED, a complex result over an ordered subquery), WHERE
//!   terms pushed into the bodies that stay, `SEARCH t EXISTS`
//!   semi-join loops, and the child scans of a parent DELETE / UPDATE
//!   under `PRAGMA foreign_keys` — each hipp's EXPLAIN QUERY PLAN tree
//!   against the pin's shell on the same file, with the rows;
//! - errors in result columns the reader never reads, which the pin
//!   does not evaluate (flattened or not), against the pin's answers,
//!   and errors in the columns it does read (error against error);
//! - a generated corpus (hand-rolled PRNG): views, CTEs (MATERIALIZED
//!   hints too), FROM subqueries, EXISTS / IN subqueries and joins
//!   over random schemas, rows compared in order and as sets, plans
//!   as text, under thresholds;
//! - regressions for the bugs found while building it.

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};
use std::path::{Path, PathBuf};

type HErr = Error<FileError>;

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

type Answer = (Result<Vec<u8>, String>, Result<String, String>);

/// hipp's rows (or error) and plan of each query, on one connection
/// that first runs `setup` (PRAGMAs).
fn hipp_answers(path: &Path, setup: &str, qs: &[String]) -> Vec<Answer> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        if !setup.is_empty() {
            collect(&mut db, setup).await.expect("setup");
        }
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
fn pin(path: &Path, setup: &str, sql: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new(common::oracle());
    cmd.arg(path);
    if !setup.is_empty() {
        cmd.arg("-cmd").arg(setup);
    }
    let out = cmd.arg(sql).output().expect("oracle runs");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn fresh(tag: &str, schema: &str) -> PathBuf {
    let path = common::temp_db(&format!("v17fl-{tag}"));
    common::cli(&path, schema);
    path
}

/// Every query: hipp's rows in order and its plan are the pin's (or
/// both engines fail).
fn same_on(path: &Path, setup: &str, qs: &[&str]) {
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let got = hipp_answers(path, setup, &owned);
    let mut bad = Vec::new();
    for (q, (rows, plan)) in qs.iter().zip(got) {
        let p_rows = pin(path, setup, q);
        match (&rows, &p_rows) {
            (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin rows : {p_rows:?}\n  hipp rows: {:?}",
                rows.as_ref()
                    .map(|r| String::from_utf8_lossy(r).to_string())
            )),
        }
        let p_plan = pin(path, setup, &format!("EXPLAIN QUERY PLAN {q}"));
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

/// `same_on` for the rows (or errors) alone: the plans of these
/// differ in a documented way (STATUS.md's honest gaps).
fn same_rows_on(path: &Path, setup: &str, qs: &[&str]) {
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let got = hipp_answers(path, setup, &owned);
    let mut bad = Vec::new();
    for (q, (rows, _)) in qs.iter().zip(got) {
        let p_rows = pin(path, setup, q);
        match (&rows, &p_rows) {
            (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin rows : {p_rows:?}\n  hipp rows: {:?}",
                rows.as_ref()
                    .map(|r| String::from_utf8_lossy(r).to_string())
            )),
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// `same_on` for the plans alone (the statements are not run: writes).
fn same_plans_on(path: &Path, setup: &str, qs: &[&str]) {
    let mut bad = Vec::new();
    let plans: Vec<Result<String, String>> = block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        if !setup.is_empty() {
            collect(&mut db, setup).await.expect("setup");
        }
        let mut out = Vec::new();
        for q in qs {
            out.push(eqp_of(&mut db, q).await);
        }
        out
    });
    for (q, plan) in qs.iter().zip(plans) {
        let p_plan = pin(path, setup, &format!("EXPLAIN QUERY PLAN {q}"));
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

const SCHEMA: &str = "
CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);
CREATE INDEX tb ON t(b);
CREATE TABLE u(x, y);
CREATE INDEX ux ON u(x);
CREATE TABLE w(p INTEGER, q INTEGER);
CREATE INDEX wp ON w(p);
INSERT INTO t VALUES(1,2,3),(2,3,4),(3,4,5),(4,4,6),(5,NULL,7);
INSERT INTO u VALUES(1,10),(2,20),(5,50),(4,40),(NULL,0);
INSERT INTO w VALUES(1,1),(3,3),(4,2);
CREATE VIEW v AS SELECT a, b*2 AS bb FROM t WHERE c>3;
CREATE VIEW vagg AS SELECT b, count(*) n FROM t GROUP BY b;
CREATE VIEW vv AS SELECT * FROM v WHERE bb > 2;
CREATE VIEW vbad AS SELECT a, b, abs(a - 2 - 9223372036854775807) AS bad FROM t;
";

/// Flattening: views, CTEs and FROM subqueries merge into their
/// reader; the join planner orders the merged tables.
#[test]
fn flattened_plans_match_pin() {
    let path = fresh("flat", SCHEMA);
    same_on(
        &path,
        "",
        &[
            "SELECT * FROM v",
            "SELECT * FROM u, v WHERE v.a = u.x",
            "SELECT * FROM (SELECT a, b FROM t) s, u WHERE s.b = u.x",
            "SELECT * FROM u LEFT JOIN (SELECT a, b FROM t) s ON s.a = u.x",
            "SELECT * FROM u LEFT JOIN (SELECT a, 1 AS one FROM t) s ON s.a = u.x",
            "SELECT u.x, s.one, s.k FROM u LEFT JOIN (SELECT a, 1 AS one, coalesce(b, 9) k FROM t) s ON s.a = u.x",
            "SELECT * FROM u LEFT JOIN (SELECT a, b FROM t WHERE b > 2) s ON s.a = u.x",
            "SELECT u.x, count(*) FROM u LEFT JOIN (SELECT a, b FROM t) s ON s.a = u.x GROUP BY u.x",
            "SELECT * FROM (SELECT a FROM t ORDER BY b)",
            "SELECT * FROM (SELECT a, b FROM t ORDER BY b DESC) LIMIT 2",
            "SELECT * FROM (SELECT a, b FROM t ORDER BY b DESC LIMIT 2)",
            "SELECT * FROM (SELECT a, b FROM t LIMIT 2)",
            "WITH c AS (SELECT a, b FROM t) SELECT * FROM c, u WHERE c.a = u.x",
            "WITH c AS (SELECT a, b FROM t) SELECT * FROM c x, c y WHERE x.a = y.b",
            "WITH c AS NOT MATERIALIZED (SELECT a, b FROM t) SELECT * FROM c x, c y WHERE x.a = y.b",
            "SELECT * FROM (SELECT * FROM (SELECT a, b FROM t) WHERE b > 1) WHERE a < 3",
            "SELECT * FROM vv, u WHERE vv.a = u.x",
            "SELECT a, (SELECT max(y) FROM u WHERE u.x <= s.a) FROM (SELECT a FROM t) s",
            "SELECT * FROM (SELECT a, b FROM t) s NATURAL JOIN (SELECT a, c FROM t) r",
            "SELECT DISTINCT x FROM u, (SELECT b FROM t) s WHERE s.b > u.x",
            "SELECT * FROM u WHERE x IN (SELECT a FROM v)",
            "SELECT (SELECT count(*) FROM v WHERE v.a = u.x) FROM u",
            "SELECT * FROM (SELECT b FROM t) s, (SELECT x FROM u) r WHERE s.b = r.x ORDER BY 1",
        ],
    );
}

/// Bodies the pin does not flatten keep their co-routine or
/// materialisation, with WHERE terms pushed into them.
#[test]
fn kept_subqueries_and_push_down_match_pin() {
    let path = fresh("kept", SCHEMA);
    same_on(
        &path,
        "",
        &[
            "SELECT * FROM vagg WHERE b = 3",
            "SELECT * FROM vagg WHERE n = 3",
            "SELECT * FROM vagg WHERE b = 3 AND n > 0",
            "SELECT * FROM (SELECT DISTINCT b FROM t) WHERE b = 3",
            "SELECT * FROM (SELECT b FROM t LIMIT 5) WHERE b = 3",
            "SELECT * FROM vagg WHERE b = random()",
            "SELECT count(*) FROM (SELECT a FROM t ORDER BY b)",
            "SELECT abs(a) FROM (SELECT a, b FROM t ORDER BY b LIMIT 2)",
            "SELECT abs(a) FROM (SELECT a, b FROM t ORDER BY b)",
            "SELECT * FROM (SELECT a FROM t LIMIT 2) s, u WHERE s.a = u.y",
            "SELECT * FROM (SELECT b, count(*) n FROM t GROUP BY b) s, u WHERE s.b = u.x AND s.n > 0",
            "WITH c AS MATERIALIZED (SELECT a, b FROM t) SELECT * FROM c WHERE a > 1",
            "SELECT * FROM u FULL JOIN (SELECT a, b FROM t) s ON s.a = u.x",
            "SELECT * FROM (SELECT a, b FROM t) s WHERE a IN (SELECT x FROM u)",
        ],
    );
}

/// EXISTS terms of the WHERE clause become `SEARCH t EXISTS` loops;
/// the shapes the pin leaves alone stay subqueries.
#[test]
fn exists_to_join_matches_pin() {
    let path = fresh("exists", SCHEMA);
    same_on(
        &path,
        "",
        &[
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE b = 3)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x) AND EXISTS (SELECT 1 FROM t t2 WHERE t2.a = u.y)",
            "SELECT * FROM u WHERE NOT EXISTS (SELECT 1 FROM t WHERE t.b = u.x)",
            "SELECT * FROM u WHERE u.y > 5 OR EXISTS (SELECT 1 FROM t WHERE t.b = u.x)",
            "SELECT count(*) FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x)",
            "SELECT DISTINCT x FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t, t t2 WHERE t.b = u.x AND t2.a = t.a)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x LIMIT 1)",
            "SELECT * FROM u WHERE EXISTS (SELECT count(*) FROM t WHERE t.b = u.x)",
            "SELECT * FROM u, t WHERE t.a = u.x AND EXISTS (SELECT 1 FROM t t3 WHERE t3.b = u.y)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM (SELECT b FROM t) s WHERE s.b = u.x)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM v WHERE v.a = u.x)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x) ORDER BY y DESC",
            "SELECT * FROM u LEFT JOIN t ON t.a = u.x WHERE EXISTS (SELECT 1 FROM t t3 WHERE t3.b = u.y)",
            "SELECT (SELECT count(*) FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x))",
            "SELECT * FROM u WHERE EXISTS (SELECT DISTINCT b FROM t WHERE t.b = u.x)",
            "SELECT * FROM u JOIN t ON t.a = u.x AND EXISTS (SELECT 1 FROM t t3 WHERE t3.b = u.y)",
            "SELECT x, count(*) FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b = u.x) GROUP BY x",
            "SELECT * FROM u, t WHERE t.a = u.x AND EXISTS (SELECT 1 FROM w WHERE w.p = u.y)",
            "SELECT * FROM u, t WHERE t.a = u.x AND EXISTS (SELECT 1 FROM w WHERE w.p = t.b)",
            "SELECT * FROM u, t WHERE t.a = u.x AND EXISTS (SELECT 1 FROM w WHERE w.p = 3)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM w WHERE w.q = u.y)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.c = u.y) AND EXISTS (SELECT 1 FROM w WHERE w.q = u.y)",
            "SELECT * FROM u, t WHERE EXISTS (SELECT 1 FROM w WHERE w.q = u.y)",
            "SELECT * FROM u, t WHERE t.c = u.x AND EXISTS (SELECT 1 FROM w WHERE w.q = u.y)",
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.a > u.x) AND EXISTS (SELECT 1 FROM w WHERE w.q >= u.x)",
            "SELECT x, (SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM w WHERE w.p = t.a)) FROM u",
        ],
    );
}

/// The child scans of a parent DELETE / UPDATE under
/// `PRAGMA foreign_keys = ON` (none while it is OFF).
#[test]
fn foreign_key_child_scans_match_pin() {
    let schema = "
CREATE TABLE p(id INTEGER PRIMARY KEY, k UNIQUE, z);
CREATE TABLE c1(pid REFERENCES p(id), v);
CREATE TABLE c2(pk REFERENCES p(k));
CREATE INDEX c2pk ON c2(pk);
CREATE TABLE p2(x, y, PRIMARY KEY(x, y));
CREATE TABLE c3(a, b, FOREIGN KEY(a, b) REFERENCES p2(x, y));
CREATE TABLE c4(pid REFERENCES p ON DELETE CASCADE);
CREATE TABLE c5(r INTEGER PRIMARY KEY REFERENCES p(id));
CREATE TABLE c6(a, b, FOREIGN KEY(a, b) REFERENCES p2(x, y));
CREATE INDEX c6ab ON c6(a, b);
CREATE TABLE p3(id INTEGER PRIMARY KEY, up REFERENCES p3(id));
";
    let path = fresh("fk", schema);
    let qs = [
        "DELETE FROM p WHERE id = 1",
        "DELETE FROM p",
        "UPDATE p SET k = 5 WHERE id = 1",
        "UPDATE p SET z = 5 WHERE id = 1",
        "UPDATE p SET id = id + 1",
        "DELETE FROM p2",
        "UPDATE p2 SET y = 1",
        "DELETE FROM c1 WHERE v = 1",
        "UPDATE c1 SET pid = 2",
        "DELETE FROM p3 WHERE id = 2",
    ];
    same_on(&path, "PRAGMA foreign_keys = ON", &qs);
    // (Without foreign keys the pin truncates on a DELETE without
    // WHERE, a documented difference of `tests/eqp_v16.rs`.)
    let off: Vec<&str> = qs
        .iter()
        .copied()
        .filter(|q| !q.starts_with("DELETE FROM p") || q.contains("WHERE"))
        .collect();
    same_on(&path, "", &off);
}

/// A result column the reader never reads is not computed, so an
/// error in it is not raised — flattened or not; one it reads is.
#[test]
fn unused_columns_are_not_evaluated() {
    let path = fresh("unused", SCHEMA);
    same_on(
        &path,
        "",
        &[
            "SELECT a FROM vbad",
            "SELECT count(*) FROM vbad",
            "SELECT bad FROM vbad",
            "SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t LIMIT 5) x, u WHERE x.a = u.x",
            "SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t LIMIT 5) x WHERE x.a > 1",
            "SELECT x.bad FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t LIMIT 5) x, u WHERE x.a = u.x",
            "SELECT x.a FROM (SELECT DISTINCT a, abs(a - 2 - 9223372036854775807) bad FROM t) x",
            "SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t ORDER BY bad) x",
            "SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t UNION ALL SELECT x, 1 FROM u) x",
            "SELECT x.a FROM (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t UNION SELECT x, 1 FROM u) x",
            "WITH c AS MATERIALIZED (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t) SELECT a FROM c",
            "WITH c AS (SELECT a, abs(a - 2 - 9223372036854775807) bad FROM t) SELECT a FROM c",
            "SELECT u.x, s.one FROM u LEFT JOIN (SELECT a, 1 AS one, abs(a - 9223372036854775807 - 2) AS z FROM t) s ON s.a = u.x",
            "SELECT x.a FROM (SELECT a, max(b), abs(a - 2 - 9223372036854775807) bad FROM t GROUP BY a) x",
            "SELECT count(*) FROM (SELECT a, json('[') j FROM t LIMIT 3), u",
        ],
    );
}

/// Views over flattened bodies keep their column names, which the pin
/// gives before it flattens.
#[test]
fn view_column_names_survive_flattening() {
    let path = fresh("names", SCHEMA);
    common::cli(
        &path,
        "CREATE VIEW vn AS SELECT B FROM (SELECT b AS B FROM t); \
         CREATE VIEW vn2 AS SELECT a FROM (SELECT a FROM t) s; \
         CREATE TABLE ctas AS SELECT B FROM (SELECT b B FROM t);",
    );
    same_on(
        &path,
        "",
        &[
            "SELECT * FROM pragma_table_info('vn')",
            "SELECT * FROM pragma_table_info('vn2')",
            "SELECT * FROM pragma_table_info('ctas')",
            "SELECT B FROM vn ORDER BY 1",
        ],
    );
    block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        collect(
            &mut db,
            "CREATE TABLE ctas2 AS SELECT B FROM (SELECT b B FROM t)",
        )
        .await
        .expect("ctas");
        let got = collect(&mut db, "SELECT name FROM pragma_table_info('ctas2')")
            .await
            .expect("names");
        assert_eq!(String::from_utf8_lossy(&got), "B\n");
    });
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

/// A schema of three tables (random indexes, data with NULLs and
/// repeats) and four views over them.
fn gen_schema(r: &mut Rng) -> String {
    let mut s = String::new();
    s.push_str("CREATE TABLE t1(a INTEGER PRIMARY KEY, b INTEGER, c TEXT);\n");
    s.push_str("CREATE TABLE t2(x INTEGER, y INTEGER, z);\n");
    s.push_str("CREATE TABLE t3(k INTEGER, v INTEGER);\n");
    for (t, c) in [
        ("t1", "b"),
        ("t1", "c"),
        ("t2", "x"),
        ("t2", "y"),
        ("t3", "k"),
        ("t3", "v"),
    ] {
        if r.chance(45) {
            s.push_str(&format!("CREATE INDEX i_{t}_{c} ON {t}({c});\n"));
        }
    }
    for i in 1..=12 {
        let b = if r.chance(15) {
            "NULL".to_string()
        } else {
            r.n(6).to_string()
        };
        s.push_str(&format!(
            "INSERT INTO t1 VALUES({i}, {b}, '{}');\n",
            r.pick(&["p", "q", "r", "P"])
        ));
    }
    for _ in 0..10 {
        let x = if r.chance(15) {
            "NULL".to_string()
        } else {
            r.n(13).to_string()
        };
        s.push_str(&format!(
            "INSERT INTO t2 VALUES({x}, {}, {});\n",
            r.n(5),
            r.n(9)
        ));
    }
    for _ in 0..8 {
        s.push_str(&format!(
            "INSERT INTO t3 VALUES({}, {});\n",
            r.n(6),
            r.n(13)
        ));
    }
    s.push_str("CREATE VIEW va AS SELECT a, b, c FROM t1 WHERE b IS NOT NULL;\n");
    s.push_str("CREATE VIEW vg AS SELECT y, count(*) n, sum(z) s FROM t2 GROUP BY y;\n");
    s.push_str("CREATE VIEW vj AS SELECT t1.a, t2.y, t2.z FROM t1, t2 WHERE t1.b = t2.y;\n");
    s.push_str("CREATE VIEW vd AS SELECT DISTINCT k FROM t3;\n");
    // (Review: result columns that overflow, which only a reader that
    // computes them raises.)
    s.push_str("CREATE VIEW vbad AS SELECT a, b, abs(a - 9223372036854775807 - 2) bad FROM t1;\n");
    s.push_str(
        "CREATE VIEW vbadl AS SELECT x, y, abs(x - 9223372036854775807 - 2) bad FROM t2 LIMIT 7;\n",
    );
    s
}

/// One generated query.
fn gen_query(r: &mut Rng) -> String {
    // A FROM source and the columns it offers (qualified by `q`).
    let source = |r: &mut Rng, q: &str| -> (String, Vec<String>) {
        let (text, cols): (String, Vec<&str>) = match r.n(17) {
            12 => ("vbad".into(), vec!["a", "b"]),
            13 => ("vbadl".into(), vec!["x", "y"]),
            14 => ("c2".into(), vec!["a", "b"]),
            15 => (
                "(SELECT k, v, abs(k - 9223372036854775807 - 2) e FROM t3 LIMIT 6)".into(),
                vec!["k", "v"],
            ),
            0 => ("t1".into(), vec!["a", "b"]),
            1 => ("t2".into(), vec!["x", "y"]),
            2 => ("va".into(), vec!["a", "b"]),
            3 => ("vg".into(), vec!["y", "n"]),
            4 => ("vj".into(), vec!["a", "y"]),
            5 => ("vd".into(), vec!["k"]),
            6 => ("(SELECT a, b FROM t1 WHERE a > 2)".into(), vec!["a", "b"]),
            7 => ("(SELECT x, y FROM t2 ORDER BY y)".into(), vec!["x", "y"]),
            8 => ("(SELECT k, v FROM t3 LIMIT 5)".into(), vec!["k", "v"]),
            9 => (
                "(SELECT x AS a, y AS b FROM t2 UNION ALL SELECT k, v FROM t3)".into(),
                vec!["a", "b"],
            ),
            10 => ("c1".into(), vec!["a", "b"]),
            _ => ("t3".into(), vec!["k", "v"]),
        };
        (
            format!("{text} {q}"),
            cols.iter().map(|c| format!("{q}.{c}")).collect(),
        )
    };
    let with = match r.n(4) {
        0 => "WITH c1(a, b) AS (SELECT a, b FROM t1)",
        1 => "WITH c1(a, b) AS MATERIALIZED (SELECT a, b FROM t1)",
        2 => "WITH c1(a, b) AS NOT MATERIALIZED (SELECT a, b FROM t1 WHERE b > 1)",
        _ => {
            "WITH RECURSIVE c1(a, b) AS (SELECT 1, 1 UNION ALL SELECT a + 1, b * 2 FROM c1 WHERE a < 5)"
        }
    };
    // (A CTE with an overflowing column: the pin computes every column
    // of a reference it does not flatten.)
    let with = format!(
        "{with}, c2(a, b, e) AS {}(SELECT a, b, abs(a - 9223372036854775807 - 2) FROM t1{}) ",
        r.pick(&["", "", "NOT MATERIALIZED "]),
        r.pick(&["", " LIMIT 8", " WHERE b > 0"])
    );
    let n = 1 + r.n(3) as usize;
    let mut from = String::new();
    let mut cols: Vec<String> = Vec::new();
    for i in 0..n {
        let q = format!("s{i}");
        let (mut text, mut cs) = source(r, &q);
        // (c2 computes its overflowing column whenever it is computed
        // at all; hipp materialises a later source before the join,
        // where the pin does it at the first row that reaches it — so
        // c2 only leads.)
        if i > 0 && text.starts_with("c2 ") {
            text = format!("c1 {q}");
            cs = vec![format!("{q}.a"), format!("{q}.b")];
        }
        if i == 0 {
            from.push_str(&text);
        } else {
            let join = r.pick(&[", ", " JOIN ", " LEFT JOIN ", " CROSS JOIN "]);
            from.push_str(join);
            from.push_str(&text);
            if join.contains("JOIN ") && !join.contains("CROSS") {
                let l = cols[r.n(cols.len() as u64) as usize].clone();
                from.push_str(&format!(" ON {} = {l}", cs[0]));
            }
        }
        cols.extend(cs);
    }
    let col = |r: &mut Rng, cols: &[String]| cols[r.n(cols.len() as u64) as usize].clone();
    let mut terms: Vec<String> = Vec::new();
    for _ in 0..r.n(3) {
        let c = col(r, &cols);
        terms.push(match r.n(7) {
            0 => format!("{c} = {}", r.n(6)),
            1 => format!("{c} > {}", r.n(6)),
            2 => format!("{c} IS NOT NULL"),
            3 => format!("{c} = {}", col(r, &cols)),
            4 => format!("EXISTS (SELECT 1 FROM t3 WHERE t3.k = {c})"),
            5 => format!("{c} IN (SELECT y FROM t2)"),
            _ => format!("EXISTS (SELECT 1 FROM t2 WHERE t2.y = {c} AND t2.x > 2)"),
        });
    }
    let select = if r.chance(20) {
        format!("count(*), max({})", col(r, &cols))
    } else {
        let a = col(r, &cols);
        let b = col(r, &cols);
        format!("{a}, {b}")
    };
    let mut q = format!("{with}SELECT {select} FROM {from}");
    if !terms.is_empty() {
        q.push_str(" WHERE ");
        q.push_str(&terms.join(" AND "));
    }
    if !select.starts_with("count") && r.chance(40) {
        q.push_str(" ORDER BY 1, 2");
        if r.chance(30) {
            q.push_str(" LIMIT 4");
        }
    }
    q
}

fn sorted_lines(b: &[u8]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = b.split(|c| *c == b'\n').map(|l| l.to_vec()).collect();
    v.sort();
    v
}

/// The generated corpus: no row set differs (rows of a LIMITed query
/// without a full ORDER BY are not compared as sets) and no error
/// disagrees — overflowing result columns in a view, a LIMITed view, a
/// CTE and a FROM subquery included; at most 7% come in another order
/// and at most 45% print another plan (v0.17 review: measured 5.8% and
/// 39% over 360 queries, 6.0% and 43% over 3,600 — it was 10% and 50%:
/// co-routines and materialisations keep FROM order in a join with no
/// automatic index on them, and UNION ALL bodies are not flattened into
/// a compound reader — STATUS.md's honest gaps).
#[test]
fn generated_corpus_matches_pin() {
    let seeds: u64 = std::env::var("HIPP_FLAT_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let (mut total, mut set_bad, mut order_bad, mut plan_bad, mut err_bad) = (0, 0, 0, 0, 0);
    let mut report = Vec::new();
    for seed in 1..=seeds {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ (seed * 0x1000_0001));
        let path = fresh(&format!("gen{seed}"), &gen_schema(&mut r));
        let qs: Vec<String> = (0..12).map(|_| gen_query(&mut r)).collect();
        let got = hipp_answers(&path, "", &qs);
        for (q, (rows, plan)) in qs.iter().zip(got) {
            total += 1;
            let p_rows = pin(&path, "", q);
            match (&rows, &p_rows) {
                (Ok(h), Ok(p)) => {
                    if h.as_slice() != p.as_bytes() {
                        let limited = q.contains("LIMIT");
                        if !limited && sorted_lines(h) != sorted_lines(p.as_bytes()) {
                            set_bad += 1;
                            report.push(format!(
                                "ROWS {q}\n  pin : {p:?}\n  hipp: {:?}",
                                String::from_utf8_lossy(h)
                            ));
                        } else {
                            order_bad += 1;
                        }
                    }
                }
                (Err(_), Err(_)) => {}
                _ => {
                    err_bad += 1;
                    report.push(format!("ERR {q}\n  pin : {p_rows:?}\n  hipp: {rows:?}"));
                }
            }
            let p_plan = pin(&path, "", &format!("EXPLAIN QUERY PLAN {q}"));
            match (&plan, &p_plan) {
                (Ok(h), Ok(p)) if h == p => {}
                (Err(_), Err(_)) => {}
                _ => {
                    plan_bad += 1;
                    if std::env::var("HIPP_FLAT_SHOW").is_ok() {
                        eprintln!("PLAN {q}\n  pin : {p_plan:?}\n  hipp: {plan:?}");
                    }
                }
            }
        }
    }
    eprintln!(
        "flatten corpus: {total} queries, {set_bad} row sets, {order_bad} orders, {plan_bad} plans, {err_bad} error disagreements"
    );
    assert!(
        set_bad == 0 && err_bad == 0,
        "{} differ:\n{}",
        report.len(),
        report.join("\n")
    );
    assert!(
        order_bad * 100 <= total * 7,
        "{order_bad} of {total} in another order"
    );
    assert!(
        plan_bad * 100 <= total * 45,
        "{plan_bad} of {total} plans differ"
    );
}

/// Regressions found while building the unit.
#[test]
fn regressions() {
    let path = fresh("regr", SCHEMA);
    same_on(
        &path,
        "",
        &[
            // A LEFT JOIN's merged INTEGER PRIMARY KEY is a column of
            // the merged table, never guarded by the NULL row (the
            // rowid lookup stays usable).
            "SELECT * FROM u LEFT JOIN (SELECT a, b FROM t) s ON s.a = u.x",
            // An EXISTS over a FROM subquery stays a subquery (the pin
            // rewrites before it flattens the subquery's own FROM).
            "SELECT * FROM u WHERE EXISTS (SELECT 1 FROM (SELECT b FROM t) s WHERE s.b = u.x)",
            // A pushed-down body keeps the name EXPLAIN QUERY PLAN
            // gives it.
            "SELECT * FROM vagg WHERE b = 4",
            "SELECT * FROM (SELECT DISTINCT b FROM t) s WHERE s.b > 2",
            // A semi-join source ends after its first row per
            // combination, streamed or materialised.
            "SELECT x FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b >= u.x) ORDER BY x",
            "SELECT x FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.b >= u.x)",
            "SELECT count(*) FROM u, w WHERE EXISTS (SELECT 1 FROM t WHERE t.b >= u.x)",
        ],
    );
}

/// Review regressions (v0.17 flatten review): each against the pin's
/// rows (or error) and plan.
#[test]
fn review_regressions() {
    let path = fresh("review", SCHEMA);
    common::cli(
        &path,
        "CREATE TABLE r1(a INTEGER PRIMARY KEY); INSERT INTO r1 VALUES(1),(2),(3);
         CREATE VIEW rv AS SELECT a n, abs(-9223372036854775807-1) e FROM r1 LIMIT 3;
         CREATE VIEW rvb AS SELECT a n, b e FROM t LIMIT 3;
         CREATE TABLE m1(a, c); CREATE TABLE m2(x, y);
         CREATE INDEX m2x ON m2(x); CREATE INDEX m2y ON m2(y);
         INSERT INTO m1 VALUES(1,'a'),(2,'b');
         INSERT INTO m2 VALUES(1,'p'),(2,'q'),(3,'b');",
    );
    same_on(
        &path,
        "",
        &[
            // A CTE's reference the pin does not flatten computes every
            // column (it never omits a CTE's: the error is raised).
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT r.n FROM r, r1 WHERE r.n=r1.a",
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT n FROM r WHERE n>1",
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT r.n FROM r1 LEFT JOIN r ON r.n=r1.a",
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT n FROM r UNION ALL SELECT n FROM r",
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT (SELECT max(n) FROM r)",
            // A flattened CTE's unused column is gone with the merge.
            "WITH r(n,e) AS (SELECT a, abs(-9223372036854775807-1) FROM r1) SELECT n FROM r, r1 WHERE n=a",
            // A correlated FROM subquery computes every column; the
            // reader's correlation does not count.
            "SELECT (SELECT x FROM (SELECT r1.a x, abs(-9223372036854775807-1) e FROM r1 u LIMIT 1)) FROM r1",
            "SELECT (SELECT x FROM (SELECT u.a x, abs(-9223372036854775807-1) e FROM r1 u LIMIT 2) WHERE x>r1.a) FROM r1",
            // A view read twice: a copy that lost a column computes its
            // own rows (CO-ROUTINE + MATERIALIZE); copies that did not
            // share one MATERIALIZE.
            "SELECT r.n FROM rv r, rv r2 WHERE r.n=r2.n",
            "SELECT r.n, r.e, r2.e FROM rvb r, rvb r2 WHERE r.n=r2.n",
            "SELECT r.n, r2.e FROM rvb r, rvb r2 WHERE r.n=r2.n",
            "SELECT 1 FROM rvb r, rvb r2 WHERE r.n=r2.n",
            // A NOT MATERIALIZED CTE read twice in one FROM clause is
            // materialised once; one read inside an IN counts twice.
            "WITH g(x) AS (SELECT b FROM t LIMIT 2) SELECT * FROM t WHERE b IN (SELECT x FROM g WHERE x>1)",
            "WITH g(x) AS (SELECT b FROM t LIMIT 2) SELECT b IN (SELECT x FROM g WHERE x>1) FROM t",
            "WITH g(x) AS (SELECT b FROM t LIMIT 2) SELECT * FROM t WHERE a = (SELECT x FROM g WHERE x>1)",
            // A scalar subquery probing an index is shown with its loop.
            "SELECT * FROM t WHERE c IN (SELECT b FROM t LIMIT 1) AND a=(SELECT 2 FROM t LIMIT 1)",
            "SELECT * FROM t WHERE c IN (SELECT b FROM t LIMIT 1) AND a>(SELECT 1) AND a<(SELECT 3)",
            // A MULTI-INDEX OR under a kept subquery's loop: every outer
            // row's matches.
            "SELECT s.a, y FROM (SELECT a, c FROM m1 LIMIT 9) s, m2 WHERE m2.x=s.a OR m2.y=s.c",
            "WITH m AS MATERIALIZED (SELECT a, c FROM m1) SELECT m.a, y FROM m, m2 WHERE m2.x=m.a OR m2.y=m.c",
            "SELECT s.a, y FROM (SELECT a, c FROM m1 LIMIT 9) s LEFT JOIN m2 ON m2.x=s.a OR m2.y=s.c",
            // EXISTS-to-join beside a LEFT JOIN: the LEFT JOIN stays
            // (the pin simplifies it first).
            "SELECT * FROM t LEFT JOIN u ON x=a WHERE EXISTS (SELECT 1 FROM u w WHERE w.x=u.x)",
            "SELECT * FROM t LEFT JOIN u ON x=a WHERE EXISTS (SELECT 1 FROM u w WHERE NOT (w.x <> u.x))",
            // A WHERE term on a flattened LEFT JOIN body's constant
            // column makes it an inner join.
            "SELECT * FROM t LEFT JOIN (SELECT x, y, 1 one FROM u) s ON s.x=t.a WHERE s.one=1",
            // A row value beside an EXISTS.
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x=t.b) AND (a,b)=(2,3)",
            "SELECT * FROM t WHERE (a,b)=(2,3) AND EXISTS (SELECT 1 FROM u WHERE x=t.b)",
            // An uncorrelated EXISTS's own term is checked at its loop.
            "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x > 1000000)",
        ],
    );
    // (The pin builds an automatic index on the second
    // materialisation; hipp keeps FROM order without one.)
    same_rows_on(
        &path,
        "",
        &[
            "WITH r(n,e) AS NOT MATERIALIZED (SELECT a, abs(-9223372036854775807-1) FROM r1 LIMIT 3) SELECT r.n FROM r JOIN r r2 ON r.n=r2.n",
            "WITH g AS NOT MATERIALIZED (SELECT b, count(*) c FROM t GROUP BY b) SELECT * FROM g x, g y WHERE x.c=y.c",
            "WITH g AS NOT MATERIALIZED (SELECT b, count(*) c FROM t GROUP BY b) SELECT * FROM g x LEFT JOIN g y ON x.c=y.c",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x=a) AND (a,b)=(2,3)",
            // The LEFT JOIN simplification ignores a FULL / RIGHT
            // JOIN's ON clause (it made `t LEFT JOIN u` inner, losing
            // t's rows; hipp's RIGHT / FULL join reads whole tables).
            "SELECT * FROM t LEFT JOIN u ON t.a < u.x FULL JOIN w ON w.p = u.x ORDER BY 1, 2, 3, 4, 5, 6, 7",
            "SELECT * FROM t LEFT JOIN u ON t.a < u.x RIGHT JOIN w ON w.p = u.x ORDER BY 1, 2, 3, 4, 5, 6, 7",
        ],
    );
    // Their materialisations, though, are the pin's: one, shared.
    same_plans_on(
        &path,
        "",
        &[
            "WITH g AS NOT MATERIALIZED (SELECT b, count(*) c FROM t GROUP BY b) SELECT b FROM g x WHERE b IN (SELECT b FROM g)",
            "WITH g AS NOT MATERIALIZED (SELECT b, count(*) c FROM t GROUP BY b) SELECT (SELECT max(c) FROM g), b FROM g",
        ],
    );
}

/// More than 64 sources after the flattener merged the views: the
/// pin's "at most 64 tables in a join" (32 + 32 still joins).
#[test]
fn flattened_join_past_64_sources_is_refused() {
    for n in [32usize, 33] {
        let fr: Vec<String> = (0..n).map(|i| format!("t t{i}")).collect();
        let wh: Vec<String> = (1..n).map(|i| format!("t{i}.a=t{}.a+0", i - 1)).collect();
        let schema = format!(
            "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
             CREATE VIEW v AS SELECT t0.a a, t{}.a z FROM {} WHERE {};",
            n - 1,
            fr.join(","),
            wh.join(" AND ")
        );
        let path = fresh(&format!("wide{n}"), &schema);
        same_on(
            &path,
            "",
            &["SELECT count(*), sum(x.a), sum(y.z) FROM v x, v y WHERE x.a=y.z+1"],
        );
    }
}

/// UPDATE ... FROM computes only the FROM subqueries' columns it reads
/// (the pin flattens the view; the subquery stays), so an error in an
/// unread one is not raised.
#[test]
fn update_from_omits_unused_columns() {
    let schema = "CREATE TABLE t1(a INTEGER PRIMARY KEY, b); INSERT INTO t1 VALUES(1,10),(2,20);
         CREATE TABLE t4(a, b); INSERT INTO t4 VALUES(1,2);
         CREATE TABLE t2(x INT, y, z); INSERT INTO t2 VALUES(1,'p',100),(2,'q',200);
         CREATE VIEW v AS SELECT a, b, abs(-9223372036854775807-1) bad FROM t1;";
    for (tag, write, read) in [
        (
            "uf1",
            "UPDATE t4 SET b=s.a FROM (SELECT a, abs(-9223372036854775807-1) e FROM t1 LIMIT 3) s WHERE s.a=t4.a",
            "SELECT * FROM t4",
        ),
        (
            "uf2",
            "UPDATE t2 SET z = v.b FROM v WHERE v.a=t2.x",
            "SELECT * FROM t2",
        ),
        (
            "uf3",
            "UPDATE t2 SET z = v.bad FROM v WHERE v.a=t2.x",
            "SELECT * FROM t2",
        ),
    ] {
        let hp = fresh(tag, schema);
        let pp = fresh(&format!("{tag}p"), schema);
        let h = hipp_answers(&hp, "", &[write.to_string(), read.to_string()]);
        let pw = pin(&pp, "", write);
        assert_eq!(
            h[0].0.is_ok(),
            pw.is_ok(),
            "{write}: {:?} vs {pw:?}",
            h[0].0
        );
        let pr = pin(&pp, "", read).expect("pin read");
        assert_eq!(
            h[1].0
                .as_ref()
                .map(|r| String::from_utf8_lossy(r).to_string()),
            Ok(pr),
            "{write}"
        );
    }
}

/// An uncorrelated EXISTS made a join loop is linear: its own WHERE
/// term is checked at its loop (it ran the whole join below each of its
/// rows: minutes at this size in a debug build).
#[test]
fn uncorrelated_exists_join_is_linear() {
    let path = fresh(
        "exlin",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); CREATE TABLE u(x, y);
         WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<10000) INSERT INTO t SELECT n, n%7 FROM r;
         WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<10000) INSERT INTO u SELECT n, n%5 FROM r;",
    );
    let q = "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x > 1000000)".to_string();
    let start = std::time::Instant::now();
    let got = hipp_answers(&path, "", std::slice::from_ref(&q));
    let took = start.elapsed();
    assert_eq!(
        got[0]
            .0
            .as_ref()
            .map(|r| String::from_utf8_lossy(r).to_string())
            .ok(),
        pin(&path, "", &q).ok()
    );
    assert!(took.as_secs() < 30, "took {took:?}");
}

/// The foreign-key child scans of a parent write follow the pin's
/// rules for the parent value: its affinity (an IPK or INT parent never
/// searches a TEXT / BLOB / untyped child index) and its collation (a
/// NOCASE child index is no use for a BINARY key), a covering read only
/// when the index holds the whole key, and the INSERT side: REPLACE,
/// upsert DO UPDATE of a key, several rows, a deferred key.
#[test]
fn foreign_key_child_scan_rules_match_pin() {
    let schema = "
CREATE TABLE p(id INTEGER PRIMARY KEY, v);
CREATE TABLE c1(k REFERENCES p, j); CREATE INDEX c1k ON c1(k);
CREATE TABLE c2(k TEXT REFERENCES p); CREATE INDEX c2k ON c2(k);
CREATE TABLE c3(k INT REFERENCES p); CREATE INDEX c3k ON c3(k);
CREATE TABLE c4(k BLOB REFERENCES p); CREATE INDEX c4k ON c4(k);
CREATE TABLE pu(id INTEGER PRIMARY KEY, u TEXT UNIQUE);
CREATE TABLE d1(k REFERENCES pu(u)); CREATE INDEX d1k ON d1(k);
CREATE TABLE d2(k TEXT COLLATE NOCASE REFERENCES pu(u)); CREATE INDEX d2k ON d2(k);
CREATE TABLE d3(k TEXT REFERENCES pu(u)); CREATE INDEX d3k ON d3(k COLLATE NOCASE);
CREATE TABLE pq(id INTEGER PRIMARY KEY, q TEXT COLLATE NOCASE UNIQUE);
CREATE TABLE g1(k REFERENCES pq(q)); CREATE INDEX g1k ON g1(k);
CREATE TABLE g2(k TEXT REFERENCES pq(q)); CREATE INDEX g2k ON g2(k COLLATE NOCASE);
CREATE TABLE p2(a, b, PRIMARY KEY(a, b));
CREATE TABLE qc(x, y, FOREIGN KEY(y, x) REFERENCES p2(a, b)); CREATE INDEX qcx ON qc(y);
CREATE TABLE pd(id INTEGER PRIMARY KEY, v);
CREATE TABLE dd(k REFERENCES pd DEFERRABLE INITIALLY DEFERRED, j); CREATE INDEX ddk ON dd(k);
CREATE TABLE di(k REFERENCES pd, j); CREATE INDEX dik ON di(k);
CREATE TABLE s(x);
";
    let path = fresh("fkrules", schema);
    same_plans_on(
        &path,
        "PRAGMA foreign_keys = ON",
        &[
            "DELETE FROM p WHERE id = 1",
            "DELETE FROM pu WHERE id = 1",
            "DELETE FROM pq WHERE id = 1",
            "DELETE FROM p2",
            "UPDATE p SET id = 3 WHERE id = 1",
            "INSERT INTO p VALUES(1, 2)",
            "INSERT INTO p VALUES(1, 2), (3, 4)",
            "INSERT INTO p SELECT x, x FROM s",
            "INSERT OR IGNORE INTO p VALUES(1, 2)",
            "INSERT INTO p VALUES(1, 2) ON CONFLICT DO NOTHING",
            "INSERT INTO p VALUES(1, 2) ON CONFLICT(id) DO UPDATE SET v = 3",
            "INSERT INTO p VALUES(1, 2) ON CONFLICT(id) DO UPDATE SET id = 2",
            "REPLACE INTO p VALUES(1, 2)",
            "REPLACE INTO p SELECT x, x FROM s",
            "REPLACE INTO pu VALUES(1, 'a')",
            "INSERT INTO pu VALUES(1, 'a') ON CONFLICT(u) DO UPDATE SET id = 5",
            "INSERT INTO pd VALUES(1, 2)",
            "INSERT OR REPLACE INTO pd(v) VALUES(5)",
        ],
    );
}
