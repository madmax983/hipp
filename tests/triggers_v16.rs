//! v0.16 triggers (leaf feature `triggers`), differentially vs the
//! pinned oracle (SQLite 3.54.1, live). Probed on the pin before
//! implementation; what the corpus encodes:
//!
//! - The stored text is `CREATE TRIGGER ` and the statement from the
//!   bare trigger name to its `END` (`TEMP`, `IF NOT EXISTS` and a
//!   `main.` qualifier dropped); `tbl_name` is the table as written
//!   after `ON`. Trigger names have a namespace of their own per
//!   database; DROP TABLE / DROP VIEW drop the triggers on them.
//! - Triggers fire most recently created first. BEFORE INSERT sees
//!   defaults, affinities and generated columns applied and
//!   `NEW.rowid` -1 until the rowid is chosen; `NEW.col` carries no
//!   comparison affinity but the column's collation, the rowid
//!   INTEGER affinity. UPDATE / DELETE re-read each row (a deleted
//!   one is skipped), and the BEFORE UPDATE row once more (NEW's
//!   unassigned columns take the current values; NEW's generated
//!   columns are NULL there). `UPDATE OF` fires on the SET list.
//! - A trigger does not fire while it runs unless `PRAGMA
//!   recursive_triggers`; 1000 nested programs at most ("triggers
//!   nested too deep"), a program counting before its WHEN clause.
//! - RAISE: IGNORE skips the rest of the program, the remaining
//!   triggers of the row and (BEFORE) the row; FAIL keeps the
//!   statement's changes; ABORT undoes the statement; ROLLBACK the
//!   transaction. An explicit OR clause of the firing statement
//!   overrides every body statement's. A failed body statement
//!   counts nothing in total_changes().
//! - changes() / last_insert_rowid() inside a program are a frame of
//!   their own (restored after it). REPLACE deletions fire DELETE
//!   triggers only under recursive_triggers; an upsert's DO UPDATE
//!   fires UPDATE triggers.
//! - INSTEAD OF triggers make a view writable (no affinity on NEW,
//!   changes() 0); no upsert into a view.
//! - Errors: body name errors surface when the firing statement is
//!   prepared (even for zero rows); RETURNING, qualified targets,
//!   DEFAULT VALUES, WITH before a write, parameters, RAISE outside
//!   a trigger (a CHECK may hold one until a row is written).
//! - ALTER TABLE RENAME / RENAME COLUMN / DROP COLUMN edit trigger
//!   texts as the pin (the upsert `excluded.` qualifier quirk
//!   included) and refuse a schema whose triggers stop resolving.
//!
//! Every script runs statement by statement on both engines
//! (one statement per line, the pin's shell reading stdin,
//! continuing past errors): rows and the failing statements must
//! agree, and the oracle must read hipp's final file as its own.

#![cfg(feature = "triggers")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// A thread with room for the nested programs (each costs about
/// 255 KiB of stack in a debug build).
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

/// hipp: every statement in order, rows printed, errors noted by
/// 1-based statement number (and kind) and skipped past.
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
                    kinds.push(format!("{e:?}"));
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
                        kinds.push(format!("{e:?}"));
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
    let h = common::temp_db(&format!("v16trg-{tag}-h"));
    let o = common::temp_db(&format!("v16trg-{tag}-o"));
    let (hl, he, kinds) = hipp_run(&h, stmts, cfg);
    let (ol, oe) = oracle_run(&o, stmts);
    let script = stmts.join(";\n");
    assert_eq!(hl, ol, "{tag}: rows differ:\n{script}");
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

#[test]
fn stored_text_and_namespace() {
    run_same(
        "text",
        "CREATE TABLE t(a INTEGER, b TEXT)
        CREATE TABLE log(x, y, z)
        create     trigger if not exists main.tr1   after insert on t begin insert into log values(new.a, new.b, 'ins'); end
        CREATE TRIGGER IF NOT EXISTS tr2 BEFORE UPDATE OF a, b ON t FOR EACH ROW WHEN new.a > 0 BEGIN INSERT INTO log VALUES(old.a, new.a, 'upd') ; END
        CREATE TRIGGER \"tr 3\" DELETE ON t BEGIN SELECT 1; END
        CREATE TRIGGER IF NOT EXISTS tr1 AFTER DELETE ON t BEGIN SELECT 2; END
        CREATE TRIGGER tr1 AFTER DELETE ON t BEGIN SELECT 2; END
        CREATE TRIGGER t AFTER DELETE ON t BEGIN SELECT 3; END
        CREATE TRIGGER Tab AFTER INSERT ON T BEGIN SELECT 4; END
        SELECT type, name, tbl_name, rootpage, quote(sql) FROM sqlite_schema
        CREATE TRIGGER sqlite_x AFTER INSERT ON t BEGIN SELECT 1; END
        CREATE TRIGGER x AFTER INSERT ON sqlite_master BEGIN SELECT 1; END
        CREATE TRIGGER x AFTER INSERT ON nosuch BEGIN SELECT 1; END
        CREATE VIEW v AS SELECT * FROM t
        CREATE TRIGGER x BEFORE DELETE ON v BEGIN SELECT 1; END
        CREATE TRIGGER x INSTEAD OF DELETE ON t BEGIN SELECT 1; END
        CREATE TRIGGER x INSTEAD OF DELETE ON v BEGIN SELECT 1; END
        DROP TRIGGER nosuch
        DROP TRIGGER IF EXISTS nosuch
        DROP TRIGGER TR1
        DROP TRIGGER main.tr2
        SELECT name FROM sqlite_schema WHERE type = 'trigger'
        BEGIN
        DROP TRIGGER t
        ROLLBACK
        SELECT name FROM sqlite_schema WHERE type = 'trigger'
        DROP VIEW v
        DROP TABLE t
        SELECT name FROM sqlite_schema",
    );
}

#[test]
fn insert_order_new_values_and_counters() {
    run_same(
        "insert",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT DEFAULT 'dflt', c)
        CREATE TABLE log(n INTEGER PRIMARY KEY, w, x, y, z)
        CREATE TRIGGER b1 BEFORE INSERT ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('b1', new.id, new.rowid, typeof(new.a)||':'||new.a||':'||new.b); END
        CREATE TRIGGER b2 BEFORE INSERT ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('b2', new.id, new.rowid, new.c); END
        CREATE TRIGGER a1 AFTER INSERT ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('a1', new.id, new.rowid, typeof(new.a)); END
        CREATE TRIGGER a2 AFTER INSERT ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('a2', new.id, last_insert_rowid(), changes()); END
        INSERT INTO t(a, c) VALUES('12', 'x')
        SELECT 'after', last_insert_rowid(), changes(), total_changes()
        INSERT INTO t(id, a) VALUES(10, 'zz'), (NULL, 3.0)
        SELECT 'after2', last_insert_rowid(), changes(), total_changes()
        INSERT INTO t(a) SELECT a + 1 FROM t
        SELECT 'after3', last_insert_rowid(), changes(), total_changes()
        SELECT * FROM log
        SELECT * FROM t",
    );
}

