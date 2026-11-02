//! v0.16 collating sequences and pattern matching,
//! differentially vs the pinned oracle (SQLite 3.54.1). The
//! semantics were probed on the pin before implementation; the
//! matrix the tests encode:
//!
//! - Column `COLLATE BINARY | NOCASE | RTRIM` (ungated — it used
//!   to be parsed and ignored, a silent wrong answer): the pin's
//!   selection rule for every comparison — an explicit `COLLATE`
//!   on the left operand, else one on the right, else the left
//!   operand's column collation, else the right's, else BINARY
//!   (`=`, `<`, IS, BETWEEN, CASE, joins' ON / USING, correlated
//!   subqueries). An IN list compares by its left operand's
//!   collation only (a single constant item follows the binary
//!   rule: the pin rewrites it to `=`); `x IN (SELECT ...)` by
//!   the binary rule against the subquery's — rightmost core's —
//!   item. min / max / nullif use their first argument with a
//!   collation; ORDER BY, GROUP BY, DISTINCT, count(DISTINCT),
//!   group_concat(DISTINCT) and compounds use each term's /
//!   column's (a compound column's is its leftmost core's that
//!   has one). Derived-table columns carry their item's.
//! - Ties: ORDER BY is stable, DISTINCT / GROUP BY keep the
//!   first row, min() / max() aggregates keep the first extreme,
//!   the scalar max() keeps the earlier argument and min() the
//!   later one (`min(1, 1.0)` is 1.0 — an ungated fix).
//! - GROUP BY with as many ORDER BY terms takes their ASC / DESC
//!   for its group order (ungated fix: `GROUP BY a ORDER BY b
//!   DESC` listed equal-`b` groups ascending).
//! - Compounds run the pin's merge: each side sorted by the
//!   compound key, equal rows advance the left side (a UNION
//!   keeps the right row), duplicates dropped against the last
//!   emitted row under the operator's own collations; an ORDER
//!   BY with a different collation sorts the finished compound.
//! - Indexes (ungated): key columns order and match by their
//!   collation (an explicit `COLLATE` in CREATE INDEX / a table
//!   UNIQUE, else the column's), so UNIQUE NOCASE / RTRIM
//!   constraints, OR IGNORE / OR REPLACE and upsert targets
//!   behave as on the pin, files written by either engine pass
//!   the other's integrity check, and tables with such indexes
//!   are writable (they used to be refused as foreign).
//! - Unknown collation names are refused (`UnknownCollation`).
//! - Under `v16`: the `expr COLLATE name` operator, GLOB / NOT
//!   GLOB, `LIKE ... ESCAPE`, `like()` / `glob()`; LIKE folds
//!   ASCII only, both read operands up to their first NUL.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

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
        out.status.success() && out.stderr.is_empty(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
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
    let hipp = common::temp_db(&format!("v16co-{tag}-h"));
    let oracle = common::temp_db(&format!("v16co-{tag}-o"));
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

    /// The oracle reads the hipp file clean and both twins hold
    /// the same rows for `dump`.
    fn finish(&self, dump: &str) {
        assert_eq!(oracle_raw(&self.hipp, "PRAGMA integrity_check;").1, ["ok"]);
        assert_eq!(oracle_raw(&self.hipp, dump), oracle_raw(&self.oracle, dump));
    }
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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

/// SQL string literal for `s` (single quotes doubled).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

const TABLE: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT COLLATE NOCASE, \
     r TEXT COLLATE RTRIM, b TEXT, x COLLATE BINARY); \
     INSERT INTO t VALUES (1, 'abc', 'x  ', 'ABC', 'x'), (2, 'B', 'y', 'b', 'y  '), \
     (3, 'a', NULL, 'A', 'z'), (4, 'ABC', 'x', 'abc', 'X'), (5, NULL, 'Y', NULL, NULL), \
     (6, 'b', 'y ', 'B', 'y')";

// ---------------- column collations (ungated) ----------------

