//! v0.2 dialect growth, differentially vs the pinned oracle
//! (SPEC §9.4): multi-row VALUES, `WHERE rowid = <literal>`,
//! explicit transactions BEGIN/COMMIT/ROLLBACK. Oracle-side
//! visibility is the verdict for transactions: the oracle must see
//! committed data and never see rolled-back data.

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

async fn exec_err<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Error<S::Error>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    st.step().await.expect_err("step must fail")
}

/// Collect `SELECT <cols>` as `a|b|c` lines via hipp.
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

// ---------------- multi-row VALUES ----------------

#[test]
fn multi_row_values_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("v02-multi-a");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL)")
                .await
                .unwrap();
            // Mixed explicit + auto rowids, out of order, one row.
            exec(
                &mut db,
                "INSERT INTO t VALUES (3, 'three', 3.5), (1, 'one', NULL), (2, 'two', -0.25)",
            )
            .await
            .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (10, 'ten', 0.0)").await.unwrap();
            // Auto rowids continue from the max (11, 12).
            exec(&mut db, "INSERT INTO t (name) VALUES ('auto-a'), ('auto-b')")
                .await
                .unwrap();
            // Explicit column list, reordered.
            exec(
                &mut db,
                "INSERT INTO t (name, id) VALUES ('x', 100), ('y', 50)",
            )
            .await
            .unwrap();
        }
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        let out = common::cli(&path, "SELECT id, name, v FROM t;");
        assert_eq!(
            out,
            "\
1|one|
2|two|-0.25
3|three|3.5
10|ten|0.0
11|auto-a|
12|auto-b|
50|y|
100|x|
",
            "oracle version: {}",
            common::oracle_version()
        );
    });
}

#[test]
fn multi_row_values_oracle_writes_hipp_reads() {
    block_on(async {
        let path = common::temp_db("v02-multi-b");
        let out = common::cli(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); \
             INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three'); \
             INSERT INTO t (name) VALUES ('four'), ('five');",
        );
        assert_eq!(out, "");
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let rows = collect(&mut db, "SELECT id, name FROM t").await;
        assert_eq!(rows, vec!["1|one", "2|two", "3|three", "4|four", "5|five"]);
    });
}

#[test]
fn multi_row_values_errors_are_atomic() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default()).await.expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        exec(&mut db, "INSERT INTO t VALUES (1, 'one')").await.unwrap();
        // Width mismatch in the second row: prepare-time error.
        let r = db
            .prepare("INSERT INTO t VALUES (2, 'two'), (3)")
            .await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: hipp::SchemaKind::ColumnCountMismatch
                })
            ),
            "width: {:?}",
            r.err()
        );
        // Duplicate rowid across rows of one statement: the whole
        // statement rolls back, including its earlier rows.
        let e = exec_err(&mut db, "INSERT INTO t VALUES (5, 'five'), (1, 'dup')").await;
        assert!(
            matches!(
                e,
                Error::Schema {
                    kind: hipp::SchemaKind::DuplicateRowid
                }
            ),
            "dup: {e:?}"
        );
        let rows = collect(&mut db, "SELECT id, name FROM t").await;
        assert_eq!(rows, vec!["1|one"], "failed statement left no trace");
    });
}

// ---------------- WHERE rowid = ----------------

