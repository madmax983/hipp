//! v0.16 ALTER TABLE ... RENAME TO / RENAME [COLUMN] / DROP
//! [COLUMN], differentially vs the pinned oracle (SQLite 3.54.1,
//! live). Probed on the pin before implementation; the matrix the
//! tests encode:
//!
//! - The pin edits stored texts token by token and keeps every
//!   other byte: RENAME TO always writes `"new"`; RENAME COLUMN
//!   writes the new name bare where the old token was bare and the
//!   new name was written bare, else `"new"` (a space follows when
//!   the next byte is `"`).
//! - RENAME TO edits the table's own text (its name, `t.` in CHECK
//!   / generated expressions, self-references), every `REFERENCES
//!   t`, its indexes (`ON t`, `sqlite_autoindex_t_N` names,
//!   `tbl_name`), views' FROM names and `t.` qualifiers that read
//!   it (not an alias, not a CTE, not `x IN t`), and the
//!   `sqlite_sequence` row.
//! - RENAME COLUMN edits the column's definition, PRIMARY KEY /
//!   UNIQUE / FOREIGN KEY lists, CHECK and generated expressions,
//!   index keys and WHERE clauses, `REFERENCES t(col)` anywhere, and
//!   view references that resolve to it (with an alias's own name,
//!   USING lists and other tables' columns left alone).
//! - DROP COLUMN removes the column's definition text (up to the
//!   next column; the last one from the comma before it up to the
//!   table constraints or `)`), and rewrites every row without it,
//!   defaults of short rows materialised. It refuses PRIMARY KEY /
//!   UNIQUE columns, the only column, and a column an index, CHECK,
//!   generated column, foreign key or view uses.
//! - Every view must plan before and after; names are checked
//!   (`sqlite_*`, taken, rowid aliases, unknown columns); views and
//!   system tables may not be altered.
//!
//! Every script runs on both engines; rows (the stored schema text
//! included), failing statements, the oracle's integrity check and
//! `.dump` of both files must agree.

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// One case line: statements separated by `; `, then the schema.
fn split(c: &str) -> Vec<String> {
    let mut v: Vec<String> = c
        .split("; ")
        .map(|s| s.trim_end_matches(';').to_string())
        .collect();
    v.push("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema".to_string());
    v
}

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
    same_on(tag, "", stmts);
}

