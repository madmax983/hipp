//! v0.16 R*Tree (leaf `rtree`), differentially vs the pinned
//! oracle (SQLite 3.54.1, `SQLITE_ENABLE_RTREE`):
//!
//! - `rtree` / `rtree_i32` of 1 to 5 dimensions with auxiliary
//!   `+columns`: CREATE (the shadow tables `%_node` / `%_parent` /
//!   `%_rowid` with the pin's text, the empty root of the node size
//!   the page size gives), its argument errors, DROP, ALTER TABLE
//!   RENAME.
//! - Writes as the pin's `rtreeUpdate`: float32 coordinates rounded
//!   outward (`rtreeValueDown` / `rtreeValueUp`), integers clamped
//!   to 32 bits, the id column's integer reading, "rtree constraint
//!   failed" and the UNIQUE conflict under every conflict clause,
//!   REPLACE, UPDATE (scan order) and DELETE (rowid order).
//! - The tree itself: ChooseLeaf, the R*-tree split, the forced
//!   reinsertion (once per level per connection, as the pin keeps
//!   `iReinsertHeight`), node removal and reinsertion on delete,
//!   node numbering. Generated corpora of 1-5 dimensional inserts,
//!   deletes and updates compare every shadow-table row byte for
//!   byte, and each engine's `rtreecheck()` / integrity_check passes
//!   on the other's file; files move between the engines mid-way.
//! - Reads: rowid lookups, `=`, `<`, `<=`, `>`, `>=`, BETWEEN on
//!   coordinates through xBestIndex, in the pin's order; MATCH is
//!   refused (as by the pin without a geometry callback).
//! - `rtreecheck()`, `rtreenode()`, `rtreedepth()`; transactions,
//!   savepoints and statement rollback over the shadow tables.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// One row as the pin's shell prints it in list mode.
fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
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
}

/// Split a script on `;` outside quotes.
fn split(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in script.chars() {
        match q {
            Some(x) if c == x => q = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '[' => q = Some(if c == '[' { ']' } else { c }),
            None if c == ';' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            None => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A script on one hipp connection, as the CLI runs it: every row,
/// stopping at the first error.
fn hipp_script(
    path: &std::path::Path,
    script: &str,
) -> (bool, Vec<String>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        for stmt in split(script) {
            let mut q = match db.prepare(&stmt).await {
                Ok(q) => q,
                Err(e) => return (false, lines, Some(e)),
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        for part in l.split('\n') {
                            lines.push(part.to_string());
                        }
                    }
                    Ok(Step::Done) => break,
                    Err(e) => return (false, lines, Some(e)),
                }
            }
        }
        (true, lines, None)
    })
}

/// The same script on the pin's shell (`-bail`: it stops at the
/// first error, as `hipp_script` does).
fn oracle_script(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(path)
        .arg(script)
        .output()
        .expect("oracle runs");
    (
        out.status.success() && out.stderr.is_empty(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// A script on fresh twin files: same rows, same success; hipp's
/// error when both failed.
fn same(tag: &str, script: &str) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v16rt-{tag}-h"));
    let o = common::temp_db(&format!("v16rt-{tag}-o"));
    same_on(&h, &o, script)
}

fn same_on(h: &std::path::Path, o: &std::path::Path, script: &str) -> Option<Error<FsError>> {
    let (hok, hl, he) = hipp_script(h, script);
    let (ook, ol) = oracle_script(o, script);
    assert_eq!(hl, ol, "script: {script}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {script}\nhipp error: {he:?}");
    he
}

/// Both refuse; hipp with `kind`.
fn refuse(tag: &str, script: &str, kind: SchemaKind) {
    match same(tag, script) {
        Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "{script}"),
        other => panic!("expected {kind:?} for {script}, got {other:?}"),
    }
}

/// A deterministic xorshift generator.
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
}

/// Every statement of `script` on fresh twin files, continuing past
/// errors as the shells do without `-bail`: the same stdout lines
/// and the same number of failed statements.
fn same_all(tag: &str, script: &str) {
    let h = common::temp_db(&format!("v16rt-{tag}-h"));
    let o = common::temp_db(&format!("v16rt-{tag}-o"));
    same_all_on(&h, &o, script);
}

