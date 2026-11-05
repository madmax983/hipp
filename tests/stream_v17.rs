//! v0.17 streamed rows (`session::stream`), differentially vs the
//! pinned oracle (SQLite 3.54.1): a top-level SELECT returns its rows
//! one `step` at a time, as the pin's VDBE does.
//!
//! - **Rows before an error.** A statement that fails on row N
//!   returns rows 1..N-1 first, on the pin and on hipp: plain scans,
//!   joins (inner, LEFT, a virtual table or a JSON table-valued
//!   function on either side), DISTINCT, ORDER BY rowid, derived
//!   tables and views read by a streaming outer query, and every
//!   virtual-table module (generate_series, dbstat, R*Tree, fts5,
//!   fts5vocab).
//! - **LIMIT / OFFSET.** LIMIT stops the join (`generate_series(1)`
//!   with a LIMIT answers at once); OFFSET rows are skipped
//!   unprojected, as on the pin, so their result columns cannot fail.
//! - **Nested loops.** A virtual table on the inner side is filtered
//!   again for each outer row with that row's values.
//! - **Constant memory.** count(*) / sum() / min / max over
//!   generate_series(1, 50000000) run under a small statement arena
//!   and with `Config::max_vtab_rows` far below the row count, with
//!   the heap this thread holds bounded (a counting allocator).
//! - **Blocking operators** (ORDER BY without an index, GROUP BY,
//!   DISTINCT) still buffer; `max_vtab_rows` bounds what they buffer
//!   from a generate_series cursor, and an ORDER BY with a LIMIT
//!   keeps only the rows it may return.
//! - A generated corpus of joins mixing tables and modules, with
//!   error injection, LIMIT and OFFSET.
//! - Transactions: a statement reset or dropped part-way, or failing
//!   part-way, leaves the transaction and the file as the pin does.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, MemStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

// ---------------- a per-thread counting allocator ----------------

/// The system allocator, counting the bytes each thread holds (and
/// its peak) so a test can bound what one statement allocates.
struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = LIVE.try_with(|l| {
        let v = l.get() + delta;
        l.set(v);
        let _ = PEAK.try_with(|p| {
            if v > p.get() {
                p.set(v);
            }
        });
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        note(-(layout.size() as isize));
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new) };
        if !p.is_null() {
            note(new as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The bytes this thread holds now, and the peak since `reset_peak`.
fn live() -> isize {
    LIVE.with(|l| l.get())
}
fn reset_peak() {
    let v = live();
    PEAK.with(|p| p.set(v));
}
fn peak() -> isize {
    PEAK.with(|p| p.get())
}

// ---------------- harness ----------------

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// One row as the pin's shell prints it in list mode.
fn render<S: hipp::Storage>(q: &hipp::Statement<'_, S>, line: &mut String) {
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

/// A script on one hipp connection, as the CLI runs it: every row
/// (each as soon as `step` returns it), stopping at the first error.
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

/// A script on fresh twin files: the same rows (those before an
/// error included), the same success; hipp's error when both failed.
fn same(tag: &str, script: &str) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v17st-{tag}-h"));
    let o = common::temp_db(&format!("v17st-{tag}-o"));
    same_on(&h, &o, script)
}

fn same_on(h: &std::path::Path, o: &std::path::Path, script: &str) -> Option<Error<FsError>> {
    let (hok, hl, he) = hipp_script(h, script);
    let (ook, ol) = oracle_script(o, script);
    assert_eq!(hl, ol, "script: {script}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {script}\nhipp error: {he:?}");
    he
}

/// `same`, and the script must fail part-way with rows before it.
fn fails_after_rows(tag: &str, script: &str) {
    let probe = common::temp_db(&format!("v17st-{tag}-p"));
    let (ook, ol) = oracle_script(&probe, script);
    assert!(!ook, "the pin must fail: {script}");
    assert!(!ol.is_empty(), "the pin must print rows first: {script}");
    same(tag, script);
}

/// A file the pin builds with `setup`; `script` then runs on two
/// copies of it, one per engine.
fn on_pin_file(tag: &str, setup: &str, script: &str) -> Option<Error<FsError>> {
    let base = common::temp_db(&format!("v17st-{tag}-base"));
    let (ok, _) = oracle_script(&base, setup);
    assert!(ok, "setup failed: {setup}");
    let h = common::temp_db(&format!("v17st-{tag}-h"));
    let o = common::temp_db(&format!("v17st-{tag}-o"));
    std::fs::copy(&base, &h).expect("copy");
    std::fs::copy(&base, &o).expect("copy");
    same_on(&h, &o, script)
}

/// The rows of `sql` on a fresh in-memory connection with `cfg`, or
/// its error.
fn mem_rows(
    cfg: Config,
    setup: &str,
    sql: &str,
) -> Result<Vec<String>, Error<hipp::storage::MemError>> {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        for s in split(setup) {
            let mut q = db.prepare(&s).await.expect("prep");
            while q.step().await.expect("step") == Step::Row {}
        }
        let mut q = db.prepare(sql).await?;
        let mut out = Vec::new();
        while q.step().await? == Step::Row {
            let mut l = String::new();
            render(&q, &mut l);
            out.push(l);
        }
        Ok(out)
    })
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
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// A row-level runtime error the pin raises: `json()` of malformed
/// text when `cond` holds.
fn bad(cond: &str) -> String {
    format!("json(CASE WHEN {cond} THEN 'x' ELSE 1 END)")
}

const T: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT); \
    INSERT INTO t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, 30, 'z'), (4, 40, 'w'), (5, NULL, 'v'); \
    CREATE TABLE u(k, v); \
    INSERT INTO u VALUES (1, 'p'), (2, 'q'), (3, 'r');";

