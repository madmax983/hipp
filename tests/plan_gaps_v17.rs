//! v0.17 parity unit `gaps_plan_cli`: planner divergences the v0.16
//! and v0.17 units recorded as honest gaps, differentially vs the
//! pinned oracle (SQLite 3.54.1), each probed live on the pin first:
//!
//! - `INDEXED BY` names an index of the source's own table: a plain
//!   single-table scan (`SELECT a FROM t INDEXED BY nx`) took a path
//!   that never looked the index up, so an index of another table,
//!   a TEMP or attached table's, an automatic index's name or none at
//!   all was accepted (the pin: "no such index").
//! - A WHERE / inner-join ON term is simplified as the pin's
//!   `whereClauseInsert` does (`sqlite3ExprSimplifiedAndOr`, only a
//!   plain integer literal counting): `x OR 1` constrains nothing, so
//!   it no longer moves the join order, and `x OR 0` is `x`, which an
//!   index may serve; a LEFT JOIN's ON clause keeps its literals.
//! - WHERE push-down into a compound FROM subquery: arms of different
//!   affinities take the term, each comparing with its own (the pin's
//!   `pushDownWhereTerms`), and so do UNION / INTERSECT / EXCEPT arms
//!   whose columns are all BINARY; a term the pin's constant
//!   propagation rewrites is not pushed (its fixed column filters an
//!   arm wholly or not at all); the copy carries the leftmost arm's
//!   collation where the pin's `substExpr` adds one, so `x IS NOT
//!   NULL` under an untyped arm is no index range.
//! - The pin's compound column types (`sqlite3ExprDataType`) read
//!   through COLLATE and unary plus.
//! - A merged compound member is read in the order of the merge key's
//!   collation (the leftmost core's, `multiSelectCollSeq`), not its
//!   own: a BINARY index no longer serves a NOCASE key, so UNION
//!   keeps the pin's row of a NOCASE-equal group (a silent wrong
//!   answer).
//! - `x IS NULL` on a never-NULL column is the constant 0 in a lone
//!   table's planning too (`k IS NULL ORDER BY k` read a covering
//!   index); an ORDER BY term held constant by a WHERE equality must
//!   be a column (`upper(x) = 1 ORDER BY upper(x)` sorts); a merged
//!   aggregate member without GROUP BY needs no sort.
//!
//! Each query's rows (in order) and its EXPLAIN QUERY PLAN must be
//! the pin's, or both engines must fail.
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
    if out.status.success() && out.stderr.is_empty() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn fresh(tag: &str, schema: &str) -> PathBuf {
    let path = common::temp_db(&format!("v17pg-{tag}"));
    common::cli(&path, schema);
    path
}

/// Whether a plan difference is the one pre-existing gap this unit
/// leaves: the pin flattens a compound FROM subquery whose arms agree
/// in affinity into its reader, hipp keeps it a co-routine.
fn compound_flattened(pin_plan: &str, hipp_plan: &str) -> bool {
    (pin_plan.starts_with("QUERY PLAN\n`--COMPOUND QUERY")
        || pin_plan.starts_with("QUERY PLAN\n`--MERGE ("))
        && hipp_plan.starts_with("QUERY PLAN\n|--CO-ROUTINE")
}

/// Every difference of `qs` on the file at `path`: rows in order and
/// plans (`plans`: compare them; a plan the pin cannot give, as for
/// an error, needs none).
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

fn same(tag: &str, schema: &str, qs: &[&str]) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs_on(&path, &owned, true);
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

// ------------------------------------------------------- INDEXED BY

const IB: &str = "CREATE TABLE t(a, b); CREATE TABLE u(c, d); CREATE INDEX nx ON u(c); \
    CREATE TABLE ti(a, b); CREATE INDEX tia ON ti(a); \
    CREATE TABLE k(id INTEGER PRIMARY KEY, v UNIQUE); \
    CREATE TABLE w(p PRIMARY KEY, q) WITHOUT ROWID; CREATE INDEX wq ON w(q); \
    INSERT INTO t VALUES (1, 2), (3, 4); INSERT INTO u VALUES (5, 6); \
    INSERT INTO ti VALUES (2, 1), (1, 2); INSERT INTO k VALUES (1, 'x'); \
    INSERT INTO w VALUES ('p', 1);";

/// `INDEXED BY` resolves on the named source's own table in every
/// statement shape: plain scans (the path that never looked the name
/// up), WHERE / ORDER BY / aggregate forms, joins, subqueries, views,
/// CTEs, UPDATE / DELETE targets and FROM, and a real index of the
/// table still serves.
#[test]
fn indexed_by_names_an_index_of_its_own_table() {
    same(
        "ib",
        IB,
        &[
            "SELECT a FROM t INDEXED BY nx",
            "SELECT * FROM t INDEXED BY nx",
            "SELECT a, b FROM t INDEXED BY nosuch",
            "SELECT rowid FROM t INDEXED BY tia",
            "SELECT a FROM t INDEXED BY nx WHERE a = 1",
            "SELECT a FROM t INDEXED BY nx WHERE rowid = 1",
            "SELECT a FROM t INDEXED BY nx ORDER BY a",
            "SELECT count(*) FROM t INDEXED BY nx",
            "SELECT a + 1 FROM t INDEXED BY nx",
            "SELECT * FROM k INDEXED BY sqlite_autoindex_k_1",
            "SELECT * FROM t INDEXED BY sqlite_autoindex_k_1",
            "SELECT * FROM k INDEXED BY nx",
            "SELECT * FROM w INDEXED BY wq",
            "SELECT * FROM w INDEXED BY nx",
            "SELECT * FROM w INDEXED BY sqlite_autoindex_w_1",
            "SELECT a FROM ti INDEXED BY tia",
            "SELECT a, b FROM ti INDEXED BY tia WHERE a > 0",
            "SELECT * FROM ti NOT INDEXED",
            "SELECT * FROM t NOT INDEXED WHERE a = 1",
            "SELECT * FROM t, u INDEXED BY tia",
            "SELECT * FROM t INDEXED BY nx, u",
            "SELECT * FROM t LEFT JOIN u INDEXED BY nx ON 1",
            "SELECT * FROM u LEFT JOIN t INDEXED BY nx ON 1",
            "SELECT (SELECT a FROM t INDEXED BY nx)",
            "SELECT 1 WHERE EXISTS (SELECT 1 FROM t INDEXED BY nx)",
            "SELECT * FROM (SELECT a FROM t INDEXED BY nx)",
            "WITH c AS (SELECT a FROM t INDEXED BY nx) SELECT * FROM c",
            "SELECT a FROM t WHERE a IN (SELECT c FROM u INDEXED BY tia)",
            "SELECT a FROM t INDEXED BY nx UNION SELECT 1",
        ],
    );
    // Writes and schema objects: errors on both, files unchanged.
    for (i, q) in [
        "UPDATE t INDEXED BY nx SET b = 0",
        "DELETE FROM t INDEXED BY nx",
        "DELETE FROM t INDEXED BY nosuch WHERE a = 1",
        "UPDATE t SET b = 1 FROM t AS t2 INDEXED BY nx",
        "UPDATE ti INDEXED BY tia SET b = 9 WHERE a = 1",
        "CREATE VIEW v AS SELECT a FROM t INDEXED BY nx; SELECT * FROM v",
    ]
    .iter()
    .enumerate()
    {
        let path = fresh(&format!("ibw{i}"), IB);
        let p = pin(&path, q);
        let path2 = fresh(&format!("ibw{i}h"), IB);
        let h = block_on(async {
            let fs = FileStorage::open(&path2).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            let mut ok = true;
            for s in q.split("; ") {
                if collect(&mut db, s).await.is_err() {
                    ok = false;
                    break;
                }
            }
            ok
        });
        assert_eq!(p.is_ok(), h, "{q}: pin {p:?}");
        let dump = |p: &Path| {
            pin(
                p,
                "SELECT * FROM t; SELECT * FROM ti; SELECT count(*) FROM sqlite_schema",
            )
        };
        assert_eq!(dump(&path), dump(&path2), "{q}");
    }
}

