//! Profiling workload (valgrind --tool=callgrind target): bulk insert,
//! index build, point lookups, filtered scan, join, GROUP BY on
//! in-memory storage, driven through the public Connection API.
//!
//! Reproduce (instruction counts, deterministic):
//!   CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_workload
//!   valgrind --tool=callgrind --callgrind-out-file=cg.out \
//!     target/release/examples/bolt_workload 1000
//!   callgrind_annotate --inclusive=yes cg.out

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

fn n() -> i64 {
    std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000)
}

fn main() {
    let n = n();
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut total = 0i64;
        macro_rules! run {
            ($sql:expr) => {{
                let mut st = db.prepare(&$sql[..]).await.expect("prepare");
                while st.step().await.expect("step") == Step::Row {
                    total += st.column_i64(0).unwrap_or(0);
                }
            }};
        }
        run!("CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, name TEXT, v REAL)");
        run!("CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, w INTEGER)");
        run!("BEGIN");
        for i in 1..=n {
            run!(format!(
                "INSERT INTO t VALUES ({i}, {}, 'name-{i}', {}.5)",
                i % 50,
                i
            ));
            run!(format!(
                "INSERT INTO u VALUES ({i}, {}, {})",
                (i * 7) % n + 1,
                i % 13
            ));
        }
        run!("COMMIT");
        run!("CREATE INDEX t_g ON t(g)");
        run!("CREATE INDEX u_t ON u(t_id)");
        for i in (1..=n).step_by(5) {
            run!(format!("SELECT id FROM t WHERE id = {i}"));
            run!(format!("SELECT COUNT(*) FROM t WHERE g = {}", i % 50));
        }
        run!("SELECT id FROM t WHERE v > 100.0 AND name LIKE 'name-1%'");
        run!("SELECT t.id FROM t JOIN u ON u.t_id = t.id WHERE u.w = 3");
        run!("SELECT SUM(v) FROM t GROUP BY g");
        run!("UPDATE t SET v = v + 1 WHERE g = 7");
        println!("{total}");
    });
}
