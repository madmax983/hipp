//! v0.5 write-side dialect, differentially vs the pinned oracle
//! (SPEC §2 v0.5): UPDATE, DELETE, DROP TABLE with full index
//! maintenance, dropped-table page return to the freelist, and
//! statement-level atomicity. The oracle's `integrity_check` is
//! the structural verdict on every hipp-written file — it
//! verifies index key order and entry-per-row consistency after
//! hipp's delete paths (which never merge or redistribute).

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, MemStorage, SchemaKind, Step};

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

// ---------------- UPDATE / DELETE basics, both directions ----------------

#[test]
fn update_delete_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("v05-basics-h");
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
                "INSERT INTO t VALUES (1,'one',10,1.5),(2,'two',20,2.5),(3,'three',30,3.5),(4,'four',40,4.5),(5,'five',50,5.5)",
            )
            .await
            .unwrap();
            // Plain WHERE update, expression SET, function SET.
            exec(&mut db, "UPDATE t SET b = b + 5 WHERE id <= 2")
                .await
                .unwrap();
            exec(&mut db, "UPDATE t SET a = upper(a) WHERE id = 3")
                .await
                .unwrap();
            // No WHERE: every row.
            exec(&mut db, "UPDATE t SET c = c * 2").await.unwrap();
            // Duplicate SET column: expressions read the ORIGINAL
            // row, last assignment wins (oracle-verified).
            exec(&mut db, "UPDATE t SET b = 1, b = b + 100 WHERE id = 4")
                .await
                .unwrap();
            exec(&mut db, "DELETE FROM t WHERE id = 5").await.unwrap();
            exec(&mut db, "DELETE FROM t WHERE b < 20 AND a LIKE 'o%'")
                .await
                .unwrap();
        }
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id;"),
            vec!["2|two|25|5.0", "3|THREE|30|7.0", "4|four|140|9.0"]
        );
    });
}

#[test]
fn update_delete_oracle_writes_hipp_reads_and_writes() {
    block_on(async {
        let path = common::temp_db("v05-basics-o");
        common::cli(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER);
             INSERT INTO t VALUES (1,'one',10),(2,'two',20),(3,'three',30),(4,'four',40);
             UPDATE t SET b = b * 10 WHERE id >= 3;
             DELETE FROM t WHERE id = 2;",
        );
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            assert_eq!(
                collect(&mut db, "SELECT * FROM t ORDER BY id").await,
                vec!["1|one|10", "3|three|300", "4|four|400"]
            );
            // hipp continues writing into the oracle-shaped file.
            exec(&mut db, "UPDATE t SET a = 'tres' WHERE id = 3")
                .await
                .unwrap();
            exec(&mut db, "DELETE FROM t WHERE b >= 400").await.unwrap();
            exec(&mut db, "INSERT INTO t VALUES (9,'nine',90)")
                .await
                .unwrap();
        }
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id;"),
            vec!["1|one|10", "3|tres|300", "9|nine|90"]
        );
    });
}

#[test]
fn update_where_general_expressions() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
        )
        .await
        .unwrap();
        exec(
            &mut db,
            "INSERT INTO t VALUES (1,'apple',10),(2,'banana',20),(3,'cherry',30),(4,NULL,40),(5,'fig',50)",
        )
        .await
        .unwrap();
        exec(
            &mut db,
            "UPDATE t SET b = CASE WHEN b >= 30 THEN b + 100 ELSE b END WHERE a IS NOT NULL",
        )
        .await
        .unwrap();
        exec(&mut db, "DELETE FROM t WHERE id IN (2, 4) OR a LIKE 'f%'")
            .await
            .unwrap();
        exec(
            &mut db,
            "UPDATE t SET a = a || '!' WHERE id BETWEEN 1 AND 3",
        )
        .await
        .unwrap();
        // Table alias: SET targets are bare names (a qualified
        // SET target is a syntax error in the oracle too), while
        // expressions and the filter qualify through the alias.
        exec(&mut db, "UPDATE t AS tt SET b = tt.b + 1 WHERE tt.id = 3")
            .await
            .unwrap();
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|apple!|10", "3|cherry!|131"]
        );
        // Aggregates are not legal in UPDATE/DELETE expressions.
        assert!(matches!(
            exec_err(&mut db, "UPDATE t SET b = count(*)").await,
            Error::Unsupported
        ));
        assert!(matches!(
            exec_err(&mut db, "DELETE FROM t WHERE sum(b) > 1").await,
            Error::Unsupported
        ));
    });
}