#[test]
fn update_of_when_and_collation() {
    run_same(
        "update",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT COLLATE NOCASE, c)
        CREATE TABLE log(n INTEGER PRIMARY KEY, w, x, y, z)
        INSERT INTO t VALUES(1, 10, 'x', 'c1'), (2, 20, 'y', 'c2'), (3, 30, 'z', 'c3')
        CREATE TRIGGER u1 BEFORE UPDATE OF a ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('u1', old.id||'/'||new.id, old.a||'->'||new.a, typeof(new.a)||(new.b='X')); END
        CREATE TRIGGER u2 AFTER UPDATE ON t BEGIN INSERT INTO log(w,x,y,z) VALUES('u2', old.rowid||'/'||new.rowid, old.c||'->'||new.c, changes()); END
        CREATE TRIGGER u3 AFTER UPDATE OF b, id ON t WHEN new.a > 15 BEGIN INSERT INTO log(w,x,y,z) VALUES('u3', new.id, new.b, old.b); END
        CREATE TRIGGER u4 AFTER UPDATE OF zz ON t BEGIN INSERT INTO log(w) VALUES('never'); END
        UPDATE t SET a = '15' WHERE id = 1
        UPDATE t SET b = upper(b) WHERE id >= 2
        UPDATE t SET id = id + 10 WHERE id = 3
        UPDATE t SET c = c
        SELECT 'chg', changes(), total_changes()
        SELECT * FROM log
        SELECT * FROM t",
    );
}

#[test]
fn new_old_affinity_and_collation() {
    run_same(
        "affinity",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT COLLATE NOCASE, r REAL, n NUMERIC)
        CREATE TABLE u(w, x)
        CREATE TABLE s(v TEXT, q INT)
        INSERT INTO s VALUES('5', 5), ('B', 1)
        CREATE TRIGGER tr AFTER UPDATE ON t BEGIN INSERT INTO u VALUES('a=s', new.a = '5'); INSERT INTO u VALUES('s=a', '5' = new.a); INSERT INTO u VALUES ('oa=s', old.a = '4'); INSERT INTO u VALUES('b=5', new.b = 5); INSERT INTO u VALUES('b=B', new.b = 'B'); INSERT INTO u VALUES('B=b', 'B' = new.b); INSERT INTO u VALUES('id=s', new.id = '1'); INSERT INTO u VALUES('rid=s', new.rowid = '1'); INSERT INTO u VALUES('in', new.a IN ('5')); INSERT INTO u VALUES('cmpcol', (SELECT count(*) FROM s WHERE v = new.b)); INSERT INTO u VALUES('textcol', (SELECT count(*) FROM s WHERE v = new.a)); INSERT INTO u VALUES('intcol', (SELECT count(*) FROM s WHERE q = new.n)); INSERT INTO u VALUES('between', new.a BETWEEN '4' AND '6'); INSERT INTO u VALUES('real', typeof(new.r) || new.r); END
        INSERT INTO t VALUES(1, 4, 'b', 1, '5')
        UPDATE t SET a = 5, r = 2
        SELECT * FROM u",
    );
}

#[test]
fn delete_and_update_reread_rows() {
    run_same(
        "reread",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a)
        CREATE TABLE log(w, x, y)
        INSERT INTO t VALUES(1, 10), (2, 20), (3, 30), (4, 40)
        CREATE TRIGGER d1 BEFORE DELETE ON t BEGIN INSERT INTO log VALUES('d1', old.id, old.a); DELETE FROM t WHERE id = old.id + 1; UPDATE t SET a = a + 1 WHERE id = old.id + 2; END
        CREATE TRIGGER d2 AFTER DELETE ON t BEGIN INSERT INTO log VALUES('d2', old.id, (SELECT count(*) FROM t)); END
        DELETE FROM t WHERE a < 100
        SELECT 'chg', changes()
        SELECT * FROM log
        SELECT * FROM t
        DROP TRIGGER d1
        DROP TRIGGER d2
        DELETE FROM log
        INSERT INTO t VALUES(1, 10), (2, 20), (3, 30), (4, 40)
        CREATE TRIGGER u1 BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES('u1', old.id, old.a || '>' || new.a); UPDATE t SET a = a + 1000 WHERE id = old.id + 1; END
        CREATE TRIGGER u0 BEFORE UPDATE ON t WHEN old.id = 3 BEGIN DELETE FROM t WHERE id = 4; END
        CREATE TRIGGER u2 AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('u2', old.id, old.a || '>' || new.a); END
        UPDATE t SET a = a + 1 WHERE a < 100
        SELECT 'chg', changes()
        SELECT * FROM log
        SELECT * FROM t",
    );
}

#[test]
fn before_update_rereads_unassigned_columns() {
    run_same(
        "unassigned",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, c AS (a || b))
        CREATE TABLE log(x)
        INSERT INTO t VALUES(1, 'a1', 'b1'), (2, 'a2', 'b2')
        CREATE TRIGGER tr BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES(old.c || '/' || new.c); UPDATE t SET b = 'chg', a = 'trig' WHERE id = old.id; END
        CREATE TRIGGER tr2 AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('after ' || old.a || old.b || '/' || new.a || new.b || '/' || new.c); END
        UPDATE t SET a = 'new' WHERE id = 1
        UPDATE t SET id = 5 WHERE id = 2
        SELECT * FROM t
        SELECT * FROM log",
    );
}

