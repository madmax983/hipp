//! v0.17 parity unit `result_gaps`: the result-level wrong answers the
//! wave-18 units (`order_offset`, `values_chain`) recorded against the
//! pinned oracle (SQLite 3.54.1), each probed live on the pin first:
//!
//! - The pin's top-N sorter (`pushOntoSorter` with `pDeferredRowLoad`)
//!   computes a row's ORDER BY key before its result columns and
//!   skips a row whose key sorts at or past the largest of the rows it
//!   holds (OFFSET plus LIMIT), so that row's columns are never computed:
//!   at the top level, in a FROM / IN subquery, a view or CTE read
//!   once, over groups and windows, and in each core of a UNION ALL
//!   compound's merge; not in a scalar subquery, an INSERT's SELECT, a
//!   materialized CTE, under DISTINCT or a partly delivered ORDER BY.
//! - A WITHOUT ROWID table read in its own order delivers `ORDER BY k,
//!   abs(-9223372036854775808)` when its other columns are NOT NULL.
//! - A lone FROM subquery's order delivers its reader's GROUP BY
//!   (direction ignored): the groups come in its order, a group ending
//!   where the next row's key differs.
//! - A subquery's ORDER BY written as an ordinal or alias under
//!   COLLATE is that result column for its reader.
//! - An EXISTS computes no result column, ORDER BY or DISTINCT of its
//!   UNION / UNION ALL cores (found on the way).
//! - A TEMP CREATE TABLE ... AS keeps a FLEXNUM column's REAL; a CTE
//!   reference counts one toward the pin's "nested too deep" limit,
//!   whatever its body's size; a VALUES list of a few thousand
//!   non-constant rows fits the default statement arena.
//!
//! Each query's rows (in order, or the error) and, where shapes allow,
//! its EXPLAIN QUERY PLAN must be the pin's (an error on both counts
//! as agreement; the generated corpora must mostly answer on the pin).
//! With the hidden `vm` feature they run on today's executor
//! (`Config::vm_off`).
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
    let path = common::temp_db(&format!("v17rg-{tag}"));
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

/// The overflowing term a row's result column computes when its `a`
/// is 1 (`$X`), or its `b` is 5 (`$Y`).
const BAD: &str = "abs(-9223372036854775808)";

fn fill(q: &str) -> String {
    q.replace("$X", "CASE WHEN a=1 THEN BAD ELSE a END")
        .replace("$Y", "CASE WHEN b=5 THEN BAD ELSE b END")
        .replace("BAD", BAD)
}

/// Most of a corpus must answer on the pin (an error on both sides
/// agrees, so a corpus of errors would test nothing).
fn answers_mostly(path: &Path, qs: &[String], part: usize) {
    let ok = qs.iter().filter(|q| pin(path, q).is_ok()).count();
    assert!(
        ok * part >= qs.len(),
        "only {ok} of {} answer on the pin",
        qs.len()
    );
}