#[test]
fn update_delete_empty_and_no_match() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
            .await
            .unwrap();
        // Empty table: both are successful no-ops.
        exec(&mut db, "UPDATE t SET a = 'x'").await.unwrap();
        exec(&mut db, "DELETE FROM t").await.unwrap();
        exec(&mut db, "INSERT INTO t VALUES (1,'a'),(2,'b')")
            .await
            .unwrap();
        // Predicates matching nothing.
        exec(&mut db, "UPDATE t SET a = 'x' WHERE id > 100")
            .await
            .unwrap();
        exec(&mut db, "DELETE FROM t WHERE id > 100").await.unwrap();
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|a", "2|b"]
        );
        // DELETE with no WHERE takes every row; the table lives.
        exec(&mut db, "DELETE FROM t").await.unwrap();
        assert_eq!(collect(&mut db, "SELECT * FROM t").await.len(), 0);
        exec(&mut db, "INSERT INTO t VALUES (7,'seven')")
            .await
            .unwrap();
        assert_eq!(collect(&mut db, "SELECT * FROM t").await, vec!["7|seven"]);
    });
}

// ---------------- rowid rules ----------------

#[test]
fn update_rowid_and_alias_rules() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
            .await
            .unwrap();
        exec(&mut db, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')")
            .await
            .unwrap();
        // Move a row by its alias and by the rowid pseudo-column.
        exec(&mut db, "UPDATE t SET id = 30 WHERE id = 3")
            .await
            .unwrap();
        exec(&mut db, "UPDATE t SET rowid = 20 WHERE rowid = 2")
            .await
            .unwrap();
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|a", "20|b", "30|c"]
        );
        // Taken rowid: refused, statement rolled back.
        assert!(matches!(
            exec_err(&mut db, "UPDATE t SET id = 20 WHERE id = 1").await,
            Error::Schema {
                kind: SchemaKind::DuplicateRowid
            }
        ));
        // NULL / non-integer rowid in UPDATE: datatype mismatch
        // (the oracle refuses these, unlike INSERT's NULL rule).
        assert!(matches!(
            exec_err(&mut db, "UPDATE t SET id = NULL WHERE id = 1").await,
            Error::Schema {
                kind: SchemaKind::DatatypeMismatch
            }
        ));
        assert!(matches!(
            exec_err(&mut db, "UPDATE t SET id = 'x' WHERE id = 1").await,
            Error::Schema {
                kind: SchemaKind::DatatypeMismatch
            }
        ));
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|a", "20|b", "30|c"]
        );
        // A table with no alias: the rowid is directly settable.
        exec(&mut db, "CREATE TABLE u (a TEXT)").await.unwrap();
        exec(&mut db, "INSERT INTO u VALUES ('p'),('q')")
            .await
            .unwrap();
        exec(&mut db, "UPDATE u SET rowid = 99 WHERE rowid = 1")
            .await
            .unwrap();
        assert_eq!(
            collect(&mut db, "SELECT rowid, a FROM u ORDER BY rowid").await,
            vec!["2|q", "99|p"]
        );
    });
}

// ---------------- indexes: maintenance, UNIQUE, atomicity ----------------

