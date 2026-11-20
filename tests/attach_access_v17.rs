//! v0.17 index and rowid access into attached and TEMP tables (leaf
//! `attach`, with `expr_index` / `planner` planning them),
//! differentially vs the pinned oracle (SQLite 3.54.1, live).
//!
//! Probed on the pin before implementation: a statement of main that
//! reads a table of an attached database or the TEMP database plans
//! it as it plans a main table — rowid seeks (`SEARCH aux.t USING
//! INTEGER PRIMARY KEY (rowid=?)`, ranges, IN lists), index seeks
//! (`USING INDEX tb (b=?)`, `COVERING INDEX`, partial and expression
//! indexes, collations), index order for ORDER BY, `INDEXED BY` /
//! `NOT INDEXED`, the IN operator reading the index (`USING INDEX tb
//! FOR IN-OPERATOR`), MULTI-INDEX OR, automatic indexes, Bloom filters
//! and the join order (a join may be driven from the TEMP or attached
//! table). hipp now does the same through the other database's own
//! catalog and pager (`Connection::owner`, `pager_of`); before, such a
//! table was read whole and scanned once per outer row (`SCAN aux.t`,
//! FROM order), and a 3,000 x 3,000 correlated lookup took seconds.
//!
//! Every script runs on both engines in twin directories, statement
//! by statement: rows (EXPLAIN QUERY PLAN as the pin shell's tree),
//! and whether the statement failed, must agree. The generated corpus
//! also runs each script with every table in main: a statement whose
//! all-main twin already differs on hipp (a planner divergence of
//! main tables, outside this unit) is compared as a set of rows.

#![cfg(all(feature = "attach", feature = "planner"))]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fresh_dir(tag: &str) -> PathBuf {
    let p = common::temp_db(&format!("v17acc-{tag}"));
    p.parent().expect("dir").to_path_buf()
}

fn value(q: &hipp::Statement<'_, FileStorage>, i: usize, line: &mut String) {
    match q.column_value(i) {
        Some(hipp::Value::Null) | None => {}
        Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
        Some(hipp::Value::Real(f)) => {
            let mut t = hipp::arena::AVec::new();
            hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
            line.push_str(&String::from_utf8_lossy(t.as_slice()));
        }
        Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
            line.push_str(&String::from_utf8_lossy(b));
        }
    }
}

/// The pin shell's tree of EXPLAIN QUERY PLAN rows (id, parent,
/// detail).
fn tree(rows: &[(i64, i64, String)], out: &mut Vec<String>) {
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
    if !rows.is_empty() {
        out.push("QUERY PLAN".to_string());
        level(rows, 0, &mut String::new(), out);
    }
}

/// One statement's answer: its output lines, then `!` when it failed.
type Block = Vec<String>;

async fn run_on(db: &mut Connection<FileStorage>, stmts: &[String]) -> Vec<Block> {
    let mut out = Vec::new();
    for sql in stmts.iter() {
        let mut b = Vec::new();
        let eqp = sql.starts_with("EXPLAIN QUERY PLAN");
        let mut plan = Vec::new();
        match db.prepare(sql).await {
            Err(_) => b.push("!".to_string()),
            Ok(mut q) => loop {
                match q.step().await {
                    Ok(Step::Row) if eqp => {
                        let id = q.column_i64(0).unwrap_or(0);
                        let parent = q.column_i64(1).unwrap_or(0);
                        let d =
                            String::from_utf8_lossy(q.column_bytes(3).unwrap_or(b"")).to_string();
                        plan.push((id, parent, d));
                    }
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        for i in 0..q.column_count() {
                            if i > 0 {
                                l.push('|');
                            }
                            value(&q, i, &mut l);
                        }
                        b.push(l);
                    }
                    Ok(Step::Done) => break,
                    Err(_) => {
                        b.push("!".to_string());
                        break;
                    }
                }
            },
        }
        if eqp {
            let failed = b.last().is_some_and(|l| l == "!");
            b.clear();
            tree(&plan, &mut b);
            if failed {
                b.push("!".to_string());
            }
        }
        out.push(b);
    }
    out
}

fn hipp_run(dir: &Path, stmts: &[String]) -> Vec<Block> {
    block_on(async {
        let fs = FileStorage::open(dir.join("main.db")).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let r = run_on(&mut db, stmts).await;
        db.close().await.expect("close");
        r
    })
}

