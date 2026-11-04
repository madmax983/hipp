//! v0.16 expression indexes, partial indexes and index-aware planning
//! (leaf feature `expr_index`), differentially vs the pinned oracle
//! (SQLite 3.54.1). The semantics were probed on the pin before
//! implementation (STATUS.md, "Expression indexes, partial indexes
//! and index-aware planning"); the matrix the tests encode:
//!
//! - Partial indexes (`CREATE [UNIQUE] INDEX ... WHERE`) and
//!   expression indexes (`ON t(a + b, lower(c) COLLATE NOCASE DESC)`)
//!   are maintained by every write path — INSERT, UPDATE, DELETE,
//!   OR REPLACE / REPLACE, upsert — UNIQUE is enforced over the rows
//!   a partial index's WHERE admits, and an upsert target names an
//!   expression index by its expression and a partial index by its
//!   WHERE (the same expression). The pin's `integrity_check` passes
//!   on every file hipp wrote, and hipp reads and writes files the pin
//!   made.
//! - The pin's definition errors (subqueries, non-deterministic
//!   functions, aggregates, window calls, qualified or rowid
//!   references in key expressions, unknown columns, row values)
//!   are errors in hipp too; PRAGMA index_list / index_info /
//!   index_xinfo (cid -2, no name), integrity_check, REINDEX, VACUUM
//!   and ALTER TABLE RENAME / RENAME COLUMN / DROP COLUMN answer as
//!   the pin.
//! - Planning never changes a result, only the row order, which
//!   matches the pin's wherever its choice follows from the schema
//!   and the query: single-table equality / IN / range / IS NULL
//!   lookups through the rowid and every kind of index, covering
//!   index scans, ORDER BY delivered by an index (ties in the index's
//!   direction), GROUP BY groups in index order, LIMIT, the first row
//!   of a subquery, and an UPDATE's processing order (one-pass in
//!   index order, else rowid order). Joins compare order-insensitively
//!   (the pin may reorder them).
//! - Generated corpora (xorshift PRNG) of random schemas, indexes,
//!   data, queries and writes, against the live pin; and a
//!   100,000-row table whose point lookups through an index run far
//!   faster than full scans.

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
    common::temp_db(&format!("v16xi-{tag}"))
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
    std::env::var("HIPP_EXPRIDX_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ------------------------------------------------------------ writes

#[test]
fn partial_index_maintenance() {
    let stmts = lines(
        "CREATE TABLE t(a, b, c);
         CREATE UNIQUE INDEX u1 ON t(a) WHERE b > 0;
         CREATE INDEX p2 ON t(c) WHERE c IS NOT NULL AND a > 1;
         INSERT INTO t VALUES (1, 1, 'x');
         INSERT INTO t VALUES (1, 0, 'y');
         INSERT INTO t VALUES (1, -1, 'z');
         INSERT INTO t VALUES (1, 2, 'w');
         INSERT INTO t VALUES (2, 2, 'W'), (3, 3, NULL), (4, -4, 'Q');
         SELECT * FROM t;
         UPDATE t SET b = 5 WHERE c = 'y';
         UPDATE t SET b = -5 WHERE c = 'x';
         UPDATE t SET b = 5 WHERE c = 'y';
         INSERT OR REPLACE INTO t VALUES (1, 3, 'r');
         SELECT rowid, * FROM t;
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT (a) DO UPDATE SET c = 'upd';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT (a) WHERE b > 0 DO UPDATE SET c = 'upd';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT (a) WHERE b > 1 DO UPDATE SET c = 'upd2';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT (a) WHERE 0 < b DO UPDATE SET c = 'upd3';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT (a) WHERE (b > 0) DO UPDATE SET c = 'upd4';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT DO UPDATE SET c = 'upd5';
         INSERT INTO t VALUES (1, 9, 'u') ON CONFLICT DO NOTHING;
         INSERT OR IGNORE INTO t VALUES (1, 7, 'ig');
         REPLACE INTO t VALUES (4, 8, 'rep');
         DELETE FROM t WHERE a = 4;
         UPDATE t SET a = a + 10 WHERE b <= 0;
         UPDATE OR REPLACE t SET b = 1 WHERE c = 'z';
         SELECT rowid, * FROM t;
         SELECT a FROM t WHERE b > 0 AND a = 1;
         PRAGMA integrity_check;
         CREATE UNIQUE INDEX bad ON t(b) WHERE a > 0;
         CREATE UNIQUE INDEX good ON t(b) WHERE a > 100;
         SELECT name FROM sqlite_schema ORDER BY name",
    );
    let errs = agree("partial", &stmts, &ordered);
    assert!(errs >= 4, "the pin's UNIQUE / upsert-target errors: {errs}");
}

#[test]
fn expression_index_maintenance() {
    let stmts = lines(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d AS (b || '!') VIRTUAL);
         CREATE INDEX e1 ON t(lower(b) COLLATE NOCASE DESC, c);
         CREATE UNIQUE INDEX e2 ON t(c * 2) WHERE c > 0;
         CREATE INDEX e3 ON t(d, a + 1) WHERE b IS NOT NULL;
         CREATE INDEX e4 ON t(length(b) * 2) WHERE length(b) > 1;
         INSERT INTO t(a, b, c) VALUES (1, 'X', 1), (2, 'y', -1), (3, NULL, 2), (4, 'kk', -1);
         INSERT INTO t(b, c) VALUES ('Zed', 5), ('q', -9);
         INSERT INTO t(b, c) VALUES ('dup', 2.5);
         INSERT INTO t(b, c) VALUES ('dup', 2.0);
         UPDATE t SET c = c + 10 WHERE a = 2;
         UPDATE t SET b = upper(b);
         DELETE FROM t WHERE a = 1;
         INSERT OR REPLACE INTO t(a, b, c) VALUES (9, 'r', 2);
         REPLACE INTO t(b, c) VALUES ('s', 15);
         INSERT INTO t(b, c) VALUES ('up', 15) ON CONFLICT (c * 2) WHERE c > 0 DO UPDATE SET b = 'hit';
         INSERT INTO t(b, c) VALUES ('up', 15) ON CONFLICT (c * 2) DO UPDATE SET b = 'miss';
         INSERT INTO t(b, c) VALUES ('up', 15) ON CONFLICT (c + c) WHERE c > 0 DO UPDATE SET b = 'miss';
         SELECT a, b, c, d FROM t;
         CREATE TABLE s(x, y);
         CREATE UNIQUE INDEX ue ON s(x + y);
         INSERT INTO s VALUES (1, 1), (2, 0);
         INSERT INTO s VALUES (NULL, 2), (NULL, 3);
         INSERT INTO s VALUES (0, 2) ON CONFLICT (x + y) DO UPDATE SET x = x * 10;
         INSERT INTO s VALUES (0, 2) ON CONFLICT ((x + y)) DO UPDATE SET x = x * 10;
         INSERT INTO s VALUES (0, 2) ON CONFLICT (y + x) DO UPDATE SET x = x * 10;
         INSERT INTO s VALUES (0, 2) ON CONFLICT (x + y COLLATE NOCASE) DO UPDATE SET x = x * 10;
         INSERT INTO s VALUES (0, 2) ON CONFLICT (x + y COLLATE BINARY) DO UPDATE SET x = -1;
         UPDATE OR IGNORE s SET y = 0 WHERE y = 2;
         SELECT rowid, * FROM s;
         CREATE TABLE j(doc);
         CREATE INDEX jx ON j(doc ->> '$.k');
         INSERT INTO j VALUES ('{\"k\":3}'), ('{\"k\":1}');
         INSERT INTO j VALUES ('not json');
         SELECT doc FROM j WHERE doc ->> '$.k' = 1;
         PRAGMA integrity_check",
    );
    agree("expr", &stmts, &ordered);
}