// ---------------- rows before an error: plain scans ----------------

#[test]
fn plain_scan_returns_rows_before_the_failing_one() {
    for (tag, q) in [
        ("item", format!("SELECT a, {} FROM t;", bad("a = 3"))),
        ("where", format!("SELECT a FROM t WHERE {};", bad("a = 4"))),
        (
            "abs",
            "SELECT a, abs(CASE WHEN a = 3 THEN -9223372036854775807 - 1 ELSE a END) FROM t;"
                .to_string(),
        ),
        (
            "expr",
            format!(
                "SELECT a * 2, c || 'z', {} FROM t WHERE b IS NOT NULL;",
                bad("b = 40")
            ),
        ),
        (
            "rowid-order",
            format!("SELECT a, {} FROM t ORDER BY a;", bad("a = 3")),
        ),
        (
            "rowid-order2",
            format!("SELECT a, {} FROM t ORDER BY rowid;", bad("a = 2")),
        ),
        // An index / rowid loop that delivers the ORDER BY: no sort.
        (
            "rowid-desc",
            format!("SELECT a, {} FROM t ORDER BY a DESC;", bad("a = 2")),
        ),
        (
            "distinct",
            format!("SELECT DISTINCT b % 20, {} FROM t;", bad("a = 4")),
        ),
        (
            "like-escape",
            "SELECT a, c LIKE 'x' ESCAPE CASE WHEN a = 3 THEN 'ab' ELSE '\\' END FROM t;"
                .to_string(),
        ),
    ] {
        fails_after_rows(&format!("scan-{tag}"), &format!("{T} {q}"));
    }
}

#[test]
fn blocking_operators_fail_before_any_row() {
    // The pin sorts or groups first: no row comes before the error.
    for (tag, q) in [
        (
            "order",
            format!("SELECT a, {} FROM t ORDER BY c;", bad("a = 3")),
        ),
        (
            "group",
            format!(
                "SELECT b, count(*) FROM t GROUP BY b HAVING {};",
                bad("b = 30")
            ),
        ),
        ("agg", format!("SELECT count({}) FROM t;", bad("a = 3"))),
    ] {
        same(&format!("block-{tag}"), &format!("{T} {q}"));
    }
}

