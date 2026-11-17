//! v0.17 parity unit `order_offset`: the ORDER BY / OFFSET result
//! divergences STATUS.md recorded against the pinned oracle (SQLite
//! 3.54.1), each probed live on the pin before the fix:
//!
//! - The OFFSET skips a row before its result columns are computed
//!   (the pin's `selectInnerLoop` codes `codeOffset` ahead of the
//!   columns when no sorter and no DISTINCT handle the rows): a
//!   FROM-less core, a subquery over a table, a UNION ALL chain's
//!   cores, views and CTEs, window queries and aggregates (whose
//!   aggregates still step) — `SELECT abs(-9223372036854775808) LIMIT
//!   1 OFFSET 1` is no row. DISTINCT, a sorter and the other compound
//!   operators compute first.
//! - An ORDER BY whose loop makes the rows distinct (the rowid, a
//!   UNIQUE index over NOT NULL columns, a WITHOUT ROWID table's key
//!   with NOT NULL columns) delivers every later term that reads the
//!   tables already distinct or is constant (`isOrderDistinct`,
//!   `orderDistinctMask`): those terms are never computed, so `ORDER
//!   BY rowid, abs(-9223372036854775808)` answers. A later term of
//!   another kind (`random()`, a subquery) leaves the whole ORDER BY
//!   to the sorter.
//! - A FROM subquery's ORDER BY without a LIMIT is dropped when its
//!   reader has an ORDER BY (or the merge key of a compound) or more
//!   than one source and no aggregate but count / min / max
//!   (`SQLITE_OmitOrderBy`).
//! - A reader's ORDER BY that a lone co-routine's own order delivers
//!   (its written ORDER BY, or a compound's merge key) by result
//!   column number is no sorter, whatever the reader's collation
//!   (`SQLITE_OrderBySubq`, `wherePathMatchSubqueryOB`).
//!
//! Each query's rows (in order, or the error) and its EXPLAIN QUERY
//! PLAN must be the pin's (an error on both counts as agreement).
//! With the hidden `vm` feature they run on today's executor
//! (`Config::vm_off`): the frozen VM (ADR 0005 / 0007) keeps its own
//! OFFSET and ORDER BY handling, recorded as a gap in STATUS.md.
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
    let path = common::temp_db(&format!("v17oo-{tag}"));
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

/// The overflowing constant every corpus uses as "a term that fails".
const BAD: &str = "abs(-9223372036854775808)";

