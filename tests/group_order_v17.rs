//! GROUP BY with ORDER BY and LIMIT, differentially vs the pinned
//! oracle (SQLite 3.54.1): v0.17 review fix of a silent wrong answer
//! in every shipped profile (found reviewing the VM unit `vm_joins`).
//!
//! The pin emits a grouped query's groups as it finalizes them, and
//! stops at OFFSET + LIMIT, only when they come out in the ORDER BY's
//! order: its ORDER BY is the GROUP BY (`orderByGrp`: the same terms,
//! either direction, the default NULLs placement) and the groups are
//! sorted in that order — by its group sorter, or by an index walked
//! in the ORDER BY's directions (`WHERE_SORTBYGROUP`: an ascending
//! index read backwards for `ORDER BY b DESC`, so each group's rows
//! come in reverse too). hipp capped the groups whenever the ORDER BY
//! named the GROUP BY terms, so `ORDER BY b NULLS LAST LIMIT 2` kept
//! the NULL group, and `ORDER BY b DESC LIMIT 2` over an index on `b`
//! kept the two smallest groups.
//!
//! Each query runs on hipp and on the pin over copies of one file and
//! must give the same rows in the same order (those before an error
//! included) and the same success. With the hidden `vm` feature the
//! corpus also runs on the VM (strict: no fallback) and on today's
//! executor (`Config::vm_off`).

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
                Ok(Step::Done) => {
                    if sql.starts_with("EXPLAIN QUERY PLAN") {
                        rows = eqp_tree(&rows);
                    }
                    return (Run { ok: true, rows }, vm);
                }
                Err(_) => return (Run { ok: false, rows }, vm),
            }
        }
    })
}

/// The pin shell's rendering of EXPLAIN QUERY PLAN rows
/// (`id|parent|notused|detail`) as a tree.
fn eqp_tree(rows: &[String]) -> Vec<String> {
    fn level(rows: &[(i64, i64, String)], parent: i64, prefix: &mut String, out: &mut Vec<String>) {
        let kids: Vec<&(i64, i64, String)> = rows.iter().filter(|r| r.1 == parent).collect();
        for (i, r) in kids.iter().enumerate() {
            let more = i + 1 < kids.len();
            out.push(format!(
                "{prefix}{}{}",
                if more { "|--" } else { "`--" },
                r.2
            ));
            let n = prefix.len();
            prefix.push_str(if more { "|  " } else { "   " });
            level(rows, r.0, prefix, out);
            prefix.truncate(n);
        }
    }
    let parsed: Vec<(i64, i64, String)> = rows
        .iter()
        .map(|r| {
            let mut f = r.splitn(4, '|');
            let id = f.next().and_then(|x| x.parse().ok()).unwrap_or(-1);
            let parent = f.next().and_then(|x| x.parse().ok()).unwrap_or(-1);
            let _ = f.next();
            (id, parent, f.next().unwrap_or("").to_string())
        })
        .collect();
    let mut out = vec!["QUERY PLAN".to_string()];
    level(&parsed, 0, &mut String::new(), &mut out);
    out
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
    let base = common::temp_db(&format!("gord17-{tag}-base"));
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
fn engines() -> Vec<(&'static str, Config, bool)> {
    #[allow(unused_mut)]
    let mut v = vec![("default", Config::default(), false)];
    #[cfg(feature = "vm")]
    {
        v.push(("vm", Config::default().vm_strict(true), true));
        v.push(("executor", Config::default().vm_off(true), false));
    }
    v
}

/// Every difference between hipp (each engine) and the pin over `qs`.
fn diffs(base: &Path, tag: &str, qs: &[String]) -> Vec<String> {
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let o = copy_of(base, &format!("gord17-{tag}{i}-o"));
        let want = pin_run(&o, q);
        for (name, cfg, must_vm) in engines() {
            let h = copy_of(base, &format!("gord17-{tag}{i}-{name}"));
            let (got, on_vm) = hipp_run(&h, cfg, q);
            // (EXPLAIN runs on today's executor until unit vm_rest.)
            if must_vm && !on_vm && !q.starts_with("EXPLAIN") {
                bad.push(format!("[{name}] not on the VM: {q}"));
            } else if got != want {
                bad.push(format!("[{name}] {q}\n  pin : {want:?}\n  hipp: {got:?}"));
            }
        }
    }
    bad
}

