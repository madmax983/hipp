//! Everything the **max** profile adds over full
//! (`--no-default-features --features max`, ≤ 1 MiB of code on
//! thumbv7em): the heavyweight leaves. `max` is `full` + `window`,
//! `triggers`, `foreign_keys`, `json`, `expr_index`, `without_rowid`,
//! `utf16`, `wal`, `datetime`, `attach`, `autovacuum`, `vtab`,
//! `rtree`, `fts5` and `explain`.
//!
//! ```text
//! cargo run --example max --no-default-features --features max
//! ```
//!
//! `docs/profiles/max.md` walks through it section by section.

mod common;

use common::{check, exec, exec_all, fails, plan, rows, section};
use hipp::storage::{AttachTarget, block_on};
use hipp::{Config, Connection, MemStorage};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// 2026-01-02 03:04:05.678 UTC, in Unix milliseconds.
fn fixed_clock() -> i64 {
    1_767_323_045_678
}

/// A fixed time zone: UTC+02:00 at every instant.
fn plus_two(_unix_seconds: i64) -> Option<i64> {
    Some(2 * 3600)
}

type Db = Connection<MemStorage>;

fn main() {
    block_on(async {
        let cfg = Config::default()
            .clock(fixed_clock)
            .tz_offset(Some(plus_two));
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        exec_all(
            &mut db,
            &[
                "CREATE TABLE sales (id INTEGER PRIMARY KEY, region TEXT, month INTEGER, amount INTEGER)",
                "INSERT INTO sales (region, month, amount) VALUES
                    ('north', 1, 100), ('north', 2, 150), ('north', 3, 120),
                    ('south', 1, 80), ('south', 2, 80), ('south', 3, 200)",
            ],
        )
        .await;

        window(&mut db).await;
        triggers(&mut db).await;
        foreign_keys(&mut db).await;
        json(&mut db).await;
        expr_index(&mut db).await;
        without_rowid(&mut db).await;
        datetime(&mut db).await;
        vtab(&mut db).await;
        rtree(&mut db).await;
        fts5(&mut db).await;
        explain(&mut db).await;
        attach(&mut db).await;
        db.close().await.expect("close");

        wal().await;
        utf16().await;
        autovacuum().await;
    });
    println!("\nmax: ok");
}

async fn window(db: &mut Db) {
    section("Window functions (feature `window`)");
    check(
        db,
        "SELECT region, month, amount,
                sum(amount) OVER (PARTITION BY region ORDER BY month) AS running,
                rank() OVER (PARTITION BY region ORDER BY amount DESC) AS rnk
         FROM sales ORDER BY region, month",
        &[
            "north|1|100|100|3",
            "north|2|150|250|1",
            "north|3|120|370|2",
            "south|1|80|80|2",
            "south|2|80|160|2",
            "south|3|200|360|1",
        ],
    )
    .await;
    check(
        db,
        "SELECT month, amount, lag(amount) OVER w, lead(amount, 1, 0) OVER w, row_number() OVER w
         FROM sales WHERE region = 'south' WINDOW w AS (ORDER BY month)",
        &["1|80||80|1", "2|80|80|200|2", "3|200|80|0|3"],
    )
    .await;
    check(
        db,
        "SELECT id, round(avg(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), 2),
                dense_rank() OVER (ORDER BY amount), ntile(2) OVER (ORDER BY id),
                first_value(amount) OVER (ORDER BY id GROUPS UNBOUNDED PRECEDING)
         FROM sales WHERE region = 'north'",
        &["1|125.0|1|1|100", "2|123.33|3|1|100", "3|135.0|2|2|100"],
    )
    .await;
}

