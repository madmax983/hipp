//! v0.16 WITHOUT ROWID tables (leaf feature `without_rowid`),
//! differentially vs the pinned oracle (SQLite 3.54.1). The semantics
//! were probed on the pin before implementation (STATUS.md, "WITHOUT
//! ROWID tables"); the matrix the tests encode:
//!
//! - Definitions as the pin: PRIMARY KEY required, no AUTOINCREMENT,
//!   `WITHOUT <other>` an unknown option, `rowid` / `oid` / `_rowid_`
//!   unknown names (SELECT, WHERE, RETURNING, UPDATE SET, INSERT
//!   column lists, CREATE INDEX, CHECK, partial-index WHERE, trigger
//!   NEW / OLD), NOT NULL key columns, INTEGER PRIMARY KEY no rowid
//!   alias, a repeated key column dropped, key collations and DESC
//!   ordering the table, `last_insert_rowid()` left alone.
//! - Every write path: INSERT, OR IGNORE / REPLACE, upsert (DO
//!   NOTHING / DO UPDATE, also of the key), RETURNING, UPDATE of key
//!   columns (the row moves; conflicts as the pin reports them),
//!   UPDATE OR REPLACE / IGNORE / FAIL, DELETE, triggers, foreign keys
//!   with WITHOUT ROWID parents and children, savepoints.
//! - Secondary indexes holding the key columns, UNIQUE constraints
//!   (their appended key columns ascending, as on the pin), expression
//!   and partial indexes; PRAGMA table_info / table_list / index_list
//!   / index_info / index_xinfo, integrity_check; ALTER TABLE RENAME /
//!   ADD COLUMN / RENAME COLUMN / DROP COLUMN, DROP TABLE, VACUUM,
//!   REINDEX, ANALYZE.
//! - Scans in the pin's order (its covering-index choice for these
//!   tables), lookups, ranges, joins, subqueries and views.
//! - Files interoperate both ways, rows spilling to overflow pages
//!   included: the pin's integrity_check passes on hipp's files and
//!   hipp reads and writes the pin's. Generated corpora (xorshift
//!   PRNG) of random schemas — composite, collated and DESC keys,
//!   UNIQUE constraints and indexes — with random write sequences,
//!   against the live pin.

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};
use std::path::{Path, PathBuf};

type HErr = Error<FileError>;

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> Vec<u8> {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    t.as_slice().to_vec()
}

/// The shell's byte rendering: control bytes but tab and newline in
/// caret notation; text up to its first NUL.
fn shell_bytes(b: &[u8], text: bool) -> Vec<u8> {
    let b = match (text, b.iter().position(|c| *c == 0)) {
        (true, Some(i)) => &b[..i],
        _ => b,
    };
    let mut out = Vec::with_capacity(b.len());
    for &c in b {
        if c < 0x20 && c != b'\t' && c != b'\n' {
            out.push(b'^');
            out.push(c + 0x40);
        } else {
            out.push(c);
        }
    }
    out
}

/// One statement's rows in the CLI's list rendering, or its error.
async fn collect(db: &mut Connection<FileStorage>, sql: &str) -> Result<Vec<u8>, HErr> {
    let mut q = db.prepare(sql).await?;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    while q.step().await? == Step::Row {
        if !first {
            out.push(b'\n');
        }
        first = false;
        for i in 0..q.column_count() {
            if i > 0 {
                out.push(b'|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => out.extend_from_slice(v.to_string().as_bytes()),
                Some(Value::Real(f)) => out.extend_from_slice(&fmt_real(f)),
                Some(Value::Text(b)) => out.extend(shell_bytes(b, true)),
                Some(Value::Blob(b)) => out.extend(shell_bytes(b, false)),
            }
        }
    }
    Ok(out)
}

fn fresh(tag: &str) -> PathBuf {
    common::temp_db(&format!("v16wr-{tag}"))
}

/// The statements through hipp on the file at `path`.
fn hipp_file(path: &Path, stmts: &[String]) -> Vec<Result<Vec<u8>, HErr>> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut out = Vec::new();
        for s in stmts {
            out.push(collect(&mut db, s).await);
        }
        out
    })
}

/// The statements through the oracle CLI on the file at `path`: per
/// statement, its output (`Some`), or None when the pin reported an
/// error for it.
fn oracle_file(path: &Path, stmts: &[String]) -> Vec<Option<Vec<u8>>> {
    let mut script = String::new();
    for (i, s) in stmts.iter().enumerate() {
        assert!(!s.contains('\n'), "statements are one line each: {s}");
        script.push_str(&format!("SELECT '#@#{i}';\n{s};\n"));
    }
    let spath = path.with_extension("sql");
    std::fs::write(&spath, script.as_bytes()).expect("write script");
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::fs::File::open(&spath).expect("script"))
        .output()
        .expect("oracle runs");
    let _ = std::fs::remove_file(&spath);
    let mut res: Vec<Option<Vec<u8>>> = vec![Some(Vec::new()); stmts.len()];
    let o = &out.stdout;
    let mut marks: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0usize;
    while at < o.len() {
        if o[at..].starts_with(b"#@#") && (at == 0 || o[at - 1] == b'\n') {
            let end = o[at..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(o.len(), |k| at + k);
            if let Some(k) = std::str::from_utf8(&o[at + 3..end])
                .ok()
                .and_then(|t| t.parse::<usize>().ok())
            {
                marks.push((k, at, (end + 1).min(o.len())));
            }
            at = end;
        }
        at += 1;
    }
    for (m, &(k, _, body)) in marks.iter().enumerate() {
        let stop = marks.get(m + 1).map_or(o.len(), |n| n.1);
        let mut chunk = o[body.min(stop)..stop].to_vec();
        if chunk.last() == Some(&b'\n') {
            chunk.pop();
        }
        if k < res.len() {
            res[k] = Some(chunk);
        }
    }
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        let Some(at) = line.find("near line ") else {
            continue;
        };
        let n: usize = line[at + 10..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if n.is_multiple_of(2) && n / 2 >= 1 && n / 2 <= stmts.len() {
            res[n / 2 - 1] = None;
        }
    }
    res
}

