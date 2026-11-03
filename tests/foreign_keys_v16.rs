//! v0.16 foreign-key enforcement (leaf feature `foreign_keys`),
//! differentially vs the pinned oracle (SQLite 3.54.1, live). Probed
//! on the pin before implementation; what the corpus encodes:
//!
//! - Violations are counted, not judged per row: a child row added
//!   with its parent missing counts one, removed takes one away
//!   (while the counter is non-zero); a parent row removed counts one
//!   per child still naming it, added takes one away per child it
//!   satisfies. An immediate key's count fails its statement when it
//!   ends ("FOREIGN KEY constraint failed", the statement undone even
//!   under OR FAIL / IGNORE / ROLLBACK); a deferred key's (DEFERRABLE
//!   INITIALLY DEFERRED, or any key under `PRAGMA defer_foreign_keys`)
//!   fails COMMIT and a committing RELEASE, the transaction staying
//!   open; ROLLBACK TO restores the count. So a multi-row statement
//!   may fix its own violations — and an orphan left by `foreign_keys
//!   = OFF` may cancel another's (the pin's quirk).
//! - A single-row INSERT of a top-level statement (no triggers,
//!   RETURNING, REPLACE or upsert update) fails at once and as a
//!   parent resolves nothing.
//! - Lookups give the child value the parent column's affinity and
//!   find it in the parent's PRIMARY KEY / UNIQUE index or rowid;
//!   child scans compare `parent = child` with both affinities and
//!   the parent's collation; action programs with `OLD.` (no
//!   affinity) — so a cascade can miss rows the count found.
//! - Actions: CASCADE, SET NULL, SET DEFAULT, RESTRICT (immediate even
//!   when deferred, skipped under defer_foreign_keys), NO ACTION; ON
//!   UPDATE only when the key changed; under ABORT; frames of their
//!   own for changes(); the trigger depth limit; child triggers fire.
//! - "foreign key mismatch" for a parent key that is no PRIMARY KEY /
//!   UNIQUE index, "no such table" for a missing parent — only for
//!   the keys a statement uses. DROP TABLE runs an implicit DELETE.
//!   ALTER TABLE RENAME rewrites REFERENCES. `foreign_key_check`.
//!
//! Every script runs statement by statement on both engines (one
//! statement per line, the pin's shell reading stdin, continuing past
//! errors): rows and the failing statements must agree, and the
//! oracle must read hipp's final file as its own.

#![cfg(feature = "foreign_keys")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// A thread with room for nested action programs.
fn big<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(f)
        .expect("thread")
        .join()
        .expect("test thread");
}

async fn open(path: &std::path::Path, cfg: Config) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), cfg)
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

