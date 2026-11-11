//! A FROM subquery / view / CTE whose column has REAL affinity
//! across a compound, differentially vs the pinned oracle (SQLite
//! 3.54.1): v0.17 review fix (wave 14, unit `vm_flip` retry), ungated
//! on the `sql` derived-table path.
//!
//! The pin reads a REAL column of a subquery it does not flatten with
//! `OP_RealAffinity`, so an integer from another compound core reads
//! as a real (`SELECT x FROM (SELECT real_col x FROM t UNION ALL
//! SELECT 1)` gives `1.0`); the recursive CTE's queue reads alike.
//! It takes the compound column's affinity from the first core whose
//! item has one (`SELECT 1 x UNION SELECT real_col ...` is REAL; a
//! column, even an untyped one, stops the search), and a scalar
//! subquery's affinity from its rightmost core. hipp kept the
//! integers, read only the leftmost core, and took a scalar
//! subquery's leftmost core (CREATE TABLE ... AS declared REAL where
//! the pin declares nothing; PRAGMA table_info of such a view showed
//! no type).
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

/// `sql` (statements separated by `;`, none inside a literal) on
/// hipp under `cfg`, stopping at the first error as the pin's
/// `-bail` does: its run and whether every statement ran on the VM.
fn hipp_run(path: &Path, cfg: Config, sql: &str) -> (Run, bool) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        let mut rows = Vec::new();
        let mut all_vm = true;
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let Ok(mut q) = db.prepare(stmt).await else {
                return (Run { ok: false, rows }, all_vm);
            };
            let mut on_vm = None;
            loop {
                let r = q.step().await;
                #[cfg(feature = "vm")]
                let vm = *on_vm.get_or_insert_with(|| q.runs_on_vm());
                #[cfg(not(feature = "vm"))]
                let vm = *on_vm.get_or_insert(true);
                match r {
                    Ok(Step::Row) => rows.push(render(&q)),
                    Ok(Step::Done) => {
                        // (Only a SELECT is the VM's to run.)
                        let select = stmt.starts_with("SELECT") || stmt.starts_with("WITH");
                        all_vm &= vm || !select;
                        break;
                    }
                    Err(_) => return (Run { ok: false, rows }, all_vm),
                }
            }
        }
        (Run { ok: true, rows }, all_vm)
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
    let base = common::temp_db(&format!("caff17-{tag}-base"));
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
    // (A corpus the VM does not claim falls back by design; under
    // `--cfg hipp_vm_strict`, the whole-suite strict run, the default
    // `Config` is strict, so it is made non-strict here, as the `vm`
    // engine is.)
    #[cfg(feature = "vm")]
    let dflt = if claimed {
        Config::default()
    } else {
        Config::default().vm_strict(false)
    };
    #[cfg(not(feature = "vm"))]
    let dflt = Config::default();
    #[allow(unused_mut)]
    let mut v = vec![("default", dflt, false)];
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
        let o = copy_of(base, &format!("caff17-{tag}{i}-o"));
        let want = pin_run(&o, q);
        for (name, cfg, must_vm) in engines(claimed) {
            let h = copy_of(base, &format!("caff17-{tag}{i}-{name}"));
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
    let base = pin_file(tag, setup);
    let qs: Vec<String> = qs.iter().map(|q| q.to_string()).collect();
    let bad = diffs(&base, tag, &qs, true);
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

const S: &str = "CREATE TABLE t(c REAL); INSERT INTO t VALUES (0.0); \
    CREATE TABLE u(i INTEGER, s TEXT); INSERT INTO u VALUES (1,'a'),(2,'b'); \
    CREATE TABLE b(z, w BLOB); INSERT INTO b VALUES (5,6); \
    CREATE TABLE t2(c REAL, k INT); INSERT INTO t2 VALUES (2.5,1),(3,2),(NULL,3); \
    CREATE VIEW v AS SELECT c x FROM t UNION SELECT 1; \
    CREATE VIEW v2 AS SELECT 1 x UNION ALL SELECT c FROM t; \
    CREATE VIEW v3 AS SELECT 1 x UNION ALL SELECT 'a' UNION ALL SELECT c FROM t; \
    CREATE VIEW v4 AS SELECT 1 x UNION ALL SELECT 2;";

/// The review's repros and the shapes around them.
const SHAPES: &[&str] = &[
    // The review's repros.
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION SELECT c FROM t)",
    "SELECT max(x), typeof(max(x)) FROM (SELECT c x FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM v",
    "SELECT typeof(x) FROM (SELECT c x FROM t UNION ALL VALUES(1))",
    // CTEs, shared materializations, nesting, the other operators.
    "WITH w(x) AS (SELECT c FROM t UNION ALL SELECT 2) SELECT x, typeof(x) FROM w",
    "WITH w(x) AS MATERIALIZED (SELECT c FROM t UNION ALL SELECT 2) \
     SELECT a.x, typeof(b.x) FROM w a, w b",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT c FROM t)",
    "SELECT x, typeof(x) FROM (SELECT 1 x EXCEPT SELECT c FROM t)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t INTERSECT SELECT 0)",
    "SELECT y, typeof(y) FROM (SELECT x y FROM (SELECT c x FROM t UNION ALL SELECT 1))",
    "SELECT y, typeof(y) FROM (SELECT x y FROM (SELECT c x FROM t UNION ALL SELECT 1) LIMIT 9)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT i FROM u)",
    "SELECT x, typeof(x) FROM (SELECT i x FROM u UNION ALL SELECT c FROM t)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT NULL)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 9223372036854775807)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT -9223372036854775808)",
    "SELECT x + 0, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1) WHERE x = 1",
    "SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1) WHERE x = 1.0",
    "SELECT count(*), sum(x), typeof(sum(x)) FROM (SELECT c x FROM t UNION ALL SELECT 1)",
    "SELECT group_concat(x) FROM (SELECT c x FROM t UNION ALL SELECT 1)",
    "SELECT DISTINCT x FROM (SELECT c x FROM t UNION ALL SELECT 1 UNION ALL SELECT 1)",
    "SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1) ORDER BY x DESC",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1 ORDER BY 1 DESC LIMIT 1)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1 UNION ALL SELECT 1) GROUP BY 1",
    "SELECT * FROM (SELECT c x FROM t UNION ALL SELECT 1), (SELECT c y FROM t UNION ALL SELECT 2) \
     ORDER BY 1, 2",
    "SELECT * FROM (SELECT c x FROM t UNION ALL SELECT 1) a JOIN (SELECT 1 y) b ON a.x = b.y",
    "SELECT a.x, typeof(a.x) FROM u LEFT JOIN (SELECT c x FROM t UNION ALL SELECT 1) a ON 1",
    "SELECT a.x, typeof(a.x), i FROM u LEFT JOIN (SELECT c x FROM t UNION ALL SELECT 1) a \
     ON a.x = u.i",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1) WHERE x IN (SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1) AS d \
     WHERE d.x BETWEEN 0.5 AND 1.5",
    "SELECT (SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1) LIMIT 1 OFFSET 1)",
    "SELECT typeof((SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1) LIMIT 1 OFFSET 1))",
    "SELECT 1 IN (SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1))",
    "SELECT x, typeof(x) FROM (SELECT c x, 1 z FROM t UNION ALL SELECT 1, 2 ORDER BY z)",
    "SELECT x, typeof(x), count(*) FROM (SELECT c x FROM t2 UNION ALL SELECT k FROM t2) \
     GROUP BY x ORDER BY 1",
    "SELECT x, typeof(x) FROM (SELECT k x FROM t2 UNION ALL SELECT c FROM t2) ORDER BY 1",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t2 UNION SELECT k FROM t2) ORDER BY 1",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t2 UNION ALL SELECT k FROM t2 LIMIT 2 OFFSET 2)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t2 UNION ALL SELECT k FROM t2) WHERE x > 1 \
     ORDER BY 1",
    // The first core whose item has an affinity gives it; a column
    // (even untyped) stops the search; demotion counts every core.
    "SELECT x, typeof(x) FROM (SELECT max(c) x FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT 'a' x UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT CAST(c AS REAL) FROM t UNION ALL SELECT 2)",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT c FROM t UNION ALL SELECT 'z')",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT s FROM u UNION ALL SELECT 2)",
    "SELECT x, typeof(x) FROM (SELECT NULL x UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT i FROM u UNION ALL SELECT 2.0)",
    "SELECT x, typeof(x) FROM (SELECT x'41' x UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT z x FROM b UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT w x FROM b UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT z + 1 x FROM b UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT (SELECT 1) x UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT (SELECT c FROM t) x UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT +c x FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT c COLLATE NOCASE x FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT y x FROM (SELECT 1 y LIMIT 1) UNION ALL SELECT c FROM t \
     UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT CAST(1 AS BLOB) x UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT rowid x FROM t UNION ALL SELECT c FROM t UNION ALL SELECT 1)",
    "SELECT x, typeof(x) FROM (SELECT CAST(5 AS REAL) x UNION ALL SELECT 2)",
    "SELECT x, typeof(x) FROM (SELECT CAST(5 AS NUMERIC) x UNION ALL SELECT 2)",
    "SELECT x, typeof(x) FROM (SELECT 1 x UNION ALL SELECT 2.0 UNION ALL SELECT c FROM t)",
    "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT '3')",
    "SELECT x, typeof(x) FROM v2",
    "SELECT x, typeof(x) FROM v3",
    "SELECT x, typeof(x) FROM v4",
    // A plain core: REAL when the pin does not flatten it (its
    // DISTINCT, its GROUP BY, an outer DISTINCT over its LIMIT);
    // flattened, the item's own value. A scalar subquery's affinity
    // is its rightmost core's.
    "SELECT x, typeof(x) FROM (SELECT (SELECT c FROM t UNION ALL SELECT 1 LIMIT 1 OFFSET 1) x)",
    "SELECT x, typeof(x) FROM (SELECT (SELECT c FROM t UNION ALL SELECT 1 LIMIT 1 OFFSET 1) x \
     FROM u GROUP BY i)",
    "SELECT (SELECT c FROM t UNION ALL SELECT 1 LIMIT 1 OFFSET 1) = '1', \
     (SELECT 1 UNION ALL SELECT c FROM t LIMIT 1) = '1'",
    "SELECT x, typeof(x) FROM (SELECT (SELECT c FROM t UNION ALL SELECT 1 LIMIT 1 OFFSET 1) x \
     UNION ALL SELECT 2)",
    "SELECT x, typeof(x) FROM (SELECT (SELECT 1 UNION ALL SELECT c FROM t LIMIT 1) x \
     UNION ALL SELECT 2)",
    "SELECT k, typeof(k) FROM (SELECT (SELECT c FROM t LIMIT 1) k UNION ALL SELECT z FROM b)",
    // The recursive CTE: its rows and its queue as the step reads it.
    "WITH RECURSIVE r(x) AS (SELECT c FROM t UNION ALL SELECT x + 1 FROM r WHERE x < 2) \
     SELECT x, typeof(x) FROM r",
    "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT c FROM t, r WHERE x < 1) \
     SELECT x, typeof(x) FROM r",
    "WITH RECURSIVE r(x, n, k) AS (SELECT 1, 0, '' UNION ALL \
     SELECT (SELECT c FROM t), n + 1, typeof(x) FROM r WHERE n < 2) SELECT * FROM r",
    "WITH RECURSIVE r(x, n) AS (SELECT 1, 0 UNION ALL SELECT (SELECT c FROM t), n + 1 FROM r \
     WHERE n < 2) SELECT max(x), typeof(max(x)) FROM r",
    "WITH RECURSIVE r(x, n) AS (SELECT 1, 0 UNION SELECT (SELECT c FROM t), n + 1 FROM r \
     WHERE n < 2) SELECT x, typeof(x), n FROM r",
    "WITH RECURSIVE r(x, n) AS (SELECT 1, 0 UNION ALL SELECT x, n + 1 FROM r WHERE n < 2) \
     SELECT x, typeof(x), n FROM r",
    // Declared types: CREATE TABLE ... AS and PRAGMA table_info.
    "CREATE TABLE ct AS SELECT 1 x UNION ALL SELECT c FROM t; \
     SELECT sql FROM sqlite_schema WHERE name = 'ct'; SELECT x, typeof(x) FROM ct",
    "CREATE TABLE ct2 AS SELECT (SELECT c FROM t) x, (SELECT c FROM t UNION ALL SELECT 1) y, \
     (SELECT 1 UNION ALL SELECT c FROM t) z; SELECT sql FROM sqlite_schema WHERE name = 'ct2'",
    "CREATE TABLE ct3 AS SELECT (SELECT c FROM t UNION ALL SELECT 1 LIMIT 1 OFFSET 1) x; \
     SELECT x, typeof(x) FROM ct3",
    "PRAGMA table_info(v2)",
    "PRAGMA table_info(v3)",
    "PRAGMA table_info(v4)",
    // Writes that read one.
    "CREATE TABLE z(a); INSERT INTO z SELECT x FROM (SELECT c x FROM t UNION ALL SELECT 1); \
     SELECT a, typeof(a) FROM z",
    "CREATE TABLE z(a); INSERT INTO z SELECT * FROM (SELECT c x FROM t UNION ALL SELECT 1) \
     RETURNING a, typeof(a)",
    "CREATE TABLE z(a); INSERT INTO z VALUES (1); UPDATE z SET a = (SELECT x FROM \
     (SELECT c x FROM t UNION ALL SELECT 7) WHERE x > 1); SELECT a, typeof(a) FROM z",
    "CREATE TABLE z(a, b); INSERT INTO z VALUES (1, 0); UPDATE z SET b = d.x FROM \
     (SELECT c x FROM t UNION ALL SELECT 1) d WHERE d.x = z.a; SELECT a, b, typeof(b) FROM z",
    // A top-level compound is no subquery: no conversion.
    "SELECT c FROM t UNION ALL SELECT 1",
    "SELECT 1 UNION SELECT c FROM t",
];

