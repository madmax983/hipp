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

/// Each query's answer from hipp, on the frozen VM when `vm` is set
/// and the hidden `vm` feature is on (else on today's executor).
fn hipp_answers_on(path: &Path, qs: &[String], vm: bool) -> Vec<Answer> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        // (The frozen VM, `vm`, keeps its own OFFSET / ORDER BY
        // handling: these shapes run on today's executor.)
        #[cfg(feature = "vm")]
        let cfg = Config::default().vm_off(!vm);
        #[cfg(not(feature = "vm"))]
        let cfg = {
            let _ = vm;
            Config::default()
        };
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
    diffs_vm(path, qs, plans, false)
}

/// `diffs_on`, on the frozen VM when `vm` is set (`hipp_answers_on`).
fn diffs_vm(path: &Path, qs: &[String], plans: bool, vm: bool) -> Vec<String> {
    let got = hipp_answers_on(path, qs, vm);
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

// ------------------------------------------------ review fixes (v0.17)

/// A later VALUES arm is the pin's `SELECT * FROM (chain)`: its
/// columns keep the chain's names, a first-row column reference's
/// included (review fix: the wrapper named `column1` and failed when
/// `(VALUES (t.i), ...)` named its column `i`). Generated correlated
/// chains under plain, aliased and WITH heads.
#[test]
fn correlated_chain_arms_keep_their_names() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let vals = [
        "t.i",
        "t.r",
        "t.s",
        "t.rowid",
        "t.n",
        "1",
        "'3'",
        "2.5",
        "NULL",
        "CAST(4 AS REAL)",
        "t.i+0",
        "t.b",
    ];
    let heads = ["SELECT 0 AS x", "SELECT 0", "SELECT t.i AS x", "SELECT t.s"];
    let aggs = ["group_concat(1)", "count(*)", "sum(typeof(1) = 'integer')"];
    let mut qs: Vec<String> = vec![
        "SELECT (SELECT group_concat(x) FROM (SELECT 0 AS x UNION ALL VALUES (t.i),(t.r))) FROM t"
            .into(),
        "SELECT t.rowid, (SELECT count(*) FROM (SELECT 1 UNION ALL VALUES (t.i),(t.r))) FROM t"
            .into(),
        "SELECT (WITH w AS (SELECT 1) SELECT group_concat(x) FROM (SELECT 0 AS x UNION ALL VALUES (t.s),(1))) FROM t"
            .into(),
        "SELECT (SELECT group_concat(x) FROM (SELECT 0 AS x UNION ALL VALUES (t.rowid),(t.r))) FROM t"
            .into(),
    ];
    for _ in 0..200 {
        let n = 2 + rng.n(3) as usize;
        let mut v = String::new();
        for k in 0..n {
            if k > 0 {
                v.push(',');
            }
            v.push_str(&format!("({})", rng.pick(&vals)));
        }
        qs.push(match rng.n(3) {
            0 => format!(
                "SELECT (WITH w AS (SELECT 1) SELECT group_concat(x || typeof(x)) FROM (SELECT 0 AS x UNION ALL VALUES {v})) FROM t"
            ),
            1 => format!(
                "SELECT (SELECT group_concat(x || typeof(x)) FROM (SELECT 0 AS x UNION ALL VALUES {v}) WHERE x > '2') FROM t"
            ),
            _ => format!(
                "SELECT t.rowid, (SELECT {} FROM ({} UNION ALL VALUES {v})) FROM t",
                rng.pick(&aggs),
                rng.pick(&heads)
            ),
        });
    }
    let path = fresh("corr", S);
    let bad = diffs_on(&path, &qs, false);
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        qs.len(),
        bad.join("\n")
    );
}