#[test]
fn definition_errors() {
    let mut stmts = vec!["CREATE TABLE t(a, b, c, d INTEGER PRIMARY KEY)".to_string()];
    for d in [
        "CREATE INDEX x ON t(a) WHERE (SELECT 1)",
        "CREATE INDEX x ON t(a) WHERE random() > 0",
        "CREATE INDEX x ON t(a) WHERE zz > 0",
        "CREATE INDEX x ON t(a) WHERE max(b) > 0",
        "CREATE INDEX x ON t(a) WHERE b IN (SELECT 1)",
        "CREATE INDEX x ON t(a) WHERE exists (SELECT 1)",
        "CREATE INDEX x ON t(random())",
        "CREATE INDEX x ON t((SELECT 1))",
        "CREATE INDEX x ON t(max(a))",
        "CREATE INDEX x ON t(t.a)",
        "CREATE INDEX x ON t(rowid + 1)",
        "CREATE INDEX x ON t(a, rowid)",
        "CREATE INDEX x ON t('zz')",
        "CREATE INDEX x ON t(row_number() OVER ())",
        "CREATE INDEX x ON t(sqlite_version())",
        "CREATE INDEX x ON t(changes())",
        "CREATE INDEX x ON t((a, a))",
        "CREATE INDEX x ON t(nosuchfn(a))",
        "CREATE INDEX x ON t(a COLLATE nosuch)",
        "CREATE INDEX x ON t(abs(a, b))",
    ] {
        stmts.push(d.to_string());
    }
    // Accepted by both, with the pin's stored text.
    for ok in [
        "CREATE INDEX o1 ON t(a) WHERE t.b > 0",
        "CREATE INDEX o2 ON t(a) WHERE rowid > 0",
        "CREATE INDEX o3 ON t((a))",
        "CREATE INDEX o4 ON t(1)",
        "CREATE INDEX o5 ON t(a) WHERE 1",
        "CREATE INDEX o6 ON t(a COLLATE nocase + 1)",
        "CREATE INDEX o7 ON t(abs(a)) WHERE abs(a) > 1",
        "CREATE INDEX o8 ON t(a, a + 0)",
        "CREATE INDEX o9 ON t('a')",
        "CREATE INDEX o10 ON t(b) WHERE a IN (1, 2)",
        "CREATE INDEX o11 ON t(b) WHERE a LIKE 'x%'",
        "CREATE INDEX o12 ON t(CAST(a AS TEXT))",
        "CREATE INDEX o13 ON t(a) WHERE b",
        "CREATE INDEX o13b ON t(a) WHERE (a, a) = (1, 1)",
        "CREATE INDEX o14 ON t(+a)",
        "CREATE INDEX o15 ON t(d + 0, (d))",
        "CREATE INDEX o16 ON t(coalesce(a, 1), iif(a, 1, 2), CASE WHEN a THEN 1 END, a BETWEEN 1 AND 2, -a, NOT a)",
        "CREATE INDEX IF NOT EXISTS o16 ON t(a + 1)",
        "SELECT name, sql FROM sqlite_schema ORDER BY name",
    ] {
        stmts.push(ok.to_string());
    }
    let errs = agree("defs", &stmts, &ordered);
    assert_eq!(errs, 20);
}

#[test]
fn introspection() {
    let stmts = lines(
        "CREATE TABLE t(a, b, c, d INTEGER PRIMARY KEY);
         CREATE INDEX i10 ON t(a + b);
         CREATE INDEX i16 ON t((a));
         CREATE INDEX i17 ON t(lower(c) COLLATE NOCASE DESC, a);
         CREATE INDEX i18 ON t(1);
         CREATE INDEX i40 ON t((d));
         CREATE INDEX i41 ON t(d + 0);
         CREATE INDEX i42 ON t(a COLLATE NOCASE);
         CREATE INDEX i43 ON t((a COLLATE NOCASE));
         CREATE INDEX i44 ON t(+a);
         CREATE UNIQUE INDEX i1 ON t(a) WHERE b > 0;
         PRAGMA index_list(t);
         PRAGMA index_xinfo(i10);
         PRAGMA index_xinfo(i16);
         PRAGMA index_xinfo(i17);
         PRAGMA index_info(i17);
         PRAGMA index_xinfo(i18);
         PRAGMA index_xinfo(i40);
         PRAGMA index_xinfo(i41);
         PRAGMA index_xinfo(i42);
         PRAGMA index_xinfo(i43);
         PRAGMA index_xinfo(i44);
         PRAGMA index_info(i1);
         SELECT * FROM pragma_index_xinfo('i17');
         INSERT INTO t VALUES (1, 2, 'X', 1), (2, 0, 'y', 2), (NULL, 5, NULL, 3);
         PRAGMA integrity_check;
         PRAGMA quick_check;
         PRAGMA integrity_check(t);
         ANALYZE;
         SELECT * FROM sqlite_stat1 ORDER BY idx",
    );
    agree("intro", &stmts, &ordered);
}

