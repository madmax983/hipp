//! v0.16: a file's triggers are never skipped silently. The pin
//! (SQLite 3.54.1) writes a file whose table has a trigger; a hipp
//! build without the `triggers` feature reads it, but refuses every
//! write to that table (`SchemaKind::TableHasTriggers`) while other
//! tables stay writable, and DROP TABLE / DROP VIEW still remove the
//! triggers on them, as the pin does. With `triggers` the same
//! writes fire the pin's trigger. Every test here compiles in every
//! profile; the default run also builds and runs them without the
//! feature — in the tiny profile (`std` only: INSERT) and in `full`
//! (`std,full`: UPDATE, DELETE, DROP).

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// A file the pin made: `t` with a trigger logging into `log`, and
/// an untriggered `u`.
fn pin_file(tag: &str) -> std::path::PathBuf {
    let p = common::temp_db(&format!("v16trgoff-{tag}"));
    common::cli(
        &p,
        "CREATE TABLE t(a); CREATE TABLE log(x); CREATE TABLE u(b); \
         INSERT INTO t VALUES (1); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.a); END;",
    );
    p
}

/// Run one statement on `p`: Ok(rows as text) or Err(kind text).
fn run(p: &std::path::Path, sql: &str) -> Result<Vec<String>, String> {
    block_on(async {
        let mut db = Connection::open(FileStorage::open(p).expect("fs"), Config::default())
            .await
            .expect("open");
        let mut q = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
        let mut rows = Vec::new();
        while let Step::Row = q.step().await.map_err(|e| format!("{e:?}"))? {
            let mut l = String::new();
            for i in 0..q.column_count() {
                if let Some(hipp::Value::Integer(v)) = q.column_value(i) {
                    l.push_str(&v.to_string());
                }
            }
            rows.push(l);
        }
        Ok(rows)
    })
}

#[test]
fn insert_into_a_triggered_table() {
    let p = pin_file("insert");
    let r = run(&p, "INSERT INTO t VALUES (2)");
    if cfg!(feature = "triggers") {
        assert_eq!(r, Ok(vec![]));
        assert_eq!(common::cli(&p, "SELECT x FROM log;"), "2\n");
    } else {
        assert!(
            r.as_ref().is_err_and(|e| e.contains("TableHasTriggers")),
            "{r:?}"
        );
        assert_eq!(common::cli(&p, "SELECT count(*) FROM t;"), "1\n");
        assert_eq!(common::cli(&p, "SELECT count(*) FROM log;"), "0\n");
    }
    // Reading and writing the other tables work in every build.
    assert_eq!(run(&p, "INSERT INTO u VALUES (5)"), Ok(vec![]));
    assert_eq!(
        run(&p, "SELECT * FROM t").map(|r| r.len()),
        Ok(if cfg!(feature = "triggers") { 2 } else { 1 })
    );
    assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
}

#[cfg(feature = "v05")]
#[test]
fn update_delete_and_drop() {
    let p = pin_file("write");
    let up = run(&p, "UPDATE t SET a = 3");
    let del = run(&p, "DELETE FROM t");
    if cfg!(feature = "triggers") {
        assert_eq!(up, Ok(vec![]));
        assert_eq!(del, Ok(vec![]));
    } else {
        assert!(up.is_err_and(|e| e.contains("TableHasTriggers")));
        assert!(del.is_err_and(|e| e.contains("TableHasTriggers")));
        assert_eq!(common::cli(&p, "SELECT a FROM t;"), "1\n");
    }
    // DROP TABLE takes the table's triggers with it (every build).
    common::cli(
        &p,
        "CREATE VIEW v AS SELECT * FROM u; \
         CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN SELECT 1; END;",
    );
    assert_eq!(run(&p, "DROP TABLE t"), Ok(vec![]));
    assert_eq!(run(&p, "DROP VIEW v"), Ok(vec![]));
    assert_eq!(
        common::cli(&p, "SELECT name FROM sqlite_schema ORDER BY name;"),
        "log\nu\n"
    );
    assert_eq!(common::cli(&p, "PRAGMA integrity_check;"), "ok\n");
}

/// The default run builds these tests without `triggers` too: in the
/// tiny profile and in `full`.
#[cfg(feature = "triggers")]
#[test]
fn builds_without_triggers_refuse() {
    for features in ["std", "std,full"] {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let target = concat!(env!("CARGO_TARGET_TMPDIR"), "/tiny-depth");
        let out = std::process::Command::new(cargo)
            .args([
                "test",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                "-p",
                "hipp",
                "--no-default-features",
                "--features",
                features,
                "--test",
                "triggers_off_v16",
            ])
            .env("CARGO_TARGET_DIR", target)
            .output()
            .expect("cargo runs");
        assert!(
            out.status.success(),
            "{features}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
