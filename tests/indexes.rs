//! v0.3 index B-trees, differentially vs the pinned oracle
//! (SPEC §2 v0.3): CREATE [UNIQUE] INDEX, secondary-index
//! lookups through the planner's access seam, UNIQUE enforcement
//! (explicit indexes, column/table UNIQUE constraints and
//! non-rowid PRIMARY KEYs via implicit indexes), DROP INDEX, and
//! freelist reuse hardening. The oracle's `integrity_check` is
//! the structural verdict on every hipp-written file — it
//! verifies index key order and entry-per-row consistency.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, MemStorage, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Result<(), S::Error>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
    Ok(())
}

async fn try_exec<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<(), Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await?;
    let _ = st.step().await?;
    Ok(())
}

async fn exec_err<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Error<S::Error>
where
    S::Error: core::fmt::Debug,
{
    // The error may surface at plan time (prepare) or at step;
    // either way the statement failed with it.
    match db.prepare(sql).await {
        Err(e) => e,
        Ok(mut st) => st.step().await.expect_err("step must fail"),
    }
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

fn integrity(path: &std::path::Path) {
    assert_eq!(
        common::cli(path, "PRAGMA integrity_check;").trim(),
        "ok",
        "oracle version: {}",
        common::oracle_version()
    );
}

// ---------------- CREATE INDEX, both directions ----------------

#[test]
fn create_index_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("v03-idx-a");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)",
            )
            .await
            .unwrap();
            exec(
                &mut db,
                "INSERT INTO t VALUES (1,'one',10,1.5),(2,'two',20,2.5),(3,'one',30,3.5),(4,NULL,NULL,NULL)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_a ON t(a)").await.unwrap();
            exec(&mut db, "CREATE UNIQUE INDEX ux_ba ON t(b, a)")
                .await
                .unwrap();
            // Inserts after creation are maintained.
            exec(&mut db, "INSERT INTO t VALUES (5,'five',50,5.5)")
                .await
                .unwrap();
        }
        integrity(&path);
        // Schema rows agree (name/type/tbl/sql; rootpages differ
        // legitimately by allocation order and are not compared).
        let schema = common::cli(
            &path,
            "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE type='index' ORDER BY name;",
        );
        assert_eq!(
            schema,
            "index|ix_a|t|CREATE INDEX ix_a ON t(a)\nindex|ux_ba|t|CREATE UNIQUE INDEX ux_ba ON t(b, a)\n"
        );
        // The index root pages are index leaves (0x0A).
        let bytes = std::fs::read(&path).expect("read db");
        for name in ["ix_a", "ux_ba"] {
            let root: usize = common::cli(
                &path,
                &format!("SELECT rootpage FROM sqlite_schema WHERE name='{name}';"),
            )
            .trim()
            .parse()
            .expect("rootpage");
            assert_eq!(bytes[(root - 1) * 4096], 0x0A, "{name} root is index leaf");
        }
        // Oracle lookups through the indexes see every row.
        assert_eq!(
            common::cli(&path, "SELECT id FROM t WHERE a='one' ORDER BY id;"),
            "1\n3\n"
        );
        assert_eq!(common::cli(&path, "SELECT id FROM t WHERE b=50;"), "5\n");
    });
}

#[test]
fn oracle_indexes_hipp_reads_and_maintains() {
    block_on(async {
        let path = common::temp_db("v03-idx-b");
        let out = common::cli(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER); \
             CREATE INDEX ix_a ON t(a); CREATE UNIQUE INDEX ux_b ON t(b); \
             INSERT INTO t VALUES (1,'one',10),(2,'two',20),(3,'one',30);",
        );
        assert_eq!(out, "");
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        // hipp lookups via the oracle-built indexes.
        assert_eq!(
            collect(&mut db, "SELECT id FROM t WHERE a = 'one'").await,
            vec!["1", "3"]
        );
        assert_eq!(
            collect(&mut db, "SELECT a FROM t WHERE b = 20").await,
            vec!["two"]
        );
        // hipp maintains them on insert.
        exec(&mut db, "INSERT INTO t VALUES (4,'one',40)")
            .await
            .unwrap();
        // The oracle's UNIQUE index rejects a hipp-visible dup too.
        let e = exec_err(&mut db, "INSERT INTO t VALUES (5,'x',40)").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "dup b: {e:?}"
        );
        drop(db);
        integrity(&path);
        assert_eq!(
            common::cli(&path, "SELECT id FROM t WHERE a='one' ORDER BY id;"),
            "1\n3\n4\n"
        );
    });
}

