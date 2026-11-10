//! The bytecode VM (ADR 0005 Stage 2, hidden feature `vm`), unit
//! `vm_rest`, differentially vs the pinned oracle (SQLite 3.54.1):
//! window functions, virtual and table-valued sources
//! (generate_series, dbstat, rtree, fts5, fts5vocab, json_each /
//! json_tree), `sqlite_schema` and the `pragma_*` functions, WITHOUT
//! ROWID, TEMP and attached tables, the SELECTs of trigger programs,
//! the streamed virtual-table limits and plain EXPLAIN. Built only
//! with `--features vm`; every SELECT a corpus claims runs under
//! `Config::vm_strict` and asserts `Statement::runs_on_vm`.
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
    let base = common::temp_db(&format!("vmr17-{tag}-base"));
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
        let h = copy_of(base, &format!("vmr17-{tag}{i}-h"));
        let o = copy_of(base, &format!("vmr17-{tag}{i}-o"));
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

// ---------------- the corpora ----------------

const S: &str = "CREATE TABLE t(a, b TEXT); \
    INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, NULL), (NULL, 'z'), (2, 'Y'); \
    CREATE TABLE j(id INTEGER PRIMARY KEY, doc); \
    INSERT INTO j VALUES (1, '[1,2,3]'), (2, '{\"a\":1,\"b\":[4,5]}'), (3, NULL), (4, '[]'); \
    CREATE TABLE w(k TEXT PRIMARY KEY, v INT, z) WITHOUT ROWID; \
    INSERT INTO w VALUES ('b', 2, 'x'), ('a', 1, NULL), ('d', 4, 'y'), ('c', 3, 'x'); \
    CREATE TABLE w2(p INT, q INT, r, PRIMARY KEY (p DESC, q)) WITHOUT ROWID; \
    INSERT INTO w2 VALUES (1, 1, 'a'), (1, 2, 'b'), (2, 1, 'c'), (3, 3, NULL); \
    CREATE INDEX w_v ON w(v); \
    CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1, y0, y1); \
    INSERT INTO rt VALUES (1, 0, 10, 0, 10), (2, 5, 15, 5, 15), (3, 20, 30, 20, 30); \
    CREATE VIRTUAL TABLE ft USING fts5(body, tag UNINDEXED); \
    INSERT INTO ft VALUES ('alpha beta', 'one'), ('beta gamma', 'two'), ('gamma delta alpha', 'three'); \
    CREATE VIRTUAL TABLE fv USING fts5vocab(ft, 'row'); \
    CREATE VIEW vw AS SELECT k, v FROM w WHERE v > 1;";

const SOURCES: &[&str] = &[
    "SELECT * FROM generate_series(1, 5);",
    "SELECT value FROM generate_series(10, 1, -3);",
    "SELECT * FROM generate_series(1, 20) WHERE value % 7 = 0;",
    "SELECT value FROM generate_series(1) LIMIT 4;",
    "SELECT value FROM generate_series(1) WHERE value > 3 LIMIT 2;",
    "SELECT a, value FROM t, generate_series(1, a) ORDER BY a, value;",
    "SELECT a, value FROM t JOIN generate_series(1, 2) ON value = a;",
    "SELECT a, value FROM t LEFT JOIN generate_series(1, a) ON 1 ORDER BY 1, 2;",
    "SELECT count(*), sum(value) FROM generate_series(1, 1000);",
    "SELECT value FROM generate_series(1, 10) ORDER BY value LIMIT 3 OFFSET 2;",
    "SELECT DISTINCT value % 3 FROM generate_series(1, 10);",
    "SELECT value, count(*) FROM generate_series(1, 10) GROUP BY value % 2 ORDER BY 2;",
    "SELECT (SELECT sum(value) FROM generate_series(1, a)) FROM t;",
    "SELECT * FROM j, json_each(j.doc) ORDER BY j.id, key;",
    "SELECT id, key, value, type, atom, path FROM j, json_each(doc);",
    "SELECT id, fullkey, value FROM j, json_tree(doc) WHERE type <> 'object';",
    "SELECT key, value FROM json_each('[10, 20, 30]') WHERE value > 15;",
    "SELECT key, value FROM json_each('{\"x\":1,\"y\":2}');",
    "SELECT * FROM json_each('[1,2,3]') AS a, json_each('[4,5]') AS b;",
    "SELECT id, (SELECT count(*) FROM json_each(j.doc)) FROM j;",
    "SELECT id, count(*) FROM j, json_each(j.doc) GROUP BY id;",
    "SELECT * FROM j LEFT JOIN json_each(j.doc) ON 1;",
    "SELECT value FROM json_each('[3,1,2]') ORDER BY value LIMIT 2;",
    "SELECT * FROM rt ORDER BY id;",
    "SELECT id FROM rt WHERE x0 > 4;",
    "SELECT id FROM rt WHERE x1 < 12 AND y0 >= 0;",
    "SELECT id FROM rt WHERE id = 2;",
    "SELECT a, id FROM t, rt WHERE rt.x0 <= a * 5 ORDER BY 1, 2;",
    "SELECT rowid, body FROM ft WHERE ft MATCH 'alpha';",
    "SELECT rowid, body, tag FROM ft WHERE ft MATCH 'beta OR delta' ORDER BY rank;",
    "SELECT rowid, highlight(ft, 0, '[', ']') FROM ft WHERE ft MATCH 'gamma';",
    "SELECT count(*) FROM ft;",
    "SELECT * FROM ft;",
    "SELECT term, doc, cnt FROM fv ORDER BY term;",
    "SELECT a, rowid FROM t, ft WHERE ft MATCH 'beta' ORDER BY 1, 2;",
    "SELECT name, path, pageno FROM dbstat WHERE name = 't';",
    "SELECT count(*) > 0 FROM dbstat;",
    "SELECT * FROM w;",
    "SELECT * FROM w ORDER BY k DESC;",
    "SELECT * FROM w WHERE k = 'c';",
    "SELECT * FROM w WHERE k > 'a' AND k < 'd';",
    "SELECT * FROM w WHERE v = 3;",
    "SELECT k FROM w WHERE v > 1 ORDER BY v;",
    "SELECT * FROM w2;",
    "SELECT * FROM w2 WHERE p = 1;",
    "SELECT a, k FROM t JOIN w ON w.v = t.a ORDER BY 1, 2;",
    "SELECT a, k FROM t LEFT JOIN w ON w.k = t.b ORDER BY 1;",
    "SELECT k, count(*) FROM w, t GROUP BY k;",
    "SELECT * FROM w LIMIT 2;",
    "SELECT * FROM vw;",
    "SELECT * FROM w RIGHT JOIN t ON t.a = w.v;",
    "SELECT type, name, tbl_name FROM sqlite_schema ORDER BY name;",
    "SELECT name FROM sqlite_schema WHERE type = 'table';",
    "SELECT count(*) FROM sqlite_master;",
    "SELECT name, sql IS NULL FROM sqlite_schema WHERE rootpage > 0 LIMIT 3;",
    "SELECT * FROM pragma_table_info('t');",
    "SELECT name, pk FROM pragma_table_info('w') ORDER BY pk;",
    "SELECT name FROM pragma_index_list('w');",
];

