//! v0.4 dialect, differentially vs the pinned oracle (SPEC §2
//! v0.4): joins, aggregates + GROUP BY, the expression library,
//! ALTER TABLE ADD COLUMN, inert views. SELECT results are
//! compared row-for-row against the oracle CLI over the same
//! file; writes are verified from the oracle side.

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

/// Assert hipp and the oracle agree on `query` over the file at
/// `path` — row order included, so queries that need determinism
/// carry an ORDER BY (the oracle's own order is otherwise its
/// implementation detail, except GROUP BY, which sorts).
async fn diff_query(path: &std::path::Path, query: &str) {
    let oracle_out = common::cli(path, query);
    let want: Vec<String> = oracle_out.lines().map(|s| s.to_string()).collect();
    let fs = FileStorage::open(path).expect("fs open");
    let mut db = Connection::open(fs, Config::default())
        .await
        .expect("open");
    let got = collect(&mut db, query).await;
    assert_eq!(got, want, "query: {query}\noracle: {}", common::oracle_version());
}

fn oracle_db(tag: &str, setup: &str) -> std::path::PathBuf {
    let path = common::temp_db(tag);
    common::cli(&path, setup);
    path
}

// ---------------- joins ----------------

#[test]
fn joins_differential() {
    block_on(async {
        let path = oracle_db(
            "v04-joins",
            "CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT, k INTEGER);
             CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT, k INTEGER);
             CREATE TABLE c (id INTEGER PRIMARY KEY, z TEXT, b_id INTEGER);
             INSERT INTO a VALUES (1,'a1',10),(2,'a2',20),(3,'a3',30);
             INSERT INTO b VALUES (1,'b1',10),(2,'b2',99);
             INSERT INTO c VALUES (1,'c1',1),(2,'c2',1),(3,'c3',2);",
        );
        for q in [
            "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k ORDER BY a.id",
            "SELECT a.x, b.y FROM a INNER JOIN b ON a.k = b.k ORDER BY 1",
            "SELECT a.x, b.y FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id",
            "SELECT a.x, b.y FROM a LEFT OUTER JOIN b ON a.k = b.k ORDER BY a.id",
            "SELECT a.x, b.y FROM a, b WHERE a.k = b.k ORDER BY a.id",
            "SELECT a.x, b.y FROM a CROSS JOIN b ORDER BY a.id, b.id",
            "SELECT t1.x, t2.y FROM a AS t1 JOIN b AS t2 ON t1.k = t2.k",
            "SELECT a.x, c.z FROM a JOIN c ON c.b_id = a.id WHERE a.id < 3 ORDER BY c.id",
            "SELECT a.x, b.y, c.z FROM a JOIN b ON a.k = b.k JOIN c ON c.b_id = b.id ORDER BY a.id",
            "SELECT a1.x, a2.x FROM a a1 JOIN a a2 ON a1.k < a2.k ORDER BY a1.id, a2.id",
            "SELECT x, y FROM a JOIN b ON a.k = b.k AND b.y LIKE 'b%' ORDER BY x",
            "SELECT a.rowid, b.rowid FROM a JOIN b ON a.k = b.k",
            "SELECT count(*) FROM a LEFT JOIN b ON a.k = b.k",
            "SELECT a.x FROM a LEFT JOIN b ON a.k = b.k WHERE b.id IS NULL ORDER BY a.x",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- aggregates + GROUP BY ----------------

#[test]
fn aggregates_differential() {
    block_on(async {
        let path = oracle_db(
            "v04-agg",
            "CREATE TABLE s (id INTEGER PRIMARY KEY, g TEXT, v INTEGER, w REAL, t TEXT);
             INSERT INTO s VALUES (1,'x',10,1.5,'p'),(2,'x',20,2.5,'q'),(3,'y',NULL,3.0,'r'),
             (4,'y',5,NULL,NULL),(5,NULL,7,1.0,'s'),(6,'x',NULL,NULL,'t');
             CREATE TABLE empty_t (a INTEGER, b TEXT);",
        );
        for q in [
            "SELECT count(*) FROM s",
            "SELECT count(v), count(t), count(DISTINCT g) FROM s",
            "SELECT sum(v), total(v), avg(v) FROM s",
            "SELECT min(v), max(v), min(t), max(t) FROM s",
            "SELECT sum(w), avg(w), min(w), max(w) FROM s",
            "SELECT group_concat(t), group_concat(t, ';') FROM s WHERE t IS NOT NULL",
            "SELECT count(*) FROM s WHERE v > 6",
            "SELECT count(*), sum(a), avg(a), min(a), max(a), group_concat(b) FROM empty_t",
            "SELECT sum(b), total(b), count(b) FROM empty_t",
            "SELECT g, count(*), sum(v) FROM s GROUP BY g",
            "SELECT g, count(*) AS n, sum(v) AS sv FROM s GROUP BY g ORDER BY n DESC, g",
            "SELECT g, count(*) FROM s GROUP BY g HAVING count(*) > 1",
            "SELECT g, sum(v) FROM s GROUP BY g HAVING sum(v) >= 30",
            "SELECT v % 2 AS parity, count(*), sum(v) FROM s GROUP BY parity ORDER BY parity",
            "SELECT count(*) FROM s GROUP BY g",
            "SELECT g, group_concat(t, '+') FROM s WHERE t IS NOT NULL GROUP BY g ORDER BY g",
            "SELECT min(v), t FROM s",
            "SELECT max(v), t FROM s",
            "SELECT sum(DISTINCT v), count(DISTINCT v) FROM s",
            "SELECT upper(g), count(*) FROM s GROUP BY upper(g) ORDER BY 1",
            "SELECT count(*) FROM s HAVING count(*) > 2",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- expressions ----------------

#[test]
fn expressions_differential() {
    block_on(async {
        let path = oracle_db(
            "v04-expr",
            "CREATE TABLE e (id INTEGER PRIMARY KEY, i INTEGER, r REAL, t TEXT, b BLOB);
             INSERT INTO e VALUES (1, 7, 2.5, 'Hello World', x'00ff'),
             (2, -3, 0.5, 'abc', NULL), (3, NULL, NULL, '12abc', x'61'),
             (4, 100, 4.0, '', NULL);",
        );
        for q in [
            "SELECT i + 1, i - 1, i * 3, i / 2, i % 3 FROM e ORDER BY id",
            "SELECT r * 2, i + r, i / 0, r / 0, i % 0 FROM e ORDER BY id",
            "SELECT t || '!' || i FROM e ORDER BY id",
            "SELECT -i, -(i + 1), +i FROM e ORDER BY id",
            "SELECT '12abc' + 0, 'abc' + 0, '3.5' * 2, '' + 1 FROM e WHERE id = 1",
            "SELECT i > 5, i <= 7, i = 7, i != 7, i <> 7 FROM e ORDER BY id",
            "SELECT i IS NULL, i IS NOT NULL, t IS 'abc', i IS 7 FROM e ORDER BY id",
            "SELECT i > 0 AND i < 10, i < 0 OR i > 50, NOT (i > 5) FROM e ORDER BY id",
            "SELECT NULL AND 0, NULL AND 1, NULL OR 0, NULL OR 1 FROM e WHERE id = 1",
            "SELECT i IN (1, 7, 100), t IN ('abc', 'x'), i NOT IN (7) FROM e ORDER BY id",
            "SELECT i BETWEEN 5 AND 10, i NOT BETWEEN 5 AND 10 FROM e ORDER BY id",
            "SELECT t LIKE 'h%o%', t LIKE 'ABC', t LIKE 'a_c', t NOT LIKE 'a%' FROM e ORDER BY id",
            "SELECT CASE WHEN i > 50 THEN 'big' WHEN i > 0 THEN 'pos' ELSE 'other' END FROM e ORDER BY id",
            "SELECT CASE i WHEN 7 THEN 'seven' WHEN -3 THEN 'neg' ELSE '?' END FROM e ORDER BY id",
            "SELECT coalesce(i, 42), ifnull(t, 'dflt'), nullif(i, 7), nullif(i, 8) FROM e ORDER BY id",
            "SELECT abs(i), abs(r), length(t), length(b), upper(t), lower(t) FROM e ORDER BY id",
            "SELECT typeof(i), typeof(r), typeof(t), typeof(b), typeof(NULL) FROM e ORDER BY id",
            "SELECT substr(t, 1, 5), substr(t, 7), substr(t, -5, 2), substr(t, 0, 3) FROM e ORDER BY id",
            "SELECT round(r), round(r * 10, 1), round(2.5), round(-2.5) FROM e ORDER BY id",
            "SELECT hex(t), hex(255), instr(t, 'o'), instr(t, 'zz'), replace(t, 'l', 'L') FROM e ORDER BY id",
            "SELECT trim('  ' || t || '  '), sign(i), sign(r - 3), iif(i > 5, 'y', 'n') FROM e ORDER BY id",
            "SELECT CAST(i AS TEXT), CAST(t AS INTEGER), CAST('12.9' AS INTEGER), CAST('abc' AS REAL), CAST(r AS INTEGER) FROM e ORDER BY id",
            "SELECT CAST('12abc' AS NUMERIC), CAST('1.5' AS NUMERIC), CAST(7 AS REAL), CAST(7.0 AS BLOB) FROM e WHERE id = 1",
            "SELECT min(i, 5), max(i, 5, 200), min(t, 'zzz') FROM e ORDER BY id",
            "SELECT i FROM e WHERE i * 2 > 10 ORDER BY i",
            "SELECT id FROM e WHERE t = 'abc' OR i IS NULL ORDER BY id",
            "SELECT id FROM e WHERE NOT (t LIKE 'a%') AND i IS NOT NULL ORDER BY id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- ORDER BY / LIMIT / DISTINCT ----------------

#[test]
fn order_limit_distinct_differential() {
    block_on(async {
        let path = oracle_db(
            "v04-ord",
            "CREATE TABLE o (id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
             INSERT INTO o VALUES (1,3,'x'),(2,NULL,'y'),(3,1,'x'),(4,2,NULL),(5,3,'a'),(6,1,'x');",
        );
        for q in [
            "SELECT id, a FROM o ORDER BY a",
            "SELECT id, a FROM o ORDER BY a DESC",
            "SELECT id FROM o ORDER BY a ASC, b DESC, id",
            "SELECT id FROM o ORDER BY b, id LIMIT 3",
            "SELECT id FROM o ORDER BY id LIMIT 2 OFFSET 2",
            "SELECT id FROM o ORDER BY id LIMIT 2, 2",
            "SELECT id FROM o ORDER BY id LIMIT -1 OFFSET 4",
            "SELECT id FROM o ORDER BY id DESC LIMIT 0",
            "SELECT a FROM o ORDER BY 1 DESC LIMIT 3",
            "SELECT DISTINCT b FROM o ORDER BY 1",
            "SELECT DISTINCT a, b FROM o ORDER BY a, b",
            "SELECT DISTINCT a FROM o",
            "SELECT a + 1 AS p FROM o ORDER BY p DESC, id LIMIT 4",
            "SELECT id FROM o ORDER BY abs(a - 2), id",
        ] {
            diff_query(&path, q).await;
        }
    });
}

// ---------------- INSERT expressions ----------------

#[test]
fn insert_expressions_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("v04-ins-expr");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default())
                .await
                .expect("open");
            exec(&mut db, "CREATE TABLE t (a INTEGER, b TEXT, c REAL)")
                .await
                .unwrap();
            exec(
                &mut db,
                "INSERT INTO t VALUES (1 + 2, 'x' || 'y', abs(-1.5)), (10 * 2, upper('ab'), 7 / 2)",
            )
            .await
            .unwrap();
            exec(&mut db, "INSERT INTO t (b, a) VALUES (coalesce(NULL, 'n'), -5)")
                .await
                .unwrap();
        }
        assert_eq!(
            common::cli(&path, "PRAGMA integrity_check;").trim(),
            "ok"
        );
        assert_eq!(
            common::cli(&path, "SELECT a, b, c FROM t ORDER BY rowid;"),
            "3|xy|1.5\n20|AB|3.0\n-5|n|\n"
        );
    });
}

// ---------------- ALTER TABLE ADD COLUMN ----------------

#[test]
fn alter_add_column_both_directions() {
    block_on(async {
        // hipp alters; the oracle verifies bytes + behaviour.
        let path = common::temp_db("v04-alter-a");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default())
                .await
                .expect("open");
            exec(&mut db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1, 'one'), (2, 'two')")
                .await
                .unwrap();
            exec(&mut db, "ALTER TABLE t ADD COLUMN c REAL DEFAULT 1.5")
                .await
                .unwrap();
            exec(&mut db, "ALTER TABLE t ADD d TEXT NOT NULL DEFAULT 'z'")
                .await
                .unwrap();
            let rows = collect(&mut db, "SELECT * FROM t ORDER BY a").await;
            assert_eq!(rows, vec!["1|one|1.5|z", "2|two|1.5|z"]);
            exec(&mut db, "INSERT INTO t VALUES (3, 'three', 9.5, 'w')")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t (a, b) VALUES (4, 'four')")
                .await
                .unwrap();
        }
        assert_eq!(
            common::cli(&path, "PRAGMA integrity_check;").trim(),
            "ok"
        );
        assert_eq!(
            common::cli(&path, "SELECT sql FROM sqlite_schema WHERE name = 't';"),
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, c REAL DEFAULT 1.5, d TEXT NOT NULL DEFAULT 'z')\n"
        );
        assert_eq!(
            common::cli(&path, "SELECT * FROM t ORDER BY a;"),
            "1|one|1.5|z\n2|two|1.5|z\n3|three|9.5|w\n4|four|1.5|z\n"
        );

        // The oracle alters; hipp reads the widened table.
        let path2 = oracle_db(
            "v04-alter-b",
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT);
             INSERT INTO t VALUES (1,'one'),(2,'two');
             ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42;",
        );
        diff_query(&path2, "SELECT * FROM t ORDER BY a").await;
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default())
            .await
            .expect("open");
        exec(&mut db, "INSERT INTO t (a, b) VALUES (3, 'three')")
            .await
            .unwrap();
        let rows = collect(&mut db, "SELECT a, c FROM t ORDER BY a").await;
        assert_eq!(rows, vec!["1|42", "2|42", "3|42"]);
    });
}

#[test]
fn alter_add_column_rejections() {
    block_on(async {
        let path = common::temp_db("v04-alter-err");
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)")
            .await
            .unwrap();
        for sql in [
            "ALTER TABLE t ADD COLUMN c INTEGER PRIMARY KEY",
            "ALTER TABLE t ADD COLUMN c TEXT UNIQUE",
            "ALTER TABLE t ADD COLUMN c TEXT NOT NULL",
            "ALTER TABLE t ADD COLUMN c TEXT DEFAULT CURRENT_TIMESTAMP",
            "ALTER TABLE t ADD COLUMN b INTEGER",
            "ALTER TABLE nope ADD COLUMN c INTEGER",
        ] {
            let r = db.prepare(sql).await;
            assert!(r.is_err(), "must refuse: {sql}");
        }
        // The table is untouched and still usable.
        exec(&mut db, "INSERT INTO t VALUES (1, 'x')").await.unwrap();
        let rows = collect(&mut db, "SELECT * FROM t").await;
        assert_eq!(rows, vec!["1|x"]);
    });
}