#[test]
fn derived_real_affinity_matches_the_pin() {
    corpus(S, "shapes", SHAPES);
}

/// A plain core the pin does not flatten (its GROUP BY, its
/// DISTINCT) reads its REAL column as reals too; hipp knows which
/// cores the pin flattens only with the flattener's model
/// (`planner`).
#[cfg(feature = "planner")]
#[test]
fn unflattened_plain_cores_match_the_pin() {
    corpus(
        S,
        "plain",
        &[
            "SELECT x, typeof(x) FROM (SELECT (SELECT 1 UNION ALL SELECT c FROM t LIMIT 1) x \
             FROM u GROUP BY i)",
            "SELECT x, typeof(x) FROM (SELECT DISTINCT (SELECT 1 UNION ALL SELECT c FROM t \
             LIMIT 1) x FROM u)",
            "SELECT DISTINCT x, typeof(x) FROM (SELECT (SELECT 1 UNION ALL SELECT c FROM t \
             LIMIT 1) x FROM u LIMIT 1)",
            "SELECT x, typeof(x) FROM (SELECT (SELECT 1 UNION ALL SELECT c FROM t LIMIT 1) x \
             FROM u LIMIT 1)",
        ],
    );
}

/// A window function over such a subquery.
#[cfg(feature = "window")]
#[test]
fn windows_over_it_match_the_pin() {
    corpus(
        S,
        "win",
        &[
            "SELECT x, typeof(x), row_number() OVER (ORDER BY x) FROM \
             (SELECT c x FROM t2 UNION ALL SELECT k FROM t2)",
            "SELECT x, sum(x) OVER (ORDER BY x ROWS UNBOUNDED PRECEDING) FROM \
             (SELECT k x FROM t2 UNION ALL SELECT c FROM t2 UNION ALL SELECT 1) ORDER BY 1",
        ],
    );
}