#[test]
fn generated_columns_in_new_and_old() {
    run_same(
        "generated",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, c AS (a || b), d AS (b||a) STORED)
        CREATE TABLE log(w, x)
        CREATE TRIGGER bi BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('bi', quote(new.c)||quote(new.d)); END
        CREATE TRIGGER ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES('ai', quote(new.c)||quote(new.d)); END
        CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES('bu', quote(old.c)||quote(old.d)||'/'||quote(new.c)||quote(new.d)); END
        CREATE TRIGGER au AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('au', quote(old.c)||quote(old.d)||'/'||quote(new.c)||quote(new.d)); END
        CREATE TRIGGER bd BEFORE DELETE ON t BEGIN INSERT INTO log VALUES('bd', quote(old.c)||quote(old.d)); END
        CREATE TRIGGER ad AFTER DELETE ON t BEGIN INSERT INTO log VALUES('ad', quote(old.c)||quote(old.d)); END
        INSERT INTO t(a,b) VALUES('x','y')
        UPDATE t SET a = 'z'
        DELETE FROM t
        SELECT * FROM log",
    );
}

/// RAISE(IGNORE / FAIL / ABORT / ROLLBACK) from BEFORE and AFTER
/// triggers, inside an open transaction.
#[test]
fn raise_matrix() {
    big(|| {
        let raises = [
            "SELECT RAISE(IGNORE)",
            "SELECT CASE WHEN new.a = 2 THEN RAISE(IGNORE) END",
            "SELECT CASE WHEN new.a = 2 THEN RAISE(FAIL, 'f2') END",
            "SELECT CASE WHEN new.a = 2 THEN RAISE(ABORT, 'a2') END",
            "SELECT CASE WHEN new.a = 2 THEN RAISE(ROLLBACK, 'r2') END",
            "SELECT RAISE(FAIL, 'x' || new.a) WHERE new.a = 3",
            "SELECT coalesce(1, RAISE(ABORT, 'lazy'))",
        ];
        for (i, r) in raises.iter().enumerate() {
            for w in ["BEFORE", "AFTER"] {
                let script = format!(
                    "CREATE TABLE t(a)
                    CREATE TABLE u(x)
                    INSERT INTO t VALUES(0)
                    BEGIN
                    INSERT INTO t VALUES(-1)
                    CREATE TRIGGER tr {w} INSERT ON t BEGIN INSERT INTO u VALUES(new.a); {r}; INSERT INTO u VALUES(new.a*10); END
                    INSERT INTO t VALUES(1),(2),(3)
                    SELECT 'T', * FROM t
                    SELECT 'U', * FROM u
                    SELECT 'txn', count(*) FROM sqlite_schema WHERE type='trigger'
                    SELECT changes(), total_changes()
                    COMMIT"
                );
                same(&format!("raise{i}{w}"), &lines(&script));
            }
        }
    });
}

#[test]
fn raise_messages_and_ignore_nesting() {
    big(|| {
        let kinds = same(
            "raise-msg",
            &lines(
                "CREATE TABLE t(a)
                CREATE TRIGGER tr BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT, 'no ' || new.a) WHERE new.a > 1; END
                INSERT INTO t VALUES(1)
                INSERT INTO t VALUES(2)
                SELECT RAISE(IGNORE)
                CREATE TABLE z(a CHECK (RAISE(IGNORE)))
                INSERT INTO z VALUES(1)
                DROP TABLE z
                SELECT * FROM t",
            ),
        );
        assert!(kinds[0].contains("RaiseAbort"), "{kinds:?}");
        assert!(kinds[1].contains("RaiseOutsideTrigger"), "{kinds:?}");
        // The message, through the API.
        block_on(async {
            let mut db = Connection::open(hipp::MemStorage::new(), Config::default())
                .await
                .expect("open");
            for s in [
                "CREATE TABLE t(a)",
                "CREATE TRIGGER tr BEFORE INSERT ON t BEGIN SELECT RAISE(FAIL, 'bad ' || new.a); END",
            ] {
                let mut q = db.prepare(s).await.expect("prepare");
                q.step().await.expect("step");
            }
            let mut q = db
                .prepare("INSERT INTO t VALUES (7)")
                .await
                .expect("prepare");
            assert!(q.step().await.is_err());
            drop(q);
            assert_eq!(db.raise_message().as_slice(), b"bad 7");
        });
        same(
            "ignore-nesting",
            &lines(
                "CREATE TABLE t(a)
                CREATE TABLE u(x)
                CREATE TABLE w(y)
                CREATE TRIGGER t1 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(new.a); INSERT INTO u VALUES(new.a+100); END
                CREATE TRIGGER t2 BEFORE INSERT ON u BEGIN SELECT RAISE(IGNORE) WHERE new.x = 2; END
                CREATE TRIGGER t3 AFTER INSERT ON u BEGIN INSERT INTO w VALUES(new.x); END
                INSERT INTO t VALUES(1),(2),(3)
                SELECT 'chg', changes()
                SELECT 'U', * FROM u
                SELECT 'W', * FROM w
                CREATE TABLE z(a)
                CREATE TABLE zl(x)
                CREATE TRIGGER z1 BEFORE INSERT ON z BEGIN INSERT INTO zl VALUES('z1:'||new.a); END
                CREATE TRIGGER z2 BEFORE INSERT ON z BEGIN SELECT RAISE(IGNORE) WHERE new.a = 2; INSERT INTO zl VALUES('z2:'||new.a); END
                CREATE TRIGGER z3 AFTER INSERT ON z BEGIN INSERT INTO zl VALUES('z3:'||new.a); END
                INSERT INTO z VALUES(1),(2),(3)
                SELECT 'chg', changes()
                SELECT * FROM zl
                DELETE FROM zl
                CREATE TRIGGER z4 BEFORE DELETE ON z BEGIN SELECT RAISE(IGNORE) WHERE old.a = 1; END
                CREATE TRIGGER z5 BEFORE UPDATE ON z BEGIN SELECT RAISE(IGNORE) WHERE old.a = 3; END
                CREATE TRIGGER z6 AFTER UPDATE ON z WHEN new.a > 15 BEGIN SELECT RAISE(IGNORE); INSERT INTO zl VALUES('never'); END
                DELETE FROM z WHERE a < 2
                SELECT 'chg', changes()
                UPDATE z SET a = a * 10
                SELECT 'chg', changes()
                SELECT * FROM z
                SELECT * FROM zl",
            ),
        );
    });
}