/// The pin's shell over stdin, one statement per line, each after a
/// marker row: per statement, its output lines and whether it failed.
fn oracle_run(dir: &Path, stmts: &[String]) -> Vec<Block> {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(dir.join("main.db"))
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    let mut input = String::new();
    for (i, s) in stmts.iter().enumerate() {
        assert!(!s.contains('\n'), "one statement per line: {s}");
        input.push_str(&format!("SELECT '#~{i}';\n{s};\n"));
    }
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let mut blocks: Vec<Block> = vec![Vec::new(); stmts.len()];
    let mut cur: Option<usize> = None;
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(n) = l.strip_prefix("#~").and_then(|n| n.parse::<usize>().ok()) {
            cur = Some(n);
            continue;
        }
        if let Some(b) = cur.and_then(|c| blocks.get_mut(c)) {
            b.push(l.to_string());
        }
    }
    for l in String::from_utf8_lossy(&out.stderr).lines() {
        if let Some(i) = l.find(" near line ") {
            let n: String = l[i + 11..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            // Statement k (0-based) is input line 2k + 2.
            if let Some(b) = n
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(2))
                .and_then(|n| blocks.get_mut(n / 2))
                && b.last().is_none_or(|l| l != "!")
            {
                b.push("!".to_string());
            }
        }
    }
    blocks
}

fn answers(tag: &str, stmts: &[String]) -> (Vec<Block>, Vec<Block>) {
    let h = fresh_dir(&format!("{tag}-h"));
    let o = fresh_dir(&format!("{tag}-o"));
    let stmts: Vec<String> = stmts
        .iter()
        .map(|s| s.replace("{d}", &h.to_string_lossy()))
        .collect();
    let hb = hipp_run(&h, &stmts);
    let stmts: Vec<String> = stmts
        .iter()
        .map(|s| s.replace(&*h.to_string_lossy(), &o.to_string_lossy()))
        .collect();
    let ob = oracle_run(&o, &stmts);
    (hb, ob)
}

/// Run `stmts` on twin directories: every statement's answer agrees.
fn same(tag: &str, stmts: &[String]) {
    let (hb, ob) = answers(tag, stmts);
    for (i, (h, o)) in hb.iter().zip(ob.iter()).enumerate() {
        assert_eq!(
            h,
            o,
            "{tag}: statement {} differs: {}",
            i + 1,
            stmts.get(i).map_or("", |s| s.as_str())
        );
    }
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

/// Each query twice: its plan, then its rows.
fn with_plans(setup: &str, queries: &str) -> Vec<String> {
    let mut s = lines(setup);
    for q in lines(queries) {
        s.push(format!("EXPLAIN QUERY PLAN {q}"));
        s.push(q);
    }
    s
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned(),
        "oracle is {}, not the 3.54.1 pin",
        common::oracle_version()
    );
}