#[test]
fn alter_reindex_vacuum() {
    let stmts = lines(
        "CREATE TABLE t(a, b, c);
         CREATE INDEX e1 ON t(a + b);
         CREATE INDEX p1 ON t(c) WHERE a > 0;
         CREATE INDEX e2 ON t(lower(c)) WHERE b IS NOT NULL;
         INSERT INTO t VALUES (1, 2, 'X'), (-1, 3, 'Y'), (2, NULL, 'Z');
         ALTER TABLE t RENAME COLUMN a TO aa;
         SELECT sql FROM sqlite_schema ORDER BY name;
         ALTER TABLE t RENAME TO u;
         SELECT sql FROM sqlite_schema ORDER BY name;
         ALTER TABLE u DROP COLUMN aa;
         ALTER TABLE u DROP COLUMN b;
         ALTER TABLE u DROP COLUMN c;
         ALTER TABLE u ADD COLUMN d;
         CREATE INDEX e3 ON u(d * 2);
         ALTER TABLE u DROP COLUMN d;
         DROP INDEX e3;
         ALTER TABLE u DROP COLUMN d;
         INSERT INTO u VALUES (5, 6, 'W');
         UPDATE u SET aa = aa * 2;
         REINDEX e1;
         REINDEX u;
         REINDEX;
         PRAGMA integrity_check;
         SELECT * FROM u WHERE aa + b > 3;
         VACUUM;
         PRAGMA integrity_check;
         CREATE TABLE \"x y\"(\"q r\", s);
         CREATE INDEX i3 ON \"x y\"(\"q r\" || s) WHERE \"q r\" > 0;
         ALTER TABLE \"x y\" RENAME COLUMN \"q r\" TO [new col];
         ALTER TABLE \"x y\" RENAME TO zz;
         SELECT sql FROM sqlite_schema ORDER BY name;
         INSERT INTO zz VALUES (1, 2);
         SELECT * FROM zz WHERE \"new col\" || s = '12'",
    );
    agree("alter", &stmts, &ordered);
}

/// Files the pin made: hipp reads them and writes them, and the pin
/// finds them intact and equal to its own result of the same writes.
#[test]
fn pin_files_round_trip() {
    let o = fresh("pinmade-o");
    let h = fresh("pinmade-h");
    common::cli(
        &o,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d AS (b || '!') VIRTUAL);
         CREATE INDEX e1 ON t(lower(b) COLLATE NOCASE DESC, c);
         CREATE UNIQUE INDEX p1 ON t(c) WHERE c > 0;
         CREATE INDEX e2 ON t(d, a + 1) WHERE b IS NOT NULL;
         CREATE INDEX e3 ON t(length(b) * 2) WHERE length(b) > 1;
         WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 300)
         INSERT INTO t(a, b, c) SELECT x, char(65 + x % 26) || x, (x % 37) - 10 + x * 0.001 FROM n;",
    );
    std::fs::copy(&o, &h).expect("copy");
    let writes = lines(
        "INSERT INTO t(b, c) VALUES ('Zed', 500), ('q', -9);
         UPDATE t SET c = c + 1000 WHERE a % 7 = 0;
         UPDATE t SET b = upper(b) WHERE a BETWEEN 10 AND 60;
         DELETE FROM t WHERE a % 11 = 0;
         INSERT OR REPLACE INTO t(a, b, c) VALUES (9, 'r', 2), (12, NULL, 7);
         REPLACE INTO t(b, c) VALUES ('s', 1015);
         UPDATE OR IGNORE t SET c = 5 WHERE a < 40;
         INSERT INTO t(b, c) VALUES ('up', 1015) ON CONFLICT (c) WHERE c > 0 DO UPDATE SET b = 'hit'",
    );
    let got = hipp_file(&h, &writes);
    let want = oracle_file(&o, &writes);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.is_ok(), w.is_some(), "{}: {g:?}", writes[i]);
    }
    assert_eq!(common::cli(&h, "PRAGMA integrity_check;").trim(), "ok");
    for q in [
        "SELECT a, b, c, d FROM t ORDER BY a",
        "SELECT a FROM t WHERE c > 0 ORDER BY c, a",
        "SELECT a FROM t INDEXED BY e1 WHERE lower(b) > 'm' ORDER BY a",
        "SELECT a FROM t INDEXED BY e2 WHERE d IS NOT NULL AND b IS NOT NULL ORDER BY a",
        "SELECT count(*) FROM t INDEXED BY e3 WHERE length(b) > 1",
    ] {
        assert_eq!(common::cli(&h, q), common::cli(&o, q), "{q}");
    }
    // hipp reads the pin's indexes the way the pin does.
    let reads: Vec<String> = [
        "SELECT a FROM t WHERE lower(b) = 'b1'",
        "SELECT a, c FROM t WHERE c > 25",
        "SELECT a FROM t WHERE length(b) * 2 = 6 AND length(b) > 1",
        "SELECT b FROM t WHERE c BETWEEN 3 AND 4",
        "SELECT a FROM t ORDER BY lower(b) COLLATE NOCASE DESC, c LIMIT 7",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let got = hipp_file(&h, &reads);
    let want = oracle_file(&h, &reads);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.as_ref().ok(), w.as_ref(), "{}", reads[i]);
    }
}

// ----------------------------------------------------------- planner

