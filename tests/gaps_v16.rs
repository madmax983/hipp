//! v0.16 honest-gap sweep, differentially vs the pinned oracle
//! (SQLite 3.54.1): every case runs statement by statement on a
//! fresh hipp file and a fresh pin file; the rows, which statements
//! fail, and the final files (`.dump` by the pin, integrity check
//! "ok") must agree. Semantics probed on the pin first (STATUS.md,
//! "Honest-gap sweep"):
//!
//! - Constraint-level `ON CONFLICT` clauses: an explicit statement
//!   `OR` clause wins, then the constraint's own clause, then ABORT
//!   (`ConflictAction::over`). NOT NULL ... REPLACE takes the
//!   column default (a NULL default fails as ABORT, after the other
//!   columns had their turn); a CHECK's clause is parsed and
//!   ignored; the column-level `CHECK ... ON CONFLICT` is a syntax
//!   error. Indexes are checked newest first with every
//!   `ON CONFLICT REPLACE` one last, the rowid REPLACE after all
//!   of them; the first violated constraint that does not resolve
//!   by REPLACE decides. An upsert's DO UPDATE always resolves by
//!   ABORT. Trigger bodies take the firing statement's explicit
//!   clause, else their own constraints'.
//! - `INDEXED BY name` / `NOT INDEXED` in FROM, UPDATE and DELETE:
//!   "no such index" (also on a view or CTE), "no query solution"
//!   for a partial index the WHERE does not imply; the forced
//!   index's full scan is always a candidate, so rows come in its
//!   order; NOT INDEXED keeps rowid lookups and is ignored on a
//!   WITHOUT ROWID table.
//! - Three-part names `schema.table.column` in every expression
//!   context (not RETURNING, generated columns, index keys, or
//!   `schema.table.*`), the table part matching the source's
//!   qualifier (its alias when it has one).
//! - TEMP across databases: main statements reading temp tables,
//!   temp INSERT / CREATE TABLE ... AS fed by a SELECT over main,
//!   temp views over both, pragma functions on temp objects.

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// A thread with room for nested trigger programs.
fn big<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test thread");
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

/// hipp: every statement in order, rows printed, failing statements
/// noted by 1-based number (and kind) and skipped past.
fn hipp_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, Vec<usize>, Vec<String>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        let mut errs = Vec::new();
        let mut kinds = Vec::new();
        for (i, sql) in stmts.iter().enumerate() {
            let mut q = match db.prepare(sql).await {
                Ok(q) => q,
                Err(e) => {
                    errs.push(i + 1);
                    kinds.push(format!("{}: {e:?}", i + 1));
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
                    Err(e) => {
                        errs.push(i + 1);
                        kinds.push(format!("{}: {e:?}", i + 1));
                        break;
                    }
                }
            }
        }
        (lines, errs, kinds)
    })
}

/// The pin's shell over stdin, one statement per line: stdout rows,
/// and the lines its errors name.
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
            if let Ok(n) = n.parse()
                && !errs.contains(&n)
            {
                errs.push(n);
            }
        }
    }
    (lines, errs)
}