/// The pin merges a plain `SELECT ... FROM (compound)` (a subquery or
/// a view, a view's VALUES chain included) into its reader before it
/// pushes a WHERE term into the compound's arms, each with its own
/// affinity (review fix: the term stayed in the reader with the
/// compound's affinity). Fixed shapes, then a generated corpus.
#[test]
fn push_down_through_merged_subqueries() {
    let schema = format!(
        "{S} CREATE TABLE ra(a REAL); INSERT INTO ra VALUES (3.0); \
         CREATE VIEW vc AS SELECT * FROM (VALUES (3),(4) UNION ALL SELECT r FROM t); \
         CREATE VIEW vs AS SELECT 3 AS x UNION ALL SELECT r FROM t; \
         CREATE VIEW vw AS SELECT x FROM (SELECT 3 AS x UNION ALL SELECT s FROM t); \
         CREATE VIEW vv AS SELECT * FROM vs;"
    );
    let mut qs: Vec<String> = [
        "SELECT x, typeof(x) FROM (SELECT x FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra)) WHERE x = '3'",
        "SELECT x, typeof(x) FROM (SELECT * FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra)) WHERE x = '3'",
        "SELECT column1, typeof(column1) FROM vc WHERE column1 = '3'",
        "SELECT x FROM vs WHERE x = '3'",
        "SELECT x FROM vw WHERE x = '3'",
        "SELECT x FROM vv WHERE x = '3'",
        "SELECT x FROM (SELECT * FROM vw) WHERE x = '3'",
        "SELECT x FROM (SELECT 1 AS x UNION ALL SELECT * FROM (SELECT 3 UNION ALL SELECT CAST(4 AS REAL))) WHERE x = '3'",
        "SELECT x FROM (SELECT * FROM (SELECT * FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra))) WHERE x = '3'",
        "SELECT x FROM (SELECT x FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra) WHERE x > 0) WHERE x = '3'",
        "SELECT * FROM u, (SELECT x FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra)) WHERE x = '3' AND u.a = 3",
        "SELECT x FROM (SELECT x FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra) LIMIT 5) WHERE x = '3'",
        "SELECT count(*) FROM (SELECT x FROM (SELECT 3 AS x UNION ALL SELECT a FROM ra)) WHERE x = '3'",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    let mut rng = Rng(0x6a09_e667_f3bc_c909);
    let arms = [
        ("SELECT r AS x FROM t", "SELECT r FROM t"),
        ("SELECT s AS x FROM t", "SELECT s FROM t"),
        ("SELECT i AS x FROM t", "SELECT i FROM t"),
        ("SELECT n AS x FROM t", "SELECT n FROM t"),
        ("SELECT 3 AS x", "SELECT 3"),
        ("SELECT '3' AS x", "SELECT '3'"),
        ("SELECT CAST(3 AS REAL) AS x", "SELECT CAST(3 AS REAL)"),
        ("SELECT CAST(3 AS TEXT) AS x", "VALUES (3),(4)"),
        ("SELECT 4 AS x", "VALUES ('3'),(random()*0+3)"),
    ];
    let lits = ["'3'", "3", "'3.0'", "'4'", "4.0", "'x'"];
    let ops = ["=", "<>", ">", "<="];
    for _ in 0..240 {
        let n = 2 + rng.n(2) as usize;
        let mut body = String::from(arms[rng.n(arms.len() as u64) as usize].0);
        for _ in 1..n {
            body.push_str(" UNION ALL ");
            body.push_str(arms[rng.n(arms.len() as u64) as usize].1);
        }
        let mid = match rng.n(4) {
            0 => format!("SELECT * FROM ({body})"),
            1 => format!("SELECT x FROM ({body})"),
            2 => format!("SELECT * FROM (SELECT * FROM ({body}))"),
            _ => body.clone(),
        };
        let lit = rng.pick(&lits);
        let term = if rng.n(5) == 0 {
            format!("x IN ({lit}, 9)")
        } else {
            format!("x {} {lit}", rng.pick(&ops))
        };
        qs.push(match rng.n(3) {
            0 => format!("SELECT x, typeof(x) FROM ({mid}) WHERE {term}"),
            1 => format!(
                "SELECT x, typeof(x) FROM {} WHERE {term}",
                rng.pick(&["vs", "vw", "vv"])
            ),
            _ => format!("SELECT x FROM (SELECT x, typeof(x) FROM ({mid}) WHERE {term}) WHERE 1"),
        });
    }
    let path = fresh("pdm", &schema);
    let bad = diffs_on(&path, &qs, false);
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        qs.len(),
        bad.join("\n")
    );
    // (Plans: not through `vw`, whose co-routine number differs, nor
    // beside `u`, whose automatic index hipp does not build: both
    // EQP-only and older than these fixes.)
    let plans: Vec<String> = qs
        .iter()
        .step_by(5)
        .filter(|q| !q.contains("vw") && !q.contains("u,"))
        .cloned()
        .collect();
    let bad = diffs_on(&path, &plans, true);
    assert!(
        bad.is_empty(),
        "{} of {} plans differ:\n{}",
        bad.len(),
        plans.len(),
        bad.join("\n")
    );
}