#[test]
fn create_index_on_populated_table_and_empty() {
    block_on(async {
        let path = common::temp_db("v03-idx-pop");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX empty_ix ON t(a)")
                .await
                .unwrap();
            for chunk in 0..6 {
                let mut sql = String::from("INSERT INTO t VALUES ");
                for i in 0..50 {
                    let id = chunk * 50 + i + 1;
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str(&format!("({id}, {})", id * 7 % 100));
                }
                exec(&mut db, &sql).await.unwrap();
            }
            // Index created AFTER the data exists (300 rows).
            exec(&mut db, "CREATE INDEX ix_a ON t(a)").await.unwrap();
            // Lookups agree with a full scan filtered by hand.
            for probe in [0, 7, 42, 99] {
                let via_index =
                    collect(&mut db, &format!("SELECT id FROM t WHERE a = {probe}")).await;
                let mut want: Vec<String> = (1..=300)
                    .filter(|id| id * 7 % 100 == probe)
                    .map(|id| id.to_string())
                    .collect();
                want.sort_by_key(|s| s.parse::<i64>().unwrap());
                let mut got = via_index.clone();
                got.sort_by_key(|s| s.parse::<i64>().unwrap());
                assert_eq!(got, want, "probe {probe}");
            }
        }
        integrity(&path);
        assert_eq!(
            common::cli(&path, "SELECT count(*) FROM t WHERE a=42;").trim(),
            "3"
        );
    });
}

// ---------------- lookups: types, affinity, multi-column, DESC ----------------

#[test]
fn index_lookup_types_and_conversions() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)",
        )
        .await
        .unwrap();
        exec(&mut db, "CREATE INDEX ia ON t(a)").await.unwrap();
        exec(&mut db, "CREATE INDEX ib ON t(b)").await.unwrap();
        exec(&mut db, "CREATE INDEX ic ON t(c)").await.unwrap();
        exec(
            &mut db,
            "INSERT INTO t VALUES (1,'2',2,2.5),(2,'10',10,10.0),(3,'x',-3,-0.5),(4,NULL,NULL,NULL)",
        )
        .await
        .unwrap();
        for (sql, want) in [
            // TEXT column: integer literal converts to text.
            ("SELECT id FROM t WHERE a = 2", vec!["1"]),
            ("SELECT id FROM t WHERE a = '2'", vec!["1"]),
            ("SELECT id FROM t WHERE a = '10'", vec!["2"]),
            // INTEGER column: text literals convert numerically.
            ("SELECT id FROM t WHERE b = '2'", vec!["1"]),
            ("SELECT id FROM t WHERE b = 2.0", vec!["1"]),
            ("SELECT id FROM t WHERE b = '2.0'", vec!["1"]),
            ("SELECT id FROM t WHERE b = -3", vec!["3"]),
            // REAL column.
            ("SELECT id FROM t WHERE c = 2.5", vec!["1"]),
            ("SELECT id FROM t WHERE c = 10", vec!["2"]),
            ("SELECT id FROM t WHERE c = '10.0'", vec!["2"]),
            // Misses: no match, never an error.
            ("SELECT id FROM t WHERE a = 'nope'", vec![]),
            ("SELECT id FROM t WHERE b = 99", vec![]),
            ("SELECT id FROM t WHERE b = 'abc'", vec![]),
            ("SELECT id FROM t WHERE a = NULL", vec![]),
            ("SELECT id FROM t WHERE c = 2.25", vec![]),
        ] {
            let rows = collect(&mut db, sql).await;
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(rows, want, "{sql}");
        }
        // A column with no index is still Unsupported.
        let r = db.prepare("SELECT id FROM t WHERE rowid = 1").await;
        assert!(r.is_ok(), "rowid path still works");
        exec(&mut db, "CREATE TABLE u (id INTEGER PRIMARY KEY, z TEXT)")
            .await
            .unwrap();
        let r = db.prepare("SELECT id FROM u WHERE z = 'a'").await;
        assert!(
            matches!(r, Err(Error::Unsupported)),
            "unindexed predicate: {:?}",
            r.err()
        );
    });
}

