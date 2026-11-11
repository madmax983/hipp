//! The bytecode VM (ADR 0005 Stage 2, hidden feature `vm`),
//! differentially vs the pinned oracle (SQLite 3.54.1). Built only
//! with `--features vm`; every SELECT a section claims runs under
//! `Config::vm_strict`, where falling back to today's executor is an
//! error, and asserts `Statement::runs_on_vm`.
//!
//! Unit `vm_core` (this section): a SELECT over one rowid table or
//! none.
//! - **Shapes.** WHERE (constant terms first), result columns of any
//!   subquery-free expression, ORDER BY through the sorter (ties keep
//!   their order, NULLS FIRST / LAST, collations), LIMIT / OFFSET as
//!   expressions (their conversions and errors), full scans,
//!   rowid equality and rowid ranges (ascending and descending), the
//!   legacy scan plan, parameters (each execution compiles its own
//!   plan), defaults past a record's end, generated columns,
//!   overflow rows, an empty table and a table of several B-tree
//!   levels. Rows before a failing row are returned first, as on the
//!   pin; three no-FROM cases where today's executor errors and the
//!   pin does not (OFFSET skipping an erroring row, an erroring
//!   ORDER BY term, LIMIT 0) match the pin on the VM.
//! - **Fallback.** Every shape no unit claims yet fails under
//!   `vm_strict` with `Error::VmFallback`, and without it runs on
//!   today's executor (`runs_on_vm` false) with the pin's rows.
//! - **Restart.** The corpus again with the smallest page cache the
//!   engine allows (one frame) and with two, so nearly every opcode
//!   misses, is re-run after the read, and must give the same rows:
//!   a miss copies the pages the opcode read, so a descent of any
//!   height completes in one frame, as today's executor does (no
//!   frame at all fails on both alike).
//! - **Cancellation.** A storage whose reads return `Pending` first:
//!   step futures dropped after one poll, again and again, then the
//!   next step (and a reset part-way) give exactly the
//!   uninterrupted rows.
#![cfg(feature = "vm")]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use hipp::storage::{MemStorage, Sidecar, Storage, block_on};
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as Storage>::Error;

// ---------------- harness ----------------

/// One row as the pin's shell prints it in list mode.
fn render<S: Storage>(q: &hipp::Statement<'_, S>) -> String {
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

fn is_select(s: &str) -> bool {
    s.trim_start()
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("select"))
}

/// The configuration the claimed shapes run under.
fn strict() -> Config {
    Config::default().vm_strict(true)
}

/// A script on one hipp connection: every row (as soon as `step`
/// returns it), stopping at the first error. Every SELECT must run
/// on the VM.
async fn run_script<S: Storage>(
    db: &mut Connection<S>,
    script: &str,
) -> (bool, Vec<String>, Option<Error<S::Error>>)
where
    S::Error: core::fmt::Debug,
{
    let mut lines = Vec::new();
    for stmt in split(script) {
        let mut q = match db.prepare(&stmt).await {
            Ok(q) => q,
            Err(e) => return (false, lines, Some(e)),
        };
        let mut first = true;
        loop {
            let r = q.step().await;
            if first && is_select(&stmt) {
                assert!(q.runs_on_vm(), "not on the VM: {stmt}");
            }
            first = false;
            match r {
                Ok(Step::Row) => lines.push(render(&q)),
                Ok(Step::Done) => break,
                Err(e) => return (false, lines, Some(e)),
            }
        }
    }
    (true, lines, None)
}

fn hipp_file(
    path: &std::path::Path,
    cfg: Config,
    script: &str,
) -> (bool, Vec<String>, Option<Error<FsError>>) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        run_script(&mut db, script).await
    })
}

/// The script on the pin's shell (`-bail`: it stops at the first
/// error, as `run_script` does).
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

/// A file the pin builds with `setup`, copied once per engine.
fn pin_file(tag: &str, setup: &str) -> std::path::PathBuf {
    let base = common::temp_db(&format!("vm17-{tag}-base"));
    let (ok, _) = oracle_script(&base, setup);
    assert!(ok, "setup failed: {setup}");
    base
}

fn copy_of(base: &std::path::Path, tag: &str) -> std::path::PathBuf {
    let p = common::temp_db(tag);
    std::fs::copy(base, &p).expect("copy");
    p
}

/// `query` (a script of SELECTs) on copies of the pin-built `base`:
/// hipp under `cfg` gives the pin's rows (those before an error
/// included) and its success.
fn same_on(base: &std::path::Path, tag: &str, cfg: Config, query: &str) -> Option<Error<FsError>> {
    let h = copy_of(base, &format!("vm17-{tag}-h"));
    let o = copy_of(base, &format!("vm17-{tag}-o"));
    let (hok, hl, he) = hipp_file(&h, cfg, query);
    let (ook, ol) = oracle_script(&o, query);
    assert_eq!(hl, ol, "query: {query}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {query}\nhipp error: {he:?}");
    he
}

/// Each query of `queries` on its own, strict, on copies of `base`.
fn corpus(base: &std::path::Path, tag: &str, cfg: &Config, queries: &[&str]) {
    for (i, q) in queries.iter().enumerate() {
        same_on(base, &format!("{tag}{i}"), cfg.clone(), q);
    }
}

// ---------------- the corpus ----------------

const T: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c TEXT COLLATE NOCASE, d REAL, e BLOB, f NUMERIC); \
    INSERT INTO t VALUES (1, 10, 'b', 1, x'4142', 5), (2, -9223372036854775808, 'A', 2.5, NULL, '5'), \
      (3, 7, 'a', NULL, 'x', 5.0), (4, NULL, 'B', 3, 4, 'abc'), (5, '10', 'c', 2.5, x'', NULL), \
      (6, 10.0, 'C', -1, 1.5, 5), (7, 'text', NULL, 1e20, NULL, 2), (9, 0, '', 0, 0, 0); \
    CREATE TABLE e(x, y); \
    CREATE TABLE g(x INTEGER, y AS (x * 2), z TEXT AS (x || '!') STORED); \
    INSERT INTO g(x) VALUES (1), (NULL), (3); \
    CREATE TABLE al(p INTEGER PRIMARY KEY, q); INSERT INTO al VALUES (1, 'one'), (2, 'two'); \
    ALTER TABLE al ADD COLUMN r DEFAULT 'dflt'; ALTER TABLE al ADD COLUMN s REAL DEFAULT 3; \
    INSERT INTO al VALUES (3, 'three', 'set', 4);";