#[test]
fn planner_order() {
    let stmts = lines(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);
         INSERT INTO t VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c'), (4, 3, 'd'), (5, NULL, 'e'), (6, 'x', 'f'), (7, 1.5, 'g');
         CREATE INDEX ib ON t(b);
         SELECT a FROM t WHERE b > 2;
         SELECT a FROM t WHERE b > 2 LIMIT 2;
         SELECT DISTINCT b FROM t;
         SELECT b FROM t;
         SELECT b, a FROM t;
         SELECT a FROM t WHERE b < 'z';
         SELECT a FROM t WHERE b IN (9, 3, 'x');
         SELECT a FROM t WHERE b = 3 OR b = 9;
         SELECT (SELECT a FROM t WHERE b > 2);
         SELECT a FROM t WHERE b IS NULL OR b > 0;
         SELECT a FROM t WHERE a > 2;
         SELECT a FROM t WHERE a IN (6, 1, 2);
         SELECT a FROM t ORDER BY b DESC;
         SELECT a FROM t WHERE b BETWEEN 1 AND 6;
         SELECT a FROM t WHERE b >= 3 ORDER BY b DESC;
         SELECT a FROM t WHERE b = 3 AND a > 2;
         SELECT a FROM t WHERE rowid < 5 ORDER BY rowid DESC;
         SELECT * FROM t WHERE b = 3;
         SELECT b, count(*) FROM t GROUP BY b;
         SELECT b, max(a) FROM t WHERE b > 0 GROUP BY b;
         CREATE TABLE d(k, v);
         CREATE INDEX dk ON d(k DESC);
         INSERT INTO d VALUES (1, 'a'), (2, 'b'), (3, 'c'), (2, 'bb'), (1, 'aa');
         SELECT v FROM d WHERE k IN (1, 3, 2);
         SELECT k, count(*) FROM d GROUP BY k;
         SELECT k, group_concat(v) FROM d GROUP BY k;
         SELECT v FROM d WHERE k > 1;
         SELECT v FROM d ORDER BY k;
         SELECT v FROM d ORDER BY k DESC LIMIT 3;
         SELECT DISTINCT k FROM d;
         CREATE TABLE m(a, b, c, d);
         CREATE INDEX mb ON m(b);
         CREATE INDEX mc ON m(c);
         CREATE INDEX mbc ON m(b, c DESC);
         CREATE UNIQUE INDEX md ON m(d);
         INSERT INTO m VALUES (1, 1, 2, 10), (2, 1, 1, 11), (3, 2, 2, 12), (4, 1, 2, 13), (5, 2, 1, 14), (6, NULL, 3, NULL);
         SELECT a FROM m WHERE b = 1 AND c = 2;
         SELECT a FROM m WHERE b > 0 AND c > 1;
         SELECT a FROM m WHERE b = 1 AND c > 0;
         SELECT a FROM m WHERE b BETWEEN 1 AND 2 AND c > 0;
         SELECT a FROM m WHERE b IN (1, 2) AND c = 2;
         SELECT a FROM m WHERE b IN (1, 2) AND c IN (1, 2, 3);
         SELECT a FROM m WHERE b = 1 ORDER BY c;
         SELECT a FROM m WHERE b > 0 ORDER BY c;
         SELECT a FROM m WHERE d > 11;
         SELECT a FROM m WHERE d = 12 AND b = 2;
         SELECT b, c FROM m;
         SELECT c FROM m WHERE b IS NULL;
         SELECT a FROM m WHERE rowid > 1 AND b = 1;
         CREATE TABLE x(a, b, c);
         CREATE INDEX xe ON x(a * 10 + b);
         CREATE INDEX xp ON x(c) WHERE b > 0;
         INSERT INTO x VALUES (1, 2, 'z'), (0, 12, 'y'), (3, 4, 'x'), (0, -1, 'w'), (1, 1, 'v');
         SELECT * FROM x WHERE a * 10 + b = 12;
         SELECT * FROM x WHERE a * 10 + b > 10;
         SELECT * FROM x ORDER BY a * 10 + b DESC;
         SELECT c FROM x WHERE b > 0;
         SELECT c FROM x WHERE b > 0 AND c > 'w';
         SELECT c FROM x WHERE c > 'u' AND b > 0 ORDER BY c DESC",
    );
    agree("order", &stmts, &ordered);
}

#[test]
fn write_order() {
    let stmts = lines(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);
         INSERT INTO t VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c'), (4, 3, 'd'), (5, NULL, 'e'), (6, 'x', 'f'), (7, 1.5, 'g');
         CREATE INDEX ib ON t(b);
         CREATE UNIQUE INDEX uc ON t(c);
         UPDATE OR FAIL t SET c = 'Z' WHERE b > 2;
         SELECT a, c FROM t;
         UPDATE OR IGNORE t SET c = 'Y' WHERE b > 2;
         SELECT a, c FROM t;
         UPDATE t SET c = c || '!' WHERE b > 2 RETURNING a;
         DELETE FROM t WHERE b >= 3 RETURNING a;
         SELECT * FROM t;
         CREATE TABLE u(k INTEGER PRIMARY KEY, v UNIQUE, w);
         INSERT INTO u VALUES (1, 3, 1), (2, 2, 2), (3, 1, 3);
         CREATE INDEX uw ON u(w);
         UPDATE u SET v = v + 1 WHERE w > 0;
         SELECT * FROM u;
         UPDATE u SET v = v - 1 WHERE w > 0;
         SELECT * FROM u;
         UPDATE u SET v = v + 1 WHERE v > 0;
         SELECT * FROM u;
         UPDATE OR FAIL u SET v = 9 WHERE w IN (3, 1, 2);
         SELECT * FROM u;
         UPDATE OR FAIL u SET v = 8 WHERE w BETWEEN 1 AND 3 RETURNING k;
         SELECT * FROM u;
         CREATE TABLE log(x);
         CREATE TRIGGER tr AFTER UPDATE ON u BEGIN INSERT INTO log VALUES (new.k); END;
         UPDATE u SET v = v * 10 WHERE w > 1;
         SELECT group_concat(x) FROM log",
    );
    agree("writeorder", &stmts, &ordered);
}

#[test]
fn joins_and_subqueries() {
    let stmts = lines(
        "CREATE TABLE a(id INTEGER PRIMARY KEY, k, v);
         CREATE TABLE b(k, w);
         CREATE INDEX bk ON b(k);
         CREATE INDEX ak ON a(k);
         INSERT INTO a VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 2, 'z'), (4, NULL, 'n');
         INSERT INTO b VALUES (2, 'p'), (1, 'q'), (2, 'r'), (3, 's'), (NULL, 't');
         SELECT a.v, b.w FROM a JOIN b ON b.k = a.k;
         SELECT a.v, b.w FROM a LEFT JOIN b ON b.k = a.k;
         SELECT a.v, b.w FROM a, b WHERE b.k = a.k AND a.k > 1;
         SELECT a.v, b.w FROM a JOIN b ON b.k > a.k;
         SELECT a.v, (SELECT group_concat(w) FROM b WHERE b.k = a.k) FROM a;
         SELECT a.v, (SELECT w FROM b WHERE b.k > a.k) FROM a;
         SELECT v FROM a WHERE k IN (SELECT k FROM b WHERE w > 'q');
         SELECT v FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND w = 'r');
         SELECT a.v, b.w FROM a RIGHT JOIN b ON b.k = a.k;
         SELECT a.v, b.w FROM a FULL JOIN b ON b.k = a.k;
         UPDATE a SET v = v || '+' WHERE k = (SELECT max(k) FROM b WHERE w < 's');
         SELECT * FROM a;
         WITH c(n) AS (SELECT k FROM b WHERE k > 1) SELECT v FROM a WHERE k IN c;
         CREATE VIEW vw AS SELECT v FROM a WHERE k = 2;
         SELECT * FROM vw",
    );
    let join = |s: &str| s.contains("JOIN") || s.contains(" a, b ");
    agree("joins", &stmts, &join);
}

