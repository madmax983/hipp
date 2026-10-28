//! v0.10 NOT NULL enforcement, differentially vs the pinned
//! oracle (SQLite 3.54.1). Semantics probed on the pin before
//! implementation; the matrix the tests encode:
//!
//! - The proposed row of an INSERT (VALUES, SELECT source, or
//!   upsert proposal) and the result row of an UPDATE are
//!   checked AFTER affinity and assignment resolution, BEFORE
//!   any uniqueness/rowid conflict is considered (NOT NULL
//!   wins error precedence everywhere).
//! - ABORT / FAIL / ROLLBACK (and the default): the statement
//!   errors with a NOT NULL violation, atomically.
//! - IGNORE: the offending row (INSERT) or combination
//!   (UPDATE) is skipped; everything else proceeds.
//! - REPLACE (INSERT OR REPLACE, the REPLACE verb, UPDATE OR
//!   REPLACE): a NULL in a NOT NULL column is replaced by the
//!   column's default (column affinity applied, as for any
//!   stored value); when the default itself is NULL the
//!   statement errors like ABORT. Substitution precedes
//!   uniqueness checks, so a substituted default can itself
//!   become a REPLACE victim-maker.
//! - An upsert proposal is resolved under the statement's own
//!   conflict action BEFORE arbitration (so OR IGNORE +
//!   upsert skips a violating proposal wholesale, and the DO
//!   UPDATE never runs for it); the DO UPDATE result row is
//!   then checked with ABORT semantics unconditionally — no
//!   substitution, no skip, whatever the statement's action.
//! - The rowid-alias column is exempt (its NULL record field
//!   is the rowid itself; an explicit NULL auto-assigns). A
//!   non-alias PRIMARY KEY column (TEXT PK, INTEGER PK DESC,
//!   INT PK) is an ordinary column for this check.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Result<(), S::Error>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
    Ok(())
}

async fn exec_err<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Error<S::Error>
where
    S::Error: core::fmt::Debug,
{
    match db.prepare(sql).await {
        Err(e) => e,
        Ok(mut st) => st.step().await.expect_err("step must fail"),
    }
}

/// Step a statement to completion, collecting every row it
/// yields (SELECT rows, or a write's RETURNING rows).
async fn collect<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Vec<String>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await.expect("prepare");
    let mut rows = Vec::new();
    while q.step().await.expect("step") == Step::Row {
        let mut line = String::new();
        for i in 0..q.column_count() {
            if i > 0 {
                line.push('|');
            }
            match q.column_value(i) {
                Some(hipp::Value::Null) | None => {}
                Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
                Some(hipp::Value::Real(f)) => {
                    let s = format!("{f}");
                    line.push_str(&s);
                    if !s.contains('.') {
                        line.push_str(".0");
                    }
                }
                Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                    line.push_str(&String::from_utf8_lossy(b));
                }
            }
        }
        rows.push(line);
    }
    rows
}

fn integrity(path: &std::path::Path) {
    assert_eq!(
        common::cli(path, "PRAGMA integrity_check;").trim(),
        "ok",
        "oracle version: {}",
        common::oracle_version()
    );
}

fn oracle_rows(path: &std::path::Path, sql: &str) -> Vec<String> {
    let out = common::cli(path, sql);
    out.lines().map(str::to_string).collect()
}

/// Run one SQL batch through the oracle CLI; returns
/// (success, stdout lines) without asserting, for statements
/// expected to fail on both engines.
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

/// A pair of twin databases with identical setup: `hipp_path`
/// is written by hipp, `oracle_path` by the oracle CLI.
struct Twins {
    hipp_path: std::path::PathBuf,
    oracle_path: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp_path = common::temp_db(&format!("{tag}-h"));
    let oracle_path = common::temp_db(&format!("{tag}-o"));
    common::cli(&oracle_path, setup);
    block_on(async {
        let fs = FileStorage::open(&hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                exec(&mut db, stmt).await.unwrap();
            }
        }
    });
    Twins {
        hipp_path,
        oracle_path,
    }
}

impl Twins {
    async fn open_hipp(&self) -> Connection<FileStorage> {
        Connection::open(
            FileStorage::open(&self.hipp_path).expect("fs open"),
            Config::default(),
        )
        .await
        .expect("open")
    }

    /// Apply one write statement to both twins; it must
    /// succeed on both.
    async fn write(&self, sql: &str) {
        let mut db = self.open_hipp().await;
        exec(&mut db, sql).await.unwrap();
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(ok, "oracle rejected: {sql}");
    }