/// A trigger's `new.rowid` keeps its INTEGER affinity as a VALUES
/// chain's row and as a subquery's item (review fix: it had none, so
/// `column1 = '3'` matched nothing); `new.i` has none on the pin.
#[test]
fn trigger_rowid_keeps_its_affinity() {
    let bodies = [
        "(SELECT group_concat(column1 || typeof(column1)) FROM (VALUES (3),(new.rowid),(4)) WHERE column1 = '3')",
        "(SELECT group_concat(column1 || typeof(column1)) FROM (VALUES (3),(new.i),(4)) WHERE column1 = '3')",
        "(SELECT group_concat(column1 || typeof(column1)) FROM (VALUES (3),(new.rowid)) WHERE column1 = '1')",
        "(SELECT group_concat(x || typeof(x)) FROM (SELECT 3 AS x UNION ALL SELECT new.rowid) WHERE x = '1')",
        "(SELECT group_concat(column1 || typeof(column1)) FROM (VALUES (3),(new.r),(4)) WHERE column1 = '3')",
        "(SELECT group_concat(column1 || typeof(column1)) FROM (VALUES (3),(new.s),(4)) WHERE column1 = '3.0')",
        "(SELECT group_concat(x || typeof(x)) FROM (SELECT new.rowid AS x) WHERE x = '1')",
        "(SELECT group_concat(x || typeof(x)) FROM (SELECT new.i AS x) WHERE x = '3')",
        "((SELECT new.rowid) = '1') || ((SELECT new.i) = '3') || (new.i = '3') || (new.rowid = '1')",
        "'1' IN (SELECT new.rowid)",
        "(SELECT group_concat(x || typeof(x)) FROM (SELECT new.r AS x UNION ALL SELECT 3) WHERE x = '7')",
        "(SELECT group_concat(column1) FROM (VALUES (new.rowid * 1),(3)) WHERE column1 = '1')",
    ];
    let mut scripts = Vec::new();
    for b in bodies.iter() {
        scripts.push(format!(
            "CREATE TABLE tt(r REAL, i INT, s TEXT);; CREATE TABLE lg(a, b);; \
             CREATE TRIGGER tr AFTER INSERT ON tt BEGIN INSERT INTO lg SELECT 1, {b}; END;; \
             INSERT INTO tt VALUES (7, 3, '3.0');; INSERT INTO tt VALUES (8, 4, 'x');; SELECT * FROM lg"
        ));
    }
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("trid", S, &refs);
}