/// `same`, on twin files the pin first prepared with `prep` (a
/// schema hipp cannot write itself: expression and partial
/// indexes, triggers).
fn same_on(tag: &str, prep: &str, stmts: &[String]) {
    let h = common::temp_db(&format!("v16al-{tag}-h"));
    let o = common::temp_db(&format!("v16al-{tag}-o"));
    if !prep.is_empty() {
        common::cli(&h, prep);
        common::cli(&o, prep);
    }
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

#[test]
fn rename_table() {
    // RENAME TO: the table, its indexes and autoindex names, views, foreign keys and the AUTOINCREMENT counter follow; the pin's errors.
    for (i, c) in [
        "CREATE TABLE t(a, b UNIQUE); CREATE INDEX i ON t(a); ALTER TABLE t RENAME TO u",
        "CREATE TABLE \"t\"(a); ALTER TABLE t RENAME TO \"new name\"",
        "CREATE TABLE [t] (a); ALTER TABLE t RENAME TO u",
        "CREATE TABLE   t   (a); ALTER TABLE t RENAME TO select",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO \"select\"",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO 'u'",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO [u]",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO U",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO T",
        "CREATE TABLE t(a); CREATE TABLE u(b); ALTER TABLE t RENAME TO U",
        "CREATE TABLE t(a); CREATE INDEX u ON t(a); ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a); CREATE VIEW u AS SELECT 1; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a); ALTER TABLE t RENAME TO sqlite_x",
        "CREATE TABLE t(a); ALTER TABLE nosuch RENAME TO x",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE v RENAME TO w",
        "CREATE TABLE t(a PRIMARY KEY, b UNIQUE); ALTER TABLE t RENAME TO u; INSERT INTO u VALUES(1,2); SELECT * FROM u",
        "CREATE TABLE t(a); ALTER TABLE main.t RENAME TO u",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.a, b FROM t WHERE t.a > 0; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM main.t AS x JOIN t ON x.a=t.a; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT (SELECT max(a) FROM t) FROM \"T\" WHERE a IN (SELECT a FROM [t]); ALTER TABLE t RENAME TO u",
        "CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(x REFERENCES p(id), y, FOREIGN KEY(y) REFERENCES \"p\"); ALTER TABLE p RENAME TO q",
        "CREATE TABLE t(a, b CHECK (b > 0)); CREATE VIEW v AS WITH t AS (SELECT 1 a) SELECT a FROM t; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM nosuch; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT t.a FROM t; ALTER TABLE t RENAME TO \"x y\"",
        "CREATE TABLE t(a); CREATE VIEW v(c) AS SELECT a FROM t UNION SELECT a FROM t ORDER BY 1; ALTER TABLE t RENAME TO u",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT); INSERT INTO t DEFAULT VALUES; ALTER TABLE t RENAME TO u; SELECT * FROM sqlite_sequence; INSERT INTO u DEFAULT VALUES; SELECT * FROM sqlite_sequence",
        "CREATE TABLE t(a); BEGIN; ALTER TABLE t RENAME TO u; ROLLBACK",
        "CREATE TABLE t(a, b AS (a+1), CHECK(t.a>0)); ALTER TABLE t RENAME TO u",
        "CREATE TABLE \"t\"\"x\"(a); ALTER TABLE \"t\"\"x\" RENAME TO \"u\"\"y\"",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH c AS (SELECT a FROM t) SELECT 1; ALTER TABLE t RENAME TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.* FROM t; ALTER TABLE t RENAME TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.rowid FROM t; ALTER TABLE t RENAME TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t WHERE a IN t; ALTER TABLE t RENAME TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.a FROM t AS t; ALTER TABLE t RENAME TO x",
        "CREATE TABLE sq(a INTEGER PRIMARY KEY AUTOINCREMENT); ALTER TABLE sqlite_sequence RENAME TO s",
        "CREATE TABLE sq(a INTEGER PRIMARY KEY AUTOINCREMENT); ALTER TABLE sqlite_schema RENAME TO s",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("rename_table{i}"), &split(c));
    }
}

#[test]
fn rename_column() {
    // RENAME COLUMN: definitions, constraint lists, CHECK / generated expressions, indexes, views; bare vs quoted edits.
    for (i, c) in [
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME a TO \"x y\"",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME a TO \"x\"",
        "CREATE TABLE t(\"a\" INT, b); ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t([a] INT, b); ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME a TO 'x'",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME A TO B",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME a TO A",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME a TO rowid",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME z TO y",
        "CREATE TABLE t(a INT, b); ALTER TABLE t RENAME COLUMN rowid TO y",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a, t.a, b AS a2 FROM t WHERE a > 1 ORDER BY a; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v(p,q) AS SELECT a, b FROM t; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE TABLE u(a, c); CREATE VIEW v AS SELECT u.a, t.a FROM t, u WHERE t.a=u.a; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM t; ALTER TABLE t RENAME a TO x; SELECT * FROM v",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT (SELECT a FROM t) AS z, a FROM (SELECT a FROM t) WHERE a IN (SELECT a FROM t); ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a PRIMARY KEY, b UNIQUE, UNIQUE(a,b)); ALTER TABLE t RENAME a TO x",
        "CREATE TABLE p(a INTEGER PRIMARY KEY); CREATE TABLE c(x REFERENCES p(a), FOREIGN KEY(x) REFERENCES p(a)); ALTER TABLE p RENAME a TO z",
        "CREATE TABLE c(x, y, FOREIGN KEY(x) REFERENCES p(a)); ALTER TABLE c RENAME x TO z",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH c AS (SELECT a FROM t) SELECT 1; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE TABLE u(a); CREATE VIEW v AS SELECT * FROM t JOIN u USING(a); ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT x.a FROM t AS x ORDER BY a; ALTER TABLE t RENAME a TO c",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT b AS a FROM t ORDER BY a; ALTER TABLE t RENAME a TO c",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a AS z FROM t ORDER BY z; ALTER TABLE t RENAME a TO c",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a, count(*) FROM t GROUP BY a HAVING a>1; ALTER TABLE t RENAME a TO \"c\"",
        "CREATE TABLE t(\"a b\", c); CREATE VIEW v AS SELECT \"a b\" FROM t; ALTER TABLE t RENAME \"a b\" TO d",
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a\"x\" FROM t; ALTER TABLE t RENAME a TO d",
        "CREATE TABLE t(a, b, CHECK(a>0), c AS (a*2), d DEFAULT 'a'); CREATE INDEX i ON t(a, b); ALTER TABLE t RENAME COLUMN a TO x",
        "CREATE TABLE t(a, b, c AS (a*2) STORED, CHECK(t.a > 0)); CREATE INDEX i ON t(a COLLATE nocase DESC, b); ALTER TABLE t RENAME COLUMN a TO \"x\"; INSERT INTO t(x,b) VALUES(1,2); SELECT * FROM t",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("rename_column{i}"), &split(c));
    }
}