/// The firing statement's OR clause against the body statement's.
#[test]
fn conflict_override_matrix() {
    big(|| {
        let outers = [
            "INSERT INTO",
            "INSERT OR IGNORE INTO",
            "INSERT OR REPLACE INTO",
            "INSERT OR FAIL INTO",
            "INSERT OR ABORT INTO",
            "INSERT OR ROLLBACK INTO",
            "REPLACE INTO",
        ];
        let bodies = [
            "INSERT INTO u VALUES(new.a % 2, new.a)",
            "INSERT OR FAIL INTO u VALUES(new.a % 2, new.a)",
            "INSERT OR IGNORE INTO u VALUES(new.a % 2, new.a)",
            "INSERT OR ROLLBACK INTO u VALUES(new.a % 2, new.a)",
            "REPLACE INTO u VALUES(new.a % 2, new.a)",
            "UPDATE OR FAIL u SET k = new.a % 2 WHERE v IS NULL",
        ];
        for (i, o) in outers.iter().enumerate() {
            for (j, b) in bodies.iter().enumerate() {
                let script = format!(
                    "CREATE TABLE t(a)
                    CREATE TABLE u(k UNIQUE, v)
                    INSERT INTO u VALUES(7, NULL), (8, NULL)
                    BEGIN
                    INSERT INTO t VALUES(-1)
                    CREATE TRIGGER tr AFTER INSERT ON t BEGIN {b}; END
                    {o} t VALUES(1),(2),(3),(4)
                    SELECT 'T', * FROM t
                    SELECT 'U', * FROM u
                    SELECT 'txn', count(*) FROM sqlite_schema WHERE type='trigger'
                    SELECT changes(), total_changes()
                    COMMIT"
                );
                same(&format!("conf{i}-{j}"), &lines(&script));
            }
        }
        // UPDATE / DELETE as the firing statement, and a table-level
        // ON CONFLICT ROLLBACK reached from a trigger.
        same(
            "conf-update",
            &lines(
                "CREATE TABLE t(a)
                CREATE TABLE u(k UNIQUE ON CONFLICT ROLLBACK, v)
                INSERT INTO t VALUES(1), (2)
                INSERT INTO u VALUES(1, 0)
                CREATE TRIGGER tr AFTER UPDATE ON t BEGIN INSERT INTO u VALUES(new.a, 1); END
                BEGIN
                UPDATE t SET a = a - 1
                SELECT 'txn', count(*) FROM u
                UPDATE OR IGNORE t SET a = a + 10
                SELECT * FROM u
                UPDATE OR FAIL t SET a = 1
                SELECT * FROM t
                COMMIT
                CREATE TRIGGER td AFTER DELETE ON t BEGIN INSERT INTO u VALUES(1, 9); END
                DELETE FROM t
                SELECT * FROM t
                SELECT * FROM u",
            ),
        );
    });
}

#[test]
fn recursion_and_depth() {
    big(|| {
        for n in [50, 999, 1000, 1001] {
            same_cfg(
                &format!("deep{n}"),
                &lines(&format!(
                    "PRAGMA recursive_triggers=1
                    CREATE TABLE t(a)
                    CREATE TRIGGER r AFTER INSERT ON t WHEN new.a < {n} BEGIN INSERT INTO t VALUES(new.a+1); END
                    INSERT INTO t VALUES(1)
                    SELECT count(*), max(a) FROM t
                    PRAGMA recursive_triggers = 0
                    INSERT INTO t VALUES(1)
                    SELECT count(*), max(a) FROM t"
                )),
                Config::default().max_trigger_depth(1000),
            );
        }
        same(
            "chain",
            &lines(
                "CREATE TABLE t(a)
                CREATE TABLE u(a)
                CREATE TRIGGER r AFTER INSERT ON t WHEN new.a < 10 BEGIN INSERT INTO u VALUES(new.a+1); END
                CREATE TRIGGER r2 AFTER INSERT ON u WHEN new.a < 10 BEGIN INSERT INTO t VALUES(new.a+1); END
                INSERT INTO t VALUES(1)
                SELECT count(*), max(a) FROM t
                SELECT count(*), max(a) FROM u
                PRAGMA recursive_triggers = ON
                INSERT INTO t VALUES(1)
                SELECT count(*), max(a) FROM t
                SELECT count(*), max(a) FROM u
                PRAGMA recursive_triggers",
            ),
        );
        // A recursive BEFORE trigger and a self-updating one.
        same(
            "self",
            &lines(
                "PRAGMA recursive_triggers = 1
                CREATE TABLE t(id INTEGER PRIMARY KEY, n)
                INSERT INTO t VALUES(1, 0)
                CREATE TRIGGER up AFTER UPDATE ON t WHEN new.n < 20 BEGIN UPDATE t SET n = n + 1 WHERE id = new.id; END
                UPDATE t SET n = 1
                SELECT * FROM t
                CREATE TRIGGER bi BEFORE INSERT ON t WHEN new.n > 0 BEGIN INSERT INTO t(n) VALUES(new.n - 1); END
                INSERT INTO t(n) VALUES(5)
                SELECT * FROM t",
            ),
        );
    });
    // hipp's limit, lowered.
    big(|| {
        block_on(async {
            let cfg = Config::default().max_trigger_depth(5);
            let mut db = Connection::open(hipp::MemStorage::new(), cfg)
                .await
                .expect("open");
            for s in [
                "PRAGMA recursive_triggers=1",
                "CREATE TABLE t(a)",
                "CREATE TRIGGER r AFTER INSERT ON t WHEN new.a < 5 BEGIN INSERT INTO t VALUES(new.a+1); END",
                "INSERT INTO t VALUES(1)",
            ] {
                let mut q = db.prepare(s).await.expect("prepare");
                q.step().await.expect("step");
            }
            let mut q = db
                .prepare("INSERT INTO t VALUES(0)")
                .await
                .expect("prepare");
            assert!(matches!(
                q.step().await,
                Err(hipp::Error::Schema {
                    kind: hipp::SchemaKind::TriggerDepth
                })
            ));
        });
    });
}