/// Long VALUES lists: a co-routine begun after chained rows exempts
/// the compound from the 500-SELECT limit (the pin's `SF_Values` on
/// its reader; review fix: 600 `(random())` rows then `(1),(2)` were
/// refused), and `SELECT count(*) FROM (chain)` is the pin's
/// `countOfViewOptimization`, a sum of one term per arm that is "too
/// large" at 1000 arms (review fix: hipp answered any length). A CTE
/// of a long chain reads (review fix: its rows counted as copies).
#[test]
fn long_value_lists_match_pin() {
    fn rows(n: usize, e: &str) -> String {
        vec![e; n].join(",")
    }
    let v = |n: usize| format!("VALUES {}", rows(n, "(random())"));
    let qs: Vec<String> = vec![
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2))",
            rows(600, "(random())")
        ),
        format!("SELECT count(*) FROM ({})", v(600)),
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2),(random()))",
            rows(600, "(random())")
        ),
        format!(
            "SELECT count(*) FROM (SELECT 1 UNION ALL VALUES {},(1),(2))",
            rows(600, "(random())")
        ),
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2),{})",
            rows(300, "(random())"),
            rows(300, "(random())")
        ),
        format!("VALUES {},(1),(2)", rows(600, "(random()*0)")),
        format!(
            "SELECT count(*) FROM (VALUES {},{})",
            rows(500, "(random())"),
            rows(500, "(1),(2)")
        ),
        format!(
            "SELECT count(*) FROM ({} UNION ALL VALUES (1),(2))",
            vec!["SELECT 1"; 600].join(" UNION ALL ")
        ),
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2) UNION ALL SELECT 5)",
            rows(600, "(random())")
        ),
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2))",
            rows(498, "(random())")
        ),
        format!(
            "SELECT count(*) FROM (VALUES {},(1),(2))",
            rows(499, "(random())")
        ),
        format!("SELECT count(*) FROM ({})", v(999)),
        format!("SELECT count(*) FROM ({})", v(1000)),
        format!("SELECT (SELECT count(*) FROM ({}))", v(1000)),
        format!("SELECT count(*) FROM (SELECT 1 UNION ALL {})", v(999)),
        format!("SELECT count(*) FROM (SELECT 1 UNION ALL {})", v(1000)),
        format!("SELECT 1 IN ({})", v(1000)),
        format!("SELECT EXISTS ({})", v(1000)),
        format!("WITH c AS ({}) SELECT count(*) FROM c", v(1200)),
        format!(
            "WITH c AS (SELECT * FROM ({})) SELECT count(*) FROM c",
            v(1000)
        ),
        format!(
            "WITH c AS (SELECT 1) SELECT count(*) FROM (VALUES {})",
            rows(1000, "(1)")
        ),
        format!("SELECT count(*) FROM (SELECT * FROM ({}))", v(1000)),
        format!(
            "SELECT count(*) FROM (SELECT column1 AS x FROM ({}))",
            v(1000)
        ),
        format!(
            "SELECT count(*) FROM (SELECT * FROM ({}) WHERE column1 IS NOT NULL)",
            v(1000)
        ),
        format!("SELECT count(*) FROM ({}) WHERE column1 > 0 OR 1", v(1000)),
        format!("SELECT count(*) FROM (SELECT 1), ({})", v(1000)),
        format!("SELECT count(1) FROM ({})", v(1000)),
        format!("SELECT count(*) FILTER (WHERE 1) FROM ({})", v(1000)),
        format!("SELECT count(*) FROM ({}) ORDER BY 1", v(1000)),
        format!(
            "SELECT count(*) FROM (SELECT DISTINCT 1 UNION ALL {})",
            v(1000)
        ),
        format!("SELECT count(*) AS c FROM ({}) AS s", v(1000)),
        format!(
            "WITH a AS (VALUES {}) SELECT (SELECT count(*) FROM a) + (SELECT count(*) FROM a) + (SELECT count(*) FROM a)",
            rows(900, "(random())")
        ),
    ];
    let path = fresh("long", S);
    let bad = diffs_on(&path, &qs, false);
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        qs.len(),
        bad.join("\n")
    );
    let scripts = [
        format!("CREATE VIEW v AS {};; SELECT count(*) FROM v", v(1000)),
        format!("CREATE VIEW v AS {};; SELECT count(*) FROM v", v(999)),
        format!(
            "CREATE TABLE tt(a);; INSERT INTO tt SELECT * FROM (VALUES {},(1),(2));; SELECT count(*) FROM tt",
            rows(600, "(random()*0)")
        ),
        format!(
            "CREATE TABLE tt(a);; INSERT INTO tt SELECT * FROM ({});; SELECT count(*) FROM tt",
            v(1200)
        ),
    ];
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("longs", S, &refs);
}