/// Run `stmts` on twin fresh files: rows, failing statements and
/// the final files must agree. Returns (rows, failed statements).
fn same(tag: &str, stmts: &[String]) -> (usize, usize) {
    let h = common::temp_db(&format!("v16gap-{tag}-h"));
    let o = common::temp_db(&format!("v16gap-{tag}-o"));
    let (hl, he, kinds) = hipp_run(&h, stmts);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ ({kinds:?}):\n{script}");
    assert_eq!(
        he, oe,
        "{tag}: failing statements differ ({kinds:?}):\n{script}"
    );
    let hi = common::cli(&h, "PRAGMA integrity_check;");
    assert_eq!(hi.trim(), "ok", "{tag}: hipp's file\n{script}");
    assert_eq!(
        common::cli(&h, ".dump"),
        common::cli(&o, ".dump"),
        "{tag}: final files differ:\n{script}"
    );
    (hl.len(), he.len())
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn run_same(tag: &'static str, script: &'static str) {
    big(move || {
        same(tag, &lines(script));
    });
}

/// The deterministic PRNG of `tests/fuzz_smoke.rs` (xorshift64).
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
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

/// Generated tables with random constraint-level clauses, written
/// by random multi-row INSERTs and UPDATEs under random statement
/// clauses, in and out of explicit transactions.
#[test]
fn conflict_clauses_generated() {
    const ACTS: [&str; 7] = [
        "",
        "",
        " ON CONFLICT ROLLBACK",
        " ON CONFLICT ABORT",
        " ON CONFLICT FAIL",
        " ON CONFLICT IGNORE",
        " ON CONFLICT REPLACE",
    ];
    const OR: [&str; 7] = [
        "",
        "",
        "",
        " OR ABORT",
        " OR FAIL",
        " OR IGNORE",
        " OR REPLACE",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut seen, mut fails) = (0, 0);
    for case in 0..160 {
        let mut stmts = Vec::new();
        let ipk = rng.below(2) == 0;
        let mut cols = Vec::new();
        if ipk {
            cols.push(format!("id INTEGER PRIMARY KEY{}", rng.pick(&ACTS)));
        }
        cols.push(format!(
            "a NOT NULL{}{}",
            rng.pick(&ACTS),
            if rng.below(2) == 0 { " DEFAULT 7" } else { "" }
        ));
        cols.push(format!("b UNIQUE{}", rng.pick(&ACTS)));
        cols.push(format!(
            "c{}",
            if rng.below(2) == 0 {
                " NOT NULL DEFAULT 0"
            } else {
                ""
            }
        ));
        if rng.below(2) == 0 {
            cols.push(format!("UNIQUE(a, c){}", rng.pick(&ACTS)));
        }
        stmts.push(format!("CREATE TABLE t({})", cols.join(", ")));
        if rng.below(3) == 0 {
            stmts.push("CREATE UNIQUE INDEX tc ON t(c) WHERE c > 3".to_string());
        }
        let val = |rng: &mut Rng| match rng.below(6) {
            0 => "NULL".to_string(),
            n => (n % 4).to_string(),
        };
        let mut open = false;
        for _ in 0..8 {
            match rng.below(10) {
                0 if !open => {
                    stmts.push("BEGIN".to_string());
                    open = true;
                }
                1 if open => {
                    stmts.push("COMMIT".to_string());
                    open = false;
                }
                2..=6 => {
                    let n = 1 + rng.below(3);
                    let mut rows = Vec::new();
                    for _ in 0..n {
                        let mut r = Vec::new();
                        if ipk {
                            r.push(val(&mut rng));
                        }
                        r.push(val(&mut rng));
                        r.push(val(&mut rng));
                        r.push(val(&mut rng));
                        rows.push(format!("({})", r.join(",")));
                    }
                    stmts.push(format!(
                        "INSERT{} INTO t VALUES {}",
                        rng.pick(&OR),
                        rows.join(",")
                    ));
                }
                _ => {
                    let col = rng.pick(&["a", "b", "c"]);
                    stmts.push(format!(
                        "UPDATE{} t SET {col} = {} WHERE {} >= {}",
                        rng.pick(&OR),
                        val(&mut rng),
                        rng.pick(&["a", "b", "c", "rowid"]),
                        rng.below(4)
                    ));
                }
            }
        }
        stmts.push("SELECT * FROM t ORDER BY rowid".to_string());
        if open {
            stmts.push("COMMIT".to_string());
        }
        let tag = leak(format!("gen-cc-{case}"));
        let (r, f) = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || same(tag, &stmts))
            .expect("spawn")
            .join()
            .expect("case");
        seen += r;
        fails += f;
    }
    // The corpus exercises both outcomes.
    assert!(seen > 100 && fails > 20, "rows {seen}, failures {fails}");
}

/// Generated `INDEXED BY` / `NOT INDEXED` queries without ORDER BY:
/// the rows must come in the forced access path's order.
#[test]
fn indexed_by_generated() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut seen = 0;
    for case in 0..96 {
        let mut stmts = vec![
            "CREATE TABLE t(a, b, c)".to_string(),
            "CREATE INDEX ia ON t(a)".to_string(),
            "CREATE INDEX ibd ON t(b DESC, a)".to_string(),
            "CREATE INDEX ic ON t(c) WHERE c > 2".to_string(),
        ];
        let mut rows = Vec::new();
        for _ in 0..12 {
            rows.push(format!(
                "({},{},{})",
                rng.below(5),
                rng.below(4),
                if rng.below(4) == 0 {
                    "NULL".to_string()
                } else {
                    rng.below(6).to_string()
                }
            ));
        }
        stmts.push(format!("INSERT INTO t VALUES {}", rows.join(",")));
        for _ in 0..6 {
            let hint = rng.pick(&[
                " INDEXED BY ia",
                " INDEXED BY ibd",
                " INDEXED BY ic",
                " NOT INDEXED",
                "",
            ]);
            let term = match rng.below(6) {
                0 => String::new(),
                1 => format!(" WHERE a = {}", rng.below(5)),
                2 => format!(" WHERE b > {}", rng.below(4)),
                3 => format!(" WHERE c > {}", 2 + rng.below(3)),
                4 => format!(" WHERE a IN ({}, {})", rng.below(5), rng.below(5)),
                _ => format!(" WHERE rowid = {}", 1 + rng.below(12)),
            };
            stmts.push(format!("SELECT rowid, * FROM t{hint}{term}"));
        }
        let tag = leak(format!("gen-ib-{case}"));
        seen += same(tag, &stmts).0;
    }
    assert!(seen > 200, "rows {seen}");
}

