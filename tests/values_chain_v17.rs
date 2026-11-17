//! v0.17 parity unit `values_chain`: the result-level divergences
//! STATUS.md recorded against the pinned oracle (SQLite 3.54.1) for
//! VALUES chains, unknown collations, inert aggregates, nested
//! window ORDER BYs and view column types, each probed live on the
//! pin before the fix:
//!
//! - A multi-row VALUES is what the pin's `sqlite3MultiValues` makes
//!   of it row by row: a co-routine, or a UNION ALL chain of one-row
//!   selects (after a WITH clause began, in a view's or trigger's
//!   text, for a non-constant row, after a one-row select that is not
//!   constant or has an affinity; a later constant row starts a
//!   co-routine again). A reader's WHERE term is pushed into each
//!   chained row with its own affinity, into a later chain's
//!   `SELECT * FROM (chain)` wrapper with the chain's column affinity,
//!   and never into a chain of one-row selects throughout
//!   (`SF_MultiValue`); a wrapper's REAL column reads every integer as
//!   a real. EXPLAIN QUERY PLAN shows `SCAN CONSTANT ROW` per chained
//!   row, `SCAN n CONSTANT ROWS` for a pure chain, `0-ROW VALUES
//!   CLAUSE` for a FROM item over one.
//! - An unknown collation is looked up only where the pin does: a
//!   comparison, an IN list's left operand, min / max / nullif, a
//!   sort or grouping key, a DISTINCT, a compound's merge key, a FROM
//!   subquery's columns; `SELECT 1 COLLATE bogus` runs.
//! - An enclosing query's aggregate in a dropped ORDER BY or an
//!   EXISTS result list steps in its aggregate owner, so its errors
//!   are raised.
//! - Inside a dropped ORDER BY, a nested window query's own ORDER BY
//!   raises no code-generation error.
//! - A view's column read through a compound FROM subquery takes its
//!   declared type from the subquery's rightmost core.
//!
//! Each query's rows (in order, or the error) and, where shapes
//! allow, its EXPLAIN QUERY PLAN must be the pin's (an error on both
//! counts as agreement). With the hidden `vm` feature they run on
//! today's executor (`Config::vm_off`).
#![cfg(feature = "sql")]

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};
use std::path::{Path, PathBuf};

type HErr = Error<FileError>;

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> Vec<u8> {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    t.as_slice().to_vec()
}

/// One statement's rows in the shell's list rendering.
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
#[cfg(feature = "explain")]
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

#[cfg(feature = "explain")]
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

/// Without `explain` there is no plan to compare.
#[cfg(not(feature = "explain"))]
async fn eqp_of(_db: &mut Connection<FileStorage>, _sql: &str) -> Result<String, String> {
    Err(String::new())
}

/// One query's answer: its rows (or the error) and its plan.
type Answer = (Result<Vec<u8>, String>, Result<String, String>);

fn hipp_answers(path: &Path, qs: &[String]) -> Vec<Answer> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        // (The frozen VM, `vm`, keeps its own OFFSET / ORDER BY
        // handling: these shapes run on today's executor.)
        #[cfg(feature = "vm")]
        let cfg = Config::default().vm_off(true);
        #[cfg(not(feature = "vm"))]
        let cfg = Config::default();
        let mut db = Connection::open(fs, cfg).await.expect("open");
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
    if out.status.success() && out.stderr.is_empty() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn fresh(tag: &str, schema: &str) -> PathBuf {
    let path = common::temp_db(&format!("v17vc-{tag}"));
    common::cli(&path, schema);
    path
}

/// Whether a plan difference is the pre-existing gap `gaps_plan_cli`
/// recorded: the pin flattens a compound FROM subquery whose arms
/// agree in affinity into its reader, hipp keeps a co-routine.
fn compound_flattened(pin_plan: &str, hipp_plan: &str) -> bool {
    (pin_plan.starts_with("QUERY PLAN\n`--COMPOUND QUERY")
        || pin_plan.starts_with("QUERY PLAN\n`--MERGE (")
        || pin_plan.starts_with("QUERY PLAN\n|--COMPOUND QUERY")
        || pin_plan.starts_with("QUERY PLAN\n|--MERGE ("))
        && hipp_plan.starts_with("QUERY PLAN\n|--CO-ROUTINE")
}

/// Every difference of `qs` on the file at `path`: rows in order and
/// (with `plans`) plans.
fn diffs_on(path: &Path, qs: &[String], plans: bool) -> Vec<String> {
    let got = hipp_answers(path, qs);
    let mut bad = Vec::new();
    for (q, (rows, plan)) in qs.iter().zip(got) {
        let p_rows = pin(path, q);
        match (&rows, &p_rows) {
            (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin rows : {p_rows:?}\n  hipp rows: {:?}",
                rows.as_ref()
                    .map(|r| String::from_utf8_lossy(r).to_string())
            )),
        }
        if !plans || !cfg!(feature = "explain") {
            continue;
        }
        let p_plan = pin(path, &format!("EXPLAIN QUERY PLAN {q}"));
        match (&plan, &p_plan) {
            (Ok(h), Ok(p)) if h == p || compound_flattened(p, h) => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{q}\n  pin plan : {p_plan:?}\n  hipp plan: {plan:?}"
            )),
        }
    }
    bad
}