fn same_all_on(h: &std::path::Path, o: &std::path::Path, script: &str) {
    let (hl, herr) = block_on(async {
        let mut db = open(h).await;
        let mut lines = Vec::new();
        let mut errs = 0usize;
        for stmt in split(script) {
            let mut q = match db.prepare(&stmt).await {
                Ok(q) => q,
                Err(_) => {
                    errs += 1;
                    continue;
                }
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        for part in l.split('\n') {
                            lines.push(part.to_string());
                        }
                    }
                    Ok(Step::Done) => break,
                    Err(_) => {
                        errs += 1;
                        break;
                    }
                }
            }
        }
        (lines, errs)
    });
    let out = std::process::Command::new(common::oracle())
        .arg(o)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            let mut sin = c.stdin.take().expect("stdin");
            for stmt in split(script) {
                writeln!(sin, "{stmt};")?;
            }
            drop(sin);
            c.wait_with_output()
        })
        .expect("oracle runs");
    let ol: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let oerr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("rror"))
        .count();
    assert_eq!(hl, ol, "script: {script}");
    assert_eq!(herr, oerr, "error count differs: {script}");
}

/// The rows of `sql` from the pin on `db`.
fn pin_rows(db: &std::path::Path, sql: &str) -> Vec<String> {
    oracle_script(db, sql).1
}

/// The rows of `sql` from hipp on `db`.
fn hipp_rows(db: &std::path::Path, sql: &str) -> Vec<String> {
    let (ok, l, e) = hipp_script(db, sql);
    assert!(ok, "{sql}: {e:?}");
    l
}

const SHADOW: &str = "select nodeno, hex(data) from t_node order by nodeno; \
    select * from t_parent order by nodeno; select * from t_rowid order by rowid";

// ---------------- CREATE ----------------

#[test]
fn create_and_schema() {
    same(
        "create",
        "create virtual table r using rtree(id, x0 , x1, +aux1, +\"aux 2\" text); \
         select rowid, * from sqlite_schema; select length(data), hex(data) = hex(zeroblob(length(data))) from r_node; \
         pragma table_info(r); pragma table_xinfo(r); \
         select * from pragma_table_list where type in ('virtual', 'shadow') order by name; \
         create virtual table if not exists r using rtree(id, a, b); \
         create virtual table r3 using rtree_i32(id, a, b, c, d, e, f); \
         select length(data) from r3_node; pragma integrity_check",
    );
    for ps in [512, 1024, 4096, 8192, 65536] {
        same(
            &format!("nodesize{ps}"),
            &format!(
                "pragma page_size = {ps}; create virtual table a using rtree(i, x, y); \
                 create virtual table b using rtree(i, x0, x1, y0, y1, z0, z1, u0, u1, v0, v1); \
                 select length(data) from a_node; select length(data) from b_node"
            ),
        );
    }
    for (i, (sql, k)) in [
        ("create virtual table r using rtree(id)", SchemaKind::RtreeTooFewColumns),
        ("create virtual table r using rtree(id,a)", SchemaKind::RtreeTooFewColumns),
        ("create virtual table r using rtree", SchemaKind::RtreeTooFewColumns),
        ("create virtual table r using rtree(id,a,b,c)", SchemaKind::RtreeWrongColumns),
        (
            "create virtual table r using rtree(id,a,b,c,d,e,f,g,h,i,j,k,l)",
            SchemaKind::RtreeTooManyColumns,
        ),
        ("create virtual table r using rtree(id,a,b,+c,d)", SchemaKind::RtreeAuxNotLast),
        ("create virtual table r using rtree(id,a,b); create virtual table r using rtree(id,a,b)", SchemaKind::DuplicateTable),
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("bad{i}"), sql, *k);
    }
}

// ---------------- writes and reads ----------------