/// A file the pin wrote with three-part names in a CHECK, a partial
/// index's WHERE and a view (pin-written definitions used to be
/// opaque): hipp writes it, the pin checks it, and an ALTER TABLE
/// RENAME rewrites the names as the pin's does.
#[test]
fn pin_written_three_part_names() {
    let h = common::temp_db("v16gap-pin3-h");
    let o = common::temp_db("v16gap-pin3-o");
    let setup = "CREATE TABLE t(a, b, CHECK (main.t.a > 0)); \
                 CREATE INDEX i ON t(b) WHERE main.t.b > 0; \
                 CREATE VIEW v AS SELECT main.t.a AS z FROM t; \
                 INSERT INTO t VALUES (1, 2), (2, -1);";
    common::cli(&h, setup);
    common::cli(&o, setup);
    let stmts = lines(
        "INSERT INTO t VALUES (3, 5)
         INSERT INTO t VALUES (-3, 5)
         UPDATE t SET b = b + 1
         SELECT * FROM t INDEXED BY i WHERE b > 0
         SELECT * FROM v
         ALTER TABLE t RENAME TO t2
         SELECT sql FROM sqlite_master ORDER BY name",
    );
    let (hl, he, kinds) = hipp_run(&h, &stmts);
    let (ol, oe) = oracle_run(&o, &stmts);
    assert_eq!(hl, ol, "{kinds:?}");
    assert_eq!(he, oe, "{kinds:?}");
    assert_eq!(common::cli(&h, "PRAGMA integrity_check;").trim(), "ok");
    assert_eq!(common::cli(&h, ".dump"), common::cli(&o, ".dump"));
}

#[test]
fn conflict_clauses() {
    for (tag, script) in [
        (
            "nn_ignore",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE, b);
INSERT INTO t VALUES(1,1),(NULL,2),(3,3);
SELECT * FROM t;
"#,
        ),
        (
            "nn_replace_default",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE DEFAULT 9, b);
INSERT INTO t VALUES(1,1),(NULL,2);
SELECT * FROM t;
"#,
        ),
        (
            "nn_replace_nodefault",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE, b);
INSERT INTO t VALUES(1,1),(NULL,2);
SELECT * FROM t;
"#,
        ),
        (
            "nn_fail",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT FAIL, b);
INSERT INTO t VALUES(1,1),(NULL,2);
SELECT * FROM t;
"#,
        ),
        (
            "nn_fail_or_abort",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT FAIL, b);
INSERT OR ABORT INTO t VALUES(1,1),(NULL,2);
SELECT * FROM t;
"#,
        ),
        (
            "uq_ignore",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, b);
INSERT INTO t VALUES(1,1),(1,2),(3,3);
SELECT * FROM t;
"#,
        ),
        (
            "uq_replace",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b);
INSERT INTO t VALUES(1,1),(1,2),(3,3);
SELECT * FROM t;
"#,
        ),
        (
            "ipk_replace",
            r#"
CREATE TABLE t(a INTEGER PRIMARY KEY ON CONFLICT REPLACE, b);
INSERT INTO t VALUES(1,1),(1,2),(3,3);
SELECT * FROM t;
"#,
        ),
        (
            "check_ignore_ignored",
            r#"
CREATE TABLE t(a, b, CHECK(a>0) ON CONFLICT IGNORE);
INSERT INTO t VALUES(1,1),(-1,2),(3,3);
SELECT * FROM t;
"#,
        ),
        (
            "check_col_onconf_syntax",
            r#"
CREATE TABLE t(a CHECK(a>0) ON CONFLICT IGNORE, b);
SELECT count(*) FROM sqlite_master;
"#,
        ),
        (
            "tbl_uq_ignore",
            r#"
CREATE TABLE t(a, b, UNIQUE(a) ON CONFLICT IGNORE);
INSERT INTO t VALUES(1,1),(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "tbl_pk_replace",
            r#"
CREATE TABLE t(a, b, PRIMARY KEY(a) ON CONFLICT REPLACE);
INSERT INTO t VALUES(1,1),(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "order_ignore_fail",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, b UNIQUE ON CONFLICT FAIL);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,2),(1,1);
SELECT * FROM t;
"#,
        ),
        (
            "order_fail_ignore",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT FAIL, b UNIQUE ON CONFLICT IGNORE);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,2),(1,1);
SELECT * FROM t;
"#,
        ),
        (
            "replace_vs_default",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b UNIQUE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "ipk_replace_uq_ignore",
            r#"
CREATE TABLE t(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, b UNIQUE ON CONFLICT IGNORE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "ipk_ignore_uq_replace",
            r#"
CREATE TABLE t(id INTEGER PRIMARY KEY ON CONFLICT IGNORE, b UNIQUE ON CONFLICT REPLACE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "ipk_fail",
            r#"
CREATE TABLE t(id INTEGER PRIMARY KEY ON CONFLICT FAIL, b);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,2),(1,3),(4,4);
SELECT * FROM t;
"#,
        ),
        (
            "upd_nn_ignore",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE, b);
INSERT INTO t VALUES(1,1),(2,2);
UPDATE t SET a=NULL WHERE b=2;
SELECT * FROM t;
"#,
        ),
        (
            "upd_nn_replace",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE DEFAULT 7, b);
