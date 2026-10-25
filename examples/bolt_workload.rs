//! Deterministic mixed workload on in-memory storage, driven through
//! the public Connection/Statement API. Intended for counter-based
//! measurement (callgrind / dhat), not wall-clock:
//! `cargo build --release --example bolt_workload` then
//! `valgrind --tool=callgrind target/release/examples/bolt_workload [phase]`.
//! Phases: all (default), load, point, scan, join, agg, write, subq.

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

const N: i64 = 2000;

async fn run(db: &mut Connection<MemStorage>, sql: &str) -> u64 {
    let mut st = db.prepare(sql).await.expect("prepare");
    let mut rows = 0u64;
    let mut sink = 0i64;
    while st.step().await.expect("step") == Step::Row {
        rows += 1;
        for c in 0..st.column_count() {
            sink = sink.wrapping_add(st.column_i64(c).unwrap_or(0));
            sink = sink.wrapping_add(st.column_bytes(c).map_or(0, |b| b.len() as i64));
        }
    }
    std::hint::black_box(sink);
    rows
}

fn main() {
    let phase = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let on = |p: &str| phase == "all" || phase == p;
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default().cache_pages(64))
            .await
            .expect("open");
        run(&mut db, "CREATE TABLE u (id INTEGER PRIMARY KEY, grp INTEGER, name TEXT, score REAL)").await;
        run(&mut db, "CREATE TABLE o (id INTEGER PRIMARY KEY, uid INTEGER, amt INTEGER, note TEXT)").await;
        run(&mut db, "CREATE INDEX o_uid ON o(uid)").await;
        run(&mut db, "CREATE INDEX u_grp ON u(grp)").await;
        // `load` always runs: every other phase needs the data.
        run(&mut db, "BEGIN").await;
        for i in 1..=N {
            let g = i % 37;
            run(&mut db, &format!("INSERT INTO u VALUES ({i}, {g}, 'user-{i}', {}.5)", i % 101)).await;
        }
        for i in 1..=N * 3 {
            let uid = (i * 7) % N + 1;
            run(&mut db, &format!("INSERT INTO o VALUES ({i}, {uid}, {}, 'note number {i}')", i % 997)).await;
        }
        run(&mut db, "COMMIT").await;
        if on("point") {
            for i in 1..=500 {
                run(&mut db, &format!("SELECT * FROM u WHERE id = {}", (i * 13) % N + 1)).await;
                run(&mut db, &format!("SELECT amt, note FROM o WHERE uid = {}", (i * 17) % N + 1)).await;
            }
        }
        if on("scan") {
            for _ in 0..5 {
                run(&mut db, "SELECT id, name FROM u WHERE score > 50 AND grp < 20").await;
                run(&mut db, "SELECT * FROM o WHERE note LIKE '%99%' OR amt BETWEEN 10 AND 40").await;
                run(&mut db, "SELECT id, amt FROM o ORDER BY amt DESC, id LIMIT 100").await;
            }
        }
        if on("join") {
            for _ in 0..3 {
                run(&mut db, "SELECT u.name, o.amt FROM u JOIN o ON o.uid = u.id WHERE u.grp = 5").await;
            }
        }
        if on("agg") {
            for _ in 0..5 {
                run(&mut db, "SELECT grp, COUNT(*), AVG(score), MAX(id) FROM u GROUP BY grp").await;
                run(&mut db, "SELECT uid, SUM(amt) FROM o GROUP BY uid HAVING SUM(amt) > 2000").await;
            }
        }
        if on("write") {
            run(&mut db, "BEGIN").await;
            run(&mut db, "UPDATE o SET amt = amt + 1 WHERE uid < 500").await;
            run(&mut db, "DELETE FROM o WHERE amt > 990").await;
            run(&mut db, "COMMIT").await;
        }
        if on("subq") {
            run(&mut db, "SELECT id FROM u WHERE id IN (SELECT uid FROM o WHERE amt > 900)").await;
            run(&mut db, "SELECT id, (SELECT COUNT(*) FROM o WHERE o.uid = u.id) FROM u WHERE grp = 3").await;
        }
    });
}