/// A tiny deterministic PRNG (xorshift64*), as tests/fuzz_smoke.rs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// Generated compounds over the corpus' tables: random cores (REAL,
/// INTEGER, TEXT and untyped columns, literals of every class, CASTs,
/// scalar subqueries, expressions) under every compound operator,
/// read as a FROM subquery, a CTE, a view and an aggregate's input.
#[test]
fn generated_compound_affinities_match_the_pin() {
    // (expression, its FROM clause)
    const ITEMS: &[(&str, &str)] = &[
        ("c", " FROM t2"),
        ("k", " FROM t2"),
        ("s", " FROM u"),
        ("i", " FROM u"),
        ("z", " FROM b"),
        ("1", ""),
        ("2.5", ""),
        ("-3", ""),
        ("'7'", ""),
        ("'q'", ""),
        ("NULL", ""),
        ("x'41'", ""),
        ("CAST(k AS REAL)", " FROM t2"),
        ("CAST(i AS TEXT)", " FROM u"),
        ("CAST(2 AS NUMERIC)", ""),
        ("(SELECT c FROM t)", ""),
        ("(SELECT 4 UNION ALL SELECT c FROM t LIMIT 1)", ""),
        ("k + 1", " FROM t2"),
        ("c * 2", " FROM t2"),
        ("max(i)", " FROM u"),
        ("c COLLATE NOCASE", " FROM t2"),
    ];
    const OPS: &[&str] = &["UNION ALL", "UNION ALL", "UNION", "EXCEPT", "INTERSECT"];
    const READS: &[&str] = &[
        "SELECT x, typeof(x) FROM ({}) ORDER BY 1, 2",
        "SELECT x, typeof(x) FROM ({})",
        "SELECT max(x), typeof(max(x)), min(x), typeof(min(x)) FROM ({})",
        "SELECT sum(x), count(x), group_concat(x) FROM ({})",
        "WITH w(x) AS ({}) SELECT x, typeof(x) FROM w ORDER BY 1, 2",
        "SELECT x, typeof(x) FROM ({}) WHERE x > 0 ORDER BY 1, 2",
        "SELECT DISTINCT typeof(x) FROM ({}) ORDER BY 1",
        "SELECT d.x, typeof(d.x), u.i FROM u JOIN ({}) d ON d.x >= u.i ORDER BY 1, 2, 3",
        "CREATE TABLE g AS {}; SELECT sql FROM sqlite_schema WHERE name = 'g'; \
         SELECT x, typeof(x) FROM g ORDER BY 1, 2",
        "CREATE VIEW gv AS {}; SELECT x, typeof(x) FROM gv ORDER BY 1, 2; PRAGMA table_info(gv)",
    ];
    let mut rng = Rng(0x5eed_cafe_f00d_0017);
    let mut qs = Vec::new();
    for _ in 0..240 {
        let n = 2 + (rng.next() % 3) as usize;
        let item = |rng: &mut Rng| ITEMS[(rng.next() % ITEMS.len() as u64) as usize];
        // The leftmost core names the column.
        let (e, f) = item(&mut rng);
        let mut body = format!("SELECT {e} x{f}");
        for _ in 1..n {
            let op = rng.pick(OPS);
            let (e, f) = item(&mut rng);
            body.push_str(&format!(" {op} SELECT {e}{f}"));
        }
        qs.push(rng.pick(READS).replace("{}", &body));
    }
    let qs: Vec<&str> = qs.iter().map(String::as_str).collect();
    corpus(S, "gen", &qs);
}
