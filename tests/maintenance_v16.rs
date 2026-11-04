//! v0.16 maintenance statements — VACUUM [INTO], REINDEX,
//! ANALYZE — differentially vs the pinned oracle (SQLite 3.54.1,
//! live). Probed on the pin before implementation; the matrix the
//! tests encode:
//!
//! - VACUUM rebuilds the file compactly: freelist 0, schema
//!   version + 1, user_version / application_id / default cache
//!   size kept, the pin's schema order (tables with their
//!   automatic indexes, sqlite_sequence after the first
//!   AUTOINCREMENT table, then other indexes, then views and
//!   triggers), rowids kept except for a table with neither an
//!   INTEGER PRIMARY KEY nor any index (renumbered 1..n; VACUUM
//!   INTO keeps them all). A pending `PRAGMA page_size` applies.
//!   VACUUM inside a transaction, of an unknown schema, and INTO
//!   a file that holds data are errors.
//! - REINDEX [collation | [main.] table | [main.] index]; anything
//!   else is "unable to identify the object to be reindexed".
//! - ANALYZE writes sqlite_stat1 exactly as the pin computes it,
//!   with the pin's row deletion and rowid rules; the pin then
//!   plans the same over hipp's file as over its own.
//!
//! Every script runs statement by statement on both engines,
//! continuing past errors; rows and failing statements must agree
//! and the oracle must read hipp's final file as it reads its own.

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
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
}

/// hipp: every statement in order, rows printed, errors noted by
/// 1-based statement number and skipped past.
fn hipp_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    block_on(async {
        let mut db = open(path).await;
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
                        let mut l = String::new();
                        render(&q, &mut l);
                        lines.push(l);
                    }
                    Ok(Step::Done) => break,
                    Err(_) => {
                        errs.push(i + 1);
                        break;
                    }
                }
            }
        }
        (lines, errs)
    })
}

/// The pin's shell over stdin, one statement per line: stdout
/// rows, and the lines its errors name.
fn oracle_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, Vec<usize>) {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(path)
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
    (lines, errs)
}

/// The oracle's view of a database file: integrity check and dump.
fn oracle_view(path: &std::path::Path) -> (String, String) {
    (
        common::cli(path, "PRAGMA integrity_check;"),
        common::cli(path, ".dump"),
    )
}

/// Run `stmts` on twin fresh files; rows, failing statements and
/// final files must agree.
fn same(tag: &str, stmts: &[String]) {
    let h = common::temp_db(&format!("v16mt-{tag}-h"));
    let o = common::temp_db(&format!("v16mt-{tag}-o"));
    let (hl, he) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "rows differ:\n{script}");
    assert_eq!(he, oe, "failing statements differ:\n{script}");
    let (hi, hd) = oracle_view(&h);
    let (oi, od) = oracle_view(&o);
    assert_eq!(hi.trim(), "ok", "{tag}: hipp's file\n{script}");
    assert_eq!(oi.trim(), "ok");
    assert_eq!(hd, od, "final files differ:\n{script}");
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

/// Run `stmts` through hipp alone on an existing file, stopping
/// at the first error (returned).
fn hipp_exec(
    path: &std::path::Path,
    stmts: &[&str],
) -> Option<hipp::Error<hipp::storage::FileError>> {
    block_on(async {
        let mut db = open(path).await;
        for sql in stmts {
            let mut q = match db.prepare(sql).await {
                Ok(q) => q,
                Err(e) => return Some(e),
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {}
                    Ok(Step::Done) => break,
                    Err(e) => return Some(e),
                }
            }
        }
        None
    })
}