#[test]
fn column_collation_drives_comparisons() {
    let tw = twins("cmp", TABLE);
    tw.all(&[
        // Left column wins, else the right column's; literals
        // carry none.
        "SELECT id, n = b, b = n, n = 'ABC', 'ABC' = n, b = 'abc', r = 'x', 'x' = r, \
         r = x, x = r, n = x, x = n FROM t ORDER BY id",
        "SELECT id, n < 'b', n >= 'B', r > 'x', r <= 'x ', n <> 'A', n IS 'A', \
         n IS NOT 'A', r IS 'Y   ' FROM t ORDER BY id",
        "SELECT id FROM t WHERE n BETWEEN 'A' AND 'AZ' ORDER BY id",
        "SELECT id FROM t WHERE n NOT BETWEEN 'b' AND 'B' ORDER BY id",
        // CASE compares like `=`.
        "SELECT id, CASE n WHEN 'ABC' THEN 1 WHEN 'b' THEN 2 ELSE 0 END, \
         CASE 'abc' WHEN n THEN 'hit' END, CASE b WHEN n THEN 'eq' ELSE 'ne' END \
         FROM t ORDER BY id",
        // IN lists: the left operand alone, except one constant.
        "SELECT id, n IN ('abc', 'b'), 'ABC' IN (n), b IN (n), 'ABC' IN (n, 'q'), \
         n IN ('A'), n NOT IN ('A', 'q') FROM t ORDER BY id",
        // Functions carry no collation; CAST and unary + do.
        "SELECT id FROM t WHERE upper(n) = 'abc' ORDER BY id",
        "SELECT id FROM t WHERE n || '' = 'ABC' ORDER BY id",
        "SELECT id FROM t WHERE CAST(n AS TEXT) = 'ABC' ORDER BY id",
        "SELECT id FROM t WHERE +n = 'ABC' ORDER BY id",
        // nullif / scalar min / max: the first argument with one.
        "SELECT id, nullif(n, 'ABC'), nullif('ABC', n), max(n, 'Z'), min(b, n), \
         max(r, 'x '), min('y', r) FROM t ORDER BY id",
        // Rowid / IPK carry none: the other side's decides.
        "SELECT id FROM t WHERE id = n OR rowid = b ORDER BY id",
    ]);
}

#[test]
fn ordering_grouping_and_distinct_use_collations() {
    let tw = twins("ord", TABLE);
    tw.all(&[
        "SELECT n, id FROM t ORDER BY n",
        "SELECT n, id FROM t ORDER BY n DESC",
        "SELECT r, id FROM t ORDER BY r, id DESC",
        "SELECT b, id FROM t ORDER BY 1",
        "SELECT n AS z, id FROM t ORDER BY z NULLS LAST",
        // Groups form and sort by NOCASE; bare columns read the
        // group's first row.
        "SELECT n, count(*), group_concat(id), b FROM t GROUP BY n",
        "SELECT r, count(*), min(id) FROM t GROUP BY r",
        "SELECT n, sum(id) FROM t GROUP BY n HAVING n > 'A' ORDER BY n DESC",
        "SELECT DISTINCT n FROM t",
        "SELECT DISTINCT r FROM t",
        "SELECT DISTINCT b FROM t",
        "SELECT DISTINCT n, r FROM t",
        // Aggregates over a collated argument.
        "SELECT min(n), max(n), min(r), max(r), min(b), max(b) FROM t",
        "SELECT count(DISTINCT n), count(DISTINCT r), count(DISTINCT b), \
         group_concat(DISTINCT n), sum(DISTINCT id) FROM t",
        // The extremum row supplies bare columns.
        "SELECT max(n), id FROM t",
        "SELECT min(n), id FROM t",
        "SELECT n, max(id), b FROM t GROUP BY n",
    ]);
}

#[test]
fn ties_follow_the_pins_rules() {
    let tw = twins(
        "ties",
        "CREATE TABLE t(a TEXT COLLATE NOCASE, b TEXT); \
         INSERT INTO t VALUES ('B','1'),('a','2'),('b','3'),('A','4'),('x','5'),('X','6'),('x','7')",
    );
    tw.all(&[
        "SELECT max(a), min(a) FROM t",
        "SELECT max(a), b FROM t",
        "SELECT min(a), b FROM t",
        "SELECT a, b, count(*) FROM t GROUP BY a",
        "SELECT a, max(b) FROM t GROUP BY a",
        "SELECT DISTINCT a FROM t",
        "SELECT a, b FROM t ORDER BY a",
        "SELECT group_concat(DISTINCT a), count(DISTINCT a) FROM t",
        // Regression (ungated): the scalar min() takes the later
        // of equal arguments, max() the earlier.
        "SELECT min(1, 1.0), max(1, 1.0), min(1.0, 1), max(1.0, 1)",
        "SELECT min(a, 'A'), max(a, 'A'), max('A', a) FROM t ORDER BY b",
    ]);
}

/// Regression (ungated, found while probing GROUP BY order):
/// with as many ORDER BY terms as GROUP BY terms the pin's group
/// sorter takes the ORDER BY directions, so ORDER BY ties keep a
/// descending group order. hipp always grouped ascending.
#[test]
fn group_order_takes_order_by_directions() {
    let tw = twins(
        "grpdir",
        "CREATE TABLE u(a, b); INSERT INTO u VALUES (1,'x'),(2,'x'),(3,'y'),(NULL,'x')",
    );
    tw.all(&[
        "SELECT a, b FROM u GROUP BY a ORDER BY b DESC",
        "SELECT a, b FROM u GROUP BY a ORDER BY b",
        "SELECT a, b FROM u GROUP BY a, b ORDER BY b DESC, a",
        "SELECT a, b FROM u GROUP BY a ORDER BY b DESC NULLS FIRST",
        "SELECT a, count(*) FROM u GROUP BY a ORDER BY count(*) DESC",
        "SELECT b, count(*) FROM u GROUP BY b ORDER BY count(*) DESC, b",
    ]);
}

