//! Everything the **full** profile adds over tiny
//! (`--no-default-features --features full`, ≤ 512 KiB of code on
//! thumbv7em): the core SQL dialect. `full` is `indexes` + `query` +
//! `write` + `sql` + `params` + `math`.
//!
//! ```text
//! cargo run --example full --no-default-features --features full
//! ```
//!
//! (`full` is `no_std`; adding `std` only brings `FileStorage` and the
//! system clock.) `docs/profiles/full.md` walks through it section by
//! section.

mod common;

use common::{check, exec, exec_all, fails, rows, section};
use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step, Value};

/// A fixed wall clock (Unix milliseconds): no_std builds have no
/// clock of their own, so you supply one. `DEFAULT CURRENT_TIMESTAMP`
/// reads it. 2026-01-02 03:04:05 UTC.
fn fixed_clock() -> i64 {
    1_767_323_045_000
}

fn main() {
    block_on(async {
        let cfg = Config::default().clock(fixed_clock);
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");

        schema(&mut db).await;
        expressions(&mut db).await;
        indexes(&mut db).await;
        queries(&mut db).await;
        joins(&mut db).await;
        aggregates(&mut db).await;
        subqueries(&mut db).await;
        writes(&mut db).await;
        conflicts(&mut db).await;
        views_and_ctes(&mut db).await;
        functions(&mut db).await;
        math(&mut db).await;
        params(&mut db).await;
        transactions(&mut db).await;
        pragmas_and_maintenance(&mut db).await;
        alter(&mut db).await;
        temp(&mut db).await;
    });
    println!("\nfull: ok");
}

type Db = Connection<MemStorage>;

async fn schema(db: &mut Db) {
    section("Table definitions: constraints, defaults, generated columns, STRICT");
    exec_all(
        db,
        &[
            "CREATE TABLE dept (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
            "CREATE TABLE emp (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                dept INTEGER,
                salary INTEGER CHECK (salary >= 0),
                hired TEXT DEFAULT CURRENT_TIMESTAMP,
                bonus REAL DEFAULT 0.0,
                total REAL GENERATED ALWAYS AS (salary + bonus) VIRTUAL
            )",
            "INSERT INTO dept (name) VALUES ('eng'), ('ops'), ('sales'), ('legal')",
            "INSERT INTO emp (name, dept, salary, bonus) VALUES
                ('ada', 1, 120, 10), ('brian', 1, 100, 0), ('cleo', 2, 90, 5),
                ('dev', 2, 80, 0), ('eve', 3, 70, 20), ('finn', NULL, 60, 0)",
        ],
    )
    .await;
    check(
        db,
        "SELECT name, salary, bonus, total, hired FROM emp WHERE id = 1",
        &["ada|120|10.0|130.0|2026-01-02 03:04:05"],
    )
    .await;
    // CHECK, NOT NULL and UNIQUE are enforced with named errors.
    fails(db, "INSERT INTO emp (name, salary) VALUES ('neg', -1)").await;
    fails(db, "INSERT INTO dept (name) VALUES ('eng')").await;
    fails(db, "UPDATE emp SET total = 0").await;
    // STRICT tables refuse values that do not fit the column type.
    exec(
        db,
        "CREATE TABLE strict_t (a INTEGER, b TEXT, c ANY) STRICT",
    )
    .await;
    exec(
        db,
        "INSERT INTO strict_t VALUES (1, 'x', 2.5), ('7', 'y', X'00')",
    )
    .await;
    check(
        db,
        "SELECT a, typeof(a), c, typeof(c) FROM strict_t",
        &["1|integer|2.5|real", "7|integer|X'00'|blob"],
    )
    .await;
    fails(db, "INSERT INTO strict_t VALUES ('seven', 'z', NULL)").await;
}