/// The pin's view of a file's layout-independent state: schema
/// rows in order, rowids of every table, header values.
fn oracle_schema(path: &std::path::Path) -> String {
    let mut out = common::cli(
        path,
        "SELECT type, name, tbl_name, sql FROM sqlite_schema; \
         PRAGMA freelist_count; PRAGMA schema_version; PRAGMA user_version; \
         PRAGMA application_id; PRAGMA page_size; PRAGMA default_cache_size; \
         PRAGMA integrity_check;",
    );
    let tables = common::cli(
        path,
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND rootpage > 0;",
    );
    for t in tables.lines() {
        out.push_str(&common::cli(
            path,
            &format!("SELECT rowid, * FROM \"{t}\";"),
        ));
    }
    out
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

const VACUUM_BASE: &str = "PRAGMA user_version = 7
    PRAGMA application_id = 99
    CREATE TABLE t(a PRIMARY KEY, b)
    CREATE VIEW v AS SELECT * FROM t
    CREATE INDEX tb ON t(b DESC)
    CREATE TABLE u(x INTEGER PRIMARY KEY, y UNIQUE, z COLLATE NOCASE)
    CREATE TABLE w(p, q)
    CREATE TABLE wi(p, q)
    CREATE INDEX wiq ON wi(q COLLATE RTRIM)
    CREATE VIEW v2 AS SELECT p FROM w
    INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 200) SELECT i, zeroblob(100) || i FROM c
    INSERT INTO u VALUES (5, 1, 'a'), (10, 2, 'B'), (11, NULL, NULL), (12, 3.5, x'00ff')
    INSERT INTO w VALUES (1, 2), (3, 4), (5, 6), (7, 8)
    INSERT INTO wi SELECT * FROM w
    INSERT INTO w(rowid, p, q) VALUES (100, 9, 10)
    INSERT INTO t VALUES (1000, zeroblob(9000))
    DELETE FROM w WHERE p = 3
    DELETE FROM wi WHERE p = 3
    DELETE FROM t WHERE a % 2 = 0";

#[test]
fn vacuum_rebuilds_like_the_pin() {
    let mut s = lines(VACUUM_BASE);
    s.extend(lines(
        "VACUUM
        PRAGMA freelist_count
        PRAGMA schema_version
        PRAGMA user_version
        PRAGMA application_id
        SELECT type, name, tbl_name, rootpage FROM sqlite_schema
        SELECT rowid, * FROM w
        SELECT rowid, * FROM wi
        SELECT rowid, x, y, hex(z) FROM u
        SELECT count(*), sum(a), sum(length(b)) FROM t
        SELECT * FROM v2
        VACUUM main
        VACUUM temp
        PRAGMA schema_version
        PRAGMA integrity_check",
    ));
    same("vacuum", &s);
    // The same file, vacuumed by each engine, reads the same.
    let h = common::temp_db("v16mt-vac2-h");
    let o = common::temp_db("v16mt-vac2-o");
    let (_, he) = hipp_run(&h, &lines(VACUUM_BASE));
    assert!(he.is_empty(), "{he:?}");
    std::fs::copy(&h, &o).expect("copy");
    assert!(hipp_exec(&h, &["VACUUM"]).is_none());
    common::cli(&o, "VACUUM;");
    assert_eq!(oracle_schema(&h), oracle_schema(&o));
}

#[test]
fn vacuum_applies_pending_page_size() {
    let mut s = lines(VACUUM_BASE);
    s.extend(lines(
        "PRAGMA page_size = 1024
        PRAGMA page_size
        VACUUM
        PRAGMA page_size
        SELECT count(*), sum(a) FROM t
        PRAGMA page_size = 65536
        VACUUM
        PRAGMA page_size
        PRAGMA page_size = 1000
        VACUUM
        PRAGMA page_size
        PRAGMA page_size = 512
        VACUUM
        PRAGMA page_size
        SELECT rowid, * FROM w
        PRAGMA integrity_check",
    ));
    same("pagesize", &s);
}

#[test]
fn vacuum_errors() {
    same(
        "vac-errors",
        &lines(
            "CREATE TABLE t(a)
            INSERT INTO t VALUES (1)
            BEGIN
            VACUUM
            COMMIT
            SAVEPOINT s
            VACUUM
            RELEASE s
            VACUUM nosuch
            VACUUM main.t
            VACUUM 'main'
            VACUUM INTO
            SELECT * FROM t",
        ),
    );
}