/// SELECTs over `T` that run without error (a row error would end
/// the script; those are in `ERRORS`).
const OK: &[&str] = &[
    "SELECT * FROM t;",
    "SELECT a, b, c FROM t;",
    "SELECT rowid, * FROM t;",
    "SELECT oid, _rowid_ FROM t;",
    "SELECT d, typeof(d), f, typeof(f) FROM t;",
    "SELECT a FROM t WHERE b = 10;",
    "SELECT a FROM t WHERE b = '10';",
    "SELECT a FROM t WHERE f = 5;",
    "SELECT a FROM t WHERE f = '5';",
    "SELECT a FROM t WHERE c = 'b';",
    "SELECT a FROM t WHERE c = 'B' COLLATE BINARY;",
    "SELECT a FROM t WHERE d > 1;",
    "SELECT a FROM t WHERE d IS NULL;",
    "SELECT a FROM t WHERE b IS NOT NULL AND c IS NOT NULL;",
    "SELECT a FROM t WHERE b IN (10, 7, '10');",
    "SELECT a FROM t WHERE b NOT IN (10, NULL);",
    "SELECT a FROM t WHERE c LIKE 'a%' OR c GLOB 'C*';",
    "SELECT a FROM t WHERE b BETWEEN 0 AND 10;",
    "SELECT a FROM t WHERE e = x'4142';",
    "SELECT a FROM t WHERE NULL;",
    "SELECT a FROM t WHERE 1;",
    "SELECT a FROM t WHERE 0 AND b;",
    "SELECT a FROM t WHERE a = 1 AND 1 = 1;",
    "SELECT a FROM t WHERE 'x';",
    "SELECT a + 1, a * 2.5, c || '!', a % 3, a / 2, -a, +c FROM t;",
    "SELECT CASE WHEN a > 3 THEN 'big' WHEN a > 1 THEN 'mid' END, CASE c WHEN 'a' THEN 1 ELSE 0 END FROM t;",
    "SELECT coalesce(d, 'none'), ifnull(c, 'nil'), nullif(a, 3), iif(a > 4, 'y', 'n') FROM t;",
    "SELECT length(c), upper(c), lower(c), substr(c, 1, 1), hex(e), quote(e) FROM t;",
    "SELECT CAST(b AS TEXT), CAST(d AS INTEGER), CAST(f AS REAL), CAST(c AS BLOB) FROM t WHERE a <> 2;",
    "SELECT round(d, 1), abs(d), max(a, 3), min(a, 3), typeof(a / 0) FROM t;",
    "SELECT a < b, a = f, c = 'A', c < 'b', e > 0, d >= 2.5 FROM t;",
    "SELECT a IS b, a IS NOT NULL, b IS NULL, a IN (1, 3, 5) FROM t;",
    "SELECT 1, 'lit', NULL, 2.5, x'42' FROM t;",
    "SELECT a FROM t ORDER BY b;",
    "SELECT a, b FROM t ORDER BY b DESC;",
    "SELECT a, c FROM t ORDER BY c;",
    "SELECT a, c FROM t ORDER BY c DESC;",
    "SELECT a, c FROM t ORDER BY c COLLATE BINARY;",
    "SELECT a, b FROM t ORDER BY b NULLS LAST;",
    "SELECT a, b FROM t ORDER BY b DESC NULLS FIRST;",
    "SELECT a, f FROM t ORDER BY f;",
    "SELECT a, f FROM t ORDER BY f DESC;",
    "SELECT a, f, b FROM t ORDER BY f, b DESC;",
    "SELECT a, d FROM t ORDER BY 2;",
    "SELECT a, d FROM t ORDER BY 2 DESC, 1 DESC;",
    "SELECT a FROM t ORDER BY a;",
    "SELECT a FROM t ORDER BY rowid;",
    "SELECT a FROM t ORDER BY a DESC;",
    "SELECT a FROM t ORDER BY lower(c), a DESC;",
    "SELECT a * 2 AS x FROM t ORDER BY x DESC;",
    "SELECT a, c FROM t ORDER BY c COLLATE NOCASE, e;",
    "SELECT a, typeof(b) FROM t ORDER BY typeof(b), a;",
    "SELECT a FROM t ORDER BY 'const';",
    "SELECT a FROM t LIMIT 2;",
    "SELECT a FROM t LIMIT 2 OFFSET 1;",
    "SELECT a FROM t LIMIT -1 OFFSET 2;",
    "SELECT a FROM t LIMIT 0;",
    "SELECT a FROM t LIMIT '2';",
    "SELECT a FROM t LIMIT 1 + 1;",
    "SELECT a FROM t LIMIT 2, 3;",
    "SELECT a FROM t LIMIT 3 OFFSET -2;",
    "SELECT a FROM t LIMIT 2.0;",
    "SELECT a FROM t LIMIT ' 3 ';",
    "SELECT a FROM t LIMIT 100 OFFSET 7;",
    "SELECT a FROM t LIMIT 9223372036854775807 OFFSET 9223372036854775807;",
    "SELECT a FROM t LIMIT 0 OFFSET abs(-9223372036854775808);",
    "SELECT a, f FROM t ORDER BY f LIMIT 3;",
    "SELECT a, f FROM t ORDER BY f LIMIT 2 OFFSET 2;",
    "SELECT a, f FROM t ORDER BY f DESC LIMIT -1 OFFSET 3;",
    "SELECT a, b FROM t ORDER BY b LIMIT 0;",
    "SELECT a FROM t WHERE a = 3;",
    "SELECT a FROM t WHERE rowid = 3;",
    "SELECT a FROM t WHERE a = '3';",
    "SELECT a FROM t WHERE a = 3.0;",
    "SELECT a FROM t WHERE a = 3.5;",
    "SELECT a FROM t WHERE a = NULL;",
    "SELECT a FROM t WHERE a = 8;",
    "SELECT a FROM t WHERE a = 'x';",
    "SELECT * FROM t WHERE a = 3 AND b = 7;",
    "SELECT * FROM t WHERE a = 3 AND b = 8;",
    "SELECT a FROM t WHERE a > 2;",
    "SELECT a FROM t WHERE a >= 2 AND a < 5;",
    "SELECT a FROM t WHERE a > 2 ORDER BY a DESC;",
    "SELECT a FROM t WHERE a < 4 ORDER BY a DESC;",
    "SELECT a FROM t WHERE a < 3.5;",
    "SELECT a FROM t WHERE a > 3.5;",
    "SELECT a FROM t WHERE a > '2';",
    "SELECT a FROM t WHERE a > 'x';",
    "SELECT a FROM t WHERE a < 'x';",
    "SELECT a FROM t WHERE a BETWEEN 2 AND 4;",
    "SELECT a FROM t WHERE a BETWEEN 4 AND 2;",
    "SELECT a FROM t WHERE a > 9223372036854775807;",
    "SELECT a FROM t WHERE a < -9223372036854775808;",
    "SELECT a FROM t WHERE a >= 9223372036854775807;",
    "SELECT a FROM t WHERE a > NULL;",
    "SELECT a FROM t WHERE a > 1 AND a < 8 AND c <> 'a' ORDER BY a DESC LIMIT 2;",
    "SELECT a, c FROM t WHERE a > 1 ORDER BY c LIMIT 3;",
    "SELECT a FROM t WHERE a > 1e1;",
    "SELECT a FROM t WHERE a >= -1.5 AND a <= 2.5;",
    "SELECT * FROM e;",
    "SELECT x FROM e WHERE y > 0 ORDER BY x DESC LIMIT 3;",
    "SELECT * FROM g;",
    "SELECT y, z FROM g WHERE y > 1 ORDER BY y DESC;",
    "SELECT * FROM al;",
    "SELECT r, s, typeof(s) FROM al ORDER BY r;",
    "SELECT 1, 'a', NULL, 2.5, hex(x'00ff');",
    "SELECT 1 WHERE 0;",
    "SELECT 1 WHERE NULL;",
    "SELECT 1 WHERE 1 AND 2;",
    "SELECT 1 LIMIT 1 OFFSET 1;",
    "SELECT 1 LIMIT 0;",
    "SELECT typeof(1.0), 1 / 0, 5 % 0, '1' + '2', 7 / 2, 7.0 / 2;",
    "SELECT abs(-9223372036854775808) LIMIT 1 OFFSET 1;",
    "SELECT 1 ORDER BY abs(-9223372036854775808);",
    "SELECT abs(-9223372036854775808) LIMIT 0;",
    "SELECT 1 WHERE abs(-9223372036854775808) LIMIT 0;",
    "SELECT 2 AS x ORDER BY x LIMIT 5;",
    // (Unit `vm_shrink`, regression: the join generator, now the only
    // one, never computes a FROM-less statement's ORDER BY terms.)
    "SELECT 1, 2 ORDER BY abs(-9223372036854775808) DESC LIMIT 5;",
    "SELECT 1 WHERE 1 ORDER BY abs(-9223372036854775808), 1;",
    "SELECT 1 ORDER BY abs(-9223372036854775808) LIMIT 1 OFFSET 0;",
    "SELECT 1 ORDER BY abs(-9223372036854775808) LIMIT 1 OFFSET 1;",
    "SELECT 'a' COLLATE nocase AS c ORDER BY c DESC, abs(-9223372036854775808);",
];

