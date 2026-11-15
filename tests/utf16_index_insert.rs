//! Index inserts into a UTF-16 database place each key by its
//! UTF-8 view (the form `assemble_payload` hands the B-tree layer),
//! never by the raw UTF-16 bytes on the page. Runs without the
//! oracle: the index must pass `integrity_check`, find every key
//! and list them in order.

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

fn key(i: u32) -> String {
    // Scrambled insertion order, mixed lengths, a non-ASCII tail.
    let k = (i * 7919) % 1000;
    format!("k{k:03}-{}é", "x".repeat((k % 7) as usize))
}

async fn rows(db: &mut Connection<MemStorage>, sql: &str) -> Vec<String> {
    let mut st = db.prepare(sql).await.expect("prepare");
    let mut out = Vec::new();
    while st.step().await.expect("step") == Step::Row {
        let b = st.column_bytes(0).unwrap_or(&[]);
        out.push(String::from_utf8_lossy(b).into_owned());
    }
    out
}

fn check(encoding: &str) {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        rows(&mut db, &format!("PRAGMA encoding = '{encoding}'")).await;
        rows(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        )
        .await;
        // `a` is maintained row by row (index_insert); `b` is built
        // over the full table by CREATE INDEX.
        rows(&mut db, "CREATE INDEX a ON t(name)").await;
        rows(&mut db, "BEGIN").await;
        for i in 0..1000 {
            rows(
                &mut db,
                &format!("INSERT INTO t VALUES ({i}, '{}')", key(i)),
            )
            .await;
        }
        rows(&mut db, "COMMIT").await;
        rows(&mut db, "CREATE INDEX b ON t(name COLLATE NOCASE DESC)").await;

        assert_eq!(rows(&mut db, "PRAGMA integrity_check").await, ["ok"]);
        let mut want: Vec<String> = (0..1000).map(key).collect();
        want.sort();
        let got = rows(&mut db, "SELECT name FROM t INDEXED BY a ORDER BY name").await;
        assert_eq!(got, want, "{encoding}: index a order");
        for i in (0..1000).step_by(37) {
            let k = key(i);
            let q = format!("SELECT name FROM t INDEXED BY a WHERE name = '{k}'");
            assert_eq!(
                rows(&mut db, &q).await,
                std::slice::from_ref(&k),
                "{encoding}: a finds {k}"
            );
            let q = format!("SELECT name FROM t INDEXED BY b WHERE name = '{k}' COLLATE NOCASE");
            assert_eq!(
                rows(&mut db, &q).await,
                std::slice::from_ref(&k),
                "{encoding}: b finds {k}"
            );
        }
    });
}

#[test]
fn utf16le_index_inserts_find_and_order_every_key() {
    check("UTF-16le");
}

#[test]
fn utf16be_index_inserts_find_and_order_every_key() {
    check("UTF-16be");
}

#[test]
fn utf8_index_inserts_find_and_order_every_key() {
    check("UTF-8");
}
