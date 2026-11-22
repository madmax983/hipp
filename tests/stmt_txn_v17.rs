//! v0.17 `stmt_txn` unit: reads across databases that hipp refused
//! or read whole (leaf `attach`; planning under `expr_index` /
//! `planner`, the pragma tables under `vtab`), differentially vs the
//! pinned oracle (SQLite 3.54.1, live).
//!
//! Probed on the pin first (STATUS.md's notes):
//! - `pragma_integrity_check` / `pragma_quick_check` are eponymous
//!   virtual tables (`pragmaVtabFilter` runs `PRAGMA "schema".
//!   integrity_check('arg')`): the argument is always a table name
//!   (`pragma_integrity_check(2)` is "no such table: 2"), found in the
//!   `schema` argument's database or — without one — in temp, main,
//!   then each attached database; hidden columns `arg` and `schema`;
//!   `SCAN pragma_integrity_check VIRTUAL TABLE INDEX 0:`; a check
//!   that meets a corrupt page gives its rows, then the error (none
//!   when a LIMIT stops first).
//! - A write of an attached or TEMP table reading another database's
//!   view (or a TEMP view over another database's tables) runs on the
//!   pin, the view expanded where it is read.
//! - A WITHOUT ROWID table of another database is searched by its
//!   PRIMARY KEY and its indexes from main's statements (`SEARCH
//!   aux.w USING PRIMARY KEY (k=?)`, `COVERING INDEX`, the join
//!   driven from it), as a main one is.
//!
//! Every script runs on both engines in twin directories, statement
//! by statement: rows (EXPLAIN QUERY PLAN as the pin shell's tree),
//! and whether the statement failed, must agree.

#![cfg(all(feature = "attach", feature = "planner", feature = "vtab"))]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};
use std::path::{Path, PathBuf};

fn fresh_dir(tag: &str) -> PathBuf {
    let p = common::temp_db(&format!("v17stx-{tag}"));
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
                        // (The shell prints a value's lines as lines.)
                        b.extend(l.split('\n').map(str::to_string));
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

/// A deterministic xorshift stream.
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

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned(),
        "oracle is {}, not the 3.54.1 pin",
        common::oracle_version()
    );
}

/// `answers` with `prep` run on each directory first (a file the
/// script attaches, written by the pin).
fn answers_prep(tag: &str, stmts: &[String], prep: &dyn Fn(&Path)) -> (Vec<Block>, Vec<Block>) {
    let h = fresh_dir(&format!("{tag}-h"));
    let o = fresh_dir(&format!("{tag}-o"));
    prep(&h);
    prep(&o);
    let hs: Vec<String> = stmts
        .iter()
        .map(|s| s.replace("{d}", &h.to_string_lossy()))
        .collect();
    let os: Vec<String> = stmts
        .iter()
        .map(|s| s.replace("{d}", &o.to_string_lossy()))
        .collect();
    (hipp_run(&h, &hs), oracle_run(&o, &os))
}

const IC_SETUP: &str = "
CREATE TABLE t(a, b);
INSERT INTO t VALUES (1, 2);
CREATE INDEX ti ON t(b);
CREATE VIEW v AS SELECT a FROM t;
ATTACH '{d}/a.db' AS aux;
CREATE TABLE aux.u(x PRIMARY KEY, y) WITHOUT ROWID;
INSERT INTO aux.u VALUES (1, 2);
";

/// The table-valued integrity checks: argument forms, errors, hidden
/// columns, plans, joins, qualifiers and the TEMP database.
#[test]
fn integrity_tvf_forms() {
    let q = "
        SELECT * FROM pragma_integrity_check;
        SELECT * FROM pragma_integrity_check();
        SELECT * FROM pragma_integrity_check(2);
        SELECT * FROM pragma_integrity_check('2');
        SELECT * FROM pragma_integrity_check(-1);
        SELECT * FROM pragma_integrity_check(1.5);
        SELECT * FROM pragma_integrity_check(x'75');
        SELECT * FROM pragma_integrity_check('t');
        SELECT * FROM pragma_integrity_check('T');
        SELECT * FROM pragma_integrity_check('aux');
        SELECT * FROM pragma_integrity_check('u');
        SELECT * FROM pragma_integrity_check('v');
        SELECT * FROM pragma_integrity_check('ti');
        SELECT * FROM pragma_integrity_check('main');
        SELECT * FROM pragma_integrity_check(NULL);
        SELECT * FROM pragma_integrity_check('t', 'main');
        SELECT * FROM pragma_integrity_check('u', 'aux');
        SELECT * FROM pragma_integrity_check('u', 'main');
        SELECT * FROM pragma_integrity_check('t', 'nosuch');
        SELECT * FROM pragma_integrity_check(NULL, 'aux');
        SELECT * FROM pragma_integrity_check(NULL, NULL);
        SELECT * FROM pragma_integrity_check(NULL, 2);
        SELECT * FROM pragma_integrity_check('t', NULL);
        SELECT * FROM pragma_quick_check(1, 2, 3);
        SELECT * FROM pragma_quick_check('sqlite_schema');
        SELECT * FROM pragma_quick_check('sqlite_master');
        SELECT * FROM pragma_quick_check('sqlite_temp_master');
        SELECT * FROM aux.pragma_quick_check;
        SELECT * FROM main.pragma_quick_check('u');
        SELECT * FROM temp.pragma_quick_check;
        SELECT integrity_check FROM pragma_integrity_check;
        SELECT quick_check FROM pragma_quick_check;
        SELECT integrity_check FROM pragma_quick_check;
        SELECT *, schema, arg FROM pragma_integrity_check('t');
        SELECT arg, schema, typeof(arg) FROM pragma_quick_check(x'75', 'aux');
        SELECT rowid, * FROM pragma_quick_check;
        SELECT * FROM pragma_quick_check WHERE schema = 'aux';
        SELECT * FROM pragma_quick_check WHERE arg = 'u';
        SELECT * FROM pragma_quick_check WHERE arg = 'u' AND schema = 'main';
        SELECT * FROM pragma_quick_check WHERE arg > 'a';
        SELECT * FROM pragma_quick_check WHERE quick_check = 'ok';
        SELECT * FROM (SELECT 'u' AS n) JOIN pragma_quick_check(n);
        SELECT * FROM (SELECT 'zz' AS n) JOIN pragma_quick_check(n);
        SELECT n, quick_check FROM (SELECT 't' AS n UNION ALL SELECT 'u') JOIN pragma_quick_check(n);
        SELECT count(*) FROM pragma_quick_check JOIN t;
        SELECT * FROM t WHERE (SELECT integrity_check FROM pragma_integrity_check) = 'ok';
        SELECT * FROM t LEFT JOIN pragma_quick_check('zz') ON 1;
        SELECT * FROM t, aux.u, pragma_quick_check WHERE t.a = u.x;
        PRAGMA table_xinfo(pragma_integrity_check);
    ";
    let mut s = lines(IC_SETUP);
    for l in lines(q) {
        s.push(format!("EXPLAIN QUERY PLAN {l}"));
        s.push(l);
    }
    s.push("CREATE TEMP TABLE tt(q)".to_string());
    for l in lines(
        "SELECT x FROM aux.u WHERE EXISTS (SELECT 1 FROM pragma_quick_check);
         SELECT * FROM pragma_quick_check('tt');
         SELECT * FROM pragma_quick_check('tt', 'main');
         SELECT * FROM pragma_quick_check('tt', 'temp');
         SELECT * FROM pragma_quick_check(NULL, 'temp');
         SELECT * FROM tt, pragma_quick_check;
         INSERT INTO t SELECT 9, quick_check FROM pragma_quick_check;
         SELECT * FROM t;
         INSERT INTO tt SELECT * FROM pragma_integrity_check('t');
         SELECT * FROM tt;
         CREATE TEMP VIEW tv AS SELECT * FROM pragma_quick_check;
         SELECT * FROM tv;
         BEGIN;
         SELECT * FROM pragma_quick_check(NULL, 'aux');
         DETACH aux;
         COMMIT;
         DETACH aux;
         SELECT * FROM pragma_quick_check('u')",
    ) {
        s.push(l);
    }
    same("icforms", &s);
}