/// SELECTs whose statement fails, on the pin and on hipp (the rows
/// before the failing one first).
const ERRORS: &[&str] = &[
    "SELECT a, abs(b) FROM t;",
    "SELECT a, abs(b) FROM t ORDER BY a;",
    "SELECT a, abs(b) FROM t ORDER BY a DESC;",
    "SELECT a, abs(b) FROM t ORDER BY c;",
    "SELECT a FROM t ORDER BY abs(b);",
    "SELECT a FROM t WHERE abs(b) > 0;",
    "SELECT a FROM t ORDER BY abs(-9223372036854775808);",
    "SELECT a FROM t WHERE a > 0 AND abs(-9223372036854775808);",
    "SELECT a FROM t LIMIT 2.5;",
    "SELECT a FROM t LIMIT NULL;",
    "SELECT a FROM t LIMIT 'x';",
    "SELECT a FROM t LIMIT x'01';",
    "SELECT a FROM t LIMIT 1e30;",
    "SELECT a FROM t LIMIT '0x10';",
    "SELECT a FROM t LIMIT 1 OFFSET 'x';",
    "SELECT a FROM t LIMIT 1 OFFSET 2.5;",
    "SELECT a FROM t LIMIT abs(-9223372036854775808);",
    "SELECT a FROM t WHERE a = abs(-9223372036854775808);",
    "SELECT a FROM t WHERE a > abs(-9223372036854775808);",
    "SELECT a, json(CASE WHEN a = 4 THEN 'x' ELSE 1 END) FROM t WHERE a > 1 ORDER BY a DESC;",
    "SELECT 1 WHERE abs(-9223372036854775808);",
    "SELECT abs(-9223372036854775808) AS x ORDER BY x;",
    "SELECT 1 LIMIT 'x' OFFSET abs(-9223372036854775808);",
    "SELECT 1 LIMIT 1 OFFSET abs(-9223372036854775808);",
];

/// `T` built by the pin, under a tag of the calling test's own (the
/// tests run in parallel, each on its own files).
fn pin_t(tag: &str) -> std::path::PathBuf {
    pin_file(tag, T)
}

#[test]
fn claimed_shapes_match_the_pin() {
    assert!(common::oracle_is_pinned(), "{}", common::oracle_version());
    let base = pin_t("t-ok");
    corpus(&base, "ok", &strict(), OK);
}