/// hipp: every statement in order, rows printed (a statement's rows
/// are kept when it fails after them, as the shell prints them),
/// errors noted by 1-based statement number and skipped past.
fn hipp_run(
    path: &std::path::Path,
    stmts: &[String],
    cfg: Config,
) -> (Vec<String>, Vec<usize>, Vec<String>) {
    block_on(async {
        let mut db = open(path, cfg).await;
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

fn oracle_view(path: &std::path::Path) -> (String, String) {
    (
        common::cli(path, "PRAGMA integrity_check;"),
        common::cli(path, ".dump"),
    )
}

/// Run `stmts` on twin fresh files; rows, failing statements and
/// final files must agree. Returns hipp's error kinds.
fn same_cfg(tag: &str, stmts: &[String], cfg: Config) -> Vec<String> {
    let h = common::temp_db(&format!("v16fk-{tag}-h"));
    let o = common::temp_db(&format!("v16fk-{tag}-o"));
    let (hl, he, kinds) = hipp_run(&h, stmts, cfg);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{:3} {s}", i + 1))
        .collect::<Vec<_>>()
        .join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ ({kinds:?}):\n{script}");
    assert_eq!(
        he, oe,
        "{tag}: failing statements differ ({kinds:?}):\n{script}"
    );
    let (hi, hd) = oracle_view(&h);
    let (oi, od) = oracle_view(&o);
    assert_eq!(hi.trim(), "ok", "{tag}: hipp's file\n{script}");
    assert_eq!(oi.trim(), "ok");
    assert_eq!(hd, od, "{tag}: final files differ:\n{script}");
    kinds
}

/// `same_cfg` with the pin's trigger depth (1000 programs).
fn same(tag: &str, stmts: &[String]) -> Vec<String> {
    same_cfg(tag, stmts, Config::default().max_trigger_depth(1000))
}

fn lines(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

fn run_same(tag: &'static str, script: &'static str) {
    big(move || {
        same(tag, &lines(script));
    });
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

/// Immediate keys: the per-statement counter, multi-row statements
/// fixing their own violations, the orphan quirk, NULL keys, the
/// pin's lookup affinity (MustBeInt for a rowid parent).
#[test]
fn immediate_counting() {
    run_same(
        "count",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE, c)
        CREATE TABLE c(x REFERENCES p, y REFERENCES p(b))
        CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s)
        INSERT INTO p VALUES (1, 'one', 0), (2, 'two', 0)
        INSERT INTO c VALUES (1, 'one')
        INSERT INTO c VALUES (3, NULL)
        INSERT INTO c VALUES (NULL, NULL), (NULL, 'one')
        INSERT INTO c VALUES (1, 'nope')
        INSERT INTO c VALUES ('1', 'one'), (1.0, 'one'), ('1.0', 'one'), (' 1 ', 'one')
        INSERT INTO c VALUES (x'31', 'one')
        INSERT INTO c VALUES (1.5, 'one')
        SELECT x, typeof(x), y FROM c
        INSERT INTO s VALUES (1, 2), (2, NULL)
        INSERT INTO s VALUES (3, 4)
        INSERT INTO s VALUES (5, 5)
        PRAGMA foreign_keys = OFF
        INSERT INTO s VALUES (6, 100)
        PRAGMA foreign_keys = ON
        INSERT INTO s VALUES (7, 200), (100, NULL)
        INSERT INTO s VALUES (8, 300), (200, NULL)
        SELECT * FROM s ORDER BY id
        DELETE FROM s WHERE id = 2
        DELETE FROM s WHERE id = 1
        UPDATE s SET par = 999 WHERE id = 6
        UPDATE s SET id = 1000 WHERE id = 100
        UPDATE s SET par = par
        UPDATE p SET b = 'uno' WHERE a = 1
        UPDATE p SET c = 5
        DELETE FROM p WHERE a = 2
        INSERT OR IGNORE INTO c VALUES (9, NULL)
        INSERT OR REPLACE INTO c VALUES (9, NULL)
        INSERT OR FAIL INTO c VALUES (2, NULL), (9, NULL)
        INSERT OR ROLLBACK INTO c VALUES (9, NULL)
        SELECT count(*) FROM c
        SELECT * FROM s ORDER BY id
        PRAGMA foreign_key_check(s)
        PRAGMA foreign_key_check(c)",
    );
}

/// Deferred keys: COMMIT and a committing RELEASE fail with the
/// transaction open; ROLLBACK TO restores the counter; autocommit
/// statements check at their end.
#[test]
fn deferred_keys() {
    run_same(
        "deferred",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        CREATE TABLE c1(x REFERENCES p, y DEFERRABLE INITIALLY DEFERRED)
        CREATE TABLE c2(x REFERENCES p NOT DEFERRABLE INITIALLY DEFERRED)
        CREATE TABLE c3(x REFERENCES p DEFERRABLE INITIALLY IMMEDIATE)
        CREATE TABLE c4(x, FOREIGN KEY (x) REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        INSERT INTO c VALUES (1)
        SELECT count(*) FROM c
        BEGIN
        INSERT INTO c VALUES (1)
        INSERT INTO c1(x) VALUES (1)
        INSERT INTO c2 VALUES (1)
        INSERT INTO c3 VALUES (1)
        INSERT INTO c4 VALUES (1)
        SELECT 'in', (SELECT count(*) FROM c), (SELECT count(*) FROM c1), (SELECT count(*) FROM c4)
        COMMIT
        SELECT 'still open', count(*) FROM c
        INSERT INTO p VALUES (1)
        COMMIT
        SELECT 'after', count(*) FROM c
        BEGIN
        INSERT INTO c VALUES (2)
        ROLLBACK
        SAVEPOINT a
        INSERT INTO c VALUES (3)
        RELEASE a
        SELECT 'rel', count(*) FROM c
        ROLLBACK TO a
        RELEASE a
        BEGIN
        SAVEPOINT b
        INSERT INTO c VALUES (4)
        RELEASE b
        COMMIT
        DELETE FROM c WHERE x = 4
        COMMIT
        BEGIN
        INSERT INTO c VALUES (5)
        SAVEPOINT q
        DELETE FROM c WHERE x = 5
        ROLLBACK TO q
        COMMIT
        DELETE FROM c WHERE x = 5
        COMMIT
        BEGIN
        INSERT INTO c VALUES (6)
        DELETE FROM p WHERE a = 1
        INSERT INTO p VALUES (1), (6)
        COMMIT
        SELECT * FROM c",
    );
}

/// `PRAGMA defer_foreign_keys`: every key deferred, RESTRICT not
/// enforced, cleared when a transaction ends (a read of the file in
/// autocommit ends one).
#[test]
fn defer_pragma() {
    run_same(
        "defer",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p ON DELETE RESTRICT)
        CREATE TABLE d(x REFERENCES p)
        INSERT INTO p VALUES (1), (2)
        INSERT INTO c VALUES (1)
        INSERT INTO d VALUES (2)
        PRAGMA defer_foreign_keys
        BEGIN
        PRAGMA defer_foreign_keys = ON
        PRAGMA defer_foreign_keys
        INSERT INTO d VALUES (9)
        DELETE FROM p WHERE a = 1
        SELECT 'p', * FROM p
        COMMIT
        PRAGMA defer_foreign_keys
        INSERT INTO p VALUES (1), (9)
        COMMIT
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA defer_foreign_keys
        SELECT 1
        PRAGMA defer_foreign_keys
        INSERT INTO d VALUES (77)
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        SELECT count(*) FROM d
        PRAGMA defer_foreign_keys
        BEGIN
        PRAGMA defer_foreign_keys = ON
        ROLLBACK
        PRAGMA defer_foreign_keys
        BEGIN
        PRAGMA foreign_keys = OFF
        PRAGMA foreign_keys
        COMMIT
        SELECT * FROM p
        SELECT * FROM d",
    );
}

/// ON DELETE / ON UPDATE actions, the counts they leave, and
/// changes().
#[test]
fn actions() {
    run_same(
        "actions",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c1(x REFERENCES p ON DELETE CASCADE ON UPDATE CASCADE, n)
        CREATE TABLE c2(x REFERENCES p(b) ON DELETE SET NULL ON UPDATE SET NULL, n)
        CREATE TABLE c3(x DEFAULT 2 REFERENCES p ON DELETE SET DEFAULT ON UPDATE SET DEFAULT, n)
        CREATE TABLE c4(x REFERENCES p ON DELETE RESTRICT ON UPDATE RESTRICT, n)
        CREATE TABLE c5(x DEFAULT 99 REFERENCES p ON DELETE SET DEFAULT, n)
        INSERT INTO p VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')
        INSERT INTO c1 VALUES (1, 'c1-1'), (1, 'c1-1b'), (3, 'c1-3')
        INSERT INTO c2 VALUES ('a', 'c2-a'), ('c', 'c2-c')
        INSERT INTO c3 VALUES (1, 'c3-1'), (3, 'c3-3')
        INSERT INTO c4 VALUES (4, 'c4-4')
        INSERT INTO c5 VALUES (5, 'c5-5')
        DELETE FROM p WHERE a = 1
        SELECT changes(), total_changes()
        SELECT 'c1', * FROM c1
        SELECT 'c2', * FROM c2
        SELECT 'c3', * FROM c3
        UPDATE p SET a = 30, b = 'cc' WHERE a = 3
        SELECT changes(), total_changes()
        SELECT 'c1', * FROM c1
        SELECT 'c2', * FROM c2
        SELECT 'c3', * FROM c3
        UPDATE p SET a = 40 WHERE a = 4
        DELETE FROM p WHERE a = 4
        DELETE FROM p WHERE a = 2
        DELETE FROM p WHERE a = 5
        UPDATE p SET a = a WHERE a = 4
        UPDATE p SET b = 'd' WHERE a = 4
        UPDATE p SET a = 4, b = 'd' WHERE a = 4
        SELECT * FROM p
        SELECT 'c5', * FROM c5",
    );
}

/// The three comparisons: the lookup (parent affinity + index), the
/// count's scan (both affinities, parent collation) and an action's
/// `OLD.` (no affinity) — composite keys in any order, NOCASE.
#[test]
fn affinity_collation_composite() {
    run_same(
        "affinity",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a TEXT COLLATE NOCASE, b INTEGER, c, PRIMARY KEY(a, b))
        CREATE TABLE c(x, y INT, z TEXT, FOREIGN KEY (y, x) REFERENCES p(b, a) ON UPDATE CASCADE ON DELETE SET NULL)
        INSERT INTO p VALUES ('Abc', 1, 'r1'), ('x', '2', 'r2'), (3, 3, 'r3')
        INSERT INTO c VALUES ('abc', 1, 'ok-nocase')
        INSERT INTO c VALUES ('ABC', '1', 'ok-text-int')
        INSERT INTO c VALUES ('x', 2.0, 'ok-real')
        INSERT INTO c VALUES (3, 3, 'ok-int-text')
        INSERT INTO c VALUES ('3', 3, 'ok-text')
        INSERT INTO c VALUES ('3.0', 3, 'bad')
        INSERT INTO c VALUES ('Abc', NULL, 'null')
        INSERT INTO c VALUES ('nope', 1, 'bad')
        SELECT * FROM c
        UPDATE p SET a = 'ABC' WHERE b = 1
        SELECT * FROM c
        UPDATE p SET a = 'new' WHERE b = 1
        SELECT * FROM c
        DELETE FROM p WHERE b = 3
        SELECT * FROM c
        PRAGMA foreign_key_check(c)
        CREATE TABLE q(a REAL UNIQUE)
        CREATE TABLE qc(x TEXT REFERENCES q(a) ON DELETE CASCADE)
        INSERT INTO q VALUES (1), (2.5)
        INSERT INTO qc VALUES ('1'), ('1.0'), (' 1'), ('2.5'), ('2.50')
        SELECT x, typeof(x) FROM qc
        DELETE FROM q WHERE a = 1
        DELETE FROM q
        SELECT x FROM qc
        CREATE TABLE r(k TEXT PRIMARY KEY)
        CREATE TABLE rc(x INTEGER REFERENCES r ON DELETE CASCADE ON UPDATE CASCADE)
        INSERT INTO r VALUES ('1'), ('01'), ('a')
        INSERT INTO rc VALUES (1), ('01'), (1.0)
        SELECT x, typeof(x) FROM rc
        UPDATE r SET k = '7' WHERE k = '1'
        SELECT x, typeof(x) FROM rc
        DELETE FROM r WHERE k = '7'
        SELECT x, typeof(x) FROM rc",
    );
}

/// Self-referencing keys: a row satisfying itself, cascades through
/// the table, UPDATE seeing the old row gone.
#[test]
fn self_reference() {
    run_same(
        "selfref",
        "PRAGMA foreign_keys = ON
        CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s ON UPDATE CASCADE ON DELETE CASCADE)
        INSERT INTO s VALUES (1, NULL), (2, 1), (3, 2), (4, 4)
        UPDATE s SET id = 10 WHERE id = 1
        SELECT * FROM s
        UPDATE s SET id = id + 100
        SELECT * FROM s
        DELETE FROM s WHERE id = 110
        SELECT * FROM s
        CREATE TABLE s2(id INTEGER PRIMARY KEY, par REFERENCES s2)
        INSERT INTO s2 VALUES (1, 1)
        UPDATE s2 SET id = 5, par = 1 WHERE id = 1
        UPDATE s2 SET id = 5, par = 5 WHERE id = 1
        SELECT * FROM s2
        CREATE TABLE s3(a, b, k UNIQUE, FOREIGN KEY (a, b) REFERENCES s3(k, k))
        CREATE TABLE s4(a UNIQUE, b REFERENCES s4(a) ON DELETE SET NULL)
        INSERT INTO s4 VALUES (1, 1), (2, 1), (3, 2)
        DELETE FROM s4 WHERE a = 1
        SELECT * FROM s4
        INSERT INTO s4 VALUES (9, 9)
        INSERT INTO s4 VALUES (8, 7)
        UPDATE s4 SET a = 20 WHERE a = 2
        SELECT * FROM s4",
    );
}

/// "foreign key mismatch" and "no such table": only for the keys a
/// statement uses (a single-row INSERT into the parent uses none).
#[test]
fn mismatch_and_missing_parent() {
    run_same(
        "mismatch",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a, b, c INTEGER PRIMARY KEY)
        CREATE TABLE c1(x REFERENCES p(a))
        CREATE TABLE c2(x REFERENCES p)
        CREATE TABLE c3(x, y, FOREIGN KEY (x, y) REFERENCES p)
        CREATE TABLE c4(x REFERENCES nosuch)
        CREATE TABLE c5(x REFERENCES p(rowid))
        INSERT INTO c1 VALUES (NULL)
        INSERT INTO c1 SELECT 1 WHERE 0
        INSERT INTO c2 VALUES (NULL)
        INSERT INTO c3 VALUES (NULL, NULL)
        INSERT INTO c4 VALUES (NULL)
        INSERT INTO c5 VALUES (NULL)
        INSERT INTO p VALUES (1, 2, 3)
        INSERT INTO p VALUES (1, 2, 4), (1, 2, 5)
        DELETE FROM p WHERE 0
        UPDATE p SET c = c WHERE 0
        UPDATE p SET b = b WHERE 0
        UPDATE p SET a = a WHERE 0
        DELETE FROM c1
        UPDATE c1 SET x = x
        DELETE FROM c4
        PRAGMA foreign_key_check(c1)
        PRAGMA foreign_key_check(c4)
        PRAGMA foreign_key_check(c2)
        PRAGMA foreign_key_check(nosuch)
        PRAGMA foreign_keys = OFF
        INSERT INTO c1 VALUES (5)
        INSERT INTO c4 VALUES (5)
        PRAGMA foreign_keys = ON
        PRAGMA foreign_key_check(c4)
        CREATE VIEW v AS SELECT 1 AS k
        CREATE TABLE c6(x REFERENCES v)
        INSERT INTO c6 VALUES (1)
        CREATE TABLE q(k COLLATE NOCASE UNIQUE, j UNIQUE)
        CREATE TABLE qc(x REFERENCES q(k), y REFERENCES q(j))
        INSERT INTO qc VALUES ('A', NULL)
        CREATE TABLE q2(k)
        CREATE UNIQUE INDEX q2k ON q2(k COLLATE NOCASE)
        CREATE TABLE q2c(x REFERENCES q2(k))
        INSERT INTO q2c VALUES (NULL)",
    );
}

