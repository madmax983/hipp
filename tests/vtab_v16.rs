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
    // Review-fix regressions: its shadow tables (fts4 declares
    // `'c0x'` columns) read as ordinary tables, VACUUM and a full
    // integrity_check run, PRAGMA table_list counts their columns.
    on_pin_file(
        "fts4-shadow-read",
        setup,
        "select * from f_content; select count(*) from f_segdir; select docid, hex(size) from f_docsize; \
         select name, ncol, wr, type from pragma_table_list where name like 'f\\_%' escape '\\' \
         order by name; pragma integrity_check; vacuum; pragma integrity_check; \
         select * from f_content; select type, name from sqlite_schema order by name; \
         create table 'q'(x, 'y z'); insert into q values (1, 2); select \"y z\" from q; \
         pragma table_info(q)",
    );
    let base = common::temp_db("v16vt-fts4-base2");
    let (ok, _) = oracle_script(&base, setup);
    assert!(ok);
    for (i, s) in [
        "select * from f",
        "insert into f values ('x')",
        "drop table f",
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

// ---------------- review fixes (v0.16) ----------------

/// generate_series with a negative step under `value` constraints:
/// the first value moved below the upper bound (the filter's test
/// was `<`, so rows above a consumed `value <` / `=` came back), and
/// a step of `i64::MIN` (its magnitude is 2^63).
#[test]
fn series_descending_constraints() {
    same(
        "gs-desc",
        "select value from generate_series(100,1,-10) where value = 51; \
         select value from generate_series(100,1,-10) where value < 55; \
         select value from generate_series(100,1,-10) where value > 12 and value < 29; \
         select value from generate_series(10,1,-3) where value = 5; \
         select value from generate_series(10,1,-3) where value <= 8; \
         select value from generate_series(10,1,-3) where value between 2 and 6; \
         select value from generate_series(-5,-50,-7) where value < -6.5; \
         select value from generate_series(9223372036854775807, -9223372036854775808, -9223372036854775808); \
         select value from generate_series(-9223372036854775808, 9223372036854775807, -9223372036854775808); \
         select value, step from generate_series(9223372036854775807, 0, -9223372036854775808) where value < 5",
    );
    // A generated corpus of descending series under `value` bounds.
    let mut rng = Rng(0xde5c_e4d5);
    let mut script = String::new();
    for _ in 0..200 {
        let start = rng.range(-60, 60);
        let stop = rng.range(-60, 60).min(start);
        let step = -rng.range(1, 13);
        let mut q = format!("select value from generate_series({start}, {stop}, {step})");
        let ops = ["<", "<=", ">", ">=", "="];
        for j in 0..rng.range(1, 2) {
            let op = ops[rng.below(ops.len() as u64) as usize];
            let v = rng.range(-65, 65);
            q.push_str(if j == 0 { " where " } else { " and " });
            q.push_str(&format!("value {op} {v}"));
        }
        script.push_str(&q);
        script.push_str(";\n");
    }
    same("gs-desc-gen", &script);
}

/// Where a statement holds a virtual-table cursor's rows in memory,
/// past `Config::max_vtab_rows` it fails cleanly (it aborted the
/// process out of memory); a LIMIT stays under it. (v0.17: a streamed
/// plan holds none, so only blocking operators are bounded.)
#[test]
fn series_row_bound() {
    block_on(async {
        let cfg = Config::default().max_vtab_rows(1000);
        let mut db = Connection::open(hipp::storage::MemStorage::new(), cfg)
            .await
            .expect("open");
        let one = |n: &str| format!("select count(*) from generate_series(1, {n})");
        for (q, want) in [
            (one("1000"), Some(1000)),
            (
                "select value from generate_series(1) limit 3".to_string(),
                Some(1),
            ),
            // v0.17: a streamed aggregate is not bounded (the pin's
            // answer); what a blocking operator buffers still is.
            (one("1001"), Some(1001)),
            (
                "select count(distinct value) from generate_series(1, 1001)".to_string(),
                None,
            ),
            (
                "select value from generate_series(1, 1001) order by value desc".to_string(),
                None,
            ),
        ] {
            let mut st = db.prepare(&q).await.expect("prepare");
            match (st.step().await, want) {
                (Ok(Step::Row), Some(n)) => {
                    assert_eq!(st.column_value(0), Some(hipp::Value::Integer(n)), "{q}");
                }
                (Err(Error::Schema { kind }), None) => {
                    assert_eq!(kind, SchemaKind::VtabTooManyRows, "{q}")
                }
                (r, w) => panic!("{q}: {r:?} (wanted {w:?})"),
            }
        }
    });
}

/// CREATE VIRTUAL TABLE whose shadow tables' names are taken fails
/// and creates nothing (hipp wrote a second `r_node`, a schema the pin
/// refuses as malformed); names a module reserves for its shadow
/// tables are refused to CREATE TABLE / VIEW / INDEX / TRIGGER /
/// RENAME TO / TEMP objects.
#[test]
fn shadow_names_are_reserved() {
    for (i, (setup, create)) in [
        (
            "create table r_node(a); insert into r_node values ('userdata')",
            "create virtual table r using rtree(id, x0, x1)",
        ),
        (
            "create table r_parent(a)",
            "create virtual table r using rtree(id, x0, x1)",
        ),
        (
            "create table r_rowid(a)",
            "create virtual table r using rtree_i32(id, x0, x1)",
        ),
        (
            "create view f_data as select 1",
            "create virtual table f using fts5(x)",
        ),
        (
            "create table f_config(a)",
            "create virtual table f using fts5(x)",
        ),
        (
            "create table f_docsize(a)",
            "create virtual table f using fts5(x, content='')",
        ),
        (
            "create table f_idx(a)",
            "create virtual table f using fts5(x, content='', columnsize=0)",
        ),
    ]
    .iter()
    .enumerate()
    {
        let e = same(
            &format!("clash{i}"),
            &format!("{setup}; {create}; select 1"),
        );
        assert!(
            matches!(
                e,
                Some(Error::Schema {
                    kind: SchemaKind::DuplicateTable
                })
            ),
            "{create}: {e:?}"
        );
        same(
            &format!("clash-after{i}"),
            &format!(
                "{setup}; select type, name from sqlite_schema order by name; pragma integrity_check"
            ),
        );
    }
    // A contentless table's unused `_content` / `_docsize` names stay
    // free beside it only before it exists.
    same(
        "clash-free",
        "create table x_content(a); create virtual table x using fts5(a, content=''); \
         select type, name from sqlite_schema order by name",
    );
    for (i, s) in [
        "create table h_content(a)",
        "create table H_CONTENT(a)",
        "create view h_content as select 1",
        "create index h_content on t(a)",
        "alter table t rename to h_content",
        "create temp table h_content(a)",
        "create temp view h_docsize as select 1",
        "create virtual table h_content using fts5(y)",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("reserved{i}"),
            &format!("create table t(a); create virtual table h using fts5(x, content=''); {s}"),
            SchemaKind::ReservedName,
        );
    }
    // CREATE TRIGGER (its body's `;` keeps it out of the scripts).
    block_on(async {
        let mut db = Connection::open(hipp::storage::MemStorage::new(), Config::default())
            .await
            .expect("open");
        for s in [
            "create table t(a)",
            "create virtual table h using fts5(x, content='')",
        ] {
            let mut st = db.prepare(s).await.expect("prepare");
            assert!(matches!(st.step().await, Ok(Step::Done)));
        }
        let r = db
            .prepare("create trigger h_content after insert on t begin select 1; end")
            .await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: SchemaKind::ReservedName
                })
            ),
            "{:?}",
            r.err()
        );
    });
    same(
        "reserved-ok",
        "create table t(a); create virtual table h using fts5(x, content=''); \
         create table h_other(a); create table h_docsize2(a); create table hx_content(a); \
         select name from sqlite_schema order by name",
    );
}

