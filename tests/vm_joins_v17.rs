//! The bytecode VM (ADR 0005 Stage 2, hidden feature `vm`), unit
//! `vm_joins`, differentially vs the pinned oracle (SQLite 3.54.1).
//! Built only with `--features vm`; every SELECT a corpus claims runs
//! under `Config::vm_strict` (falling back to today's executor is an
//! error) and asserts `Statement::runs_on_vm`.
//!
//! - **Joins** in the planner's order: inner, comma and CROSS joins,
//!   LEFT JOIN NULL-extension, RIGHT and FULL joins (the unmatched
//!   second pass, several of them, mixed with LEFT joins), NATURAL /
//!   USING column sets, ON terms placed where the planner put them,
//!   semi-joins from flattened EXISTS terms, empty tables on either
//!   side.
//! - **Access paths**: rowid lookups and ranges per outer row, index
//!   equality, ranges and IN lists (sorted, deduplicated, walked in
//!   the key's order, DESC columns, reverse scans), covering,
//!   expression and partial indexes, NOCASE index columns,
//!   WHERE_BIGNULL_SORT, MULTI-INDEX OR (each row once, in the pin's
//!   order), skip-scans, the min() / max() early-out, automatic
//!   indexes and Bloom filters.
//! - **Aggregates**: the streamed fold (count / sum / total / avg /
//!   min / max), GROUP BY groups under collations, HAVING (its moved
//!   terms, the aggregates finalized first), DISTINCT aggregates,
//!   min() / max() bare-column rows, the empty group, DISTINCT with
//!   NULLs, ORDER BY / LIMIT / OFFSET over all of them.
//! - **Rows before an error**: a RIGHT JOIN's main loop, a GROUP BY's
//!   earlier groups and a GROUP BY ordered by its own terms return
//!   their rows before the failing one, as on the pin (today's
//!   materialising executor returns none).
//! - **Against today's executor**: generated joins, index and
//!   aggregate queries (xorshift) give exactly the rows, order and
//!   errors of the executor the VM replaces (`Config::vm_off`), and
//!   the pin's rows wherever today's executor gives them.
//! - **Restart**: the corpus again with the smallest page cache (one
//!   frame), so
//!   nearly every index seek and step misses and runs again.
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
    let base = common::temp_db(&format!("vmj17-{tag}-base"));
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
        let h = copy_of(base, &format!("vmj17-{tag}{i}-h"));
        let o = copy_of(base, &format!("vmj17-{tag}{i}-o"));
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

const S: &str = "CREATE TABLE a(x INTEGER PRIMARY KEY, y, z TEXT); \
    INSERT INTO a VALUES (1, 1, 'p'), (2, 2, 'Q'), (3, NULL, 'r'), (4, 2, NULL), (5, '2', 'q'), (6, 7.5, 'P'); \
    CREATE TABLE b(p, q TEXT COLLATE NOCASE, r REAL); \
    INSERT INTO b VALUES (1, 'one', 1), (2, 'Two', 2), (2, 'two', 2.5), (NULL, 'nil', NULL), \
      (7, 'seven', 7), ('2', 'str', 0), (5, 'five', -1); \
    CREATE TABLE c(k INTEGER PRIMARY KEY, v); \
    INSERT INTO c VALUES (1, 'c1'), (3, 'c3'), (5, 'c5'), (8, 'c8'); \
    CREATE TABLE d(m, n); \
    CREATE TABLE n1(id, v); INSERT INTO n1 VALUES (1, 'a'), (2, 'b'), (NULL, 'n'), (4, 'd'); \
    CREATE TABLE n2(id, w); INSERT INTO n2 VALUES (2, 'B'), (4, 'D'), (4, 'DD'), (5, 'E'), (NULL, 'N'); \
    CREATE TABLE i(u, w, s TEXT); \
    INSERT INTO i VALUES (1, 1, 'ab'), (3, 2, 'AB'), (3, 5, 'cd'), (NULL, 4, NULL), (5, NULL, 'Ab'), \
      (2, 9, 'x'), (3, 3, 'y'), ('3', 7, 'z'), (4.5, 1, 'ab'), (7, 2, NULL), (0, 0, ''), (3, NULL, 'q'); \
    CREATE INDEX i_u ON i(u); CREATE INDEX i_uw ON i(u, w DESC); \
    CREATE INDEX i_s ON i(s COLLATE NOCASE); CREATE INDEX i_e ON i(u + w); \
    CREATE INDEX i_p ON i(w) WHERE w > 2; \
    CREATE TABLE g(k, v); INSERT INTO g VALUES (1, 1), (2, 2), (2, 3), (NULL, 4), (1, NULL), ('a', 5), ('A', 6), (2.0, 7); \
    CREATE VIEW va AS SELECT * FROM a;";