/// `same` over `$X` / `$Y` lines, most of which must answer.
fn filled(tag: &str, schema: &str, qs: &[&str], plans: bool) {
    let qs: Vec<String> = qs.iter().map(|q| fill(q)).collect();
    let path = fresh(tag, schema);
    answers_mostly(&path, &qs, 2);
    let bad = diffs_on(&path, &qs, plans);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The overflowing row (`a` 1) comes third, so a sorter not yet full
/// computes its columns only under a LIMIT + OFFSET above two.
const T: &str = "CREATE TABLE t(a INTEGER, b, c TEXT); \
    INSERT INTO t VALUES (2,1,'x'),(3,2,'Y'),(1,5,'z'),(4,0,'w'),(5,3,NULL),(6,NULL,'v'); \
    CREATE TABLE ti(a INTEGER, b, c TEXT); CREATE INDEX tib ON ti(b); \
    INSERT INTO ti SELECT * FROM t; \
    CREATE TABLE u(x); \
    CREATE VIEW v AS SELECT CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END AS y, b FROM t ORDER BY b LIMIT 1;";

// ------------------------------------------------------ top-N sorter

/// The pin's top-N sorter computes a row's key first and skips a row
/// whose key is not below the largest it holds: its columns are never
/// computed (the recorded gap: `ORDER BY b LIMIT 1` over a row that
/// overflows answered on the pin and failed in hipp).
#[test]
fn top_n_sorter_skips_rows_past_the_limit() {
    filled(
        "topn",
        T,
        &[
            // The recorded gap and its neighbours.
            "SELECT $X FROM t ORDER BY b LIMIT 1",
            "SELECT $X FROM t ORDER BY b LIMIT 2",
            "SELECT $X FROM t ORDER BY b LIMIT 1 OFFSET 1",
            "SELECT $X FROM t ORDER BY b LIMIT 3",
            "SELECT $X FROM t ORDER BY b DESC LIMIT 1",
            "SELECT $X, b FROM t ORDER BY b LIMIT 2",
            "SELECT $X FROM t ORDER BY b NULLS LAST LIMIT 1",
            "SELECT $X FROM t ORDER BY -b DESC LIMIT 1",
            "SELECT $X FROM t ORDER BY b, a LIMIT 1",
            "SELECT $X FROM t ORDER BY b + 0 LIMIT 2",
            "SELECT $X FROM t ORDER BY c LIMIT 1",
            "SELECT $X FROM t ORDER BY c COLLATE NOCASE LIMIT 2",
            "SELECT $X FROM t ORDER BY (SELECT t.b) LIMIT 1",
            "SELECT $X FROM t ORDER BY rowid DESC LIMIT 1",
            "SELECT $X FROM t WHERE a > 1 OR a = 1 ORDER BY b LIMIT 1",
            // Ties: an equal key never enters.
            "SELECT $X FROM t ORDER BY b >= 1 LIMIT 1",
            "SELECT $X FROM t ORDER BY b >= 2 LIMIT 1",
            "SELECT $X FROM t ORDER BY 1 = 1 LIMIT 3",
            // The key itself is computed: an overflowing key fails.
            "SELECT $X AS x FROM t ORDER BY x LIMIT 1",
            "SELECT a FROM t ORDER BY $X LIMIT 1",
            "SELECT $X FROM t ORDER BY 1 LIMIT 1",
            // No top-N: DISTINCT, no LIMIT, LIMIT -1, a full sorter.
            "SELECT DISTINCT $X FROM t ORDER BY b LIMIT 1",
            "SELECT $X FROM t ORDER BY b",
            "SELECT $X FROM t ORDER BY b LIMIT -1",
            "SELECT $X FROM t ORDER BY b LIMIT 0",
            "SELECT $X FROM t ORDER BY b LIMIT 1 OFFSET 2",
            // Destinations: FROM / IN subqueries, a view, a CTE read once
            // (co-routines) defer; a scalar subquery, a materialized CTE
            // and an INSERT do not.
            "SELECT * FROM (SELECT $X FROM t ORDER BY b LIMIT 1)",
            "SELECT count(*) FROM (SELECT $X FROM t ORDER BY b LIMIT 1)",
            "SELECT 1 IN (SELECT $X FROM t ORDER BY b LIMIT 1)",
            "SELECT * FROM v",
            "WITH c AS (SELECT $X AS y FROM t ORDER BY b LIMIT 1) SELECT * FROM c",
            "WITH c AS (SELECT $X AS y FROM t ORDER BY b LIMIT 1) SELECT * FROM c, c AS d",
            "WITH c AS MATERIALIZED (SELECT $X AS y FROM t ORDER BY b LIMIT 1) SELECT * FROM c",
            "SELECT (SELECT $X FROM t ORDER BY b LIMIT 1)",
            "SELECT z.a, s.* FROM t AS z, (SELECT $X FROM t ORDER BY b LIMIT 1) AS s WHERE z.a = 4",
            "SELECT * FROM (SELECT $X AS x, b FROM t ORDER BY b LIMIT 2) ORDER BY x",
            // The sorter holds its own LIMIT's rows, whatever its reader
            // takes (found by the corpus: the reader's bound was used).
            "SELECT * FROM (SELECT $X FROM t ORDER BY b LIMIT 3) LIMIT 1",
            "SELECT * FROM (SELECT $X FROM t ORDER BY b LIMIT 2) LIMIT 1",
            "SELECT 4 IN (SELECT * FROM (SELECT $X FROM t ORDER BY b LIMIT 3) LIMIT 1)",
            "SELECT * FROM (SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 3) LIMIT 1",
            // Joins.
            "SELECT t.b, $X FROM t, (SELECT 1) ORDER BY t.b LIMIT 1",
            "SELECT x.b, CASE WHEN x.a=1 THEN BAD ELSE x.a END FROM t x, t y WHERE y.a = 2 ORDER BY x.b LIMIT 1",
            // Groups.
            "SELECT b, $X FROM t GROUP BY b ORDER BY b + 0 LIMIT 1",
            "SELECT b, $X FROM t GROUP BY b ORDER BY sum(b) LIMIT 1",
            "SELECT a, $X, count(*) FROM t GROUP BY a ORDER BY a DESC LIMIT 1",
            "SELECT a, $X FROM t GROUP BY a ORDER BY max(b) LIMIT 1",
            "SELECT b, $X FROM t GROUP BY b HAVING b IS NOT NULL ORDER BY -b DESC LIMIT 2",
            // Windows: the rows reach the sorter in the window's order.
            "SELECT sum(a) OVER (), $X FROM t ORDER BY b LIMIT 2",
            "SELECT row_number() OVER (ORDER BY b), $X FROM t ORDER BY b LIMIT 1",
            "SELECT row_number() OVER (ORDER BY a), $X FROM t ORDER BY b LIMIT 1",
            "SELECT row_number() OVER (), $X FROM t ORDER BY b LIMIT 1",
            // An index that delivers part of the ORDER BY loads every
            // row first (`nOBSat`); the whole of it, no sorter.
            "SELECT $X FROM ti ORDER BY b, c LIMIT 1",
            "SELECT $X FROM ti ORDER BY b LIMIT 1",
            "SELECT $X FROM ti ORDER BY c LIMIT 1",
            "SELECT $X FROM ti WHERE b > 0 ORDER BY c LIMIT 1",
            // Compound merges: each core sorted under the LIMIT when the
            // operators to its right are UNION ALL.
            "SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1",
            "SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 2",
            "SELECT b, $X FROM t UNION ALL SELECT -1, 9 ORDER BY 1 LIMIT 1",
            "SELECT 9, 9 UNION ALL SELECT b, $X FROM t ORDER BY 1 LIMIT 1",
            "SELECT b, $X FROM t UNION SELECT 9, 9 ORDER BY 1 LIMIT 1",
            "SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1 OFFSET 1",
            "SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1",
            "SELECT b, $X FROM t UNION ALL SELECT b, a FROM t ORDER BY 1 LIMIT 3",
            "SELECT b, $X FROM t UNION SELECT 7, 7 UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1",
            "SELECT 7, 7 UNION SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1",
            "SELECT 7, 7 UNION ALL SELECT b, $X FROM t UNION SELECT 9, 9 ORDER BY 1 LIMIT 1",
            "SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 DESC LIMIT 1",
            "SELECT * FROM (SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1)",
            "SELECT (SELECT b FROM t UNION ALL SELECT $X ORDER BY 1 LIMIT 1)",
            "SELECT c, $X FROM t UNION ALL SELECT 'a', 9 ORDER BY 1 COLLATE NOCASE LIMIT 2",
        ],
        // (Rows: the plans of a join over a co-routine and of an IN
        // list's Bloom filter are recorded EXPLAIN QUERY PLAN gaps.)
        false,
    );
}

/// An INSERT's SELECT fills preallocated registers (no top-N: the
/// overflow fails it); CREATE TABLE ... AS reads a co-routine (top-N).
#[test]
fn top_n_sorter_writes() {
    let t = T.replace("CREATE VIEW", "CREATE VIEW IF NOT EXISTS");
    script_same(
        "topnw",
        &t,
        &[
            &fill("INSERT INTO u SELECT $X FROM t ORDER BY b LIMIT 1;; SELECT * FROM u"),
            &fill(
                "INSERT INTO u SELECT * FROM (SELECT $X FROM t ORDER BY b LIMIT 1);; SELECT * FROM u",
            ),
            &fill("CREATE TABLE w AS SELECT $X AS x FROM t ORDER BY b LIMIT 1;; SELECT * FROM w"),
            &fill("CREATE TABLE w AS SELECT $X AS x FROM t ORDER BY b LIMIT 3;; SELECT * FROM w"),
            &fill(
                "CREATE TABLE w AS SELECT b, $X FROM t UNION ALL SELECT 9, 9 ORDER BY 1 LIMIT 1;; SELECT * FROM w",
            ),
            &fill("UPDATE u SET x = (SELECT $X FROM t ORDER BY b LIMIT 1);; SELECT * FROM u"),
            &fill(
                "INSERT INTO u VALUES (1);; DELETE FROM u WHERE x IN (SELECT $X FROM t ORDER BY b LIMIT 1);; SELECT count(*) FROM u",
            ),
        ],
    );
}

/// A generated corpus of top-N shapes (xorshift): the overflowing row
/// at a random place, keys of every direction and NULLS placement,
/// LIMIT / OFFSET pairs, and each destination.
#[test]
fn generated_top_n_matches_the_pin() {
    let mut rng = Rng(0x5eed_0717_u64);
    let keys = [
        "b",
        "b",
        "-b DESC",
        "c",
        "c DESC",
        "b NULLS LAST",
        "b DESC NULLS FIRST",
        "b, c",
        "c, b DESC",
        "b + 0",
        "a % 3, b",
        "coalesce(b, 9)",
        "length(c)",
        "b * 0",
    ];
    let mut qs = Vec::new();
    let mut schemas = Vec::new();
    for k in 0..6 {
        // Six row orders: the overflowing row moves.
        // (The overflowing row `(1,5,'z')` comes fourth or later, so
        // most short LIMITs answer.)
        let mut rows = vec![
            "(2,1,'x')",
            "(3,2,'Y')",
            "(4,0,'w')",
            "(5,3,NULL)",
            "(6,NULL,'v')",
            "(7,4,'b')",
        ];
        for i in (1..rows.len()).rev() {
            let j = rng.n(i as u64 + 1) as usize;
            rows.swap(i, j);
        }
        rows.insert(3 + rng.n(4) as usize, "(1,5,'z')");
        schemas.push(format!(
            "CREATE TABLE t(a INTEGER, b, c TEXT); INSERT INTO t VALUES {};",
            rows.join(",")
        ));
        let _ = k;
    }
    let mut bad = Vec::new();
    for (si, schema) in schemas.iter().enumerate() {
        qs.clear();
        for _ in 0..40 {
            let key = rng.pick(&keys);
            let lim = 1 + rng.n(2);
            let off = rng.n(2);
            let lo = if off > 0 {
                format!("LIMIT {lim} OFFSET {off}")
            } else {
                format!("LIMIT {lim}")
            };
            let q = match rng.n(9) {
                // (Scalar subqueries compute every row: kept rare.)
                4 if rng.n(3) > 0 => format!("SELECT $X, b FROM t ORDER BY {key} {lo}"),
                0 | 1 => format!("SELECT $X, b FROM t ORDER BY {key} {lo}"),
                2 => format!("SELECT * FROM (SELECT $X AS x, b FROM t ORDER BY {key} {lo})"),
                3 => format!("SELECT 4 IN (SELECT $X FROM t ORDER BY {key} {lo})"),
                4 => format!("SELECT (SELECT $X FROM t ORDER BY {key} {lo})"),
                5 => format!("SELECT a % 3 AS g, $X FROM t GROUP BY g ORDER BY min(b) {lo}"),
                6 => format!("SELECT sum(a) OVER (), $X FROM t ORDER BY {key} {lo}"),
                7 => format!(
                    "SELECT b, $X FROM t UNION ALL SELECT {}, 9 ORDER BY 1 {lo}",
                    rng.n(6)
                ),
                _ => format!(
                    "SELECT $X FROM t WHERE a <> {} ORDER BY {key} {lo}",
                    rng.n(8)
                ),
            };
            qs.push(fill(&q));
        }
        let path = fresh(&format!("gen{si}"), schema);
        answers_mostly(&path, &qs, 4);
        bad.extend(diffs_on(&path, &qs, false));
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

// ------------------------------------------------- WITHOUT ROWID order

/// A WITHOUT ROWID table read in its own order delivers the ORDER BY
/// whose later terms its NOT NULL columns make distinct: the last
/// term is never computed (the recorded gap; `ORDER BY k, v, X` over a
/// nullable `z` still sorts its last term).
#[test]
fn without_rowid_natural_order_delivers() {
    same(
        "wr",
        "CREATE TABLE w(k INTEGER NOT NULL, v NOT NULL, z, PRIMARY KEY(k)) WITHOUT ROWID; \
         INSERT INTO w VALUES (1,'a',NULL),(2,'b',3),(3,'c',4); \
         CREATE TABLE w2(k INTEGER NOT NULL, v, PRIMARY KEY(k)) WITHOUT ROWID; \
         INSERT INTO w2 VALUES (1,'a'),(2,'b'); \
         CREATE TABLE w3(a NOT NULL, b NOT NULL, c NOT NULL, PRIMARY KEY(a, b)) WITHOUT ROWID; \
         INSERT INTO w3 VALUES (1,1,1),(1,2,2),(2,1,3);",
        &[
            "SELECT k FROM w ORDER BY k, abs(-9223372036854775808)",
            "SELECT k FROM w ORDER BY k, v, abs(-9223372036854775808)",
            "SELECT k FROM w ORDER BY k DESC, abs(-9223372036854775808)",
            "SELECT k FROM w2 ORDER BY k, abs(-9223372036854775808)",
            "SELECT k FROM w ORDER BY k, abs(-9223372036854775808) LIMIT 1 OFFSET 1",
            "SELECT k, z FROM w ORDER BY k, v, z",
            "SELECT k FROM w ORDER BY k, z, abs(-9223372036854775808)",
            "SELECT a, b FROM w3 ORDER BY a, b, abs(-9223372036854775808)",
            "SELECT a, b FROM w3 ORDER BY a, b, c, abs(-9223372036854775808)",
            "SELECT a, b FROM w3 ORDER BY a, abs(-9223372036854775808)",
            "SELECT a, b FROM w3 ORDER BY a, b LIMIT 2",
            "SELECT * FROM (SELECT k FROM w ORDER BY k, abs(-9223372036854775808) LIMIT 2)",
            "SELECT CASE WHEN k = 3 THEN abs(-9223372036854775808) ELSE k END FROM w ORDER BY k LIMIT 2",
            "SELECT CASE WHEN k = 1 THEN abs(-9223372036854775808) ELSE k END FROM w ORDER BY k, v, z LIMIT 1",
        ],
        true,
    );
}

// --------------------------------------------- co-routine GROUP BY

const G: &str = "CREATE TABLE t(c COLLATE NOCASE, n); \
    INSERT INTO t VALUES ('A',1),('b',2),('C',3),(NULL,4),('a',5),('B',6),(1,7),('b',1);";

/// A lone FROM subquery's order delivers its reader's GROUP BY, in
/// either direction (the recorded gap: hipp sorted the groups); its
/// groups end where the next row's key differs, so a subquery ordered
/// under another collation splits equal keys.
#[test]
fn coroutine_order_delivers_group_by() {
    same(
        "grp",
        G,
        &[
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1) GROUP BY x",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC LIMIT 99) GROUP BY x",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x ORDER BY x",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x ORDER BY x DESC",
            "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY x COLLATE BINARY DESC) GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY x COLLATE BINARY) GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1) GROUP BY x COLLATE BINARY",
            "SELECT x, y, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC, 2) GROUP BY x, y",
            "SELECT y, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC, 2) GROUP BY y",
            "SELECT x, y, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC, 2 DESC) GROUP BY y, x",
            "SELECT x, y, count(*) FROM (SELECT c AS x, n % 2 AS y FROM t ORDER BY 1 DESC, 2 DESC) GROUP BY x, y",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC, 2 DESC) GROUP BY x ORDER BY count(*)",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC LIMIT 5) GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) GROUP BY x LIMIT 2",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) GROUP BY x LIMIT 2 OFFSET 1",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) GROUP BY x HAVING count(*) > 1",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) WHERE y > 1 GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC NULLS FIRST) GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY x COLLATE BINARY DESC) GROUP BY x ORDER BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t UNION SELECT 'z', 9 ORDER BY 1 DESC) GROUP BY x",
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) GROUP BY +x",
            "SELECT DISTINCT x FROM (SELECT c AS x, n AS y FROM t ORDER BY x COLLATE BINARY DESC)",
            "SELECT (SELECT group_concat(x, '') FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY 1 DESC LIMIT 9) GROUP BY x))",
            "WITH q AS (SELECT c AS x, n FROM t ORDER BY 1 DESC LIMIT 9) SELECT x, sum(n) FROM q GROUP BY x",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x ORDER BY x LIMIT 1",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x ORDER BY x DESC LIMIT 2",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1) GROUP BY x ORDER BY x LIMIT 2 OFFSET 1",
            "SELECT x, group_concat(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 DESC) GROUP BY x ORDER BY x NULLS LAST LIMIT 2",
        ],
        true,
    );
    // (Rows: a join's order over a co-routine is a recorded EXPLAIN
    // QUERY PLAN gap.)
    same(
        "grp2",
        G,
        &[
            "SELECT x, count(*) FROM (SELECT c AS x, n AS y FROM t ORDER BY 1 DESC) AS s, (SELECT 1) GROUP BY x",
        ],
        false,
    );
}

