//! v0.16 virtual-table framework (leaf `vtab`), differentially vs
//! the pinned oracle (SQLite 3.54.1, whose shell ships
//! `generate_series` and has `SQLITE_ENABLE_DBSTAT_VTAB`).
//!
//! - `CREATE VIRTUAL TABLE [IF NOT EXISTS] t USING m(args)`: the
//!   `sqlite_schema` row the pin writes (type `table`, root page 0,
//!   `CREATE VIRTUAL TABLE ` and the text from the name token to the
//!   closing parenthesis), its errors, DROP TABLE.
//! - A file holding a virtual table of a module hipp lacks (the
//!   pin's `fts4`) opens; the rest of the file reads and writes;
//!   statements touching that table are "no such module"; its shadow
//!   tables are read only.
//! - `generate_series(start, stop, step)` as the pin's shell ships
//!   it: hidden columns, the constraint handling of its xBestIndex
//!   (`value` =, >, >=, <, <=, BETWEEN; the "first argument" error)
//!   and its filter's 64-bit arithmetic, over a corpus and a
//!   generated one.
//! - `dbstat`: every column, byte for byte, on the same file (one
//!   the pin wrote: overflow pages, free pages, indexes, WITHOUT
//!   ROWID), `aggregate`, `schema`, `name`, ORDER BY name, a `dbstat`
//!   virtual table of the file.
//! - PRAGMA table_info / table_xinfo / table_list of virtual tables.
//!
//! Regression tests for the bugs found along the way are marked.

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
    let h = common::temp_db(&format!("v16vt-{tag}-h"));
    let o = common::temp_db(&format!("v16vt-{tag}-o"));
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

/// A file the pin builds with `setup`; `script` then runs on two
/// copies of it, one per engine.
fn on_pin_file(tag: &str, setup: &str, script: &str) -> Option<Error<FsError>> {
    let base = common::temp_db(&format!("v16vt-{tag}-base"));
    let (ok, _) = oracle_script(&base, setup);
    assert!(ok, "setup failed: {setup}");
    let h = common::temp_db(&format!("v16vt-{tag}-h"));
    let o = common::temp_db(&format!("v16vt-{tag}-o"));
    std::fs::copy(&base, &h).expect("copy");
    std::fs::copy(&base, &o).expect("copy");
    same_on(&h, &o, script)
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

// ---------------- CREATE / DROP VIRTUAL TABLE ----------------

#[test]
fn create_virtual_table_is_stored_as_the_pin_stores_it() {
    same(
        "create",
        "create   virtual table if not exists s using dbstat; \
         CREATE VIRTUAL TABLE \"q t\"   USING   dbstat ( main ) ; \
         create virtual table main.d2 using dbstat(  'main' ,  ); \
         select rowid, * from sqlite_schema; \
         create virtual table if not exists s using nosuch(a); \
         select count(*) from sqlite_schema; pragma integrity_check",
    );
    // A second CREATE of the name, an index of it, unknown and
    // eponymous-only modules.
    refuse(
        "dup",
        "create virtual table s using dbstat; create virtual table s using dbstat",
        SchemaKind::DuplicateTable,
    );
    refuse(
        "dup-table",
        "create table s(a); create virtual table s using dbstat",
        SchemaKind::DuplicateTable,
    );
    refuse(
        "nomod",
        "create virtual table s using nosuch(a, b)",
        SchemaKind::NoSuchModule,
    );
    refuse(
        "series-not-creatable",
        "create virtual table g using generate_series",
        SchemaKind::NoSuchModule,
    );
    refuse(
        "reserved",
        "create virtual table sqlite_x using dbstat",
        SchemaKind::ReservedName,
    );
}

#[test]
fn drop_and_shadowing() {
    same(
        "drop",
        "create virtual table s using dbstat; drop table s; select count(*) from sqlite_schema; \
         drop table if exists s; create table generate_series(value); \
         insert into generate_series values (7); select * from generate_series; \
         pragma integrity_check",
    );
    refuse("drop-missing", "drop table s", SchemaKind::UnknownTable);
    // Writes, indexes and ALTER of a module with no xUpdate.
    for (i, w) in [
        "insert into s(name) values ('x')",
        "delete from s",
        "update s set pgsize = 1",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("ro{i}"),
            &format!("create virtual table s using dbstat; {w}"),
            SchemaKind::VtabReadOnly,
        );
    }
    refuse(
        "index",
        "create virtual table s using dbstat; create index i on s(name)",
        SchemaKind::VtabNotIndexable,
    );
    refuse(
        "alter",
        "create virtual table s using dbstat; alter table s add column z",
        SchemaKind::VtabNotAlterable,
    );
}