#[test]
fn vacuum_into_writes_the_pins_file() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-v16mt-into");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let src = dir.join("src.db");
    let (_, e) = hipp_run(&src, &lines(VACUUM_BASE));
    assert!(e.is_empty());
    let hi = dir.join("hipp-into.db");
    let oi = dir.join("ora-into.db");
    let into = |p: &std::path::Path| format!("VACUUM INTO '{}'", p.display());
    assert!(hipp_exec(&src, &["PRAGMA page_size = 2048", &into(&hi)]).is_none());
    common::cli(&src, &format!("PRAGMA page_size = 2048; {};", into(&oi)));
    assert_eq!(oracle_schema(&hi), oracle_schema(&oi));
    // The source keeps its page size; its own pending size is the
    // connection's, gone with it.
    assert_eq!(common::cli(&src, "PRAGMA page_size;"), "4096\n");
    // An existing database is refused; an existing empty file is
    // written; inside a transaction, refused.
    match hipp_exec(&src, &[&into(&hi)]) {
        Some(hipp::Error::Schema {
            kind: hipp::SchemaKind::VacuumTarget,
        }) => {}
        other => panic!("{other:?}"),
    }
    let empty = dir.join("empty.db");
    std::fs::write(&empty, b"").expect("empty");
    assert!(hipp_exec(&src, &[&into(&empty)]).is_none());
    assert_eq!(common::cli(&empty, "PRAGMA integrity_check;"), "ok\n");
    let txn = dir.join("txn.db");
    match hipp_exec(&src, &["BEGIN", &into(&txn)]) {
        Some(hipp::Error::Schema {
            kind: hipp::SchemaKind::VacuumInTransaction,
        }) => {}
        other => panic!("{other:?}"),
    }
    assert!(!txn.exists());
    // Without a writer (a no_std host's default), VACUUM INTO is
    // refused.
    block_on(async {
        let cfg = Config {
            vacuum_into: None,
            ..Config::default()
        };
        let mut db = Connection::open(FileStorage::open(&src).expect("fs"), cfg)
            .await
            .expect("open");
        let p = dir.join("none.db");
        let mut q = db.prepare(&into(&p)).await.expect("prep");
        assert!(matches!(q.step().await, Err(hipp::Error::Unsupported)));
    });
}

/// Files the pin made, with what hipp does not create itself
/// (AUTOINCREMENT, triggers): hipp's VACUUM agrees with the pin's.
#[test]
fn vacuum_pin_made_files() {
    let o = common::temp_db("v16mt-pinmade-o");
    let h = common::temp_db("v16mt-pinmade-h");
    common::cli(
        &o,
        "PRAGMA default_cache_size = 123;
         PRAGMA user_version = 5;
         CREATE TABLE t(a, b);
         INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, 'z');
         DELETE FROM t WHERE a = 2;
         CREATE TABLE s(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE);
         CREATE TABLE x(q);
         CREATE TABLE s2(a INTEGER PRIMARY KEY AUTOINCREMENT, b);
         INSERT INTO s(b) VALUES (1), (2);
         INSERT INTO s2(b) VALUES (3);
         CREATE INDEX xi ON x(q);
         CREATE VIEW w AS SELECT 1;
         CREATE TRIGGER tr AFTER INSERT ON x BEGIN SELECT 1; END;
         CREATE TABLE y(z);
         INSERT INTO x VALUES (5), (6);",
    );
    std::fs::copy(&o, &h).expect("copy");
    assert!(hipp_exec(&h, &["VACUUM"]).is_none());
    common::cli(&o, "VACUUM;");
    assert_eq!(oracle_schema(&h), oracle_schema(&o));
    // An expression index: copied (v0.16, `expr_index`), else
    // refused, never mis-copied.
    common::cli(&o, "CREATE INDEX ex ON t(a + 1);");
    std::fs::copy(&o, &h).expect("copy");
    if cfg!(feature = "expr_index") {
        assert!(hipp_exec(&h, &["VACUUM"]).is_none());
        common::cli(&o, "VACUUM;");
        assert_eq!(oracle_schema(&h), oracle_schema(&o));
    } else {
        assert!(matches!(
            hipp_exec(&h, &["VACUUM"]),
            Some(hipp::Error::Unsupported)
        ));
    }
    assert_eq!(common::cli(&h, "PRAGMA integrity_check;"), "ok\n");
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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Random tables, indexes and churn, then VACUUM (and ANALYZE).
#[test]
fn generated_vacuum_analyze_corpus() {
    let mut rng = Rng(0x7ac0_0016_3141_5926);
    let colls = ["", " COLLATE NOCASE", " COLLATE RTRIM", " COLLATE BINARY"];
    let vals = [
        "NULL", "1", "2", "'a'", "'A'", "'a '", "2.5", "x'01'", "'b'", "-7",
    ];
    for case in 0..16 {
        let mut s = Vec::new();
        let ntab = 1 + rng.below(3);
        for t in 0..ntab {
            let ipk = rng.below(2) == 0;
            s.push(format!(
                "CREATE TABLE t{t}({}a{}, b{}, c)",
                if ipk { "k INTEGER PRIMARY KEY, " } else { "" },
                colls[rng.below(colls.len())],
                colls[rng.below(colls.len())],
            ));
            for i in 0..rng.below(3) {
                let cols = ["a", "b", "c", "a, b", "b, a", "a DESC, c", "c, b, a"];
                s.push(format!(
                    "CREATE INDEX t{t}i{i} ON t{t}({})",
                    cols[rng.below(cols.len())]
                ));
            }
            for _ in 0..rng.below(60) {
                s.push(format!(
                    "INSERT INTO t{t}(a, b, c) VALUES ({}, {}, {})",
                    vals[rng.below(vals.len())],
                    vals[rng.below(vals.len())],
                    if rng.below(4) == 0 {
                        format!("zeroblob({})", rng.below(3000))
                    } else {
                        vals[rng.below(vals.len())].to_string()
                    }
                ));
            }
            s.push(format!(
                "DELETE FROM t{t} WHERE rowid % {} = 0",
                2 + rng.below(4)
            ));
        }
        if rng.below(2) == 0 {
            s.push("ANALYZE".to_string());
        }
        s.push("VACUUM".to_string());
        s.push("ANALYZE".to_string());
        s.push("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx".to_string());
        for t in 0..ntab {
            s.push(format!(
                "SELECT rowid, quote(a), quote(b), typeof(c), length(c) FROM t{t}"
            ));
        }
        s.push("SELECT type, name, rootpage FROM sqlite_schema".to_string());
        same(&format!("gen{case}"), &s);
    }
}