#[test]
fn sources_match_the_pin() {
    let base = pin_file("src", S);
    corpus(&base, "s", &strict(), SOURCES);
}

const W: &str = "CREATE TABLE t(a, b, c, n TEXT COLLATE NOCASE); \
    INSERT INTO t VALUES (1,'x',5,'A'),(2,'y',3,'a'),(1,'z',4,'B'),(2,'w',1,NULL),(1,'q',2,'b'),(0,'p',6,'c'); \
    CREATE TABLE d(k, v); \
    INSERT INTO d VALUES (1,1),(2,2),(NULL,3),(2.5,4),('a',5),('b',6),(x'41',7),(NULL,8),(4,9),(6,10); \
    CREATE TABLE e(x); \
    CREATE VIEW tv AS SELECT * FROM t;";

const WINDOWS: &[&str] = &[
    "SELECT b, a, row_number() OVER (ORDER BY a) FROM t;",
    "SELECT b, a, row_number() OVER (PARTITION BY a) FROM t;",
    "SELECT a, rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w FROM t WINDOW w AS (ORDER BY a);",
    "SELECT a, c, ntile(4) OVER (ORDER BY c), ntile(10) OVER (ORDER BY c DESC) FROM t;",
    "SELECT c, lag(c) OVER (ORDER BY c), lead(c) OVER (ORDER BY c), lag(c, 2, -1) OVER (ORDER BY c) FROM t;",
    "SELECT c, first_value(b) OVER w, last_value(b) OVER w, nth_value(b, 2) OVER w FROM t WINDOW w AS (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING);",
    "SELECT c, sum(c) OVER (ORDER BY c), sum(c) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t;",
    "SELECT a, c, sum(c) OVER (ORDER BY a RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING), sum(c) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t;",
    "SELECT a, c, sum(c) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP), sum(c) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t;",
    "SELECT a, c, sum(c) FILTER (WHERE c > 2) OVER (ORDER BY a), group_concat(b) FILTER (WHERE c % 2) OVER () FROM t;",
    "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM d;",
    "SELECT k, v, first_value(v) OVER (ORDER BY k NULLS LAST) FROM d;",
    "SELECT b, a, c, rank() OVER (ORDER BY b), row_number() OVER (ORDER BY a DESC), row_number() OVER (ORDER BY c) FROM t;",
    "SELECT b, a, c, rank() OVER (ORDER BY b) FROM t ORDER BY row_number() OVER (ORDER BY a DESC);",
    "SELECT a, sum(c) OVER (PARTITION BY a ORDER BY c) FROM t ORDER BY 2 DESC LIMIT 3;",
    "SELECT a, row_number() OVER (PARTITION BY a) AS rn FROM t ORDER BY rn, a;",
    "SELECT DISTINCT a, count(*) OVER (PARTITION BY a) FROM t;",
    "SELECT a, row_number() OVER () FROM t LIMIT 2 OFFSET 1;",
    "SELECT *, row_number() OVER () FROM t;",
    "SELECT a, b FROM t ORDER BY row_number() OVER (ORDER BY c DESC) LIMIT 2;",
    "SELECT a, c, sum(c) OVER (PARTITION BY a COLLATE NOCASE), sum(c) OVER (PARTITION BY n) FROM t;",
    "SELECT count(*) OVER () FROM t GROUP BY a;",
    "SELECT a, sum(sum(c)) OVER (ORDER BY a) FROM t GROUP BY a;",
    "SELECT a, count(*), sum(count(*)) OVER (ORDER BY a), rank() OVER (ORDER BY count(*) DESC) FROM t GROUP BY a;",
    "SELECT a, max(c), first_value(max(c)) OVER (ORDER BY a DESC) FROM t GROUP BY a ORDER BY a;",
    "SELECT a, count(*) OVER () FROM t GROUP BY a HAVING a > 0;",
    "SELECT a, count(*) OVER () FROM t GROUP BY a LIMIT 1;",
    "SELECT sum(a), row_number() OVER (), count(*) OVER () FROM t;",
    "SELECT count(*), count(*) OVER () FROM t WHERE 0;",
    "SELECT sum(a) OVER (), a FROM t WHERE 0;",
    "SELECT max(c) OVER (), min(c) FROM t;",
    "SELECT a, b, max(c) OVER (PARTITION BY a) FROM t WHERE c > 1 ORDER BY 1, 2;",
    "SELECT (SELECT row_number() OVER () FROM t LIMIT 1);",
    "SELECT a, (SELECT count(*) OVER () FROM t u WHERE u.a = t.a LIMIT 1) FROM t;",
    "SELECT * FROM (SELECT a, row_number() OVER (ORDER BY c) rn FROM t) WHERE rn > 3;",
    "WITH w AS (SELECT a, sum(c) OVER (PARTITION BY a) s FROM t) SELECT * FROM w ORDER BY a;",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n, sum(n) OVER (ORDER BY n ROWS 1 PRECEDING) FROM r;",
    "SELECT a, b, row_number() OVER (ORDER BY (SELECT count(*) FROM t u WHERE u.a <= t.a)) FROM t;",
    "SELECT a, b, sum((SELECT max(c) FROM t u WHERE u.a = t.a)) OVER (ORDER BY b) FROM t;",
    "SELECT a, b, sum(c) OVER () + 1, -row_number() OVER (ORDER BY c), abs(rank() OVER (ORDER BY a) - 3) FROM t;",
    "SELECT t.a, u.b, row_number() OVER (ORDER BY t.c, u.c) FROM t JOIN t u ON t.a = u.a;",
    "SELECT value, sum(value) OVER (ORDER BY value ROWS 2 PRECEDING) FROM generate_series(1, 6);",
    "SELECT key, value, rank() OVER (ORDER BY value) FROM json_each('[3,1,2,1]');",
    "SELECT x, row_number() OVER () FROM e;",
    "SELECT c, ntile(0) OVER (ORDER BY c) FROM t;",
    "SELECT c, nth_value(c, 0) OVER (ORDER BY c) FROM t;",
    "SELECT c, lag(c, -9223372036854775808) OVER (ORDER BY c) FROM t;",
    "SELECT a, sum(c) OVER (ORDER BY a RANGE BETWEEN -1 PRECEDING AND CURRENT ROW) FROM t;",
    "SELECT a, sum(abs(-9223372036854775808)) OVER () FROM t;",
    // Review fix: a window query reads every input row, so its LIMIT
    // (or a scalar subquery's one row) bounds no first source — a
    // view, a CTE, a FROM subquery or a table-valued source read only
    // the LIMIT + OFFSET rows.
    "SELECT value, count(*) OVER () FROM json_each('[1,2,3,4]') LIMIT 2;",
    "SELECT key, sum(value) OVER (ORDER BY key DESC) FROM json_each('[5,6,7,8]') LIMIT 3 OFFSET 1;",
    "SELECT id, count(*) OVER () FROM json_tree('[1,[2,3],4]') LIMIT 2;",
    "SELECT x, count(*) OVER () FROM (SELECT value x FROM generate_series(1, 9)) LIMIT 3;",
    "SELECT value, count(*) OVER (), last_value(value) OVER () FROM generate_series(1, 9) LIMIT 2;",
    "SELECT a, count(*) OVER () FROM tv LIMIT 2;",
    "WITH c AS (SELECT a FROM t) SELECT a, count(*) OVER () FROM c LIMIT 2;",
    "WITH c AS MATERIALIZED (SELECT a FROM t) SELECT a, sum(a) OVER () FROM c LIMIT 2 OFFSET 1;",
    "SELECT (SELECT count(*) OVER () FROM (SELECT a FROM t));",
    "SELECT (SELECT count(*) OVER () FROM json_each('[1,2,3]'));",
    "SELECT (SELECT sum(value) OVER () FROM generate_series(1, 5));",
    "SELECT (SELECT last_value(a) OVER () FROM tv);",
    "SELECT a, row_number() OVER (ORDER BY a), count(*) OVER () FROM (SELECT a FROM t ORDER BY a DESC) LIMIT 2;",
    "SELECT x, count(*) OVER () FROM (SELECT a x FROM t UNION ALL SELECT 9) LIMIT 2;",
    "SELECT x, count(*) OVER () FROM (SELECT 1 x UNION ALL SELECT 2) LIMIT 1;",
    "SELECT * FROM (SELECT a, count(*) OVER () n FROM (SELECT a FROM t)) LIMIT 2;",
    "SELECT a, (SELECT count(*) OVER () FROM (SELECT a FROM t u WHERE u.a >= t.a)) FROM t;",
    "SELECT x FROM (SELECT a x, count(*) OVER () n FROM tv) WHERE n = 6 LIMIT 1;",
    "SELECT a, count(*) OVER () FROM (SELECT a FROM t) WHERE a > 0 LIMIT 2;",
    "SELECT a, count(*) OVER () FROM (SELECT a FROM t) LIMIT 0;",
];

