//! v0.17 join ordering and automatic indexes (leaf `join_order`),
//! differentially vs the pinned oracle (SQLite 3.54.1).
//!
//! The pin's `where.c` planner orders a join's FROM sources by cost
//! (`wherePathSolver`) and may read a source through an automatic
//! index built for the statement, with a Bloom filter in front of
//! it; with `sqlite_stat1` rows it also puts Bloom filters in front
//! of index and rowid lookups. hipp now plans joins the same way
//! (`src/xjoin.rs`, `session::join`), so a query without ORDER BY
//! returns its rows in the pin's order, and EXPLAIN QUERY PLAN prints
//! the pin's loops (SCAN / SEARCH order, `AUTOMATIC [PARTIAL]
//! COVERING INDEX`, `BLOOM FILTER ON`). The corpus here:
//!
//! - a generated corpus (hand-rolled PRNG) of 2- to 6-way joins over
//!   random schemas (rowid, INTEGER PRIMARY KEY and WITHOUT ROWID
//!   tables; single- and two-column, UNIQUE indexes; none), random
//!   data with NULLs and mixed types, with no statistics, with the
//!   pin's ANALYZE and with hipp's; comma joins, JOIN ... ON, LEFT
//!   JOIN and CROSS JOIN; equality, range, IN, BETWEEN and IS NOT NULL
//!   terms; ORDER BY, GROUP BY, DISTINCT and LIMIT. The rows must be
//!   the pin's (none may differ as a set, a LIMIT's checked without
//!   it), in the pin's order (at most 2 in 100 queries may come in
//!   another, 0.75% measured over 1,600), and the plans the pin's (at
//!   most 10 in 100 may differ, 6.6% measured, most of them 4- to
//!   6-way joins: STATUS.md lists the classes);
//! - a curated corpus of every plan form: reordering, the automatic
//!   index and its partial form, Bloom filters with and without
//!   statistics, LEFT / CROSS JOIN ordering constraints, LEFT JOIN
//!   simplification, constant propagation, MATCH and table-valued
//!   arguments reading a table listed later in FROM (refused before
//!   v0.17), with data;
//! - regressions for the bugs the unit fixed (the collation of a
//!   commuted join term, `IS NOT NULL` as the right operand of the
//!   LEFT JOIN simplification, ANALYZE read back in the same
//!   connection);
//! - performance: an automatic index makes a 2,000 x 2,000 equi-join
//!   far faster than the nested loop it replaces.

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

/// One statement's rows in the CLI's list rendering (one line per
/// row, each ended by a newline, as the shell prints them).
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

/// hipp's answers on the file at `path`: per query, its rows (or the
/// error) and its EXPLAIN QUERY PLAN.
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

/// Run `sql` through hipp on `path` (writes included).
fn hipp_exec(path: &Path, sql: &str) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut st = db.prepare(sql).await.expect("prepare");
        while st.step().await.expect("step") == Step::Row {}
    })
}

fn fresh(tag: &str, schema: &str) -> PathBuf {
    let path = common::temp_db(&format!("v17jo-{tag}"));
    common::cli(&path, schema);
    path
}

/// Every query of `qs` on a file with `schema`: hipp's rows, in
/// order, and its plan are the pin's (or both engines fail).
fn same(tag: &str, schema: &str, qs: &[&str]) {
    let path = fresh(tag, schema);
    same_on(&path, qs);
}