/// FROM-less statements with a subquery somewhere and an ORDER BY term
/// that would fail (unit `vm_shrink` review): the pin's loop over no
/// table satisfies the ORDER BY, so its terms are never computed, a
/// subquery anywhere in the statement or in the term itself
/// notwithstanding. A deliberate change from base 5b67982, where the
/// removed single-table generator refused every subquery and the join
/// generator computed the terms (`IntegerOverflow`, as today's
/// executor still does for every row of this list).
#[cfg(feature = "sql")]
const FREE_ORDER_SUBQUERIES: &[&str] = &[
    "SELECT 1 ORDER BY (SELECT abs(-9223372036854775808));",
    "SELECT 1 ORDER BY (SELECT 1/0), abs(-9223372036854775808);",
    "SELECT 1 WHERE (SELECT 1) ORDER BY abs(-9223372036854775808);",
    "SELECT 1 WHERE EXISTS (SELECT 1) ORDER BY abs(-9223372036854775808);",
    "SELECT (SELECT 2) ORDER BY abs(-9223372036854775808);",
    "SELECT 1 ORDER BY abs(-9223372036854775808) LIMIT (SELECT 1);",
    "SELECT 1 IN (SELECT 1) ORDER BY abs(-9223372036854775808);",
    "SELECT 1 ORDER BY 1 IN (SELECT abs(-9223372036854775808));",
    "SELECT 1 WHERE 0 ORDER BY (SELECT abs(-9223372036854775808));",
    "SELECT 1 ORDER BY (SELECT 2) LIMIT 1 OFFSET (SELECT 1);",
    "SELECT 1 ORDER BY (SELECT abs(-9223372036854775808)) LIMIT 1 OFFSET (SELECT 0);",
    "WITH c AS (SELECT 1) SELECT 1 ORDER BY abs(-9223372036854775808);",
    "SELECT 1 LIMIT 0 OFFSET abs(-9223372036854775808);",
    "SELECT 1, 2 ORDER BY 2 DESC, abs(-9223372036854775808);",
];

#[cfg(feature = "sql")]
#[test]
fn from_less_order_by_terms_stay_uncomputed_with_subqueries() {
    let base = pin_t("t-free-sub");
    corpus(&base, "free-sub", &strict(), FREE_ORDER_SUBQUERIES);
}

#[test]
fn row_errors_come_after_the_rows_before_them() {
    let base = pin_t("t-err");
    for (i, q) in ERRORS.iter().enumerate() {
        let e = same_on(&base, &format!("err{i}"), strict(), q);
        assert!(e.is_some(), "must fail: {q}");
        assert!(
            !matches!(e, Some(Error::VmFallback)),
            "fell back: {q}: {e:?}"
        );
    }
}

/// Rows `n` of the big table: several B-tree levels at 512-byte
/// pages, a gap in the rowids, and long text every 97th row.
fn big_setup(n: usize) -> String {
    format!(
        "PRAGMA page_size = 512; \
         CREATE TABLE big(id INTEGER PRIMARY KEY, k INT, s TEXT); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < {n}) \
         INSERT INTO big SELECT i * 3, (i * 7919) % 1000, \
           CASE WHEN i % 97 = 0 THEN printf('%.1200c', char(65 + i % 26)) ELSE 'r' || i END FROM c; \
         CREATE TABLE ov(id INTEGER PRIMARY KEY, blob BLOB, txt TEXT); \
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 12) \
         INSERT INTO ov SELECT i, zeroblob(i * 900), printf('%.*c', i * 1500, char(97 + i)) FROM c;"
    )
}

const BIG: &[&str] = &[
    "SELECT id, k FROM big WHERE k = 7;",
    "SELECT id, length(s), substr(s, 1, 3) FROM big WHERE length(s) > 100;",
    "SELECT id FROM big ORDER BY k DESC, id LIMIT 12;",
    "SELECT id, k FROM big ORDER BY k LIMIT 5 OFFSET 990;",
    "SELECT id FROM big WHERE id > 5000 AND id < 5040;",
    "SELECT id FROM big WHERE id >= 8990 ORDER BY id DESC;",
    "SELECT id FROM big WHERE id < 40 ORDER BY id DESC;",
    "SELECT id, s FROM big WHERE id = 2997;",
    "SELECT id FROM big WHERE id = 2998;",
    "SELECT id FROM big LIMIT 3 OFFSET 2995;",
    "SELECT id, length(blob), hex(substr(blob, 900, 2)), length(txt), substr(txt, 1490, 20) FROM ov;",
    "SELECT id, length(txt) FROM ov ORDER BY length(txt) DESC LIMIT 4;",
    "SELECT id FROM ov WHERE id > 3 AND id < 11 ORDER BY id DESC;",
];

#[test]
fn large_and_overflow_tables_match_the_pin() {
    let base = pin_file("big", &big_setup(3000));
    corpus(&base, "big", &strict(), BIG);
}

#[test]
fn empty_table_and_no_rows() {
    let base = pin_file("empty", "CREATE TABLE z(a INTEGER PRIMARY KEY, b);");
    corpus(
        &base,
        "empty",
        &strict(),
        &[
            "SELECT * FROM z;",
            "SELECT b FROM z ORDER BY b;",
            "SELECT a FROM z WHERE a = 1;",
            "SELECT a FROM z WHERE a > 1 ORDER BY a DESC;",
            "SELECT abs(-9223372036854775808) FROM z;",
            "SELECT b FROM z ORDER BY abs(-9223372036854775808);",
            "SELECT b FROM z LIMIT 1 OFFSET 1;",
        ],
    );
}

// ---------------- the legacy scan plan, parameters ----------------

#[test]
fn legacy_scan_plans_run_on_the_vm() {
    // `select_is_simple` shapes plan as `Plan::Scan` on a table
    // without an index.
    let base = pin_t("t-scan");
    corpus(
        &base,
        "scan",
        &strict(),
        &[
            "SELECT * FROM t;",
            "SELECT c, a, rowid FROM t;",
            "SELECT * FROM t WHERE a = 4;",
            "SELECT * FROM t WHERE rowid = 99;",
            "SELECT * FROM t WHERE a = 'abc';",
            "SELECT d FROM t WHERE a = 6;",
            "SELECT * FROM al;",
            "SELECT s, r FROM al WHERE p = 2;",
        ],
    );
}