fn same(tag: &str, schema: &str, qs: &[&str], plans: bool) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs_on(&path, &owned, plans);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// A script of statements (separated by `;;`, which the pin's shell
/// reads as a statement and an empty one) run in order on a fresh
/// file by each engine, stopping at the first error: the same rows
/// and the same error presence (DDL the pin accepts or refuses
/// included).
fn script_same(tag: &str, schema: &str, scripts: &[&str]) {
    let mut bad = Vec::new();
    for (k, sc) in scripts.iter().enumerate() {
        let path = fresh(&format!("{tag}{k}"), schema);
        let p = pin(&path, sc);
        let path2 = fresh(&format!("{tag}{k}h"), schema);
        let h: Result<String, String> = block_on(async {
            let fs = FileStorage::open(&path2).expect("fs open");
            #[cfg(feature = "vm")]
            let cfg = Config::default().vm_off(true);
            #[cfg(not(feature = "vm"))]
            let cfg = Config::default();
            let mut db = Connection::open(fs, cfg).await.expect("open");
            let mut out = Vec::new();
            for st in sc.split(";;").map(str::trim).filter(|s| !s.is_empty()) {
                match collect(&mut db, st).await {
                    Ok(r) => out.extend_from_slice(&r),
                    Err(e) => return Err(format!("{e:?}")),
                }
            }
            Ok(String::from_utf8_lossy(&out).to_string())
        });
        match (&p, &h) {
            (Ok(a), Ok(b)) if a == b => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!("{sc}\n  pin : {p:?}\n  hipp: {h:?}")),
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

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
        self.next() % k
    }
    fn pick<'a>(&mut self, l: &[&'a str]) -> &'a str {
        l[self.n(l.len() as u64) as usize]
    }
}

/// Columns of every affinity, and values whose text and numeric forms
/// differ by affinity.
const S: &str = "CREATE TABLE t(r REAL, s TEXT, i INTEGER, b BLOB, n); \
    INSERT INTO t VALUES (3, '3', 3, 3, 3), (1.5, 'x', 2, x'41', NULL), (4, '4.0', 4, '4', '4'); \
    CREATE TABLE u(a INTEGER, b TEXT, c); \
    INSERT INTO u VALUES (1,'x',1),(2,'Y',2),(3,'z',NULL),(-9223372036854775807,'w',4);";

// ------------------------------------------------------ VALUES chains

/// The pin pushes a reader's WHERE term into each chained row with
/// its own affinity, into a later chain's wrapper with the chain's
/// column affinity, and never into a chain of one-row selects
/// throughout.
#[test]
fn chain_push_down_matches_pin() {
    let qs = [
        // The recorded gap: `3.0` once (the chained `3` fails `= '3'`).
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x = '3'",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x, typeof(x) FROM q",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x = 3",
        "WITH q(x) AS (VALUES (3), ('4') UNION ALL SELECT s FROM t) SELECT x FROM q WHERE x = 4",
        "WITH q(x) AS (VALUES (3), ('4') UNION ALL SELECT i FROM t) SELECT x FROM q WHERE x = '4'",
        "WITH q(x) AS (VALUES (3), ('4') UNION ALL SELECT i FROM t) SELECT x FROM q WHERE x > '2'",
        "WITH q(x) AS (VALUES ('3'), (3) UNION ALL SELECT i FROM t) SELECT x, typeof(x) FROM q WHERE x = 3",
        "WITH q(x) AS (VALUES (3), (NULL) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x IS NOT NULL",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x IN ('3', '4')",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x BETWEEN '3' AND '4'",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT count(*) FROM q WHERE x = '3'",
        "WITH q(x) AS (VALUES (3), (4) UNION SELECT r FROM t) SELECT x FROM q WHERE x = '3'",
        "WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (3), ('3')) SELECT x FROM q WHERE x = '3'",
        "WITH q(x) AS (SELECT s FROM t UNION ALL VALUES (3), ('3')) SELECT x FROM q WHERE x = 3",
        // A later chain: its wrapper's REAL column (no push into it).
        "WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (3), (CAST(4 AS REAL))) SELECT x FROM q WHERE x = '3'",
        "WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (3), (CAST(4 AS REAL))) SELECT x, typeof(x) FROM q",
        "WITH w AS (SELECT 1) SELECT x FROM (SELECT 1 AS x UNION ALL VALUES (3), (CAST(4 AS REAL))) WHERE x = '3'",
        "WITH w AS (SELECT 1) SELECT x FROM (SELECT s AS x FROM t UNION ALL VALUES (3), (CAST(4 AS REAL))) WHERE x = 3",
        "WITH w AS (SELECT 1) SELECT x FROM (SELECT s AS x FROM t UNION ALL VALUES ('3'), (CAST(4 AS INT))) WHERE x = 3",
        "WITH q(x) AS (SELECT i FROM t UNION ALL VALUES (CAST(3 AS REAL)), ('3'), (3)) SELECT x, typeof(x) FROM q WHERE x = '3'",
        "WITH q(x) AS (SELECT n FROM t UNION ALL VALUES (CAST(3 AS TEXT)), (3), ('3')) SELECT x, typeof(x) FROM q WHERE x = 3",
        "WITH q(x) AS (SELECT n FROM t UNION ALL VALUES (CAST(3 AS TEXT)), (3), ('3')) SELECT x, typeof(x) FROM q WHERE x > 2",
        "WITH q(x) AS (SELECT s FROM t UNION ALL VALUES (CAST(3 AS TEXT)), (3)) SELECT x, typeof(x) FROM q WHERE x = '3'",
        // A chain of one-row selects throughout takes no term.
        "WITH w AS (SELECT 1) SELECT column1 FROM (VALUES (3), (CAST(4 AS REAL))) WHERE column1 = '3'",
        "WITH w AS (SELECT 1) SELECT column1 FROM (VALUES (3), (CAST(4 AS REAL)) UNION ALL SELECT 5) WHERE column1 = '3'",
        "WITH q(x) AS (VALUES (3), (4)) SELECT x FROM q WHERE x = '3'",
        "WITH q(x) AS (VALUES (CAST(3 AS TEXT)), (3)) SELECT x, typeof(x) FROM q WHERE x = 3",
        "WITH q(x) AS (VALUES (CAST(3 AS TEXT)), (3)) SELECT x, typeof(x) FROM q WHERE x = '3'",
        // A result alias in the WHERE is the item it names.
        "SELECT column1 AS x FROM (VALUES (3.0), (4) UNION ALL SELECT i FROM t) WHERE x <> '3'",
        "SELECT i AS x FROM (SELECT i FROM t UNION ALL SELECT s FROM t) WHERE x = '3'",
        "SELECT s AS i FROM (SELECT i, s FROM t UNION ALL SELECT s, i FROM t) WHERE i = '3'",
        // Co-routines (no WITH): untyped, no push.
        "SELECT column1 FROM (VALUES (3), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT x FROM (SELECT r AS x FROM t UNION ALL VALUES (3), ('3')) WHERE x = '3'",
        // Non-constant rows chain (no WITH needed); a later constant
        // row after a constant one starts a co-routine again.
        "SELECT column1, typeof(column1) FROM (VALUES (3), ((SELECT 3)), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT column1, typeof(column1) FROM (VALUES (3), ((SELECT 3)), (4), (5) UNION ALL SELECT r FROM t) WHERE column1 = '4'",
        "SELECT column1 FROM (VALUES (1), (2), ((SELECT 3)), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT column1 FROM (VALUES (CAST(3 AS INT)), (3), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT column1 FROM (VALUES (1), (2), (random() % 1 + 3) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT column1 FROM (VALUES (1), (2), (abs(-3)) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "SELECT x, typeof(x) FROM (SELECT r AS x FROM t UNION ALL VALUES (1), ((SELECT 3)), (2)) WHERE x = 3",
        "SELECT x, typeof(x) FROM (SELECT i AS x FROM t UNION ALL VALUES (CAST(3 AS REAL)), (2), (abs(-4)))",
        // A REAL wrapper reads a correlated column's integer as a real.
        "SELECT (SELECT group_concat(typeof(x)) FROM (SELECT 'a' AS x UNION ALL VALUES (CAST(1 AS REAL)), (u.a))) FROM u",
        "WITH w AS (SELECT 1) SELECT (SELECT group_concat(x) FROM (SELECT 'a' AS x UNION ALL VALUES (CAST(1 AS REAL)), (u.a))) FROM u",
        // A first row's column reference names the VALUES column.
        "SELECT (SELECT group_concat(r) FROM (VALUES (t.r), (2))) FROM t",
        "SELECT (SELECT group_concat(column1) FROM (VALUES (t.r))) FROM t",
        "SELECT (SELECT group_concat(R) FROM (VALUES (T.R COLLATE nocase), (2))) FROM t",
        "SELECT (SELECT group_concat(rowid) FROM (VALUES (t.rowid), (2))) FROM t",
        "SELECT (SELECT group_concat(id) FROM (VALUES (p.id), (2))) FROM p",
        "SELECT (SELECT group_concat(s) FROM (VALUES (t.r, t.s), (1, 2))) FROM t",
        "SELECT (SELECT group_concat(column2) FROM (VALUES (t.r, t.s))) FROM t",
        // Views read their text as a chain.
        "SELECT * FROM vc WHERE column1 = '3'",
        "SELECT * FROM vw WHERE x = '3'",
    ];
    let schema = format!(
        "{S} CREATE TABLE p(id INTEGER PRIMARY KEY, v); INSERT INTO p VALUES (7, 1); \
         CREATE VIEW vc AS VALUES (3), (4) UNION ALL SELECT r FROM t; \
         CREATE VIEW vw AS SELECT s AS x FROM t UNION ALL VALUES (3), (CAST(4 AS REAL));"
    );
    same("push", &schema, &qs, true);
}

