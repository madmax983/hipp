# The full profile

```toml
hipp = { git = "https://github.com/madmax983/hipp", default-features = false, features = ["full"] }
```

**Budget:** 512 KiB of code on thumbv7em (ratchet 498,188 B). `no_std`.
**Example:** [`examples/full.rs`](../../examples/full.rs).

```bash
cargo run --example full --no-default-features --features full
```

full is the core SQL dialect: what most applications use from SQLite.
`full` = `indexes` + `query` + `write` + `sql` + `params` + `math`, on top of
[tiny](tiny.md).

| feature | adds |
|---|---|
| `indexes` | index B-trees: `CREATE [UNIQUE] INDEX`, `DROP INDEX`, UNIQUE / PRIMARY KEY enforcement through automatic indexes, index lookups |
| `query` | the expression evaluator (operators, CAST, CASE, affinity and collation rules, the core scalar and aggregate functions), WHERE / ORDER BY / LIMIT / OFFSET, joins, aggregates, GROUP BY / HAVING, CHECK, DEFAULT, generated columns, STRICT tables, `ALTER TABLE ADD COLUMN` |
| `write` | UPDATE, DELETE and DROP TABLE, with index maintenance |
| `sql` | the rest of the dialect: subqueries, conflict clauses, UPSERT, RETURNING, UPDATE ... FROM, INSERT ... SELECT, compound SELECTs, derived tables, views and CTEs, PRAGMA, savepoints, VACUUM / REINDEX / ANALYZE, ALTER RENAME / DROP COLUMN, AUTOINCREMENT, TEMP objects, and the full function library |
| `params` | bound parameters |
| `math` | the SQL math functions (SQLite's `SQLITE_ENABLE_MATH_FUNCTIONS`), on an in-crate, correctly rounded floating-point library (no libm) |

Without `std` there is no clock. The example supplies a fixed one:

```rust
fn fixed_clock() -> i64 { 1_767_323_045_000 }   // 2026-01-02 03:04:05 UTC, in ms
let cfg = Config::default().clock(fixed_clock);
```

Each section below is a function of `examples/full.rs`, with its output.
They share one database: a `dept` table (`eng`, `ops`, `sales`, `legal`) and
an `emp` table of six employees.

## Table definitions

```sql
CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE emp (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    dept INTEGER,
    salary INTEGER CHECK (salary >= 0),
    hired TEXT DEFAULT CURRENT_TIMESTAMP,
    bonus REAL DEFAULT 0.0,
    total REAL GENERATED ALWAYS AS (salary + bonus) VIRTUAL
);
```

```text
sqlite> SELECT name, salary, bonus, total, hired FROM emp WHERE id = 1
ada|120|10.0|130.0|2026-01-02 03:04:05
sqlite> INSERT INTO emp (name, salary) VALUES ('neg', -1)
Error: Schema { kind: CheckViolation }
sqlite> INSERT INTO dept (name) VALUES ('eng')
Error: Schema { kind: UniqueViolation }
sqlite> UPDATE emp SET total = 0
Error: Schema { kind: GeneratedColumnWrite }
```

STRICT tables enforce column types (`ANY` accepts anything):

```text
sqlite> CREATE TABLE strict_t (a INTEGER, b TEXT, c ANY) STRICT
sqlite> INSERT INTO strict_t VALUES (1, 'x', 2.5), ('7', 'y', X'00')
sqlite> SELECT a, typeof(a), c, typeof(c) FROM strict_t
1|integer|2.5|real
7|integer|X'00'|blob
sqlite> INSERT INTO strict_t VALUES ('seven', 'z', NULL)
Error: Schema { kind: StrictTypeViolation }
```

Also: `STORED` generated columns, `DEFAULT (expr)`, table-level
`PRIMARY KEY` / `UNIQUE` / `CHECK`, `ON CONFLICT` clauses on constraints, and
`CREATE TABLE ... AS SELECT`.

## Expressions

```text
sqlite> SELECT 1 + 2 * 3, 7 / 2, 7 % 3, 7 / 2.0, -(-5)
7|3|1|3.5|5
sqlite> SELECT 'a' || 'b' || 3, length('héllo'), upper('abc'), lower('ABC')
ab3|5|ABC|abc
sqlite> SELECT CAST('42' AS INTEGER) + 1, CAST(3.9 AS INTEGER), CAST(10 AS TEXT) || '!', typeof(CAST('1.5' AS REAL))
43|3|10!|real
sqlite> SELECT CASE WHEN 2 > 1 THEN 'yes' ELSE 'no' END, CASE 3 WHEN 1 THEN 'one' WHEN 3 THEN 'three' END
yes|three
sqlite> SELECT NULL IS NULL, 1 IS NOT NULL, NULL = NULL, coalesce(NULL, NULL, 'c'), ifnull(NULL, 0), nullif(5, 5)
1|1||c|0|
sqlite> SELECT 5 BETWEEN 1 AND 10, 3 IN (1, 2, 3), 'abc' LIKE 'A%', 'abc' GLOB 'A*', 'a_c' LIKE 'a\_c' ESCAPE '\'
1|1|1|0|1
sqlite> SELECT 6 & 3, 6 | 3, 1 << 4, 256 >> 2, ~0
2|7|16|64|-1
sqlite> SELECT 1 IS DISTINCT FROM NULL, NULL IS NOT DISTINCT FROM NULL, (1, 2) = (1, 2), (1, 2) < (1, 3)
1|1|1|1
sqlite> SELECT 'abc' = 'ABC', 'abc' = 'ABC' COLLATE NOCASE, 'x  ' = 'x' COLLATE RTRIM
0|1|1
sqlite> SELECT typeof(1), typeof(1.0), typeof('1'), typeof(X'01'), typeof(NULL)
integer|real|text|blob|null
sqlite> SELECT typeof(9223372036854775807 + 1), 9223372036854775807 + 1 > 9e18
real|1
```

Integer overflow becomes REAL, NULL propagates, and comparisons apply
SQLite's affinity and collation rules (BINARY, NOCASE, RTRIM).

## Indexes

```sql
CREATE INDEX emp_dept ON emp(dept);
CREATE UNIQUE INDEX emp_name ON emp(name);
CREATE INDEX IF NOT EXISTS emp_dept ON emp(dept);
CREATE INDEX emp_salary_desc ON emp(salary DESC, name COLLATE NOCASE);
```

```text
indexes: ["emp_dept", "emp_name", "emp_salary_desc", "sqlite_autoindex_dept_1"]
sqlite> INSERT INTO emp (name, salary) VALUES ('ada', 1)
Error: Schema { kind: UniqueViolation }
sqlite> SELECT name FROM emp WHERE dept = 2
cleo
dev
sqlite> SELECT name FROM emp INDEXED BY emp_dept WHERE dept = 1
ada
brian
```

`sqlite_autoindex_dept_1` is the automatic index behind `dept.name UNIQUE`.
`Connection::indexes()` lists indexes from Rust. `DROP INDEX` removes one.
Expression and partial indexes are in [max](max.md) (`expr_index`). A full
build lists them in a file it opens, and refuses writes to their tables
rather than leave them stale.

## SELECT

```text
sqlite> SELECT name FROM emp WHERE salary > 80 AND dept IS NOT NULL ORDER BY salary DESC
ada
brian
cleo
sqlite> SELECT name, salary FROM emp ORDER BY salary LIMIT 2 OFFSET 1
eve|70
dev|80
sqlite> SELECT DISTINCT dept FROM emp ORDER BY dept NULLS LAST
1
2
3

sqlite> SELECT name AS who, salary * 12 AS yearly FROM emp WHERE yearly > 1000 ORDER BY who
ada|1440
brian|1200
cleo|1080
sqlite> SELECT name FROM emp WHERE name LIKE '%e%' ORDER BY 1
cleo
dev
eve
sqlite> SELECT name FROM emp WHERE dept IN (2, 3) ORDER BY name COLLATE NOCASE DESC
eve
dev
cleo
sqlite> VALUES (1, 'one'), (2, 'two')
1|one
2|two
sqlite> SELECT x FROM (SELECT 3 AS x UNION ALL SELECT 1 UNION ALL SELECT 2) ORDER BY x LIMIT 2
1
2
```

Result-column aliases work in WHERE, GROUP BY, HAVING and ORDER BY, as on
SQLite. LIMIT and OFFSET take expressions. `x IN table` and three-part names
(`main.t.col`) work too.

### Compound SELECTs

```text
sqlite> SELECT dept FROM emp UNION SELECT id FROM dept ORDER BY 1

1
2
3
4
sqlite> SELECT id FROM dept EXCEPT SELECT dept FROM emp
4
sqlite> SELECT id FROM dept INTERSECT SELECT dept FROM emp
1
2
3
```

(The empty first row is NULL, which sorts first.)

## Joins

```text
sqlite> SELECT e.name, d.name FROM emp e JOIN dept d ON d.id = e.dept WHERE d.name = 'ops' ORDER BY e.name
cleo|ops
dev|ops
sqlite> SELECT e.name, d.name FROM emp e LEFT JOIN dept d ON d.id = e.dept WHERE d.id IS NULL
finn|
sqlite> SELECT d.name, e.name FROM emp e RIGHT JOIN dept d ON d.id = e.dept WHERE e.id IS NULL
legal|
sqlite> SELECT count(*) FROM emp e FULL OUTER JOIN dept d ON d.id = e.dept
7
sqlite> SELECT count(*) FROM dept a CROSS JOIN dept b
16
sqlite> SELECT name, city FROM emp JOIN loc USING (dept) ORDER BY name
ada|oslo
brian|oslo
cleo|lima
dev|lima
sqlite> SELECT a.name, b.name FROM emp a JOIN emp b ON a.dept = b.dept AND a.id < b.id ORDER BY 1
ada|brian
cleo|dev
```

INNER, LEFT, RIGHT, FULL OUTER, CROSS and NATURAL joins, `USING`, and
self-joins are all supported. In full and max, joins run in FROM order, each table read by
its best single-table access path. [xl](xl.md) adds SQLite's cost-based join
ordering. The answers are the same, and only the speed differs.

## Aggregates

```text
sqlite> SELECT count(*), count(dept), sum(salary), round(avg(salary), 2), min(salary), max(salary), total(bonus) FROM emp
6|5|520|86.67|60|120|35.0
sqlite> SELECT dept, count(*), sum(salary) FROM emp GROUP BY dept HAVING count(*) > 1 ORDER BY dept
1|2|220
2|2|170
sqlite> SELECT group_concat(name, ',') FROM (SELECT name FROM emp ORDER BY name)
ada,brian,cleo,dev,eve,finn
sqlite> SELECT string_agg(name, '/' ORDER BY salary DESC) FROM emp WHERE dept = 1
ada/brian
sqlite> SELECT count(*) FILTER (WHERE salary >= 90), count(DISTINCT dept) FROM emp
3|3
sqlite> SELECT median(salary), percentile(salary, 25), percentile_cont(salary, 0.5) FROM emp
85.0|72.5|85.0
sqlite> SELECT name, max(salary) FROM emp
ada|120
```

`sum` / `avg` / `total` round as SQLite does. The aggregate `ORDER BY`,
`FILTER (WHERE ...)`, `DISTINCT`, `median`, `percentile`,
`percentile_cont` / `percentile_disc` are all here. A bare column next to a
lone `min()` / `max()` comes from that row, as on SQLite.

## Subqueries

```text
sqlite> SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY name
ada
brian
cleo
sqlite> SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE emp.dept = dept.id)
legal
sqlite> SELECT name FROM emp WHERE dept IN (SELECT id FROM dept WHERE name LIKE 's%')
eve
sqlite> SELECT d.name, (SELECT max(salary) FROM emp WHERE emp.dept = d.id) FROM dept d ORDER BY d.id
eng|120
ops|90
sales|70
legal|
sqlite> SELECT dname, n FROM (SELECT d.name AS dname, count(e.id) AS n FROM dept d LEFT JOIN emp e ON e.dept = d.id GROUP BY d.id) WHERE n < 2
sales|1
legal|0
```

Scalar, `IN (SELECT ...)`, `EXISTS`, correlated subqueries and FROM-clause
derived tables, in SELECT, UPDATE, DELETE and INSERT.

## Writing: UPDATE, DELETE, RETURNING

```sql
UPDATE emp SET salary = salary + 5 WHERE dept = 2;          -- db.changes() == 2
```

```text
sqlite> UPDATE emp SET bonus = bonus + 1 WHERE name = 'finn' RETURNING name, bonus, total
finn|1.0|61.0
```

`UPDATE ... FROM`:

```sql
CREATE TABLE raises (dept INTEGER, pct INTEGER);
INSERT INTO raises VALUES (1, 10);
UPDATE emp SET salary = salary * (100 + raises.pct) / 100 FROM raises WHERE raises.dept = emp.dept;
```

```text
sqlite> SELECT name, salary FROM emp WHERE dept = 1
ada|132
brian|110
```

`INSERT ... SELECT`, `DELETE ... RETURNING`, `INSERT ... DEFAULT VALUES`,
`DROP TABLE`:

```text
sqlite> DELETE FROM emp WHERE dept IS NULL RETURNING id, name
6|finn
sqlite> SELECT * FROM archive
finn|60
sqlite> INSERT INTO emp (name, dept, salary) VALUES ('gus', 3, 75) RETURNING id
6
last_insert_rowid() = 6
sqlite> SELECT count(*), count(name) FROM archive
2|1
sqlite> SELECT * FROM archive
Error: Schema { kind: UnknownTable }
```

Without AUTOINCREMENT a new rowid is max(rowid) + 1, so the deleted 6 is
reused. `Connection::changes()`, `total_changes()` and `last_insert_rowid()`
match SQLite's functions of the same names.

## Conflict clauses and UPSERT

```sql
CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER, hits INTEGER DEFAULT 1);
INSERT INTO kv (k, v) VALUES ('a', 1), ('b', 2);
INSERT OR IGNORE INTO kv (k, v) VALUES ('a', 100);     -- kept 1
INSERT OR REPLACE INTO kv (k, v) VALUES ('b', 20);     -- replaced
-- three times:
INSERT INTO kv (k, v) VALUES ('a', 5) ON CONFLICT (k) DO UPDATE SET hits = hits + 1, v = excluded.v;
INSERT INTO kv (k, v) VALUES ('c', 3) ON CONFLICT DO NOTHING;
```

```text
sqlite> SELECT k, v, hits FROM kv ORDER BY k
a|5|4
b|20|1
c|3|1
sqlite> INSERT OR ABORT INTO kv (k, v) VALUES ('c', 0)
Error: Schema { kind: UniqueViolation }
```

`OR ROLLBACK | ABORT | FAIL | IGNORE | REPLACE` work on INSERT and UPDATE,
and on column and table constraints. UPSERT takes a conflict target with an
optional `WHERE`, and `DO UPDATE ... WHERE`. Known gap: SQLite's chained
upsert (several `ON CONFLICT` clauses on one INSERT) is a parse error in
hipp (`STATUS.md`, honest gaps).

## Views and CTEs

```sql
CREATE VIEW payroll AS
  SELECT d.name AS dept, sum(e.salary) AS cost FROM emp e JOIN dept d ON d.id = e.dept GROUP BY d.name;
```

```text
sqlite> SELECT * FROM payroll ORDER BY cost DESC
eng|242
ops|180
sales|145
sqlite> WITH rich AS (SELECT name, salary FROM emp WHERE salary > 100) SELECT name FROM rich ORDER BY salary
brian
ada
sqlite> WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT group_concat(i, ' ') FROM n
1 2 3 4 5
```

A recursive CTE walking a tree:

```text
sqlite> WITH RECURSIVE chain(id, title, depth) AS (
            SELECT id, title, 0 FROM org WHERE title = 'dev'
            UNION ALL
            SELECT o.id, o.title, depth + 1 FROM org o JOIN chain c ON o.id = (SELECT boss FROM org WHERE id = c.id)
         ) SELECT title, depth FROM chain
dev|0
lead|1
cto|2
ceo|3
```

`Connection::views()` lists views from Rust. Views are read-only in full.
INSTEAD OF triggers make them writable in [max](max.md#triggers-triggers).

## The function library

```text
sqlite> SELECT substr('hello', 2, 3), replace('a-b-c', '-', '+'), trim('  x  '), ltrim('xxy', 'x'), rtrim('yxx', 'x')
ell|a+b+c|x|y|y
sqlite> SELECT instr('hello', 'll'), hex('AB'), unhex('4142'), quote('it''s'), char(72, 105), unicode('A')
3|4142|X'4142'|'it''s'|Hi|65
sqlite> SELECT printf('%5.2f|%-4s|%04d|%x', 3.14159, 'ab', 42, 255), format('%s=%d', 'n', 7)
 3.14|ab  |0042|ff|n=7
sqlite> SELECT abs(-3), round(2.567, 2), round(2.5), sign(-7), max(1, 5, 3), min(4, 2)
3|2.57|3.0|-1|5|2
sqlite> SELECT concat('a', NULL, 'b'), concat_ws('-', 'x', 'y', 'z'), octet_length('héllo'), length(zeroblob(4))
ab|x-y-z|6|4
sqlite> SELECT iif(1 > 2, 'a', 'b'), if(0, 'x', 1, 'y'), likely(1), typeof(random()), length(randomblob(8))
b|y|1|integer|8
sqlite> SELECT sqlite_version()
3.54.1
```

Also: `like()`, `glob()`, `last_insert_rowid()`, `changes()`,
`total_changes()`, `unlikely()`, `likelihood()`, and the rest of SQLite's
core functions (`length`, `lower`, `upper`, `typeof`, `zeroblob`, ...).

## Math functions

```text
sqlite> SELECT sqrt(16), pow(2, 10), floor(-1.5), ceil(1.2), trunc(-2.7), mod(10, 3)
4.0|1024.0|-2.0|2.0|-2.0|1.0
sqlite> SELECT round(pi(), 6), round(exp(1), 6), ln(1), log10(1000), log(2, 8), round(degrees(pi()), 1)
3.141593|2.718282|0.0|3.0|3.0|180.0
sqlite> SELECT round(sin(pi() / 2), 6), round(cos(0), 6), round(atan2(1, 1) * 4, 6)
1.0|1.0|3.141593
```

The full set: `acos acosh asin asinh atan atan2 atanh ceil ceiling cos cosh
degrees exp floor ln log log10 log2 log(B, X) mod pi pow power radians sin
sinh sqrt tan tanh trunc`.

## Bound parameters

```rust
let mut ins = db.prepare("INSERT INTO emp (name, dept, salary) VALUES (?1, ?2, ?3)").await?;
for (name, dept, salary) in [("hal", 4, 50), ("ivy", 4, 55)] {
    ins.bind_text(1, name)?;
    ins.bind_i64(2, dept)?;
    ins.bind_i64(3, salary)?;
    ins.step().await?;
    ins.reset();
}
drop(ins);

let mut q = db.prepare("SELECT name FROM emp WHERE dept = :dept AND salary >= @min ORDER BY name").await?;
let dept = q.parameter_index(":dept").unwrap();
let min = q.parameter_index("@min").unwrap();
q.bind_i64(dept, 4)?;
q.bind_value(min, Value::Integer(52))?;
// dept 4, salary >= 52: ["ivy"]
```

See the [guide §5](../GUIDE.md#5-bound-parameters).

## Transactions and savepoints

```sql
BEGIN;
INSERT INTO dept (name) VALUES ('r&d');
SAVEPOINT sp1;
INSERT INTO dept (name) VALUES ('temp1');
SAVEPOINT sp2;
INSERT INTO dept (name) VALUES ('temp2');
ROLLBACK TO sp1;      -- undoes temp1 and temp2
RELEASE sp1;
COMMIT;
```

```text
sqlite> SELECT name FROM dept WHERE id > 4
r&d
```

A failed statement inside a transaction undoes only itself:

```text
sqlite> INSERT INTO dept (name) VALUES ('kept')
Error: Schema { kind: UniqueViolation }
sqlite> SELECT count(*) FROM dept WHERE name = 'kept'
1
```

## PRAGMA and maintenance

```text
sqlite> PRAGMA table_info(dept)
0|id|INTEGER|0||1
1|name|TEXT|1||0
sqlite> SELECT name FROM pragma_table_info('kv') ORDER BY cid
k
v
hits
sqlite> SELECT type, name FROM sqlite_schema WHERE tbl_name = 'dept' ORDER BY name
table|dept
index|sqlite_autoindex_dept_1
sqlite> PRAGMA index_list(emp)
0|emp_name|1|c|0
1|emp_dept|0|c|0
sqlite> PRAGMA page_size
4096
sqlite> PRAGMA user_version
0
sqlite> PRAGMA user_version
7
sqlite> PRAGMA integrity_check
ok
sqlite> PRAGMA quick_check
ok
sqlite> SELECT tbl, idx FROM sqlite_stat1 WHERE tbl = 'emp' ORDER BY idx
emp|emp_dept
emp|emp_name
VACUUM: 41 pages (28 free) -> 13 pages
sqlite> PRAGMA freelist_count
0
```

The PRAGMAs cover header values (`page_size`, `user_version`,
`application_id`, `schema_version`, `freelist_count`, `page_count`,
`encoding`, ...), flags (`case_sensitive_like`, `recursive_triggers`,
`foreign_keys`, ...), schema introspection (`table_info`, `table_xinfo`,
`table_list`, `index_list`, `index_info`, `index_xinfo`, `foreign_key_list`,
`database_list`, ...), and `integrity_check` / `quick_check`. Each one that
returns rows is also a table-valued `pragma_*` function. `sqlite_schema` (and
`sqlite_master`) is readable.

`VACUUM` rebuilds the file. `VACUUM INTO 'name'` writes a copy through
`Config::vacuum_into` (a file under `std`). `REINDEX` rebuilds indexes, and
`ANALYZE` writes `sqlite_stat1`.

## ALTER TABLE and AUTOINCREMENT

```sql
ALTER TABLE loc ADD COLUMN zip TEXT DEFAULT '0000';
ALTER TABLE loc RENAME COLUMN city TO town;
ALTER TABLE loc RENAME TO site;
ALTER TABLE site DROP COLUMN zip;
```

```text
sqlite> SELECT dept, town, zip FROM site ORDER BY dept
1|oslo|0000
2|lima|0000
sqlite> SELECT sql FROM sqlite_schema WHERE name = 'site'
CREATE TABLE "site" (dept INTEGER, town TEXT)
```

The stored `CREATE TABLE` text is rewritten as SQLite rewrites it.
AUTOINCREMENT never reuses a rowid:

```text
sqlite> SELECT id, what FROM ticket
1|a
3|c
sqlite> SELECT name, seq FROM sqlite_sequence
ticket|3
```

## TEMP objects

```sql
CREATE TEMP TABLE scratch (x);
INSERT INTO scratch SELECT id FROM dept WHERE id <= 2;
```

```text
sqlite> SELECT x, (SELECT name FROM main.dept WHERE id = x) FROM temp.scratch
1|eng
2|ops
```

TEMP tables, views and indexes live in a separate in-memory database
(`Connection::temp_db()`). They never reach the main file, and statements
can mix both.

## Not in full

Window functions, triggers, foreign-key enforcement, JSON, date / time
functions, expression and partial indexes, WITHOUT ROWID, UTF-16, WAL,
ATTACH, auto-vacuum, virtual tables, FTS5, R*Tree and EXPLAIN QUERY PLAN are
in [max](max.md). Without `datetime`, `date()` and the other date and time
functions are unknown, but `DEFAULT CURRENT_TIMESTAMP` still works.