fn corpus(setup: &str, tag: &str, qs: &[&str]) {
    let base = pin_file(tag, setup);
    let qs: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs(&base, tag, &qs);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The queries over one table, each run without an index, with an
/// ascending one, a descending one and a two-column one.
const SHAPES: &[&str] = &[
    // The review's repros: the other NULLs placement, and DESC over
    // an ascending index.
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b NULLS LAST LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC NULLS FIRST LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b ASC NULLS FIRST LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC NULLS LAST LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b LIMIT 2 OFFSET 1",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC LIMIT 3 OFFSET 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b NULLS LAST LIMIT 1 OFFSET 3",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY 1 DESC LIMIT 2",
    "SELECT b AS k, count(*) FROM t GROUP BY b ORDER BY k DESC LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b COLLATE NOCASE LIMIT 3",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY b COLLATE NOCASE DESC LIMIT 3",
    "SELECT b, count(*) FROM t GROUP BY b COLLATE NOCASE ORDER BY b LIMIT 3",
    "SELECT b, count(*) FROM t GROUP BY b COLLATE NOCASE ORDER BY b COLLATE NOCASE DESC LIMIT 3",
    // Each group's rows in the walk's order (an index read
    // backwards for DESC).
    "SELECT b, count(*), group_concat(a) FROM t GROUP BY b ORDER BY b DESC",
    "SELECT b, count(*), group_concat(a) FROM t GROUP BY b ORDER BY b",
    "SELECT b, group_concat(a) FROM t GROUP BY b ORDER BY b DESC NULLS FIRST",
    "SELECT b, group_concat(a) FROM t WHERE b > 1 GROUP BY b ORDER BY b DESC LIMIT 2",
    "SELECT b, group_concat(a) FROM t WHERE b IN (1, 'b', 3) GROUP BY b ORDER BY b DESC",
    // Two terms: directions mixed or not.
    "SELECT b, c, count(*), group_concat(a) FROM t GROUP BY b, c ORDER BY b DESC, c LIMIT 3",
    "SELECT b, c, count(*), group_concat(a) FROM t GROUP BY b, c ORDER BY b, c DESC LIMIT 3",
    "SELECT b, c, count(*), group_concat(a) FROM t GROUP BY b, c ORDER BY b DESC, c DESC LIMIT 3",
    "SELECT b, c, count(*), group_concat(a) FROM t GROUP BY b, c ORDER BY b, c LIMIT 3",
    "SELECT b, c, count(*) FROM t GROUP BY b, c ORDER BY b NULLS LAST, c LIMIT 3",
    // Not the GROUP BY: every group sorted.
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*) DESC, b LIMIT 2",
    "SELECT b, count(*) FROM t GROUP BY b ORDER BY c LIMIT 2",
    // The rowid: read backwards.
    "SELECT a, b FROM t GROUP BY a ORDER BY a DESC LIMIT 3",
    "SELECT a, count(*) FROM t WHERE a > 2 GROUP BY a ORDER BY a DESC LIMIT 2",
    // EXPLAIN QUERY PLAN: the ORDER BY sorter only where the groups
    // do not come in its order.
    "EXPLAIN QUERY PLAN SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC LIMIT 2",
    "EXPLAIN QUERY PLAN SELECT b, count(*) FROM t GROUP BY b ORDER BY b NULLS LAST LIMIT 2",
    "EXPLAIN QUERY PLAN SELECT b, c, count(*) FROM t GROUP BY b, c ORDER BY b DESC, c",
    "EXPLAIN QUERY PLAN SELECT b, c, count(*) FROM t GROUP BY b, c ORDER BY b, c DESC",
    "EXPLAIN QUERY PLAN SELECT b, c, count(*) FROM t GROUP BY b, c ORDER BY b DESC, c DESC",
    "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t GROUP BY a ORDER BY a DESC LIMIT 3",
];

const T: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); \
    INSERT INTO t VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 3, 'z'), (4, 1, 'w'), (5, NULL, 'n'), \
      (6, 'B', 'q'), (7, 'b', 'r'), (8, 2, 'x'), (9, 'a', NULL), (10, NULL, 'm'), (11, 3, 'y');";

#[test]
fn group_order_and_limit_match_the_pin() {
    for (tag, idx) in [
        ("plain", ""),
        ("asc", "CREATE INDEX tb ON t(b);"),
        ("desc", "CREATE INDEX tbd ON t(b DESC);"),
        ("two", "CREATE INDEX tbc ON t(b, c);"),
        ("twod", "CREATE INDEX tbc ON t(b DESC, c);"),
        ("nocase", "CREATE INDEX tbn ON t(b COLLATE NOCASE);"),
    ] {
        corpus(&format!("{T} {idx}"), tag, SHAPES);
    }
}

/// Which groups finalize (an erroring aggregate fails the statement):
/// the pin stops after the LIMIT's groups only where they come in the
/// ORDER BY's order — an ascending index read backwards for DESC too.
#[test]
fn later_groups_finalize_only_when_sorted_again() {
    const G: &str = "CREATE TABLE g(a INTEGER PRIMARY KEY, k, v); \
        INSERT INTO g VALUES (1, 1, 1), (2, 2, 9223372036854775807), (3, 2, 1), (4, 3, 1);";
    const Q: &[&str] = &[
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k DESC LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k NULLS LAST LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k DESC NULLS FIRST LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k COLLATE NOCASE LIMIT 1",
        "SELECT k, sum(v) FROM g WHERE k > 0 GROUP BY k ORDER BY k DESC LIMIT 1",
        "SELECT x.k, sum(x.v) FROM g x JOIN g y ON x.a = y.a GROUP BY x.k ORDER BY x.k DESC LIMIT 1",
        "SELECT x.k, sum(x.v) FROM g x JOIN g y ON x.a = y.a GROUP BY x.k ORDER BY x.k LIMIT 1",
    ];
    corpus(G, "fin", Q);
    corpus(&format!("{G} CREATE INDEX gk ON g(k);"), "fini", Q);
    corpus(&format!("{G} CREATE INDEX gkd ON g(k DESC);"), "finid", Q);
}