/// Regression (ungated, found while probing min / max ties):
/// bare columns come from the extremum row of the LAST min() /
/// max() aggregate, whatever other aggregates the query has,
/// and a group whose min / max argument is all NULL reads its
/// last row. hipp honoured only a lone min / max aggregate and
/// read the first row otherwise.
#[test]
fn bare_columns_follow_the_last_minmax_aggregate() {
    let tw = twins(
        "bare",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, x, y, g, n TEXT COLLATE NOCASE); \
         INSERT INTO t VALUES (1, NULL, 9, 1, 'b'), (2, NULL, 4, 1, 'B'), (3, 5, 1, 2, 'a'), \
         (4, NULL, 2, 2, 'A'), (5, 7, NULL, 2, 'c')",
    );
    tw.all(&[
        "SELECT count(*), id FROM t",
        "SELECT max(x), id FROM t",
        "SELECT max(x), id FROM t GROUP BY g",
        "SELECT min(x), count(*), id FROM t GROUP BY g",
        "SELECT max(x), min(y), id FROM t GROUP BY g",
        "SELECT min(y), max(x), id FROM t GROUP BY g",
        "SELECT max(y), id FROM t GROUP BY g HAVING min(x) > 0",
        "SELECT g, id FROM t GROUP BY g ORDER BY max(y)",
        "SELECT id, max(y), sum(x) FROM t WHERE x IS NULL",
        "SELECT max(n), count(*), id FROM t",
        "SELECT min(n), id FROM t GROUP BY g",
        "SELECT (SELECT max(n) FROM t AS u WHERE u.g = t.g), max(id), id FROM t GROUP BY g",
    ]);
}

#[test]
fn compounds_merge_like_the_pin() {
    let tw = twins(
        "comp",
        "CREATE TABLE t(a TEXT COLLATE NOCASE, b TEXT); \
         INSERT INTO t VALUES ('B','b'),('a','A'),('b','B'),('A','a'),('x','X')",
    );
    tw.all(&[
        // The leftmost core with a collation decides; a UNION
        // keeps the right side's row of an equal pair.
        "SELECT a FROM t UNION SELECT b FROM t",
        "SELECT b FROM t UNION SELECT a FROM t",
        "SELECT a FROM t UNION SELECT a FROM t",
        "SELECT 'X' UNION SELECT a FROM t",
        "SELECT a FROM t UNION SELECT 'X'",
        "SELECT a FROM t UNION SELECT 'q'",
        "SELECT b FROM t INTERSECT SELECT a FROM t",
        "SELECT a FROM t INTERSECT SELECT b FROM t",
        "SELECT a FROM t EXCEPT SELECT 'b'",
        "SELECT 'b' EXCEPT SELECT a FROM t",
        "SELECT a FROM t INTERSECT SELECT 'B' UNION SELECT 'c'",
        "SELECT a FROM t UNION ALL SELECT 'X' UNION SELECT 'q'",
        "SELECT a FROM t UNION SELECT b FROM t UNION ALL SELECT 'q'",
        // An inner operator dedups by its own collations while
        // sorting by the key pushed down from the outer one.
        "SELECT 'x' UNION SELECT 'X' UNION SELECT a FROM t",
        "SELECT 'x' UNION SELECT 'X' UNION ALL SELECT a FROM t",
        // A multi-row VALUES core reads as a BINARY column.
        "VALUES ('X'), ('x') UNION SELECT a FROM t",
        "SELECT a FROM t UNION VALUES ('X'), ('q')",
        "SELECT a, b FROM t UNION SELECT b, a FROM t",
        "SELECT a, b FROM t UNION SELECT b, a FROM t ORDER BY 2",
        "SELECT a FROM t UNION SELECT b FROM t ORDER BY 1 DESC",
        "SELECT a FROM t UNION ALL SELECT b FROM t ORDER BY 1",
        "SELECT a FROM t UNION ALL SELECT b FROM t ORDER BY 1 DESC",
        "SELECT a FROM t UNION SELECT 'X' LIMIT 2 OFFSET 1",
        // Compounds inside subqueries / derived tables.
        "SELECT * FROM (SELECT a FROM t UNION SELECT 'Y') ORDER BY 1",
        "SELECT max(a) FROM (SELECT a FROM t UNION ALL SELECT 'Z')",
        "SELECT a, count(*) FROM (SELECT a FROM t UNION ALL SELECT 'Y') GROUP BY a",
        // IN over a compound compares by its rightmost core.
        "SELECT 'X' IN (SELECT a FROM t UNION SELECT 'q'), \
         'X' IN (SELECT 'q' UNION SELECT a FROM t)",
    ]);
}