/// Lines sorted: a result compared without its row order.
fn sorted(b: &[u8]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = b.split(|c| *c == b'\n').map(|l| l.to_vec()).collect();
    v.sort();
    v
}

/// Every statement agrees with the pin, each engine on its own fresh
/// file: equal output (without its row order when `unordered` says
/// so), or both fail; then the pin's integrity_check passes on the
/// file hipp wrote. Returns how many statements failed on both.
fn agree(tag: &str, stmts: &[String], unordered: &dyn Fn(&str) -> bool) -> usize {
    let hp = fresh(&format!("{tag}-h"));
    let op = fresh(&format!("{tag}-o"));
    let got = hipp_file(&hp, stmts);
    let want = oracle_file(&op, stmts);
    let mut bad = Vec::new();
    let mut both_err = 0;
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        let ok = match (g, w) {
            (Ok(a), Some(b)) => a == b || (unordered(&stmts[i]) && sorted(a) == sorted(b)),
            (Err(_), None) => {
                both_err += 1;
                true
            }
            _ => false,
        };
        if !ok {
            bad.push(format!(
                "{}\n  hipp:   {:?}\n  oracle: {:?}",
                stmts[i],
                g.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
                w.as_ref().map(|b| String::from_utf8_lossy(b).into_owned())
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{tag}: {} of {} differ from the pin:\n{}\nschema:\n{}",
        bad.len(),
        stmts.len(),
        bad.iter().take(10).cloned().collect::<Vec<_>>().join("\n"),
        stmts
            .iter()
            .filter(|s| s.starts_with("CREATE"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    let ic = common::cli(&hp, "PRAGMA integrity_check;");
    assert_eq!(
        ic.trim(),
        "ok",
        "{tag}: the pin's integrity_check on hipp's file"
    );
    both_err
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

fn ordered(_: &str) -> bool {
    false
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
}

fn corpus_size(default: usize) -> usize {
    std::env::var("HIPP_WR_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn gen_val(r: &mut Rng) -> String {
    let x = r.below(100);
    if x < 6 {
        "NULL".into()
    } else if x < 45 {
        r.int(-3, 9).to_string()
    } else if x < 58 {
        format!("{}.5", r.int(-3, 9))
    } else if x < 92 {
        format!(
            "'{}'",
            r.pick(&["a", "B", "b", "c", "A", "10", "2", "a ", "B  "])
        )
    } else {
        format!("x'0{}'", r.below(10))
    }
}

/// The pin's integrity_check and hipp's agree that `p` is sound.
fn both_ok(p: &Path) {
    assert_eq!(
        common::cli(p, "PRAGMA integrity_check;").trim(),
        "ok",
        "pin check"
    );
    let got = hipp_file(p, &["PRAGMA integrity_check".to_string()]);
    assert_eq!(
        got[0].as_ref().map(|b| b.clone()).ok(),
        Some(b"ok".to_vec()),
        "hipp check"
    );
}

// ------------------------------------------------------- definitions

#[test]
fn definitions_and_errors() {
    let stmts = lines(
        "CREATE TABLE t0(a) WITHOUT ROWID;
         CREATE TABLE t1(a INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID;
         CREATE TABLE t2(a PRIMARY KEY) WITHOUT foo;
         CREATE TABLE t3(a PRIMARY KEY) without RowId;
         CREATE TABLE t4(a PRIMARY KEY) WITHOUT ROWID, WITHOUT ROWID;
         CREATE TABLE t5(a PRIMARY KEY) STRICT, WITHOUT ROWID;
         CREATE TABLE t6(a INT PRIMARY KEY) STRICT, WITHOUT ROWID;
         CREATE TABLE t7(a, b, c AS (a + b), PRIMARY KEY(c)) WITHOUT ROWID;
         CREATE TABLE t8(a PRIMARY KEY, PRIMARY KEY(a)) WITHOUT ROWID;
         CREATE TABLE t9 WITHOUT ROWID AS SELECT 1;
         CREATE TABLE t10(a PRIMARY KEY, b, CHECK(rowid > 0)) WITHOUT ROWID;
         CREATE TABLE t11(a PRIMARY KEY, b AS (rowid)) WITHOUT ROWID;
         CREATE TABLE t12(a, b, PRIMARY KEY(a, a)) WITHOUT ROWID;
         CREATE TABLE t13(a, b, PRIMARY KEY(a, A COLLATE NOCASE)) WITHOUT ROWID;
         CREATE TABLE t14(a INTEGER PRIMARY KEY DESC, b) WITHOUT ROWID;
         CREATE TABLE t15(a UNIQUE, b, PRIMARY KEY(a)) WITHOUT ROWID;
         CREATE TABLE t16(a, b, UNIQUE(a, b), PRIMARY KEY(a)) WITHOUT ROWID;
         CREATE TEMP TABLE t17(a PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO t6 VALUES ('5');
         INSERT INTO t6 VALUES ('x');
         SELECT a, typeof(a) FROM t6;
         SELECT * FROM sqlite_schema ORDER BY name;
         SELECT * FROM temp.sqlite_schema;
         SELECT name, type, ncol, wr, strict FROM pragma_table_list ORDER BY name;
         PRAGMA table_info(t12);
         PRAGMA index_xinfo(sqlite_autoindex_t12_1);
         PRAGMA index_xinfo(sqlite_autoindex_t13_1);
         PRAGMA index_list(t15);
         PRAGMA index_list(t16);
         PRAGMA index_xinfo(sqlite_autoindex_t16_1)",
    );
    let errs = agree("defs", &stmts, &ordered);
    assert!(errs >= 10, "the pin's definition errors: {errs}");
}

#[test]
fn rowid_names_refused() {
    let stmts = lines(
        "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID;
         CREATE TABLE r(a PRIMARY KEY, rowid) WITHOUT ROWID;
         INSERT INTO t VALUES (1, 2);
         INSERT INTO r VALUES (1, 7);
         SELECT rowid FROM t;
         SELECT oid FROM t;
         SELECT _rowid_ FROM t;
         SELECT t.rowid FROM t;
         SELECT * FROM t WHERE rowid = 1;
         SELECT rowid, a FROM r;
         INSERT INTO t VALUES (2, 3) RETURNING rowid;
         UPDATE t SET rowid = 1;
         INSERT INTO t(rowid, a) VALUES (1, 1);
         CREATE INDEX i ON t(rowid);
         CREATE INDEX j ON t(b) WHERE rowid > 0;
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT NEW.rowid; END;
         INSERT INTO t VALUES (3, 4);
         SELECT * FROM t;
         SELECT last_insert_rowid()",
    );
    let errs = agree("rowid", &stmts, &ordered);
    assert!(errs >= 10, "rowid names: {errs}");
}

/// The connection's `last_insert_rowid()` ignores WITHOUT ROWID
/// inserts, as the pin's does; `changes()` counts them.
#[test]
fn last_insert_rowid_unaffected() {
    let p = fresh("lastid");
    let got = hipp_file(
        &p,
        &lines(
            "CREATE TABLE r(x);
             INSERT INTO r VALUES (5), (6);
             CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID;
             INSERT INTO t VALUES (1, 1), (2, 2), (3, 3);
             SELECT last_insert_rowid(), changes(), total_changes()",
        ),
    );
    assert_eq!(got[4].as_ref().ok(), Some(&b"2|3|5".to_vec()));
}

// ------------------------------------------------------------ writes

#[test]
fn writes_and_conflicts() {
    let stmts = lines(
        "CREATE TABLE t(a PRIMARY KEY, b UNIQUE, c) WITHOUT ROWID;
         INSERT INTO t VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 3, 'z');
         INSERT INTO t VALUES (NULL, 9, 'n');
         INSERT INTO t(b) VALUES (9);
         INSERT INTO t DEFAULT VALUES;
         INSERT INTO t VALUES (1, 9, 'dup');
         INSERT INTO t VALUES (9, 1, 'dupb');
         INSERT OR IGNORE INTO t VALUES (1, 8, 'i'), (NULL, 8, 'i'), (4, 4, 'w');
         INSERT OR REPLACE INTO t VALUES (NULL, 8, 'r');
         REPLACE INTO t VALUES (1, 2, 'both');
         SELECT * FROM t;
         INSERT INTO t VALUES (3, 5, 'u') ON CONFLICT(a) DO UPDATE SET c = excluded.c || c RETURNING *;
         INSERT INTO t VALUES (7, 4, 'u') ON CONFLICT(b) DO UPDATE SET a = a + 100 RETURNING *;
         INSERT INTO t VALUES (7, 4, 'n') ON CONFLICT DO NOTHING;
         SELECT * FROM t;
         UPDATE t SET a = a + 1;
         UPDATE t SET a = a + 10 RETURNING *;
         UPDATE OR IGNORE t SET a = 1 RETURNING a;
         UPDATE OR REPLACE t SET a = 11 WHERE a = 13 RETURNING *;
         UPDATE OR FAIL t SET a = a - 1;
         SELECT * FROM t;
         UPDATE t SET a = NULL WHERE a = 11;
         UPDATE OR REPLACE t SET b = 5 WHERE c = 'w';
         DELETE FROM t WHERE b > 4 RETURNING *;
         SELECT * FROM t;
         SELECT count(*), changes(), total_changes() FROM t;
         DELETE FROM t;
         SELECT count(*) FROM t",
    );
    agree("writes", &stmts, &ordered);
}

#[test]
fn key_order_and_lookups() {
    let stmts = lines(
        "CREATE TABLE d(a, b, PRIMARY KEY(a DESC, b)) WITHOUT ROWID;
         INSERT INTO d VALUES (1, 1), (2, 2), (1, 3), (3, 0), (2, 1);
         SELECT * FROM d;
         SELECT * FROM d ORDER BY a;
         SELECT * FROM d WHERE a = 2;
         SELECT * FROM d WHERE a BETWEEN 2 AND 3;
         SELECT * FROM d WHERE a > 1 ORDER BY a DESC, b LIMIT 2;
         SELECT max(a), min(b), count(*) FROM d;
         SELECT a, count(*) FROM d GROUP BY a;
         SELECT DISTINCT a FROM d;
         CREATE TABLE n(a TEXT COLLATE NOCASE PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO n VALUES ('b', 1), ('A', 2), ('C', 3);
         INSERT INTO n VALUES ('a', 9);
         SELECT * FROM n;
         SELECT * FROM n WHERE a = 'c';
         SELECT * FROM n WHERE a = 'c' COLLATE BINARY;
         SELECT * FROM n WHERE a > 'B';
         UPDATE n SET a = upper(a);
         SELECT * FROM n;
         CREATE TABLE r(a TEXT COLLATE RTRIM PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO r VALUES ('x', 1);
         INSERT INTO r VALUES ('x  ', 2);
         CREATE TABLE m(a, b COLLATE NOCASE, PRIMARY KEY(a, b COLLATE BINARY)) WITHOUT ROWID;
         INSERT INTO m VALUES (1, 'a'), (1, 'A'), (0, 'b');
         SELECT * FROM m;
         CREATE TABLE ty(a PRIMARY KEY) WITHOUT ROWID;
         INSERT INTO ty VALUES (1), (1.5), ('x'), (x'00'), (0.5), ('1');
         SELECT a, typeof(a) FROM ty;
         SELECT * FROM ty WHERE a > 1;
         CREATE TABLE ip(a INTEGER PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO ip VALUES ('abc', 1), (2.0, 3), ('5', 1);
         INSERT INTO ip(b) VALUES (1);
         SELECT a, typeof(a) FROM ip;
         CREATE TABLE rl(a REAL PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO rl VALUES (1, 1), (1.5, 2), ('2', 3);
         INSERT INTO rl VALUES (1.0, 9);
         SELECT a, typeof(a) FROM rl WHERE a = 1.0;
         CREATE TABLE c(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID;
         INSERT INTO c VALUES (1, 1, 'p'), (1, 2, 'q'), (2, 1, 'r'), (3, 3, 's');
         SELECT * FROM c WHERE a IN (3, 1) ORDER BY a DESC, b;
         SELECT * FROM c WHERE (a, b) > (1, 1);
         SELECT * FROM c WHERE a = 1 AND b = 2;
         SELECT (SELECT c FROM c WHERE a = 2 AND b = 1);
         SELECT * FROM c WHERE EXISTS (SELECT 1 FROM d WHERE d.a = c.a);
         CREATE VIEW v AS SELECT b, a FROM c WHERE a < 3;
         SELECT * FROM v;
         SELECT x.c, y.c FROM c AS x JOIN c AS y ON x.a = y.b AND x.b = y.a;
         SELECT * FROM c LEFT JOIN d ON d.a = c.b ORDER BY c.c, d.b;
         CREATE TABLE u AS SELECT * FROM c;
         SELECT rowid, * FROM u",
    );
    agree("order", &stmts, &|q| q.contains("JOIN"));
}

/// The pin scans these tables through its cheapest covering index —
/// the newest one on a tie, never a UNIQUE constraint's own index for
/// the key columns it appended (see `xplan`).
#[test]
fn scan_order_follows_the_pins_planner() {
    let stmts = lines(
        "CREATE TABLE t1(a INT, b TEXT, c, PRIMARY KEY(c, a)) WITHOUT ROWID;
         INSERT INTO t1 VALUES (1, 'x', 10), (2, 'y', 5), (3, 'a', 7);
         SELECT * FROM t1;
         CREATE INDEX t1b ON t1(b);
         SELECT * FROM t1;
         SELECT a FROM t1;
         SELECT c, a FROM t1 WHERE c > 5;
         CREATE TABLE t2(a PRIMARY KEY, b, c) WITHOUT ROWID;
         CREATE INDEX t2b ON t2(b);
         INSERT INTO t2 VALUES (1, 'z', 1), (2, 'a', 2), (3, 'm', 0);
         SELECT * FROM t2;
         SELECT a, b FROM t2;
         SELECT b FROM t2 WHERE b > 'b';
         SELECT * FROM t2 WHERE b = 'a';
         UPDATE t2 SET c = c + 1 RETURNING a;
         UPDATE t2 SET c = c + 1 WHERE b > 'a' RETURNING a;
         DELETE FROM t2 WHERE b > 'a' RETURNING a;
         CREATE TABLE t3(a PRIMARY KEY, b UNIQUE) WITHOUT ROWID;
         INSERT INTO t3 VALUES (1, 3), (2, 2), (3, 1);
         SELECT * FROM t3;
         SELECT b FROM t3;
         CREATE TABLE t4(x UNIQUE, a TEXT, b TEXT, c TEXT, PRIMARY KEY(a, b, c)) WITHOUT ROWID;
         INSERT INTO t4 VALUES (3, 'a', 'b', 'c'), (1, 'b', 'a', 'a'), (2, 'a', 'a', 'z');
         SELECT x FROM t4;
         SELECT x, a FROM t4;
         CREATE TABLE t5(a PRIMARY KEY DESC, b UNIQUE) WITHOUT ROWID;
         INSERT INTO t5 VALUES (1, 'q'), (2, 'q ');
         PRAGMA index_xinfo(sqlite_autoindex_t5_2);
         SELECT b FROM t5;
         CREATE TABLE t6(a TEXT PRIMARY KEY, b INT) WITHOUT ROWID;
         CREATE INDEX t6b ON t6(b DESC);
         INSERT INTO t6 VALUES ('x', 2), ('y', 1), ('z', 3);
         SELECT * FROM t6;
         SELECT * FROM t6 ORDER BY a;
         SELECT * FROM t6 ORDER BY b, a",
    );
    agree("planner", &stmts, &ordered);
}

// --------------------------------------------- indexes and schema

#[test]
fn secondary_indexes_and_pragmas() {
    let stmts = lines(
        "CREATE TABLE t(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID;
         CREATE INDEX i1 ON t(b);
         CREATE INDEX i2 ON t(c, a);
         CREATE INDEX i3 ON t(b COLLATE NOCASE);
         CREATE INDEX i4 ON t(b DESC, c);
         CREATE UNIQUE INDEX i5 ON t(c, b);
         CREATE INDEX i6 ON t(c * 2) WHERE c > 0;
         INSERT INTO t VALUES (1, 'x', 3), (2, 'Y', 1), (1, 'y', 2), (3, 'x', NULL), (2, 'a', -1);
         INSERT INTO t VALUES (9, 'x', 3);
         PRAGMA index_list(t);
         PRAGMA index_info(sqlite_autoindex_t_1);
         PRAGMA index_xinfo(sqlite_autoindex_t_1);
         PRAGMA index_xinfo(i1);
         PRAGMA index_xinfo(i2);
         PRAGMA index_xinfo(i3);
         PRAGMA index_xinfo(i4);
         PRAGMA index_xinfo(i5);
         PRAGMA index_xinfo(i6);
         PRAGMA table_info(t);
         PRAGMA table_xinfo(t);
         SELECT * FROM t WHERE b = 'x';
         SELECT * FROM t WHERE c > 1;
         SELECT * FROM t WHERE c * 2 = 4 AND c > 0;
         UPDATE t SET b = b || '!' WHERE c > 1;
         UPDATE t SET a = a + 10 WHERE b = 'a';
         SELECT * FROM t;
         PRAGMA integrity_check;
         CREATE TABLE g(a PRIMARY KEY, b, c AS (a + b), d AS (a * b) STORED, e) WITHOUT ROWID;
         CREATE INDEX gi ON g(c);
         INSERT INTO g(a, b, e) VALUES (1, 2, 'p'), (2, 2, 'q');
         SELECT * FROM g WHERE c = 4;
         UPDATE g SET b = 5 WHERE a = 1;
         PRAGMA index_xinfo(sqlite_autoindex_g_1);
         PRAGMA index_xinfo(gi);
         SELECT * FROM g;
         PRAGMA integrity_check;
         CREATE TABLE \"x y\"(a PRIMARY KEY, b) WITHOUT ROWID;
         CREATE INDEX xy ON \"x y\"(b);
         PRAGMA index_list(\"x y\");
         SELECT name FROM pragma_index_list('x y')",
    );
    agree("indexes", &stmts, &ordered);
}

#[test]
fn alter_drop_vacuum_reindex_analyze() {
    let stmts = lines(
        "CREATE TABLE t(a, b, c, d, PRIMARY KEY(b, a)) WITHOUT ROWID;
         CREATE INDEX ic ON t(c);
         INSERT INTO t VALUES (1, 2, 3, 0), (4, 5, 6, 1), (0, 1, 1, 0);
         ALTER TABLE t ADD COLUMN e DEFAULT 7;
         INSERT INTO t VALUES (2, 2, 2, 2, 2);
         SELECT * FROM t;
         ALTER TABLE t ADD COLUMN f PRIMARY KEY;
         ALTER TABLE t ADD COLUMN g NOT NULL;
         ALTER TABLE t DROP COLUMN a;
         ALTER TABLE t DROP COLUMN c;
         ALTER TABLE t DROP COLUMN d;
         SELECT * FROM t;
         SELECT sql FROM sqlite_schema;
         PRAGMA integrity_check;
         ALTER TABLE t RENAME COLUMN a TO z;
         ALTER TABLE t RENAME TO u;
         SELECT sql FROM sqlite_schema;
         PRAGMA index_list(u);
         SELECT * FROM u WHERE c = 6;
         REINDEX u;
         REINDEX ic;
         REINDEX;
         SELECT * FROM u WHERE c = 6;
         PRAGMA integrity_check;
         ANALYZE;
         SELECT * FROM sqlite_stat1 ORDER BY idx;
         VACUUM;
         SELECT * FROM u;
         PRAGMA integrity_check;
         DROP TABLE u;
         PRAGMA integrity_check;
         PRAGMA freelist_count;
         SELECT count(*) FROM sqlite_schema",
    );
    agree("alter", &stmts, &ordered);
}

#[test]
fn triggers_foreign_keys_and_transactions() {
    let stmts = lines(
        "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID;
         CREATE TABLE log(x);
         CREATE TRIGGER tb BEFORE INSERT ON t BEGIN INSERT INTO log VALUES ('b' || NEW.a); END;
         CREATE TRIGGER td AFTER DELETE ON t BEGIN INSERT INTO log VALUES ('d' || OLD.a); END;
         CREATE TRIGGER tu AFTER UPDATE ON t BEGIN INSERT INTO log VALUES (OLD.a || '>' || NEW.a); END;
         INSERT INTO t VALUES (2, 1), (1, 1);
         REPLACE INTO t VALUES (1, 5);
         UPDATE t SET a = a * 10;
         DELETE FROM t WHERE a = 10;
         SELECT * FROM log;
         SELECT * FROM t;
         PRAGMA foreign_keys = ON;
         CREATE TABLE p(a TEXT COLLATE NOCASE PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE c(x REFERENCES p ON UPDATE CASCADE ON DELETE SET NULL);
         INSERT INTO p VALUES ('k'), ('m');
         INSERT INTO c VALUES ('k'), ('K'), ('m');
         INSERT INTO c VALUES ('q');
         UPDATE p SET a = 'z' WHERE a = 'k';
         SELECT * FROM c;
         DELETE FROM p WHERE a = 'm';
         SELECT * FROM c;
         CREATE TABLE p2(a, b, PRIMARY KEY(b DESC, a)) WITHOUT ROWID;
         CREATE TABLE c2(x PRIMARY KEY, y, z, FOREIGN KEY(y, z) REFERENCES p2(a, b) ON DELETE CASCADE) WITHOUT ROWID;
         INSERT INTO p2 VALUES (1, 2), (3, 4);
         INSERT INTO c2 VALUES (10, 1, 2), (20, 3, 4), (30, 1, 2);
         INSERT INTO c2 VALUES (40, 1, 4);
         DELETE FROM p2 WHERE a = 1;
         SELECT * FROM c2;
         PRAGMA foreign_key_check;
         PRAGMA foreign_keys = OFF;
         BEGIN;
         INSERT INTO t VALUES (7, 7);
         SAVEPOINT s;
         UPDATE t SET a = 70 WHERE a = 7;
         INSERT INTO t VALUES (8, 8);
         ROLLBACK TO s;
         RELEASE s;
         COMMIT;
         SELECT * FROM t;
         BEGIN;
         DELETE FROM t;
         ROLLBACK;
         SELECT * FROM t;
         PRAGMA integrity_check",
    );
    agree("trigfk", &stmts, &ordered);
}

// ---------------------------------------------------------- overflow

/// Rows and keys past a page's local payload: index-cell overflow
/// rules (not the table-cell ones), both ways.
#[test]
fn overflow_rows_and_keys() {
    let stmts = lines(
        "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO t VALUES (1, zeroblob(0)), (2, zeroblob(5000)), (3, printf('%.*c', 3000, 'x')), (4, printf('%.*c', 1100, 'y'));
         SELECT a, length(b) FROM t;
         UPDATE t SET b = b || b WHERE a > 2;
         SELECT a, length(b), substr(b, 1000, 3) FROM t;
         CREATE TABLE k(a PRIMARY KEY, b) WITHOUT ROWID;
         CREATE INDEX kb ON k(b);
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 60) INSERT INTO k SELECT printf('%.*c', 900 + i * 37, char(65 + i % 26)) || i, i FROM c;
         SELECT count(*), sum(length(a)) FROM k;
         DELETE FROM k WHERE b % 3 = 0;
         UPDATE k SET a = a || 'tail' WHERE b % 5 = 0;
         SELECT b, length(a) FROM k WHERE b < 20;
         SELECT b FROM k WHERE a > 'M';
         PRAGMA integrity_check",
    );
    agree("overflow", &stmts, &ordered);
}

// ------------------------------------------------------------ files

/// A file the pin wrote (composite collated DESC key, a UNIQUE
/// constraint, explicit, partial and generated-column indexes,
/// overflowing rows) is read and written by hipp as the pin would;
/// the pin then reads hipp's result and checks it.
#[test]
fn pin_files_round_trip() {
    let o = fresh("pinmade-o");
    let h = fresh("pinmade-h");
    common::cli(
        &o,
        "CREATE TABLE t(a TEXT COLLATE NOCASE, b INT, c, d AS (b * 2), e BLOB, PRIMARY KEY(b DESC, a), UNIQUE(c)) WITHOUT ROWID;
         CREATE INDEX i1 ON t(c DESC, a);
         CREATE INDEX i2 ON t(e) WHERE e IS NOT NULL;
         CREATE INDEX i3 ON t(d);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
         INSERT INTO t(a, b, c, e) SELECT char(65 + i % 26) || i, i % 37, i * 3,
           CASE WHEN i % 4 = 0 THEN zeroblob(i * 10) END FROM n;",
    );
    std::fs::copy(&o, &h).expect("copy");
    let writes = lines(
        "UPDATE t SET b = b + 100 WHERE b % 5 = 0;
         DELETE FROM t WHERE c % 7 = 0;
         INSERT INTO t(a, b, c, e) VALUES ('zz', 1, -1, zeroblob(3000));
         REPLACE INTO t(a, b, c) VALUES ('a1', 1, 3);
         INSERT INTO t(a, b, c) VALUES ('A1', 1, 99999);
         INSERT OR IGNORE INTO t(a, b, c) VALUES ('new', 2, 6);
         UPDATE t SET a = lower(a) WHERE b = 3;
         UPDATE OR REPLACE t SET c = 30 WHERE a = 'b25';
         INSERT INTO t(a, b, c) VALUES ('B25', 25, 1) ON CONFLICT DO UPDATE SET e = x'ff'",
    );
    let got = hipp_file(&h, &writes);
    let want = oracle_file(&o, &writes);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.is_ok(), w.is_some(), "{}: {g:?}", writes[i]);
    }
    both_ok(&h);
    for q in [
        "SELECT a, b, c, d, hex(e) FROM t ORDER BY b DESC, a",
        "SELECT a, b, c, d, length(e) FROM t",
        "SELECT a FROM t INDEXED BY i1 WHERE c > 1000 ORDER BY c DESC, a",
        "SELECT count(*) FROM t INDEXED BY i2 WHERE e IS NOT NULL",
        "SELECT d, a FROM t INDEXED BY i3 WHERE d > 60",
    ] {
        assert_eq!(common::cli(&h, q), common::cli(&o, q), "{q}");
    }
    let reads = lines(
        "SELECT a, b, d FROM t WHERE b = 5 ORDER BY a;
         SELECT a FROM t WHERE c = 30;
         SELECT b, count(*) FROM t GROUP BY b;
         SELECT * FROM t WHERE a = 'C2' AND b = 2;
         SELECT a, c FROM t ORDER BY c LIMIT 5;
         PRAGMA index_list(t);
         PRAGMA index_xinfo(sqlite_autoindex_t_1);
         PRAGMA index_xinfo(sqlite_autoindex_t_2);
         PRAGMA index_xinfo(i1)",
    );
    let got = hipp_file(&h, &reads);
    let want = oracle_file(&h, &reads);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.as_ref().ok(), w.as_ref(), "{}", reads[i]);
    }
}

/// A file hipp wrote reads on the pin as the pin's own file does,
/// with the pin's integrity_check passing, and VACUUM on either side
/// keeps it so.
#[test]
fn hipp_files_read_by_pin() {
    let h = fresh("hippmade-h");
    let o = fresh("hippmade-o");
    let stmts = lines(
        "CREATE TABLE t(a INT, b TEXT COLLATE NOCASE, c, d BLOB, PRIMARY KEY(b, a DESC)) WITHOUT ROWID;
         CREATE UNIQUE INDEX tu ON t(c);
         CREATE INDEX td ON t(d, b);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500) INSERT INTO t SELECT i % 50, char(97 + i % 26) || (i / 26), i * 7, CASE WHEN i % 9 = 0 THEN randomblob(1500) ELSE x'01' END FROM n;
         DELETE FROM t WHERE a % 4 = 1;
         UPDATE t SET a = a + 1000 WHERE c % 3 = 0;
         UPDATE t SET b = upper(b) WHERE a = 2;
         INSERT INTO t VALUES (1, 'k', -5, NULL)",
    );
    let got = hipp_file(&h, &stmts);
    let want = oracle_file(&o, &stmts);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.is_ok(), w.is_some(), "{}: {g:?}", stmts[i]);
    }
    both_ok(&h);
    for q in [
        "SELECT a, b, c, length(d) FROM t",
        "SELECT a, b FROM t ORDER BY b, a DESC",
        "SELECT c FROM t WHERE c > 3000",
        "SELECT count(*), sum(a), sum(c) FROM t",
        "SELECT b FROM t INDEXED BY td WHERE d = x'01' LIMIT 20",
    ] {
        let hq = common::cli(&h, q);
        let oq = common::cli(&o, q);
        // randomblob() differs between the engines; only lengths and
        // keys are compared.
        assert_eq!(hq, oq, "{q}");
    }
    common::cli(&h, "VACUUM;");
    both_ok(&h);
    let got = hipp_file(&h, &lines("VACUUM;\n SELECT count(*), sum(c) FROM t"));
    assert_eq!(
        got[1].as_ref().ok().cloned(),
        Some(
            common::cli(&o, "SELECT count(*), sum(c) FROM t;")
                .trim()
                .as_bytes()
                .to_vec()
        )
    );
    both_ok(&h);
}

// ---------------------------------------------------------- corpora

/// One random WITHOUT ROWID schema: its CREATE statements and the
/// column names.
fn gen_schema(r: &mut Rng, name: &str) -> (Vec<String>, Vec<&'static str>) {
    let cols = ["a", "b", "c", "d"];
    let mut defs: Vec<String> = Vec::new();
    for c in cols {
        let ty = r.pick(&["", "INT", "TEXT", "REAL", "NUMERIC", "BLOB"]);
        let coll = match r.below(8) {
            0 => " COLLATE NOCASE",
            1 => " COLLATE RTRIM",
            _ => "",
        };
        defs.push(format!("{c} {ty}{coll}"));
    }
    let mut pool: Vec<&str> = cols.to_vec();
    let nk = 1 + r.below(3) as usize;
    let mut key = Vec::new();
    for _ in 0..nk {
        let i = r.below(pool.len() as u64) as usize;
        let c = pool.remove(i);
        let coll = match r.below(10) {
            0 => " COLLATE NOCASE",
            1 => " COLLATE BINARY",
            _ => "",
        };
        let dir = r.pick(&["", "", " DESC", " ASC"]);
        key.push(format!("{c}{coll}{dir}"));
    }
    if r.chance(0.2) {
        // A repeated key column (dropped, or kept under another
        // collation).
        let k0 = key[0].split(' ').next().unwrap_or("a").to_string();
        key.push(format!("{k0}{}", r.pick(&["", " COLLATE NOCASE"])));
    }
    let mut out = Vec::new();
    let mut extra = String::new();
    if r.chance(0.3) {
        extra = format!(", UNIQUE({})", pool[r.below(pool.len() as u64) as usize]);
    }
    out.push(format!(
        "CREATE TABLE {name}({}, PRIMARY KEY({}){extra}) WITHOUT ROWID",
        defs.join(", "),
        key.join(", ")
    ));
    for i in 0..r.below(3) {
        let k = 1 + r.below(2) as usize;
        let mut items = Vec::new();
        for _ in 0..k {
            let c = r.pick(&cols);
            items.push(format!(
                "{c}{}{}",
                r.pick(&["", "", " COLLATE NOCASE"]),
                r.pick(&["", "", " DESC"])
            ));
        }
        let u = if r.chance(0.25) { "UNIQUE " } else { "" };
        out.push(format!(
            "CREATE {u}INDEX {name}i{i} ON {name}({})",
            items.join(", ")
        ));
    }
    (out, cols.to_vec())
}

fn gen_where(r: &mut Rng, cols: &[&str]) -> String {
    let c = r.pick(cols);
    let v1 = gen_val(r);
    let v2 = gen_val(r);
    match r.below(7) {
        0 => format!("{c} > {v1}"),
        1 => format!("{c} = {v1}"),
        2 => format!("{c} IN ({v1}, {v2})"),
        3 => format!("{c} BETWEEN {v1} AND {v2}"),
        4 => format!("{c} IS NULL"),
        5 => format!("{c} = {v1} OR {c} = {v2}"),
        _ => "1".into(),
    }
}

/// Random schemas and write sequences: every statement's rows, row
/// order (scans included) and errors agree with the pin, and the pin
/// checks hipp's file.
#[test]
fn generated_schemas_and_writes() {
    let n = corpus_size(120);
    let mut r = Rng(0x5eed_0f00_0001_2345);
    for case in 0..n {
        let (mut s, cols) = gen_schema(&mut r, "t");
        for _ in 0..4 + r.below(10) {
            let v: Vec<String> = (0..4).map(|_| gen_val(&mut r)).collect();
            let verb = r.pick(&["INSERT", "INSERT OR IGNORE", "INSERT OR REPLACE", "REPLACE"]);
            s.push(format!(
                "{verb} INTO t(a, b, c, d) VALUES ({})",
                v.join(", ")
            ));
        }
        for _ in 0..10 {
            let w = gen_where(&mut r, &cols);
            let c = r.pick(&cols);
            let c2 = r.pick(&cols);
            match r.below(16) {
                0 | 1 => {
                    let v: Vec<String> = (0..4).map(|_| gen_val(&mut r)).collect();
                    let verb =
                        r.pick(&["INSERT", "INSERT OR IGNORE", "INSERT OR REPLACE", "REPLACE"]);
                    s.push(format!(
                        "{verb} INTO t(a, b, c, d) VALUES ({})",
                        v.join(", ")
                    ));
                }
                2 => {
                    let v: Vec<String> = (0..4).map(|_| gen_val(&mut r)).collect();
                    s.push(format!(
                        "INSERT INTO t(a, b, c, d) VALUES ({}) ON CONFLICT DO UPDATE SET {c2} = {} RETURNING *",
                        v.join(", "),
                        gen_val(&mut r)
                    ));
                }
                3 => {
                    let v: Vec<String> = (0..4).map(|_| gen_val(&mut r)).collect();
                    s.push(format!(
                        "INSERT INTO t(a, b, c, d) VALUES ({}) ON CONFLICT DO NOTHING",
                        v.join(", ")
                    ));
                }
                4..=6 => {
                    let act = r.pick(&["", "OR IGNORE ", "OR REPLACE ", "OR FAIL "]);
                    let set = match r.below(3) {
                        0 => gen_val(&mut r),
                        1 => format!("{c2} + 1"),
                        _ => format!("{c2} || 'x'"),
                    };
                    let ret = if r.chance(0.3) { " RETURNING *" } else { "" };
                    s.push(format!("UPDATE {act}t SET {c} = {set} WHERE {w}{ret}"));
                }
                7 => s.push(format!(
                    "DELETE FROM t WHERE {w}{}",
                    if r.chance(0.5) { " RETURNING *" } else { "" }
                )),
                8..=10 => s.push("SELECT * FROM t".into()),
                11 => s.push(format!("SELECT * FROM t WHERE {w}")),
                12 => s.push(format!(
                    "SELECT {c}, count(*) FROM t WHERE {w} GROUP BY {c}"
                )),
                13 => s.push(format!(
                    "SELECT * FROM t ORDER BY {c}{} LIMIT {}",
                    r.pick(&["", " DESC"]),
                    1 + r.below(4)
                )),
                14 => s.push(format!("SELECT {c}, {c2} FROM t")),
                _ => s.push(format!("SELECT count(*), max({c}), min({c2}) FROM t")),
            }
        }
        s.push("SELECT * FROM t".into());
        s.push("PRAGMA integrity_check".into());
        agree(&format!("gen{case}"), &s, &ordered);
    }
}

/// Files cross between the engines mid-sequence: the pin writes a
/// random WITHOUT ROWID table, hipp continues the writes, the pin
/// checks and reads the result — and the other way round.
#[test]
fn generated_interop() {
    let n = corpus_size(120) / 4;
    let mut r = Rng(0x1fee_d0c5_7777_0001);
    for case in 0..n {
        let (schema, cols) = gen_schema(&mut r, "t");
        let mut a: Vec<String> = schema.clone();
        let mut b: Vec<String> = Vec::new();
        for i in 0..30 + r.below(60) {
            let v: Vec<String> = (0..4).map(|_| gen_val(&mut r)).collect();
            let st = if i % 7 == 6 {
                format!(
                    "UPDATE OR IGNORE t SET {} = {} WHERE {}",
                    r.pick(&cols),
                    gen_val(&mut r),
                    gen_where(&mut r, &cols)
                )
            } else if i % 11 == 10 {
                format!("DELETE FROM t WHERE {}", gen_where(&mut r, &cols))
            } else {
                format!(
                    "INSERT OR REPLACE INTO t(a, b, c, d) VALUES ({})",
                    v.join(", ")
                )
            };
            if i % 2 == 0 { a.push(st) } else { b.push(st) }
        }
        let checks = lines(
            "SELECT * FROM t;
             SELECT count(*) FROM t;
             PRAGMA integrity_check",
        );
        // pin -> hipp -> pin
        let o = fresh(&format!("io{case}-o"));
        let h = fresh(&format!("io{case}-h"));
        let _ = oracle_file(&o, &a);
        std::fs::copy(&o, &h).expect("copy");
        let gb = hipp_file(&h, &b);
        let wb = oracle_file(&o, &b);
        for (i, (g, w)) in gb.iter().zip(wb.iter()).enumerate() {
            assert_eq!(g.is_ok(), w.is_some(), "case {case} {}: {g:?}", b[i]);
        }
        assert_eq!(
            oracle_file(&h, &checks),
            oracle_file(&o, &checks),
            "case {case}: pin reads"
        );
        // hipp -> pin -> hipp
        let h2 = fresh(&format!("io{case}-h2"));
        let o2 = fresh(&format!("io{case}-o2"));
        let _ = hipp_file(&h2, &a);
        let _ = oracle_file(&o2, &a);
        assert_eq!(
            common::cli(&h2, "PRAGMA integrity_check;").trim(),
            "ok",
            "case {case}"
        );
        let _ = oracle_file(&h2, &b);
        let _ = oracle_file(&o2, &b);
        let got = hipp_file(&h2, &checks);
        let want = oracle_file(&o2, &checks);
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert_eq!(g.as_ref().ok(), w.as_ref(), "case {case}: {}", checks[i]);
        }
    }
}

// ------------------------------------------------------- regressions

/// Bugs found while building the unit, each pinned.
#[test]
fn regressions() {
    let stmts = lines(
        // REINDEX of a table rebuilt its clustered tree as an index
        // (and emptied it).
        "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID;
         CREATE INDEX tb ON t(b);
         INSERT INTO t VALUES (1, 3), (2, 1);
         REINDEX t;
         SELECT * FROM t WHERE b = 1;
         SELECT * FROM t;
         -- DROP TABLE freed the tree twice (once as the key's index).
         CREATE TABLE d(a PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO d VALUES (1, 1), (2, 2);
         DROP TABLE d;
         PRAGMA freelist_count;
         PRAGMA integrity_check;
         -- CREATE INDEX filled a new index with rowid entries.
         CREATE TABLE c(a INT, b TEXT, c, PRIMARY KEY(c, a)) WITHOUT ROWID;
         INSERT INTO c VALUES (1, 'x', 10), (2, 'y', 5);
         CREATE INDEX cb ON c(b);
         INSERT INTO c VALUES (3, 'a', 7);
         SELECT * FROM c WHERE b = 'a';
         PRAGMA integrity_check;
         -- DROP COLUMN rewrote rows by the old layout.
         CREATE TABLE x(a PRIMARY KEY, b, c) WITHOUT ROWID;
         INSERT INTO x VALUES (0, 1, 0), (1, 0, 1);
         ALTER TABLE x DROP COLUMN b;
         SELECT * FROM x;
         -- ANALYZE names the key's statistics by the table.
         ANALYZE;
         SELECT tbl, idx FROM sqlite_stat1 ORDER BY tbl, idx;
         -- An UPDATE that moves a row onto a REPLACE victim's key
         -- processes each row once.
         CREATE TABLE m(a PRIMARY KEY, b) WITHOUT ROWID;
         INSERT INTO m VALUES (1, 1), (2, 2), (3, 3);
         UPDATE OR REPLACE m SET a = a + 1 RETURNING *;
         SELECT * FROM m;
         PRAGMA integrity_check;
         -- `IS NULL` through a UNIQUE index finds many rows: their
         -- GROUP BY groups are sorted (also for a rowid table; the
         -- planner took the lookup for a one-row loop).
         CREATE TABLE n(k PRIMARY KEY, a UNIQUE, b) WITHOUT ROWID;
         INSERT INTO n VALUES (1, NULL, x'01'), (2, NULL, -2), (3, NULL, 5);
         SELECT b, count(*) FROM n WHERE a IS NULL GROUP BY b;
         CREATE TABLE nr(a UNIQUE, b);
         INSERT INTO nr VALUES (NULL, x'01'), (NULL, -2), (NULL, 5);
         SELECT b, count(*) FROM nr WHERE a IS NULL GROUP BY b;
         -- A one-pass UPDATE OR IGNORE of the key is two-pass on the
         -- pin (key order), not the order of the index it scans.
         CREATE TABLE o(a INT, b INT, c, PRIMARY KEY(a), UNIQUE(c)) WITHOUT ROWID;
         CREATE INDEX ob ON o(b);
         INSERT INTO o VALUES (1, 30, 'x'), (2, 10, 'y'), (3, 20, 'z');
         UPDATE OR IGNORE o SET a = 9 WHERE b > 0;
         UPDATE OR IGNORE o SET c = 'w' WHERE a > 1;
         SELECT * FROM o;
         -- One-byte records take a 4-byte cell, as on the pin.
         CREATE TABLE tiny(a PRIMARY KEY) WITHOUT ROWID;
         INSERT INTO tiny VALUES (0), (1), (2), (NULL);
         INSERT INTO tiny VALUES (3);
         DELETE FROM tiny WHERE a = 1;
         SELECT * FROM tiny;
         PRAGMA integrity_check",
    );
    agree("regress", &stmts, &ordered);
}