fn same_on(path: &Path, qs: &[&str]) {
    let owned: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let got = hipp_answers(path, &owned);
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
        let p_plan = pin(path, &format!("EXPLAIN QUERY PLAN {q}"));
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

/// A schema of 2 to 6 tables with data, and its tables.
fn gen_schema(r: &mut Rng) -> (String, Vec<Tab>) {
    let nt = 2 + r.n(5) as usize;
    let mut out = String::new();
    let mut tabs = Vec::new();
    for ti in 0..nt {
        let name = format!("t{}", ti + 1);
        let ncol = 2 + r.n(3) as usize;
        let cols: Vec<String> = (0..ncol)
            .map(|i| ["a", "b", "c", "d"][i].to_string())
            .collect();
        let ipk = r.chance(30);
        let wr = !ipk && r.chance(10);
        let mut defs = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            if i == 0 && ipk {
                defs.push(format!("{c} INTEGER PRIMARY KEY"));
            } else {
                let t = r.pick(&["INTEGER", "TEXT", "", "INT", "REAL"]);
                defs.push(format!("{c} {t}").trim().to_string());
            }
        }
        if wr {
            defs.push("PRIMARY KEY(a)".into());
        }
        out += &format!(
            "CREATE TABLE {name}({}){};\n",
            defs.join(", "),
            if wr { " WITHOUT ROWID" } else { "" }
        );
        for k in 0..r.n(3) {
            let first = if ipk { 1 } else { 0 };
            let c1 = &cols[first + r.n((ncol - first) as u64) as usize];
            let mut ic = vec![c1.clone()];
            if r.chance(40) {
                let c2 = &cols[r.n(ncol as u64) as usize];
                if c2 != c1 && !(ipk && c2 == "a") {
                    ic.push(c2.clone());
                }
            }
            let u = if r.chance(15) { "UNIQUE " } else { "" };
            out += &format!(
                "CREATE {u}INDEX IF NOT EXISTS {name}_i{k} ON {name}({});\n",
                ic.join(",")
            );
        }
        for j in 0..r.n(26) {
            let mut vals = Vec::new();
            for c in 0..ncol {
                if c == 0 && (ipk || wr) {
                    vals.push((j + 1).to_string());
                    continue;
                }
                let x = r.n(100);
                vals.push(if x < 10 {
                    "NULL".into()
                } else if x < 25 {
                    format!("'{}'", r.pick(&["x", "y", "z"]))
                } else {
                    r.n(7).to_string()
                });
            }
            out += &format!("INSERT OR IGNORE INTO {name} VALUES({});\n", vals.join(","));
        }
        tabs.push(Tab { name, cols });
    }
    (out, tabs)
}

/// A 2- to 6-way join over `tabs`.
fn gen_query(r: &mut Rng, tabs: &[Tab]) -> String {
    let k = 2 + r.n(tabs.len() as u64 - 1) as usize;
    let mut order: Vec<usize> = (0..tabs.len()).collect();
    for i in (1..order.len()).rev() {
        let j = r.n(i as u64 + 1) as usize;
        order.swap(i, j);
    }
    let used: Vec<&Tab> = order[..k].iter().map(|&i| &tabs[i]).collect();
    let mut from = String::new();
    let mut conds: Vec<String> = Vec::new();
    let mut aliases: Vec<(String, &Tab)> = Vec::new();
    for (i, t) in used.iter().enumerate() {
        let al = format!("x{i}");
        let pick_prev = |r: &mut Rng, aliases: &Vec<(String, &Tab)>| {
            let (pa, pt) = &aliases[r.n(aliases.len() as u64) as usize];
            format!("{pa}.{}", pt.cols[r.n(pt.cols.len() as u64) as usize])
        };
        if i == 0 {
            from = format!("{} {al}", t.name);
        } else {
            let op = r.pick(&[",", ",", " JOIN ", " LEFT JOIN ", " CROSS JOIN ", " JOIN "]);
            let mut on = String::new();
            if op.trim() == "JOIN" || op.trim() == "LEFT JOIN" {
                let c = &t.cols[r.n(t.cols.len() as u64) as usize];
                on = format!(" ON {al}.{c}={}", pick_prev(r, &aliases));
                if r.chance(20) {
                    let c = &t.cols[r.n(t.cols.len() as u64) as usize];
                    on += &format!(" AND {al}.{c}>{}", r.n(6));
                }
            } else if r.chance(80) {
                let c = &t.cols[r.n(t.cols.len() as u64) as usize];
                conds.push(format!("{al}.{c}={}", pick_prev(r, &aliases)));
            }
            from += &format!("{op}{} {al}{on}", t.name);
        }
        aliases.push((al, t));
    }
    for _ in 0..r.n(3) {
        let (al, t) = &aliases[r.n(aliases.len() as u64) as usize];
        let c = &t.cols[r.n(t.cols.len() as u64) as usize];
        let v = r.n(7);
        conds.push(match r.n(6) {
            0 => format!("{al}.{c}={v}"),
            1 => format!("{al}.{c}>{v}"),
            2 => format!("{al}.{c}<{v}"),
            3 => format!("{al}.{c} IS NOT NULL"),
            4 => format!("{al}.{c} IN (1,2,{v})"),
            _ => format!("{al}.{c} BETWEEN 1 AND {v}"),
        });
    }
    let mut sel: Vec<String> = Vec::new();
    for (al, t) in aliases.iter() {
        for c in t.cols.iter() {
            if r.chance(50) {
                sel.push(format!("{al}.{c}"));
            }
        }
    }
    if sel.is_empty() {
        sel.push("*".into());
    }
    let mode = r.n(100);
    let mut tail = String::new();
    let mut dist = "";
    if mode < 15 && sel[0] != "*" {
        tail = format!(" ORDER BY {}", sel[r.n(sel.len() as u64) as usize]);
    } else if mode < 25 && sel[0] != "*" {
        let g = sel[r.n(sel.len() as u64) as usize].clone();
        sel = vec![g.clone(), "count(*)".into()];
        tail = format!(" GROUP BY {g}");
    } else if mode < 32 {
        dist = "DISTINCT ";
    }
    let mut q = format!("SELECT {dist}{} FROM {from}", sel.join(", "));
    if !conds.is_empty() {
        q += &format!(" WHERE {}", conds.join(" AND "));
    }
    q += &tail;
    if r.chance(10) {
        q += &format!(" LIMIT {}", 1 + r.n(5));
    }
    q
}