const JOINS: &[&str] = &[
    "SELECT * FROM a, b WHERE a.y = b.p;",
    "SELECT a.x, b.q FROM a JOIN b ON a.y = b.p ORDER BY 1, 2;",
    "SELECT * FROM a CROSS JOIN c;",
    "SELECT * FROM a, c WHERE c.k = a.x;",
    "SELECT * FROM a JOIN c ON c.k = a.y;",
    "SELECT * FROM c JOIN a ON a.x = c.k + 1;",
    "SELECT * FROM a, c WHERE c.k > a.x AND c.k < a.x + 4;",
    "SELECT * FROM a LEFT JOIN b ON a.y = b.p;",
    "SELECT * FROM a LEFT JOIN b ON a.y = b.p AND b.r > 1;",
    "SELECT * FROM a LEFT JOIN b ON a.y = b.p WHERE b.p IS NULL;",
    "SELECT * FROM a LEFT JOIN c ON c.k = a.x WHERE c.v IS NULL OR a.x > 4;",
    "SELECT * FROM a RIGHT JOIN b ON a.y = b.p;",
    "SELECT * FROM a FULL JOIN b ON a.y = b.p;",
    "SELECT * FROM a FULL JOIN c ON a.x = c.k;",
    "SELECT * FROM a RIGHT JOIN c ON a.x = c.k WHERE a.z IS NULL;",
    "SELECT * FROM a FULL JOIN c ON a.x = c.k ORDER BY c.v DESC, a.x;",
    "SELECT * FROM a JOIN b ON a.y = b.p LEFT JOIN c ON c.k = b.p;",
    "SELECT * FROM a RIGHT JOIN b ON a.y = b.p LEFT JOIN c ON c.k = b.r;",
    "SELECT * FROM a RIGHT JOIN b ON a.y = b.p RIGHT JOIN c ON c.k = b.p;",
    "SELECT * FROM a FULL JOIN b ON a.y = b.p FULL JOIN c ON c.k = a.x;",
    "SELECT * FROM a LEFT JOIN b ON a.y = b.p RIGHT JOIN c ON c.k = a.x;",
    "SELECT * FROM a LEFT JOIN d ON d.m = a.x;",
    "SELECT * FROM d RIGHT JOIN a ON d.m = a.x;",
    "SELECT * FROM a FULL JOIN d ON d.m = a.x;",
    "SELECT * FROM d FULL JOIN a ON d.m = a.x;",
    "SELECT * FROM d, a;",
    "SELECT * FROM a, d;",
    "SELECT * FROM d RIGHT JOIN d AS e ON 1;",
    "SELECT * FROM n1 NATURAL JOIN n2;",
    "SELECT * FROM n1 NATURAL LEFT JOIN n2;",
    "SELECT * FROM n1 NATURAL RIGHT JOIN n2;",
    "SELECT * FROM n1 NATURAL FULL JOIN n2;",
    "SELECT * FROM n1 JOIN n2 USING (id);",
    "SELECT id, v, w FROM n1 LEFT JOIN n2 USING (id) ORDER BY id;",
    "SELECT id FROM n1 FULL JOIN n2 USING (id) ORDER BY id;",
    "SELECT * FROM n1 RIGHT JOIN n2 USING (id);",
    "SELECT * FROM a AS a1 JOIN a AS a2 ON a1.y = a2.x JOIN a AS a3 ON a3.x = a2.y;",
    "SELECT a.x, c.v FROM a LEFT JOIN c ON c.k = a.x AND a.z > 'p';",
    "SELECT * FROM a JOIN b ON 1 WHERE a.x = 2 AND b.p = 2;",
    "SELECT * FROM a JOIN b ON 0;",
    "SELECT * FROM a LEFT JOIN b ON 0;",
    "SELECT * FROM a WHERE 0;",
    "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.p = a.y);",
    "SELECT * FROM va JOIN c ON c.k = va.x;",
    "SELECT * FROM (SELECT * FROM a) AS s JOIN c ON c.k = s.x;",
    "SELECT a.x, b.p FROM a, b WHERE a.y = b.p LIMIT 3;",
    "SELECT a.x, b.p FROM a, b WHERE a.y = b.p LIMIT 2 OFFSET 2;",
    "SELECT * FROM a RIGHT JOIN b ON a.y = b.p LIMIT 6 OFFSET 2;",
    "SELECT a.x, b.p FROM a, b ORDER BY b.p, a.x DESC LIMIT 5;",
    "SELECT a.rowid, b.rowid, c.rowid FROM a, b, c WHERE a.x = c.k AND b.p = a.y;",
];

#[test]
fn joins_match_the_pin() {
    let base = pin_file("joins", S);
    corpus(&base, "j", &strict(), JOINS);
}

const AGGS: &[&str] = &[
    "SELECT count(*) FROM a;",
    "SELECT count(*), sum(y), avg(y), min(z), max(z), total(y), count(y) FROM a;",
    "SELECT count(*), sum(m), max(m), m FROM d;",
    "SELECT count(*) FROM a WHERE 0;",
    "SELECT max(r), q FROM b;",
    "SELECT min(y), x, z FROM a;",
    "SELECT min(r) FILTER (WHERE p > 1), q FROM b;",
    "SELECT y, count(*) FROM a GROUP BY y;",
    "SELECT q, count(*) FROM b GROUP BY q;",
    "SELECT k, count(*), sum(v) FROM g GROUP BY k;",
    "SELECT k, group_concat(v, '-') FROM g GROUP BY k ORDER BY k DESC;",
    "SELECT k COLLATE NOCASE, count(*) FROM g GROUP BY k COLLATE NOCASE;",
    "SELECT p, count(*) FROM b GROUP BY p HAVING count(*) > 1;",
    "SELECT p, sum(r) FROM b GROUP BY p HAVING p > 1 ORDER BY 2 DESC;",
    "SELECT p, max(r), q FROM b GROUP BY p;",
    "SELECT p, min(r), q FROM b GROUP BY p ORDER BY p DESC;",
    "SELECT group_concat(z) FROM a;",
    "SELECT y, group_concat(x, '-') FROM a GROUP BY y;",
    "SELECT count(DISTINCT y), sum(DISTINCT p), group_concat(DISTINCT q) FROM a, b;",
    "SELECT a.y, count(b.p) FROM a LEFT JOIN b ON a.y = b.p GROUP BY a.y;",
    "SELECT y, count(*) FROM a GROUP BY y ORDER BY y DESC;",
    "SELECT y, count(*) FROM a GROUP BY y LIMIT 2 OFFSET 1;",
    "SELECT y, count(*) FROM a GROUP BY y ORDER BY count(*) DESC, y;",
    "SELECT count(*);",
    "SELECT sum(1), max(2), group_concat('x');",
    "SELECT count(*) WHERE 0;",
    "SELECT count(*) FROM a LIMIT 0;",
    "SELECT count(*) FROM a LIMIT 1 OFFSET 1;",
    "SELECT x, y FROM a GROUP BY 2 HAVING x > 1;",
    "SELECT y FROM a GROUP BY y HAVING 0;",
    "SELECT sum(y) FROM a HAVING sum(y) > 100;",
    "SELECT count(*), y FROM a;",
    "SELECT k, max(v), min(v) FROM g GROUP BY k;",
    "SELECT max(x) + 1, count(*) * 2 FROM a WHERE y IS NOT NULL;",
    "SELECT DISTINCT y FROM a;",
    "SELECT DISTINCT q FROM b;",
    "SELECT DISTINCT p, q FROM b ORDER BY p;",
    "SELECT DISTINCT r FROM b;",
    "SELECT DISTINCT y FROM a LIMIT 2 OFFSET 1;",
    "SELECT DISTINCT a.y, b.r FROM a JOIN b ON b.p = a.y;",
    "SELECT DISTINCT y FROM a ORDER BY y DESC;",
    "SELECT DISTINCT k FROM g ORDER BY k;",
    "SELECT DISTINCT count(*) FROM a GROUP BY y;",
    "SELECT DISTINCT 1;",
    "SELECT group_concat(z, ',' ORDER BY z DESC) FROM a;",
    "SELECT y, group_concat(x ORDER BY x DESC) FROM a GROUP BY y;",
    "SELECT sum(x) FILTER (WHERE y > 1), count(*) FILTER (WHERE z IS NULL) FROM a;",
    "SELECT p, total(r) FILTER (WHERE r > 0) FROM b GROUP BY p;",
];

