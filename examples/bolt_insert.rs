//! Profiling workload (valgrind --tool=callgrind target): a bulk load,
//! n rows inserted into a rowid table inside one transaction, one
//! prepared INSERT per row (no indexes), on in-memory storage, driven
//! through the public Connection API. The table-INSERT phase of
//! `bolt_workload`, on its own.
//!
//! Reproduce (instruction counts, deterministic):
//!   CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_insert
//!   valgrind --tool=callgrind --callgrind-out-file=cg.out \
//!     target/release/examples/bolt_insert 2000
//!   callgrind_annotate --inclusive=yes cg.out

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

fn n() -> i64 {
    std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000)
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
        run!("BEGIN");
        for i in 1..=n {
            run!(format!(
                "INSERT INTO t VALUES ({i}, {}, 'name-{i}', {}.5)",
                i % 50,
                i
            ));
        }
        run!("COMMIT");
        run!("SELECT COUNT(*), SUM(g) FROM t");
        println!("{total}");
    });
}