/// A file holding a virtual table of a module hipp lacks (the pin's
/// `fts4`): it opens, everything else works, the table and its
/// shadow tables are refused for what hipp cannot honour.
#[test]
fn unknown_module_file_opens() {
    let setup = "create table t(a, b); insert into t values (1, 'x'); \
                 create virtual table f using fts4(body); \
                 insert into f values ('hello world'); create table u(c)";
    on_pin_file(
        "fts4-read",
        setup,
        "select * from t; insert into t values (2, 'y'); select count(*) from t; \
         select name, type from pragma_table_list where name like 'f%' order by name; \
         pragma integrity_check(t)",
    );
    let base = common::temp_db("v16vt-fts4-base2");
    let (ok, _) = oracle_script(&base, setup);
    assert!(ok);
    // (fts4's shadow tables quote their names as strings, which
    // hipp's grammar does not read: they are listed, not read.)
    for (i, s) in [
        "select * from f",
        "insert into f values ('x')",
        "drop table f",
        "select count(*) from f_content",
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16vt-fts4-{i}"));
        std::fs::copy(&base, &h).expect("copy");
        let (ok, _, e) = hipp_script(&h, s);
        assert!(!ok, "{s}");
        assert!(
            matches!(
                e,
                Some(Error::Schema {
                    kind: SchemaKind::NoSuchModule
                })
            ),
            "{s}: {e:?}"
        );
    }
    // A shadow table of the unknown module is never written.
    let h = common::temp_db("v16vt-fts4-shadow");
    std::fs::copy(&base, &h).expect("copy");
    let (ok, _, e) = hipp_script(&h, "delete from f_content");
    assert!(!ok);
    assert!(matches!(
        e,
        Some(Error::Schema {
            kind: SchemaKind::ShadowReadOnly
        })
    ));
}

// ---------------- generate_series ----------------

#[test]
fn generate_series_corpus() {
    let corpus = [
        "select * from generate_series(1,5)",
        "select *, start, stop, step from generate_series(1,10,3)",
        "select rowid, value from generate_series(5,1,-2)",
        "select value from generate_series(1,10) where value > 3 and value < 6",
        "select value from generate_series(1,10) order by value limit 3",
        "select value from generate_series(0.5, 3)",
        "select value from generate_series('2', '4')",
        "select value from generate_series(1,3,0)",
        "select value from generate_series(10) limit 3",
        "select value from generate_series where start=3 and stop=5",
        "select value from generate_series(1, null)",
        "select count(*) from generate_series(1,100) where value % 7 = 0",
        "select value from generate_series(1,20) where value > '3' limit 2",
        "select value from generate_series(1,5) where value=2.5",
        "select value from generate_series(1,5) where value=2.0",
        "select value from generate_series(1,5) where value>=2.5",
        "select value from generate_series(1,5) where rowid=3",
        "select value from generate_series(-9223372036854775808, -9223372036854775806)",
        "select value from generate_series(9223372036854775806, 9223372036854775807)",
        "select value from generate_series(1,10,-3)",
        "select value from generate_series(10,1,-3)",
        "select value from generate_series(1,10,-9223372036854775808)",
        "select value from generate_series(1,5,'x')",
        "select value from generate_series(x'01', 3)",
        "select value from generate_series(1,5) where value is 3",
        "select value from generate_series(1,5) where value < 3 or value > 4",
        "select value from generate_series(3,5) where stop = 4",
        "select value, start from generate_series(3,5) where start = 3",
        "select value from generate_series(1,9) where value > 2 and value > 5",
        "select value from generate_series(1,9) where value > 5 and value > 2",
        "select value, value from generate_series(1,2)",
        "select * from generate_series(1,3) as g where g.value=2",
        "select value from generate_series where value between 3 and 5",
        "select value from generate_series where value >= 3 and value <= 5",
        "select value from generate_series where 4 > value and value > 1",
        "select typeof(value), value from generate_series(1.5,3.5)",
        "select a.value, b.value from generate_series(1,3) a, generate_series(a.value, 3) b",
        "select value from generate_series(1, (select 4))",
        "select (select count(*) from generate_series(1, 9))",
        "select sum(value) from generate_series(1, 1000)",
        "select value from generate_series(5) where value < 8",
        "select value from generate_series(1, 3) union all select value from generate_series(7, 8)",
        "select * from generate_series(1, 2) join generate_series(5, 6) on 1",
        "select value from generate_series(1,5) where value in (2, 4)",
        "pragma table_xinfo(generate_series)",
        "pragma table_info(generate_series)",
    ];
    for (i, q) in corpus.iter().enumerate() {
        same(&format!("gs{i}"), q);
    }
    // Lateral arguments, joins and correlated subqueries.
    same(
        "gs-lateral",
        "create table t(a, b); insert into t values (2, 4), (3, 3), (5, 1), (null, 2); \
         select a, value from t, generate_series(t.a, t.b); \
         select a, (select count(*) from generate_series(1, t.a)) from t; \
         select a, value from t join generate_series(1, 2) on value = t.b",
    );
    // The pin's "first argument" error: no start, an IN list (not a
    // usable start), or a start no plan can supply.
    for (i, q) in [
        "select * from generate_series() limit 2",
        "select value from generate_series where value in (3,5,9)",
        "select value from generate_series where stop = 3",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("gs-first{i}"), q, SchemaKind::SeriesFirstArgument);
    }
    refuse(
        "gs-args",
        "select * from generate_series(1,2,3,4)",
        SchemaKind::WrongArgumentCount,
    );
}