// ---------------- views (inert) ----------------

#[test]
fn views_inert_both_directions() {
    block_on(async {
        // hipp creates views; the oracle sees stored SQL only.
        let path = common::temp_db("v04-view-a");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default())
                .await
                .expect("open");
            exec(&mut db, "CREATE TABLE t (a INTEGER, b TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1, 'x'), (2, 'y')")
                .await
                .unwrap();
            exec(&mut db, "CREATE VIEW v AS SELECT a, b FROM t WHERE a > 0")
                .await
                .unwrap();
            exec(&mut db, "CREATE VIEW w(x, y) AS SELECT b, count(*) FROM t GROUP BY b")
                .await
                .unwrap();
            // Inert: selecting from a view is refused, by design.
            let r = db.prepare("SELECT * FROM v").await;
            assert!(
                matches!(r, Err(Error::Unsupported)),
                "view select: {:?}",
                r.err()
            );
            // Creating over unknown objects fails at prepare.
            assert!(db.prepare("CREATE VIEW bad AS SELECT * FROM nope").await.is_err());
            assert!(db.prepare("CREATE VIEW bad2 AS SELECT nope FROM t").await.is_err());
            // Duplicate names fail; IF NOT EXISTS is a no-op.
            assert!(db.prepare("CREATE VIEW v AS SELECT a FROM t").await.is_ok());
            let mut st = db
                .prepare("CREATE VIEW v AS SELECT a FROM t")
                .await
                .expect("prepare");
            assert!(st.step().await.is_err(), "duplicate view must fail");
            drop(st);
            exec(&mut db, "CREATE VIEW IF NOT EXISTS v AS SELECT a FROM t")
                .await
                .unwrap();
            // Catalog access for tools.
            let views = db.views();
            assert_eq!(views.len(), 2);
            assert_eq!(views.as_slice()[0].name, b"v");
            assert_eq!(views.as_slice()[0].columns.len(), 2);
            assert_eq!(views.as_slice()[1].columns.as_slice()[0], b"x");
            exec(&mut db, "DROP VIEW w").await.unwrap();
            assert!(db.prepare("DROP VIEW w").await.is_err());
            exec(&mut db, "DROP VIEW IF EXISTS w").await.unwrap();
            assert_eq!(db.views().len(), 1);
        }
        assert_eq!(
            common::cli(&path, "PRAGMA integrity_check;").trim(),
            "ok"
        );
        assert_eq!(
            common::cli(
                &path,
                "SELECT type, name, rootpage, sql FROM sqlite_schema WHERE type = 'view';"
            ),
            "view|v|0|CREATE VIEW v AS SELECT a, b FROM t WHERE a > 0\n"
        );

        // The oracle creates a view; hipp catalogues it and still
        // refuses to execute it.
        let path2 = oracle_db(
            "v04-view-b",
            "CREATE TABLE t (a INTEGER, b TEXT);
             INSERT INTO t VALUES (1,'x');
             CREATE VIEW ov AS SELECT a + 1 AS c, b FROM t;",
        );
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default())
            .await
            .expect("open");
        let views = db.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views.as_slice()[0].columns.as_slice()[0], b"c");
        let r = db.prepare("SELECT * FROM ov").await;
        assert!(matches!(r, Err(Error::Unsupported)));
        exec(&mut db, "DROP VIEW ov").await.unwrap();
        assert_eq!(
            common::cli(&path2, "SELECT count(*) FROM sqlite_schema WHERE type='view';"),
            "0\n"
        );
    });
}