/// A file whose index page is corrupt, written by the pin: the
/// attached database's check reports it.
fn corrupt_aux(dir: &Path) {
    let a = dir.join("a.db");
    common::cli(
        &a,
        "CREATE TABLE u(x INTEGER PRIMARY KEY, y); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 300) \
         INSERT INTO u SELECT i, randomblob(100) FROM c; CREATE INDEX uy ON u(y);",
    );
    let mut b = std::fs::read(&a).expect("read");
    let off = 4 * 4096;
    b[off..off + 8].copy_from_slice(&[0x0d, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&a, b).expect("write");
}

/// A check meeting a corrupt page: its rows, then the error after the
/// last one (not when a LIMIT stops first, nor for the database not
/// asked about).
#[test]
fn integrity_tvf_corrupt() {
    let s = lines(
        "CREATE TABLE t(a);
         ATTACH '{d}/a.db' AS aux;
         SELECT 'A', * FROM pragma_quick_check;
         SELECT 'B', * FROM aux.pragma_quick_check;
         SELECT 'C', * FROM pragma_quick_check(NULL, 'aux');
         SELECT 'D', * FROM pragma_quick_check(NULL, 'main');
         SELECT 'E', * FROM pragma_quick_check('u');
         SELECT 'F', count(*) FROM pragma_integrity_check;
         SELECT 'G', * FROM pragma_integrity_check LIMIT 2;
         SELECT 'H', * FROM pragma_quick_check LIMIT 3;
         SELECT 'I', * FROM pragma_quick_check LIMIT 4;
         SELECT 'J', * FROM pragma_quick_check ORDER BY 2;
         SELECT 'K', * FROM pragma_quick_check('t');
         SELECT 'L', * FROM (SELECT 1 UNION ALL SELECT 2), pragma_integrity_check(NULL, 'aux');
         PRAGMA quick_check;
         SELECT 'M', * FROM pragma_quick_check(3)",
    );
    let (h, o) = answers_prep("iccorrupt", &s, &corrupt_aux);
    for (i, (a, b)) in h.iter().zip(o.iter()).enumerate() {
        // (The frozen VM reads a later join source whole: the error
        // comes without the rows, an honest gap of `--features vm`.)
        if cfg!(feature = "vm") && s[i].starts_with("SELECT 'L'") {
            assert_eq!(a, &vec!["!".to_string()]);
            continue;
        }
        assert_eq!(a, b, "statement {}: {}", i + 1, s[i]);
    }
    // The check reported the damage (the setup corrupted something).
    assert!(h[2].iter().any(|l| l.contains("page 5")), "{:?}", h[2]);
}

const VIEW_SETUP: &str = "
CREATE TABLE m(x INTEGER PRIMARY KEY, y TEXT COLLATE NOCASE, z);
INSERT INTO m VALUES (1, 'A', 1.5), (2, 'b', '7'), (3, 'C', NULL), (4, 'd', 4);
CREATE VIEW mv AS SELECT x, z * 2 AS y2 FROM m WHERE x < 3;
CREATE VIEW mc(k, label) AS SELECT x, y FROM m WHERE x < 4;
CREATE VIEW mc2 AS SELECT k * 10 AS kk, label FROM mc;
CREATE VIEW agg AS SELECT count(*) AS n, max(x) AS mx FROM m;
CREATE VIEW lim AS SELECT x FROM m ORDER BY x DESC LIMIT 2;
CREATE VIEW withv AS WITH c(q) AS (SELECT x FROM m) SELECT q FROM c WHERE q > 1;
ATTACH '{d}/a.db' AS aux;
CREATE TABLE aux.t(a INTEGER PRIMARY KEY, c, d TEXT);
INSERT INTO aux.t VALUES (1, NULL, 'a'), (2, NULL, 'B'), (3, NULL, 'c'), (7, NULL, 'D');
CREATE TABLE aux.base(p, q);
INSERT INTO aux.base VALUES (1, 'one'), (2, 'two');
CREATE VIEW aux.av AS SELECT p, upper(q) AS uq FROM base;
CREATE TEMP TABLE tt(k, v);
INSERT INTO tt VALUES (1, 0), (2, 0), (5, 0);
CREATE TEMP VIEW tv AS SELECT tt.k, m.y FROM tt JOIN m ON m.x = tt.k;
CREATE TEMP VIEW tv2 AS SELECT t.a, t.d FROM aux.t;
CREATE TEMP VIEW tv3 AS SELECT k, v FROM tt WHERE k < 3;
";

/// Writes of attached and TEMP tables reading other databases' views:
/// the views' rows, affinities and collations as the pin reads them.
#[test]
fn views_in_cross_database_writes() {
    let q = "
        UPDATE aux.t SET c = (SELECT y2 FROM mv WHERE mv.x = t.a);
        SELECT * FROM aux.t;
        UPDATE tt SET v = (SELECT y2 FROM mv WHERE mv.x = tt.k);
        SELECT * FROM tt;
        INSERT INTO aux.t(c) SELECT y2 FROM mv;
        SELECT * FROM aux.t;
        INSERT INTO tt SELECT * FROM aux.av;
        SELECT * FROM tt;
        DELETE FROM aux.t WHERE a IN (SELECT x FROM mv);
        SELECT * FROM aux.t;
        UPDATE tt SET v = (SELECT uq FROM aux.av WHERE p = k);
        SELECT * FROM tt;
        UPDATE aux.t SET c = (SELECT label FROM mc WHERE mc.k = t.a);
        SELECT * FROM aux.t;
        UPDATE aux.t SET c = (SELECT kk FROM mc2 WHERE kk = t.a * 10);
        SELECT * FROM aux.t;
        UPDATE aux.t SET c = (SELECT n || '/' || mx FROM agg);
        SELECT * FROM aux.t;
        DELETE FROM aux.t WHERE a IN (SELECT x FROM lim);
        SELECT * FROM aux.t;
        INSERT INTO aux.t(c) SELECT q FROM withv;
        SELECT * FROM aux.t;
        UPDATE aux.t SET c = (SELECT y FROM tv WHERE tv.k = t.a);
        SELECT * FROM aux.t;
        UPDATE tt SET v = (SELECT d FROM tv2 WHERE tv2.a = tt.k);
        SELECT * FROM tt;
        UPDATE tt SET v = (SELECT count(*) FROM tv3);
        SELECT * FROM tt;
        UPDATE tt SET v = (SELECT count(*) FROM mc, mc2);
        SELECT * FROM tt;
        WITH w AS (SELECT 5 AS five) UPDATE aux.t SET c = (SELECT five FROM w) + (SELECT count(*) FROM main.mv);
        SELECT * FROM aux.t;
        UPDATE aux.t SET d = 'x' WHERE d = (SELECT label FROM mc WHERE k = 2);
        SELECT * FROM aux.t;
        UPDATE aux.t SET d = 'y' WHERE (SELECT label FROM mc WHERE k = 3) = 'c';
        SELECT * FROM aux.t;
        INSERT INTO aux.t(a, c) SELECT k + 100, label FROM mc RETURNING *;
        INSERT INTO aux.t(a, c) SELECT k, label FROM mc WHERE true ON CONFLICT(a) DO UPDATE SET c = excluded.c || '!';
        SELECT * FROM aux.t;
        INSERT INTO aux.t(c) SELECT k FROM tv3;
        SELECT * FROM aux.t;
        UPDATE aux.base SET q = (SELECT uq FROM av WHERE p = base.p);
        SELECT * FROM aux.base;
        UPDATE m SET y = (SELECT uq FROM aux.av WHERE p = x);
        SELECT * FROM m;
        INSERT INTO aux.t(c) SELECT y2 FROM mv WHERE x IN (SELECT p FROM aux.av);
        SELECT * FROM aux.t;
        SELECT changes(), total_changes() > 0;
        BEGIN;
        UPDATE aux.t SET c = (SELECT count(*) FROM mv);
        DETACH aux;
        ROLLBACK;
        SELECT * FROM aux.t;
        CREATE TABLE aux.mv(z);
        UPDATE aux.t SET c = (SELECT count(*) FROM main.mv);
        UPDATE aux.t SET c = (SELECT count(*) FROM mv);
        SELECT * FROM aux.t;
    ";
    let mut s = lines(VIEW_SETUP);
    s.extend(lines(q));
    same("views", &s);
}

/// Refused (an honest gap): a statement whose own CTE takes the name
/// of another database's view it reads (the view would be written in
/// under that name), which the pin answers.
#[test]
fn view_name_clash_refused() {
    let mut s = lines(VIEW_SETUP);
    s.push("WITH mv AS (SELECT 1 AS x) UPDATE aux.t SET c = (SELECT count(*) FROM main.mv)".into());
    let (h, o) = answers("clash", &s);
    let n = s.len() - 1;
    assert!(
        h[..n].iter().all(|b| b.last().is_none_or(|l| l != "!")),
        "{h:?}"
    );
    assert_eq!(h[n], vec!["!".to_string()]);
    assert!(o[n].is_empty(), "{:?}", o[n]);
}

/// One random script of views in main, TEMP and an attached database
/// (joins, aggregates, column lists, views over views, collations and
/// affinities) and writes of TEMP and attached tables reading them,
/// each followed by the written table.
fn view_script(rng: &mut Rng) -> Vec<String> {
    let mut s = vec!["ATTACH '{d}/a.db' AS aux".to_string()];
    let decls = ["", "INT", "TEXT", "REAL", "TEXT COLLATE NOCASE", "NUMERIC"];
    let vals = [
        "NULL", "1", "2", "'2'", "'a'", "'A'", "1.5", "'b'", "3", "x'62'",
    ];
    let dbs = ["main", "temp", "aux"];
    for d in dbs.iter() {
        s.push(format!(
            "CREATE TABLE {d}.s_{d}(k INTEGER PRIMARY KEY, a {}, b {})",
            rng.pick(&decls),
            rng.pick(&decls)
        ));
        for k in 1..=(2 + rng.int(5)) {
            s.push(format!(
                "INSERT INTO {d}.s_{d} VALUES ({k}, {}, {})",
                rng.pick(&vals),
                rng.pick(&vals)
            ));
        }
    }
    // Views: each database's over its own table; TEMP ones over any.
    let mut views: Vec<(String, String)> = Vec::new();
    for (i, d) in dbs.iter().enumerate() {
        for j in 0..2 {
            let name = format!("v_{d}{j}");
            let src = if *d == "temp" {
                format!("s_{}", dbs[rng.int(3) as usize])
            } else {
                format!("s_{d}")
            };
            let body = match rng.int(5) {
                0 => format!("SELECT k, a AS c FROM {src}"),
                1 => format!("SELECT k, a || b AS c FROM {src} WHERE a IS NOT NULL"),
                2 => format!("SELECT max(k) AS k, count(*) AS c FROM {src} GROUP BY b"),
                3 if j == 1 => {
                    let base = format!("v_{d}0");
                    format!("SELECT k + 1 AS k, c FROM {base}")
                }
                _ => format!("SELECT k, b AS c FROM {src} ORDER BY k DESC LIMIT 2"),
            };
            let cols = if rng.int(3) == 0 { "(k, c)" } else { "" };
            let tmp = if *d == "temp" { "TEMP " } else { "" };
            let q = if *d == "aux" { "aux." } else { "" };
            s.push(format!("CREATE {tmp}VIEW {q}{name}{cols} AS {body}"));
            views.push((name, dbs[i].to_string()));
        }
    }
    s.push("CREATE TABLE aux.w(k, c)".to_string());
    s.push("CREATE TEMP TABLE tw(k, c)".to_string());
    for _ in 0..10 {
        let target = rng.pick(&["aux.w", "tw"]);
        let (v, vd) = views[rng.int(views.len() as u64) as usize].clone();
        let vq = if rng.int(2) == 0 {
            format!("{vd}.{v}")
        } else {
            v.clone()
        };
        let w = match rng.int(5) {
            0 => format!("INSERT INTO {target} SELECT k, c FROM {vq}"),
            1 => format!("INSERT INTO {target} SELECT k, c FROM {vq} WHERE c > 1 ORDER BY k"),
            2 => format!(
                "UPDATE {target} SET c = (SELECT c FROM {vq} WHERE {vq}.k = {}.k)",
                target.trim_start_matches("aux.")
            ),
            3 => format!("DELETE FROM {target} WHERE k IN (SELECT k FROM {vq})"),
            _ => format!("UPDATE {target} SET c = (SELECT count(*) FROM {vq}) WHERE k > 1"),
        };
        s.push(w);
        s.push(format!("SELECT * FROM {target}"));
        s.push("SELECT changes()".to_string());
    }
    s
}

#[test]
fn generated_views_in_writes() {
    let n: u64 = std::env::var("HIPP_STX_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let mut rng = Rng(0x51ed_270b_27f1_9e3d);
    let mut writes = 0usize;
    for i in 0..n {
        let s = view_script(&mut rng);
        let (h, o) = answers(&format!("gv{i}"), &s);
        for (j, (a, b)) in h.iter().zip(o.iter()).enumerate() {
            assert_eq!(
                a,
                b,
                "gv{i}: statement {} differs: {}\nscript:\n{}",
                j + 1,
                s[j],
                s.join(";\n")
            );
            if s[j].starts_with("SELECT changes") && a.first().is_some_and(|l| l != "!") {
                writes += 1;
            }
        }
    }
    assert!(writes > 0);
}

const WR_SETUP: &str = "
CREATE TABLE m(x INTEGER PRIMARY KEY, y);
INSERT INTO m VALUES (1, 10), (2, 20), (3, 30), (4, 200);
ATTACH '{d}/a.db' AS aux;
CREATE TABLE aux.w(k, v, z, PRIMARY KEY(k, v)) WITHOUT ROWID;
INSERT INTO aux.w VALUES (1, 'a', 100), (1, 'b', 101), (2, 'a', 200), (3, 'c', 300);
CREATE INDEX aux.wz ON w(z);
CREATE TABLE aux.wd(k TEXT COLLATE NOCASE PRIMARY KEY, n INT) WITHOUT ROWID;
INSERT INTO aux.wd VALUES ('A', 1), ('b', 2), ('C', 3);
CREATE TABLE aux.wdesc(k, v, PRIMARY KEY(k DESC)) WITHOUT ROWID;
INSERT INTO aux.wdesc VALUES (1, 'one'), (2, 'two'), (3, 'three');
CREATE TEMP TABLE tw(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO tw VALUES (1, 't1'), (2, 't2');
CREATE INDEX temp.twv ON tw(v);
";

/// WITHOUT ROWID tables of an attached and the TEMP database read by
/// main's statements through their PRIMARY KEY and indexes, with the
/// pin's plans (they were read whole, in FROM order; a TEMP one was
/// refused).
#[test]
fn without_rowid_seeks() {
    let q = "
        SELECT * FROM aux.w WHERE k = 1;
        SELECT * FROM aux.w WHERE k = 1 AND v = 'b';
        SELECT * FROM aux.w WHERE z = 200;
        SELECT * FROM m JOIN aux.w ON w.k = m.x;
        SELECT (SELECT z FROM aux.w WHERE k = m.x AND v = 'a') FROM m;
        SELECT * FROM aux.w ORDER BY k DESC;
        SELECT * FROM aux.w WHERE k > 1;
        SELECT max(z) FROM aux.w;
        SELECT * FROM m, tw WHERE tw.k = m.x;
        SELECT * FROM tw WHERE k = 2;
        SELECT * FROM aux.w WHERE k IN (1, 3);
        SELECT * FROM aux.w, m WHERE w.z = m.y * 10;
        SELECT m.x, wd.n FROM m JOIN aux.wd ON wd.k = 'a' AND m.x = wd.n;
        SELECT * FROM aux.wd WHERE k = 'B';
        SELECT * FROM m WHERE x IN (SELECT n FROM aux.wd);
        SELECT * FROM m WHERE EXISTS (SELECT 1 FROM aux.w WHERE w.k = m.x AND w.v = 'a');
        SELECT * FROM aux.wdesc WHERE k BETWEEN 2 AND 3 ORDER BY k;
        SELECT * FROM aux.wdesc ORDER BY k DESC;
        SELECT * FROM m LEFT JOIN aux.wdesc ON wdesc.k = m.x;
        SELECT * FROM m, tw WHERE tw.v = 't' || m.x;
        SELECT tw.k, w.z FROM tw JOIN aux.w ON w.k = tw.k JOIN m ON m.x = w.k;
        SELECT * FROM aux.w INDEXED BY wz WHERE z > 150;
        SELECT * FROM aux.w NOT INDEXED WHERE k = 2;
        SELECT k, count(*) FROM aux.w GROUP BY k;
    ";
    let mut s = lines(WR_SETUP);
    for l in lines(q) {
        s.push(format!("EXPLAIN QUERY PLAN {l}"));
        s.push(l);
    }
    for l in lines(
        "UPDATE m SET y = (SELECT z FROM aux.w WHERE w.k = m.x AND w.v = 'a');
         SELECT * FROM m;
         DELETE FROM m WHERE x IN (SELECT n FROM aux.wd WHERE k = 'c');
         SELECT * FROM m;
         INSERT INTO m SELECT k + 10, v FROM tw;
         SELECT * FROM m;
         UPDATE m SET y = (SELECT v FROM tw WHERE tw.k = m.x) WHERE x < 3;
         SELECT * FROM m;
         BEGIN;
         SELECT * FROM m JOIN aux.w ON w.k = m.x AND w.v = 'zz';
         DETACH aux;
         COMMIT;
         DROP INDEX aux.wz;
         SELECT * FROM aux.w WHERE z = 200;
         DELETE FROM aux.w WHERE k = 1;
         SELECT * FROM m JOIN aux.w ON w.k = m.x;
         DROP TABLE tw;
         CREATE TEMP TABLE tw(k, v, PRIMARY KEY(v, k)) WITHOUT ROWID;
         INSERT INTO tw VALUES (1, 'x'), (3, 'y');
         SELECT * FROM m JOIN tw ON tw.k = m.x;",
    ) {
        s.push(l);
    }
    same("wrseek", &s);
}

/// The databases of the WITHOUT ROWID corpus.
const DBS: [&str; 3] = ["main", "temp", "aux"];

/// A random script of WITHOUT ROWID tables (key shapes, DESC and
/// collated keys, secondary and partial indexes, NULL-free keys of
/// mixed types) in main, TEMP or the attached database, and queries of
/// main joining and looking into them, each with its plan.
fn wr_script(rng: &mut Rng) -> Vec<String> {
    let mut s = vec!["ATTACH '{d}/a.db' AS aux".to_string()];
    s.push("CREATE TABLE m(x INTEGER PRIMARY KEY, y, z)".to_string());
    for k in 1..=6 {
        s.push(format!(
            "INSERT INTO m VALUES ({k}, {}, {})",
            rng.pick(&["1", "2", "'a'", "'B'", "NULL", "3"]),
            rng.int(5)
        ));
    }
    let keys = [
        "PRIMARY KEY(a)",
        "PRIMARY KEY(a, b)",
        "PRIMARY KEY(b, a)",
        "PRIMARY KEY(a DESC)",
        "PRIMARY KEY(a, b DESC)",
    ];
    for t in 0..2 {
        let db = (rng.int(2) + 1) as usize;
        let coll = rng.pick(&["", " COLLATE NOCASE"]);
        s.push(format!(
            "CREATE TABLE @{db}.w{t}(a INT NOT NULL, b TEXT{coll} NOT NULL, c, {}) WITHOUT ROWID",
            rng.pick(&keys)
        ));
        for r in 0..(3 + rng.int(8)) {
            s.push(format!(
                "INSERT OR IGNORE INTO @{db}.w{t} VALUES ({}, {}, {})",
                1 + rng.int(6),
                rng.pick(&["'a'", "'A'", "'b'", "'c'"]),
                r
            ));
        }
        match rng.int(3) {
            0 => s.push(format!("CREATE INDEX @{db}.w{t}c ON w{t}(c)")),
            1 => s.push(format!(
                "CREATE INDEX @{db}.w{t}c ON w{t}(c, b) WHERE c > 1"
            )),
            _ => {}
        }
        if rng.int(3) == 0 {
            s.push(format!("ANALYZE @{db}.w{t}"));
        }
    }
    let terms = [
        "w{t}.a = m.x",
        "w{t}.a = m.x AND w{t}.b = 'a'",
        "w{t}.c = m.z",
        "w{t}.b = m.y",
        "w{t}.a > m.x",
        "w{t}.a IN (1, 3)",
    ];
    for _ in 0..10 {
        let t = rng.int(2);
        let term = rng.pick(&terms).replace("{t}", &t.to_string());
        let q = match rng.int(5) {
            0 => format!("SELECT m.x, w{t}.a, w{t}.b, w{t}.c FROM m JOIN @?.w{t} ON {term}"),
            1 => format!("SELECT x, (SELECT max(c) FROM @?.w{t} WHERE {term}) FROM m"),
            2 => format!("SELECT x FROM m WHERE EXISTS (SELECT 1 FROM @?.w{t} WHERE {term})"),
            3 => format!(
                "SELECT m.x, w{t}.c FROM @?.w{t} LEFT JOIN m ON {term} ORDER BY w{t}.a, w{t}.b"
            ),
            _ => {
                format!("SELECT count(*), sum(w{t}.c) FROM m, @?.w{t} WHERE {term} AND w{t}.c >= 0")
            }
        };
        s.push(format!("EXPLAIN QUERY PLAN {q}"));
        s.push(q);
    }
    s.push("UPDATE m SET z = (SELECT min(c) FROM @?.w0 WHERE w0.a = m.x)".to_string());
    s.push("SELECT * FROM m".to_string());
    s
}

/// `script` with every `@k.` qualifier given database k's name (or,
/// `foreign` false, main's), and each `@?.` the qualifier of the
/// table's own database.
fn wr_layout(script: &[String], foreign: bool) -> Vec<String> {
    let mut home: std::collections::HashMap<String, usize> = Default::default();
    for l in script {
        for k in 1..DBS.len() {
            if let Some(r) = l.strip_prefix(&format!("CREATE TABLE @{k}.")) {
                let name: String = r.chars().take_while(|c| c.is_alphanumeric()).collect();
                home.insert(name, k);
            }
        }
    }
    script
        .iter()
        .map(|l| {
            let mut l = l.clone();
            while let Some(i) = l.find("@?.") {
                let name: String = l[i + 3..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric())
                    .collect();
                let k = home.get(&name).copied().unwrap_or(0);
                l.replace_range(i..i + 3, &format!("@{k}."));
            }
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
fn generated_without_rowid_seeks() {
    let n: u64 = std::env::var("HIPP_STX_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let mut rng = Rng(0x2f6b_1d3c_8a47_95e1);
    let (mut compared, mut as_sets) = (0usize, 0usize);
    for i in 0..n {
        let script = wr_script(&mut rng);
        let fs = wr_layout(&script, true);
        let ms = wr_layout(&script, false);
        let (fh, fo) = answers(&format!("gw{i}f"), &fs);
        let (mh, mo) = answers(&format!("gw{i}m"), &ms);
        for (j, sql) in fs.iter().enumerate() {
            let (Some(a), Some(b)) = (fh.get(j), fo.get(j)) else {
                panic!("gw{i}: statement {} lost", j + 1);
            };
            compared += 1;
            if a == b {
                continue;
            }
            // The same statement over main tables already differs on
            // hipp (a planner divergence outside this unit): its rows
            // as a set, its plan not compared.
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
                "gw{i}: statement {} differs: {sql}\nhipp: {a:?}\npin:  {b:?}\nscript:\n{}",
                j + 1,
                fs.join(";\n")
            );
        }
    }
    eprintln!("{as_sets} of {compared} statements compared as sets");
    assert!(
        as_sets * 10 <= compared,
        "{as_sets} of {compared} statements compared as sets"
    );
}

/// Regression (found by `generated_views_in_writes`, also on main
/// tables): a WHERE over a subquery or view that reads a LIMITed one
/// through another merged into it filtered before the LIMIT (hipp
/// merged the LIMITed body under the WHERE, and pushed the term into
/// it). The pin's flattener meets that LIMIT once the middle body is
/// merged and stops: restrictions (8), (9), (16) and (19).
#[test]
fn limit_under_merged_where() {
    let setup = "
        CREATE TABLE s(k INTEGER PRIMARY KEY, b);
        INSERT INTO s VALUES (1, NULL), (2, 2), (3, 1), (4, 2), (5, NULL);
        CREATE VIEW v0(k, c) AS SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2;
        CREATE VIEW v1 AS SELECT k + 1 AS k, c FROM v0;
        CREATE TABLE u(k);
        INSERT INTO u VALUES (4), (5);
    ";
    let rows = "
        SELECT * FROM v0 WHERE c > 1;
        SELECT * FROM v1 WHERE c > 1;
        SELECT * FROM (SELECT k + 1 AS k, c FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2)) WHERE c > 1;
        SELECT * FROM (SELECT k, c FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2)) WHERE c > 1;
        SELECT * FROM (SELECT * FROM (SELECT * FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2))) WHERE c > 1;
        SELECT * FROM (SELECT * FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2) WHERE k > 0) WHERE c > 1;
        SELECT * FROM (SELECT * FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2 OFFSET 1)) WHERE c > 1;
        SELECT * FROM (SELECT * FROM (SELECT k, b AS c FROM s WHERE b > 0 ORDER BY k LIMIT 3)) WHERE k > 2;
        SELECT count(*), max(c) FROM v1;
        SELECT * FROM v1, u WHERE v1.k = u.k;
        SELECT * FROM v1 ORDER BY k;
        SELECT DISTINCT c FROM v1;
        SELECT * FROM v1 LIMIT 1;
        UPDATE u SET k = k + 10 WHERE k IN (SELECT k FROM v1 WHERE c > 1);
        SELECT * FROM u;
    ";
    let mut s = lines(setup);
    s.extend(lines(rows));
    // (The plans agree but for the name the pin gives the co-routine
    // left after the merge, `v0` / `(subquery-1)`: hipp names it after
    // the body it merged into, an honest gap.)
    for q in [
        "SELECT * FROM v0 WHERE c > 1",
        "SELECT * FROM (SELECT * FROM (SELECT k, b AS c FROM s ORDER BY k DESC LIMIT 2) WHERE k > 0) WHERE c > 1",
    ] {
        s.push(format!("EXPLAIN QUERY PLAN {q}"));
    }
    same("limitmerge", &s);
}

// ---- Review fixes (v0.17 `stmt_txn` review) ----

const FN_SETUP: &str = "
CREATE TABLE m(x INTEGER PRIMARY KEY, y);
INSERT INTO m VALUES (1, 'a'), (2, 'b');
ATTACH '{d}/a.db' AS aux;
CREATE TABLE aux.m(zz, ww, vv);
INSERT INTO aux.m VALUES (7, 8, 9);
CREATE TABLE aux.o(v);
CREATE TABLE aux.only(k1, k2);
INSERT INTO aux.only VALUES (1, 2), (3, 4);
CREATE VIEW aux.av AS SELECT zz FROM m;
CREATE TEMP TABLE tm(t1);
CREATE TEMP TABLE o(v);
";

/// A table-valued pragma function (or dbstat) in a statement that
/// names an attached table is answered by the database the pin finds
/// its object in: main runs a query that reads one main answers, and
/// an attached write whose function another database answers is
/// refused, not answered from the attached file (review fix: such a
/// write copied the attached `m`'s columns, a query read them).
#[test]
fn pragma_functions_beside_attached_tables() {
    let q = "
        SELECT name FROM pragma_table_info('m');
        SELECT name FROM pragma_table_info('only');
        SELECT name FROM pragma_table_info('tm');
        SELECT name, (SELECT count(*) FROM aux.only) FROM pragma_table_info('m');
        SELECT count(*) FROM aux.only, pragma_table_info('m');
        SELECT k1, name FROM aux.only JOIN pragma_table_info('m') ORDER BY 1, 2;
        SELECT name FROM aux.pragma_table_info('m');
        SELECT * FROM aux.av, pragma_table_info('m');
        SELECT * FROM aux.av, main.m;
        SELECT k1 FROM aux.only WHERE 0 < (SELECT count(*) FROM pragma_table_info('m'));
        SELECT DISTINCT name, (SELECT count(*) FROM aux.o) FROM dbstat ORDER BY 1;
        SELECT name FROM pragma_database_list, aux.only ORDER BY 1;
        INSERT INTO aux.o SELECT name FROM pragma_table_info('only');
        SELECT * FROM aux.o;
        DELETE FROM aux.o;
        INSERT INTO aux.o SELECT value FROM generate_series(1, 3);
        SELECT * FROM aux.o;
        DELETE FROM aux.o;
        INSERT INTO aux.o SELECT x FROM m;
        SELECT * FROM aux.o;
        DELETE FROM aux.o;
        INSERT INTO o SELECT name FROM pragma_table_info('m');
        SELECT * FROM o;
        UPDATE aux.m SET zz = (SELECT count(*) FROM m);
        SELECT * FROM aux.m;
        CREATE VIEW pv AS SELECT name FROM pragma_table_info('m');
        INSERT INTO aux.o SELECT name FROM pv;
        SELECT count(*) FROM aux.o;
    ";
    let mut s = lines(FN_SETUP);
    s.extend(lines(q));
    same("fnatt", &s);
}

/// The refusals the routing keeps (the pin answers them): an attached
/// write reading a pragma function or dbstat that main (or TEMP)
/// answers fails, and writes nothing.
#[test]
fn pragma_functions_in_attached_writes_refused() {
    let mut s = lines(FN_SETUP);
    let refused = lines(
        "INSERT INTO aux.o SELECT name FROM pragma_table_info('m');
         INSERT INTO aux.o SELECT name FROM main.pragma_table_info('m');
         INSERT INTO aux.o SELECT name FROM pragma_table_info('m', 'main');
         INSERT INTO aux.o VALUES ((SELECT group_concat(name) FROM pragma_table_info('tm')));
         INSERT INTO aux.o SELECT name FROM pragma_index_list('m');
         INSERT INTO aux.o SELECT name FROM pragma_database_list;
         UPDATE aux.o SET v = (SELECT count(*) FROM pragma_table_info('m'));
         INSERT INTO aux.o SELECT name FROM dbstat;
         CREATE TABLE aux.x AS SELECT name FROM pragma_table_info('m');
         WITH c AS (SELECT name FROM pragma_table_info('m')) INSERT INTO aux.o SELECT * FROM c",
    );
    let n = s.len();
    s.extend(refused.iter().cloned());
    s.push("SELECT count(*) FROM aux.o".to_string());
    let (h, o) = answers("fnref", &s);
    for i in n..n + refused.len() {
        assert_eq!(h[i], vec!["!".to_string()], "{}", s[i]);
        assert!(!o[i].contains(&"!".to_string()), "pin answers {}", s[i]);
    }
    assert_eq!(h[n + refused.len()], vec!["0".to_string()]);
}

const IN_SETUP: &str = "
CREATE TABLE t(a);
CREATE TABLE u(c, d);
CREATE INDEX ui ON u(c);
CREATE VIEW v AS SELECT a FROM t;
ATTACH '{d}/a.db' AS aux;
CREATE TABLE aux.w(z);
";

/// `IN` and an OR of `=` on a table-valued function's arguments: one
/// filter per distinct, sorted, non-NULL value, as the pin's IN loop
/// over the argument (review fix: the pragma check tables returned no
/// rows, generate_series ignored the list).
#[test]
fn in_arguments_of_table_functions() {
    let q = "
        SELECT * FROM pragma_quick_check WHERE arg IN ('t');
        SELECT * FROM pragma_quick_check WHERE arg IN ('t', 'u');
        SELECT * FROM pragma_quick_check WHERE arg = 't' OR arg = 'u';
        SELECT * FROM pragma_quick_check WHERE arg IN (SELECT 't');
        SELECT * FROM pragma_quick_check WHERE arg = 't' AND schema IN ('main', 'temp');
        SELECT * FROM pragma_quick_check WHERE arg IN ('t', 'zz');
        SELECT * FROM pragma_quick_check WHERE arg IN ('zz', 't');
        SELECT arg FROM pragma_quick_check WHERE arg IN ('u', 't', 'u');
        SELECT arg FROM pragma_quick_check WHERE arg = 'u' OR arg = 't';
        SELECT arg FROM pragma_quick_check WHERE arg IN (SELECT 'u' UNION ALL SELECT 't' UNION ALL SELECT 'u');
        SELECT arg, schema FROM pragma_quick_check WHERE arg IN ('u', 't') AND schema IN ('main', 'main');
        SELECT arg, schema FROM pragma_integrity_check WHERE arg IN ('w', 't') AND schema IN ('aux', 'main');
        SELECT arg FROM pragma_quick_check WHERE arg IN ('u', NULL);
        SELECT arg, * FROM pragma_quick_check WHERE arg IN (NULL);
        SELECT arg, * FROM pragma_quick_check WHERE arg IN (NULL, NULL);
        SELECT arg, * FROM pragma_quick_check WHERE arg IN (SELECT NULL);
        SELECT value FROM generate_series(1, 3) WHERE step IN (NULL);
        SELECT step, value FROM generate_series(1, 4) WHERE step IN (3, '2');
        SELECT arg FROM pragma_quick_check WHERE arg IN ('u', 3, '2', x'74');
        SELECT step FROM generate_series(1, 4) WHERE step IN (2.0, 1, '1');
        SELECT arg FROM pragma_quick_check WHERE arg IN (1, 't');
        SELECT arg FROM pragma_quick_check WHERE arg IN ('u', 't') ORDER BY arg DESC;
        SELECT a.arg FROM pragma_quick_check a WHERE a.arg IN (SELECT name FROM sqlite_schema);
        SELECT arg FROM pragma_integrity_check WHERE arg IN ('t', 'u') AND schema = 'main';
        SELECT arg, schema FROM pragma_integrity_check WHERE schema IN ('main', 'aux');
        SELECT arg FROM pragma_quick_check WHERE arg IN ();
        SELECT arg FROM pragma_quick_check WHERE arg NOT IN ('t');
        SELECT arg FROM pragma_quick_check WHERE arg NOT IN (SELECT 't');
        SELECT value FROM generate_series(1, 3) WHERE step NOT IN (SELECT 2);
        SELECT arg FROM pragma_quick_check WHERE arg = 't' OR arg = 'u' OR arg = 't';
        SELECT arg FROM pragma_quick_check WHERE arg = 't' OR schema = 'main';
        SELECT arg FROM pragma_quick_check WHERE arg IN ('v', 'w');
        SELECT x, arg FROM (SELECT 't' x UNION SELECT 'u'), pragma_quick_check WHERE arg IN (x, 'u');
        SELECT x, arg FROM (SELECT 't' x UNION SELECT 'u') LEFT JOIN pragma_quick_check ON arg IN (x);
        SELECT count(*) FROM pragma_quick_check WHERE arg IN ('t', 'u');
        SELECT EXISTS (SELECT 1 FROM pragma_quick_check WHERE arg IN ('t', 'zz'));
        SELECT (SELECT arg FROM pragma_quick_check WHERE arg IN ('u', 't'));
        SELECT value, step FROM generate_series(1, 3) WHERE step IN (2, 1, 2);
        SELECT value FROM generate_series(1, 3) WHERE start IN (1, 2);
        SELECT value FROM generate_series WHERE start IN (1, 5) AND stop = 6;
        SELECT value FROM generate_series(1, 3) WHERE step IN (1, 2) AND stop IN (2, 3);
        SELECT value FROM generate_series(1, 3) WHERE step IN (1, 2) OR value = 2;
        SELECT value FROM generate_series(1, 3) WHERE step = 1 OR step = 2;
        SELECT x, value FROM (SELECT 1 x UNION SELECT 2), generate_series(1, 3) WHERE step IN (x, 3);
        SELECT value FROM generate_series(1, 10) WHERE step IN (3, 4) LIMIT 3;
        SELECT value FROM generate_series(1, 3) WHERE step IN ('2');
        SELECT value FROM generate_series(1, 3) WHERE step IN (SELECT 2 UNION SELECT 1);
        SELECT value FROM generate_series((SELECT 1), 3);
        SELECT (SELECT 5), value FROM generate_series((SELECT 1), 3) WHERE step IN (SELECT 1);
    ";
    let mut s = lines(IN_SETUP);
    for l in lines(q) {
        s.push(format!("EXPLAIN QUERY PLAN {l}"));
        s.push(l);
    }
    same("inargs", &s);
}

/// The IN rule's shapes, generated: lists and ORs of `=` (values,
/// duplicates, NULLs, unknown tables, numbers) on the pragma check
/// tables' `arg` / `schema` and generate_series' arguments.
#[test]
fn generated_in_arguments() {
    let mut rng = Rng(0x5eed_0f1e_2026_1122);
    let vals = [
        "'t'",
        "'u'",
        "'v'",
        "'w'",
        "'zz'",
        "NULL",
        "1",
        "'T'",
        "'sqlite_master'",
    ];
    let schemas = ["'main'", "'aux'", "'temp'", "NULL", "'nodb'"];
    let nums = ["1", "2", "3", "NULL", "'2'", "0"];
    let list = |rng: &mut Rng, xs: &[&str]| -> String {
        let n = 1 + rng.int(3);
        (0..n).map(|_| rng.pick(xs)).collect::<Vec<_>>().join(", ")
    };
    let mut s = lines(IN_SETUP);
    let n = std::env::var("HIPP_STX_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u64);
    for _ in 0..n {
        let f = rng.pick(&["pragma_quick_check", "pragma_integrity_check"]);
        let arg = match rng.int(4) {
            0 => format!("arg IN ({})", list(&mut rng, &vals)),
            1 => {
                let (a, b) = (rng.pick(&vals), rng.pick(&vals));
                format!("(arg = {a} OR arg = {b})")
            }
            2 => format!(
                "arg IN (SELECT name FROM sqlite_schema WHERE type = {})",
                rng.pick(&["'table'", "'view'", "'index'"])
            ),
            _ => format!("arg = {}", rng.pick(&vals)),
        };
        let q = match rng.int(3) {
            0 => format!("SELECT arg, schema, * FROM {f} WHERE {arg}"),
            1 => format!(
                "SELECT arg, schema FROM {f} WHERE {arg} AND schema IN ({})",
                list(&mut rng, &schemas)
            ),
            _ => {
                let c = rng.pick(&["start", "stop", "step"]);
                format!(
                    "SELECT value, start, stop, step FROM generate_series(1, 4) WHERE {c} IN ({})",
                    list(&mut rng, &nums)
                )
            }
        };
        s.push(q);
    }
    same("genin", &s);
}

/// The check tables' table argument found as the pin's
/// sqlite3FindTable finds it: `sqlite_master` / `sqlite_schema` in
/// every database, the temp schema's other names only there, a
/// catalog table or view otherwise (review fix: `('sqlite_master',
/// 'temp')` was "no such table", `('sqlite_temp_master', 'main')` and
/// `('sqlite_sequence')` were checked).
#[test]
fn integrity_tvf_schema_tables() {
    let mut s = lines(
        "CREATE TABLE t(a);
         CREATE VIEW v AS SELECT 1;
         CREATE INDEX ti ON t(a);
         CREATE TEMP TABLE tt(b);
         CREATE TEMP VIEW tv AS SELECT 2;
         ATTACH '{d}/a.db' AS aux;
         CREATE TABLE aux.x(c);
         CREATE VIEW aux.av AS SELECT 3",
    );
    for a in [
        "sqlite_master",
        "sqlite_schema",
        "sqlite_temp_master",
        "sqlite_temp_schema",
        "SQLITE_MASTER",
        "sqlite_sequence",
        "sqlite_stat1",
        "sqlite_foo",
        "pragma_foo",
        "v",
        "tv",
        "av",
        "ti",
    ] {
        s.push(format!("SELECT '{a}', * FROM pragma_quick_check('{a}')"));
        for d in ["main", "temp", "aux"] {
            s.push(format!(
                "SELECT '{a} {d}', * FROM pragma_quick_check('{a}', '{d}')"
            ));
        }
    }
    same("icschema", &s);
}

/// The check's unqualified lookup opens the TEMP database, a main
/// table's check too (review fix: `pragma_database_list` lacked it).
#[test]
fn integrity_tvf_opens_temp() {
    for q in [
        "SELECT * FROM pragma_quick_check('t')",
        "SELECT * FROM pragma_integrity_check('t')",
        "SELECT * FROM pragma_quick_check WHERE arg = 't'",
        "SELECT * FROM pragma_quick_check('x')",
        "SELECT * FROM pragma_quick_check('zz')",
        "SELECT * FROM pragma_quick_check('t', 'main')",
        "SELECT * FROM pragma_quick_check(NULL, 'main')",
        "SELECT * FROM pragma_quick_check('t', 'nodb')",
    ] {
        let mut s = lines(
            "CREATE TABLE t(a);
             ATTACH '{d}/a.db' AS aux;
             CREATE TABLE aux.x(b)",
        );
        s.push(q.to_string());
        s.push("SELECT name FROM pragma_database_list".to_string());
        same("ictemp", &s);
    }
}

/// A corrupt attached file read where the reader stops first: EXISTS
/// and a scalar subquery take the rows before the error, as the pin's
/// do (review fix: the error came instead).
#[test]
fn integrity_tvf_corrupt_readers_that_stop() {
    let s = lines(
        "CREATE TABLE t(a);
         ATTACH '{d}/a.db' AS aux;
         SELECT EXISTS (SELECT 1 FROM pragma_quick_check);
         SELECT (SELECT quick_check FROM pragma_quick_check);
         SELECT (SELECT quick_check FROM pragma_quick_check LIMIT 1 OFFSET 1);
         SELECT EXISTS (SELECT 1 FROM pragma_quick_check WHERE quick_check = 'zz');
         SELECT 1 WHERE 'ok' IN (SELECT * FROM pragma_quick_check);
         SELECT EXISTS (SELECT 1 FROM pragma_quick_check WHERE arg IN ('t', 'u'));
         SELECT * FROM pragma_quick_check WHERE arg IN ('t', 'u');
         SELECT * FROM pragma_quick_check WHERE schema IN ('main', 'aux') AND arg IS NULL;
         SELECT arg, schema FROM pragma_quick_check WHERE arg IS NULL AND schema IN ('aux', 'main')",
    );
    let (h, o) = answers_prep("iccorstop", &s, &corrupt_aux);
    for (i, (a, b)) in h.iter().zip(o.iter()).enumerate() {
        assert_eq!(a, b, "statement {}: {}", i + 1, s[i]);
    }
}

/// TEMP views over the pragma check tables and over pragma_table_list
/// (review fix: a view over pragma_table_list listed itself without
/// end, overflowing the stack).
#[test]
fn temp_views_over_pragma_tables() {
    let s = lines(
        "CREATE TABLE t(a);
         CREATE TEMP VIEW tv AS SELECT * FROM pragma_quick_check;
         CREATE TEMP VIEW tv2(c1) AS SELECT * FROM pragma_quick_check;
         CREATE TEMP VIEW tv3 AS SELECT quick_check AS qc FROM pragma_quick_check;
         CREATE TEMP VIEW tv4 AS SELECT quick_check, arg FROM pragma_quick_check('t');
         CREATE TEMP VIEW tl AS SELECT * FROM pragma_table_list;
         SELECT * FROM tv;
         SELECT * FROM tv2;
         SELECT * FROM tv3;
         SELECT * FROM tv4;
         SELECT * FROM tv, tv3;
         SELECT * FROM tl WHERE name NOT LIKE 'tv%' ORDER BY 1, 2;
         SELECT name, ncol FROM tl WHERE name = 'tl';
         SELECT schema, name, ncol FROM pragma_table_list WHERE name IN ('tl', 't') ORDER BY 2",
    );
    same("tvprag", &s);
}

/// The plans of attached and TEMP writes reading another database's
/// tables directly (a view read so keeps a co-routine, an honest gap:
/// STATUS.md).
#[test]
fn cross_database_write_plans() {
    let s = lines(
        "CREATE TABLE m(x INTEGER PRIMARY KEY, y, z);
         INSERT INTO m VALUES (1, 2, 3), (2, 4, 6), (5, 1, 1);
         ATTACH '{d}/a.db' AS aux;
         CREATE TABLE aux.t(a INTEGER PRIMARY KEY, c);
         INSERT INTO aux.t VALUES (1, 0), (2, 0);
         EXPLAIN QUERY PLAN INSERT INTO aux.t(c) SELECT y FROM m;
         EXPLAIN QUERY PLAN UPDATE aux.t SET c = 1 WHERE a = 2;
         EXPLAIN QUERY PLAN DELETE FROM aux.t WHERE a > 1;
         INSERT INTO aux.t(c) SELECT y FROM m;
         SELECT * FROM aux.t",
    );
    same("xwplans", &s);
}