/// Bugs found while building the unit, each pinned.
#[test]
fn regressions() {
    let stmts = lines(
        "-- An IN list walks its DESC key column largest first.
         CREATE TABLE r1(a INTEGER, b COLLATE NOCASE, c, d);
         CREATE INDEX r1b ON r1(b DESC);
         INSERT INTO r1 VALUES (1, x'06', 0, 0), (2, 9, 0, 0), (3, 1, 0, 0), (4, 9, 0, 0), (5, 'x', 0, 0);
         SELECT a FROM r1 WHERE b IN (x'06', 1, 9);
         -- `x IS NULL` uses an index of any collation.
         CREATE TABLE r2(a, b BLOB, c TEXT COLLATE NOCASE);
         CREATE UNIQUE INDEX r2c ON r2(c COLLATE NOCASE, b);
         INSERT INTO r2 VALUES (1, 2, NULL), (2, 1, 'a'), (3, 0, NULL), (4, 5, 'b');
         SELECT a, b FROM r2 WHERE c IS NULL ORDER BY a;
         -- Cost ties go to the cheaper unsorted plan (a covering
         -- scan before the sort).
         CREATE TABLE r3(a NUMERIC, b BLOB COLLATE NOCASE, c TEXT, d);
         CREATE INDEX r3i ON r3(b COLLATE NOCASE, d COLLATE NOCASE);
         INSERT INTO r3 VALUES (1, 'b', 1, 2), (2, 'a', 1, 2), (3, 'c', 1, 1);
         SELECT b FROM r3 WHERE 1 ORDER BY d;
         SELECT b, d FROM r3;
         -- A comparison implies its operand IS NOT NULL through
         -- arithmetic (the partial index is used).
         CREATE TABLE r4(a BLOB, b TEXT, c REAL, d COLLATE NOCASE);
         CREATE UNIQUE INDEX r4i ON r4(c COLLATE NOCASE, d + 1) WHERE a IS NOT NULL;
         INSERT INTO r4 VALUES (x'03', 1, 6, 3), (x'05', 1, -1.5, '10'), (x'09', 9, -3, '2'), (4, 2, 'a', x'01');
         SELECT a FROM r4 WHERE a + 1 = 1;
         -- An IN on a key column delivers the ORDER BY, backwards.
         CREATE TABLE r5(a, d INTEGER);
         CREATE INDEX r5d ON r5(d);
         INSERT INTO r5 VALUES (-3.5, 2), (3, 2), (NULL, '2'), (7, -1.5);
         SELECT a FROM r5 WHERE d IN (2, -1.5, 'b') ORDER BY d DESC;
         -- GROUP BY groups come in a DESC index's order.
         CREATE TABLE r6(a, b);
         CREATE INDEX r6b ON r6(b DESC);
         INSERT INTO r6 VALUES (1, 1), (2, 2), (3, 3), (4, 2);
         SELECT b, count(*) FROM r6 WHERE b > 0 GROUP BY b;
         -- A nested scalar subquery as a probe value.
         CREATE TABLE r7(id INTEGER PRIMARY KEY, name);
         CREATE TABLE r8(id INTEGER PRIMARY KEY, r7id);
         INSERT INTO r7 VALUES (1, 'one'), (2, 'two');
         INSERT INTO r8 VALUES (1, 2), (2, 1);
         SELECT name FROM r7 WHERE id = (SELECT r7id FROM r8 WHERE id = (SELECT 1));
         -- A covering index on `x IS NOT NULL` (the pin's virtual
         -- `x > NULL` range).
         CREATE TABLE r9(id INTEGER PRIMARY KEY, a, b TEXT, c NUMERIC, d INTEGER);
         CREATE INDEX r9i ON r9(a, b, c, d);
         INSERT INTO r9 VALUES (1, 3, 'x', 1, 1), (2, NULL, 'y', 2, 2), (3, 1, 'z', 3, 3), (4, 2, 'w', 0, 0);
         SELECT * FROM r9 WHERE a IS NOT NULL",
    );
    agree("regress", &stmts, &ordered);
}

/// Review fixes (v0.16), each pinned: `x IN (SELECT ...)` probes an
/// index in the subquery's sorted key order (the pin's ephemeral IN
/// table), NOT BETWEEN / NOT IN (SELECT) do not imply a partial
/// index's `IS NOT NULL`, a commuted comparison (`0 < b`) implies the
/// WHERE `b > 0`, and an ORDER BY whose NULLs the index puts at the
/// other end is the pin's two-pass WHERE_BIGNULL_SORT scan.
#[test]
fn review_regressions() {
    let stmts = lines(
        "CREATE TABLE t(a);
         CREATE INDEX i ON t(a);
         INSERT INTO t VALUES (1), (2), ('1'), ('a'), (3), (1.5);
         CREATE TABLE s(x);
         INSERT INTO s VALUES (3), ('a'), (1), (2), ('1'), (NULL);
         SELECT rowid, a FROM t WHERE a IN (SELECT x FROM s);
         SELECT rowid, a FROM t WHERE a IN (SELECT x FROM s ORDER BY x DESC);
         SELECT rowid, a FROM t WHERE a IN (SELECT x FROM s) ORDER BY a DESC;
         SELECT rowid, a FROM t WHERE rowid IN (SELECT x FROM s);
         SELECT rowid, a FROM t WHERE a NOT IN (SELECT x FROM s WHERE x IS NOT NULL);
         SELECT count(*) FROM t WHERE a IN (SELECT x FROM s WHERE x > 1);
         SELECT rowid FROM t WHERE a IN (SELECT x FROM s WHERE x = t.a);
         CREATE TABLE u(b TEXT);
         CREATE INDEX iu ON u(lower(b) DESC);
         INSERT INTO u VALUES ('A'), ('b'), ('C'), ('b');
         CREATE TABLE s2(x);
         INSERT INTO s2 VALUES ('c'), ('b'), ('a');
         SELECT rowid, b FROM u WHERE lower(b) IN (SELECT x FROM s2);
         CREATE TABLE p(a, b, c);
         CREATE INDEX p2 ON p(b DESC) WHERE a IS NOT NULL;
         INSERT INTO p VALUES (1,1,1), (1,2,2), (2,NULL,3), (NULL,1,4), (3,3,5), (2,2,6), (1.5,'x',7), ('a',x'01',8), (NULL,NULL,9);
         SELECT c FROM p WHERE a NOT BETWEEN 1 AND 2;
         SELECT c FROM p WHERE a NOT IN (SELECT 1);
         SELECT c FROM p WHERE a BETWEEN 1 AND 2;
         SELECT c FROM p WHERE a IN (SELECT 1);
         CREATE TABLE q(a, b, c);
         CREATE INDEX q1 ON q(a) WHERE b > 0;
         CREATE INDEX q2 ON q(c) WHERE b IS NOT NULL;
         INSERT INTO q VALUES (1,1,1), (1,0,2), (1,NULL,3), (2,5,6), (2,-1,7), (1,1,NULL), (3,'x',1.5), (1,x'00',9);
         SELECT rowid FROM q WHERE a = 1 AND 0 < b;
         SELECT rowid FROM q WHERE a = 1 AND b > 0;
         CREATE TABLE v(a, b, c);
         CREATE INDEX v1 ON v(a DESC, b);
         INSERT INTO v VALUES (1,1,1), (1,2,2), (2,NULL,3), (NULL,1,4), (3,3,5), (2,2,6), (1.5,'x',7), ('a',x'01',8), (NULL,NULL,9);
         SELECT c FROM v ORDER BY a NULLS LAST;
         SELECT c FROM v ORDER BY a DESC NULLS FIRST;
         SELECT c FROM v ORDER BY a NULLS LAST, b;
         SELECT c FROM v ORDER BY a DESC NULLS FIRST LIMIT 3;
         CREATE TABLE w(x, a, c);
         CREATE INDEX w1 ON w(x, a DESC);
         INSERT INTO w VALUES (1,1,1), (1,2,2), (1,NULL,3), (2,NULL,4), (1,3,5), (2,2,6), (1,1.5,7), (2,'a',8), (1,NULL,9), (2,1,10);
         SELECT c FROM w WHERE x = 1 ORDER BY a NULLS LAST;
         SELECT c FROM w WHERE x IN (2, 1) ORDER BY x, a NULLS LAST;
         SELECT c FROM w WHERE x IN (2, 1) ORDER BY x DESC, a DESC NULLS FIRST;
         SELECT c FROM w ORDER BY x, a NULLS LAST
         -- UPDATE / DELETE probe an index with an uncorrelated IN
         -- (SELECT ...), evaluated on a row of the table's width (a
         -- probe on the bare empty row read the subquery's columns
         -- from the wrong slots and found nothing).
         CREATE TABLE wt(a, b REAL);
         CREATE INDEX wti ON wt(a);
         CREATE TABLE ws(x);
         INSERT INTO ws VALUES (3), (4);
         INSERT INTO wt VALUES (3, 5), (4, 6), (5, 7), (4, 8);
         DELETE FROM wt WHERE a IN (SELECT x FROM ws WHERE x = 3) RETURNING b;
         UPDATE wt SET b = b + 1 WHERE a IN (SELECT x FROM ws) RETURNING b;
         UPDATE wt SET b = b + 1 WHERE a IN (SELECT a FROM wt WHERE a > 4) RETURNING b;
         DELETE FROM wt WHERE a IN (SELECT a + 1 FROM wt) RETURNING b;
         SELECT * FROM wt",
    );
    agree("review", &stmts, &ordered);
}

