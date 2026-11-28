//! Everything the **xl** profile adds over max
//! (`--no-default-features --features xl`, ≤ 1.25 MiB of code on
//! thumbv7em; the default features are `std` + `xl`): the leaves past
//! `max`'s budget (ADR 0004).
//!
//! - `planner`: SQLite's query planner past single-table access
//!   paths: join ordering by cost, automatic indexes, Bloom filters,
//!   `sqlite_stat1` estimates, MULTI-INDEX OR, the min() / max()
//!   lookup, skip-scans, the subquery flattener, WHERE push-down and
//!   EXISTS-to-join.
//! - `fts5_delete`: FTS5 deletes on `contentless_delete=1` tables
//!   (tombstones) and `secure-delete`.
//!
//! The planner changes how fast a query runs, never what it returns:
//! a `max` build answers every query here identically, in FROM order.
//! So this example shows each strategy through EXPLAIN QUERY PLAN.
//!
//! ```text
//! cargo run --example xl --no-default-features --features xl
//! ```
//!
//! `docs/profiles/xl.md` walks through it section by section.

mod common;

use common::{check, exec, exec_all, plan, section};
use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage};

type Db = Connection<MemStorage>;

fn main() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        setup(&mut db).await;
        join_order(&mut db).await;
        automatic_index(&mut db).await;
        or_and_minmax(&mut db).await;
        statistics(&mut db).await;
        flattening(&mut db).await;
        fts5_delete(&mut db).await;
    });
    println!("\nxl: ok");
}

async fn setup(db: &mut Db) {
    section("A small star schema");
    exec_all(
        db,
        &[
            "CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT, country TEXT)",
            "CREATE TABLE product (id INTEGER PRIMARY KEY, name TEXT, price INTEGER)",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer INTEGER, product INTEGER, qty INTEGER, day TEXT)",
            "CREATE INDEX orders_customer ON orders(customer)",
            "CREATE INDEX orders_day ON orders(day)",
            "CREATE INDEX customer_country ON customer(country)",
            "INSERT INTO customer (name, country)
                SELECT 'c' || value, CASE value % 4 WHEN 0 THEN 'no' WHEN 1 THEN 'se' WHEN 2 THEN 'dk' ELSE 'fi' END
                FROM generate_series(1, 200)",
            "INSERT INTO product (name, price) SELECT 'p' || value, value * 10 FROM generate_series(1, 50)",
            "INSERT INTO orders (customer, product, qty, day)
                SELECT 1 + value % 200, 1 + value % 50, 1 + value % 5, '2026-01-' || printf('%02d', 1 + value % 28)
                FROM generate_series(1, 2000)",
        ],
    )
    .await;
    check(db, "SELECT count(*) FROM orders", &["2000"]).await;
}

async fn join_order(db: &mut Db) {
    section("Join ordering (feature `planner`)");
    // FROM order is product, orders, customer, but the selective
    // filter is on customer.country: the planner starts there and
    // reaches orders through its index on `customer`.
    plan(
        db,
        "SELECT p.name, sum(o.qty) FROM product p, orders o, customer c
         WHERE o.product = p.id AND o.customer = c.id AND c.country = 'no'
         GROUP BY p.name",
        &[
            "SEARCH c USING COVERING INDEX customer_country (country=?)",
            "SEARCH o USING INDEX orders_customer (customer=?)",
            "SEARCH p USING INTEGER PRIMARY KEY (rowid=?)",
            "USE TEMP B-TREE FOR GROUP BY",
        ],
    )
    .await;
    check(
        db,
        "SELECT count(*), sum(o.qty) FROM product p, orders o, customer c
         WHERE o.product = p.id AND o.customer = c.id AND c.country = 'no'",
        &["500|1500"],
    )
    .await;
    // Here the planner reverses FROM order: scanning orders once and
    // looking each product up by its rowid beats probing orders
    // (which has no index on `product`) for every product.
    plan(
        db,
        "SELECT p.name, o.qty FROM product p JOIN orders o ON o.product = p.id WHERE p.price > 480",
        &["SCAN o", "SEARCH p USING INTEGER PRIMARY KEY (rowid=?)"],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM product p JOIN orders o ON o.product = p.id WHERE p.price > 480",
        &["80"],
    )
    .await;
}

async fn automatic_index(db: &mut Db) {
    section("Automatic indexes and Bloom filters");
    // CROSS JOIN pins the order (product first), as on SQLite. With
    // no index on orders.product, the planner builds a temporary
    // covering index for the statement, plus a Bloom filter that
    // skips probes that cannot match.
    plan(
        db,
        "SELECT count(*) FROM product p CROSS JOIN orders o WHERE o.product = p.id",
        &[
            "SCAN p",
            "BLOOM FILTER ON o (product=?)",
            "SEARCH o USING AUTOMATIC COVERING INDEX (product=?)",
        ],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM product p CROSS JOIN orders o WHERE o.product = p.id",
        &["2000"],
    )
    .await;
}