#[test]
fn aggregates_and_distinct_match_the_pin() {
    let base = pin_file("aggs", S);
    corpus(&base, "g", &strict(), AGGS);
}

const INDEXED: &[&str] = &[
    "SELECT * FROM i WHERE u = 3;",
    "SELECT * FROM i WHERE u = '3';",
    "SELECT * FROM i WHERE u IN (1, 3, 5, 3, NULL);",
    "SELECT * FROM i WHERE u IN (5, 1) ORDER BY u DESC;",
    "SELECT * FROM i WHERE u > 2 AND u < 5;",
    "SELECT * FROM i WHERE u >= 3;",
    "SELECT * FROM i WHERE u < 3 ORDER BY u DESC;",
    "SELECT * FROM i WHERE u = 3 AND w > 2;",
    "SELECT * FROM i WHERE u = 3 AND w < 4 ORDER BY w;",
    "SELECT * FROM i WHERE u = 3 ORDER BY w DESC;",
    "SELECT * FROM i WHERE u = 3 ORDER BY w;",
    "SELECT u FROM i WHERE u > 2;",
    "SELECT u, w FROM i WHERE u IN (3, 7) AND w IN (2, 5, 9);",
    "SELECT * FROM i WHERE u + w = 5;",
    "SELECT * FROM i WHERE w > 2 AND w < 8;",
    "SELECT * FROM i WHERE s = 'ab' COLLATE NOCASE;",
    "SELECT * FROM i WHERE s > 'b' COLLATE NOCASE;",
    "SELECT * FROM i WHERE u IS NULL;",
    "SELECT * FROM i WHERE u = NULL;",
    "SELECT * FROM i WHERE u = 1 OR s = 'x';",
    "SELECT * FROM i WHERE u = 7 OR u + w = 5 OR w > 8;",
    "SELECT min(u) FROM i;",
    "SELECT max(u) FROM i;",
    "SELECT max(u), s FROM i WHERE w > 1;",
    "SELECT min(w) FROM i WHERE u = 3;",
    "SELECT u, count(*) FROM i GROUP BY u;",
    "SELECT u, count(*) FROM i GROUP BY u ORDER BY u DESC;",
    "SELECT DISTINCT u FROM i;",
    "SELECT * FROM i ORDER BY u;",
    "SELECT * FROM i ORDER BY u DESC, w;",
    "SELECT u, w FROM i ORDER BY u NULLS LAST;",
    "SELECT * FROM a JOIN i ON i.u = a.x;",
    "SELECT * FROM a JOIN i ON i.u = a.y;",
    "SELECT * FROM a LEFT JOIN i ON i.u = a.x AND i.w > 2;",
    "SELECT * FROM a JOIN i ON i.u IN (a.x, a.y);",
    "SELECT * FROM a JOIN i ON i.u > a.x AND i.u < a.x + 2;",
    "SELECT a.x, i.s FROM a JOIN i ON i.s = a.z;",
    "SELECT * FROM i WHERE rowid IN (3, 1, 2, 3);",
    "SELECT * FROM i WHERE rowid = 4 OR rowid = 2;",
    "SELECT * FROM a, b WHERE b.p = a.x;",
    "SELECT * FROM b, a WHERE b.p = a.y AND b.q = a.z;",
    "SELECT * FROM i WHERE u = 3 LIMIT 2 OFFSET 1;",
    "SELECT * FROM i INDEXED BY i_uw WHERE u = 3;",
    "SELECT * FROM i NOT INDEXED WHERE u = 3;",
];

#[test]
fn index_access_matches_the_pin() {
    let base = pin_file("idx", S);
    corpus(&base, "i", &strict(), INDEXED);
}

/// Failing statements: the rows before the failing one come first,
/// as on the pin — a RIGHT JOIN's main loop, a GROUP BY's earlier
/// groups (and with ORDER BY its own terms, the groups in that
/// order), a streamed join's rows; then conversion and aggregate
/// errors.
const ERRORS: &[&str] = &[
    "SELECT a.x, abs(b.p) FROM a RIGHT JOIN b ON b.p = a.x;",
    "SELECT a.x, abs(c.k - 9223372036854775807 - 2) FROM a FULL JOIN c ON c.k = a.x;",
    "SELECT a.x, abs(b.r) FROM a LEFT JOIN b ON b.p = a.y + 100;",
    "SELECT k, sum(v) FROM g2 GROUP BY k;",
    "SELECT k, sum(v) FROM g2 GROUP BY k ORDER BY k DESC;",
    "SELECT k, sum(v) FROM g2 GROUP BY k ORDER BY sum(v);",
    "SELECT sum(v) FROM g2;",
    "SELECT a.x, c.k FROM a, c WHERE abs(c.k - a.x - 9223372036854775807) > 0;",
    "SELECT DISTINCT abs(v) FROM g2;",
    "SELECT * FROM a, c LIMIT 'x';",
    "SELECT count(*) FROM a LIMIT 1 OFFSET 'y';",
    "SELECT x FROM a GROUP BY x HAVING abs(-9223372036854775808) > 0;",
    "SELECT * FROM a JOIN c ON c.k = abs(-9223372036854775808);",
];