#[test]
fn replace_and_upsert_triggers() {
    big(|| {
        for rt in [0, 1] {
            same(
                &format!("replace{rt}"),
                &lines(&format!(
                    "PRAGMA recursive_triggers={rt}
                    CREATE TABLE t(id INTEGER PRIMARY KEY, k UNIQUE, v)
                    CREATE TABLE log(w, x, y)
                    INSERT INTO t VALUES(1, 'a', 10), (2, 'b', 20)
                    CREATE TRIGGER bi BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('bi', new.id, new.k); END
                    CREATE TRIGGER ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES('ai', new.id, new.k); END
                    CREATE TRIGGER bd BEFORE DELETE ON t BEGIN INSERT INTO log VALUES('bd', old.id, old.k); END
                    CREATE TRIGGER ad AFTER DELETE ON t BEGIN INSERT INTO log VALUES('ad', old.id, old.k); END
                    CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES('bu', old.id||'>'||new.id, old.k||'>'||new.k); END
                    CREATE TRIGGER au AFTER UPDATE OF v ON t BEGIN INSERT INTO log VALUES('au', old.id||'>'||new.id, old.v||'>'||new.v); END
                    INSERT OR REPLACE INTO t VALUES(1, 'b', 99)
                    SELECT 'chg', changes()
                    INSERT INTO t VALUES(5, 'b', 5) ON CONFLICT(k) DO UPDATE SET v = excluded.v + 1000
                    SELECT 'chg', changes()
                    INSERT INTO t VALUES(6, 'b', 6) ON CONFLICT(k) DO NOTHING
                    INSERT INTO t VALUES(6, 'b', 6) ON CONFLICT(k) DO UPDATE SET k = 'c'
                    SELECT 'chg', changes()
                    UPDATE OR REPLACE t SET k = 'c' WHERE id = 5
                    INSERT INTO t VALUES(7, 'z', 1)
                    UPDATE OR REPLACE t SET id = 7 WHERE id = 1
                    SELECT 'chg', changes(), total_changes()
                    SELECT * FROM log
                    SELECT * FROM t"
                )),
            );
        }
    });
}

#[test]
fn instead_of_triggers_on_views() {
    run_same(
        "views",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)
        INSERT INTO t VALUES(1, 10, 'x'), (2, 20, 'y')
        CREATE VIEW v AS SELECT id, a, b, a*2 AS d FROM t
        CREATE TABLE log(w, x, y, z)
        INSERT INTO v VALUES(3, 30, 'z', 0)
        UPDATE v SET a = 1
        DELETE FROM v
        CREATE TRIGGER vi INSTEAD OF INSERT ON v BEGIN INSERT INTO log VALUES('vi', new.id, typeof(new.a)||new.a, new.d); INSERT INTO t VALUES(new.id, new.a, new.b); END
        CREATE TRIGGER vu INSTEAD OF UPDATE OF a ON v BEGIN INSERT INTO log VALUES('vu', old.id||'>'||new.id, old.a||'>'||new.a, old.d||'>'||new.d); END
        CREATE TRIGGER vd INSTEAD OF DELETE ON v FOR EACH ROW BEGIN INSERT INTO log VALUES('vd', old.id, old.a, old.d); DELETE FROM t WHERE id = old.id; END
        INSERT INTO v VALUES(3, '30', 'z', 0)
        SELECT 'chg', changes(), last_insert_rowid()
        INSERT INTO v(a, id) VALUES(40, 4), (50, 5)
        SELECT 'chg', changes(), last_insert_rowid()
        INSERT INTO v(a, nosuch) VALUES(1, 2)
        INSERT INTO v VALUES(1, 2)
        UPDATE v SET a = a + 1, d = 7 WHERE id > 3
        SELECT 'chg', changes()
        UPDATE v SET b = 'q'
        UPDATE v AS w SET a = w.a WHERE w.id = 1
        DELETE FROM v WHERE a = 10
        SELECT 'chg', changes()
        INSERT INTO v SELECT * FROM v WHERE id = 2
        INSERT INTO v VALUES(1,2,3,4) ON CONFLICT DO NOTHING
        INSERT OR REPLACE INTO v VALUES(9,2,3,4)
        SELECT * FROM log
        SELECT * FROM t
        DROP VIEW v
        SELECT name FROM sqlite_schema",
    );
}

#[test]
fn body_errors_surface_at_prepare() {
    run_same(
        "errors",
        "CREATE TABLE t(a,b)
        CREATE TABLE u(x)
        CREATE TRIGGER e1 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(old.a); END
        INSERT INTO t VALUES(1,2)
        INSERT INTO t SELECT 1,2 WHERE 0
        SELECT 'cnt', count(*) FROM t
        DROP TRIGGER e1
        CREATE TRIGGER e2 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(new.zz); END
        INSERT INTO t VALUES(1,2)
        DROP TRIGGER e2
        CREATE TRIGGER e3 AFTER INSERT ON t BEGIN INSERT INTO nosuch VALUES(1); END
        INSERT INTO t VALUES(1,2)
        UPDATE t SET a = 1
        DROP TRIGGER e3
        CREATE TRIGGER e4 AFTER INSERT ON t BEGIN INSERT INTO main.u VALUES(1); END
        CREATE TRIGGER e5 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(1) RETURNING x; END
        CREATE TRIGGER e6 AFTER INSERT ON t BEGIN WITH c AS (SELECT 1) INSERT INTO u SELECT * FROM c; END
        CREATE TRIGGER e9 AFTER INSERT ON t BEGIN CREATE TABLE z(a); END
        CREATE TRIGGER e19 AFTER INSERT ON t FOR EACH STATEMENT BEGIN SELECT 1; END
        CREATE TRIGGER e26 AFTER INSERT ON t BEGIN INSERT INTO u DEFAULT VALUES; END
        CREATE TRIGGER e27 AFTER INSERT ON t BEGIN SELECT new.*; END
        INSERT INTO t VALUES(1, 2)
        DROP TRIGGER e27
        CREATE TRIGGER e7 AFTER INSERT ON t BEGIN DELETE FROM u WHERE x = new.a; UPDATE u SET x = new.b WHERE x IS NULL; SELECT * FROM u; END
        CREATE TRIGGER e8 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(1) ON CONFLICT DO NOTHING; END
        CREATE TRIGGER e11 AFTER INSERT ON t WHEN old.a BEGIN SELECT 1; END
        CREATE TRIGGER e12 AFTER INSERT ON t BEGIN WITH c AS (SELECT new.a AS q) SELECT q FROM c; VALUES(1); SELECT 1 UNION SELECT 2; END
        CREATE TRIGGER e13 AFTER INSERT ON t BEGIN INSERT INTO u SELECT new.a FROM t AS new; END
        INSERT INTO t VALUES (3, 4)
        DROP TRIGGER e11
        INSERT INTO t VALUES (3, 4)
        SELECT name, tbl_name FROM sqlite_schema
        SELECT * FROM u",
    );
}

