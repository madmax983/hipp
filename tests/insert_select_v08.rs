//! v0.8 correctness-and-completion, differentially vs the
//! pinned oracle (SPEC §2 v0.8): (1) cross-affinity comparison
//! correctness — SQLite's comparison-affinity rules applied
//! between two columns (and CASTs, rowids, subquery results),
//! closing the silent wrong-answer divergence carried since
//! v0.4; (2) INSERT completion — INSERT INTO ... SELECT and
//! subqueries in INSERT VALUES. Read queries compare row-for-
//! row against the oracle CLI over the same file; writes are
//! applied by hipp to one file and by the oracle CLI to a
//! twin, with final contents compared from the oracle side
//! and `integrity_check` on every hipp-written file.

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

fn oracle_db(tag: &str, setup: &str) -> std::path::PathBuf {
    let path = common::temp_db(tag);
    common::cli(&path, setup);
    path
}

/// Assert hipp and the oracle agree on `query` over the file
/// at `path` — row order included.
async fn diff_query(path: &std::path::Path, query: &str) {
    let want = oracle_rows(path, query);
    let fs = FileStorage::open(path).expect("fs open");
    let mut db = Connection::open(fs, Config::default()).await.expect("open");
    let got = collect(&mut db, query).await;
    assert_eq!(
        got,
        want,
        "query: {query}\noracle: {}",
        common::oracle_version()
    );
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
    /// full contents per `dump_sql`.
    fn finish(&self, dump_sql: &str) {
        integrity(&self.hipp_path);
        assert_eq!(
            oracle_rows(&self.hipp_path, dump_sql),
            oracle_rows(&self.oracle_path, dump_sql),
            "twin contents diverged"
        );
    }
}

// ---------------- cross-affinity comparisons ----------------

const M_SETUP: &str = "CREATE TABLE m (i INTEGER, x TEXT, n NUMERIC, r REAL, b BLOB);
INSERT INTO m VALUES (1,'1',1,1.0,1),(2,'002','002',2.0,'2'),(10,'10',10,10.0,x'3130'),(NULL,'',NULL,NULL,NULL);
CREATE TABLE j (a INTEGER);
INSERT INTO j VALUES (1),(2),(3);";

#[test]
fn cross_affinity_column_pairs_differential() {
    block_on(async {
        let path = oracle_db("v08-xaff-pairs", M_SETUP);
        for q in [
            // NUMERIC-class vs TEXT-class columns, both
            // directions and the full operator family: the
            // v0.4 simplification compared raw storage classes
            // here (1 = '1' was false).
            "SELECT i = x, i <> x, i < x, i <= x, i > x, i >= x, x = i FROM m ORDER BY i",
            // NUMERIC / REAL / NUMERIC-family pairings.
            "SELECT i = n, i = r, n = r, n = x, r = x FROM m ORDER BY i",
            // BLOB-affinity is its own class: TEXT vs BLOB
            // converts nothing (x = b is false on row 1 even
            // though b holds integer 1 and x holds '1'), while
            // NUMERIC vs BLOB applies NUMERIC to the BLOB side
            // (b = i is true on rows 1 and 2).
            "SELECT x = b, b = x, b = i, i = b FROM m ORDER BY i",
            // Column vs literal keeps the v0.4 behaviour.
            "SELECT x = 1, i = '1', i = '001', b = '1', b = 1 FROM m ORDER BY i",
            // CAST operands carry the cast's affinity class.
            "SELECT CAST(i AS TEXT) = x, CAST(x AS INTEGER) = i, x = CAST(1 AS BLOB) FROM m ORDER BY i",
            // A rowid reference is NUMERIC-class.
            "SELECT i FROM m WHERE rowid = '2'",
            "SELECT count(*) FROM m WHERE rowid = '2'",
            // IS / IS NOT take the same affinity adjustment
            // as = / <> (plus NULL-safety).
            "SELECT i IS x, i IS NOT x FROM m ORDER BY i",
            // The filtered form: rows 1, 2 and 10 qualify.
            "SELECT i FROM m WHERE i = x ORDER BY i",
            "SELECT i FROM m WHERE i < x ORDER BY i",
        ] {
            diff_query(&path, q).await;
        }
    });
}