/// The chain's plans: `SCAN CONSTANT ROW` per chained row, `SCAN n
/// CONSTANT ROWS` alone, `0-ROW VALUES CLAUSE` for a FROM item over a
/// chain of one-row selects throughout; co-routine segments where the
/// pin keeps them.
#[test]
fn chain_plans_match_pin() {
    let qs = [
        "WITH q(x) AS (VALUES (3), (4)) SELECT x FROM q",
        "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x FROM q WHERE x = '3'",
        "WITH w AS (SELECT 1) SELECT * FROM (VALUES (3), (4))",
        "WITH w AS (SELECT 1) SELECT * FROM (VALUES (3), (4), (5)) WHERE column1 = 3",
        "WITH w AS (SELECT 1) VALUES (1),(2)",
        "WITH w AS (SELECT 1) VALUES (1),(2) UNION ALL VALUES (3),(4)",
        "WITH w AS (SELECT 1) VALUES (1),(2) UNION VALUES (3),(4)",
        "WITH w AS (SELECT 1) SELECT 0 UNION ALL VALUES (1),(2) UNION ALL VALUES (3),(4)",
        "WITH w AS (SELECT 1) SELECT * FROM (VALUES (1),(2) UNION ALL SELECT 3) WHERE column1 = 1",
        "VALUES (1),(2),(random() * 0)",
        "VALUES (1),(2),(abs(-4))",
        "VALUES (1),((SELECT 3)),(4),(5)",
        "VALUES (1),((SELECT 3)),(4),(5),((SELECT 6)),(7)",
        "VALUES (1),(2),(CAST(3 AS INT)),(4)",
        "VALUES (CAST(1 AS INT)),(2),(3)",
        "VALUES (1),(CAST(2 AS INT)),(3)",
        "VALUES (1),(2),(changes())",
        "VALUES (1),(2),(1+2),(+3),('a' COLLATE nocase),(x'41'),(NULL)",
        "VALUES (1),(2),(CASE WHEN 1 THEN 2 END),(1 IN (1,2)),('a' LIKE 'a'),(coalesce(1,2))",
        "VALUES ((SELECT 1)),(2),(3)",
        "VALUES (1),(2),(3) UNION ALL SELECT 4",
        "SELECT 0 UNION ALL VALUES (1),(2),(abs(-4)),(5)",
        "SELECT 0 UNION ALL VALUES (1),((SELECT 3)),(4)",
        "SELECT 0 UNION ALL VALUES (1),(2),((SELECT 3)),(4)",
        "SELECT * FROM (VALUES (1),((SELECT 3)),(4))",
        "SELECT * FROM (VALUES (1),(2),((SELECT 3)),(4))",
        "VALUES (1),((SELECT 3)),(4)",
        "VALUES (CAST(1 AS INT)),(2)",
        "SELECT * FROM (SELECT r FROM t UNION ALL VALUES (1), (2), (abs(-4)), ('5'))",
        "SELECT column1 FROM (VALUES (1), (2), (abs(-4)), ('5') UNION ALL SELECT r FROM t) WHERE column1 = 5",
        "WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (3), ('3')) SELECT x FROM q WHERE x = '3'",
        "SELECT * FROM vc",
    ];
    let schema = format!("{S} CREATE VIEW vc AS VALUES (3), (4) UNION ALL SELECT r FROM t;");
    same("plans", &schema, &qs, true);
}