#[test]
fn where_rowid_hit_and_miss() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default()).await.expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        exec(
            &mut db,
            "INSERT INTO t VALUES (-7, 'neg'), (1, 'one'), (2, 'two'), (3, 'three')",
        )
        .await
        .unwrap();
        // Hit via the alias column, the pseudo-columns, literals of
        // other types that convert, and negative rowids.
        for (sql, want) in [
            ("SELECT name FROM t WHERE id = 2", vec!["two"]),
            ("SELECT name FROM t WHERE rowid = 1", vec!["one"]),
            ("SELECT name FROM t WHERE _rowid_ = 3", vec!["three"]),
            ("SELECT name FROM t WHERE oid = -7", vec!["neg"]),
            ("SELECT id, name FROM t WHERE id = 2", vec!["2|two"]),
            ("SELECT name FROM t WHERE id = '3'", vec!["three"]),
            ("SELECT name FROM t WHERE id = 2.0", vec!["two"]),
            ("SELECT name FROM t WHERE id = '1.0'", vec!["one"]),
        ] {
            let rows = collect(&mut db, sql).await;
            assert_eq!(rows, want, "{sql}");
        }
        // Misses: absent id, fractional real, non-numeric text,
        // NULL, blob — all yield zero rows, never an error.
        for sql in [
            "SELECT name FROM t WHERE id = 99",
            "SELECT name FROM t WHERE id = 2.5",
            "SELECT name FROM t WHERE id = 'abc'",
            "SELECT name FROM t WHERE id = NULL",
            "SELECT name FROM t WHERE id = x'02'",
        ] {
            let rows = collect(&mut db, sql).await;
            assert_eq!(rows, Vec::<String>::new(), "{sql}");
        }
        // A non-rowid column predicate is beyond v0.2's executor.
        let r = db.prepare("SELECT id FROM t WHERE name = 'one'").await;
        assert!(
            matches!(r, Err(Error::Unsupported)),
            "non-rowid predicate: {:?}",
            r.err()
        );
        // Unknown column is a schema error, as in the select list.
        let r = db.prepare("SELECT id FROM t WHERE nope = 1").await;
        assert!(
            matches!(
                r,
                Err(Error::Schema {
                    kind: hipp::SchemaKind::UnknownColumn
                })
            ),
            "unknown col: {:?}",
            r.err()
        );
    });
}

#[test]
fn where_rowid_deep_tree_and_oracle_agreement() {
    block_on(async {
        let path = common::temp_db("v02-where-deep");
        let payload = "p".repeat(60);
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, data TEXT)")
                .await
                .unwrap();
            for chunk in 0..10 {
                let mut sql = String::from("INSERT INTO t VALUES ");
                for i in 0..50 {
                    let id = chunk * 50 + i + 1;
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str(&format!("({id}, '{payload}{id}')"));
                }
                exec(&mut db, &sql).await.unwrap();
            }
            // Seek hits across the split tree.
            for id in [1, 250, 499, 500] {
                let rows = collect(
                    &mut db,
                    &format!("SELECT id, data FROM t WHERE rowid = {id}"),
                )
                .await;
                assert_eq!(rows, vec![format!("{id}|{payload}{id}")]);
            }
            let rows = collect(&mut db, "SELECT id FROM t WHERE rowid = 501").await;
            assert!(rows.is_empty());
        }
        // The oracle agrees on the same file, hit and miss.
        assert_eq!(
            common::cli(&path, "SELECT id, data FROM t WHERE rowid = 250;"),
            format!("250|{payload}250\n")
        );
        assert_eq!(common::cli(&path, "SELECT id FROM t WHERE rowid = 501;"), "");
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
    });
}

// ---------------- explicit transactions ----------------

#[test]
fn txn_commit_visible_to_oracle() {
    block_on(async {
        let path = common::temp_db("v02-txn-commit");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            exec(&mut db, "BEGIN").await.unwrap();
            assert!(db.in_explicit_transaction());
            exec(&mut db, "INSERT INTO t VALUES (1, 'one'), (2, 'two')")
                .await
                .unwrap();
            // Read-your-writes inside the bracket.
            let rows = collect(&mut db, "SELECT id, name FROM t").await;
            assert_eq!(rows, vec!["1|one", "2|two"]);
            exec(&mut db, "COMMIT").await.unwrap();
            assert!(!db.in_explicit_transaction());
            // Autocommit resumes after the bracket.
            exec(&mut db, "INSERT INTO t VALUES (3, 'three')").await.unwrap();
        }
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(
            common::cli(&path, "SELECT id, name FROM t;"),
            "1|one\n2|two\n3|three\n"
        );
    });
}