const G2: &str = "CREATE TABLE g2(k, v);     INSERT INTO g2 VALUES (1, 1), (2, 2), (3, 9223372036854775807), (3, 1), (4, 1), (4, -9223372036854775808);";

#[test]
fn errors_follow_the_rows_before_them() {
    let base = pin_file("err", &format!("{S} {G2}"));
    corpus(&base, "e", &strict(), ERRORS);
}

/// A bigger file at 512-byte pages: indexes and tables several
/// B-tree levels high, for the restart corpus.
fn big_setup() -> String {
    let mut s = String::from(
        "PRAGMA page_size = 512; CREATE TABLE big(id INTEGER PRIMARY KEY, k, t TEXT, r REAL); \
         CREATE INDEX big_k ON big(k); CREATE INDEX big_tk ON big(t, k DESC); \
         CREATE TABLE sm(id INTEGER PRIMARY KEY, k); BEGIN;",
    );
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 1..=3000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let k = x % 400;
        let t = ["alpha", "beta", "gamma", "delta", "eps"][(x % 5) as usize];
        s += &format!(
            "INSERT INTO big VALUES ({i}, {k}, '{t}{}', {}.5);",
            x % 37,
            x % 1000
        );
    }
    for i in 1..=60u64 {
        s += &format!("INSERT INTO sm VALUES ({i}, {});", (i * 7) % 400);
    }
    s += "COMMIT;";
    s
}

const BIG: &[&str] = &[
    "SELECT id, k FROM big WHERE k = 17;",
    "SELECT id, k FROM big WHERE k IN (3, 399, 0, 200) ORDER BY k DESC;",
    "SELECT count(*), sum(k), max(t) FROM big WHERE k BETWEEN 100 AND 120;",
    "SELECT t, count(*) FROM big WHERE t > 'gamma' GROUP BY t;",
    "SELECT sm.id, big.id FROM sm JOIN big ON big.k = sm.k WHERE sm.id < 20;",
    "SELECT sm.id, big.t FROM sm LEFT JOIN big ON big.id = sm.k * 3;",
    "SELECT sm.id, count(big.id) FROM sm LEFT JOIN big ON big.k = sm.k GROUP BY sm.id;",
    "SELECT k FROM big WHERE t = 'beta12' ORDER BY k DESC;",
    "SELECT DISTINCT k FROM big WHERE k < 30;",
    "SELECT min(k), max(k) FROM big;",
    "SELECT big.id FROM sm RIGHT JOIN big ON big.id = sm.k WHERE big.id > 2990;",
];

#[test]
fn big_tables_match_the_pin() {
    let base = pin_file("big", &big_setup());
    corpus(&base, "b", &strict(), BIG);
}

/// Every corpus again with a page cache of one frame, the smallest
/// the engine allows (the big file is three levels high): nearly
/// every seek, step and row read misses, the page is read and the
/// opcode runs again — same rows.
#[test]
fn restart_with_the_smallest_cache() {
    let cfg = strict().cache_pages(1);
    let base = pin_file("rs", &format!("{S} {G2}"));
    corpus(&base, "rsj", &cfg, JOINS);
    corpus(&base, "rsg", &cfg, AGGS);
    corpus(&base, "rsi", &cfg, INDEXED);
    corpus(&base, "rse", &cfg, ERRORS);
    let big = pin_file("rsbig", &big_setup());
    corpus(&big, "rsb", &cfg, BIG);
}

// ---------------- the planner suites' corpora, strict ----------------

/// `tests/multi_index_v17.rs`' schema.
const MI: &str = "CREATE TABLE t(a, b, c, d); \
     CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b); CREATE INDEX tbc ON t(b, c); \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<60) \
     INSERT INTO t SELECT i%7, i%5, i%3, i FROM r; \
     INSERT INTO t VALUES (NULL, NULL, NULL, 100), ('x', 'y', 'z', 101); \
     CREATE TABLE u(x INTEGER PRIMARY KEY, y, z); CREATE INDEX uy ON u(y); \
     INSERT INTO u SELECT d, a, b FROM t WHERE d < 40;";

/// Its MULTI-INDEX OR queries (those without a subquery or a WITHOUT
/// ROWID table, which other units claim).
const MI_OR: &[&str] = &[
    "SELECT d FROM t WHERE a=1 OR b=2",
    "SELECT * FROM t WHERE a=1 OR b>3",
    "SELECT d FROM t WHERE a IN (1,2) OR b=2",
    "SELECT d FROM t WHERE a=1 OR a=3 OR b=2",
    "SELECT d FROM t WHERE (a=1 AND d>3) OR b=2",
    "SELECT d FROM t WHERE a=1 OR (b=2 AND c=1)",
    "SELECT d FROM t WHERE (a=1 AND b=2) OR (c=1 AND b=3)",
    "SELECT d FROM t WHERE a<1 OR a>5",
    "SELECT d FROM t WHERE a=1 OR a>5",
    "SELECT d FROM t WHERE a IS NULL OR b=2",
    "SELECT d FROM t WHERE a=1 OR b BETWEEN 1 AND 2",
    "SELECT d FROM t WHERE (a=1 OR b=2) AND (c=1 OR d=2)",
    "SELECT d FROM t WHERE a=1 OR rowid=5",
    "SELECT d FROM t WHERE a=1 OR d+1=2",
    "SELECT d FROM t WHERE a=1 OR c=2",
    "SELECT * FROM t WHERE (a=1 OR b=2) AND c=1",
    "SELECT * FROM t WHERE (a=1 OR b=2) AND c>1",
    "SELECT * FROM t WHERE (a=1 OR b=2) LIMIT 3",
    "SELECT * FROM t WHERE (a=1 OR b=2) ORDER BY a",
    "SELECT DISTINCT a FROM t WHERE a=1 OR b=2",
    "SELECT count(*) FROM t WHERE a=1 OR b=2",
    "SELECT a, b FROM t WHERE a=1 OR b=2",
    "SELECT b FROM t WHERE b=1 OR b>3",
    "SELECT * FROM t WHERE a='x' OR b='y'",
    "SELECT * FROM u WHERE x=3 OR y=2",
    "SELECT * FROM u WHERE x<3 OR y=2",
    "SELECT d FROM t WHERE a<3 OR a=3",
    "SELECT d FROM t WHERE (a>=2 AND a<=4) OR (a>=2 AND a<=4)",
    "SELECT d FROM t INDEXED BY ta WHERE a=1 OR b=2",
    "SELECT d FROM t NOT INDEXED WHERE a=1 OR b=2",
];