INSERT INTO t VALUES(1,1),(2,2);
UPDATE t SET a=NULL WHERE b=2;
SELECT * FROM t;
"#,
        ),
        (
            "upd_nn_fail",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT FAIL, b);
INSERT INTO t VALUES(1,1),(2,2),(3,3);
UPDATE t SET a=CASE b WHEN 2 THEN NULL ELSE a+10 END;
SELECT * FROM t;
"#,
        ),
        (
            "nn_rollback_per_column",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT ROLLBACK, b NOT NULL);
BEGIN;
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,NULL);
INSERT INTO t VALUES(3,3);
COMMIT;
SELECT * FROM t;
"#,
        ),
        (
            "nn_rollback_hit",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT ROLLBACK, b NOT NULL);
BEGIN;
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(NULL,2);
INSERT INTO t VALUES(3,3);
COMMIT;
SELECT * FROM t;
"#,
        ),
        (
            "uq_rollback_per_index",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT ROLLBACK, b UNIQUE);
BEGIN;
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,1);
INSERT INTO t VALUES(3,3);
COMMIT;
SELECT * FROM t;
"#,
        ),
        (
            "or_abort_overrides_rollback",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT ROLLBACK, b);
BEGIN;
INSERT INTO t VALUES(1,1);
INSERT OR ABORT INTO t VALUES(1,1);
INSERT INTO t VALUES(3,3);
COMMIT;
SELECT * FROM t;
"#,
        ),
        (
            "explicit_index_first",
            r#"
CREATE TABLE t(a, b, UNIQUE(a) ON CONFLICT IGNORE);
CREATE UNIQUE INDEX i ON t(a);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(1,2);
SELECT * FROM t;
"#,
        ),
        (
            "conflicting_clauses",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE UNIQUE ON CONFLICT FAIL);
SELECT count(*) FROM sqlite_master;
"#,
        ),
        (
            "merge_clause",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, UNIQUE(a));
INSERT INTO t VALUES(1);
INSERT INTO t VALUES(1);
SELECT * FROM t;
SELECT name FROM sqlite_master;
"#,
        ),
        (
            "pk_uq_conflict",
            r#"
CREATE TABLE t(a PRIMARY KEY ON CONFLICT IGNORE, b UNIQUE, UNIQUE(a) ON CONFLICT FAIL);
SELECT count(*) FROM sqlite_master;
"#,
        ),
        (
            "nn_last_wins",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE NOT NULL ON CONFLICT FAIL, b);
INSERT INTO t VALUES(1,1),(NULL,2);
SELECT * FROM t;
"#,
        ),
        (
            "upsert_other_ignore",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, b UNIQUE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(1,5) ON CONFLICT(b) DO UPDATE SET a=a+100;
SELECT * FROM t;
"#,
        ),
        (
            "upsert_other_replace",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b UNIQUE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(1,5) ON CONFLICT(b) DO UPDATE SET a=a+100;
SELECT * FROM t;
"#,
        ),
        (
            "upsert_doupdate_abort",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b UNIQUE);
INSERT INTO t VALUES(1,1),(2,2);
INSERT INTO t VALUES(5,2) ON CONFLICT(b) DO UPDATE SET a=1;
SELECT * FROM t;
"#,
        ),
        (
            "upsert_nn_ignore_abort",
            r#"
CREATE TABLE t(k UNIQUE, a NOT NULL ON CONFLICT IGNORE);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(1,2) ON CONFLICT(k) DO UPDATE SET a=NULL;
SELECT * FROM t;
"#,
        ),
        (
            "upsert_or_fail_doupdate",
            r#"
CREATE TABLE t(k UNIQUE, a NOT NULL);
INSERT INTO t VALUES(1,1);
INSERT OR FAIL INTO t VALUES(5,5),(1,2) ON CONFLICT(k) DO UPDATE SET a=NULL;
SELECT * FROM t;
"#,
        ),
        (
            "wr_pk_replace",
            r#"
CREATE TABLE t(k PRIMARY KEY ON CONFLICT REPLACE, v) WITHOUT ROWID;
INSERT INTO t VALUES(1,1),(1,2),(2,3);
SELECT * FROM t;
"#,
        ),
        (
            "wr_pk_ignore",
            r#"
CREATE TABLE t(k PRIMARY KEY ON CONFLICT IGNORE, v) WITHOUT ROWID;
INSERT INTO t VALUES(1,1),(1,2),(2,3);
SELECT * FROM t;
"#,
        ),
        (
            "upd_ipk_replace",
            r#"
CREATE TABLE t(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, v);
INSERT INTO t VALUES(1,'a'),(2,'b');
UPDATE t SET id=2 WHERE id=1;
SELECT * FROM t;
"#,
        ),
        (
            "upd_uq_ignore",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, b);