/// A trigger body's VALUES is a chain on the pin (its text is read
/// with the schema), with the same push-down and REAL wrapper.
#[test]
fn trigger_values_are_chains() {
    let bodies = [
        "INSERT INTO lg SELECT column1, typeof(column1) FROM (VALUES (3), (4) UNION ALL SELECT r FROM t)",
        "INSERT INTO lg SELECT column1, typeof(column1) FROM (VALUES (3), (4) UNION ALL SELECT r FROM t) WHERE column1 = '3'",
        "INSERT INTO lg SELECT column1, typeof(column1) FROM (VALUES (3), (4) UNION ALL SELECT s FROM t) WHERE column1 = 3",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT s AS x FROM t UNION ALL VALUES (2), (CAST(3 AS REAL)))",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT i AS x FROM t UNION ALL VALUES (2), (CAST(3 AS REAL)))",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT s AS x FROM t UNION ALL VALUES (1), (2)) WHERE x = 1",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT r AS x FROM t UNION ALL VALUES (1), ('1')) WHERE x = 1",
        "INSERT INTO lg SELECT column1, typeof(column1) FROM (VALUES (1), ('1') UNION ALL SELECT r FROM t) WHERE column1 = 1",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT r AS x FROM t UNION ALL VALUES (1), (new.k))",
        "INSERT INTO lg SELECT x, typeof(x) FROM (SELECT 'a' AS x UNION ALL VALUES (CAST(1 AS REAL)), (new.k))",
        "INSERT INTO lg VALUES (new.k, 1), (2, 2)",
        "INSERT INTO lg SELECT * FROM (VALUES (new.k, 'a'), (2, 'b')) WHERE column2 = 'b'",
    ];
    let mut scripts = Vec::new();
    for b in bodies.iter() {
        scripts.push(format!(
            "CREATE TABLE lg(x, ty);; CREATE TABLE g(k);; \
             CREATE TRIGGER tr AFTER INSERT ON g BEGIN {b}; END;; \
             INSERT INTO g VALUES (1);; SELECT * FROM lg"
        ));
    }
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("trig", S, &refs);
}

/// Generated compounds of VALUES lists (co-routines, chains, mixed
/// rows) beside typed arms, under WITH and without, read with WHERE
/// terms of every affinity: rows, and plans.
#[test]
fn generated_chains_match_pin() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let rows = [
        "(3)",
        "(4)",
        "('3')",
        "(3.0)",
        "(NULL)",
        "(CAST(3 AS REAL))",
        "(CAST(3 AS TEXT))",
        "(CAST('3' AS INT))",
        "((SELECT 3))",
        "((SELECT '3'))",
        "(abs(-3))",
        "(x'33')",
        "(+3)",
        "('4.0')",
    ];
    let arms = [
        "SELECT r FROM t",
        "SELECT s FROM t",
        "SELECT i FROM t",
        "SELECT b FROM t",
        "SELECT n FROM t",
        "SELECT 3",
        "SELECT '3'",
        "SELECT CAST(3 AS REAL)",
    ];
    let terms = [
        "x = 3",
        "x = '3'",
        "x = 3.0",
        "x > '2'",
        "x < 4",
        "x IN (3, '4')",
        "x = '4.0'",
        "x IS NULL",
        "x <> '3'",
        "x BETWEEN 3 AND '4'",
        "x = 4",
    ];
    let mut qs = Vec::new();
    for _ in 0..240 {
        let n = 2 + rng.n(3) as usize;
        let mut v = String::from("VALUES ");
        for k in 0..n {
            if k > 0 {
                v.push_str(", ");
            }
            v.push_str(rng.pick(&rows));
        }
        let arm = rng.pick(&arms);
        let body = match rng.n(3) {
            0 => format!("{v} UNION ALL {arm}"),
            1 => format!("{arm} UNION ALL {v}"),
            _ => format!("{arm} UNION ALL {v} UNION ALL {}", rng.pick(&arms)),
        };
        let term = rng.pick(&terms);
        let q = match rng.n(3) {
            0 => format!("WITH q(x) AS ({body}) SELECT x, typeof(x) FROM q WHERE {term}"),
            1 => format!(
                "WITH w AS (SELECT 1) SELECT x, typeof(x) FROM (SELECT * FROM ({body})) AS d(x) WHERE {term}"
            ),
            _ => format!("SELECT x, typeof(x) FROM ({body}) WHERE {term}").replace(
                "SELECT x, typeof(x) FROM (VALUES",
                "SELECT column1 AS x, typeof(column1) FROM (VALUES",
            ),
        };
        qs.push(q);
    }
    let path = fresh("gen", S);
    let bad = diffs_on(&path, &qs, false);
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        qs.len(),
        bad.join("\n")
    );
    let plans: Vec<String> = qs.iter().step_by(4).cloned().collect();
    let bad = diffs_on(&path, &plans, true);
    assert!(
        bad.is_empty(),
        "{} of {} plans differ:\n{}",
        bad.len(),
        plans.len(),
        bad.join("\n")
    );
}