#[test]
fn derived_tables_and_views_stream_their_outer_query() {
    for (tag, q) in [
        (
            "sub",
            format!("SELECT a FROM (SELECT a FROM t) WHERE {};", bad("a = 3")),
        ),
        (
            "cte",
            format!(
                "WITH c AS (SELECT a, b FROM t) SELECT a, {} FROM c;",
                bad("a = 4")
            ),
        ),
        (
            "view",
            format!(
                "CREATE VIEW vw AS SELECT a, c FROM t WHERE a > 1; SELECT c, {} FROM vw;",
                bad("a = 4")
            ),
        ),
    ] {
        fails_after_rows(&format!("derived-{tag}"), &format!("{T} {q}"));
    }
}

// ---------------- joins ----------------

#[test]
fn joins_return_rows_before_the_failing_combination() {
    for (tag, q) in [
        ("inner", format!("SELECT t.a, u.k FROM t CROSS JOIN u WHERE {};", bad("t.a = 2 AND u.k = 3"))),
        ("on", format!("SELECT t.a, u.v FROM t CROSS JOIN u ON u.k <= t.a AND {};", bad("t.a = 3 AND u.k = 2"))),
        ("left", format!("SELECT t.a, u.v FROM t LEFT JOIN u ON u.k = t.a + 10 WHERE {};", bad("t.a = 4"))),
        ("left-items", format!("SELECT t.a, u.v, {} FROM t LEFT JOIN u ON u.k = t.a;", bad("t.a = 5"))),
        ("three", format!("SELECT t.a, u.k, x.k FROM t CROSS JOIN u CROSS JOIN u AS x WHERE {};", bad("t.a = 2 AND x.k = 2"))),
        (
            "series-inner",
            format!("SELECT t.a, s.value FROM t CROSS JOIN generate_series(1, 3) s WHERE {};", bad("t.a = 3 AND s.value = 2")),
        ),
        (
            "series-outer",
            format!("SELECT s.value, t.a FROM generate_series(1, 4) s CROSS JOIN t WHERE {};", bad("s.value = 2 AND t.a = 4")),
        ),
        (
            "series-lateral",
            format!("SELECT t.a, s.value FROM t, generate_series(1, t.a) s WHERE {};", bad("t.a = 4 AND s.value = 3")),
        ),
        (
            "json-each",
            "SELECT t.a, j.value FROM t, json_each(CASE WHEN t.a = 3 THEN '[1,' ELSE '[7,8]' END) j;".to_string(),
        ),
        (
            "json-tree-items",
            format!("SELECT t.a, j.key, {} FROM t, json_tree('{{\"a\":[1,2]}}') j;", bad("t.a = 2 AND j.key = 1")),
        ),
    ] {
        fails_after_rows(&format!("join-{tag}"), &format!("{T} {q}"));
    }
}

#[test]
fn inner_virtual_tables_filter_again_per_outer_row() {
    // The inner generate_series takes the outer row's values (a
    // usable `value = t.a` constraint and its arguments), row by row.
    for (tag, q) in [
        (
            "args",
            "SELECT t.a, s.value FROM t, generate_series(t.a, t.a + 2) s;",
        ),
        (
            "eq",
            "SELECT t.a, s.value FROM t, generate_series(1, 10) s WHERE s.value = t.a * 2;",
        ),
        (
            "range",
            "SELECT t.a, s.value FROM t, generate_series(1, 10) s WHERE s.value > t.b / 10 AND s.value < 6;",
        ),
        (
            "left",
            "SELECT t.a, s.value FROM t LEFT JOIN generate_series(1, 3) s ON s.value = t.a;",
        ),
        (
            "left-args",
            "SELECT t.a, s.value FROM t LEFT JOIN generate_series(t.a, 3) s;",
        ),
        (
            "nested",
            "SELECT x.value, y.value FROM generate_series(1, 4) x, generate_series(x.value, 4) y;",
        ),
        (
            "limit",
            "SELECT t.a, s.value FROM t, generate_series(1, t.a) s LIMIT 7;",
        ),
        (
            "offset",
            "SELECT t.a, s.value FROM t, generate_series(1, t.a) s LIMIT 4 OFFSET 5;",
        ),
        (
            "agg",
            "SELECT count(*), sum(s.value), max(t.a) FROM t, generate_series(1, t.a) s;",
        ),
        (
            "json",
            "SELECT t.a, j.value FROM t, json_each(json_array(t.a, t.b)) j;",
        ),
    ] {
        same(&format!("inner-{tag}"), &format!("{T} {q}"));
    }
}