#[test]
fn drop_column() {
    // DROP COLUMN: the text the pin removes, its refusals, rows rewritten without the column (VIRTUAL ones untouched).
    for (i, c) in [
        "CREATE TABLE t(a, b, c, d); INSERT INTO t VALUES(1,2,3,4); ALTER TABLE t DROP COLUMN b; SELECT * FROM t",
        "CREATE TABLE t(a, b, c); ALTER TABLE t DROP b",
        "CREATE TABLE t(a,b,c); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a,b,c); ALTER TABLE t DROP COLUMN c",
        "CREATE TABLE t(a INT,  b TEXT DEFAULT 'x' NOT NULL  ,  c  ); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, c, UNIQUE(a)); ALTER TABLE t DROP COLUMN c",
        "CREATE TABLE t(a /* x */, b /* y */, c); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a PRIMARY KEY, b); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a, b, PRIMARY KEY(a,b)); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a UNIQUE, b); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a, b, UNIQUE(a,b)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(b); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b CHECK(b>0)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, CHECK(b>0)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, CHECK(a>0)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, c AS (b+1)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, c AS (a+1)); INSERT INTO t(a,b) VALUES(1,2); ALTER TABLE t DROP COLUMN c; SELECT * FROM t",
        "CREATE TABLE t(a, b, c AS (a+1) STORED); INSERT INTO t(a,b) VALUES(1,2); ALTER TABLE t DROP COLUMN c; SELECT * FROM t",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT b FROM t; ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b REFERENCES p(x)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, FOREIGN KEY(b) REFERENCES p(x)); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE p(x PRIMARY KEY, y); CREATE TABLE c(z REFERENCES p(x)); ALTER TABLE p DROP COLUMN y",
        "CREATE TABLE t(a ANY,b ANY,c ANY) STRICT; ALTER TABLE t DROP c",
        "CREATE TABLE t(a, b COLLATE nocase); ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b); ALTER TABLE t DROP COLUMN rowid",
        "CREATE TABLE t(a, b); ALTER TABLE t DROP COLUMN \"B\"",
        "CREATE TABLE t(a, \"b c\", d); ALTER TABLE t DROP COLUMN \"b c\"",
        "CREATE TABLE t(a , b  ); ALTER TABLE t DROP b",
        "CREATE TABLE t(a , b  , CHECK(a>0)); ALTER TABLE t DROP b",
        "CREATE TABLE t(a , b /*c*/ , CHECK(a>0)); ALTER TABLE t DROP b",
        "CREATE TABLE t(a , b  ,c, CHECK(a>0)); ALTER TABLE t DROP c",
        "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3); BEGIN; ALTER TABLE t DROP b; SELECT * FROM t; ROLLBACK; SELECT * FROM t",
        "CREATE TABLE t(a, x); INSERT INTO t VALUES(1, 2); ALTER TABLE t ADD b DEFAULT 7; ALTER TABLE t ADD c; INSERT INTO t VALUES(5,6,8,9); ALTER TABLE t DROP COLUMN x; SELECT * FROM t",
        "CREATE TABLE t(i INTEGER PRIMARY KEY, a, g AS (a+1), x, s AS (a*2) STORED); INSERT INTO t VALUES(10,1,2); ALTER TABLE t DROP COLUMN x; ALTER TABLE t DROP COLUMN s; ALTER TABLE t DROP COLUMN g; SELECT * FROM t",
        "CREATE TABLE sq(a INTEGER PRIMARY KEY AUTOINCREMENT); ALTER TABLE sqlite_sequence DROP COLUMN seq",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("drop_column{i}"), &split(c));
    }
}