// ------------------------------------------------- unknown collations

const C: &str = "CREATE TABLE t(a INTEGER, b TEXT, c); \
    INSERT INTO t VALUES (1,'x',1),(2,'Y',2),(3,'z',NULL);";

/// An unknown collation is refused only where the pin looks it up.
#[test]
fn unknown_collation_is_looked_up_where_the_pin_does() {
    let qs = [
        "SELECT 1 COLLATE bogus",
        "SELECT 'a' COLLATE bogus",
        "SELECT a COLLATE bogus FROM t",
        "SELECT a FROM t WHERE a COLLATE bogus = 1",
        "SELECT a FROM t ORDER BY a COLLATE bogus",
        "SELECT 1 ORDER BY 1 COLLATE bogus",
        "SELECT a FROM t GROUP BY a COLLATE bogus",
        "SELECT DISTINCT a COLLATE bogus FROM t",
        "SELECT a COLLATE bogus FROM t UNION SELECT 1",
        "SELECT a COLLATE bogus FROM t UNION ALL SELECT 1",
        "SELECT max(a COLLATE bogus) FROM t",
        "SELECT count(b COLLATE bogus) FROM t",
        "SELECT count(DISTINCT b COLLATE bogus) FROM t",
        "SELECT group_concat(b, ',' COLLATE bogus) FROM t",
        "SELECT group_concat(b COLLATE bogus) FROM t",
        "SELECT (SELECT 1 COLLATE bogus)",
        "SELECT 1 WHERE 1 COLLATE bogus",
        "SELECT 1 COLLATE bogus = 1",
        "SELECT 1 = 1 COLLATE bogus",
        "SELECT 'a' COLLATE nocase = 'a' COLLATE bogus",
        "SELECT 'a' COLLATE bogus = 'a' COLLATE nocase",
        "SELECT 1 IN (1 COLLATE bogus)",
        "SELECT 1 COLLATE bogus IN (1, 2)",
        "SELECT 'a' COLLATE bogus IN ()",
        "SELECT 'a' NOT IN ('b' COLLATE bogus)",
        "SELECT 'a' COLLATE bogus NOT IN ('b')",
        "SELECT b FROM t WHERE b IN ('x' COLLATE bogus, 'y')",
        "SELECT 1 COLLATE bogus BETWEEN 0 AND 2",
        "SELECT b FROM t WHERE b BETWEEN 'a' AND 'z' COLLATE bogus",
        "SELECT 'a' COLLATE bogus LIKE 'a'",
        "SELECT 'a' COLLATE bogus GLOB 'a'",
        "SELECT 'a' COLLATE bogus < 'b'",
        "SELECT typeof(1 COLLATE bogus)",
        "SELECT CASE 1 COLLATE bogus WHEN 1 THEN 2 END",
        "SELECT CASE 1 WHEN 1 COLLATE bogus THEN 2 END",
        "SELECT CASE WHEN b = 'x' THEN 'q' COLLATE bogus END FROM t",
        "SELECT CASE WHEN 0 THEN 'a' COLLATE bogus = 'a' END",
        "SELECT coalesce(NULL COLLATE bogus, 2)",
        "SELECT nullif(1 COLLATE bogus, 1)",
        "SELECT nullif('a', 'a' COLLATE bogus)",
        "SELECT instr('abc' COLLATE bogus, 'b')",
        "SELECT min(1 COLLATE bogus, 2)",
        "SELECT max(1, 2 COLLATE bogus)",
        "SELECT max(b, 'q' COLLATE bogus) FROM t",
        "SELECT max(b COLLATE nocase, 'q' COLLATE bogus) FROM t",
        "SELECT 1 COLLATE bogus IS NULL",
        "SELECT 1 COLLATE bogus IS 1",
        "SELECT 'a' COLLATE bogus IS DISTINCT FROM 'a'",
        "SELECT b COLLATE bogus || 'x' FROM t",
        "SELECT (b COLLATE bogus) + 1 FROM t",
        "SELECT a FROM t WHERE b COLLATE bogus IS NOT NULL",
        "SELECT b FROM t WHERE b COLLATE bogus",
        "SELECT b FROM t WHERE NOT b COLLATE bogus",
        "SELECT b FROM t WHERE b || ('a' COLLATE bogus) = 'xa'",
        "SELECT b FROM t WHERE upper(b COLLATE bogus) = 'X'",
        "SELECT b FROM t WHERE b COLLATE bogus COLLATE nocase = 'y'",
        "SELECT 1 COLLATE nocase COLLATE bogus",
        "SELECT x FROM (SELECT b COLLATE bogus AS x FROM t)",
        "WITH q AS (SELECT 1 COLLATE bogus AS x) SELECT x FROM q",
        "SELECT * FROM (VALUES (1 COLLATE bogus))",
        "SELECT * FROM (VALUES (1 COLLATE bogus), (2))",
        "VALUES (1 COLLATE bogus)",
        "VALUES (1 COLLATE bogus), (2)",
        "SELECT a FROM t WHERE a IN (SELECT a COLLATE bogus FROM t)",
        "SELECT a FROM t WHERE a = (SELECT a COLLATE bogus FROM t LIMIT 1)",
        "SELECT 1 IN (SELECT 1 COLLATE bogus)",
        "SELECT 1 IN (SELECT 2 UNION ALL SELECT 1 COLLATE bogus)",
        "SELECT b IN (SELECT 1 COLLATE bogus UNION ALL SELECT 2) FROM t",
        "SELECT (b, 1) IN (SELECT 'x' COLLATE bogus, 1) FROM t",
        "SELECT 1 FROM t WHERE (a, b) = (1, 'x' COLLATE bogus)",
        "SELECT (SELECT 1 COLLATE bogus UNION ALL SELECT 2)",
        "SELECT EXISTS (SELECT 1 COLLATE bogus)",
        "SELECT 1 COLLATE bogus LIMIT 0",
        "SELECT 1 FROM t WHERE 0 AND b COLLATE bogus = 'x'",
        "SELECT 1 WHERE 'a' COLLATE bogus = 'a' AND 0",
        "SELECT 1 WHERE 1 = 1 OR 'a' COLLATE bogus = 'a'",
        "SELECT b FROM t WHERE 0 ORDER BY b COLLATE bogus",
        "SELECT b FROM t ORDER BY b COLLATE bogus LIMIT 0",
        "SELECT count(*) FROM t ORDER BY b COLLATE bogus",
        "SELECT 1 ORDER BY 'a' COLLATE bogus",
        "SELECT 1 ORDER BY 'a' COLLATE bogus = 'a'",
        "SELECT 1 ORDER BY 'b' COLLATE nocase || ('a' COLLATE bogus)",
        "SELECT b FROM t ORDER BY b COLLATE nocase || ('a' COLLATE bogus)",
        "SELECT b FROM t ORDER BY (b COLLATE bogus) IS NULL",
        "SELECT b FROM t ORDER BY EXISTS (SELECT b COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT 'a' COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT b FROM t ORDER BY b COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT 1 WHERE 'a' COLLATE bogus = 'a')",
        "SELECT 1 ORDER BY (SELECT 1 GROUP BY 1 COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT max(1 COLLATE bogus))",
        "SELECT 1 ORDER BY (1 IN (SELECT 1 COLLATE bogus))",
        "SELECT 1 ORDER BY (SELECT 1 UNION SELECT 1 COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT DISTINCT 1 COLLATE bogus)",
        "SELECT 1 ORDER BY (SELECT x FROM (SELECT 1 COLLATE bogus AS x))",
        "SELECT count(*) FROM t ORDER BY (SELECT 1 WHERE 'a' COLLATE bogus = 'a')",
        "SELECT (SELECT 1 ORDER BY 'a' COLLATE bogus = 'a')",
        "SELECT (SELECT 1 ORDER BY (SELECT x FROM (SELECT 1 x ORDER BY 1 COLLATE bogus))) FROM t",
        "SELECT 1 COLLATE bogus UNION SELECT 2",
        "SELECT 1 UNION SELECT 2 COLLATE bogus",
        "SELECT 'a' COLLATE nocase UNION SELECT 'b' COLLATE bogus",
        "SELECT 1 UNION ALL SELECT 2 COLLATE bogus ORDER BY 1",
        "SELECT 1 x UNION ALL SELECT 2 COLLATE bogus ORDER BY x",
        "SELECT 1 UNION ALL SELECT 2 ORDER BY 1 COLLATE bogus",
        "SELECT a FROM t ORDER BY a, a COLLATE bogus",
        "SELECT a AS q FROM t ORDER BY q COLLATE bogus",
        "SELECT b COLLATE bogus FROM t GROUP BY b",
        "SELECT b FROM t GROUP BY b HAVING b COLLATE bogus = 'x'",
        "SELECT row_number() OVER (PARTITION BY 1 COLLATE bogus) FROM t",
        "SELECT row_number() OVER (ORDER BY 1 COLLATE bogus) FROM t",
        "SELECT max(b COLLATE bogus) OVER () FROM t",
        "SELECT sum(a) OVER (PARTITION BY b COLLATE bogus) FROM t",
        "SELECT json_object('a', 1 COLLATE bogus)",
        "SELECT iif(1, 2, 3 COLLATE bogus)",
        "SELECT iif('a' COLLATE bogus = 'a', 1, 2)",
        "SELECT likelihood('a' COLLATE bogus = 'a', 0.5)",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION SELECT x + 1 COLLATE bogus FROM r WHERE x < 3) SELECT x FROM r",
        "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 COLLATE bogus FROM r WHERE x < 3) SELECT x FROM r",
    ];
    same("coll", C, &qs, false);
}