    /// Apply one write statement that must fail on both
    /// engines with hipp's NOT NULL error kind, leaving both
    /// twins untouched.
    async fn write_not_null(&self, sql: &str) {
        let mut db = self.open_hipp().await;
        let e = exec_err(&mut db, sql).await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: SchemaKind::NotNullViolation
                }
            ),
            "hipp error for {sql}: {e:?}"
        );
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(!ok, "oracle accepted: {sql}");
    }

    /// Run one row-producing statement on both twins and
    /// compare the rows.
    async fn rows(&self, sql: &str) -> Vec<String> {
        let mut db = self.open_hipp().await;
        let got = collect(&mut db, sql).await;
        let want = oracle_rows(&self.oracle_path, sql);
        assert_eq!(got, want, "statement: {sql}");
        got
    }

    /// Final verdict: the oracle reads the hipp file clean
    /// (integrity + contents), and both twins agree on the
    /// full contents dump.
    fn finish(&self, dump_sql: &str) {
        integrity(&self.hipp_path);
        assert_eq!(
            oracle_rows(&self.hipp_path, dump_sql),
            oracle_rows(&self.oracle_path, dump_sql),
            "twin contents diverged"
        );
    }
}

const DUMP: &str = "SELECT rowid, * FROM t ORDER BY rowid";

// ---------------- INSERT: abort family ----------------

#[test]
fn insert_abort_family() {
    block_on(async {
        let tw = twins(
            "v10-ins-abort",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL); INSERT INTO t VALUES (1,'one');",
        );
        for sql in [
            "INSERT INTO t VALUES (2, NULL)",
            "INSERT OR ABORT INTO t VALUES (2, NULL)",
            "INSERT OR FAIL INTO t VALUES (2, NULL)",
            "INSERT OR ROLLBACK INTO t VALUES (2, NULL)",
            // A NULL produced by an expression is a violation.
            "INSERT INTO t VALUES (2, NULL || NULL)",
            "INSERT INTO t VALUES (2, CAST(NULL AS TEXT))",
            // The column omitted: its default is NULL.
            "INSERT INTO t (a) VALUES (2)",
            // Multi-row: the whole statement is atomic.
            "INSERT INTO t VALUES (2,'ok'),(3,NULL),(4,'ok2')",
        ] {
            tw.write_not_null(sql).await;
        }
        tw.finish(DUMP);
    });
    block_on(async {
        // DEFAULT NULL is still NULL: explicit and omitted
        // forms both violate.
        let tw = twins(
            "v10-ins-defnull",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL DEFAULT NULL);",
        );
        tw.write_not_null("INSERT INTO t VALUES (1, NULL)").await;
        tw.write_not_null("INSERT INTO t (a) VALUES (1)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // INSERT ... SELECT source rows are checked the
        // same way, and plain failure is atomic over the
        // whole source.
        let tw = twins(
            "v10-ins-sel-abort",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL);\
             CREATE TABLE s (x INTEGER, y TEXT);\
             INSERT INTO s VALUES (1,'ok'),(2,NULL);",
        );
        tw.write_not_null("INSERT INTO t SELECT * FROM s").await;
        tw.finish(DUMP);
    });
}

// ---------------- INSERT: IGNORE ----------------

#[test]
fn insert_ignore_skips() {
    block_on(async {
        let tw = twins(
            "v10-ins-ignore",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL);",
        );
        // The violating row is skipped; its neighbours land.
        tw.write("INSERT OR IGNORE INTO t VALUES (1,'ok'),(2,NULL),(3,'ok2')")
            .await;
        // INSERT ... SELECT: same skip rule per source row.
        tw.write("CREATE TABLE s (x INTEGER, y TEXT)").await;
        tw.write("INSERT INTO s VALUES (4,'four'),(5,NULL)").await;
        tw.write("INSERT OR IGNORE INTO t SELECT * FROM s").await;
        // A skipped row returns nothing through RETURNING.
        let got = tw
            .rows("INSERT OR IGNORE INTO t VALUES (9, NULL) RETURNING *")
            .await;
        assert!(got.is_empty());
        let got = tw
            .rows("INSERT OR IGNORE INTO t VALUES (9, 'nine') RETURNING *")
            .await;
        assert_eq!(got, vec!["9|nine".to_string()]);
        tw.finish(DUMP);
    });
}

// ---------------- INSERT: REPLACE substitution ----------------