/// DROP COLUMN finds the comma before the column as a token: one
/// inside a comment or a quoted name is not it (review fix: a raw
/// byte scan cut there, the text no longer parsed, and the failed
/// ALTER left the table out of the session's catalog, so a second
/// CREATE TABLE of the name wrote a duplicate schema row).
#[test]
fn drop_column_skips_commas_in_comments() {
    for (i, c) in [
        "CREATE TABLE t(a, b, /* x,y */ c); INSERT INTO t VALUES(1,2,3); ALTER TABLE t DROP COLUMN c; SELECT * FROM t",
        "CREATE TABLE t(a, b /* q */ , c); ALTER TABLE t DROP COLUMN c",
        "CREATE TABLE t(a, b, /* x */ c /* y, */); ALTER TABLE t DROP COLUMN c; ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, c /* , */ , UNIQUE(a)); INSERT INTO t VALUES(1,2,3); ALTER TABLE t DROP COLUMN c; SELECT * FROM t",
        "CREATE TABLE t(a, b, c, /* x,y */ UNIQUE(a)); ALTER TABLE t DROP COLUMN c",
        "CREATE TABLE t(a, b, c /* , */ , /* , */ UNIQUE(a)); ALTER TABLE t DROP COLUMN c; ALTER TABLE t DROP COLUMN b",
        "CREATE TABLE t(a, b, c, \"x,\" TEXT); ALTER TABLE t DROP COLUMN \"x,\"; ALTER TABLE t DROP COLUMN c",
        "CREATE TABLE t(a, b, [x,y] c); ALTER TABLE t DROP COLUMN [x,y]",
        "CREATE TABLE t(a, b, /* x,y */ c); INSERT INTO t VALUES(1,2,3); ALTER TABLE t DROP COLUMN c; SELECT * FROM t; CREATE TABLE t(q); SELECT count(*) FROM sqlite_schema",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("drop_comma{i}"), &split(c));
    }
    // A `--` comment runs to the end of its line.
    for (i, (create, col)) in [
        ("CREATE TABLE t(a, b, c, -- ,\n d)", "d"),
        ("CREATE TABLE t(a, b, c, -- ,\n d)", "c"),
        ("CREATE TABLE t(a, b -- x,\n, c)", "c"),
        ("CREATE TABLE t(a, b, c, -- ,\n d -- ,\n)", "d"),
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16al-dash{i}-h"));
        let o = common::temp_db(&format!("v16al-dash{i}-o"));
        let stmts = [
            create.to_string(),
            "INSERT INTO t(a, b) VALUES (1, 2)".to_string(),
            format!("ALTER TABLE t DROP COLUMN {col}"),
        ];
        let (_, errs) = hipp_run(&h, &stmts);
        assert!(errs.is_empty(), "{create}: {errs:?}");
        common::cli(&o, &stmts.join(";\n"));
        let q = "SELECT sql FROM sqlite_schema; SELECT * FROM t; PRAGMA integrity_check;";
        assert_eq!(common::cli(&h, q), common::cli(&o, q), "{create}");
    }
}