#[test]
fn writes_corpus() {
    same_all(
        "writes",
        "create virtual table r using rtree(id, x0, x1, y0, y1, +name); \
         insert into r values(1, 0.1, 0.2, 5, 6, 'a'); \
         insert into r values(null, 1, 2, 3, 4, x'00'); \
         insert into r(x0,x1,y0,y1) values(1e40, -1e40, 0, 0); \
         insert into r values(3, 5, 4, 1, 1, 'z'); \
         insert into r values('7', '1.5', '2', 3, 4, '5'); \
         insert into r values(2, 1, 1, 1, 1, 1); \
         insert or replace into r values(2, 9, 9, 9, 9, 'rep'); \
         insert into r values(10.5, 1,1,1,1,1); \
         insert into r values('abc', 1,1,1,1,1); \
         insert or ignore into r values(11, 5, 4, 0, 0, 'ign'); \
         insert or ignore into r values(1, 0, 0, 0, 0, 'ign'); \
         insert into r(name, x0, x1, y0, y1) values ('cols', 1, 2, 3, 4); \
         select * from r; \
         select typeof(id), typeof(x0), typeof(name) from r; \
         select rowid, * from r_rowid; select nodeno, hex(data) from r_node; select * from r_parent; \
         update r set x1 = 0 where id = 7; \
         update r set x1 = 100, id = 50 where id = 7; \
         update r set id = 2 where id = 50; \
         update or replace r set id = 2 where id = 50; \
         select * from r where x0 > 1; select * from r where x0 = 9; \
         select * from r where id between 1 and 5; \
         select rtreecheck('r'); select rtreecheck('main', 'r'); \
         select rtreedepth(data) from r_node where nodeno = 1; \
         select rtreenode(2, data) from r_node where nodeno = 1; \
         delete from r where x0 < 2; select changes(); \
         select * from r; select last_insert_rowid(); \
         insert into r select id + 100, x0, x1, y0, y1, name from r; select * from r; \
         insert into r default values; \
         select rtreecheck('r'); pragma integrity_check",
    );
    // rtree_i32: integer readings, 32-bit clamping.
    same_all(
        "i32",
        "create virtual table r using rtree_i32(id, a, b); \
         insert into r values(1, 1.7, 2.2), (2, -5000000000, 5000000000), (3, 'x', '9'), (4, -1.5, -0.5); \
         insert into r values(5, 3, 2); \
         select * from r; select typeof(a) from r; select rtreecheck('r'); \
         select * from r where a >= -1 and b <= 9; select hex(data) from r_node",
    );
    // Rounding outward at float32 boundaries.
    same_all(
        "round",
        "create virtual table r using rtree(id, a, b); \
         insert into r values (1, 0.1, 0.1), (2, 1e-45, 1e-45), (3, -1e-45, 3.4028235e38), \
         (4, 16777217, 16777217), (5, -16777217, 1e39), (6, 1e300, 1e300), (7, -0.0, 0.0), \
         (8, 1.0000001, 1.0000001), (9, 9007199254740993, 9007199254740993); \
         select * from r; select hex(data) from r_node; \
         select id from r where a = 0.1; select id from r where a <= 0.1 and b >= 0.1",
    );
}

#[test]
fn query_corpus() {
    let setup = "create virtual table r using rtree(id, a, b); \
                 insert into r values(1,1,2),(2,5,6),(3,-1,0.5),(4,2,9);";
    for (i, q) in [
        "select id from r where id='1'",
        "select id from r where a = 'x'",
        "select id from r where a <= 'x'",
        "select id from r where a < 'x'",
        "select id from r where a >= 'x'",
        "select id from r where a > 'x'",
        "select id from r where a > '1'",
        "select id from r where a = x'00'",
        "select id from r where a < 1e300",
        "select id from r where id > 1",
        "select id from r where id = 'a'",
        "select id from r where id in (2,1) order by id",
        "select id from r where a between 1 and 5",
        "select id from r where +a = 1",
        "select id from r where 1 = a",
        "select id from r where b < 2.0000001",
        "select id from r where a is 1",
        "select id from r where a > 9223372036854775807",
        "select id from r where a > 281474976710656",
        "select id from r where a < -281474976710656",
        "select id from r where rowid=2",
        "select id from r where rowid=2.0",
        "select id from r where rowid=2.5",
        "select id from r where a > null",
        "select id from r where a <= 2 and b >= 2 and id > 1",
        "select count(*) from r",
        "select * from r where id = 4 and a = 2",
        "select x.id, y.id from r x, r y where y.a > x.b",
        "select x.id, y.id from r x join r y on y.id = x.id + 1",
        "select id, (select count(*) from r y where y.a < x.a) from r x",
    ]
    .iter()
    .enumerate()
    {
        same_all(&format!("q{i}"), &format!("{setup} {q}"));
    }
    // MATCH needs a geometry callback: refused on both.
    let (ok, _, _) = hipp_script(
        &common::temp_db("v16rt-match-h"),
        &format!("{setup} select id from r where a match 1"),
    );
    assert!(!ok);
    let (ook, _) = oracle_script(
        &common::temp_db("v16rt-match-o"),
        &format!("{setup} select id from r where a match 1"),
    );
    assert!(!ook);
}