async fn expressions(db: &mut Db) {
    section("Expressions, operators, CAST, CASE, affinity");
    check(
        db,
        "SELECT 1 + 2 * 3, 7 / 2, 7 % 3, 7 / 2.0, -(-5)",
        &["7|3|1|3.5|5"],
    )
    .await;
    check(
        db,
        "SELECT 'a' || 'b' || 3, length('héllo'), upper('abc'), lower('ABC')",
        &["ab3|5|ABC|abc"],
    )
    .await;
    check(
        db,
        "SELECT CAST('42' AS INTEGER) + 1, CAST(3.9 AS INTEGER), CAST(10 AS TEXT) || '!', typeof(CAST('1.5' AS REAL))",
        &["43|3|10!|real"],
    )
    .await;
    check(
        db,
        "SELECT CASE WHEN 2 > 1 THEN 'yes' ELSE 'no' END, CASE 3 WHEN 1 THEN 'one' WHEN 3 THEN 'three' END",
        &["yes|three"],
    )
    .await;
    check(db, "SELECT NULL IS NULL, 1 IS NOT NULL, NULL = NULL, coalesce(NULL, NULL, 'c'), ifnull(NULL, 0), nullif(5, 5)", &[
        "1|1||c|0|",
    ])
    .await;
    check(db, "SELECT 5 BETWEEN 1 AND 10, 3 IN (1, 2, 3), 'abc' LIKE 'A%', 'abc' GLOB 'A*', 'a_c' LIKE 'a\\_c' ESCAPE '\\'", &[
        "1|1|1|0|1",
    ])
    .await;
    check(
        db,
        "SELECT 6 & 3, 6 | 3, 1 << 4, 256 >> 2, ~0",
        &["2|7|16|64|-1"],
    )
    .await;
    check(db, "SELECT 1 IS DISTINCT FROM NULL, NULL IS NOT DISTINCT FROM NULL, (1, 2) = (1, 2), (1, 2) < (1, 3)", &[
        "1|1|1|1",
    ])
    .await;
    check(
        db,
        "SELECT 'abc' = 'ABC', 'abc' = 'ABC' COLLATE NOCASE, 'x  ' = 'x' COLLATE RTRIM",
        &["0|1|1"],
    )
    .await;
    check(
        db,
        "SELECT typeof(1), typeof(1.0), typeof('1'), typeof(X'01'), typeof(NULL)",
        &["integer|real|text|blob|null"],
    )
    .await;
    // Integer overflow becomes REAL, as on SQLite.
    check(
        db,
        "SELECT typeof(9223372036854775807 + 1), 9223372036854775807 + 1 > 9e18",
        &["real|1"],
    )
    .await;
}

async fn indexes(db: &mut Db) {
    section("Indexes (feature `indexes`)");
    exec_all(
        db,
        &[
            "CREATE INDEX emp_dept ON emp(dept)",
            "CREATE UNIQUE INDEX emp_name ON emp(name)",
            "CREATE INDEX IF NOT EXISTS emp_dept ON emp(dept)",
            "CREATE INDEX emp_salary_desc ON emp(salary DESC, name COLLATE NOCASE)",
        ],
    )
    .await;
    let mut names: Vec<String> = db
        .indexes()
        .iter()
        .map(|i| String::from_utf8_lossy(i.name).into_owned())
        .collect();
    names.sort();
    println!("indexes: {names:?}");
    // UNIQUE / PRIMARY KEY constraints create automatic indexes.
    assert!(names.iter().any(|n| n.starts_with("sqlite_autoindex_dept")));
    fails(db, "INSERT INTO emp (name, salary) VALUES ('ada', 1)").await;
    check(db, "SELECT name FROM emp WHERE dept = 2", &["cleo", "dev"]).await;
    check(
        db,
        "SELECT name FROM emp INDEXED BY emp_dept WHERE dept = 1",
        &["ada", "brian"],
    )
    .await;
    exec(db, "DROP INDEX emp_salary_desc").await;
}