#[test]
fn update_delete_with_indexes() {
    block_on(async {
        let path = common::temp_db("v05-idx");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_b ON t(b)").await.unwrap();
            exec(
                &mut db,
                "INSERT INTO t VALUES (1,'one',10),(2,'two',20),(3,'three',30),(4,'four',40)",
            )
            .await
            .unwrap();
            // Update the indexed column: lookups follow.
            exec(&mut db, "UPDATE t SET b = 99 WHERE id = 2")
                .await
                .unwrap();
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE b = 99").await,
                vec!["2"]
            );
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE b = 20")
                    .await
                    .len(),
                0
            );
            // Update a non-indexed column: index stays exact.
            exec(&mut db, "UPDATE t SET a = 'uno' WHERE id = 1")
                .await
                .unwrap();
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE b = 10").await,
                vec!["1"]
            );
            // Update the UNIQUE column to its own value: fine.
            exec(&mut db, "UPDATE t SET a = 'uno' WHERE id = 1")
                .await
                .unwrap();
            // Delete removes the index entries too.
            exec(&mut db, "DELETE FROM t WHERE id = 3").await.unwrap();
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE b = 30")
                    .await
                    .len(),
                0
            );
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE a = 'three'")
                    .await
                    .len(),
                0
            );
        }
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id;"),
            vec!["1|uno|10", "2|two|99", "4|four|40"]
        );
    });
}

#[test]
fn update_unique_violation_is_atomic() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER)",
        )
        .await
        .unwrap();
        exec(
            &mut db,
            "INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30)",
        )
        .await
        .unwrap();
        // Row 2 updates cleanly, row 3 would collide with row 1:
        // the whole statement must roll back (row 2 unchanged).
        let e = exec_err(
            &mut db,
            "UPDATE t SET a = CASE WHEN id = 3 THEN 'a' ELSE 'n' END, b = b + 1 WHERE id >= 2",
        )
        .await;
        assert!(matches!(
            e,
            Error::Schema {
                kind: SchemaKind::UniqueViolation
            }
        ));
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|a|10", "2|b|20", "3|c|30"]
        );
        // The connection is fully usable after the failed statement.
        exec(&mut db, "UPDATE t SET b = 21 WHERE id = 2")
            .await
            .unwrap();
        assert_eq!(
            collect(&mut db, "SELECT b FROM t WHERE id = 2").await,
            vec!["21"]
        );
    });
}

#[test]
fn update_or_ignore_skips_conflicts() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE)",
        )
        .await
        .unwrap();
        exec(&mut db, "INSERT INTO t VALUES (1,'x'),(2,'y'),(3,'z')")
            .await
            .unwrap();
        // Row 3's new value collides with row 1: skipped under
        // OR IGNORE; the statement otherwise succeeds.
        exec(&mut db, "UPDATE OR IGNORE t SET a = 'x' WHERE id = 3")
            .await
            .unwrap();
        exec(&mut db, "UPDATE OR IGNORE t SET a = 'w' WHERE id = 3")
            .await
            .unwrap();
        // Rowid collision under OR IGNORE: skipped too.
        exec(&mut db, "UPDATE OR IGNORE t SET id = 1 WHERE id = 2")
            .await
            .unwrap();
        assert_eq!(
            collect(&mut db, "SELECT * FROM t ORDER BY id").await,
            vec!["1|x", "2|y", "3|w"]
        );
        // OR REPLACE is a named v0.5 refusal, not a silent ABORT.
        assert!(matches!(
            exec_err(&mut db, "UPDATE OR REPLACE t SET a = 'x' WHERE id = 3").await,
            Error::Parse { .. }
        ));
    });
}

// ---------------- deep trees: interior index keys ----------------