const MI_JOIN: &str = "CREATE TABLE t(a INTEGER, b INTEGER, c INTEGER); CREATE INDEX tb ON t(b, a); \
     CREATE TABLE u(x INTEGER, y INTEGER); CREATE INDEX ux ON u(x); CREATE INDEX uy ON u(y); \
     CREATE TABLE v(p INTEGER, q INTEGER); CREATE INDEX vp ON v(p); \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<30) \
     INSERT INTO t SELECT i%6, i%4, i FROM r; \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<25) \
     INSERT INTO u SELECT i%5, i%3 FROM r; \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<20) \
     INSERT INTO v SELECT i%7, i FROM r;";

const MI_JOIN_Q: &[&str] = &[
    "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.a = u.y",
    "SELECT t.c, u.rowid FROM u, t WHERE t.b = u.x OR t.b = u.y",
    "SELECT t.c, u.rowid FROM u, t WHERE t.c = u.x OR t.c = u.y",
    "SELECT t.c, u.rowid FROM u, t WHERE (t.c = u.x OR t.c = u.y) AND t.b=1",
    "SELECT t.c, v.q FROM t, v WHERE v.p = t.a OR v.p = t.b",
    "SELECT t.c, v.q FROM t, v WHERE t.a = v.p OR t.b = v.p",
    "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.a = u.x",
    "SELECT t.c, u.rowid FROM u, t WHERE t.a = u.x OR t.b = u.y",
    "SELECT t.c, u.rowid FROM u, t WHERE (t.a = u.x OR t.a = u.y) AND t.b = 3",
    "SELECT t.c, u.rowid FROM t LEFT JOIN u ON (u.x = t.a OR u.y = t.b)",
    "SELECT t.c, v.q FROM t JOIN v ON v.p = t.a WHERE t.b = 1 OR t.a = 2",
    "SELECT t.c, u.rowid FROM t, u WHERE u.rowid = t.c AND (u.x = 1 OR u.y = 2)",
    "SELECT max(u.y), t.c FROM t, u WHERE u.x = t.a OR u.y = t.b",
];

const MM: &str = "CREATE TABLE t(a, b, c, d); CREATE INDEX ta ON t(a); \
     CREATE INDEX tbd ON t(b DESC); \
     INSERT INTO t VALUES (NULL,NULL,1,1),(3,3,1,2),(1,1,1,3),(3,3,1,4),(1,1,1,5), \
     (NULL,NULL,1,6),('x','x',1,7),('x','x',1,8),(X'41',X'41',1,9),(X'41',X'41',1,10); \
     CREATE TABLE n(a, d); CREATE INDEX na ON n(a); \
     INSERT INTO n VALUES (NULL,1),(NULL,2),(NULL,3); \
     CREATE TABLE m(a, d); INSERT INTO m VALUES (NULL,1),(NULL,2),(NULL,3); \
     CREATE TABLE k(a, b, d); CREATE INDEX kb ON k(b, a); \
     INSERT INTO k VALUES (NULL,1,1),(NULL,1,2),(5,1,3),(5,1,4),(NULL,1,5),(2,2,6),(NULL,2,7); \
     CREATE TABLE kd(a, d); CREATE INDEX kdd ON kd(a DESC); \
     INSERT INTO kd VALUES (NULL,1),(NULL,2),(4,3),(4,4),(NULL,5); \
     CREATE TABLE e(a, b);";

/// `multi_index_v17`'s min() / max() lookups.
const MM_Q: &[&str] = &[
    "SELECT max(a), d FROM t",
    "SELECT min(a), d FROM t",
    "SELECT max(b), d FROM t",
    "SELECT min(b), d FROM t",
    "SELECT max(a), d FROM t WHERE c=1",
    "SELECT max(a), d FROM t WHERE d>3",
    "SELECT min(a) FROM t WHERE a IS NULL",
    "SELECT max(a) FROM t WHERE a < 'x'",
    "SELECT max(a) FROM t WHERE a IN (1,3)",
    "SELECT max(a) FROM t WHERE a>2",
    "SELECT min(a) FROM t WHERE a>=1 AND a<'z'",
    "SELECT min(rowid), d FROM t",
    "SELECT max(rowid), d FROM t",
    "SELECT max(a)+1 FROM t",
    "SELECT max(a)+max(a) FROM t",
    "SELECT max(abs(a)) FROM t",
    "SELECT max(+a) FROM t",
    "SELECT max(a COLLATE nocase) FROM t",
    "SELECT max(a), min(a) FROM t",
    "SELECT max(a), count(*) FROM t",
    "SELECT max(a) FROM t GROUP BY c",
    "SELECT max(a) FROM t HAVING max(a)>1",
    "SELECT max(a) FILTER (WHERE d>1) FROM t",
    "SELECT max(DISTINCT a) FROM t",
    "SELECT DISTINCT max(a) FROM t",
    "SELECT max(a) FROM t LIMIT 1",
    "SELECT max(a) FROM t ORDER BY 1",
    "SELECT max(a) FROM t AS q",
    "SELECT min(a) FROM t INDEXED BY tbd",
    "SELECT min(a) FROM t NOT INDEXED",
    "SELECT min(a), d FROM n",
    "SELECT max(a), d FROM n",
    "SELECT min(a), d FROM m",
    "SELECT max(a), d FROM m",
    "SELECT min(a), d FROM n WHERE d>1",
    "SELECT max(a), d FROM n WHERE d>1",
    "SELECT min(a), d FROM k WHERE b=1",
    "SELECT max(a), d FROM k WHERE b=1",
    "SELECT min(a), d FROM k WHERE b=2 AND a IS NULL",
    "SELECT min(a), d FROM k WHERE b IN (1,2)",
    "SELECT max(a), d FROM k WHERE b IN (1,2)",
    "SELECT max(DISTINCT a) FROM k WHERE b=1",
    "SELECT min(a), d FROM kd",
    "SELECT max(a), d FROM kd",
    "SELECT min(a), d FROM kd WHERE d<3",
    "SELECT max(a), d FROM kd WHERE d<3",
    "SELECT max(a) FROM e",
    "SELECT max(t.a), e.b FROM t, e",
    "SELECT max(t.a) FROM t LEFT JOIN e ON 1",
    "SELECT max(t.a), k.d FROM t, k WHERE k.b=t.c",
    "SELECT min(k.a), t.d FROM t, k WHERE k.b=t.c",
];