#[test]
fn subqueries_derived_tables_and_joins() {
    let tw = twins(
        "subq",
        "CREATE TABLE a(k TEXT COLLATE NOCASE, v); CREATE TABLE b(k TEXT, w); \
         INSERT INTO a VALUES ('x',1),('Y',2),('z',3); \
         INSERT INTO b VALUES ('X',10),('y',20),('Z ',30)",
    );
    tw.all(&[
        "SELECT * FROM a JOIN b USING (k) ORDER BY v",
        "SELECT * FROM b JOIN a USING (k) ORDER BY v",
        "SELECT * FROM a JOIN b ON a.k = b.k ORDER BY v",
        "SELECT * FROM a JOIN b ON b.k = a.k ORDER BY v",
        "SELECT * FROM a NATURAL JOIN b ORDER BY v",
        "SELECT * FROM a LEFT JOIN b ON b.k = a.k ORDER BY v",
        // IN (SELECT): the binary rule against the item.
        "SELECT k FROM a WHERE k IN (SELECT k FROM b) ORDER BY k",
        "SELECT k FROM b WHERE k IN (SELECT k FROM a) ORDER BY k",
        "SELECT k FROM b WHERE k NOT IN (SELECT k FROM a) ORDER BY k",
        "SELECT 'X' IN (SELECT k FROM a), 'X' IN (SELECT k FROM b)",
        // A scalar subquery carries no collation.
        "SELECT (SELECT k FROM a ORDER BY v LIMIT 1) = 'X'",
        "SELECT v, (SELECT count(*) FROM b WHERE b.k = a.k), \
         (SELECT count(*) FROM b WHERE a.k = b.k) FROM a ORDER BY v",
        // Derived columns carry their item's collation.
        "SELECT c FROM (SELECT k AS c FROM a) WHERE c = 'X'",
        "SELECT c FROM (SELECT k || '' AS c FROM a) WHERE c = 'X'",
        "SELECT c FROM (SELECT k AS c FROM b) WHERE c = 'x'",
        "SELECT count(*) FROM (SELECT 'X' AS c UNION ALL SELECT k FROM a) WHERE c = 'x'",
        "SELECT count(*) FROM (SELECT k AS c FROM a UNION ALL SELECT 'X') WHERE c = 'x'",
        "SELECT count(*) FROM (SELECT 'X' AS c, k FROM a) WHERE k = c",
        "SELECT count(*) FROM (SELECT 'X' AS c, k FROM a LIMIT 9) WHERE c = k",
    ]);
}

#[test]
fn constraints_and_writes_compare_by_collation() {
    let tw = twins(
        "writes",
        "CREATE TABLE t(a COLLATE NOCASE CHECK (a <> 'bad'), b COLLATE RTRIM, \
         g AS (a = 'X')); \
         INSERT INTO t(a, b) VALUES ('A','x'),('a','x '),('B','y'),('x','q')",
    );
    tw.refuse(
        "INSERT INTO t(a, b) VALUES ('BAD', 'z')",
        SchemaKind::CheckViolation,
    );
    tw.all(&[
        "UPDATE t SET b = 'z' WHERE a = 'a'",
        "DELETE FROM t WHERE b = 'y   '",
        "SELECT a, b, g FROM t",
        "SELECT * FROM t WHERE a IN ('a') ORDER BY b",
    ]);
    tw.finish("SELECT a, b, g FROM t ORDER BY rowid");
}

// ---------------- indexes (ungated) ----------------

#[test]
fn unique_nocase_and_rtrim_constraints() {
    let tw = twins(
        "uniq",
        "CREATE TABLE t(a TEXT UNIQUE COLLATE NOCASE, b, UNIQUE (b COLLATE RTRIM DESC, a)); \
         INSERT INTO t VALUES ('x', 'p')",
    );
    tw.refuse(
        "INSERT INTO t VALUES ('X', 'q')",
        SchemaKind::UniqueViolation,
    );
    tw.all(&[
        "INSERT OR IGNORE INTO t VALUES ('X', 'q')",
        "INSERT INTO t VALUES ('y', 'p')",
        "INSERT OR IGNORE INTO t VALUES ('Y', 'p  ')",
        "INSERT OR REPLACE INTO t VALUES ('X', 'r')",
        "INSERT INTO t VALUES ('w', 'r ')",
        "SELECT * FROM t ORDER BY rowid",
        "SELECT * FROM t WHERE a = 'x'",
        "SELECT * FROM t WHERE b = 'p   ' ORDER BY a",
    ]);
    tw.refuse(
        "UPDATE t SET a = 'W' WHERE a = 'x'",
        SchemaKind::UniqueViolation,
    );
    tw.all(&[
        "UPDATE OR REPLACE t SET a = 'W' WHERE a = 'x'",
        "UPDATE t SET a = upper(a)",
        "SELECT * FROM t ORDER BY rowid",
    ]);
    tw.finish("SELECT * FROM t ORDER BY rowid");
}