// ---------------------------------------- ORDER BY 1 COLLATE BINARY

/// A subquery's ORDER BY written as an ordinal or an alias under
/// COLLATE is its result column (`resolveOrderGroupBy`), so its reader
/// keeps its order (the recorded gap: hipp sorted again in the
/// column's own collation); a column name or an expression under
/// COLLATE is none.
#[test]
fn collated_ordinal_is_a_result_column() {
    same(
        "coll",
        G,
        &[
            "SELECT x FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE BINARY LIMIT 10) ORDER BY x",
            "SELECT x FROM (SELECT c AS x, n FROM t ORDER BY x COLLATE NOCASE LIMIT 10) ORDER BY x",
            "SELECT x FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE BINARY) ORDER BY x",
            "SELECT x FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE BINARY LIMIT 10)",
            "SELECT x FROM (SELECT c AS x, n FROM t ORDER BY x COLLATE BINARY LIMIT 10) ORDER BY x",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY 1 LIMIT 10) ORDER BY x COLLATE BINARY)",
            "SELECT group_concat(y) FROM (SELECT x AS y FROM (SELECT c AS x FROM t ORDER BY 1 LIMIT 10) ORDER BY y COLLATE BINARY)",
            "SELECT group_concat(y) FROM (SELECT x AS y FROM (SELECT c AS x FROM t ORDER BY 1 LIMIT 10) ORDER BY 1 COLLATE BINARY)",
            "SELECT group_concat(y) FROM (SELECT x AS y FROM (SELECT c AS x FROM t ORDER BY 1 LIMIT 10) ORDER BY y COLLATE NOCASE)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY 1 COLLATE BINARY LIMIT 10) ORDER BY x COLLATE NOCASE)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY c COLLATE BINARY LIMIT 10) ORDER BY x)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY x COLLATE BINARY LIMIT 10) ORDER BY x)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY 1 COLLATE BINARY DESC LIMIT 10) ORDER BY x DESC)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE BINARY, 2 LIMIT 10) ORDER BY x, n)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY +x COLLATE BINARY LIMIT 10) ORDER BY x)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c COLLATE BINARY AS x FROM t ORDER BY 1 LIMIT 10) ORDER BY x)",
            "SELECT group_concat(x) FROM (SELECT x FROM (SELECT c AS x FROM t ORDER BY 1 COLLATE BINARY COLLATE NOCASE LIMIT 10) ORDER BY x)",
            "SELECT x, count(*) FROM (SELECT c AS x FROM t ORDER BY 1 COLLATE BINARY LIMIT 10) GROUP BY x",
            "WITH q AS (SELECT c AS x FROM t ORDER BY 1 COLLATE BINARY LIMIT 10) SELECT x FROM q ORDER BY x",
        ],
        true,
    );
}

