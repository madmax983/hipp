//! An enclosing query's aggregate that a nested query uses as an
//! index or rowid bound, differentially vs the pinned oracle (SQLite
//! 3.54.1): v0.17 review fix (unit `vm_flip`'s item (d)) of a refusal
//! in every profile with subqueries and index planning.
//!
//! The pin gives an aggregate whose arguments read only an enclosing
//! query to that query, even inside a nested aggregate query's WHERE
//! clause (`SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a))
//! FROM t` is one row: the count of `u` rows up to the largest
//! `t.a`). hipp substituted each such aggregate's value into the
//! nested plan's expressions before running it (`subst_owned_aggs`),
//! but not into the plan's access path: with an index on `u.k` (or an
//! INTEGER PRIMARY KEY / rowid term) the planner had copied the term
//! into the index probe or range bound, which still held the
//! aggregate, and the statement failed with `MisusedAggregate`.
//! The walk now covers access paths (probes, IN lists, ranges, the
//! automatic index's keys, MULTI-INDEX OR branches, the min() / max()
//! early-out's condition) and virtual-table constraint arguments, on
//! today's executor and on the VM alike (the VM's placeholders come
//! from the same walk).
//!
//! Each query runs on hipp and on the pin over copies of one file and
//! must give the same rows in the same order and the same success
//! (an error on both counts as agreement). With the hidden `vm`
//! feature the corpus also runs on the VM (strict: no fallback) and
//! on today's executor (`Config::vm_off`).
#![cfg(feature = "sql")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};
use std::path::{Path, PathBuf};

/// One row as the pin's shell prints it in list mode.
fn render<S: hipp::storage::Storage>(q: &hipp::Statement<'_, S>) -> String {
    let mut line = String::new();
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut out = hipp::arena::AVec::new();
                hipp::record::render_real(f, &mut out);
                line.push_str(&String::from_utf8_lossy(out.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
    line
}

/// What one engine made of one query.
#[derive(Debug, PartialEq)]
struct Run {
    ok: bool,
    rows: Vec<String>,
}

/// `sql` on hipp under `cfg`: its run and whether it ran on the VM.
fn hipp_run(path: &Path, cfg: Config, sql: &str) -> (Run, bool) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        let Ok(mut q) = db.prepare(sql).await else {
            let run = Run {
                ok: false,
                rows: Vec::new(),
            };
            return (run, true);
        };
        let mut rows = Vec::new();
        let mut on_vm = None;
        loop {
            let r = q.step().await;
            #[cfg(feature = "vm")]
            let vm = *on_vm.get_or_insert_with(|| q.runs_on_vm());
            #[cfg(not(feature = "vm"))]
            let vm = *on_vm.get_or_insert(false);
            match r {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => return (Run { ok: true, rows }, vm),
                Err(_) => return (Run { ok: false, rows }, vm),
            }
        }
    })
}

/// The pin's run of `sql` (`-bail`: it stops at the first error).
fn pin_run(path: &Path, sql: &str) -> Run {
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    Run {
        ok: out.status.success() && out.stderr.is_empty(),
        rows: String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    }
}

/// A file the pin builds with `setup`.
fn pin_file(tag: &str, setup: &str) -> PathBuf {
    let base = common::temp_db(&format!("oab17-{tag}-base"));
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(&base)
        .arg(setup)
        .output()
        .expect("oracle runs");
    assert!(
        out.status.success(),
        "setup failed: {setup}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    base
}

fn copy_of(base: &Path, tag: &str) -> PathBuf {
    let p = common::temp_db(tag);
    std::fs::copy(base, &p).expect("copy");
    p
}

/// The engines a query runs on: hipp's default; with `vm`, the VM
/// (strict: it must run there) and today's executor.
/// `claimed`: the VM must run every query (strict); otherwise it may
/// fall back to today's executor (the sources the driver reads with
/// today's readers, `vm::gen::sub::host_reads_preset`).
fn engines(claimed: bool) -> Vec<(&'static str, Config, bool)> {
    let _ = claimed;
    #[allow(unused_mut)]
    let mut v = vec![("default", Config::default(), false)];
    #[cfg(feature = "vm")]
    {
        let vm = Config::default().vm_strict(claimed);
        v.push(("vm", vm, claimed));
        v.push(("executor", Config::default().vm_off(true), false));
    }
    v
}

/// Every difference between hipp (each engine) and the pin over `qs`.
fn diffs(base: &Path, tag: &str, qs: &[String], claimed: bool) -> Vec<String> {
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let o = copy_of(base, &format!("oab17-{tag}{i}-o"));
        let want = pin_run(&o, q);
        for (name, cfg, must_vm) in engines(claimed) {
            let h = copy_of(base, &format!("oab17-{tag}{i}-{name}"));
            let (got, on_vm) = hipp_run(&h, cfg, q);
            if must_vm && !on_vm {
                bad.push(format!("[{name}] not on the VM: {q}"));
            } else if got != want {
                bad.push(format!("[{name}] {q}\n  pin : {want:?}\n  hipp: {got:?}"));
            }
        }
    }
    bad
}