#[test]
fn index_lookup_multi_column_prefix_and_desc() {
    block_on(async {
        let path = common::temp_db("v03-idx-multi");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_ab ON t(a, b)")
                .await
                .unwrap();
            exec(&mut db, "CREATE INDEX ix_desc ON t(b DESC)")
                .await
                .unwrap();
            exec(
                &mut db,
                "INSERT INTO t VALUES (1,5,'p'),(2,5,'q'),(3,9,'p'),(4,5,'p'),(5,9,'z')",
            )
            .await
            .unwrap();
            // Prefix lookup on the composite index's first column.
            let mut rows = collect(&mut db, "SELECT id FROM t WHERE a = 5").await;
            rows.sort();
            assert_eq!(rows, vec!["1", "2", "4"]);
            let rows = collect(&mut db, "SELECT id FROM t WHERE a = 9").await;
            assert_eq!(rows.len(), 2);
            // DESC index lookup (equality is direction-blind).
            let mut rows = collect(&mut db, "SELECT id FROM t WHERE b = 'p'").await;
            rows.sort();
            assert_eq!(rows, vec!["1", "3", "4"]);
        }
        integrity(&path);
    });
}

#[test]
fn index_lookup_deep_tree_interior_keys() {
    block_on(async {
        // 512-byte pages force the index interior to hold promoted
        // keys (true B-tree entries); lookups must find keys that
        // live in interior pages, not just leaves.
        let path = common::temp_db("v03-idx-deep");
        let fs0 = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs0, Config::default().page_size(512))
            .await
            .expect("open");
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)",
        )
        .await
        .unwrap();
        exec(&mut db, "CREATE INDEX ix ON t(a)").await.unwrap();
        for chunk in 0..6 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..50 {
                let id = chunk * 50 + i + 1;
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("({id}, {})", id * 10));
            }
            exec(&mut db, &sql).await.unwrap();
        }
        // Every key is findable, including interior-resident ones.
        for id in [1, 52, 103, 151, 202, 253, 300] {
            let rows = collect(&mut db, &format!("SELECT id FROM t WHERE a = {}", id * 10)).await;
            assert_eq!(rows, vec![id.to_string()], "a={}", id * 10);
        }
        let rows = collect(&mut db, "SELECT id FROM t WHERE a = 9999").await;
        assert!(rows.is_empty());
        drop(db);
        integrity(&path);
        // The oracle finds the same keys in hipp's tree.
        for a in [520, 1030, 3000] {
            assert_eq!(
                common::cli(&path, &format!("SELECT id FROM t WHERE a={a};")),
                format!("{}\n", a / 10)
            );
        }
        // Reverse direction: the oracle builds a deep index tree,
        // hipp searches it.
        let path2 = common::temp_db("v03-idx-deep-oracle");
        let mut script = String::from(
            "PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER); CREATE INDEX ix ON t(a);",
        );
        for id in 1..=300 {
            script.push_str(&format!("INSERT INTO t VALUES ({id}, {});", id * 10));
        }
        common::cli(&path2, &script);
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for id in [1, 52, 103, 200, 300] {
            let rows = collect(&mut db, &format!("SELECT id FROM t WHERE a = {}", id * 10)).await;
            assert_eq!(rows, vec![id.to_string()], "oracle tree a={}", id * 10);
        }
        // And hipp maintains the oracle's tree on insert.
        exec(&mut db, "INSERT INTO t VALUES (301, 3010)")
            .await
            .unwrap();
        drop(db);
        integrity(&path2);
        assert_eq!(
            common::cli(&path2, "SELECT id FROM t WHERE a=3010;"),
            "301\n"
        );
    });
}