async fn triggers(db: &mut Db) {
    section("Triggers (feature `triggers`)");
    exec_all(
        db,
        &[
            "CREATE TABLE audit (at INTEGER, what TEXT)",
            "CREATE TRIGGER sales_ins AFTER INSERT ON sales
             BEGIN INSERT INTO audit VALUES (NEW.id, 'insert ' || NEW.region); END",
            "CREATE TRIGGER sales_upd AFTER UPDATE OF amount ON sales WHEN NEW.amount > OLD.amount
             BEGIN INSERT INTO audit VALUES (NEW.id, 'raised ' || OLD.amount || '->' || NEW.amount); END",
            "CREATE TRIGGER no_neg BEFORE INSERT ON sales WHEN NEW.amount < 0
             BEGIN SELECT RAISE(ABORT, 'negative amount'); END",
            "INSERT INTO sales (region, month, amount) VALUES ('east', 1, 10)",
            "UPDATE sales SET amount = amount + 5 WHERE id IN (1, 7)",
        ],
    )
    .await;
    check(
        db,
        "SELECT * FROM audit",
        &["7|insert east", "1|raised 100->105", "7|raised 10->15"],
    )
    .await;
    fails(
        db,
        "INSERT INTO sales (region, month, amount) VALUES ('east', 2, -1)",
    )
    .await;
    // INSTEAD OF triggers make a view writable.
    exec_all(
        db,
        &[
            "CREATE VIEW totals AS SELECT region, sum(amount) AS total FROM sales GROUP BY region",
            "CREATE TRIGGER totals_ins INSTEAD OF INSERT ON totals
             BEGIN INSERT INTO sales (region, month, amount) VALUES (NEW.region, 0, NEW.total); END",
            "INSERT INTO totals VALUES ('west', 42)",
        ],
    )
    .await;
    check(
        db,
        "SELECT total FROM totals WHERE region = 'west'",
        &["42"],
    )
    .await;
    exec_all(db, &["DROP TRIGGER sales_ins", "DROP TRIGGER sales_upd"]).await;
}

async fn foreign_keys(db: &mut Db) {
    section("Foreign keys (feature `foreign_keys`, PRAGMA foreign_keys = ON)");
    exec_all(
        db,
        &[
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE author (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT,
                author INTEGER REFERENCES author(id) ON DELETE CASCADE ON UPDATE CASCADE)",
            "INSERT INTO author VALUES (1, 'le guin'), (2, 'banks')",
            "INSERT INTO book VALUES (10, 'the dispossessed', 1), (11, 'excession', 2), (12, 'use of weapons', 2)",
        ],
    )
    .await;
    fails(db, "INSERT INTO book VALUES (13, 'orphan', 99)").await;
    exec(db, "UPDATE author SET id = 20 WHERE id = 2").await;
    check(
        db,
        "SELECT id, author FROM book ORDER BY id",
        &["10|1", "11|20", "12|20"],
    )
    .await;
    exec(db, "DELETE FROM author WHERE id = 20").await;
    check(db, "SELECT title FROM book", &["the dispossessed"]).await;
    check(db, "PRAGMA foreign_key_check", &[]).await;
}

