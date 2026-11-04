//! v0.16 auto-vacuum files (leaf `autovacuum`), differentially vs
//! the pinned oracle (SQLite 3.54.1, live). Probed on the pin before
//! implementation; the matrix the tests encode:
//!
//! - Pointer-map pages at `2 + k * (usable / 5 + 1)` (the lock-byte
//!   page skipped) hold a 5-byte (role, parent) entry per page:
//!   ROOTPAGE (1, 0), FREEPAGE (2, 0), OVERFLOW1 (3, cell's page),
//!   OVERFLOW2 (4, previous overflow page), BTREE (5, parent page).
//!   Header offset 52 is the largest root page (1 with no table),
//!   offset 64 is 1 for INCREMENTAL.
//! - CREATE TABLE / INDEX put the root after the largest root
//!   (skipping pointer-map pages), moving the page there; DROP moves
//!   the largest root into each freed root slot, largest root
//!   dropped first, and rewrites its `sqlite_schema.rootpage`.
//! - FULL: every commit moves pages from the end into the free
//!   pages and truncates (freelist_count 0 after each commit).
//!   INCREMENTAL: commits keep the free pages; `PRAGMA
//!   incremental_vacuum(N)` removes N pages from the end (all for
//!   no N, N <= 0, or N not a 32-bit integer by `sqlite3GetInt32`),
//!   one empty result row per page removed.
//! - `PRAGMA auto_vacuum = X` on an empty database writes its page 1
//!   at once (the page size is then fixed); FULL <-> INCREMENTAL
//!   switch at once on an auto-vacuum file; to or from NONE waits for
//!   VACUUM, which (VACUUM INTO too) keeps the file's mode otherwise.
//!   The mode set on an empty database survives a ROLLBACK, and the
//!   next write makes the file auto-vacuum.
//!
//! Every script runs on both engines over one connection (continuing
//! past errors); rows and failing statements must agree, and so must
//! the files' page counts, pointer maps and root pages where the
//! pin's page choice is deterministic. The pin's integrity check
//! passes on hipp's files and hipp's on the pin's.

#![cfg(feature = "autovacuum")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};
use std::path::{Path, PathBuf};

fn render<S: hipp::Storage>(q: &hipp::Statement<'_, S>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut t = hipp::arena::AVec::new();
                hipp::record::render_real(f, &mut t);
                line.push_str(&String::from_utf8_lossy(t.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
}

/// Run `stmts` through hipp on one connection: rows, and the
/// 1-based numbers of the statements that failed.
fn hipp_run(path: &Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut lines = Vec::new();
        let mut errs = Vec::new();
        for (i, sql) in stmts.iter().enumerate() {
            let mut q = match db.prepare(sql).await {
                Ok(q) => q,
                Err(_) => {
                    errs.push(i + 1);
                    continue;
                }
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        // A value with newlines prints as several
                        // lines in the pin's shell.
                        let mut l = String::new();
                        render(&q, &mut l);
                        lines.extend(l.split('\n').map(str::to_string));
                    }
                    Ok(Step::Done) => break,
                    Err(_) => {
                        errs.push(i + 1);
                        break;
                    }
                }
            }
        }
        db.close().await.expect("close");
        (lines, errs)
    })
}