/// Writes and DDL: a CHECK naming an unknown collation is accepted
/// and fails the write that codes it; an index key or partial index
/// comparison, a generated column's comparison and CREATE TABLE AS
/// fail at once; INSERT / UPDATE values and a view's text do not.
#[test]
fn unknown_collation_in_writes_and_ddl() {
    let scripts = [
        "INSERT INTO t VALUES (1 COLLATE bogus, 'q', 3);; SELECT count(*) FROM t",
        "UPDATE t SET b = b COLLATE bogus;; SELECT b FROM t",
        "UPDATE t SET c = 1 WHERE b = 'x' COLLATE bogus",
        "DELETE FROM t WHERE b = 'q' COLLATE bogus",
        "INSERT INTO t SELECT 1, 'a' COLLATE bogus, 1;; SELECT count(*) FROM t",
        "INSERT INTO t SELECT 1, 'a' COLLATE bogus, 1 UNION ALL SELECT 2,'b',2;; SELECT count(*) FROM t",
        "INSERT INTO t SELECT 1, 'a' COLLATE bogus, 1 UNION SELECT 2,'b',2",
        "INSERT INTO t VALUES (1, 'a' COLLATE bogus, 1),(2,'b',2);; SELECT count(*) FROM t",
        "CREATE VIEW vb AS SELECT 1 COLLATE bogus;; SELECT 1",
        "CREATE VIEW vb AS SELECT 1 COLLATE bogus;; SELECT * FROM vb",
        "CREATE VIEW vb AS SELECT 1 COLLATE bogus AS z;; SELECT count(*) FROM vb",
        "CREATE TABLE z AS SELECT 'a' COLLATE bogus AS y",
        "CREATE TABLE z AS SELECT 'a' AS y;; INSERT INTO z SELECT 'b' COLLATE bogus;; SELECT * FROM z",
        "CREATE INDEX ib ON t(b COLLATE bogus)",
        "CREATE INDEX ib ON t(b) WHERE b COLLATE bogus = 'x'",
        "CREATE INDEX ib ON t(b) WHERE b COLLATE bogus;; SELECT 1",
        "CREATE TABLE z(y CHECK (y COLLATE bogus = 'a'));; SELECT 1",
        "CREATE TABLE z(y CHECK (y COLLATE bogus = 'a'));; INSERT INTO z VALUES ('a')",
        "CREATE TABLE z(y CHECK (y COLLATE bogus));; INSERT INTO z VALUES (1);; SELECT * FROM z",
        "CREATE TABLE z(y CHECK (max(1, y COLLATE bogus)));; SELECT 1",
        "CREATE TABLE z(y DEFAULT ('a' COLLATE bogus));; INSERT INTO z DEFAULT VALUES;; SELECT * FROM z",
        "CREATE TABLE z(y, w AS (y COLLATE bogus));; INSERT INTO z(y) VALUES (1);; SELECT * FROM z",
        "CREATE TABLE z(y, w AS (y COLLATE bogus = 1))",
        "CREATE TABLE z(y COLLATE bogus)",
    ];
    script_same("colld", C, &scripts);
}