#[test]
fn create_index_collate_orders_and_finds_keys() {
    let tw = twins(
        "cidx",
        "CREATE TABLE t(a COLLATE NOCASE, b COLLATE RTRIM, c); \
         CREATE INDEX i1 ON t(a); CREATE INDEX i2 ON t(b, a DESC); \
         CREATE UNIQUE INDEX i3 ON t(c COLLATE NOCASE, b); \
         CREATE INDEX i4 ON t(a COLLATE BINARY); \
         INSERT INTO t VALUES ('b','x','q'),('A','x ','r'),('a','y','Q'),('B','y ','s'),(NULL,NULL,NULL),(3,3,3)",
    );
    tw.refuse(
        "INSERT INTO t VALUES ('z','y  ','S')",
        SchemaKind::UniqueViolation,
    );
    tw.all(&[
        "SELECT c FROM t WHERE a = 'a'",
        "SELECT c FROM t WHERE b = 'x'",
        "SELECT a FROM t ORDER BY a",
        "UPDATE t SET a = upper(a) WHERE c > 'q'",
        "DELETE FROM t WHERE a = 'b' AND c = 'q'",
        "INSERT INTO t VALUES ('Q','z','T')",
        "SELECT * FROM t WHERE a = 'a'",
    ]);
    tw.finish("SELECT * FROM t ORDER BY rowid");
    // The index trees order by collation: the oracle reads them
    // back in its own index order.
    assert_eq!(
        oracle_raw(&tw.hipp, "SELECT a FROM t INDEXED BY i1 WHERE a > 'A'"),
        oracle_raw(&tw.oracle, "SELECT a FROM t INDEXED BY i1 WHERE a > 'A'")
    );
}

#[test]
fn upsert_targets_match_index_collations() {
    let tw = twins(
        "upsert",
        "CREATE TABLE t(a TEXT COLLATE NOCASE PRIMARY KEY, b); INSERT INTO t VALUES ('x', 1)",
    );
    tw.all(&[
        "INSERT INTO t VALUES ('X', 2) ON CONFLICT (a) DO UPDATE SET b = b + 10",
        "INSERT INTO t VALUES ('X', 2) ON CONFLICT (a COLLATE nocase) DO UPDATE SET b = b + 100",
        "SELECT * FROM t",
    ]);
    // A target COLLATE naming another collation matches no index.
    assert!(
        tw.same("INSERT INTO t VALUES ('X', 2) ON CONFLICT (a COLLATE BINARY) DO NOTHING")
            .is_some()
    );
    tw.finish("SELECT * FROM t");
}

/// An oracle-written file with NOCASE / RTRIM indexes: hipp
/// used to refuse every write to such a table (foreign index);
/// now it maintains the indexes and the oracle checks them.
#[test]
fn oracle_written_collated_indexes_are_writable() {
    let path = common::temp_db("v16co-foreign");
    let setup = "CREATE TABLE t(a TEXT COLLATE NOCASE UNIQUE, b COLLATE RTRIM, c); \
                 CREATE INDEX ib ON t(b, c DESC); CREATE INDEX ic ON t(c COLLATE NOCASE); \
                 INSERT INTO t VALUES ('Abc','x  ','Q'),('d','y','q');";
    assert!(oracle_raw(&path, setup).0);
    block_on(async {
        let mut db = open(&path).await;
        match try_collect(&mut db, "INSERT INTO t VALUES ('ABC', 'z', 'w')").await {
            Err(Error::Schema {
                kind: SchemaKind::UniqueViolation,
            }) => {}
            other => panic!("expected a NOCASE unique violation, got {other:?}"),
        }
        for sql in [
            "INSERT INTO t VALUES ('e', 'x', 'W')",
            "UPDATE t SET c = 'QQ' WHERE c = 'q'",
            "DELETE FROM t WHERE b = 'x'",
            "INSERT INTO t VALUES ('F', 'y ', 'f')",
        ] {
            try_collect(&mut db, sql).await.expect(sql);
        }
        assert_eq!(
            try_collect(&mut db, "SELECT a FROM t WHERE a = 'D'")
                .await
                .expect("select"),
            ["d"]
        );
    });
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
    assert_eq!(
        oracle_raw(&path, "SELECT * FROM t ORDER BY rowid;").1,
        ["d|y|QQ", "F|y |f"]
    );
}

#[test]
fn unknown_collations_are_refused() {
    let tw = twins("unknown", "CREATE TABLE t(a)");
    tw.refuse(
        "CREATE TABLE u(a COLLATE foo)",
        SchemaKind::UnknownCollation,
    );
    tw.refuse(
        "CREATE TABLE u(a, UNIQUE (a COLLATE foo))",
        SchemaKind::UnknownCollation,
    );
    tw.refuse(
        "CREATE INDEX i ON t(a COLLATE foo)",
        SchemaKind::UnknownCollation,
    );
    tw.all(&[
        "CREATE TABLE v(a COLLATE \"NoCase\", b COLLATE 'rtrim', c COLLATE Binary)",
        "INSERT INTO v VALUES ('A', 'b ', 'C')",
        "SELECT a = 'a', b = 'b', c = 'c' FROM v",
    ]);
}