async fn queries(db: &mut Db) {
    section("SELECT: WHERE, ORDER BY, LIMIT / OFFSET, DISTINCT, aliases");
    check(
        db,
        "SELECT name FROM emp WHERE salary > 80 AND dept IS NOT NULL ORDER BY salary DESC",
        &["ada", "brian", "cleo"],
    )
    .await;
    check(
        db,
        "SELECT name, salary FROM emp ORDER BY salary LIMIT 2 OFFSET 1",
        &["eve|70", "dev|80"],
    )
    .await;
    check(
        db,
        "SELECT DISTINCT dept FROM emp ORDER BY dept NULLS LAST",
        &["1", "2", "3", ""],
    )
    .await;
    check(
        db,
        "SELECT name AS who, salary * 12 AS yearly FROM emp WHERE yearly > 1000 ORDER BY who",
        &["ada|1440", "brian|1200", "cleo|1080"],
    )
    .await;
    check(
        db,
        "SELECT name FROM emp WHERE name LIKE '%e%' ORDER BY 1",
        &["cleo", "dev", "eve"],
    )
    .await;
    check(
        db,
        "SELECT name FROM emp WHERE dept IN (2, 3) ORDER BY name COLLATE NOCASE DESC",
        &["eve", "dev", "cleo"],
    )
    .await;
    check(db, "VALUES (1, 'one'), (2, 'two')", &["1|one", "2|two"]).await;
    check(
        db,
        "SELECT x FROM (SELECT 3 AS x UNION ALL SELECT 1 UNION ALL SELECT 2) ORDER BY x LIMIT 2",
        &["1", "2"],
    )
    .await;

    section("Compound SELECTs");
    check(
        db,
        "SELECT dept FROM emp UNION SELECT id FROM dept ORDER BY 1",
        &["", "1", "2", "3", "4"],
    )
    .await;
    check(
        db,
        "SELECT id FROM dept EXCEPT SELECT dept FROM emp",
        &["4"],
    )
    .await;
    check(
        db,
        "SELECT id FROM dept INTERSECT SELECT dept FROM emp",
        &["1", "2", "3"],
    )
    .await;
}

async fn joins(db: &mut Db) {
    section("Joins (feature `query`)");
    check(
        db,
        "SELECT e.name, d.name FROM emp e JOIN dept d ON d.id = e.dept WHERE d.name = 'ops' ORDER BY e.name",
        &["cleo|ops", "dev|ops"],
    )
    .await;
    check(
        db,
        "SELECT e.name, d.name FROM emp e LEFT JOIN dept d ON d.id = e.dept WHERE d.id IS NULL",
        &["finn|"],
    )
    .await;
    check(
        db,
        "SELECT d.name, e.name FROM emp e RIGHT JOIN dept d ON d.id = e.dept WHERE e.id IS NULL",
        &["legal|"],
    )
    .await;
    check(
        db,
        "SELECT count(*) FROM emp e FULL OUTER JOIN dept d ON d.id = e.dept",
        &["7"],
    )
    .await;
    check(db, "SELECT count(*) FROM dept a CROSS JOIN dept b", &["16"]).await;
    exec(db, "CREATE TABLE loc (dept INTEGER, city TEXT)").await;
    exec(db, "INSERT INTO loc VALUES (1, 'oslo'), (2, 'lima')").await;
    check(
        db,
        "SELECT name, city FROM emp JOIN loc USING (dept) ORDER BY name",
        &["ada|oslo", "brian|oslo", "cleo|lima", "dev|lima"],
    )
    .await;
    check(
        db,
        "SELECT a.name, b.name FROM emp a JOIN emp b ON a.dept = b.dept AND a.id < b.id ORDER BY 1",
        &["ada|brian", "cleo|dev"],
    )
    .await;
}

async fn aggregates(db: &mut Db) {
    section("Aggregates, GROUP BY, HAVING, FILTER, ordered aggregates");
    check(db, "SELECT count(*), count(dept), sum(salary), round(avg(salary), 2), min(salary), max(salary), total(bonus) FROM emp", &[
        "6|5|520|86.67|60|120|35.0",
    ])
    .await;
    check(
        db,
        "SELECT dept, count(*), sum(salary) FROM emp GROUP BY dept HAVING count(*) > 1 ORDER BY dept",
        &["1|2|220", "2|2|170"],
    )
    .await;
    check(
        db,
        "SELECT group_concat(name, ',') FROM (SELECT name FROM emp ORDER BY name)",
        &["ada,brian,cleo,dev,eve,finn"],
    )
    .await;
    check(
        db,
        "SELECT string_agg(name, '/' ORDER BY salary DESC) FROM emp WHERE dept = 1",
        &["ada/brian"],
    )
    .await;
    check(
        db,
        "SELECT count(*) FILTER (WHERE salary >= 90), count(DISTINCT dept) FROM emp",
        &["3|3"],
    )
    .await;
    check(
        db,
        "SELECT median(salary), percentile(salary, 25), percentile_cont(salary, 0.5) FROM emp",
        &["85.0|72.5|85.0"],
    )
    .await;
    // A bare column next to max() comes from the max row, as on SQLite.
    check(db, "SELECT name, max(salary) FROM emp", &["ada|120"]).await;
}