/// Generated placements of an unknown collation in expressions,
/// clauses and subqueries.
#[test]
fn generated_unknown_collations_match_pin() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let atoms = ["b", "a", "'x'", "1", "c", "(b || 'q')", "upper(b)"];
    let bog = [
        "COLLATE bogus",
        "COLLATE nocase",
        "COLLATE bogus COLLATE nocase",
        "COLLATE nocase COLLATE bogus",
    ];
    let shapes = [
        "SELECT {e} FROM t",
        "SELECT {e} = 'x' FROM t",
        "SELECT 'x' = {e} FROM t",
        "SELECT b FROM t WHERE {e} = 'x'",
        "SELECT b FROM t WHERE {e} IN ('x', 'y')",
        "SELECT b FROM t WHERE 'x' IN ({e}, 'y')",
        "SELECT b FROM t ORDER BY {e}",
        "SELECT 1 ORDER BY {e}",
        "SELECT 1 ORDER BY (SELECT {e} FROM t ORDER BY 1)",
        "SELECT count(*) FROM t GROUP BY {e}",
        "SELECT DISTINCT {e} FROM t",
        "SELECT max({e}) FROM t",
        "SELECT max({e}, 'q') FROM t",
        "SELECT x FROM (SELECT {e} AS x FROM t)",
        "SELECT {e} FROM t UNION SELECT 'q'",
        "SELECT {e} FROM t UNION ALL SELECT 'q'",
        "SELECT b FROM t WHERE b IN (SELECT {e} FROM t)",
        "SELECT CASE {e} WHEN 'x' THEN 1 END FROM t",
        "SELECT {e} IS NULL FROM t",
        "SELECT (SELECT {e} FROM t LIMIT 1)",
    ];
    let mut qs = Vec::new();
    for _ in 0..200 {
        let e = format!("{} {}", rng.pick(&atoms), rng.pick(&bog));
        let e = if rng.n(3) == 0 {
            format!("({e}) || ''")
        } else {
            e
        };
        qs.push(rng.pick(&shapes).replace("{e}", &e));
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    same("collg", C, &refs, false);
}

// ---------------------------------------------- inert aggregates step

/// An enclosing query's aggregate in a dropped ORDER BY or an EXISTS
/// subquery's result list steps in its aggregate owner (its errors
/// raise), and stays inert where the owner does not aggregate.
#[test]
fn inert_aggregates_step_in_their_owner() {
    let qs = [
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(u.a)) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT EXISTS (SELECT sum(u.a)) FROM u",
        "SELECT EXISTS (SELECT sum(u.a)), count(*) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u WHERE 0",
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))), count(*) FROM u",
        "SELECT a FROM u WHERE (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3)))",
        "SELECT a FROM u WHERE EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3)))",
        "SELECT (SELECT 1 ORDER BY max(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT (SELECT 1 ORDER BY total(u.a) + abs(-9223372036854775808)) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u LIMIT 1",
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u ORDER BY 1",
        "SELECT (SELECT count(*) FROM t ORDER BY (SELECT sum(abs(u.a - 9223372036854775807 - 3)))) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3)) FROM t) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3)) FROM t WHERE 0) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3))) FROM u GROUP BY b",
        "SELECT (SELECT 1 ORDER BY group_concat(u.b)) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(u.a) OVER ()) FROM u",
        "SELECT EXISTS (SELECT sum(u.c + 9223372036854775807)) FROM u",
        "SELECT EXISTS (SELECT sum(u.a) UNION SELECT 1) FROM u",
        "SELECT EXISTS (SELECT avg(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT count(*) FROM u WHERE EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3)))",
        "SELECT (SELECT 1 ORDER BY count(*)) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(u.c)) AS k, sum(u.a) FROM u",
        "SELECT b, EXISTS (SELECT max(u.a)) FROM u GROUP BY b HAVING EXISTS (SELECT sum(u.c))",
    ];
    same("inert", S, &qs, false);
}