#[test]
fn windows_match_the_pin() {
    let base = pin_file("win", W);
    corpus(&base, "w", &strict(), WINDOWS);
}

// ---------------- plain EXPLAIN ----------------

/// Every row of `sql` on hipp, each value as its pin `quote()` form.
fn hipp_quoted(path: &Path, sql: &str) -> Result<Vec<Vec<String>>, Error<FsError>> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, strict()).await.expect("open");
        let mut q = db.prepare(sql).await?;
        let mut rows = Vec::new();
        while let Step::Row = q.step().await? {
            let mut r = Vec::new();
            for i in 0..q.column_count() {
                r.push(match q.column_value(i) {
                    Some(hipp::Value::Null) | None => "NULL".to_string(),
                    Some(hipp::Value::Integer(v)) => v.to_string(),
                    Some(hipp::Value::Text(b)) => format!("'{}'", String::from_utf8_lossy(b)),
                    Some(other) => format!("{other:?}"),
                });
            }
            rows.push(r);
        }
        Ok(rows)
    })
}

/// The pin's EXPLAIN rows of `sql` in `.mode quote` (its explain
/// formatting off), or None when it fails.
fn pin_quoted(path: &Path, sql: &str) -> Option<Vec<Vec<String>>> {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(".explain off")
        .arg(".mode quote")
        .arg(sql)
        .output()
        .expect("oracle runs");
    if !out.status.success() || !out.stderr.is_empty() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(split_quoted)
            .collect(),
    )
}

/// One `.mode quote` line's values (commas inside quotes kept).
fn split_quoted(l: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in l.chars() {
        match ch {
            '\'' => {
                quoted = !quoted;
                cur.push(ch);
            }
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

/// A value's class in an EXPLAIN row: integer, text or NULL.
fn class(v: &str) -> &'static str {
    if v == "NULL" {
        "null"
    } else if v.starts_with('\'') || v.starts_with("unistr(") {
        // (`.mode quote` writes text with control characters as
        // `unistr(...)`.)
        "text"
    } else if v.parse::<i64>().is_ok() {
        "int"
    } else {
        "other"
    }
}

/// The column classes an EXPLAIN row may hold, per the pin: `addr`,
/// `p1` .. `p3`, `p5` integers; `opcode` text; `p4` and `comment` text
/// or NULL.
fn row_shape_ok(r: &[String]) -> bool {
    let want: [&[&str]; 8] = [
        &["int"],
        &["text"],
        &["int"],
        &["int"],
        &["int"],
        &["text", "null"],
        &["int"],
        &["text", "null"],
    ];
    r.len() == 8 && r.iter().zip(want).all(|(v, w)| w.contains(&class(v)))
}

const EXPLAIN_SQL: &[&str] = &[
    "EXPLAIN SELECT 1",
    "EXPLAIN SELECT a FROM t WHERE b > 'x' ORDER BY a",
    "EXPLAIN SELECT a, count(*) FROM t GROUP BY a",
    "EXPLAIN SELECT * FROM t, w WHERE t.a = w.v",
    "EXPLAIN SELECT (SELECT max(a) FROM t), * FROM generate_series(1, 3)",
    "EXPLAIN SELECT a, row_number() OVER (ORDER BY a) FROM t",
    "EXPLAIN SELECT * FROM w WHERE k = 'a'",
    "explain /* c */ SELECT * FROM j, json_each(j.doc)",
    "EXPLAIN INSERT INTO t VALUES (9, 'n')",
    "EXPLAIN UPDATE t SET a = 1",
    "EXPLAIN DELETE FROM t",
    "EXPLAIN CREATE TABLE z(q)",
    "EXPLAIN DROP TABLE t",
    "EXPLAIN PRAGMA user_version",
];

/// Plain EXPLAIN has the pin's shape: eight columns of the pin's
/// classes, row 0 an `Init` whose comment is "Start at N", addresses
/// in order; it runs nothing (the file the pin reads afterwards is
/// unchanged), and the statements the pin refuses are refused.
#[test]
fn explain_has_the_pins_shape() {
    let base = pin_file("explain", S);
    let mut bad = Vec::new();
    for (i, q) in EXPLAIN_SQL.iter().enumerate() {
        let h = copy_of(&base, &format!("vmr17-ex{i}-h"));
        let o = copy_of(&base, &format!("vmr17-ex{i}-o"));
        let pin = pin_quoted(&o, q).expect("the pin explains it");
        assert!(
            pin.iter().all(|r| row_shape_ok(r)),
            "pin shape {q}: {pin:?}"
        );
        let ours = match hipp_quoted(&h, q) {
            Ok(r) => r,
            Err(e) => {
                bad.push(format!("{q}: {e:?}"));
                continue;
            }
        };
        if ours.is_empty() || !ours.iter().all(|r| row_shape_ok(r)) {
            bad.push(format!("{q}: shape {ours:?}"));
            continue;
        }
        for (k, r) in ours.iter().enumerate() {
            if r[0] != k.to_string() {
                bad.push(format!("{q}: addr {k} is {}", r[0]));
            }
        }
        // Nothing ran: the file is as it was.
        let state = "SELECT count(*) FROM t; SELECT group_concat(name) FROM sqlite_schema;";
        assert_eq!(common::cli(&h, state), common::cli(&base, state), "{q} ran");
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    // Errors agree with the pin's.
    for q in [
        "EXPLAIN EXPLAIN SELECT 1",
        "EXPLAIN SELECT * FROM nosuch",
        "EXPLAIN SELECT nosuch FROM t",
        "EXPLAIN",
        "EXPLAIN QUERY PLAN EXPLAIN SELECT 1",
    ] {
        let h = copy_of(&base, "vmr17-exerr-h");
        let o = copy_of(&base, "vmr17-exerr-o");
        assert!(pin_quoted(&o, q).is_none(), "pin accepts {q}");
        assert!(hipp_quoted(&h, q).is_err(), "hipp accepts {q}");
    }
}

/// hipp's own listing, golden: the program of a scan with a filter
/// and of a statement on today's executor (`Host`).
#[test]
fn explain_lists_the_program() {
    let base = pin_file("explain-golden", S);
    let h = copy_of(&base, "vmr17-exg-h");
    let got = hipp_quoted(&h, "EXPLAIN SELECT a FROM t WHERE b > 'x'").expect("explain");
    let text: Vec<String> = got.iter().map(|r| r.join(",")).collect();
    assert_eq!(text, GOLDEN_SCAN, "{text:#?}");
    let got = hipp_quoted(&h, "EXPLAIN DELETE FROM t").expect("explain");
    let text: Vec<String> = got.iter().map(|r| r.join(",")).collect();
    assert_eq!(text, ["0,'Host',2,0,0,NULL,0,NULL"]);
}

const GOLDEN_SCAN: &[&str] = &[
    "0,'Init',0,11,0,NULL,0,'Start at 11'",
    "1,'Eval',0,0,0,'expr',2,NULL",
    "2,'IfNot',0,10,0,NULL,0,NULL",
    "3,'OpenRead',0,0,0,'table root=2 ncols=2',0,NULL",
    "4,'Rewind',0,10,0,NULL,0,NULL",
    "5,'Eval',1,0,0,'expr',1,NULL",
    "6,'IfNot',0,9,0,NULL,0,NULL",
    "7,'Eval',1,1,0,'expr',0,NULL",
    "8,'ResultRow',1,1,1,NULL,0,NULL",
    "9,'Next',0,5,0,NULL,0,NULL",
    "10,'Halt',0,0,0,NULL,0,NULL",
    "11,'Goto',0,1,0,NULL,0,NULL",
];

// ---------------- restart at the smallest cache ----------------

/// Every corpus again with the smallest page cache the engine allows
/// (one frame) and with two: nearly every opcode misses and runs
/// again after the read, inside window partitions, host cursors and
/// the driver-awaited readers alike.
#[test]
fn restart_with_the_smallest_cache() {
    for pages in [1, 2] {
        let cfg = strict().cache_pages(pages);
        let base = pin_file(&format!("rs-src{pages}"), S);
        corpus(&base, &format!("rs{pages}"), &cfg, SOURCES);
        let base = pin_file(&format!("rs-win{pages}"), W);
        corpus(&base, &format!("rw{pages}"), &cfg, WINDOWS);
    }
}

// ---------------- TEMP and attached tables, triggers ----------------

/// `setup` then `query` on one hipp connection under `cfg`: the
/// query's run and whether it ran on the VM.
fn hipp_script(path: &Path, cfg: Config, setup: &[&str], query: &str) -> (Run, bool) {
    let cfg_strict = cfg.vm_strict;
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, cfg).await.expect("open");
        for s in setup {
            let mut q = db.prepare(s).await.expect(s);
            while q.step().await.expect(s) == Step::Row {}
        }
        let mut q = match db.prepare(query).await {
            Ok(q) => q,
            Err(_) => {
                return (
                    Run {
                        ok: false,
                        rows: Vec::new(),
                    },
                    true,
                );
            }
        };
        // A statement naming only TEMP (or attached) tables runs on
        // that database's connection, which has this one's `Config`:
        // strict there, a fallback would be its `VmFallback`.
        let mut rows = Vec::new();
        loop {
            match q.step().await {
                Ok(Step::Row) => rows.push(render(&q)),
                Ok(Step::Done) => {
                    let v = q.runs_on_vm() || cfg_strict;
                    return (Run { ok: true, rows }, v);
                }
                Err(e) => {
                    let v = q.runs_on_vm() || (cfg_strict && !matches!(e, Error::VmFallback));
                    return (Run { ok: false, rows }, v);
                }
            }
        }
    })
}

