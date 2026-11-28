//! A database file on disk with `FileStorage` (feature `std`).
//!
//! `FileStorage::open(path)` opens (or creates) `path`; the rollback
//! journal is `path-journal` beside it, as SQLite names it. The file
//! is an ordinary SQLite 3 database: `sqlite3 path` opens it.
//!
//! ```text
//! cargo run --example file_db -- /tmp/demo.db
//! ```
//!
//! With no argument it uses a file in the system temp directory.

mod common;

use common::{check, exec, exec_all, section};
use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage};

fn main() {
    let path = std::env::args().nth(1).map_or_else(
        || std::env::temp_dir().join(format!("hipp-file-db-{}.db", std::process::id())),
        std::path::PathBuf::from,
    );
    // Start from a fresh file so the example is repeatable.
    let _ = std::fs::remove_file(&path);

    block_on(async {
        section("Create and fill");
        let storage = FileStorage::open(&path).expect("open file");
        let mut db = Connection::open(storage, Config::default())
            .await
            .expect("open database");
        exec_all(
            &mut db,
            &[
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, created TEXT DEFAULT CURRENT_TIMESTAMP)",
                "BEGIN",
                "INSERT INTO notes (body) VALUES ('first'), ('second')",
                "COMMIT",
            ],
        )
        .await;
        // `close` rolls back anything uncommitted (and in WAL mode
        // checkpoints). Dropping a connection is safe too: what was
        // committed is on disk.
        db.close().await.expect("close");
        println!("wrote {}", path.display());

        section("Reopen");
        let mut db = Connection::open(FileStorage::open(&path).expect("open"), Config::default())
            .await
            .expect("reopen");
        check(
            &mut db,
            "SELECT id, body FROM notes",
            &["1|first", "2|second"],
        )
        .await;
        check(
            &mut db,
            "SELECT count(*) FROM notes WHERE created IS NOT NULL",
            &["2"],
        )
        .await;
        check(&mut db, "PRAGMA integrity_check", &["ok"]).await;
        // VACUUM INTO writes a compacted copy (refusing an existing file).
        let copy = path.with_extension("copy.db");
        let _ = std::fs::remove_file(&copy);
        exec(&mut db, &format!("VACUUM INTO '{}'", copy.display())).await;
        db.close().await.expect("close");
        let header = std::fs::read(&copy).expect("read copy");
        assert_eq!(&header[..16], b"SQLite format 3\0");
        println!(
            "VACUUM INTO wrote {} ({} bytes)",
            copy.display(),
            header.len()
        );
        let _ = std::fs::remove_file(&copy);
    });
    if std::env::args().nth(1).is_none() {
        let _ = std::fs::remove_file(&path);
    }
    println!("\nfile_db: ok");
}