/// A TEMP table's and an attached database's sources resolve their
/// INDEXED BY in their own database (one session each).
#[test]
fn indexed_by_on_temp_and_attached_tables() {
    let path = fresh("ibt", IB);
    let aux = common::temp_db("v17pg-ibt-aux");
    let aux_s = aux.to_string_lossy().replace('\'', "''");
    let scripts = [
        "CREATE TEMP TABLE tt(x); INSERT INTO tt VALUES (1); SELECT x FROM tt INDEXED BY nx".to_string(),
        "CREATE TEMP TABLE tt(x); CREATE INDEX tx ON tt(x); INSERT INTO tt VALUES (2); SELECT x FROM tt INDEXED BY tx".to_string(),
        "CREATE TEMP TABLE tt(x); CREATE INDEX tx ON tt(x); SELECT a FROM t INDEXED BY tx".to_string(),
        format!("ATTACH '{aux_s}' AS aux; CREATE TABLE IF NOT EXISTS aux.z(q); INSERT INTO aux.z VALUES (1); SELECT * FROM aux.z INDEXED BY nx"),
        format!("ATTACH '{aux_s}' AS aux; CREATE INDEX IF NOT EXISTS aux.zq ON z(q); SELECT * FROM aux.z INDEXED BY zq"),
        format!("ATTACH '{aux_s}' AS aux; SELECT * FROM t INDEXED BY zq"),
    ];
    for s in scripts.iter() {
        let p = pin(&path, s);
        let h = block_on(async {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            let mut out = Vec::new();
            for stmt in s.split("; ") {
                match collect(&mut db, stmt).await {
                    Ok(r) => out.extend_from_slice(&r),
                    Err(_) => return Err(()),
                }
            }
            Ok(String::from_utf8_lossy(&out).to_string())
        });
        match (&p, &h) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{s}"),
            (Err(_), Err(())) => {}
            _ => panic!("{s}\n  pin : {p:?}\n  hipp: {h:?}"),
        }
    }
}

// ------------------------------------------------- OR 1 / OR 0 terms

const OR: &str = "CREATE TABLE a(a INTEGER PRIMARY KEY, x); CREATE TABLE b(a, y); \
    CREATE INDEX by ON b(y); CREATE TABLE c(k, v); \
    INSERT INTO a VALUES (1, 1), (2, 2), (3, 3); \
    INSERT INTO b VALUES (1, 10), (2, 20), (-1, 30), (3, 1); \
    INSERT INTO c VALUES (1, 'p'), (2, 'q');";

/// Constant-true and constant-false operands of a WHERE / ON term:
/// the pin's simplification decides the join order (and so the order
/// of unordered rows) and whether an index serves what is left.
#[cfg(feature = "planner")]
#[test]
fn or_true_terms_keep_the_pin_join_order() {
    same(
        "or",
        OR,
        &[
            "SELECT * FROM a JOIN b ON b.a > 0 OR 1",
            "SELECT * FROM a JOIN b ON 1 OR b.a > 0",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 2",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 0x1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR true",
            "SELECT * FROM a JOIN b ON b.a > 0 OR (1)",
            "SELECT * FROM a JOIN b ON b.a > 0 OR (1 AND 1)",
            "SELECT * FROM a JOIN b ON b.a > 0 OR (1 OR b.y)",
            "SELECT * FROM a JOIN b ON b.a > 0 OR (b.y AND 0) OR 1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR -1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR +1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 1.0",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 'x'",
            "SELECT * FROM a JOIN b ON b.a > 0 OR (1+1)",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 1 COLLATE nocase",
            "SELECT * FROM a JOIN b ON b.a > 0 OR likely(1)",
            "SELECT * FROM a JOIN b ON (b.a > 0 OR 1) = 1",
            "SELECT * FROM a JOIN b ON NOT (b.a > 0 OR 1)",
            "SELECT * FROM a JOIN b ON CASE WHEN b.a > 0 OR 1 THEN 1 END",
            "SELECT * FROM a JOIN b ON b.a > 0",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 0",
            "SELECT * FROM a JOIN b ON b.a > 0 AND 1",
            "SELECT * FROM a, b WHERE b.a > 0 OR 1",
            "SELECT * FROM a JOIN b ON 1 WHERE b.a > 0 OR 1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 1 WHERE a.x > 1",
            "SELECT * FROM a JOIN b ON (b.a > 0 OR 1) AND (a.x > 0 OR 1)",
            "SELECT * FROM a LEFT JOIN b ON b.a > 0 OR 1",
            "SELECT * FROM a LEFT JOIN b ON b.y = a.x OR 0",
            "SELECT * FROM a LEFT JOIN b ON b.y = a.x AND 1",
            "SELECT * FROM a LEFT JOIN b ON (b.y = a.x OR 0) AND b.a > 0",
            "SELECT * FROM a LEFT JOIN b ON b.y = a.x WHERE b.a > 0 OR 0",
            "SELECT * FROM a LEFT JOIN b ON 1 WHERE b.a > 0 OR 1",
            "SELECT * FROM a JOIN b ON b.y = a.x OR 0",
            "SELECT * FROM a JOIN b ON b.y = a.x OR false",
            "SELECT * FROM a JOIN b ON b.y = a.x AND (a.x = 5 OR 1)",
            "SELECT * FROM a JOIN b JOIN c ON b.a > 0 OR 1 WHERE c.k = a.a OR 1",
            "SELECT * FROM a JOIN b ON b.a > 0 OR 1 ORDER BY 1, 3",
            "SELECT count(*) FROM a JOIN b ON b.a > abs(-9223372036854775808) OR 1",
        ],
    );
}