#[test]
fn index_overflow_keys() {
    block_on(async {
        let path = common::temp_db("v03-idx-overflow");
        let big = "k".repeat(2000);
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
                .await
                .unwrap();
            exec(&mut db, "CREATE INDEX ix ON t(a)").await.unwrap();
            exec(&mut db, &format!("INSERT INTO t VALUES (1, '{big}')"))
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (2, 'small')")
                .await
                .unwrap();
            let rows = collect(&mut db, &format!("SELECT id FROM t WHERE a = '{big}'")).await;
            assert_eq!(rows, vec!["1"]);
        }
        integrity(&path);
        assert_eq!(
            common::cli(&path, &format!("SELECT id FROM t WHERE a = '{big}';")),
            "1\n"
        );
        assert_eq!(
            common::cli(&path, "SELECT length(a) FROM t WHERE id=1;").trim(),
            "2000"
        );
    });
}

// ---------------- UNIQUE enforcement ----------------

#[test]
fn unique_enforcement_all_origins() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        // 1. Explicit UNIQUE index.
        exec(&mut db, "CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT)")
            .await
            .unwrap();
        exec(&mut db, "CREATE UNIQUE INDEX ux_a ON a(x)")
            .await
            .unwrap();
        exec(&mut db, "INSERT INTO a VALUES (1,'one'),(2,'two')")
            .await
            .unwrap();
        let e = exec_err(&mut db, "INSERT INTO a VALUES (3,'one')").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "explicit unique: {e:?}"
        );
        // NULLs never conflict.
        exec(&mut db, "INSERT INTO a VALUES (4,NULL),(5,NULL)")
            .await
            .unwrap();
        // A failing multi-row statement is atomic across indexes.
        let e = exec_err(&mut db, "INSERT INTO a VALUES (6,'six'),(7,'two')").await;
        assert!(matches!(
            e,
            Error::Schema {
                kind: hipp::SchemaKind::UniqueViolation
            }
        ));
        assert_eq!(
            collect(&mut db, "SELECT id FROM a WHERE x = 'six'").await,
            Vec::<String>::new()
        );

        // 2. Column UNIQUE constraint (implicit index).
        exec(
            &mut db,
            "CREATE TABLE b (id INTEGER PRIMARY KEY, x TEXT UNIQUE)",
        )
        .await
        .unwrap();
        exec(&mut db, "INSERT INTO b VALUES (1,'one')")
            .await
            .unwrap();
        let e = exec_err(&mut db, "INSERT INTO b VALUES (2,'one')").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "column unique: {e:?}"
        );
        exec(&mut db, "INSERT INTO b VALUES (3,NULL),(4,NULL)")
            .await
            .unwrap();

        // 3. Table-level composite UNIQUE.
        exec(
            &mut db,
            "CREATE TABLE c (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, UNIQUE(x, y))",
        )
        .await
        .unwrap();
        exec(&mut db, "INSERT INTO c VALUES (1,1,1),(2,1,2),(3,2,1)")
            .await
            .unwrap();
        let e = exec_err(&mut db, "INSERT INTO c VALUES (4,1,1)").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "composite unique: {e:?}"
        );
        exec(&mut db, "INSERT INTO c VALUES (5,NULL,1),(6,NULL,1)")
            .await
            .unwrap();

        // 4. Non-INTEGER PRIMARY KEY (implicit unique index).
        exec(&mut db, "CREATE TABLE d (k TEXT PRIMARY KEY, v INTEGER)")
            .await
            .unwrap();
        exec(&mut db, "INSERT INTO d VALUES ('a',1)").await.unwrap();
        let e = exec_err(&mut db, "INSERT INTO d VALUES ('a',2)").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "text pk: {e:?}"
        );

        // 5. INTEGER PRIMARY KEY DESC is NOT the rowid alias in
        // SQLite (the quirk): it gets an implicit index too.
        exec(
            &mut db,
            "CREATE TABLE e (id INTEGER PRIMARY KEY DESC, v TEXT)",
        )
        .await
        .unwrap();
        exec(&mut db, "INSERT INTO e VALUES (1,'x')").await.unwrap();
        let e2 = exec_err(&mut db, "INSERT INTO e VALUES (1,'y')").await;
        assert!(
            matches!(
                e2,
                Error::Schema {
                    kind: hipp::SchemaKind::UniqueViolation
                }
            ),
            "desc pk: {e2:?}"
        );
    });
}