/// A failed ALTER leaves the session's catalog as the file is: the
/// table is still there, and its name is still taken.
#[test]
fn failed_alter_keeps_the_catalog() {
    for (i, c) in [
        "CREATE TABLE t(a, b, c); INSERT INTO t VALUES(1,2,3); CREATE VIEW v AS SELECT * FROM nosuch; ALTER TABLE t RENAME TO u; SELECT * FROM t; ALTER TABLE t DROP COLUMN c; SELECT * FROM t; ALTER TABLE t RENAME COLUMN a TO z; CREATE TABLE t(q); INSERT INTO t VALUES(4,5,6); SELECT * FROM t",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM nosuch; BEGIN; INSERT INTO t VALUES(1,2); ALTER TABLE t RENAME TO u; SELECT * FROM t; COMMIT; SELECT * FROM t; CREATE TABLE t(q)",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("failed_alter{i}"), &split(c));
    }
}

#[test]
fn views_follow_the_rename() {
    // Views and CTEs: scoping decides which tokens are edited (aliases, subqueries, compound ORDER BY, rowid of the INTEGER PRIMARY KEY, unused CTEs).
    for (i, c) in [
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,'x'),(2,'y'); CREATE VIEW v AS SELECT a, (SELECT count(*) FROM t AS t2 WHERE t2.a < t.a) AS n FROM t; ALTER TABLE t RENAME a TO z; SELECT * FROM v",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t AS q WHERE q.a = t.a); ALTER TABLE t RENAME TO w; SELECT * FROM v",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH c(x) AS (SELECT a FROM t) SELECT x FROM c; ALTER TABLE t RENAME a TO z; SELECT * FROM v",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH c AS (SELECT a FROM t) SELECT a FROM c; ALTER TABLE t RENAME a TO z",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n FROM r; ALTER TABLE t RENAME a TO z",
        "CREATE TABLE t(a, b); CREATE VIEW v AS WITH RECURSIVE r(n) AS (SELECT a FROM t UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n FROM r; ALTER TABLE t RENAME TO tt",
        "CREATE TABLE t(a, b); CREATE VIEW v1 AS SELECT a FROM t; CREATE VIEW v2 AS SELECT a FROM v1; ALTER TABLE t RENAME a TO z",
        "CREATE TABLE t(a, b); CREATE VIEW v1 AS SELECT a AS a FROM t; CREATE VIEW v2 AS SELECT a FROM v1; ALTER TABLE t RENAME a TO z; SELECT sql FROM sqlite_schema",
        "CREATE TABLE t(a, b); CREATE VIEW v1 AS SELECT a FROM t; CREATE VIEW v2 AS SELECT a FROM v1; ALTER TABLE t RENAME TO z",
        "CREATE TABLE t(a, b); CREATE TABLE u(c, d); CREATE VIEW v AS SELECT a, c FROM t LEFT JOIN u ON a = c; ALTER TABLE t RENAME a TO x; ALTER TABLE u RENAME TO uu",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT group_concat(a ORDER BY b) FILTER (WHERE a > 1) FROM t; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t UNION SELECT b FROM t ORDER BY a; ALTER TABLE t RENAME a TO x",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT CASE a WHEN 1 THEN b ELSE a END, a COLLATE nocase, CAST(a AS TEXT), a BETWEEN 1 AND b, a IN (1,b), a LIKE b FROM t; ALTER TABLE t RENAME a TO \"x x\"",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM t WHERE (a, b) = (1, 2); ALTER TABLE t RENAME COLUMN b TO c",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM (SELECT a AS q FROM t) WHERE q > 0; ALTER TABLE t RENAME COLUMN a TO c; SELECT * FROM v",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM (SELECT * FROM t) AS s WHERE s.a > 0; ALTER TABLE t RENAME COLUMN a TO c",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a); CREATE VIEW v AS SELECT id, t.id, rowid FROM t; ALTER TABLE t RENAME id TO k",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a); CREATE INDEX i ON t(a, id); ALTER TABLE t RENAME id TO k; INSERT INTO t(a) VALUES(5); SELECT k, a FROM t",
        "CREATE TABLE t(a, b); CREATE TABLE u(x REFERENCES t(a), y, FOREIGN KEY(y) REFERENCES t); ALTER TABLE t RENAME a TO z; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b REFERENCES t(a)); ALTER TABLE t RENAME a TO z; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM main.t; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT \"t\".a FROM \"t\"; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.\"a\" FROM t; ALTER TABLE t RENAME a TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT t.\"a\"FROM t; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t\"x\"; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT x.a FROM t\"x\"; ALTER TABLE t RENAME TO \"w\"",
        "CREATE TABLE T(a, b); CREATE VIEW v AS SELECT t.a FROM t; ALTER TABLE t RENAME TO w",
        "CREATE TABLE t(A, b); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t RENAME a TO w",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT b FROM t; ALTER TABLE t DROP COLUMN a; SELECT * FROM v",
        "CREATE TABLE t(a, b, c CHECK (c > a)); ALTER TABLE t DROP COLUMN a",
        "CREATE TABLE t(a, b, c CHECK (c > 0)); INSERT INTO t VALUES (1,2,3); ALTER TABLE t DROP COLUMN c; INSERT INTO t VALUES(4,5); SELECT * FROM t",
        "CREATE TABLE t(a, b DEFAULT 4 NOT NULL, c); INSERT INTO t VALUES(1, 2, 3); ALTER TABLE t DROP COLUMN a; INSERT INTO t(c) VALUES(9); SELECT * FROM t",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b, c); INSERT INTO t(b,c) VALUES(1,2); ALTER TABLE t DROP COLUMN b; INSERT INTO t(c) VALUES(7); SELECT * FROM t; SELECT * FROM sqlite_sequence",
        "CREATE TABLE t(a, b); CREATE INDEX ti ON t(b); ALTER TABLE t RENAME TO u; DROP INDEX ti; CREATE INDEX ti ON u(a); ALTER TABLE u RENAME a TO z",
        "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT a FROM t; BEGIN; ALTER TABLE t RENAME a TO x; SELECT * FROM v; ROLLBACK; SELECT sql FROM sqlite_schema",
        "CREATE TABLE t(a, b); ALTER TABLE t RENAME COLUMN a TO b",
        "CREATE TABLE t(a, b); ALTER TABLE t RENAME COLUMN column TO z",
        "CREATE TABLE t(\"column\", b); ALTER TABLE t RENAME COLUMN column TO z",
        "CREATE TABLE t(\"column\", b); ALTER TABLE t RENAME column TO z",
        "CREATE TABLE t(a, b); ALTER TABLE t DROP COLUMN column",
        "CREATE TABLE t(\"column\", b); ALTER TABLE t DROP column",
        "CREATE TABLE t(a, b) STRICT; ALTER TABLE t RENAME a TO c",
        "CREATE TABLE t(a INT, b TEXT) STRICT; INSERT INTO t VALUES(1,'x'); ALTER TABLE t DROP COLUMN a; INSERT INTO t VALUES('y'); SELECT * FROM t",
        "CREATE TABLE t(a, b); ALTER TABLE t RENAME TO t2; ALTER TABLE t2 RENAME TO t; SELECT name FROM sqlite_schema",
        "CREATE TABLE t(a, b UNIQUE); INSERT INTO t VALUES(1,2); ALTER TABLE t RENAME TO u; INSERT INTO u VALUES(3,2)",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("views_follow_the_rename{i}"), &split(c));
    }
}