// ---------------- the COLLATE operator (v16) ----------------

#[cfg(feature = "v16")]
#[test]
fn collate_operator_wins_comparisons() {
    let tw = twins("op", TABLE);
    tw.all(&[
        "SELECT 'a' = 'A' COLLATE NOCASE, 'a ' = 'a' COLLATE RTRIM, 'a' COLLATE NOCASE = 'A', \
         'a' < 'B' COLLATE NOCASE, 'a' COLLATE BINARY = 'A' COLLATE NOCASE, \
         'a' COLLATE NOCASE = 'A' COLLATE BINARY",
        "SELECT 'é' = 'É' COLLATE NOCASE, 'a  ' = 'a' COLLATE RTRIM, ' a' = 'a' COLLATE RTRIM, \
         'a ' < 'a' COLLATE RTRIM, 'a ' > 'a' COLLATE RTRIM",
        "SELECT 1 = '1' COLLATE NOCASE, x'41' = 'a' COLLATE NOCASE, x'61' = 'a' COLLATE NOCASE",
        // An explicit collation inside an operand lends it.
        "SELECT ('a' COLLATE NOCASE) || 'x' = 'AX', upper('a' COLLATE NOCASE) = 'a', \
         ('a' COLLATE RTRIM) COLLATE NOCASE = 'A ', 'a' COLLATE RTRIM = ('A ' COLLATE NOCASE)",
        "SELECT 'A' IN ('a' COLLATE NOCASE), 'A' COLLATE NOCASE IN ('a'), \
         'A' IN ('a' COLLATE NOCASE, 'b', 'c'), CASE 'A' WHEN 'a' COLLATE NOCASE THEN 1 ELSE 0 END, \
         'A' BETWEEN 'a' COLLATE NOCASE AND 'a', nullif('A', 'a' COLLATE NOCASE), \
         coalesce('A' COLLATE NOCASE, 1) = 'a', ('A' COLLATE NOCASE) IS 'a', \
         iif(1, 'A' COLLATE NOCASE, 'b') = 'a', (SELECT 'A' COLLATE NOCASE) = 'a', \
         -('1' COLLATE NOCASE), typeof('a' COLLATE BINARY)",
        "SELECT 'a' COLLATE nOcAsE = 'A', 'a' COLLATE \"rtrim\" = 'a ', 'a' COLLATE 'BINARY' = 'A'",
        // Over columns: the explicit side beats a column's.
        "SELECT id FROM t WHERE n COLLATE BINARY = 'ABC' ORDER BY id",
        "SELECT id FROM t WHERE b = 'abc' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM t WHERE r COLLATE NOCASE = 'X' ORDER BY id",
        // ORDER BY / GROUP BY / DISTINCT terms.
        "SELECT b, id FROM t ORDER BY b COLLATE NOCASE, id",
        "SELECT n, id FROM t ORDER BY n COLLATE BINARY",
        "SELECT n FROM t ORDER BY 1 COLLATE BINARY DESC",
        "SELECT n AS z FROM t ORDER BY z COLLATE BINARY",
        "SELECT b COLLATE NOCASE, count(*) FROM t GROUP BY 1",
        "SELECT count(*) FROM t GROUP BY b COLLATE NOCASE",
        "SELECT DISTINCT b COLLATE NOCASE FROM t",
        "SELECT DISTINCT n COLLATE BINARY FROM t",
        "SELECT max(n COLLATE BINARY), min(b COLLATE NOCASE), \
         count(DISTINCT n COLLATE BINARY) FROM t",
        "SELECT b, count(*) FROM t GROUP BY 1 ORDER BY 1 COLLATE NOCASE DESC",
        // Compound ORDER BY with another collation sorts the
        // finished compound.
        "SELECT n FROM t UNION SELECT b FROM t ORDER BY 1 COLLATE BINARY",
        "SELECT b FROM t UNION SELECT n FROM t ORDER BY 1 COLLATE NOCASE",
        "SELECT n, b FROM t UNION SELECT b, n FROM t ORDER BY 2 COLLATE NOCASE",
        "SELECT n FROM t UNION ALL SELECT b FROM t ORDER BY 1 COLLATE BINARY",
        "SELECT n FROM t UNION ALL SELECT b FROM t UNION SELECT 'q' ORDER BY 1 COLLATE BINARY",
        "SELECT n FROM t UNION SELECT b FROM t UNION ALL SELECT 'q' ORDER BY 1 COLLATE BINARY",
        "SELECT b COLLATE NOCASE FROM t UNION SELECT n FROM t",
    ]);
    tw.refuse(
        "SELECT n FROM t ORDER BY n COLLATE bogus",
        SchemaKind::UnknownCollation,
    );
    tw.refuse(
        "SELECT 'a' COLLATE bogus = 'a'",
        SchemaKind::UnknownCollation,
    );
}

