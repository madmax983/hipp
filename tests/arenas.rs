//! A9: arena exhaustion semantics (SPEC §9.2, §7.5.4) — errors,
//! never panics; a failed write leaves the file byte-identical.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, MemStorage, Scope, Step};

async fn seeded() -> MemStorage {
    let mem = MemStorage::new();
    let mut db = Connection::open(mem.clone(), Config::default())
        .await
        .expect("open");
    for sql in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t VALUES (1, 'one')",
        "INSERT INTO t VALUES (2, 'two')",
    ] {
        let mut st = db.prepare(sql).await.expect("prep");
        assert_eq!(st.step().await.expect("step"), Step::Done);
        drop(st);
    }
    mem
}

#[test]
fn a9_statement_arena_exhaustion() {
    block_on(async {
        let mem = seeded().await;
        // A statement arena too small to hold the parsed statement:
        // prepare fails with ArenaExhausted{Statement}, no panic.
        let cfg = Config::default().statement_arena(16, 16);
        let mut db = Connection::open(mem.clone(), cfg).await.expect("open");
        let r = db.prepare("SELECT id, name FROM t").await;
        assert!(
            matches!(
                r,
                Err(Error::ArenaExhausted {
                    scope: Scope::Statement,
                    ..
                })
            ),
            "select with tiny stmt arena: {:?}",
            r.err()
        );
    });
}

#[test]
fn a9_txn_arena_exhaustion_rolls_back() {
    block_on(async {
        let mem = seeded().await;
        let before = mem.snapshot();
        // The pager's per-transaction charge (cache_cap*4 + 64)
        // cannot fit an 8-byte transaction arena: the INSERT must
        // fail with ArenaExhausted{Transaction} before any byte is
        // written anywhere.
        let cfg = Config::default().txn_arena(8, 8);
        let mut db = Connection::open(mem.clone(), cfg).await.expect("open");
        let mut st = db
            .prepare("INSERT INTO t VALUES (3, 'three')")
            .await
            .expect("prep");
        let r = st.step().await;
        assert!(
            matches!(
                r,
                Err(Error::ArenaExhausted {
                    scope: Scope::Transaction,
                    ..
                })
            ),
            "insert with tiny txn arena: {:?}",
            r.err()
        );
        drop(st);
        drop(db);
        assert_eq!(mem.snapshot(), before, "file byte-identical after failed insert");
        assert!(mem.journal_snapshot().is_none(), "no journal left behind");
        // And the oracle agrees the file is untouched and valid.
        let path = common::temp_db("a9");
        std::fs::write(&path, &before).expect("write");
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(common::cli(&path, "SELECT count(*) FROM t;").trim(), "2");
    });
}