#[test]
fn functions_and_errors() {
    same_all(
        "funcs",
        "create virtual table r using rtree(id,x0,x1); insert into r values(1,1,2),(2,3,4); \
         select rtreecheck('r'); select rtreecheck('main','r'); select rtreecheck('nosuch'); \
         select rtreecheck(); select rtreecheck(1, 2, 3); \
         select rtreenode(1, x'00'), rtreedepth(x'0003'); select rtreedepth(x'00'); select rtreedepth('ab'); \
         select rtreenode(257, data) from r_node; select rtreenode(1.9, data) from r_node; \
         select rtreenode(1, ''), typeof(rtreenode(1, x'00000000')), rtreenode(0, data), rtreenode(6, data) from r_node; \
         select rtreecheck(name) from sqlite_schema where name = 'r'; \
         create table plain(a); select rtreecheck('plain'); \
         drop table r_node; insert into r_node values(9,1); update r_parent set parentnode=1; \
         delete from r_rowid; alter table r_node add column z; alter table r_node rename to x; \
         create index i on r(x0); alter table r add column z; alter table r rename column x0 to y; \
         select r_node.nodeno, length(data), r.* from r_node, r where r.id = 1 and r_node.nodeno = 1; \
         create view v as select * from r; select * from v; \
         vacuum; select * from r; select name from sqlite_schema; reindex; select rtreecheck('r'); \
         drop table v; drop view v; alter table r rename to q; select name, sql from sqlite_schema; \
         select * from q; select rtreecheck('q'); insert into q values (3, 5, 6); \
         select * from q where x0 > 2; drop table q; select count(*) from sqlite_schema; \
         pragma integrity_check",
    );
}

#[test]
fn transactions_and_savepoints() {
    same_all(
        "txn",
        "create virtual table r using rtree(id, x0, x1, +t); \
         insert into r select value, value, value + 1, 'v' || value from generate_series(1, 60); \
         begin; insert into r values (100, 1, 2, 'x'); delete from r where id < 20; rollback; \
         select count(*) from r; select rtreecheck('r'); \
         begin; savepoint a; insert into r select value + 200, value, value, null from generate_series(1, 80); \
         savepoint b; delete from r where id % 2 = 0; rollback to b; \
         update r set x1 = x1 + 5 where id > 250; release a; commit; \
         select count(*), sum(x1) from r; select rtreecheck('r'); \
         begin; insert into r values (1000, 1, 2, 'a'), (1001, 3, 2, 'b'); commit; \
         select count(*) from r where id >= 1000; \
         insert or fail into r values (1002, 1, 2, 'a'), (1003, 3, 2, 'b'); \
         select id from r where id >= 1000; \
         begin; insert or rollback into r values (1004, 1, 2, 'a'), (1, 3, 4, 'b'); \
         select id from r where id >= 1000; \
         insert or abort into r values (1005, 1, 2, 'a'), (1, 3, 4, 'b'); \
         select count(*) from r where id >= 1000; \
         savepoint s; delete from r; rollback to s; release s; select count(*) from r; \
         select rtreecheck('r'); pragma integrity_check; select * from t_node limit 0",
    );
}

// ---------------- generated corpora ----------------

/// One generated R*Tree workload: its writes, and the queries that
/// compare the engines afterwards.
fn workload(seed: u64) -> (String, String) {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let nd = 1 + r.below(5) as usize;
    let i32m = r.below(10) < 3;
    let naux = r.below(3) as usize;
    let ps = [512, 1024, 4096][r.below(3) as usize];
    let mut cols = vec!["id".to_string()];
    for k in 0..2 * nd {
        cols.push(format!("c{k}"));
    }
    for k in 0..naux {
        cols.push(format!("+a{k}"));
    }
    let module = if i32m { "rtree_i32" } else { "rtree" };
    let mut w = vec![
        format!("pragma page_size={ps}"),
        format!("create virtual table t using {module}({})", cols.join(",")),
    ];
    let mut ids: Vec<i64> = Vec::new();
    let coord = |r: &mut Rng| -> (String, String) {
        if i32m {
            let a = r.range(-1000, 1000);
            (a.to_string(), (a + r.range(0, 50)).to_string())
        } else {
            let a = r.range(-1_000_000, 1_000_000) as f64 / 1000.0;
            let wd = r.range(0, 50_000) as f64 / 1000.0;
            (format!("{a}"), format!("{}", a + wd))
        }
    };
    let n = 40 + r.below(260);
    for _ in 0..n {
        let x = r.below(100);
        if x < 70 || ids.is_empty() {
            let id = if r.below(2) == 0 {
                let v = r.range(1, 1_000_000);
                ids.push(v);
                v.to_string()
            } else {
                "null".to_string()
            };
            let mut vals = vec![id];
            for _ in 0..nd {
                let (a, b) = coord(&mut r);
                vals.push(a);
                vals.push(b);
            }
            for _ in 0..naux {
                vals.push(if r.below(2) == 0 {
                    r.below(10).to_string()
                } else {
                    format!("'s{}'", r.below(10))
                });
            }
            w.push(format!(
                "insert or ignore into t values({})",
                vals.join(",")
            ));
        } else if x < 85 {
            let id = ids[r.below(ids.len() as u64) as usize];
            w.push(format!("delete from t where id={id}"));
        } else if x < 95 {
            let c = 2 * r.below(nd as u64);
            let id = ids[r.below(ids.len() as u64) as usize];
            w.push(format!("update t set c{c}=c{c}-1 where id={id}"));
        } else {
            let k = r.below(2 * nd as u64);
            let v = r.range(-1000, 1000);
            w.push(format!(
                "delete from t where c{k} < {v} and c{k} > {}",
                v - 30
            ));
        }
    }
    let mut q = vec![
        "select count(*) from t".to_string(),
        "select * from t".to_string(),
        SHADOW.to_string(),
        "select rtreecheck('t')".to_string(),
    ];
    for _ in 0..8 {
        let k = r.below(2 * nd as u64);
        let v = r.range(-1000, 1000);
        let op = ["<", "<=", ">", ">=", "="][r.below(5) as usize];
        q.push(format!("select id from t where c{k} {op} {v}"));
        q.push(format!(
            "select id from t where c{k} between {v} and {} and c{} > {v}",
            v + 100,
            (k + 2) % (2 * nd as u64)
        ));
    }
    (w.join(";\n"), q.join(";\n"))
}