fn corpus(setup: &str, tag: &str, qs: &[&str]) {
    corpus_on(setup, tag, qs, true);
}

fn corpus_on(setup: &str, tag: &str, qs: &[&str], claimed: bool) {
    let base = pin_file(tag, setup);
    let qs: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs(&base, tag, &qs, claimed);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

const S: &str = "CREATE TABLE t(a, b TEXT); \
    INSERT INTO t VALUES (1,'x'),(2,'y'),(3,NULL),(NULL,'z'),(2,'Y'); \
    CREATE TABLE u(k INTEGER, v); \
    INSERT INTO u VALUES (1,10),(1,11),(2,20),(NULL,30),(5,50),('2',21),(2.5,25); \
    CREATE TABLE w(id INTEGER PRIMARY KEY, p, q); \
    INSERT INTO w VALUES (1,1,'a'),(2,2,'A'),(3,2,'b'),(4,NULL,'B'),(5,5,'c');";

/// The review's repros and the shapes around them: every place an
/// access path can take the term from (equality, range, BETWEEN, IN
/// list, rowid, a second key column, a join's lookup, an OR branch,
/// the min() / max() early-out), every place the subquery can sit
/// (result column, HAVING, ORDER BY, GROUP BY queries, bare columns
/// beside the aggregate), and the shapes the pin refuses (a nested
/// query that is not an aggregate, an argument-less count(*)).
const SHAPES: &[&str] = &[
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM w WHERE w.id <= max(t.a)) FROM t",
    "SELECT (SELECT sum(v) FROM u WHERE u.k = min(t.a)) FROM t",
    "SELECT count(*) FROM t HAVING (SELECT count(*) FROM u WHERE u.k <= max(t.a)) > 1",
    "SELECT b, (SELECT count(*) FROM u WHERE u.k = max(t.a)) FROM t GROUP BY b ORDER BY 1",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= count(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)), a FROM t",
    "SELECT max(t.a), (SELECT count(*) FROM u WHERE u.k = max(t.a)) FROM t",
    "SELECT min(t.a), (SELECT count(*) FROM u WHERE u.k > min(t.a)), b FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k BETWEEN min(t.a) AND max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k IN (min(t.a), max(t.a))) FROM t",
    "SELECT (SELECT count(*) FROM w WHERE w.id IN (min(t.a), max(t.a), NULL)) FROM t",
    "SELECT (SELECT max(v) FROM u WHERE u.rowid = max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k > avg(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.v > 15 AND u.k < total(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a) AND u.v > 10) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a) + 1) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)) FROM t WHERE a > 1",
    "SELECT (SELECT count(*) FROM u WHERE u.k > max(t.a)) FROM t WHERE 0",
    "SELECT (SELECT count(*) FROM u WHERE u.k < max(t.b)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k = max(t.a) COLLATE NOCASE) FROM t",
    "SELECT (SELECT max(k) FROM u WHERE u.k <= max(t.a)) FROM t",
    "SELECT (SELECT min(k) FROM u WHERE u.k >= min(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k = max(t.a) OR u.v = 50) FROM t",
    "SELECT (SELECT count(*) FROM u, w WHERE w.p = u.k AND u.k <= max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM w WHERE w.p = max(t.a) AND w.q > 'a') FROM t",
    "SELECT (SELECT count(*) FROM w WHERE w.q = 'b' AND w.id < max(t.a) + 2) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k IN (1, max(t.a))) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= (SELECT max(t.a) + 0)) FROM t",
    "SELECT b, (SELECT count(*) FROM u WHERE u.k <= max(t.a) AND u.k >= min(t.a)) FROM t GROUP BY b ORDER BY 1",
    "SELECT (SELECT group_concat(v) FROM u WHERE u.k = max(t.a) ORDER BY v) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)) AS c FROM t ORDER BY c",
    "SELECT a, (SELECT count(*) FROM u WHERE u.k <= max(t.a)) FROM t GROUP BY a ORDER BY 1",
    "SELECT b FROM t GROUP BY b ORDER BY (SELECT count(*) FROM u WHERE u.k < max(t.a)), b",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= (SELECT count(*) FROM w WHERE w.id <= max(t.a))) FROM t",
    "SELECT (SELECT sum(v) FROM u WHERE u.k <= max(t.a)) FROM t WHERE b IS NOT NULL",
    "SELECT (SELECT count(*) FROM u AS u1 JOIN u AS u2 ON u2.k = u1.k WHERE u1.k <= max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u LEFT JOIN w ON w.id = max(t.a)) FROM t",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a) UNION ALL SELECT 7) FROM t",
    "SELECT b, EXISTS (SELECT 1 FROM u WHERE u.k = max(t.a) GROUP BY u.k) FROM t GROUP BY b ORDER BY 1",
    "SELECT b, (SELECT count(*) FROM u WHERE u.k = max(t.a)) IN (1, 2) FROM t GROUP BY b ORDER BY 1",
    "SELECT (SELECT count(*) FROM u WHERE u.k <= max(t.a)) FROM t GROUP BY b HAVING count(*) > 1",
    // The pin refuses these ("misuse of aggregate"): the nested query
    // is not an aggregate, or count(*) reads no column.
    "SELECT (SELECT q FROM w WHERE w.id = min(t.a)) FROM t",
    "SELECT EXISTS (SELECT 1 FROM u WHERE u.k = max(t.a)) FROM t",
    "SELECT b, (SELECT count(*) FROM w WHERE w.id <= count(*)) FROM t GROUP BY b ORDER BY 1",
    "SELECT (SELECT count(*) FROM u WHERE u.k IN (SELECT a FROM t AS t2 WHERE t2.a <= max(t.a))) FROM t",
    "SELECT (SELECT count(*) FROM (SELECT k FROM u WHERE u.k <= max(t.a))) FROM t",
];