/// Schema paths that resolve a statement without coding it look no
/// collation up but the ones the pin does (review fixes): an ADD
/// COLUMN that verifies its rows codes every CHECK; an UPDATE codes
/// the CHECKs that read a changed column, a foreign key's SET NULL /
/// SET DEFAULT / ON UPDATE CASCADE those that read its key;
/// `integrity_check` every CHECK; `PRAGMA table_info` of a view only
/// its columns' collations (and its FROM subqueries'); ALTER TABLE's
/// rename none.
#[test]
fn unknown_collation_in_schema_paths() {
    let scripts = [
        // ADD COLUMN.
        "CREATE TABLE a1(a);; ALTER TABLE a1 ADD COLUMN c CHECK (c COLLATE bogus = 'q');; SELECT sql FROM sqlite_schema",
        "CREATE TABLE a1(a);; INSERT INTO a1 VALUES (1);; ALTER TABLE a1 ADD COLUMN c CHECK (c COLLATE bogus = 'q')",
        "CREATE TABLE a1(a CHECK (a COLLATE bogus = 'x'));; ALTER TABLE a1 ADD COLUMN c CHECK (c > 0)",
        "CREATE TABLE a1(a CHECK (a COLLATE bogus = 'x'));; ALTER TABLE a1 ADD COLUMN c;; SELECT sql FROM sqlite_schema",
        "CREATE TABLE a1(a, CHECK (a COLLATE bogus = 'x'));; ALTER TABLE a1 ADD COLUMN c AS (a);; SELECT sql FROM sqlite_schema",
        "CREATE TABLE a1(a CHECK (a COLLATE bogus = 'x'));; ALTER TABLE a1 ADD COLUMN c AS (1) NOT NULL",
        "CREATE TABLE a1(a ANY) STRICT;; ALTER TABLE a1 ADD COLUMN c INT CHECK (c COLLATE bogus = 'q')",
        "CREATE TABLE a1(a ANY CHECK (a COLLATE bogus = 'x')) STRICT;; ALTER TABLE a1 ADD COLUMN c INT",
        "CREATE TABLE a1(a);; ALTER TABLE a1 ADD COLUMN c CHECK (c COLLATE nocase = 'q');; SELECT sql FROM sqlite_schema",
        "CREATE TABLE a1(a CHECK (raise(abort, 'x') IS NULL));; ALTER TABLE a1 ADD COLUMN c CHECK (c > 0)",
        "CREATE TABLE a1(a);; ALTER TABLE a1 ADD COLUMN c CHECK (raise(abort, 'x') IS NULL)",
        // UPDATE / INSERT / FK actions / integrity_check.
        "CREATE TABLE b1(a, b, CHECK (a COLLATE bogus = 'x'));; UPDATE b1 SET b = 1;; SELECT 'ok'",
        "CREATE TABLE b1(a CHECK (a COLLATE bogus = 'x'), b);; UPDATE b1 SET b = 1;; SELECT 'ok'",
        "CREATE TABLE b1(a CHECK (a COLLATE bogus = 'x'), b);; UPDATE b1 SET a = 1",
        "CREATE TABLE b1(a CHECK (a COLLATE bogus = 'x'), b);; INSERT INTO b1(b) VALUES (5)",
        "CREATE TABLE b1(a, g AS (a + 1), CHECK (g COLLATE bogus = 'x'), b);; UPDATE b1 SET a = 1",
        "CREATE TABLE b1(a, g AS (a + 1), CHECK (g COLLATE bogus = 'x'), b);; UPDATE b1 SET b = 1;; SELECT 'ok'",
        "CREATE TABLE b1(a INTEGER PRIMARY KEY, b, CHECK (a COLLATE bogus = 'q'));; UPDATE b1 SET rowid = 1",
        "CREATE TABLE b1(a INTEGER PRIMARY KEY, b, CHECK (a COLLATE bogus = 'q'));; UPDATE b1 SET b = 1;; SELECT 'ok'",
        "CREATE TABLE b1(a PRIMARY KEY, b, CHECK (b COLLATE bogus = 'q')) WITHOUT ROWID;; UPDATE b1 SET a = 1;; SELECT 'ok'",
        "CREATE TABLE b1(a, g AS (a + 1), CHECK (a COLLATE bogus = 'x'));; SELECT * FROM b1;; DELETE FROM b1;; CREATE INDEX bi ON b1(a);; SELECT 'ok'",
        "CREATE TABLE b1(a, CHECK (a COLLATE bogus = 'x'));; PRAGMA integrity_check",
        "CREATE TABLE b1(a, CHECK (a COLLATE bogus = 'x'));; PRAGMA quick_check",
        "PRAGMA foreign_keys = 1;; CREATE TABLE p(id INTEGER PRIMARY KEY);; \
         CREATE TABLE c(x REFERENCES p ON DELETE SET NULL, y, CHECK (y COLLATE bogus = 'q'));; \
         INSERT INTO p VALUES (1);; DELETE FROM p;; SELECT 'ok'",
        "PRAGMA foreign_keys = 1;; CREATE TABLE p(id INTEGER PRIMARY KEY);; \
         CREATE TABLE c(x REFERENCES p ON DELETE CASCADE, y, CHECK (x COLLATE bogus = 'q'));; \
         INSERT INTO p VALUES (1);; DELETE FROM p;; SELECT 'ok'",
        "PRAGMA foreign_keys = 1;; CREATE TABLE p(id INTEGER PRIMARY KEY);; \
         CREATE TABLE c(x REFERENCES p ON DELETE SET NULL, y, CHECK (x COLLATE bogus = 'q'));; \
         INSERT INTO p VALUES (1);; DELETE FROM p",
        "PRAGMA foreign_keys = 1;; CREATE TABLE p(id INTEGER PRIMARY KEY);; \
         CREATE TABLE c(x REFERENCES p ON UPDATE CASCADE, y, CHECK (x COLLATE bogus = 'q'));; \
         INSERT INTO p VALUES (1);; UPDATE p SET id = 2",
        "PRAGMA foreign_keys = 1;; CREATE TABLE p(id INTEGER PRIMARY KEY);; \
         CREATE TABLE c(x REFERENCES p ON UPDATE CASCADE, y, CHECK (x COLLATE bogus = 'q'));; \
         INSERT INTO p VALUES (1);; DELETE FROM p;; SELECT 'ok'",
        // PRAGMA table_info of a view.
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM v0 ORDER BY b COLLATE bogus;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM v0 WHERE b = 1 COLLATE bogus;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM v0 WHERE b = 1 COLLATE bogus;; SELECT * FROM pragma_table_info('v')",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM v0 GROUP BY b COLLATE bogus;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a COLLATE bogus FROM v0;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM (SELECT b COLLATE bogus AS a FROM v0);; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT 1 AS k FROM (SELECT b COLLATE bogus AS a FROM v0);; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a FROM v0 WHERE a IN (SELECT b COLLATE bogus FROM v0);; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT (SELECT b FROM v0 ORDER BY b COLLATE bogus) AS z FROM v0;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS WITH c AS (SELECT b COLLATE bogus AS q FROM v0) SELECT 1 AS k;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS WITH c AS (SELECT b COLLATE bogus AS q FROM v0) SELECT q FROM c;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT (1, 2) AS x;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT raise(ignore) AS r FROM v0;; PRAGMA table_info(v)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a COLLATE bogus AS x FROM v0;; \
         CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN SELECT 1; END;; INSERT INTO v VALUES (1)",
        "CREATE TABLE v0(a, b);; CREATE VIEW v AS SELECT a AS x FROM v0 WHERE b = 1 COLLATE bogus;; \
         CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN SELECT 1; END;; SELECT 'ok'",
        // ALTER TABLE.
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM r0 WHERE b = 1 COLLATE bogus;; \
         ALTER TABLE r0 RENAME COLUMN b TO c;; SELECT sql FROM sqlite_schema WHERE name = 'v'",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM r0 WHERE b = 1 COLLATE bogus;; \
         ALTER TABLE r0 RENAME TO r1;; SELECT sql FROM sqlite_schema WHERE name = 'v'",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a COLLATE bogus FROM r0;; \
         ALTER TABLE r0 RENAME TO r1;; SELECT sql FROM sqlite_schema WHERE name = 'v'",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM (SELECT b COLLATE bogus AS a FROM r0);; \
         ALTER TABLE r0 RENAME TO r1;; SELECT sql FROM sqlite_schema WHERE name = 'v'",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM r0 ORDER BY b COLLATE bogus;; \
         ALTER TABLE r0 RENAME COLUMN b TO c;; SELECT sql FROM sqlite_schema WHERE name = 'v'",
        "CREATE TABLE r0(a, b);; CREATE TRIGGER tr AFTER INSERT ON r0 BEGIN SELECT 1 WHERE new.b = 1 COLLATE bogus; END;; \
         ALTER TABLE r0 RENAME COLUMN b TO c;; SELECT sql FROM sqlite_schema WHERE name = 'tr'",
        "CREATE TABLE r0(a, b);; CREATE TRIGGER tr AFTER INSERT ON r0 BEGIN INSERT INTO r0 SELECT a, b FROM r0 ORDER BY b COLLATE bogus; END;; \
         ALTER TABLE r0 RENAME TO r1;; SELECT 'ok'",
        "CREATE TABLE r0(a, b, c);; CREATE VIEW v AS SELECT a FROM r0 WHERE b = 1 COLLATE bogus;; \
         ALTER TABLE r0 DROP COLUMN c;; SELECT 'ok'",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM r0 WHERE b = 1 COLLATE bogus;; \
         ALTER TABLE r0 DROP COLUMN a",
        "CREATE TABLE r0(a, b);; CREATE VIEW v AS SELECT a FROM r0 WHERE (a, b) = (1, 2, 3);; ALTER TABLE r0 RENAME TO r1",
    ];
    script_same("ucs", S, &scripts);
}