INSERT INTO t VALUES(1,1),(2,2);
UPDATE t SET a=1 WHERE b=2;
SELECT * FROM t;
"#,
        ),
        (
            "upd_uq_replace",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b);
INSERT INTO t VALUES(1,1),(2,2);
UPDATE t SET a=1 WHERE b=2;
SELECT * FROM t;
"#,
        ),
        (
            "upd_uq_fail",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT FAIL, b);
INSERT INTO t VALUES(1,1),(2,2),(3,3);
UPDATE t SET a=a+10 WHERE b<3;
UPDATE t SET a=3 WHERE b=1;
SELECT * FROM t;
"#,
        ),
        (
            "trig_replace_recursive",
            r#"
CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, v);
CREATE TABLE log(x);
CREATE TRIGGER d AFTER DELETE ON t BEGIN INSERT INTO log VALUES(old.v); END;
INSERT INTO t VALUES(1,'a');
INSERT INTO t VALUES(1,'b');
PRAGMA recursive_triggers=1;
INSERT INTO t VALUES(1,'c');
SELECT * FROM t;
SELECT * FROM log;
"#,
        ),
        (
            "fk_cascade_replace",
            r#"
PRAGMA foreign_keys=1;
CREATE TABLE p(a UNIQUE ON CONFLICT REPLACE, v);
CREATE TABLE c(x REFERENCES p(a) ON DELETE CASCADE);
INSERT INTO p VALUES(1,'a');
INSERT INTO c VALUES(1);
INSERT INTO p VALUES(1,'b');
SELECT * FROM p;
SELECT count(*) FROM c;
"#,
        ),
        (
            "trig_body_inherits_default",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE, b);
CREATE TABLE s(x);
CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO t VALUES(new.x, 1); END;
INSERT INTO s VALUES(NULL),(2);
SELECT * FROM t;
SELECT * FROM s;
"#,
        ),
        (
            "trig_body_outer_abort",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE, b);
CREATE TABLE s(x);
CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO t VALUES(new.x, 1); END;
INSERT OR ABORT INTO s VALUES(NULL),(2);
SELECT * FROM t;
SELECT * FROM s;
"#,
        ),
        (
            "nn_replace_null_default_then_fail",
            r#"
CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE DEFAULT NULL, b NOT NULL ON CONFLICT FAIL);
INSERT INTO t VALUES(1,1);
INSERT INTO t VALUES(2,2),(NULL,NULL);
SELECT * FROM t;
"#,
        ),
    ] {
        run_same(leak(format!("conflict_clauses-{tag}")), script);
    }
}