/// The index setups each shape runs over: none (the scan the review
/// saw answer correctly), the review's index on `u.k`, a descending
/// one, two-column ones, and indexes on every column a shape bounds.
const INDEXES: &[(&str, &str)] = &[
    ("plain", ""),
    ("uk", "CREATE INDEX u_k ON u(k);"),
    ("ukd", "CREATE INDEX u_k ON u(k DESC);"),
    ("ukv", "CREATE INDEX u_kv ON u(k, v);"),
    (
        "all",
        "CREATE INDEX u_k ON u(k); CREATE INDEX u_v ON u(v); CREATE INDEX w_pq ON w(p, q); \
         CREATE INDEX w_q ON w(q);",
    ),
];

#[test]
fn outer_aggregate_bounds_match_the_pin() {
    for (tag, idx) in INDEXES {
        corpus(&format!("{S} {idx}"), tag, SHAPES);
    }
}

/// Virtual and table-valued sources: an R*Tree constraint, an fts5
/// rowid bound, generate_series' arguments and a json_each filter
/// bounded by an enclosing query's aggregate.
#[test]
fn outer_aggregate_bounds_on_virtual_sources_match_the_pin() {
    #[allow(unused_mut)]
    let mut setup = String::from(S);
    #[allow(unused_mut)]
    let mut qs: Vec<&str> = Vec::new();
    #[cfg(feature = "rtree")]
    {
        setup.push_str(
            " CREATE VIRTUAL TABLE r USING rtree(id, x0, x1); \
              INSERT INTO r VALUES (1,0,1),(2,1,3),(3,2,5);",
        );
        qs.extend([
            "SELECT (SELECT count(*) FROM r WHERE r.x0 <= max(t.a)) FROM t",
            "SELECT (SELECT count(*) FROM r WHERE r.id = max(t.a)) FROM t",
            "SELECT b, (SELECT group_concat(id) FROM r WHERE r.x1 >= min(t.a) AND r.x0 < max(t.a)) FROM t GROUP BY b ORDER BY 1",
        ]);
    }
    #[cfg(feature = "fts5")]
    {
        setup.push_str(
            " CREATE VIRTUAL TABLE f USING fts5(body); \
              INSERT INTO f VALUES ('a b'),('b c'),('c d');",
        );
        qs.extend([
            "SELECT (SELECT count(*) FROM f WHERE f MATCH 'b' AND rowid <= max(t.a)) FROM t",
            "SELECT (SELECT count(*) FROM f WHERE rowid = min(t.a)) FROM t",
        ]);
    }
    #[cfg(feature = "vtab")]
    qs.extend([
        "SELECT (SELECT count(*) FROM generate_series(1, max(t.a))) FROM t",
        "SELECT (SELECT count(*) FROM generate_series(1, 10) WHERE value <= max(t.a)) FROM t",
    ]);
    #[cfg(feature = "json")]
    qs.extend([
        "SELECT (SELECT count(*) FROM json_each('[1,2,3]') WHERE key < max(t.a)) FROM t",
        "SELECT (SELECT group_concat(value) FROM json_each(json_array(1, max(t.a)))) FROM t",
        "SELECT b, (SELECT count(*) FROM json_each(json_array(min(t.a), max(t.a), 9)) WHERE value > count(t.a)) FROM t GROUP BY b ORDER BY 1",
    ]);
    // (The driver reads these sources with today's readers: the VM
    // falls back for them, `host_reads_preset`.)
    corpus_on(&setup, "vt", &qs, false);
}