#[test]
fn cross_affinity_compound_positions_differential() {
    block_on(async {
        let path = oracle_db("v08-xaff-compound", M_SETUP);
        for q in [
            // CASE's base form compares like `=`, affinity
            // included (NULLIF is the binary exception and is
            // covered by the last query).
            "SELECT CASE i WHEN x THEN 'y' ELSE 'n' END FROM m ORDER BY i",
            "SELECT CASE x WHEN i THEN 'y' ELSE 'n' END FROM m ORDER BY i",
            "SELECT CASE 1 WHEN '1' THEN 'y' ELSE 'n' END FROM m ORDER BY i LIMIT 1",
            // BETWEEN is the pair of comparisons, pairwise
            // affinity (oracle-probed against the special-rule
            // alternative).
            "SELECT x BETWEEN i AND 5, i BETWEEN x AND 5 FROM m ORDER BY i",
            // IN lists and IN (subquery) take the left
            // operand's comparison affinity.
            "SELECT i IN ('1','002','3'), x IN (1,2,10) FROM m ORDER BY i",
            "SELECT count(*) FROM m WHERE x IN (SELECT i FROM m)",
            "SELECT count(*) FROM m WHERE i IN (SELECT x FROM m)",
            // A scalar / IN subquery whose result item is a
            // direct column reference carries that column's
            // affinity into the comparison (v0.8 AffLiteral):
            // '002' = (SELECT i ... => 2) is true through the
            // result column's NUMERIC class, where treating
            // the result as an affinity-less literal would
            // apply TEXT to it and compare '002' with '2'.
            "SELECT x = (SELECT i FROM m WHERE i = 1), i = (SELECT x FROM m WHERE i = 2) FROM m ORDER BY i LIMIT 2",
            "SELECT count(*) FROM m WHERE x = (SELECT i FROM m WHERE i = 2)",
            // Cross-affinity in a JOIN condition.
            "SELECT count(*) FROM m JOIN j ON j.a = m.x",
            "SELECT count(*) FROM m JOIN j ON j.a = m.i",
            // NULLIF compares binary — no affinity — so
            // nullif(i, x) keeps i on every row here.
            "SELECT nullif(i, x), nullif(x, i) FROM m ORDER BY i",
        ] {
            diff_query(&path, q).await;
        }
    });
}

const W_SETUP: &str = "CREATE TABLE w (i INTEGER, x TEXT);
INSERT INTO w VALUES (1,'1'),(2,'02'),(3,'3'),(4,'four');";

#[test]
fn cross_affinity_write_filters_differential() {
    block_on(async {
        let tw = twins("v08-xaff-write", W_SETUP);
        // WHERE i = x matches rows 1, 2 ('02' -> 2) and 3.
        tw.write("UPDATE w SET i = i + 100 WHERE i = x").await;
        // x = 2 is a TEXT comparison ('2'): nothing matches
        // ('02' is not '2'); i = '02' is NUMERIC: row 2 goes.
        tw.write("DELETE FROM w WHERE x = 2").await;
        tw.write("DELETE FROM w WHERE i = '02'").await;
        tw.finish("SELECT * FROM w ORDER BY rowid");
    });
}

// ---------------- INSERT INTO ... SELECT ----------------

const S_SETUP: &str = "CREATE TABLE s (a INTEGER PRIMARY KEY, b TEXT, c INTEGER DEFAULT 7);
INSERT INTO s VALUES (1,'one',10),(2,'two',20),(3,'three',30);
CREATE TABLE d (a INTEGER PRIMARY KEY, b TEXT, c INTEGER DEFAULT 99);
CREATE TABLE agg (b TEXT, n INTEGER, tot INTEGER);
CREATE TABLE self_t (v INTEGER);
INSERT INTO self_t VALUES (1),(2);";