#[test]
fn unique_schema_rows_match_oracle_shape() {
    block_on(async {
        let path = common::temp_db("v03-autoindex");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE m (a TEXT UNIQUE, b TEXT, c TEXT UNIQUE, UNIQUE(b, c))",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE TABLE p (k TEXT PRIMARY KEY, v INTEGER)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO m VALUES ('x','y','z')")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO p VALUES ('k1',1)")
                .await
                .unwrap();
        }
        integrity(&path);
        let names = common::cli(
            &path,
            "SELECT name FROM sqlite_schema WHERE type='index' ORDER BY rowid;",
        );
        assert_eq!(
            names,
            "sqlite_autoindex_m_1\nsqlite_autoindex_m_2\nsqlite_autoindex_m_3\nsqlite_autoindex_p_1\n"
        );
        // Implicit indexes carry NULL sql in sqlite_schema.
        assert_eq!(
            common::cli(
                &path,
                "SELECT count(*) FROM sqlite_schema WHERE type='index' AND sql IS NULL;"
            )
            .trim(),
            "4"
        );
    });
}

#[test]
fn create_unique_index_over_duplicates_fails_atomically() {
    block_on(async {
        let path = common::temp_db("v03-idx-dupfail");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1,'x'),(2,'x'),(3,'y')")
                .await
                .unwrap();
            let e = exec_err(&mut db, "CREATE UNIQUE INDEX ux ON t(a)").await;
            assert!(
                matches!(
                    e,
                    Error::Schema {
                        kind: hipp::SchemaKind::UniqueViolation
                    }
                ),
                "create over dups: {e:?}"
            );
            // No schema trace, data intact, connection usable.
            assert_eq!(db.indexes().len(), 0);
            let rows = collect(&mut db, "SELECT id FROM t").await;
            assert_eq!(rows, vec!["1", "2", "3"]);
            // A plain index over the same data succeeds.
            exec(&mut db, "CREATE INDEX ix ON t(a)").await.unwrap();
            assert_eq!(db.indexes().len(), 1);
        }
        integrity(&path);
        assert_eq!(
            common::cli(&path, "SELECT name FROM sqlite_schema WHERE type='index';"),
            "ix\n"
        );
    });
}

#[test]
fn create_index_in_transaction_rollback_and_commit() {
    block_on(async {
        let path = common::temp_db("v03-idx-txn");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1,'one'),(2,'two')")
                .await
                .unwrap();
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "CREATE INDEX gone ON t(a)").await.unwrap();
            exec(&mut db, "ROLLBACK").await.unwrap();
            assert_eq!(db.indexes().len(), 0, "rolled-back index is gone");
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "CREATE INDEX kept ON t(a)").await.unwrap();
            exec(&mut db, "COMMIT").await.unwrap();
            assert_eq!(db.indexes().len(), 1);
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE a = 'two'").await,
                vec!["2"]
            );
        }
        integrity(&path);
        assert_eq!(
            common::cli(&path, "SELECT name FROM sqlite_schema WHERE type='index';"),
            "kept\n"
        );
    });
}

// ---------------- DROP INDEX + freelist ----------------