/// WITHOUT ROWID tables: the clustered key's equality and range
/// bounds, a secondary index on one, and a join of one with a rowid
/// table, bounded by an enclosing query's aggregate.
#[cfg(feature = "without_rowid")]
#[test]
fn outer_aggregate_bounds_on_without_rowid_tables_match_the_pin() {
    let setup = format!(
        "{S} CREATE TABLE x(k, j, v, PRIMARY KEY (k, j)) WITHOUT ROWID; \
         INSERT INTO x VALUES (1,1,'p'),(1,2,'q'),(2,1,'r'),(3,1,'s'),(5,5,'t'); \
         CREATE INDEX x_v ON x(v); CREATE INDEX u_k ON u(k);"
    );
    // (The driver reads a WITHOUT ROWID table's access path with
    // today's readers: the VM falls back, `host_reads_preset`.)
    corpus_on(
        &setup,
        "wr",
        &[
            "SELECT (SELECT count(*) FROM x WHERE x.k <= max(t.a)) FROM t",
            "SELECT (SELECT group_concat(v) FROM x WHERE x.k = max(t.a)) FROM t",
            "SELECT (SELECT count(*) FROM x WHERE x.k = min(t.a) AND x.j >= count(t.a) - 4) FROM t",
            "SELECT (SELECT count(*) FROM x WHERE x.v < max(t.b)) FROM t",
            "SELECT b, (SELECT count(*) FROM x WHERE x.k IN (min(t.a), max(t.a))) FROM t GROUP BY b ORDER BY 1",
            "SELECT (SELECT count(*) FROM u JOIN x ON x.k = u.k WHERE x.k <= max(t.a)) FROM t",
            "SELECT (SELECT count(*) FROM x JOIN u ON u.k = x.k WHERE u.k <= max(t.a)) FROM t",
            "SELECT (SELECT count(*) FROM u LEFT JOIN x ON x.k = max(t.a)) FROM t",
        ],
        false,
    );
}

/// A repeated min() / max() (v0.17 review fix, found by the generated
/// corpus below): the pin's aggregate list holds each aggregate once,
/// whatever query level repeats it, so the LAST distinct min() / max()
/// decides the bare columns; hipp counted the repeat and let `max(a),
/// min(a), max(a), a` read the max() row (3) where the pin reads the
/// min() row (1). A silent wrong answer in every profile with
/// aggregates.
#[test]
fn repeated_min_max_bare_columns_match_the_pin() {
    let mut qs = vec![
        "SELECT max(a), min(a), max(a), a FROM t",
        "SELECT min(a), max(a), min(a), a FROM t",
        "SELECT max(a), min(a), max(+a), a FROM t",
        "SELECT max(b), min(b), max(b), a FROM t",
        "SELECT max(a, 1), min(a), max(a), a FROM t",
        "SELECT max(a), min(a), b FROM t ORDER BY max(a)",
        "SELECT max(a), min(a), b FROM t HAVING max(a) > 0",
        "SELECT max(a), min(a), b FROM t GROUP BY b IS NULL ORDER BY max(a)",
        "SELECT b, max(a), min(a), max(a), a FROM t GROUP BY b ORDER BY 1",
        "SELECT max(a), (SELECT min(t.a) + max(t.a)), a FROM t",
        "SELECT max(a), (SELECT min(t.a)), max(t.a), a FROM t",
        "SELECT max(t.a), (SELECT max(k) FROM u WHERE u.rowid BETWEEN min(t.a) AND max(t.a) - 1), a FROM t WHERE a IS NOT NULL",
        "SELECT b, (SELECT max(k) FROM u WHERE u.k <= max(t.a)), min(t.a), max(t.a), a FROM t GROUP BY b ORDER BY 1",
    ];
    #[cfg(feature = "sql")]
    qs.extend([
        "SELECT max(a), min(a), max(DISTINCT a), a FROM t",
        "SELECT max(a), min(a), max(a) FILTER (WHERE b > 'a'), a FROM t",
        "SELECT max(a COLLATE NOCASE), min(a), max(a COLLATE NOCASE), a FROM t",
    ]);
    #[cfg(feature = "window")]
    qs.push("SELECT max(a) OVER (), min(a), max(a), a FROM t");
    for (tag, idx) in INDEXES {
        corpus(&format!("{S} {idx}"), &format!("rep{tag}"), &qs);
    }
}

