//! v0.17 parity unit `order_eqp`: the ORDER BY / LIMIT wrong answers
//! and EXPLAIN QUERY PLAN differences the v0.17 units `order_offset`,
//! `values_chain`, `result_gaps` and `gaps_plan_cli` recorded against
//! the pinned oracle (SQLite 3.54.1), each probed live on the pin first:
//!
//! - DISTINCT under an ORDER BY an index delivers in part stops after
//!   the LIMIT's distinct rows (the pin's distinct table drops a repeat
//!   before its sorter counts it); a DISTINCT whose ORDER BY repeats
//!   its result columns is the pin's GROUP BY and sorts every row.
//! - `x IN (SELECT col FROM tbl ORDER BY ...)` where the rowid or an
//!   index serves `col` reads the table in place of the subquery
//!   (`sqlite3FindInIndex`): its ORDER BY never runs.
//! - A signed `LIMIT +1` in a scalar / EXISTS subquery takes no
//!   UNION-with-LIMIT-1 shortcut; such a subquery is planned for the
//!   LIMIT `sqlite3CodeSubselect` leaves it (its implicit 1 or a bare
//!   written 1; any other `X` became the non-constant `X <> 0`).
//! - A lone FROM subquery delivers ORDER BY terms its reader's WHERE
//!   fixes to one value, then its own order (wherePathSatisfiesOrderBy).
//! - The IN operator's Bloom filter lines where the pin writes the set's
//!   rows (each core of a UNION ALL, a merge's output routines, each
//!   row of a VALUES chain; none under a sorter); a merged VALUES core's
//!   sorter; a VALUES chain under a scalar / EXISTS LIMIT is one row.
//!
//! Each query's rows (in order, or the error) and its EXPLAIN QUERY
//! PLAN must be the pin's (an error on both counts as agreement). With
//! the hidden `vm` feature they run on today's executor
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
    let path = common::temp_db(&format!("v17oe-{tag}"));
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

/// The failing expression the corpora use: integer overflow for a = 9.
const X: &str = "CASE WHEN a=9 THEN abs(-9223372036854775808) ELSE a END";
const BAD: &str = "abs(-9223372036854775808)";