// ---------------- LIMIT / OFFSET ----------------

#[test]
fn limit_stops_the_join_early() {
    let start = std::time::Instant::now();
    for (tag, q) in [
        ("series", "SELECT value FROM generate_series(1) LIMIT 3;"),
        (
            "series-where",
            "SELECT value FROM generate_series(1) WHERE value % 7 = 0 LIMIT 3;",
        ),
        (
            "series-offset",
            "SELECT value FROM generate_series(1) LIMIT 2 OFFSET 1000;",
        ),
        (
            "series-join",
            "SELECT a.value, b.value FROM generate_series(1) a, generate_series(1, 2) b LIMIT 5;",
        ),
        (
            "series-distinct",
            "SELECT DISTINCT value / 10 FROM generate_series(1) LIMIT 4;",
        ),
        (
            "series-expr",
            "SELECT value * value FROM generate_series(1) WHERE value > 3 LIMIT 2;",
        ),
        (
            "series-left",
            "SELECT a.value, b.value FROM generate_series(1) a LEFT JOIN generate_series(5, 6) b ON b.value = a.value LIMIT 7;",
        ),
    ] {
        same(&format!("limit-{tag}"), q);
    }
    // Each would read 4294967295 rows without the early stop.
    assert!(
        start.elapsed().as_secs() < 60,
        "LIMIT did not stop the join"
    );
}

#[test]
fn limit_returns_the_first_rows_at_once() {
    // The first `step` of an endless series returns its first row.
    let rows = mem_rows(
        Config::default(),
        "",
        "SELECT value FROM generate_series(1) LIMIT 3",
    )
    .expect("rows");
    assert_eq!(rows, ["1", "2", "3"]);
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut q = db
            .prepare("SELECT value FROM generate_series(1)")
            .await
            .expect("prep");
        for want in 1..=5 {
            assert_eq!(q.step().await.expect("step"), Step::Row);
            assert_eq!(q.column_i64(0), Some(want));
        }
        // Dropped part-way: nothing more is read.
    });
}

#[test]
fn offset_rows_are_not_projected() {
    for (tag, q) in [
        (
            "scan",
            format!("SELECT {} FROM t LIMIT 2 OFFSET 2;", bad("a <= 2")),
        ),
        (
            "series",
            format!(
                "SELECT {} FROM generate_series(1, 9) LIMIT 1 OFFSET 3;",
                bad("value < 4")
            ),
        ),
        (
            "join",
            format!(
                "SELECT {} FROM t CROSS JOIN u LIMIT 3 OFFSET 4;",
                bad("t.a = 1")
            ),
        ),
        ("limit-zero", format!("SELECT {} FROM t LIMIT 0;", bad("1"))),
        (
            "limit-zero-agg",
            format!("SELECT count({}) FROM t LIMIT 0;", bad("1")),
        ),
        (
            "limit-zero-order",
            format!("SELECT {} FROM t ORDER BY 1 LIMIT 0;", bad("1")),
        ),
        (
            "after-limit",
            format!("SELECT a, {} FROM t LIMIT 2;", bad("a = 3")),
        ),
        (
            "where-in-offset",
            format!("SELECT a FROM t WHERE {} LIMIT 1 OFFSET 1;", bad("a = 3")),
        ),
    ] {
        same(&format!("offset-{tag}"), &format!("{T} {q}"));
    }
}