#[test]
fn reindex_forms() {
    same(
        "reindex",
        &lines(
            "CREATE TABLE t(a, b COLLATE NOCASE)
            INSERT INTO t VALUES (1, 'x'), (2, 'Y'), (3, 'z'), (2, NULL)
            CREATE INDEX ia ON t(a)
            CREATE INDEX ib ON t(b)
            CREATE INDEX ic ON t(a COLLATE RTRIM, b DESC)
            CREATE TABLE u(p UNIQUE)
            INSERT INTO u VALUES ('q'), ('r')
            REINDEX
            REINDEX t
            REINDEX ia
            REINDEX nocase
            REINDEX binary
            REINDEX rtrim
            REINDEX nosuch
            REINDEX main.ia
            REINDEX main.t
            REINDEX main.nocase
            REINDEX main.nosuch
            REINDEX temp.ia
            REINDEX sqlite_autoindex_u_1
            REINDEX sqlite_autoindex_t_1
            CREATE VIEW vv AS SELECT 1
            REINDEX vv
            REINDEX sqlite_schema
            REINDEX \"ia\"
            REINDEX 'ia'
            CREATE TABLE nocase(z)
            CREATE INDEX inc ON nocase(z)
            INSERT INTO nocase VALUES (1)
            REINDEX nocase
            BEGIN
            REINDEX
            INSERT INTO t VALUES (9, 'w')
            ROLLBACK
            SAVEPOINT s
            REINDEX ib
            ROLLBACK TO s
            RELEASE s
            SELECT * FROM t WHERE b = 'y'
            SELECT a FROM t ORDER BY a
            PRAGMA integrity_check",
        ),
    );
}

