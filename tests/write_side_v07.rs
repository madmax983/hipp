//! v0.7 write-side completion, differentially vs the pinned
//! oracle (SPEC §2 v0.7): OR REPLACE (INSERT and UPDATE),
//! UPDATE ... FROM, RETURNING, and UPSERT. Writes are applied
//! by hipp to one file and by the oracle CLI to a twin; final
//! contents are compared from the oracle side, and
//! `integrity_check` is the structural verdict on every
//! hipp-written file (REPLACE and upsert delete rows through
//! every index they own). RETURNING statements run on both
//! engines and their row streams are compared directly.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};

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
    /// Apply one write statement to both twins; it must
    /// succeed on both.
    async fn write(&self, sql: &str) {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        exec(&mut db, sql).await.unwrap();
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(ok, "oracle rejected: {sql}");
    }

    /// Apply one write statement that must fail on both
    /// engines, leaving both twins untouched.
    async fn write_err(&self, sql: &str) {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let _ = exec_err(&mut db, sql).await;
        let (ok, _) = oracle_raw(&self.oracle_path, sql);
        assert!(!ok, "oracle accepted: {sql}");
    }

    /// Run one row-producing statement (SELECT or a write
    /// with RETURNING) on both twins and compare the rows.
    async fn rows(&self, sql: &str) -> Vec<String> {
        let fs = FileStorage::open(&self.hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let got = collect(&mut db, sql).await;
        let want = oracle_rows(&self.oracle_path, sql);
        assert_eq!(got, want, "statement: {sql}");
        got
    }

    /// Final verdict: the oracle reads the hipp file clean
    /// (integrity + contents), and both twins agree on the
    /// full contents of `tables` (in the given order-by form).
    fn finish(&self, dump_sql: &str) {
        integrity(&self.hipp_path);
        assert_eq!(
            oracle_rows(&self.hipp_path, dump_sql),
            oracle_rows(&self.oracle_path, dump_sql),
            "twin contents diverged"
        );
    }
}

const T_SETUP: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, v INTEGER);
INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30);";

// ---------------- INSERT OR REPLACE / OR IGNORE ----------------