// ---------------- each module ----------------

#[test]
fn generate_series_streams_rows_before_an_error() {
    for (tag, q) in [
        (
            "items",
            format!(
                "SELECT value, {} FROM generate_series(1, 10);",
                bad("value = 4")
            ),
        ),
        (
            "where",
            format!(
                "SELECT value FROM generate_series(10, 1, -1) WHERE {};",
                bad("value = 6")
            ),
        ),
        (
            "step",
            format!(
                "SELECT value FROM generate_series(0, 100, 7) WHERE value > 20 AND {};",
                bad("value = 49")
            ),
        ),
        (
            "hidden",
            format!(
                "SELECT value, start, stop, step, {} FROM generate_series(5, 9);",
                bad("value = 8")
            ),
        ),
    ] {
        fails_after_rows(&format!("series-{tag}"), &q);
    }
}

#[test]
fn dbstat_streams_rows_before_an_error() {
    let setup = "PRAGMA page_size = 1024; CREATE TABLE big(x); \
        INSERT INTO big SELECT randomblob(300) FROM generate_series(1, 40); \
        CREATE INDEX bi ON big(x);";
    let q = format!(
        "SELECT name, pageno, pagetype, {} FROM dbstat;",
        bad("pageno = 6")
    );
    let e = on_pin_file("dbstat", setup, &q);
    assert!(e.is_some());
    let q = format!(
        "SELECT name, {} FROM dbstat WHERE aggregate = 1;",
        bad("name = 'bi'")
    );
    on_pin_file("dbstat-agg", setup, &q);
}

#[test]
fn rtree_streams_rows_before_an_error() {
    let setup = "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1, +tag); \
        INSERT INTO r SELECT value, value, value + 1, -value, -value + 2, 'g' || value \
        FROM generate_series(1, 200);";
    for (tag, q) in [
        (
            "scan",
            format!("SELECT id, tag, {} FROM r;", bad("id = 57")),
        ),
        (
            "range",
            format!(
                "SELECT id, x0 FROM r WHERE x0 > 20 AND x1 < 120 AND {};",
                bad("id = 90")
            ),
        ),
        (
            "rowid",
            format!("SELECT id, {} FROM r WHERE id = 7;", bad("id = 7")),
        ),
        (
            "join",
            format!(
                "SELECT g.value, r.id FROM generate_series(1, 5) g, r WHERE r.id = g.value * 3 AND {};",
                bad("g.value = 4")
            ),
        ),
        (
            "limit",
            "SELECT id, y0 FROM r WHERE y1 < -50 LIMIT 4;".to_string(),
        ),
    ] {
        on_pin_file(&format!("rtree-{tag}"), setup, &q);
    }
}

#[test]
fn fts5_streams_rows_before_an_error() {
    let setup = "CREATE VIRTUAL TABLE f USING fts5(body, tag); \
        INSERT INTO f(rowid, body, tag) SELECT value, 'alpha beta w' || (value % 7), 'k' || value \
        FROM generate_series(1, 60); \
        CREATE VIRTUAL TABLE fv USING fts5vocab(f, 'row');";
    for (tag, q) in [
        (
            "scan",
            format!("SELECT rowid, tag, {} FROM f;", bad("rowid = 9")),
        ),
        (
            "match",
            format!(
                "SELECT rowid, body FROM f WHERE f MATCH 'w3' AND {};",
                bad("rowid = 31")
            ),
        ),
        (
            "rank",
            format!(
                "SELECT rowid, {} FROM f WHERE f MATCH 'alpha' ORDER BY rank;",
                bad("rowid = 5")
            ),
        ),
        (
            "highlight",
            format!(
                "SELECT highlight(f, 0, '[', ']'), {} FROM f WHERE f MATCH 'w5';",
                bad("rowid = 26")
            ),
        ),
        (
            "vocab",
            format!("SELECT term, doc, {} FROM fv;", bad("term = 'w2'")),
        ),
        (
            "join",
            format!(
                "SELECT g.value, f.tag FROM generate_series(1, 4) g, f WHERE f MATCH ('w' || g.value) AND {};",
                bad("g.value = 3")
            ),
        ),
    ] {
        on_pin_file(&format!("fts5-{tag}"), setup, &q);
    }
}