const SETUP: &str = "
ATTACH '{d}/a.db' AS aux;
CREATE TABLE m(x INTEGER PRIMARY KEY, y, z);
CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c, g AS (c * 2), h AS (c + 1) STORED);
CREATE INDEX aux.tb ON t(b);
CREATE INDEX aux.tc ON t(c DESC) WHERE c > 0;
CREATE INDEX aux.tx ON t(lower(b));
CREATE INDEX aux.tg ON t(g);
CREATE TABLE aux.u(k INT UNIQUE, v);
CREATE TABLE aux.w(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TEMP TABLE tt(k INTEGER PRIMARY KEY, v, q DEFAULT 'qq');
CREATE INDEX temp.tv ON tt(v, k);
INSERT INTO m VALUES (1, 'a', 1), (2, 'B', 2), (3, 'c', 3), (4, NULL, 4), (5, 'A', -1);
INSERT INTO aux.t(a, b, c) VALUES (1, 'A', 1), (2, 'b', 2), (3, 'C', -3), (4, NULL, 4), (5, 'a', 0);
INSERT INTO aux.u VALUES (1, 'u1'), (3, 'u3'), (NULL, 'un'), (5, 'u5');
INSERT INTO aux.w VALUES (1, 'one'), (2, 'two'), (3, 'three');
INSERT INTO tt(k, v) VALUES (1, 'a'), (2, 'b'), (3, 'A');
ALTER TABLE tt ADD COLUMN r DEFAULT 7;
ALTER TABLE aux.t ADD COLUMN e DEFAULT 'ee';
INSERT INTO tt VALUES (4, 'z', 'q4', 8);
";

/// The probed forms: rowid and index seeks into attached and TEMP
/// tables from main statements, with the pin's plans.
#[test]
fn seeks_into_other_databases() {
    let q = "
SELECT (SELECT b FROM aux.t WHERE a = m.x) FROM m;
SELECT * FROM m, aux.t WHERE t.a = m.x;
SELECT m.x, t.* FROM m, aux.t WHERE t.b = m.y;
SELECT m.x, t.a, t.e FROM m, aux.t WHERE t.c = m.z AND t.c > 0;
SELECT m.x, t.a FROM m, aux.t WHERE lower(t.b) = m.y;
SELECT m.x, t.a, t.h FROM m, aux.t WHERE t.g = m.z * 2;
SELECT * FROM m, aux.u WHERE u.k = m.x;
SELECT * FROM m JOIN aux.t AS q ON q.a = m.z;
SELECT * FROM m, aux.t WHERE t.a > m.x AND t.a < m.x + 2;
SELECT * FROM m, aux.t WHERE t.a IN (m.x, m.z) ORDER BY 1, 4;
SELECT * FROM m, aux.t WHERE t.a BETWEEN m.x AND m.x + 1 AND m.y IS NOT NULL;
SELECT m.x, tt.* FROM m, tt WHERE tt.k = m.x;
SELECT m.x, tt.* FROM m, temp.tt WHERE tt.v = m.y ORDER BY tt.v, tt.k;
SELECT *, (SELECT q || r FROM tt WHERE k = m.x) FROM m;
SELECT (SELECT max(v) FROM tt WHERE v < m.y) FROM m;
SELECT * FROM aux.t, tt WHERE t.b = tt.v;
SELECT * FROM tt, aux.t WHERE t.a = tt.k AND tt.v = 'b';
SELECT * FROM m WHERE x IN (SELECT a FROM aux.t WHERE b = 'a');
SELECT * FROM m WHERE y IN (SELECT b FROM aux.t);
SELECT * FROM m WHERE x IN (SELECT k FROM tt);
SELECT * FROM m WHERE EXISTS (SELECT 1 FROM aux.t WHERE t.a = m.x AND t.c > 1);
SELECT * FROM aux.t, m WHERE t.a = m.x OR t.b = m.y;
SELECT * FROM m LEFT JOIN aux.t ON t.a = m.x LEFT JOIN tt ON tt.k = t.a;
SELECT * FROM m, aux.t INDEXED BY tc WHERE t.c > 0 AND t.a = m.x;
SELECT * FROM m, aux.t NOT INDEXED WHERE t.a = m.x;
SELECT * FROM m, tt INDEXED BY tv WHERE tt.v = m.y;
SELECT * FROM m, aux.t INDEXED BY nosuch WHERE t.a = m.x;
SELECT * FROM m, tt INDEXED BY nosuch WHERE tt.k = m.x;
SELECT m.x, count(*) FROM m, aux.t WHERE t.b = m.y GROUP BY m.x;
SELECT * FROM m, aux.t WHERE t.a = m.x ORDER BY t.a DESC LIMIT 2;
SELECT max(t.a) FROM m, aux.t WHERE t.b = m.y;
";
    same("seeks", &with_plans(SETUP, q));
}

/// Writes of main reading attached / TEMP tables through their
/// indexes, and the statement-level checks around them: a statement
/// reading an attached table locks it against DETACH for the rest of
/// the transaction even when its seeks read no row (regression: an
/// `UPDATE main.t ... FROM b.u` over an empty `t` left `b` unlocked).
#[test]
fn writes_and_transactions() {
    let s = "
UPDATE m SET z = (SELECT c FROM aux.t WHERE a = m.x) WHERE x < 4;
SELECT * FROM m;
DELETE FROM m WHERE x IN (SELECT k FROM tt WHERE v = 'b');
SELECT * FROM m;
INSERT INTO m SELECT a + 10, b, c FROM aux.t WHERE a IN (SELECT k FROM tt);
SELECT * FROM m;
UPDATE m SET y = (SELECT v FROM tt WHERE tt.v = m.y COLLATE NOCASE ORDER BY k LIMIT 1);
SELECT * FROM m;
BEGIN;
SELECT count(*) FROM m, aux.t WHERE t.a = m.x;
DETACH aux;
COMMIT;
CREATE TABLE e(x INTEGER PRIMARY KEY);
BEGIN;
SELECT * FROM e, aux.t WHERE t.a = e.x;
DETACH aux;
ROLLBACK;
BEGIN;
SELECT * FROM e WHERE x IN (SELECT a FROM aux.t WHERE a = 9);
DETACH aux;
ROLLBACK;
DETACH aux;
SELECT * FROM m, aux.t WHERE t.a = m.x;
";
    let mut v = lines(SETUP);
    v.extend(lines(s));
    same("writes", &v);
}

/// Rows read in place keep the table's defaults (columns added by
/// ALTER TABLE after the rows were written) and generated columns, a
/// covering index's entries keep the index's columns only, and a
/// WITHOUT ROWID table of another database is still read whole (its
/// plan is the honest gap STATUS.md records: rows only here).
#[test]
fn row_decoding() {
    let q = "
SELECT t.a, t.e, t.g, t.h FROM m, aux.t WHERE t.a = m.x;
SELECT t.b, t.e FROM m, aux.t WHERE t.b = m.y ORDER BY 1, 2;
SELECT tt.q, tt.r FROM m, tt WHERE tt.k = m.x;
SELECT tt.v, tt.k FROM m, tt WHERE tt.v = m.y;
";
    let mut v = with_plans(SETUP, q);
    v.push("SELECT * FROM m, aux.w WHERE w.k = m.x ORDER BY 1".to_string());
    same("decode", &v);
}

/// Plans follow the other database's schema as it changes: after DROP
/// INDEX, DROP TABLE and a re-CREATE of the table, and a TEMP index
/// dropped. (A `Statement` borrows its connection, so no DDL runs
/// while one is prepared; the read still checks the table and index
/// it planned are there, `Connection::check_src`.)
#[test]
fn schema_changes_between_statements() {
    let q = "SELECT m.x, t.a FROM m, aux.t WHERE t.b = m.y";
    let q2 = "SELECT m.x, tt.k FROM m, tt WHERE tt.v = m.y";
    let mut v = lines(SETUP);
    for step in [
        "DROP INDEX aux.tb",
        "CREATE INDEX aux.tb2 ON t(b, c)",
        "DROP TABLE aux.t",
        "CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b)",
        "INSERT INTO aux.t VALUES (1, 'a'), (2, 'zz')",
        "CREATE INDEX aux.tb ON t(b)",
        "DROP INDEX temp.tv",
        "CREATE INDEX temp.tv ON tt(v DESC)",
    ] {
        for x in [q, q2] {
            v.push(format!("EXPLAIN QUERY PLAN {x}"));
            v.push(x.to_string());
        }
        v.push(step.to_string());
    }
    same("schema", &v);
}

/// Correlated lookups and joins into 4,000-row attached and TEMP
/// tables seek instead of scanning per outer row (hipp took about
/// 15 s in a debug build for the first two before; well under a
/// second now).
#[test]
fn lookups_seek() {
    let mut s = lines(
        "
ATTACH '{d}/a.db' AS aux;
CREATE TABLE m(x INTEGER PRIMARY KEY, y);
CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b, c);
CREATE INDEX aux.tb ON t(b);
CREATE TEMP TABLE tt(k INTEGER PRIMARY KEY, v);
CREATE INDEX temp.tv ON tt(v);
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4000) INSERT INTO m SELECT i, i % 100 FROM c;
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4000) INSERT INTO aux.t SELECT i, i % 50, 'x' || i FROM c;
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4000) INSERT INTO tt SELECT i, i % 70 FROM c;
",
    );
    let queries = [
        "SELECT sum(length((SELECT c FROM aux.t WHERE a = m.x))) FROM m",
        "SELECT count(*) FROM m, tt WHERE tt.k = m.x",
        "SELECT count(*) FROM m, aux.t WHERE t.b = m.y AND t.a = m.x",
        "SELECT sum((SELECT count(*) FROM tt WHERE v = m.y)) FROM m",
    ];
    for q in queries {
        s.push(q.to_string());
    }
    let dir = fresh_dir("perf");
    let s: Vec<String> = s
        .iter()
        .map(|x| x.replace("{d}", &dir.to_string_lossy()))
        .collect();
    let t = Instant::now();
    let hb = hipp_run(&dir, &s);
    let took = t.elapsed();
    let o = fresh_dir("perf-o");
    let os: Vec<String> = s
        .iter()
        .map(|x| x.replace(&*dir.to_string_lossy(), &o.to_string_lossy()))
        .collect();
    let ob = oracle_run(&o, &os);
    assert_eq!(hb, ob, "rows differ");
    assert!(
        took < Duration::from_secs(8),
        "lookups into attached / TEMP tables took {took:?}"
    );
}

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
    fn int(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.int(xs.len() as u64) as usize]
    }
}