/// Review fixes, generated (xorshift PRNG): IN (SELECT ...) on plain,
/// expression and rowid keys, NOT BETWEEN / NOT IN and commuted
/// comparisons against partial indexes, NULLS FIRST / LAST over ASC
/// and DESC indexes — rows and row order agree with the pin.
#[test]
fn generated_review() {
    let n = corpus_size(150);
    let mut r = Rng(0x7e71_e3ed_2026_1104);
    let cols = ["a", "b", "c"];
    for case in 0..n {
        let mut s: Vec<String> = Vec::new();
        let mut defs: Vec<String> = Vec::new();
        for c in cols {
            defs.push(format!(
                "{c} {}",
                r.pick(&["", "INTEGER", "TEXT", "REAL", "NUMERIC"])
            ));
        }
        s.push(format!("CREATE TABLE t({})", defs.join(", ")));
        s.push("CREATE TABLE s(x)".into());
        for i in 0..1 + r.below(3) {
            let k = 1 + r.below(2) as usize;
            let mut items = Vec::new();
            for _ in 0..k {
                let c = r.pick(&cols);
                items.push(match r.below(8) {
                    0 => format!("{c} + 1"),
                    1 => format!("lower({c})"),
                    _ => format!("{c}{}", r.pick(&["", " DESC", " DESC"])),
                });
            }
            let w = match r.below(5) {
                0 => format!(" WHERE {} IS NOT NULL", r.pick(&cols)),
                1 => format!(" WHERE {} > 0", r.pick(&cols)),
                _ => String::new(),
            };
            s.push(format!("CREATE INDEX i{i} ON t({}){w}", items.join(", ")));
        }
        for _ in 0..r.below(20) {
            let v: Vec<String> = (0..3).map(|_| gen_val(&mut r, true)).collect();
            s.push(format!("INSERT INTO t VALUES ({})", v.join(", ")));
        }
        for _ in 0..r.below(6) {
            s.push(format!("INSERT INTO s VALUES ({})", gen_val(&mut r, true)));
        }
        for _ in 0..6 {
            let c = r.pick(&cols);
            let c2 = r.pick(&cols);
            let sel = r.pick(&["*", "rowid", "a", "rowid, b", c]);
            let v1 = gen_val(&mut r, false);
            let v2 = gen_val(&mut r, false);
            let w = match r.below(12) {
                0 | 1 => format!("{c} IN (SELECT x FROM s)"),
                2 => format!("lower({c}) IN (SELECT x FROM s)"),
                3 => format!("{c} + 1 IN (SELECT x FROM s WHERE x IS NOT NULL)"),
                4 => "rowid IN (SELECT x FROM s)".into(),
                5 => format!("{c} NOT BETWEEN {v1} AND {v2}"),
                6 => format!("{c} NOT IN (SELECT x FROM s)"),
                7 => format!("{v1} < {c}"),
                8 => format!("{c} = {v1} AND 0 < {c2}"),
                9 => format!("{c2} IN (SELECT x FROM s) AND {c} > {v1}"),
                _ => "1".into(),
            };
            let ob = match r.below(6) {
                0 | 1 => format!(
                    " ORDER BY {}{}",
                    r.pick(&cols),
                    r.pick(&[
                        " NULLS LAST",
                        " DESC NULLS FIRST",
                        " NULLS FIRST",
                        " DESC NULLS LAST"
                    ])
                ),
                2 => format!(
                    " ORDER BY {}{}, {}",
                    r.pick(&cols),
                    r.pick(&[" NULLS LAST", " DESC NULLS FIRST"]),
                    r.pick(&cols)
                ),
                _ => String::new(),
            };
            s.push(format!("SELECT {sel} FROM t WHERE {w}{ob}"));
        }
        // Writes probing with an IN subquery (rowid order, RETURNING).
        let c = r.pick(&cols);
        match r.below(3) {
            0 => s.push(format!(
                "UPDATE t SET {} = {} WHERE {c} IN (SELECT x FROM s) RETURNING rowid, *",
                r.pick(&cols),
                gen_val(&mut r, false)
            )),
            1 => s.push(format!(
                "DELETE FROM t WHERE lower({c}) IN (SELECT x FROM s) RETURNING *"
            )),
            _ => s.push(format!(
                "DELETE FROM t WHERE {c} IN (SELECT {c} FROM t WHERE {c} > 1) RETURNING rowid"
            )),
        }
        s.push("SELECT rowid, * FROM t".into());
        agree(&format!("genr{case}"), &s, &ordered);
    }
}