/// A join whose first loop delivers the GROUP BY: read backwards for
/// a DESC ORDER BY that repeats it.
#[test]
fn joined_groups_match_the_pin() {
    const J: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); \
        INSERT INTO t VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 3, 'z'), (4, 1, 'w'), (5, NULL, 'n'), \
          (6, 'b', 'q'), (7, 2, 'r'); \
        CREATE TABLE u(k, v); INSERT INTO u VALUES (1, 'p'), (2, 'q'), (3, 'r'), (7, 's'), (6, 't'); \
        CREATE INDEX tb ON t(b);";
    corpus(
        J,
        "join",
        &[
            "SELECT t.b, count(*), group_concat(u.v) FROM t JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b DESC LIMIT 2",
            "SELECT t.b, count(*), group_concat(u.v) FROM t JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b LIMIT 2",
            "SELECT t.b, count(*) FROM t JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b NULLS LAST LIMIT 2",
            "SELECT t.b, count(u.v) FROM t LEFT JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b DESC LIMIT 3",
            "EXPLAIN QUERY PLAN SELECT t.b, count(*) FROM t JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b DESC LIMIT 2",
            "EXPLAIN QUERY PLAN SELECT t.b, count(*) FROM t JOIN u ON u.k = t.a GROUP BY t.b ORDER BY t.b NULLS LAST",
        ],
    );
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

/// A file: `t(a, b, c, e)` with a few rows and up to two indexes.
fn gen_setup(r: &mut Rng) -> String {
    const VALS: &[&str] = &["NULL", "1", "2", "3", "-1", "'a'", "'b'", "'c'", "'B'"];
    let mut s = String::from("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, e);");
    for _ in 0..r.n(14) {
        s += &format!(
            "INSERT INTO t(b, c, e) VALUES ({}, {}, {});",
            r.pick(VALS),
            r.pick(VALS),
            r.n(4)
        );
    }
    for _ in 0..r.n(3) {
        s += r.pick(&[
            "CREATE INDEX IF NOT EXISTS tb ON t(b);",
            "CREATE INDEX IF NOT EXISTS tbd ON t(b DESC);",
            "CREATE INDEX IF NOT EXISTS tbc ON t(b, c);",
            "CREATE INDEX IF NOT EXISTS tbcd ON t(b DESC, c DESC);",
            "CREATE INDEX IF NOT EXISTS tc ON t(c);",
        ]);
    }
    s
}

/// A grouped query whose ORDER BY sorts its groups completely: the
/// GROUP BY terms in some directions and NULLs placements.
fn gen_query(r: &mut Rng) -> String {
    let g: &[&str] = match r.n(4) {
        0 => &["b"],
        1 => &["c"],
        2 => &["b", "c"],
        _ => &["c", "b"],
    };
    let mut ob = Vec::new();
    for t in g {
        let mut s = t.to_string();
        s += r.pick(&["", " ASC", " DESC"]);
        if r.chance(25) {
            s += r.pick(&[" NULLS FIRST", " NULLS LAST"]);
        }
        ob.push(s);
    }
    let agg = r.pick(&[
        "count(*)",
        "group_concat(a)",
        "sum(e)",
        "max(a)",
        "total(e)",
    ]);
    let wh = r.pick(&[
        "",
        "",
        " WHERE b IS NOT NULL",
        " WHERE b > 1",
        " WHERE e < 3",
    ]);
    let mut q = format!(
        "SELECT {}, {agg} FROM t{wh} GROUP BY {} ORDER BY {}",
        g.join(", "),
        g.join(", "),
        ob.join(", ")
    );
    if r.chance(75) {
        q += &format!(" LIMIT {}", r.n(4));
        if r.chance(30) {
            q += &format!(" OFFSET {}", r.n(3));
        }
    }
    q
}

/// Generated grouped queries over generated files: the pin's rows,
/// in its order.
#[test]
fn generated_group_orders_match_the_pin() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut bad = Vec::new();
    for f in 0..24 {
        let setup = gen_setup(&mut r);
        let base = pin_file(&format!("gen{f}"), &setup);
        let qs: Vec<String> = (0..10).map(|_| gen_query(&mut r)).collect();
        for d in diffs(&base, &format!("g{f}-"), &qs) {
            bad.push(format!("{setup}\n{d}"));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}