/// The four databases: main, TEMP and two attached ones.
const DBS: [&str; 4] = ["main", "temp", "aux", "bb"];

/// One random script (`@k.` stands for database k's qualifier): tables of every database with indexes (several
/// columns, DESC, partial, collations), data with NULLs and mixed
/// types, columns added later with defaults, generated columns,
/// statistics; then joins, correlated subqueries, IN subqueries,
/// ranges, ORDER BY / LIMIT, GROUP BY, OR terms and LEFT JOINs
/// across them, each with its plan.
fn random_script(rng: &mut Rng) -> Vec<String> {
    let mut s = vec![
        "ATTACH '{d}/a.db' AS aux".to_string(),
        "ATTACH '{d}/b.db' AS bb".to_string(),
    ];
    let mut names = Vec::new();
    for k in 0..4 {
        let name = format!("{}{k}", rng.pick(&["t", "u", "m"]));
        let a = if rng.int(10) < 7 {
            "a INTEGER PRIMARY KEY"
        } else {
            "a INT UNIQUE"
        };
        let b = rng.pick(&["b TEXT", "b INT", "b", "b REAL"]);
        let c = rng.pick(&["c TEXT COLLATE NOCASE", "c INT", "c"]);
        let g = if rng.int(10) < 3 {
            ", g AS (b || c)"
        } else {
            ""
        };
        s.push(format!("CREATE TABLE @{k}.{name}({a}, {b}, {c}, d{g})"));
        for i in 0..rng.int(40) {
            let a = if rng.int(10) < 9 { i + 1 } else { rng.int(60) };
            let b = match rng.int(4) {
                0 => format!("{}", rng.int(10)),
                1 => format!("'x{}'", rng.int(6)),
                2 => "NULL".to_string(),
                _ => format!("{}.5", rng.int(10)),
            };
            let c = rng.pick(&["'A'", "'a'", "'b'", "'B'", "NULL", "0", "1", "2"]);
            s.push(format!(
                "INSERT OR IGNORE INTO @{k}.{name}(a, b, c, d) VALUES ({a}, {b}, {c}, {})",
                rng.int(6)
            ));
        }
        if rng.int(10) < 4 {
            s.push(format!(
                "ALTER TABLE @{k}.{name} ADD COLUMN e DEFAULT {}",
                rng.int(4)
            ));
        }
        for j in 0..rng.int(4) {
            let mut cols = vec!["b", "c", "d"];
            let n = 1 + rng.int(2) as usize;
            let mut picked = Vec::new();
            for _ in 0..n {
                let i = rng.int(cols.len() as u64) as usize;
                picked.push(format!("{}{}", cols.remove(i), rng.pick(&["", " DESC"])));
            }
            let w = rng.pick(&["", "", " WHERE d > 1"]);
            s.push(format!(
                "CREATE INDEX @{k}.{name}_i{j} ON {name}({}){w}",
                picked.join(", ")
            ));
        }
        if rng.int(10) < 3 {
            s.push(format!("ANALYZE @{k}.{name}"));
        }
        names.push(name);
    }
    let tab = |k: usize| format!("@{k}.{}", names[k]);
    for _ in 0..25 {
        let n = 2 + rng.int(2) as usize;
        let mut ds: Vec<usize> = Vec::new();
        while ds.len() < n {
            let d = rng.int(4) as usize;
            if !ds.contains(&d) {
                ds.push(d);
            }
        }
        let al = ["x", "y", "z"];
        let from: Vec<String> = ds
            .iter()
            .zip(al.iter())
            .map(|(d, a)| format!("{} AS {a}", tab(*d)))
            .collect();
        let col = |rng: &mut Rng| rng.pick(&["a", "b", "c", "d"]);
        let q = match rng.int(9) {
            0 => {
                let w: Vec<String> = (1..n)
                    .map(|i| format!("{}.{} = {}.{}", al[i], col(rng), al[i - 1], col(rng)))
                    .collect();
                format!(
                    "SELECT * FROM {} WHERE {}",
                    from.join(", "),
                    w.join(" AND ")
                )
            }
            1 => format!(
                "SELECT x.a, (SELECT max(b) FROM {} WHERE {} = x.{}) FROM {}",
                tab(ds[1]),
                col(rng),
                rng.pick(&["a", "b", "d"]),
                from[0]
            ),
            2 => format!(
                "SELECT * FROM {} JOIN {} ON y.{} = x.{} WHERE y.d {} ORDER BY 1, 2, 3, 4",
                from[0],
                from[1],
                rng.pick(&["a", "b", "c"]),
                rng.pick(&["a", "b", "c"]),
                rng.pick(&[
                    "= 2",
                    "> 2",
                    "<= 2",
                    "IN (1, 3)",
                    "BETWEEN 1 AND 3",
                    "IS NULL"
                ])
            ),
            3 => format!(
                "SELECT * FROM {} LEFT JOIN {} ON y.a = x.d",
                from[0], from[1]
            ),
            4 => format!(
                "SELECT x.b, count(*) FROM {} WHERE x.a = y.a GROUP BY 1",
                from.join(", ")
            ),
            5 => format!(
                "SELECT * FROM {} WHERE x.b IN (SELECT b FROM {} WHERE d > 1)",
                from[0],
                tab(ds[1])
            ),
            6 => format!(
                "SELECT * FROM {}, {} WHERE y.c = x.c AND y.b > {} ORDER BY y.c, x.a, y.a LIMIT 7",
                from[0],
                from[1],
                rng.int(6)
            ),
            7 => format!(
                "SELECT x.* FROM {}, {} WHERE y.rowid = x.a OR y.d = x.d",
                from[0], from[1]
            ),
            _ => format!(
                "SELECT * FROM {} WHERE EXISTS (SELECT 1 FROM {} WHERE a = x.{} AND d > {})",
                from[0],
                tab(ds[1]),
                col(rng),
                rng.int(4)
            ),
        };
        s.push(format!("EXPLAIN QUERY PLAN {q}"));
        s.push(q);
    }
    s
}