/// DROP TABLE: the implicit DELETE (cascades run, the dropped
/// table's triggers do not), immediate violations fail the drop,
/// deferred ones the commit; a dropped parent is "no such table".
#[test]
fn drop_table() {
    run_same(
        "drop",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p)
        CREATE TABLE cc(x REFERENCES p ON DELETE CASCADE)
        INSERT INTO p VALUES (1), (2), (3)
        INSERT INTO c VALUES (1)
        INSERT INTO cc VALUES (2)
        DROP TABLE p
        SELECT * FROM p
        DELETE FROM c
        SELECT total_changes()
        DROP TABLE p
        SELECT changes(), total_changes()
        SELECT 'cc', * FROM cc
        SELECT name FROM sqlite_schema
        INSERT INTO c VALUES (NULL)
        INSERT INTO c VALUES (1)
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        INSERT INTO c VALUES (1)
        CREATE TABLE d(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        BEGIN
        INSERT INTO d VALUES (5)
        DROP TABLE d
        COMMIT
        SELECT name FROM sqlite_schema
        CREATE TABLE e(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        INSERT INTO p VALUES (7)
        INSERT INTO e VALUES (7)
        BEGIN
        DROP TABLE p
        SELECT name FROM sqlite_schema
        COMMIT
        ROLLBACK
        SELECT name FROM sqlite_schema
        DROP TABLE p
        DROP TABLE e
        DROP TABLE p
        SELECT name FROM sqlite_schema
        CREATE TABLE f(a INTEGER PRIMARY KEY)
        CREATE TABLE fc(x REFERENCES f)
        INSERT INTO f VALUES (1)
        INSERT INTO fc VALUES (1)
        DROP TABLE fc
        DROP TABLE f",
    );
}

/// ALTER TABLE RENAME rewrites REFERENCES in child tables; ADD
/// COLUMN refuses a REFERENCES column with a non-NULL default while
/// the pragma is on.
#[test]
fn alter_table() {
    run_same(
        "alter",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c(x REFERENCES p, y REFERENCES \"p\"(b), FOREIGN KEY (x) REFERENCES p (a))
        INSERT INTO p VALUES (1, 'one')
        INSERT INTO c VALUES (1, 'one')
        ALTER TABLE p RENAME TO q
        SELECT sql FROM sqlite_schema
        INSERT INTO c VALUES (2, 'one')
        INSERT INTO q VALUES (2, 'two')
        INSERT INTO c VALUES (2, 'two')
        ALTER TABLE q RENAME COLUMN b TO bb
        INSERT INTO c VALUES (2, 'zz')
        SELECT sql FROM sqlite_schema
        PRAGMA foreign_keys = OFF
        ALTER TABLE q RENAME TO r
        PRAGMA foreign_keys = ON
        SELECT sql FROM sqlite_schema
        DELETE FROM r WHERE a = 1
        CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s)
        ALTER TABLE s RENAME TO s2
        INSERT INTO s2 VALUES (1, 2)
        SELECT sql FROM sqlite_schema WHERE name = 's2'
        CREATE TABLE t(x)
        INSERT INTO t VALUES (1)
        ALTER TABLE t ADD COLUMN y REFERENCES r DEFAULT 5
        ALTER TABLE t ADD COLUMN y REFERENCES r DEFAULT NULL
        ALTER TABLE t ADD COLUMN z REFERENCES r
        INSERT INTO t VALUES (1, 2, 3)
        PRAGMA foreign_keys = OFF
        ALTER TABLE t ADD COLUMN w REFERENCES r DEFAULT 5
        PRAGMA foreign_keys = ON
        SELECT * FROM t
        PRAGMA foreign_key_check(t)
        UPDATE t SET x = 2
        UPDATE t SET w = 1",
    );
}

/// REPLACE and upsert: a replaced parent row's actions run, its
/// children count, the new row resolves them; DO UPDATE of a key.
#[test]
fn replace_and_upsert() {
    run_same(
        "replace",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c(x REFERENCES p ON DELETE CASCADE, n)
        CREATE TABLE d(x REFERENCES p, n)
        INSERT INTO p VALUES (1, 'a'), (2, 'b')
        INSERT INTO c VALUES (1, 'c1')
        INSERT INTO d VALUES (2, 'd2')
        REPLACE INTO p VALUES (1, 'z')
        SELECT 'c', * FROM c
        REPLACE INTO p VALUES (2, 'y')
        REPLACE INTO p VALUES (3, 'b')
        INSERT INTO c VALUES (1, 'c1')
        INSERT INTO p VALUES (1, 'q') ON CONFLICT(a) DO UPDATE SET b = 'w'
        SELECT 'c', * FROM c
        INSERT INTO p VALUES (1, 'q') ON CONFLICT(a) DO UPDATE SET a = 10
        INSERT INTO p VALUES (2, 'q') ON CONFLICT(a) DO UPDATE SET a = 20
        INSERT INTO p VALUES (3, 'q') ON CONFLICT(a) DO UPDATE SET a = 30
        INSERT INTO p VALUES (9, 'q') ON CONFLICT DO NOTHING
        SELECT 'p', * FROM p
        UPDATE OR REPLACE p SET b = 'w' WHERE a = 2
        SELECT 'p', * FROM p
        SELECT 'c', * FROM c
        INSERT INTO c VALUES (2, 'c2')
        UPDATE OR REPLACE p SET a = 2 WHERE b = 'w'
        UPDATE OR REPLACE p SET a = 2 WHERE a = 30
        SELECT 'p', * FROM p
        SELECT 'c', * FROM c
        SELECT 'd', * FROM d",
    );
}

/// OR FAIL keeps nothing when the statement left violations; OR
/// IGNORE skips a conflicting row before its keys count; RETURNING
/// rows are withheld for immediate violations and served before a
/// deferred one fails an autocommit statement.
#[test]
fn conflict_clauses_and_returning() {
    run_same(
        "conflict",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p, u UNIQUE)
        INSERT INTO p VALUES (1)
        INSERT INTO c VALUES (1, 'dup')
        INSERT OR FAIL INTO c VALUES (9, 'a'), (1, 'dup')
        SELECT 'after fail', count(*) FROM c
        INSERT OR FAIL INTO c VALUES (1, 'b'), (1, 'dup')
        SELECT 'after fail2', count(*) FROM c
        INSERT OR IGNORE INTO c VALUES (9, 'dup')
        INSERT OR IGNORE INTO c VALUES (9, 'zz'), (1, 'dup')
        SELECT 'ign', count(*) FROM c
        CREATE TABLE cd(x REFERENCES p DEFERRABLE INITIALLY DEFERRED, u UNIQUE)
        INSERT INTO cd VALUES (1, 'dup')
        INSERT OR FAIL INTO cd VALUES (9, 'a'), (1, 'dup')
        SELECT 'cd', count(*) FROM cd
        BEGIN
        INSERT OR FAIL INTO cd VALUES (9, 'a'), (1, 'dup')
        SELECT 'cd in txn', count(*) FROM cd
        ROLLBACK
        INSERT INTO c VALUES (3, 'r') RETURNING *
        INSERT INTO c VALUES (1, 'r1'), (3, 'r2') RETURNING *
        INSERT INTO cd VALUES (1, 'r3'), (5, 'r4') RETURNING x, u
        SELECT count(*) FROM cd
        UPDATE c SET x = 7 RETURNING *
        DELETE FROM p RETURNING *
        PRAGMA defer_foreign_keys = ON
        UPDATE c SET x = 8 RETURNING *
        SELECT * FROM c",
    );
}

/// `PRAGMA foreign_key_check`: rows in rowid order, keys in list
/// order (fkid), the parent as written, a missing parent table.
#[test]
fn foreign_key_check() {
    run_same(
        "check",
        "CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c(x REFERENCES P, y REFERENCES p(b), z REFERENCES gone)
        INSERT INTO p VALUES (1, 'one')
        INSERT INTO c VALUES (1, 'one', NULL), (2, 'one', 3), (1, 'two', 4), (NULL, 'zz', NULL)
        PRAGMA foreign_key_check
        PRAGMA foreign_key_check(c)
        PRAGMA foreign_key_check(p)
        PRAGMA main.foreign_key_check(c)
        SELECT 'fk on'
        PRAGMA foreign_keys = ON
        PRAGMA foreign_key_check(c)
        DELETE FROM c WHERE rowid = 2",
    );
}

/// Cascades nest as programs: the trigger depth limit, counted
/// before the WHEN clause.
#[test]
fn cascade_depth() {
    run_same(
        "depth",
        "PRAGMA foreign_keys = ON
        CREATE TABLE t(id INTEGER PRIMARY KEY, par REFERENCES t ON DELETE CASCADE)
        WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 1100) INSERT INTO t SELECT i, CASE WHEN i > 1 THEN i - 1 END FROM r
        SELECT count(*) FROM t
        DELETE FROM t WHERE id = 1095
        SELECT count(*) FROM t
        DELETE FROM t WHERE id = 101
        SELECT count(*) FROM t
        DELETE FROM t WHERE id = 100
        SELECT count(*) FROM t
        DELETE FROM t WHERE id = 50
        SELECT count(*) FROM t",
    );
}

/// The MATCH clause parses and is ignored (`foreign_key_list` says
/// NONE); ON INSERT actions parse.
#[test]
fn match_clauses() {
    run_same(
        "match",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a PRIMARY KEY, b)
        CREATE TABLE m(x REFERENCES p MATCH FULL, y REFERENCES p(a) MATCH SIMPLE ON DELETE CASCADE MATCH PARTIAL)
        PRAGMA foreign_key_list(m)
        INSERT INTO p VALUES (1, 2)
        INSERT INTO m VALUES (1, 1)
        INSERT INTO m VALUES (1, 2)
        DELETE FROM p
        SELECT * FROM m",
    );
}

/// Bugs found while testing (each fixed; see STATUS.md).
#[test]
fn regressions() {
    // An UPDATE OR REPLACE's victim ran its actions only for the keys
    // the UPDATE assigns.
    run_same(
        "reg-victim",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c(x REFERENCES p ON DELETE CASCADE)
        INSERT INTO p VALUES (1, 'a'), (2, 'b')
        INSERT INTO c VALUES (1), (2)
        UPDATE OR REPLACE p SET b = 'a' WHERE a = 2
        SELECT * FROM c",
    );
    // With defer_foreign_keys in autocommit, RETURNING rows come
    // before the failure (hipp withheld them).
    run_same(
        "reg-late",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p)
        PRAGMA defer_foreign_keys = ON
        INSERT INTO c VALUES (2), (3) RETURNING *
        SELECT count(*) FROM c",
    );
}

