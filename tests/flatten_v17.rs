//! v0.17 query flattening, WHERE push-down, unused subquery columns,
//! EXISTS-to-join and foreign-key child scans (leaf `flatten`),
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
    s
}

/// One generated query.
fn gen_query(r: &mut Rng) -> String {
    // A FROM source and the columns it offers (qualified by `q`).
    let source = |r: &mut Rng, q: &str| -> (String, Vec<String>) {
        let (text, cols): (String, Vec<&str>) = match r.n(12) {
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
        0 => "WITH c1(a, b) AS (SELECT a, b FROM t1) ",
        1 => "WITH c1(a, b) AS MATERIALIZED (SELECT a, b FROM t1) ",
        2 => "WITH c1(a, b) AS NOT MATERIALIZED (SELECT a, b FROM t1 WHERE b > 1) ",
        _ => {
            "WITH RECURSIVE c1(a, b) AS (SELECT 1, 1 UNION ALL SELECT a + 1, b * 2 FROM c1 WHERE a < 5) "
        }
    };
    let n = 1 + r.n(3) as usize;
    let mut from = String::new();
    let mut cols: Vec<String> = Vec::new();
    for i in 0..n {
        let q = format!("s{i}");
        let (text, cs) = source(r, &q);
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
/// disagrees; at most 10% come in another order and at most 50% print
/// another plan (measured 8.9% and 45% over 360 queries: co-routines
/// and materialisations keep FROM order in a join, and UNION ALL
/// bodies are not flattened into a compound reader — STATUS.md's
/// honest gaps).
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
        order_bad * 100 <= total * 10,
        "{order_bad} of {total} in another order"
    );
    assert!(
        plan_bad * 100 <= total * 50,
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