#[test]
fn savepoints_and_statement_rollback() {
    run_same(
        "savepoints",
        "CREATE TABLE t(a UNIQUE)
        CREATE TABLE log(x)
        CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(new.a); END
        CREATE TRIGGER td BEFORE DELETE ON t BEGIN INSERT INTO log VALUES(-old.a); END
        BEGIN
        INSERT INTO t VALUES(1)
        SAVEPOINT s1
        INSERT INTO t VALUES(2), (3)
        DELETE FROM t WHERE a = 1
        SELECT 'in', * FROM log
        ROLLBACK TO s1
        SELECT 'after rb', * FROM log
        INSERT INTO t VALUES(4), (1)
        SELECT 'after abort', * FROM log
        INSERT INTO t VALUES(5)
        RELEASE s1
        COMMIT
        SELECT * FROM t
        SELECT * FROM log
        SELECT changes(), total_changes()
        BEGIN
        CREATE TRIGGER tx AFTER UPDATE ON t BEGIN SELECT RAISE(ROLLBACK, 'nope'); END
        UPDATE t SET a = a + 100
        SELECT 'txn?', count(*) FROM sqlite_schema
        INSERT INTO t VALUES(77)
        COMMIT
        SELECT * FROM t",
    );
}

#[test]
fn alter_table_rewrites_trigger_texts() {
    run_same(
        "alter1",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)
        CREATE TABLE log(w, x, y)
        CREATE TABLE other(a, b)
        CREATE TRIGGER tr1 AFTER INSERT ON t WHEN new.a > 0 BEGIN INSERT INTO log(w, x) VALUES('ins', new.a); UPDATE t SET b = new.a WHERE id = new.id; DELETE FROM log WHERE x = (SELECT a FROM t WHERE t.id = new.id); SELECT o.a, t.b FROM t, other AS o WHERE o.a = t.a; END
        CREATE TRIGGER tr2 BEFORE UPDATE OF a, b ON t BEGIN INSERT INTO other(a, b) SELECT a, b FROM t WHERE a = old.a; END
        CREATE TRIGGER tr3 AFTER INSERT ON log BEGIN UPDATE t SET a = a WHERE a = new.x; INSERT INTO t(a) VALUES(new.x) ON CONFLICT(id) DO UPDATE SET a = excluded.a; END
        CREATE TRIGGER \"tr4\" AFTER DELETE ON [t] BEGIN SELECT old.a, old.\"a\", OLD.[a]; END
        ALTER TABLE t RENAME COLUMN a TO aa
        SELECT name, sql FROM sqlite_schema WHERE type = 'trigger'
        ALTER TABLE t RENAME TO \"t 2\"
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE type = 'trigger'
        ALTER TABLE log RENAME COLUMN x TO xx
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE name='tr3' OR name = 'tr1'
        ALTER TABLE \"t 2\" DROP COLUMN b
        ALTER TABLE other DROP COLUMN b
        ALTER TABLE log DROP COLUMN y
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE type = 'trigger'
        INSERT INTO \"t 2\"(aa) VALUES (5)
        SELECT * FROM log",
    );
    run_same(
        "alter2",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)
        CREATE TABLE u(x, y)
        CREATE VIEW v AS SELECT a, b FROM t
        CREATE TRIGGER t1 AFTER INSERT ON u WHEN (SELECT count(*) FROM t WHERE a = new.x) > 0 BEGIN UPDATE t SET b = 'hit' WHERE t.a = new.x; INSERT INTO t(a, b) SELECT x, y FROM u AS q WHERE q.x NOT IN (SELECT a FROM t); END
        CREATE TRIGGER t2 BEFORE DELETE ON t FOR EACH ROW BEGIN DELETE FROM u WHERE x = old.a OR y = OLD.b; SELECT T.a FROM t AS T; SELECT a FROM t x WHERE x.b IS NOT NULL; END
        CREATE TRIGGER t3 INSTEAD OF UPDATE OF a ON v BEGIN UPDATE t SET a = new.a WHERE a = old.a; END
        CREATE TRIGGER t4 AFTER UPDATE OF A, b ON T BEGIN INSERT INTO u VALUES (NEW.A, new.B); END
        ALTER TABLE t RENAME COLUMN a TO \"a b\"
        SELECT name, sql FROM sqlite_schema WHERE type IN ('trigger', 'view')
        ALTER TABLE t RENAME TO tt
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE type IN ('trigger', 'view')
        ALTER TABLE u RENAME COLUMN x TO xx
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE type IN ('trigger')
        ALTER TABLE u DROP COLUMN y
        SELECT name, tbl_name, sql FROM sqlite_schema WHERE type IN ('trigger')
        INSERT INTO u VALUES(1)
        SELECT * FROM tt",
    );
    run_same(
        "alter3",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c)
        CREATE TABLE u(x, y)
        CREATE TRIGGER t1 AFTER INSERT ON u BEGIN UPDATE t SET b = new.y WHERE a = new.x; END
        ALTER TABLE t DROP COLUMN a
        ALTER TABLE t DROP COLUMN b
        ALTER TABLE t DROP COLUMN c
        SELECT sql FROM sqlite_schema
        CREATE TRIGGER t2 AFTER INSERT ON t BEGIN INSERT INTO u VALUES(new.c, 1); END
        CREATE TRIGGER t3 AFTER INSERT ON t BEGIN INSERT INTO nosuch VALUES(1); END
        ALTER TABLE t RENAME COLUMN a TO z
        ALTER TABLE u RENAME TO uu
        ALTER TABLE u DROP COLUMN y
        DROP TRIGGER t3
        DROP TRIGGER t2
        CREATE TRIGGER t4 AFTER INSERT ON t BEGIN INSERT INTO u(x) VALUES(new.a) ON CONFLICT(x) DO UPDATE SET y = excluded.x + u.x; END
        ALTER TABLE u RENAME COLUMN x TO xx
        ALTER TABLE u RENAME TO uu
        SELECT name, sql FROM sqlite_schema WHERE type = 'trigger'",
    );
}

#[test]
fn temp_triggers() {
    run_same(
        "temp",
        "CREATE TEMP TABLE tt(a)
        CREATE TEMP TABLE tl(x)
        CREATE TRIGGER x1 AFTER INSERT ON tt BEGIN INSERT INTO tl VALUES(new.a); END
        CREATE TEMP TRIGGER x2 BEFORE INSERT ON temp.tt BEGIN INSERT INTO tl VALUES(-new.a); END
        SELECT 'main', name FROM sqlite_schema
        SELECT 'temp', name, tbl_name, sql FROM sqlite_temp_schema
        INSERT INTO tt VALUES(5)
        SELECT * FROM tl
        DROP TRIGGER x1
        DROP TRIGGER temp.x2
        SELECT 'temp', name FROM sqlite_temp_schema
        CREATE TEMP TRIGGER temp.x3 AFTER INSERT ON tt BEGIN SELECT 1; END
        CREATE TABLE m(a)
        CREATE TRIGGER x4 AFTER INSERT ON m BEGIN SELECT * FROM temp.tt; END
        CREATE TRIGGER x5 AFTER INSERT ON temp.tt BEGIN SELECT 1; END
        SELECT 'temp', name FROM sqlite_temp_schema
        DROP TABLE tt
        SELECT 'temp', name FROM sqlite_temp_schema",
    );
}