// ---------------- LIKE / GLOB (v16) ----------------

#[test]
fn like_folds_ascii_only_and_reads_to_nul() {
    // Ungated: the LIKE operator's matcher.
    let tw = twins(
        "like",
        "CREATE TABLE s(x); INSERT INTO s VALUES ('ab'),('AB'),('é'),(1),(2.5)",
    );
    tw.all(&[
        "SELECT 'abc' LIKE 'A_C', 'ÄBC' LIKE 'äbc', 'é' LIKE 'É', 'aé' LIKE 'a_', \
         'abc' LIKE '%B%', 'abc' NOT LIKE 'b%', NULL LIKE 'a', 'a' LIKE NULL",
        "SELECT CAST(x'610062' AS TEXT) LIKE 'a', 'a' LIKE CAST(x'610062' AS TEXT), \
         x'41' LIKE 'a', 'a' LIKE x'41', 12 LIKE '1%', 2.5 LIKE '2._'",
        "SELECT x, x LIKE 'a%', x LIKE '%B', x NOT LIKE '_' FROM s ORDER BY rowid",
    ]);
    #[cfg(feature = "v16")]
    tw.same("SELECT 'a' COLLATE NOCASE LIKE 'A'");
    // The pattern-length limit (50,000 bytes, checked before
    // matching). A larger statement arena holds the literal.
    let cfg = Config::default().statement_arena(1 << 16, 1 << 22);
    for (n, ok) in [(50_000usize, true), (50_001, false)] {
        let sql = format!("SELECT 'a' LIKE {}", lit(&"%".repeat(n)));
        let got = block_on(async {
            let mut db = Connection::open(FileStorage::open(&tw.hipp).expect("fs"), cfg.clone())
                .await
                .expect("open");
            try_collect(&mut db, &sql).await
        });
        let (oracle_ok, want) = oracle_raw(&tw.oracle, &sql);
        assert_eq!(oracle_ok, ok, "oracle at {n} bytes");
        match got {
            Ok(rows) => {
                assert!(ok, "hipp ran a {n}-byte pattern");
                assert_eq!(rows, want);
            }
            Err(e) => {
                assert!(!ok, "hipp refused a {n}-byte pattern: {e:?}");
                assert!(matches!(
                    e,
                    Error::Schema {
                        kind: SchemaKind::PatternTooComplex
                    }
                ));
            }
        }
    }
}

#[cfg(feature = "v16")]
#[test]
fn glob_escape_and_the_pattern_functions() {
    let tw = twins("glob", "CREATE TABLE t(a)");
    tw.all(&[
        "SELECT 'abc' GLOB 'a*', 'abc' GLOB 'A*', 'abc' NOT GLOB '?b?', 'a]c' GLOB 'a[]]c', \
         'abc' GLOB 'a[^x]c', 'abc' GLOB 'a[a-c]c', 'a-c' GLOB 'a[a-]c', 'a-c' GLOB 'a[-a]c', \
         'abc' GLOB 'a[', 'a' GLOB '[', '' GLOB '*', '' GLOB '', NULL GLOB 'a', 'a' GLOB NULL",
        "SELECT 'a*c' GLOB 'a[*]c', 'abc' GLOB 'a[*]c', 'a?c' GLOB 'a[?]c', 'aXc' GLOB '*[X]*', \
         'abc' GLOB '*[^a]', 'ab' GLOB 'a[b-a]', 'ab' GLOB 'a[a-]', 'a]' GLOB 'a[^]]', \
         'ab' GLOB 'a[^]]', 'é' GLOB '[à-ê]', 'abcabc' GLOB '*b*b*', 'ab' GLOB 'a**?', \
         'ab' GLOB '*?*?*?', 'aé' GLOB 'a?'",
        "SELECT 'a%c' LIKE 'a\\%c' ESCAPE '\\', 'abc' LIKE 'a\\%c' ESCAPE '\\', \
         'a_c' LIKE 'a|_c' ESCAPE '|', 'abc' LIKE 'a|_c' ESCAPE '|', 'abc' LIKE 'abc|' ESCAPE '|', \
         'a%' LIKE 'a%%' ESCAPE '%', 'ab' LIKE 'a%%' ESCAPE '%', 'a' LIKE 'a' ESCAPE NULL, \
         'a_' LIKE 'a__' ESCAPE '_', 'ab' LIKE 'a__' ESCAPE '_', 'ab' LIKE 'a%' ESCAPE 'b', \
         'abc' LIKE '%' ESCAPE 'é', 'A' LIKE 'a' ESCAPE 'x', 'abc' NOT LIKE 'b%' ESCAPE '\\'",
        "SELECT 'a' LIKE 'a' ESCAPE 1, 'a1' LIKE 'a11' ESCAPE 1, 'x' LIKE 'X' ESCAPE x'41'",
        "SELECT like('a%', 'abc'), like('a\\%', 'a%', '\\'), glob('a*', 'abc'), glob('A*', 'abc'), \
         like(NULL, 'a'), glob('a', NULL), like('A', 'a', NULL), like('a', 'a', 'a')",
        "SELECT 1.5 GLOB '1.*', x'6162' GLOB 'a?', 'a' GLOB 'A' COLLATE NOCASE, \
         'ab' || 'c' LIKE 'abc', 'abc' LIKE 'ab' || 'c', 'abc' GLOB 'a*' AND 1",
        // An escape error needs a row to evaluate on.
        "SELECT a LIKE 'x' ESCAPE 'ab' FROM t",
    ]);
    tw.refuse(
        "SELECT 'abc' LIKE 'a%' ESCAPE ''",
        SchemaKind::EscapeNotSingleChar,
    );
    tw.refuse(
        "SELECT 'abc' LIKE 'a%' ESCAPE 'ab'",
        SchemaKind::EscapeNotSingleChar,
    );
    tw.refuse(
        "SELECT like('a', 'b', 'cd')",
        SchemaKind::EscapeNotSingleChar,
    );
    for bad in [
        "SELECT like('a')",
        "SELECT glob('a', 'b', 'c')",
        "SELECT like('a', 'b', 'c', 'd')",
        "SELECT 'abc' GLOB 'a*' ESCAPE 'x'",
    ] {
        assert!(tw.same(bad).is_some(), "both refuse {bad}");
    }
}