// --------------------------------------------------------- generated

fn gen_val(r: &mut Rng, blobs: bool) -> String {
    let x = r.below(100);
    if x < 10 {
        "NULL".into()
    } else if x < 50 {
        r.int(-3, 9).to_string()
    } else if x < 65 {
        format!("{}.5", r.int(-3, 9))
    } else if x < 85 || !blobs {
        format!("'{}'", r.pick(&["a", "B", "b", "c", "A", "10", "2"]))
    } else {
        format!("x'0{}'", r.below(10))
    }
}

/// Random tables (types, collations, rowid aliases), random column /
/// expression / partial / UNIQUE indexes, random data and random
/// single-table queries: rows and row order agree with the pin.
#[test]
fn generated_queries() {
    let n = corpus_size(150);
    let mut r = Rng(0x5eed_1234_9876_0001);
    let cols = ["a", "b", "c", "d"];
    for case in 0..n {
        let mut s: Vec<String> = Vec::new();
        let mut defs: Vec<String> = Vec::new();
        if r.chance(0.3) {
            defs.push("id INTEGER PRIMARY KEY".into());
        }
        for c in cols {
            let ty = r.pick(&["", "INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB"]);
            let co = r.pick(&["", "", " COLLATE NOCASE"]);
            defs.push(format!("{c} {ty}{co}"));
        }
        s.push(format!("CREATE TABLE t({})", defs.join(", ")));
        for i in 0..r.below(4) {
            let k = 1 + r.below(2) as usize;
            let mut items = Vec::new();
            for _ in 0..k {
                let c = r.pick(&cols);
                items.push(match r.below(10) {
                    0 | 1 => format!("{c} + 1"),
                    2 => format!("lower({c})"),
                    _ => format!("{c}{}", r.pick(&["", "", " DESC", " COLLATE NOCASE"])),
                });
            }
            let u = if r.chance(0.2) { "UNIQUE " } else { "" };
            let w = if r.chance(0.25) {
                format!(" WHERE {} IS NOT NULL", r.pick(&cols))
            } else {
                String::new()
            };
            s.push(format!(
                "CREATE {u}INDEX i{i} ON t({}){w}",
                items.join(", ")
            ));
        }
        for _ in 0..r.below(26) {
            let v: Vec<String> = (0..4).map(|_| gen_val(&mut r, true)).collect();
            s.push(format!(
                "INSERT OR IGNORE INTO t(a, b, c, d) VALUES ({})",
                v.join(", ")
            ));
        }
        for _ in 0..6 {
            let c = r.pick(&cols);
            let c2 = r.pick(&cols);
            let sel = r.pick(&["*", "a", "b", "a, b", "rowid, c", c]);
            let w = match r.below(20) {
                0..=3 => format!("{c} = {}", gen_val(&mut r, true)),
                4..=6 => format!("{c} > {}", gen_val(&mut r, true)),
                7 | 8 => format!(
                    "{c} BETWEEN {} AND {}",
                    gen_val(&mut r, true),
                    gen_val(&mut r, true)
                ),
                9 | 10 => format!(
                    "{c} IN ({}, {}, {})",
                    gen_val(&mut r, true),
                    gen_val(&mut r, true),
                    gen_val(&mut r, true)
                ),
                11 | 12 => format!(
                    "{c} = {} AND {c2} > {}",
                    gen_val(&mut r, true),
                    gen_val(&mut r, true)
                ),
                13 => format!("{c} + 1 = {}", gen_val(&mut r, true)),
                14 => format!("lower({c}) = {}", gen_val(&mut r, true)),
                15 => format!("{c} IS NULL"),
                16 => format!("{c} IS NOT NULL"),
                17 => format!("rowid > {}", r.below(10)),
                _ => "1".into(),
            };
            let ob = match r.below(10) {
                0..=2 => format!(" ORDER BY {}{}", r.pick(&cols), r.pick(&["", " DESC"])),
                3 => format!(" ORDER BY {}, {} DESC", r.pick(&cols), r.pick(&cols)),
                _ => String::new(),
            };
            let lim = if r.chance(0.2) {
                format!(" LIMIT {}", 1 + r.below(5))
            } else {
                String::new()
            };
            s.push(format!("SELECT {sel} FROM t WHERE {w}{ob}{lim}"));
        }
        agree(&format!("genq{case}"), &s, &ordered);
    }
}

