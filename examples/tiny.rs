//! Everything the **tiny** profile does (`--no-default-features`,
//! ≤ 128 KiB of code on thumbv7em): rowid tables, INSERT, scan and
//! point-lookup SELECT, explicit transactions, the rollback journal
//! (crash recovery: `examples/custom_storage.rs`), overflow values,
//! bounded arenas, and the catalog API.
//!
//! Run it in a tiny build (the same code also runs in every larger
//! profile):
//!
//! ```text
//! cargo run --example tiny --no-default-features
//! ```
//!
//! `docs/profiles/tiny.md` walks through this file section by section.

mod common;

use common::{check, exec, exec_all, fails, section};
use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step, Value};

fn main() {
    block_on(async {
        // `Config::tiny()` is the reference embedded configuration:
        // a 4-page pool and 16 / 8 / 16 KiB statement / transaction /
        // catalog arenas (56 KiB of engine RAM). `Config::default()`
        // is the roomier hosted configuration.
        let storage = MemStorage::new();
        let mut db = Connection::open(storage, Config::tiny())
            .await
            .expect("open");

        section("CREATE TABLE");
        exec_all(
            &mut db,
            &[
                // An INTEGER PRIMARY KEY column is the rowid.
                "CREATE TABLE sensors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, room TEXT)",
                "CREATE TABLE readings (sensor INTEGER, at INTEGER, celsius REAL, raw BLOB)",
                // IF NOT EXISTS is a no-op when the table is there.
                "CREATE TABLE IF NOT EXISTS sensors (x)",
            ],
        )
        .await;
        // The catalog is readable from Rust.
        let names: Vec<String> = db
            .tables()
            .iter()
            .map(|t| String::from_utf8_lossy(t.name).into_owned())
            .collect();
        println!("tables: {names:?}");
        assert_eq!(names, ["sensors", "readings"]);

        section("INSERT");
        exec_all(
            &mut db,
            &[
                // Multi-row VALUES; an omitted rowid is assigned.
                "INSERT INTO sensors (name, room) VALUES ('north', 'hall'), ('south', 'lab'), ('east', 'lab')",
                // An explicit rowid.
                "INSERT INTO sensors VALUES (10, 'roof', NULL)",
                // Every storage class: INTEGER, REAL, TEXT, BLOB, NULL.
                "INSERT INTO readings VALUES (1, 1700000000, 21.5, X'00ff'), (2, 1700000000, 19.25, NULL)",
                "INSERT INTO readings VALUES (1, 1700000060, 22, NULL), (10, 1700000060, -4.5, X'cafe')",
            ],
        )
        .await;

        section("SELECT: full scans, in rowid order");
        check(
            &mut db,
            "SELECT * FROM sensors",
            &["1|north|hall", "2|south|lab", "3|east|lab", "10|roof|"],
        )
        .await;
        // Column affinity: 22 went into a REAL column and reads back
        // as 22.0, as on SQLite.
        check(
            &mut db,
            "SELECT sensor, celsius, raw FROM readings",
            &["1|21.5|X'00FF'", "2|19.25|", "1|22.0|", "10|-4.5|X'CAFE'"],
        )
        .await;
        check(
            &mut db,
            "SELECT rowid, name FROM sensors",
            &["1|north", "2|south", "3|east", "10|roof"],
        )
        .await;

        section("SELECT: WHERE rowid = literal");
        // The one filter the tiny executor has: a rowid equality (on
        // `rowid` / `oid` / `_rowid_`, or the INTEGER PRIMARY KEY
        // alias). It is a B-tree point lookup, not a scan.
        check(
            &mut db,
            "SELECT name FROM sensors WHERE rowid = 10",
            &["roof"],
        )
        .await;
        check(
            &mut db,
            "SELECT name, room FROM sensors WHERE id = 2",
            &["south|lab"],
        )
        .await;
        check(&mut db, "SELECT name FROM sensors WHERE id = 99", &[]).await;
        // Any other WHERE needs the expression evaluator (`query`, in
        // `full`). In tiny, filter in Rust while you step a scan:
        let mut st = db
            .prepare("SELECT name, room FROM sensors")
            .await
            .expect("prepare");
        let mut in_lab = Vec::new();
        while st.step().await.expect("step") == Step::Row {
            if st.column_bytes(1) == Some(&b"lab"[..]) {
                in_lab.push(
                    String::from_utf8_lossy(st.column_bytes(0).unwrap_or_default()).into_owned(),
                );
            }
        }
        drop(st);
        println!("sensors in the lab (filtered in Rust): {in_lab:?}");
        assert_eq!(in_lab, ["south", "east"]);

        section("Reading columns from Rust");
        let mut st = db
            .prepare("SELECT id, name, room FROM sensors WHERE rowid = 10")
            .await
            .expect("prepare");
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert_eq!(st.column_count(), 3);
        assert_eq!(st.column_i64(0), Some(10));
        assert_eq!(st.column_bytes(1), Some(&b"roof"[..]));
        assert!(st.column_is_null(2));
        assert_eq!(st.column_value(1), Some(Value::Text(b"roof")));
        println!(
            "id={:?} name={:?} room is NULL: {}",
            st.column_i64(0),
            st.column_bytes(1).map(String::from_utf8_lossy),
            st.column_is_null(2)
        );
        assert_eq!(st.step().await.expect("step"), Step::Done);
        drop(st);

        section("Constraints");
        // NOT NULL and a duplicate rowid are named errors.
        fails(&mut db, "INSERT INTO sensors (name) VALUES (NULL)").await;
        fails(&mut db, "INSERT INTO sensors VALUES (1, 'again', NULL)").await;
        fails(&mut db, "SELECT nope FROM sensors").await;
        fails(&mut db, "SELECT * FROM missing").await;

        section("Transactions");
        // ROLLBACK discards everything since BEGIN.
        exec_all(
            &mut db,
            &["BEGIN", "INSERT INTO sensors (name) VALUES ('temporary')"],
        )
        .await;
        assert!(db.in_explicit_transaction());
        check(
            &mut db,
            "SELECT name FROM sensors WHERE rowid = 11",
            &["temporary"],
        )
        .await;
        exec(&mut db, "ROLLBACK").await;
        check(&mut db, "SELECT name FROM sensors WHERE rowid = 11", &[]).await;
        // COMMIT makes a batch durable as one unit (one journal, one
        // sync), which is also much faster than one commit per row.
        exec(&mut db, "BEGIN").await;
        for i in 0..50 {
            let sql = format!(
                "INSERT INTO readings VALUES (3, {}, {i}.5, NULL)",
                1_700_000_100 + i
            );
            exec(&mut db, &sql).await;
        }
        exec(&mut db, "COMMIT").await;
        assert!(!db.in_transaction());
        let n = common::rows(&mut db, "SELECT at FROM readings").await.len() - 4;
        println!("committed {n} readings in one transaction");
        assert_eq!(n, 50);

        section("Overflow values and arena limits");
        // Values bigger than a page (4096 B here) spill to overflow
        // chains, transparently.
        let big = "x".repeat(6_000);
        exec(
            &mut db,
            &format!("INSERT INTO sensors (id, name) VALUES (11, '{big}')"),
        )
        .await;
        let mut st = db
            .prepare("SELECT name FROM sensors WHERE rowid = 11")
            .await
            .expect("prepare");
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert_eq!(st.column_bytes(0).map(<[u8]>::len), Some(6_000));
        println!("read back a {}-byte value", big.len());
        drop(st);
        // Every statement works inside a bounded arena (16 KiB under
        // `Config::tiny()`). A statement that needs more fails with
        // `ArenaExhausted`, naming the scope and the bytes it needed;
        // it never panics or aborts. Raise the limit with
        // `Config::statement_arena(init, max)` if you need to.
        let huge = "y".repeat(20_000);
        let e = fails(
            &mut db,
            &format!("INSERT INTO sensors (name) VALUES ('{huge}')"),
        )
        .await;
        assert!(e.starts_with("ArenaExhausted"), "{e}");

        section("Out of the tiny profile");
        // Statements a profile does not build are refused with a named
        // error, never half-run. These need `query` / `write` / `sql`
        // (the `full` profile); in a larger build they succeed, so only
        // check them in a tiny one.
        if !cfg!(feature = "query") {
            fails(&mut db, "UPDATE sensors SET room = 'attic'").await;
            fails(&mut db, "SELECT count(*) FROM sensors").await;
            fails(&mut db, "SELECT * FROM sensors ORDER BY name").await;
            fails(&mut db, "SELECT name FROM sensors WHERE room = 'lab'").await;
            fails(&mut db, "CREATE INDEX sensors_room ON sensors(room)").await;
        }

        section("The file is a SQLite database");
        // MemStorage holds the database image in memory; `snapshot`
        // copies it out. Those bytes are a valid SQLite 3 file:
        // write them to disk and `sqlite3` opens them.
        let stats = db.arena_stats();
        println!(
            "page_count={} catalog_high_water={}B txn_high_water={}B",
            stats.page_count, stats.catalog_high_water, stats.txn_high_water
        );
        let image = db.storage().snapshot();
        assert_eq!(&image[..16], b"SQLite format 3\0");
        println!(
            "database image: {} bytes, header {:?}",
            image.len(),
            "SQLite format 3"
        );
        db.close().await.expect("close");

        // ...and reopening the image gives the same rows back.
        let mut db = Connection::open(MemStorage::from_bytes(image), Config::tiny())
            .await
            .expect("reopen");
        check(
            &mut db,
            "SELECT name FROM sensors WHERE rowid = 1",
            &["north"],
        )
        .await;

        // Crash recovery (a commit cut off by power loss, rolled back
        // by the next open) is shown with a custom `Storage` in
        // `examples/custom_storage.rs`.
    });
    println!("\ntiny: ok");
}