async fn or_and_minmax(db: &mut Db) {
    section("MULTI-INDEX OR, the min() / max() lookup");
    plan(
        db,
        "SELECT id FROM orders WHERE customer = 7 OR day = '2026-01-05'",
        &[
            "MULTI-INDEX OR",
            "INDEX 1",
            "SEARCH orders USING INDEX orders_customer (customer=?)",
            "INDEX 2",
            "SEARCH orders USING INDEX orders_day (day=?)",
        ],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM orders WHERE customer = 7 OR day = '2026-01-05'",
        &["82"],
    )
    .await;
    // A lone max(x) reads the last entry of x's index.
    plan(
        db,
        "SELECT max(day) FROM orders",
        &["SEARCH orders USING COVERING INDEX orders_day"],
    )
    .await;
    check(
        db,
        "SELECT min(day), max(day) FROM orders",
        &["2026-01-01|2026-01-28"],
    )
    .await;
}

async fn statistics(db: &mut Db) {
    section("ANALYZE statistics and skip-scans");
    // A two-column index whose first column has few values: after
    // ANALYZE, sqlite_stat1 tells the planner a skip-scan pays off.
    exec_all(
        db,
        &[
            "CREATE TABLE reading (sensor INTEGER, at INTEGER, v REAL)",
            "CREATE INDEX reading_sensor_at ON reading(sensor, at)",
            "INSERT INTO reading SELECT value % 3, value, value * 0.5 FROM generate_series(1, 3000)",
            "ANALYZE",
        ],
    )
    .await;
    check(
        db,
        "SELECT stat FROM sqlite_stat1 WHERE idx = 'reading_sensor_at'",
        &["3000 1000 1"],
    )
    .await;
    plan(
        db,
        "SELECT v FROM reading WHERE at = 1500",
        &["SEARCH reading USING INDEX reading_sensor_at (ANY(sensor) AND at=?)"],
    )
    .await;
    check(db, "SELECT v FROM reading WHERE at = 1500", &["750.0"]).await;
}

async fn flattening(db: &mut Db) {
    section("Subquery flattening, WHERE push-down, EXISTS-to-join");
    // A FROM subquery (or view, or CTE) is merged into its reader, so
    // the outer WHERE reaches the base table's index.
    exec(
        db,
        "CREATE VIEW big_orders AS SELECT * FROM orders WHERE qty >= 4",
    )
    .await;
    plan(
        db,
        "SELECT id FROM big_orders WHERE customer = 9",
        &["SEARCH orders USING INDEX orders_customer (customer=?)"],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM big_orders WHERE customer = 9",
        &["10"],
    )
    .await;
    // An aggregate subquery cannot be merged; the WHERE term is
    // pushed down into it instead, before its GROUP BY.
    plan(
        db,
        "SELECT * FROM (SELECT customer, sum(qty) AS n FROM orders GROUP BY customer) WHERE customer = 9",
        &[
            "CO-ROUTINE (subquery-1)",
            "SEARCH orders USING INDEX orders_customer (customer=?)",
            "SCAN (subquery-1)",
        ],
    )
    .await;
    check(
        db,
        "SELECT * FROM (SELECT customer, sum(qty) AS n FROM orders GROUP BY customer) WHERE customer = 9",
        &["9|40"],
    )
    .await;
    // A correlated EXISTS runs as a semi-join loop: the inner search
    // stops at its first match.
    plan(
        db,
        "SELECT name FROM customer c WHERE country = 'se' AND EXISTS (SELECT 1 FROM orders o WHERE o.customer = c.id AND o.qty = 5)",
        &[
            "SEARCH c USING INDEX customer_country (country=?)",
            "SEARCH o EXISTS USING INDEX orders_customer (customer=?)",
        ],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM customer c WHERE country = 'se' AND EXISTS (SELECT 1 FROM orders o WHERE o.customer = c.id AND o.qty = 5)",
        &["10"],
    )
    .await;
}

async fn fts5_delete(db: &mut Db) {
    section("FTS5 contentless-delete and secure-delete (feature `fts5_delete`)");
    // A contentless table stores only the index, not the text, and
    // with contentless_delete=1 its rows can still be deleted.
    exec_all(
        db,
        &[
            "CREATE VIRTUAL TABLE notes USING fts5(body, content = '', contentless_delete = 1)",
            "INSERT INTO notes (rowid, body) VALUES (1, 'buy milk'), (2, 'call mum'), (3, 'buy stamps')",
            "DELETE FROM notes WHERE rowid = 1",
        ],
    )
    .await;
    check(
        db,
        "SELECT rowid FROM notes WHERE notes MATCH 'buy'",
        &["3"],
    )
    .await;
    // secure-delete removes a deleted row's entries from the index
    // itself, so nothing of it is left in the file.
    exec_all(
        db,
        &[
            "CREATE VIRTUAL TABLE secrets USING fts5(body)",
            "INSERT INTO secrets(secrets, rank) VALUES ('secure-delete', 1)",
            "INSERT INTO secrets (rowid, body) VALUES (1, 'password hunter2'), (2, 'nothing here')",
            "DELETE FROM secrets WHERE rowid = 1",
        ],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM secrets WHERE secrets MATCH 'hunter2'",
        &["0"],
    )
    .await;
    exec(
        db,
        "INSERT INTO secrets(secrets) VALUES ('integrity-check')",
    )
    .await;
    check(db, "PRAGMA integrity_check", &["ok"]).await;
}