#[test]
fn drop_index_frees_pages_and_reuse() {
    block_on(async {
        let path = common::temp_db("v03-drop");
        let pages_before;
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default().page_size(512))
                .await
                .expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix ON t(a)").await.unwrap();
            for chunk in 0..4 {
                let mut sql = String::from("INSERT INTO t VALUES ");
                for i in 0..50 {
                    let id = chunk * 50 + i + 1;
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str(&format!("({id}, {})", id * 3));
                }
                exec(&mut db, &sql).await.unwrap();
            }
            pages_before = db.arena_stats().page_count;
            exec(&mut db, "DROP INDEX ix").await.unwrap();
            assert_eq!(db.indexes().len(), 0);
            // Freed pages are reused by subsequent growth: insert
            // another 200 rows and the file must not grow past its
            // pre-drop page count.
            for chunk in 4..8 {
                let mut sql = String::from("INSERT INTO t VALUES ");
                for i in 0..50 {
                    let id = chunk * 50 + i + 1;
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str(&format!("({id}, {})", id * 3));
                }
                exec(&mut db, &sql).await.unwrap();
            }
            let pages_after = db.arena_stats().page_count;
            assert!(
                pages_after <= pages_before,
                "freelist reuse: before {pages_before}, after {pages_after}"
            );
            let rows = collect(&mut db, "SELECT id FROM t WHERE id = 400").await;
            assert_eq!(rows, vec!["400"]);
            // Errors: missing index, and IF EXISTS is a no-op.
            let e = exec_err(&mut db, "DROP INDEX ix").await;
            assert!(
                matches!(
                    e,
                    Error::Schema {
                        kind: hipp::SchemaKind::UnknownIndex
                    }
                ),
                "re-drop: {e:?}"
            );
            exec(&mut db, "DROP INDEX IF EXISTS ix").await.unwrap();
        }
        integrity(&path);
        assert_eq!(common::cli(&path, "SELECT count(*) FROM t;").trim(), "400");
        assert_eq!(
            common::cli(&path, "SELECT name FROM sqlite_schema WHERE type='index';"),
            ""
        );
    });
}

#[test]
fn drop_autoindex_is_refused() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (a TEXT UNIQUE)")
            .await
            .unwrap();
        let e = exec_err(&mut db, "DROP INDEX sqlite_autoindex_t_1").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::DropAutoIndex
                }
            ),
            "drop autoindex: {e:?}"
        );
    });
}

#[test]
fn freelist_oracle_generated_multi_trunk_reuse() {
    block_on(async {
        // Oracle drops a 300-page table at 512-byte pages: the
        // freelist needs several trunk pages (126 leaves each).
        let path = common::temp_db("v03-freelist-oracle");
        let mut script = String::from(
            "PRAGMA page_size=512; CREATE TABLE big (id INTEGER PRIMARY KEY, d TEXT);",
        );
        for chunk in 0..12 {
            script.push_str("INSERT INTO big VALUES ");
            for i in 0..50 {
                let id = chunk * 50 + i + 1;
                if i > 0 {
                    script.push_str(", ");
                }
                script.push_str(&format!("({id}, '{}')", "x".repeat(80)));
            }
            script.push(';');
        }
        common::cli(&path, &script);
        common::cli(&path, "DROP TABLE big;");
        let header = std::fs::read(&path).expect("read");
        let fl_count = u32::from_be_bytes(header[36..40].try_into().unwrap());
        assert!(fl_count > 100, "freelist has {fl_count} pages");
        let orig_pages = std::fs::metadata(&path).expect("meta").len() / 512;
        // hipp consumes the foreign freelist. (The wide cache:
        // one 50-row statement at 512-byte pages dirties more
        // pages than the default 16-page cache can pin.)
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default().cache_pages(64))
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, d TEXT)")
            .await
            .unwrap();
        for chunk in 0..8 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..50 {
                let id = chunk * 50 + i + 1;
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("({id}, '{}')", "y".repeat(80)));
            }
            exec(&mut db, &sql).await.unwrap();
        }
        let pages = db.arena_stats().page_count;
        // Reuse proof: ~120 pages of new table content landed
        // almost entirely in the 123-page freelist, so the file
        // grows only slightly past its original size (append
        // splits in hipp's table B-tree are mid-page, which is
        // why the content needs that many pages at all).
        assert!(
            (pages as u64) <= orig_pages + 24,
            "pages {pages} vs original {orig_pages} (freelist {fl_count})"
        );
        drop(db);
        integrity(&path);
        assert_eq!(common::cli(&path, "SELECT count(*) FROM t;").trim(), "400");
    });
}

