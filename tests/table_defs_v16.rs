//! v0.16 column constraints and table definitions,
//! differentially vs the pinned oracle (SQLite 3.54.1). The
//! semantics were probed on the pin before implementation; the
//! matrix the tests encode:
//!
//! - CHECK constraints (ungated wherever the v0.4 evaluator is
//!   built — they used to be parsed and ignored): column-level,
//!   table-level and `CONSTRAINT name CHECK`, enforced on INSERT
//!   (VALUES, SELECT, REPLACE), UPDATE (incl. FROM) and the
//!   upsert DO UPDATE row. A NULL result passes; the expression
//!   sees affinity-converted values and the final rowid; OR
//!   IGNORE skips the row, every other action errors (OR
//!   REPLACE does not resolve a CHECK); the DO UPDATE row errors
//!   even under OR IGNORE. Order: NOT NULL, generated columns,
//!   STRICT types, CHECK, then uniqueness.
//! - DEFAULT values (ungated): `(expr)` constants (`(1+2)` used
//!   to store 1), signed literals, TRUE / FALSE, string / blob
//!   literals, `DEFAULT identifier` (its text), `-'3'`, and
//!   CURRENT_TIME / DATE / TIMESTAMP through `Config::clock`
//!   (`Error::NoClock` without one). A column reference or
//!   subquery is refused at CREATE; an unknown function only
//!   when an INSERT needs the default. ALTER TABLE ADD COLUMN
//!   takes only (signed / CAST) literals.
//! - Generated columns (ungated; they used to read as NULL):
//!   VIRTUAL computed on read and absent from the record, STORED
//!   computed on write — the pin's record layout, so files
//!   interoperate both ways; writes naming them refused;
//!   `SELECT *` includes them; indexes on them; UPDATE
//!   recomputes; the pin's definition errors (loops, DEFAULT,
//!   PRIMARY KEY, `rowid`, qualified names, subqueries,
//!   aggregates).
//! - UPDATE ... FROM applies only the LAST matching combination
//!   per target row (ungated fix: constraint checks used to fire
//!   on the earlier combinations too).
//! - INTEGER PRIMARY KEY / rowid values convert like the pin's
//!   MustBeInt: `'8'`, `9.0` are rowids (ungated fix).
//! - Under `v16`: INSERT ... DEFAULT VALUES, CREATE TABLE ... AS
//!   SELECT (names and declared types byte-identical in
//!   `sqlite_schema.sql`), and STRICT tables.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str)
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    while st.step().await.expect("step") == Step::Row {}
}

/// Step a statement to completion, collecting its rows in the
/// CLI's list rendering (the corpora keep reals to values whose
/// Rust and SQLite renderings agree), or the error.
async fn try_collect<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await?;
    let mut rows = Vec::new();
    while q.step().await? == Step::Row {
        let mut line = String::new();
        for i in 0..q.column_count() {
            if i > 0 {
                line.push('|');
            }
            match q.column_value(i) {
                Some(hipp::Value::Null) | None => {}
                Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
                Some(hipp::Value::Real(f)) => line.push_str(&format!("{f:?}")),
                Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                    line.push_str(&String::from_utf8_lossy(b));
                }
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

/// Run one batch through the oracle CLI: (success, stdout
/// lines). Only success and stdout are compared — the local
/// stand-in oracle's error text is not the pin's.
fn oracle_raw(path: &std::path::Path, sql: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    open_with(path, Config::default()).await
}

async fn open_with(path: &std::path::Path, cfg: Config) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), cfg)
        .await
        .expect("open")
}

/// Twin databases: the setup batch is run by hipp into one
/// file and by the oracle into the other.
struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16td-{tag}-h"));
    let oracle = common::temp_db(&format!("v16td-{tag}-o"));
    let tw = Twins { hipp, oracle };
    for stmt in setup.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            tw.same(stmt);
        }
    }
    tw
}

impl Twins {
    /// One statement on both twins: equal rows (order
    /// included), or both refuse it. Returns hipp's error when
    /// both refused.
    fn same(&self, sql: &str) -> Option<Error<FsError>> {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        });
        let (ok, want) = oracle_raw(&self.oracle, sql);
        match got {
            Ok(rows) => {
                assert!(ok, "oracle refused what hipp ran: {sql}\nhipp: {rows:?}");
                assert_eq!(rows, want, "statement: {sql}");
                None
            }
            Err(e) => {
                assert!(
                    !ok,
                    "hipp refused what the oracle ran: {sql}\nhipp: {e:?}\noracle: {want:?}"
                );
                Some(e)
            }
        }
    }

    fn all(&self, sqls: &[&str]) {
        for sql in sqls {
            self.same(sql);
        }
    }

    /// Both engines refuse `sql`; hipp with schema error `kind`.
    fn refuse(&self, sql: &str, kind: SchemaKind) {
        match self.same(sql) {
            Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "statement: {sql}"),
            other => panic!("expected {kind:?} for {sql}, got {other:?}"),
        }
    }

    /// Both engines refuse `sql` (any error).
    fn refuse_any(&self, sql: &str) {
        assert!(self.same(sql).is_some(), "expected an error for {sql}");
    }

    /// The oracle reads the hipp file clean and both twins hold
    /// the same rows for `dump`.
    fn finish(&self, dump: &str) {
        assert_eq!(oracle_raw(&self.hipp, "PRAGMA integrity_check;").1, ["ok"]);
        assert_eq!(oracle_raw(&self.hipp, dump), oracle_raw(&self.oracle, dump));
    }
}

// ---------------- CHECK constraints ----------------