#[test]
fn analyze_like_the_pin() {
    // One table: the pin's rows, rowids and deletions exactly.
    same(
        "analyze",
        &lines(
            "CREATE TABLE t(a, b COLLATE NOCASE, c)
            CREATE INDEX i1 ON t(b)
            CREATE UNIQUE INDEX i4 ON t(c)
            CREATE INDEX i5 ON t(a, b, c)
            INSERT INTO t VALUES (1, 'a', 1), (1, 'A', 2), (2, 'b', 7), (NULL, NULL, 8), (NULL, NULL, 9)
            ANALYZE t
            SELECT rowid, * FROM sqlite_stat1
            SELECT sql FROM sqlite_schema WHERE name = 'sqlite_stat1'
            ANALYZE i1
            SELECT rowid, * FROM sqlite_stat1
            ANALYZE nosuch
            ANALYZE main.nosuch
            ANALYZE nosuch.t
            INSERT INTO sqlite_stat1 VALUES ('zz', 'yy', '1 1')
            INSERT INTO sqlite_stat1 VALUES ('t', 'i9', '1 1')
            ANALYZE i4
            SELECT rowid, * FROM sqlite_stat1
            ANALYZE main.t
            SELECT rowid, * FROM sqlite_stat1
            ANALYZE main
            SELECT rowid, * FROM sqlite_stat1
            ANALYZE
            SELECT rowid, * FROM sqlite_stat1
            DELETE FROM t WHERE a = 1
            ANALYZE
            SELECT rowid, * FROM sqlite_stat1
            DELETE FROM t
            ANALYZE
            SELECT count(*) FROM sqlite_stat1
            ANALYZE sqlite_schema
            ANALYZE sqlite_stat1
            DROP TABLE sqlite_stat1
            CREATE VIEW vv AS SELECT 1
            ANALYZE vv
            SELECT count(*) FROM sqlite_schema WHERE name = 'sqlite_stat1'
            SELECT count(*) FROM sqlite_stat1
            PRAGMA integrity_check",
        ),
    );
    // Several tables: the statistics, compared as a set (the pin
    // orders a whole-database ANALYZE by its schema hash).
    same(
        "analyze-many",
        &lines(
            "CREATE TABLE t(a, b COLLATE NOCASE, c)
            CREATE INDEX i1 ON t(b)
            CREATE TABLE e(x)
            CREATE INDEX ex ON e(x)
            CREATE TABLE v(x)
            CREATE TABLE zz(q PRIMARY KEY, r UNIQUE)
            CREATE TABLE ipk(k INTEGER PRIMARY KEY, w)
            CREATE INDEX ipkw ON ipk(w, k)
            INSERT INTO t VALUES (1, 'a', 1), (1, 'A', 2), (2, 'b', 7), (NULL, NULL, 8)
            INSERT INTO v VALUES (1)
            INSERT INTO zz VALUES (1, 1), (2, 2), (3, NULL), (4, NULL)
            INSERT INTO ipk VALUES (1, 'p'), (2, 'p'), (3, NULL)
            ANALYZE
            SELECT * FROM sqlite_stat1 ORDER BY tbl, idx
            ANALYZE zz
            SELECT * FROM sqlite_stat1 ORDER BY tbl, idx
            DELETE FROM t
            ANALYZE main
            SELECT * FROM sqlite_stat1 ORDER BY tbl, idx
            ANALYZE main.v
            SELECT * FROM sqlite_stat1 ORDER BY tbl, idx
            CREATE TABLE big(a, b)
            CREATE INDEX bi ON big(a, b)
            INSERT INTO big WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 11) SELECT x % 10, x FROM c
            ANALYZE big
            SELECT * FROM sqlite_stat1 WHERE tbl = 'big'
            INSERT INTO big VALUES (2, 2)
            ANALYZE big
            SELECT * FROM sqlite_stat1 WHERE tbl = 'big'
            BEGIN
            ANALYZE
            ROLLBACK
            SAVEPOINT q
            ANALYZE big
            ROLLBACK TO q
            RELEASE q
            SELECT * FROM sqlite_stat1 WHERE tbl = 'big'
            PRAGMA integrity_check",
        ),
    );
    // Small schemas: the pin's whole-database row order too
    // (reverse creation order while its schema hash is unsized).
    same(
        "analyze-order",
        &lines(
            "CREATE TABLE p(x)
            CREATE TABLE q(y)
            CREATE INDEX qy ON q(y)
            CREATE INDEX qy2 ON q(y DESC)
            INSERT INTO p VALUES (1), (2)
            INSERT INTO q VALUES (1), (1), (2)
            ANALYZE
            SELECT rowid, * FROM sqlite_stat1",
        ),
    );
}

/// ANALYZE over a large index: the pin's integer rounding, and the
/// pin then plans the same over hipp's file as over its own.
#[test]
fn analyze_statistics_interoperate() {
    let script = lines(
        "CREATE TABLE t(a, b, c)
        CREATE INDEX ta ON t(a)
        CREATE INDEX tbc ON t(b, c)
        CREATE INDEX tc ON t(c COLLATE NOCASE)
        INSERT INTO t WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3000) SELECT i % 7, i % 300, CASE WHEN i % 2 THEN 'X' ELSE 'x' END || (i % 13) FROM n
        ANALYZE
        SELECT * FROM sqlite_stat1 ORDER BY tbl, idx",
    );
    same("analyze-big", &script);
    let h = common::temp_db("v16mt-plan-h");
    let o = common::temp_db("v16mt-plan-o");
    let (_, he) = hipp_run(&h, &script);
    let (_, oe) = oracle_run(&o, &script);
    assert!(he.is_empty() && oe.is_empty());
    for q in [
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 3 AND b = 5",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE c = 'x1' AND a = 2",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b > 10 AND a = 1",
    ] {
        assert_eq!(common::cli(&h, q), common::cli(&o, q), "{q}");
    }
}