/// More bugs the generated corpus found (each fixed).
#[test]
fn regressions_corpus() {
    // The pin's `isSetNullAction` quirk: a REPLACE that compiles the
    // deletion programs of a self-referencing ON DELETE SET NULL key
    // skips that key's lookups for its new rows — unless compiling
    // that program compiles others after it (UPDATE triggers).
    run_same(
        "reg-setnull",
        "PRAGMA foreign_keys = ON
        PRAGMA recursive_triggers = ON
        CREATE TABLE lg(m)
        CREATE TABLE a(id INTEGER PRIMARY KEY, par REFERENCES a ON DELETE SET NULL)
        INSERT OR REPLACE INTO a VALUES (1, 99)
        INSERT OR REPLACE INTO a(par) VALUES (99)
        INSERT INTO a VALUES (2, 98)
        CREATE TABLE u(id INTEGER PRIMARY KEY, par REFERENCES u ON DELETE SET NULL, k UNIQUE)
        INSERT INTO u VALUES (1, NULL, 1)
        UPDATE OR REPLACE u SET par = 99 WHERE id = 1
        UPDATE u SET par = 98 WHERE id = 1
        INSERT OR REPLACE INTO u(par, k) VALUES (77, 3)
        CREATE TABLE i(id INTEGER PRIMARY KEY, par REFERENCES i ON DELETE SET NULL, q REFERENCES i ON DELETE SET NULL)
        INSERT OR REPLACE INTO i VALUES (1, 99, NULL)
        INSERT OR REPLACE INTO i VALUES (2, NULL, 99)
        CREATE TABLE c3(id INTEGER PRIMARY KEY, par REFERENCES c3 ON DELETE CASCADE)
        INSERT OR REPLACE INTO c3 VALUES (1, 99)
        SELECT * FROM a
        SELECT * FROM u
        SELECT * FROM i",
    );
    #[cfg(feature = "triggers")]
    run_same(
        "reg-setnull-trigger",
        "PRAGMA foreign_keys = ON
        CREATE TABLE t2(id INTEGER PRIMARY KEY, par REFERENCES t2 ON DELETE SET NULL)
        CREATE TRIGGER x AFTER UPDATE ON t2 BEGIN SELECT 1; END
        INSERT OR REPLACE INTO t2 VALUES (5, 4)
        CREATE TABLE t3(id INTEGER PRIMARY KEY, par REFERENCES t3 ON DELETE SET NULL)
        CREATE TRIGGER y AFTER UPDATE OF id ON t3 BEGIN SELECT 1; END
        INSERT OR REPLACE INTO t3 VALUES (5, 4)
        SELECT * FROM t2
        SELECT * FROM t3",
    );
    // The keys an action program would check resolve when the firing
    // statement is prepared (a mismatch two cascades away fails it);
    // an UPDATE changing any key locates every key on its table;
    // DROP TABLE's DELETE ignores its own unusable keys.
    run_same(
        "reg-tree",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE)
        CREATE TABLE c(x REFERENCES p ON DELETE CASCADE, n UNIQUE)
        CREATE TABLE g(k REFERENCES c(n) ON DELETE CASCADE, j REFERENCES c)
        CREATE TABLE h(m REFERENCES p(b))
        DELETE FROM p WHERE 0
        DELETE FROM c WHERE 0
        UPDATE p SET b = b WHERE 0
        UPDATE p SET a = a WHERE 0
        INSERT INTO p VALUES (1, 'x')
        INSERT INTO p VALUES (2, 'y'), (3, 'z')
        DROP TABLE c
        SELECT name FROM sqlite_schema",
    );
    // changes() after DROP TABLE: the implicit DELETE's count when
    // the pin compiles it (0 when skipped), else unchanged; a failed
    // statement's prepare-time key error leaves changes() alone.
    run_same(
        "reg-changes",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        CREATE TABLE o(z)
        INSERT INTO o VALUES (1), (2)
        DROP TABLE c
        SELECT changes()
        INSERT INTO o VALUES (3), (4)
        CREATE TABLE d(x REFERENCES gone)
        INSERT INTO d VALUES (1)
        SELECT changes()
        DROP TABLE o
        SELECT changes()",
    );
}

/// Without the pragma nothing is checked.
#[test]
fn pragma_off() {
    run_same(
        "off",
        "CREATE TABLE p(a INTEGER PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p ON DELETE CASCADE)
        INSERT INTO c VALUES (5)
        INSERT INTO p VALUES (1)
        INSERT INTO c VALUES (1)
        DELETE FROM p
        SELECT * FROM c
        DROP TABLE p
        SELECT * FROM c",
    );
}

/// With `triggers`: cascades fire the child's triggers in the pin's
/// order (BEFORE, check, delete, actions, AFTER), each a frame for
/// changes(); an action's statements run under ABORT (a body's OR
/// IGNORE included); RAISE inside a cascade.
#[cfg(feature = "triggers")]
#[test]
fn actions_fire_triggers() {
    run_same(
        "triggers",
        "PRAGMA foreign_keys = ON
        CREATE TABLE log(m)
        CREATE TABLE p(a INTEGER PRIMARY KEY, b)
        CREATE TABLE c(x REFERENCES p ON DELETE CASCADE ON UPDATE CASCADE, n)
        CREATE TABLE g(y REFERENCES c(n) ON DELETE SET NULL)
        CREATE UNIQUE INDEX cn ON c(n)
        CREATE TRIGGER pbd BEFORE DELETE ON p BEGIN INSERT INTO log VALUES ('p before del ' || old.a || ' ' || changes()); END
        CREATE TRIGGER pad AFTER DELETE ON p BEGIN INSERT INTO log VALUES ('p after del ' || old.a || ' ' || changes()); END
        CREATE TRIGGER cbd BEFORE DELETE ON c BEGIN INSERT INTO log VALUES ('c before del ' || old.n || ' ' || changes()); END
        CREATE TRIGGER cad AFTER DELETE ON c BEGIN INSERT INTO log VALUES ('c after del ' || old.n || ' ' || changes()); END
        CREATE TRIGGER cbu BEFORE UPDATE ON c BEGIN INSERT INTO log VALUES ('c before upd ' || old.x || '->' || new.x); END
        CREATE TRIGGER cau AFTER UPDATE ON c BEGIN INSERT INTO log VALUES ('c after upd ' || old.x || '->' || new.x); END
        CREATE TRIGGER gu AFTER UPDATE ON g BEGIN INSERT INTO log VALUES ('g upd ' || quote(old.y) || '->' || quote(new.y)); END
        INSERT INTO p VALUES (1, 'x'), (2, 'y')
        INSERT INTO c VALUES (1, 'n1'), (1, 'n2'), (2, 'n3')
        INSERT INTO g VALUES ('n1'), ('n2'), ('n3')
        DELETE FROM p WHERE a = 1
        SELECT changes()
        SELECT * FROM log
        DELETE FROM log
        UPDATE p SET a = 20 WHERE a = 2
        SELECT * FROM log
        SELECT * FROM c
        SELECT * FROM g
        CREATE TABLE pp(a INTEGER PRIMARY KEY)
        CREATE TABLE ch2(x REFERENCES pp ON UPDATE CASCADE)
        CREATE TABLE lg(m UNIQUE)
        CREATE TRIGGER t2 AFTER UPDATE ON ch2 BEGIN INSERT OR IGNORE INTO lg VALUES ('k'); END
        INSERT INTO lg VALUES ('k')
        INSERT INTO pp VALUES (1), (2)
        INSERT INTO ch2 VALUES (1)
        UPDATE OR IGNORE pp SET a = 5 WHERE a = 1
        UPDATE pp SET a = 5 WHERE a = 1
        SELECT * FROM pp
        SELECT * FROM ch2
        CREATE TABLE rp(a INTEGER PRIMARY KEY)
        CREATE TABLE rc(x REFERENCES rp ON DELETE CASCADE)
        CREATE TRIGGER rb BEFORE DELETE ON rc WHEN old.x = 2 BEGIN SELECT RAISE(ABORT, 'no'); END
        CREATE TRIGGER rf BEFORE DELETE ON rc WHEN old.x = 3 BEGIN SELECT RAISE(FAIL, 'f'); END
        CREATE TRIGGER ri BEFORE DELETE ON rc WHEN old.x = 4 BEGIN SELECT RAISE(IGNORE); END
        INSERT INTO rp VALUES (1), (2), (3), (4)
        INSERT INTO rc VALUES (1), (2), (3), (4)
        DELETE FROM rp WHERE a = 2
        DELETE FROM rp WHERE a IN (1, 3)
        SELECT * FROM rp
        DELETE FROM rp WHERE a = 4
        SELECT * FROM rp
        SELECT * FROM rc",
    );
}