/// CREATE TABLE ... AS stores a FLEXNUM column (a numeric CAST in a
/// compound's leftmost core, or a FROM subquery's such column) as the
/// pin does: NUMERIC that leaves a REAL alone (review fix: `2.0` was
/// stored as the integer 2).
#[test]
fn create_as_flexnum_columns() {
    let sels = [
        "SELECT 1 AS x UNION ALL VALUES (CAST(2 AS REAL)),(3)",
        "SELECT 1 AS x UNION ALL SELECT * FROM (SELECT CAST(2 AS REAL) UNION ALL SELECT 3)",
        "SELECT 1 AS x UNION ALL SELECT CAST(2 AS REAL)",
        "SELECT CAST(2 AS REAL) AS x",
        "SELECT CAST(2.0 AS NUMERIC) AS x, CAST('2.0' AS NUMERIC) AS y, CAST(2.5 AS INT) AS z",
        "SELECT x FROM (SELECT CAST(2 AS REAL) AS x UNION ALL SELECT 3)",
        "SELECT CAST(2 AS INT) AS x UNION ALL SELECT 3.0",
        "SELECT a AS x FROM nn UNION ALL SELECT 3.0",
        "SELECT * FROM (SELECT CAST(2 AS REAL) AS x UNION ALL SELECT 3) WHERE x > 0",
        "SELECT x + 0 AS x FROM (SELECT CAST(2 AS REAL) AS x UNION ALL SELECT 3)",
        "SELECT CAST(x AS REAL) AS x FROM (SELECT 2 AS x UNION ALL SELECT 3)",
        "SELECT 1 AS x UNION ALL SELECT x FROM (SELECT CAST(2 AS REAL) AS x UNION ALL SELECT 3.0)",
        "SELECT CAST(2 AS INT) AS x UNION ALL SELECT '3.0' UNION ALL SELECT 4.0",
        "SELECT CAST(2 AS NUMERIC) AS x UNION ALL SELECT 3.0 UNION ALL SELECT '4.0'",
        "SELECT x FROM (SELECT a AS x FROM nn UNION ALL SELECT 3.0)",
        "SELECT CAST(2 AS REAL) AS x UNION ALL SELECT a FROM nn",
        "SELECT 'a' AS x UNION ALL VALUES (CAST(2 AS REAL)),(3)",
    ];
    let mut scripts = Vec::new();
    for s in sels.iter() {
        scripts.push(format!(
            "CREATE TABLE nn(a NUMERIC);; INSERT INTO nn VALUES (5), (6.0);; \
             CREATE TABLE q AS {s};; SELECT sql FROM sqlite_schema WHERE name = 'q';; \
             SELECT *, typeof(x) FROM q;; INSERT INTO q(x) VALUES (7.0);; SELECT x, typeof(x) FROM q"
        ));
    }
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("flex", S, &refs);
}