/// `ANALYZE temp` makes a temp-schema statistics table that hides
/// the main one for unqualified reads (refused until the TEMP
/// database landed, tests/temp_v16.rs; now as on the pin).
#[test]
fn analyze_temp_makes_temp_statistics() {
    same(
        "antemp",
        &lines(
            "CREATE TABLE t(a)
            CREATE INDEX ti ON t(a)
            INSERT INTO t VALUES (1), (1), (2)
            ANALYZE
            CREATE TEMP TABLE x(b)
            CREATE INDEX xi ON x(b)
            INSERT INTO x VALUES (3)
            ANALYZE temp
            SELECT * FROM sqlite_stat1
            SELECT * FROM main.sqlite_stat1
            SELECT name FROM sqlite_temp_schema",
        ),
    );
}

/// Found while probing ANALYZE (ungated fix): an index on the rowid
/// pseudo-column is the pin's "no such column"; an INTEGER PRIMARY
/// KEY column by its own name is fine.
#[test]
fn create_index_on_rowid_pseudo_column_is_refused() {
    same(
        "index-rowid",
        &lines(
            "CREATE TABLE q(y)
            CREATE INDEX q1 ON q(y, rowid)
            CREATE INDEX q2 ON q(oid)
            CREATE INDEX q3 ON q(_rowid_ DESC)
            CREATE TABLE r(k INTEGER PRIMARY KEY, rowid)
            CREATE INDEX r1 ON r(k, rowid)
            INSERT INTO r VALUES (1, 'x'), (2, 'y')
            SELECT name FROM sqlite_schema ORDER BY name
            SELECT k FROM r WHERE rowid = 'y'",
        ),
    );
}

/// Review regression: DROP TABLE / DROP INDEX delete their
/// statistics rows (`tbl` / `idx` = the stored name, exactly) from
/// sqlite_stat1, as the pin's DROP does; DROP VIEW and ALTER TABLE
/// leave them. A later table of the same name starts with none.
#[test]
fn drop_clears_statistics() {
    same(
        "drop-stat-index",
        &lines(
            "CREATE TABLE t(a, b)
            CREATE INDEX ia ON t(a)
            CREATE INDEX ib ON t(b)
            INSERT INTO t VALUES (1, 2), (3, 4)
            ANALYZE
            DROP INDEX ia
            SELECT * FROM sqlite_stat1 ORDER BY idx
            DROP TABLE t
            SELECT count(*) FROM sqlite_stat1",
        ),
    );
    same(
        "drop-stat-table",
        &lines(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b)
            INSERT INTO t VALUES (1, 2), (3, 4)
            ANALYZE
            DROP TABLE t
            SELECT * FROM sqlite_stat1
            CREATE TABLE t(z)
            SELECT * FROM sqlite_stat1",
        ),
    );
    same(
        "drop-stat-exact-names",
        &lines(
            "CREATE TABLE T(a)
            CREATE INDEX IA ON T(a)
            INSERT INTO T VALUES (1)
            ANALYZE
            INSERT INTO sqlite_stat1 VALUES ('t', 'ia', 'x'), ('T', 'other', 'y')
            DROP INDEX ia
            SELECT * FROM sqlite_stat1 ORDER BY 1, 2
            DROP TABLE t
            SELECT * FROM sqlite_stat1",
        ),
    );
    same(
        "drop-stat-txn-view-alter",
        &lines(
            "CREATE TABLE t(a)
            CREATE TABLE w(a, b)
            CREATE INDEX wa ON w(a)
            INSERT INTO t VALUES (1)
            INSERT INTO w VALUES (1, 2)
            ANALYZE
            BEGIN
            DROP TABLE t
            SELECT count(*) FROM sqlite_stat1
            ROLLBACK
            SELECT * FROM sqlite_stat1 ORDER BY tbl
            CREATE VIEW v AS SELECT 1
            INSERT INTO sqlite_stat1 VALUES ('v', NULL, '3')
            DROP VIEW v
            ALTER TABLE w RENAME TO w2
            ALTER TABLE w2 DROP COLUMN b
            SELECT * FROM sqlite_stat1 ORDER BY tbl
            DROP TABLE sqlite_stat1
            SELECT name FROM sqlite_schema
            ANALYZE
            SELECT * FROM sqlite_stat1 ORDER BY tbl",
        ),
    );
}