#[test]
fn files_the_pin_made() {
    // Expression and partial indexes (outside hipp's CREATE INDEX)
    // are edited token by token; a file with triggers is refused
    // (hipp cannot parse them; the pin edits them too).
    for (i, (prep, c)) in [
        (
            "CREATE TABLE t(a, b); CREATE INDEX i ON t(a+b, lower(b)) WHERE a > 0;",
            "ALTER TABLE t RENAME a TO x",
        ),
        (
            "CREATE TABLE t(a, b); CREATE INDEX i ON t(a+b, lower(b)) WHERE t.a > 0;",
            "ALTER TABLE t RENAME TO u",
        ),
        (
            "CREATE TABLE t(a, b); CREATE INDEX i ON t(b) WHERE a IS NOT NULL;",
            "ALTER TABLE t DROP COLUMN a",
        ),
        (
            "CREATE TABLE t(a, b, c); CREATE INDEX i ON t(b) WHERE c IS NOT NULL;",
            "ALTER TABLE t DROP COLUMN a; SELECT * FROM t",
        ),
    ]
    .iter()
    .enumerate()
    {
        same_on(&format!("pin{i}"), prep, &split(c));
    }
    // A file with triggers: refused without the `triggers` feature;
    // with it, its trigger texts are edited as the pin edits them
    // (tests/triggers_v16.rs has the corpus).
    #[cfg(not(feature = "triggers"))]
    {
        let p = common::temp_db("v16al-trigger");
        common::cli(
            &p,
            "CREATE TABLE t(a, b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;",
        );
        let (_, errs) = hipp_run(&p, &split("ALTER TABLE t RENAME TO u"));
        assert_eq!(errs, [1], "a file with triggers is refused");
        assert_eq!(
            common::cli(&p, "SELECT name FROM sqlite_schema WHERE type = 'table';"),
            "t\n"
        );
    }
    #[cfg(feature = "triggers")]
    same_on(
        "pin-trigger",
        "CREATE TABLE t(a, b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT new.a, t.b FROM t; END;",
        &split(
            "ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME a TO x; SELECT sql FROM sqlite_schema",
        ),
    );
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

/// Each engine alters the other's file: the pin's own ALTER and
/// hipp's agree on files either made.
#[test]
fn files_interoperate() {
    let p = common::temp_db("v16al-interop");
    common::cli(
        &p,
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE, c CHECK (c > 0));
         INSERT INTO t(b, c) VALUES ('x', 1), ('y', 2);
         CREATE INDEX tc ON t(c);
         CREATE VIEW v AS SELECT t.b, c FROM t WHERE c > 0;",
    );
    let (_, errs) = hipp_run(
        &p,
        &lines(
            "ALTER TABLE t RENAME COLUMN c TO cc
            ALTER TABLE t RENAME TO tt
            ALTER TABLE tt ADD COLUMN d DEFAULT 7
            INSERT INTO tt(b, cc) VALUES ('z', 3)",
        ),
    );
    assert!(errs.is_empty(), "{errs:?}");
    common::cli(
        &p,
        "ALTER TABLE tt DROP COLUMN d; INSERT INTO tt(b, cc) VALUES ('w', 4);",
    );
    let (rows, errs) = hipp_run(
        &p,
        &lines(
            "SELECT * FROM v
            SELECT * FROM tt
            SELECT * FROM sqlite_sequence
            SELECT sql FROM sqlite_schema",
        ),
    );
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(
        rows,
        [
            "x|1",
            "y|2",
            "z|3",
            "w|4",
            "1|x|1",
            "2|y|2",
            "3|z|3",
            "4|w|4",
            "tt|4",
            "CREATE TABLE \"tt\"(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE, cc CHECK (cc > 0))",
            "",
            "CREATE TABLE sqlite_sequence(name,seq)",
            "CREATE INDEX tc ON \"tt\"(cc)",
            "CREATE VIEW v AS SELECT \"tt\".b, cc FROM \"tt\" WHERE cc > 0",
        ]
    );
    assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
}