#[test]
fn indexed_by() {
    for (tag, script) in [
        (
            "ib_full",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t INDEXED BY ia;
SELECT * FROM t INDEXED BY ia WHERE a>0;
SELECT * FROM t INDEXED BY ib WHERE a>0;
"#,
        ),
        (
            "nosuch",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t INDEXED BY nosuch;
SELECT 1 FROM t INDEXED BY ux;
SELECT * FROM t INDEXED BY IA WHERE a=2;
"#,
        ),
        (
            "not_indexed",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t NOT INDEXED WHERE a>0;
SELECT * FROM t NOT INDEXED WHERE rowid=2;
SELECT * FROM t NOT INDEXED ORDER BY b;
"#,
        ),
        (
            "alias",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t AS x INDEXED BY ia WHERE x.a=1;
SELECT * FROM t x INDEXED BY ib WHERE x.a>=1;
SELECT * FROM t AS z NOT INDEXED;
"#,
        ),
        (
            "partial",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t INDEXED BY ip;
SELECT * FROM t INDEXED BY ip WHERE c>'x';
SELECT * FROM t INDEXED BY ip WHERE c='z';
"#,
        ),
        (
            "write",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
UPDATE t INDEXED BY ia SET c='q' WHERE a=1;
DELETE FROM t INDEXED BY ib WHERE b=3;
SELECT * FROM t;
UPDATE t INDEXED BY ib SET c='q' WHERE a=1;
DELETE FROM t NOT INDEXED WHERE a=3;
SELECT * FROM t;
UPDATE t INDEXED BY nosuch SET c=1;
DELETE FROM t INDEXED BY ib;
SELECT count(*) FROM t;
"#,
        ),
        (
            "write_partial",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
UPDATE t INDEXED BY ip SET b=b WHERE c='y';
DELETE FROM t INDEXED BY ip WHERE 1;
SELECT count(*) FROM t;
DELETE FROM t INDEXED BY ip WHERE c>'x';
SELECT * FROM t;
"#,
        ),
        (
            "view_cte",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
CREATE VIEW v AS SELECT * FROM t;
SELECT * FROM v INDEXED BY ia;
WITH w AS (SELECT * FROM t) SELECT * FROM w INDEXED BY ia;
SELECT * FROM (SELECT * FROM t) INDEXED BY ia;
SELECT * FROM t INDEXED BY main.ia;
"#,
        ),
        (
            "join",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM u, t INDEXED BY ib WHERE t.a=u.x;
SELECT * FROM u JOIN t INDEXED BY ia ON t.a=u.x;
SELECT * FROM u LEFT JOIN t NOT INDEXED ON t.a=u.x ORDER BY 1;
"#,
        ),
        (
            "terms",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT * FROM t INDEXED BY ia ORDER BY b;
SELECT * FROM t INDEXED BY ia WHERE a IN (1,2);
SELECT * FROM t INDEXED BY ia WHERE a IS NULL;
SELECT * FROM t INDEXED BY ia WHERE a=1 OR b=3;
SELECT * FROM t INDEXED BY ia WHERE rowid=2;
SELECT * FROM t INDEXED BY ia WHERE a=1 AND b=2;
INSERT INTO t INDEXED BY ia VALUES(1,2,3);
"#,
        ),
        (
            "autoidx",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
CREATE TABLE k(id INTEGER PRIMARY KEY, v UNIQUE);
INSERT INTO k VALUES(1,'b'),(2,'a');
SELECT * FROM k INDEXED BY sqlite_autoindex_k_1;
SELECT * FROM k NOT INDEXED WHERE v='a';
SELECT * FROM k NOT INDEXED ORDER BY v;
SELECT * FROM t INDEXED BY sqlite_autoindex_t_1;
"#,
        ),
        (
            "expr",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
CREATE TABLE e(a,b);
CREATE INDEX ie ON e(a+b);
INSERT INTO e VALUES(1,5),(2,1),(0,0);
SELECT * FROM e INDEXED BY ie;
SELECT * FROM e INDEXED BY ie WHERE a+b>1;
SELECT * FROM e NOT INDEXED WHERE a+b>1;
"#,
        ),
        (
            "temp",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
CREATE TEMP TABLE tt(x);
CREATE INDEX temp.tx ON tt(x);
INSERT INTO tt VALUES(2),(1);
SELECT * FROM tt INDEXED BY tx;
SELECT * FROM temp.tt INDEXED BY tx;
SELECT * FROM tt INDEXED BY ia;
"#,
        ),
        (
            "subq",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT (SELECT max(b) FROM t INDEXED BY ia WHERE a<3);
SELECT x FROM u WHERE x IN (SELECT a FROM t INDEXED BY ib);
SELECT * FROM t INDEXED BY ib WHERE b>1 LIMIT 1;
SELECT a FROM t INDEXED BY ib GROUP BY a;
"#,
        ),
        (
            "wr",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE INDEX wv ON w(v);
INSERT INTO w VALUES(1,'c'),(2,'a'),(3,'b');
SELECT * FROM w INDEXED BY wv;
SELECT * FROM w NOT INDEXED;
SELECT * FROM w NOT INDEXED WHERE v>'a';
"#,
        ),
        (
            "indexed_alias",
            r#"
CREATE TABLE t(a, b, c);
CREATE INDEX ia ON t(a);
CREATE INDEX ib ON t(b);
CREATE INDEX ip ON t(c) WHERE c>'x';
CREATE TABLE u(x);
CREATE INDEX ux ON u(x);
INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(2,3,'z');
INSERT INTO u VALUES(1),(2);
SELECT indexed.a FROM t indexed;
SELECT * FROM t AS indexed INDEXED BY ia;
"#,
        ),
    ] {
        run_same(leak(format!("indexed_by-{tag}")), script);
    }
}