/// The pin's shell over stdin, one statement per line: rows, and
/// the lines its errors name.
fn oracle_run(path: &Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(path)
        .current_dir(path.parent().expect("dir"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    let mut input = String::new();
    for s in stmts {
        assert!(!s.contains('\n'), "one statement per line: {s}");
        input.push_str(s);
        input.push_str(";\n");
    }
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut errs = Vec::new();
    for l in String::from_utf8_lossy(&out.stderr).lines() {
        if let Some(i) = l.find(" near line ") {
            let rest = &l[i + 11..];
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = n.parse() {
                errs.push(n);
            }
        }
    }
    errs.dedup();
    (lines, errs)
}

/// Split a script on `;` (no `;` inside the statements used here).
fn stmts(script: &str) -> Vec<String> {
    script
        .split(';')
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

/// A recursive row source of `n` rows (`v` from 1).
fn rows_of(n: u32) -> String {
    format!("WITH RECURSIVE s(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM s WHERE v < {n})")
}

fn twin_paths(tag: &str) -> (PathBuf, PathBuf) {
    (
        common::temp_db(&format!("v16av-{tag}-h")),
        common::temp_db(&format!("v16av-{tag}-o")),
    )
}

/// The file's auto-vacuum structure: (page count, freelist count,
/// offset 52, offset 64, every pointer-map entry of a page in the
/// file as (page, role, parent)).
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    pages: u32,
    free: u32,
    largest_root: u32,
    incr: u32,
    map: Vec<(u32, u8, u32)>,
}

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn layout(path: &Path) -> Layout {
    let d = std::fs::read(path).expect("read");
    if d.is_empty() {
        // A database never written (an attached main left empty).
        return Layout {
            pages: 0,
            free: 0,
            largest_root: 0,
            incr: 0,
            map: Vec::new(),
        };
    }
    let raw = u16::from_be_bytes([d[16], d[17]]) as usize;
    let ps = if raw == 1 { 65536 } else { raw };
    let pages = (d.len() / ps) as u32;
    let usable = (ps - d[20] as usize) as u32;
    let mut map = Vec::new();
    if be32(&d, 52) != 0 {
        let per = usable / 5 + 1;
        let mut p = 2u32;
        while p <= pages {
            let pg = &d[(p as usize - 1) * ps..p as usize * ps];
            for i in 0..usable / 5 {
                let q = p + 1 + i;
                if q > pages {
                    break;
                }
                let e = 5 * i as usize;
                map.push((q, pg[e], be32(pg, e + 1)));
            }
            p += per;
        }
    }
    Layout {
        pages,
        free: be32(&d, 36),
        largest_root: be32(&d, 52),
        incr: be32(&d, 64),
        map,
    }
}

/// Run `script` on fresh twins; rows and errors must agree, both
/// integrity checks pass on the other engine's file, and (with
/// `exact`) the files' layouts match the pin's.
fn twin(tag: &str, script: &str, exact: bool) -> (PathBuf, PathBuf) {
    let (h, o) = twin_paths(tag);
    let st = stmts(script);
    let a = hipp_run(&h, &st);
    let b = oracle_run(&o, &st);
    assert_eq!(a, b, "{tag}: rows / errors\n{script}");
    assert_eq!(
        common::cli(&h, "PRAGMA integrity_check;").trim(),
        "ok",
        "{tag}: the pin checks hipp's file"
    );
    let (r, e) = hipp_run(&o, &stmts("PRAGMA integrity_check"));
    assert_eq!(
        (r, e),
        (vec!["ok".to_string()], vec![]),
        "{tag}: hipp checks the pin's"
    );
    if exact {
        assert_eq!(layout(&h), layout(&o), "{tag}: layout\n{script}");
    }
    (h, o)
}

/// Deterministic scripts whose page choices the pin makes the same
/// way hipp does: page counts, every pointer-map entry and the root
/// pages must match.
#[test]
fn layouts_match_the_pin() {
    let cases: Vec<(&str, String)> = vec![
        (
            "roots",
            "PRAGMA auto_vacuum = FULL; CREATE TABLE t1(a); CREATE TABLE t2(a UNIQUE); \
             CREATE TABLE t3(a); CREATE INDEX i3 ON t3(a); SELECT name, rootpage FROM sqlite_schema"
                .into(),
        ),
        (
            "drop-moves-root",
            "PRAGMA auto_vacuum = 1; CREATE TABLE t1(a); CREATE TABLE t2(a UNIQUE); \
             CREATE TABLE t3(a); CREATE INDEX i3 ON t3(a); INSERT INTO t1 VALUES (1); DROP TABLE t1; \
             SELECT type, name, rootpage FROM sqlite_schema; PRAGMA page_count"
                .into(),
        ),
        (
            "drop-index-moves-root",
            "PRAGMA auto_vacuum = 1; CREATE TABLE t1(a); CREATE INDEX i1 ON t1(a); CREATE TABLE t2(b); \
             CREATE INDEX i2 ON t2(b); DROP INDEX i1; SELECT name, rootpage FROM sqlite_schema"
                .into(),
        ),
        (
            "create-moves-leaf",
            format!(
                "PRAGMA auto_vacuum = 1; CREATE TABLE t1(a); INSERT INTO t1 {} \
                 SELECT zeroblob(1000) || v FROM s; CREATE TABLE t2(a); \
                 SELECT name, rootpage FROM sqlite_schema; PRAGMA page_count",
                rows_of(10)
            ),
        ),
        (
            "overflow-chains",
            format!(
                "PRAGMA auto_vacuum = 1; CREATE TABLE t1(a); INSERT INTO t1 {} \
                 SELECT zeroblob(5000) || v FROM s; CREATE TABLE t2(b); \
                 SELECT name, rootpage FROM sqlite_schema; SELECT sum(length(a)) FROM t1",
                rows_of(3)
            ),
        ),
        (
            "full-delete-truncates",
            format!(
                "PRAGMA auto_vacuum = FULL; CREATE TABLE t(a, b); INSERT INTO t {} \
                 SELECT v, zeroblob(500) || v FROM s; PRAGMA page_count; DELETE FROM t WHERE a % 2 = 0; \
                 PRAGMA page_count; PRAGMA freelist_count; DELETE FROM t; PRAGMA page_count",
                rows_of(300)
            ),
        ),
        (
            "full-drop-with-index",
            format!(
                "PRAGMA auto_vacuum = full; CREATE TABLE t(a, b); CREATE INDEX i ON t(b); \
                 CREATE TABLE u(x); INSERT INTO t {} SELECT v, zeroblob(300) || v FROM s; \
                 INSERT INTO u VALUES (1); DROP TABLE t; SELECT rootpage, name FROM sqlite_schema; \
                 PRAGMA page_count; PRAGMA freelist_count",
                rows_of(40)
            ),
        ),
        (
            "incremental",
            format!(
                "PRAGMA auto_vacuum = incremental; CREATE TABLE t(a); CREATE TABLE u(b); \
                 INSERT INTO t {g} SELECT zeroblob(3000) FROM s; INSERT INTO u {g} SELECT v FROM s; \
                 DELETE FROM t WHERE rowid % 3 > 0; PRAGMA page_count; PRAGMA freelist_count; \
                 PRAGMA incremental_vacuum(3); PRAGMA page_count; PRAGMA freelist_count; \
                 PRAGMA incremental_vacuum; PRAGMA page_count; PRAGMA freelist_count",
                g = rows_of(60)
            ),
        ),
        (
            "incremental-drop",
            format!(
                "PRAGMA auto_vacuum = 2; CREATE TABLE t1(a); CREATE TABLE t2(a UNIQUE); \
                 CREATE TABLE t3(a); INSERT INTO t3 {} SELECT zeroblob(1000) || v FROM s; \
                 DROP TABLE t2; SELECT type, name, rootpage FROM sqlite_schema; \
                 PRAGMA freelist_count; PRAGMA incremental_vacuum(1); PRAGMA freelist_count",
                rows_of(10)
            ),
        ),
        (
            "autoincrement-drop",
            "PRAGMA auto_vacuum = 1; CREATE TABLE a(x INTEGER PRIMARY KEY AUTOINCREMENT, y); \
             CREATE TABLE b(z); INSERT INTO a(y) VALUES (1), (2); INSERT INTO b VALUES (3); \
             DROP TABLE a; SELECT name, rootpage FROM sqlite_schema; SELECT * FROM sqlite_sequence; \
             SELECT * FROM b"
                .into(),
        ),
    ];
    for (tag, s) in cases.iter() {
        twin(tag, s, true);
    }
}

/// Pointer-map positions at every page size, with the maps spanning
/// several pointer-map pages (512-byte pages hold 102 entries).
#[test]
fn page_sizes_and_map_pages() {
    for ps in [512u32, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        for mode in ["FULL", "INCREMENTAL"] {
            let n = if ps <= 1024 { 400 } else { 60 };
            let s = format!(
                "PRAGMA page_size = {ps}; PRAGMA auto_vacuum = {mode}; CREATE TABLE t(a, b); \
                 INSERT INTO t {g} SELECT v, zeroblob(v % 7 * 90) || v FROM s; CREATE TABLE u(c); \
                 INSERT INTO u {g} SELECT zeroblob({ps}) FROM s WHERE v % 50 = 0; \
                 PRAGMA page_count; DELETE FROM t WHERE a % 3 = 0; PRAGMA page_count; PRAGMA freelist_count; \
                 PRAGMA incremental_vacuum(20); PRAGMA page_count; DROP TABLE t; PRAGMA page_count; \
                 PRAGMA freelist_count; SELECT count(*), sum(length(c)) FROM u",
                g = rows_of(n)
            );
            let (h, _) = twin(&format!("ps{ps}-{mode}"), &s, true);
            let l = layout(&h);
            assert!(l.largest_root >= 3, "{ps}: {l:?}");
        }
    }
}

/// PRAGMA auto_vacuum: values, the empty database, the switch
/// between FULL and INCREMENTAL, the changes that wait for VACUUM,
/// the mode surviving ROLLBACK and ROLLBACK TO.
#[test]
fn pragma_auto_vacuum() {
    let cases = [
        "PRAGMA auto_vacuum = full; PRAGMA auto_vacuum; PRAGMA page_count",
        "PRAGMA auto_vacuum = incremental; PRAGMA auto_vacuum; CREATE TABLE t(a); PRAGMA page_count",
        "PRAGMA auto_vacuum = 0; PRAGMA auto_vacuum; CREATE TABLE t(a); PRAGMA page_count",
        "PRAGMA auto_vacuum = 3; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = -1; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 'full'; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = \"FULL\"; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 1.5; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = abc; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 2; PRAGMA auto_vacuum = 0; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 1; PRAGMA page_size = 1024; PRAGMA page_size; CREATE TABLE t(a); PRAGMA page_size",
        "PRAGMA page_size = 1024; PRAGMA auto_vacuum = 1; PRAGMA page_size; CREATE TABLE t(a); PRAGMA page_count",
        "PRAGMA auto_vacuum = 1; CREATE TABLE t(a); PRAGMA auto_vacuum = 2; PRAGMA auto_vacuum; \
         PRAGMA auto_vacuum = 0; PRAGMA auto_vacuum; PRAGMA auto_vacuum = 1; PRAGMA auto_vacuum",
        "CREATE TABLE t(a); PRAGMA auto_vacuum = 1; PRAGMA auto_vacuum; PRAGMA auto_vacuum = 2; PRAGMA auto_vacuum",
        "PRAGMA user_version = 5; PRAGMA auto_vacuum = 1; PRAGMA auto_vacuum",
        "BEGIN; PRAGMA auto_vacuum = 1; ROLLBACK; PRAGMA auto_vacuum; CREATE TABLE t(a); PRAGMA auto_vacuum",
        "BEGIN; PRAGMA auto_vacuum = 2; COMMIT; PRAGMA auto_vacuum",
        "BEGIN; CREATE TABLE t(a); PRAGMA auto_vacuum = 1; COMMIT; PRAGMA auto_vacuum",
        "SAVEPOINT a; PRAGMA auto_vacuum = 2; ROLLBACK TO a; RELEASE a; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 1; CREATE TABLE t(a); BEGIN; PRAGMA auto_vacuum = 2; ROLLBACK; PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 1; PRAGMA encoding; CREATE TABLE t(a); PRAGMA auto_vacuum",
        "PRAGMA auto_vacuum = 1; SELECT * FROM pragma_auto_vacuum",
    ];
    for (i, s) in cases.iter().enumerate() {
        twin(&format!("prag{i}"), s, true);
    }
}

/// PRAGMA incremental_vacuum's argument (the pin's
/// `sqlite3GetInt32`), its rows, and its other forms.
#[test]
fn incremental_vacuum_arguments() {
    let setup = format!(
        "PRAGMA auto_vacuum = 2; CREATE TABLE t(a); INSERT INTO t {} SELECT zeroblob(3000) FROM s; \
         DELETE FROM t",
        rows_of(90)
    );
    for (i, arg) in [
        "(1)",
        "('3')",
        " = 2",
        "(2.9)",
        "(' 2')",
        "('2abc')",
        "(+2)",
        "(0x2)",
        "('0x10')",
        "(-2)",
        "(0)",
        "(00000000003)",
        "('')",
        "(true)",
        "(9999999999)",
        "(2147483647)",
        "(-0)",
        "",
    ]
    .iter()
    .enumerate()
    {
        let s = format!(
            "{setup}; PRAGMA freelist_count; PRAGMA incremental_vacuum{arg}; PRAGMA freelist_count; \
             PRAGMA page_count; PRAGMA incremental_vacuum(1); PRAGMA freelist_count"
        );
        twin(&format!("iv{i}"), &s, true);
    }
    // Other databases and forms: the temp database and a plain file
    // have nothing to vacuum; FULL inside a transaction does; the
    // pragma has no table-valued form.
    for (i, s) in [
        format!("{setup}; PRAGMA temp.incremental_vacuum(2); PRAGMA main.incremental_vacuum(2); PRAGMA freelist_count"),
        format!("CREATE TABLE t(a); INSERT INTO t {} SELECT zeroblob(3000) FROM s; DELETE FROM t; PRAGMA incremental_vacuum; PRAGMA freelist_count", rows_of(5)),
        format!("PRAGMA auto_vacuum = 1; CREATE TABLE t(a); INSERT INTO t {} SELECT zeroblob(3000) FROM s; BEGIN; DELETE FROM t WHERE rowid > 5; PRAGMA freelist_count; PRAGMA incremental_vacuum(4); PRAGMA page_count; PRAGMA freelist_count; COMMIT; PRAGMA page_count; PRAGMA freelist_count", rows_of(30)),
        format!("{setup}; SELECT * FROM pragma_incremental_vacuum; PRAGMA freelist_count"),
        format!("{setup}; BEGIN; PRAGMA incremental_vacuum(5); ROLLBACK; PRAGMA freelist_count; PRAGMA page_count"),
        format!("{setup}; SAVEPOINT a; PRAGMA incremental_vacuum(5); INSERT INTO t VALUES (zeroblob(9000)); ROLLBACK TO a; PRAGMA incremental_vacuum(2); RELEASE a; PRAGMA freelist_count; PRAGMA page_count; SELECT count(*) FROM t"),
    ]
    .iter()
    .enumerate()
    {
        twin(&format!("ivf{i}"), s, true);
    }
}

/// VACUUM and VACUUM INTO keep the mode or apply a pending one,
/// page for page as on the pin.
#[test]
fn vacuum_of_auto_vacuum_files() {
    let data = format!(
        "CREATE TABLE t(a, b); INSERT INTO t {} SELECT v, zeroblob(v * 20) FROM s; \
         CREATE TABLE u(x UNIQUE); INSERT INTO u VALUES (1), (2); DELETE FROM t WHERE a % 3 = 0",
        rows_of(100)
    );
    for (i, s) in [
        format!("PRAGMA auto_vacuum = 2; {data}; VACUUM; PRAGMA auto_vacuum; PRAGMA page_count; PRAGMA freelist_count"),
        format!("PRAGMA auto_vacuum = 1; {data}; VACUUM; PRAGMA auto_vacuum; PRAGMA page_count"),
        format!("{data}; PRAGMA auto_vacuum = 1; VACUUM; PRAGMA auto_vacuum; PRAGMA page_count; INSERT INTO t VALUES (0, zeroblob(9000)); PRAGMA page_count"),
        format!("{data}; PRAGMA auto_vacuum = 2; VACUUM; PRAGMA auto_vacuum; DELETE FROM t; PRAGMA freelist_count; PRAGMA incremental_vacuum; PRAGMA page_count"),
        format!("PRAGMA auto_vacuum = 1; {data}; PRAGMA auto_vacuum = 0; VACUUM; PRAGMA auto_vacuum; PRAGMA page_count"),
        format!("PRAGMA auto_vacuum = 1; {data}; PRAGMA auto_vacuum = 2; VACUUM; PRAGMA auto_vacuum"),
        format!("PRAGMA auto_vacuum = 2; {data}; PRAGMA page_size = 1024; VACUUM; PRAGMA page_size; PRAGMA page_count"),
    ]
    .iter()
    .enumerate()
    {
        twin(&format!("vac{i}"), s, true);
    }
    // VACUUM INTO: the copy keeps the source's mode (or the pending
    // one), and the source is unchanged.
    for (i, (pre, mid)) in [
        ("PRAGMA auto_vacuum = 1", ""),
        ("PRAGMA auto_vacuum = 2", ""),
        ("PRAGMA auto_vacuum = 1", "PRAGMA auto_vacuum = 2; "),
        ("PRAGMA auto_vacuum = 2", "PRAGMA auto_vacuum = 0; "),
        ("", "PRAGMA auto_vacuum = 1; "),
    ]
    .iter()
    .enumerate()
    {
        let (h, o) = twin_paths(&format!("into{i}"));
        let hi = h.with_file_name("into.db");
        let oi = o.with_file_name("into.db");
        let st = |p: &Path| {
            stmts(&format!(
                "{pre}; {data}; {mid}VACUUM INTO '{}'; PRAGMA auto_vacuum",
                p.display()
            ))
        };
        assert_eq!(hipp_run(&h, &st(&hi)), oracle_run(&o, &st(&oi)), "into{i}");
        assert_eq!(layout(&hi), layout(&oi), "into{i}");
        assert_eq!(layout(&h), layout(&o), "into{i}: source");
        assert_eq!(common::cli(&hi, "PRAGMA integrity_check;").trim(), "ok");
        let q = "PRAGMA auto_vacuum; SELECT count(*), sum(length(b)) FROM t;";
        assert_eq!(common::cli(&hi, q), common::cli(&oi, q), "into{i}");
    }
}

/// Each engine writes the other's auto-vacuum files: the pin's file
/// written by hipp and hipp's by the pin, in both modes and two page
/// sizes; contents agree and both integrity checks pass.
#[test]
fn files_cross_both_ways() {
    for mode in [1, 2] {
        for ps in [1024, 4096] {
            let setup = format!(
                "PRAGMA page_size = {ps}; PRAGMA auto_vacuum = {mode}; CREATE TABLE t(a, b); \
                 CREATE INDEX ti ON t(b); INSERT INTO t {} SELECT v, zeroblob(v % 50 * 30) || v FROM s; \
                 CREATE TABLE u(x); DELETE FROM t WHERE a % 5 = 0",
                rows_of(300)
            );
            let work = format!(
                "INSERT INTO u {} SELECT zeroblob(900) FROM s; DELETE FROM t WHERE a % 7 = 0; \
                 DROP INDEX ti; CREATE INDEX tj ON t(a); PRAGMA incremental_vacuum(5); \
                 SELECT count(*), sum(length(b)) FROM t; DROP TABLE u; \
                 SELECT name, rootpage FROM sqlite_schema; PRAGMA auto_vacuum",
                rows_of(50)
            );
            for (writer, reader) in [("pin", "hipp"), ("hipp", "pin")] {
                let (a, b) = twin_paths(&format!("x{mode}-{ps}-{writer}"));
                let st = stmts(&setup);
                if writer == "pin" {
                    oracle_run(&a, &st);
                } else {
                    hipp_run(&a, &st);
                }
                std::fs::copy(&a, &b).expect("copy");
                let w = stmts(&work);
                let (r1, e1) = if reader == "hipp" {
                    hipp_run(&a, &w)
                } else {
                    oracle_run(&a, &w)
                };
                let (r2, e2) = oracle_run(&b, &w);
                assert_eq!(
                    (r1, e1),
                    (r2, e2),
                    "{mode}/{ps}: {writer} wrote, {reader} writes"
                );
                assert_eq!(common::cli(&a, "PRAGMA integrity_check;").trim(), "ok");
                let (r, _) = hipp_run(&a, &stmts("PRAGMA integrity_check"));
                assert_eq!(r, vec!["ok"]);
                let q = "SELECT count(*), sum(a), sum(length(b)) FROM t;";
                assert_eq!(common::cli(&a, q), common::cli(&b, q));
            }
        }
    }
}

/// WAL mode, savepoints, ROLLBACK and statement rollback on
/// auto-vacuum files.
#[test]
fn transactions_and_wal() {
    let data = |n| {
        format!(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t {} \
             SELECT v, zeroblob(700) || v FROM s",
            rows_of(n)
        )
    };
    let cases = [
        format!(
            "PRAGMA auto_vacuum = 1; {}; BEGIN; DELETE FROM t WHERE a % 2 = 0; PRAGMA page_count; PRAGMA freelist_count; COMMIT; PRAGMA page_count; PRAGMA freelist_count",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 1; {}; BEGIN; DELETE FROM t WHERE a % 3 = 1; ROLLBACK; PRAGMA page_count; SELECT count(*) FROM t",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 1; {}; SAVEPOINT a; DELETE FROM t WHERE a % 2 = 0; SAVEPOINT b; DROP TABLE t; ROLLBACK TO b; DELETE FROM t WHERE a % 3 = 0; RELEASE a; PRAGMA page_count; SELECT count(*), sum(length(b)) FROM t",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 2; {}; SAVEPOINT a; DELETE FROM t; PRAGMA incremental_vacuum(10); ROLLBACK TO a; PRAGMA incremental_vacuum(3); RELEASE a; PRAGMA page_count; PRAGMA freelist_count; SELECT count(*) FROM t",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 1; {}; INSERT INTO t VALUES (500, zeroblob(9000)), (3, 'dup'); PRAGMA page_count; SELECT count(*) FROM t",
            data(50)
        ),
        format!(
            "PRAGMA auto_vacuum = 1; PRAGMA journal_mode = wal; {}; DELETE FROM t WHERE a % 2 = 1; PRAGMA page_count; PRAGMA freelist_count; PRAGMA wal_checkpoint(TRUNCATE); PRAGMA page_count",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 2; PRAGMA journal_mode = wal; {}; DELETE FROM t; PRAGMA incremental_vacuum(7); PRAGMA page_count; PRAGMA journal_mode = delete; PRAGMA incremental_vacuum; PRAGMA page_count",
            data(200)
        ),
        format!(
            "PRAGMA auto_vacuum = 1; {}; PRAGMA journal_mode = wal; DROP TABLE t; CREATE TABLE u(x); PRAGMA page_count; SELECT name, rootpage FROM sqlite_schema",
            data(100)
        ),
    ];
    for (i, s) in cases.iter().enumerate() {
        twin(&format!("txn{i}"), s, true);
    }
}

/// The pin's integrity_check messages for damaged pointer maps and
/// header fields, on hipp too.
#[test]
fn integrity_check_of_damaged_maps() {
    let (_, o) = twin_paths("damage-src");
    oracle_run(
        &o,
        &stmts(&format!(
            "PRAGMA auto_vacuum = 1; CREATE TABLE t(a); CREATE INDEX i ON t(a); \
             INSERT INTO t {} SELECT zeroblob(1500) || v FROM s",
            rows_of(30)
        )),
    );
    let good = std::fs::read(&o).expect("read");
    let ps = 4096usize;
    // (offset, byte): entry kinds and parents, the largest-root field,
    // the incremental flag.
    let edits: Vec<(usize, u8)> = vec![
        (ps, 0),                 // page 3's role
        (ps + 5, 2),             // page 4's role
        (ps + 5 * 3 + 4, 9),     // page 6's parent
        (ps + 5 * 10, 1),        // page 13's role
        (55, 7),                 // largest root
        (67, 1),                 // incremental flag
        (ps + 5 * 20 + 1, 0xff), // page 23's parent, high byte
    ];
    for (i, (off, b)) in edits.iter().enumerate() {
        let (h, o2) = twin_paths(&format!("damage{i}"));
        let mut d = good.clone();
        d[*off] = *b;
        std::fs::write(&h, &d).expect("write");
        std::fs::write(&o2, &d).expect("write");
        let q = stmts("PRAGMA integrity_check; PRAGMA integrity_check(3); PRAGMA quick_check");
        assert_eq!(hipp_run(&h, &q), oracle_run(&o2, &q), "damage {i}");
    }
}

/// The freelist trunk holds the pin's `usable/4 - 8` leaves (v0.16
/// fix, ungated: hipp filled trunks to capacity, so its freelists —
/// and the pages auto-vacuum moves into them — differed).
#[test]
fn freelist_trunk_capacity() {
    for (i, av) in ["", "PRAGMA auto_vacuum = 2; "].iter().enumerate() {
        let s = format!(
            "PRAGMA page_size = 512; {av}CREATE TABLE t(a); CREATE TABLE k(b); INSERT INTO k VALUES (1); \
             INSERT INTO t {} SELECT zeroblob(400) FROM s; DELETE FROM t; PRAGMA freelist_count",
            rows_of(400)
        );
        let (h, o) = twin(&format!("trunk{i}"), &s, i == 1);
        let trunks = |p: &Path| {
            let d = std::fs::read(p).expect("read");
            let mut out = Vec::new();
            let mut t = be32(&d, 32);
            while t != 0 {
                let pg = &d[(t as usize - 1) * 512..t as usize * 512];
                out.push(be32(pg, 4));
                t = be32(pg, 0);
            }
            out
        };
        assert_eq!(trunks(&h), trunks(&o), "{av}");
    }
}

// ---------------------------------------------------------------
// Generated corpora: random write scripts on FULL and INCREMENTAL
// files at several page sizes (journal and WAL), against the live
// pin. Rows and errors must agree; page counts may differ where
// hipp's B-tree splits differ from the pin's, so they are not
// compared — but FULL leaves no free page after a commit, the pin's
// integrity check passes on hipp's file and hipp's on the pin's.
// ---------------------------------------------------------------

/// A small deterministic PRNG (xorshift64*), as in fuzz_smoke.rs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn gen_script(rng: &mut Rng, mode: &str, ps: u32, wal: bool) -> Vec<String> {
    let mut st = vec![
        format!("PRAGMA page_size = {ps}"),
        format!("PRAGMA auto_vacuum = {mode}"),
    ];
    if wal {
        st.push("PRAGMA journal_mode = wal".into());
    }
    let mut tables: Vec<String> = Vec::new();
    let mut nt = 0;
    let steps = 6 + rng.below(20);
    for _ in 0..steps {
        let r = rng.below(100);
        if r < 14 || tables.is_empty() {
            nt += 1;
            let t = format!("t{nt}");
            st.push(format!("CREATE TABLE {t}(a INTEGER PRIMARY KEY, b, c)"));
            if rng.below(10) < 4 {
                st.push(format!("CREATE INDEX {t}i ON {t}(c)"));
            }
            tables.push(t);
        } else if r < 48 {
            let t = &tables[rng.below(tables.len() as u64) as usize];
            let n = 1 + rng.below(60) as u32;
            let w = rng.pick(&["1", "10", "100", "600", "1500", "5000", "20000"]);
            st.push(format!(
                "INSERT INTO {t}(b, c) {} SELECT zeroblob({w} * (v % 3 + 1)) || v, v % 7 FROM s",
                rows_of(n)
            ));
        } else if r < 62 {
            let t = &tables[rng.below(tables.len() as u64) as usize];
            let m = 2 + rng.below(4);
            st.push(format!("DELETE FROM {t} WHERE a % {m} = {}", rng.below(m)));
        } else if r < 69 {
            let t = &tables[rng.below(tables.len() as u64) as usize];
            let w = rng.pick(&["5", "300", "3000"]);
            st.push(format!(
                "UPDATE {t} SET b = zeroblob({w}) || a WHERE a % 3 = 1"
            ));
        } else if r < 75 && tables.len() > 1 {
            let i = rng.below(tables.len() as u64) as usize;
            let t = tables.remove(i);
            st.push(format!("DROP TABLE {t}"));
        } else if r < 81 {
            st.push(
                rng.pick(&[
                    "PRAGMA incremental_vacuum",
                    "PRAGMA incremental_vacuum(3)",
                    "PRAGMA incremental_vacuum(1)",
                ])
                .into(),
            );
        } else if r < 85 {
            st.push("VACUUM".into());
        } else if r < 92 {
            let t = tables[rng.below(tables.len() as u64) as usize].clone();
            st.push("BEGIN".into());
            st.push(format!("DELETE FROM {t} WHERE a % 2 = 0"));
            st.push(format!(
                "INSERT INTO {t}(b, c) {} SELECT zeroblob(2000), v FROM s",
                rows_of(5)
            ));
            st.push(rng.pick(&["COMMIT", "ROLLBACK"]).into());
        } else if r < 96 {
            // Other trees and schema writes: expression indexes,
            // WITHOUT ROWID tables, CREATE TABLE AS, ALTER, REINDEX,
            // ANALYZE's statistics table.
            let t = tables[rng.below(tables.len() as u64) as usize].clone();
            let ops = [
                format!("CREATE INDEX IF NOT EXISTS {t}x ON {t}(length(b), c)"),
                format!("DROP INDEX IF EXISTS {t}x"),
                format!("DROP INDEX IF EXISTS {t}i"),
                "REINDEX".to_string(),
                format!("ALTER TABLE {t} ADD COLUMN d DEFAULT 5"),
                format!("CREATE TABLE IF NOT EXISTS w{nt}(k PRIMARY KEY, v) WITHOUT ROWID"),
                format!("INSERT OR REPLACE INTO w{nt} SELECT c, b FROM {t}"),
                format!("CREATE TABLE IF NOT EXISTS s{nt} AS SELECT * FROM {t}"),
                format!("DROP TABLE IF EXISTS s{nt}"),
                "ANALYZE".to_string(),
            ];
            st.push(ops[rng.below(ops.len() as u64) as usize].clone());
        } else {
            let t = tables[rng.below(tables.len() as u64) as usize].clone();
            st.push("SAVEPOINT sp".into());
            st.push(format!("DELETE FROM {t}"));
            st.push(rng.pick(&["ROLLBACK TO sp", "RELEASE sp"]).into());
            st.push("RELEASE sp".into());
        }
        // A marker row, then the free-page count (compared for FULL
        // only by hipp's own invariant below).
        st.push("SELECT 'free'".into());
        st.push("PRAGMA freelist_count".into());
    }
    for t in &tables {
        st.push(format!("SELECT count(*), sum(length(b)), sum(c) FROM {t}"));
    }
    st.push("SELECT type, name, tbl_name FROM sqlite_schema ORDER BY name".into());
    st
}

/// Rows with every free-page count (the line after each `free`
/// marker) and every incremental_vacuum row removed.
fn data_rows(rows: &[String]) -> (Vec<String>, Vec<String>) {
    let mut data = Vec::new();
    let mut free = Vec::new();
    let mut it = rows.iter();
    while let Some(r) = it.next() {
        if r == "free" {
            if let Some(n) = it.next() {
                free.push(n.clone());
            }
        } else if !r.is_empty() {
            data.push(r.clone());
        }
    }
    (data, free)
}

fn corpus(tag: &str, seed0: u64, n: u64, wal: bool) {
    for seed in seed0..seed0 + n {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mode = if rng.below(2) == 0 {
            "FULL"
        } else {
            "INCREMENTAL"
        };
        let ps = [512u32, 1024, 4096][rng.below(3) as usize];
        let st = gen_script(&mut rng, mode, ps, wal);
        let (h, o) = twin_paths(&format!("{tag}{seed}"));
        let (hr, he) = hipp_run(&h, &st);
        let (or, oe) = oracle_run(&o, &st);
        let (hd, hf) = data_rows(&hr);
        let (od, _) = data_rows(&or);
        assert_eq!(
            (hd, he),
            (od, oe),
            "{tag} seed {seed} ({mode}, {ps}): {st:#?}"
        );
        if mode == "FULL" {
            // Outside a transaction a FULL file has no free page;
            // inside one (BEGIN ... / SAVEPOINT ...) it may.
            let mut depth = 0;
            let mut k = 0;
            for s in &st {
                if s == "BEGIN" || s.starts_with("SAVEPOINT") {
                    depth += 1;
                }
                if s == "COMMIT" || s == "ROLLBACK" || s == "RELEASE sp" {
                    depth = 0;
                }
                if s == "PRAGMA freelist_count" {
                    if depth == 0 {
                        assert_eq!(hf[k], "0", "{tag} seed {seed}: FULL left free pages");
                    }
                    k += 1;
                }
            }
        }
        assert_eq!(
            common::cli(&h, "PRAGMA integrity_check;").trim(),
            "ok",
            "{tag} seed {seed}: the pin checks hipp's file"
        );
        let (r, _) = hipp_run(&o, &stmts("PRAGMA integrity_check"));
        assert_eq!(r, vec!["ok"], "{tag} seed {seed}: hipp checks the pin's");
    }
}

#[test]
fn generated_corpus_journal() {
    let n = std::env::var("HIPP_AV_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    corpus("gj", 1, n, false);
}

#[test]
fn generated_corpus_wal() {
    let n = std::env::var("HIPP_AV_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    corpus("gw", 1000, n, true);
}

/// Page 1 as a root that loses its last cell keeps its child when
/// the child does not fit beside the 100-byte header (v0.16 fix,
/// ungated, found by the generated corpus: the cells were packed
/// over page 1's pointer array, so a DROP TABLE or ALTER TABLE of a
/// 512-byte-page file whose `sqlite_schema` had two leaves left it
/// malformed). Plain and auto-vacuum files.
#[test]
fn page1_root_keeps_an_oversized_child() {
    let schema = "CREATE TABLE t1(a INTEGER PRIMARY KEY, b, c); CREATE INDEX t1i ON t1(c); \
                  CREATE TABLE t2(a INTEGER PRIMARY KEY, b, c); CREATE INDEX t2i ON t2(c); \
                  CREATE TABLE t4(a INTEGER PRIMARY KEY, b, c); CREATE INDEX t4i ON t4(c); \
                  CREATE TABLE t5(a INTEGER PRIMARY KEY, b, c); CREATE TABLE t6(a INTEGER PRIMARY KEY, b, c); \
                  DROP TABLE t1; CREATE TABLE t7(a INTEGER PRIMARY KEY, b, c); \
                  CREATE TABLE t8(a INTEGER PRIMARY KEY, b, c); CREATE TABLE t9(a INTEGER PRIMARY KEY, b, c); \
                  DROP TABLE t5";
    for (i, av) in ["", "PRAGMA auto_vacuum = 1; ", "PRAGMA auto_vacuum = 2; "]
        .iter()
        .enumerate()
    {
        for (j, tail) in [
            "DROP TABLE t2",
            "ALTER TABLE t2 ADD COLUMN d DEFAULT 5",
            "DROP TABLE t2; DROP TABLE t4; DROP TABLE t9; CREATE TABLE u(x)",
        ]
        .iter()
        .enumerate()
        {
            // A plain file reuses free pages in hipp's order (its
            // roots differ from the pin's); an auto-vacuum file's are
            // the pin's.
            // (hipp's ALTER rewrites the schema row by delete and
            // insert, the pin's in place, and the pin's deletes
            // shrink a tree later than hipp's, so their schema trees
            // differ after those.)
            let exact = i > 0 && j == 0;
            let cols = if exact { "name, rootpage" } else { "name" };
            let count = if exact { "PRAGMA page_count; " } else { "" };
            let s = format!(
                "PRAGMA page_size = 512; {av}{schema}; {tail}; SELECT {cols} FROM sqlite_schema; \
                 {count}PRAGMA integrity_check"
            );
            twin(&format!("page1-{i}-{j}"), &s, exact);
        }
    }
}

/// Savepoints around `PRAGMA incremental_vacuum`, which shrinks the
/// file mid-transaction: a `ROLLBACK TO` a savepoint taken after the
/// shrink, then one taken before it, must bring back the pages cut
/// off in between — moved in-use pages, pages changed before the
/// savepoint, and cut pages the file grew back into (v0.16 review
/// fix: the cut-off tail is no longer journaled up front, so the
/// savepoints keep what they need themselves).
#[test]
fn savepoints_across_a_shrinking_file() {
    let base = format!(
        "PRAGMA page_size = 1024; PRAGMA auto_vacuum = 2; \
         CREATE TABLE t(a INTEGER PRIMARY KEY, b); CREATE TABLE k(y); \
         INSERT INTO t {} SELECT v, hex(zeroblob(150)) || v FROM s; \
         INSERT INTO k {} SELECT hex(zeroblob(1250)) || v FROM s; \
         DELETE FROM t WHERE a % 2 = 0",
        rows_of(300),
        rows_of(20)
    );
    let check = "SELECT count(*), sum(length(b)) FROM t; SELECT count(*), sum(length(y)), group_concat(substr(y, 2501)) FROM k; PRAGMA integrity_check";
    let ins = |from: u32, n: u32| {
        format!(
            "INSERT INTO t {} SELECT v + {from}, hex(zeroblob(450)) || v FROM s",
            rows_of(n)
        )
    };
    let cases = [
        // Shrink, savepoint, grow back into the cut pages, roll back
        // to both.
        format!(
            "{base}; BEGIN; UPDATE k SET y = hex(zeroblob(1200)) || rowid WHERE rowid > 10; SAVEPOINT a; \
             DELETE FROM t WHERE a < 120; PRAGMA incremental_vacuum(40); PRAGMA page_count; \
             SAVEPOINT b; {}; PRAGMA page_count; ROLLBACK TO b; {}; ROLLBACK TO a; \
             PRAGMA page_count; {check}; COMMIT; PRAGMA page_count; PRAGMA freelist_count; {check}",
            ins(1000, 60),
            ins(2000, 10)
        ),
        // The same, released instead of rolled back to the outer one.
        format!(
            "{base}; BEGIN; SAVEPOINT a; DELETE FROM t WHERE a > 200; PRAGMA incremental_vacuum(25); \
             SAVEPOINT b; {}; ROLLBACK TO b; PRAGMA incremental_vacuum(5); RELEASE a; COMMIT; \
             PRAGMA page_count; PRAGMA freelist_count; {check}",
            ins(1000, 30)
        ),
        // Two shrinks in nested savepoints, the inner one rolled back
        // twice.
        format!(
            "{base}; SAVEPOINT a; PRAGMA incremental_vacuum(10); SAVEPOINT b; DELETE FROM k WHERE rowid % 2 = 0; \
             PRAGMA incremental_vacuum(30); {}; ROLLBACK TO b; PRAGMA incremental_vacuum(3); {}; \
             ROLLBACK TO b; {check}; ROLLBACK TO a; {check}; RELEASE a; PRAGMA page_count; {check}",
            ins(1000, 40),
            ins(3000, 5)
        ),
        // A FULL file whose transaction shrinks only at commit.
        format!(
            "{}; BEGIN; SAVEPOINT a; DELETE FROM t WHERE a < 250; SAVEPOINT b; {}; ROLLBACK TO b; \
             DELETE FROM k WHERE rowid < 15; RELEASE a; COMMIT; PRAGMA page_count; PRAGMA freelist_count; {check}",
            base.replace("auto_vacuum = 2", "auto_vacuum = 1"),
            ins(1000, 30)
        ),
    ];
    for (i, s) in cases.iter().enumerate() {
        twin(&format!("spshrink{i}"), s, false);
    }
}

// ---------------------------------------------------------------
// The lock-byte page. At 64 KiB pages it is page 16385 (the page
// holding byte offset 2^30); it is never part of the database, has no
// pointer-map entry, and is skipped when the file grows, shrinks
// (`finalDbSize`) and by `PRAGMA incremental_vacuum`. A file reaching
// it is 1 GiB, so the test crafts a sparse one: a table of four rows
// whose overflow chains fill pages 4 to 16384 (each overflow page
// holds only its 4-byte link; the rest are holes), with the pin's
// page 1 and pointer maps around them.
// ---------------------------------------------------------------

const BIG_PS: usize = 65536;
const BIG_PAGES: u32 = 16384;

/// The SQLite varint of `v` (below 2^56).
fn varint(mut v: u64, out: &mut Vec<u8>) {
    let mut b = Vec::new();
    loop {
        b.push((v & 0x7f) as u8);
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    b.reverse();
    let n = b.len();
    for (i, x) in b.into_iter().enumerate() {
        out.push(if i + 1 < n { x | 0x80 } else { x });
    }
}

#[cfg(unix)]
fn write_big(path: &Path, mode: &str) {
    use std::os::unix::fs::FileExt;
    // The pin's empty auto-vacuum file with one table: pages 1, 2
    // (pointer map) and 3 (t's root).
    let base = path.with_extension("base");
    let _ = std::fs::remove_file(&base);
    oracle_run(
        &base,
        &stmts(&format!(
            "PRAGMA page_size = 65536; PRAGMA auto_vacuum = {mode}; CREATE TABLE t(x)"
        )),
    );
    let b = std::fs::read(&base).expect("base");
    assert_eq!(b.len(), 3 * BIG_PS, "three pages");
    let usable = BIG_PS as u32;
    let per = usable / 5 + 1;
    let is_map = |p: u32| p >= 2 && (p - 2).is_multiple_of(per);
    let chain_pages: Vec<u32> = (4..=BIG_PAGES).filter(|p| !is_map(*p)).collect();
    assert_eq!(chain_pages.len(), 4 * 4095);
    let f = std::fs::File::create(path).expect("create");
    f.set_len(u64::from(BIG_PAGES) * BIG_PS as u64)
        .expect("len");
    let mut p1 = b[..BIG_PS].to_vec();
    p1[28..32].copy_from_slice(&BIG_PAGES.to_be_bytes());
    f.write_all_at(&p1, 0).expect("page 1");
    // Four cells, each a one-blob record whose payload leaves the
    // pin's minimum local part (8199 bytes) on the leaf and fills
    // exactly 4095 overflow pages.
    let min_local = (usable - 12) * 32 / 255 - 23;
    let total = u64::from(min_local) + 4095 * u64::from(usable - 4);
    let blob = total - 6;
    let mut leaf = vec![0u8; BIG_PS];
    leaf[0] = 13;
    let mut maps: Vec<(u32, u8, u32)> = vec![(3, 1, 0)];
    for r in 0..4usize {
        let pages = &chain_pages[r * 4095..(r + 1) * 4095];
        let mut cell = Vec::new();
        varint(total, &mut cell);
        varint(r as u64 + 1, &mut cell);
        let mut rec = Vec::new();
        varint(2 * blob + 12, &mut rec);
        assert_eq!(rec.len(), 5);
        cell.push(6);
        cell.extend_from_slice(&rec);
        cell.resize(cell.len() + min_local as usize - 6, 0);
        cell.extend_from_slice(&pages[0].to_be_bytes());
        let off = BIG_PS - cell.len() * (r + 1);
        leaf[off..off + cell.len()].copy_from_slice(&cell);
        leaf[8 + 2 * r..10 + 2 * r].copy_from_slice(&(off as u16).to_be_bytes());
        leaf[5..7].copy_from_slice(&(off as u16).to_be_bytes());
        for (i, p) in pages.iter().enumerate() {
            let next = pages.get(i + 1).copied().unwrap_or(0);
            f.write_all_at(&next.to_be_bytes(), u64::from(p - 1) * BIG_PS as u64)
                .expect("link");
            maps.push(if i == 0 {
                (*p, 3, 3)
            } else {
                (*p, 4, pages[i - 1])
            });
        }
    }
    leaf[3..5].copy_from_slice(&4u16.to_be_bytes());
    f.write_all_at(&leaf, 2 * BIG_PS as u64).expect("leaf");
    for m in [2u32, 2 + per] {
        let mut pg = vec![0u8; BIG_PS];
        for (p, t, par) in maps.iter().filter(|(p, _, _)| *p > m && *p < m + per) {
            let e = 5 * (p - m - 1) as usize;
            pg[e] = *t;
            pg[e + 1..e + 5].copy_from_slice(&par.to_be_bytes());
        }
        f.write_all_at(&pg, u64::from(m - 1) * BIG_PS as u64)
            .expect("map");
    }
}

/// [`Layout`] of a big sparse file, read page by page.
#[cfg(unix)]
fn big_layout(path: &Path) -> Layout {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).expect("open");
    let mut h = [0u8; 100];
    f.read_exact_at(&mut h, 0).expect("header");
    let pages = be32(&h, 28);
    assert_eq!(
        f.metadata().expect("meta").len(),
        u64::from(pages) * BIG_PS as u64,
        "{}: the file ends at its page count",
        path.display()
    );
    let per = BIG_PS as u32 / 5 + 1;
    let mut map = Vec::new();
    let mut m = 2u32;
    while m <= pages {
        let mut pg = vec![0u8; BIG_PS];
        f.read_exact_at(&mut pg, u64::from(m - 1) * BIG_PS as u64)
            .expect("map page");
        for q in m + 1..(m + per).min(pages + 1) {
            let e = 5 * (q - m - 1) as usize;
            map.push((q, pg[e], be32(&pg, e + 1)));
        }
        m += per;
    }
    Layout {
        pages,
        free: be32(&h, 36),
        largest_root: be32(&h, 52),
        incr: be32(&h, 64),
        map,
    }
}

/// Grow past the lock-byte page (CREATE moving a page to the end,
/// an insert), then shrink back across it (FULL's commit, PRAGMA
/// incremental_vacuum), statement by statement against the pin:
/// rows, page counts, pointer maps and root pages must match, and
/// each engine's integrity check passes on the other's file.
#[cfg(unix)]
#[test]
fn lock_byte_page() {
    let pend = 1_073_741_824 / BIG_PS as u32 + 1;
    assert_eq!(pend, 16385);
    for (mode, script) in [
        (
            "FULL",
            "CREATE TABLE u(y); PRAGMA page_count; INSERT INTO u VALUES (zeroblob(200000)); \
             PRAGMA page_count; DROP TABLE u; PRAGMA page_count; CREATE TABLE v(y); \
             INSERT INTO v VALUES (zeroblob(70000)); DELETE FROM v; PRAGMA page_count",
        ),
        (
            "INCREMENTAL",
            "CREATE TABLE u(y); INSERT INTO u VALUES (zeroblob(200000)); DROP TABLE u; \
             PRAGMA page_count; PRAGMA freelist_count; PRAGMA incremental_vacuum(2); \
             PRAGMA freelist_count; PRAGMA incremental_vacuum; PRAGMA page_count; \
             PRAGMA freelist_count; CREATE TABLE w(y); PRAGMA page_count",
        ),
    ] {
        let (h, o) = twin_paths(&format!("lockbyte-{mode}"));
        write_big(&h, mode);
        write_big(&o, mode);
        assert_eq!(big_layout(&h), big_layout(&o));
        let mut crossed = false;
        for st in stmts(script) {
            let one = vec![st.clone()];
            assert_eq!(hipp_run(&h, &one), oracle_run(&o, &one), "{mode}: {st}");
            let (lh, lo) = (big_layout(&h), big_layout(&o));
            crossed |= lh.pages > pend;
            assert_eq!(lh, lo, "{mode}: layout after {st}");
            assert!(
                lh.map.iter().any(|e| e.0 == pend) == (lh.pages >= pend),
                "{mode}: the map covers the lock-byte page's slot"
            );
            if lh.pages >= pend {
                let e = lh.map.iter().find(|e| e.0 == pend).copied();
                assert_eq!(
                    e,
                    Some((pend, 0, 0)),
                    "{mode}: no entry for the lock-byte page"
                );
            }
        }
        assert!(crossed, "{mode}: the file grew past the lock-byte page");
        // The pin checks both files (hipp's integrity check and
        // count(*) read every row's 268 MB payload, too slow for a
        // test: the layouts matching stands in for them).
        let q = stmts("PRAGMA integrity_check; SELECT count(*) FROM t");
        let ok = (vec!["ok".to_string(), "4".to_string()], vec![]);
        assert_eq!(oracle_run(&h, &q), ok, "{mode}: the pin checks hipp's file");
        assert_eq!(oracle_run(&o, &q), ok, "{mode}: and its own");
        let q = stmts("SELECT name, rootpage FROM sqlite_schema ORDER BY 1");
        assert_eq!(hipp_run(&o, &q), oracle_run(&h, &q), "{mode}: schemas");
        // The crafted files are 1 GiB sparse: do not leave them behind.
        let _ = std::fs::remove_file(&h);
        let _ = std::fs::remove_file(&o);
    }
}

// ---------------------------------------------------------------
// Auto-vacuum files as attached databases (with `attach`): the
// schema-qualified pragmas, root moves in an attached file, a FULL
// attached file truncated inside a multi-database commit, VACUUM of
// an attached file. Twin directories; `{d}` in a script is the
// directory. Rows and errors must agree, every file's layout must be
// the pin's, and the pin's integrity check passes on hipp's files.
// ---------------------------------------------------------------

#[cfg(feature = "attach")]
fn attached_twin(tag: &str, script: &str, files: &[&str]) {
    let (h, o) = twin_paths(&format!("att-{tag}"));
    let (hd, od) = (h.parent().expect("dir"), o.parent().expect("dir"));
    let sub = |d: &Path| {
        stmts(script)
            .into_iter()
            .map(|s| s.replace("{d}", &d.to_string_lossy()))
            .collect::<Vec<_>>()
    };
    let a = hipp_run(&h, &sub(hd));
    let b = oracle_run(&o, &sub(od));
    assert_eq!(a, b, "{tag}: rows / errors\n{script}");
    for f in files {
        let (fh, fo) = (hd.join(f), od.join(f));
        // A main database the script never writes stays empty on the
        // pin (hipp writes its page 1 on open: not this unit's).
        if std::fs::metadata(&fo).map_or(0, |m| m.len()) == 0 {
            continue;
        }
        assert_eq!(
            common::cli(&fh, "PRAGMA integrity_check;").trim(),
            "ok",
            "{tag}: the pin checks hipp's {f}"
        );
        assert_eq!(layout(&fh), layout(&fo), "{tag}: {f}'s layout\n{script}");
    }
}

#[cfg(feature = "attach")]
#[test]
fn attached_auto_vacuum_files() {
    let data = |db: &str, n: u32| {
        format!(
            "CREATE TABLE {db}.t(a INTEGER PRIMARY KEY, b); INSERT INTO {db}.t {} \
             SELECT v, zeroblob(700) || v FROM s",
            rows_of(n)
        )
    };
    let files = ["test.db", "aux.db"];
    let cases = [
        // The mode set on an empty attached file; CREATE / DROP move
        // roots there and rewrite its schema's rootpage.
        format!(
            "ATTACH '{{d}}/aux.db' AS aux; PRAGMA aux.auto_vacuum = 1; PRAGMA aux.auto_vacuum; \
             PRAGMA main.auto_vacuum; {}; CREATE TABLE aux.u(c); INSERT INTO aux.u VALUES (1); \
             CREATE INDEX aux.ub ON t(b); DROP TABLE aux.t; SELECT name, rootpage FROM aux.sqlite_schema; \
             PRAGMA aux.page_count; PRAGMA aux.freelist_count; PRAGMA aux.integrity_check",
            data("aux", 120)
        ),
        // INCREMENTAL: PRAGMA aux.incremental_vacuum(N); the
        // unqualified pragma runs on main (not auto-vacuum).
        format!(
            "ATTACH '{{d}}/aux.db' AS aux; PRAGMA aux.auto_vacuum = 2; {}; \
             DELETE FROM aux.t WHERE a % 2 = 0; PRAGMA aux.freelist_count; PRAGMA aux.incremental_vacuum(5); \
             PRAGMA aux.freelist_count; PRAGMA aux.page_count; PRAGMA incremental_vacuum; \
             PRAGMA aux.freelist_count; PRAGMA aux.incremental_vacuum; PRAGMA aux.page_count; \
             PRAGMA aux.freelist_count",
            data("aux", 150)
        ),
        // A transaction across main and a FULL aux: the aux file is
        // truncated inside the super-journal commit.
        format!(
            "ATTACH '{{d}}/aux.db' AS aux; PRAGMA aux.auto_vacuum = 1; {}; CREATE TABLE m(x); \
             BEGIN; INSERT INTO m VALUES (1); DELETE FROM aux.t WHERE a > 40; PRAGMA aux.page_count; \
             COMMIT; PRAGMA aux.page_count; PRAGMA aux.freelist_count; SELECT count(*) FROM aux.t; \
             BEGIN; INSERT INTO m VALUES (2); DELETE FROM aux.t WHERE a > 10; ROLLBACK; \
             PRAGMA aux.page_count; SELECT count(*) FROM aux.t; SELECT * FROM m",
            data("aux", 150)
        ),
        // Both files auto-vacuum, different modes, savepoints across.
        format!(
            "PRAGMA auto_vacuum = 1; ATTACH '{{d}}/aux.db' AS aux; PRAGMA aux.auto_vacuum = 2; {}; {}; \
             SAVEPOINT s; DELETE FROM main.t WHERE a % 3 = 0; DELETE FROM aux.t WHERE a % 3 = 1; \
             PRAGMA aux.incremental_vacuum(4); SAVEPOINT r; DROP TABLE main.t; ROLLBACK TO r; \
             RELEASE s; PRAGMA main.page_count; PRAGMA main.freelist_count; PRAGMA aux.page_count; \
             PRAGMA aux.freelist_count; SELECT count(*) FROM main.t; SELECT count(*) FROM aux.t",
            data("main", 100),
            data("aux", 100)
        ),
        // VACUUM aux keeps its mode; a pending change is applied by
        // it; VACUUM aux INTO a new file.
        format!(
            "ATTACH '{{d}}/aux.db' AS aux; PRAGMA aux.auto_vacuum = 2; {}; DELETE FROM aux.t WHERE a < 60; \
             VACUUM aux; PRAGMA aux.auto_vacuum; PRAGMA aux.page_count; PRAGMA aux.freelist_count; \
             VACUUM aux INTO '{{d}}/copy.db'; PRAGMA aux.auto_vacuum = 0; PRAGMA aux.auto_vacuum; \
             VACUUM aux; PRAGMA aux.auto_vacuum; PRAGMA aux.page_count; \
             ATTACH '{{d}}/copy.db' AS c; PRAGMA c.auto_vacuum; PRAGMA c.page_count; \
             SELECT count(*) FROM c.t",
            data("aux", 120)
        ),
        // A plain aux turned auto-vacuum by VACUUM.
        format!(
            "ATTACH '{{d}}/aux.db' AS aux; {}; PRAGMA aux.auto_vacuum = 1; PRAGMA aux.auto_vacuum; \
             VACUUM aux; PRAGMA aux.auto_vacuum; CREATE TABLE aux.u(c); DROP TABLE aux.t; \
             PRAGMA aux.page_count; PRAGMA aux.freelist_count",
            data("aux", 80)
        ),
    ];
    for (i, s) in cases.iter().enumerate() {
        let mut fs = files.to_vec();
        if s.contains("copy.db") {
            fs.push("copy.db");
        }
        attached_twin(&format!("{i}"), s, &fs);
    }
}