/// Random writes through expression / partial / UNIQUE indexes —
/// UPDATE OR FAIL / IGNORE / ABORT (whose partial work follows the
/// processing order), RETURNING, DELETE, REPLACE — with GROUP BY,
/// DISTINCT, correlated subqueries and joins in between: results
/// agree with the pin (joins without their row order) and the pin's
/// integrity_check passes on hipp's file.
#[test]
fn generated_writes() {
    let n = corpus_size(150) / 2;
    let mut r = Rng(0x0dd_ba11_c0ff_ee01);
    let cols = ["a", "b", "c", "d"];
    for case in 0..n {
        let mut s: Vec<String> = Vec::new();
        let mut defs: Vec<String> = Vec::new();
        if r.chance(0.3) {
            defs.push("id INTEGER PRIMARY KEY".into());
        }
        for c in cols {
            defs.push(format!(
                "{c} {}",
                r.pick(&["", "INTEGER", "TEXT", "REAL", "NUMERIC"])
            ));
        }
        s.push(format!("CREATE TABLE t({})", defs.join(", ")));
        s.push("CREATE TABLE u(k INTEGER, x, y)".into());
        s.push("CREATE INDEX uk ON u(k)".into());
        for i in 0..1 + r.below(3) {
            let k = 1 + r.below(2) as usize;
            let mut items = Vec::new();
            for _ in 0..k {
                let c = r.pick(&cols);
                items.push(match r.below(10) {
                    0 | 1 => format!("{c} * 2"),
                    2 => format!("abs({c})"),
                    _ => format!("{c}{}", r.pick(&["", "", " DESC"])),
                });
            }
            let u = if r.chance(0.3) { "UNIQUE " } else { "" };
            let w = match r.below(4) {
                0 => format!(" WHERE {} > 0", r.pick(&cols)),
                1 => format!(" WHERE {} IS NOT NULL", r.pick(&cols)),
                _ => String::new(),
            };
            s.push(format!(
                "CREATE {u}INDEX i{i} ON t({}){w}",
                items.join(", ")
            ));
        }
        for _ in 0..5 + r.below(26) {
            let v: Vec<String> = (0..4).map(|_| gen_val(&mut r, false)).collect();
            s.push(format!(
                "INSERT OR IGNORE INTO t(a, b, c, d) VALUES ({})",
                v.join(", ")
            ));
        }
        for _ in 0..3 + r.below(13) {
            s.push(format!(
                "INSERT INTO u VALUES ({}, {}, {})",
                r.int(-2, 9),
                gen_val(&mut r, false),
                gen_val(&mut r, false)
            ));
        }
        for _ in 0..8 {
            let c = r.pick(&cols);
            let c2 = r.pick(&cols);
            let v1 = gen_val(&mut r, false);
            let v2 = gen_val(&mut r, false);
            let w = match r.below(8) {
                0 => format!("{c} > {v1}"),
                1 => format!("{c} = {v1}"),
                2 => format!("{c} IN ({v1}, {v2})"),
                3 => format!("{c} BETWEEN {v1} AND {v2}"),
                4 => format!("{c} * 2 > {v1}"),
                5 => format!("abs({c}) = {v1}"),
                6 => format!("{c} > 0"),
                _ => "1".into(),
            };
            match r.below(20) {
                0..=2 => {
                    let act = r.pick(&["FAIL", "IGNORE", "ABORT"]);
                    let set = match r.below(3) {
                        0 => gen_val(&mut r, false),
                        1 => format!("{c2} + 1"),
                        _ => format!("{c2} - 1"),
                    };
                    s.push(format!("UPDATE OR {act} t SET {c2} = {set} WHERE {w}"));
                    s.push("SELECT * FROM t".into());
                }
                3 => s.push(format!(
                    "UPDATE t SET {c2} = {} WHERE {w} RETURNING *",
                    gen_val(&mut r, false)
                )),
                4 => s.push(format!("DELETE FROM t WHERE {w} RETURNING a, b")),
                5 | 6 => s.push(format!("SELECT {c}, count(*), b FROM t WHERE {w} GROUP BY {c}")),
                7 => s.push(format!("SELECT DISTINCT {c} FROM t WHERE {w}")),
                8 | 9 => s.push(format!(
                    "SELECT a, (SELECT x FROM u WHERE u.k > t.{c} LIMIT 1), (SELECT group_concat(y) FROM u WHERE u.k = t.{c2}) FROM t WHERE {w}"
                )),
                10 => s.push(format!("SELECT max({c}), min({c2}), count(*) FROM t WHERE {w}")),
                11 | 12 => s.push(format!(
                    "SELECT * FROM t WHERE {w} ORDER BY {c2}{} LIMIT {}",
                    r.pick(&["", " DESC"]),
                    1 + r.below(4)
                )),
                13 => {
                    let v: Vec<String> = (0..4).map(|_| gen_val(&mut r, false)).collect();
                    s.push(format!(
                        "INSERT OR REPLACE INTO t(a, b, c, d) VALUES ({})",
                        v.join(", ")
                    ));
                }
                14 => s.push(format!("SELECT * FROM t WHERE {w} AND {c2} IS NOT NULL")),
                _ => s.push(format!("SELECT t.a, u.x FROM t JOIN u ON u.k = t.{c} WHERE {w}")),
            }
        }
        s.push("PRAGMA integrity_check".into());
        let join = |q: &str| q.contains("JOIN");
        agree(&format!("genw{case}"), &s, &join);
    }
}

// ------------------------------------------------------------- scale

/// Point lookups through an index into a 100,000-row table: the
/// answers are the pin's, and 300 of them take less time than three
/// full scans of the same table (each lookup reads a handful of pages,
/// a scan every row) — far inside any machine's noise.
#[test]
fn point_lookups_scale() {
    use std::time::Instant;
    let p = fresh("scale");
    common::cli(
        &p,
        "CREATE TABLE t(id INTEGER PRIMARY KEY, k INTEGER, v TEXT);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000)
         INSERT INTO t SELECT x, (x * 7919) % 100003, 'v' || x FROM c;
         CREATE INDEX tk ON t(k);",
    );
    let mut r = Rng(0x00c0_ffee_1234_5678);
    let keys: Vec<i64> = (0..300).map(|_| r.int(0, 100_003)).collect();
    let (lookups, scans, sample) = block_on(async {
        let fs = FileStorage::open(&p).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let t0 = Instant::now();
        let mut sample = Vec::new();
        for k in keys.iter() {
            let rows = collect(&mut db, &format!("SELECT v FROM t WHERE k = {k}"))
                .await
                .expect("lookup");
            sample.push((*k, rows));
        }
        let lookups = t0.elapsed();
        let t1 = Instant::now();
        for i in 0..3 {
            let rows = collect(&mut db, &format!("SELECT k FROM t WHERE v = 'v{}'", 7 + i))
                .await
                .expect("scan");
            assert!(!rows.is_empty());
        }
        (lookups, t1.elapsed(), sample)
    });
    assert!(
        lookups < scans,
        "300 index lookups took {lookups:?}, 3 full scans {scans:?}"
    );
    assert!(lookups.as_secs() < 60, "{lookups:?}");
    for (k, rows) in sample.iter().take(25) {
        let want = common::cli(&p, &format!("SELECT v FROM t WHERE k = {k};"));
        assert_eq!(
            String::from_utf8_lossy(rows),
            want.trim_end_matches('\n'),
            "k = {k}"
        );
    }
    // Range, IN and ORDER BY over the big table: the pin's rows in
    // the pin's order.
    let qs: Vec<String> = [
        "SELECT id FROM t WHERE k BETWEEN 5000 AND 5040",
        "SELECT id FROM t WHERE k IN (99999, 7, 12345, 7)",
        "SELECT k FROM t WHERE k > 99990",
        "SELECT v FROM t ORDER BY k DESC LIMIT 5",
        "SELECT count(*) FROM t WHERE k < 1000",
        "SELECT v FROM t WHERE id > 99995",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let got = hipp_file(&p, &qs);
    let want = oracle_file(&p, &qs);
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(g.as_ref().ok(), w.as_ref(), "{}", qs[i]);
    }
}
