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
//!   terms, OR terms and IN subqueries (v0.17, `multi_index`); ORDER
//!   BY, GROUP BY, DISTINCT, min() / max() and LIMIT. The rows must be
//!   the pin's (none may differ as a set, a LIMIT's checked without
//!   it), in the pin's order (at most 3 in 200 queries may come in
//!   another, 0.94% measured over 1,600), and the plans the pin's (at
//!   most 9 in 100 may differ, 7.9% measured, most of them 4- to
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
use std::time::Duration;

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
        // (v0.17, `multi_index`: OR terms and IN subqueries too.)
        conds.push(match r.n(9) {
            0 => format!("{al}.{c}={v}"),
            1 => format!("{al}.{c}>{v}"),
            2 => format!("{al}.{c}<{v}"),
            3 => format!("{al}.{c} IS NOT NULL"),
            4 => format!("{al}.{c} IN (1,2,{v})"),
            5 => {
                let d = &t.cols[r.n(t.cols.len() as u64) as usize];
                format!("({al}.{c}={v} OR {al}.{d}={})", r.n(7))
            }
            6 => {
                let d = &t.cols[r.n(t.cols.len() as u64) as usize];
                let (pa, pt) = &aliases[r.n(aliases.len() as u64) as usize];
                let pc = &pt.cols[r.n(pt.cols.len() as u64) as usize];
                format!("({al}.{c}={pa}.{pc} OR {al}.{d}={})", r.n(7))
            }
            7 => format!("{al}.{c} IN (SELECT a FROM t1 WHERE b>{v})"),
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
    // (v0.17, `multi_index`: a min() / max() over the join.)
    if mode >= 92 && sel[0] != "*" {
        let a = sel[r.n(sel.len() as u64) as usize].clone();
        let f = if r.chance(50) { "min" } else { "max" };
        sel = vec![format!("{f}({a})"), sel[0].clone()];
    } else if mode < 15 && sel[0] != "*" {
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
    // (v0.17, `multi_index`: OR terms, IN subqueries and min() / max()
    // in the corpus; 15 orders and 126 plans of 1,600 at 400 seeds, 3
    // and 20 of 240 at the default 60 — the limits were 2 and 10 in
    // 100.)
    assert!(
        order_bad.len() * 200 <= n * 3,
        "{} of {n} row orders differ:\n{}",
        order_bad.len(),
        order_bad.join("\n")
    );
    assert!(
        plan_bad.len() * 100 <= n * 9,
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

// ------------------------------------------------- review regressions

/// hipp's output of `script`'s statements (`;`-separated, none with a
/// `;` inside) run in one connection on `path`, rendered as the pin's
/// shell renders them (rows in list mode, EXPLAIN QUERY PLAN trees).
fn hipp_script(path: &Path, script: &str) -> String {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut out = String::new();
        for stmt in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(q) = stmt.strip_prefix("EXPLAIN QUERY PLAN ") {
                out.push_str(&eqp_of(&mut db, q).await.expect(stmt));
            } else {
                let rows = collect(&mut db, stmt).await.expect(stmt);
                out.push_str(&String::from_utf8_lossy(&rows));
            }
        }
        out
    })
}

/// `script` in one connection of each engine, on twin files with
/// `schema`: the same output (the statements must not fail).
fn same_session(tag: &str, schema: &str, script: &str) {
    let h = fresh(&format!("{tag}-h"), schema);
    let o = fresh(&format!("{tag}-o"), schema);
    let want = pin(&o, script).expect("pin runs the script");
    assert_eq!(hipp_script(&h, script), want, "script:\n{script}");
}

/// A GROUP BY a join's loops deliver in another column order: the
/// groups come in loop order, as the pin emits them (no group sorter;
/// hipp used to sort them by the written key while its plan said it
/// did not), for a join and for one table read in rowid order.
#[test]
fn group_by_in_loop_order() {
    same(
        "grp",
        "CREATE TABLE t1(id INTEGER PRIMARY KEY, a, b); CREATE INDEX t1ba ON t1(b, a); \
         INSERT INTO t1 VALUES(1,1,2),(2,2,1),(3,1,1),(4,2,2),(5,3,0); \
         CREATE TABLE t0(id INTEGER PRIMARY KEY, v); INSERT INTO t0 VALUES(1,1),(2,2),(3,3); \
         CREATE TABLE t2(id INTEGER PRIMARY KEY, c0, c2); \
         INSERT INTO t2 VALUES(1,5,1),(2,4,2),(3,3,3),(4,5,4); \
         CREATE TABLE t3(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID; \
         INSERT INTO t3 VALUES(1,2,3),(2,1,3),(1,1,4),(3,0,0);",
        &[
            "SELECT t1.a, t1.b, count(*) FROM t1, t0 WHERE t0.id = t1.a GROUP BY t1.a, t1.b",
            "SELECT t2.c0, t2.id, count(*) FROM t2 JOIN t0 ON t0.id = t2.id GROUP BY t2.c0, t2.id",
            "SELECT t2.c0, t2.id, count(*) FROM t2 GROUP BY t2.c0, t2.id",
            "SELECT c0, id, c2, count(*) FROM t2 GROUP BY c2, c0, id",
            "SELECT b, a, count(*) FROM t3 GROUP BY b, a",
            "SELECT t3.b, t3.a, count(*) FROM t3, t2 WHERE t2.id = t3.a GROUP BY t3.b, t3.a",
            "SELECT t1.b, t1.a, sum(t0.v) FROM t1, t0 WHERE t0.id = t1.a GROUP BY t1.a, t1.b \
             HAVING count(*) > 0",
            "SELECT t1.a, t1.b, count(*) FROM t1, t0 WHERE t0.id = t1.a GROUP BY t1.a, t1.b \
             ORDER BY 3, 1",
        ],
    );
}

/// likely() / unlikely() / likelihood() and COLLATE around a WHERE
/// term are removed before the term is analysed (the pin's
/// `whereClauseInsert`), so the term still drives an index, a rowid
/// lookup or an automatic index; their probability replaces the
/// planner's estimate for the term (`truthProb`), its commuted copy
/// and BETWEEN halves included.
#[test]
fn likelihood_wrapped_terms() {
    let schema = "CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); \
         CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v NOT NULL DEFAULT 0, w); \
         CREATE INDEX bax ON b(ax); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<20) \
         INSERT INTO a SELECT i, i%6, i%4 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<15) \
         INSERT INTO b SELECT i, i%9, i%3, CASE WHEN i%2 THEN NULL ELSE i END FROM n; \
         CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, c); CREATE INDEX ta ON t(a); \
         CREATE INDEX tb ON t(b); CREATE INDEX tbc ON t(b, c); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<50) \
         INSERT INTO t SELECT i, i%7, i%5, i%3 FROM n;";
    let qs = [
        "SELECT * FROM b WHERE likely(ax = 3)",
        "SELECT * FROM b WHERE unlikely(ax = 3)",
        "SELECT * FROM b WHERE (ax = 3) COLLATE NOCASE",
        "SELECT * FROM b WHERE likelihood(ax = 3, 0.5)",
        "SELECT a.id, b.id FROM a, b WHERE likely(b.ax = a.x)",
        "SELECT a.id, b.id FROM b, a WHERE likely(a.id = b.ax)",
        "SELECT a.id, b.id FROM a, b WHERE (b.ax = a.x) COLLATE NOCASE",
        "SELECT a.id, b.id FROM a, b WHERE unlikely(b.ax = a.x)",
        "SELECT a.id, b.id FROM a, b WHERE likelihood(b.ax = a.x, 0.9)",
        "SELECT a.id, b.id FROM a JOIN b ON likely(b.ax = a.x)",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON likely(b.ax = a.x)",
        "SELECT * FROM t WHERE unlikely(a = 1) AND b = 2",
        "SELECT * FROM t WHERE a = 1 AND unlikely(b = 2)",
        "SELECT * FROM t WHERE likelihood(a = 1, 0.0) AND b = 2",
        "SELECT * FROM t WHERE likelihood(a = 1, 1.0) AND b = 2",
        "SELECT * FROM t WHERE a > 1 AND unlikely(b > 2)",
        "SELECT * FROM t WHERE likelihood(a BETWEEN 1 AND 3, 0.01) AND b > 2",
        "SELECT * FROM t WHERE unlikely(id = 3) AND a = 1",
        "SELECT * FROM t WHERE unlikely(a IN (1, 2)) AND b = 1",
        "SELECT * FROM t WHERE unlikely(a IS NULL) AND b = 1",
        "SELECT * FROM t WHERE likely(a = 1 AND b = 2)",
        "SELECT * FROM t WHERE unlikely(a = 1 OR a = 2) AND b = 1",
        "SELECT count(*) FROM t WHERE unlikely(a = 1) AND b = 2",
        "SELECT likely(1), unlikely(2), likelihood(3, 0.5), typeof(likely('x'))",
    ];
    same("likely", schema, &qs);
    same(
        "likely-stat",
        &format!(
            "{schema} ANALYZE; CREATE TABLE u(id INTEGER PRIMARY KEY, x, y); \
             CREATE INDEX ux ON u(x); \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<30) \
             INSERT INTO u SELECT i, i%4, i%9 FROM n;"
        ),
        &[
            "SELECT * FROM t WHERE unlikely(a = 1) AND b = 2",
            "SELECT * FROM t WHERE a = 1 AND likely(b = 2)",
            "SELECT * FROM t, u WHERE unlikely(u.x = t.a) AND t.b = 1",
            "SELECT * FROM t, u WHERE likely(u.x = t.a) AND t.b = 1",
            "SELECT * FROM t, u WHERE u.x = t.a AND unlikely(t.b = 1)",
            "SELECT * FROM t, u WHERE likelihood(u.y = t.c, 0.5)",
            "SELECT * FROM u, t WHERE u.y = t.c AND likely(t.a = u.x)",
            "SELECT * FROM u, t WHERE likely(t.a = u.x)",
            "SELECT * FROM u, t WHERE u.y = t.c AND likelihood(t.a = u.x, 0.5)",
        ],
    );
}

/// The LEFT JOIN simplification looks through likely() wrappers and
/// takes `x IS DISTINCT FROM NULL` (the pin's NOTNULL) as
/// null-rejecting; `IS NOT DISTINCT FROM NULL` is `IS NULL`.
#[test]
fn left_join_strength_reduction() {
    same(
        "leftlike",
        "CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); \
         CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v NOT NULL DEFAULT 0, w); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<20) \
         INSERT INTO a SELECT i, i%6, i%4 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<15) \
         INSERT INTO b SELECT i, i%9, i%3, CASE WHEN i%2 THEN NULL ELSE i END FROM n;",
        &[
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x WHERE likely(b.w > 0)",
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x WHERE b.w IS DISTINCT FROM NULL",
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x WHERE (b.w > 0) COLLATE NOCASE",
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x \
             WHERE unlikely(b.w > 0) AND a.y = 1",
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x \
             WHERE a.y = 1 AND likely(b.w > 0)",
            "SELECT a.id, b.id, b.w FROM a LEFT JOIN b ON b.ax = a.x \
             WHERE b.w IS NOT DISTINCT FROM NULL",
            "SELECT count(*) FROM b WHERE w IS DISTINCT FROM NULL",
            "SELECT count(*) FROM b WHERE w IS NOT DISTINCT FROM NULL",
            "SELECT 1 IS DISTINCT FROM NULL, NULL IS DISTINCT FROM NULL, NULL IS NOT DISTINCT FROM NULL",
            "SELECT * FROM b WHERE w IS DISTINCT FROM NULL",
        ],
    );
}

/// The statistics held in the connection follow the pin's: ANALYZE
/// reads them all back (a table's estimate is not reset first), a
/// CREATE / DROP keeps the rest (a dropped index's table keeps its
/// estimate; a new index takes its defaults from it), and an ALTER,
/// VACUUM or the rollback of a schema change reads the schema and its
/// statistics again.
#[test]
fn statistics_follow_ddl() {
    let schema = "CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); \
         CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v); CREATE INDEX bax ON b(ax); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300) \
         INSERT INTO a SELECT i, i%6, i%40 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300) \
         INSERT INTO b SELECT i, i%150, i%3 FROM n;";
    let q = "EXPLAIN QUERY PLAN SELECT a.id, b.id FROM a, b WHERE a.y = b.ax; \
             SELECT a.id, b.id FROM a, b WHERE a.y = b.ax LIMIT 5;";
    let steps: &[&[&str]] = &[
        &["ANALYZE", "DROP INDEX bax"],
        &["ANALYZE", "ALTER TABLE b RENAME COLUMN v TO w"],
        &[
            "ANALYZE",
            "DROP INDEX bax",
            "ALTER TABLE b RENAME COLUMN v TO w",
        ],
        &["ANALYZE", "DROP INDEX bax", "CREATE INDEX bax2 ON b(ax)"],
        &["ANALYZE", "DROP INDEX bax", "ALTER TABLE b ADD COLUMN z"],
        &[
            "ANALYZE",
            "DROP INDEX bax",
            "CREATE TABLE zz(q)",
            "DROP TABLE zz",
        ],
        &["BEGIN", "ANALYZE", "ROLLBACK"],
        &["ANALYZE", "BEGIN", "DROP INDEX bax", "ROLLBACK"],
        &["ANALYZE", "DROP INDEX bax", "VACUUM"],
        &["ANALYZE", "DROP INDEX bax", "CREATE VIEW vv AS SELECT 1"],
        &["ANALYZE", "DROP INDEX bax", "ALTER TABLE b DROP COLUMN v"],
        &[
            "ANALYZE",
            "DROP INDEX bax",
            "BEGIN",
            "CREATE TABLE q1(x)",
            "ROLLBACK",
        ],
        &[
            "ANALYZE",
            "DROP INDEX bax",
            "SAVEPOINT s1",
            "CREATE TABLE q1(x)",
            "ROLLBACK TO s1",
            "RELEASE s1",
        ],
        &["ANALYZE", "DROP INDEX bax", "ANALYZE a"],
        &[
            "ANALYZE",
            "UPDATE sqlite_stat1 SET stat = '5 1' WHERE idx = 'bax'",
            "CREATE INDEX bv ON b(v)",
            "ALTER TABLE b RENAME COLUMN v TO w",
        ],
        &[
            "CREATE INDEX bv ON b(v)",
            "ANALYZE b",
            "DROP INDEX bax",
            "INSERT INTO sqlite_stat1 VALUES('b','bax','300 10')",
            "CREATE INDEX bax ON b(ax)",
        ],
    ];
    for (k, st) in steps.iter().enumerate() {
        let mut script = String::new();
        for s in st.iter() {
            script.push_str(s);
            script.push_str("; ");
            script.push_str(q);
            script.push(' ');
        }
        same_session(&format!("ddl{k}"), schema, &script);
    }
    // The stat rows are read as the pin's `analysisLoader` reads them:
    // counts that wrap, a row naming another table's index, one naming
    // no index, a non-digit inside a count, at the next open.
    same(
        "stat-odd",
        &format!(
            "{} CREATE INDEX ay ON a(y); CREATE INDEX bv ON b(v); ANALYZE; \
             UPDATE sqlite_stat1 SET stat = '18446744073709551615 99999999999999999999' \
             WHERE idx = 'bax'; \
             INSERT INTO sqlite_stat1 VALUES('b','ay','1 1'); \
             INSERT INTO sqlite_stat1 VALUES('a',NULL,'7x 3'); \
             INSERT INTO sqlite_stat1 VALUES('b','nosuch','4000'); \
             INSERT INTO sqlite_stat1 VALUES('nosuch','bv','4000');",
            schema
        ),
        &[
            "SELECT a.id, b.id FROM b, a WHERE a.id = b.v AND b.ax = 3",
            "SELECT a.id, b.id FROM b, a WHERE a.y = b.v",
            "SELECT a.id, b.id FROM a, b WHERE a.y = b.ax",
            "SELECT * FROM b WHERE ax = 5 AND v = 2",
            "SELECT * FROM a, b WHERE a.x = b.v AND b.ax = 3",
        ],
    );
}

/// UPDATE ... FROM is planned as the pin plans it: the target and the
/// FROM sources as one join in cost order (automatic index, Bloom
/// filter), each target row updated from the last combination that
/// order gives it — the final table is the pin's when several FROM
/// rows match.
#[test]
fn update_from_is_a_join() {
    let schema = "CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); \
         CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v); CREATE TABLE c(k INTEGER PRIMARY KEY, w); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<30) \
         INSERT INTO a SELECT i, i%6, i%4 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<40) \
         INSERT INTO b SELECT i, i%9, i*7%11 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<10) \
         INSERT INTO c SELECT i, i*3 FROM n;";
    let script = "EXPLAIN QUERY PLAN UPDATE a SET y = b.v FROM b WHERE b.ax = a.x; \
         UPDATE a SET y = b.v FROM b WHERE b.ax = a.x; SELECT * FROM a; \
         EXPLAIN QUERY PLAN UPDATE a SET y = b.v + c.w FROM b, c WHERE b.ax = a.x AND c.k = b.v; \
         UPDATE a SET y = b.v + c.w FROM b, c WHERE b.ax = a.x AND c.k = b.v; SELECT * FROM a; \
         EXPLAIN QUERY PLAN UPDATE a SET y = b.v FROM b WHERE b.id = a.id + 1; \
         UPDATE a SET y = b.v FROM b WHERE b.id = a.id + 1; SELECT * FROM a; \
         UPDATE a SET x = b.ax FROM b JOIN c ON c.k = b.id WHERE a.y = c.w; SELECT * FROM a; \
         UPDATE a SET y = b.v FROM b LEFT JOIN c ON c.k = b.v WHERE b.ax = a.x AND c.w IS NULL; \
         SELECT * FROM a; \
         CREATE INDEX bax ON b(ax); \
         EXPLAIN QUERY PLAN UPDATE a SET y = b.id FROM b WHERE b.ax = a.x; \
         UPDATE a SET y = b.id FROM b WHERE b.ax = a.x AND a.id < 5; SELECT * FROM a; \
         UPDATE a SET y = (SELECT count(*) FROM c WHERE c.k < b.v) FROM b WHERE b.ax = a.x; \
         SELECT * FROM a; \
         UPDATE a SET y = j.value FROM json_each('[1,2,3]') j WHERE j.key = a.x; SELECT * FROM a; \
         UPDATE a AS t SET y = b.v FROM b WHERE b.ax = t.x AND t.id % 2 = 0; SELECT * FROM a; \
         UPDATE a SET y = b.v FROM a AS a2, b WHERE a2.id = a.id + 1 AND b.ax = a2.x; \
         SELECT * FROM a;";
    same_session("updfrom", schema, script);
    // RETURNING shows the rows as updated, in the target's order.
    same_session(
        "updfrom-ret",
        schema,
        "UPDATE a SET y = b.id FROM b WHERE b.ax = a.x RETURNING id, y",
    );
    // An automatic index makes a 2,000 x 2,000 UPDATE ... FROM far
    // faster than the nested loop (`NOT INDEXED` on both rules it out).
    let big = "CREATE TABLE a(id INTEGER PRIMARY KEY, x, y); CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000) \
         INSERT INTO a SELECT i, i, 0 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000) \
         INSERT INTO b SELECT i, 2001-i, i*7%11 FROM n;";
    let time = |tag: &str, q: &str| {
        let path = fresh(tag, big);
        let t0 = std::time::Instant::now();
        let out = hipp_script(&path, &format!("{q}; SELECT sum(y), count(*) FROM a"));
        (t0.elapsed(), out)
    };
    let fast_q = "UPDATE a SET y = b.v FROM b WHERE b.ax = a.x";
    let slow_q = "UPDATE a NOT INDEXED SET y = b.v FROM b NOT INDEXED WHERE b.ax = a.x";
    // The same 2,000-row write without the join (`base`): opening the
    // file and writing the rows cost both runs the same, and in an
    // instrumented (coverage) build that shared cost is most of
    // `fast`, so the comparison is made net of it. Each figure is the
    // best of three interleaved samples, so a run slowed by other tests
    // competing for the CPU does not decide the comparison.
    let (mut fast, mut slow, mut base) = (Duration::MAX, Duration::MAX, Duration::MAX);
    let (mut fo, mut so) = (String::new(), String::new());
    for i in 0..3 {
        let (t, out) = time(&format!("updfrom-fast{i}"), fast_q);
        (fast, fo) = (fast.min(t), out);
        let (t, out) = time(&format!("updfrom-slow{i}"), slow_q);
        (slow, so) = (slow.min(t), out);
        base = base.min(time(&format!("updfrom-base{i}"), "UPDATE a SET y = id % 11").0);
    }
    let o = fresh("updfrom-perf-o", big);
    let want = pin(&o, &format!("{fast_q}; SELECT sum(y), count(*) FROM a")).expect("pin");
    assert_eq!(fo, want);
    assert_eq!(so, want);
    let (net_fast, net_slow) = (fast.saturating_sub(base), slow.saturating_sub(base));
    assert!(
        net_fast * 5 < net_slow,
        "automatic index {fast:?} vs nested loop {slow:?}, both net of the {base:?} write"
    );
}

/// A join of 64 tables (the pin's limit) is planned like a smaller
/// one: a chain of equi-joins runs through automatic indexes, not a
/// 64-deep nested loop.
#[test]
fn join_of_64_tables() {
    let mut schema = String::new();
    for i in 0..64 {
        schema.push_str(&format!(
            "CREATE TABLE t{i}(a INTEGER PRIMARY KEY, b, c); \
             INSERT INTO t{i} VALUES(1,{},{}),(2,{},{}),(3,{},{}); ",
            i % 3,
            (i + 1) % 3,
            (i + 1) % 3,
            i % 3,
            (i + 2) % 3,
            (i * 7) % 3
        ));
    }
    let from: Vec<String> = (0..64).map(|i| format!("t{i}")).collect();
    let terms: Vec<String> = (0..63).map(|i| format!("t{i}.b = t{}.c", i + 1)).collect();
    let q = format!(
        "SELECT count(*) FROM {} WHERE {}",
        from.join(", "),
        terms.join(" AND ")
    );
    let t0 = std::time::Instant::now();
    same("j64", &schema, &[q.as_str()]);
    assert!(t0.elapsed() < Duration::from_secs(60));
}

/// An expression index the statement reads only through its
/// expressions covers it (the pin's `WHERE_EXPRIDX`), and a BETWEEN
/// whose bounds are columns also probes the bounds' table (its
/// halves' commuted copies), with and without statistics.
#[test]
fn expression_covering_and_column_bounds() {
    let schema = "CREATE TABLE w(id INTEGER PRIMARY KEY, v); \
         CREATE TABLE b(id INTEGER PRIMARY KEY, ax, v); CREATE INDEX bexp ON b(v+1); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<200) \
         INSERT INTO w SELECT i, i%7 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300) \
         INSERT INTO b SELECT i, i%5, i%3 FROM n; \
         CREATE TABLE t2(x, y, z); CREATE INDEX t2y ON t2(y); \
         CREATE TABLE t1(a INTEGER PRIMARY KEY, b); \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<50) \
         INSERT INTO t2 SELECT i%7, i%5, i FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<40) \
         INSERT INTO t1 SELECT i, i%6 FROM n;";
    let qs = [
        "SELECT w.id, b.id FROM w, b WHERE b.v+1 = w.v",
        "SELECT b.id FROM b WHERE b.v+1 = 2",
        "SELECT b.id, b.v+1 FROM b WHERE b.v+1 > 1",
        "SELECT b.id, b.v FROM b WHERE b.v+1 = 2",
        "SELECT count(*) FROM b WHERE b.v+1 = 2",
        "SELECT b.v+1 FROM b",
        "SELECT b.v+1 FROM b ORDER BY b.v+1",
        "SELECT * FROM t1, t2 WHERE t1.b BETWEEN t2.y AND t2.x",
        "SELECT * FROM t2 WHERE 5 BETWEEN x AND y",
        "SELECT * FROM t2 WHERE 3 BETWEEN y AND x",
        "SELECT * FROM t1, t2 WHERE t2.y BETWEEN t1.a AND t1.b",
        "SELECT * FROM t1, t2 WHERE t1.a BETWEEN t2.y AND 30",
        "SELECT * FROM t1, t2 WHERE t1.b NOT BETWEEN t2.y AND t2.x",
    ];
    same("xcov", schema, &qs);
    same("xcov-stat", &format!("{schema} ANALYZE;"), &qs);
}

/// A table-valued function's argument that reads a table it may not
/// follow (past a CROSS JOIN) is not given to the call: without its
/// JSON argument json_each has no rows, without its root argument the
/// root is `$`, and the argument's `hidden column = value` term stays
/// a WHERE term — the pin's answers (hipp refused these).
#[test]
fn tvf_arguments_across_cross_join() {
    same(
        "tvfcross",
        "CREATE TABLE t(n INT, j TEXT); INSERT INTO t VALUES(2,'[1,2]'),(3,'[5]'),(9,'$'); \
         CREATE TABLE u(k); INSERT INTO u VALUES(1),(2);",
        &[
            "SELECT count(*) FROM json_each(t.j) CROSS JOIN t",
            "SELECT count(*) FROM json_each(t.j), u CROSS JOIN t",
            "SELECT * FROM json_each(t.j) CROSS JOIN t",
            "SELECT count(*) FROM u CROSS JOIN json_each(t.j) CROSS JOIN t",
            "SELECT count(*) FROM u LEFT JOIN json_each(t.j) ON 1 CROSS JOIN t",
            "SELECT count(*) FROM json_tree(t.j) CROSS JOIN t",
            "SELECT count(*) FROM json_each(t.j, '$') CROSS JOIN t",
            "SELECT * FROM json_each('[1,2]', t.j) CROSS JOIN t",
            "SELECT key, value, n FROM json_each('[1,[2,3]]', t.j) CROSS JOIN t",
            "SELECT count(*) FROM u, json_each(t.j) CROSS JOIN t WHERE u.k = 1",
            "SELECT count(*) FROM json_each(t.j) JOIN t",
        ],
    );
}