// ----------------------------------------------------------- EXISTS

/// An EXISTS computes no result column, ORDER BY term or DISTINCT of
/// its UNION / UNION ALL cores (`SRT_Exists`, `IgnorableOrderby`):
/// only its aggregates step and finish, and its HAVING, WHERE and FROM
/// subqueries run; under INTERSECT / EXCEPT the values decide.
#[test]
fn exists_computes_no_result_column() {
    same(
        "ex",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES (1,1),(1,2),(2,3); \
         CREATE TABLE big(x); INSERT INTO big VALUES (9223372036854775807),(1); CREATE TABLE e(a);",
        &[
            "SELECT EXISTS (SELECT abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT CASE WHEN a=2 THEN abs(-9223372036854775808) END FROM t)",
            "SELECT NOT EXISTS (SELECT abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT a FROM t ORDER BY abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT a, abs(-9223372036854775808) FROM t ORDER BY 2 LIMIT 1 OFFSET 2)",
            "SELECT EXISTS (SELECT a FROM t ORDER BY abs(-9223372036854775808) LIMIT 1 OFFSET 3)",
            "SELECT EXISTS (SELECT DISTINCT a FROM t LIMIT 1 OFFSET 2)",
            "SELECT EXISTS (SELECT DISTINCT a FROM t LIMIT 1 OFFSET 1)",
            "SELECT EXISTS (SELECT DISTINCT abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT sum(x) FROM big)",
            "SELECT EXISTS (SELECT abs(sum(b) - 9223372036854775807 - 10) FROM t)",
            "SELECT EXISTS (SELECT max(a), abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT count(*) FROM e)",
            "SELECT EXISTS (SELECT a FROM t GROUP BY a HAVING sum(b) > 2)",
            "SELECT EXISTS (SELECT a FROM t GROUP BY a HAVING abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT a FROM t GROUP BY a ORDER BY abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT row_number() OVER (), abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT row_number() OVER (ORDER BY abs(-9223372036854775808)) FROM t)",
            "SELECT EXISTS (SELECT 1 FROM t WHERE 0 UNION ALL SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT 1 FROM t WHERE 0 UNION SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT abs(-9223372036854775808) FROM t WHERE 0 UNION ALL SELECT 1)",
            "SELECT EXISTS (SELECT 1 INTERSECT SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT 1 EXCEPT SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT 1 FROM t WHERE 0 UNION SELECT a + abs(-9223372036854775808) FROM t)",
            "SELECT EXISTS (SELECT a FROM t UNION SELECT abs(-9223372036854775808) ORDER BY 1)",
            "SELECT EXISTS (SELECT max(a) FROM t WHERE 0 UNION ALL SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (SELECT a FROM t INTERSECT SELECT a FROM t UNION ALL SELECT abs(-9223372036854775808))",
            "SELECT EXISTS (VALUES (abs(-9223372036854775808)))",
            "SELECT EXISTS (VALUES (1),(abs(-9223372036854775808)))",
            "SELECT EXISTS (SELECT * FROM (SELECT abs(-9223372036854775808)))",
            "SELECT a FROM t WHERE EXISTS (SELECT abs(-9223372036854775808) FROM t AS u WHERE u.b = t.b + 1)",
            "SELECT a FROM t WHERE NOT EXISTS (SELECT abs(-9223372036854775808) FROM t AS u WHERE u.b = t.b + 1)",
            "SELECT EXISTS (SELECT a FROM t ORDER BY 1)",
            "SELECT EXISTS (SELECT a FROM t WHERE a = abs(-9223372036854775808))",
        ],
        true,
    );
    // (Rows only: hipp's plan still lists a result column's subqueries,
    // which the pin never codes; an EXPLAIN QUERY PLAN gap.)
    same(
        "ex2",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES (1,1),(1,2),(2,3);",
        &[
            "SELECT NOT EXISTS (SELECT max(b), (SELECT abs(-9223372036854775808)) FROM t)",
            "SELECT EXISTS (SELECT 1 IN (SELECT abs(-9223372036854775808)))",
            "SELECT EXISTS (SELECT (SELECT abs(-9223372036854775808) FROM t AS u WHERE u.a = t.a) FROM t)",
            "SELECT a FROM t WHERE EXISTS (SELECT (SELECT abs(-9223372036854775808)) FROM t AS u WHERE u.a = t.a)",
        ],
        false,
    );
}

// ------------------------------------------------------------- CTEs