/// `dbstat(schema)`: an unknown schema at CREATE is "no such
/// database" (hipp made a table that read nothing); `temp` and
/// attached databases list their pages.
#[test]
fn dbstat_other_databases() {
    let aux = common::temp_db("v16vt-dbstat-aux");
    let (ok, _) = oracle_script(
        &aux,
        "create table z(a, b); insert into z select value, randomblob(300) from generate_series(1, 40); \
         create index zi on z(a)",
    );
    assert!(ok);
    let a = aux.display();
    same(
        "dbstat-db",
        &format!(
            "create table t(a); select * from dbstat('temp'); \
             create temp table tt(a); insert into tt values (1); select * from dbstat('temp'); \
             attach '{a}' as aux; select * from dbstat('aux'); \
             select name, pageno, schema from dbstat where schema = 'aux'; \
             select * from dbstat('AUX', 1); select count(*) from dbstat('nosuch'); \
             create virtual table q2 using dbstat(aux); select name, schema from q2; \
             create virtual table q3 using dbstat(temp); create virtual table q4 using dbstat('main'); \
             select count(*) from q4"
        ),
    );
    refuse(
        "dbstat-nosuch",
        "create virtual table q using dbstat(nosuch)",
        SchemaKind::UnknownDatabase,
    );
    refuse(
        "dbstat-nosuch2",
        "create virtual table q using dbstat(x, y, z)",
        SchemaKind::UnknownDatabase,
    );
}