async fn subqueries(db: &mut Db) {
    section("Subqueries (feature `sql`)");
    check(
        db,
        "SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY name",
        &["ada", "brian", "cleo"],
    )
    .await;
    check(
        db,
        "SELECT name FROM dept WHERE NOT EXISTS (SELECT 1 FROM emp WHERE emp.dept = dept.id)",
        &["legal"],
    )
    .await;
    check(
        db,
        "SELECT name FROM emp WHERE dept IN (SELECT id FROM dept WHERE name LIKE 's%')",
        &["eve"],
    )
    .await;
    check(
        db,
        "SELECT d.name, (SELECT max(salary) FROM emp WHERE emp.dept = d.id) FROM dept d ORDER BY d.id",
        &["eng|120", "ops|90", "sales|70", "legal|"],
    )
    .await;
    check(
        db,
        "SELECT dname, n FROM (SELECT d.name AS dname, count(e.id) AS n FROM dept d LEFT JOIN emp e ON e.dept = d.id GROUP BY d.id) WHERE n < 2",
        &["sales|1", "legal|0"],
    )
    .await;
}

async fn writes(db: &mut Db) {
    section("UPDATE, DELETE, RETURNING, UPDATE ... FROM, INSERT ... SELECT");
    exec(db, "UPDATE emp SET salary = salary + 5 WHERE dept = 2").await;
    println!("changes() = {}", db.changes());
    assert_eq!(db.changes(), 2);
    check(
        db,
        "SELECT name, salary FROM emp WHERE dept = 2",
        &["cleo|95", "dev|85"],
    )
    .await;
    check(
        db,
        "UPDATE emp SET bonus = bonus + 1 WHERE name = 'finn' RETURNING name, bonus, total",
        &["finn|1.0|61.0"],
    )
    .await;
    exec(db, "CREATE TABLE raises (dept INTEGER, pct INTEGER)").await;
    exec(db, "INSERT INTO raises VALUES (1, 10)").await;
    exec(db, "UPDATE emp SET salary = salary * (100 + raises.pct) / 100 FROM raises WHERE raises.dept = emp.dept").await;
    check(
        db,
        "SELECT name, salary FROM emp WHERE dept = 1",
        &["ada|132", "brian|110"],
    )
    .await;
    exec(db, "CREATE TABLE archive (name TEXT, salary INTEGER)").await;
    exec(
        db,
        "INSERT INTO archive SELECT name, salary FROM emp WHERE dept IS NULL",
    )
    .await;
    check(
        db,
        "DELETE FROM emp WHERE dept IS NULL RETURNING id, name",
        &["6|finn"],
    )
    .await;
    check(db, "SELECT * FROM archive", &["finn|60"]).await;
    // Without AUTOINCREMENT the next rowid is max(rowid) + 1, so the
    // deleted 6 is reused (see `alter` for AUTOINCREMENT).
    check(
        db,
        "INSERT INTO emp (name, dept, salary) VALUES ('gus', 3, 75) RETURNING id",
        &["6"],
    )
    .await;
    println!("last_insert_rowid() = {}", db.last_insert_rowid());
    assert_eq!(db.last_insert_rowid(), 6);
    exec(db, "INSERT INTO archive DEFAULT VALUES").await;
    check(db, "SELECT count(*), count(name) FROM archive", &["2|1"]).await;
    exec(db, "DELETE FROM archive").await;
    exec(db, "DROP TABLE archive").await;
    fails(db, "SELECT * FROM archive").await;
}

async fn conflicts(db: &mut Db) {
    section("Conflict clauses and UPSERT");
    exec(
        db,
        "CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER, hits INTEGER DEFAULT 1)",
    )
    .await;
    exec(db, "INSERT INTO kv (k, v) VALUES ('a', 1), ('b', 2)").await;
    exec(db, "INSERT OR IGNORE INTO kv (k, v) VALUES ('a', 100)").await;
    exec(db, "INSERT OR REPLACE INTO kv (k, v) VALUES ('b', 20)").await;
    check(db, "SELECT k, v FROM kv ORDER BY k", &["a|1", "b|20"]).await;
    // UPSERT: insert, or update the existing row on conflict.
    for _ in 0..3 {
        exec(
            db,
            "INSERT INTO kv (k, v) VALUES ('a', 5) ON CONFLICT (k) DO UPDATE SET hits = hits + 1, v = excluded.v",
        )
        .await;
    }
    exec(
        db,
        "INSERT INTO kv (k, v) VALUES ('c', 3) ON CONFLICT DO NOTHING",
    )
    .await;
    check(
        db,
        "SELECT k, v, hits FROM kv ORDER BY k",
        &["a|5|4", "b|20|1", "c|3|1"],
    )
    .await;
    fails(db, "INSERT OR ABORT INTO kv (k, v) VALUES ('c', 0)").await;
    exec(db, "UPDATE OR IGNORE kv SET k = 'a' WHERE k = 'c'").await;
    check(db, "SELECT count(*) FROM kv WHERE k = 'c'", &["1"]).await;
}