// ---------------- constant memory ----------------

#[test]
fn streamed_aggregates_run_in_constant_memory() {
    const N: i64 = 50_000_000;
    let path = common::temp_db("v17st-const-o");
    let pin = common::cli(
        &path,
        &format!(
            "SELECT count(*), sum(value), min(value), max(value), total(value) FROM generate_series(1, {N});"
        ),
    );
    // A small statement arena, and a cursor bound far below the row
    // count: neither may be what the statement grows into.
    let cfg = Config::default()
        .statement_arena(64 << 10, 64 << 10)
        .max_vtab_rows(1000);
    let before = live();
    reset_peak();
    let rows = mem_rows(
        cfg,
        "",
        &format!("SELECT count(*), sum(value), min(value), max(value), total(value) FROM generate_series(1, {N})"),
    )
    .expect("streamed aggregate");
    let grew = peak() - before;
    assert_eq!(format!("{}\n", rows.join("\n")), pin);
    assert!(
        grew < 8 << 20,
        "the statement held {grew} bytes at its peak"
    );
}

#[test]
fn streamed_rows_do_not_accumulate() {
    // A plain streamed scan of a long series: the heap stays flat
    // between the first rows and the millionth.
    block_on(async {
        let cfg = Config::default().max_vtab_rows(1000);
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        let mut q = db
            .prepare("SELECT value, value * 2 FROM generate_series(1) WHERE value % 3 = 0")
            .await
            .expect("prep");
        for _ in 0..1000 {
            assert_eq!(q.step().await.expect("step"), Step::Row);
        }
        let base = live();
        reset_peak();
        for _ in 0..300_000 {
            assert_eq!(q.step().await.expect("step"), Step::Row);
        }
        assert_eq!(q.column_i64(0), Some(3 * 301_000));
        let grew = peak() - base;
        assert!(grew < 1 << 20, "streaming grew the heap by {grew} bytes");
    });
}

#[test]
fn blocking_operators_bound_what_they_buffer() {
    let cfg = || Config::default().max_vtab_rows(1000);
    // DISTINCT / GROUP BY / ORDER BY without LIMIT keep every row.
    for q in [
        "SELECT DISTINCT value FROM generate_series(1, 5000)",
        "SELECT value % 3, count(*) FROM generate_series(1, 5000) GROUP BY 1",
        "SELECT value FROM generate_series(1, 5000) ORDER BY value DESC",
    ] {
        match mem_rows(cfg(), "", q) {
            Err(Error::Schema {
                kind: SchemaKind::VtabTooManyRows,
            }) => {}
            other => panic!("{q}: {other:?}"),
        }
    }
    // Within the bound they answer as on the pin.
    let path = common::temp_db("v17st-bound-o");
    for q in [
        "SELECT DISTINCT value % 7 FROM generate_series(1, 900)",
        "SELECT value % 3, count(*) FROM generate_series(1, 900) GROUP BY 1",
        "SELECT value FROM generate_series(1, 900) ORDER BY value % 10, value DESC LIMIT 5",
    ] {
        let rows = mem_rows(cfg(), "", q).expect(q);
        assert_eq!(
            format!("{}\n", rows.join("\n")),
            common::cli(&path, &format!("{q};")),
            "{q}"
        );
    }
    // An ORDER BY with a LIMIT keeps only the rows it may return: far
    // more rows than the bound sort as on the pin.
    let q = "SELECT value FROM generate_series(1, 200000) ORDER BY value % 1000 DESC, value LIMIT 4 OFFSET 2";
    let rows = mem_rows(cfg(), "", q).expect("top-n");
    assert_eq!(
        format!("{}\n", rows.join("\n")),
        common::cli(&path, &format!("{q};"))
    );
    // A plain streamed query is not bounded at all.
    let rows = mem_rows(
        cfg(),
        "",
        "SELECT count(*) FROM generate_series(1, 200000) WHERE value % 2 = 0",
    )
    .expect("count");
    assert_eq!(rows, ["100000"]);
}