#[test]
fn insert_replace_substitutes_default() {
    block_on(async {
        let tw = twins(
            "v10-ins-replace",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one');",
        );
        // No conflict at all: the NULL is still substituted.
        tw.write("INSERT OR REPLACE INTO t VALUES (2, NULL)").await;
        // The REPLACE verb is the same statement.
        tw.write("REPLACE INTO t VALUES (3, NULL)").await;
        // INSERT ... SELECT source: substitution applies.
        tw.write("CREATE TABLE s (x INTEGER, y TEXT)").await;
        tw.write("INSERT INTO s VALUES (4, NULL)").await;
        tw.write("INSERT OR REPLACE INTO t SELECT * FROM s").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Substitution precedes uniqueness: the substituted
        // default collides with an existing row, which
        // becomes the REPLACE victim.
        let tw = twins(
            "v10-ins-replace-collide",
            "CREATE TABLE t (b TEXT NOT NULL UNIQUE DEFAULT 'D', a INTEGER);\
             INSERT INTO t VALUES ('D',1),('x',2);",
        );
        tw.write("INSERT OR REPLACE INTO t VALUES (NULL, 3)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Without a (non-NULL) default, REPLACE degrades to
        // an error — and a conflicting row survives untouched.
        let tw = twins(
            "v10-ins-replace-nodef",
            "CREATE TABLE t (a INTEGER UNIQUE, b TEXT NOT NULL);\
             INSERT INTO t VALUES (1,'one');",
        );
        tw.write_not_null("INSERT OR REPLACE INTO t VALUES (1, NULL)")
            .await;
        tw.write_not_null("INSERT OR REPLACE INTO t VALUES (2, NULL)")
            .await;
        tw.write_not_null("REPLACE INTO t VALUES (2, NULL)").await;
        // Two NOT NULL columns: one substitutable, one not —
        // the statement still errors.
        let tw2 = twins(
            "v10-ins-replace-two",
            "CREATE TABLE t (a TEXT NOT NULL DEFAULT 'A', b TEXT NOT NULL);",
        );
        tw2.write_not_null("INSERT OR REPLACE INTO t VALUES (NULL, NULL)")
            .await;
        tw2.finish(DUMP);
        tw.finish(DUMP);
    });
    block_on(async {
        // The substituted default takes the column's
        // affinity, like any other stored value: DEFAULT '7'
        // in an INT column stores an integer.
        let tw = twins(
            "v10-ins-replace-aff",
            "CREATE TABLE t (a INTEGER, b INT NOT NULL DEFAULT '7');",
        );
        tw.write("INSERT OR REPLACE INTO t VALUES (1, NULL)").await;
        let got = tw.rows("SELECT b, typeof(b) FROM t").await;
        assert_eq!(got, vec!["7|integer".to_string()]);
        // The omitted-column path stores the same value:
        // a column default is affinity-applied however the
        // row acquires it (pre-v0.10 hipp stored the raw
        // text '7' for the omitted form).
        tw.write("INSERT INTO t (a) VALUES (2)").await;
        let got = tw.rows("SELECT b, typeof(b) FROM t ORDER BY a").await;
        assert_eq!(got, vec!["7|integer".to_string(), "7|integer".to_string()]);
        tw.finish(DUMP);
    });
}

// ---------------- UPDATE ----------------

#[test]
fn update_not_null_actions() {
    block_on(async {
        let tw = twins(
            "v10-upd",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one'),(2,'two');",
        );
        // Default action family: error (a default's presence
        // does NOT save a plain UPDATE).
        tw.write_not_null("UPDATE t SET b = NULL WHERE a = 1").await;
        tw.write_not_null("UPDATE OR ABORT t SET b = NULL WHERE a = 1")
            .await;
        tw.write_not_null("UPDATE OR FAIL t SET b = NULL WHERE a = 1")
            .await;
        // Last assignment wins: the check reads the final row.
        tw.write("UPDATE t SET b = NULL, b = 'x' WHERE a = 1").await;
        tw.write_not_null("UPDATE t SET b = 'y', b = NULL WHERE a = 1")
            .await;
        // IGNORE: the row is left unchanged.
        tw.write("UPDATE OR IGNORE t SET b = NULL WHERE a = 1")
            .await;
        // REPLACE: the default is substituted.
        tw.write("UPDATE OR REPLACE t SET b = NULL WHERE a = 2")
            .await;
        tw.finish(DUMP);
    });
    block_on(async {
        // REPLACE with no default errors; IGNORE without a
        // default still just skips.
        let tw = twins(
            "v10-upd-nodef",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL);\
             INSERT INTO t VALUES (1,'one'),(2,'two');",
        );
        tw.write_not_null("UPDATE OR REPLACE t SET b = NULL WHERE a = 1")
            .await;
        tw.write("UPDATE OR IGNORE t SET b = NULL").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Substitution precedes uniqueness in UPDATE too:
        // the substituted 'D' collides with row 2, which
        // OR REPLACE deletes as the victim.
        let tw = twins(
            "v10-upd-collide",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL UNIQUE DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one'),(2,'D');",
        );
        tw.write("UPDATE OR REPLACE t SET b = NULL WHERE a = 1")
            .await;
        tw.finish(DUMP);
    });
    block_on(async {
        // UPDATE ... FROM combinations are resolved one by
        // one: IGNORE skips the violating combination and a
        // later combination still applies; REPLACE
        // substitutes per combination.
        let tw = twins(
            "v10-upd-from",
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL DEFAULT 'D');\
             CREATE TABLE f (w TEXT);\
             INSERT INTO t VALUES (1,'one');\
             INSERT INTO f VALUES (NULL),('fromval');",
        );
        tw.write("UPDATE OR IGNORE t SET b = f.w FROM f WHERE t.a = 1")
            .await;
        let got = tw.rows("SELECT b FROM t").await;
        assert_eq!(got, vec!["fromval".to_string()]);
        tw.write("UPDATE t SET b = 'reset' WHERE a = 1").await;
        tw.write("UPDATE OR REPLACE t SET b = f.w FROM f WHERE t.a = 1 AND f.w IS NULL")
            .await;
        let got = tw.rows("SELECT b FROM t").await;
        assert_eq!(got, vec!["D".to_string()]);
        tw.finish(DUMP);
    });
}