async fn json(db: &mut Db) {
    section("JSON and JSONB (feature `json`)");
    exec_all(
        db,
        &[
            "CREATE TABLE events (id INTEGER PRIMARY KEY, body TEXT)",
            r#"INSERT INTO events (body) VALUES
                ('{"kind":"click","pos":{"x":10,"y":20},"tags":["a","b"]}'),
                ('{"kind":"key","code":65,"tags":[]}')"#,
        ],
    )
    .await;
    check(
        db,
        "SELECT body ->> '$.kind', body -> '$.pos', json_extract(body, '$.pos.x'), json_array_length(body, '$.tags') FROM events",
        &[r#"click|{"x":10,"y":20}|10|2"#, "key|||0"],
    )
    .await;
    check(
        db,
        "SELECT json_object('a', 1, 'b', json_array(1, 2, 'x')), json('{ \"z\" : [1,2] }')",
        &[r#"{"a":1,"b":[1,2,"x"]}|{"z":[1,2]}"#],
    )
    .await;
    check(db, r#"SELECT json_set('{"a":1}', '$.b', 2), json_remove('[1,2,3]', '$[1]'), json_patch('{"a":1,"b":2}', '{"b":null,"c":3}')"#, &[
        r#"{"a":1,"b":2}|[1,3]|{"a":1,"c":3}"#,
    ])
    .await;
    check(db, "SELECT json_type(body, '$.code'), json_valid(body), json_valid('{bad') FROM events WHERE id = 2", &[
        "integer|1|0",
    ])
    .await;
    check(
        db,
        "SELECT key, value FROM events, json_each(events.body, '$.tags') WHERE events.id = 1",
        &["0|a", "1|b"],
    )
    .await;
    check(
        db,
        "SELECT json_group_array(region) FROM (SELECT DISTINCT region FROM sales ORDER BY 1)",
        &[r#"["east","north","south","west"]"#],
    )
    .await;
    // JSONB: the binary format, byte-identical to SQLite's.
    check(
        db,
        "SELECT hex(jsonb('[1,\"a\"]')), json(jsonb('[1,\"a\"]'))",
        &["4B13311761|[1,\"a\"]"],
    )
    .await;
}

async fn expr_index(db: &mut Db) {
    section("Expression and partial indexes, index-aware planning (feature `expr_index`)");
    exec_all(
        db,
        &[
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, active INTEGER)",
            "INSERT INTO users (email, active) VALUES ('Ann@X.io', 1), ('bob@y.io', 0), ('CAT@z.io', 1)",
            "CREATE UNIQUE INDEX users_email_lower ON users(lower(email))",
            "CREATE INDEX users_active ON users(email) WHERE active = 1",
        ],
    )
    .await;
    check(
        db,
        "SELECT id FROM users WHERE lower(email) = 'cat@z.io'",
        &["3"],
    )
    .await;
    // The UNIQUE expression index is enforced.
    fails(
        db,
        "INSERT INTO users (email, active) VALUES ('ann@x.io', 0)",
    )
    .await;
    plan(
        db,
        "SELECT email FROM users WHERE lower(email) = 'cat@z.io'",
        &["SEARCH users USING INDEX users_email_lower (<expr>=?)"],
    )
    .await;
    check(
        db,
        "PRAGMA index_xinfo(users_email_lower)",
        &["0|-2||0|BINARY|1", "1|-1||0|BINARY|0"],
    )
    .await;
}

async fn without_rowid(db: &mut Db) {
    section("WITHOUT ROWID tables (feature `without_rowid`)");
    exec_all(
        db,
        &[
            "CREATE TABLE settings (scope TEXT, key TEXT, value, PRIMARY KEY (scope, key)) WITHOUT ROWID",
            "INSERT INTO settings VALUES ('ui', 'theme', 'dark'), ('net', 'port', 8080), ('ui', 'font', 'mono')",
        ],
    )
    .await;
    // Rows are stored (and scanned) in PRIMARY KEY order.
    check(
        db,
        "SELECT scope, key, value FROM settings",
        &["net|port|8080", "ui|font|mono", "ui|theme|dark"],
    )
    .await;
    exec(
        db,
        "UPDATE settings SET value = 'light' WHERE scope = 'ui' AND key = 'theme'",
    )
    .await;
    check(
        db,
        "SELECT value FROM settings WHERE scope = 'ui' AND key = 'theme'",
        &["light"],
    )
    .await;
    fails(db, "SELECT rowid FROM settings").await;
}

async fn datetime(db: &mut Db) {
    section("Date and time functions (feature `datetime`)");
    check(
        db,
        "SELECT date('2026-03-15'), time('2026-03-15 13:45:10'), datetime(0, 'unixepoch')",
        &["2026-03-15|13:45:10|1970-01-01 00:00:00"],
    )
    .await;
    check(db, "SELECT date('2026-01-31', '+1 month'), date('2026-03-15', 'start of month', '-1 day'), date('2026-03-15', 'weekday 0')", &[
        "2026-03-03|2026-02-28|2026-03-15",
    ])
    .await;
    check(db, "SELECT julianday('2000-01-01 12:00:00'), unixepoch('2026-01-01'), strftime('%Y week %W day %j', '2026-03-15')", &[
        "2451545.0|1767225600|2026 week 10 day 074",
    ])
    .await;
    check(
        db,
        "SELECT timediff('2026-03-15', '2026-01-01')",
        &["+0000-02-14 00:00:00.000"],
    )
    .await;
    // 'now' and CURRENT_* read `Config::clock`; 'localtime' reads
    // `Config::tz_offset` (here a fixed UTC+2).
    check(db, "SELECT datetime('now'), datetime('now', 'localtime'), CURRENT_DATE, strftime('%H:%M:%f', 'now')", &[
        "2026-01-02 03:04:05|2026-01-02 05:04:05|2026-01-02|03:04:05.678",
    ])
    .await;
}

async fn vtab(db: &mut Db) {
    section("Virtual tables: generate_series, dbstat (feature `vtab`)");
    check(
        db,
        "SELECT group_concat(value, ',') FROM generate_series(1, 10, 3)",
        &["1,4,7,10"],
    )
    .await;
    check(
        db,
        "SELECT value * value FROM generate_series(1, 5) WHERE value % 2 = 1",
        &["1", "9", "25"],
    )
    .await;
    let pages = rows(db, "SELECT count(*) FROM dbstat WHERE name = 'sales'").await;
    println!("dbstat: sales uses {} page(s)", pages[0]);
    assert_eq!(pages, ["1"]);
}

async fn rtree(db: &mut Db) {
    section("R*Tree spatial index (feature `rtree`)");
    exec_all(
        db,
        &[
            "CREATE VIRTUAL TABLE zones USING rtree(id, min_x, max_x, min_y, max_y, +label)",
            "INSERT INTO zones VALUES (1, 0, 10, 0, 10, 'home'), (2, 5, 15, 5, 15, 'park'), (3, 20, 30, 20, 30, 'lake')",
        ],
    )
    .await;
    // Which zones contain the point (7, 7)?
    check(
        db,
        "SELECT id, label FROM zones WHERE min_x <= 7 AND max_x >= 7 AND min_y <= 7 AND max_y >= 7",
        &["1|home", "2|park"],
    )
    .await;
    check(db, "SELECT rtreecheck('zones')", &["ok"]).await;
}

async fn fts5(db: &mut Db) {
    section("FTS5 full-text search (feature `fts5`)");
    exec_all(
        db,
        &[
            "CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize = 'porter')",
            "INSERT INTO docs VALUES
                ('Rust embedded', 'Running databases on microcontrollers with no_std'),
                ('SQLite files', 'Reading and writing the SQLite file format'),
                ('Gardening', 'Growing tomatoes in small spaces')",
        ],
    )
    .await;
    // The porter stemmer matches "run" to "Running".
    check(
        db,
        "SELECT title FROM docs WHERE docs MATCH 'run'",
        &["Rust embedded"],
    )
    .await;
    check(
        db,
        "SELECT title FROM docs WHERE docs MATCH 'sqlite OR tomato*' ORDER BY rank",
        &["SQLite files", "Gardening"],
    )
    .await;
    check(
        db,
        "SELECT highlight(docs, 1, '[', ']') FROM docs WHERE docs MATCH 'sqlite'",
        &["Reading and writing the [SQLite] file format"],
    )
    .await;
    check(
        db,
        "SELECT title FROM docs WHERE docs MATCH 'NEAR(file format, 2)'",
        &["SQLite files"],
    )
    .await;
    exec(db, "INSERT INTO docs(docs) VALUES ('integrity-check')").await;
}

async fn explain(db: &mut Db) {
    section("EXPLAIN QUERY PLAN (feature `explain`)");
    exec(db, "CREATE INDEX sales_region ON sales(region)").await;
    plan(
        db,
        "SELECT * FROM sales WHERE region = 'north' ORDER BY month",
        &[
            "SEARCH sales USING INDEX sales_region (region=?)",
            "USE TEMP B-TREE FOR ORDER BY",
        ],
    )
    .await;
    plan(
        db,
        "SELECT * FROM sales WHERE id = 3",
        &["SEARCH sales USING INTEGER PRIMARY KEY (rowid=?)"],
    )
    .await;
    plan(
        db,
        "SELECT region, count(*) FROM sales GROUP BY region",
        &["SCAN sales USING COVERING INDEX sales_region"],
    )
    .await;
    // The raw rows hold four columns: id, parent, notused, detail.
    let raw = rows(db, "EXPLAIN QUERY PLAN SELECT * FROM sales").await;
    assert_eq!(raw.len(), 1);
    assert!(raw[0].ends_with("|SCAN sales"));
}

async fn attach(db: &mut Db) {
    section("ATTACH / DETACH (feature `attach`)");
    // Without std there is no file system: the attach hook maps the
    // names ATTACH gives to storage. Here, shared in-memory "files".
    let files: Rc<RefCell<HashMap<Vec<u8>, MemStorage>>> = Rc::default();
    let hook_files = files.clone();
    db.set_attach_hook(move |target| match target {
        AttachTarget::File(name) => Some(
            hook_files
                .borrow_mut()
                .entry(name.to_vec())
                .or_default()
                .clone(),
        ),
        AttachTarget::Memory => Some(MemStorage::memory_db()),
        AttachTarget::Temporary => Some(MemStorage::new()),
    });
    exec_all(
        db,
        &[
            "ATTACH DATABASE 'archive.db' AS archive",
            "CREATE TABLE archive.old_sales AS SELECT * FROM sales WHERE month = 1",
        ],
    )
    .await;
    check(
        db,
        "SELECT name FROM pragma_database_list ORDER BY seq",
        &["main", "archive"],
    )
    .await;
    check(
        db,
        "SELECT s.region, s.amount, o.amount FROM sales s JOIN archive.old_sales o USING (id) ORDER BY s.id",
        &["north|105|105", "south|80|80", "east|15|15"],
    )
    .await;
    // One transaction across both files: committed atomically
    // through a super-journal.
    exec_all(
        db,
        &[
            "BEGIN",
            "DELETE FROM archive.old_sales WHERE region = 'east'",
            "UPDATE sales SET amount = 0 WHERE region = 'east'",
            "COMMIT",
            "DETACH archive",
        ],
    )
    .await;
    // The attached database is a SQLite file of its own.
    let image = files.borrow()[&b"archive.db"[..]].snapshot();
    let mut other = Connection::open(MemStorage::from_bytes(image), Config::default())
        .await
        .expect("open archive");
    check(
        &mut other,
        "SELECT region FROM old_sales",
        &["north", "south"],
    )
    .await;
}

async fn wal() {
    section("Write-ahead logging (feature `wal`)");
    let medium = MemStorage::new();
    let mut db = Connection::open(medium.clone(), Config::default())
        .await
        .expect("open");
    check(&mut db, "PRAGMA journal_mode = WAL", &["wal"]).await;
    exec_all(
        &mut db,
        &[
            "CREATE TABLE log (msg TEXT)",
            "INSERT INTO log VALUES ('one'), ('two')",
        ],
    )
    .await;
    // Commits append frames to the -wal file; the main file is not
    // rewritten until a checkpoint.
    let wal_bytes = medium.wal_snapshot().map_or(0, |w| w.len());
    println!("-wal holds {wal_bytes} bytes before the checkpoint");
    assert!(wal_bytes > 0);
    check(&mut db, "PRAGMA wal_checkpoint(TRUNCATE)", &["0|0|0"]).await;
    exec(&mut db, "INSERT INTO log VALUES ('three')").await;
    // Close checkpoints and removes the log, as SQLite's close does.
    db.close().await.expect("close");
    assert!(medium.wal_snapshot().is_none());
    let mut db = Connection::open(medium, Config::default())
        .await
        .expect("reopen");
    check(&mut db, "SELECT msg FROM log", &["one", "two", "three"]).await;
    check(&mut db, "PRAGMA journal_mode", &["wal"]).await;
}

async fn utf16() {
    section("UTF-16 databases (feature `utf16`)");
    let medium = MemStorage::new();
    let mut db = Connection::open(medium.clone(), Config::default())
        .await
        .expect("open");
    exec_all(
        &mut db,
        &[
            "PRAGMA encoding = 'UTF-16le'",
            "CREATE TABLE words (w TEXT)",
            "INSERT INTO words VALUES ('héllo'), ('wörld')",
        ],
    )
    .await;
    check(&mut db, "PRAGMA encoding", &["UTF-16le"]).await;
    // Text is UTF-8 at the API; the file stores UTF-16.
    check(
        &mut db,
        "SELECT w, length(w), hex(w) FROM words WHERE w = 'héllo'",
        &["héllo|5|6800E9006C006C006F00"],
    )
    .await;
    db.close().await.expect("close");
    let header = medium.snapshot();
    // Header bytes 56..60: text encoding, 2 = UTF-16le.
    assert_eq!(&header[56..60], &[0, 0, 0, 2]);
}

async fn autovacuum() {
    section("Auto-vacuum (feature `autovacuum`)");
    let mut db = Connection::open(MemStorage::new(), Config::default())
        .await
        .expect("open");
    exec_all(
        &mut db,
        &[
            "PRAGMA auto_vacuum = FULL",
            "CREATE TABLE blobs (b BLOB)",
            "INSERT INTO blobs SELECT randomblob(3000) FROM generate_series(1, 20)",
        ],
    )
    .await;
    let full = db.arena_stats().page_count;
    exec(&mut db, "DELETE FROM blobs").await;
    let after = db.arena_stats().page_count;
    println!("auto_vacuum = FULL: {full} pages -> {after} pages after the DELETE's commit");
    assert!(after < full);
    check(&mut db, "PRAGMA auto_vacuum", &["1"]).await;
    check(&mut db, "PRAGMA integrity_check", &["ok"]).await;
}
