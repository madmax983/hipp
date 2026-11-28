# The xl profile

```toml
hipp = { git = "https://github.com/madmax983/hipp" }   # default features: std + xl
# no_std:
hipp = { git = "https://github.com/madmax983/hipp", default-features = false, features = ["xl"] }
```

**Budget:** 1.25 MiB of code on thumbv7em (ratchet 1,106,664 B). `no_std`.
It is the default profile.
**Example:** [`examples/xl.rs`](../../examples/xl.rs).

```bash
cargo run --example xl --no-default-features --features xl
```

xl is [max](max.md) plus the two leaves that do not fit max's 1 MiB
(ADR 0004):

| feature | adds |
|---|---|
| `planner` | SQLite's query planner past single-table access paths: cost-based join order (`wherePathSolver`), automatic indexes and Bloom filters, `sqlite_stat1` estimates, MULTI-INDEX OR, the min() / max() lookup, skip-scans, the IN operator's statistics rules, the subquery flattener, WHERE push-down, unused-column removal, EXISTS-to-join |
| `fts5_delete` | FTS5 deletes on `contentless_delete=1` tables (tombstone pages) and `secure-delete` |

**The planner changes speed, never results.** Without it (in full and max)
joins run in FROM order, each table read by its own best access path. OR
terms filter a scan, `min()` / `max()` read the whole table, and subqueries
run as co-routines. Every query returns the same rows either way, in the
same order wherever SQL defines one. So this page shows the planner through
`EXPLAIN QUERY PLAN`, and checks the answers alongside. Every plan and
answer below was checked against real SQLite (3.53.1) on the same schema and
data, and matches.

The example builds a small star schema: 200 customers in four countries, 50
products, 2,000 orders, with indexes on `orders(customer)`, `orders(day)` and
`customer(country)`. Data comes from `generate_series`:

```sql
INSERT INTO orders (customer, product, qty, day)
  SELECT 1 + value % 200, 1 + value % 50, 1 + value % 5, '2026-01-' || printf('%02d', 1 + value % 28)
  FROM generate_series(1, 2000);
```

## Join ordering

FROM order is product, orders, customer, but the selective term is on
`customer.country`. The planner starts there and reaches `orders` through
its index on `customer`:

```text
sqlite> EXPLAIN QUERY PLAN SELECT p.name, sum(o.qty) FROM product p, orders o, customer c
         WHERE o.product = p.id AND o.customer = c.id AND c.country = 'no'
         GROUP BY p.name
`--SEARCH c USING COVERING INDEX customer_country (country=?)
`--SEARCH o USING INDEX orders_customer (customer=?)
`--SEARCH p USING INTEGER PRIMARY KEY (rowid=?)
`--USE TEMP B-TREE FOR GROUP BY
sqlite> SELECT count(*), sum(o.qty) FROM product p, orders o, customer c
         WHERE o.product = p.id AND o.customer = c.id AND c.country = 'no'
500|1500
```

Here scanning `orders` once and looking each product up by rowid is cheaper
than probing the unindexed `orders.product` for every product, so the
planner reverses the FROM order:

```text
sqlite> EXPLAIN QUERY PLAN SELECT p.name, o.qty FROM product p JOIN orders o ON o.product = p.id WHERE p.price > 480
`--SCAN o
`--SEARCH p USING INTEGER PRIMARY KEY (rowid=?)
sqlite> SELECT count(*) FROM product p JOIN orders o ON o.product = p.id WHERE p.price > 480
80
```

`CROSS JOIN` and `LEFT JOIN` constrain the order as they do in SQLite, and
LEFT JOINs that cannot produce NULL rows are simplified to inner joins.
Virtual tables and JSON table-valued functions join by their `xBestIndex`
costs.

## Automatic indexes and Bloom filters

`CROSS JOIN` pins `product` first. With no index on `orders.product`, the
planner builds a temporary covering index for the statement, plus a Bloom
filter that rejects probes that cannot match:

```text
sqlite> EXPLAIN QUERY PLAN SELECT count(*) FROM product p CROSS JOIN orders o WHERE o.product = p.id
`--SCAN p
`--BLOOM FILTER ON o (product=?)
`--SEARCH o USING AUTOMATIC COVERING INDEX (product=?)
sqlite> SELECT count(*) FROM product p CROSS JOIN orders o WHERE o.product = p.id
2000
```

When the WHERE constrains that table alone, the automatic index is partial.

## MULTI-INDEX OR and the min() / max() lookup

```text
sqlite> EXPLAIN QUERY PLAN SELECT id FROM orders WHERE customer = 7 OR day = '2026-01-05'
`--MULTI-INDEX OR
`--INDEX 1
`--SEARCH orders USING INDEX orders_customer (customer=?)
`--INDEX 2
`--SEARCH orders USING INDEX orders_day (day=?)
sqlite> SELECT count(*) FROM orders WHERE customer = 7 OR day = '2026-01-05'
82
sqlite> EXPLAIN QUERY PLAN SELECT max(day) FROM orders
`--SEARCH orders USING COVERING INDEX orders_day
sqlite> SELECT min(day), max(day) FROM orders
2026-01-01|2026-01-28
```

Each OR branch is read through its own index, and a row matching both
branches is returned once. A lone `max(x)` reads one entry from the end of
`x`'s index.

## ANALYZE and skip-scans

```sql
CREATE TABLE reading (sensor INTEGER, at INTEGER, v REAL);
CREATE INDEX reading_sensor_at ON reading(sensor, at);
INSERT INTO reading SELECT value % 3, value, value * 0.5 FROM generate_series(1, 3000);
ANALYZE;
```

```text
sqlite> SELECT stat FROM sqlite_stat1 WHERE idx = 'reading_sensor_at'
3000 1000 1
sqlite> EXPLAIN QUERY PLAN SELECT v FROM reading WHERE at = 1500
`--SEARCH reading USING INDEX reading_sensor_at (ANY(sensor) AND at=?)
sqlite> SELECT v FROM reading WHERE at = 1500
750.0
```

`sqlite_stat1` shows that `sensor` has only 3 values, so the planner
skip-scans the index (one seek per sensor) instead of scanning the table.
Statistics are read at schema load and again after `ANALYZE`.

## Flattening, push-down, EXISTS-to-join

A FROM subquery, view or CTE is merged into the query that reads it, so the
outer WHERE reaches the base table's index:

```sql
CREATE VIEW big_orders AS SELECT * FROM orders WHERE qty >= 4;
```

```text
sqlite> EXPLAIN QUERY PLAN SELECT id FROM big_orders WHERE customer = 9
`--SEARCH orders USING INDEX orders_customer (customer=?)
sqlite> SELECT count(*) FROM big_orders WHERE customer = 9
10
```

An aggregate subquery cannot be merged. The outer WHERE term is pushed down
into it, ahead of its GROUP BY:

```text
sqlite> EXPLAIN QUERY PLAN SELECT * FROM (SELECT customer, sum(qty) AS n FROM orders GROUP BY customer) WHERE customer = 9
`--CO-ROUTINE (subquery-1)
`--SEARCH orders USING INDEX orders_customer (customer=?)
`--SCAN (subquery-1)
sqlite> SELECT * FROM (SELECT customer, sum(qty) AS n FROM orders GROUP BY customer) WHERE customer = 9
9|40
```

A correlated EXISTS runs as a semi-join loop that stops at its first match:

```text
sqlite> EXPLAIN QUERY PLAN SELECT name FROM customer c WHERE country = 'se' AND EXISTS (SELECT 1 FROM orders o WHERE o.customer = c.id AND o.qty = 5)
`--SEARCH c USING INDEX customer_country (country=?)
`--SEARCH o EXISTS USING INDEX orders_customer (customer=?)
sqlite> SELECT count(*) FROM customer c WHERE country = 'se' AND EXISTS (SELECT 1 FROM orders o WHERE o.customer = c.id AND o.qty = 5)
10
```

Also part of `planner`: dropping a materialised subquery's result columns
that its reader never uses, and EXPLAIN QUERY PLAN's lines for foreign-key
child scans of a parent DELETE / UPDATE.

## FTS5 deletes (`fts5_delete`)

A contentless FTS5 table stores only the index, not the text. With
`contentless_delete=1`, its rows can still be deleted (as tombstone pages,
in SQLite's format):

```sql
CREATE VIRTUAL TABLE notes USING fts5(body, content = '', contentless_delete = 1);
INSERT INTO notes (rowid, body) VALUES (1, 'buy milk'), (2, 'call mum'), (3, 'buy stamps');
DELETE FROM notes WHERE rowid = 1;
```

```text
sqlite> SELECT rowid FROM notes WHERE notes MATCH 'buy'
3
```

`secure-delete` removes a deleted row's entries from the index segments in
place, so no trace of it remains in the file:

```sql
CREATE VIRTUAL TABLE secrets USING fts5(body);
INSERT INTO secrets(secrets, rank) VALUES ('secure-delete', 1);
INSERT INTO secrets (rowid, body) VALUES (1, 'password hunter2'), (2, 'nothing here');
DELETE FROM secrets WHERE rowid = 1;
INSERT INTO secrets(secrets) VALUES ('integrity-check');
```

```text
sqlite> SELECT count(*) FROM secrets WHERE secrets MATCH 'hunter2'
0
sqlite> PRAGMA integrity_check
ok
```

Without `fts5_delete`, those writes are refused.

## Known differences

xl's plans follow SQLite 3.54.1's planner. Remaining differences are listed
in [`STATUS.md`](../../STATUS.md) under each unit's "Still open". One found
while writing this page: for an expression index that answers a query on
its own (`SELECT id FROM users WHERE lower(email) = ?` with an index on
`lower(email)`), xl prints `USING COVERING INDEX` where max and SQLite
3.53.1 print `USING INDEX`. The rows are the same.
