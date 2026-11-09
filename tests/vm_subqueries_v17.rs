//! The bytecode VM (ADR 0005 Stage 2, hidden feature `vm`), unit
//! `vm_subqueries`, differentially vs the pinned oracle (SQLite
//! 3.54.1). Built only with `--features vm`; every SELECT a corpus
//! claims runs under `Config::vm_strict` (falling back to today's
//! executor is an error) and asserts `Statement::runs_on_vm`.
#![cfg(feature = "vm")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};
use std::path::{Path, PathBuf};

type FsError = <FileStorage as hipp::storage::Storage>::Error;

// ---------------- harness ----------------

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

/// What one engine made of one query: whether it succeeded, the rows
/// it returned (before an error included), whether it ran on the VM.
#[derive(Debug, PartialEq)]
struct Run {
    ok: bool,
    rows: Vec<String>,
}

/// `sql` (one SELECT) on hipp under `cfg`: its run, whether it ran on
/// the VM (true for a statement that failed to prepare: nothing ran),
/// and the error.
fn hipp_run(path: &Path, cfg: Config, sql: &str) -> (Run, bool, Option<Error<FsError>>) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        let mut q = match db.prepare(sql).await {
            Ok(q) => q,
            Err(e) => {
                // (Nothing to run: no fallback either.)
                return (
                    Run {
                        ok: false,
                        rows: Vec::new(),
                    },
                    true,
                    Some(e),
                );
            }
        };
        let mut rows = Vec::new();
        let mut first = true;
        let mut on_vm = false;
        loop {
            let r = q.step().await;
            if first {
                on_vm = q.runs_on_vm();
                first = false;
            }
            match r {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => return (Run { ok: true, rows }, on_vm, None),
                Err(e) => return (Run { ok: false, rows }, on_vm, Some(e)),
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

/// A file the pin builds with `setup` (on its standard input: a big
/// setup does not fit an argument).
fn pin_file(tag: &str, setup: &str) -> PathBuf {
    use std::io::Write;
    let base = common::temp_db(&format!("vms17-{tag}-base"));
    let mut child = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(&base)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    if let Some(mut i) = child.stdin.take() {
        i.write_all(setup.as_bytes()).expect("setup written");
    }
    let out = child.wait_with_output().expect("oracle runs");
    assert!(
        out.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    base
}

fn copy_of(base: &Path, tag: &str) -> PathBuf {
    let p = common::temp_db(tag);
    std::fs::copy(base, &p).expect("copy");
    p
}

fn strict() -> Config {
    Config::default().vm_strict(true)
}

/// Each query of `qs`, strict, on copies of `base`: it runs on the
/// VM and gives the pin's rows (those before an error included) and
/// success; every difference is listed.
fn corpus(base: &Path, tag: &str, cfg: &Config, qs: &[&str]) {
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let h = copy_of(base, &format!("vms17-{tag}{i}-h"));
        let o = copy_of(base, &format!("vms17-{tag}{i}-o"));
        let (hr, on_vm, he) = hipp_run(&h, cfg.clone(), q);
        let or = pin_run(&o, q);
        if !on_vm {
            bad.push(format!("not on the VM: {q}  ({he:?})"));
        } else if hr != or {
            bad.push(format!("{q}\n  pin : {or:?}\n  hipp: {hr:?} {he:?}"));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

// ---------------- the corpus ----------------

const S: &str = "CREATE TABLE t(a, b TEXT); \
    INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, NULL), (NULL, 'z'), (2, 'Y'); \
    CREATE TABLE u(k INTEGER, v); \
    INSERT INTO u VALUES (1, 10), (1, 11), (2, 20), (NULL, 30), (5, 50); \
    CREATE TABLE e(x); \
    CREATE TABLE w(id INTEGER PRIMARY KEY, p, q TEXT COLLATE NOCASE); \
    INSERT INTO w VALUES (1, 1, 'a'), (2, 2, 'A'), (3, 2, 'b'), (4, NULL, 'B'); \
    CREATE INDEX u_k ON u(k); \
    CREATE VIEW vt AS SELECT a, b FROM t WHERE a > 1; \
    CREATE VIEW vg AS SELECT k, count(*) AS n, sum(v) AS s FROM u GROUP BY k;";

const SUBQ: &[&str] = &[
    "SELECT a, (SELECT v FROM u WHERE k = a) FROM t;",
    "SELECT a, (SELECT max(v) FROM u) FROM t;",
    "SELECT a FROM t WHERE a IN (SELECT k FROM u);",
    "SELECT a FROM t WHERE a NOT IN (SELECT k FROM u);",
    "SELECT a FROM t WHERE a NOT IN (SELECT k FROM u WHERE k IS NOT NULL);",
    "SELECT a, a IN (SELECT k FROM u), a NOT IN (SELECT k FROM u) FROM t;",
    "SELECT NULL IN (SELECT k FROM e), NULL NOT IN (SELECT x FROM e);",
    "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE k = a);",
    "SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE k = a);",
    "SELECT a, EXISTS (SELECT 1 FROM u WHERE k = a LIMIT 1) FROM t;",
    "SELECT (SELECT k FROM u ORDER BY v DESC);",
    "SELECT (SELECT 2 UNION SELECT 1);",
    "SELECT (SELECT 2 UNION ALL SELECT 1);",
    "SELECT a, (SELECT 1 UNION SELECT 2) FROM t LIMIT 1;",
    "SELECT a FROM t WHERE a IN (SELECT k FROM u UNION SELECT 3);",
    "SELECT a, (SELECT count(*) FROM u WHERE k <= t.a) FROM t ORDER BY 2, 1;",
    "SELECT a, (SELECT group_concat(v) FROM u WHERE k = t.a) FROM t;",
    "SELECT * FROM t WHERE a = (SELECT max(k) FROM u WHERE k < 5);",
    "SELECT * FROM t WHERE a > (SELECT avg(k) FROM u) OR b = 'z';",
    "SELECT * FROM t WHERE b IS NULL OR a IN (SELECT k FROM u WHERE v > 15);",
    "SELECT * FROM t WHERE (SELECT count(*) FROM u WHERE k = t.a) > 1;",
    "SELECT a, (SELECT b FROM t AS t2 WHERE t2.a = t.a + 1) FROM t;",
    "SELECT a, (SELECT (SELECT max(v) FROM u WHERE u.k = t2.a) FROM t AS t2 WHERE t2.a = t.a LIMIT 1) FROM t;",
    "SELECT a FROM t ORDER BY (SELECT count(*) FROM u WHERE k = a), a;",
    "SELECT k, count(*) FROM u GROUP BY k HAVING count(*) > (SELECT 1);",
    "SELECT k, (SELECT count(*) FROM t WHERE t.a = u.k) FROM u GROUP BY k;",
    "SELECT k, sum((SELECT count(*) FROM t WHERE t.a = u.k)) FROM u GROUP BY k;",
    "SELECT sum((SELECT max(v) FROM u)) FROM t;",
    "SELECT a, b FROM t JOIN w ON w.p = (SELECT min(k) FROM u WHERE k >= t.a);",
    "SELECT a FROM t LEFT JOIN w ON w.p = t.a AND w.q IN (SELECT 'a' UNION SELECT 'b');",
    "SELECT (a, b) IN (SELECT k, v FROM u) FROM t;",
    "SELECT (SELECT k, v FROM u WHERE k = 2) = (2, 20);",
    "SELECT coalesce(a, (SELECT abs(-9223372036854775808))) FROM t WHERE a IS NOT NULL;",
    "SELECT * FROM u WHERE k IN (SELECT a FROM t);",
    "SELECT * FROM u WHERE k IN (SELECT a FROM t WHERE b > 'x');",
    "SELECT * FROM u WHERE k = (SELECT 2);",
    "SELECT * FROM u WHERE k IN (SELECT a FROM t) ORDER BY k DESC;",
    "SELECT * FROM t LIMIT (SELECT 2) OFFSET (SELECT count(*) FROM e);",
    "SELECT * FROM t LIMIT (SELECT 2 UNION SELECT 1);",
    "SELECT DISTINCT (SELECT count(*) FROM u WHERE k = t.a) FROM t;",
    "SELECT (SELECT 1 FROM (SELECT abs(-9223372036854775808) UNION SELECT 1) WHERE 0);",
    "SELECT * FROM t WHERE a IN (SELECT a FROM t WHERE a > (SELECT 1));",
];

#[test]
fn subqueries_match_the_pin() {
    let base = pin_file("sub", S);
    corpus(&base, "s", &strict(), SUBQ);
}

const FROMQ: &[&str] = &[
    "SELECT * FROM (SELECT a, b FROM t ORDER BY a DESC LIMIT 3) WHERE a > 1;",
    "SELECT * FROM (SELECT k, count(*) c FROM u GROUP BY k) AS g JOIN t ON t.a = g.k;",
    "SELECT * FROM vg ORDER BY s DESC;",
    "SELECT * FROM vg, vg AS g2 WHERE vg.k = g2.k;",
    "SELECT * FROM t, (SELECT max(v) m FROM u);",
    "SELECT * FROM t LEFT JOIN (SELECT k, v FROM u WHERE v > 15) s ON s.k = t.a;",
    "SELECT * FROM (SELECT a FROM t UNION SELECT k FROM u) ORDER BY 1;",
    "SELECT * FROM (SELECT a x FROM t UNION ALL SELECT k FROM u) LIMIT 4;",
    "WITH c AS (SELECT k, sum(v) s FROM u GROUP BY k) SELECT * FROM c, c AS d WHERE c.k = d.k;",
    "WITH c AS MATERIALIZED (SELECT a FROM t) SELECT * FROM c ORDER BY a;",
    "SELECT x FROM (SELECT k x, sum(v) FROM u GROUP BY k) LIMIT 2;",
    "SELECT * FROM (SELECT DISTINCT b FROM t) ORDER BY b;",
    "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(n, s);",
    "SELECT * FROM (SELECT * FROM t LIMIT 2) RIGHT JOIN w ON w.p = a;",
    "SELECT * FROM e, (SELECT 1 UNION SELECT 2);",
];

#[test]
fn from_subqueries_views_and_ctes_match_the_pin() {
    let base = pin_file("from", S);
    corpus(&base, "f", &strict(), FROMQ);
}

const COMPQ: &[&str] = &[
    "SELECT a FROM t UNION SELECT k FROM u;",
    "SELECT a FROM t UNION ALL SELECT k FROM u;",
    "SELECT a FROM t UNION ALL SELECT k FROM u ORDER BY 1 DESC LIMIT 3 OFFSET 1;",
    "SELECT a FROM t INTERSECT SELECT k FROM u;",
    "SELECT a FROM t EXCEPT SELECT k FROM u;",
    "SELECT b FROM t UNION SELECT q FROM w;",
    "SELECT q FROM w UNION SELECT b FROM t;",
    "SELECT a, b FROM t UNION SELECT k, v FROM u ORDER BY 2;",
    "VALUES (1, 'a'), (2, 'b') UNION ALL SELECT a, b FROM t;",
    "VALUES (1), (2), (1);",
    "VALUES (3) UNION VALUES (1) UNION VALUES (3);",
    "SELECT 1 UNION SELECT 2 LIMIT 1;",
    "SELECT 2 UNION SELECT 1 LIMIT 1;",
    "SELECT a FROM t UNION ALL SELECT k FROM u LIMIT 3;",
    "SELECT a FROM t UNION SELECT k FROM u EXCEPT SELECT 2;",
    "SELECT DISTINCT a FROM t UNION ALL SELECT DISTINCT k FROM u;",
];

#[test]
fn compounds_match_the_pin() {
    let base = pin_file("comp", S);
    corpus(&base, "c", &strict(), COMPQ);
}

const RECQ: &[&str] = &[
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c LIMIT 5;",
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c WHERE x % 3 = 0 LIMIT 3;",
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION SELECT x % 3 + 1 FROM c) SELECT x FROM c;",
    "WITH RECURSIVE c(x, d) AS (SELECT 1, 0 UNION ALL SELECT x * 2 + i, d + 1 FROM c, (SELECT 0 i UNION ALL SELECT 1) WHERE d < 2 ORDER BY 1 DESC) SELECT * FROM c;",
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10 LIMIT 4 OFFSET 2) SELECT * FROM c;",
    "WITH RECURSIVE c(x) AS (SELECT 5 UNION ALL SELECT x - 1 FROM c WHERE x > 0) SELECT sum(x), count(*) FROM c;",
    "SELECT a, (WITH RECURSIVE c(x) AS (SELECT t.a UNION ALL SELECT x + 1 FROM c WHERE x < 4) SELECT count(*) FROM c) FROM t;",
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT (SELECT x FROM c WHERE x > 3);",
];

#[test]
fn recursive_ctes_match_the_pin() {
    let base = pin_file("rec", S);
    corpus(&base, "r", &strict(), RECQ);
}

/// Failing statements: the rows before the failing one come first, as
/// on the pin; a failing subquery's error is raised only where an
/// expression reads its value (`SubValue::Fail`).
const ERRORS: &[&str] = &[
    "SELECT a, (SELECT abs(-9223372036854775808) WHERE a = 3) FROM t;",
    "SELECT a FROM t WHERE a < 3 OR (SELECT abs(-9223372036854775808) WHERE a = 3);",
    "SELECT a, coalesce(a, (SELECT abs(-9223372036854775808))) FROM t;",
    "SELECT a FROM t UNION ALL SELECT abs(-9223372036854775808);",
    "SELECT 1 UNION ALL SELECT abs(-9223372036854775808) UNION ALL SELECT 3 LIMIT 1 OFFSET 2;",
    "SELECT x FROM (SELECT a x FROM t UNION ALL SELECT abs(-9223372036854775808)) LIMIT 2;",
    "SELECT a FROM t WHERE a IN (SELECT abs(k - 9223372036854775807 - 2) FROM u);",
    "SELECT a, (SELECT k FROM u LIMIT 'x') FROM t;",
    "SELECT (SELECT 1 FROM u WHERE v = (SELECT abs(-9223372036854775808)));",
    "SELECT * FROM t LIMIT (SELECT abs(-9223372036854775808));",
    "WITH RECURSIVE c(x) AS (SELECT 9223372036854775806 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c LIMIT 3;",
    "SELECT k, sum((SELECT abs(-9223372036854775808) WHERE u.k = 5)) FROM u GROUP BY k;",
];

#[test]
fn errors_follow_the_rows_before_them() {
    let base = pin_file("err", S);
    corpus(&base, "e", &strict(), ERRORS);
}

/// A bigger file at 512-byte pages, several B-tree levels high, for
/// the restart corpus: correlated lookups through an index, IN sets,
/// derived tables and a recursive CTE over it.
fn big_setup() -> String {
    let mut s = String::from(
        "PRAGMA page_size = 512; CREATE TABLE big(id INTEGER PRIMARY KEY, k, t TEXT); \
         CREATE INDEX big_k ON big(k); CREATE TABLE sm(id INTEGER PRIMARY KEY, k); BEGIN;",
    );
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 1..=2000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s += &format!(
            "INSERT INTO big VALUES ({i}, {}, 'w{}-{}');",
            x % 300,
            x % 7,
            "pad".repeat((x % 20) as usize)
        );
    }
    for i in 1..=40u64 {
        s += &format!("INSERT INTO sm VALUES ({i}, {});", (i * 11) % 300);
    }
    s += "COMMIT;";
    s
}

const BIG: &[&str] = &[
    "SELECT id, (SELECT count(*) FROM big WHERE big.k = sm.k) FROM sm;",
    "SELECT id FROM sm WHERE k IN (SELECT k FROM big WHERE id < 50);",
    "SELECT id FROM sm WHERE EXISTS (SELECT 1 FROM big WHERE big.k = sm.k AND big.t LIKE 'w3%');",
    "SELECT count(*) FROM (SELECT k, count(*) n FROM big GROUP BY k) WHERE n > 7;",
    "SELECT k FROM sm UNION SELECT k FROM big WHERE id > 1990 ORDER BY 1;",
    "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 300) \
     SELECT count(*) FROM c WHERE n IN (SELECT k FROM big);",
    "SELECT sm.id, s.m FROM sm, (SELECT max(id) m FROM big) s WHERE sm.id < 4;",
    "SELECT (SELECT t FROM big WHERE id = sm.id * 40) FROM sm WHERE id < 10;",
];

#[test]
fn big_tables_match_the_pin() {
    let base = pin_file("big", &big_setup());
    corpus(&base, "b", &strict(), BIG);
}

/// Every corpus again with a page cache of one frame, the smallest
/// the engine allows: nearly every seek, step and row read inside a
/// subquery, a FROM subquery's code or a recursive step misses, the
/// page is read and the opcode runs again — same rows.
#[test]
fn restart_with_the_smallest_cache() {
    let cfg = strict().cache_pages(1);
    let base = pin_file("rs", S);
    corpus(&base, "rss", &cfg, SUBQ);
    corpus(&base, "rsf", &cfg, FROMQ);
    corpus(&base, "rsc", &cfg, COMPQ);
    corpus(&base, "rsr", &cfg, RECQ);
    corpus(&base, "rse", &cfg, ERRORS);
    let big = pin_file("rsbig", &big_setup());
    corpus(&big, "rsb", &cfg, BIG);
    corpus(&big, "rsb2", &strict().cache_pages(2), BIG);
}

/// Subquery-plan executions (`Connection::subquery_executions`) on
/// the VM equal today's executor's for each query: an uncorrelated
/// node once per statement, a correlated one per row that reaches it,
/// a FROM subquery per execution of its query, a recursive CTE's
/// setup and the CTE itself once each.
#[test]
fn subqueries_run_as_often_as_today() {
    let base = pin_file("count", S);
    let qs: &[&str] = &[
        "SELECT a FROM t WHERE a < (SELECT max(k) FROM u);",
        "SELECT (SELECT count(*) FROM u) FROM t;",
        "SELECT a FROM t WHERE a IN (SELECT k FROM u);",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE v > 5 LIMIT 2);",
        "SELECT (SELECT max(v) FROM u) + (SELECT min(v) FROM u) FROM t;",
        "SELECT sum((SELECT max(v) FROM u)) FROM t;",
        "SELECT a, sum((SELECT max(v) FROM u)) FROM t GROUP BY a ORDER BY a;",
        "SELECT (SELECT count(*) FROM u WHERE u.v <= (SELECT max(v) FROM u)) FROM t;",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.a LIMIT 2);",
        "SELECT a, (SELECT v FROM u WHERE k = a) FROM t WHERE a > 1;",
        "SELECT a FROM t WHERE b > 'x' AND a IN (SELECT k FROM u WHERE v > t.a);",
        "SELECT a FROM t WHERE a = 1 OR (SELECT count(*) FROM u WHERE k = t.a) > 0;",
        "SELECT * FROM (SELECT a FROM t), (SELECT k FROM u);",
        "SELECT * FROM vg;",
        "WITH c AS (SELECT a FROM t) SELECT * FROM c, c AS d;",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5) SELECT * FROM c;",
        "SELECT a, (SELECT 1 UNION SELECT 2) FROM t;",
        "SELECT a FROM t UNION SELECT (SELECT max(v) FROM u);",
        "SELECT a, (SELECT (SELECT max(v) FROM u WHERE u.k = t2.a) FROM t AS t2 WHERE t2.a = t.a) FROM t;",
    ];
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let count = |cfg: Config, tag: &str| {
            block_on(async {
                let fs = FileStorage::open(copy_of(&base, tag)).expect("fs open");
                let mut db = Connection::open(fs, cfg).await.expect("open");
                let before = db.subquery_executions();
                let mut st = db.prepare(q).await.expect("prepare");
                let mut rows = Vec::new();
                while st.step().await.expect("step") == Step::Row {
                    rows.push(render(&st));
                }
                let vm = st.runs_on_vm();
                drop(st);
                (db.subquery_executions() - before, rows, vm)
            })
        };
        let (n_vm, r_vm, on) = count(strict(), &format!("vms17-n{i}-v"));
        let (n_old, r_old, _) = count(Config::default().vm_off(true), &format!("vms17-n{i}-o"));
        if !on || n_vm != n_old || r_vm != r_old {
            bad.push(format!(
                "{q}: vm {n_vm} {r_vm:?} (on {on}), today {n_old} {r_old:?}"
            ));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---------------- generated queries ----------------

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

fn gen_val(r: &mut Rng) -> String {
    match r.n(10) {
        0 => "NULL".into(),
        1 => format!("{}.5", r.n(4)),
        2 => format!("'{}'", r.pick(&["x", "X", "1", "ab"])),
        _ => r.n(5).to_string(),
    }
}

/// Three small tables of random rows (one may be empty), sometimes
/// with indexes.
fn gen_schema(r: &mut Rng) -> String {
    let mut s = String::from(
        "CREATE TABLE p(a, b, c TEXT); CREATE TABLE q(a INT, b, c TEXT COLLATE NOCASE); \
         CREATE TABLE z(a INTEGER PRIMARY KEY, b, c);",
    );
    for i in [
        "CREATE INDEX qa ON q(a);",
        "CREATE INDEX pb ON p(b, a);",
        "CREATE INDEX zb ON z(b);",
    ] {
        if r.chance(40) {
            s += i;
        }
    }
    for (t, ipk) in [("p", false), ("q", false), ("z", true)] {
        let n = if r.chance(10) { 0 } else { r.n(9) };
        for j in 0..n {
            let first = if ipk { (j + 1).to_string() } else { gen_val(r) };
            s += &format!(
                "INSERT INTO {t} VALUES ({first}, {}, {});",
                gen_val(r),
                gen_val(r)
            );
        }
    }
    s
}

/// A random subquery expression over outer table alias `o` (or none).
fn gen_sub(r: &mut Rng, o: &str, depth: u32) -> String {
    let t = r.pick(&["p", "q", "z"]);
    let col = r.pick(&["a", "b", "c"]);
    let corr = if !o.is_empty() && r.chance(60) {
        format!(
            " WHERE s{depth}.{} {} {o}.{}",
            r.pick(&["a", "b"]),
            r.pick(&["=", "<", ">="]),
            r.pick(&["a", "b", "c"])
        )
    } else if r.chance(30) {
        format!(" WHERE s{depth}.b > {}", r.n(4))
    } else {
        String::new()
    };
    let inner = if depth < 2 && r.chance(25) {
        format!(
            " AND s{depth}.a IN ({})",
            gen_set(r, &format!("s{depth}"), depth + 1)
        )
    } else {
        String::new()
    };
    let corr = if corr.is_empty() && !inner.is_empty() {
        format!(" WHERE 1{inner}")
    } else {
        corr + &inner
    };
    match r.n(7) {
        0 => format!("(SELECT count(*) FROM {t} s{depth}{corr})"),
        1 => format!("(SELECT max(s{depth}.{col}) FROM {t} s{depth}{corr})"),
        2 => format!("(SELECT s{depth}.{col} FROM {t} s{depth}{corr} ORDER BY 1 DESC)"),
        3 => format!("EXISTS (SELECT 1 FROM {t} s{depth}{corr})"),
        4 => format!("(SELECT s{depth}.{col} FROM {t} s{depth}{corr} UNION SELECT 3)"),
        5 => format!("(SELECT group_concat(s{depth}.{col}) FROM {t} s{depth}{corr})"),
        _ => format!("(SELECT s{depth}.{col} FROM {t} s{depth}{corr} LIMIT 1 OFFSET 1)"),
    }
}

/// A random IN set: a subquery, a compound or VALUES.
fn gen_set(r: &mut Rng, o: &str, depth: u32) -> String {
    let t = r.pick(&["p", "q", "z"]);
    let col = r.pick(&["a", "b", "c"]);
    match r.n(5) {
        0 => format!("SELECT {col} FROM {t}"),
        1 => format!("SELECT {col} FROM {t} WHERE b IS NOT NULL"),
        2 => format!("SELECT {col} FROM {t} UNION ALL SELECT NULL"),
        3 if !o.is_empty() => {
            format!("SELECT x{depth}.{col} FROM {t} x{depth} WHERE x{depth}.a = {o}.b")
        }
        3 => format!("SELECT {col} FROM {t} EXCEPT SELECT 1"),
        _ => format!("VALUES (1), ({}), (NULL)", gen_val(r)),
    }
}

/// A random query: subqueries (scalar, EXISTS, IN, NOT IN, nested,
/// correlated) in the result columns, WHERE, ORDER BY and HAVING;
/// FROM subqueries, views and CTEs; compounds; recursive CTEs.
fn gen_query(r: &mut Rng) -> String {
    let t = r.pick(&["p", "q", "z"]);
    match r.n(10) {
        0 => {
            let lim = 1 + r.n(6);
            // (An outer WHERE over an endless CTE could match nothing
            // and never stop, on the pin too: the step is then bounded.)
            let outer = r.chance(40);
            let bound = outer || r.chance(50);
            format!(
                "WITH RECURSIVE c(x) AS (SELECT {} UNION{} SELECT x {} FROM c{}) SELECT x{} FROM c{} LIMIT {lim};",
                r.n(3),
                r.pick(&["", " ALL"]),
                r.pick(&["+ 1", "* 2 % 7", "+ 2"]),
                if bound { " WHERE x < 9" } else { "" },
                if r.chance(40) {
                    format!(", {}", gen_sub(r, "c", 0))
                } else {
                    String::new()
                },
                if outer { " WHERE x % 2 = 0" } else { "" },
            )
        }
        1 => {
            let op = r.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
            let ord = if r.chance(50) { " ORDER BY 1" } else { "" };
            let lim = if r.chance(40) {
                format!(" LIMIT {}", 1 + r.n(4))
            } else {
                String::new()
            };
            format!(
                "SELECT a FROM {t} {op} SELECT {} FROM {}{ord}{lim};",
                r.pick(&["a", "b", "c"]),
                r.pick(&["p", "q", "z"])
            )
        }
        2 => {
            let inner = format!(
                "SELECT a, b, {} AS s FROM {t} x{}",
                gen_sub(r, "x", 0),
                if r.chance(50) {
                    " ORDER BY a DESC LIMIT 4"
                } else {
                    ""
                }
            );
            format!(
                "SELECT * FROM ({inner}) AS d{};",
                r.pick(&["", " WHERE d.a > 1", " LIMIT 2", " ORDER BY s, a, b"])
            )
        }
        3 => format!(
            "WITH w AS (SELECT a, count(*) n FROM {t} GROUP BY a) SELECT w.a, w.n, {} FROM w, {} y WHERE y.a {} w.a;",
            gen_sub(r, "w", 0),
            r.pick(&["p", "q", "z"]),
            r.pick(&["=", "<"]),
        ),
        4 => format!(
            "SELECT b, count(*), sum({}) FROM {t} x GROUP BY b{};",
            gen_sub(r, "x", 0),
            if r.chance(50) {
                " HAVING count(*) > 0"
            } else {
                ""
            }
        ),
        _ => {
            let mut conds = Vec::new();
            for _ in 0..r.n(3) {
                conds.push(match r.n(5) {
                    0 => format!("x.a IN ({})", gen_set(r, "x", 0)),
                    1 => format!("x.b NOT IN ({})", gen_set(r, "x", 0)),
                    2 => gen_sub(r, "x", 0),
                    3 => format!("x.a {} {}", r.pick(&["=", ">", "<="]), gen_sub(r, "x", 0)),
                    _ => format!("(x.c = 'x' OR {})", gen_sub(r, "x", 0)),
                });
            }
            let mut q = format!("SELECT x.a, {} FROM {t} x", gen_sub(r, "x", 0));
            if !conds.is_empty() {
                q += &format!(" WHERE {}", conds.join(r.pick(&[" AND ", " OR "])));
            }
            if r.chance(30) {
                q += &format!(" ORDER BY {}, 1", gen_sub(r, "x", 0));
            }
            if r.chance(20) {
                q += &format!(" LIMIT {}", 1 + r.n(3));
            }
            q + ";"
        }
    }
}

/// The rows as a sorted multiset.
fn sorted(rows: &[String]) -> Vec<String> {
    let mut v = rows.to_vec();
    v.sort();
    v
}

/// Generated queries (`gen_query`): every one on the VM, strict; the
/// VM gives today's executor's rows, order and success exactly
/// (`Config::vm_off`), or the pin's where today's executor does not.
#[test]
fn generated_queries_match_the_executor_and_the_pin() {
    let seeds: u64 = std::env::var("HIPP_VMS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let mut bad = Vec::new();
    let (mut total, mut fixed) = (0usize, 0usize);
    for seed in 0..seeds {
        let mut r = Rng(0x5ab5_0000 + seed * 7919);
        let base = pin_file(&format!("gen{seed}"), &gen_schema(&mut r));
        for qi in 0..12 {
            let q = gen_query(&mut r);
            let tag = format!("vms17-g{seed}-{qi}");
            let (vm, on_vm, ve) = hipp_run(&copy_of(&base, &format!("{tag}-v")), strict(), &q);
            let (old, _, oe) = hipp_run(
                &copy_of(&base, &format!("{tag}-l")),
                Config::default().vm_off(true),
                &q,
            );
            let pin = pin_run(&copy_of(&base, &format!("{tag}-p")), &q);
            total += 1;
            if !on_vm {
                bad.push(format!("not on the VM: {q} {ve:?}"));
                continue;
            }
            let old_pin = old.ok == pin.ok && sorted(&old.rows) == sorted(&pin.rows);
            let ok = if vm == old {
                true
            } else {
                fixed += 1;
                !old_pin && vm.ok == pin.ok && sorted(&vm.rows) == sorted(&pin.rows)
            };
            if !ok {
                bad.push(format!(
                    "seed {seed}: {q}\n  vm  : {vm:?} {ve:?}\n  old : {old:?} {oe:?}\n  pin : {pin:?}"
                ));
            }
        }
    }
    eprintln!(
        "{total} generated queries on the VM; {fixed} where it gives the pin's rows and today's executor does not"
    );
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}