// ---------------- transactions, reset, re-execution ----------------

#[test]
fn a_statement_failing_part_way_keeps_the_transaction() {
    let script = format!(
        "{T} BEGIN; INSERT INTO u VALUES (4, 's'); \
         SELECT k, {} FROM u; ",
        bad("k = 3")
    );
    // Both fail on the SELECT after two rows; the transaction (and
    // its insert) is still open: a new connection's view is the
    // committed state, and the file checks clean on the pin.
    let h = common::temp_db("v17st-txn-h");
    let o = common::temp_db("v17st-txn-o");
    same_on(&h, &o, &script);
    let rows = common::cli(&h, "PRAGMA integrity_check; SELECT count(*) FROM u;");
    assert_eq!(
        rows,
        common::cli(&o, "PRAGMA integrity_check; SELECT count(*) FROM u;")
    );
    // On one connection: rows, the error, then the transaction rolls
    // back and commits as before.
    block_on(async {
        let mut db = open(&h).await;
        for s in ["BEGIN", "INSERT INTO u VALUES (9, 'z')"] {
            let mut q = db.prepare(s).await.expect("prep");
            q.step().await.expect("step");
        }
        {
            let mut q = db
                .prepare(&format!("SELECT k FROM u WHERE {}", bad("k = 9")))
                .await
                .expect("prep");
            let mut n = 0;
            let err = loop {
                match q.step().await {
                    Ok(Step::Row) => n += 1,
                    Ok(Step::Done) => panic!("no error"),
                    Err(e) => break e,
                }
            };
            assert_eq!(n, 3);
            assert!(
                matches!(
                    err,
                    Error::Schema {
                        kind: SchemaKind::JsonMalformed
                    }
                ),
                "{err:?}"
            );
        }
        let mut q = db.prepare("ROLLBACK").await.expect("prep");
        q.step().await.expect("rollback");
    });
    assert_eq!(common::cli(&h, "SELECT count(*) FROM u;"), "3\n");
    assert_eq!(common::cli(&h, "PRAGMA integrity_check;"), "ok\n");
}

#[test]
fn reset_and_drop_part_way() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        for s in split(T) {
            let mut q = db.prepare(&s).await.expect("prep");
            q.step().await.expect("step");
        }
        // Reset part-way: the next execution starts over.
        {
            let mut q = db
                .prepare("SELECT t.a, j.value FROM t, json_each(json_array(t.a, t.c)) j")
                .await
                .expect("prep");
            for _ in 0..3 {
                assert_eq!(q.step().await.expect("step"), Step::Row);
            }
            q.reset();
            let mut n = 0;
            while q.step().await.expect("step") == Step::Row {
                n += 1;
            }
            assert_eq!(n, 10);
            // Stepped again after an error, a statement starts over.
            q.reset();
        }
        // Dropped part-way (a JSON table-valued function's documents
        // held), then other statements: unaffected.
        {
            let mut q = db
                .prepare("SELECT j.value FROM t, json_each(json_array(t.c, t.c)) j")
                .await
                .expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            assert_eq!(q.column_bytes(0), Some(&b"x"[..]));
        }
        let mut q = db
            .prepare("SELECT group_concat(j.value) FROM u, json_each(json_array(u.v)) j")
            .await
            .expect("prep");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        assert_eq!(q.column_bytes(0), Some(&b"p,q,r"[..]));
        drop(q);
        // An error part-way, stepped again: it runs from the start.
        let mut q = db
            .prepare(&format!("SELECT a FROM t WHERE {}", bad("a = 3")))
            .await
            .expect("prep");
        for round in 0..2 {
            let mut n = 0;
            let e = loop {
                match q.step().await {
                    Ok(Step::Row) => n += 1,
                    Ok(Step::Done) => panic!("no error"),
                    Err(e) => break e,
                }
            };
            assert_eq!(n, 2, "round {round}");
            assert!(matches!(e, Error::Schema { .. }));
        }
    });
}