#[test]
fn deep_tree_index_delete_and_update() {
    block_on(async {
        let path = common::temp_db("v05-deep-h");
        let cfg = Config::default().page_size(512);
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, cfg).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_k ON t(k)").await.unwrap();
            // 400 rows at 512-byte pages: table and index trees
            // are several levels deep, with many keys resident in
            // interior index pages.
            for chunk in 0..8 {
                let mut sql = String::from("INSERT INTO t VALUES ");
                for i in 0..50 {
                    let n = chunk * 50 + i + 1;
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push_str(&format!("({n},'key-{n:04}',{n})"));
                }
                exec(&mut db, &sql).await.unwrap();
            }
            // Delete every second row: half the index entries go,
            // interior-resident ones among them (predecessor
            // replacement in btree::index_delete).
            exec(&mut db, "DELETE FROM t WHERE id % 2 = 0")
                .await
                .unwrap();
            // Rewrite most surviving keys.
            exec(&mut db, "UPDATE t SET k = 'upd-' || id WHERE id % 3 != 0")
                .await
                .unwrap();
            // Lookups agree with a full scan.
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE k = 'upd-7'").await,
                vec!["7"]
            );
            assert_eq!(
                collect(&mut db, "SELECT count(*) FROM t").await,
                vec!["200"]
            );
        }
        integrity(&path);
        // The oracle reaches the same end state by the same script.
        let opath = common::temp_db("v05-deep-o");
        common::cli(
            &opath,
            "PRAGMA page_size=512;
             CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER);
             CREATE INDEX ix_k ON t(k);",
        );
        for chunk in 0..8 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..50 {
                let n = chunk * 50 + i + 1;
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({n},'key-{n:04}',{n})"));
            }
            common::cli(&opath, &sql);
        }
        common::cli(
            &opath,
            "DELETE FROM t WHERE id % 2 = 0;
             UPDATE t SET k = 'upd-' || id WHERE id % 3 != 0;",
        );
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id;"),
            oracle_rows(&opath, "SELECT * FROM t ORDER BY id;")
        );
    });
}

#[test]
fn oracle_built_deep_tree_hipp_deletes() {
    block_on(async {
        let path = common::temp_db("v05-deep-o2");
        common::cli(
            &path,
            "PRAGMA page_size=512;
             CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER);
             CREATE UNIQUE INDEX ux_k ON t(k);",
        );
        for chunk in 0..6 {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..50 {
                let n = chunk * 50 + i + 1;
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({n},'key-{n:04}',{n})"));
            }
            common::cli(&path, &sql);
        }
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            // hipp deletes from the oracle's own balanced trees,
            // interior keys included, and updates UNIQUE keys.
            exec(&mut db, "DELETE FROM t WHERE id % 3 = 0")
                .await
                .unwrap();
            exec(&mut db, "UPDATE t SET k = 'hipp-' || id WHERE id <= 30")
                .await
                .unwrap();
            assert_eq!(
                collect(&mut db, "SELECT count(*) FROM t").await,
                vec!["200"]
            );
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE k = 'hipp-7'").await,
                vec!["7"]
            );
        }
        integrity(&path);
    });
}

// ---------------- overflow + freelist ----------------

#[test]
fn delete_overflow_rows_return_pages() {
    block_on(async {
        let path = common::temp_db("v05-ovf");
        let big = "z".repeat(5000);
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)",
            )
            .await
            .unwrap();
            for i in 1..=6 {
                exec(&mut db, &format!("INSERT INTO t VALUES ({i}, '{big}')"))
                    .await
                    .unwrap();
            }
            exec(&mut db, "DELETE FROM t WHERE id <= 5").await.unwrap();
        }
        integrity(&path);
        let pages: i64 = common::cli(&path, "PRAGMA page_count;")
            .trim()
            .parse()
            .expect("page_count");
        let free: i64 = common::cli(&path, "PRAGMA freelist_count;")
            .trim()
            .parse()
            .expect("freelist_count");
        // The file must not have shrunk (rollback mode never
        // truncates outside a rollback/VACUUM): 6 rows at one
        // overflow page each + header + interior root + 2
        // leaves = 10 pages before and after the delete.
        // Five deleted rows return at least their five overflow
        // pages (the mid-statement rebuild of the emptied leaf
        // shuffles a few more through the freelist).
        assert!(free >= 5, "freelist_count {free} after overflow deletes");
        assert_eq!(pages, 10, "page_count {pages}");
        // Refilling reuses the freelist instead of growing.
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            for i in 11..=15 {
                exec(&mut db, &format!("INSERT INTO t VALUES ({i}, '{big}')"))
                    .await
                    .unwrap();
            }
        }
        integrity(&path);
        let pages2: i64 = common::cli(&path, "PRAGMA page_count;")
            .trim()
            .parse()
            .expect("page_count");
        assert_eq!(pages2, pages, "refill must reuse freed pages");
    });
}