#[test]
fn signed_limit_one_takes_no_shortcut() {
    let schema = "CREATE TABLE t(a); INSERT INTO t VALUES(2),(1);";
    let mut qs: Vec<String> = Vec::new();
    for l in [
        "LIMIT 1",
        "LIMIT +1",
        "LIMIT ++1",
        "LIMIT -(-1)",
        "LIMIT (1)",
        "LIMIT (+1)",
        "LIMIT 01",
        "LIMIT 0x1",
        "LIMIT 0x01",
        "LIMIT 1+0",
        "LIMIT '1'",
        "LIMIT 1.0",
        "LIMIT 0,1",
        "LIMIT 1 OFFSET 0",
        "LIMIT 2",
    ] {
        let u = format!("SELECT a FROM t UNION SELECT a FROM t {l}");
        qs.push(format!("SELECT ({u})"));
        qs.push(format!("SELECT EXISTS ({u})"));
        qs.push(format!("SELECT 1 IN ({u})"));
        qs.push(format!("SELECT a FROM t WHERE a IN ({u})"));
        qs.push(format!("SELECT * FROM ({u})"));
        qs.push(u);
    }
    let path = fresh("lim1", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn first_row_subquery_planned_for_its_limit() {
    let schema = "CREATE TABLE t(a,b); CREATE INDEX tb ON t(b); \
                  INSERT INTO t VALUES(7,0),(3,3),(9,0),(8,2),(7,2),(1,0),(7,0),(1,3),(6,NULL),(6,0),(1,0);";
    let mut qs: Vec<String> = Vec::new();
    for l in [
        "",
        "LIMIT 1",
        "LIMIT 2",
        "LIMIT +1",
        "LIMIT 0x1",
        "LIMIT (1)",
        "LIMIT 2-1",
        "LIMIT 1 OFFSET 0",
        "LIMIT 1 OFFSET 1",
        "LIMIT 3 OFFSET 1",
        "LIMIT 0",
        "LIMIT -1",
        "LIMIT '1'",
    ] {
        qs.push(format!("SELECT (SELECT {X} FROM t ORDER BY b, a {l})"));
        qs.push(format!("SELECT (SELECT a FROM t ORDER BY b, a {l})"));
        qs.push(format!("SELECT EXISTS (SELECT a FROM t ORDER BY b, a {l})"));
        qs.push(format!(
            "SELECT a FROM t WHERE a = (SELECT a FROM t ORDER BY b, a {l})"
        ));
        qs.push(format!(
            "SELECT (SELECT DISTINCT {X} FROM t ORDER BY b, {X} {l})"
        ));
    }
    qs.push("SELECT a FROM t ORDER BY b, a LIMIT 2+0".into());
    qs.push("SELECT a FROM t ORDER BY b, a LIMIT 1".into());
    let path = fresh("firstrow", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn distinct_partial_order_stops_at_its_limit() {
    let schema = "CREATE TABLE t(a,b); CREATE INDEX tb ON t(b); \
                  INSERT INTO t VALUES(1,0),(7,0),(1,0),(3,1),(5,1),(9,2),(4,3),(4,3);";
    let mut qs: Vec<String> = Vec::new();
    for ob in [
        "b, a",
        "b, 2 DESC",
        "b, -a",
        "b, 2",
        "b, a DESC",
        "b NULLS LAST, a",
    ] {
        for l in [
            "LIMIT 1",
            "LIMIT 2",
            "LIMIT 3",
            "LIMIT 4",
            "LIMIT 5",
            "LIMIT 1 OFFSET 1",
            "LIMIT 1 OFFSET 2",
            "LIMIT 1 OFFSET 3",
            "LIMIT 0",
            "LIMIT -1",
        ] {
            let q = format!("SELECT DISTINCT b, {X} FROM t WHERE b>=0 ORDER BY {ob} {l}");
            qs.push(format!("SELECT * FROM ({q})"));
            qs.push(format!("SELECT count(*) FROM ({q})"));
            qs.push(format!("SELECT 2 IN ({q})").replace("DISTINCT b, ", "DISTINCT "));
            qs.push(q);
        }
    }
    // The pin's DISTINCT-to-GROUP-BY rewrite: ORDER BY repeats the
    // result columns (ordinals, CASE expressions), so every row sorts.
    qs.push(format!(
        "SELECT DISTINCT b, {X} FROM t WHERE b>=0 ORDER BY b, {X} LIMIT 2"
    ));
    qs.push(format!(
        "SELECT DISTINCT b, {X} FROM t WHERE b>=0 ORDER BY 1, 2 DESC LIMIT 2"
    ));
    qs.push(
        "SELECT DISTINCT b, a LIKE 'x' FROM t WHERE b>=0 ORDER BY b, a LIKE 'x' LIMIT 2".into(),
    );
    let path = fresh("dpart", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn distinct_partial_order_generated() {
    let mut r = Rng(0x0e9d_1717);
    let mut bad = Vec::new();
    for k in 0..160 {
        let rows: Vec<String> = (0..2 + r.n(11))
            .map(|_| {
                let a = r.pick(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "9", "1", "1"]);
                let b = r.pick(&["0", "1", "2", "3", "NULL", "0"]);
                format!("({a},{b})")
            })
            .collect();
        let idx = r.pick(&[
            "CREATE INDEX tb ON t(b)",
            "CREATE INDEX tba ON t(b,a)",
            "CREATE INDEX tbd ON t(b DESC)",
        ]);
        let schema = format!(
            "CREATE TABLE t(a,b); INSERT INTO t VALUES{}; {idx};",
            rows.join(",")
        );
        let sel = r.pick(&["b, $X", "$X", "a%3, $X", "b%2, $X", "b, a%2"]);
        let ob = r.pick(&[
            "b, a",
            "b, 2 DESC",
            "b DESC, a",
            "b, -a",
            "b, $X",
            "b NULLS LAST, a",
            "b, 1",
        ]);
        let wh = r.pick(&["WHERE b>=0 ", "WHERE b>0 ", ""]);
        let lim = r.pick(&[
            "LIMIT 1",
            "LIMIT 2",
            "LIMIT 3",
            "LIMIT 1 OFFSET 1",
            "LIMIT 2 OFFSET 2",
            "LIMIT 5",
            "LIMIT 0",
        ]);
        let q = format!("SELECT DISTINCT {sel} FROM t {wh}ORDER BY {ob} {lim}").replace("$X", X);
        let q = match r.n(6) {
            0 => format!("SELECT * FROM ({q})"),
            1 => format!("SELECT ({q})"),
            2 => format!("SELECT count(*) FROM ({q})"),
            3 => format!("SELECT 1 WHERE 2 IN ({q})"),
            _ => q,
        };
        // (Plans without a WHERE: the pin's DISTINCT-to-GROUP-BY rewrite
        // plans for the GROUP BY, which hipp does not model; see the gaps.)
        let path = fresh(&format!("dgen{k}"), &schema);
        bad.extend(diffs_on(&path, &[q], !wh.is_empty()));
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn in_subquery_reads_its_index() {
    let schema = "CREATE TABLE t(a, b TEXT, c INT, d COLLATE NOCASE, e); CREATE INDEX tb ON t(b); \
                  CREATE INDEX tc ON t(c); CREATE INDEX td ON t(d); CREATE INDEX tde ON t(d COLLATE BINARY, e); \
                  CREATE UNIQUE INDEX tu ON t(e); INSERT INTO t VALUES(1,'1',1,'x',1),(2,'2',2,'y',2); \
                  CREATE TABLE o(x, y TEXT, z INT); CREATE INDEX ox ON o(x); INSERT INTO o VALUES(1,'1',1),(2,'2',2),(3,'3',3); \
                  CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID; CREATE INDEX wv ON w(v); INSERT INTO w VALUES(1,'a'),(2,'b');";
    let o = format!("ORDER BY {BAD}");
    let qs: Vec<String> = vec![
        format!("SELECT 2 IN (SELECT rowid FROM t {o})"),
        format!("SELECT 2 NOT IN (SELECT rowid FROM t {o})"),
        format!("SELECT 2 IN (SELECT b FROM t {o})"),
        format!("SELECT '2' IN (SELECT b FROM t {o})"),
        format!("SELECT 2 IN (SELECT c FROM t {o})"),
        format!("SELECT '2' IN (SELECT c FROM t {o})"),
        format!("SELECT 'Y' IN (SELECT d FROM t {o})"),
        format!("SELECT 'Y' COLLATE BINARY IN (SELECT d FROM t {o})"),
        format!("SELECT 2 IN (SELECT e FROM t {o})"),
        format!("SELECT 2 IN (SELECT a FROM t {o})"),
        format!("SELECT ('y',2) IN (SELECT d, e FROM t {o})"),
        format!("SELECT x FROM o WHERE x IN (SELECT e FROM t {o})"),
        format!("SELECT x FROM o WHERE x IN (SELECT c FROM t {o})"),
        format!("SELECT z FROM o WHERE z IN (SELECT c FROM t {o})"),
        format!("SELECT x FROM o WHERE x IN (SELECT rowid FROM t {o})"),
        format!("SELECT x FROM o WHERE o.rowid IN (SELECT c FROM t {o})"),
        format!("SELECT x FROM o WHERE o.rowid IN (SELECT rowid FROM t {o})"),
        format!("SELECT x FROM o WHERE z IN (SELECT rowid FROM t WHERE 1 {o})"),
        format!("SELECT x FROM o WHERE z IN (SELECT rowid FROM t {o} LIMIT 5)"),
        format!("SELECT x FROM o WHERE z IN (SELECT DISTINCT rowid FROM t {o})"),
        format!("SELECT x FROM o WHERE z IN (SELECT rowid FROM t UNION SELECT 1 {o})"),
        format!("SELECT 1 IN (SELECT k FROM w {o})"),
        format!("SELECT 'a' IN (SELECT v FROM w {o})"),
        format!("SELECT 2 IN (SELECT rowid FROM t ORDER BY 1 DESC, {BAD})"),
        "SELECT 2 IN (SELECT rowid FROM t ORDER BY random())".into(),
        format!("SELECT 2 IN (SELECT +c FROM t {o})"),
        format!("SELECT 2 IN (SELECT c FROM (SELECT c FROM t) {o})"),
    ];
    let path = fresh("inix", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn in_subquery_index_generated() {
    let mut r = Rng(0x1dc0_de17);
    let mut bad = Vec::new();
    for k in 0..140 {
        let wr = r.n(4) == 0;
        let cols = "a INT, b TEXT, c, d COLLATE NOCASE, e REAL";
        let tdef = if wr {
            format!("CREATE TABLE t({cols}, PRIMARY KEY(a)) WITHOUT ROWID")
        } else {
            format!("CREATE TABLE t({cols})")
        };
        let mut idx = String::new();
        for i in [
            "CREATE INDEX i1 ON t(b)",
            "CREATE UNIQUE INDEX i2 ON t(c)",
            "CREATE INDEX i3 ON t(d)",
            "CREATE INDEX i4 ON t(d COLLATE BINARY, c)",
            "CREATE INDEX i5 ON t(e)",
            "CREATE INDEX i6 ON t(c, b)",
            "CREATE INDEX i7 ON t(b) WHERE b>0",
        ] {
            if r.n(3) == 0 {
                idx.push_str(i);
                idx.push_str("; ");
            }
        }
        // (`i2` is UNIQUE on c: then c is the row number.)
        let uc = idx.contains("i2");
        let rows: Vec<String> = (1..2 + r.n(5))
            .map(|i| {
                let c = if uc {
                    i.to_string()
                } else {
                    r.pick(&["1", "2", "NULL", "'1'"]).to_string()
                };
                format!(
                    "({i},'{}',{c},'{}',{})",
                    r.pick(&["1", "2", "x", "Y"]),
                    r.pick(&["x", "y", "X", "Y"]),
                    r.pick(&["1.0", "2", "NULL"])
                )
            })
            .collect();
        let col = if wr {
            r.pick(&["a", "b", "c", "d", "e", "d, c", "c, b", "a, c"])
        } else {
            r.pick(&["rowid", "a", "b", "c", "d", "e", "d, c", "c, b", "+c"])
        };
        let row = col.contains(',');
        // (A row value probing an index on the outer column is the pin's
        // vector IN loop, which hipp does not plan: see the gaps.)
        let ox = if row { "" } else { "CREATE INDEX ox ON o(x);" };
        let schema = format!(
            "{tdef}; {idx}INSERT INTO t VALUES{}; CREATE TABLE o(x, y TEXT, z INT); {ox} \
             INSERT INTO o VALUES(1,'1',1),(2,'2',2),('x','x',3);",
            rows.join(",")
        );
        let ob = r
            .pick(&["$B", "1, $B", "$B DESC", "random()", "1", "c, $B"])
            .replace("$B", BAD);
        let wh = if r.n(7) == 0 { "WHERE 1 " } else { "" };
        let sub = format!("SELECT {col} FROM t {wh}ORDER BY {ob}");
        let lhs = if row {
            r.pick(&["('y',2)", "('x',1)", "(x, y)", "(y, z)"])
        } else {
            r.pick(&[
                "2",
                "'2'",
                "'x'",
                "'X'",
                "1.0",
                "NULL",
                "'Y' COLLATE BINARY",
                "x",
                "y",
                "z",
                "o.rowid",
            ])
        };
        let neg = r.pick(&["", "NOT "]);
        let q = match r.n(4) {
            0 => format!("SELECT {lhs} {neg}IN ({sub}) FROM o"),
            1 => format!("SELECT x FROM o WHERE {lhs} {neg}IN ({sub})"),
            2 => format!("SELECT count(*) FROM o WHERE {lhs} {neg}IN ({sub})"),
            _ => format!("SELECT x FROM o WHERE {lhs} {neg}IN ({sub}) ORDER BY x"),
        };
        let path = fresh(&format!("igen{k}"), &schema);
        bad.extend(diffs_on(&path, &[q], true));
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn coroutine_order_fixed_by_where() {
    let schema = "CREATE TABLE u(c INT, d TEXT, e); \
                  INSERT INTO u VALUES(3,'X',3),(2,'x',2),(3,'y',1),(NULL,'X',3),(2,NULL,1);";
    let mut qs: Vec<String> = Vec::new();
    for w in [
        "WHERE c = 3 ORDER BY c",
        "WHERE c = 3 ORDER BY c, d",
        "WHERE c = 3 ORDER BY d, c",
        "WHERE c = d ORDER BY c",
        "WHERE c = 3 ORDER BY c COLLATE NOCASE",
        "WHERE d = 'x' ORDER BY d COLLATE NOCASE",
        "WHERE d = 'x' COLLATE NOCASE ORDER BY d",
        "WHERE +c = 3 ORDER BY c",
        "WHERE c IS 3 ORDER BY c",
        "WHERE 3 = c ORDER BY c",
        "WHERE c = 3 AND d = 'a' ORDER BY d, c",
        "WHERE c = 3 OR c = 4 ORDER BY c",
        "WHERE c = random() ORDER BY c",
        "WHERE c = (SELECT 1) ORDER BY c",
        "WHERE c IN (3) ORDER BY c",
        "WHERE c IN (3, 4) ORDER BY c",
        "WHERE c = 3 ORDER BY c DESC",
        "WHERE c = 3 ORDER BY 1",
        "WHERE c IS NULL ORDER BY c, e",
        "WHERE c IS NOT NULL ORDER BY c",
        "WHERE c = 3 ORDER BY c LIMIT 1",
        "WHERE c = 2 ORDER BY c, d LIMIT 1 OFFSET 1",
    ] {
        qs.push(format!("SELECT * FROM (SELECT c, d, e FROM u LIMIT 5) {w}"));
        qs.push(format!(
            "SELECT * FROM (SELECT c, d, e FROM u ORDER BY d LIMIT 5) {w}"
        ));
        qs.push(format!(
            "SELECT * FROM (SELECT c, d, e FROM u ORDER BY e DESC LIMIT 5) {w}"
        ));
    }
    let path = fresh("cofix", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn coroutine_order_fixed_generated() {
    let mut r = Rng(0xc0_7017);
    let mut bad = Vec::new();
    for k in 0..200 {
        let rows: Vec<String> = (0..1 + r.n(7))
            .map(|_| {
                format!(
                    "({},{},{})",
                    r.pick(&["1", "2", "3", "NULL", "'a'", "'A'"]),
                    r.pick(&["'x'", "'X'", "'y'", "NULL", "2"]),
                    r.pick(&["1", "2", "3"])
                )
            })
            .collect();
        let schema = format!(
            "CREATE TABLE u(c INT, d TEXT COLLATE NOCASE, e); INSERT INTO u VALUES{}; \
             CREATE VIEW v AS SELECT c, d, e FROM u ORDER BY d LIMIT 9;",
            rows.join(",")
        );
        let sob = r.pick(&[
            "",
            "ORDER BY d",
            "ORDER BY c",
            "ORDER BY c, d",
            "ORDER BY d DESC",
            "ORDER BY 2, 1",
            "ORDER BY e",
        ]);
        let src = if r.n(4) == 0 {
            "v s".to_string()
        } else {
            format!(
                "(SELECT c, d, e FROM u {sob} LIMIT {}) s",
                r.pick(&["9", "3"])
            )
        };
        let wh = r.pick(&[
            "c = 2",
            "d = 'x'",
            "c = 2 AND d = 'x'",
            "e = 1",
            "c IS NULL",
            "c IS 2",
            "2 = c",
            "c IN (2)",
            "d = 'X' COLLATE BINARY",
            "c = e",
            "c = 2 OR d = 'x'",
            "+c = 2",
            "c = (SELECT 2)",
            "e = 3 AND c = 1",
        ]);
        let ob = r.pick(&[
            "c",
            "d",
            "c, d",
            "d, c",
            "e, d",
            "c, e",
            "c DESC",
            "d, e, c",
            "c, d DESC",
            "d COLLATE BINARY",
            "1",
            "2, 1",
        ]);
        let lim = r.pick(&["", "", " LIMIT 2", " LIMIT 1 OFFSET 1"]);
        let q = format!("SELECT * FROM {src} WHERE {wh} ORDER BY {ob}{lim}");
        let path = fresh(&format!("cgen{k}"), &schema);
        bad.extend(diffs_on(&path, &[q], true));
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn bloom_filters_and_values_plans() {
    let schema = "CREATE TABLE t(a,b,c); CREATE TABLE o(x,y); INSERT INTO t VALUES(1,2,3),(4,5,6); \
                  INSERT INTO o VALUES(1,1),(5,2),(9,3);";
    let mut qs: Vec<String> = Vec::new();
    for sub in [
        "SELECT c FROM t",
        "SELECT c FROM t ORDER BY c",
        "SELECT c FROM t ORDER BY 1 LIMIT 3",
        "SELECT c FROM t LIMIT 3",
        "SELECT DISTINCT c FROM t ORDER BY a",
        "SELECT DISTINCT c FROM t",
        "SELECT c FROM t GROUP BY c ORDER BY c",
        "SELECT c FROM t GROUP BY c ORDER BY count(*)",
        "SELECT a FROM t UNION ALL SELECT b FROM t",
        "SELECT a FROM t UNION SELECT b FROM t",
        "SELECT a FROM t UNION ALL SELECT b FROM t UNION ALL SELECT c FROM t",
        "SELECT a FROM t UNION ALL SELECT b FROM t ORDER BY 1",
        "SELECT a FROM t UNION ALL SELECT b FROM t ORDER BY 1 LIMIT 2",
        "SELECT a FROM t EXCEPT SELECT b FROM t",
        "SELECT a FROM t INTERSECT SELECT b FROM t",
        "SELECT a FROM t UNION SELECT b FROM t UNION ALL SELECT c FROM t",
        "SELECT a FROM t UNION SELECT b FROM t UNION SELECT c FROM t",
        "SELECT a FROM t UNION ALL SELECT b FROM t UNION SELECT c FROM t",
        "SELECT a FROM t UNION SELECT b FROM t ORDER BY 1",
        "SELECT 1 UNION ALL SELECT 2",
        "VALUES (1),(2)",
        "VALUES (1),(random() IS NOT NULL)",
        "VALUES (1),(random() IS NOT NULL),(3)",
        "VALUES (random() IS NOT NULL),(2),(3)",
        "SELECT a FROM t UNION ALL VALUES (1),(2)",
    ] {
        qs.push(format!("SELECT x FROM o WHERE x IN ({sub})"));
        qs.push(format!("SELECT x IN ({sub}) FROM o"));
        qs.push(format!("SELECT x FROM o WHERE x COLLATE NOCASE IN ({sub})"));
    }
    for q in [
        "SELECT (VALUES (1),(random() IS NOT NULL))",
        "SELECT EXISTS (VALUES (1),(random() IS NOT NULL))",
        "SELECT (VALUES (random() IS NOT NULL),(2),(3))",
        "SELECT (VALUES (1),(2))",
        "VALUES (1),(2) UNION VALUES (3),(4)",
        "VALUES (1),(random() IS NOT NULL) UNION VALUES (3),(4)",
        "VALUES (1),(2) UNION SELECT 3",
        "SELECT 3 UNION VALUES (1),(2)",
        "SELECT a FROM t UNION VALUES (1),(2)",
        "VALUES (1),(2) EXCEPT VALUES (3),(4)",
        "VALUES (1,2),(3,4) UNION VALUES (5,6),(7,8)",
        "SELECT * FROM (VALUES (1),(2) UNION VALUES (3),(4))",
    ] {
        qs.push(q.into());
    }
    let path = fresh("bloom", schema);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}