fn same(tag: &str, schema: &str, qs: &[&str], plans: bool) {
    let path = fresh(tag, schema);
    let owned: Vec<String> = qs.iter().map(|q| q.replace("BAD", BAD)).collect();
    let bad = diffs_on(&path, &owned, plans);
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

/// The schema every corpus reads: a NOCASE column, nullable and NOT
/// NULL UNIQUE indexes, a WITHOUT ROWID table with and without a
/// nullable column, rows whose rowid order differs from their value
/// order. `f(n)` = `abs(n - 9223372036854775807 - k)` overflows for
/// the row whose `n` is `k - 1`.
const S: &str = "CREATE TABLE t(c TEXT COLLATE NOCASE, n INT); \
    INSERT INTO t VALUES ('b',1),('A',2),('C',3),('a',4),(NULL,5); \
    CREATE TABLE u(a INT, b TEXT); CREATE UNIQUE INDEX ua ON u(a); \
    INSERT INTO u VALUES (3,'x'),(1,'y'),(2,'z'),(NULL,'w'); \
    CREATE TABLE w(a INT NOT NULL, b TEXT, c INT); CREATE UNIQUE INDEX wa ON w(a); \
    CREATE INDEX wb ON w(b); \
    INSERT INTO w VALUES (3,'x',1),(1,'y',2),(2,'x',3),(5,'z',4); \
    CREATE TABLE wr(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID; \
    INSERT INTO wr VALUES ('b',1),('a',2),('c',3); \
    CREATE TABLE wn(k TEXT PRIMARY KEY, v INT NOT NULL) WITHOUT ROWID; \
    INSERT INTO wn VALUES ('b',1),('a',2),('c',3); \
    CREATE TABLE t2(x TEXT COLLATE NOCASE, n INT); \
    INSERT INTO t2 VALUES ('a',1),('B',5),('A',3),('b',2),('c',4),('C',0); \
    CREATE VIEW vo AS SELECT n, abs(n-9223372036854775807-2) AS f FROM t LIMIT 2 OFFSET 1; \
    CREATE VIEW vs AS SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC;";

// ------------------------------------------------------------ OFFSET

/// The OFFSET skips rows before their result columns are computed on
/// every executor path; DISTINCT, a sorter and a merged compound
/// compute them first.
#[test]
fn offset_skips_before_result_columns() {
    same(
        "off",
        S,
        &[
            "SELECT BAD LIMIT 1 OFFSET 1",
            "SELECT BAD LIMIT -1 OFFSET 1",
            "SELECT BAD, 2 LIMIT 5 OFFSET 1",
            "SELECT * FROM (SELECT BAD LIMIT 1 OFFSET 1)",
            "SELECT (SELECT BAD LIMIT 1 OFFSET 1)",
            "SELECT EXISTS (SELECT BAD LIMIT 1 OFFSET 1)",
            "SELECT 3 IN (SELECT BAD LIMIT 1 OFFSET 1)",
            "SELECT BAD UNION ALL SELECT 2 LIMIT 1 OFFSET 1",
            "SELECT 1 UNION ALL SELECT BAD LIMIT 1 OFFSET 1",
            "SELECT 1 UNION ALL SELECT BAD UNION ALL SELECT 3 LIMIT 1 OFFSET 2",
            "SELECT BAD UNION ALL SELECT BAD UNION ALL SELECT 7 LIMIT 1 OFFSET 2",
            "SELECT BAD UNION SELECT 2 LIMIT 1 OFFSET 1",
            "SELECT BAD UNION ALL SELECT 2 ORDER BY 1 LIMIT 1 OFFSET 1",
            "SELECT 1 UNION ALL SELECT 2 UNION SELECT BAD LIMIT 1 OFFSET 2",
            "SELECT * FROM (SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1)",
            "SELECT * FROM (SELECT n, abs(n-9223372036854775807-6) FROM t LIMIT 2 OFFSET 4)",
            "SELECT (SELECT abs(n-9223372036854775807-2) FROM t LIMIT 1 OFFSET 1)",
            "SELECT n FROM t WHERE n IN (SELECT abs(n-9223372036854775807-2) FROM t LIMIT 1 OFFSET 1)",
            "WITH c AS (SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1) SELECT * FROM c",
            "SELECT * FROM vo",
            "SELECT n, abs(n-9223372036854775807-2) FROM t UNION ALL SELECT 9, 9 LIMIT 2 OFFSET 1",
            "SELECT 9, 9 UNION ALL SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 2",
            "SELECT n, abs(n-9223372036854775807-2) FROM t UNION ALL SELECT n, abs(n-9223372036854775807-2) FROM t LIMIT 3 OFFSET 6",
            "SELECT * FROM (SELECT BAD UNION ALL SELECT 2 LIMIT 1 OFFSET 1)",
            "WITH c(v) AS (SELECT BAD UNION ALL SELECT 2 LIMIT 1 OFFSET 1) SELECT * FROM c",
            "SELECT n FROM t WHERE n IN (SELECT BAD UNION ALL SELECT 2 LIMIT 1 OFFSET 1)",
            "SELECT DISTINCT n, abs(n-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1",
            "SELECT DISTINCT BAD LIMIT 1 OFFSET 1",
            "SELECT n, abs(n-9223372036854775807-2) FROM t ORDER BY n LIMIT 2 OFFSET 1",
            "SELECT n, abs(n-9223372036854775807-2) FROM t ORDER BY rowid LIMIT 2 OFFSET 1",
            "SELECT * FROM (SELECT n, abs(n-9223372036854775807-2) FROM t ORDER BY rowid LIMIT 2 OFFSET 1)",
            "SELECT n, abs(n-9223372036854775807-2) FROM t ORDER BY n DESC LIMIT 2 OFFSET 3",
            "SELECT BAD ORDER BY 1 LIMIT 1 OFFSET 1",
            "SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c LIMIT 2 OFFSET 1",
            "SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c LIMIT 1 OFFSET 1",
            "SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c HAVING count(*) > 0 LIMIT 1 OFFSET 1",
            "SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c ORDER BY c LIMIT 1 OFFSET 1",
            "SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c ORDER BY c DESC LIMIT 1 OFFSET 3",
            "SELECT * FROM (SELECT c, abs(count(*)-9223372036854775807-2) FROM t GROUP BY c LIMIT 1 OFFSET 1)",
            "SELECT abs(count(*)-9223372036854775807-6) FROM t LIMIT 1 OFFSET 1",
            "SELECT count(*), BAD LIMIT 1 OFFSET 1",
            "SELECT (SELECT abs(count(*)-9223372036854775807-6) FROM t LIMIT 1 OFFSET 1)",
            "SELECT sum(abs(n-9223372036854775807-6)) FROM t LIMIT 1 OFFSET 1",
            "SELECT sum(9223372036854775807) FROM (SELECT 1 UNION ALL SELECT 2) LIMIT 1 OFFSET 1",
            "SELECT c, sum(9223372036854775807) FROM t GROUP BY c LIMIT 1 OFFSET 9",
            "SELECT n, abs(n-9223372036854775807-2), row_number() OVER () FROM t LIMIT 2 OFFSET 1",
            "SELECT n, abs(row_number() OVER ()-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1",
            "SELECT n, abs(row_number() OVER (ORDER BY n DESC)-9223372036854775807-2) FROM t LIMIT 2 OFFSET 4",
            "SELECT n, abs(row_number() OVER (ORDER BY n DESC)-9223372036854775807-2) FROM t ORDER BY n LIMIT 2 OFFSET 4",
            "SELECT * FROM (SELECT n, abs(row_number() OVER ()-9223372036854775807-2) FROM t LIMIT 2 OFFSET 1)",
            "SELECT a.n, abs(a.n-9223372036854775807-2) FROM t a JOIN t b ON a.n = b.n LIMIT 2 OFFSET 1",
            "SELECT n, (SELECT abs(n-9223372036854775807-2)) FROM t LIMIT 2 OFFSET 1",
            "SELECT * FROM (SELECT 1 UNION ALL VALUES (BAD), (2)) LIMIT 1 OFFSET 2",
            "SELECT n FROM t LIMIT 2 OFFSET 'x'",
            "SELECT n FROM t LIMIT 0 OFFSET BAD",
            "SELECT DISTINCT n, abs(n-9223372036854775807-6) FROM t LIMIT 2",
            "SELECT * FROM (SELECT DISTINCT n, abs(n-9223372036854775807-6) FROM t LIMIT 3 OFFSET 0)",
            "SELECT n, c FROM t UNION ALL SELECT BAD, 9 LIMIT 3 OFFSET 1",
            "SELECT * FROM (SELECT n, c FROM t UNION ALL SELECT BAD, 9 LIMIT 3 OFFSET 1)",
            "SELECT n, abs(n-9223372036854775807-6) FROM t UNION ALL SELECT 8, 9 LIMIT 2 OFFSET 2",
        ],
        false,
    );
}

/// Generated OFFSET queries over every executor path: FROM-less and
/// table cores, nested in FROM / scalar / IN subqueries, CTEs, UNION
/// ALL chains, aggregates, windows, DISTINCT and ORDER BY.
#[test]
fn generated_offsets_match_the_pin() {
    let mut r = Rng(0x0ff5_e7a1_9e57_1234);
    let cols = [
        "abs(n-9223372036854775807-2)",
        "abs(n-9223372036854775807-6)",
        "n",
        "c",
        "abs(row_number() OVER ()-9223372036854775807-2)",
        "BAD",
    ];
    let aggs = [
        "abs(count(*)-9223372036854775807-2)",
        "abs(count(*)-9223372036854775807-6)",
        "count(*)",
        "max(n)",
    ];
    let mut qs: Vec<String> = Vec::new();
    for _ in 0..160 {
        let lim = r.pick(&["1", "2", "-1", "3"]);
        let off = r.pick(&["0", "1", "2", "4", "5"]);
        let core = match r.n(7) {
            0 => format!("SELECT {}", r.pick(&["BAD", "1", "BAD, 2"])),
            1 => format!("SELECT n, {} FROM t", r.pick(&cols)),
            2 => {
                format!("SELECT {}{} FROM t", r.pick(&["", "c, "]), r.pick(&aggs))
                    + r.pick(&["", " GROUP BY c", " GROUP BY c HAVING count(*) > 1"])
            }
            3 => format!(
                "SELECT n, {} FROM t UNION ALL SELECT {}, 9",
                r.pick(&cols[..4]),
                r.pick(&["BAD", "8"])
            ),
            4 => format!("SELECT DISTINCT n, {} FROM t", r.pick(&cols[..4])),
            5 => format!(
                "SELECT n, {} FROM t ORDER BY {}",
                r.pick(&cols[..4]),
                r.pick(&["n", "rowid", "n DESC", "c"])
            ),
            _ => format!(
                "SELECT {} UNION ALL SELECT {}",
                r.pick(&["BAD", "1"]),
                r.pick(&["BAD", "2"])
            ),
        };
        let tail = format!("{core} LIMIT {lim} OFFSET {off}");
        let q = match r.n(5) {
            0 => tail,
            1 => format!("SELECT * FROM ({tail})"),
            2 => format!("WITH q AS ({tail}) SELECT * FROM q"),
            3 => format!("SELECT count(*) FROM ({tail})"),
            _ => format!("SELECT * FROM ({tail}) LIMIT 9"),
        };
        qs.push(q.replace("BAD", BAD));
    }
    let path = fresh("goff", S);
    let bad = diffs_on(&path, &qs, false);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

// ------------------------------------------------- distinct ORDER BY

/// Past the rowid, a UNIQUE NOT NULL key or a WITHOUT ROWID table's
/// NOT NULL key the rows are distinct: later terms reading the table
/// or constant are never computed; other terms sort everything.
#[test]
fn distinct_order_by_tails_match_the_pin() {
    same(
        "dist",
        S,
        &[
            "SELECT a FROM u ORDER BY rowid, BAD",
            "SELECT a FROM u ORDER BY rowid DESC, BAD",
            "SELECT b FROM u ORDER BY rowid NULLS LAST, BAD",
            "SELECT b FROM u ORDER BY rowid DESC NULLS FIRST, BAD",
            "SELECT b FROM u ORDER BY rowid, BAD + a",
            "SELECT b FROM u ORDER BY rowid, (SELECT BAD WHERE u.a)",
            "SELECT b FROM u ORDER BY rowid, (SELECT BAD)",
            "SELECT b FROM u ORDER BY rowid, random()",
            "SELECT a FROM u ORDER BY a, BAD",
            "SELECT a FROM w ORDER BY a, BAD",
            "SELECT a, b FROM w ORDER BY a DESC, BAD",
            "SELECT a, b FROM w ORDER BY a, random()",
            "SELECT b FROM w ORDER BY b, BAD",
            "SELECT b FROM w ORDER BY b, rowid, BAD",
            "SELECT b FROM w ORDER BY b, a, BAD",
            "SELECT b FROM w WHERE a = 2 ORDER BY BAD",
            "SELECT b FROM w WHERE a = 2 ORDER BY (SELECT BAD)",
            "SELECT b FROM w WHERE a = 2 ORDER BY random()",
            "SELECT b FROM w WHERE b = 'x' ORDER BY b, rowid, BAD",
            "SELECT b FROM w WHERE a > 1 ORDER BY a, BAD",
            "SELECT k FROM wr ORDER BY k, BAD",
            "SELECT k FROM wr ORDER BY k DESC, v + BAD",
            "SELECT k FROM wr ORDER BY k, v",
            "SELECT k FROM wn ORDER BY k DESC, v + BAD",
            "SELECT x FROM t2 ORDER BY rowid COLLATE NOCASE, BAD",
            "SELECT * FROM (SELECT b FROM u ORDER BY rowid, BAD LIMIT 9)",
            "SELECT (SELECT b FROM u ORDER BY rowid, BAD)",
            "SELECT DISTINCT b FROM u ORDER BY rowid, BAD",
            "SELECT b FROM u ORDER BY rowid, BAD LIMIT 2 OFFSET 1",
            "SELECT b FROM u GROUP BY rowid ORDER BY rowid, BAD",
            "SELECT n, row_number() OVER () FROM t ORDER BY rowid, BAD",
            "SELECT u.b, t.n FROM u, t ORDER BY u.rowid, t.rowid, BAD LIMIT 3",
            "SELECT u.b, t.n FROM u, t ORDER BY u.rowid, t.n, BAD LIMIT 3",
            "SELECT u.b, w.a FROM u, w ORDER BY u.rowid, w.a, BAD LIMIT 3",
            "SELECT u.b, w.a FROM u, w ORDER BY u.rowid, BAD, w.a LIMIT 3",
            "SELECT u.b, w.a FROM u, w ORDER BY u.rowid, u.b, w.a LIMIT 5",
            "SELECT u.b FROM u, w ORDER BY u.rowid, (SELECT BAD) LIMIT 3",
        ],
        true,
    );
}

/// Generated ORDER BY tails over rowid tables, indexes and joins.
#[test]
fn generated_order_tails_match_the_pin() {
    let mut r = Rng(0x0d15_7123_4567_89ab);
    let lead = [
        "rowid",
        "rowid DESC",
        "a",
        "a DESC",
        "b",
        "b, rowid",
        "c",
        "a NULLS LAST",
    ];
    let tail = ["BAD", "BAD + c", "c", "b DESC", "(SELECT BAD)", "1", "-a"];
    let mut qs: Vec<String> = Vec::new();
    for _ in 0..140 {
        let mut ob = vec![r.pick(&lead).to_string()];
        for _ in 0..r.n(3) {
            ob.push(r.pick(&tail).to_string());
        }
        let ob = ob.join(", ");
        let wh = r.pick(&["", " WHERE a > 1", " WHERE a = 2", " WHERE b = 'x'"]);
        let q = match r.n(4) {
            0 => format!("SELECT a, b FROM w{wh} ORDER BY {ob}"),
            1 => format!("SELECT a, b FROM u{wh} ORDER BY {ob}"),
            2 => format!("SELECT * FROM (SELECT a, b FROM w{wh} ORDER BY {ob} LIMIT 9)"),
            _ => format!(
                "SELECT w.a, u.b FROM w, u ORDER BY w.{}, u.rowid, {} LIMIT 6",
                r.pick(&["rowid", "a"]),
                r.pick(&tail).replace('c', "w.c").replace("-a", "-w.a")
            ),
        };
        qs.push(q.replace("BAD", BAD));
    }
    let path = fresh("gtail", S);
    let bad = diffs_on(&path, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

// ------------------------------------------- subqueries' ORDER BY

/// A FROM subquery's ORDER BY without a LIMIT is dropped under an
/// ordered reader, a join or a merged compound, unless the reader
/// aggregates in order (any aggregate but count / min / max) or the
/// CTE is MATERIALIZED.
#[test]
fn omitted_subquery_order_matches_the_pin() {
    same(
        "omit",
        S,
        &[
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC)",
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC LIMIT 99) ORDER BY x",
            "SELECT x, n FROM (SELECT DISTINCT x, n FROM t2 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT DISTINCT x, n FROM t2 ORDER BY n DESC) a, u WHERE u.a = 1",
            "SELECT a.x, a.n FROM (SELECT x, n FROM t2 ORDER BY n DESC) a, (SELECT 1)",
            "SELECT sum(n), group_concat(x) FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY 1",
            "SELECT total(n), avg(n) FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY 1",
            "SELECT count(*), max(x) FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY 1",
            "SELECT x, group_concat(n) FROM (SELECT x, n FROM t2 ORDER BY n DESC) GROUP BY x",
            "SELECT x, group_concat(n) FROM (SELECT x, n FROM t2 ORDER BY n DESC) GROUP BY x ORDER BY x",
            "SELECT x, max(n) FROM (SELECT x, n FROM t2 ORDER BY n DESC) GROUP BY x ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC), (SELECT 1)",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION SELECT 'Z', 9 ORDER BY n DESC), (SELECT 1)",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 INTERSECT SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 EXCEPT SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) WHERE n > 0",
            "WITH q AS (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) SELECT x, n FROM q ORDER BY x",
            "WITH q AS (SELECT x, n FROM t2 ORDER BY n DESC) SELECT x FROM q ORDER BY x",
            "WITH q AS NOT MATERIALIZED (SELECT x, n FROM t2 ORDER BY n DESC) SELECT x FROM q ORDER BY x",
            "WITH q AS MATERIALIZED (SELECT x, n FROM t2 ORDER BY n DESC) SELECT x FROM q ORDER BY x",
            "WITH q AS MATERIALIZED (SELECT x, n FROM t2 ORDER BY n DESC) SELECT a.x FROM q a, u b WHERE b.a = 2",
            "WITH q AS (SELECT x, n FROM t2 ORDER BY n DESC) SELECT a.x, b.n FROM q a, q b WHERE a.n = b.n + 1",
            "SELECT x, n FROM vs ORDER BY x",
            "SELECT x, n FROM vs",
            "SELECT x FROM (SELECT x, n FROM t2 ORDER BY n DESC) UNION SELECT 'q'",
            "SELECT x FROM (SELECT x, n FROM t2 ORDER BY n DESC) UNION ALL SELECT 'q'",
            "SELECT x FROM (SELECT x, n FROM t2 ORDER BY n DESC) UNION ALL SELECT 'q' ORDER BY 1",
            "SELECT x, row_number() OVER (ORDER BY x) FROM (SELECT x, n FROM t2 ORDER BY n DESC)",
            "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 4 ORDER BY 1) SELECT i FROM r ORDER BY i",
        ],
        false,
    );
    // The plans where hipp keeps the pin's shape (a co-routine in a
    // join keeps FROM order in hipp, and the pin flattens a subquery
    // into a compound's core: pre-existing, recorded in STATUS.md).
    same(
        "omitp",
        S,
        &[
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY n DESC LIMIT 99) ORDER BY x",
            "SELECT x, n FROM (SELECT DISTINCT x, n FROM t2 ORDER BY n DESC) ORDER BY x",
            "SELECT sum(n), group_concat(x) FROM (SELECT x, n FROM t2 ORDER BY n DESC) ORDER BY 1",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION ALL SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION SELECT 'Z', 9 ORDER BY n DESC) ORDER BY x",
            "WITH q AS MATERIALIZED (SELECT x, n FROM t2 ORDER BY n DESC) SELECT x FROM q ORDER BY x",
            "SELECT x, n FROM vs ORDER BY x",
        ],
        true,
    );
}