/// The script with every table in its database (`foreign`) or in
/// main.
fn layout(script: &[String], foreign: bool) -> Vec<String> {
    script
        .iter()
        .map(|l| {
            let mut l = l.clone();
            for (k, d) in DBS.iter().enumerate() {
                let q = if foreign && k > 0 {
                    format!("{d}.")
                } else {
                    String::new()
                };
                l = l.replace(&format!("@{k}."), &q);
            }
            l
        })
        .collect()
}

#[test]
fn generated_cross_database_plans() {
    let n: u64 = std::env::var("HIPP_ACCESS_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut compared, mut as_sets) = (0usize, 0usize);
    for i in 0..n {
        let script = random_script(&mut rng);
        let fs = layout(&script, true);
        let ms = layout(&script, false);
        let (fh, fo) = answers(&format!("gen{i}f"), &fs);
        let (mh, mo) = answers(&format!("gen{i}m"), &ms);
        for (j, sql) in fs.iter().enumerate() {
            let (Some(a), Some(b)) = (fh.get(j), fo.get(j)) else {
                panic!("gen{i}: statement {} lost", j + 1);
            };
            compared += 1;
            if a == b {
                continue;
            }
            // The same statement over main tables already differs on
            // hipp (outside this unit): its rows as a set, its plan
            // not compared.
            if mh.get(j) != mo.get(j) {
                as_sets += 1;
                if sql.starts_with("EXPLAIN") {
                    continue;
                }
                let (mut a, mut b) = (a.clone(), b.clone());
                a.sort();
                b.sort();
                if a == b {
                    continue;
                }
            }
            panic!(
                "gen{i}: statement {} differs: {sql}\nhipp: {a:?}\npin:  {b:?}\nscript:\n{}",
                j + 1,
                fs.join(";\n")
            );
        }
    }
    eprintln!("{as_sets} of {compared} statements compared as sets");
    // (Measured: 87 of 21,691 statements, 0.4%, over 150 scripts;
    // all of them plans and row orders the same statements show over
    // main tables, mostly automatic indexes the pin builds in
    // correlated subqueries and 3-way join orders.)
    assert!(
        as_sets * 10 <= compared,
        "{as_sets} of {compared} statements compared as sets"
    );
}