/// A generated corpus: random tables (affinities, defaults, NULLs,
/// generated and constrained columns, ADD COLUMN'd short rows),
/// indexes and views, then random RENAME TO / RENAME COLUMN / DROP
/// COLUMN / ADD COLUMN sequences — accepted or refused alike.
#[test]
fn generated_corpus() {
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
        fn pick(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    let mut rng = Rng(0xa17e_7ab1_e000_0016);
    let types = ["", "INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB"];
    let vals = [
        "NULL", "0", "1", "-7", "2.5", "'x'", "'12'", "x'4142'", "1e100",
    ];
    for case in 0..16 {
        let mut cols: Vec<String> = (0..2 + rng.pick(4)).map(|i| format!("c{i}")).collect();
        let mut defs: Vec<String> = cols
            .iter()
            .map(|c| {
                let mut d = format!("{c} {}", types[rng.pick(types.len() as u64) as usize]);
                match rng.pick(6) {
                    0 => d.push_str(" DEFAULT 5"),
                    1 => d.push_str(&format!(" CHECK ({c} IS NOT 99)")),
                    2 => d.push_str(" COLLATE NOCASE"),
                    _ => {}
                }
                d
            })
            .collect();
        if rng.pick(3) == 0 {
            defs.insert(0, "id INTEGER PRIMARY KEY".to_string());
            cols.insert(0, "id".to_string());
        }
        if rng.pick(3) == 0 {
            let g = format!("g AS ({} || 'g')", cols[cols.len() - 1]);
            defs.push(g);
        }
        let mut stmts = vec![format!("CREATE TABLE t({})", defs.join(", "))];
        for _ in 0..4 {
            let row: Vec<&str> = cols
                .iter()
                .map(|c| {
                    if c == "id" {
                        "NULL"
                    } else {
                        vals[rng.pick(vals.len() as u64) as usize]
                    }
                })
                .collect();
            stmts.push(format!(
                "INSERT INTO t({}) VALUES ({})",
                cols.join(", "),
                row.join(", ")
            ));
        }
        if rng.pick(2) == 0 {
            stmts.push("ALTER TABLE t ADD COLUMN late DEFAULT 'L'".to_string());
            cols.push("late".to_string());
        }
        let k = cols[rng.pick(cols.len() as u64) as usize].clone();
        stmts.push(format!("CREATE INDEX ti ON t({k})"));
        stmts.push(format!(
            "CREATE VIEW v AS SELECT {}, t.{} AS q FROM t WHERE {} IS NOT NULL ORDER BY 1",
            cols[0],
            cols[cols.len() - 1],
            cols[0]
        ));
        let mut tname = "t".to_string();
        for step in 0..5 {
            let c = cols[rng.pick(cols.len() as u64) as usize].clone();
            let s = match rng.pick(5) {
                0 => {
                    let n = format!("r{case}_{step}");
                    let s = format!("ALTER TABLE {tname} RENAME TO {n}");
                    tname = n;
                    s
                }
                1 | 2 => {
                    let n = if rng.pick(2) == 0 {
                        format!("n{step}")
                    } else {
                        format!("\"N {step}\"")
                    };
                    let s = format!("ALTER TABLE {tname} RENAME COLUMN {c} TO {n}");
                    // Later steps use the new name (if the rename
                    // failed, they fail on both engines alike).
                    for x in cols.iter_mut() {
                        if *x == c {
                            *x = n.clone();
                        }
                    }
                    s
                }
                3 => format!("ALTER TABLE {tname} DROP COLUMN {c}"),
                _ => format!("ALTER TABLE {tname} ADD COLUMN a{step} DEFAULT {step}"),
            };
            stmts.push(s);
            stmts.push("SELECT type, name, tbl_name, sql FROM sqlite_schema".to_string());
            // Column names are whatever the renames made them: read
            // the table through its current name only.
            stmts.push(format!("SELECT * FROM {tname}"));
            stmts.push("SELECT * FROM v".to_string());
        }
        same(&format!("gen{case}"), &stmts);
    }
}