/// A UNION ALL compound's ORDER BY keeps equal rows in core order,
/// each core in its own order (the pin's merge by the ORDER BY terms
/// alone), also after a UNION / EXCEPT core.
#[test]
fn union_all_order_ties_match_the_pin() {
    same(
        "ties",
        S,
        &[
            "SELECT x, n FROM t2 UNION ALL SELECT c, n FROM t ORDER BY n DESC",
            "SELECT x, n FROM t2 UNION ALL SELECT 'a', 1 ORDER BY 1 DESC",
            "SELECT x, n FROM t2 UNION ALL SELECT 'a', 1 ORDER BY 1",
            "SELECT x, n FROM t2 UNION ALL SELECT 'a', 1 UNION ALL SELECT 'A', 0 ORDER BY 1, 2 DESC",
            "SELECT x FROM t2 UNION ALL SELECT c FROM t ORDER BY 1 COLLATE BINARY",
            "SELECT x, n FROM t2 UNION SELECT 'a', 1 UNION ALL SELECT 'A', 0 ORDER BY 1",
            "SELECT x, n FROM t2 UNION ALL SELECT 'a', 1 UNION SELECT 'A', 0 ORDER BY 1",
            "SELECT x, n FROM t2 UNION SELECT 'a', 1 UNION ALL SELECT 'A', 0 ORDER BY 2 DESC",
            "SELECT x, n FROM t2 EXCEPT SELECT 'a', 1 UNION ALL SELECT 'A', 0 ORDER BY 1 DESC, 2",
            "SELECT * FROM (SELECT x, n FROM t2 UNION ALL SELECT 'a', 1 ORDER BY 1 DESC LIMIT 99)",
            "SELECT x, n FROM t2 UNION ALL SELECT c, n FROM t ORDER BY n DESC LIMIT 4 OFFSET 2",
        ],
        false,
    );
}

