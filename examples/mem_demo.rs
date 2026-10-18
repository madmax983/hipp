//! End-to-end demo on in-memory storage (SPEC §10): create a
//! table, insert rows, scan them back. Run with `cargo run
//! --example mem_demo`.

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step, Value};

fn main() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut st = db
            .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL)")
            .await
            .expect("prepare create");
        assert_eq!(st.step().await.expect("create"), Step::Done);
        drop(st);
        for sql in [
            "INSERT INTO t VALUES (1, 'one', 1.5)",
            "INSERT INTO t VALUES (2, 'two', NULL)",
            "INSERT INTO t (name) VALUES ('auto')",
        ] {
            let mut st = db.prepare(sql).await.expect("prepare insert");
            assert_eq!(st.step().await.expect("insert"), Step::Done);
            drop(st);
        }
        let mut q = db
            .prepare("SELECT id, name, v FROM t")
            .await
            .expect("prepare select");
        while q.step().await.expect("step") == Step::Row {
            let id = q.column_i64(0).expect("id");
            let name = q
                .column_bytes(1)
                .map(|b| String::from_utf8_lossy(b).into_owned());
            let v = match q.column_value(2) {
                Some(Value::Real(f)) => f.to_string(),
                Some(Value::Null) => "NULL".to_string(),
                other => format!("{other:?}"),
            };
            println!("{id}\t{}\t{v}", name.as_deref().unwrap_or("NULL"));
        }
    });
}