/// The generated corpus (see the module comment). `HIPP_JO_SEEDS`
/// runs more seeds.
#[test]
fn generated_joins_match_pin() {
    let seeds: u64 = std::env::var("HIPP_JO_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (mut n, mut order_bad, mut plan_bad) = (0usize, Vec::new(), Vec::new());
    let mut hard = Vec::new();
    for seed in 1..=seeds {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut schema, tabs) = gen_schema(&mut r);
        let stats = r.n(100);
        if stats < 25 {
            schema += "ANALYZE;\n";
        }
        let path = fresh(&format!("gen{seed}"), &schema);
        if (25..40).contains(&stats) {
            hipp_exec(&path, "ANALYZE");
        }
        let qs: Vec<String> = (0..4).map(|_| gen_query(&mut r, &tabs)).collect();
        let got = hipp_answers(&path, &qs);
        for (q, (rows, plan)) in qs.iter().zip(got) {
            n += 1;
            let p_rows = pin(&path, q);
            match (&rows, &p_rows) {
                (Ok(h), Ok(p)) if h.as_slice() == p.as_bytes() => {}
                (Ok(h), Ok(p)) => {
                    let h = String::from_utf8_lossy(h).to_string();
                    // (A LIMIT keeps rows by their order: the rows
                    // without it must be the same.)
                    let (h_all, p_all) = match q.find(" LIMIT ") {
                        Some(at) => {
                            let q_all = q[..at].to_string();
                            let h_all = hipp_answers(&path, std::slice::from_ref(&q_all))
                                .pop()
                                .and_then(|x| x.0.ok())
                                .map(|r| String::from_utf8_lossy(&r).to_string());
                            (h_all, pin(&path, &q_all).ok())
                        }
                        None => (Some(h.clone()), Some(p.clone())),
                    };
                    let (h_all, p_all) = (h_all.unwrap_or_default(), p_all.unwrap_or_default());
                    let mut a: Vec<&str> = h_all.lines().collect();
                    let mut b: Vec<&str> = p_all.lines().collect();
                    a.sort_unstable();
                    b.sort_unstable();
                    if a == b {
                        order_bad.push(format!("seed {seed}: {q}"));
                    } else {
                        hard.push(format!("seed {seed}: {q}\n  pin : {p:?}\n  hipp: {h:?}"));
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
        order_bad.len() * 100 <= n * 2,
        "{} of {n} row orders differ:\n{}",
        order_bad.len(),
        order_bad.join("\n")
    );
    assert!(
        plan_bad.len() * 100 <= n * 10,
        "{} of {n} plans differ:\n{}",
        plan_bad.len(),
        plan_bad.join("\n")
    );
    if !order_bad.is_empty() || !plan_bad.is_empty() {
        eprintln!(
            "{} orders, {} plans of {n} differ:\n{}\n{}",
            order_bad.len(),
            plan_bad.len(),
            order_bad.join("\n"),
            plan_bad.join("\n")
        );
    }
}

// ----------------------------------------------------------- curated

const SCHEMA: &str = "CREATE TABLE a(x INTEGER PRIMARY KEY, y, z TEXT); \
     CREATE TABLE b(p, q INT, r); CREATE INDEX bq ON b(q); \
     CREATE TABLE c(s, t, u NOT NULL); \
     CREATE TABLE d(k INT, v TEXT, PRIMARY KEY(k)) WITHOUT ROWID; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<30) \
     INSERT INTO a SELECT i, i%7, 'z'||(i%4) FROM n; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<40) \
     INSERT INTO b SELECT i%9, i%11, NULLIF(i%5, 0) FROM n; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<25) \
     INSERT INTO c SELECT i%6, i%13, i FROM n; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<12) \
     INSERT INTO d SELECT i, 'v'||i FROM n;";

/// Reordering, automatic indexes (partial with a single-table term,
/// or a constant one), Bloom filters, and the LEFT / CROSS JOIN
/// constraints.
#[test]
fn curated_plans_and_rows() {
    same(
        "curated",
        SCHEMA,
        &[
            // The rowid lookup puts `a` second.
            "SELECT * FROM a, b WHERE a.x = b.p",
            "SELECT a.z, b.r FROM b, a WHERE b.q = a.x AND b.r > 2",
            // An automatic index (and its Bloom filter).
            "SELECT * FROM a, c WHERE c.t = a.y",
            "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y",
            // ... partial: a term on the indexed table alone.
            "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y WHERE c.u > 3",
            "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y WHERE 1",
            // ... over several columns.
            "SELECT * FROM a, c WHERE c.t = a.y AND c.s = a.x",
            // A LEFT JOIN's right side runs after its left side.
            "SELECT * FROM c LEFT JOIN a ON a.x = c.s",
            "SELECT * FROM a LEFT JOIN c ON c.t = a.x",
            // ... unless the WHERE clause makes it an inner join.
            "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE a.y = 3",
            "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE a.y IS NOT NULL AND c.s = 1",
            "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE c.s = 1 AND a.y IS NOT NULL",
            // CROSS JOIN keeps its order.
            "SELECT * FROM b CROSS JOIN a WHERE a.x = b.q",
            "SELECT * FROM b CROSS JOIN c, a WHERE a.x = b.q AND c.s = a.y",
            // `x IS NOT NULL` on a NOT NULL column is the constant 1.
            "SELECT c.s, a.z FROM c, a WHERE a.x = c.t AND c.u IS NOT NULL",
            // Constant propagation.
            "SELECT * FROM a, b WHERE a.x = b.q AND b.q = 4",
            // WITHOUT ROWID.
            "SELECT d.v, a.z FROM a, d WHERE d.k = a.y",
            // ORDER BY / GROUP BY / DISTINCT.
            "SELECT a.x, b.r FROM a, b WHERE b.q = a.x ORDER BY a.x DESC",
            "SELECT b.q, count(*) FROM a, b WHERE b.q = a.x GROUP BY b.q",
            "SELECT DISTINCT a.y, c.s FROM a, c WHERE c.t = a.x",
            "SELECT * FROM a, b, c WHERE b.q = a.x AND c.t = b.p LIMIT 5",
            "SELECT count(*) FROM a, b, c, d WHERE b.q = a.x AND c.t = b.p AND d.k = c.s",
        ],
    );
}

/// With `sqlite_stat1` (the pin's ANALYZE, and hipp's in the same
/// connection, read back at once): row estimates, the Bloom filter on
/// a lookup that runs more often than its table has rows, pulled
/// down to the first loop it can be built at.
#[test]
fn statistics_and_bloom_filters() {
    let schema = format!("{SCHEMA} CREATE INDEX cs ON c(s); ANALYZE;");
    same(
        "stat1",
        &schema,
        &[
            "SELECT * FROM b, c WHERE c.s = b.p AND c.t > 2",
            "SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1",
            "SELECT b.r, c.u FROM a, b, c WHERE b.q = a.y AND c.s = a.y AND c.t > 1 AND b.r > 1",
            "SELECT * FROM a, c WHERE c.t = a.y",
        ],
    );
    // hipp's ANALYZE, then the plans in the same connection.
    let path = fresh("stat1-hipp", SCHEMA);
    let q = "SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1";
    let before = pin(&path, &format!("EXPLAIN QUERY PLAN {q}"));
    let after_hipp = block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut st = db.prepare("ANALYZE").await.expect("prepare");
        while st.step().await.expect("step") == Step::Row {}
        drop(st);
        eqp_of(&mut db, q).await
    });
    let after_pin = pin(&path, &format!("EXPLAIN QUERY PLAN {q}"));
    assert_eq!(after_hipp, after_pin, "{q}");
    assert_ne!(before, after_pin, "the statistics change the plan: {q}");
}

/// MATCH and table-valued arguments reading a table listed later in
/// FROM: the pin runs that table first; hipp refused these before
/// v0.17.
#[test]
fn virtual_tables_reordered() {
    same(
        "vtab",
        "CREATE TABLE t(n INT, j TEXT); INSERT INTO t VALUES(2,'[1,2]'),(3,'[5]'),(0,'{\"a\":1}'); \
         CREATE VIRTUAL TABLE f USING fts5(x); \
         INSERT INTO f VALUES('a b'),('b c'),('c d'),('d e'); \
         CREATE TABLE m(q TEXT); INSERT INTO m VALUES('b'),('c'),('e');",
        &[
            "SELECT x, q FROM f, m WHERE f MATCH m.q",
            "SELECT x, q FROM f JOIN m ON f MATCH m.q",
            "SELECT x, q FROM m, f WHERE f MATCH m.q",
            "SELECT f.rowid, q FROM f, m WHERE f MATCH m.q AND f.rowid > 1",
            "SELECT value, n FROM generate_series(1, t.n), t",
            "SELECT value, n FROM generate_series(1, t.n) JOIN t ON t.n > 1",
            "SELECT key, value, n FROM json_each(t.j), t",
            "SELECT key, n FROM json_each(t.j) JOIN t ON t.n >= 2",
            "SELECT x, value FROM f, json_each('[\"b\",\"d\"]') WHERE f MATCH value",
        ],
    );
}

/// Bugs the unit fixed, with data.
#[test]
fn regressions() {
    // A commuted join term keeps its comparison's collation: the
    // automatic index on `b.k` compares NOCASE (rows were lost).
    same(
        "coll",
        "CREATE TABLE a(k TEXT COLLATE NOCASE, v); CREATE TABLE b(k TEXT, w); \
         INSERT INTO a VALUES ('x',1),('Y',2),('z',3); \
         INSERT INTO b VALUES ('X',10),('y',20),('Z ',30)",
        &[
            "SELECT * FROM a JOIN b USING (k) ORDER BY v",
            "SELECT * FROM a, b WHERE a.k = b.k",
            "SELECT * FROM a, b WHERE b.k = a.k",
        ],
    );
    // A top-level `IS NOT NULL` that is not the first AND term does
    // not make a LEFT JOIN inner (`sqlite3ExprImpliesNonNullRow`).
    same(
        "leftnn",
        "CREATE TABLE p(a INTEGER PRIMARY KEY, b); CREATE TABLE q(a, c); \
         INSERT INTO p VALUES (1,1),(2,2),(6,3); INSERT INTO q VALUES (6,1),(6,NULL),(9,9)",
        &[
            "SELECT * FROM p LEFT JOIN q ON q.a = p.a WHERE p.a = 6 AND q.c IS NOT NULL",
            "SELECT * FROM p LEFT JOIN q ON q.a = p.a WHERE q.c IS NOT NULL AND p.a = 6",
        ],
    );
    // A JSON table-valued function reading a table after it across a
    // RIGHT JOIN cannot be reordered: both engines refuse or agree.
    let path = fresh(
        "tvf-err",
        "CREATE TABLE t(j); INSERT INTO t VALUES('[1]'); CREATE TABLE u(x); INSERT INTO u VALUES(1);",
    );
    let q = "SELECT * FROM json_each(t.j) RIGHT JOIN t ON 1";
    let h = hipp_answers(&path, &[q.to_string()]);
    let p = pin(&path, q);
    assert_eq!(h[0].0.is_ok(), p.is_ok(), "{q}: {:?} vs {p:?}", h[0].0);
}

/// An automatic index turns a 2,000 x 2,000 equi-join from a nested
/// loop of 4,000,000 comparisons into 2,000 probes: the planned join
/// must be far faster than the same join with automatic indexes
/// ruled out (`NOT INDEXED`), and give the same answer as the pin.
#[test]
fn automatic_index_is_faster() {
    let path = fresh(
        "perf",
        "CREATE TABLE a(x, y); CREATE TABLE b(x, z); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000) \
         INSERT INTO a SELECT i, i%10 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000) \
         INSERT INTO b SELECT (i*7)%2000, i FROM n;",
    );
    let fast_q = "SELECT count(*), sum(b.z) FROM a, b WHERE b.x = a.x";
    let slow_q = "SELECT count(*), sum(b.z) FROM a NOT INDEXED, b NOT INDEXED WHERE b.x = a.x";
    let time = |q: &str| {
        let t0 = std::time::Instant::now();
        let r = hipp_answers(&path, &[q.to_string()]);
        (t0.elapsed(), r)
    };
    let (fast, rf) = time(fast_q);
    let (slow, rs) = time(slow_q);
    let want = pin(&path, fast_q).expect("pin");
    assert_eq!(rf[0].0.as_deref().ok(), Some(want.as_bytes()));
    assert_eq!(rs[0].0.as_deref().ok(), Some(want.as_bytes()));
    assert!(
        rf[0]
            .1
            .as_deref()
            .is_ok_and(|p| p.contains("AUTOMATIC COVERING INDEX")),
        "{:?}",
        rf[0].1
    );
    assert!(
        fast * 10 < slow,
        "automatic index {fast:?} vs nested loop {slow:?}"
    );
}