/// The pin counts each CTE reference one toward its "nested too deep"
/// limit (1,000), whatever the body's size, and a written FROM subquery
/// or view one more when references were expanded inside it (the
/// recorded gap: three reads of a 450-arm CTE were refused in hipp).
#[test]
fn cte_references_count_toward_the_nesting_limit() {
    let arms = |n: usize, f: &dyn Fn(usize) -> String| -> String {
        (0..n).map(f).collect::<Vec<_>>().join(" UNION ALL ")
    };
    let items = |n: usize, s: &str| -> String { vec![s; n].join(",") };
    // (Inside a FROM subquery the columns need names of their own: the
    // pin names a sixth equal one at random.)
    let named = |n: usize, s: &str| -> String {
        (0..n)
            .map(|i| format!("{s} AS a{i}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let chain = |n: usize| -> String {
        let mut c = vec!["c0(x) AS (SELECT 1)".to_string()];
        for i in 1..n {
            c.push(format!("c{i}(x) AS (SELECT x FROM c{})", i - 1));
        }
        c.join(",")
    };
    let mut qs: Vec<String> = Vec::new();
    for n in [300usize, 450, 499] {
        let a = arms(n, &|i| format!("SELECT {i}"));
        qs.push(format!(
            "WITH c(x) AS ({a}) SELECT (SELECT sum(x) FROM c), (SELECT max(x) FROM c), (SELECT min(x) FROM c)"
        ));
        qs.push(format!(
            "WITH c(x) AS ({a}) SELECT count(*) FROM c AS a, c AS b WHERE a.x = b.x AND a.x IN (SELECT x FROM c)"
        ));
        qs.push(format!(
            "WITH c(x) AS MATERIALIZED ({a}) SELECT (SELECT sum(x) FROM c), (SELECT max(x) FROM c), (SELECT min(x) FROM c)"
        ));
    }
    qs.push(format!(
        "WITH c(x) AS ({}) SELECT (SELECT sum(x) FROM c), (SELECT max(x) FROM c), (SELECT min(x) FROM c), \
         (SELECT count(x) FROM c), (SELECT avg(x) FROM c), (SELECT total(x) FROM c)",
        arms(300, &|i| format!("SELECT {i}"))
    ));
    for n in [998usize, 999, 1000] {
        qs.push(format!("WITH {} SELECT x FROM c{}", chain(n), n - 1));
    }
    for n in [9usize, 10] {
        let mut c = vec!["c0(x) AS (SELECT 1)".to_string()];
        for i in 1..n {
            c.push(format!(
                "c{i}(x) AS (SELECT x FROM c{} UNION ALL SELECT x FROM c{})",
                i - 1,
                i - 1
            ));
        }
        qs.push(format!(
            "WITH {} SELECT count(*) FROM c{}",
            c.join(","),
            n - 1
        ));
    }
    for k in [249usize, 250] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1), c1(x) AS (SELECT x FROM c0 UNION ALL SELECT x FROM c0 UNION ALL SELECT x FROM c0) \
             SELECT count(*) FROM ({})",
            vec!["SELECT x FROM c1"; k].join(" UNION ALL ")
        ));
    }
    for n in [999usize, 1000] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT {}",
            items(n, "(SELECT x FROM c0)")
        ));
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT 1 IN ({})",
            items(n, "(SELECT x FROM c0)")
        ));
    }
    for n in [998usize, 999] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT 1 FROM (SELECT {})",
            named(n, "(SELECT x FROM c0)")
        ));
        qs.push(format!(
            "WITH c0(x) AS MATERIALIZED (SELECT 1) SELECT {}",
            items(n, "(SELECT x FROM c0)")
        ));
    }
    for n in [997usize, 998] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT count(*) FROM (SELECT 1 FROM (SELECT {}))",
            named(n, "(SELECT x FROM c0)")
        ));
        qs.push(format!(
            "WITH RECURSIVE c0(x) AS (SELECT 1), r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r WHERE x<2) \
             SELECT (SELECT count(*) FROM r), {}",
            items(n, "(SELECT x FROM c0)")
        ));
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT count(*) FROM c0 AS a JOIN c0 AS b WHERE 1 IN ({})",
            items(n, "(SELECT x FROM c0)")
        ));
    }
    for (n, s) in [
        (
            333usize,
            "(SELECT x FROM (SELECT x FROM c0 UNION ALL SELECT x FROM c0))",
        ),
        (
            334,
            "(SELECT x FROM (SELECT x FROM c0 UNION ALL SELECT x FROM c0))",
        ),
        (499, "(SELECT y FROM (SELECT (SELECT x FROM c0) AS y))"),
        (500, "(SELECT y FROM (SELECT (SELECT x FROM c0) AS y))"),
        (
            333,
            "(SELECT y FROM (SELECT y FROM (SELECT x AS y FROM c0)))",
        ),
        (
            334,
            "(SELECT y FROM (SELECT y FROM (SELECT x AS y FROM c0)))",
        ),
        (999, "(SELECT y FROM (SELECT 1 AS y))"),
        (1500, "(SELECT y FROM (SELECT 1 AS y))"),
    ] {
        qs.push(format!("WITH c0(x) AS (SELECT 1) SELECT {}", items(n, s)));
    }
    let path = fresh(
        "cte",
        &format!(
            "CREATE VIEW v AS SELECT 1 AS y; CREATE VIEW v0(x) AS SELECT 1; \
             CREATE VIEW v1(x) AS SELECT x FROM v0 UNION ALL SELECT x FROM v0 UNION ALL SELECT x FROM v0; \
             CREATE VIEW vc AS WITH c AS (SELECT 1 AS y) SELECT y FROM c; \
             CREATE VIEW vw AS WITH {} SELECT x FROM c599;",
            chain(600)
        ),
    );
    // A view counts before the references after it, not after them.
    for n in [998usize, 999] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT (SELECT y FROM v), {}",
            items(n, "(SELECT x FROM c0)")
        ));
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1) SELECT {}, (SELECT y FROM v), (SELECT y FROM v)",
            items(n, "(SELECT x FROM c0)")
        ));
    }
    for k in [999usize, 1100] {
        qs.push(format!(
            "WITH c AS (SELECT 1 x) SELECT count(*) FROM (SELECT {}, (SELECT x FROM c) z)",
            named(k, "(SELECT y FROM v)")
        ));
    }
    // A CTE body's FROM subquery counts at each reference.
    for n in [333usize, 334] {
        qs.push(format!(
            "WITH c0(x) AS (SELECT 1), c1(x) AS (SELECT x FROM (SELECT x FROM c0)) SELECT {}",
            items(n, "(SELECT x FROM c1)")
        ));
    }
    for k in [249usize, 400] {
        qs.push(format!(
            "SELECT count(*) FROM ({})",
            vec!["SELECT x FROM v1"; k].join(" UNION ALL ")
        ));
    }
    for n in [499usize, 500, 1500] {
        qs.push(format!("SELECT {}", items(n, "(SELECT y FROM vc)")));
    }
    qs.push("SELECT x FROM vw".into());
    qs.push("SELECT a.x FROM vw a, vw b".into());
    qs.push(format!("WITH {} SELECT x FROM c449, vw", chain(450)));
    // The pin's nesting and depth limits, an arena to expand the
    // copies in, and a stack for the expansion's recursion.
    let p2 = path.clone();
    let q2 = qs.clone();
    let got: Vec<Result<Vec<u8>, String>> = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || {
            block_on(async {
                let cfg = Config::default()
                    .max_nesting(1000)
                    .depth_limits(
                        hipp::sql::parser::PIN_EXPR_DEPTH,
                        hipp::sql::parser::PIN_PARSER_DEPTH,
                    )
                    .statement_arena(16 * 1024, 16 * 1024 * 1024);
                #[cfg(feature = "vm")]
                let cfg = cfg.vm_off(true);
                let fs = FileStorage::open(&p2).expect("fs open");
                let mut db = Connection::open(fs, cfg).await.expect("open");
                let mut out = Vec::new();
                for q in q2.iter() {
                    out.push(collect(&mut db, q).await.map_err(|e| format!("{e:?}")));
                }
                out
            })
        })
        .expect("thread")
        .join()
        .expect("join");
    let mut bad = Vec::new();
    for (q, h) in qs.iter().zip(got) {
        let p = pin(&path, q);
        match (&h, &p) {
            (Ok(a), Ok(b)) if a.as_slice() == b.as_bytes() => {}
            (Err(_), Err(_)) => {}
            _ => bad.push(format!(
                "{}...\n  pin : {:?}\n  hipp: {:?}",
                &q[..q.len().min(160)],
                p.map(|x| x.len()),
                h.map(|x| x.len())
            )),
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// A derived table's fifth equal column name is `:4`, as the pin's
/// `sqlite3ColumnsFromExprList` forms it before its random suffixes
/// (found on the way: hipp refused it).
#[test]
fn fifth_equal_column_name() {
    same(
        "names",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT * FROM (SELECT (SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1));",
        &[
            "SELECT * FROM (SELECT (SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1))",
            "SELECT count(*) FROM (SELECT (SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1),(SELECT 1))",
            "SELECT name FROM pragma_table_info('v')",
            "SELECT * FROM (SELECT 1 AS a, 2 AS a, 3 AS a, 4 AS a, 5 AS a)",
            "SELECT * FROM (SELECT 1 AS a, 2 AS a, 3 AS a, 4 AS a, 5 AS a, 6 AS b)",
        ],
        true,
    );
}

// ---------------------------------------------------- TEMP CTAS

/// A TEMP CREATE TABLE ... AS that reads main keeps a FLEXNUM column's
/// REAL, as the main one does (the recorded gap: `2` for `2.0`).
#[test]
fn temp_create_table_as_keeps_flexnum_reals() {
    script_same(
        "flex",
        "CREATE TABLE t(r REAL, i INT, s TEXT); INSERT INTO t VALUES (2.0, 3, '4.0'), (5.5, 6, 'x');",
        &[
            "CREATE TEMP TABLE q AS SELECT CAST(1 AS REAL) AS x UNION ALL SELECT r FROM t;; SELECT x, typeof(x) FROM q",
            "CREATE TEMP TABLE q AS SELECT CAST(r AS NUMERIC) AS x FROM t;; SELECT x, typeof(x) FROM q",
            "CREATE TEMP TABLE q AS SELECT 1 AS x UNION ALL SELECT r FROM t;; SELECT x, typeof(x) FROM q",
            "CREATE TEMP TABLE q AS SELECT CAST(i AS REAL) AS x, s FROM t UNION ALL SELECT r, s FROM t;; SELECT x, typeof(x), s FROM q",
            "CREATE TEMP TABLE q AS SELECT CAST(2.0 AS NUMERIC) AS x, CAST(s AS NUMERIC) AS y FROM t;; SELECT x, typeof(x), y, typeof(y) FROM q",
            "CREATE TEMP TABLE q AS SELECT x FROM (SELECT CAST(1 AS REAL) AS x UNION ALL SELECT s FROM t);; SELECT x, typeof(x) FROM q",
            "CREATE TEMP TABLE q AS SELECT CAST(2 AS REAL) AS x UNION ALL SELECT 3;; SELECT x, typeof(x) FROM q",
            "CREATE TEMP TABLE q AS SELECT CAST(1 AS REAL) AS x UNION ALL SELECT r FROM t;; SELECT sql FROM sqlite_temp_master",
            "CREATE TABLE q AS SELECT CAST(1 AS REAL) AS x UNION ALL SELECT r FROM t;; SELECT x, typeof(x) FROM q",
        ],
    );
}

// --------------------------------------------- the statement arena

/// A VALUES list of a few thousand non-constant rows reads under the
/// default statement arena (the recorded gap: `ArenaExhausted`): the
/// statement's names are copied once, not once per router, and the
/// arena's last chunk takes the rest of its budget.
#[test]
fn long_values_lists_fit_the_default_arena() {
    let mut qs = Vec::new();
    for n in [1000usize, 2000, 3000] {
        let rows: Vec<String> = (0..n).map(|_| "(random())".to_string()).collect();
        qs.push(format!(
            "SELECT {n}, typeof(sum(column1 % 7)) FROM (VALUES {})",
            rows.join(",")
        ));
        let rows: Vec<String> = (0..n).map(|i| format!("(abs({i}))")).collect();
        qs.push(format!(
            "SELECT {n}, max(column1) FROM (VALUES {})",
            rows.join(",")
        ));
        let rows: Vec<String> = (0..n / 2).map(|i| format!("((SELECT {i}))")).collect();
        qs.push(format!(
            "SELECT {n}, sum(column1) FROM (VALUES {})",
            rows.join(",")
        ));
        let rows: Vec<String> = (0..n).map(|_| "(random())".to_string()).collect();
        qs.push(format!(
            "SELECT count(*) > 0 FROM (SELECT 1 UNION ALL VALUES {})",
            rows.join(",")
        ));
    }
    let path = fresh("arena", "CREATE TABLE t(a);");
    let got = hipp_answers_on(&path, &qs, false);
    for (q, (rows, _)) in qs.iter().zip(got) {
        let p = pin(&path, q);
        assert!(p.is_ok(), "pin refused {}", &q[..60]);
        let rows = rows.unwrap_or_else(|e| panic!("hipp refused {}: {e}", &q[..60]));
        // (random() rows: compare the shape, not the sum.)
        let h = String::from_utf8_lossy(&rows).to_string();
        let p = p.unwrap();
        assert_eq!(h.split('|').next(), p.split('|').next(), "{}", &q[..60]);
        if !q.contains("random") {
            assert_eq!(h, p, "{}", &q[..60]);
        }
    }
}

// ------------------------------------------------- review fixes

/// The pin's UNION-with-LIMIT-1 shortcut (review fix): a compound
/// with no ORDER BY or OFFSET and a LIMIT of exactly 1 reads the
/// UNIONs of its trailing run of UNION / UNION ALL operators as UNION
/// ALL (also an EXISTS, whose ORDER BY is dropped, or a scalar
/// subquery without a LIMIT); with another LIMIT or an OFFSET an
/// EXISTS over a UNION merges and deduplicates, computing every core
/// (regression: `mark_exists_inert` made every UNION a UNION ALL).
#[test]
fn union_limit_one_shortcut_and_exists_merges() {
    let schema = "CREATE TABLE t(a,b); INSERT INTO t VALUES(2,1),(3,2),(1,5),(4,0),(2,7);";
    let bad = BAD;
    let mut qs: Vec<String> = [
        // The reviewer's repro and its neighbours.
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t LIMIT 2 OFFSET 4), EXISTS (SELECT 1 FROM t UNION SELECT 1 FROM t LIMIT 1 OFFSET 1)",
        "SELECT EXISTS (SELECT a FROM t UNION ALL SELECT a FROM t UNION SELECT a FROM t LIMIT 1 OFFSET 4)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t UNION ALL SELECT 5 LIMIT 1 OFFSET 4)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t UNION ALL SELECT 5 LIMIT 1 OFFSET 5)",
        "SELECT EXISTS (SELECT DISTINCT a FROM t LIMIT 1 OFFSET 4)",
        "SELECT EXISTS (SELECT a FROM t UNION ALL SELECT a FROM t ORDER BY 1 LIMIT 1 OFFSET 9)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t ORDER BY 1 LIMIT 1 OFFSET 4)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t LIMIT 0)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t LIMIT -1 OFFSET 4)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t LIMIT -1)",
        "SELECT EXISTS (SELECT a FROM t EXCEPT SELECT a FROM t UNION SELECT 1 LIMIT 1 OFFSET 0)",
        "SELECT EXISTS (SELECT a, b FROM t UNION SELECT a, b FROM t LIMIT 1 OFFSET 4)",
        // Top level, FROM subqueries, CTEs, views and scalar subqueries.
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 1",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT +1",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT -(-1)",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 0x1",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT (1)",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 0+1",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT '1'",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 1.0",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 2",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 1 OFFSET 0",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 1 OFFSET 1",
        "SELECT a FROM t UNION SELECT a FROM t LIMIT 0, 1",
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY 1 DESC LIMIT 1",
        "SELECT a FROM t UNION ALL SELECT 3 UNION SELECT a FROM t LIMIT 1",
        "SELECT a FROM t UNION SELECT 3 UNION ALL SELECT a FROM t LIMIT 1",
        "SELECT a FROM t INTERSECT SELECT a FROM t UNION SELECT 0 LIMIT 1",
        "SELECT a FROM t EXCEPT SELECT 1 UNION SELECT 0 LIMIT 1",
        "SELECT a FROM t UNION SELECT a FROM t EXCEPT SELECT 9 UNION SELECT 0 LIMIT 1",
        "SELECT b FROM t UNION SELECT a FROM t INTERSECT SELECT a FROM t LIMIT 1",
        "SELECT a, count(*) FROM t GROUP BY a UNION SELECT b, 0 FROM t LIMIT 1",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT 3 UNION SELECT a FROM t LIMIT 1)",
        "WITH c(x) AS (SELECT b FROM t UNION SELECT a FROM t LIMIT 1) SELECT * FROM c",
        "SELECT (SELECT a FROM t UNION ALL SELECT 3 UNION SELECT a FROM t)",
        "SELECT (SELECT a FROM t UNION ALL SELECT 3 UNION SELECT a FROM t LIMIT 1)",
        "SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT 1)",
        "SELECT (SELECT b FROM t UNION SELECT a FROM t LIMIT 0x1)",
        "SELECT (SELECT b FROM t UNION SELECT a FROM t LIMIT (1))",
        "SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT 2)",
        "SELECT (SELECT a FROM t UNION SELECT a FROM t LIMIT 1 OFFSET 0)",
        "SELECT (SELECT b FROM t UNION SELECT a FROM t ORDER BY 1 DESC)",
        "SELECT 1 UNION ALL SELECT 2 UNION SELECT 1 LIMIT 1",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    // Errors: a merge computes every core; the shortcut stops at its
    // first row; a collation the merge would need is never looked up.
    for q in [
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B LIMIT 2)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B LIMIT -1)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B LIMIT 1 OFFSET 0)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B LIMIT 1)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B ORDER BY 1)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B ORDER BY 1 LIMIT 1)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B ORDER BY 1 LIMIT 2)",
        "SELECT EXISTS (SELECT a FROM t UNION ALL SELECT a FROM t UNION SELECT $B LIMIT 1)",
        "SELECT EXISTS (SELECT a FROM t UNION ALL SELECT 1 UNION SELECT $B LIMIT 2)",
        "SELECT EXISTS (SELECT $B FROM t UNION SELECT 1 UNION ALL SELECT 2 LIMIT 1)",
        "SELECT EXISTS (SELECT $B FROM t UNION SELECT 1 UNION ALL SELECT 2 LIMIT 2)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT 1 UNION ALL SELECT $B LIMIT 1)",
        "SELECT EXISTS (SELECT a FROM t EXCEPT SELECT a FROM t UNION SELECT $B)",
        "SELECT EXISTS (SELECT a FROM t EXCEPT SELECT a FROM t UNION SELECT $B LIMIT 2)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT a FROM t EXCEPT SELECT $B)",
        "SELECT EXISTS (SELECT a FROM t UNION SELECT $B LIMIT 0)",
        "SELECT 1 UNION SELECT $B LIMIT 1",
        "SELECT 1 UNION SELECT $B LIMIT 2",
        "SELECT (SELECT 1 UNION SELECT $B)",
        "SELECT (SELECT 1 UNION SELECT $B LIMIT 1)",
        "SELECT a COLLATE bogus FROM t UNION SELECT 1 LIMIT 1",
        "SELECT a COLLATE bogus FROM t UNION SELECT 1 LIMIT 2",
    ] {
        qs.push(q.replace("$B", bad));
    }
    let path = fresh("ul1", schema);
    let mut bad = diffs_on(&path, &qs, true);
    // (Rows only: these plans differ for reasons of their own — the
    // pin sorts a VALUES merge arm in a temp b-tree, and builds Bloom
    // filters for an IN list's cores.)
    let rows_only: Vec<String> = [
        "SELECT EXISTS (VALUES(1),(1) UNION SELECT 1 LIMIT 1 OFFSET 1)",
        "SELECT 5 IN (SELECT b FROM t UNION SELECT a FROM t LIMIT 1)",
        "SELECT 5 IN (SELECT b FROM t UNION SELECT a FROM t LIMIT 2)",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    bad.extend(diffs_on(&path, &rows_only, false));
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// GROUP BY and an equal ORDER BY over a FROM subquery ordered by an
/// ordinal under another collation (review fix): the subquery's order
/// delivers both, so the groups come in its order with no sorter, as
/// on the pin (hipp sorted them again under the column's collation).
#[test]
fn coroutine_collated_order_delivers_group_and_order() {
    let schema = "CREATE TABLE t(c, n); INSERT INTO t VALUES('a',1),('A',2),('b',3),('B',4),(NULL,5),('a',6); \
                  CREATE TABLE u(c TEXT COLLATE NOCASE, n); INSERT INTO u SELECT * FROM t;";
    let mut qs: Vec<String> = [
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x",
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM u ORDER BY 1 COLLATE BINARY LIMIT 9) GROUP BY x ORDER BY x",
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY x COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x",
        "SELECT x, sum(n) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE, 2 LIMIT 9) GROUP BY x ORDER BY x",
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x DESC",
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x LIMIT 2 OFFSET 1",
        "WITH q AS (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) SELECT x, count(*) FROM q GROUP BY x ORDER BY x",
        "SELECT * FROM (SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x)",
        "SELECT x, count(*) FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x, count(*)",
        "SELECT DISTINCT x FROM (SELECT c AS x, n FROM t ORDER BY 1 COLLATE NOCASE LIMIT 9) GROUP BY x ORDER BY x",
    ]
    .iter()
    .map(|q| q.to_string())
    .collect();
    // A generated corpus of subquery orders and reader GROUP BY /
    // ORDER BY shapes.
    let mut r = Rng(0x5eed_c011);
    for _ in 0..240 {
        let tbl = r.pick(&["t", "u"]);
        let ob = r.pick(&["1", "x", "c", "1, 2", "2", "x, n", "2, 1"]);
        let coll = r.pick(&["", " COLLATE NOCASE", " COLLATE BINARY", " COLLATE RTRIM"]);
        let dir = r.pick(&["", " DESC", " NULLS LAST", " DESC NULLS FIRST"]);
        let lim = r.pick(&[" LIMIT 9", "", " LIMIT 4", " LIMIT 9 OFFSET 1"]);
        let mut parts: Vec<String> = ob.split(", ").map(str::to_string).collect();
        parts[0] = format!("{}{coll}{dir}", parts[0]);
        let gb = r.pick(&["x", "x, n", "n, x", "x COLLATE NOCASE", "x COLLATE BINARY"]);
        let rob = r.pick(&[
            "",
            " ORDER BY x",
            " ORDER BY x DESC",
            " ORDER BY x, n",
            " ORDER BY 1",
            " ORDER BY x COLLATE NOCASE",
            " ORDER BY x COLLATE BINARY",
            " ORDER BY x NULLS LAST",
            " ORDER BY n, x",
        ]);
        let hav = r.pick(&["", "", " HAVING count(*) > 0"]);
        let rl = r.pick(&["", "", " LIMIT 3", " LIMIT 2 OFFSET 1"]);
        qs.push(format!(
            "SELECT x, count(*), sum(n) FROM (SELECT c AS x, n FROM {tbl} ORDER BY {}{lim}) GROUP BY {gb}{hav}{rob}{rl}",
            parts.join(", ")
        ));
    }
    let path = fresh("cgo", schema);
    let bad = diffs_on(&path, &qs, false);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// An ORDER BY an index delivers in part, under a LIMIT (review fix):
/// the pin computes each row as it comes and ends its loop at the
/// first row of the prefix group past OFFSET + LIMIT rows
/// (`pushOntoSorter`'s `OP_IfNot iLimit`), so a later row's failing
/// result column is never computed.
#[test]
fn partly_delivered_order_stops_at_the_limit_group() {
    let x = format!("CASE WHEN a=9 THEN {BAD} ELSE a END");
    let base =
        "CREATE TABLE t(a,b);; INSERT INTO t VALUES(1,0),(7,0),(2,0),(3,1),(5,1),(9,2),(4,3);;";
    let mut scripts: Vec<String> = Vec::new();
    for lim in [
        "LIMIT 1",
        "LIMIT 2",
        "LIMIT 3",
        "LIMIT 4",
        "LIMIT 5",
        "LIMIT 1 OFFSET 2",
        "LIMIT 1 OFFSET 3",
        "LIMIT 2 OFFSET 2",
        "LIMIT 3 OFFSET 1",
        "LIMIT 0",
        "LIMIT -1",
    ] {
        for (idx, q) in [
            (
                "CREATE INDEX tb ON t(b)",
                format!("SELECT {x} FROM t WHERE b>=0 ORDER BY b, a {lim}"),
            ),
            (
                "CREATE INDEX tba ON t(b,a)",
                format!("SELECT b, {x} FROM t ORDER BY b, 2 {lim}"),
            ),
            (
                "CREATE INDEX tb ON t(b)",
                format!("SELECT * FROM (SELECT {x} FROM t WHERE b>=0 ORDER BY b, a {lim})"),
            ),
            (
                "CREATE INDEX tb ON t(b)",
                format!("SELECT (SELECT {x} FROM t WHERE b>=0 ORDER BY b, a {lim})"),
            ),
            (
                "CREATE INDEX tb ON t(b)",
                format!("SELECT count(*) FROM (SELECT {x} FROM t WHERE b>=0 ORDER BY b, -a {lim})"),
            ),
        ] {
            scripts.push(format!("{base} {idx};; {q}"));
        }
    }
    // The reviewer's repro.
    scripts.push(format!(
        "CREATE TABLE t(a,b);; INSERT INTO t VALUES(2,1),(3,2),(1,5),(4,0);; CREATE INDEX tb ON t(b);; \
         SELECT {} FROM t WHERE b>=0 ORDER BY b, a LIMIT 1",
        x.replace("a=9", "a=1")
    ));
    // A generated corpus (no DISTINCT: see the gaps).
    let mut r = Rng(0x0b5a_7001);
    for _ in 0..200 {
        let rows: Vec<String> = (0..2 + r.n(8))
            .map(|_| {
                let a = r.pick(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "9"]);
                let b = r.pick(&["0", "1", "2", "3", "NULL"]);
                format!("({a},{b})")
            })
            .collect();
        let idx = r.pick(&[
            "CREATE INDEX tb ON t(b)",
            "CREATE INDEX tba ON t(b,a)",
            "CREATE INDEX tbd ON t(b DESC)",
        ]);
        let sel = r.pick(&["$X", "b, $X", "a, $X", "$X, a"]);
        let ob = r.pick(&[
            "b, a",
            "b, 2",
            "b, a DESC",
            "b DESC, a",
            "b, -a",
            "b, a+0",
            "b, $X",
            "b NULLS LAST, a",
        ]);
        // (Not `b IS NOT NULL`: the pin's planner reads it as an index
        // range, hipp's does not — see the gaps.)
        let wh = r.pick(&["", "WHERE b>=0 ", "WHERE b>0 "]);
        let lim = r.pick(&[
            "LIMIT 1",
            "LIMIT 2",
            "LIMIT 3",
            "LIMIT 1 OFFSET 1",
            "LIMIT 2 OFFSET 2",
            "LIMIT 0",
            "LIMIT 5",
            "LIMIT -1",
        ]);
        let wrap = r.pick(&[
            "{}",
            "{}",
            "SELECT * FROM ({})",
            "SELECT count(*) FROM ({})",
        ]);
        let q = wrap.replace(
            "{}",
            &format!("SELECT {sel} FROM t {wh}ORDER BY {ob} {lim}"),
        );
        scripts.push(format!(
            "CREATE TABLE t(a,b);; INSERT INTO t VALUES{};; {idx};; {}",
            rows.join(","),
            q.replace("$X", &x)
        ));
    }
    let owned: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("pobs", "", &owned);
}