async fn views_and_ctes(db: &mut Db) {
    section("Views and common table expressions (WITH, WITH RECURSIVE)");
    exec(
        db,
        "CREATE VIEW payroll AS SELECT d.name AS dept, sum(e.salary) AS cost FROM emp e JOIN dept d ON d.id = e.dept GROUP BY d.name",
    )
    .await;
    check(
        db,
        "SELECT * FROM payroll ORDER BY cost DESC",
        &["eng|242", "ops|180", "sales|145"],
    )
    .await;
    let views: Vec<String> = db
        .views()
        .iter()
        .map(|v| String::from_utf8_lossy(v.name).into_owned())
        .collect();
    assert_eq!(views, ["payroll"]);
    check(
        db,
        "WITH rich AS (SELECT name, salary FROM emp WHERE salary > 100) SELECT name FROM rich ORDER BY salary",
        &["brian", "ada"],
    )
    .await;
    check(
        db,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT group_concat(i, ' ') FROM n",
        &["1 2 3 4 5"],
    )
    .await;
    // A recursive CTE walking a tree.
    exec_all(
        db,
        &[
            "CREATE TABLE org (id INTEGER PRIMARY KEY, boss INTEGER, title TEXT)",
            "INSERT INTO org VALUES (1, NULL, 'ceo'), (2, 1, 'cto'), (3, 2, 'lead'), (4, 3, 'dev'), (5, 1, 'cfo')",
        ],
    )
    .await;
    check(
        db,
        "WITH RECURSIVE chain(id, title, depth) AS (
            SELECT id, title, 0 FROM org WHERE title = 'dev'
            UNION ALL
            SELECT o.id, o.title, depth + 1 FROM org o JOIN chain c ON o.id = (SELECT boss FROM org WHERE id = c.id)
         ) SELECT title, depth FROM chain",
        &["dev|0", "lead|1", "cto|2", "ceo|3"],
    )
    .await;
    exec(db, "DROP VIEW payroll").await;
}

async fn functions(db: &mut Db) {
    section("The scalar function library");
    check(db, "SELECT substr('hello', 2, 3), replace('a-b-c', '-', '+'), trim('  x  '), ltrim('xxy', 'x'), rtrim('yxx', 'x')", &[
        "ell|a+b+c|x|y|y",
    ])
    .await;
    check(db, "SELECT instr('hello', 'll'), hex('AB'), unhex('4142'), quote('it''s'), char(72, 105), unicode('A')", &[
        "3|4142|X'4142'|'it''s'|Hi|65",
    ])
    .await;
    check(
        db,
        "SELECT printf('%5.2f|%-4s|%04d|%x', 3.14159, 'ab', 42, 255), format('%s=%d', 'n', 7)",
        &[" 3.14|ab  |0042|ff|n=7"],
    )
    .await;
    check(
        db,
        "SELECT abs(-3), round(2.567, 2), round(2.5), sign(-7), max(1, 5, 3), min(4, 2)",
        &["3|2.57|3.0|-1|5|2"],
    )
    .await;
    check(db, "SELECT concat('a', NULL, 'b'), concat_ws('-', 'x', 'y', 'z'), octet_length('héllo'), length(zeroblob(4))", &[
        "ab|x-y-z|6|4",
    ])
    .await;
    check(db, "SELECT iif(1 > 2, 'a', 'b'), if(0, 'x', 1, 'y'), likely(1), typeof(random()), length(randomblob(8))", &[
        "b|y|1|integer|8",
    ])
    .await;
    check(db, "SELECT sqlite_version()", &["3.54.1"]).await;
}