#[test]
fn three_part_names() {
    for (tag, script) in [
        (
            "basic",
            r#"
CREATE TABLE t(a,b);
INSERT INTO t VALUES(1,2),(3,4);
SELECT main.t.a, main.t.b FROM t;
SELECT main.t.a FROM main.t;
SELECT temp.t.a FROM t;
SELECT foo.t.a FROM t;
SELECT main.t.a FROM t AS x;
SELECT main.x.a FROM t AS x;
SELECT main.t.* FROM t;
SELECT * FROM t WHERE main.t.a=3;
SELECT main.T.A FROM T;
SELECT main.t.rowid FROM t;
SELECT "main"."t"."a" FROM t;
SELECT main.t.nosuch FROM t;
"#,
        ),
        (
            "temp_shadow",
            r#"
CREATE TABLE t(a,b);
CREATE TEMP TABLE t(a,b);
INSERT INTO main.t VALUES(1,2);
INSERT INTO temp.t VALUES(5,6);
SELECT temp.t.a FROM temp.t;
SELECT main.t.a FROM main.t;
SELECT main.t.a FROM t;
SELECT temp.t.a FROM t;
SELECT t.a FROM main.t;
"#,
        ),
        (
            "check_partial",
            r#"
CREATE TABLE t(a,b, CHECK(main.t.a>0));
INSERT INTO t VALUES(1,2);
INSERT INTO t VALUES(-1,2);
CREATE INDEX i ON t(b) WHERE main.t.b > 0;
CREATE INDEX j ON t(main.t.a+1);
UPDATE t SET main.t.a=5;
UPDATE t SET a=main.t.a+1 WHERE main.t.b=2;
SELECT * FROM t;
DELETE FROM t WHERE main.t.a=2;
SELECT count(*) FROM t;
INSERT INTO t VALUES(7,8);
SELECT * FROM t INDEXED BY i WHERE b>0;
PRAGMA integrity_check;
"#,
        ),
        (
            "clauses",
            r#"
CREATE TABLE t(a,b);
INSERT INTO t VALUES(1,2);
SELECT * FROM t ORDER BY main.t.a;
SELECT main.t.a, count(*) FROM t GROUP BY main.t.a;
SELECT (SELECT main.t.b) FROM t;
SELECT * FROM t JOIN t AS u ON main.t.a = u.a;
INSERT INTO t VALUES(3,4) RETURNING main.t.a;
CREATE TABLE s(x, y AS (main.s.x*2));
CREATE TABLE d(x DEFAULT (main.d.x));
"#,
        ),
        (
            "views_cte",
            r#"
CREATE TABLE t(a,b);
INSERT INTO t VALUES(1,2);
CREATE VIEW v AS SELECT * FROM t;
SELECT main.v.a FROM v;
WITH w AS (SELECT * FROM t) SELECT main.w.a FROM w;
SELECT main.s.a FROM (SELECT * FROM t) s;
SELECT main.sqlite_master.name FROM sqlite_master;
SELECT main.sqlite_schema.name FROM sqlite_schema;
SELECT main.sqlite_master.name FROM sqlite_schema;
SELECT MAIN.t.a FROM t;
SELECT main.t.a FROM main.t AS t;
CREATE VIEW v2 AS SELECT main.t.a AS z FROM t;
SELECT * FROM v2;
SELECT sql FROM sqlite_master WHERE name='v2';
SELECT main.t.a FROM t, t AS u;
SELECT x.t.a FROM t;
CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t SELECT main.t.a+100, 0 FROM t WHERE main.t.a<10 AND new.b>0; END;
INSERT INTO t VALUES(5,5);
SELECT * FROM t ORDER BY 1;
SELECT main.t.a FROM t WHERE main.t.a IN (SELECT main.t.a FROM t) ORDER BY 1;
"#,
        ),
        (
            "temp_only",
            r#"
CREATE TABLE t(a,b);
CREATE TEMP TABLE tt(x);
INSERT INTO t VALUES(1,2);
INSERT INTO tt VALUES(9);
SELECT temp.tt.x FROM tt;
SELECT main.tt.x FROM tt;
SELECT temp.tt.x FROM temp.tt;
UPDATE tt SET x=temp.tt.x+1;
SELECT * FROM tt;
DELETE FROM tt WHERE temp.tt.x=10;
SELECT count(*) FROM tt;
"#,
        ),
    ] {
        run_same(leak(format!("three_part_names-{tag}")), script);
    }
}