/// The frozen VM (hidden `vm` feature) computes an enclosing query's
/// aggregate in a dropped ORDER BY or an EXISTS result list in its
/// owner too (review fix: these suites ran with the VM off, so its
/// change had no coverage). Without `vm`, today's executor.
#[test]
fn inert_aggregates_on_the_vm() {
    let mut qs: Vec<String> = [
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(u.a)) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3))) FROM u",
        "SELECT EXISTS (SELECT sum(u.a)) FROM u",
        "SELECT EXISTS (SELECT sum(u.a)), count(*) FROM u",
        "SELECT (SELECT 1 ORDER BY sum(abs(u.a - 9223372036854775807 - 3))), count(*) FROM u",
        "SELECT EXISTS (SELECT sum(abs(u.a - 9223372036854775807 - 3))) FROM u GROUP BY b",
        "SELECT b, EXISTS (SELECT max(u.a)) FROM u GROUP BY b HAVING EXISTS (SELECT sum(u.c))",
        "SELECT x FROM (SELECT 1 AS x UNION ALL VALUES (3),(random()*0+3)) WHERE x = '3'",
        "SELECT 1 COLLATE bogus",
        "SELECT a FROM u WHERE b = 'x' COLLATE bogus",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    let mut rng = Rng(0xbb67_ae85_84ca_a73b);
    let aggs = [
        "sum(abs(u.a - 9223372036854775807 - 3))",
        "sum(u.c)",
        "count(u.b)",
        "total(u.a)",
    ];
    let shapes = [
        "SELECT (SELECT 1 ORDER BY {a}) FROM u",
        "SELECT EXISTS (SELECT {a}) FROM u",
        "SELECT EXISTS (SELECT {a} FROM t) FROM u",
        "SELECT a FROM u WHERE EXISTS (SELECT {a})",
    ];
    for _ in 0..40 {
        qs.push(rng.pick(&shapes).replace("{a}", rng.pick(&aggs)));
    }
    let path = fresh("vm", S);
    let bad = diffs_vm(&path, &qs, false, true);
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        qs.len(),
        bad.join("\n")
    );
}