/// An ORDER BY a lone co-routine's own order delivers, matched by
/// result column number, is no sorter (rows in the subquery's order
/// and collation); a COLLATE, another direction or NULLS placement,
/// or a term past the subquery's ORDER BY sorts.
#[test]
fn coroutine_order_matches_the_pin() {
    same(
        "corder",
        S,
        &[
            "WITH q(x) AS (SELECT 2.5 UNION SELECT c FROM t) SELECT x FROM q ORDER BY 1",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x DESC",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x COLLATE NOCASE",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x COLLATE BINARY",
            "SELECT x FROM (SELECT 2.5 x UNION SELECT c FROM t) ORDER BY x NULLS LAST",
            "SELECT x FROM (SELECT c x FROM t ORDER BY 1 LIMIT 9) ORDER BY 1",
            "SELECT x, n FROM (SELECT c x, n FROM t ORDER BY n DESC LIMIT 9) ORDER BY n DESC",
            "SELECT x, n FROM (SELECT c x, n FROM t ORDER BY n LIMIT 9) ORDER BY n DESC",
            "SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 'z' ORDER BY 1 LIMIT 9) ORDER BY 1",
            "SELECT x FROM (SELECT c x FROM t UNION SELECT 'z') ORDER BY 1",
            "SELECT x FROM (SELECT c x FROM t UNION SELECT 'z' UNION SELECT 'y') ORDER BY 1",
            "SELECT x, n FROM (SELECT x, n FROM t2 ORDER BY x COLLATE BINARY LIMIT 99) ORDER BY x",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION SELECT 'Z', 9) ORDER BY x, n",
            "SELECT x, n FROM (SELECT x, n FROM t2 UNION SELECT 'Z', 9) ORDER BY n, x",
            "SELECT x, n FROM (SELECT n, x FROM t2 UNION SELECT 9, 'Z') ORDER BY n",
            "SELECT x FROM (SELECT x FROM t2 UNION SELECT 'Z' ORDER BY 1 DESC LIMIT 9) ORDER BY x DESC",
            "SELECT x FROM (SELECT x FROM t2 UNION SELECT 'Z' ORDER BY 1 DESC LIMIT 9) ORDER BY x",
            "SELECT a FROM (SELECT a FROM w ORDER BY a LIMIT 9) ORDER BY a",
            "SELECT x FROM (SELECT x FROM t2 INTERSECT SELECT x FROM t2) ORDER BY 1",
            "SELECT x FROM (SELECT x FROM t2 EXCEPT SELECT 'q') ORDER BY 1",
            "SELECT x FROM (SELECT x FROM t2 EXCEPT SELECT 'q') ORDER BY 1 LIMIT 2 OFFSET 1",
            "SELECT x FROM (SELECT x FROM t2 UNION SELECT 'q') WHERE x > 'a' ORDER BY 1",
            "SELECT x, y FROM (SELECT x, 1 y FROM t2 UNION SELECT 'q', 2) ORDER BY 1, 2",
        ],
        true,
    );
}