#[test]
fn check_enforced_with_the_pins_truth_rules() {
    let tw = twins(
        "chk-basic",
        "CREATE TABLE t(a CHECK(a > 0), b TEXT, CONSTRAINT lt CHECK(length(b) < 4), CHECK(a < 100))",
    );
    tw.all(&[
        "INSERT INTO t VALUES (1, 'x')",
        // NULL passes; 'x' > 0 is true (text sorts above numbers).
        "INSERT INTO t VALUES (NULL, 'n')",
        "INSERT INTO t VALUES (NULL, NULL)",
        "INSERT INTO t VALUES (99, 'abc')",
    ]);
    for sql in [
        // 'x' > 0 holds (text sorts above numbers), 'x' < 100 not.
        "INSERT INTO t VALUES ('x', NULL)",
        "INSERT INTO t VALUES (-1, 'x')",
        "INSERT INTO t VALUES (0, 'x')",
        "INSERT INTO t VALUES (100, 'x')",
        "INSERT INTO t VALUES (5, 'long')",
        // Multi-row: atomic.
        "INSERT INTO t VALUES (2, 'a'), (3, 'bbbb'), (4, 'c')",
        "INSERT OR REPLACE INTO t VALUES (-5, 'r')",
        "INSERT OR ABORT INTO t VALUES (-5, 'r')",
        "INSERT OR FAIL INTO t VALUES (-5, 'r')",
        "INSERT OR ROLLBACK INTO t VALUES (-5, 'r')",
        "UPDATE t SET a = -a",
        "UPDATE OR REPLACE t SET b = 'toolong'",
    ] {
        tw.refuse(sql, SchemaKind::CheckViolation);
    }
    tw.all(&[
        "INSERT OR IGNORE INTO t VALUES (-1, 'x'), (7, 'ok'), (8, 'long')",
        "UPDATE OR IGNORE t SET a = a * 10",
        "SELECT rowid, a, b FROM t ORDER BY rowid",
    ]);
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn check_sees_affinity_and_truthiness() {
    let tw = twins(
        "chk-aff",
        "CREATE TABLE u(a INTEGER CHECK(a < '5'), b CHECK(b), c REAL CHECK(typeof(c) = 'real'))",
    );
    tw.all(&[
        // '3' is the integer 3 by the time the CHECK runs.
        "INSERT INTO u VALUES ('3', 1, 1)",
        "INSERT INTO u VALUES (3, '1abc', 2.5)",
        "INSERT INTO u VALUES (4, 0.5, '7')",
        "INSERT OR IGNORE INTO u VALUES ('7', 1, 1)",
        "INSERT OR IGNORE INTO u VALUES (1, 0, 1)",
        "INSERT OR IGNORE INTO u VALUES (1, 'abc', 1)",
        "INSERT OR IGNORE INTO u VALUES (1, x'00', 1)",
        "INSERT OR IGNORE INTO u VALUES (1, NULL, 'q')",
        "SELECT a, typeof(a), b, c, typeof(c) FROM u ORDER BY rowid",
    ]);
    tw.finish("SELECT rowid, * FROM u ORDER BY rowid");
}

#[test]
fn check_reads_the_final_rowid() {
    let tw = twins(
        "chk-rowid",
        "CREATE TABLE t(a INTEGER PRIMARY KEY CHECK(a > 5), b);\
         CREATE TABLE u(a CHECK(rowid <> 3), b)",
    );
    tw.refuse(
        "INSERT INTO t(b) VALUES (1)", // auto rowid 1 fails a > 5
        SchemaKind::CheckViolation,
    );
    tw.all(&[
        "INSERT INTO t VALUES (6, 1)",
        "INSERT INTO t(b) VALUES (2)",
        "INSERT INTO u VALUES (1, 1), (2, 2)",
        "INSERT OR IGNORE INTO u VALUES (3, 3)",
        "INSERT OR IGNORE INTO u(rowid, a) VALUES (3, 3)",
        "INSERT INTO u(rowid, a, b) VALUES (4, 4, 4)",
        "SELECT rowid, * FROM u",
        "SELECT * FROM t",
    ]);
    tw.refuse(
        "UPDATE u SET rowid = 3 WHERE a = 4",
        SchemaKind::CheckViolation,
    );
    tw.refuse("UPDATE t SET a = 1 WHERE a = 6", SchemaKind::CheckViolation);
    tw.finish("SELECT rowid, * FROM u ORDER BY rowid");
}

#[test]
fn check_precedence_against_not_null_and_unique() {
    let tw = twins(
        "chk-prec",
        "CREATE TABLE t(a NOT NULL CHECK(a > 0), b UNIQUE CHECK(b < 10));\
         INSERT INTO t VALUES (1, 1)",
    );
    // NOT NULL before CHECK; CHECK before UNIQUE.
    tw.refuse(
        "INSERT INTO t VALUES (NULL, 1)",
        SchemaKind::NotNullViolation,
    );
    tw.refuse("INSERT INTO t VALUES (-1, 1)", SchemaKind::CheckViolation);
    tw.refuse("INSERT INTO t VALUES (2, 1)", SchemaKind::UniqueViolation);
    tw.refuse("INSERT INTO t VALUES (2, 20)", SchemaKind::CheckViolation);
    tw.refuse(
        "INSERT OR REPLACE INTO t VALUES (-1, 1)",
        SchemaKind::CheckViolation,
    );
    tw.all(&[
        "INSERT OR IGNORE INTO t VALUES (-1, 1)",
        "INSERT OR IGNORE INTO t VALUES (-1, 5)",
        // REPLACE resolves the UNIQUE conflict once CHECK passes.
        "INSERT OR REPLACE INTO t VALUES (3, 1)",
        "INSERT INTO t VALUES (4, 4), (5, 5)",
        "UPDATE OR IGNORE t SET b = b + 5",
        "UPDATE OR IGNORE t SET a = -5 WHERE b = 6",
        "SELECT * FROM t ORDER BY rowid",
    ]);
    tw.refuse(
        "UPDATE OR REPLACE t SET b = 20 WHERE a = 3",
        SchemaKind::CheckViolation,
    );
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn check_on_upsert_replace_and_insert_select() {
    let tw = twins(
        "chk-upsert",
        "CREATE TABLE t(k PRIMARY KEY, v CHECK(v < 10));\
         INSERT INTO t VALUES (1, 1);\
         CREATE TABLE s(x);\
         INSERT INTO s VALUES (5), (50), (6)",
    );
    // The DO UPDATE row errors whatever the statement's action.
    tw.refuse(
        "INSERT INTO t VALUES (1, 5) ON CONFLICT(k) DO UPDATE SET v = excluded.v + 20",
        SchemaKind::CheckViolation,
    );
    tw.refuse(
        "INSERT OR IGNORE INTO t VALUES (1, 5) ON CONFLICT(k) DO UPDATE SET v = excluded.v + 20",
        SchemaKind::CheckViolation,
    );
    // The proposal is checked before arbitration: an error, or
    // under OR IGNORE a skip (the DO UPDATE never runs).
    tw.refuse(
        "INSERT INTO t VALUES (1, 50) ON CONFLICT(k) DO UPDATE SET v = 2",
        SchemaKind::CheckViolation,
    );
    tw.all(&[
        "INSERT OR IGNORE INTO t VALUES (1, 50) ON CONFLICT(k) DO UPDATE SET v = 3",
        "INSERT INTO t VALUES (1, 4) ON CONFLICT(k) DO UPDATE SET v = excluded.v + 1",
        "SELECT * FROM t",
        "REPLACE INTO t VALUES (1, 9)",
    ]);
    tw.refuse("REPLACE INTO t VALUES (1, 99)", SchemaKind::CheckViolation);
    tw.refuse(
        "INSERT INTO t SELECT x + 10, x FROM s",
        SchemaKind::CheckViolation,
    );
    tw.all(&[
        "INSERT OR IGNORE INTO t SELECT x + 10, x FROM s",
        "SELECT * FROM t ORDER BY k",
    ]);
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn update_from_applies_the_last_combination_only() {
    // Regression (found probing CHECK): the pin materialises
    // UPDATE ... FROM keyed by target rowid, so only the last
    // matching combination updates a row and only it is checked.
    let tw = twins(
        "uf-last",
        "CREATE TABLE t(k, v CHECK(v < 10));\
         INSERT INTO t VALUES (1, 1), (2, 2);\
         CREATE TABLE n(k, v NOT NULL);\
         INSERT INTO n VALUES (1, 1);\
         CREATE TABLE s(x)",
    );
    tw.all(&[
        "INSERT INTO s VALUES (5), (50), (6)",
        "UPDATE t SET v = s.x FROM s",
        "SELECT * FROM t",
        "UPDATE t SET v = s.x - 3 FROM s WHERE s.x < 10 RETURNING k, v",
        "DELETE FROM s",
        "INSERT INTO s VALUES (5), (6), (50)",
    ]);
    tw.refuse("UPDATE t SET v = s.x FROM s", SchemaKind::CheckViolation);
    tw.all(&[
        "UPDATE OR IGNORE t SET v = s.x FROM s",
        "SELECT * FROM t",
        "DELETE FROM s",
        "INSERT INTO s VALUES (NULL), (7)",
        "UPDATE n SET v = s.x FROM s",
        "SELECT * FROM n",
        "DELETE FROM s",
        "INSERT INTO s VALUES (7), (NULL)",
        "UPDATE OR IGNORE n SET v = s.x FROM s",
        "SELECT * FROM n",
    ]);
    tw.refuse("UPDATE n SET v = s.x FROM s", SchemaKind::NotNullViolation);
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn check_definition_errors_agree() {
    let tw = twins("chk-def", "CREATE TABLE base(a)");
    tw.refuse(
        "CREATE TABLE t1(a CHECK((SELECT 1)))",
        SchemaKind::SubqueryInConstraint,
    );
    tw.refuse(
        "CREATE TABLE t2(a CHECK(count(*) > 0))",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse(
        "CREATE TABLE t3(a CHECK(zz > 0))",
        SchemaKind::UnknownColumn,
    );
    tw.refuse(
        "CREATE TABLE t5(a CHECK(x.a > 0))",
        SchemaKind::UnknownColumn,
    );
    // An unknown function is refused at CREATE (not deferred).
    tw.refuse_any("CREATE TABLE t8(a CHECK(nofunc(a)))");
    tw.refuse_any("CREATE TABLE t9(a CHECK)");
    tw.all(&[
        "CREATE TABLE t4(a CHECK(t4.a > 0), b CHECK(rowid > 0))",
        "INSERT INTO t4 VALUES (2, 1)",
        "SELECT * FROM t4",
    ]);
    tw.refuse("INSERT INTO t4 VALUES (-2, 1)", SchemaKind::CheckViolation);
    tw.finish("SELECT * FROM t4");
}

#[test]
fn check_in_oracle_written_file_is_enforced() {
    // A file whose CHECK the oracle wrote: hipp enforces it.
    let path = common::temp_db("v16td-chk-foreign");
    common::cli(
        &path,
        "CREATE TABLE t(a CHECK(a BETWEEN 1 AND 9), b DEFAULT (2*21)); INSERT INTO t(a) VALUES (1);",
    );
    block_on(async {
        let mut db = open(&path).await;
        let e = try_collect(&mut db, "INSERT INTO t(a) VALUES (10)").await;
        assert!(matches!(
            e,
            Err(Error::Schema {
                kind: SchemaKind::CheckViolation
            })
        ));
        exec(&mut db, "INSERT INTO t(a) VALUES (2)").await;
        assert_eq!(
            try_collect(&mut db, "SELECT a, b FROM t").await.unwrap(),
            ["1|42", "2|42"]
        );
    });
    assert_eq!(oracle_raw(&path, "SELECT a, b FROM t").1, ["1|42", "2|42"]);
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
}

// ---------------- DEFAULT values ----------------

#[test]
fn default_forms_match_the_pin() {
    let tw = twins(
        "def-forms",
        "CREATE TABLE t(a DEFAULT (1+2), b DEFAULT -1, c DEFAULT +7, d DEFAULT TRUE, \
         e DEFAULT FALSE, f DEFAULT 'x', g DEFAULT x'0a', h DEFAULT hello, i DEFAULT (upper('q')), \
         j INTEGER DEFAULT '7', k DEFAULT -'3', l DEFAULT (NULL), m DEFAULT ('a'||'b'), \
         n DEFAULT (abs(-4)), o DEFAULT \"dq\", p DEFAULT -x'01', q DEFAULT -NULL, \
         r DEFAULT +'abc', s DEFAULT -'1.5', u INT DEFAULT +'7', v DEFAULT (CAST('5' AS INTEGER)), \
         w DEFAULT (CASE WHEN 1 THEN 'y' END), x DEFAULT 0x10, y DEFAULT -0x10, \
         z DEFAULT (10 / 4), zz)",
    );
    tw.all(&[
        "INSERT INTO t(zz) VALUES (0)",
        "INSERT INTO t(zz) VALUES (1), (2)",
        "SELECT a, typeof(a), b, c, d, e, f, typeof(g), hex(g), h, typeof(h), i, j, typeof(j), \
         k, typeof(k), l, m, n, o, typeof(p), p, q, r, typeof(r), s, typeof(s), u, typeof(u), \
         v, typeof(v), w, x, y, z FROM t",
        // An explicit value always wins over the default.
        "INSERT INTO t(a, k, zz) VALUES ('given', NULL, 3)",
        "SELECT a, k, zz FROM t WHERE zz = 3",
    ]);
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn default_errors_agree() {
    let tw = twins("def-err", "CREATE TABLE base(a)");
    for sql in [
        "CREATE TABLE t2(a DEFAULT (zz))",
        "CREATE TABLE t3(a DEFAULT ((SELECT 1)))",
        "CREATE TABLE t12(a DEFAULT (b), b)",
        "CREATE TABLE t13(a DEFAULT (base.a))",
    ] {
        tw.refuse(sql, SchemaKind::NotConstantDefault);
    }
    for sql in [
        "CREATE TABLE t10(a DEFAULT 1+2)",
        "CREATE TABLE t16(a DEFAULT -TRUE)",
        "CREATE TABLE t17(a DEFAULT)",
    ] {
        tw.refuse_any(sql);
    }
    // An unknown function or an aggregate is accepted by CREATE
    // and errors only when an INSERT needs the default.
    tw.all(&[
        "CREATE TABLE t5(a DEFAULT (nofunc()), z)",
        "CREATE TABLE t11(a DEFAULT (count(*)), z)",
        "INSERT INTO t5(a, z) VALUES (1, 1)",
        "INSERT INTO t11 VALUES (2, 2)",
        "SELECT * FROM t5",
        "SELECT * FROM t11",
    ]);
    tw.refuse_any("INSERT INTO t5(z) VALUES (1)");
    tw.refuse_any("INSERT INTO t11(z) VALUES (1)");
    tw.finish("SELECT * FROM t5");
}

#[test]
fn default_substitutes_under_or_replace() {
    let tw = twins(
        "def-replace",
        "CREATE TABLE t(a NOT NULL DEFAULT (2*3), b NOT NULL DEFAULT -'4', c)",
    );
    tw.all(&[
        "INSERT OR REPLACE INTO t VALUES (NULL, NULL, 1)",
        "INSERT INTO t VALUES (1, 1, 2)",
        "UPDATE OR REPLACE t SET a = NULL, b = NULL WHERE c = 2",
        "SELECT a, typeof(a), b, typeof(b), c FROM t ORDER BY c",
    ]);
    tw.refuse(
        "INSERT INTO t VALUES (NULL, 1, 3)",
        SchemaKind::NotNullViolation,
    );
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

fn fixed_clock() -> i64 {
    // 2023-11-14 22:13:20.123 UTC.
    1_700_000_000_123
}

fn epoch_clock() -> i64 {
    -1
}

#[test]
fn clock_defaults_read_the_config_hook() {
    let path = common::temp_db("v16td-clock");
    let probe = common::temp_db("v16td-clock-probe");
    let want = oracle_raw(
        &probe,
        "SELECT time(1700000000, 'unixepoch'), date(1700000000, 'unixepoch'), \
         datetime(1700000000, 'unixepoch');",
    )
    .1;
    let before = oracle_raw(
        &probe,
        "SELECT time(-1, 'unixepoch'), date(-1, 'unixepoch'), datetime(-1, 'unixepoch');",
    )
    .1;
    block_on(async {
        let mut db = open_with(&path, Config::default().clock(fixed_clock)).await;
        exec(
            &mut db,
            "CREATE TABLE t(a DEFAULT CURRENT_TIME, b DEFAULT CURRENT_DATE, \
             c DEFAULT CURRENT_TIMESTAMP, d DEFAULT (current_timestamp || '!'), z)",
        )
        .await;
        exec(&mut db, "INSERT INTO t(z) VALUES (1), (2)").await;
        let rows = try_collect(&mut db, "SELECT a, b, c, d FROM t")
            .await
            .unwrap();
        let parts: Vec<&str> = want[0].split('|').collect();
        let row = format!("{}|{}|{}|{}!", parts[0], parts[1], parts[2], parts[2]);
        assert_eq!(rows, [row.clone(), row]);
        // A supplied value needs no clock.
        drop(db);
        let mut db = open(&path).await;
        exec(&mut db, "INSERT INTO t VALUES (1, 2, 3, 4, 5)").await;
        let e = try_collect(&mut db, "INSERT INTO t(z) VALUES (6)").await;
        assert!(matches!(e, Err(Error::NoClock)), "{e:?}");
        drop(db);
        let mut db = open_with(&path, Config::default().clock(epoch_clock)).await;
        exec(&mut db, "INSERT INTO t(z) VALUES (7)").await;
        let rows = try_collect(&mut db, "SELECT a, b, c FROM t WHERE z = 7")
            .await
            .unwrap();
        assert_eq!(rows, before);
    });
    // The oracle reads the clock-defaulted rows back unchanged.
    assert_eq!(oracle_raw(&path, "SELECT count(*) FROM t;").1, ["4"]);
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
    // CURRENT_* on the oracle has the same shapes.
    let tw = twins(
        "clock-shape",
        "CREATE TABLE t(a DEFAULT CURRENT_TIME, b DEFAULT CURRENT_DATE, c DEFAULT CURRENT_TIMESTAMP, z)",
    );
    let (ok, rows) = oracle_raw(
        &tw.oracle,
        "INSERT INTO t(z) VALUES (1); SELECT length(a), length(b), length(c), typeof(c) FROM t;",
    );
    assert!(ok);
    assert_eq!(rows, ["8|10|19|text"]);
}

#[test]
fn clock_values_match_the_pins_calendar() {
    // Leap days, centuries, the epoch, pre-epoch and the 9999 edge
    // against the oracle's own date arithmetic.
    let probe = common::temp_db("v16td-calendar");
    let path = common::temp_db("v16td-calendar-h");
    let mut secs: Vec<i64> = vec![
        0,
        -1,
        -86_400,
        951_782_400,     // 2000-02-29
        951_868_799,     // 2000-02-29 23:59:59
        4_107_542_400,   // 2100-03-01
        -2_208_988_800,  // 1900-01-01
        253_402_300_799, // 9999-12-31 23:59:59
        -62_135_596_800, // 0001-01-01
        1_234_567_890,
    ];
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..40 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        secs.push((rng % 253_402_300_799) as i64 - 30_000_000_000);
    }
    for s in secs {
        let want = oracle_raw(
            &probe,
            &format!(
                "SELECT time({s}, 'unixepoch'), date({s}, 'unixepoch'), datetime({s}, 'unixepoch');"
            ),
        )
        .1;
        let got = block_on(async {
            let _ = std::fs::remove_file(&path);
            // A fn pointer cannot capture: route the case's time
            // through a thread-local (999 ms: seconds truncate).
            CLOCK_SECS.with(|c| c.set(s));
            let clock: fn() -> i64 = || CLOCK_SECS.with(|c| c.get()) * 1000 + 999;
            let mut db = open_with(&path, Config::default().clock(clock)).await;
            exec(
                &mut db,
                "CREATE TABLE t(a DEFAULT CURRENT_TIME, b DEFAULT CURRENT_DATE, c DEFAULT CURRENT_TIMESTAMP, z)",
            )
            .await;
            exec(&mut db, "INSERT INTO t(z) VALUES (1)").await;
            try_collect(&mut db, "SELECT a, b, c FROM t").await.unwrap()
        });
        assert_eq!(got, want, "unix seconds {s}");
    }
}

std::thread_local! {
    static CLOCK_SECS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

#[test]
fn alter_add_column_default_rules() {
    let tw = twins(
        "alter-def",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1), (NULL)",
    );
    tw.all(&[
        "ALTER TABLE t ADD COLUMN c DEFAULT -1",
        "ALTER TABLE t ADD COLUMN f DEFAULT -'3'",
        "ALTER TABLE t ADD COLUMN g DEFAULT hello",
        "ALTER TABLE t ADD COLUMN h DEFAULT (5)",
        "ALTER TABLE t ADD COLUMN i DEFAULT (CAST(7 AS TEXT))",
        "ALTER TABLE t ADD COLUMN j DEFAULT -x'01'",
        "ALTER TABLE t ADD COLUMN k CHECK(k > 0)",
        "ALTER TABLE t ADD COLUMN q DEFAULT x'01'",
        "SELECT a, c, f, typeof(f), g, h, i, typeof(i), j, typeof(j), k, hex(q) FROM t",
        "INSERT INTO t(a) VALUES (2)",
        "SELECT a, c, f, g, h, i, j, k, hex(q) FROM t ORDER BY rowid",
    ]);
    for sql in [
        "ALTER TABLE t ADD COLUMN b DEFAULT (1+2)",
        "ALTER TABLE t ADD COLUMN d DEFAULT CURRENT_TIMESTAMP",
        "ALTER TABLE t ADD COLUMN e DEFAULT (abs(-3))",
        "ALTER TABLE t ADD COLUMN m NOT NULL DEFAULT (NULL)",
        "ALTER TABLE t ADD COLUMN n NOT NULL",
        "ALTER TABLE t ADD COLUMN s AS (a * 2) STORED",
        "ALTER TABLE t ADD COLUMN u UNIQUE",
    ] {
        tw.refuse(sql, SchemaKind::CannotAddColumn);
    }
    // Existing rows are verified against a new CHECK (the pin's
    // post-ADD quick_check): the default 5 fails `l > 10`.
    tw.refuse(
        "ALTER TABLE t ADD COLUMN l DEFAULT 5 CHECK(l > 10)",
        SchemaKind::CheckViolation,
    );
    // ... and a generated NOT NULL column against every row.
    tw.refuse(
        "ALTER TABLE t ADD COLUMN p AS (a * 2) NOT NULL",
        SchemaKind::NotNullViolation,
    );
    tw.all(&[
        "ALTER TABLE t ADD COLUMN o AS (a * 2)",
        "SELECT a, o FROM t ORDER BY rowid",
        "UPDATE t SET a = 10 WHERE a IS NULL",
        "ALTER TABLE t ADD COLUMN p AS (a * 2) NOT NULL",
        "SELECT a, o, p FROM t ORDER BY rowid",
    ]);
    tw.finish("SELECT rowid, a, c, f, g, h, i, j, k, hex(q), o, p FROM t ORDER BY rowid");
    // The rewritten schema text matches the pin's.
    let q = "SELECT sql FROM sqlite_schema";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
    // An empty table takes a new failing CHECK (nothing to verify).
    let tw = twins("alter-def-empty", "CREATE TABLE t(a)");
    tw.all(&["ALTER TABLE t ADD COLUMN l DEFAULT 5 CHECK(l > 10)"]);
    tw.refuse("INSERT INTO t(a) VALUES (1)", SchemaKind::CheckViolation);
}

// ---------------- generated columns ----------------

const GEN: &str = "CREATE TABLE t(a, b AS (a * 2), c GENERATED ALWAYS AS (a + b) STORED, \
                   d INT AS (a || 'x') VIRTUAL, e, f REAL AS (a) STORED, g TEXT GENERATED ALWAYS AS (e))";

#[test]
fn generated_columns_compute_and_interoperate() {
    let tw = twins("gen-basic", GEN);
    tw.all(&[
        "INSERT INTO t(a, e) VALUES (3, 'e')",
        "INSERT INTO t VALUES (4, 5)",
        "INSERT INTO t(e) VALUES (NULL)",
        "INSERT INTO t(a) VALUES ('7')",
        "SELECT * FROM t",
        "SELECT typeof(b), typeof(c), typeof(d), typeof(f), typeof(g) FROM t",
        "SELECT t.*, b + c FROM t WHERE b > 5 ORDER BY c DESC",
        "SELECT x.d, x.g FROM t x WHERE x.c IS NOT NULL",
        "SELECT a, (SELECT max(c) FROM t), b FROM t ORDER BY a",
        "UPDATE t SET a = a + 10 WHERE b = 8",
        "UPDATE t SET e = 'z' WHERE a IS NULL",
        "SELECT * FROM t ORDER BY rowid",
        "DELETE FROM t WHERE c = 9 RETURNING *",
        "SELECT * FROM t ORDER BY rowid",
    ]);
    tw.refuse(
        "INSERT INTO t VALUES (1, 2, 3)",
        SchemaKind::ColumnCountMismatch,
    );
    for sql in [
        "INSERT INTO t(b) VALUES (1)",
        "INSERT INTO t(a, c) VALUES (1, 2)",
        "UPDATE t SET b = 1",
        "UPDATE t SET a = 1, g = 'q'",
    ] {
        tw.refuse(sql, SchemaKind::GeneratedColumnWrite);
    }
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

#[test]
fn generated_columns_in_oracle_written_file() {
    // The other direction: the oracle writes the file (VIRTUAL
    // columns absent from its records); hipp reads it, writes
    // more rows and updates; the oracle reads hipp's writes.
    let path = common::temp_db("v16td-gen-foreign");
    common::cli(
        &path,
        &format!(
            "{GEN}; INSERT INTO t(a, e) VALUES (1, 'one'), (2, NULL), (NULL, 'n'); \
             CREATE INDEX tb ON t(b); CREATE INDEX td ON t(d, e);"
        ),
    );
    let check = "SELECT rowid, * FROM t ORDER BY rowid; SELECT a FROM t WHERE b = 20; \
                 SELECT count(*) FROM t INDEXED BY tb WHERE b > 0; \
                 SELECT a FROM t INDEXED BY td WHERE d = '10x';";
    block_on(async {
        let mut db = open(&path).await;
        assert_eq!(
            try_collect(&mut db, "SELECT * FROM t ORDER BY rowid")
                .await
                .unwrap(),
            oracle_raw(&path, "SELECT * FROM t ORDER BY rowid").1
        );
        exec(&mut db, "INSERT INTO t(a, e) VALUES (10, 'ten')").await;
        exec(&mut db, "UPDATE t SET a = a * 3 WHERE a = 2").await;
        exec(&mut db, "DELETE FROM t WHERE a IS NULL").await;
        assert_eq!(
            try_collect(&mut db, "SELECT a, b, c, d FROM t WHERE b >= 12 ORDER BY b")
                .await
                .unwrap(),
            ["6|12|18|6x", "10|20|30|10x"]
        );
    });
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
    let mirror = common::temp_db("v16td-gen-foreign-mirror");
    common::cli(
        &mirror,
        &format!(
            "{GEN}; INSERT INTO t(a, e) VALUES (1, 'one'), (2, NULL), (NULL, 'n'); \
             CREATE INDEX tb ON t(b); CREATE INDEX td ON t(d, e); \
             INSERT INTO t(a, e) VALUES (10, 'ten'); UPDATE t SET a = a * 3 WHERE a = 2; \
             DELETE FROM t WHERE a IS NULL;"
        ),
    );
    assert_eq!(oracle_raw(&path, check), oracle_raw(&mirror, check));
}

#[test]
fn generated_columns_with_indexes_upsert_and_replace() {
    let tw = twins(
        "gen-index",
        "CREATE TABLE t(k PRIMARY KEY, a, b AS (a * 2), c AS (a + 1) STORED, \
         d AS (b + c) UNIQUE);\
         CREATE INDEX ib ON t(b);\
         CREATE UNIQUE INDEX ic ON t(c)",
    );
    tw.all(&[
        "INSERT INTO t(k, a) VALUES (1, 1), (2, 2), (3, 3)",
        "SELECT k FROM t WHERE b = 4",
        "INSERT OR IGNORE INTO t(k, a) VALUES (4, 1)",
        "INSERT INTO t(k, a) VALUES (1, 9) ON CONFLICT(k) DO UPDATE SET a = excluded.a + 100",
        "INSERT INTO t(k, a) VALUES (2, 9) ON CONFLICT(k) DO UPDATE SET a = excluded.b",
        "SELECT * FROM t ORDER BY k",
        "INSERT INTO t(k, a) VALUES (7, 3) ON CONFLICT DO NOTHING",
        "INSERT OR REPLACE INTO t(k, a) VALUES (5, 3)",
        "SELECT * FROM t ORDER BY k",
        "UPDATE t SET a = a + 1 RETURNING k, b, c, d",
        "UPDATE OR REPLACE t SET a = 110 WHERE k = 5",
        "SELECT * FROM t ORDER BY k",
        "CREATE INDEX idd ON t(d DESC, b)",
        "SELECT k, d FROM t WHERE d > 10 ORDER BY d",
    ]);
    tw.refuse(
        "INSERT INTO t(k, a) VALUES (8, 19)", // c = 20 collides
        SchemaKind::UniqueViolation,
    );
    tw.refuse(
        "INSERT INTO t(k, a) VALUES (3, 9) ON CONFLICT(k) DO UPDATE SET b = 1",
        SchemaKind::GeneratedColumnWrite,
    );
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
    let q = "SELECT k FROM t INDEXED BY ib WHERE b > 0 ORDER BY b";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
}

#[test]
fn generated_column_rules_and_order() {
    let tw = twins("gen-rules", "CREATE TABLE base(a)");
    tw.all(&[
        // Dependency order, not column order.
        "CREATE TABLE g1(a, b AS (c + 1), c AS (a * 2), d AS (b || c) STORED)",
        "INSERT INTO g1(a) VALUES (5), (NULL)",
        "SELECT * FROM g1",
        // A rowid alias reads the new rowid.
        "CREATE TABLE g10(a INTEGER PRIMARY KEY, b AS (a * 10))",
        "INSERT INTO g10 VALUES (NULL)",
        "INSERT INTO g10 VALUES (7)",
        "SELECT * FROM g10",
        "CREATE TABLE g12(a, b AS (a) UNIQUE)",
        "INSERT INTO g12 VALUES (1)",
        "INSERT OR IGNORE INTO g12 VALUES (1)",
        "CREATE TABLE g13(a, b AS (a) NOT NULL)",
        "INSERT OR IGNORE INTO g13 VALUES (NULL)",
        "INSERT OR IGNORE INTO g13 VALUES (2)",
        "SELECT * FROM g13",
        "CREATE TABLE g14(a, b AS (a) CHECK(b > 0))",
        "INSERT OR IGNORE INTO g14 VALUES (0)",
        "INSERT INTO g14 VALUES (1)",
        "SELECT * FROM g14",
        "CREATE TABLE g16(a, b AS (1) STORED, c AS (2))",
        "INSERT INTO g16 VALUES (0)",
        "SELECT * FROM g16",
        "CREATE TABLE g19(a, b TEXT GENERATED ALWAYS AS (a) COLLATE nocase)",
        // `GENERATED` alone is a type name: type GENERATED
        // (NUMERIC affinity), then the short `AS (...)` form.
        "CREATE TABLE g20(a, b GENERATED AS (a))",
        "INSERT INTO g20 VALUES ('7')",
        "SELECT b, typeof(b) FROM g20",
        "CREATE TABLE s(x, y)",
        "INSERT INTO s VALUES (1, 2)",
        "CREATE TABLE g30(k, a, b AS (a * 2))",
        "INSERT INTO g30 SELECT x + 20, y FROM s",
        "INSERT INTO g30(k, a) SELECT y, x FROM s",
        "SELECT * FROM g30",
    ]);
    tw.refuse("INSERT INTO g12 VALUES (1)", SchemaKind::UniqueViolation);
    tw.refuse(
        "INSERT INTO g13 VALUES (NULL)",
        SchemaKind::NotNullViolation,
    );
    tw.refuse(
        "INSERT OR REPLACE INTO g13 VALUES (NULL)",
        SchemaKind::NotNullViolation,
    );
    tw.refuse("INSERT INTO g14 VALUES (-1)", SchemaKind::CheckViolation);
    tw.refuse(
        "CREATE TABLE g2(a, b AS (b + 1))",
        SchemaKind::GeneratedColumnLoop,
    );
    tw.refuse(
        "CREATE TABLE g3(a, b AS (c), c AS (d), d AS (b))",
        SchemaKind::GeneratedColumnLoop,
    );
    tw.refuse(
        "CREATE TABLE g4(a, b AS ((SELECT 1)))",
        SchemaKind::SubqueryInConstraint,
    );
    tw.refuse(
        "CREATE TABLE g6(a, b AS (count(*)))",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse("CREATE TABLE g7(a, b AS (zz))", SchemaKind::UnknownColumn);
    tw.refuse(
        "CREATE TABLE g11(a, b AS (rowid))",
        SchemaKind::UnknownColumn,
    );
    for sql in [
        "CREATE TABLE g8(a, b AS (a) DEFAULT 5)",
        "CREATE TABLE g9(a, b AS (a) PRIMARY KEY)",
        "CREATE TABLE g9b(a, b AS (a), PRIMARY KEY(b))",
        "CREATE TABLE g15(b AS (1))",
        "CREATE TABLE g22(a, b AS (g22.a))",
    ] {
        tw.refuse(sql, SchemaKind::InvalidGeneratedColumn);
    }
    for sql in [
        "CREATE TABLE g17(a, b AS (nofunc(a)))",
        "CREATE TABLE g18(a, b GENERATED ALWAYS AS (a) VIRTUAL STORED)",
        "CREATE TABLE g20b(a, b GENERATED ALWAYS (a))",
        "CREATE TABLE g21(a, b AS a)",
    ] {
        tw.refuse_any(sql);
    }
    tw.finish("SELECT * FROM g1");
}

#[test]
fn rowid_values_convert_like_must_be_int() {
    let tw = twins("rowid-conv", "CREATE TABLE t(a INTEGER PRIMARY KEY, b)");
    tw.all(&[
        "INSERT INTO t VALUES ('8', 1)",
        "INSERT INTO t VALUES (9.0, 1)",
        "INSERT INTO t VALUES (' 10 ', 1)",
        "INSERT INTO t VALUES ('1e2', 1)",
        "INSERT INTO t(rowid, b) VALUES ('11', 2)",
        "UPDATE t SET a = '5' WHERE a = 8",
        "UPDATE t SET rowid = 7.0 WHERE a = 9",
        "SELECT rowid, * FROM t",
    ]);
    for sql in [
        "INSERT INTO t VALUES ('8.5', 1)",
        "INSERT INTO t VALUES ('abc', 1)",
        "INSERT INTO t VALUES (x'01', 1)",
        "UPDATE t SET a = 'x'",
        "UPDATE t SET a = NULL",
    ] {
        tw.refuse(sql, SchemaKind::DatatypeMismatch);
    }
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

/// A deterministic PRNG corpus: random tables mixing CHECK,
/// generated columns (VIRTUAL / STORED), expression and literal
/// DEFAULTs and NOT NULL, under random INSERT / UPDATE / upsert
/// statements with every conflict action — each statement must
/// agree on success vs failure and on the rows it returns.
#[test]
fn prng_constraint_corpus() {
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
    let mut rng = Rng(0x5eed_0016_c0de_d0e5);
    for round in 0..6 {
        let gen_kind = rng.pick(&["", " STORED", " VIRTUAL"]);
        let lim = 3 + rng.next() % 6;
        let setup = format!(
            "CREATE TABLE t(k INTEGER PRIMARY KEY, a INT {nn} CHECK(a IS NULL OR a < {lim}), \
             b TEXT DEFAULT {bdef}, c AS (coalesce(a, 0) * 2 + length(b)){gen_kind}, \
             d DEFAULT (1 + 1) CHECK(d <> 'bad'), UNIQUE(b))",
            nn = rng.pick(&["", "NOT NULL DEFAULT 0"]),
            bdef = rng.pick(&["'x'", "(upper('q') || 'r')", "-'7'", "NULL"]),
        );
        let tw = twins(&format!("prng-{round}"), &setup);
        let vals = [
            "NULL", "0", "1", "2", "5", "9", "'3'", "'x'", "'bad'", "2.0", "-1",
        ];
        for _ in 0..40 {
            let a = rng.pick(&vals);
            let b = rng.pick(&vals);
            let d = rng.pick(&vals);
            let act = rng.pick(&["", " OR IGNORE", " OR REPLACE", " OR ABORT"]);
            let sql = match rng.next() % 6 {
                0 => format!("INSERT{act} INTO t(a, b, d) VALUES ({a}, {b}, {d})"),
                1 => format!("INSERT{act} INTO t(a) VALUES ({a})"),
                2 => format!("UPDATE{act} t SET a = {a} WHERE c > {}", rng.next() % 9),
                3 => format!(
                    "UPDATE{act} t SET b = {b}, d = {d} WHERE k % 2 = {}",
                    rng.next() % 2
                ),
                4 => format!(
                    "INSERT INTO t(k, a, b) VALUES ({}, {a}, {b}) \
                     ON CONFLICT DO UPDATE SET a = excluded.a, d = {d}",
                    1 + rng.next() % 4
                ),
                _ => format!("DELETE FROM t WHERE c = {} RETURNING k, c", rng.next() % 9),
            };
            tw.same(&sql);
        }
        tw.same("SELECT * FROM t ORDER BY k");
        tw.finish("SELECT k, a, b, c, d FROM t ORDER BY k");
    }
}

// ---------------- v16: DEFAULT VALUES, CTAS, STRICT ----------------

#[cfg(feature = "v16")]
#[test]
fn insert_default_values() {
    let tw = twins(
        "dv",
        "CREATE TABLE t(a DEFAULT 5, b, c INTEGER PRIMARY KEY, d AS (a + 1), e DEFAULT (2 * 3));\
         CREATE TABLE u(a NOT NULL);\
         CREATE TABLE w(a UNIQUE DEFAULT 1)",
    );
    tw.all(&[
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t DEFAULT VALUES",
        "SELECT * FROM t",
        "INSERT INTO t DEFAULT VALUES RETURNING *",
        "INSERT OR IGNORE INTO t DEFAULT VALUES",
        "REPLACE INTO t DEFAULT VALUES",
        "SELECT count(*) FROM t",
        "INSERT OR IGNORE INTO u DEFAULT VALUES",
        "INSERT INTO w DEFAULT VALUES",
        "INSERT OR IGNORE INTO w DEFAULT VALUES",
        "SELECT count(*) FROM w",
    ]);
    tw.refuse("INSERT INTO u DEFAULT VALUES", SchemaKind::NotNullViolation);
    tw.refuse("INSERT INTO w DEFAULT VALUES", SchemaKind::UniqueViolation);
    tw.refuse_any("INSERT INTO w DEFAULT VALUES ON CONFLICT DO NOTHING");
    tw.refuse_any("INSERT INTO t(a) DEFAULT VALUES");
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

/// CREATE TABLE ... AS SELECT: run on both twins, then compare
/// `sqlite_schema` (read by the oracle from both files) byte for
/// byte, plus the contents.
#[cfg(feature = "v16")]
fn ctas(tag: &str, setup: &str, stmts: &[&str], dump: &str) {
    let tw = twins(tag, setup);
    tw.all(stmts);
    let q = "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY rowid";
    let (ok, want) = oracle_raw(&tw.oracle, q);
    assert!(ok);
    assert_eq!(oracle_raw(&tw.hipp, q).1, want, "{tag}");
    tw.finish(dump);
    // hipp reads its own CTAS tables like the oracle does.
    tw.same(dump);
}

#[cfg(feature = "v16")]
#[test]
fn create_table_as_select_names_and_types() {
    ctas(
        "ctas-types",
        "CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB, e, f NUMERIC, g VARCHAR(10), \
         h INT PRIMARY KEY, i DOUBLE, \"my col\" int);\
         INSERT INTO t VALUES (1, 'x', 1.5, x'41', NULL, 5, 'g', 1, 2.0, 3)",
        &[
            "CREATE TABLE u1 AS SELECT * FROM t",
            "CREATE TABLE u2 AS SELECT a, a+1, b||'y', c*2, 1, 'lit', 2.5, x'41', NULL, \
             CAST(a AS TEXT), CAST(b AS INTEGER), CAST(a AS REAL), CAST(a AS NUMERIC), \
             CAST(a AS BLOB), CAST(a AS VARCHAR), a AS z, t.b AS \"q q\", count(*) FROM t",
            "CREATE TABLE u3 AS SELECT a, a FROM t",
            "CREATE TABLE u4 AS SELECT a AS x, b AS X, c AS x FROM t",
            "CREATE TABLE u5 AS SELECT a, a, a, a FROM t",
            "CREATE TABLE u6 AS SELECT a AS \"a:1\", a, a FROM t",
            "CREATE TABLE u7 AS SELECT \"a\", [b], `c` FROM t WHERE 0",
            "CREATE TABLE \"u 8\" AS SELECT a AS \"x\"\"y\", b AS 'z' FROM t",
            "CREATE TABLE u9 AS SELECT a AS \"order\", b AS \"select\", a AS key, b AS _x, \
             a AS \"1a\", a AS a1, a AS \"é\", a AS within FROM t",
            "CREATE TABLE aaaaaaaaaaaaaaaaaaaaaaaaaaaa AS SELECT b AS bbbbbbbbbb, b AS c FROM t",
            "CREATE TABLE aaaaaaaaaaaaaaaaaaaaaaaaaaaaa AS SELECT b AS bbbbbbbbbb, b AS c FROM t",
            "CREATE TABLE IF NOT EXISTS u1 AS SELECT 1",
        ],
        "SELECT * FROM u2",
    );
}

#[cfg(feature = "v16")]
#[test]
fn create_table_as_select_shapes() {
    ctas(
        "ctas-shapes",
        "CREATE TABLE t(a INTEGER, b TEXT, id INTEGER PRIMARY KEY);\
         INSERT INTO t VALUES (1, '2', 5), (3, '4', 9), (3, 'x', 11)",
        &[
            "CREATE TABLE c1 AS SELECT a FROM t UNION ALL SELECT b FROM t",
            "CREATE TABLE c2 AS SELECT b FROM t UNION SELECT 3",
            "CREATE TABLE c3 AS SELECT CAST(a AS INT) FROM t UNION SELECT 4",
            "CREATE TABLE c4 AS SELECT 1 UNION SELECT 'a'",
            "CREATE TABLE c5 AS VALUES (1, 2), ('a', x'00')",
            "CREATE TABLE c6 AS SELECT (SELECT a FROM t), (SELECT max(a) FROM t), rowid, oid, \
             _rowid_, id FROM t",
            "CREATE TABLE c7 AS SELECT a, count(*) AS n FROM t GROUP BY a ORDER BY a DESC",
            "CREATE TABLE c8 AS SELECT x.a, y.b FROM t x JOIN t y ON x.id = y.id LIMIT 2",
            "CREATE TABLE c9 AS SELECT * FROM (SELECT a + 1 AS p, b FROM t) WHERE p > 2",
            "CREATE TABLE c10 AS SELECT a FROM t WHERE a > 100",
        ],
        "SELECT rowid, * FROM c7",
    );
    let tw = twins(
        "ctas-err",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t",
    );
    tw.refuse("CREATE TABLE t AS SELECT 1", SchemaKind::DuplicateTable);
    tw.refuse_any("CREATE TABLE nope AS SELECT * FROM missing");
    tw.refuse_any("CREATE TABLE v AS SELECT 1");
}

#[cfg(feature = "v16")]
#[test]
fn strict_tables_enforce_types() {
    let tw = twins(
        "strict",
        "CREATE TABLE s(a INT, b INTEGER, c REAL, d TEXT, e BLOB, f ANY) STRICT",
    );
    tw.all(&[
        "INSERT INTO s VALUES (1, 2, 3.5, 't', x'01', NULL)",
        "INSERT INTO s VALUES ('12', '13', '1.5', 14, x'02', 'x')",
        "INSERT INTO s VALUES (1.0, 2.0, 3, 4.5, x'03', 5)",
        "INSERT INTO s VALUES (' 12 ', '1e2', ' 1.5', 't', x'01', '12')",
        "INSERT INTO s VALUES (NULL, NULL, NULL, NULL, NULL, x'41')",
        "SELECT a, typeof(a), b, typeof(b), c, typeof(c), d, typeof(d), typeof(e), f, typeof(f) FROM s",
        // ANY keeps text: no NUMERIC affinity in comparisons.
        "SELECT count(*) FROM s WHERE f = 12",
        "SELECT count(*) FROM s WHERE f = '12'",
        "UPDATE s SET a = '99' WHERE rowid = 1",
        "SELECT a, typeof(a) FROM s WHERE rowid = 1",
    ]);
    for sql in [
        "INSERT INTO s(a) VALUES (1.5)",
        "INSERT INTO s(a) VALUES ('abc')",
        "INSERT INTO s(a) VALUES (x'01')",
        "INSERT INTO s(b) VALUES ('9223372036854775808')",
        "INSERT INTO s(b) VALUES ('0x10')",
        "INSERT INTO s(c) VALUES ('abc')",
        "INSERT INTO s(c) VALUES (x'01')",
        "INSERT INTO s(d) VALUES (x'01')",
        "INSERT INTO s(e) VALUES ('blobtext')",
        "INSERT INTO s(e) VALUES (1)",
        // Not resolvable by any conflict action.
        "INSERT OR IGNORE INTO s(a) VALUES ('abc')",
        "INSERT OR REPLACE INTO s(a) VALUES ('abc')",
        "UPDATE s SET a = 'x'",
        "UPDATE OR IGNORE s SET e = 1",
    ] {
        tw.refuse(sql, SchemaKind::StrictTypeViolation);
    }
    tw.finish("SELECT rowid, * FROM s ORDER BY rowid");
}

#[cfg(feature = "v16")]
#[test]
fn strict_table_definitions_and_precedence() {
    let tw = twins(
        "strict-def",
        "CREATE TABLE s4(a int, b Text, c ANY, d integer primary key) STRICT;\
         CREATE TABLE p(a INT UNIQUE CHECK(typeof(a) = 'integer'), b TEXT NOT NULL) STRICT;\
         CREATE TABLE g(a INT, b INT AS (a || 'x'), c INT AS (a + 1) STORED) STRICT;\
         CREATE TABLE s9(a INT) STRICT, STRICT",
    );
    tw.all(&[
        "INSERT INTO s4(a) VALUES ('5')",
        "INSERT INTO s4 VALUES (1, 'x', '007', NULL)",
        "INSERT INTO s4 VALUES (1, 'x', '007', '8')",
        "SELECT a, typeof(a), c, typeof(c), d FROM s4",
        "INSERT INTO p VALUES (1, 'x')",
        "INSERT INTO s9 VALUES (1)",
        "SELECT * FROM s9",
    ]);
    tw.refuse(
        "INSERT INTO s4 VALUES (1, 'x', '007', 'abc')",
        SchemaKind::DatatypeMismatch,
    );
    // NOT NULL, then STRICT, then CHECK, then UNIQUE.
    tw.refuse(
        "INSERT INTO p VALUES ('q', NULL)",
        SchemaKind::NotNullViolation,
    );
    tw.refuse(
        "INSERT INTO p VALUES (1.5, 'x')",
        SchemaKind::StrictTypeViolation,
    );
    tw.refuse("INSERT INTO p VALUES (1, 2)", SchemaKind::UniqueViolation);
    // Computed values are typed too, VIRTUAL ones included.
    tw.refuse(
        "INSERT INTO g(a) VALUES (1)",
        SchemaKind::StrictTypeViolation,
    );
    for sql in [
        "CREATE TABLE s2(a) STRICT",
        "CREATE TABLE s3(a VARCHAR) STRICT",
        "CREATE TABLE s5(a INT, b AS (a) STORED) STRICT",
        "CREATE TABLE s6(a INT(10)) STRICT",
    ] {
        tw.refuse(sql, SchemaKind::StrictColumnType);
    }
    for sql in [
        "CREATE TABLE s7(a INT) STRICT STRICT",
        "CREATE TABLE s8(a INT) FOO",
        "CREATE TABLE s10(a INT) STRICT,",
    ] {
        tw.refuse_any(sql);
    }
    // WITHOUT ROWID stays cleanly refused (the pin takes these).
    block_on(async {
        let mut db = open(&tw.hipp).await;
        for sql in [
            "CREATE TABLE s11(a INT PRIMARY KEY) WITHOUT ROWID, STRICT",
            "CREATE TABLE s12(a INT PRIMARY KEY) STRICT, WITHOUT ROWID",
        ] {
            assert!(
                matches!(
                    try_collect(&mut db, sql).await,
                    Err(Error::Parse {
                        kind: hipp::error::ParseKind::UnsupportedClause,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    });
    // ALTER on a STRICT table: a DEFAULT the type rejects fails
    // only when rows exist to verify.
    tw.all(&["ALTER TABLE s9 ADD COLUMN b INT DEFAULT 7"]);
    tw.refuse(
        "ALTER TABLE s9 ADD COLUMN c INT DEFAULT 'abc'",
        SchemaKind::StrictTypeViolation,
    );
    tw.refuse_any("ALTER TABLE s9 ADD COLUMN d");
    tw.finish("SELECT rowid, * FROM s4 ORDER BY rowid");
    let q = "SELECT sql FROM sqlite_schema ORDER BY rowid";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
}

#[cfg(feature = "v16")]
#[test]
fn strict_oracle_written_file() {
    let path = common::temp_db("v16td-strict-foreign");
    common::cli(
        &path,
        "CREATE TABLE s(a INT, b ANY, c TEXT) STRICT; INSERT INTO s VALUES (1, '01', 2);",
    );
    block_on(async {
        let mut db = open(&path).await;
        assert!(matches!(
            try_collect(&mut db, "INSERT INTO s VALUES ('z', 1, 1)").await,
            Err(Error::Schema {
                kind: SchemaKind::StrictTypeViolation
            })
        ));
        exec(&mut db, "INSERT INTO s VALUES ('2', '02', 3)").await;
        assert_eq!(
            try_collect(
                &mut db,
                "SELECT a, typeof(a), b, typeof(b), c, typeof(c) FROM s"
            )
            .await
            .unwrap(),
            ["1|integer|01|text|2|text", "2|integer|02|text|3|text"]
        );
    });
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
}

#[test]
fn update_checks_only_what_it_assigns() {
    // Rows violating a CHECK can exist (the oracle writes them
    // with ignore_check_constraints). An UPDATE evaluates a CHECK
    // only when it reads an assigned column, a generated column
    // computed from one, or a changed rowid — hipp must agree.
    let setup = "CREATE TABLE t(a, b CHECK(b > 0), c AS (a + 1), d INT, CHECK(c < 100));\
                 CREATE TABLE r(a CHECK(rowid < 50), b);\
                 PRAGMA ignore_check_constraints = 1;\
                 INSERT INTO t(a, b) VALUES (1, -1), (200, 5);\
                 INSERT INTO r(rowid, a, b) VALUES (60, 1, 1);";
    let hipp_path = common::temp_db("v16td-chk-upd-h");
    let oracle_path = common::temp_db("v16td-chk-upd-o");
    common::cli(&hipp_path, setup);
    common::cli(&oracle_path, setup);
    let tw = Twins {
        hipp: hipp_path,
        oracle: oracle_path,
    };
    tw.all(&[
        "UPDATE t SET a = 2 WHERE b = -1",
        "UPDATE t SET d = 1",
        "UPDATE r SET b = 2",
        "INSERT INTO t(rowid, a, b) VALUES (1, 0, 1) ON CONFLICT DO UPDATE SET d = 7",
        "SELECT rowid, * FROM t",
        "SELECT rowid, * FROM r",
    ]);
    tw.refuse("UPDATE t SET b = b WHERE a = 2", SchemaKind::CheckViolation);
    tw.refuse(
        "UPDATE t SET a = a WHERE a = 200",
        SchemaKind::CheckViolation,
    );
    tw.refuse("UPDATE r SET rowid = 61", SchemaKind::CheckViolation);
    tw.refuse(
        "INSERT INTO t(rowid, a, b) VALUES (1, 0, 1) ON CONFLICT DO UPDATE SET b = b",
        SchemaKind::CheckViolation,
    );
    // The violating rows are still there on both: integrity_check
    // reports the same CHECK failures for both files.
    for q in [
        "PRAGMA integrity_check;",
        "SELECT rowid, * FROM t ORDER BY rowid; SELECT rowid, * FROM r;",
    ] {
        assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q), "{q}");
    }
}

// ---------------- review fixes (second pass) ----------------

/// hipp and the oracle agree on whether a batch runs on a fresh
/// database; whenever hipp runs it, the oracle loads hipp's file
/// and passes `PRAGMA integrity_check` (before the fix hipp
/// accepted, and stored verbatim, definitions the pin rejects —
/// a schema the pin then refuses as "malformed").
fn same_verdict(tag: &str, batch: &str) {
    let h = common::temp_db(&format!("v16td-{tag}-h"));
    let o = common::temp_db(&format!("v16td-{tag}-o"));
    let got = block_on(async {
        let mut db = open(&h).await;
        for stmt in batch.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            try_collect(&mut db, stmt).await?;
        }
        Ok::<(), Error<FsError>>(())
    });
    let (ok, _) = oracle_raw(&o, batch);
    assert_eq!(got.is_ok(), ok, "verdicts differ on {batch}\nhipp: {got:?}");
    if ok {
        assert_eq!(
            oracle_raw(&h, "PRAGMA integrity_check;").1,
            ["ok"],
            "hipp file unreadable after {batch}"
        );
    }
}

#[test]
fn malformed_definitions_are_refused_like_the_pin() {
    let corpus = [
        // The review's reproductions.
        "CREATE TABLE t(a CHECK(a > 0) bar)",
        "CREATE TABLE t(a, CHECK(a > 0) foo)",
        "CREATE TABLE t(a, c DEFAULT abs(1))",
        "CREATE TABLE t(a DEFAULT cast('5' AS integer))",
        "CREATE TABLE t(a DEFAULT foo bar)",
        "CREATE TABLE t(a DEFAULT \"x\" y)",
        "CREATE TABLE t(a DEFAULT - - 1)",
        "CREATE TABLE t(a DEFAULT (1)(2))",
        "CREATE TABLE t(b DEFAULT current_timestamp foo)",
        "CREATE TABLE t(b DEFAULT current_timestamp(1))",
        "CREATE TABLE t(b DEFAULT \"x\"(1))",
        "CREATE TABLE t(b DEFAULT 1 COLLATE)",
        "CREATE TABLE t(b DEFAULT x.y)",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); ALTER TABLE t ADD COLUMN b DEFAULT abs(-1)",
        // The same pattern elsewhere in the column grammar.
        "CREATE TABLE t(a INT PRIMARY KEY foo)",
        "CREATE TABLE t(a VARCHAR(10) foo)",
        "CREATE TABLE t(a VARCHAR(10, 20, 30))",
        "CREATE TABLE t(a INT SELECT)",
        "CREATE TABLE t(a DEFAULT SELECT)",
        "CREATE TABLE t(a REFERENCES p(x) foo)",
        "CREATE TABLE t(a REFERENCES p ON DELETE NO)",
        "CREATE TABLE t(a ON CONFLICT IGNORE)",
        "CREATE TABLE t(a UNIQUE ON CONFLICT)",
        "CREATE TABLE t(a CHECK(a) ON CONFLICT IGNORE)",
        "CREATE TABLE t(a, PRIMARY KEY (a) foo)",
        "CREATE TABLE t(a, UNIQUE (a AUTOINCREMENT))",
        "CREATE TABLE t(a, FOREIGN KEY (a))",
        "CREATE TABLE t(a, CHECK(a > 0), b)",
        "CREATE TABLE t(a, CONSTRAINT c, b)",
        "CREATE TABLE t(a, INDEX (a))",
        "CREATE TABLE t(a, key INT); INSERT INTO t VALUES (1, 2)",
        "CREATE TABLE t(a PRIMARY KEY PRIMARY KEY)",
        "CREATE TABLE t(a, b, UNIQUE (a) PRIMARY KEY (b) CHECK (a < b))",
        // Accepted forms stay accepted, and load on the pin.
        "CREATE TABLE t(a CHECK(a > 0) CHECK(a < 9), b, CHECK(b > 0) CHECK(b < 9))",
        "CREATE TABLE t(a DEFAULT 1 DEFAULT 2, b NULL NOT NULL DEFAULT 0, c DEFAULT key)",
        "CREATE TABLE t(a 'text' \"int\" [x] `y`, b VARCHAR(-10, +20.5), c DOUBLE PRECISION(10))",
        "CREATE TABLE t(a GENERATED ALWAYS (1), b GENERATED AS (a))",
        "CREATE TABLE p(id); CREATE TABLE t(a REFERENCES p (id) ON DELETE CASCADE \
         ON UPDATE SET NULL MATCH FULL NOT DEFERRABLE INITIALLY IMMEDIATE, b DEFERRABLE, \
         CONSTRAINT f FOREIGN KEY (a, b) REFERENCES p ON DELETE SET DEFAULT \
         DEFERRABLE INITIALLY DEFERRED, CONSTRAINT lone)",
        "CREATE TABLE t(a, b, PRIMARY KEY (a DESC, b) ON CONFLICT FAIL UNIQUE (b), \
         CHECK (a > b) ON CONFLICT ROLLBACK)",
        "CREATE TABLE t(a DEFAULT 0x10, b DEFAULT -0x10, c DEFAULT +NULL, d DEFAULT indexed, \
         e DEFAULT -current_time, f DEFAULT -'3')",
    ];
    for (i, sql) in corpus.iter().enumerate() {
        same_verdict(&format!("gram-{i}"), sql);
    }
}

#[test]
fn prng_definition_grammar_corpus() {
    // Random column definitions from valid and malformed clause
    // fragments: the verdicts agree, and every accepted schema
    // loads on the pin.
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let types = [
        "",
        "INT",
        "INTEGER",
        "TEXT",
        "VARCHAR(10)",
        "DECIMAL(10, 2)",
        "\"odd type\"",
        "KEY",
    ];
    let frags = [
        "NOT NULL",
        "NULL",
        "UNIQUE",
        "PRIMARY KEY",
        "PRIMARY KEY DESC",
        "DEFAULT 1",
        "DEFAULT -1.5",
        "DEFAULT (1+2)",
        "DEFAULT abs(1)",
        "DEFAULT 'x'",
        "DEFAULT hello",
        "DEFAULT - - 1",
        "CHECK(# > 0)",
        "CHECK(# > 0) x",
        "COLLATE NOCASE",
        "COLLATE",
        "REFERENCES p",
        "REFERENCES p(id) ON DELETE CASCADE",
        "REFERENCES p MATCH",
        "ON CONFLICT IGNORE",
        "UNIQUE ON CONFLICT REPLACE",
        "DEFERRABLE INITIALLY DEFERRED",
        "NOT DEFERRABLE",
        "AS (1)",
        "GENERATED ALWAYS AS (2) STORED",
        "CONSTRAINT c",
        "CONSTRAINT",
        "foo",
        "(1)",
        "DEFAULT",
        "DEFAULT CURRENT_TIME",
        "DEFAULT x.y",
        "NOT",
    ];
    let tcons = [
        "PRIMARY KEY (a)",
        "UNIQUE (a, b)",
        "UNIQUE (a) UNIQUE (b)",
        "CHECK (b < 5)",
        "CHECK (b < 5) ON CONFLICT FAIL",
        "FOREIGN KEY (a) REFERENCES p",
        "CONSTRAINT k UNIQUE (b)",
        "PRIMARY KEY (a) foo",
        "UNIQUE a",
        "FOREIGN KEY (a)",
        "CONSTRAINT z",
    ];
    let pick = |r: u64, xs: &[&'static str]| xs[(r % xs.len() as u64) as usize];
    for case in 0..120 {
        let mut items = Vec::new();
        for name in ["a", "b"] {
            let mut d = name.to_string();
            if rnd() % 3 != 0 {
                d.push(' ');
                d.push_str(pick(rnd(), &types));
            }
            for _ in 0..rnd() % 3 {
                d.push(' ');
                d.push_str(&pick(rnd(), &frags).replace('#', name));
            }
            items.push(d);
        }
        if rnd() % 2 == 0 {
            items.push(pick(rnd(), &tcons).to_string());
        }
        if rnd() % 4 == 0 {
            items.push("c".to_string());
        }
        let sql = format!("CREATE TABLE p(id); CREATE TABLE t({})", items.join(", "));
        same_verdict(&format!("gram-prng-{case}"), &sql);
    }
}

#[test]
fn repeated_constraint_sets_share_one_autoindex() {
    // A UNIQUE / PRIMARY KEY set repeating an earlier one (sort
    // order aside) gets no index of its own — hipp used to write a
    // second autoindex, which the pin reports as an orphan index.
    let setup = "CREATE TABLE t(a PRIMARY KEY UNIQUE, b UNIQUE, UNIQUE (b DESC), \
                 UNIQUE (b COLLATE NOCASE), UNIQUE (a, b), UNIQUE (b, a), UNIQUE (a));\
                 CREATE TABLE u(a UNIQUE, b, PRIMARY KEY (a));\
                 CREATE TABLE w(a INTEGER PRIMARY KEY UNIQUE)";
    let tw = twins("autoidx", setup);
    tw.all(&[
        "INSERT INTO t VALUES (1, 'x'), (2, 'y')",
        "INSERT INTO u VALUES (1, 1)",
        "INSERT INTO w VALUES (5)",
    ]);
    tw.refuse("INSERT INTO t VALUES (1, 'z')", SchemaKind::UniqueViolation);
    tw.refuse("INSERT INTO t VALUES (3, 'X')", SchemaKind::UniqueViolation);
    tw.refuse("INSERT INTO u VALUES (1, 2)", SchemaKind::UniqueViolation);
    tw.refuse("INSERT INTO w VALUES (5)", SchemaKind::UniqueViolation);
    tw.finish("SELECT * FROM t ORDER BY a");
    let q = "SELECT type, name, tbl_name FROM sqlite_schema ORDER BY name";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
}

#[test]
fn main_qualifier_is_left_out_of_the_schema_text() {
    let tw = twins(
        "mainq",
        "CREATE TABLE main.t(a INT);\
         CREATE TABLE IF NOT EXISTS \"main\" . u(b);\
         CREATE UNIQUE INDEX main.i ON t(a);\
         CREATE VIEW MAIN.v AS SELECT a FROM t",
    );
    tw.all(&["INSERT INTO main.t VALUES (1)", "SELECT * FROM t"]);
    tw.refuse_any("CREATE INDEX j ON main.t(a)");
    tw.refuse_any("CREATE TABLE foo.x(a)");
    tw.finish("SELECT * FROM t");
    let q = "SELECT type, name, sql FROM sqlite_schema ORDER BY name";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
}

#[test]
fn unary_plus_drops_affinity() {
    // `+x` has no affinity on the pin (hipp's parser used to drop
    // the `+`, so `+a = '1'` converted '1' and matched).
    let tw = twins(
        "uplus",
        "CREATE TABLE s(a INT, b TEXT, c TEXT COLLATE NOCASE, d);\
         INSERT INTO s VALUES (1, '1', 'X', 2), (2, '2.0', 'y', '2')",
    );
    tw.all(&[
        "SELECT count(*) FROM s WHERE +a = '1'",
        "SELECT count(*) FROM s WHERE a = '1'",
        "SELECT a, +b = 1, b = 1, +c = 'x', '2' = +d, +a IN ('1', 2) FROM s ORDER BY a",
        "SELECT a FROM s WHERE +b = 2.0",
        "SELECT +a, -+a, +-a, +(a), +'x', typeof(+b) FROM s ORDER BY 1",
        "SELECT a FROM s ORDER BY +1",
        "SELECT +a FROM s GROUP BY +a HAVING +a > 1",
        "SELECT \"+a\" FROM (SELECT +a FROM s) WHERE \"+a\" = '1'",
        "SELECT x FROM (SELECT +a AS x FROM s) WHERE x = '1'",
    ]);
}

#[cfg(feature = "v16")]
#[test]
fn create_table_as_select_names_follow_resolution() {
    // The pin names CTAS columns over the resolved list: `+a` is
    // an expression (`"+a"`, no affinity), `(a)` is the column,
    // and a column reference takes the table's spelling (`A` and
    // `S.B` are `a` and `b`); an explicit alias is kept as written.
    ctas(
        "names2",
        "CREATE TABLE s(a INT, b TEXT, id INTEGER PRIMARY KEY);\
         INSERT INTO s VALUES (1, 'x', 5)",
        &[
            "CREATE TABLE t1 AS SELECT +a, +b, (a), ((b)) FROM s",
            "CREATE TABLE t2 AS SELECT a, A FROM s",
            "CREATE TABLE t3 AS SELECT A, S.B, ID, Rowid, oid AS OID, a AS A, b COLLATE NOCASE FROM s",
            "CREATE TABLE t4 AS SELECT X, Y FROM (SELECT a AS x, b y FROM s)",
            "SELECT typeof(\"+a\"), typeof(a), \"+b\" FROM t1",
            "SELECT * FROM t3",
        ],
        "SELECT * FROM t1",
    );
}

#[test]
fn minimum_integer_literal_through_rules() {
    // `-9223372036854775808` is the INTEGER i64::MIN on the pin;
    // hipp evaluated it as a REAL and stored that through
    // DEFAULT, generated columns, CHECK and ALTER.
    let tw = twins(
        "i64min",
        "CREATE TABLE t(a, b DEFAULT -9223372036854775808, c DEFAULT (-9223372036854775808), \
         d AS (-9223372036854775808), e CHECK(e = -9223372036854775808), \
         f AS (-(9223372036854775808)) STORED)",
    );
    tw.all(&[
        "INSERT INTO t(a, e) VALUES (1, -9223372036854775808)",
        "ALTER TABLE t ADD COLUMN g DEFAULT -9223372036854775808",
        "SELECT b, typeof(b), c, typeof(c), d, typeof(d), e, typeof(e), f, typeof(f), g, typeof(g) FROM t",
        "SELECT -9223372036854775808, - 9223372036854775808, -(9223372036854775808), \
         -+9223372036854775808, typeof(- -9223372036854775808), typeof(-9223372036854775809), \
         typeof(-9223372036854775808.0)",
    ]);
    tw.refuse(
        "INSERT INTO t(a, e) VALUES (2, -9223372036854775807)",
        SchemaKind::CheckViolation,
    );
    tw.finish("SELECT rowid, a, b, c, e, g FROM t");
}

#[test]
fn stored_values_match_the_pins_evaluator() {
    // Values the evaluator computes now land on disk (STORED
    // generated columns, expression DEFAULTs, TEXT affinity), so
    // its divergences became file divergences: `||` over a blob is
    // TEXT, abs(i64::MIN) is "integer overflow", abs() of TEXT is
    // REAL, and REAL-to-TEXT uses the pin's `%!.17g` layout.
    let tw = twins(
        "evalstore",
        "CREATE TABLE t(a, c, d AS (c || a) STORED, e DEFAULT (x'41' || x'42'), \
         f TEXT, g AS (a || '') STORED, h AS (abs(c)) STORED)",
    );
    tw.all(&[
        "INSERT INTO t(a, c, f) VALUES (3.5, x'01', 1e20), (1e-5, '-2', 1e16), \
         (1e17, 'z', 0.0001), (-2.5e-300, 4, 123456.75), (100.0, -3.5, -0.0)",
        "SELECT typeof(d), hex(d), typeof(e), e, f, typeof(f), g, h, typeof(h) FROM t",
        "SELECT x'41' || x'42', typeof(x'01' || 1), typeof('' || x'')",
        "SELECT 1e20 || '', 1e15 || '', 1e16 || '', 1e17 || '', 1.5e-7 || '', 0.0001 || '', \
         CAST(-2.5e30 AS TEXT), 1e100 || '', 1.7976931348623157e308 || '', (0.1 + 0.2) || ''",
        "SELECT abs('5'), typeof(abs('-2.5')), abs(x'35'), abs('abc'), abs(-3), \
         abs(-9223372036854775807)",
    ]);
    tw.refuse(
        "SELECT abs(-9223372036854775808)",
        SchemaKind::IntegerOverflow,
    );
    tw.refuse(
        "INSERT INTO t(a, c) VALUES (1, -9223372036854775808)",
        SchemaKind::IntegerOverflow,
    );
    tw.finish("SELECT rowid, typeof(d), hex(d), hex(e), hex(f), hex(g), h FROM t");
}

#[test]
fn alter_add_column_on_an_empty_table() {
    // The pin refuses these only when the table has rows
    // (`sqlite3ErrorIfNotEmpty`); hipp refused them always.
    let tw = twins("alter-empty", "CREATE TABLE t(a); CREATE TABLE f(a)");
    tw.all(&[
        "ALTER TABLE t ADD COLUMN b DEFAULT (1 + 2)",
        "ALTER TABLE t ADD COLUMN c DEFAULT (abs(-3))",
        "ALTER TABLE t ADD COLUMN d NOT NULL",
        "ALTER TABLE t ADD COLUMN e NOT NULL DEFAULT NULL",
        "ALTER TABLE t ADD COLUMN g AS (a * 2) STORED",
        "INSERT INTO t(a, d, e) VALUES (5, 'd', 'e')",
        "SELECT * FROM t",
        "INSERT INTO f VALUES (1)",
    ]);
    for sql in [
        "ALTER TABLE f ADD COLUMN b DEFAULT (1 + 2)",
        "ALTER TABLE f ADD COLUMN d NOT NULL",
        "ALTER TABLE f ADD COLUMN g AS (a * 2) STORED",
        "ALTER TABLE t ADD COLUMN h DEFAULT (2 * 2)",
    ] {
        tw.refuse(sql, SchemaKind::CannotAddColumn);
    }
    tw.finish("SELECT rowid, * FROM t");
    let q = "SELECT sql FROM sqlite_schema ORDER BY name";
    assert_eq!(oracle_raw(&tw.hipp, q), oracle_raw(&tw.oracle, q));
}

std::thread_local! {
    static TICKS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

#[test]
fn clock_is_read_once_per_statement() {
    // A clock advancing one second per call, starting one second
    // before midnight: one statement's CURRENT_DATE, CURRENT_TIME
    // and CURRENT_TIMESTAMP — over every row — share one instant
    // (the clock used to be read per column, so a row could say
    // 1970-01-01 and 00:00:00 at once).
    let path = common::temp_db("v16td-clock-once");
    let rows = block_on(async {
        TICKS.with(|c| c.set(0));
        let clock: fn() -> i64 = || {
            TICKS.with(|c| {
                let n = c.get();
                c.set(n + 1);
                86_399_000 + n * 1000
            })
        };
        let mut db = open_with(&path, Config::default().clock(clock)).await;
        exec(
            &mut db,
            "CREATE TABLE t(a, d DEFAULT CURRENT_DATE, t DEFAULT CURRENT_TIME, \
             s DEFAULT CURRENT_TIMESTAMP)",
        )
        .await;
        exec(&mut db, "INSERT INTO t(a) VALUES (1), (2)").await;
        exec(&mut db, "INSERT INTO t(a) VALUES (3)").await;
        try_collect(&mut db, "SELECT a, d, t, s FROM t")
            .await
            .unwrap()
    });
    assert_eq!(
        rows,
        [
            "1|1970-01-01|23:59:59|1970-01-01 23:59:59",
            "2|1970-01-01|23:59:59|1970-01-01 23:59:59",
            "3|1970-01-02|00:00:00|1970-01-02 00:00:00",
        ]
    );
    assert_eq!(TICKS.with(|c| c.get()), 2, "one clock read per statement");
}

#[test]
fn autoincrement_is_kept_or_refused() {
    // Without `v16` hipp keeps no `sqlite_sequence`: CREATE refuses
    // the clause and an AUTOINCREMENT table the pin made is
    // read-only (an INSERT that left the sequence behind let the
    // pin hand out a rowid twice). With `v16` both work and the
    // sequence is the pin's (tests/autoinc_v16.rs covers it).
    let h = common::temp_db("v16td-autoinc-h");
    for sql in [
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)",
        "CREATE TABLE t2(a INTEGER, b, PRIMARY KEY (a AUTOINCREMENT))",
    ] {
        let r = block_on(async {
            let mut db = open(&h).await;
            try_collect(&mut db, sql).await
        });
        if cfg!(feature = "v16") {
            assert!(r.is_ok(), "{sql}: {r:?}");
        } else {
            assert!(matches!(r, Err(Error::Unsupported)), "{sql}: {r:?}");
        }
    }
    let o = common::temp_db("v16td-autoinc-o");
    common::cli(
        &o,
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b); INSERT INTO t(b) VALUES ('x');",
    );
    let (ins, rows) = block_on(async {
        let mut db = open(&o).await;
        let ins = try_collect(&mut db, "INSERT INTO t(b) VALUES ('y')").await;
        (
            ins,
            try_collect(&mut db, "SELECT a, b FROM t").await.unwrap(),
        )
    });
    if cfg!(feature = "v16") {
        assert!(ins.is_ok(), "{ins:?}");
        assert_eq!(rows, ["1|x", "2|y"]);
        assert_eq!(oracle_raw(&o, "SELECT * FROM sqlite_sequence;").1, ["t|2"]);
    } else {
        assert!(matches!(ins, Err(Error::Unsupported)), "{ins:?}");
        assert_eq!(rows, ["1|x"]);
    }
    assert_eq!(oracle_raw(&o, "PRAGMA integrity_check;").1, ["ok"]);
    assert!(oracle_raw(&o, "INSERT INTO t(b) VALUES ('z');").0);
}