/// Bind `vals` (1-based) on `q`.
fn bind<S: Storage>(q: &mut hipp::Statement<'_, S>, vals: &[hipp::Value<'_>])
where
    S::Error: core::fmt::Debug,
{
    for (i, v) in vals.iter().enumerate() {
        q.bind_value(i + 1, *v).expect("bind");
    }
}

#[test]
fn parameters_compile_per_execution() {
    use hipp::Value as V;
    let base = pin_t("t-par");
    let h = copy_of(&base, "vm17-par-h");
    let o = copy_of(&base, "vm17-par-o");
    let sql = "SELECT a, b, c FROM t WHERE a > ? AND c <> ? ORDER BY c, a LIMIT ? OFFSET ?";
    let runs: &[(&[V<'_>], &str)] = &[
        (
            &[V::Integer(1), V::Text(b"x"), V::Integer(3), V::Integer(1)],
            "SELECT a, b, c FROM t WHERE a > 1 AND c <> 'x' ORDER BY c, a LIMIT 3 OFFSET 1;",
        ),
        (
            &[V::Text(b"4"), V::Text(b"B"), V::Integer(-1), V::Integer(1)],
            "SELECT a, b, c FROM t WHERE a > '4' AND c <> 'B' ORDER BY c, a LIMIT -1 OFFSET 1;",
        ),
        (
            &[V::Real(2.5), V::Text(b"c"), V::Text(b"2"), V::Integer(0)],
            "SELECT a, b, c FROM t WHERE a > 2.5 AND c <> 'c' ORDER BY c, a LIMIT '2' OFFSET 0;",
        ),
    ];
    let hipp_rows = block_on(async {
        let fs = FileStorage::open(&h).expect("fs open");
        let mut db = Connection::open(fs, strict()).await.expect("open");
        let mut q = db.prepare(sql).await.expect("prepare");
        let mut all = Vec::new();
        for (vals, _) in runs {
            q.reset();
            bind(&mut q, vals);
            let mut rows = Vec::new();
            let mut r = q.step().await;
            assert!(q.runs_on_vm(), "parameters: not on the VM");
            loop {
                match r {
                    Ok(Step::Row) => rows.push(render(&q)),
                    Ok(Step::Done) => break,
                    Err(e) => {
                        rows.push(format!("error {e:?}"));
                        break;
                    }
                }
                r = q.step().await;
            }
            all.push(rows);
        }
        // A rowid lookup and a range bound by parameters.
        let mut q = db
            .prepare("SELECT a, d FROM t WHERE a = ?")
            .await
            .expect("prepare");
        bind(&mut q, &[V::Text(b"6")]);
        let mut rows = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            rows.push(render(&q));
        }
        assert!(q.runs_on_vm());
        all.push(rows);
        let mut q = db
            .prepare("SELECT a FROM t WHERE a >= ? AND a < ? ORDER BY a DESC")
            .await
            .expect("prepare");
        bind(&mut q, &[V::Real(1.5), V::Integer(6)]);
        let mut rows = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            rows.push(render(&q));
        }
        assert!(q.runs_on_vm());
        all.push(rows);
        all
    });
    let mut want = Vec::new();
    for (_, lit) in runs {
        want.push(oracle_script(&o, lit).1);
    }
    want.push(oracle_script(&o, "SELECT a, d FROM t WHERE a = '6';").1);
    want.push(
        oracle_script(
            &o,
            "SELECT a FROM t WHERE a >= 1.5 AND a < 6 ORDER BY a DESC;",
        )
        .1,
    );
    assert_eq!(hipp_rows, want);
}

// ---------------- fallback ----------------

/// The shapes that still fall back after unit `vm_rest`: each only to
/// reproduce today's executor's refusal of it. Strict mode refuses
/// them (`VmFallback`); without it they run on today's executor and
/// fail as it does. (Every other shape moved to the suites of the
/// units that claimed it: `tests/vm_joins_v17.rs`,
/// `tests/vm_subqueries_v17.rs`, `tests/vm_rest_v17.rs`.)
const UNCLAIMED: &[&str] = &[
    // An enclosing query's aggregate over a subquery argument: today's
    // `Unsupported` (the pin answers).
    "SELECT a, (SELECT group_concat((SELECT k FROM u WHERE t.a > 0 LIMIT 1)) FROM t AS t2) FROM t;",
    // An enclosing query's aggregate inside an inner aggregate: the
    // pin's misuse error, refused by today's executor.
    "SELECT (SELECT sum((SELECT max(t.a))) FROM u) FROM t;",
    // (A nested query's index bound reading an enclosing query's
    // aggregate, `u.k <= count(t.a)` over an index on u.k, was today's
    // `MisusedAggregate`; since the `vm_flip` review fixes both engines
    // answer it like the pin: tests/vm_subqueries_v17.rs,
    // tests/outer_agg_bound_v17.rs.)
];

const U: &str = "CREATE TABLE u(k, v); INSERT INTO u VALUES (1, 'p'), (2, 'q'), (3, 'r'); \
    CREATE INDEX u_k ON u(k); CREATE VIEW v AS SELECT a FROM t WHERE a < 3; \
    CREATE TABLE wr(x PRIMARY KEY, y) WITHOUT ROWID; INSERT INTO wr VALUES (1, 2);";