/// The simplification on a lone table: `x OR 0` is `x` (an index
/// serves it), a simplified AND stays one term, `x OR 1` is no term.
#[test]
fn or_false_terms_reach_the_index() {
    same(
        "or1",
        OR,
        &[
            "SELECT * FROM b WHERE b.y = 10 OR 0",
            "SELECT * FROM b WHERE (b.y = 10 OR 0) AND 1",
            "SELECT * FROM b WHERE b.y = 10 OR (0 AND b.a)",
            "SELECT * FROM b WHERE b.y = 10 OR +0",
            "SELECT * FROM b WHERE b.y = 10 OR -0",
            "SELECT * FROM b WHERE (b.y = 10 OR 0) AND b.a > 0",
            "SELECT * FROM b WHERE b.a > 0 AND (b.y = 10 OR 0)",
            "SELECT * FROM b WHERE ((b.y = 10 OR 0) OR 0)",
            "SELECT * FROM b WHERE (b.y = 10 OR 0) OR b.a = 7",
            "SELECT * FROM b WHERE (b.y = 10 AND b.a > 0) OR 0",
            "SELECT * FROM b WHERE (b.y = 10 AND b.a > 0) OR (0 AND b.a)",
            "SELECT * FROM b WHERE b.y IN (10, 20) OR 0",
            "SELECT * FROM b WHERE (b.y = 10 OR b.y = 20) OR 0",
            "SELECT * FROM b WHERE b.y = 10 OR b.a = 6 OR 0",
            "SELECT * FROM b WHERE likely(b.y = 10 OR 0)",
            "SELECT * FROM b WHERE (b.y = 10 OR 0) COLLATE nocase",
            "SELECT * FROM b WHERE b.y = 10 AND (b.a = 1 OR 1)",
            "SELECT * FROM b WHERE b.a > 0 OR 1",
            "SELECT * FROM b WHERE b.y > 5 OR 1 ORDER BY y",
            "SELECT * FROM b WHERE abs(-9223372036854775808) > 0 OR 1",
            "UPDATE b SET a = a WHERE b.y = 10 OR 0",
            "DELETE FROM b WHERE b.y = 99 OR 0",
        ],
    );
}

// ------------------------------------------- compound push-down

const PD: &str = "CREATE TABLE t(r REAL, i INTEGER, x TEXT, n NUMERIC, b BLOB, k INTEGER PRIMARY KEY); \
    INSERT INTO t(r, i, x, n, b) VALUES (1, 1, '1', 1, 1), (2.5, 2, '2', 2.5, '2'), \
    (NULL, 3, '1.0', '1.0', x'31'), ('a', ' 1', 'abc', '3', 2.0), (3, '3', ' 3', 3, NULL), \
    (0, 0, '0', '0', '0'); \
    CREATE TABLE u(p, q TEXT, s REAL, z TEXT COLLATE NOCASE); \
    INSERT INTO u VALUES (1, '1', 1, 'a'), ('1', 1, '1', 'A'), (2.0, '2.0', 2, '1'), (NULL, 'x', NULL, NULL); \
    CREATE INDEX ti ON t(i); CREATE INDEX tx ON t(x); CREATE INDEX uq ON u(q); CREATE INDEX uz ON u(z); \
    CREATE VIEW v AS SELECT r AS c FROM t UNION ALL SELECT 1;";