/// A deterministic corpus of random patterns and subjects over
/// the wildcard / class / escape / case alphabet: LIKE, LIKE
/// ESCAPE and GLOB answers must match the oracle's exactly.
#[cfg(feature = "v16")]
#[test]
fn generated_pattern_corpus_matches_oracle() {
    let tw = twins("pcorpus", "CREATE TABLE t(a)");
    let alphabet = [
        "a", "A", "b", "B", "%", "_", "*", "?", "[", "]", "^", "-", "\\", "é", " ", "c",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let make = |rng: &mut Rng, max: usize| -> String {
        let n = rng.below(max + 1);
        (0..n).map(|_| rng.pick(&alphabet)).collect()
    };
    for _ in 0..40 {
        let mut items = Vec::new();
        for _ in 0..6 {
            let s = make(&mut rng, 6);
            let p = make(&mut rng, 5);
            items.push(format!("{} LIKE {}", lit(&s), lit(&p)));
            items.push(format!("{} LIKE {} ESCAPE '\\'", lit(&s), lit(&p)));
            items.push(format!("{} GLOB {}", lit(&s), lit(&p)));
        }
        tw.same(&format!("SELECT {}", items.join(", ")));
    }
}

/// A deterministic corpus over collated columns: random data,
/// then random comparisons, ORDER BY / GROUP BY / DISTINCT and
/// compound queries, each compared with the oracle.
#[test]
fn generated_collation_corpus_matches_oracle() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let words = [
        "a", "A", "a ", "b", "B ", "ab", "AB", "aB ", "", " ", "é", "É", "c",
    ];
    let mut setup = String::from(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT COLLATE NOCASE, r TEXT COLLATE RTRIM, \
         b TEXT);",
    );
    let mut rows = Vec::new();
    for id in 1..=24 {
        let col = |rng: &mut Rng| {
            if rng.below(8) == 0 {
                "NULL".to_string()
            } else {
                lit(rng.pick(&words))
            }
        };
        rows.push(format!(
            "({id}, {}, {}, {})",
            col(&mut rng),
            col(&mut rng),
            col(&mut rng)
        ));
    }
    setup.push_str(&format!("INSERT INTO t VALUES {}", rows.join(", ")));
    let tw = twins("ccorpus", &setup);
    let cols = ["n", "r", "b"];
    let ops = ["=", "<", ">=", "<>", "IS"];
    for _ in 0..60 {
        let c1 = rng.pick(&cols);
        let c2 = rng.pick(&cols);
        let w = lit(rng.pick(&words));
        let op = rng.pick(&ops);
        let sql = match rng.below(7) {
            0 => format!("SELECT id FROM t WHERE {c1} {op} {w} ORDER BY id"),
            1 => format!("SELECT id FROM t WHERE {w} {op} {c1} ORDER BY id"),
            2 => format!("SELECT id, {c1} {op} {c2} FROM t ORDER BY id"),
            3 => format!("SELECT {c1}, id FROM t ORDER BY {c1}, id DESC"),
            4 => format!("SELECT {c1}, count(*), min(id) FROM t GROUP BY {c1}"),
            5 => format!("SELECT DISTINCT {c1} FROM t"),
            _ => format!("SELECT {c1} FROM t UNION SELECT {c2} FROM t"),
        };
        tw.same(&sql);
    }
}