const SKIP: &str = "CREATE TABLE t(a, b, c, d); CREATE INDEX tab ON t(a, b); \
     WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<2000) \
     INSERT INTO t SELECT i%3, i, i%50, i FROM r;";

/// `multi_index_v17`'s skip-scan and IN-list queries.
const SKIP_Q: &[&str] = &[
    "SELECT * FROM t WHERE b=5",
    "SELECT d FROM t WHERE b>5 AND b<9",
    "SELECT a, b FROM t WHERE b=5",
    "SELECT * FROM t WHERE b IN (5, 6)",
    "SELECT * FROM t WHERE a=1 AND b IN (4, 7, 10)",
    "SELECT * FROM t WHERE c=5",
    "SELECT * FROM t WHERE b=5 ORDER BY a DESC",
    "SELECT a FROM t WHERE b<10 ORDER BY a",
    "SELECT a, b FROM t WHERE b BETWEEN 5 AND 30 ORDER BY a DESC",
    "SELECT * FROM t WHERE b=5 OR b=7",
    "SELECT count(*) FROM t WHERE b>1990",
];

const JO: &str = "CREATE TABLE a(x INTEGER PRIMARY KEY, y, z TEXT); \
     CREATE TABLE b(p, q INT, r); CREATE INDEX bq ON b(q); \
     CREATE TABLE c(s, t, u NOT NULL); \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<30) \
     INSERT INTO a SELECT i, i%7, 'z'||(i%4) FROM n; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<40) \
     INSERT INTO b SELECT i%9, i%11, NULLIF(i%5, 0) FROM n; \
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<25) \
     INSERT INTO c SELECT i%6, i%13, i FROM n;";

/// `join_order_v17`'s curated joins: reordering, automatic indexes
/// (partial ones too) and their Bloom filters, LEFT / CROSS JOIN
/// constraints, constant propagation, ORDER BY / GROUP BY / DISTINCT.
const JO_Q: &[&str] = &[
    "SELECT * FROM a, b WHERE a.x = b.p",
    "SELECT a.z, b.r FROM b, a WHERE b.q = a.x AND b.r > 2",
    "SELECT * FROM a, c WHERE c.t = a.y",
    "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y",
    "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y WHERE c.u > 3",
    "SELECT a.x, c.u FROM a JOIN c ON c.s = a.y WHERE 1",
    "SELECT * FROM a, c WHERE c.t = a.y AND c.s = a.x",
    "SELECT * FROM c LEFT JOIN a ON a.x = c.s",
    "SELECT * FROM a LEFT JOIN c ON c.t = a.x",
    "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE a.y = 3",
    "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE a.y IS NOT NULL AND c.s = 1",
    "SELECT * FROM c LEFT JOIN a ON a.x = c.s WHERE c.s = 1 AND a.y IS NOT NULL",
    "SELECT * FROM b CROSS JOIN a WHERE a.x = b.q",
    "SELECT * FROM b CROSS JOIN c, a WHERE a.x = b.q AND c.s = a.y",
    "SELECT c.s, a.z FROM c, a WHERE a.x = c.t AND c.u IS NOT NULL",
    "SELECT * FROM a, b WHERE a.x = b.q AND b.q = 4",
    "SELECT a.x, b.r FROM a, b WHERE b.q = a.x ORDER BY a.x DESC",
    "SELECT b.q, count(*) FROM a, b WHERE b.q = a.x GROUP BY b.q",
    "SELECT DISTINCT a.y, c.s FROM a, c WHERE c.t = a.x",
    "SELECT * FROM a, b, c WHERE b.q = a.x AND c.t = b.p LIMIT 5",
];

/// `join_order_v17`'s statistics corpus: Bloom filters on index and
/// rowid lookups the pin builds from `sqlite_stat1`.
const JO_STAT_Q: &[&str] = &[
    "SELECT * FROM b, c WHERE c.s = b.p AND c.t > 2",
    "SELECT * FROM b, a WHERE a.x = b.q AND a.y > 1",
    "SELECT b.r, c.u FROM a, b, c WHERE b.q = a.y AND c.s = a.y AND c.t > 1 AND b.r > 1",
    "SELECT * FROM a, c WHERE c.t = a.y",
];

