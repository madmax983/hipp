//! Bound parameters (v0.16, feature `params`): prepare a statement
//! once, then bind, step and reset it for each row — the pin's
//! `sqlite3_bind_*` / `sqlite3_reset` cycle. Run with `cargo run
//! --example params`.

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step, Value};

fn main() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut st = db
            .prepare("CREATE TABLE readings (id INTEGER PRIMARY KEY, sensor TEXT, v REAL)")
            .await
            .expect("prepare create");
        assert_eq!(st.step().await.expect("create"), Step::Done);
        drop(st);

        // One prepared INSERT, executed once per row. `:sensor`
        // and `:v` are named; `parameter_index` finds their
        // indexes.
        let mut ins = db
            .prepare("INSERT INTO readings (sensor, v) VALUES (:sensor, :v)")
            .await
            .expect("prepare insert");
        assert_eq!(ins.parameter_count(), 2);
        let sensor = ins.parameter_index(":sensor").expect(":sensor");
        let v = ins.parameter_index(":v").expect(":v");
        for (name, value) in [("north", 21.5), ("south", 19.25), ("north", 22.0)] {
            ins.bind_text(sensor, name).expect("bind sensor");
            ins.bind_f64(v, value).expect("bind v");
            assert_eq!(ins.step().await.expect("insert"), Step::Done);
            ins.reset();
        }
        // An unbound parameter is NULL.
        ins.clear_bindings();
        ins.bind_text(sensor, "east").expect("bind sensor");
        assert_eq!(ins.step().await.expect("insert"), Step::Done);
        drop(ins);

        // A prepared query re-run with different bindings.
        let mut q = db
            .prepare("SELECT id, v FROM readings WHERE sensor = ?1 ORDER BY id")
            .await
            .expect("prepare select");
        for name in ["north", "east"] {
            q.bind_text(1, name).expect("bind");
            println!("{name}:");
            while q.step().await.expect("step") == Step::Row {
                let id = q.column_i64(0).expect("id");
                let v = match q.column_value(1) {
                    Some(Value::Real(f)) => f.to_string(),
                    _ => "NULL".to_string(),
                };
                println!("  {id}\t{v}");
            }
            q.reset();
        }
    });
}