// ---------------- a generated corpus ----------------

#[test]
fn generated_joins_match_the_pin() {
    let setup = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT); \
        INSERT INTO t SELECT value, value % 4, char(96 + value % 5) FROM generate_series(1, 9); \
        CREATE TABLE u(k, v); INSERT INTO u SELECT value % 5, value * 3 FROM generate_series(1, 7); \
        CREATE VIRTUAL TABLE r USING rtree(id, x0, x1); \
        INSERT INTO r SELECT value, value, value + 2 FROM generate_series(1, 12); \
        CREATE VIRTUAL TABLE f USING fts5(body); \
        INSERT INTO f(rowid, body) SELECT value, 'w' || (value % 3) || ' z' FROM generate_series(1, 8);";
    let sources = [
        ("t", &["t.a", "t.b", "t.c"][..]),
        ("u", &["u.k", "u.v"][..]),
        ("generate_series(1, 5) g", &["g.value"][..]),
        ("r", &["r.id", "r.x0"][..]),
        ("f", &["f.rowid", "f.body"][..]),
        ("json_each('[1,2,3]') j", &["j.value", "j.key"][..]),
    ];
    let mut rng = Rng(0x5eed_1717);
    let mut script = String::new();
    let mut n = 0;
    for _ in 0..160 {
        let k = 1 + rng.below(3) as usize;
        let mut from = Vec::new();
        let mut cols = Vec::new();
        let mut used = Vec::new();
        for _ in 0..k {
            let mut i = rng.below(sources.len() as u64) as usize;
            while used.contains(&i) {
                i = (i + 1) % sources.len();
            }
            used.push(i);
            from.push(sources[i].0);
            cols.extend_from_slice(sources[i].1);
        }
        let col = |rng: &mut Rng| cols[rng.below(cols.len() as u64) as usize];
        let mut sql = format!("SELECT {}, {}", col(&mut rng), col(&mut rng));
        let fail = rng.below(3) == 0;
        if fail {
            let c = col(&mut rng);
            let v = rng.below(5);
            sql.push_str(&format!(", {}", bad(&format!("{c} = {v}"))));
        }
        // CROSS JOIN: the pin keeps the FROM order (its planner may
        // reorder a plain join, which hipp does not model).
        let filtered = rng.below(2) == 0;
        // A LEFT JOIN only without a WHERE (the pin makes one whose
        // WHERE rejects the NULL row an inner join it may reorder).
        let join = if k > 1 && !filtered && rng.below(3) == 0 {
            // A LEFT JOIN of the last source.
            let last = from.pop().unwrap_or("u");
            let on = format!("{} = {}", col(&mut rng), col(&mut rng));
            format!("{} LEFT JOIN {last} ON {on}", from.join(" CROSS JOIN "))
        } else {
            from.join(" CROSS JOIN ")
        };
        sql.push_str(&format!(" FROM {join}"));
        if filtered {
            let w = rng.pick(&["<", ">", "=", "<>", ">="]);
            sql.push_str(&format!(" WHERE {} {w} {}", col(&mut rng), rng.below(6)));
        }
        if rng.below(3) == 0 {
            sql.push_str(&format!(" LIMIT {}", rng.below(8)));
            if rng.below(2) == 0 {
                sql.push_str(&format!(" OFFSET {}", rng.below(5)));
            }
        }
        sql.push(';');
        // Each query on the same file, one by one (the first error
        // ends a script).
        let tag = format!("gen-{n}");
        on_pin_file(&tag, setup, &sql);
        n += 1;
        script.push_str(&sql);
    }
    assert!(n > 100);
}