async fn math(db: &mut Db) {
    section("Math functions (feature `math`)");
    check(
        db,
        "SELECT sqrt(16), pow(2, 10), floor(-1.5), ceil(1.2), trunc(-2.7), mod(10, 3)",
        &["4.0|1024.0|-2.0|2.0|-2.0|1.0"],
    )
    .await;
    check(db, "SELECT round(pi(), 6), round(exp(1), 6), ln(1), log10(1000), log(2, 8), round(degrees(pi()), 1)", &[
        "3.141593|2.718282|0.0|3.0|3.0|180.0",
    ])
    .await;
    check(
        db,
        "SELECT round(sin(pi() / 2), 6), round(cos(0), 6), round(atan2(1, 1) * 4, 6)",
        &["1.0|1.0|3.141593"],
    )
    .await;
}

async fn params(db: &mut Db) {
    section("Bound parameters (feature `params`)");
    // Prepare once, bind / step / reset per row.
    let mut ins = db
        .prepare("INSERT INTO emp (name, dept, salary) VALUES (?1, ?2, ?3)")
        .await
        .expect("prepare");
    assert_eq!(ins.parameter_count(), 3);
    for (name, dept, salary) in [("hal", 4, 50), ("ivy", 4, 55)] {
        ins.bind_text(1, name).expect("bind");
        ins.bind_i64(2, dept).expect("bind");
        ins.bind_i64(3, salary).expect("bind");
        assert_eq!(ins.step().await.expect("step"), Step::Done);
        ins.reset();
    }
    drop(ins);
    // Named parameters: :name, @name, $name.
    let mut q = db
        .prepare("SELECT name FROM emp WHERE dept = :dept AND salary >= @min ORDER BY name")
        .await
        .expect("prepare");
    let dept = q.parameter_index(":dept").expect(":dept");
    let min = q.parameter_index("@min").expect("@min");
    assert_eq!(q.parameter_name(dept), Some(":dept"));
    q.bind_i64(dept, 4).expect("bind");
    q.bind_value(min, Value::Integer(52)).expect("bind");
    let mut got = Vec::new();
    while q.step().await.expect("step") == Step::Row {
        got.push(String::from_utf8_lossy(q.column_bytes(0).unwrap_or_default()).into_owned());
    }
    println!("dept 4, salary >= 52: {got:?}");
    assert_eq!(got, ["ivy"]);
    // An unbound parameter is NULL.
    q.reset();
    q.clear_bindings();
    assert_eq!(q.step().await.expect("step"), Step::Done);
    drop(q);
    let mut st = db
        .prepare("SELECT ?, typeof(?), ? || ?")
        .await
        .expect("prepare");
    st.bind_f64(1, 2.5).expect("bind");
    st.bind_blob(2, &[1, 2]).expect("bind");
    st.bind_text(3, "a").expect("bind");
    st.bind_null(4).expect("bind");
    assert_eq!(st.step().await.expect("step"), Step::Row);
    assert_eq!(st.column_f64(0), Some(2.5));
    assert_eq!(st.column_bytes(1), Some(&b"blob"[..]));
    assert!(st.column_is_null(2));
    drop(st);
}

async fn transactions(db: &mut Db) {
    section("Transactions and savepoints");
    exec_all(
        db,
        &[
            "BEGIN",
            "INSERT INTO dept (name) VALUES ('r&d')",
            "SAVEPOINT sp1",
            "INSERT INTO dept (name) VALUES ('temp1')",
            "SAVEPOINT sp2",
            "INSERT INTO dept (name) VALUES ('temp2')",
            "ROLLBACK TO sp1",
            "RELEASE sp1",
            "COMMIT",
        ],
    )
    .await;
    check(db, "SELECT name FROM dept WHERE id > 4", &["r&d"]).await;
    exec_all(
        db,
        &[
            "BEGIN IMMEDIATE",
            "DELETE FROM dept WHERE name = 'r&d'",
            "ROLLBACK",
        ],
    )
    .await;
    check(db, "SELECT count(*) FROM dept", &["5"]).await;
    // A failed statement inside a transaction undoes only itself.
    exec(db, "BEGIN").await;
    exec(db, "INSERT INTO dept (name) VALUES ('kept')").await;
    fails(db, "INSERT INTO dept (name) VALUES ('kept')").await;
    exec(db, "COMMIT").await;
    check(db, "SELECT count(*) FROM dept WHERE name = 'kept'", &["1"]).await;
}