#[test]
fn planner_suites_run_strict_on_the_vm() {
    let cfg = strict();
    corpus(&pin_file("mi", MI), "mi", &cfg, MI_OR);
    corpus(&pin_file("mij", MI_JOIN), "mij", &cfg, MI_JOIN_Q);
    corpus(&pin_file("mm", MM), "mm", &cfg, MM_Q);
    corpus(&pin_file("sk0", SKIP), "sk0", &cfg, SKIP_Q);
    corpus(
        &pin_file("sk1", &format!("{SKIP} ANALYZE;")),
        "sk1",
        &cfg,
        SKIP_Q,
    );
    corpus(
        &pin_file(
            "sk2",
            &format!(
                "{SKIP} ANALYZE; DELETE FROM sqlite_stat1; \
                 INSERT INTO sqlite_stat1 VALUES('t', 'tab', '2000 17 1');"
            ),
        ),
        "sk2",
        &cfg,
        SKIP_Q,
    );
    corpus(&pin_file("jo", JO), "jo", &cfg, JO_Q);
    corpus(
        &pin_file("jo1", &format!("{JO} ANALYZE;")),
        "jo1",
        &cfg,
        JO_Q,
    );
    let stat = format!("{JO} CREATE INDEX cs ON c(s); ANALYZE;");
    corpus(&pin_file("jos", &stat), "jos", &cfg, JO_STAT_Q);
    // The restart rule under the minimum cache, for every one of them.
    let small = strict().cache_pages(1);
    corpus(&pin_file("mi-r", MI), "mi-r", &small, MI_OR);
    corpus(&pin_file("mij-r", MI_JOIN), "mij-r", &small, MI_JOIN_Q);
    corpus(&pin_file("mm-r", MM), "mm-r", &small, MM_Q);
    corpus(
        &pin_file("sk1-r", &format!("{SKIP} ANALYZE;")),
        "sk1-r",
        &small,
        SKIP_Q,
    );
    corpus(
        &pin_file("jo1-r", &format!("{JO} ANALYZE;")),
        "jo1-r",
        &small,
        JO_Q,
    );
    corpus(&pin_file("jos-r", &stat), "jos-r", &small, JO_STAT_Q);
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

/// A random value: NULL, a small integer, a real, a text.
fn gen_val(r: &mut Rng) -> String {
    match r.n(10) {
        0 => "NULL".into(),
        1 => format!("{}.5", r.n(5)),
        2 => format!("'{}'", r.pick(&["x", "X", "y", "1", "ab"])),
        _ => r.n(6).to_string(),
    }
}

/// Four tables of random rows (one may be empty) with random
/// indexes — plain, two-column with a DESC column, NOCASE,
/// expression and partial ones — and, sometimes, the pin's ANALYZE.
fn gen_schema(r: &mut Rng) -> String {
    let mut s = String::from(
        "CREATE TABLE t1(a INTEGER PRIMARY KEY, b, c TEXT); \
         CREATE TABLE t2(a, b INT, c TEXT COLLATE NOCASE); \
         CREATE TABLE t3(a, b, c); CREATE TABLE t4(a, b, c REAL);",
    );
    let idx = [
        "CREATE INDEX t2b ON t2(b);",
        "CREATE INDEX t2bc ON t2(b, c DESC);",
        "CREATE INDEX t2c ON t2(c);",
        "CREATE INDEX t3a ON t3(a DESC);",
        "CREATE INDEX t3ab ON t3(a, b);",
        "CREATE INDEX t3e ON t3(a + b);",
        "CREATE INDEX t3p ON t3(c) WHERE b > 2;",
        "CREATE INDEX t4b ON t4(b);",
        "CREATE INDEX t1b ON t1(b, c);",
        "CREATE UNIQUE INDEX t4ab ON t4(a, b);",
    ];
    for i in idx.iter() {
        if r.chance(40) {
            s += i;
        }
    }
    for (t, ipk) in [("t1", true), ("t2", false), ("t3", false), ("t4", false)] {
        let n = if r.chance(10) { 0 } else { r.n(14) };
        for j in 0..n {
            let first = if ipk { (j + 1).to_string() } else { gen_val(r) };
            s += &format!(
                "INSERT OR IGNORE INTO {t} VALUES ({first}, {}, {});",
                gen_val(r),
                gen_val(r)
            );
        }
    }
    if r.chance(25) {
        s += "ANALYZE;";
    }
    s
}

/// A random SELECT over 1 to 3 of the tables: any join kind, ON /
/// USING / NATURAL, WHERE terms the access planner uses (equality, IN
/// lists, ranges, IS NULL, OR, expression and partial index terms),
/// aggregates with GROUP BY / HAVING, DISTINCT, ORDER BY, LIMIT /
/// OFFSET.
fn gen_query(r: &mut Rng) -> String {
    let k = 1 + r.n(3) as usize;
    let mut tabs: Vec<&str> = vec!["t1", "t2", "t3", "t4"];
    for i in (1..tabs.len()).rev() {
        let j = r.n(i as u64 + 1) as usize;
        tabs.swap(i, j);
    }
    let used = &tabs[..k];
    let cols = ["a", "b", "c"];
    let col = |r: &mut Rng, n: usize| format!("x{}.{}", r.n(n as u64), cols[r.n(3) as usize]);
    let mut from = format!("{} x0", used[0]);
    for (i, t) in used.iter().enumerate().skip(1) {
        let op = r.pick(&[
            ",",
            " JOIN ",
            " LEFT JOIN ",
            " RIGHT JOIN ",
            " FULL JOIN ",
            " CROSS JOIN ",
            " NATURAL JOIN ",
            " LEFT JOIN ",
            " JOIN ",
        ]);
        let mut on = String::new();
        match op.trim() {
            "JOIN" | "LEFT JOIN" | "RIGHT JOIN" | "FULL JOIN" => {
                if r.chance(15) {
                    on = format!(" USING ({})", cols[r.n(3) as usize]);
                } else {
                    let c = cols[r.n(3) as usize];
                    let rel = r.pick(&["=", "=", "=", "<", ">="]);
                    on = format!(" ON x{i}.{c}{rel}{}", col(r, i));
                    if r.chance(20) {
                        on += &format!(" AND x{i}.{} > {}", cols[r.n(3) as usize], r.n(4));
                    }
                }
            }
            _ => {}
        }
        from += &format!("{op}{t} x{i}{on}");
    }
    let mut conds: Vec<String> = Vec::new();
    for _ in 0..r.n(3) {
        let c = col(r, k);
        let v = r.n(6);
        conds.push(match r.n(12) {
            0 => format!("{c} = {v}"),
            1 => format!("{c} IN ({v}, {}, 'x', NULL)", r.n(6)),
            2 => format!("{c} > {v}"),
            3 => format!("{c} <= {v}"),
            4 => format!("{c} IS NULL"),
            5 => format!("{c} BETWEEN 1 AND {v}"),
            6 => format!("({c} = {v} OR {} = {})", col(r, k), r.n(6)),
            7 => format!("{c} = {}", col(r, k)),
            8 => format!("x0.a + x0.b = {v}"),
            9 => "x0.c = 'x' AND x0.b > 2".to_string(),
            10 => format!("{c} IS NOT NULL"),
            _ => format!("{c} = '{}'", r.pick(&["x", "X", "1"])),
        });
    }
    let mut sel: Vec<String> = Vec::new();
    for _ in 0..1 + r.n(3) {
        sel.push(col(r, k));
    }
    let mode = r.n(100);
    let mut tail = String::new();
    let mut dist = "";
    if mode < 20 {
        let g = col(r, k);
        let agg = r.pick(&[
            "count(*)",
            "sum(x0.b)",
            "min(x0.a)",
            "max(x0.c)",
            "group_concat(x0.a)",
            "count(DISTINCT x0.b)",
            "avg(x0.b)",
            "total(x0.a)",
        ]);
        sel = vec![g.clone(), agg.to_string(), col(r, k)];
        tail = format!(" GROUP BY {g}");
        if r.chance(30) {
            tail += " HAVING count(*) > 1";
        }
        if r.chance(40) {
            tail += &format!(" ORDER BY {g}{}", r.pick(&["", " DESC", " NULLS LAST"]));
        }
    } else if mode < 30 {
        let agg = r.pick(&[
            "count(*)",
            "sum(x0.b)",
            "min(x0.b)",
            "max(x0.a)",
            "count(x0.c)",
        ]);
        sel = vec![agg.to_string(), col(r, k)];
    } else if mode < 40 {
        dist = "DISTINCT ";
    }
    if mode >= 40 && r.chance(45) {
        let o = col(r, k);
        tail = format!(
            " ORDER BY {o}{}",
            r.pick(&["", " DESC", " DESC NULLS FIRST", ""])
        );
        if r.chance(30) {
            tail += &format!(", {}", col(r, k));
        }
    } else if (30..40).contains(&mode) && r.chance(40) {
        tail = " ORDER BY 1".into();
    }
    let mut q = format!("SELECT {dist}{} FROM {from}", sel.join(", "));
    if !conds.is_empty() {
        q += &format!(" WHERE {}", conds.join(" AND "));
    }
    q += &tail;
    if r.chance(15) {
        q += &format!(" LIMIT {}", 1 + r.n(4));
        if r.chance(40) {
            q += &format!(" OFFSET {}", r.n(3));
        }
    }
    q + ";"
}

/// The rows as a sorted multiset (the pin's order among ties may
/// differ from today's executor, which the VM ports).
fn sorted(rows: &[String]) -> Vec<String> {
    let mut v = rows.to_vec();
    v.sort();
    v
}

/// Generated queries (see `gen_query`): on the VM (where it covers
/// them) exactly today's executor's rows, order and success, and the
/// pin's rows; the corpus must mostly run on the VM.
#[test]
fn generated_queries_match_the_executor_and_the_pin() {
    let seeds: u64 = std::env::var("HIPP_VMJ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let mut bad = Vec::new();
    let (mut total, mut on, mut fixed, mut legacy_off) = (0usize, 0usize, 0usize, 0usize);
    for seed in 0..seeds {
        let mut r = Rng(0x5eed_0000 + seed * 7919);
        let base = pin_file(&format!("gen{seed}"), &gen_schema(&mut r));
        for qi in 0..12 {
            let q = gen_query(&mut r);
            let tag = format!("vmj17-g{seed}-{qi}");
            let (vm, on_vm, ve) =
                hipp_run(&copy_of(&base, &format!("{tag}-v")), Config::default(), &q);
            let (old, _, oe) = hipp_run(
                &copy_of(&base, &format!("{tag}-l")),
                Config::default().vm_off(true),
                &q,
            );
            let pin = pin_run(&copy_of(&base, &format!("{tag}-p")), &q);
            total += 1;
            if !on_vm {
                if std::env::var("HIPP_VMJ_SHOW").is_ok() {
                    eprintln!("fallback: {q}");
                }
                continue;
            }
            on += 1;
            // Today's executor exactly (its rows, their order, its
            // success), where its rows are the pin's (as a set: the
            // order among ties may differ); else the pin's rows in the
            // pin's order — today's materialising RIGHT JOIN and
            // GROUP BY return no row before an error, and a GROUP BY's
            // LIMIT can stop at the wrong groups (see STATUS.md).
            let old_pin = old.ok == pin.ok && sorted(&old.rows) == sorted(&pin.rows);
            let ok = if vm == old {
                true
            } else {
                fixed += 1;
                !old_pin && vm == pin
            };
            if !old_pin && vm == old {
                legacy_off += 1;
            }
            if !ok {
                bad.push(format!(
                    "seed {seed}: {q}\n  vm  : {vm:?} {ve:?}\n  old : {old:?} {oe:?}\n  pin : {pin:?}"
                ));
            }
            // A strict run of a claimed query never falls back.
            let (_, s_on, _) = hipp_run(&copy_of(&base, &format!("{tag}-s")), strict(), &q);
            if !s_on {
                bad.push(format!("strict fell back: {q}"));
            }
        }
    }
    eprintln!(
        "{on} of {total} on the VM; {fixed} where it gives the pin's rows and today's \
         executor does not; {legacy_off} where both give today's (not the pin's) rows"
    );
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
    assert_eq!(on, total, "generated queries fell back");
}