#[test]
fn unclaimed_shapes_fall_back_whole() {
    let base = pin_file("unclaimed", &format!("{T} {U}"));
    for (i, q) in UNCLAIMED.iter().enumerate() {
        // Strict: refused before any row.
        let h = copy_of(&base, &format!("vm17-uc{i}-s"));
        let (ok, rows, e) = block_on(async {
            let fs = FileStorage::open(&h).expect("fs open");
            let mut db = Connection::open(fs, strict()).await.expect("open");
            let mut q2 = db.prepare(q).await.expect("prepare");
            let mut rows = Vec::new();
            loop {
                match q2.step().await {
                    Ok(Step::Row) => rows.push(render(&q2)),
                    Ok(Step::Done) => return (true, rows, None),
                    Err(e) => return (false, rows, Some(e)),
                }
            }
        });
        assert!(!ok && rows.is_empty(), "strict must refuse: {q}");
        assert!(matches!(e, Some(Error::VmFallback)), "{q}: {e:?}");
        // Regression: asking the coverage hook before the first step
        // compiles the program, and used to turn strict mode off for
        // the statement (the first step then ran on today's
        // executor).
        let h = copy_of(&base, &format!("vm17-uc{i}-k"));
        let (on_vm, first) = block_on(async {
            let fs = FileStorage::open(&h).expect("fs open");
            let mut db = Connection::open(fs, strict()).await.expect("open");
            let mut q2 = db.prepare(q).await.expect("prepare");
            let on_vm = q2.runs_on_vm();
            (on_vm, q2.step().await.err())
        });
        assert!(!on_vm, "unclaimed but on the VM: {q}");
        assert!(
            matches!(first, Some(Error::VmFallback)),
            "{q}: hook first: {first:?}"
        );
        // Default: today's executor and its refusal, whatever the
        // VM's build (`vm_off` is today's executor alone).
        let run = |cfg: Config, tag: &str| {
            let h = copy_of(&base, &format!("vm17-uc{i}-{tag}"));
            block_on(async {
                let fs = FileStorage::open(&h).expect("fs open");
                let mut db = Connection::open(fs, cfg).await.expect("open");
                let mut q2 = db.prepare(q).await.expect("prepare");
                let mut rows = Vec::new();
                loop {
                    match q2.step().await {
                        Ok(Step::Row) => rows.push(render(&q2)),
                        Ok(Step::Done) => return (rows, None, q2.runs_on_vm()),
                        Err(e) => return (rows, Some(format!("{e:?}")), q2.runs_on_vm()),
                    }
                }
            })
        };
        let (hl, he, on_vm) = run(Config::default().vm_strict(false), "h");
        assert!(!on_vm, "on the VM: {q}");
        let (tl, te, _) = run(Config::default().vm_off(true), "t");
        assert_eq!((hl, he.clone()), (tl, te), "{q}");
        assert!(he.is_some(), "today's executor answers: {q}");
    }
}

/// FROM subqueries, views and CTEs the planner's flattener merges
/// into their reader are single-table plans, so they run on the VM
/// (whole: the flattened plan has no derived source left).
#[cfg(feature = "planner")]
#[test]
fn flattened_subqueries_views_and_ctes_run_on_the_vm() {
    let base = pin_file("flat", &format!("{T} {U}"));
    corpus(
        &base,
        "flat",
        &strict(),
        &[
            "SELECT * FROM (SELECT a FROM t) WHERE a > 2;",
            "WITH w AS (SELECT a, c FROM t WHERE a < 6) SELECT c FROM w ORDER BY c DESC;",
            "SELECT * FROM v;",
        ],
    );
}

#[test]
fn statements_other_than_select_never_compile() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), strict())
            .await
            .expect("open");
        for sql in [
            "CREATE TABLE x(a)",
            "INSERT INTO x VALUES (1)",
            "UPDATE x SET a = 2",
            "PRAGMA user_version",
            "DELETE FROM x",
        ] {
            let mut q = db.prepare(sql).await.expect("prepare");
            while q.step().await.expect("step") == Step::Row {}
            assert!(!q.runs_on_vm(), "{sql}");
        }
    });
}

// ---------------- restart: the smallest cache ----------------

/// Every claimed query again with a page cache of `pages` frames:
/// each opcode that misses returns, the page is read, and the opcode
/// runs again.
fn restart_corpus(pages: usize) {
    let cfg = strict().cache_pages(pages);
    let base = pin_t(&format!("t-rs{pages}"));
    corpus(&base, &format!("rs{pages}-"), &cfg, OK);
    for (i, q) in ERRORS.iter().enumerate() {
        same_on(&base, &format!("rs{pages}e{i}"), cfg.clone(), q);
    }
    let big = pin_file(&format!("rsbig{pages}"), &big_setup(3000));
    corpus(&big, &format!("rs{pages}b"), &cfg, BIG);
}

#[test]
fn restart_with_the_smallest_cache() {
    // One frame, the smallest cache the engine allows: the big table
    // is three levels high, so every descent misses at each level.
    restart_corpus(1);
}

#[test]
fn restart_with_two_frames() {
    restart_corpus(2);
}

/// `sql`'s rows (or its failure) on a copy of `base`, under `cfg`.
fn rows_of(
    base: &std::path::Path,
    tag: &str,
    cfg: Config,
    sql: &str,
) -> Result<Vec<String>, String> {
    let h = copy_of(base, tag);
    block_on(async {
        let fs = FileStorage::open(&h).expect("fs open");
        let mut db = Connection::open(fs, cfg)
            .await
            .map_err(|e| format!("{e:?}"))?;
        let mut q = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
        let mut rows = Vec::new();
        loop {
            match q.step().await {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => return Ok(rows),
                Err(e) => return Err(format!("{e:?}")),
            }
        }
    })
}