async fn pragmas_and_maintenance(db: &mut Db) {
    section("PRAGMA, sqlite_schema, integrity checks, VACUUM / REINDEX / ANALYZE");
    check(
        db,
        "PRAGMA table_info(dept)",
        &["0|id|INTEGER|0||1", "1|name|TEXT|1||0"],
    )
    .await;
    check(
        db,
        "SELECT name FROM pragma_table_info('kv') ORDER BY cid",
        &["k", "v", "hits"],
    )
    .await;
    check(
        db,
        "SELECT type, name FROM sqlite_schema WHERE tbl_name = 'dept' ORDER BY name",
        &["table|dept", "index|sqlite_autoindex_dept_1"],
    )
    .await;
    check(
        db,
        "PRAGMA index_list(emp)",
        &["0|emp_name|1|c|0", "1|emp_dept|0|c|0"],
    )
    .await;
    check(db, "PRAGMA page_size", &["4096"]).await;
    check(db, "PRAGMA user_version", &["0"]).await;
    exec(db, "PRAGMA user_version = 7").await;
    check(db, "PRAGMA user_version", &["7"]).await;
    check(db, "PRAGMA integrity_check", &["ok"]).await;
    check(db, "PRAGMA quick_check", &["ok"]).await;
    exec(db, "REINDEX").await;
    exec(db, "ANALYZE").await;
    check(
        db,
        "SELECT tbl, idx FROM sqlite_stat1 WHERE tbl = 'emp' ORDER BY idx",
        &["emp|emp_dept", "emp|emp_name"],
    )
    .await;
    // Dropping a table puts its pages on the freelist; VACUUM
    // rebuilds the file without them.
    exec_all(
        db,
        &[
            "CREATE TABLE bulk AS WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)
             SELECT i, randomblob(500) AS b FROM n",
            "DROP TABLE bulk",
        ],
    )
    .await;
    let before = db.arena_stats().page_count;
    let free = rows(db, "PRAGMA freelist_count").await;
    exec(db, "VACUUM").await;
    let after = db.arena_stats().page_count;
    println!("VACUUM: {before} pages ({} free) -> {after} pages", free[0]);
    assert!(after < before);
    check(db, "PRAGMA freelist_count", &["0"]).await;
    check(db, "PRAGMA integrity_check", &["ok"]).await;
    // VACUUM INTO needs a writer (`Config::vacuum_into`); with `std`
    // the default writes a new file.
}

async fn alter(db: &mut Db) {
    section("ALTER TABLE, AUTOINCREMENT");
    exec_all(
        db,
        &[
            "ALTER TABLE loc ADD COLUMN zip TEXT DEFAULT '0000'",
            "ALTER TABLE loc RENAME COLUMN city TO town",
            "ALTER TABLE loc RENAME TO site",
        ],
    )
    .await;
    check(
        db,
        "SELECT dept, town, zip FROM site ORDER BY dept",
        &["1|oslo|0000", "2|lima|0000"],
    )
    .await;
    exec(db, "ALTER TABLE site DROP COLUMN zip").await;
    check(
        db,
        "SELECT sql FROM sqlite_schema WHERE name = 'site'",
        &["CREATE TABLE \"site\" (dept INTEGER, town TEXT)"],
    )
    .await;
    // AUTOINCREMENT never reuses a rowid, even after a delete.
    exec_all(
        db,
        &[
            "CREATE TABLE ticket (id INTEGER PRIMARY KEY AUTOINCREMENT, what TEXT)",
            "INSERT INTO ticket (what) VALUES ('a'), ('b')",
            "DELETE FROM ticket WHERE id = 2",
            "INSERT INTO ticket (what) VALUES ('c')",
        ],
    )
    .await;
    check(db, "SELECT id, what FROM ticket", &["1|a", "3|c"]).await;
    check(db, "SELECT name, seq FROM sqlite_sequence", &["ticket|3"]).await;
}

async fn temp(db: &mut Db) {
    section("TEMP tables");
    exec_all(
        db,
        &[
            "CREATE TEMP TABLE scratch (x)",
            "INSERT INTO scratch SELECT id FROM dept WHERE id <= 2",
        ],
    )
    .await;
    check(
        db,
        "SELECT x, (SELECT name FROM main.dept WHERE id = x) FROM temp.scratch",
        &["1|eng", "2|ops"],
    )
    .await;
    // The TEMP database is separate: it is not in the main file.
    assert!(db.temp_db().is_some());
    let in_main = rows(
        db,
        "SELECT count(*) FROM sqlite_schema WHERE name = 'scratch'",
    )
    .await;
    assert_eq!(in_main, ["0"]);
}