/// A pin-made file with foreign keys: hipp enforces them; the pin
/// enforces hipp's.
#[test]
fn files_interoperate() {
    big(|| {
        let p = common::temp_db("v16fk-files");
        common::cli(
            &p,
            "CREATE TABLE p(a INTEGER PRIMARY KEY, b TEXT UNIQUE COLLATE NOCASE);
             CREATE TABLE c(x REFERENCES p ON DELETE CASCADE, y REFERENCES p(b) ON UPDATE CASCADE);
             INSERT INTO p VALUES (1, 'a'), (2, 'b');
             INSERT INTO c VALUES (1, 'A'), (2, 'b');",
        );
        let stmts = lines(
            "PRAGMA foreign_keys = ON
            INSERT INTO c VALUES (3, NULL)
            INSERT INTO c VALUES (NULL, 'B')
            UPDATE p SET b = 'bee' WHERE a = 2
            DELETE FROM p WHERE a = 1
            SELECT * FROM c",
        );
        let (rows, errs, _) = hipp_run(&p, &stmts, Config::default());
        assert_eq!(errs, vec![2]);
        assert_eq!(rows, vec!["2|bee", "|bee"]);
        let o = common::cli(
            &p,
            "PRAGMA foreign_keys = ON; DELETE FROM c WHERE x IS NULL; DELETE FROM p WHERE a = 2; SELECT count(*) FROM c;",
        );
        assert_eq!(o, "0\n");
        assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
    });
}

/// Review fixes (each a bug found in review of the first cut).
#[test]
fn review_fixes() {
    // A single-row INSERT that both repeats a rowid and violates an
    // immediate key fails on the rowid, as on the pin: its OR
    // ROLLBACK then ends the transaction (hipp failed on the key and
    // kept the transaction open).
    big(|| {
        for verb in [
            "INSERT",
            "INSERT OR ABORT",
            "INSERT OR ROLLBACK",
            "INSERT OR FAIL",
        ] {
            let kinds = same(
                "rev-rowid",
                &lines(&format!(
                    "PRAGMA foreign_keys = ON
                    CREATE TABLE p(a PRIMARY KEY)
                    CREATE TABLE c(id INTEGER PRIMARY KEY, x REFERENCES p, u UNIQUE)
                    INSERT INTO p VALUES (1)
                    INSERT INTO c VALUES (1, 1, 1)
                    BEGIN
                    INSERT INTO p VALUES (2)
                    {verb} INTO c VALUES (1, 5, 7)
                    INSERT INTO p VALUES (3)
                    COMMIT
                    BEGIN
                    INSERT INTO p VALUES (4)
                    {verb} INTO c VALUES (2, 5, 1)
                    COMMIT
                    {verb} INTO c VALUES (3, 5, 9)
                    SELECT * FROM p
                    SELECT * FROM c"
                )),
            );
            assert!(
                kinds
                    .iter()
                    .any(|k| k.starts_with("8: ") && k.contains("DuplicateRowid")),
                "{verb}: {kinds:?}"
            );
            assert!(
                kinds
                    .iter()
                    .any(|k| k.starts_with("15: ") && k.contains("ForeignKeyViolation")),
                "{verb}: {kinds:?}"
            );
        }
    });
    // ADD COLUMN of a REFERENCES column with a non-NULL default is
    // refused only when the table has rows (the pin's
    // `sqlite3ErrorIfNotEmpty`).
    run_same(
        "rev-add-column",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p)
        ALTER TABLE c ADD COLUMN z REFERENCES p DEFAULT 1
        SELECT sql FROM sqlite_schema WHERE name = 'c'
        INSERT INTO c(x) VALUES (NULL)
        INSERT INTO p VALUES (1)
        INSERT INTO c(x) VALUES (NULL)
        ALTER TABLE c ADD COLUMN w REFERENCES p DEFAULT 1
        ALTER TABLE c ADD COLUMN w REFERENCES p DEFAULT (1 + 1)
        ALTER TABLE c ADD COLUMN w NOT NULL REFERENCES p DEFAULT 2
        ALTER TABLE c ADD COLUMN w REFERENCES p
        DELETE FROM c
        ALTER TABLE c ADD COLUMN v REFERENCES p DEFAULT 2
        ALTER TABLE c ADD COLUMN u REFERENCES p DEFAULT (1 + 1)
        SELECT sql FROM sqlite_schema WHERE name = 'c'
        INSERT INTO c(x) VALUES (1)
        SELECT * FROM c",
    );
    // A counter below zero fails as one above it: a parent row that
    // matches more children than were counted — orphans left while
    // the pragma was off — fails the commit (and the statement, for
    // an immediate key), as on the pin (found by the corpus).
    run_same(
        "rev-negative",
        "CREATE TABLE p(a UNIQUE, b)
        CREATE TABLE c0(x REFERENCES p(a) DEFERRABLE INITIALLY DEFERRED, n)
        CREATE TABLE c1(x REFERENCES p(a), n)
        INSERT INTO c0 VALUES (1, 0), (1, 0)
        INSERT INTO c1 VALUES (1, 0), (1, 0)
        PRAGMA foreign_keys = ON
        BEGIN
        INSERT INTO c0 VALUES (5, 0)
        INSERT INTO p VALUES (1, 0)
        SAVEPOINT s
        RELEASE s
        COMMIT
        ROLLBACK
        SAVEPOINT a
        INSERT INTO c0 VALUES (5, 0)
        INSERT INTO p VALUES (1, 0)
        RELEASE a
        ROLLBACK
        BEGIN
        INSERT INTO c0 VALUES (1, 0)
        INSERT INTO p VALUES (1, 0)
        COMMIT
        ROLLBACK
        BEGIN
        INSERT INTO c0 VALUES (5, 0)
        INSERT INTO c0 VALUES (1, 0)
        INSERT INTO p VALUES (1, 0)
        COMMIT
        INSERT INTO c1 VALUES (5, 0), (6, 0)
        INSERT INTO p VALUES (9, 0), (1, 0)
        SELECT * FROM p
        SELECT * FROM c0",
    );
    #[cfg(feature = "triggers")]
    run_same(
        "rev-negative-stmt",
        "CREATE TABLE p(a UNIQUE, b)
        CREATE TABLE c0(x REFERENCES p(a), n)
        CREATE TABLE d(z)
        CREATE TRIGGER t AFTER INSERT ON d BEGIN INSERT INTO c0 VALUES (5, 0); INSERT INTO p VALUES (new.z, 0); END
        INSERT INTO c0 VALUES (1, 0), (1, 0), (2, 0)
        PRAGMA foreign_keys = ON
        INSERT INTO d VALUES (1)
        INSERT INTO d VALUES (2)
        SELECT * FROM c0
        SELECT * FROM p",
    );
    // An UPDATE changing no key checks none, not even a
    // self-referencing one; pragmas that read the file end the
    // autocommit transaction and so clear `defer_foreign_keys`
    // (found by the corpus).
    run_same(
        "rev-update-nokey",
        "PRAGMA foreign_keys = ON
        CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s ON DELETE SET NULL, v)
        REPLACE INTO s VALUES (1, 2, 3)
        UPDATE OR IGNORE s SET v = 3 WHERE rowid % 2 = 1
        UPDATE s SET v = 4
        UPDATE s SET par = 2
        UPDATE s SET par = 5
        SELECT * FROM s",
    );
    run_same(
        "rev-defer-pragmas",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER PRIMARY KEY, b)
        CREATE TABLE c1(id INTEGER PRIMARY KEY, x REFERENCES p(a), y)
        PRAGMA defer_foreign_keys = ON
        PRAGMA integrity_check
        PRAGMA defer_foreign_keys
        BEGIN
        INSERT OR IGNORE INTO c1 VALUES (1, 1, 2)
        COMMIT
        PRAGMA defer_foreign_keys = ON
        PRAGMA foreign_key_check(p)
        PRAGMA foreign_key_list(p)
        PRAGMA defer_foreign_keys
        PRAGMA foreign_key_check(c1)
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA foreign_key_list(c1)
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA encoding
        PRAGMA synchronous
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA table_info(c1)
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA user_version
        PRAGMA defer_foreign_keys
        PRAGMA defer_foreign_keys = ON
        PRAGMA page_size
        PRAGMA recursive_triggers
        PRAGMA case_sensitive_like = 0
        PRAGMA defer_foreign_keys
        INSERT OR REPLACE INTO c1 VALUES (1, 9, 1) RETURNING *
        SELECT * FROM c1",
    );
    // An upsert's DO UPDATE makes the INSERT a multi-row write only
    // when it fires UPDATE triggers or changes a key: a mismatched
    // key it leaves alone fails nothing (found by the corpus).
    run_same(
        "rev-upsert-multi",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a INTEGER UNIQUE, b, c)
        CREATE TABLE g(j REFERENCES p, w)
        INSERT INTO p VALUES (1, 1, 1)
        INSERT INTO p VALUES (2, 1, 1) ON CONFLICT DO UPDATE SET b = 5
        INSERT INTO p VALUES (1, 1, 1) ON CONFLICT DO UPDATE SET a = 5
        INSERT INTO p VALUES (2, 1, 1) ON CONFLICT DO UPDATE SET a = excluded.a + 10
        INSERT INTO p VALUES (3, 1, 1) ON CONFLICT (a) DO UPDATE SET c = 5
        INSERT INTO p VALUES (4, 1, 1) ON CONFLICT DO NOTHING
        INSERT INTO p VALUES (5, 1, 1), (6, 1, 1)
        INSERT INTO p VALUES (7, 1, 1) RETURNING a
        INSERT OR REPLACE INTO p VALUES (8, 1, 1)
        SELECT * FROM p
        CREATE TABLE q(a PRIMARY KEY, b)
        CREATE TABLE c(id INTEGER PRIMARY KEY, x REFERENCES q, n UNIQUE)
        CREATE TABLE cd(id INTEGER PRIMARY KEY, x REFERENCES q DEFERRABLE INITIALLY DEFERRED)
        INSERT INTO q VALUES (1, 0)
        INSERT INTO c VALUES (1, 1, 1)
        INSERT INTO c VALUES (2, 9, 2) ON CONFLICT DO UPDATE SET n = 5
        INSERT INTO c VALUES (1, 9, 2) ON CONFLICT (id) DO UPDATE SET n = 5
        INSERT INTO c VALUES (1, 9, 2) ON CONFLICT (id) DO UPDATE SET x = 7
        INSERT INTO c VALUES (1, 9, 2) ON CONFLICT (id) DO UPDATE SET x = 1
        BEGIN
        INSERT INTO cd VALUES (1, 5)
        INSERT INTO q VALUES (5, 0) ON CONFLICT DO UPDATE SET b = 1
        COMMIT
        BEGIN
        INSERT INTO c VALUES (3, 8, 3)
        INSERT INTO cd VALUES (2, 6)
        INSERT INTO q VALUES (6, 0) ON CONFLICT DO UPDATE SET b = 1
        INSERT INTO q VALUES (6, 0) ON CONFLICT DO UPDATE SET a = 7
        COMMIT
        SELECT * FROM q
        SELECT * FROM c
        SELECT * FROM cd",
    );
}