/// A TEMP trigger on a main table is refused (hipp runs a statement
/// in one database only): an error, never a trigger that does not
/// fire.
#[test]
fn temp_trigger_on_main_table_is_refused() {
    big(|| {
        block_on(async {
            let mut db = Connection::open(hipp::MemStorage::new(), Config::default())
                .await
                .expect("open");
            let mut q = db.prepare("CREATE TABLE m(a)").await.expect("prepare");
            q.step().await.expect("step");
            drop(q);
            let r = db
                .prepare("CREATE TEMP TRIGGER x AFTER INSERT ON m BEGIN SELECT 1; END")
                .await;
            assert!(matches!(r, Err(hipp::Error::Unsupported)));
        });
    });
}

/// Files the pin wrote with triggers: hipp's writes fire them, and
/// the pin fires the triggers hipp created.
#[test]
fn foreign_files_fire_both_ways() {
    big(|| {
        let p = common::temp_db("v16trg-foreign");
        common::cli(
            &p,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a); CREATE TABLE log(x); \
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES('pin:' || new.a); END;",
        );
        let (_, errs, _) = hipp_run(
            &p,
            &lines(
                "INSERT INTO t(a) VALUES (1)
                CREATE TRIGGER th BEFORE DELETE ON t BEGIN INSERT INTO log VALUES('hipp:' || old.a); END",
            ),
            Config::default(),
        );
        assert!(errs.is_empty());
        common::cli(&p, "DELETE FROM t; INSERT INTO t(a) VALUES (2);");
        assert_eq!(
            common::cli(&p, "SELECT x FROM log;"),
            "pin:1\nhipp:1\npin:2\n"
        );
        assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
    });
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
}