#[test]
fn generate_series_generated() {
    let mut rng = Rng(0x5eed_6e5e);
    let n: usize = std::env::var("HIPP_SERIES_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let mut script = String::new();
    for _ in 0..n {
        let start = rng.range(-30, 30);
        let stop = rng.range(-30, 30);
        let step = rng.range(-7, 7);
        let mut q = match rng.below(3) {
            0 => format!("select value from generate_series({start}, {stop})"),
            1 => format!("select value, step from generate_series({start}, {stop}, {step})"),
            _ => {
                format!("select value from generate_series where start = {start} and stop = {stop}")
            }
        };
        let ops = ["<", "<=", ">", ">=", "=", "<>"];
        let k = rng.below(3);
        for j in 0..k {
            let op = ops[rng.below(ops.len() as u64) as usize];
            let v = rng.range(-35, 35);
            let lit = match rng.below(4) {
                0 => format!("{v}.5"),
                1 => format!("'{v}'"),
                _ => v.to_string(),
            };
            q.push_str(if j == 0 { " where " } else { " and " });
            if rng.below(2) == 0 {
                q.push_str(&format!("value {op} {lit}"));
            } else {
                q.push_str(&format!("{lit} {op} value"));
            }
        }
        script.push_str(&q);
        script.push_str(";\n");
    }
    same("gs-gen", &script);
}

// ---------------- dbstat ----------------

const DBSTAT_FILE: &str = "pragma page_size=1024; \
    create table t(a, b); create index ti on t(b); \
    insert into t select value, randomblob(value * 7) from generate_series(1, 200); \
    create table e(x); \
    create table w(a primary key, b) without rowid; \
    insert into w select value, zeroblob(value * 13) from generate_series(1, 100); \
    create table big(x); insert into big values (zeroblob(5000)); \
    create view v as select 1; create virtual table s using dbstat; \
    delete from t where a % 3 = 0";

#[test]
fn dbstat_on_the_same_file() {
    on_pin_file(
        "dbstat",
        DBSTAT_FILE,
        "select * from dbstat; \
         select * from dbstat where aggregate = 1; \
         select * from dbstat where name = 'e'; \
         select name, path from dbstat where name = 't' and path like '%+%' limit 3; \
         select * from dbstat where schema = 'temp'; \
         select * from dbstat('main', 1); \
         select * from dbstat order by name; \
         select * from s; \
         select * from dbstat where name = 'big'; \
         select * from dbstat where name = 'w'; \
         select * from s('main', 1) where name like 's%'; \
         select count(*), sum(pgsize) from dbstat where aggregate; \
         select * from dbstat('nosuch'); \
         select count(*) from dbstat(null); \
         select * from dbstat where name = 'T'; \
         select name, sum(payload), max(mx_payload) from dbstat group by name order by name; \
         pragma table_xinfo(dbstat); pragma table_info(s)",
    );
    // A fresh, empty and a one-page file.
    same(
        "dbstat-fresh",
        "select * from dbstat; create table a(x); select * from dbstat; select * from dbstat(null, 1)",
    );
}

#[test]
fn table_list_of_virtual_tables() {
    same(
        "tablelist",
        "create virtual table s using dbstat; \
         select * from pragma_table_list where type = 'virtual'; \
         pragma table_xinfo(s)",
    );
}