// ---------------- UPSERT ----------------

#[test]
fn upsert_not_null() {
    block_on(async {
        let tw = twins(
            "v10-ups",
            "CREATE TABLE t (a INTEGER UNIQUE, b TEXT NOT NULL DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one');",
        );
        // The proposal is checked before arbitration: a
        // violating proposal errors even when a conflict
        // exists and DO UPDATE would never read the value,
        // and when there is no conflict at all.
        tw.write_not_null("INSERT INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET a = 1")
            .await;
        tw.write_not_null(
            "INSERT INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET b = excluded.b",
        )
        .await;
        tw.write_not_null("INSERT INTO t VALUES (2, NULL) ON CONFLICT(a) DO NOTHING")
            .await;
        tw.write_not_null("INSERT INTO t VALUES (2, NULL) ON CONFLICT(a) DO UPDATE SET b = 'u'")
            .await;
        // The DO UPDATE result is checked with abort
        // semantics even though a default exists.
        tw.write_not_null("INSERT INTO t VALUES (1, 'new') ON CONFLICT(a) DO UPDATE SET b = NULL")
            .await;
        // ... but a WHERE that skips the update skips the
        // check, and last-assignment-wins reads the final row.
        tw.write("INSERT INTO t VALUES (1, 'new') ON CONFLICT(a) DO UPDATE SET b = NULL WHERE 0")
            .await;
        tw.write("INSERT INTO t VALUES (1, 'new') ON CONFLICT(a) DO UPDATE SET b = NULL, b = 'x'")
            .await;
        tw.finish(DUMP);
    });
    block_on(async {
        // The statement's own action governs the proposal:
        // OR IGNORE skips a violating proposal wholesale
        // (the DO UPDATE never runs for it); OR REPLACE
        // substitutes the default, then arbitration runs.
        let tw = twins(
            "v10-ups-actions",
            "CREATE TABLE t (a INTEGER UNIQUE, b TEXT NOT NULL DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one');",
        );
        tw.write("INSERT OR IGNORE INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET b = 'z'")
            .await;
        let got = tw.rows("SELECT b FROM t WHERE a = 1").await;
        assert_eq!(got, vec!["one".to_string()]);
        tw.write("INSERT OR REPLACE INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET b = 'z'")
            .await;
        let got = tw.rows("SELECT b FROM t WHERE a = 1").await;
        assert_eq!(got, vec!["z".to_string()]);
        // The DO UPDATE result still aborts under both
        // statement actions.
        tw.write_not_null(
            "INSERT OR IGNORE INTO t VALUES (1, 'new') ON CONFLICT(a) DO UPDATE SET b = NULL",
        )
        .await;
        tw.write_not_null(
            "INSERT OR REPLACE INTO t VALUES (1, 'new') ON CONFLICT(a) DO UPDATE SET b = NULL",
        )
        .await;
        tw.finish(DUMP);
    });
}

// ---------------- PRIMARY KEY / rowid alias edges ----------------