// ---------------- generated ----------------

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
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Generated shapes: an aggregate of `t` (with or without DISTINCT,
/// an arithmetic wrapper) compared with an indexed column, the rowid
/// or the INTEGER PRIMARY KEY of a nested aggregate query, under a
/// plain, grouped or HAVING enclosing query, over every index setup.
#[test]
fn generated_outer_aggregate_bounds_match_the_pin() {
    let mut rng = Rng(0x0a6b_17c3_5d2e_9f41);
    let aggs = [
        "max(t.a)",
        "min(t.a)",
        "count(t.a)",
        "count(t.b)",
        "sum(t.a)",
        "avg(t.a)",
        "total(t.a)",
        "max(DISTINCT t.a)",
        "max(t.a) - 1",
        "min(t.a) * 2",
        "max(t.b)",
        "length(max(t.b))",
    ];
    let ops = ["=", "<", "<=", ">", ">=", "IS", "!="];
    let lhs = [
        ("u", "u.k"),
        ("u", "u.rowid"),
        ("u", "u.v"),
        ("w", "w.id"),
        ("w", "w.p"),
        ("w", "w.q"),
    ];
    // (group_concat's order is the loop's: without `expr_index` hipp
    // scans in rowid order where the pin walks an index, a known gap
    // of the profiles without index-aware planning.)
    #[cfg(feature = "expr_index")]
    let (inner, inner_w) = (
        ["count(*)", "sum(v)", "max(k)", "min(k)", "group_concat(v)"],
        ["count(*)", "sum(p)", "max(id)", "min(q)", "group_concat(q)"],
    );
    #[cfg(not(feature = "expr_index"))]
    let (inner, inner_w) = (
        ["count(*)", "sum(v)", "max(k)", "min(k)", "total(v)"],
        ["count(*)", "sum(p)", "max(id)", "min(q)", "total(p)"],
    );
    let mut qs = Vec::new();
    for _ in 0..120 {
        let (tab, col) = lhs[(rng.next() % lhs.len() as u64) as usize];
        let i = if tab == "u" {
            rng.pick(&inner)
        } else {
            rng.pick(&inner_w)
        };
        let cond = format!("{col} {} {}", rng.pick(&ops), rng.pick(&aggs));
        let cond = match rng.next() % 4 {
            0 => format!("{cond} AND {col} IS NOT NULL"),
            1 => format!("{col} BETWEEN {} AND {}", rng.pick(&aggs), rng.pick(&aggs)),
            2 => format!("{col} IN ({}, {})", rng.pick(&aggs), rng.pick(&aggs)),
            _ => cond,
        };
        let sub = format!("(SELECT {i} FROM {tab} WHERE {cond})");
        let q = match rng.next() % 4 {
            0 => format!("SELECT {sub} FROM t"),
            1 => format!("SELECT b, {sub} FROM t GROUP BY b ORDER BY 1"),
            2 => format!("SELECT count(*) FROM t HAVING {sub} IS NOT NULL"),
            _ => format!("SELECT max(t.a), {sub}, a FROM t WHERE a IS NOT NULL"),
        };
        qs.push(q);
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    for (tag, idx) in INDEXES {
        corpus(&format!("{S} {idx}"), &format!("g{tag}"), &refs);
    }
}