/// A constant multi-row INSERT of a few thousand rows under the
/// default statement arena (review fix): a statement's names and
/// string literals are handles into its stored text
/// (`Arena::handle_or_copy`), so the arena holds little beyond the
/// text — 4,000 rows of `(i, 'xi')` (61,800 B) insert as on the pin.
#[test]
fn long_insert_values_fit_the_default_arena() {
    let shapes: [(&str, &dyn Fn(usize) -> String); 4] = [
        ("t", &|i| format!("({i}, 'x{i}')")),
        ("t", &|i| format!("(abs({i}), 1)")),
        ("t", &|_| "(random(), 1)".to_string()),
        ("u", &|_| "(random())".to_string()),
    ];
    for (k, (tbl, row)) in shapes.iter().enumerate() {
        for n in [3000usize, 4000] {
            let rows: Vec<String> = (0..n).map(row).collect();
            let ins = format!("INSERT INTO {tbl} VALUES {}", rows.join(","));
            let path = fresh(
                &format!("arena{k}{n}"),
                "CREATE TABLE t(a,b); CREATE TABLE u(a);",
            );
            let path2 = fresh(
                &format!("arena{k}{n}p"),
                "CREATE TABLE t(a,b); CREATE TABLE u(a);",
            );
            assert!(
                pin(&path2, &ins).is_ok(),
                "pin refused {n} rows of shape {k}"
            );
            let p = pin(&path2, &format!("SELECT count(*) FROM {tbl}")).expect("pin count");
            let got = block_on(async {
                let fs = FileStorage::open(&path).expect("fs open");
                let mut db = Connection::open(fs, Config::default()).await.expect("open");
                let mut st = db.prepare(&ins).await.map_err(|e| format!("{e:?}"))?;
                while st.step().await.map_err(|e| format!("{e:?}"))? == Step::Row {}
                let hw = st.arena_high_water();
                drop(st);
                let rows = collect(&mut db, &format!("SELECT count(*) FROM {tbl}"))
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                Ok::<_, String>((hw, String::from_utf8_lossy(&rows).to_string()))
            });
            let (hw, h) = got.unwrap_or_else(|e| panic!("hipp refused {n} rows of shape {k}: {e}"));
            assert_eq!(h, p, "{n} rows of shape {k}");
            // (The text, and little more: no per-row copy.)
            assert!(
                hw <= ins.len() + 4096,
                "{n} rows of shape {k}: {hw} B for {} B",
                ins.len()
            );
        }
    }
}