/// The v0.16 honest gap and its neighbours: arms of mixed affinity
/// each compare the pushed term with their own affinity.
#[test]
fn compound_push_down_compares_per_arm() {
    same(
        "pd",
        PD,
        &[
            "SELECT * FROM (SELECT r AS c FROM t UNION ALL SELECT 1) WHERE c = '1'",
            "SELECT * FROM (SELECT 1 AS c UNION ALL SELECT r FROM t) WHERE c = '1'",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c <> 1",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c = '1'",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c > '1'",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c IS 1",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c BETWEEN 1 AND 1",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t) WHERE c IN (1, 2)",
            "SELECT * FROM (SELECT n AS c FROM t UNION ALL SELECT x FROM t) WHERE c = '2.5'",
            "SELECT * FROM (SELECT b AS c FROM t UNION ALL SELECT i FROM t) WHERE c = '2'",
            "SELECT * FROM (SELECT i AS c FROM t UNION ALL SELECT x FROM t UNION ALL SELECT r FROM t) WHERE c = '1'",
            "SELECT c, typeof(c) FROM (SELECT r AS c FROM t UNION ALL SELECT 1) WHERE c = '1' ORDER BY 2, 1",
            "SELECT * FROM v WHERE c = '1'",
            "WITH w AS (SELECT r AS c FROM t UNION ALL SELECT 1) SELECT * FROM w WHERE c = '1'",
            "SELECT t2.k, c FROM t AS t2 JOIN (SELECT r AS c FROM t UNION ALL SELECT 1) ON c = '1' WHERE t2.k = 1",
            // UNION / INTERSECT / EXCEPT arms take the term too.
            "SELECT c FROM (SELECT 2 AS c FROM t UNION ALL SELECT s FROM u UNION SELECT '1') WHERE c <> '1.0' AND c <= '1'",
            "SELECT c FROM (SELECT p+0 AS c FROM u UNION SELECT 2 FROM t UNION ALL SELECT x FROM t) WHERE c IS NOT 1",
            "SELECT c FROM (SELECT s AS c FROM u UNION SELECT k FROM t WHERE k > 2 UNION ALL SELECT x FROM t) WHERE c < ' 1'",
            "SELECT c FROM (SELECT k AS c FROM t EXCEPT SELECT 1) WHERE c > 2",
            "SELECT c FROM (SELECT x AS c FROM t INTERSECT SELECT q FROM u) WHERE c = 1",
            // ... unless a column is not BINARY.
            "SELECT c FROM (SELECT x COLLATE NOCASE AS c FROM t UNION SELECT q FROM u) WHERE c = 'ABC'",
            "SELECT c FROM (SELECT z AS c FROM u UNION SELECT x FROM t) WHERE c = 'A'",
            // A term constant propagation rewrites stays in the reader.
            "SELECT c, typeof(c) FROM (SELECT 2 AS c FROM t UNION ALL SELECT s FROM u UNION ALL SELECT +i FROM t) WHERE c == 3 AND c >= '2' ORDER BY 2, 1",
            "SELECT c FROM (SELECT CAST(r AS TEXT) AS c FROM t UNION ALL SELECT n FROM t WHERE k > 2) WHERE c = 1 AND c < '1'",
            "SELECT c FROM (SELECT r AS c FROM t UNION ALL SELECT x FROM t) WHERE c = 1 AND c + 0 = 1",
            // Collations the pin's `substExpr` adds.
            "SELECT c FROM (SELECT i+0 AS c FROM t WHERE k > 0 UNION ALL SELECT x FROM t) WHERE c IS NOT NULL",
            "SELECT c FROM (SELECT x AS c FROM t WHERE k > 0 UNION ALL SELECT i+0 FROM t) WHERE c IS NOT NULL",
            "SELECT c FROM (SELECT x AS c FROM t WHERE k > 0 UNION ALL SELECT i FROM t) WHERE c = 'a'",
            // Compound column types through COLLATE and unary plus.
            "SELECT count(*) FROM (SELECT s AS c FROM u UNION ALL SELECT x COLLATE NOCASE FROM t) WHERE c <> 0 AND c < 2.5",
            "SELECT c, typeof(c) FROM (SELECT s AS c FROM u UNION ALL SELECT +x FROM t)",
            "SELECT c, typeof(c) FROM (SELECT s AS c FROM u UNION ALL SELECT x COLLATE NOCASE FROM t)",
            "SELECT t2.k, c FROM t AS t2 JOIN (SELECT x||'' AS c FROM t UNION ALL SELECT x COLLATE NOCASE FROM t UNION ALL SELECT q FROM u) ON c <= 2 WHERE t2.k = 1",
            "SELECT t2.k, c FROM t AS t2 JOIN (SELECT k AS c FROM t UNION ALL SELECT x COLLATE NOCASE FROM t) ON c IN (0) WHERE t2.k = 1",
        ],
    );
    // Rows only: the pin builds an automatic index and a Bloom filter
    // on the materialisation, which hipp's join planner does not
    // (an older honest gap).
    let path = fresh("pdl", PD);
    let qs = ["SELECT t2.k, c FROM t AS t2 LEFT JOIN (SELECT r AS c FROM t UNION ALL SELECT 1) ON c = '1' WHERE t2.k < 3".to_string()];
    let bad = diffs_on(&path, &qs, false);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A generated corpus of compound FROM subqueries over every affinity
/// and operator: rows in order and plans (the pin's flattening of a
/// compound whose arms agree in affinity aside, an older gap).
#[test]
fn generated_compound_push_down_matches_pin() {
    let path = fresh("pdg", PD);
    let cols = [
        "r",
        "i",
        "x",
        "n",
        "b",
        "k",
        "i+0",
        "x||''",
        "CAST(r AS TEXT)",
        "+i",
        "1",
        "'1'",
        "1.0",
        "NULL",
        "2",
        "'2'",
        "x COLLATE NOCASE",
        "upper(x)",
    ];
    let ucols = ["p", "q", "s", "p+0", "'1'", "1", "z", "count(*)", "max(s)"];
    let consts = [
        "'a'", "1", "'1'", "1.0", "'1.0'", "2", "'2'", "' 1'", "3", "'abc'", "NULL", "0", "'0'",
        "x'31'", "2.5",
    ];
    let ops = ["=", "<>", "<", ">", "<=", ">=", "IS", "IS NOT", "==", "!="];
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut qs = Vec::new();
    // (A join's plans are not compared: the pin orders a co-routine
    // among the loops, hipp keeps FROM order, an older honest gap.)
    let mut joins = Vec::new();
    for _ in 0..240 {
        let n = 2 + r.n(2);
        let mut sub = String::new();
        for a in 0..n {
            if a > 0 {
                sub.push_str(
                    [
                        " UNION ALL ",
                        " UNION ALL ",
                        " UNION ALL ",
                        " UNION ",
                        " INTERSECT ",
                        " EXCEPT ",
                    ][r.n(6) as usize],
                );
            }
            let alias = if a == 0 { " AS c" } else { "" };
            match r.n(10) {
                0..=2 => sub.push_str(&format!("SELECT {}{alias} FROM u", r.pick(&ucols))),
                3 => sub.push_str(&format!(
                    "SELECT {}{alias}",
                    r.pick(&["1", "'1'", "1.0", "NULL", "2", "'abc'"])
                )),
                _ => {
                    let w = if r.n(10) < 7 {
                        String::new()
                    } else {
                        format!(" WHERE k > {}", r.n(5))
                    };
                    sub.push_str(&format!("SELECT {}{alias} FROM t{w}", r.pick(&cols)));
                }
            }
        }
        let term = |r: &mut Rng| -> String {
            match r.n(20) {
                0..=1 => format!("c BETWEEN {} AND {}", r.pick(&consts), r.pick(&consts)),
                2..=3 => format!("c IN ({}, {})", r.pick(&consts), r.pick(&consts)),
                4 => format!("c IN ({})", r.pick(&consts)),
                5 => format!("c LIKE {}", r.pick(&consts)),
                6 => "c IS NULL".to_string(),
                7 => format!("c + 0 {} {}", r.pick(&ops), r.pick(&consts)),
                8 => format!("{} {} c", r.pick(&consts), r.pick(&ops)),
                _ => format!("c {} {}", r.pick(&ops), r.pick(&consts)),
            }
        };
        let mut w = term(&mut r);
        if r.n(10) < 3 {
            w = format!("{w} AND {}", term(&mut r));
        }
        match r.n(10) {
            0..=5 => qs.push(format!("SELECT c FROM ({sub}) WHERE {w}")),
            6 => qs.push(format!("WITH w AS ({sub}) SELECT c FROM w WHERE {w}")),
            7 => qs.push(format!(
                "SELECT c, typeof(c) FROM ({sub}) WHERE {w} ORDER BY 2, 1"
            )),
            8 => qs.push(format!("SELECT count(*) FROM ({sub}) WHERE {w}")),
            _ => joins.push(format!(
                "SELECT t2.k, c FROM t AS t2 JOIN ({sub}) ON {w} WHERE t2.k = 1"
            )),
        }
    }
    let mut bad = diffs_on(&path, &qs, true);
    bad.extend(diffs_on(&path, &joins, false));
    assert!(
        bad.is_empty(),
        "{} of 240 differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

// ------------------------------------- merge keys and lone tables

const MK: &str = "CREATE TABLE t(x TEXT, i INT, k INTEGER PRIMARY KEY); CREATE INDEX tx ON t(x); \
    CREATE INDEX ti ON t(i); \
    INSERT INTO t(x, i) VALUES ('b', 1), ('A', 2), ('a', 3), ('B', 4), ('c', 5), ('C', 6), ('a', 7); \
    CREATE TABLE u(p, q TEXT, s REAL); CREATE INDEX uq ON u(q); \
    INSERT INTO u VALUES (1, 'a', 1.5), (2, 'B', 2.5); \
    CREATE TABLE nn(x TEXT NOT NULL, k INTEGER PRIMARY KEY); CREATE INDEX nnx ON nn(x); \
    INSERT INTO nn(x) VALUES ('b'), ('a'); \
    CREATE TABLE w(a PRIMARY KEY, b) WITHOUT ROWID; CREATE INDEX wb ON w(b); \
    INSERT INTO w VALUES ('k', 1), ('j', 2);";

/// A merged compound member reads in the merge key's collation (the
/// silent wrong answer: UNION kept `C` where the pin keeps `c`).
#[test]
fn merge_members_read_in_the_key_collation() {
    same(
        "mk",
        MK,
        &[
            "SELECT x COLLATE NOCASE AS c FROM t UNION SELECT x FROM t WHERE k > 2",
            "SELECT x COLLATE NOCASE AS c FROM t WHERE k < 3 UNION SELECT x FROM t WHERE k > 2",
            "SELECT x COLLATE NOCASE AS c FROM t WHERE k < 3 EXCEPT SELECT x FROM t WHERE k > 2",
            "SELECT x COLLATE NOCASE AS c FROM t WHERE k < 3 INTERSECT SELECT x FROM t WHERE k > 2",
            "SELECT x FROM t WHERE k < 4 UNION SELECT x COLLATE NOCASE FROM t",
            "SELECT x||'' FROM t WHERE k < 4 UNION SELECT x COLLATE NOCASE FROM t",
            "SELECT * FROM (SELECT x COLLATE NOCASE AS c FROM t UNION SELECT x FROM t WHERE k > 2)",
            "SELECT x COLLATE NOCASE FROM t UNION ALL SELECT x FROM t UNION SELECT x FROM t",
            "VALUES ('a') UNION SELECT x FROM t",
            "SELECT k||'' FROM t UNION SELECT x FROM t",
            "SELECT x COLLATE NOCASE, i FROM t UNION SELECT q, p FROM u",
            "SELECT q COLLATE NOCASE FROM u UNION SELECT x FROM t ORDER BY 1",
            // A merged aggregate member without GROUP BY sorts nothing.
            "SELECT count(*) AS c FROM u UNION SELECT x FROM t",
            "SELECT max(s) AS c FROM u HAVING max(s) > 1 UNION SELECT x FROM t",
            "SELECT x FROM t UNION SELECT count(*) FROM u",
            "SELECT x FROM t UNION SELECT count(*) FROM u GROUP BY p",
            "SELECT count(*), max(x) FROM t INTERSECT SELECT count(*), 1 FROM u ORDER BY 1",
            "SELECT count(DISTINCT p) FROM u UNION SELECT x FROM t",
        ],
    );
}

/// A lone table's planning: `x IS [NOT] NULL` on a never-NULL column
/// is a constant, an ORDER BY term held by a WHERE equality must be a
/// column, and `x IS NOT NULL` is a range only on a bare column.
#[test]
fn lone_table_constant_terms() {
    same(
        "lt",
        MK,
        &[
            "SELECT k FROM t WHERE k IS NULL ORDER BY k",
            "SELECT k FROM t WHERE k IS NULL AND k <> 1 ORDER BY k",
            "SELECT k FROM t WHERE k IS NULL",
            "SELECT k FROM t WHERE k IS NULL EXCEPT SELECT 1",
            "SELECT * FROM nn WHERE x IS NULL ORDER BY k",
            "SELECT * FROM nn WHERE x IS NOT NULL ORDER BY x",
            "SELECT * FROM nn WHERE x IS NOT NULL",
            "SELECT * FROM nn WHERE x IS NOT NULL AND x > 'a'",
            "SELECT * FROM t WHERE rowid IS NOT NULL ORDER BY i",
            "SELECT * FROM t WHERE k IS NOT NULL AND i = 3 ORDER BY k",
            "SELECT i FROM t WHERE k IS NULL GROUP BY i",
            "SELECT * FROM w WHERE a IS NULL ORDER BY a",
            "SELECT * FROM w WHERE a IS NOT NULL ORDER BY b",
            "SELECT x FROM t WHERE x IS NOT NULL",
            "SELECT x FROM t WHERE x COLLATE NOCASE IS NOT NULL",
            "SELECT x FROM t WHERE x COLLATE BINARY IS NOT NULL",
            "SELECT x FROM t WHERE +x IS NOT NULL",
            "SELECT x FROM t WHERE upper(x) = 'A' ORDER BY upper(x)",
            "SELECT x FROM t WHERE x||'' = 'a' ORDER BY x||''",
            "SELECT x FROM t WHERE +i = 3 ORDER BY +i",
            "SELECT x FROM t WHERE i = 3 ORDER BY i",
            "SELECT x FROM t WHERE i = 3 ORDER BY i, k",
            "SELECT x FROM t WHERE abs(i) = 3 ORDER BY abs(i), x",
            "DELETE FROM nn WHERE x IS NULL",
            "UPDATE t SET i = i WHERE k IS NOT NULL AND i = 2",
        ],
    );
}

// ------------------------------------------------------ review fixes

/// `same`, rows only (a VALUES chain's plan is an honest gap).
fn same_rows(tag: &str, schema: &str, qs: &[&str]) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs_on(&path, &owned, false);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

const VA: &str = "CREATE TABLE t(r REAL, i INTEGER, s TEXT); \
    INSERT INTO t VALUES (1.0, 1, '1'), (2.5, 2, '2'); \
    CREATE TABLE u(y); INSERT INTO u VALUES (1), (2); \
    CREATE VIEW v1 AS SELECT * FROM (VALUES (3), (4) UNION ALL SELECT r FROM t); \
    CREATE VIEW v2(a) AS VALUES (3), (4) UNION ALL SELECT s FROM t;";

/// Review fix: a multi-row VALUES arm and a window arm take their part
/// in a compound's column type as on the pin (`sqlite3MultiValues`: a
/// co-routine of untyped values, or after a WITH clause / in a view a
/// chain of one-row selects; a window call is a function). They were
/// read as the leftmost arm's type, so integers came back as reals
/// and a WHERE matched rows the pin rejects.
#[test]
fn values_and_window_arms_type_like_the_pin() {
    same_rows(
        "va",
        VA,
        &[
            // The review's repros.
            "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT s FROM t) SELECT x FROM q WHERE x = 2",
            "WITH q(x) AS (VALUES (3), ('abc') UNION ALL SELECT s COLLATE NOCASE FROM t) \
             SELECT x, typeof(x) FROM q WHERE 2 = x COLLATE NOCASE",
            "SELECT column1, typeof(column1) FROM (VALUES (1), ('1') UNION ALL SELECT r FROM t) \
             WHERE column1 = 1",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL SELECT row_number() OVER () FROM u)",
            "SELECT typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (1), (2))",
            "SELECT x FROM (SELECT r x FROM t UNION ALL SELECT row_number() OVER () FROM u) \
             WHERE x = '1'",
            // A co-routine VALUES first, and one in a chain.
            "SELECT column1, typeof(column1) FROM (VALUES (3), (4) UNION ALL SELECT r FROM t)",
            "WITH q(x) AS (VALUES (3), (4) UNION ALL SELECT r FROM t) SELECT x, typeof(x) FROM q",
            "WITH w AS (SELECT 1) SELECT column1, typeof(column1) \
             FROM (VALUES (3), (4) UNION ALL SELECT r FROM t)",
            "SELECT column1, typeof(column1) FROM (VALUES (3), (CAST(4 AS REAL)) UNION ALL SELECT 5)",
            "WITH q(x) AS (VALUES (3), (CAST(4 AS REAL)) UNION ALL SELECT 5) SELECT x, typeof(x) FROM q",
            "SELECT column1, typeof(column1) FROM (VALUES (CAST(4 AS TEXT)), (3)) WHERE column1 = 3",
            "SELECT column1, typeof(column1) FROM (VALUES (3.5), (4) UNION ALL SELECT r FROM t)",
            "SELECT column1, typeof(column1) FROM (VALUES (3), (abs(-4)) UNION ALL SELECT r FROM t)",
            // Later arms: co-routine, chain, one row.
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (1))",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (1), (2) UNION ALL SELECT 7)",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (1), (CAST(2 AS REAL)))",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (CAST(1 AS REAL)), (2))",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (CAST(1 AS REAL)), (2) \
             UNION ALL SELECT 9)",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (CAST(1 AS TEXT)), (2) \
             UNION ALL SELECT 9)",
            "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL VALUES (1), (2) UNION ALL SELECT r FROM t)",
            "SELECT x, typeof(x) FROM (SELECT s x FROM t UNION ALL VALUES (1), (2)) WHERE x = 1",
            "WITH q(x) AS (SELECT r FROM t UNION ALL VALUES (1), (2)) SELECT x, typeof(x) FROM q",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL VALUES (1), ('z'))",
            // Function arms (a window call, LIKE, an aggregate).
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL SELECT y FROM u)",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL SELECT y + 0 FROM u)",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL SELECT count(*) FROM u)",
            "SELECT x, typeof(x) FROM (SELECT r x FROM t UNION ALL SELECT y LIKE '1' FROM u)",
            "WITH q(x) AS (SELECT r FROM t UNION ALL SELECT row_number() OVER (ORDER BY y) FROM u) \
             SELECT x, typeof(x) FROM q WHERE x = '1'",
            // A UNION with a non-BINARY VALUES column takes no term
            // (the pin's restriction 8).
            "SELECT * FROM (VALUES ('b' COLLATE NOCASE) UNION SELECT s FROM t) WHERE column1 = 'B'",
            "SELECT * FROM (VALUES ('b' COLLATE NOCASE), ('c') UNION SELECT s FROM t) \
             WHERE column1 = 'B'",
            "SELECT * FROM (VALUES ('2'), ('c') UNION SELECT s FROM t) WHERE column1 = '2'",
            // Views read their text as the pin's schema does: a chain.
            "SELECT column1, typeof(column1) FROM v1",
            "SELECT a, typeof(a) FROM v2 WHERE a = 3",
            "PRAGMA table_info(v1)",
            "PRAGMA table_info(v2)",
        ],
    );
    // CREATE TABLE ... AS takes the same types.
    let path = fresh("vac", VA);
    let q = "CREATE TABLE c1 AS SELECT * FROM (VALUES (1), (2) UNION ALL SELECT r FROM t); \
             CREATE TABLE c2 AS WITH q AS (VALUES (1), (2) UNION ALL SELECT r FROM t) SELECT * FROM q; \
             CREATE TABLE c3 AS SELECT * FROM (SELECT r FROM t UNION ALL VALUES (1), (2)); \
             CREATE TABLE c4 AS SELECT * FROM (SELECT r FROM t UNION ALL \
             SELECT row_number() OVER () FROM u); \
             CREATE TABLE c5 AS SELECT * FROM (VALUES (CAST(1 AS REAL)), (2) UNION ALL SELECT r FROM t); \
             SELECT sql FROM sqlite_schema WHERE name LIKE 'c%' ORDER BY name";
    let path2 = fresh("vach", VA);
    let p = pin(&path, q);
    let h = block_on(async {
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut out = Vec::new();
        for s in q.split("; ") {
            out = collect(&mut db, s).await.expect("statement");
        }
        String::from_utf8(out).expect("utf8")
    });
    assert_eq!(p.as_deref(), Ok(h.as_str()));
}