// ---------------- DROP TABLE ----------------

#[test]
fn drop_table_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("v05-drop-h");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE keep (id INTEGER PRIMARY KEY, x TEXT)",
            )
            .await
            .unwrap();
            exec(&mut db, "INSERT INTO keep VALUES (1,'stay')")
                .await
                .unwrap();
            exec(
                &mut db,
                "CREATE TABLE gone (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_gb ON gone(b)")
                .await
                .unwrap();
            let big = "y".repeat(3000);
            exec(
                &mut db,
                &format!("INSERT INTO gone VALUES (1,'one',10),(2,'two',20),(3,'{big}',30)"),
            )
            .await
            .unwrap();
            exec(&mut db, "DROP TABLE gone").await.unwrap();
            // The name is free immediately, indexes and all.
            assert!(db.tables().iter().all(|t| t.name != b"gone"));
            assert!(db.indexes().iter().all(|i| i.table != b"gone"));
            exec(&mut db, "CREATE TABLE gone (n TEXT)").await.unwrap();
            exec(&mut db, "INSERT INTO gone VALUES ('back')")
                .await
                .unwrap();
            assert_eq!(collect(&mut db, "SELECT * FROM keep").await, vec!["1|stay"]);
            assert_eq!(collect(&mut db, "SELECT * FROM gone").await, vec!["back"]);
        }
        integrity(&path);
        assert_eq!(
            oracle_rows(
                &path,
                "SELECT type, name, tbl_name FROM sqlite_schema ORDER BY rowid;"
            ),
            vec!["table|keep|keep", "table|gone|gone"]
        );
        let free: i64 = common::cli(&path, "PRAGMA freelist_count;")
            .trim()
            .parse()
            .expect("freelist_count");
        assert!(free >= 2, "dropped pages returned: freelist {free}");
    });
}

#[test]
fn drop_table_oracle_writes_hipp_reads() {
    block_on(async {
        let path = common::temp_db("v05-drop-o");
        common::cli(
            &path,
            "CREATE TABLE a (id INTEGER PRIMARY KEY, x TEXT);
             CREATE TABLE b (id INTEGER PRIMARY KEY, y TEXT UNIQUE);
             CREATE INDEX ix_b ON b(y);
             INSERT INTO a VALUES (1,'one');
             INSERT INTO b VALUES (1,'uno'),(2,'dos');
             DROP TABLE b;",
        );
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            assert_eq!(collect(&mut db, "SELECT * FROM a").await, vec!["1|one"]);
            assert!(matches!(
                exec_err(&mut db, "SELECT * FROM b").await,
                Error::Schema {
                    kind: SchemaKind::UnknownTable
                }
            ));
            // hipp can recreate and use the name over the oracle's
            // freelist.
            exec(&mut db, "CREATE TABLE b (z INTEGER)").await.unwrap();
            exec(&mut db, "INSERT INTO b VALUES (42)").await.unwrap();
        }
        integrity(&path);
    });
}