#[test]
fn temp_across_databases() {
    for (tag, script) in [
        (
            "join",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
SELECT * FROM m, t WHERE m.id=t.id ORDER BY 1;
SELECT m.a, t.c FROM m JOIN t USING(id) ORDER BY 1;
SELECT * FROM m LEFT JOIN t ON m.id=t.id ORDER BY m.id;
SELECT * FROM main.m, temp.t WHERE m.id = t.id;
SELECT typeof(c), c FROM t ORDER BY id;
"#,
        ),
        (
            "subq",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
SELECT * FROM m WHERE id IN (SELECT id FROM t) ORDER BY 1;
SELECT *, (SELECT count(*) FROM m WHERE m.id<=t.id) FROM t ORDER BY 1;
SELECT * FROM t WHERE EXISTS (SELECT 1 FROM m WHERE m.id=t.id) ORDER BY 1;
SELECT id FROM m UNION SELECT id FROM t ORDER BY 1;
SELECT * FROM m WHERE id IN temp.t;
"#,
        ),
        (
            "ins_main",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
INSERT INTO m(a,b) SELECT c, d FROM t;
SELECT * FROM m ORDER BY 1;
UPDATE m SET a=(SELECT c FROM t WHERE t.id=m.id) WHERE id IN (SELECT id FROM t);
SELECT * FROM m ORDER BY 1;
DELETE FROM m WHERE id IN (SELECT id FROM temp.t);
SELECT * FROM m ORDER BY 1;
UPDATE m SET b='u' FROM t WHERE t.id=m.id+1;
SELECT * FROM m ORDER BY 1;
"#,
        ),
        (
            "ctas_main",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TABLE n AS SELECT m.a, t.c FROM m, t WHERE m.id=t.id;
SELECT * FROM n;
SELECT sql FROM sqlite_master WHERE name='n';
"#,
        ),
        (
            "ins_temp",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
INSERT INTO t(c,d) SELECT a,b FROM m;
SELECT * FROM t ORDER BY 1;
INSERT INTO temp.t SELECT * FROM main.m WHERE id=2;
"#,
        ),
        (
            "ctas_temp",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TEMP TABLE u AS SELECT * FROM m;
SELECT * FROM u;
"#,
        ),
        (
            "temp_view",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TEMP VIEW v AS SELECT m.a, t.d FROM m JOIN t ON m.id=t.id;
SELECT * FROM v ORDER BY 1;
CREATE TEMP VIEW w AS SELECT * FROM m;
SELECT * FROM w, t WHERE w.id=t.id ORDER BY 1;
SELECT count(*) FROM w;
"#,
        ),
        (
            "shadow",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TEMP TABLE m(z);
INSERT INTO temp.m VALUES(99);
SELECT * FROM m;
SELECT * FROM main.m ORDER BY 1;
SELECT * FROM main.m, m ORDER BY 1;
SELECT * FROM temp.nosuch;
"#,
        ),
        (
            "three_part_cross",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
SELECT temp.t.d, main.m.b FROM t, m WHERE t.id=m.id ORDER BY 1;
SELECT main.t.d FROM t, m;
"#,
        ),
        (
            "fed_more",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TABLE m2(a INT, b);
INSERT INTO m2 VALUES(1,2),(3,4);
CREATE TEMP TABLE u AS SELECT * FROM m2;
SELECT changes(), total_changes(), last_insert_rowid();
SELECT sql FROM sqlite_temp_master WHERE name='u';
INSERT INTO u SELECT a*10, b FROM m2;
SELECT changes(), last_insert_rowid();
SELECT * FROM u;
CREATE TEMP TABLE IF NOT EXISTS u AS SELECT * FROM m2;
SELECT count(*) FROM u;
CREATE TEMP TABLE u AS SELECT * FROM m2;
"#,
        ),
        (
            "fed_conflict",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
INSERT INTO m VALUES(1,10,'x'),(2,20,'y'),(3,30,'z');
CREATE TEMP TABLE t(id INTEGER PRIMARY KEY, c REAL, d);
INSERT INTO t VALUES(1,1.5,'p'),(3,'7','q'),(4,NULL,'r');
CREATE TABLE m2(a INT, b);
INSERT INTO m2 VALUES(1,2),(3,4);
CREATE TEMP TABLE k(x PRIMARY KEY, y);
INSERT INTO k SELECT * FROM m2;
INSERT OR REPLACE INTO k SELECT a, b*100 FROM m2;
SELECT * FROM k;
INSERT INTO k SELECT * FROM m2;
SELECT * FROM k;
INSERT INTO k(y) SELECT a FROM main.m2;
SELECT * FROM k;
INSERT INTO temp.k(y, x) SELECT b, a+100 FROM m2 JOIN t ON t.id=m2.a ORDER BY 1;
SELECT * FROM k ORDER BY 1;
INSERT INTO k SELECT 1;
"#,
        ),
    ] {
        run_same(leak(format!("temp_across_databases-{tag}")), script);
    }
}

#[test]
fn pragma_tvf_temp() {
    for (tag, script) in [
        (
            "ptvf",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
CREATE TEMP TABLE t(x INT NOT NULL, y DEFAULT 5);
CREATE INDEX temp.ti ON t(x);
CREATE TEMP VIEW tv AS SELECT * FROM m;
SELECT * FROM pragma_table_info('t');
SELECT name FROM pragma_table_info('m');
SELECT * FROM pragma_index_list('t');
SELECT * FROM pragma_index_info('ti');
SELECT * FROM pragma_table_info('t', 'temp');
SELECT * FROM pragma_table_info('m', 'main');
SELECT * FROM pragma_table_info('t', 'main');
SELECT count(*) FROM pragma_table_xinfo('t');
"#,
        ),
        (
            "pragma_stmt",
            r#"
CREATE TABLE m(id INTEGER PRIMARY KEY, a, b TEXT);
CREATE TEMP TABLE t(x INT NOT NULL, y DEFAULT 5);
CREATE INDEX temp.ti ON t(x);
CREATE TEMP VIEW tv AS SELECT * FROM m;
PRAGMA table_info(t);
PRAGMA temp.table_info(t);
PRAGMA index_list(t);
"#,
        ),
    ] {
        run_same(leak(format!("pragma_tvf_temp-{tag}")), script);
    }
}