/// Review fixes that need trigger programs.
#[cfg(feature = "triggers")]
#[test]
fn review_fixes_programs() {
    // Writes made by INSTEAD OF trigger bodies count their
    // violations into the view's statement: it fails on immediate
    // ones (in autocommit on deferred ones too), RETURNING rows or
    // not (hipp skipped the check and committed orphans).
    run_same(
        "rev-instead",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a PRIMARY KEY)
        CREATE TABLE c(x REFERENCES p)
        CREATE TABLE cd(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
        CREATE VIEW v AS SELECT * FROM c
        CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN INSERT INTO c VALUES (new.x); END
        CREATE VIEW vd AS SELECT * FROM cd
        CREATE TRIGGER tvd INSTEAD OF INSERT ON vd BEGIN INSERT INTO cd VALUES (new.x); END
        INSERT INTO v VALUES (5)
        INSERT INTO v VALUES (5) RETURNING x
        INSERT INTO vd VALUES (5)
        SELECT count(*) FROM c
        SELECT count(*) FROM cd
        BEGIN
        INSERT INTO vd VALUES (6)
        INSERT INTO v VALUES (6)
        INSERT INTO p VALUES (6)
        INSERT INTO v VALUES (6)
        COMMIT
        SELECT count(*) FROM cd
        DELETE FROM p
        CREATE VIEW w AS SELECT a FROM p
        CREATE TRIGGER td INSTEAD OF DELETE ON w BEGIN DELETE FROM p WHERE a = old.a; END
        CREATE TRIGGER tu INSTEAD OF UPDATE ON w BEGIN UPDATE p SET a = new.a WHERE a = old.a; END
        INSERT INTO p VALUES (1)
        INSERT INTO c VALUES (1)
        BEGIN
        DELETE FROM w
        UPDATE w SET a = 2
        UPDATE w SET a = 2 RETURNING a
        COMMIT
        DELETE FROM w
        SELECT * FROM p
        SELECT * FROM c
        PRAGMA foreign_key_check",
    );
    // A RETURNING INSTEAD OF write with deferred violations in
    // autocommit serves its rows, then fails.
    big(|| {
        same(
            "rev-instead-ret",
            &lines(
                "PRAGMA foreign_keys = ON
                CREATE TABLE p(a PRIMARY KEY)
                CREATE TABLE cd(x REFERENCES p DEFERRABLE INITIALLY DEFERRED)
                CREATE VIEW vd AS SELECT * FROM cd
                CREATE TRIGGER tvd INSTEAD OF INSERT ON vd BEGIN INSERT INTO cd VALUES (new.x); END
                INSERT INTO vd VALUES (5) RETURNING x
                SELECT count(*) FROM cd",
            ),
        );
    });
    // A FAIL halt inside a trigger program with violations pending
    // fails the statement with the key's error (hipp reported the
    // program's own RAISE(FAIL) / UNIQUE error).
    big(|| {
        let kinds = same(
            "rev-fail-halt",
            &lines(
                "PRAGMA foreign_keys = ON
                CREATE TABLE p(a PRIMARY KEY)
                CREATE TABLE c(x REFERENCES p)
                CREATE TABLE d(y)
                CREATE TRIGGER t AFTER INSERT ON d BEGIN INSERT INTO c VALUES (new.y); SELECT RAISE(FAIL, 'f') WHERE new.y = 3; END
                INSERT INTO p VALUES (1), (2)
                INSERT INTO d VALUES (1), (5), (3)
                CREATE TABLE u(k UNIQUE)
                CREATE TABLE e(y)
                CREATE TRIGGER t2 AFTER INSERT ON e BEGIN INSERT INTO c VALUES (new.y); INSERT INTO u VALUES (new.y % 2); END
                INSERT OR FAIL INTO e VALUES (1), (5), (3)
                INSERT INTO p VALUES (3)
                INSERT INTO d VALUES (1), (3)
                INSERT OR FAIL INTO e VALUES (2), (4)
                SELECT * FROM c
                SELECT * FROM d
                SELECT * FROM e
                SELECT * FROM u",
            ),
        );
        for n in ["7: ", "11: "] {
            assert!(
                kinds
                    .iter()
                    .any(|k| k.starts_with(n) && k.contains("ForeignKeyViolation")),
                "{n}{kinds:?}"
            );
        }
        assert!(
            kinds
                .iter()
                .any(|k| k.starts_with("13: ") && k.contains("RaiseFail")),
            "{kinds:?}"
        );
    });
    // An upsert's DO UPDATE reads its row again after BEFORE UPDATE
    // triggers, as an UPDATE does: a row they (or a cascade they
    // ran) deleted is skipped, a column they changed and DO UPDATE
    // does not assign keeps their value (found by the corpus).
    run_same(
        "rev-upsert-reread",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a PRIMARY KEY, b)
        CREATE TABLE c0(id INTEGER PRIMARY KEY, x REFERENCES p(a) ON DELETE CASCADE, y)
        CREATE TRIGGER t3 BEFORE UPDATE ON c0 BEGIN DELETE FROM p WHERE a = 1; END
        INSERT INTO p VALUES (1, 0), (2, 0), (3, 1)
        INSERT INTO c0 VALUES (NULL, 1, 1)
        INSERT INTO c0 VALUES (1, 6, 0) ON CONFLICT (id) DO UPDATE SET x = 2
        SELECT * FROM c0
        SELECT * FROM p
        CREATE TABLE c1(id INTEGER PRIMARY KEY, x, y UNIQUE)
        CREATE TRIGGER t4 BEFORE UPDATE ON c1 BEGIN UPDATE c1 SET y = 77 WHERE id = old.id; END
        INSERT INTO c1 VALUES (1, 1, 1)
        INSERT INTO c1 VALUES (1, 6, 0) ON CONFLICT (id) DO UPDATE SET x = 2
        SELECT * FROM c1
        PRAGMA integrity_check
        CREATE TABLE c2(id INTEGER PRIMARY KEY, x)
        CREATE TRIGGER t5 BEFORE UPDATE ON c2 BEGIN DELETE FROM c2 WHERE id = 1; END
        INSERT INTO c2 VALUES (1, 1), (2, 2)
        INSERT INTO c2 VALUES (1, 6) ON CONFLICT (id) DO UPDATE SET x = 3
        SELECT * FROM c2",
    );
    // The pin's `isSetNullAction` looks at the program compiled last
    // in the whole statement: a BEFORE trigger, or an earlier
    // statement of the program, that compiles a key's SET NULL action
    // makes the write after it skip that key's lookups (hipp modelled
    // only REPLACE).
    // A depth failure inside a transaction keeps the statement's
    // changes when no program holds an abort point: deferred keys,
    // and immediate keys whose parent rows go with CASCADE / SET
    // NULL, hold none; an UPDATE changing no key checks none
    // (found by the corpus; the tables have no rowid alias, which
    // hipp's model counts as an abort point for any UPDATE).
    big(|| {
        for (key, step) in [
            (
                "DEFERRABLE INITIALLY DEFERRED",
                "UPDATE c0 SET x = 8 WHERE 0",
            ),
            ("DEFERRABLE INITIALLY DEFERRED", "DELETE FROM p WHERE 0"),
            ("ON DELETE CASCADE", "DELETE FROM p WHERE 0"),
            ("ON DELETE SET NULL", "DELETE FROM p WHERE 0"),
            (
                "ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED",
                "DELETE FROM p WHERE 0",
            ),
            (
                "ON DELETE SET DEFAULT DEFERRABLE INITIALLY DEFERRED",
                "DELETE FROM p WHERE 0",
            ),
            ("", "DELETE FROM p WHERE 0"),
            ("", "UPDATE c0 SET y = 8 WHERE 0"),
            ("", "UPDATE c0 SET x = 8 WHERE 0"),
            ("ON UPDATE CASCADE", "UPDATE p SET a = 3 WHERE 0"),
        ] {
            same(
                "rev-depth-abort",
                &lines(&format!(
                    "PRAGMA foreign_keys = ON
                    PRAGMA recursive_triggers = ON
                    CREATE TABLE p(a PRIMARY KEY, b)
                    CREATE TABLE c0(id, x REFERENCES p(a) {key}, y)
                    CREATE TABLE d(z, w)
                    CREATE TRIGGER t1 AFTER INSERT ON d BEGIN INSERT INTO d VALUES (0, 0); {step}; END
                    BEGIN
                    INSERT INTO d VALUES (2, 1)
                    SELECT count(*) FROM d
                    COMMIT"
                )),
            );
        }
    });
    run_same(
        "rev-setnull-programs",
        "PRAGMA foreign_keys = ON
        CREATE TABLE p(a UNIQUE, b)
        CREATE TABLE c(id INTEGER PRIMARY KEY, x REFERENCES p(a) ON DELETE SET NULL, y)
        CREATE TRIGGER tr2 BEFORE INSERT ON c BEGIN DELETE FROM p WHERE a = new.id + 1; END
        INSERT INTO c VALUES (1, 1, 4)
        INSERT INTO c VALUES (2, 1, 4), (3, 7, 4)
        DROP TRIGGER tr2
        INSERT INTO c VALUES (4, 1, 4)
        CREATE TABLE d(z)
        CREATE TRIGGER td AFTER INSERT ON d BEGIN DELETE FROM p WHERE 0; INSERT INTO c VALUES (new.z, new.z, 0); END
        INSERT INTO d VALUES (7)
        CREATE TABLE e(z)
        CREATE TRIGGER te AFTER INSERT ON e BEGIN INSERT INTO c VALUES (new.z, new.z, 0); DELETE FROM p WHERE 0; END
        INSERT INTO e VALUES (8)
        CREATE TABLE f(z)
        CREATE TRIGGER tf AFTER INSERT ON f BEGIN UPDATE p SET a = a WHERE 0; INSERT INTO c VALUES (new.z, new.z, 0); END
        INSERT INTO f VALUES (9)
        CREATE TABLE c2(id INTEGER PRIMARY KEY, x REFERENCES p(a) ON UPDATE SET NULL)
        CREATE TABLE g(z)
        CREATE TRIGGER tg AFTER INSERT ON g BEGIN UPDATE p SET a = a WHERE 0; INSERT INTO c2 VALUES (new.z, new.z); END
        INSERT INTO g VALUES (10)
        CREATE TRIGGER tr3 BEFORE INSERT ON c2 BEGIN UPDATE p SET a = a WHERE 0; END
        INSERT INTO c2 VALUES (11, 11)
        UPDATE c2 SET x = 12 WHERE id = 11
        CREATE TRIGGER tr4 BEFORE UPDATE ON c2 BEGIN UPDATE p SET a = a WHERE 0; END
        UPDATE c2 SET x = 13 WHERE id = 11
        CREATE TABLE h(z)
        CREATE TRIGGER th AFTER INSERT ON h BEGIN DELETE FROM p WHERE 0; INSERT INTO lg VALUES (1); INSERT INTO c VALUES (new.z, new.z, 0); END
        CREATE TABLE lg(m)
        CREATE TRIGGER tlg AFTER INSERT ON lg BEGIN SELECT 1; END
        INSERT INTO h VALUES (14)
        CREATE VIEW vc AS SELECT * FROM c
        CREATE TRIGGER tvc INSTEAD OF INSERT ON vc BEGIN DELETE FROM p WHERE 0; INSERT INTO c VALUES (new.id, new.x, new.y); END
        INSERT INTO vc VALUES (15, 15, 0)
        INSERT INTO p VALUES (100, 0)
        INSERT INTO c VALUES (16, 100, 0) ON CONFLICT (id) DO UPDATE SET x = 17
        INSERT INTO c VALUES (16, 100, 0) ON CONFLICT (id) DO UPDATE SET x = 18
        SELECT * FROM c
        SELECT * FROM c2",
    );
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
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