/// `X RIGHT JOIN <virtual table> ON <its columns>`: the ON stays a
/// join condition (it was refused as a lateral argument list).
#[test]
fn right_join_of_virtual_tables() {
    same(
        "rjoin",
        "create virtual table r using rtree(id, a, b); \
         insert into r values (1, 0, 10), (2, 5, 6), (3, 20, 30); \
         create table t(k, v); insert into t values (1, 5), (2, 25), (9, 100); \
         select k, id from t right join r on r.id = t.k order by 1, 2; \
         select k, value from t right join generate_series(1, 3) on value = t.k order by 1, 2; \
         select k, id from t right join r on r.a <= t.v and r.b >= t.v order by 1, 2; \
         select k, id from t full join r on r.id = t.k order by 1, 2; \
         select k, value from t right join generate_series(1, 3) on value = t.k where value > 1 order by 1, 2; \
         select k, id from r left join t on r.id = t.k order by 1, 2",
    );
}

/// `PRAGMA writable_schema` and the `.dump` replay of a virtual
/// table: its schema row, then its shadow tables' rows.
#[test]
fn writable_schema_replays_a_dump() {
    let replay = "PRAGMA writable_schema=ON; \
        INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','r','r',0,'CREATE VIRTUAL TABLE r using rtree(id,a,b)'); \
        CREATE TABLE IF NOT EXISTS \"r_rowid\"(rowid INTEGER PRIMARY KEY,nodeno); \
        INSERT INTO r_rowid VALUES(1,1); \
        CREATE TABLE IF NOT EXISTS \"r_node\"(nodeno INTEGER PRIMARY KEY,data); \
        INSERT INTO r_node VALUES(1,x'000000010000000000000001400000004040000000000000000000000000000000000000000000000000000000000000'); \
        CREATE TABLE IF NOT EXISTS \"r_parent\"(nodeno INTEGER PRIMARY KEY,parentnode); \
        PRAGMA writable_schema=OFF";
    // (A node of 48 bytes: the file's page size makes the pin's
    // node size; the test reads the row back, not the tree.)
    let h = common::temp_db("v16vt-ws-h");
    let (ok, _, e) = hipp_script(&h, &format!("pragma page_size=512; {replay}"));
    assert!(ok, "{e:?}");
    let (ok, rows, e) = hipp_script(
        &h,
        "pragma writable_schema; select type, name, rootpage from sqlite_schema order by name; \
         select * from r_rowid",
    );
    assert!(ok, "{e:?}");
    assert_eq!(
        rows,
        [
            "0",
            "table|r|0",
            "table|r_node|3",
            "table|r_parent|4",
            "table|r_rowid|2",
            "1|1"
        ]
    );
    // Off, the schema table and the shadow tables are read only again.
    for (i, (s, kind)) in [
        ("insert into sqlite_schema values ('table','q','q',0,'CREATE VIRTUAL TABLE q using rtree(id,a,b)')", SchemaKind::SchemaTableReadOnly),
        ("insert into r_rowid values (2, 1)", SchemaKind::ShadowReadOnly),
        ("pragma writable_schema = 1; insert into sqlite_schema values ('table','r','r',0,'CREATE VIRTUAL TABLE r using rtree(id,a,b)')", SchemaKind::DuplicateTable),
    ]
    .iter()
    .enumerate()
    {
        let c = common::temp_db(&format!("v16vt-ws-{i}"));
        std::fs::copy(&h, &c).expect("copy");
        let (ok, _, e) = hipp_script(&c, s);
        assert!(!ok, "{s}");
        assert!(matches!(e, Some(Error::Schema { kind: k }) if k == *kind), "{s}: {e:?}");
    }
    // Only a virtual table's row is written that way.
    for (i, s) in [
        "insert into sqlite_schema values ('table','q','q',2,'CREATE TABLE q(a)')",
        "insert into sqlite_schema values ('view','v','v',0,'CREATE VIEW v AS SELECT 1')",
        "insert into sqlite_schema values ('table','q','x',0,'CREATE VIRTUAL TABLE q using rtree(id,a,b)')",
        "insert into sqlite_schema select 'table','q','q',0,'CREATE VIRTUAL TABLE q using rtree(id,a,b)'",
    ]
    .iter()
    .enumerate()
    {
        let c = common::temp_db(&format!("v16vt-ws-only{i}"));
        let (ok, _, e) = hipp_script(&c, &format!("pragma writable_schema = on; {s}"));
        assert!(!ok, "{s}: {e:?}");
    }
}