/// Review regression: ANALYZE of a table or index named in
/// another case replaces the rows under the stored name (it left
/// duplicates); the schema table is `sqlite_master`.
#[test]
fn analyze_names_as_stored() {
    same(
        "analyze-case",
        &lines(
            "CREATE TABLE t(a)
            INSERT INTO t VALUES (1)
            ANALYZE
            ANALYZE T
            SELECT * FROM sqlite_stat1
            CREATE INDEX i ON t(a)
            ANALYZE I
            ANALYZE MAIN.T
            SELECT * FROM sqlite_stat1 ORDER BY idx
            INSERT INTO sqlite_stat1 VALUES ('sqlite_master', NULL, '1'), ('sqlite_schema', NULL, '1'), ('V', NULL, '1')
            CREATE VIEW v AS SELECT 1
            ANALYZE SQLITE_SCHEMA
            ANALYZE V
            SELECT * FROM sqlite_stat1 ORDER BY tbl, idx",
        ),
    );
}

/// Review regression: VACUUM places sqlite_sequence after the
/// first table whose CREATE has the AUTOINCREMENT *keyword* — not
/// the word inside a string, a quoted name or a comment.
#[test]
fn vacuum_autoincrement_is_a_token() {
    for (tag, sql) in [
        (
            "autoinc-default",
            "CREATE TABLE a(x DEFAULT 'autoincrement');
             CREATE TABLE b(id INTEGER PRIMARY KEY AUTOINCREMENT, y);
             INSERT INTO b(y) VALUES (1);",
        ),
        (
            "autoinc-name-comment",
            "CREATE TABLE a(\"autoincrement\" INT /* AUTOINCREMENT */);
             CREATE TABLE c(z);
             CREATE TABLE b(id INTEGER PRIMARY KEY AUTOINCREMENT, y);
             INSERT INTO b(y) VALUES (1), (2);",
        ),
    ] {
        let o = common::temp_db(&format!("v16mt-{tag}-o"));
        let h = common::temp_db(&format!("v16mt-{tag}-h"));
        common::cli(&o, sql);
        std::fs::copy(&o, &h).expect("copy");
        assert!(hipp_exec(&h, &["VACUUM"]).is_none(), "{tag}");
        common::cli(&o, "VACUUM;");
        assert_eq!(oracle_schema(&h), oracle_schema(&o), "{tag}");
        assert_eq!(
            common::cli(&h, "SELECT name, rootpage FROM sqlite_schema;"),
            common::cli(&o, "SELECT name, rootpage FROM sqlite_schema;"),
            "{tag}"
        );
    }
}

/// Review regression: a new row's rowid when the largest rowid is
/// 9223372036854775807 — the pin picks an unused one (at random);
/// hipp failed ANALYZE (and INSERT) with DatatypeMismatch. hipp
/// now takes the smallest unused positive rowid.
#[test]
fn new_rowid_after_the_largest() {
    let o = common::temp_db("v16mt-maxrowid-o");
    let h = common::temp_db("v16mt-maxrowid-h");
    common::cli(
        &o,
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES (1); INSERT INTO u VALUES (1);
         ANALYZE; UPDATE sqlite_stat1 SET rowid = 9223372036854775807 WHERE tbl = 'u';
         CREATE TABLE w(x); INSERT INTO w(rowid, x) VALUES (9223372036854775807, 0), (1, 1), (2, 2);",
    );
    std::fs::copy(&o, &h).expect("copy");
    let stmts = [
        "ANALYZE t",
        "INSERT INTO w VALUES (3)",
        "INSERT INTO w VALUES (4)",
    ];
    assert!(hipp_exec(&h, &stmts).is_none());
    let mut script = String::new();
    for s in stmts {
        script.push_str(s);
        script.push_str(";\n");
    }
    common::cli(&o, &script);
    let view = "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl; \
                SELECT count(*), count(DISTINCT rowid), min(rowid) > 0, sum(x) FROM w; \
                PRAGMA integrity_check;";
    assert_eq!(common::cli(&h, view), common::cli(&o, view));
    // hipp's choice: the smallest unused positive rowids.
    assert_eq!(
        common::cli(&h, "SELECT rowid FROM w WHERE x >= 3 ORDER BY x;"),
        "3\n4\n"
    );
}