/// Generated compounds of VALUES (one row, several, first or later),
/// typed and untyped SELECT arms and window arms, read bare, through
/// typeof() and under a WHERE term — in FROM subqueries (co-routine
/// VALUES) and, with no WHERE, in CTEs (chained VALUES).
#[test]
fn generated_values_arms_match_pin() {
    let path = fresh("vag", VA);
    let vals = [
        "1",
        "2",
        "'1'",
        "'abc'",
        "1.0",
        "2.5",
        "NULL",
        "CAST(3 AS REAL)",
        "CAST(1 AS TEXT)",
        "x'31'",
    ];
    let arms = [
        "SELECT r FROM t",
        "SELECT i FROM t",
        "SELECT s FROM t",
        "SELECT y FROM u",
        "SELECT y + 0 FROM u",
        "SELECT row_number() OVER () FROM u",
        "SELECT s COLLATE NOCASE FROM t",
        "SELECT 1",
        "SELECT '1'",
        "SELECT CAST(i AS REAL) FROM t",
    ];
    let consts = ["1", "'1'", "2", "'2'", "1.0", "3", "'abc'", "2.5"];
    let ops = ["=", "<>", "<", ">=", "IS"];
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut qs = Vec::new();
    for _ in 0..200 {
        let values = |r: &mut Rng| -> String {
            let k = 1 + r.n(3);
            let mut v = String::from("VALUES ");
            for j in 0..k {
                if j > 0 {
                    v.push_str(", ");
                }
                v.push_str(&format!("({})", r.pick(&vals)));
            }
            v
        };
        let n = 2 + r.n(2);
        let mut sub = String::new();
        let mut first_values = false;
        for a in 0..n {
            if a > 0 {
                sub.push_str(if r.n(5) == 0 {
                    " UNION "
                } else {
                    " UNION ALL "
                });
            }
            if r.n(3) == 0 {
                first_values |= a == 0;
                sub.push_str(&values(&mut r));
            } else if a == 0 {
                sub.push_str(&format!(
                    "SELECT {} AS column1 FROM t",
                    r.pick(&["r", "i", "s", "1", "'1'"])
                ));
            } else {
                sub.push_str(r.pick(&arms));
            }
        }
        let _ = first_values;
        let w = format!("column1 {} {}", r.pick(&ops), r.pick(&consts));
        match r.n(5) {
            0 => qs.push(format!("SELECT column1, typeof(column1) FROM ({sub})")),
            1 | 2 => qs.push(format!(
                "SELECT column1, typeof(column1) FROM ({sub}) WHERE {w}"
            )),
            3 => qs.push(format!(
                "WITH q AS ({sub}) SELECT column1, typeof(column1) FROM q"
            )),
            _ => qs.push(format!("SELECT count(*) FROM ({sub}) WHERE {w}")),
        }
    }
    let bad = diffs_on(&path, &qs, false);
    assert!(
        bad.is_empty(),
        "{} of 200 differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

const JO: &str = "CREATE TABLE b(x, y, z); CREATE TABLE c(x PRIMARY KEY, y, z); \
    CREATE TABLE d(x, y); CREATE INDEX bx ON b(x); \
    CREATE TABLE e(x INTEGER, y TEXT); CREATE INDEX ex ON e(x); \
    CREATE TABLE t(c TEXT, k); CREATE TABLE u(n INTEGER, b, r REAL); CREATE INDEX un ON u(n); \
    INSERT INTO b VALUES (1, 'p', 1), (2, 'q', 2), (2, 'r', 3), (5, NULL, 4); \
    INSERT INTO c VALUES (1, 'c1', 1), (2, 'c2', 2), (9, 'c9', 3), ('a', 'x', 1); \
    INSERT INTO d VALUES (1, 1), (2, 2), (1, 2), (3, 1); \
    INSERT INTO e VALUES (1, '1'), (2, '2'), (5, '5'), (NULL, 'n'); \
    INSERT INTO t VALUES ('5', 1), ('05', 2), ('5.0', 3), (5, 4), ('abc', 5); \
    INSERT INTO u VALUES (5, '5', 5.0), (5, 5, 5.5), ('abc', 'abc', 0);";

/// Review fixes in the join planner, rows in order and plans:
/// - `x OR 0` fixes no column for constant propagation (the pin runs
///   it before `whereClauseInsert` simplifies the term), so `t.c =
///   u.n` stays a join constraint;
/// - every source after a CROSS JOIN keeps its prerequisites (a later
///   inner join no longer moves before the CROSS JOIN's left side);
/// - a LEFT JOIN's constant ON term counts against its right side.
#[test]
fn join_review_fixes_match_pin() {
    same(
        "jo",
        JO,
        &[
            "SELECT * FROM t, u WHERE t.c = u.n AND (u.n = 5 OR 0)",
            "SELECT * FROM t, u WHERE (u.n = 5 OR 0) AND t.c = u.n",
            "SELECT * FROM t, u WHERE t.c = u.n AND (u.n = 5 OR 1)",
            "SELECT * FROM t, u WHERE t.c = u.n AND u.n = 5",
            "SELECT * FROM d, b WHERE d.x = b.x AND (b.x = 2 OR 0)",
            "SELECT * FROM d, b WHERE b.x = 2 AND (d.x = b.x OR 0)",
            "SELECT * FROM t JOIN u ON t.c = u.n AND (u.n = 5 OR 0)",
            "SELECT * FROM c CROSS JOIN d JOIN b ON b.x = 1",
            "SELECT c.x, d.x, b.y FROM c CROSS JOIN d JOIN b ON b.x = 2 LIMIT 6",
            "SELECT * FROM c CROSS JOIN d, b WHERE b.x = 1",
            "SELECT * FROM c, d JOIN b ON b.x = 1",
            "SELECT * FROM c CROSS JOIN d JOIN b ON b.x = 1 JOIN e ON e.x = 2",
            "SELECT * FROM c JOIN d CROSS JOIN b JOIN e ON e.x = 2",
            "SELECT * FROM c LEFT JOIN d ON d.x = c.x CROSS JOIN b JOIN e ON e.x = 2",
            "SELECT * FROM c CROSS JOIN d LEFT JOIN b ON b.x = d.x JOIN e ON e.x = 2",
            "SELECT * FROM d, c CROSS JOIN b JOIN e ON e.x = 2",
            "SELECT * FROM c LEFT JOIN d ON d.x = c.x JOIN b ON b.x = 1 JOIN e ON e.x = 2",
            "SELECT * FROM c LEFT JOIN d ON d.x = c.x JOIN b ON b.x = 1 CROSS JOIN e JOIN u ON u.n = 3",
            "SELECT * FROM c CROSS JOIN d, b, e WHERE b.x = 1 AND e.x = 2",
            "SELECT d.x FROM d CROSS JOIN b LEFT JOIN e ON 0, c WHERE c.y = b.x",
            "SELECT d.x FROM d CROSS JOIN b LEFT JOIN e ON 1, c WHERE c.y = b.x",
            "SELECT d.x, b.x, e.x, c.x FROM d CROSS JOIN b LEFT JOIN e ON e.y = b.x AND 0, c \
             WHERE c.y = b.x LIMIT 7",
        ],
    );
}

/// Review fix: `x AND 0` is the literal 0 as the pin's parser writes
/// it (`sqlite3ExprAnd`: an operand that is the integer token 0, no
/// function call in either), so its other operand is never resolved —
/// `nosuch AND 0` reads, `GROUP BY y AND 0` is a constant term out of
/// range — and the column term is no index constraint.
#[test]
fn and_false_folds_as_the_pin_parses() {
    same(
        "af",
        "CREATE TABLE a(x, y); CREATE TABLE b(x, y); CREATE INDEX bx ON b(x); \
         INSERT INTO a VALUES (1, 2), (2, 3); INSERT INTO b VALUES (1, 1), (2, 2);",
        &[
            "SELECT * FROM b WHERE x = 2 AND 0",
            "SELECT * FROM a JOIN b ON a.x = b.x AND 0",
            "SELECT * FROM a LEFT JOIN b ON a.x = b.x AND 0",
            "SELECT * FROM b WHERE 0 AND x = 2",
            "SELECT * FROM b WHERE x = 2 AND false",
            "SELECT * FROM b WHERE x = 2 AND 0x0",
            "SELECT * FROM b WHERE x = 2 AND (0)",
            "SELECT * FROM b WHERE x = 2 AND 00",
            "SELECT * FROM b WHERE x = 2 AND (y = 1 AND 0)",
            "SELECT * FROM b WHERE x = 2 AND (y = 1 OR 0)",
            "SELECT * FROM b WHERE x = 2 AND +0",
            "SELECT * FROM b WHERE x = 2 AND 0.0",
            "SELECT * FROM b WHERE x = 2 AND 0 AND y = 1",
            "SELECT * FROM b WHERE (x = 2 AND 0) OR y = 1",
            "SELECT 1, nosuch AND 0",
            "SELECT 2, (SELECT z FROM nosuch) AND 0",
            "SELECT 3, (1, 2) AND 0",
            "SELECT 4, 0 AND nosuch",
            "SELECT 5, x AND 0 FROM a",
            "SELECT 7, nosuch AND 00",
            "SELECT 8, nosuch AND 0x0",
            "SELECT 9, nosuch AND false",
            "SELECT 10, nosuch AND 0.0",
            "SELECT 11, upper(nosuch) AND 0",
            "SELECT 12, nosuch() AND 0",
            "SELECT 13, nosuch LIKE 'a' AND 0",
            "SELECT 14, (nosuch AND 0) AND 1",
            "SELECT 15, nosuch AND (0)",
            "SELECT 16, nosuch AND +0",
            "SELECT 17, nosuch COLLATE NOCASE AND 0",
            "SELECT 18, x FROM a WHERE nosuch AND 0",
            "SELECT 19, x FROM a ORDER BY nosuch AND 0",
            "SELECT 20, sum(x) AND 0 FROM a",
            "SELECT 21, count(*) FROM a WHERE x = 1 AND 0",
            "SELECT 22, sum(x) FROM a GROUP BY y AND 0",
            "SELECT 23, nosuch BETWEEN 1 AND 0",
            "SELECT 24, x FROM a GROUP BY x HAVING sum(y) AND 0",
            "SELECT 25, 1 WHERE 0 AND nosuch OR 1",
        ],
    );
    // Stored texts: column names keep the text, a view and a CHECK
    // read the fold, ALTER TABLE ... RENAME COLUMN still sees `x`.
    let path = fresh(
        "afs",
        "CREATE TABLE a(x, y); INSERT INTO a VALUES (1, 2), (2, 3);",
    );
    let path2 = fresh(
        "afsh",
        "CREATE TABLE a(x, y); INSERT INTO a VALUES (1, 2), (2, 3);",
    );
    let q = "CREATE TABLE t2 AS SELECT x AND 0, (x) AND (0), y AND 0 AND 1 FROM a; \
             CREATE VIEW v AS SELECT x FROM a WHERE x AND 0 OR y = 3; \
             ALTER TABLE a RENAME COLUMN x TO xx; \
             CREATE VIEW w AS SELECT xx FROM a WHERE nosuch AND 0; \
             SELECT name, sql FROM sqlite_schema ORDER BY name; SELECT * FROM v; SELECT * FROM w";
    let p = pin(&path, q);
    let h = block_on(async {
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut all = Vec::new();
        for s in q.split("; ") {
            all.extend(collect(&mut db, s).await.expect("statement"));
        }
        String::from_utf8(all).expect("utf8")
    });
    assert_eq!(p.as_deref(), Ok(h.as_str()));
}

/// Generated joins of inner, CROSS and LEFT joins and comma lists
/// with ON / WHERE terms that are `OR 0`, `OR 1`, `AND 0` or `AND 1`
/// forms: rows (LIMIT 7, in order) and plans as the pin's.
#[test]
fn generated_join_terms_match_pin() {
    let path = fresh("jog", JO);
    let tabs = ["b", "c", "d", "e"];
    let mut r = Rng(0x0bad_cafe_f00d_1234);
    let mut qs = Vec::new();
    for _ in 0..240 {
        let k = 2 + r.n(3) as usize;
        let mut ts: Vec<&str> = Vec::new();
        while ts.len() < k {
            let t = r.pick(&tabs);
            if !ts.contains(&t) {
                ts.push(t);
            }
        }
        let term = |r: &mut Rng, a: &str, b: &str| -> String {
            let s = match r.n(5) {
                0 => format!("{a}.x = {b}.x"),
                1 => format!("{a}.x = {b}.y"),
                2 => format!("{a}.x = {}", 1 + r.n(5)),
                3 => format!("{a}.y = {b}.x"),
                _ => format!("{a}.x > {}", r.n(4)),
            };
            match r.n(20) {
                0..=2 => format!("({s} OR 0)"),
                3..=4 => format!("({s} OR 1)"),
                5..=6 => format!("{s} AND 0"),
                7 => format!("({s} AND 1)"),
                _ => s,
            }
        };
        let mut from = ts[0].to_string();
        let mut wh: Vec<String> = Vec::new();
        for i in 1..k {
            let prev = ts[r.n(i as u64) as usize];
            match r.n(5) {
                0 => {
                    from.push_str(&format!(", {}", ts[i]));
                    if r.n(10) < 6 {
                        wh.push(term(&mut r, ts[i], prev));
                    }
                }
                1 => {
                    from.push_str(&format!(" CROSS JOIN {}", ts[i]));
                    if r.n(10) < 3 {
                        from.push_str(&format!(" ON {}", term(&mut r, ts[i], prev)));
                    }
                }
                2 => from.push_str(&format!(
                    " LEFT JOIN {} ON {}",
                    ts[i],
                    term(&mut r, ts[i], prev)
                )),
                _ => from.push_str(&format!(" JOIN {} ON {}", ts[i], term(&mut r, ts[i], prev))),
            }
        }
        if r.n(2) == 0 {
            let a = r.pick(&ts);
            let b = r.pick(&ts);
            wh.push(term(&mut r, a, b));
        }
        let cols: Vec<String> = ts.iter().map(|t| format!("{t}.x")).collect();
        let w = if wh.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wh.join(" AND "))
        };
        qs.push(format!("SELECT {} FROM {from}{w} LIMIT 7", cols.join(", ")));
    }
    let bad = diffs_on(&path, &qs, true);
    assert!(
        bad.is_empty(),
        "{} of 240 differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