#[test]
fn txn_rollback_invisible_to_oracle() {
    block_on(async {
        let path = common::temp_db("v02-txn-rollback");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE keep (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO keep VALUES (1, 'kept')").await.unwrap();
            exec(&mut db, "BEGIN TRANSACTION").await.unwrap();
            exec(&mut db, "INSERT INTO keep VALUES (2, 'gone'), (3, 'alsogone')")
                .await
                .unwrap();
            exec(&mut db, "CREATE TABLE temp_t (id INTEGER PRIMARY KEY)")
                .await
                .unwrap();
            exec(&mut db, "ROLLBACK").await.unwrap();
            assert!(!db.in_explicit_transaction());
            // hipp side: the bracket's rows and table are gone.
            let rows = collect(&mut db, "SELECT id, name FROM keep").await;
            assert_eq!(rows, vec!["1|kept"]);
            let r = db.prepare("SELECT id FROM temp_t").await;
            assert!(
                matches!(
                    r,
                    Err(Error::Schema {
                        kind: hipp::SchemaKind::UnknownTable
                    })
                ),
                "temp table survived rollback: {:?}",
                r.err()
            );
        }
        // Oracle side: only pre-transaction data, file intact.
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(
            common::cli(&path, "SELECT id, name FROM keep;"),
            "1|kept\n"
        );
        assert_eq!(
            common::cli(
                &path,
                "SELECT name FROM sqlite_schema WHERE name='temp_t';"
            ),
            ""
        );
    });
}

#[test]
fn txn_create_commit_visible_to_oracle() {
    block_on(async {
        let path = common::temp_db("v02-txn-create");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "CREATE TABLE made (id INTEGER PRIMARY KEY, x TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO made VALUES (1, 'a'), (2, 'b')")
                .await
                .unwrap();
            exec(&mut db, "END TRANSACTION").await.unwrap();
        }
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(
            common::cli(&path, "SELECT id, x FROM made;"),
            "1|a\n2|b\n"
        );
    });
}

#[test]
fn txn_state_errors() {
    block_on(async {
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default()).await.expect("open");
        exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
        // COMMIT / ROLLBACK with no open bracket.
        for sql in ["COMMIT", "ROLLBACK"] {
            let e = exec_err(&mut db, sql).await;
            assert!(
                matches!(e, Error::TransactionState { nested: false }),
                "{sql}: {e:?}"
            );
        }
        // Nested BEGIN is refused at prepare time already.
        exec(&mut db, "BEGIN").await.unwrap();
        let r = db.prepare("BEGIN").await;
        assert!(
            matches!(r, Err(Error::TransactionState { nested: true })),
            "nested begin: {:?}",
            r.err()
        );
        // A read-only bracket commits fine (no journal ever opened).
        exec(&mut db, "COMMIT").await.unwrap();
        assert!(!db.in_explicit_transaction());
        // Empty bracket + ROLLBACK form.
        exec(&mut db, "BEGIN").await.unwrap();
        exec(&mut db, "ROLLBACK TRANSACTION").await.unwrap();
    });
}

#[test]
fn txn_statement_error_aborts_bracket() {
    block_on(async {
        let path = common::temp_db("v02-txn-abort");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            exec(&mut db, "INSERT INTO t VALUES (1, 'one')").await.unwrap();
            exec(&mut db, "BEGIN").await.unwrap();
            exec(&mut db, "INSERT INTO t VALUES (2, 'two')").await.unwrap();
            // Duplicate rowid: error aborts the whole bracket
            // (v0.2's documented, stricter-than-SQLite rule).
            let e = exec_err(&mut db, "INSERT INTO t VALUES (1, 'dup')").await;
            assert!(
                matches!(
                    e,
                    Error::Schema {
                        kind: hipp::SchemaKind::DuplicateRowid
                    }
                ),
                "dup: {e:?}"
            );
            assert!(!db.in_explicit_transaction(), "bracket closed by abort");
            let rows = collect(&mut db, "SELECT id FROM t").await;
            assert_eq!(rows, vec!["1"], "bracket rows rolled back");
        }
        assert_eq!(common::cli(&path, "SELECT id FROM t;"), "1\n");
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
    });
}