#[test]
fn deep_trees_read_in_a_cache_smaller_than_their_height() {
    // Regression: an opcode used to pin every page of its descent, so
    // a tree taller than the cache was `CacheExhausted` on the VM
    // (1 frame for a 2-level table, 2 for a 3-level one) where
    // today's executor returns the pin's rows.
    let two = pin_file(
        "deep2",
        "PRAGMA page_size = 512; CREATE TABLE t(a INTEGER PRIMARY KEY, b); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 200) \
         INSERT INTO t SELECT i, 'row' || i FROM r;",
    );
    let three = pin_file(
        "deep3",
        "PRAGMA page_size = 512; CREATE TABLE t(a INTEGER PRIMARY KEY, b); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 20000) \
         INSERT INTO t SELECT i, 'row' || i FROM r;",
    );
    let qs = [
        "SELECT a, b FROM t WHERE a % 50 = 0;",
        "SELECT b FROM t WHERE a = 150;",
        "SELECT a FROM t WHERE a > 120 AND a < 140 ORDER BY a DESC;",
        "SELECT a, b FROM t ORDER BY b DESC LIMIT 3;",
        "SELECT count(*) FROM t;",
    ];
    for (base, tag) in [(&two, "d2"), (&three, "d3")] {
        for pages in [1, 2, 3] {
            for (i, q) in qs.iter().enumerate() {
                same_on(
                    base,
                    &format!("{tag}-{pages}-{i}"),
                    strict().cache_pages(pages),
                    q,
                );
            }
        }
    }
    // No frame at all: the VM and today's executor fail alike.
    let vm = rows_of(
        &three,
        "vm17-d0-vm",
        Config::default().cache_pages(0),
        qs[1],
    );
    let legacy = rows_of(
        &three,
        "vm17-d0-legacy",
        Config::default().cache_pages(0).vm_off(true),
        qs[1],
    );
    assert!(vm.is_err(), "{vm:?}");
    assert_eq!(vm, legacy);
}

/// Index keys of several overflow pages, seeks and scans through a
/// one- and two-frame cache (the index cursors of unit `vm_joins`
/// hold their pages the same way).
#[cfg(feature = "expr_index")]
#[test]
fn index_keys_longer_than_the_cache_read_in_one_frame() {
    let base = pin_file(
        "ixov",
        "PRAGMA page_size = 512; CREATE TABLE w(a INTEGER PRIMARY KEY, s TEXT, n INT); \
         CREATE INDEX ws ON w(s); CREATE INDEX wn ON w(n); \
         WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 300) \
         INSERT INTO w SELECT i, printf('%.1800c', char(97 + i % 26)) || i, i % 17 FROM r;",
    );
    let qs = [
        "SELECT a, length(s) FROM w WHERE s > printf('%.1800c', 'q') ORDER BY s LIMIT 5;",
        "SELECT a FROM w WHERE s = printf('%.1800c', 'c') || '106';",
        "SELECT a, n FROM w WHERE n = 3 ORDER BY a;",
        "SELECT count(*), min(s) = max(s) FROM w WHERE s < 'b';",
    ];
    for pages in [1, 2] {
        for (i, q) in qs.iter().enumerate() {
            same_on(
                &base,
                &format!("ixov{pages}-{i}"),
                strict().cache_pages(pages),
                q,
            );
        }
    }
}

#[test]
fn a_small_cache_reads_overflow_chains_longer_than_itself() {
    // 18,000-byte values over 512-byte pages: chains of 35 pages
    // through a cache of 1 to 3 frames.
    let base = pin_file("chain", &big_setup(10));
    for pages in [1, 2, 3] {
        same_on(
            &base,
            &format!("chain{pages}"),
            strict().cache_pages(pages),
            "SELECT id, length(txt), substr(txt, -5), hex(substr(blob, -2)) FROM ov ORDER BY id DESC;",
        );
    }
}

// ---------------- cancellation: dropped step futures ----------------

fn noop_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: the vtable's functions ignore the data pointer.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let w = noop_waker();
    let mut cx = Context::from_waker(&w);
    fut.poll(&mut cx)
}

/// A future that is pending `left` times, then ready.
struct Pend {
    left: usize,
}