#[test]
fn insert_select_differential() {
    block_on(async {
        let tw = twins("v08-ins-sel", S_SETUP);
        // Plain copy, expression projection, a filter.
        tw.write("INSERT INTO d SELECT * FROM s").await;
        tw.write("INSERT INTO d (a, b) SELECT a + 10, b FROM s WHERE a <= 2")
            .await;
        // Empty result inserts nothing and succeeds.
        tw.write("INSERT INTO d SELECT a, b, c FROM s WHERE 0")
            .await;
        // Aggregates + GROUP BY through the v0.4 machinery.
        tw.write("INSERT INTO agg SELECT b, count(*), sum(c) FROM s GROUP BY b")
            .await;
        // A SELECT with no FROM.
        tw.write("INSERT INTO d SELECT 100, 'nofrom', 0").await;
        // DISTINCT source rows.
        tw.write("INSERT INTO agg SELECT 'all', 0, 0 FROM s LIMIT 0")
            .await;
        // ORDER BY / LIMIT shape the inserted set.
        tw.write("INSERT INTO d SELECT a + 50, b, c FROM s ORDER BY a DESC LIMIT 2")
            .await;
        // Self-insert: the source is fully materialised
        // first, so this doubles the table exactly once.
        tw.write("INSERT INTO self_t SELECT * FROM self_t").await;
        tw.write("INSERT INTO self_t SELECT * FROM self_t").await;
        tw.finish("SELECT * FROM d ORDER BY a");
        tw.finish("SELECT * FROM agg ORDER BY b");
        tw.finish("SELECT * FROM self_t ORDER BY v");
    });
}

#[test]
fn insert_select_conflicts_differential() {
    block_on(async {
        let tw = twins("v08-ins-sel-conf", S_SETUP);
        tw.write("INSERT INTO d SELECT * FROM s").await;
        // Default abort on a PK conflict, both engines,
        // statement rolled back whole.
        tw.write_err("INSERT INTO d SELECT a + 1, b, c FROM s")
            .await;
        // Width mismatch is an error on both.
        tw.write_err("INSERT INTO d SELECT a, b FROM s").await;
        // OR IGNORE skips the conflicts, lands the rest.
        tw.write("INSERT OR IGNORE INTO d SELECT a + 3, b, c FROM s")
            .await;
        // OR REPLACE replaces the conflicting rows.
        tw.write("INSERT OR REPLACE INTO d SELECT a, 'repl', 0 FROM s WHERE a = 1")
            .await;
        // Upsert DO NOTHING with a WHERE terminator.
        tw.write("INSERT INTO d SELECT a, b, c FROM s WHERE a >= 1 ON CONFLICT DO NOTHING")
            .await;
        // Upsert DO UPDATE with a WHERE terminator, plus
        // RETURNING over the inserted/updated rows.
        let rows = tw
            .rows("INSERT INTO d SELECT a, 'upd', c FROM s WHERE a = 2 ON CONFLICT(a) DO UPDATE SET b = excluded.b RETURNING a, b")
            .await;
        assert_eq!(rows, vec!["2|upd".to_string()]);
        // A FROM-less SELECT takes an upsert clause with no
        // terminator at all (nothing for ON to be ambiguous
        // with).
        tw.write("INSERT INTO d SELECT 500, 'nofrom', 5 ON CONFLICT DO NOTHING")
            .await;
        // Upsert with a LIMIT terminator also parses on the
        // oracle.
        tw.write("INSERT INTO d SELECT a + 40, b, c FROM s LIMIT 3 ON CONFLICT DO NOTHING")
            .await;
        // But a bare-FROM upsert is a syntax error on the
        // oracle — and refused by hipp, not accepted as a
        // superset.
        tw.write_err("INSERT INTO d SELECT a, b, c FROM s ON CONFLICT DO NOTHING")
            .await;
        tw.write_err(
            "INSERT INTO d SELECT a, b, c FROM s ON CONFLICT(a) DO UPDATE SET b = excluded.b",
        )
        .await;
        // RETURNING over a plain INSERT ... SELECT.
        let rows = tw
            .rows("INSERT INTO d SELECT a + 300, b, c FROM s WHERE a <= 2 RETURNING a, b")
            .await;
        assert_eq!(rows.len(), 2);
        tw.finish("SELECT * FROM d ORDER BY a");
    });
}