/// Generated FROM subqueries with and without ORDER BY / LIMIT,
/// compound operators and readers that order, join or aggregate.
#[test]
fn generated_subquery_orders_match_the_pin() {
    let mut r = Rng(0x5eed_0b5c_7a11_c0de);
    let mut qs: Vec<String> = Vec::new();
    for _ in 0..160 {
        let body = match r.n(4) {
            0 => format!(
                "SELECT x, n FROM t2 {}",
                r.pick(&["", "WHERE n > 0", "WHERE x > 'a'"])
            ),
            1 => format!(
                "SELECT x, n FROM t2 {} SELECT {}",
                r.pick(&["UNION", "UNION ALL", "EXCEPT", "INTERSECT"]),
                r.pick(&["'Z', 9", "'a', 1", "c, n FROM t"])
            ),
            2 => "SELECT c AS x, n FROM t".to_string(),
            _ => "SELECT 2.5 AS x, 0 AS n UNION SELECT c, n FROM t".to_string(),
        };
        let sub_ob = r.pick(&[
            "",
            " ORDER BY n DESC",
            " ORDER BY 1",
            " ORDER BY 1 DESC",
            " ORDER BY x, n",
            " ORDER BY 2, 1",
        ]);
        let lim = if sub_ob.is_empty() {
            ""
        } else {
            r.pick(&["", " LIMIT 99"])
        };
        let sub = format!("({body}{sub_ob}{lim})");
        let q = match r.n(5) {
            0 => format!(
                "SELECT x, n FROM {sub} ORDER BY {}",
                r.pick(&[
                    "x",
                    "1",
                    "n",
                    "x DESC",
                    "x, n",
                    "x COLLATE BINARY",
                    "x NULLS LAST"
                ])
            ),
            1 => format!("SELECT x, n FROM {sub}"),
            2 => format!("SELECT s.x, s.n FROM {sub} s, (SELECT 1)"),
            3 => format!(
                "SELECT {} FROM {sub}",
                r.pick(&[
                    "group_concat(x)",
                    "count(*), max(n)",
                    "sum(n), group_concat(n)"
                ])
            ),
            _ => format!(
                "SELECT x FROM {sub} {} SELECT 'q'",
                r.pick(&["UNION", "UNION ALL"])
            ),
        };
        qs.push(q);
    }
    let path = fresh("gsub", S);
    let bad = diffs_on(&path, &qs, false);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}