impl Future for Pend {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.left == 0 {
            Poll::Ready(())
        } else {
            self.left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// In-memory storage whose reads are pending once before they run.
#[derive(Clone)]
struct SlowReads(MemStorage);

impl Storage for SlowReads {
    type Error = <MemStorage as Storage>::Error;
    type Sidecar = <MemStorage as Storage>::Sidecar;
    async fn len(&self) -> Result<u64, Self::Error> {
        self.0.len().await
    }
    async fn read_at(&self, o: u64, b: &mut [u8]) -> Result<(), Self::Error> {
        Pend { left: 1 }.await;
        self.0.read_at(o, b).await
    }
    async fn write_at(&mut self, o: u64, b: &[u8]) -> Result<(), Self::Error> {
        self.0.write_at(o, b).await
    }
    async fn set_len(&mut self, l: u64) -> Result<(), Self::Error> {
        self.0.set_len(l).await
    }
    async fn sync(&mut self) -> Result<(), Self::Error> {
        self.0.sync().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<Self::Sidecar, Self::Error> {
        self.0.sidecar(kind).await
    }
}

/// A database image the pin built from `setup`, in memory.
fn mem_of(tag: &str, setup: &str) -> MemStorage {
    let base = pin_file(tag, setup);
    let bytes = std::fs::read(&base).expect("read");
    let mem = MemStorage::new();
    let mut m = mem.clone();
    block_on(async {
        m.write_at(0, &bytes).await.expect("write");
    });
    mem
}

/// `sql`'s rows, each step future polled once and dropped `drops`
/// times before it is driven to the end; and how many of those
/// futures were dropped while pending.
fn rows_with_drops(mem: &MemStorage, cfg: Config, sql: &str, drops: usize) -> (Vec<String>, usize) {
    let st = SlowReads(mem.clone());
    let mut db = block_on(Connection::open(st, cfg)).expect("open");
    let mut q = block_on(db.prepare(sql)).expect("prepare");
    let mut rows = Vec::new();
    let mut dropped = 0usize;
    loop {
        for _ in 0..drops {
            let mut f = Box::pin(q.step());
            let p = poll_once(f.as_mut());
            if p.is_pending() {
                dropped += 1;
            }
            if let Poll::Ready(r) = p {
                // Ready at once (no read needed): that step counts.
                drop(f);
                match r.expect("step") {
                    Step::Row => {
                        rows.push(render(&q));
                        continue;
                    }
                    Step::Done => {
                        assert!(q.runs_on_vm());
                        return (rows, dropped);
                    }
                }
            }
        }
        match block_on(q.step()).expect("step") {
            Step::Row => rows.push(render(&q)),
            Step::Done => {
                assert!(q.runs_on_vm());
                return (rows, dropped);
            }
        }
    }
}

#[test]
fn dropped_step_futures_resume_exactly() {
    let setup = big_setup(400);
    let mem = mem_of("cancel-m", &setup);
    let o = pin_file("cancel-o", &setup);
    for sql in [
        "SELECT id, k FROM big WHERE k > 900;",
        "SELECT id, length(s) FROM big ORDER BY k DESC, id LIMIT 7;",
        "SELECT id FROM big WHERE id > 300 AND id < 450 ORDER BY id DESC;",
        "SELECT id, length(txt), substr(txt, -3) FROM ov;",
        "SELECT id, s FROM big WHERE id = 291;",
    ] {
        let (_, want) = oracle_script(&o, sql);
        for (pages, drops) in [(16, 1), (3, 1), (3, 3), (5, 2), (1, 1), (1, 3), (2, 2)] {
            let (got, dropped) = rows_with_drops(&mem, strict().cache_pages(pages), sql, drops);
            assert_eq!(got, want, "{sql} (cache {pages}, drops {drops})");
            // Every query reads pages it misses: futures were dropped
            // while their read was pending.
            assert!(dropped > 0, "{sql}: no step was pending");
        }
    }
}

#[test]
fn reset_part_way_starts_over() {
    let setup = big_setup(400);
    let mem = mem_of("reset-m", &setup);
    let o = pin_file("reset-o", &setup);
    let sql = "SELECT id, k FROM big WHERE k < 50 ORDER BY k, id;";
    let (_, want) = oracle_script(&o, sql);
    let st = SlowReads(mem.clone());
    let mut db = block_on(Connection::open(st, strict().cache_pages(1))).expect("open");
    let mut q = block_on(db.prepare(sql)).expect("prepare");
    // Three rows, then a step future dropped mid-read, then reset.
    for _ in 0..3 {
        assert_eq!(block_on(q.step()).expect("step"), Step::Row);
    }
    {
        let mut f = Box::pin(q.step());
        let _ = poll_once(f.as_mut());
    }
    q.reset();
    let mut rows = Vec::new();
    while block_on(q.step()).expect("step") == Step::Row {
        rows.push(render(&q));
    }
    assert!(q.runs_on_vm());
    assert_eq!(rows, want);
    drop(q);
    // The connection goes on: a write and a read with every frame
    // evictable again.
    block_on(async {
        let mut w = db
            .prepare("INSERT INTO big VALUES (100000, 1, 'z')")
            .await
            .expect("prepare");
        assert_eq!(w.step().await.expect("step"), Step::Done);
        drop(w);
        let mut r = db
            .prepare("SELECT s FROM big WHERE id = 100000")
            .await
            .expect("prepare");
        assert_eq!(r.step().await.expect("step"), Step::Row);
        assert_eq!(render(&r), "z");
    });
}

#[test]
fn an_error_then_a_step_starts_over() {
    // The pin's statement after an error is reset; today's executors
    // start the statement over when it is stepped again, and so does
    // the VM.
    let base = pin_t("t-again");
    let h = copy_of(&base, "vm17-again-h");
    block_on(async {
        let fs = FileStorage::open(&h).expect("fs open");
        let mut db = Connection::open(fs, strict()).await.expect("open");
        let mut q = db
            .prepare("SELECT a, abs(b) FROM t")
            .await
            .expect("prepare");
        for _ in 0..2 {
            assert_eq!(q.step().await.expect("row 1"), Step::Row);
            assert_eq!(render(&q), "1|10");
            let e = q.step().await.expect_err("row 2 fails");
            assert!(
                matches!(
                    e,
                    Error::Schema {
                        kind: SchemaKind::IntegerOverflow
                    }
                ),
                "{e:?}"
            );
        }
        assert!(q.runs_on_vm());
    });
}

// ---------------- a generated corpus ----------------

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

#[test]
fn generated_single_table_selects_match_the_pin() {
    let base = pin_t("t-gen");
    let mut rng = Rng(0x766d_636f_7265_0017);
    let items = [
        "a",
        "b",
        "c",
        "d",
        "f",
        "a * 2",
        "c || a",
        "typeof(f)",
        "coalesce(d, b)",
        "length(c)",
        "e",
        "rowid",
        "CASE WHEN b > 5 THEN b END",
        "upper(c)",
    ];
    let wheres = [
        "",
        " WHERE a > 2",
        " WHERE a <= 5",
        " WHERE a = 3",
        " WHERE b > 5",
        " WHERE c = 'a'",
        " WHERE d IS NOT NULL",
        " WHERE a > 1 AND a < 7",
        " WHERE f = 5 OR b IS NULL",
        " WHERE c IN ('a', 'b')",
        " WHERE a BETWEEN 2 AND 6 AND c <> ''",
        " WHERE 1",
        " WHERE 0",
    ];
    let orders = [
        "",
        " ORDER BY a",
        " ORDER BY a DESC",
        " ORDER BY b",
        " ORDER BY c, a DESC",
        " ORDER BY d DESC NULLS LAST",
        " ORDER BY f",
        " ORDER BY 1",
        " ORDER BY 1 DESC, 2",
        " ORDER BY typeof(b), a",
    ];
    let limits = [
        "",
        " LIMIT 3",
        " LIMIT 2 OFFSET 2",
        " LIMIT -1 OFFSET 5",
        " LIMIT 0",
        " LIMIT 1, 4",
        " LIMIT '4'",
    ];
    let mut qs = Vec::new();
    for _ in 0..120 {
        let n = 1 + rng.below(3) as usize;
        let cols: Vec<&str> = (0..n).map(|_| rng.pick(&items)).collect();
        qs.push(format!(
            "SELECT {} FROM t{}{}{};",
            cols.join(", "),
            rng.pick(&wheres),
            rng.pick(&orders),
            rng.pick(&limits)
        ));
    }
    let qs: Vec<&str> = qs.iter().map(String::as_str).collect();
    corpus(&base, "gen", &strict(), &qs);
    corpus(&base, "genrs", &strict().cache_pages(1), &qs);
}