// ---------------- planner rejections ----------------

#[test]
fn query_rejections() {
    block_on(async {
        let path = oracle_db(
            "v04-reject",
            "CREATE TABLE t (a INTEGER, b INTEGER);
             CREATE TABLE u (a INTEGER);
             INSERT INTO t VALUES (1, 2); INSERT INTO u VALUES (1);",
        );
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default())
            .await
            .expect("open");
        // Aggregates in WHERE are refused at prepare.
        assert!(db.prepare("SELECT a FROM t WHERE count(*) > 1").await.is_err());
        // Unknown functions are refused at prepare.
        assert!(db.prepare("SELECT nosuchfn(a) FROM t").await.is_err());
        // Ambiguous unqualified column across a join.
        let r = db.prepare("SELECT a FROM t JOIN u ON t.a = u.a").await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: hipp::SchemaKind::AmbiguousColumn
                })
            ),
            "ambiguous: {:?}",
            r.err()
        );
        // Unknown column in a rich query.
        let r = db.prepare("SELECT nope FROM t").await;
        assert!(matches!(
            r,
            Err(Error::Schema {
                kind: hipp::SchemaKind::UnknownColumn
            })
        ));
        // Subqueries are outside v0.4.
        assert!(
            db.prepare("SELECT a FROM t WHERE a IN (SELECT a FROM u)")
                .await
                .is_err()
        );
    });
}