/// Generated enclosing aggregates in dropped ORDER BYs and EXISTS
/// result lists, with failing and plain arguments.
#[test]
fn generated_inert_aggregates_match_pin() {
    let mut rng = Rng(0xd1b5_4a32_d192_ed03);
    let aggs = [
        "sum(abs(u.a - 9223372036854775807 - 3))",
        "sum(u.c)",
        "max(abs(u.a - 9223372036854775807 - 2))",
        "count(u.b)",
        "total(u.a)",
        "sum(u.a + 9223372036854775807)",
        "group_concat(u.b)",
    ];
    let shapes = [
        "SELECT (SELECT 1 ORDER BY {a}) FROM u",
        "SELECT EXISTS (SELECT {a}) FROM u",
        "SELECT EXISTS (SELECT {a} FROM t) FROM u",
        "SELECT (SELECT 1 ORDER BY {a}) FROM u GROUP BY b",
        "SELECT a FROM u WHERE EXISTS (SELECT {a})",
        "SELECT (SELECT count(*) FROM t ORDER BY (SELECT {a})) FROM u",
        "SELECT (SELECT 1 ORDER BY {a}), count(*) FROM u WHERE a > {k}",
        "SELECT EXISTS (SELECT {a}) FROM u WHERE a > {k}",
    ];
    let ks = ["0", "1", "2", "5", "-9223372036854775808"];
    let mut qs = Vec::new();
    for _ in 0..120 {
        qs.push(
            rng.pick(&shapes)
                .replace("{a}", rng.pick(&aggs))
                .replace("{k}", rng.pick(&ks)),
        );
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    same("inertg", S, &refs, false);
}

// ------------------------------------- nested window ORDER BY waits

/// Inside an ORDER BY the pin never codes, a nested window query's
/// own ORDER BY raises no code-generation error.
#[test]
fn nested_window_order_by_waits() {
    let qs = [
        "SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY (1,2))",
        "SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY abs(-9223372036854775808))",
        "SELECT 1 ORDER BY (SELECT sum(1) OVER () ORDER BY raise(ignore))",
        "SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY (SELECT 1, 2))",
        "SELECT count(*) FROM u ORDER BY (SELECT row_number() OVER () ORDER BY (1,2))",
        "SELECT (SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY (1,2))) FROM u",
        "SELECT 1 ORDER BY (SELECT row_number() OVER (ORDER BY (1,2)))",
        "SELECT 1 ORDER BY (SELECT x FROM (SELECT 1 x) ORDER BY (1,2))",
        "SELECT 1 ORDER BY (SELECT a FROM t ORDER BY (1,2))",
        "SELECT row_number() OVER () ORDER BY (1,2)",
        "SELECT (SELECT row_number() OVER () ORDER BY (1,2))",
        "SELECT a FROM u ORDER BY (SELECT row_number() OVER () ORDER BY (1,2))",
        "SELECT 1 ORDER BY (SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY (1,2)))",
        "SELECT 1 ORDER BY (SELECT row_number() OVER () FROM u ORDER BY (1,2))",
        "SELECT 1 ORDER BY (SELECT row_number() OVER () ORDER BY nosuch)",
    ];
    same("win", S, &qs, false);
}

// --------------------------------------- view column types (table_info)

/// A view's column read through a compound FROM subquery (or CTE)
/// names its type from the subquery's rightmost core, by the standard
/// name when the affinities differ.
#[test]
fn view_types_through_compound_subqueries() {
    let views = [
        "SELECT * FROM (SELECT r, s, z, p, q FROM w UNION ALL SELECT 1, 1, 1, 1, 1)",
        "WITH c AS (SELECT r, s, z, p, q FROM w UNION ALL SELECT 1, 1, 1, 1, 1) SELECT * FROM c",
        "SELECT * FROM (SELECT r, s, z, p, q FROM w UNION ALL SELECT 1, 1, 1, 1, 1) UNION ALL SELECT 2, 2, 2, 2, 2",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT d FROM u)",
        "SELECT * FROM (SELECT d FROM u UNION ALL SELECT 2)",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT 2.5)",
        "SELECT * FROM (SELECT 2.5 UNION ALL SELECT a FROM t)",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT a FROM u)",
        "SELECT * FROM (SELECT a FROM u UNION ALL SELECT a FROM t)",
        "SELECT * FROM (SELECT b FROM t UNION SELECT d FROM u)",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT 2.5) JOIN u",
        "SELECT k.a FROM (SELECT a FROM t UNION ALL SELECT 2.5) k",
        "SELECT * FROM (SELECT p FROM w UNION ALL SELECT q FROM w UNION ALL SELECT r FROM w)",
        "SELECT * FROM (SELECT r FROM w UNION ALL SELECT CAST(1 AS REAL))",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT a FROM t)",
        "SELECT a FROM t UNION ALL SELECT d FROM u",
        "SELECT * FROM (SELECT * FROM (SELECT a FROM t UNION ALL SELECT 2.5))",
        "SELECT * FROM (VALUES (1), (2) UNION ALL SELECT a FROM t)",
        "WITH x AS (SELECT 1) SELECT * FROM (SELECT a FROM t UNION ALL VALUES (1), (2))",
    ];
    let mut scripts = Vec::new();
    for v in views.iter() {
        scripts.push(format!("CREATE VIEW v AS {v};; PRAGMA table_info(v)"));
        scripts.push(format!(
            "CREATE VIEW v AS {v};; CREATE VIEW v2 AS SELECT * FROM v;; \
             SELECT name, type FROM pragma_table_info('v2')"
        ));
    }
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same(
        "vt",
        "CREATE TABLE t(a INTEGER, b TEXT, c); CREATE TABLE u(a INT, d varchar(9)); \
         CREATE TABLE w(p DATETIME, q BOOLEAN, r DOUBLE PRECISION, s CLOB, z FLOATING);",
        &refs,
    );
}