#[test]
fn freelist_corruption_is_an_error_not_corruption() {
    block_on(async {
        // Build a file with a small freelist (oracle DROP), then
        // hand-mangle trunk fields three ways; hipp's next
        // allocation must fail with CorruptPage, never hand out a
        // bogus page or panic.
        for (tag, mangle) in [("leaf-oob", 0u8), ("leaf-dup", 1u8), ("count-over", 2u8)] {
            let path = common::temp_db(&format!("v03-fl-corrupt-{tag}"));
            let mut fixture = String::from(
                "PRAGMA page_size=512; CREATE TABLE big (id INTEGER PRIMARY KEY, d TEXT);",
            );
            for i in 1..=60 {
                fixture.push_str(&format!(
                    "INSERT INTO big VALUES ({i}, '{}');",
                    "z".repeat(80)
                ));
            }
            fixture.push_str("DROP TABLE big; CREATE TABLE keep (id INTEGER PRIMARY KEY);");
            common::cli(&path, &fixture);
            let mut bytes = std::fs::read(&path).expect("read");
            let trunk = u32::from_be_bytes(bytes[32..36].try_into().unwrap()) as usize;
            let fl_count = u32::from_be_bytes(bytes[36..40].try_into().unwrap());
            assert!(trunk >= 2 && fl_count >= 1, "fixture has a freelist");
            let toff = (trunk - 1) * 512;
            let leaf_count = u32::from_be_bytes(bytes[toff + 4..toff + 8].try_into().unwrap());
            match mangle {
                // Leaf pointer far outside the file.
                0 => {
                    assert!(leaf_count >= 1);
                    bytes[toff + 8..toff + 12].copy_from_slice(&99999u32.to_be_bytes());
                }
                // Duplicate the first leaf (a page handed out
                // twice corrupts silently without the check).
                1 => {
                    if leaf_count < 2 {
                        // Promote: set count to 2 with a copy of
                        // the only leaf.
                        let leaf = bytes[toff + 8..toff + 12].to_vec();
                        bytes[toff + 4..toff + 8].copy_from_slice(&2u32.to_be_bytes());
                        bytes[toff + 12..toff + 16].copy_from_slice(&leaf);
                        bytes[36..40].copy_from_slice(&(fl_count + 1).to_be_bytes());
                    } else {
                        let leaf = bytes[toff + 8..toff + 12].to_vec();
                        bytes[toff + 12..toff + 16].copy_from_slice(&leaf);
                    }
                }
                // Leaf count beyond the trunk's capacity.
                _ => {
                    bytes[toff + 4..toff + 8].copy_from_slice(&100000u32.to_be_bytes());
                }
            }
            std::fs::write(&path, &bytes).expect("write");
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            // Force allocations until the freelist is consumed
            // (or the corruption is hit): plain growth.
            let mut result: Result<(), Error<hipp::storage::FileError>> = Ok(());
            for i in 0..40 {
                let r = try_exec(
                    &mut db,
                    &format!("CREATE TABLE x{i} (id INTEGER PRIMARY KEY)"),
                )
                .await;
                if r.is_err() {
                    result = r;
                    break;
                }
            }
            let e = result.expect_err("corrupt freelist must fail an allocation");
            assert!(matches!(e, Error::CorruptPage { .. }), "{tag}: {e:?}");
        }
    });
}

// ---------------- opaque foreign indexes ----------------

#[test]
fn opaque_index_blocks_writes_not_reads() {
    block_on(async {
        // Expression index: outside the v0.3 grammar. hipp reads
        // the table fine but refuses to write it (it cannot
        // maintain the index) instead of desynchronising it.
        let path = common::temp_db("v03-opaque");
        common::cli(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER); \
             CREATE INDEX iex ON t(abs(a)); INSERT INTO t VALUES (1,-5),(2,7);",
        );
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let rows = collect(&mut db, "SELECT id, a FROM t").await;
        assert_eq!(rows, vec!["1|-5", "2|7"]);
        let r = db.prepare("INSERT INTO t VALUES (3, 9)").await;
        assert!(
            matches!(r, Err(Error::Unsupported)),
            "write under opaque index: {:?}",
            r.err()
        );
        drop(db);
        integrity(&path);
    });
}