/// The "nested too deep" count is the pin's cursor numbers when a CTE
/// reference expands (review fix): every FROM item a select's
/// expansion assigns before its expressions, an UPDATE / DELETE
/// target and its indexes, UPDATE ... FROM's join. Each shape is
/// checked on both sides of the pin's boundary.
#[test]
fn cte_limit_counts_the_pins_cursors() {
    let refs = |n: usize| vec!["(SELECT x FROM c0)"; n].join(",");
    let schema = "CREATE TABLE u(x); INSERT INTO u VALUES(1); CREATE TABLE w(y); INSERT INTO w VALUES(1); \
                  CREATE VIEW v AS SELECT x FROM u; CREATE VIEW v2 AS SELECT * FROM v; \
                  CREATE TABLE ui(x); INSERT INTO ui VALUES(1); CREATE INDEX uix ON ui(x); CREATE INDEX uix2 ON ui(x);";
    // (shape, the pin's first failing count)
    let shapes: [(&str, usize); 30] = [
        ("SELECT 1 WHERE 1 IN ({})", 1000),
        ("SELECT 1 FROM u WHERE 1 IN ({})", 999),
        ("SELECT count(*) FROM u WHERE x IN ({})", 999),
        ("SELECT 1 FROM u WHERE 1 = coalesce({})", 999),
        ("SELECT 1 FROM u GROUP BY x HAVING 1 IN ({})", 999),
        ("SELECT 1 FROM u GROUP BY coalesce({})", 999),
        ("SELECT 1 FROM u ORDER BY coalesce({})", 999),
        ("SELECT 1 FROM u JOIN w ON 1 IN ({})", 998),
        ("SELECT coalesce({}) FROM u", 999),
        ("SELECT 1 FROM u LIMIT coalesce({})", 999),
        ("SELECT 1 FROM (SELECT 1) WHERE 1 IN ({})", 999),
        ("SELECT 1 FROM u WHERE 1 IN ({}) UNION ALL SELECT 1", 999),
        ("SELECT (SELECT 1 FROM u), {}", 999),
        ("SELECT {}, (SELECT 1 FROM u)", 1000),
        ("SELECT 1 FROM u, u AS u2, u AS u3 WHERE 1 IN ({})", 997),
        ("SELECT 1 FROM json_each('[1]') WHERE 1 IN ({})", 999),
        ("SELECT 1 FROM v WHERE 1 IN ({})", 998),
        ("SELECT 1 FROM v2 WHERE 1 IN ({})", 997),
        ("SELECT {} FROM v", 998),
        ("SELECT 1 FROM u, (SELECT 1 WHERE 1 IN ({}))", 998),
        ("SELECT 1 FROM (SELECT 1 WHERE 1 IN ({})), u", 998),
        ("SELECT 1 FROM (SELECT 1 FROM u) WHERE 1 IN ({})", 998),
        ("SELECT EXISTS (SELECT 1 FROM u), {}", 999),
        (
            "SELECT 1 FROM u WHERE EXISTS (SELECT 1 WHERE 1 IN ({}))",
            999,
        ),
        ("DELETE FROM u WHERE x IN ({})", 999),
        ("DELETE FROM ui WHERE x IN ({})", 997),
        ("UPDATE ui SET x=1 WHERE x IN ({})", 997),
        ("UPDATE u SET x=(SELECT 1 WHERE 1 IN ({}))", 999),
        ("UPDATE u SET x=2 FROM w, w AS w2 WHERE 1 IN ({})", 994),
        ("UPDATE u SET x=2 FROM (SELECT 1 WHERE 1 IN ({}))", 996),
    ];
    let mut scripts = Vec::new();
    for (q, n) in shapes {
        for k in [n - 1, n] {
            scripts.push(format!(
                "WITH c0 AS (SELECT 1 AS x) {};; SELECT 'done'",
                q.replace("{}", &refs(k))
            ));
        }
    }
    scripts.push(format!(
        "WITH c0 AS (SELECT 1 AS x), c1 AS (SELECT 1 FROM u, w) SELECT {} FROM c1",
        refs(996)
    ));
    scripts.push(format!(
        "WITH c0 AS (SELECT 1 AS x), c1 AS (SELECT 1 FROM u, w) SELECT {} FROM c1",
        refs(997)
    ));
    let owned: Vec<&str> = scripts.iter().map(String::as_str).collect();
    script_same("ctecur", schema, &owned);
}