/// Review regression: VACUUM INTO takes any expression with a text
/// value (`'a' || 'b'`, a subquery, a bound parameter); NULL, a
/// number or a blob is the pin's "non-text filename"; `INTO ''` is
/// the pin's anonymous temporary database and succeeds, writing
/// nothing.
#[test]
fn vacuum_into_expressions() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-v16mt-intoexpr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let src = dir.join("src.db");
    let (_, e) = hipp_run(&src, &lines(VACUUM_BASE));
    assert!(e.is_empty());
    let d = dir.display();
    let concat = format!("VACUUM INTO '{d}/' || 'cat.db'");
    let sub = format!("VACUUM main INTO (SELECT lower('{d}/SUB.db'))");
    assert!(hipp_exec(&src, &[&concat, &sub, "VACUUM INTO ''"]).is_none());
    for f in ["cat.db", "sub.db"] {
        let p = dir.join(f);
        assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n", "{f}");
        assert_eq!(
            common::cli(&p, "SELECT count(*), sum(length(b)) FROM t;"),
            common::cli(&src, "SELECT count(*), sum(length(b)) FROM t;")
        );
    }
    let before = std::fs::read_dir(&dir).expect("ls").count();
    for bad in ["VACUUM INTO NULL", "VACUUM INTO 123", "VACUUM INTO x'41'"] {
        match hipp_exec(&src, &[bad]) {
            Some(hipp::Error::Schema {
                kind: hipp::SchemaKind::VacuumTarget,
            }) => {}
            other => panic!("{bad}: {other:?}"),
        }
        // The pin errors too.
        let o = std::process::Command::new(common::oracle())
            .arg(&src)
            .arg(format!("{bad};"))
            .output()
            .expect("oracle runs");
        assert!(!o.status.success(), "{bad}");
    }
    assert!(hipp_exec(&src, &["VACUUM INTO nosuchcol"]).is_some());
    assert!(hipp_exec(&src, &["VACUUM INTO count(*)"]).is_some());
    assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), before);
    // A bound parameter.
    #[cfg(feature = "params")]
    block_on(async {
        let mut db = open(&src).await;
        let p = dir.join("bound.db");
        let mut q = db.prepare("VACUUM INTO ?1").await.expect("prep");
        q.bind_text(1, &p.display().to_string()).expect("bind");
        assert_eq!(q.step().await.expect("step"), Step::Done);
        drop(q);
        assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
    });
}

/// An auto-vacuum file without the `autovacuum` leaf: VACUUM is
/// refused (hipp writes no pointer maps, so it cannot rebuild one;
/// the pin keeps auto_vacuum = 1), leaving the file untouched.
/// `PRAGMA auto_vacuum = 0` first makes it a plain file, as on the
/// pin. (With the leaf VACUUM keeps the mode:
/// tests/autovacuum_v16.rs.)
#[cfg(not(feature = "autovacuum"))]
#[test]
fn vacuum_of_an_auto_vacuum_file() {
    let o = common::temp_db("v16mt-autovac-o");
    let h = common::temp_db("v16mt-autovac-h");
    common::cli(
        &o,
        "PRAGMA auto_vacuum = FULL; CREATE TABLE t(a); INSERT INTO t VALUES (1), (2);",
    );
    std::fs::copy(&o, &h).expect("copy");
    let bytes = std::fs::read(&h).expect("read");
    assert!(matches!(
        hipp_exec(&h, &["VACUUM"]),
        Some(hipp::Error::Unsupported)
    ));
    assert_eq!(std::fs::read(&h).expect("read"), bytes);
    assert!(hipp_exec(&h, &["PRAGMA auto_vacuum = 0", "VACUUM"]).is_none());
    common::cli(&o, "PRAGMA auto_vacuum = 0; VACUUM;");
    assert_eq!(oracle_schema(&h), oracle_schema(&o));
    assert_eq!(common::cli(&h, "PRAGMA auto_vacuum;"), "0\n");
}