#[test]
fn insert_or_replace_differential() {
    block_on(async {
        let tw = twins("v07-ins-replace", T_SETUP);
        // PK conflict: row 1 is replaced.
        tw.write("INSERT OR REPLACE INTO t VALUES (1,'x',99)").await;
        // UNIQUE conflict: row 2 (holder of 'b') is deleted.
        tw.write("INSERT OR REPLACE INTO t VALUES (4,'b',40)").await;
        // One proposal, two victims: PK row 3 and UNIQUE row 4
        // both go; only the proposal remains of the three.
        tw.write("INSERT OR REPLACE INTO t VALUES (3,'b',33)").await;
        // The REPLACE verb is the same statement.
        tw.write("REPLACE INTO t VALUES (5,'b',55)").await;
        // OR IGNORE: the conflicting row is skipped, the rest
        // of the VALUES list still lands.
        tw.write("INSERT OR IGNORE INTO t VALUES (5,'q',1),(6,'r',2)")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn insert_conflict_actions_still_error_by_default() {
    block_on(async {
        let tw = twins("v07-ins-abort", T_SETUP);
        // Plain INSERT keeps the abort behaviour on both the
        // rowid and the UNIQUE index.
        tw.write_err("INSERT INTO t VALUES (1,'z',0)").await;
        tw.write_err("INSERT INTO t VALUES (9,'a',0)").await;
        // OR ABORT / OR FAIL spellings are the same abort.
        tw.write_err("INSERT OR ABORT INTO t VALUES (1,'z',0)")
            .await;
        tw.write_err("INSERT OR FAIL INTO t VALUES (9,'a',0)").await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

// ---------------- UPDATE OR REPLACE / OR IGNORE ----------------

#[test]
fn update_or_replace_differential() {
    block_on(async {
        let tw = twins("v07-upd-replace", T_SETUP);
        // UNIQUE conflict: setting row 2's u to 'a' deletes
        // row 1.
        tw.write("UPDATE OR REPLACE t SET u = 'a' WHERE id = 2")
            .await;
        // Rowid conflict: row 2 takes rowid 3, row 3 is
        // deleted.
        tw.write("UPDATE OR REPLACE t SET id = 3 WHERE id = 2")
            .await;
        // OR IGNORE skips the conflicting row (row 3 now holds
        // 'a'; asking row 3... use a fresh collision below).
        tw.write("INSERT INTO t VALUES (7,'g',70)").await;
        tw.write("UPDATE OR IGNORE t SET u = 'g' WHERE id = 3")
            .await;
        // Plain UPDATE on a conflict still aborts on both.
        tw.write_err("UPDATE t SET u = 'g' WHERE id = 3").await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

// ---------------- RETURNING ----------------

#[test]
fn returning_insert_differential() {
    block_on(async {
        let tw = twins("v07-ret-ins", T_SETUP);
        // Multi-row INSERT returns rows in VALUES order.
        let rows = tw
            .rows("INSERT INTO t VALUES (4,'d',40),(5,'e',50) RETURNING id, v")
            .await;
        assert_eq!(rows, vec!["4|40", "5|50"]);
        // Star expansion, expressions, aliases, rowid.
        tw.rows("INSERT INTO t VALUES (6,'f',60) RETURNING *, id + 1 AS nxt")
            .await;
        tw.rows("INSERT INTO t VALUES (7,'g',70) RETURNING rowid, u")
            .await;
        // OR IGNORE: skipped rows return nothing.
        let rows = tw
            .rows("INSERT OR IGNORE INTO t VALUES (7,'h',71),(8,'i',80) RETURNING id")
            .await;
        assert_eq!(rows, vec!["8"]);
        // OR REPLACE returns the replacement row.
        tw.rows("INSERT OR REPLACE INTO t VALUES (8,'eye',88) RETURNING *")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn returning_update_delete_differential() {
    block_on(async {
        let tw = twins("v07-ret-upd", T_SETUP);
        // UPDATE returns post-update values, one row per
        // target row.
        let rows = tw
            .rows("UPDATE t SET v = v + 1 WHERE id <= 2 RETURNING id, v")
            .await;
        assert_eq!(rows, vec!["1|11", "2|21"]);
        // No matching rows: no RETURNING rows.
        let rows = tw
            .rows("UPDATE t SET v = 0 WHERE id = 99 RETURNING id")
            .await;
        assert!(rows.is_empty());
        // DELETE returns pre-delete values.
        let rows = tw
            .rows("DELETE FROM t WHERE id >= 2 RETURNING id, u, v")
            .await;
        assert_eq!(rows, vec!["2|b|21", "3|c|30"]);
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn returning_subqueries_and_rejections() {
    block_on(async {
        let tw = twins(
            "v07-ret-sub",
            "CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE emp (id INTEGER PRIMARY KEY, dept_id INTEGER, pay INTEGER);
             INSERT INTO dept VALUES (1,'eng'),(2,'ops');
             INSERT INTO emp VALUES (1,1,100),(2,2,200),(3,1,300);",
        );
        // Correlated scalar subquery in RETURNING reads the
        // row just written.
        tw.rows(
            "INSERT INTO emp VALUES (4,2,400) RETURNING id, (SELECT name FROM dept WHERE dept.id = emp.dept_id)",
        )
        .await;
        tw.rows(
            "UPDATE emp SET pay = pay + 10 WHERE id = 4 RETURNING (SELECT count(*) FROM emp WHERE dept_id = 2)",
        )
        .await;
        tw.rows(
            "DELETE FROM emp WHERE id = 4 RETURNING id, (SELECT name FROM dept WHERE id = emp.dept_id)",
        )
        .await;
        // Aggregates in RETURNING are refused on both engines.
        tw.write_err("INSERT INTO emp VALUES (9,1,1) RETURNING count(*)")
            .await;
        tw.write_err("DELETE FROM emp RETURNING max(pay)").await;
        tw.finish("SELECT * FROM emp ORDER BY id");
    });
}

// ---------------- UPSERT ----------------

#[test]
fn upsert_basics_differential() {
    block_on(async {
        let tw = twins("v07-upsert", T_SETUP);
        // No target: any conflict arbitrates.
        tw.write("INSERT INTO t VALUES (1,'a2',11) ON CONFLICT DO NOTHING")
            .await;
        tw.write("INSERT INTO t VALUES (9,'b',29) ON CONFLICT DO NOTHING")
            .await;
        tw.write("INSERT INTO t VALUES (2,'b',200) ON CONFLICT DO UPDATE SET v = excluded.v")
            .await;
        // Bare names in SET bind the ORIGINAL row; excluded.*
        // is the proposal.
        tw.write("INSERT INTO t VALUES (3,'c',300) ON CONFLICT DO UPDATE SET v = v + excluded.v")
            .await;
        // Qualified original-table references work in SET
        // expressions.
        tw.write("INSERT INTO t VALUES (1,'a',1000) ON CONFLICT DO UPDATE SET v = t.v + 1")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn upsert_targets_differential() {
    block_on(async {
        let tw = twins("v07-upsert-tgt", T_SETUP);
        // Target on the rowid alias.
        tw.write("INSERT INTO t VALUES (1,'zz',0) ON CONFLICT(id) DO UPDATE SET v = 111")
            .await;
        // Target on the UNIQUE column.
        tw.write(
            "INSERT INTO t VALUES (8,'b',0) ON CONFLICT(u) DO UPDATE SET v = excluded.v + 1000",
        )
        .await;
        // ON CONFLICT(rowid) names the rowid target too.
        tw.write("INSERT INTO t VALUES (2,'q',0) ON CONFLICT(rowid) DO NOTHING")
            .await;
        // A DESC decoration on the target column is accepted.
        tw.write("INSERT INTO t VALUES (9,'c',0) ON CONFLICT(u DESC) DO NOTHING")
            .await;
        // A conflict the target does NOT name is not
        // arbitrated: it falls back to the statement action
        // (abort), on both engines, atomically.
        tw.write_err("INSERT INTO t VALUES (10,'a',0) ON CONFLICT(id) DO NOTHING")
            .await;
        tw.write_err("INSERT INTO t VALUES (1,'new',0) ON CONFLICT(u) DO NOTHING")
            .await;
        // A target naming no key at all is refused on both.
        tw.write_err("INSERT INTO t VALUES (10,'new',0) ON CONFLICT(v) DO NOTHING")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn upsert_where_and_multirow_differential() {
    block_on(async {
        let tw = twins("v07-upsert-where", T_SETUP);
        // DO UPDATE WHERE gates the update: false skips the
        // row entirely (no insert, no update).
        tw.write(
            "INSERT INTO t VALUES (1,'a',500) ON CONFLICT(id) DO UPDATE SET v = excluded.v WHERE excluded.v > 1000",
        )
        .await;
        tw.write(
            "INSERT INTO t VALUES (1,'a',500) ON CONFLICT(id) DO UPDATE SET v = excluded.v WHERE excluded.v > 100",
        )
        .await;
        // Target WHERE: resolved but ignored for non-partial
        // indexes (oracle-probed: WHERE 0 arbitrates exactly
        // like WHERE 1) — and an unknown column in it errors.
        tw.write("INSERT INTO t VALUES (2,'b',0) ON CONFLICT(u) WHERE 0 DO NOTHING")
            .await;
        tw.write_err("INSERT INTO t VALUES (2,'b',0) ON CONFLICT(u) WHERE nosuch DO NOTHING")
            .await;
        // Multi-row upsert with a named target the third row
        // does not match: rows are processed in order, the
        // third proposal (6,'e2',60) conflicts on u with the
        // row this statement just upserted, target (id) does
        // not arbitrate a u conflict, and the whole statement
        // aborts on both engines.
        tw.write_err(
            "INSERT INTO t VALUES (5,'e',50),(5,'e2',55),(6,'e2',60) ON CONFLICT(id) DO UPDATE SET u = excluded.u, v = excluded.v",
        )
        .await;
        // Without a target, every conflict arbitrates: the
        // third proposal updates row 5 instead of inserting.
        tw.write(
            "INSERT INTO t VALUES (5,'e',50),(5,'e2',55),(6,'e2',60) ON CONFLICT DO UPDATE SET u = excluded.u, v = excluded.v",
        )
        .await;
        // OR IGNORE with an upsert clause: the upsert governs.
        tw.write(
            "INSERT OR IGNORE INTO t VALUES (6,'e2',600) ON CONFLICT(id) DO UPDATE SET v = excluded.v",
        )
        .await;
        // DO UPDATE without a target but with a WHERE.
        tw.write(
            "INSERT INTO t VALUES (3,'c',33) ON CONFLICT DO UPDATE SET v = excluded.v WHERE t.v < excluded.v",
        )
        .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn upsert_returning_differential() {
    block_on(async {
        let tw = twins("v07-upsert-ret", T_SETUP);
        // DO NOTHING returns no rows for the skipped row.
        let rows = tw
            .rows("INSERT INTO t VALUES (1,'a',0) ON CONFLICT DO NOTHING RETURNING id")
            .await;
        assert!(rows.is_empty());
        // DO UPDATE returns the updated row.
        let rows = tw
            .rows(
                "INSERT INTO t VALUES (1,'a',111) ON CONFLICT(id) DO UPDATE SET v = excluded.v RETURNING id, v",
            )
            .await;
        assert_eq!(rows, vec!["1|111"]);
        // A plain insert through an upsert clause returns the
        // inserted row.
        let rows = tw
            .rows("INSERT INTO t VALUES (4,'d',40) ON CONFLICT DO NOTHING RETURNING *")
            .await;
        assert_eq!(rows, vec!["4|d|40"]);
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn upsert_syntax_rejections() {
    block_on(async {
        let tw = twins("v07-upsert-rej", T_SETUP);
        // A qualified SET left-hand side is a syntax error in
        // SQLite, and a parse error here too.
        tw.write_err("INSERT INTO t VALUES (1,'a',0) ON CONFLICT(id) DO UPDATE SET t.v = 1")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

// ---------------- UPDATE ... FROM ----------------

const FROM_SETUP: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);
CREATE TABLE f (id INTEGER PRIMARY KEY, w INTEGER);
INSERT INTO t VALUES (1,10),(2,20),(3,30);
INSERT INTO f VALUES (1,100),(2,200);";

#[test]
fn update_from_differential() {
    block_on(async {
        let tw = twins("v07-from", FROM_SETUP);
        // Basic join: only matched target rows change.
        tw.write("UPDATE t SET v = f.w FROM f WHERE f.id = t.id")
            .await;
        // SET expressions read target and FROM columns.
        tw.write("UPDATE t SET v = t.v + f.w FROM f WHERE f.id = t.id AND t.id = 1")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn update_from_multi_match_last_wins() {
    block_on(async {
        let tw = twins(
            "v07-from-multi",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);
             CREATE TABLE f (id INTEGER PRIMARY KEY, w INTEGER);
             INSERT INTO t VALUES (1,10);
             INSERT INTO f VALUES (1,1),(2,2),(3,4);",
        );
        // Three FROM combinations match the one target row;
        // each is computed from the ORIGINAL target value and
        // the last one wins: 10 + 4 = 14, not 17.
        tw.write("UPDATE t SET v = t.v + f.w FROM f").await;
        let rows = tw.rows("SELECT v FROM t").await;
        assert_eq!(rows, vec!["14"]);
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn update_from_joins_and_aliases() {
    block_on(async {
        let tw = twins(
            "v07-from-join",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);
             CREATE TABLE f (id INTEGER PRIMARY KEY, w INTEGER);
             CREATE TABLE g (x INTEGER PRIMARY KEY, y INTEGER);
             INSERT INTO t VALUES (1,10),(2,20),(3,30);
             INSERT INTO f VALUES (1,1),(2,2),(3,3);
             INSERT INTO g VALUES (1,1000),(2,2000);",
        );
        // FROM with its own JOIN, target under an alias.
        tw.write(
            "UPDATE t AS tt SET v = y FROM f JOIN g ON g.x = f.w WHERE f.id = tt.id AND tt.id = 2",
        )
        .await;
        // Self-FROM under an alias.
        tw.write("UPDATE t SET v = f.v + 1 FROM t AS f WHERE f.id = t.id AND t.id = 3")
            .await;
        // LEFT JOIN between FROM sources: an unmatched g row
        // extends with NULLs and the SET writes the NULL.
        tw.write("UPDATE t SET v = g.y FROM f LEFT JOIN g ON g.x = f.w + 10 WHERE f.id = t.id AND t.id = 1")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

#[test]
fn update_from_returning_differential() {
    block_on(async {
        let tw = twins("v07-from-ret", FROM_SETUP);
        // RETURNING sees the target table only, post-update,
        // one row per updated target row.
        let rows = tw
            .rows("UPDATE t SET v = f.w FROM f WHERE f.id = t.id RETURNING id, v")
            .await;
        assert_eq!(rows, vec!["1|100", "2|200"]);
        // FROM columns are not in RETURNING's scope, on either
        // engine.
        tw.write_err("UPDATE t SET v = f.w FROM f WHERE f.id = t.id RETURNING f.w")
            .await;
        tw.finish("SELECT * FROM t ORDER BY id");
    });
}

// ---------------- both directions over one file ----------------

#[test]
fn v07_oracle_writes_hipp_continues() {
    block_on(async {
        // The oracle builds a file using v0.7 statements;
        // hipp opens it and keeps writing with the same
        // dialect; the oracle verifies the result.
        let path = common::temp_db("v07-oracle-first");
        common::cli(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, v INTEGER);
             INSERT INTO t VALUES (1,'a',10) ON CONFLICT DO NOTHING;
             INSERT INTO t VALUES (1,'a',99) ON CONFLICT(id) DO UPDATE SET v = excluded.v;
             INSERT OR REPLACE INTO t VALUES (2,'b',20);",
        );
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let rows = collect(
            &mut db,
            "INSERT INTO t VALUES (3,'c',30) ON CONFLICT DO NOTHING RETURNING id",
        )
        .await;
        assert_eq!(rows, vec!["3"]);
        let rows = collect(&mut db, "UPDATE t SET v = v + 1 RETURNING id, v").await;
        assert_eq!(rows, vec!["1|100", "2|21", "3|31"]);
        drop(db);
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id"),
            vec!["1|a|100", "2|b|21", "3|c|31"]
        );
    });
}