#[test]
fn generated_workloads_match_byte_for_byte() {
    let n: u64 = std::env::var("HIPP_RTREE_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(14);
    for seed in 0..n {
        let (w, q) = workload(seed);
        let h = common::temp_db(&format!("v16rt-gen{seed}-h"));
        let o = common::temp_db(&format!("v16rt-gen{seed}-o"));
        let script = format!("{w};\n{q}");
        let _ = std::fs::write(h.with_extension("sql"), &script);
        same_all_on(&h, &o, &script);
        // Each engine checks the other's file.
        assert_eq!(
            pin_rows(&h, "select rtreecheck('t'); pragma integrity_check"),
            vec!["ok", "ok"],
            "seed {seed}: the pin checks hipp's file"
        );
        assert_eq!(
            hipp_rows(&o, "select rtreecheck('t'); pragma integrity_check"),
            vec!["ok", "ok"],
            "seed {seed}: hipp checks the pin's file"
        );
    }
}

/// A file moves between the engines mid-workload: the pin writes the
/// first half, then each engine writes the second half on its own
/// copy; and the other way round. The shadow tables agree.
#[test]
fn files_move_between_engines() {
    for seed in [101u64, 102, 103, 104] {
        let (w, q) = workload(seed);
        let stmts: Vec<&str> = w.split(";\n").collect();
        let (first, second) = stmts.split_at(stmts.len() / 2);
        let first = first.join(";\n");
        let second = second.join(";\n");
        // Pin first.
        let base = common::temp_db(&format!("v16rt-mv{seed}-base"));
        let (ok, _) = oracle_script(&base, &first);
        assert!(ok);
        let h = common::temp_db(&format!("v16rt-mv{seed}-h"));
        let o = common::temp_db(&format!("v16rt-mv{seed}-o"));
        std::fs::copy(&base, &h).expect("copy");
        std::fs::copy(&base, &o).expect("copy");
        same_all_on(&h, &o, &format!("{second};\n{q}"));
        // hipp first.
        let base = common::temp_db(&format!("v16rt-mv{seed}-base2"));
        let (ok, _, e) = hipp_script(&base, &first);
        assert!(ok, "{e:?}");
        assert_eq!(pin_rows(&base, "select rtreecheck('t')"), vec!["ok"]);
        let o2 = common::temp_db(&format!("v16rt-mv{seed}-o2"));
        std::fs::copy(&base, &o2).expect("copy");
        let pin_after = {
            let (ok, l) = oracle_script(&o2, &format!("{second};\n{SHADOW}"));
            assert!(ok);
            l
        };
        let h2 = common::temp_db(&format!("v16rt-mv{seed}-h2"));
        std::fs::copy(&base, &h2).expect("copy");
        let (ok, hipp_after, e) = hipp_script(&h2, &format!("{second};\n{SHADOW}"));
        assert!(ok, "{e:?}");
        assert_eq!(hipp_after, pin_after, "seed {seed}");
    }
}