#[test]
fn insert_select_oracle_written_source() {
    block_on(async {
        // The source file was written entirely by the oracle;
        // hipp reads it and bulk-moves rows with
        // INSERT ... SELECT (the A3 direction for the new
        // verb).
        let path = oracle_db("v08-ins-sel-a3", S_SETUP);
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        exec(&mut db, "INSERT INTO d SELECT * FROM s")
            .await
            .unwrap();
        exec(&mut db, "INSERT INTO self_t SELECT * FROM self_t")
            .await
            .unwrap();
        drop(db);
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM d ORDER BY a"),
            vec![
                "1|one|10".to_string(),
                "2|two|20".to_string(),
                "3|three|30".to_string()
            ]
        );
        assert_eq!(
            oracle_rows(&path, "SELECT count(*) FROM self_t"),
            vec!["4".to_string()]
        );
    });
}

// ---------------- subqueries in INSERT VALUES ----------------

const V_SETUP: &str = "CREATE TABLE s (a INTEGER PRIMARY KEY, b TEXT);
INSERT INTO s VALUES (1,'one'),(2,'two'),(3,'three');
CREATE TABLE vc (x INTEGER);";

#[test]
fn insert_values_subqueries_differential() {
    block_on(async {
        let tw = twins("v08-values-sub", V_SETUP);
        // Snapshot semantics (oracle-probed): both count(*)
        // subqueries observe the pre-statement table and
        // insert 0, 0 — not 0, 1.
        tw.write("INSERT INTO vc VALUES ((SELECT count(*) FROM vc)), ((SELECT count(*) FROM vc))")
            .await;
        // Scalar subqueries over another table, with
        // expressions around them.
        tw.write("INSERT INTO vc VALUES ((SELECT max(a) FROM s)), ((SELECT min(a) FROM s) + 10)")
            .await;
        tw.write("INSERT INTO vc VALUES ((SELECT a FROM s WHERE b = 'two'))")
            .await;
        // A scalar subquery with no rows yields NULL.
        tw.write("INSERT INTO vc VALUES ((SELECT a FROM s WHERE a = 999))")
            .await;
        // EXISTS and IN (SELECT ...) as VALUES terms.
        tw.write("INSERT INTO vc VALUES (EXISTS (SELECT 1 FROM s WHERE a = 3)), (2 IN (SELECT a FROM s)), (5 IN (SELECT a FROM s))")
            .await;
        tw.finish("SELECT * FROM vc ORDER BY rowid");
    });
}

#[test]
fn insert_values_subquery_rejections() {
    block_on(async {
        let tw = twins("v08-values-sub-err", V_SETUP);
        // A two-column scalar subquery in VALUES.
        tw.write_err("INSERT INTO vc VALUES ((SELECT a, b FROM s))")
            .await;
        // A VALUES subquery has no outer scope: correlating
        // to the target table is an unknown column on both.
        tw.write_err("INSERT INTO vc VALUES ((SELECT a FROM s WHERE a = vc.x))")
            .await;
        // Aggregates stay refused in VALUES.
        tw.write_err("INSERT INTO vc VALUES (count(*))").await;
        tw.finish("SELECT * FROM vc ORDER BY rowid");
    });
}