/// Generated schemas of triggers over two tables and a log, and
/// generated writes: rows, failures and final files agree.
#[test]
fn generated_corpus() {
    big(|| {
        let seed = std::env::var("HIPP_TRIGGER_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0x7419_6e65_7273_0016u64);
        let mut rng = Rng(seed | 1);
        let n = std::env::var("HIPP_TRIGGER_CORPUS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40usize);
        for case in 0..n {
            let mut s: Vec<String> = Vec::new();
            s.push(format!("PRAGMA recursive_triggers = {}", rng.next() % 2));
            s.push("CREATE TABLE a(id INTEGER PRIMARY KEY, k UNIQUE, v INT DEFAULT 0, w TEXT COLLATE NOCASE)".into());
            s.push("CREATE TABLE b(x, y REAL)".into());
            s.push("CREATE TABLE log(n INTEGER PRIMARY KEY, e, p, q)".into());
            s.push("CREATE VIEW va AS SELECT id, k, v FROM a".into());
            let ntrig = 1 + rng.next() % 5;
            for t in 0..ntrig {
                let on = rng.pick(&["a", "b", "a", "va"]);
                let time = if on == "va" {
                    "INSTEAD OF"
                } else {
                    rng.pick(&["BEFORE", "AFTER", "AFTER"])
                };
                let ev = rng.pick(&[
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                    "UPDATE OF v",
                    "UPDATE OF k, w",
                ]);
                let (n_ok, o_ok) = match ev {
                    "INSERT" => (true, false),
                    "DELETE" => (false, true),
                    _ => (true, true),
                };
                let col = |rng: &mut Rng, base: &str| -> String {
                    let c = if on == "b" {
                        rng.pick(&["x", "y", "rowid"])
                    } else if on == "va" {
                        rng.pick(&["id", "k", "v"])
                    } else {
                        rng.pick(&["id", "k", "v", "w", "rowid"])
                    };
                    format!("{base}.{c}")
                };
                let r = if n_ok && (!o_ok || rng.next().is_multiple_of(2)) {
                    "new"
                } else {
                    "old"
                };
                let e1 = col(&mut rng, r);
                let e2 = col(&mut rng, r);
                let when = match rng.next() % 4 {
                    0 => format!(" WHEN {e1} > 2"),
                    1 => format!(" WHEN {e1} IS NOT NULL"),
                    _ => String::new(),
                };
                let mut body = format!("INSERT INTO log(e, p, q) VALUES('t{t}', {e1}, {e2});");
                match rng.next() % 8 {
                    6 => body.push_str(&format!(
                        " SELECT RAISE(ABORT, 'a') WHERE {e1} = 5;"
                    )),
                    7 => body.push_str(&format!(
                        " SELECT RAISE(ROLLBACK, 'r') WHERE {e2} = 6;"
                    )),
                    0 => body.push_str(&format!(" SELECT RAISE(IGNORE) WHERE {e2} = 3;")),
                    1 => body.push_str(&format!(
                        " UPDATE b SET y = coalesce(y, 0) + 1 WHERE x = {e1};"
                    )),
                    // Bounded: an error past the depth limit inside BEGIN
                    // keeps the statement's changes on the pin when its
                    // program has no abort point (see STATUS.md).
                    2 => body.push_str(&format!(
                        " INSERT INTO b SELECT {e1}, {e2} WHERE (SELECT count(*) FROM b) < 40;"
                    )),
                    3 => body.push_str(&format!(" DELETE FROM b WHERE x = {e2};")),
                    4 => body.push_str(&format!(
                        " SELECT RAISE(FAIL, 'f') WHERE {e1} = 7;"
                    )),
                    _ => body.push_str(
                        " INSERT OR IGNORE INTO a(k, v) SELECT changes(), last_insert_rowid() WHERE (SELECT count(*) FROM a) < 40;",
                    ),
                }
                s.push(format!(
                    "CREATE TRIGGER g{t} {time} {ev} ON {on}{when} BEGIN {body} END"
                ));
            }
            for _ in 0..(4 + rng.next() % 6) {
                let k = rng.next() % 6;
                let v = rng.next() % 9;
                let stmt = match rng.next() % 13 {
                    9 => "BEGIN".to_string(),
                    10 => "COMMIT".to_string(),
                    11 => "SAVEPOINT s".to_string(),
                    12 => "ROLLBACK TO s".to_string(),
                    0 => format!("INSERT INTO a(k, v, w) VALUES({k}, {v}, 'w{v}')"),
                    1 => format!("INSERT OR REPLACE INTO a(k, v) VALUES({k}, {v})"),
                    2 => format!(
                        "INSERT INTO a(k, v) VALUES({k}, {v}) ON CONFLICT(k) DO UPDATE SET v = v + excluded.v"
                    ),
                    3 => format!("UPDATE a SET v = v + {v} WHERE k <= {k}"),
                    4 => format!("UPDATE OR IGNORE a SET k = k + 1 WHERE v > {v}"),
                    5 => format!("DELETE FROM a WHERE k = {k}"),
                    6 => format!("INSERT INTO b VALUES({k}, {v}), ({v}, NULL)"),
                    7 => format!("UPDATE va SET v = {v} WHERE k = {k}"),
                    _ => format!("DELETE FROM b WHERE x < {k}"),
                };
                s.push(stmt);
                s.push("SELECT changes(), total_changes()".into());
            }
            s.push("SELECT * FROM a".into());
            s.push("SELECT * FROM b".into());
            s.push("SELECT * FROM log".into());
            same(&format!("gen{case}"), &s);
        }
    });
}

/// Regressions found by the generated corpus (each once differed
/// from the pin).
#[test]
fn regressions() {
    // RAISE(FAIL) deep in a chain of REPLACE deletions keeps every
    // change so far (a program's statement used to roll its own part
    // back under a statement savepoint).
    run_same(
        "reg-fail-chain",
        "PRAGMA recursive_triggers = 1
        CREATE TABLE a(id INTEGER PRIMARY KEY, k UNIQUE, v)
        CREATE TABLE log(n INTEGER PRIMARY KEY, e, p)
        CREATE TRIGGER g0 AFTER INSERT ON a BEGIN INSERT INTO log(e,p) VALUES('t0', new.id); INSERT OR IGNORE INTO a(k, v) SELECT 1, 1 WHERE new.id < 9; END
        CREATE TRIGGER g3 AFTER DELETE ON a BEGIN INSERT INTO log(e, p) VALUES('t3', old.id); SELECT RAISE(FAIL, 'f') WHERE old.id = 3; END
        INSERT OR REPLACE INTO a(k) VALUES(1)
        SELECT changes(), total_changes()
        SELECT * FROM a
        SELECT * FROM log",
    );
    // An upsert's DO UPDATE runs its UPDATE triggers under ABORT.
    run_same(
        "reg-upsert-abort",
        "CREATE TABLE a(id INTEGER PRIMARY KEY, k UNIQUE, v INT DEFAULT 0)
        CREATE TABLE log(n INTEGER PRIMARY KEY, e, p, q)
        CREATE TRIGGER g1 BEFORE UPDATE OF v ON a BEGIN INSERT INTO log(e, p, q) VALUES('t1', new.k, new.id); INSERT OR IGNORE INTO a(k, v) VALUES(changes(), last_insert_rowid()); END
        INSERT INTO a(k, v) VALUES(1, 3)
        INSERT INTO a(k, v) VALUES(2, 7) ON CONFLICT(k) DO UPDATE SET v = v + excluded.v
        INSERT INTO a(k, v) VALUES(2, 4) ON CONFLICT(k) DO UPDATE SET v = v + excluded.v
        SELECT changes(), total_changes()
        SELECT * FROM a
        SELECT * FROM log",
    );
    // A REPLACE victim whose BEFORE DELETE trigger RAISE(IGNORE)s
    // stays, and the conflict fails the write; DELETE triggers that
    // write a conflicting row again fail it too.
    run_same(
        "reg-replace-recheck",
        "PRAGMA recursive_triggers = 1
        CREATE TABLE a(id INTEGER PRIMARY KEY, k UNIQUE, v)
        CREATE TABLE log(x)
        INSERT INTO a VALUES(1, 1, 0), (2, 2, 0), (3, 3, 0)
        CREATE TRIGGER g0 BEFORE DELETE ON a WHEN old.id = 2 BEGIN INSERT INTO log VALUES(old.id); SELECT RAISE(IGNORE); END
        INSERT OR REPLACE INTO a VALUES(9, 2, 9)
        INSERT OR REPLACE INTO a VALUES(2, 7, 9)
        UPDATE OR REPLACE a SET k = 2 WHERE id = 1
        CREATE TRIGGER g1 AFTER DELETE ON a WHEN old.id = 3 BEGIN INSERT INTO a VALUES(old.id + 10, old.k, 0); END
        INSERT OR REPLACE INTO a VALUES(5, 3, 5)
        UPDATE OR REPLACE a SET k = 3 WHERE id = 1
        SELECT * FROM a
        SELECT * FROM log",
    );
    // A RAISE(IGNORE) earlier in the statement leaves no failure
    // behind: a later RAISE(FAIL) still keeps the changes (autocommit).
    run_same(
        "reg-ignore-then-fail",
        "CREATE TABLE t(a)
        CREATE TABLE u(x)
        CREATE TRIGGER tr BEFORE INSERT ON t BEGIN INSERT INTO u VALUES (new.a); SELECT RAISE(IGNORE) WHERE new.a = 2; SELECT RAISE(FAIL, 'three') WHERE new.a = 3; END
        INSERT INTO t VALUES (1), (2), (3), (4)
        SELECT * FROM t
        SELECT * FROM u
        SELECT changes(), total_changes()",
    );
    // RAISE() is never folded at plan time.
    run_same(
        "reg-raise-fold",
        "CREATE TABLE t(a)
        CREATE TABLE log(x)
        CREATE TRIGGER g AFTER INSERT ON t BEGIN INSERT INTO log VALUES(new.a); SELECT RAISE(FAIL, 'f') WHERE 1 = 2; SELECT 1 WHERE 0 AND RAISE(ABORT, 'x'); INSERT INTO log SELECT RAISE(ABORT, 'y') WHERE 0; END
        INSERT INTO t VALUES(1), (2)
        SELECT * FROM log
        SELECT 0 AND abs(-9223372036854775808), 2 OR abs(-9223372036854775808)
        SELECT (1 AND 0) AND abs(-9223372036854775808), abs(-9223372036854775808) OR 3
        SELECT 1 AND abs(-9223372036854775808)
        SELECT 0 OR abs(-9223372036854775808)",
    );
}