#[test]
fn pk_and_alias_edges() {
    block_on(async {
        // The rowid alias is exempt: explicit NULL and an
        // omitted alias both auto-assign, NOT NULL or not.
        let tw = twins(
            "v10-alias",
            "CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, b TEXT);",
        );
        tw.write("INSERT INTO t VALUES (NULL, 'x')").await;
        tw.write("INSERT INTO t (b) VALUES ('y')").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Non-alias primary keys are ordinary columns here.
        let tw = twins(
            "v10-pk-desc",
            "CREATE TABLE t (id INTEGER PRIMARY KEY DESC NOT NULL, b TEXT);",
        );
        tw.write_not_null("INSERT INTO t VALUES (NULL, 'x')").await;
        tw.finish(DUMP);
    });
    block_on(async {
        let tw = twins(
            "v10-pk-text",
            "CREATE TABLE t (b TEXT PRIMARY KEY NOT NULL, a INTEGER);",
        );
        tw.write_not_null("INSERT INTO t VALUES (NULL, 1)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        let tw = twins(
            "v10-pk-int",
            "CREATE TABLE t (b INT PRIMARY KEY NOT NULL, a INTEGER);",
        );
        tw.write_not_null("INSERT INTO t VALUES (NULL, 1)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // A table-level PRIMARY KEY over a NOT NULL column
        // enforces the column's own NOT NULL.
        let tw = twins(
            "v10-pk-table",
            "CREATE TABLE t (b TEXT NOT NULL, a INTEGER, PRIMARY KEY (b));",
        );
        tw.write_not_null("INSERT INTO t VALUES (NULL, 1)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Without NOT NULL, a TEXT PRIMARY KEY still accepts
        // NULLs (heritage behaviour, oracle-verified in v0.3).
        let tw = twins(
            "v10-pk-plain",
            "CREATE TABLE t (b TEXT PRIMARY KEY, a INTEGER);",
        );
        tw.write("INSERT INTO t VALUES (NULL, 1)").await;
        tw.write("INSERT INTO t VALUES (NULL, 2)").await;
        tw.finish(DUMP);
    });
    block_on(async {
        // Error precedence: NOT NULL beats both a UNIQUE
        // conflict and a duplicate rowid.
        let tw = twins(
            "v10-prec",
            "CREATE TABLE t (a INTEGER UNIQUE, b TEXT NOT NULL);\
             INSERT INTO t VALUES (1,'one');",
        );
        tw.write_not_null("INSERT INTO t VALUES (1, NULL)").await;
        tw.write_not_null("UPDATE t SET a = 1, b = NULL WHERE a = 1")
            .await;
        tw.finish(DUMP);
    });
}

// ---------------- foreign files + tiny profile ----------------

#[test]
fn oracle_written_schema_enforced() {
    block_on(async {
        // The schema is written by the oracle alone; hipp
        // opens the foreign file, re-derives NOT NULL from
        // the stored CREATE TABLE text, and enforces it.
        let path = common::temp_db("v10-foreign");
        common::cli(
            &path,
            "CREATE TABLE t (a INTEGER, b TEXT NOT NULL DEFAULT 'D');\
             INSERT INTO t VALUES (1,'one');",
        );
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let e = exec_err(&mut db, "INSERT INTO t VALUES (2, NULL)").await;
        assert!(matches!(
            e,
            Error::Schema {
                kind: SchemaKind::NotNullViolation
            }
        ));
        exec(&mut db, "INSERT OR REPLACE INTO t VALUES (2, NULL)")
            .await
            .unwrap();
        let got = collect(&mut db, "SELECT rowid, * FROM t ORDER BY rowid").await;
        assert_eq!(got, vec!["1|1|one".to_string(), "2|2|D".to_string()]);
        integrity(&path);
    });
}

#[test]
fn tiny_config_enforces_not_null() {
    // Enforcement is not feature-gated (it is a correctness
    // fix in the existing INSERT path, like the v0.8 affinity
    // fix): the tiny reference configuration enforces too.
    block_on(async {
        let mem = hipp::MemStorage::new();
        let mut db = Connection::open(mem, Config::tiny()).await.expect("open");
        exec(&mut db, "CREATE TABLE t (a INTEGER, b TEXT NOT NULL)")
            .await
            .unwrap();
        let e = exec_err(&mut db, "INSERT INTO t VALUES (1, NULL)").await;
        assert!(matches!(
            e,
            Error::Schema {
                kind: SchemaKind::NotNullViolation
            }
        ));
        exec(&mut db, "INSERT INTO t VALUES (1, 'ok')")
            .await
            .unwrap();
        let got = collect(&mut db, "SELECT * FROM t").await;
        assert_eq!(got, vec!["1|ok".to_string()]);
    });
}