const VALS: &[&str] = &[
    "1", "2", "3", "4", "NULL", "'1'", "'2'", "'a'", "'A'", "1.0", "2.5", "'x'", "x'41'",
];
const KEYS: &[&str] = &["1", "2", "3", "4", "NULL"];

/// One generated script: a parent, up to three children (and maybe a
/// self-referencing table) with random keys, actions and deferral,
/// maybe logging triggers, then random writes, transactions and
/// checks.
fn gen_script(rng: &mut Rng) -> Vec<String> {
    let mut s: Vec<String> = vec!["PRAGMA foreign_keys = ON".into()];
    if rng.chance(30) {
        s.push("PRAGMA recursive_triggers = ON".into());
    }
    let acts = [
        "",
        "ON DELETE CASCADE",
        "ON DELETE SET NULL",
        "ON DELETE SET DEFAULT",
        "ON DELETE RESTRICT",
        "ON UPDATE CASCADE",
        "ON UPDATE SET NULL",
        "ON UPDATE RESTRICT",
        "ON UPDATE SET DEFAULT",
        "ON DELETE CASCADE ON UPDATE CASCADE",
    ];
    let types = ["INTEGER", "TEXT", "", "REAL", "NUMERIC"];
    let pk = rng.pick(&["ipk", "uniq", "comp"]);
    match pk {
        "ipk" => s.push(format!(
            "CREATE TABLE p(a INTEGER PRIMARY KEY, b {}, c)",
            rng.pick(&["UNIQUE", "UNIQUE COLLATE NOCASE", ""])
        )),
        "uniq" => s.push(format!(
            "CREATE TABLE p(a {} UNIQUE{}, b, c)",
            rng.pick(&types),
            rng.pick(&["", " COLLATE NOCASE"])
        )),
        _ => s.push(format!(
            "CREATE TABLE p(a {}, b {}, c, PRIMARY KEY(a, b))",
            rng.pick(&types),
            rng.pick(&types)
        )),
    }
    let mut tabs = vec!["p".to_string()];
    for i in 0..(1 + rng.next() % 3) {
        let t = format!("c{i}");
        let ty = rng.pick(&types);
        let dflt = rng.pick(&["", "DEFAULT 1", "DEFAULT 9", "DEFAULT NULL"]);
        let act = rng.pick(&acts);
        let defer = rng.pick(&["", "", "DEFERRABLE INITIALLY DEFERRED"]);
        let uniq = if rng.chance(20) { " UNIQUE" } else { "" };
        if pk == "comp" {
            s.push(format!(
                "CREATE TABLE {t}(x {ty} {dflt}, y {}, n{uniq}, FOREIGN KEY (x, y) REFERENCES p {act} {defer})",
                rng.pick(&types)
            ));
        } else {
            let r = if pk == "ipk" && rng.chance(50) {
                "p"
            } else {
                "p(a)"
            };
            s.push(format!(
                "CREATE TABLE {t}(x {ty} {dflt} REFERENCES {r} {act} {defer}, n{uniq}, y)"
            ));
        }
        tabs.push(t);
    }
    // A grandchild keyed on a child's UNIQUE column, two keys on one
    // table, a composite key in another order.
    if rng.chance(30) {
        s.push(format!(
            "CREATE TABLE g(k REFERENCES c0(n) {} {}, j REFERENCES p {} , w)",
            rng.pick(&acts),
            rng.pick(&["", "DEFERRABLE INITIALLY DEFERRED"]),
            rng.pick(&acts)
        ));
        s.push("CREATE UNIQUE INDEX IF NOT EXISTS c0n ON c0(n)".into());
        tabs.push("g".into());
    }
    if pk == "comp" && rng.chance(40) {
        s.push(format!(
            "CREATE TABLE r(u, v, FOREIGN KEY (v, u) REFERENCES p(b, a) {} {})",
            rng.pick(&acts),
            rng.pick(&["", "DEFERRABLE INITIALLY DEFERRED"])
        ));
        tabs.push("r".into());
    }
    if rng.chance(40) {
        s.push(format!(
            "CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s {} {}, v)",
            rng.pick(&acts),
            rng.pick(&["", "", "DEFERRABLE INITIALLY DEFERRED"])
        ));
        tabs.push("s".into());
    }
    let trig = cfg!(feature = "triggers") && rng.chance(60);
    if trig {
        s.push("CREATE TABLE lg(m)".into());
        for t in tabs.clone().iter() {
            if rng.chance(40) {
                let ev = rng.pick(&["DELETE", "UPDATE", "INSERT"]);
                let tm = rng.pick(&["BEFORE", "AFTER"]);
                let r = if ev == "DELETE" { "old" } else { "new" };
                s.push(format!(
                    "CREATE TRIGGER tr_{t}_{ev}_{tm} {tm} {ev} ON {t} BEGIN INSERT INTO lg VALUES ('{t} {tm} {ev} ' || quote({r}.rowid) || ' ' || changes()); END"
                ));
            }
        }
    }
    for _ in 0..(5 + rng.next() % 20) {
        let t = tabs[(rng.next() % tabs.len() as u64) as usize].clone();
        let k = rng.next() % 100;
        if k < 35 {
            let n = 1 + rng.next() % 3;
            let mut rows = Vec::new();
            for _ in 0..n {
                let row = if (t == "p" && pk == "ipk") || t == "s" {
                    let a = rng.pick(KEYS);
                    let b = if t == "s" {
                        rng.pick(KEYS)
                    } else {
                        rng.pick(VALS)
                    };
                    format!("({a}, {b}, {})", rng.pick(VALS))
                } else {
                    format!(
                        "({}, {}, {})",
                        rng.pick(VALS),
                        rng.pick(VALS),
                        rng.pick(VALS)
                    )
                };
                rows.push(row);
            }
            let verb = rng.pick(&[
                "INSERT",
                "INSERT",
                "INSERT OR IGNORE",
                "INSERT OR REPLACE",
                "INSERT OR FAIL",
                "REPLACE",
            ]);
            let ret = if rng.chance(10) { " RETURNING *" } else { "" };
            s.push(format!("{verb} INTO {t} VALUES {}{ret}", rows.join(", ")));
        } else if k < 55 {
            let cols: &[&str] = if t == "p" {
                &["rowid", "a", "b", "c"]
            } else if t == "s" {
                &["rowid", "id", "par", "v"]
            } else {
                &["rowid", "x", "y", "n"]
            };
            let col = rng.pick(cols);
            let v = if rng.chance(20) {
                format!("{col} + 1")
            } else {
                rng.pick(VALS).to_string()
            };
            let verb = rng.pick(&[
                "UPDATE",
                "UPDATE",
                "UPDATE OR IGNORE",
                "UPDATE OR REPLACE",
                "UPDATE OR FAIL",
            ]);
            s.push(format!(
                "{verb} {t} SET {col} = {v} WHERE rowid % {} = {}",
                1 + rng.next() % 3,
                rng.next() % 2
            ));
        } else if k < 70 {
            s.push(format!(
                "DELETE FROM {t} WHERE rowid % {} = {}",
                1 + rng.next() % 3,
                rng.next() % 2
            ));
        } else if k < 80 {
            s.push(
                rng.pick(&[
                    "BEGIN",
                    "COMMIT",
                    "ROLLBACK",
                    "SAVEPOINT sp",
                    "RELEASE sp",
                    "ROLLBACK TO sp",
                    "PRAGMA defer_foreign_keys = ON",
                    "PRAGMA defer_foreign_keys",
                ])
                .into(),
            );
        } else if k < 85 {
            s.push(format!("PRAGMA foreign_key_check({t})"));
        } else if k < 88 {
            s.push(
                rng.pick(&["PRAGMA foreign_keys = OFF", "PRAGMA foreign_keys = ON"])
                    .into(),
            );
        } else if k < 90 {
            s.push(format!("DROP TABLE {t}"));
        } else if k < 93 && t == "p" {
            s.push(format!(
                "INSERT INTO p VALUES ({}, {}, {}) ON CONFLICT DO UPDATE SET a = excluded.a + 10",
                rng.pick(VALS),
                rng.pick(VALS),
                rng.pick(VALS)
            ));
        } else if k < 96 {
            let src = tabs[(rng.next() % tabs.len() as u64) as usize].clone();
            s.push(format!(
                "INSERT INTO {t} SELECT * FROM {src} WHERE rowid % 2 = {}",
                rng.next() % 2
            ));
        } else {
            s.push(format!("SELECT * FROM {t}"));
        }
    }
    s.push("COMMIT".into());
    for t in tabs.iter() {
        s.push(format!("SELECT '{t}', * FROM {t}"));
    }
    if trig {
        s.push("SELECT * FROM lg".into());
    }
    for t in tabs.iter() {
        s.push(format!("PRAGMA foreign_key_check({t})"));
    }
    s
}