/// Each query after `setup` (one connection) on hipp, strict, and on
/// the pin (one script): the same rows and success, on the VM.
fn script_corpus(base: &Path, tag: &str, cfg: &Config, setup: &[&str], qs: &[&str]) {
    let mut bad = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let h = copy_of(base, &format!("vmr17-{tag}{i}-h"));
        let o = copy_of(base, &format!("vmr17-{tag}{i}-o"));
        let (hr, on_vm) = hipp_script(&h, cfg.clone(), setup, q);
        let mut script = String::new();
        for s in setup {
            script.push_str(s);
            script.push_str(";\n");
        }
        script.push_str(q);
        let or = pin_run(&o, &script);
        if !on_vm {
            bad.push(format!("not on the VM: {q}"));
        } else if hr != or {
            bad.push(format!("{q}\n  pin : {or:?}\n  hipp: {hr:?}"));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

const TEMP_SETUP: &[&str] = &[
    "CREATE TEMP TABLE tt(x, y)",
    "INSERT INTO tt VALUES (1, 'one'), (2, 'two'), (NULL, 'none'), (2, 'deux')",
    "CREATE TEMP TABLE tw(k PRIMARY KEY, v) WITHOUT ROWID",
    "INSERT INTO tw VALUES ('q', 1), ('p', 2)",
    "ATTACH ':memory:' AS aux",
    "CREATE TABLE aux.at(m, n)",
    "INSERT INTO aux.at VALUES (1, 'a1'), (3, 'a3'), (2, 'a2')",
];

const TEMP_Q: &[&str] = &[
    "SELECT a, y FROM t, tt WHERE tt.x = t.a ORDER BY 1, 2;",
    "SELECT * FROM t LEFT JOIN tt ON tt.x = t.a ORDER BY 1, 2, 3, 4;",
    "SELECT x, count(*) FROM tt, t GROUP BY x;",
    "SELECT a, (SELECT group_concat(y) FROM tt WHERE x = a) FROM t;",
    "SELECT * FROM t WHERE a IN (SELECT x FROM tt);",
    "SELECT * FROM tw ORDER BY k;",
    "SELECT a, n FROM t JOIN aux.at ON m = a ORDER BY a;",
    "SELECT m, n, k FROM aux.at, w WHERE w.v = m ORDER BY m;",
    "SELECT n FROM aux.at UNION SELECT y FROM tt ORDER BY 1;",
    "SELECT a, row_number() OVER (PARTITION BY a ORDER BY y) FROM t, tt WHERE x = a;",
    "SELECT * FROM t RIGHT JOIN tt ON tt.x = t.a;",
];

#[test]
fn temp_and_attached_tables_match_the_pin() {
    let base = pin_file("temp", S);
    script_corpus(&base, "tmp", &strict(), TEMP_SETUP, TEMP_Q);
    script_corpus(&base, "tmp1", &strict().cache_pages(1), TEMP_SETUP, TEMP_Q);
    // Where today's executor is not the pin's, the VM is today's: a
    // TEMP table's rows in a LEFT JOIN come in its scan order (the
    // pin's automatic index orders them by key), and a main table
    // joined to a TEMP WITHOUT ROWID table is refused.
    for q in [
        "SELECT * FROM t LEFT JOIN tt ON tt.x = t.a;",
        "SELECT * FROM t, tw WHERE tw.v = t.a;",
    ] {
        let h = copy_of(&base, "vmr17-tmpd-h");
        let (vm, on_vm) = hipp_script(&h, strict(), TEMP_SETUP, q);
        let h = copy_of(&base, "vmr17-tmpd-t");
        let (today, _) = hipp_script(&h, Config::default().vm_off(true), TEMP_SETUP, q);
        assert!(on_vm, "{q}");
        assert_eq!(vm, today, "{q}");
    }
}

/// Trigger programs and foreign-key actions run their SELECTs (the
/// WHEN clause's `SELECT 1 WHERE ...` too) on the VM: strict, a
/// fallback would fail the write. The table states after each write
/// are the pin's.
#[test]
fn trigger_and_action_selects_run_on_the_vm() {
    let setup = "CREATE TABLE log(m); \
        CREATE TABLE p(id INTEGER PRIMARY KEY, n); \
        CREATE TABLE c(pid REFERENCES p(id) ON DELETE CASCADE ON UPDATE SET NULL, v); \
        INSERT INTO p VALUES (1, 10), (2, 20), (3, 30); \
        INSERT INTO c VALUES (1, 'a'), (2, 'b'), (2, 'c'); \
        CREATE TRIGGER pi AFTER INSERT ON p WHEN new.n > 5 BEGIN \
          SELECT RAISE(ABORT, 'too big') WHERE new.n > 1000; \
          SELECT value FROM generate_series(1, new.n % 4); \
          SELECT count(*) OVER (), key FROM json_each('[1,2]'); \
          INSERT INTO log SELECT 'p' || new.id || ':' || (SELECT count(*) FROM c WHERE pid = new.id); \
        END; \
        CREATE TRIGGER cd AFTER DELETE ON c BEGIN \
          SELECT CASE WHEN old.v = 'z' THEN RAISE(IGNORE) END; \
          INSERT INTO log SELECT 'c' || old.v || ':' || (SELECT group_concat(v) FROM c ORDER BY v); \
        END; \
        CREATE TRIGGER cu AFTER UPDATE ON c WHEN (SELECT count(*) FROM p) > 0 BEGIN \
          INSERT INTO log VALUES ('u' || coalesce(new.pid, 'null')); \
        END;";
    let base = pin_file("trig", setup);
    let writes = [
        "PRAGMA foreign_keys = ON",
        "INSERT INTO p VALUES (4, 7)",
        "INSERT INTO p VALUES (5, 2000)",
        "DELETE FROM p WHERE id = 2",
        "UPDATE p SET id = 9 WHERE id = 1",
        "INSERT INTO c VALUES (3, 'z')",
        "DELETE FROM c WHERE v = 'z'",
    ];
    let state = "SELECT * FROM log ORDER BY rowid; SELECT * FROM c ORDER BY rowid; SELECT * FROM p ORDER BY id;";
    for cfg in [strict(), strict().cache_pages(1)] {
        let h = copy_of(&base, "vmr17-trig-h");
        let o = copy_of(&base, "vmr17-trig-o");
        let mut hres = Vec::new();
        block_on(async {
            let fs = FileStorage::open(&h).expect("fs open");
            let mut db = Connection::open(fs, cfg.clone()).await.expect("open");
            for w in writes {
                let r = async {
                    let mut q = db.prepare(w).await?;
                    while q.step().await? == Step::Row {}
                    Ok::<(), Error<FsError>>(())
                }
                .await;
                assert!(
                    !matches!(r, Err(Error::VmFallback)),
                    "{w}: a program's SELECT fell back"
                );
                hres.push(r.is_ok());
            }
        });
        // The pin, one connection per write (foreign keys on in each).
        let mut pres = Vec::new();
        for w in writes {
            pres.push(pin_run(&o, &format!("PRAGMA foreign_keys = ON; {w};")).ok);
        }
        assert_eq!(hres, pres, "successes differ");
        assert_eq!(
            common::cli(&h, state),
            common::cli(&o, state),
            "states differ"
        );
    }
}

// ---------------- streamed virtual-table limits ----------------

/// `Config::max_vtab_rows` as today's streamed executor applies it: a
/// generate_series cursor read by a query that keeps every row
/// (DISTINCT, GROUP BY, an ORDER BY without LIMIT, windows) fails past
/// the bound; one read row by row (a plain scan, an ORDER BY with a
/// LIMIT, a fold) does not. The VM and today's executor agree.
#[test]
fn max_vtab_rows_as_today() {
    let base = pin_file("cap", S);
    for q in [
        "SELECT value FROM generate_series(1, 100);",
        "SELECT value FROM generate_series(1) LIMIT 50;",
        "SELECT count(*), sum(value) FROM generate_series(1, 100);",
        "SELECT DISTINCT value FROM generate_series(1, 100);",
        "SELECT value % 3, count(*) FROM generate_series(1, 100) GROUP BY 1;",
        "SELECT value FROM generate_series(1, 100) ORDER BY value % 7;",
        "SELECT value FROM generate_series(1, 100) ORDER BY value % 7 LIMIT 3;",
        "SELECT value FROM generate_series(1, 100) ORDER BY value % 7 LIMIT -1;",
        "SELECT value, row_number() OVER () FROM generate_series(1, 100);",
        "SELECT a, value FROM t, generate_series(1, 30) WHERE value = a;",
        "SELECT DISTINCT a FROM t, generate_series(1, 30);",
    ] {
        let h = copy_of(&base, "vmr17-cap-h");
        let cap = |c: Config| {
            let mut c = c;
            c.max_vtab_rows = 40;
            c
        };
        let (vm, on_vm, _) = hipp_run(&h, cap(strict()), q);
        let (today, _, _) = hipp_run(&h, cap(Config::default().vm_off(true)), q);
        assert!(on_vm, "{q}");
        assert_eq!(vm, today, "{q}");
    }
    // Inside a subquery today's materialising executor reads the
    // cursor whole against the bound (a fold included); so does the VM
    // (review fix: it read it row by row, unbounded where a row-keeping
    // construct around it held the rows).
    let q = "SELECT (SELECT count(*) FROM generate_series(1, 100));";
    let h = copy_of(&base, "vmr17-cap-s");
    let mut c = strict();
    c.max_vtab_rows = 40;
    let (vm, on_vm, e) = hipp_run(&h, c, q);
    assert!(on_vm);
    assert!(
        !vm.ok && matches!(e, Some(Error::Schema { .. })),
        "bounded as today: {vm:?} {e:?}"
    );
}

/// Review fix: `Config::max_vtab_rows` where a construct around the
/// cursor keeps its rows — a materialised CTE, a compound's cores, an
/// IN list, a derived table, a scalar or EXISTS subquery, a RIGHT
/// JOIN, a window query, a recursive CTE's step. Today's materialising
/// executor reads such a cursor whole (under the reader's bound) and
/// fails past the bound; the VM did not count those rows (memory grew
/// with the series). Now the VM and today's executor agree, row for
/// row and error for error, at a bound of 40 rows.
#[test]
fn max_vtab_rows_where_rows_are_kept() {
    let base = pin_file("capk", S);
    let mut bad = Vec::new();
    let cap = |c: Config| {
        let mut c = c;
        c.max_vtab_rows = 40;
        c
    };
    for q in [
        "WITH c AS MATERIALIZED (SELECT value FROM generate_series(1,100)) SELECT count(*) FROM c;",
        "WITH c AS (SELECT value FROM generate_series(1,100)) SELECT count(*) FROM c;",
        "WITH c AS MATERIALIZED (SELECT value FROM generate_series(1,100)) SELECT * FROM c LIMIT 2;",
        "SELECT value FROM generate_series(1,100) UNION SELECT 1 LIMIT 1;",
        "SELECT value FROM generate_series(1,100) UNION ALL SELECT 1 ORDER BY 1 LIMIT 1;",
        "SELECT value FROM generate_series(1,100) UNION ALL SELECT 1 LIMIT 1;",
        "SELECT value FROM generate_series(1,100) UNION ALL SELECT 1;",
        "SELECT value FROM generate_series(1,100) INTERSECT SELECT 3;",
        "SELECT * FROM generate_series(1,3) WHERE value IN (SELECT value FROM generate_series(1,100));",
        "SELECT a FROM t WHERE a IN (SELECT value FROM generate_series(1,100) WHERE value < 3);",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100)) ORDER BY x DESC LIMIT 1;",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100)) ORDER BY x LIMIT 1;",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100)) LIMIT 1;",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100) WHERE value > 2) LIMIT 1;",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100)) WHERE x > 2 LIMIT 1;",
        "SELECT count(*) FROM (SELECT value FROM generate_series(1,100));",
        "SELECT * FROM (SELECT value FROM generate_series(1,100) LIMIT 5);",
        "SELECT * FROM (SELECT value FROM generate_series(1,100) UNION ALL SELECT 0) LIMIT 2;",
        "SELECT * FROM t, (SELECT value FROM generate_series(1,100)) g WHERE g.value = t.a;",
        "SELECT (SELECT value FROM generate_series(1,100) ORDER BY value+0 DESC LIMIT 1);",
        "SELECT (SELECT value FROM generate_series(1,100));",
        "SELECT (SELECT value FROM generate_series(1,100) WHERE value > 50);",
        "SELECT (SELECT count(*) FROM generate_series(1,100));",
        "SELECT (SELECT max(value) FROM generate_series(1,100));",
        "SELECT EXISTS (SELECT 1 FROM generate_series(1,100));",
        "SELECT EXISTS (SELECT 1 FROM generate_series(1,100) WHERE value > 50);",
        "SELECT a, (SELECT value FROM generate_series(1,100) WHERE value = t.a) FROM t;",
        "SELECT a, (SELECT count(*) FROM generate_series(1,t.a)) FROM t;",
        "SELECT value FROM generate_series(1,100) RIGHT JOIN t ON value = a;",
        "SELECT a, value FROM t LEFT JOIN generate_series(1,100) ON value = a;",
        "SELECT a, value FROM generate_series(1,100), t WHERE value = a;",
        "SELECT a, value FROM t, generate_series(1,100) WHERE value = a ORDER BY 1;",
        "SELECT value, sum(value) OVER () FROM generate_series(1,100) LIMIT 1;",
        "SELECT * FROM (SELECT value, row_number() OVER () FROM generate_series(1,100)) LIMIT 1;",
        "SELECT * FROM (SELECT DISTINCT value FROM generate_series(1,100)) LIMIT 1;",
        "SELECT * FROM (SELECT value FROM generate_series(1,100) ORDER BY value % 7 LIMIT 3);",
        "SELECT * FROM (SELECT value FROM generate_series(1,100) ORDER BY value % 7);",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n, (SELECT count(*) FROM generate_series(1, 100)) FROM r;",
        "SELECT value FROM generate_series(1,30) UNION SELECT 1;",
        "SELECT (SELECT group_concat(value) FROM generate_series(1,30));",
        "SELECT x FROM (SELECT value x FROM generate_series(1,100)) UNION ALL SELECT 1 LIMIT 1;",
        "SELECT a, (SELECT count(*) FROM generate_series(1, t.a * 20)) FROM t;",
        "SELECT a, (SELECT max(value) FROM generate_series(1, t.a * 20)) FROM t WHERE a < 2;",
        "SELECT (SELECT value FROM generate_series(1,100) WHERE value % 2 = 0 LIMIT 1);",
        "SELECT EXISTS (SELECT 1 FROM generate_series(1,100) WHERE value % 10 = 0);",
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM generate_series(1,100) WHERE value = t.a);",
        "SELECT (SELECT count(*) FROM t, generate_series(1,100));",
        "SELECT (SELECT count(*) FROM t, generate_series(1,30));",
        "SELECT * FROM t WHERE a IN (SELECT value FROM generate_series(1,100) LIMIT 2);",
        "SELECT * FROM (SELECT value, sum(value) OVER () FROM generate_series(1,30)) LIMIT 2;",
        "SELECT * FROM (VALUES (1), (2)) v, generate_series(1, 100) g WHERE g.value = v.column1;",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r, generate_series(1, 50) g WHERE g.value = 1 AND n < 3) SELECT * FROM r;",
        "SELECT j.id, (SELECT count(*) FROM json_each(j.doc), generate_series(1, 100)) FROM j;",
        "SELECT a FROM t UNION SELECT value FROM generate_series(1, 30) ORDER BY 1 LIMIT 3;",
        "SELECT value FROM generate_series(1, 100) EXCEPT SELECT a FROM t LIMIT 1;",
    ] {
        let h = copy_of(&base, "vmr17-capk-h");
        let (vm, on_vm, e1) = hipp_run(&h, cap(strict()), q);
        let (today, _, e2) = hipp_run(&h, cap(Config::default().vm_off(true)), q);
        if !on_vm || vm != today {
            bad.push(format!(
                "{q}\n  vm   : {vm:?} {e1:?}\n  today: {today:?} {e2:?}"
            ));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
    // A UNION ALL chain streams its cores on the VM, as on the pin: the
    // first core's row comes before the second core's error, where
    // today's executor computes every core first. Both fail.
    let q = "SELECT 1 UNION ALL SELECT value FROM generate_series(1, 100) LIMIT 3;";
    let h = copy_of(&base, "vmr17-capk-u");
    let (vm, on_vm, _) = hipp_run(&h, cap(strict()), q);
    let (today, _, _) = hipp_run(&h, cap(Config::default().vm_off(true)), q);
    assert!(on_vm && !vm.ok && !today.ok, "{vm:?} {today:?}");
    assert_eq!(vm.rows, ["1"]);
    // The reviewer's shapes at the default bound (4,194,304 rows):
    // each fails as today's executor fails, and none holds the rows
    // past the bound.
    for q in [
        "WITH c AS MATERIALIZED (SELECT value FROM generate_series(1, 4194305)) SELECT count(*) FROM c;",
        "SELECT value FROM generate_series(1, 4194305) UNION SELECT 1 LIMIT 1;",
        "SELECT * FROM generate_series(1, 3) WHERE value IN (SELECT value FROM generate_series(1, 4194305));",
        "SELECT (SELECT value FROM generate_series(1, 4194305) ORDER BY value + 0 DESC LIMIT 1);",
    ] {
        let h = copy_of(&base, "vmr17-capk-d");
        let (vm, on_vm, e) = hipp_run(&h, strict(), q);
        assert!(on_vm, "{q}");
        assert!(
            !vm.ok && matches!(e, Some(Error::Schema { .. })),
            "{q}: {vm:?} {e:?}"
        );
    }
}

/// A trigger program's SELECTs are today's materialising executor's:
/// a generate_series cursor there is read whole under
/// `Config::max_vtab_rows`, on the VM too.
#[test]
fn max_vtab_rows_in_trigger_programs() {
    let setup = "CREATE TABLE p(n); CREATE TABLE log(m); \
        CREATE TRIGGER pi AFTER INSERT ON p BEGIN \
          SELECT value FROM generate_series(1, new.n) LIMIT 1; \
          SELECT count(*) FROM generate_series(1, new.n); \
          INSERT INTO log VALUES (new.n); \
        END; \
        CREATE TABLE q(n); \
        CREATE TRIGGER qi AFTER INSERT ON q \
          WHEN (SELECT count(*) FROM generate_series(1, new.n)) > 0 BEGIN \
          INSERT INTO log VALUES (-new.n); \
        END;";
    let base = pin_file("capt", setup);
    for (t, n) in [10, 40, 41, 100]
        .iter()
        .flat_map(|n| [("p", *n), ("q", *n)])
    {
        let w = format!("INSERT INTO {t} VALUES ({n})");
        let mut res = Vec::new();
        for cfg in [strict(), Config::default().vm_off(true)] {
            let h = copy_of(&base, "vmr17-capt-h");
            let mut c = cfg;
            c.max_vtab_rows = 40;
            let r = block_on(async {
                let fs = FileStorage::open(&h).expect("fs open");
                let mut db = Connection::open(fs, c).await.expect("open");
                let r = async {
                    let mut q = db.prepare(&w).await?;
                    while q.step().await? == Step::Row {}
                    Ok::<(), Error<FsError>>(())
                }
                .await;
                assert!(!matches!(r, Err(Error::VmFallback)), "{w}");
                r.is_ok()
            });
            res.push((r, common::cli(&h, "SELECT * FROM log;")));
        }
        assert_eq!(res[0], res[1], "{w}");
        assert_eq!(res[0].0, n <= 40, "{w}");
    }
}

// ---------------- cancellation ----------------

mod cancel {
    use super::*;
    use hipp::storage::{MemStorage, Sidecar, Storage};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop_waker() -> Waker {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: the vtable's functions ignore the data pointer.
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
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

    /// `sql`'s rows, each step future polled once and dropped `drops`
    /// times before it is driven on; and how many were dropped while
    /// pending.
    fn rows_with_drops(
        mem: &MemStorage,
        cfg: Config,
        sql: &str,
        drops: usize,
    ) -> (Vec<String>, usize) {
        let st = SlowReads(mem.clone());
        let mut db = block_on(Connection::open(st, cfg)).expect("open");
        let mut q = block_on(db.prepare(sql)).expect("prepare");
        let mut rows = Vec::new();
        let mut dropped = 0usize;
        let w = noop_waker();
        let mut cx = Context::from_waker(&w);
        loop {
            for _ in 0..drops {
                let mut f = Box::pin(q.step());
                match f.as_mut().poll(&mut cx) {
                    Poll::Pending => dropped += 1,
                    Poll::Ready(r) => {
                        drop(f);
                        match r.expect("step") {
                            Step::Row => {
                                rows.push(render(&q));
                                continue;
                            }
                            Step::Done => {
                                assert!(q.runs_on_vm(), "{sql}");
                                return (rows, dropped);
                            }
                        }
                    }
                }
            }
            match block_on(q.step()).expect("step") {
                Step::Row => rows.push(render(&q)),
                Step::Done => {
                    assert!(q.runs_on_vm(), "{sql}");
                    return (rows, dropped);
                }
            }
        }
    }

    /// Dropped step futures inside window partitions, host cursors (a
    /// WITHOUT ROWID scan, an R*Tree search, generate_series) and the
    /// driver-awaited readers (json_each, fts5, a WITHOUT ROWID access
    /// path): the next step gives exactly the uninterrupted rows.
    #[test]
    fn dropped_step_futures_resume_exactly() {
        let base = pin_file("cancel", S);
        let bytes = std::fs::read(&base).expect("read");
        let mem = MemStorage::new();
        let mut m = mem.clone();
        block_on(async {
            m.write_at(0, &bytes).await.expect("write");
        });
        for sql in [
            "SELECT * FROM w;",
            "SELECT k, a FROM w, t WHERE t.a = w.v;",
            "SELECT * FROM w WHERE v > 1;",
            "SELECT id FROM rt WHERE x0 > 4;",
            "SELECT a, id FROM t, rt WHERE rt.x0 <= a * 5;",
            "SELECT a, value FROM t, generate_series(1, a);",
            "SELECT j.id, key, value FROM j, json_each(j.doc);",
            "SELECT rowid, body FROM ft WHERE ft MATCH 'alpha';",
            "SELECT a, b, row_number() OVER (PARTITION BY a ORDER BY b) FROM t;",
            "SELECT name FROM sqlite_schema ORDER BY name;",
        ] {
            let want = pin_run(&base, sql).rows;
            for (pages, drops) in [(16, 1), (1, 1), (1, 3), (2, 2)] {
                let (got, _) = rows_with_drops(&mem, strict().cache_pages(pages), sql, drops);
                assert_eq!(got, want, "{sql} (cache {pages}, drops {drops})");
            }
        }
    }
}

// ---------------- generated corpora ----------------

/// A deterministic xorshift generator (tests/fuzz_smoke.rs's).
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
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn seeds(default: u64) -> u64 {
    std::env::var("HIPP_VMR_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Generated window queries (functions, partitions, orders, ROWS /
/// RANGE / GROUPS frames with EXCLUDE, NULLs, grouped inputs, WHERE,
/// outer ORDER BY and LIMIT), strict on the VM, against the pin: the
/// same rows in the same order, and errors where the pin errs.
#[test]
fn generated_windows_match_the_pin() {
    let base = pin_file("gen-win", W);
    let funcs = [
        "row_number()",
        "rank()",
        "dense_rank()",
        "percent_rank()",
        "cume_dist()",
        "ntile(3)",
        "lag(c)",
        "lead(c, 2, 0)",
        "first_value(b)",
        "last_value(b)",
        "nth_value(c, 2)",
        "sum(c)",
        "count(*)",
        "avg(c)",
        "min(b)",
        "max(n)",
        "group_concat(b, '.')",
        "total(a)",
        "count(n)",
        "sum(c) FILTER (WHERE a > 0)",
    ];
    let parts = ["", "PARTITION BY a", "PARTITION BY n", "PARTITION BY a % 2"];
    let orders = [
        "",
        "ORDER BY c",
        "ORDER BY a DESC",
        "ORDER BY n NULLS LAST",
        "ORDER BY a, c DESC",
        "ORDER BY b",
    ];
    let frames = [
        "",
        "ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        "ROWS UNBOUNDED PRECEDING",
        "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
        "GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW",
        "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        "ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING EXCLUDE CURRENT ROW",
        "GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING EXCLUDE TIES",
        "ROWS BETWEEN 1 FOLLOWING AND 3 FOLLOWING EXCLUDE GROUP",
    ];
    let wheres = [
        "",
        " WHERE c > 1",
        " WHERE a IS NOT NULL",
        " WHERE n <> 'a'",
    ];
    let tails = [
        "",
        " ORDER BY 1, 2",
        " ORDER BY 2 DESC, 1 LIMIT 3",
        " LIMIT 2 OFFSET 1",
        " ORDER BY a, c",
    ];
    let mut rng = Rng(0x5eed_0bad_cafe_0001);
    let mut qs = Vec::new();
    for _ in 0..seeds(120) {
        let f = rng.pick(&funcs);
        let p = rng.pick(&parts);
        let o = rng.pick(&orders);
        let fr = if o.is_empty() { "" } else { rng.pick(&frames) };
        let f2 = rng.pick(&funcs);
        let o2 = rng.pick(&orders);
        let grouped = rng.next().is_multiple_of(5);
        let q = if grouped {
            format!(
                "SELECT a, count(*), sum(count(*)) OVER (ORDER BY a {}), rank() OVER (ORDER BY count(*) {}) FROM t{} GROUP BY a{};",
                if rng.next().is_multiple_of(2) {
                    "ROWS 1 PRECEDING"
                } else {
                    ""
                },
                if rng.next().is_multiple_of(2) {
                    "DESC"
                } else {
                    ""
                },
                rng.pick(&wheres),
                rng.pick(&tails),
            )
        } else {
            format!(
                "SELECT a, c, {f} OVER ({p} {o} {fr}), {f2} OVER ({o2}) FROM t{}{};",
                rng.pick(&wheres),
                rng.pick(&tails)
            )
        };
        qs.push(q);
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    corpus(&base, "gw", &strict(), &refs);
}

/// Generated window queries whose only source is a view, a CTE, a FROM
/// subquery (plain, ordered, compound, joined) or a table-valued
/// source, under a LIMIT / OFFSET or as a scalar subquery: the windows
/// see every input row (review fix: the reader's bound reached the
/// source), strict on the VM, against the pin.
#[test]
fn generated_windows_over_derived_sources_match_the_pin() {
    let base = pin_file("gen-wd", W);
    let funcs = [
        "count(*)",
        "row_number()",
        "rank()",
        "ntile(2)",
        "sum(c)",
        "last_value(c)",
        "first_value(c)",
        "lead(c)",
        "lag(c, 2)",
        "max(c)",
        "cume_dist()",
        "group_concat(c, '.')",
    ];
    let overs = [
        "",
        "ORDER BY c",
        "PARTITION BY a",
        "ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        "PARTITION BY a ORDER BY c DESC",
        "ORDER BY a GROUPS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
    ];
    let srcs = [
        "tv",
        "(SELECT a, c FROM t)",
        "(SELECT a, c FROM t ORDER BY c DESC)",
        "(SELECT a, c FROM t UNION ALL SELECT 7, 9)",
        "(SELECT 1 a, 2 c UNION ALL SELECT 3, 4 UNION ALL SELECT 1, 6)",
        "(SELECT t.a, t.c FROM t JOIN e ON 1 UNION ALL SELECT a, c FROM t)",
        "(SELECT value % 3 a, value c FROM generate_series(1, 7))",
        "(SELECT key a, value c FROM json_each('[4,1,3,1,5]'))",
        "cte",
        "mcte",
    ];
    let tails = [
        " LIMIT 1",
        " LIMIT 2",
        " LIMIT 4 OFFSET 1",
        " LIMIT 3 OFFSET 2",
        " LIMIT 2 OFFSET 9",
    ];
    let mut rng = Rng(0x5eed_d0e5_0002_0003);
    let mut qs = Vec::new();
    for _ in 0..seeds(120) {
        let f = rng.pick(&funcs);
        let o = rng.pick(&overs);
        let src = rng.pick(&srcs);
        let (with, from) = match src {
            "cte" => ("WITH x AS (SELECT a, c FROM t) ", "x"),
            "mcte" => ("WITH x AS MATERIALIZED (SELECT a, c FROM t) ", "x"),
            s => ("", s),
        };
        let tail = rng.pick(&tails);
        let q = if rng.next().is_multiple_of(4) {
            format!(
                "{with}SELECT (SELECT {f} OVER ({o}) FROM {from}), (SELECT count(*) OVER () FROM {from}{tail});"
            )
        } else {
            format!("{with}SELECT a, c, {f} OVER ({o}) FROM {from}{tail};")
        };
        qs.push(q);
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    corpus(&base, "gwd", &strict(), &refs);
}

/// Generated queries over virtual and table-valued sources joined to
/// tables (generate_series per outer row, json_each / json_tree of a
/// column, an R*Tree search, a WITHOUT ROWID table, fts5 MATCH), with
/// WHERE, aggregates, DISTINCT, ORDER BY and LIMIT, strict on the VM,
/// against the pin.
#[test]
fn generated_sources_match_the_pin() {
    let base = pin_file("gen-src", S);
    let froms = [
        "t, generate_series(1, 3) AS g",
        "t JOIN generate_series(0, t.a) AS g",
        "generate_series(1, 4) AS g, t",
        "t LEFT JOIN generate_series(1, t.a) AS g ON g.value > 1",
        "j, json_each(j.doc) AS g",
        "j, json_tree(j.doc) AS g",
        "j LEFT JOIN json_each(j.doc) AS g",
        "t, w AS g",
        "w AS g, t",
        "t, rt AS g",
    ];
    let cols = ["g.value", "g.key", "g.v", "g.id", "g.k"];
    let wheres = ["", " WHERE t.a > 1", " WHERE t.a IS NULL OR t.a < 3", ""];
    let shapes = [
        "SELECT t.a, {c} FROM {f}{w}",
        "SELECT count(*), max(t.a) FROM {f}{w}",
        "SELECT DISTINCT t.a FROM {f}{w}",
        "SELECT t.a, count(*) FROM {f}{w} GROUP BY t.a",
        "SELECT t.a, {c} FROM {f}{w} ORDER BY 2, 1 LIMIT 4",
        "SELECT t.a, {c} FROM {f}{w} LIMIT 3",
    ];
    let mut rng = Rng(0x0dd5_eed5_1234_5678);
    let mut qs = Vec::new();
    for _ in 0..seeds(120) {
        let f = rng.pick(&froms);
        let f = if f.starts_with('j') {
            f.replace("t.a", "j.id")
        } else {
            f.to_string()
        };
        // Only the columns its source has.
        let c = match () {
            _ if f.contains("json_") => rng.pick(&["g.value", "g.key", "g.type"]),
            _ if f.contains(" w ") || f.starts_with("w ") => "g.k",
            _ if f.contains("rt AS g") => "g.id",
            _ => cols[0],
        };
        let s = rng.pick(&shapes);
        let w = rng.pick(&wheres);
        let mut q = s.replace("{c}", c).replace("{f}", &f).replace("{w}", w);
        if f.starts_with('j') {
            q = q.replace("t.a", "j.id");
        }
        q.push(';');
        qs.push(q);
    }
    let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
    corpus(&base, "gs", &strict(), &refs);
}