#[test]
fn drop_table_and_view_name_rules() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)")
            .await
            .unwrap();
        exec(&mut db, "CREATE VIEW v AS SELECT id, a FROM t")
            .await
            .unwrap();
        // Missing table: error without IF EXISTS, no-op with it.
        assert!(matches!(
            exec_err(&mut db, "DROP TABLE nosuch").await,
            Error::Schema {
                kind: SchemaKind::UnknownTable
            }
        ));
        exec(&mut db, "DROP TABLE IF EXISTS nosuch").await.unwrap();
        // A view is not a table, even under IF EXISTS (oracle rule).
        assert!(matches!(
            exec_err(&mut db, "DROP TABLE v").await,
            Error::Schema {
                kind: SchemaKind::IsView
            }
        ));
        assert!(matches!(
            exec_err(&mut db, "DROP TABLE IF EXISTS v").await,
            Error::Schema {
                kind: SchemaKind::IsView
            }
        ));
        // Writes to a view are refused the same way.
        assert!(matches!(
            exec_err(&mut db, "UPDATE v SET a = 'x'").await,
            Error::Schema {
                kind: SchemaKind::IsView
            }
        ));
        assert!(matches!(
            exec_err(&mut db, "DELETE FROM v").await,
            Error::Schema {
                kind: SchemaKind::IsView
            }
        ));
        // Writes to a missing table.
        assert!(matches!(
            exec_err(&mut db, "UPDATE nosuch SET a = 'x'").await,
            Error::Schema {
                kind: SchemaKind::UnknownTable
            }
        ));
        assert!(matches!(
            exec_err(&mut db, "DELETE FROM nosuch").await,
            Error::Schema {
                kind: SchemaKind::UnknownTable
            }
        ));
        // And the mirror: DROP VIEW on a table is refused even
        // under IF EXISTS.
        assert!(matches!(
            exec_err(&mut db, "DROP VIEW IF EXISTS t").await,
            Error::Schema {
                kind: SchemaKind::IsTable
            }
        ));
        // The view itself still drops as a view.
        exec(&mut db, "DROP VIEW v").await.unwrap();
        exec(&mut db, "DROP TABLE t").await.unwrap();
    });
}

// ---------------- transactions ----------------

#[test]
fn write_side_inside_explicit_transactions() {
    block_on(async {
        let path = common::temp_db("v05-txn");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE)",
            )
            .await
            .unwrap();
            exec(&mut db, "CREATE INDEX ix_a ON t(a)").await.unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')")
                .await
                .unwrap();
            exec(&mut db, "CREATE TABLE u (n INTEGER)").await.unwrap();
            exec(&mut db, "INSERT INTO u VALUES (1)").await.unwrap();

            // UPDATE + DELETE + DROP TABLE, all rolled back.
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "UPDATE t SET a = 'z' WHERE id = 1")
                .await
                .unwrap();
            exec(&mut db, "DELETE FROM t WHERE id = 3").await.unwrap();
            exec(&mut db, "DROP TABLE u").await.unwrap();
            exec(&mut db, "ROLLBACK").await.unwrap();
            assert_eq!(
                collect(&mut db, "SELECT * FROM t ORDER BY id").await,
                vec!["1|a", "2|b", "3|c"]
            );
            assert_eq!(collect(&mut db, "SELECT * FROM u").await, vec!["1"]);
            assert_eq!(
                collect(&mut db, "SELECT id FROM t WHERE a = 'b'").await,
                vec!["2"]
            );

            // The same work committed this time.
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "UPDATE t SET a = 'z' WHERE id = 1")
                .await
                .unwrap();
            exec(&mut db, "DELETE FROM t WHERE id = 3").await.unwrap();
            exec(&mut db, "DROP TABLE u").await.unwrap();
            exec(&mut db, "COMMIT").await.unwrap();
            assert_eq!(
                collect(&mut db, "SELECT * FROM t ORDER BY id").await,
                vec!["1|z", "2|b"]
            );
            assert!(matches!(
                exec_err(&mut db, "SELECT * FROM u").await,
                Error::Schema {
                    kind: SchemaKind::UnknownTable
                }
            ));
        }
        integrity(&path);
        assert_eq!(
            oracle_rows(&path, "SELECT * FROM t ORDER BY id;"),
            vec!["1|z", "2|b"]
        );
    });
}