/// One generated script whose trigger programs write the keyed
/// tables: parent rows deleted / updated and child rows added from
/// BEFORE / AFTER / INSTEAD OF bodies, under random conflict
/// clauses, with SET NULL keys likely (the pin's `isSetNullAction`
/// reads the last program it compiled), RAISE(FAIL) halts.
#[cfg(feature = "triggers")]
fn gen_program_script(rng: &mut Rng) -> Vec<String> {
    // (No `recursive_triggers`: a program writing its own table then
    // nests to the depth limit, where hipp's statement journal model
    // is the triggers unit's approximation, see STATUS.md.)
    let mut s: Vec<String> = vec!["PRAGMA foreign_keys = ON".into()];
    let acts = [
        "",
        "ON DELETE SET NULL",
        "ON DELETE SET NULL",
        "ON UPDATE SET NULL",
        "ON DELETE CASCADE",
        "ON UPDATE CASCADE",
        "ON DELETE SET DEFAULT",
        "ON DELETE RESTRICT",
    ];
    s.push(format!(
        "CREATE TABLE p(a {}, b)",
        rng.pick(&["INTEGER PRIMARY KEY", "UNIQUE", "PRIMARY KEY"])
    ));
    let nc = 1 + rng.next() % 2;
    for i in 0..nc {
        s.push(format!(
            "CREATE TABLE c{i}(id INTEGER PRIMARY KEY, x REFERENCES p(a) {} {}, y{})",
            rng.pick(&acts),
            rng.pick(&["", "", "DEFERRABLE INITIALLY DEFERRED"]),
            rng.pick(&["", "", " UNIQUE"])
        ));
    }
    if rng.chance(40) {
        s.push(format!(
            "CREATE TABLE s(id INTEGER PRIMARY KEY, par REFERENCES s {}, v)",
            rng.pick(&acts)
        ));
    }
    s.push("CREATE TABLE d(z, w)".into());
    s.push("CREATE VIEW v AS SELECT id, x, y FROM c0".into());
    let child = |rng: &mut Rng| format!("c{}", rng.next() % nc);
    let mut bodies = Vec::new();
    for _ in 0..(1 + rng.next() % 4) {
        let mut steps = Vec::new();
        for _ in 0..(1 + rng.next() % 3) {
            let c = child(rng);
            let st = match rng.next() % 9 {
                0 => format!(
                    "DELETE FROM p WHERE {}",
                    rng.pick(&["0", "a = 99", "a = 1", "b = 0"])
                ),
                1 => format!("UPDATE p SET a = a WHERE {}", rng.pick(&["0", "a = 2"])),
                2 => "UPDATE p SET a = a + 10 WHERE a = 3".into(),
                3 | 4 => format!(
                    "{} INTO {c} VALUES (NULL, {}, {})",
                    rng.pick(&["INSERT", "INSERT", "INSERT OR IGNORE", "REPLACE"]),
                    rng.pick(&["1", "2", "7", "NULL", "9"]),
                    rng.pick(&["0", "1"])
                ),
                5 => format!(
                    "UPDATE {c} SET x = {} WHERE id % 2 = 0",
                    rng.pick(&["1", "8", "NULL"])
                ),
                6 => format!("DELETE FROM {c} WHERE id = 1"),
                7 => "SELECT RAISE(FAIL, 'f') WHERE 0".into(),
                _ => "INSERT INTO d VALUES (0, 0)".into(),
            };
            steps.push(st);
        }
        bodies.push(steps.join("; "));
    }
    for (n, b) in bodies.iter().enumerate() {
        let mut on = rng.pick(&["d", "c0", "p", "v"]);
        let ev = rng.pick(&["INSERT", "UPDATE", "DELETE"]);
        // A body updating the children on a child (or its view) can
        // grow the table per row it updates: both engines then run
        // for minutes (probed), so such bodies go on d.
        if on != "d" && on != "p" && b.contains("UPDATE c") {
            on = "d";
        }
        let tm = if on == "v" {
            "INSTEAD OF"
        } else {
            rng.pick(&["BEFORE", "AFTER"])
        };
        // A trigger on d that writes d again is skipped by the pin's
        // non-recursive rule like any other.
        s.push(format!(
            "CREATE TRIGGER t{n} {tm} {ev} ON {on} BEGIN {b}; END"
        ));
    }
    s.push("INSERT INTO p VALUES (1, 0), (2, 0), (3, 1)".into());
    for _ in 0..(6 + rng.next() % 14) {
        let c = child(rng);
        let k = rng.next() % 100;
        let st = if k < 25 {
            format!(
                "{} INTO {c} VALUES ({}, {}, {})",
                rng.pick(&[
                    "INSERT",
                    "INSERT",
                    "INSERT OR REPLACE",
                    "INSERT OR FAIL",
                    "INSERT OR IGNORE",
                    "INSERT OR ROLLBACK"
                ]),
                rng.pick(&["1", "2", "3", "NULL", "4"]),
                rng.pick(&["1", "2", "5", "NULL", "9"]),
                rng.pick(&["0", "1", "2"])
            )
        } else if k < 40 {
            format!(
                "{} INTO d VALUES ({}, {})",
                rng.pick(&["INSERT", "INSERT OR REPLACE", "INSERT OR FAIL"]),
                rng.pick(&["1", "2", "5"]),
                rng.pick(&["0", "1"])
            )
        } else if k < 50 {
            format!(
                "INSERT INTO v VALUES ({}, {}, 0)",
                rng.pick(&["NULL", "7"]),
                rng.pick(&["1", "5"])
            )
        } else if k < 58 {
            format!(
                "UPDATE v SET x = {} WHERE id % 2 = 1",
                rng.pick(&["1", "6"])
            )
        } else if k < 62 {
            "DELETE FROM v WHERE id = 2".into()
        } else if k < 72 {
            format!(
                "{} {c} SET x = {} WHERE id % 2 = {}",
                rng.pick(&["UPDATE", "UPDATE OR REPLACE", "UPDATE OR FAIL"]),
                rng.pick(&["1", "2", "6", "NULL"]),
                rng.next() % 2
            )
        } else if k < 80 {
            format!("DELETE FROM p WHERE a = {}", rng.pick(&["1", "2", "3"]))
        } else if k < 85 {
            format!(
                "UPDATE p SET a = {} WHERE a = {}",
                rng.pick(&["4", "5"]),
                rng.pick(&["1", "2"])
            )
        } else if k < 88 {
            format!(
                "INSERT INTO {c} VALUES (1, 6, 0) ON CONFLICT (id) DO UPDATE SET x = {}",
                rng.pick(&["7", "2"])
            )
        } else if k < 95 {
            rng.pick(&[
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "PRAGMA defer_foreign_keys = ON",
            ])
            .into()
        } else {
            format!("PRAGMA foreign_key_check({c})")
        };
        s.push(st);
    }
    s.push("COMMIT".into());
    for i in 0..nc {
        s.push(format!("SELECT 'c{i}', * FROM c{i}"));
    }
    s.push("SELECT 'p', * FROM p".into());
    s.push("SELECT 'd', * FROM d".into());
    s.push("PRAGMA foreign_key_check(c0)".into());
    s
}

/// Generated trigger programs writing keyed tables: rows, failing
/// statements and final files agree with the pin.
#[cfg(feature = "triggers")]
#[test]
fn generated_corpus_programs() {
    big(|| {
        let seed = std::env::var("HIPP_FK_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0x7072_6f67_7300_0016u64);
        let mut rng = Rng(seed | 1);
        let n = std::env::var("HIPP_FK_CORPUS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60usize);
        for case in 0..n {
            let s = gen_program_script(&mut rng);
            same(&format!("prog{case}"), &s);
        }
    });
}

/// Generated parent / child schemas and write sequences: rows, the
/// failing statements and final files agree with the pin.
#[test]
fn generated_corpus() {
    big(|| {
        let seed = std::env::var("HIPP_FK_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0x666b_6579_7300_0016u64);
        let mut rng = Rng(seed | 1);
        let n = std::env::var("HIPP_FK_CORPUS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60usize);
        for case in 0..n {
            let s = gen_script(&mut rng);
            same(&format!("gen{case}"), &s);
        }
    });
}
