//! v0.16 executed views and common table expressions,
//! differentially vs the pinned oracle (SQLite 3.54.1, live).
//! Probed on the pin before implementation; the matrix the tests
//! encode:
//!
//! - Views execute wherever a table is read: FROM, joins
//!   (NATURAL / USING / RIGHT / FULL), subqueries, `x IN view`,
//!   compound members, view-on-view, UPDATE ... FROM, INSERT ...
//!   SELECT, CREATE TABLE ... AS; `CREATE VIEW v(a, b)` names the
//!   columns. CREATE VIEW resolves nothing (a missing table, a
//!   column-list length mismatch or a cycle is the error of the
//!   statement that reads the view). Writing to a view, indexing
//!   it, ALTER TABLE on it and DROP TABLE of it are errors.
//! - WITH: one or many CTEs, forward references inside one
//!   clause, column lists, MATERIALIZED / NOT MATERIALIZED hints,
//!   before SELECT / VALUES / INSERT / REPLACE / UPDATE / DELETE
//!   and inside subqueries; a CTE shadows a table or view of its
//!   name (never a DML target, never a `main.`-qualified name), a
//!   view body never sees the reader's CTEs.
//! - WITH RECURSIVE: the pin's queue (FIFO; a priority queue in
//!   arrival order for ties under the CTE's ORDER BY), UNION
//!   dropping every row seen before, LIMIT / OFFSET counting the
//!   rows the queue gives up, several recursive SELECTs, the
//!   reader's LIMIT stopping an endless CTE; the pin's errors —
//!   circular reference, multiple (recursive) references,
//!   recursive aggregates, duplicate names, column counts.
//! - Ungated fixes found on the way: `q.col` / `*` over two
//!   sources named `q` are ambiguous; a derived compound's column
//!   affinity is demoted across its cores.

#![cfg(feature = "v16")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut t = hipp::arena::AVec::new();
                hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
                line.push_str(&String::from_utf8_lossy(t.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
}

/// Split a script on `;` outside quotes and parentheses-free
/// trigger bodies (the corpora hold none).
fn split(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in script.chars() {
        match q {
            Some(x) if c == x => q = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => q = Some(c),
            None if c == ';' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            None => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Run a script the way the CLI does: statements in order, every
/// row printed, stopping at the first error.
fn hipp_script(
    path: &std::path::Path,
    script: &str,
) -> (bool, Vec<String>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        for stmt in split(script) {
            let mut q = match db.prepare(&stmt).await {
                Ok(q) => q,
                Err(e) => return (false, lines, Some(e)),
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        lines.push(l);
                    }
                    Ok(Step::Done) => break,
                    Err(e) => return (false, lines, Some(e)),
                }
            }
        }
        (true, lines, None)
    })
}

fn oracle_script(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(script)
        .output()
        .expect("oracle runs");
    (
        out.status.success() && out.stderr.is_empty(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// A script on fresh twin files: same rows in the same order,
/// same success. Returns hipp's error, if both failed.
fn same(tag: &str, script: &str) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v16vc-{tag}-h"));
    let o = common::temp_db(&format!("v16vc-{tag}-o"));
    let (hok, hl, he) = hipp_script(&h, script);
    let (ook, ol) = oracle_script(&o, script);
    assert_eq!(hl, ol, "script: {script}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {script}\nhipp error: {he:?}");
    he
}

/// `same`, comparing the rows as a multiset (for joins whose row
/// order is the pin's planner's choice).
fn same_unordered(tag: &str, script: &str) {
    let h = common::temp_db(&format!("v16vcu-{tag}-h"));
    let o = common::temp_db(&format!("v16vcu-{tag}-o"));
    let (hok, mut hl, he) = hipp_script(&h, script);
    let (ook, mut ol) = oracle_script(&o, script);
    hl.sort();
    ol.sort();
    assert_eq!(hl, ol, "script: {script}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {script}\nhipp error: {he:?}");
}

/// Both refuse; hipp with schema error `kind`.
fn refuse(tag: &str, script: &str, kind: SchemaKind) {
    match same(tag, script) {
        Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "{script}"),
        other => panic!("expected {kind:?} for {script}, got {other:?}"),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const BASE: &str = "CREATE TABLE t(a INTEGER, b TEXT); \
INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, NULL), (4, 'x'), (5, 'Z'); \
CREATE TABLE u(k INTEGER PRIMARY KEY, v); \
INSERT INTO u VALUES (1, 'one'), (2, 'two'), (5, 'five'), (7, 7.5);";

// ---------------- views ----------------

#[test]
fn views_read_everywhere() {
    let cases = [
        "CREATE VIEW v AS SELECT a, b FROM t WHERE a > 1; SELECT * FROM v; SELECT v.a FROM v; SELECT w.b FROM v AS w WHERE w.a = 4",
        "CREATE VIEW v(p, q) AS SELECT a, b FROM t; SELECT * FROM v; SELECT p FROM v WHERE q = 'x'; SELECT q, count(*) FROM v GROUP BY q ORDER BY q",
        "CREATE VIEW v AS SELECT a FROM t; CREATE VIEW w AS SELECT a * 10 x FROM v; CREATE VIEW z AS SELECT x + 1 y FROM w WHERE x > 10; SELECT * FROM z",
        "CREATE VIEW v AS SELECT k AS a, v FROM u; SELECT * FROM t NATURAL JOIN v",
        "CREATE VIEW v AS SELECT k AS a, v FROM u; SELECT * FROM t LEFT JOIN v USING (a)",
        "CREATE VIEW v AS SELECT k AS a, v FROM u; SELECT * FROM v RIGHT JOIN t USING (a)",
        "CREATE VIEW v AS SELECT k AS a, v FROM u; SELECT * FROM t FULL JOIN v USING (a)",
        "CREATE VIEW v AS SELECT a FROM t WHERE a < 3; SELECT a FROM t WHERE a IN v; SELECT a FROM t WHERE a NOT IN v",
        "CREATE VIEW v AS SELECT a FROM t; SELECT * FROM v UNION ALL SELECT k FROM u; SELECT * FROM v INTERSECT SELECT k FROM u; SELECT k FROM u EXCEPT SELECT * FROM v",
        "CREATE VIEW v AS SELECT a FROM t UNION SELECT k FROM u; SELECT * FROM v; SELECT count(*) FROM v",
        "CREATE VIEW v AS SELECT b, count(*) n FROM t GROUP BY b; SELECT * FROM v ORDER BY n, b",
        "CREATE VIEW v AS SELECT a, b FROM t ORDER BY a DESC LIMIT 2; SELECT * FROM v",
        "CREATE VIEW v AS SELECT DISTINCT b FROM t; SELECT * FROM v",
        "CREATE VIEW v AS VALUES (1, 2), (3, 4); SELECT column2 FROM v WHERE column1 = 3",
        "CREATE VIEW v AS SELECT a FROM t; SELECT (SELECT max(a) FROM v), (SELECT count(*) FROM v WHERE a > t.a) FROM t",
        "CREATE VIEW v AS SELECT a FROM t; SELECT * FROM t WHERE EXISTS (SELECT 1 FROM v WHERE v.a = t.a + 1)",
        "CREATE VIEW v AS SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE k = a); SELECT * FROM v",
        "CREATE VIEW v AS SELECT name FROM sqlite_schema; SELECT * FROM v",
        "CREATE VIEW v AS SELECT a, a, b AS a FROM t; SELECT * FROM v; SELECT \"a:1\" FROM v",
        "CREATE VIEW v(c1) AS SELECT a FROM t; CREATE TABLE z AS SELECT * FROM v; SELECT sql FROM sqlite_schema WHERE name = 'z'; SELECT * FROM z",
        "CREATE VIEW v AS SELECT a + 0.5 h FROM t; SELECT typeof(h), h FROM v",
        "CREATE VIEW v AS SELECT a FROM t; SELECT * FROM v WHERE a = '1'",
        "CREATE VIEW v AS SELECT CAST(a AS TEXT) c FROM t; SELECT * FROM v WHERE c = 1",
        "CREATE VIEW v AS SELECT b COLLATE NOCASE c FROM t; SELECT * FROM v WHERE c = 'X'; SELECT DISTINCT c FROM v",
        "CREATE VIEW v AS SELECT b FROM t; SELECT count(*), group_concat(b) FROM v",
        "CREATE VIEW v AS SELECT * FROM t; CREATE TABLE t2(a); SELECT * FROM v",
        "CREATE VIEW v AS SELECT * FROM t; ALTER TABLE t ADD COLUMN c DEFAULT 9; SELECT * FROM v",
        "CREATE VIEW v AS SELECT * FROM t; DROP TABLE t; CREATE TABLE t(z, y); INSERT INTO t VALUES (5, 6); SELECT * FROM v",
        "CREATE VIEW v AS SELECT * FROM nosuch; CREATE TABLE nosuch(q); INSERT INTO nosuch VALUES (8); SELECT * FROM v",
        "CREATE VIEW v AS WITH x AS (SELECT 7 q) SELECT q FROM x; SELECT * FROM v",
        "CREATE VIEW v AS SELECT 1 x; SELECT * FROM v, (SELECT * FROM v) w",
        "CREATE VIEW v AS SELECT 1 x UNION ALL SELECT 2; SELECT * FROM v WHERE x IN (SELECT x FROM v WHERE x > 1)",
        "CREATE VIEW v AS SELECT a FROM t; DROP VIEW v; DROP VIEW IF EXISTS v; SELECT count(*) FROM sqlite_schema",
        "CREATE VIEW IF NOT EXISTS v AS SELECT 1; CREATE VIEW IF NOT EXISTS v AS SELECT 2; SELECT * FROM v",
    ];
    for (i, c) in cases.iter().enumerate() {
        assert!(
            same(&format!("vr{i}"), &format!("{BASE} {c}")).is_none(),
            "{c}"
        );
    }
}

#[test]
fn views_in_writes() {
    let cases = [
        "CREATE VIEW v AS SELECT k, v FROM u; UPDATE t SET b = (SELECT v FROM v WHERE v.k = t.a); SELECT * FROM t",
        "CREATE VIEW v AS SELECT k, v FROM u; UPDATE t SET b = v.v FROM v WHERE v.k = t.a; SELECT * FROM t",
        "CREATE VIEW v AS SELECT k, v FROM u; INSERT INTO t SELECT k, v FROM v WHERE k > 1; SELECT * FROM t",
        "CREATE VIEW v AS SELECT a, b FROM t; DELETE FROM t WHERE a IN (SELECT a FROM v WHERE b = 'x'); SELECT * FROM t",
        "CREATE VIEW v AS SELECT a FROM t; INSERT INTO u(v) SELECT a FROM v RETURNING k, v",
        "CREATE VIEW v AS SELECT a FROM t; DELETE FROM u WHERE k IN v RETURNING k",
    ];
    for (i, c) in cases.iter().enumerate() {
        assert!(
            same(&format!("vw{i}"), &format!("{BASE} {c}")).is_none(),
            "{c}"
        );
    }
}

#[test]
fn view_errors() {
    refuse(
        "ve-count",
        "CREATE TABLE t(a); CREATE VIEW v(x, y) AS SELECT a FROM t; SELECT * FROM v",
        SchemaKind::ColumnCountMismatch,
    );
    refuse(
        "ve-count2",
        "CREATE VIEW v(a) AS SELECT 1, 2; CREATE TABLE t AS SELECT * FROM v",
        SchemaKind::ColumnCountMismatch,
    );
    refuse(
        "ve-cyc",
        "CREATE VIEW v AS SELECT * FROM w; CREATE VIEW w AS SELECT * FROM v; SELECT * FROM v",
        SchemaKind::CircularView,
    );
    refuse(
        "ve-self",
        "CREATE VIEW v AS SELECT * FROM v; SELECT 1; SELECT * FROM v",
        SchemaKind::CircularView,
    );
    refuse(
        "ve-missing",
        "CREATE VIEW v AS SELECT * FROM nosuch; SELECT 1; SELECT * FROM v",
        SchemaKind::UnknownTable,
    );
    refuse(
        "ve-dropped",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t; DROP TABLE t; SELECT 1; SELECT * FROM v",
        SchemaKind::UnknownTable,
    );
    refuse(
        "ve-col",
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT nosuch FROM t; SELECT 1; SELECT * FROM v",
        SchemaKind::UnknownColumn,
    );
    for (i, w) in [
        "INSERT INTO v VALUES (1)",
        "INSERT INTO v SELECT 1",
        "UPDATE v SET a = 1",
        "DELETE FROM v",
        "DROP TABLE v",
        "DROP TABLE IF EXISTS v",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("ve-w{i}"),
            &format!("CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t; {w}"),
            SchemaKind::IsView,
        );
    }
    // Errors on both (hipp's kinds differ from the pin's texts).
    for (i, w) in [
        "CREATE INDEX i ON v(a)",
        "ALTER TABLE v ADD COLUMN b",
        "DROP VIEW v; SELECT * FROM v",
        "CREATE VIEW V AS SELECT 2",
        "CREATE TABLE v(z)",
        "DROP VIEW t",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            same(
                &format!("ve-o{i}"),
                &format!("CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t; {w}")
            )
            .is_some(),
            "{w}"
        );
    }
    // Regression (ungated): tables, views and indexes share one
    // namespace — hipp created a table over a view or an index of
    // its name, and an index over a view's.
    for (i, (w, kind)) in [
        (
            "CREATE VIEW v AS SELECT 1; CREATE TABLE v(z)",
            SchemaKind::DuplicateTable,
        ),
        (
            "CREATE VIEW v AS SELECT 1; CREATE TABLE v AS SELECT 2",
            SchemaKind::DuplicateTable,
        ),
        (
            "CREATE TABLE t(a); CREATE INDEX i ON t(a); CREATE TABLE i(z)",
            SchemaKind::DuplicateIndex,
        ),
        (
            "CREATE TABLE t(a); CREATE INDEX i ON t(a); CREATE TABLE IF NOT EXISTS i(z)",
            SchemaKind::DuplicateIndex,
        ),
        (
            "CREATE TABLE t(a); CREATE INDEX i ON t(a); CREATE TABLE IF NOT EXISTS i AS SELECT 1",
            SchemaKind::DuplicateIndex,
        ),
        (
            "CREATE TABLE t(a); CREATE VIEW v AS SELECT 1; CREATE INDEX v ON t(a)",
            SchemaKind::DuplicateTable,
        ),
        (
            "CREATE TABLE t(a); CREATE VIEW v AS SELECT 1; CREATE INDEX IF NOT EXISTS v ON t(a)",
            SchemaKind::DuplicateTable,
        ),
        (
            "CREATE TABLE t(a); CREATE INDEX IF NOT EXISTS t ON t(a)",
            SchemaKind::DuplicateTable,
        ),
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("ve-ns{i}"), w, *kind);
    }
    assert!(
        same(
            "ve-ns-ine",
            "CREATE VIEW v AS SELECT 1; CREATE TABLE IF NOT EXISTS v(z); SELECT type FROM sqlite_schema"
        )
        .is_none()
    );
    // A view's body cannot hold a parameter (the pin's "parameters
    // are not allowed in views").
    block_on(async {
        let path = common::temp_db("v16vc-ve-param");
        let mut db = open(&path).await;
        assert!(db.prepare("CREATE VIEW v AS SELECT ?").await.is_err());
    });
}

/// Views both ways between the engines: hipp reads views the
/// oracle wrote (CTE and compound bodies included), the oracle
/// reads hipp's.
#[test]
fn views_interoperate() {
    let path = common::temp_db("v16vc-interop-o");
    common::cli(
        &path,
        "CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES (1, 'p'), (2, 'q');
         CREATE VIEW ov AS SELECT a * 2 AS d, b FROM t;
         CREATE VIEW oc AS WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 4) SELECT n FROM c;
         CREATE VIEW ou(x) AS SELECT a FROM t UNION SELECT d FROM ov;",
    );
    let (ok, rows, e) = hipp_script(
        &path,
        "SELECT * FROM ov; SELECT * FROM oc; SELECT * FROM ou; SELECT x FROM ou JOIN ov ON d = x ORDER BY x",
    );
    assert!(ok, "{e:?}");
    let want: Vec<String> = common::cli(
        &path,
        "SELECT * FROM ov; SELECT * FROM oc; SELECT * FROM ou; SELECT x FROM ou JOIN ov ON d = x ORDER BY x",
    )
    .lines()
    .map(str::to_string)
    .collect();
    assert_eq!(rows, want);

    let path = common::temp_db("v16vc-interop-h");
    let (ok, _, e) = hipp_script(
        &path,
        "CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES (1, 'p'), (2, 'q');
         CREATE VIEW hv AS SELECT a, b FROM t WHERE a > 1;
         CREATE VIEW hw(z) AS WITH q AS (SELECT a FROM hv) SELECT a + 100 FROM q;",
    );
    assert!(ok, "{e:?}");
    assert_eq!(
        common::cli(&path, "SELECT * FROM hv; SELECT * FROM hw;"),
        "2|q\n102\n"
    );
    assert_eq!(common::cli(&path, "PRAGMA integrity_check;"), "ok\n");
}

#[test]
fn view_table_info() {
    let script = "CREATE TABLE t(a,b); CREATE TABLE t2(a integer, b text, c InTeGeR, d bigint, e varchar(10), f int); \
        CREATE VIEW v AS SELECT * FROM t; CREATE VIEW w AS SELECT * FROM v; \
        CREATE VIEW x AS WITH q AS (SELECT 1 z) SELECT * FROM q; \
        CREATE VIEW m(p,q,r) AS SELECT * FROM t; CREATE VIEW k AS SELECT a, a, b AS a FROM t; \
        CREATE VIEW m3 AS SELECT a,b,c,d,e,f FROM t2; CREATE VIEW m4(p,q,r,s,t,u) AS SELECT a,b,c,d,e,f FROM t2; \
        CREATE VIEW m5(p) AS SELECT * FROM t2; \
        CREATE VIEW r AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<3) SELECT * FROM c; \
        CREATE VIEW dd AS SELECT * FROM (SELECT a FROM t2) NATURAL JOIN (SELECT 2 a); \
        CREATE VIEW cu AS SELECT b FROM t2 UNION ALL SELECT 5; \
        PRAGMA table_info(v); PRAGMA table_info(w); PRAGMA table_info(x); PRAGMA table_info(m); \
        PRAGMA table_info(k); PRAGMA table_info(m3); PRAGMA table_info(m4); PRAGMA table_info(m5); \
        PRAGMA table_info(r); PRAGMA table_info(dd); PRAGMA table_info(cu); PRAGMA table_info(t2)";
    assert!(same("vti", script).is_none());
    refuse(
        "vti-missing",
        "CREATE VIEW y AS SELECT * FROM nosuch; PRAGMA table_info(y)",
        SchemaKind::UnknownTable,
    );
}

// ---------------- non-recursive CTEs ----------------

#[test]
fn ctes_select() {
    let cases = [
        "WITH x AS (SELECT 1 a), y AS (SELECT a + 1 b FROM x) SELECT * FROM x, y",
        "WITH x(a, b) AS (SELECT 1, 2) SELECT * FROM x",
        "WITH x(a) AS (SELECT 1, 2) SELECT 5",
        "WITH x AS MATERIALIZED (SELECT 1 a), y AS NOT MATERIALIZED (SELECT 2 b) SELECT * FROM x, y",
        "WITH x AS (SELECT 1 a) SELECT * FROM (WITH y AS (SELECT 2 b) SELECT * FROM y, x)",
        "WITH t AS (SELECT 10 a) SELECT * FROM t",
        "WITH t AS (SELECT 99) SELECT * FROM main.t",
        "CREATE VIEW v AS SELECT * FROM t; WITH t AS (SELECT 1 a, 2 b) SELECT * FROM v",
        "CREATE VIEW v AS SELECT 5; WITH v AS (SELECT 9) SELECT * FROM v",
        "CREATE VIEW v AS WITH x AS (SELECT 7 q) SELECT q FROM x; WITH x AS (SELECT 8 q) SELECT * FROM v, x",
        "WITH x AS (SELECT * FROM y), y AS (SELECT 1 a) SELECT * FROM x",
        "WITH A AS (SELECT 1 x) SELECT * FROM a; WITH a AS (SELECT 1 x) SELECT a.x, A.x FROM A",
        "WITH x AS (SELECT 1 a) SELECT * FROM x WHERE a IN x",
        "SELECT (WITH a AS (SELECT 7) SELECT * FROM a); SELECT 1 WHERE 7 IN (WITH a AS (SELECT 7) SELECT * FROM a); SELECT EXISTS (WITH a AS (SELECT 7) SELECT * FROM a)",
        "WITH x AS (SELECT a FROM t) SELECT * FROM x UNION ALL SELECT * FROM x",
        "WITH x AS (SELECT 1 a) VALUES (1); WITH x AS (SELECT 1 a) SELECT 1 UNION SELECT * FROM x",
        "WITH x AS (SELECT 1 a) SELECT * FROM x AS y, x AS z",
        "SELECT * FROM (WITH x AS (SELECT a FROM t) SELECT * FROM x WHERE a > 2)",
        "WITH x AS (SELECT a FROM t), y AS (SELECT a FROM x WHERE a > 1), z AS (SELECT count(*) c FROM y) SELECT * FROM z, y",
        "WITH x(a) AS (SELECT k FROM u) SELECT * FROM t NATURAL JOIN x",
        "WITH x AS (SELECT k AS a, v FROM u) SELECT * FROM t JOIN x USING (a)",
        "WITH x AS (SELECT a FROM t) SELECT * FROM x WHERE a IN (SELECT * FROM x WHERE a > 2)",
        "WITH x AS (SELECT a FROM t LIMIT 2) SELECT * FROM x",
        "WITH x AS (SELECT a FROM t ORDER BY a DESC) SELECT * FROM x",
        "WITH x AS (SELECT 1) SELECT * FROM x, (WITH x AS (SELECT 2) SELECT * FROM x)",
        "WITH x(a) AS (SELECT 1) SELECT x.a, a FROM x",
        "WITH x(p, q) AS (SELECT 1, 2) SELECT * FROM x; CREATE TABLE z AS WITH x(p, q) AS (SELECT 1, 2) SELECT * FROM x; SELECT sql FROM sqlite_schema WHERE name = 'z'",
        "WITH x(k) AS (SELECT a FROM t) SELECT * FROM x WHERE k > 1",
        "WITH x AS (SELECT b FROM t UNION ALL SELECT 5) SELECT b, b = 5, b = '5' FROM x",
    ];
    for (i, c) in cases.iter().enumerate() {
        assert!(
            same(&format!("cs{i}"), &format!("{BASE} {c}")).is_none(),
            "{c}"
        );
    }
}

#[test]
fn ctes_before_writes() {
    let cases = [
        "WITH x AS (SELECT 5) INSERT INTO t(a) SELECT * FROM x; SELECT * FROM t",
        "INSERT INTO t(a) WITH x AS (SELECT 9) SELECT * FROM x; SELECT * FROM t",
        "WITH x AS (SELECT 2) INSERT INTO t(a) VALUES ((SELECT * FROM x)); SELECT * FROM t",
        "WITH x AS (SELECT 2) REPLACE INTO u VALUES (2, (SELECT * FROM x)); SELECT * FROM u",
        "WITH t AS (SELECT 99 a) INSERT INTO t(a) SELECT a + 1 FROM t; SELECT * FROM t",
        "WITH x AS (SELECT 2 a) DELETE FROM t WHERE a IN x; SELECT * FROM t",
        "WITH t AS (SELECT 99 a) DELETE FROM t; SELECT count(*) FROM t",
        "WITH x AS (SELECT 2 a) UPDATE t SET a = (SELECT a * 7 FROM x) WHERE a = 1; SELECT * FROM t",
        "WITH t AS (SELECT 99 a) UPDATE t SET a = 5; SELECT * FROM t",
        "WITH x AS (SELECT k, v FROM u) UPDATE t SET b = x.v FROM x WHERE x.k = t.a; SELECT * FROM t",
        "WITH x AS (SELECT k FROM u) INSERT INTO t(a) SELECT k + 100 FROM x RETURNING a, (SELECT count(*) FROM x)",
        "WITH x AS (SELECT 2 k) INSERT INTO u VALUES (2, 'dup') ON CONFLICT(k) DO UPDATE SET v = (SELECT 'k' || k FROM x); SELECT * FROM u",
        "WITH x AS (SELECT 1 k) DELETE FROM u WHERE k IN x RETURNING k, (SELECT count(*) FROM x)",
        "WITH RECURSIVE x(y) AS (SELECT 1 UNION ALL SELECT y + 1 FROM x WHERE y < 3) UPDATE t SET b = 'r' WHERE a IN x; SELECT * FROM t",
        "WITH RECURSIVE x(y) AS (SELECT 1 UNION ALL SELECT y + 1 FROM x WHERE y < 3) DELETE FROM t WHERE a NOT IN x; SELECT * FROM t",
        "WITH RECURSIVE x(y) AS (SELECT 1 UNION ALL SELECT y + 1 FROM x WHERE y < 10) INSERT INTO t(a) SELECT y FROM x WHERE y > 8; SELECT * FROM t",
    ];
    for (i, c) in cases.iter().enumerate() {
        let h = common::temp_db(&format!("v16vc-cw{i}-h"));
        let o = common::temp_db(&format!("v16vc-cw{i}-o"));
        let script = format!("{BASE} {c}");
        let (hok, hl, he) = hipp_script(&h, &script);
        let (ook, ol) = oracle_script(&o, &script);
        assert_eq!(hl, ol, "{c}: {he:?}");
        assert!(hok && ook, "{c}: {he:?}");
        // The file hipp wrote reads the same on the pin.
        assert_eq!(
            common::cli(
                &h,
                "SELECT * FROM t; SELECT * FROM u; PRAGMA integrity_check;"
            ),
            common::cli(
                &o,
                "SELECT * FROM t; SELECT * FROM u; PRAGMA integrity_check;"
            ),
            "{c}"
        );
    }
}

#[test]
fn cte_errors() {
    refuse(
        "ce-count",
        "WITH x(a, b) AS (SELECT 1) SELECT * FROM x",
        SchemaKind::ColumnCountMismatch,
    );
    refuse(
        "ce-dup",
        "WITH x AS (SELECT 1 a), x AS (SELECT 2 a) SELECT * FROM x",
        SchemaKind::DuplicateCteName,
    );
    refuse(
        "ce-dup-unused",
        "WITH x AS (SELECT 1), X AS (SELECT 2) SELECT 3",
        SchemaKind::DuplicateCteName,
    );
    for (i, s) in [
        "WITH x AS (SELECT * FROM x) SELECT * FROM x",
        "WITH x AS (SELECT * FROM y), y AS (SELECT * FROM x) SELECT * FROM x",
        "WITH x AS (SELECT 1 y) SELECT (WITH x AS (SELECT y + 1 z FROM x) SELECT z FROM x)",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3 UNION ALL SELECT 9) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 INTERSECT SELECT n + 1 FROM c WHERE n < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 EXCEPT SELECT n + 1 FROM c WHERE n < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT n FROM c) SELECT * FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT (SELECT x FROM c)) SELECT * FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM (SELECT * FROM c) WHERE x < 3) SELECT * FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT 2 WHERE EXISTS (SELECT 1 FROM c)) SELECT * FROM c",
        "WITH RECURSIVE a(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM b WHERE x < 3), b(x) AS (SELECT x FROM a) SELECT * FROM a",
        "WITH RECURSIVE c(x) AS (SELECT x FROM c UNION ALL SELECT x + 1 FROM c) SELECT * FROM c",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("ce-c{i}"), s, SchemaKind::CircularReference);
    }
    for (i, s) in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT c.n + 1 FROM c, c c2 WHERE c.n < 3) SELECT * FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c JOIN c d WHERE x < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < (SELECT count(*) FROM c)) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n IN (SELECT n FROM c) AND n < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3 LIMIT (SELECT count(*) FROM c)) SELECT * FROM c",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("ce-m{i}"), s, SchemaKind::RecursiveReferences);
    }
    for (i, s) in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT max(n) + 1 FROM c WHERE n < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3 GROUP BY n) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT count(*) FROM c) SELECT * FROM c",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("ce-a{i}"), s, SchemaKind::RecursiveAggregate);
    }
    // Errors on both, other kinds: the CTE's ORDER BY must name a
    // result column, a recursive CTE has no rowid, a bad LIMIT.
    for (i, s) in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c ORDER BY n LIMIT 4) SELECT * FROM c",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3 ORDER BY 2) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT rowid FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3 LIMIT 'a') SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM main.s WHERE x < 3) SELECT * FROM s",
        "WITH c(x) AS (SELECT 1) SELECT * FROM c, c",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("ce-o{i}"), s).is_some(), "{s}");
    }
    // Syntax errors on both.
    for (i, s) in [
        "WITH SELECT 1",
        "WITH x AS SELECT 1 SELECT 1",
        "WITH x AS (SELECT 1) WITH y AS (SELECT 2) SELECT 1",
        "SELECT 1 UNION WITH x AS (SELECT 2) SELECT * FROM x",
        "WITH x AS (SELECT 1) CREATE TABLE q(a)",
        "WITH x AS (SELECT 1) PRAGMA user_version",
        "WITH x() AS (SELECT 1) SELECT 1",
        "WITH x AS MATERIALIZED SELECT 1",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            matches!(same(&format!("ce-s{i}"), s), Some(Error::Parse { .. })),
            "{s}"
        );
    }
}

// ---------------- recursive CTEs ----------------

#[test]
fn recursive_queue_semantics() {
    let cases = [
        "WITH c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION SELECT n % 3 FROM c) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c LIMIT 4 OFFSET 2) SELECT * FROM c",
        "WITH RECURSIVE c(n, d) AS (SELECT 1, 0 UNION ALL SELECT n * 2, d + 1 FROM c WHERE d < 3 UNION ALL SELECT n * 2 + 1, d + 1 FROM c WHERE d < 3) SELECT * FROM c",
        "WITH RECURSIVE c(n, d) AS (SELECT 1, 0 UNION ALL SELECT n * 2, d + 1 FROM c WHERE d < 2 UNION ALL SELECT n * 2 + 1, d + 1 FROM c WHERE d < 2 ORDER BY 1 DESC) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 5 UNION ALL SELECT 3 UNION ALL SELECT n - 1 FROM c WHERE n > 0 ORDER BY 1) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 5 UNION ALL SELECT 3 UNION ALL SELECT n - 1 FROM c WHERE n > 0 ORDER BY 1 DESC) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 5 UNION ALL SELECT 3 UNION ALL SELECT n - 1 FROM c WHERE n > 0) SELECT * FROM c",
        "WITH RECURSIVE s(x, tag) AS (SELECT 3, 'a' UNION ALL SELECT 3, 'b' UNION ALL SELECT 3, 'c' UNION ALL SELECT x - 1, tag FROM s WHERE x > 1 ORDER BY 1 DESC) SELECT * FROM s",
        "WITH RECURSIVE s(x, tag) AS (SELECT 3, 'a' UNION ALL SELECT 3, 'b' UNION ALL SELECT 3, 'c' UNION ALL SELECT x - 1, tag FROM s WHERE x > 1 ORDER BY 1) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 3 UNION ALL SELECT NULL UNION ALL SELECT x - 1 FROM s WHERE x > 0 ORDER BY 1 NULLS LAST) SELECT * FROM s",
        "WITH RECURSIVE s(x, y) AS (SELECT 1, 'b' UNION ALL SELECT x + 1, char(98 - x) FROM s WHERE x < 4 ORDER BY 2) SELECT * FROM s",
        "WITH RECURSIVE s(x, y) AS (SELECT 1 a, 'b' b UNION ALL SELECT x + 1, char(98 + x) FROM s WHERE x < 4 ORDER BY b DESC) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION SELECT x + 1 FROM s WHERE x < 3 ORDER BY 1 DESC LIMIT 2) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 5 LIMIT 0) SELECT count(*) FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 5 LIMIT -5) SELECT count(*) FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 5 LIMIT 2, 2) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 5 LIMIT (SELECT 2)) SELECT * FROM s",
        "WITH RECURSIVE c AS (SELECT 1 UNION ALL SELECT 1 + 1 FROM c LIMIT 3) SELECT * FROM c",
        "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c LIMIT 3) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT * FROM c LIMIT 3",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT n * 2 FROM c LIMIT 2 OFFSET 5",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT (SELECT n FROM c LIMIT 1 OFFSET 6)",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION SELECT 1.0 UNION SELECT x + 1 FROM s WHERE x < 3) SELECT x, typeof(x) FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION SELECT x + 0.0 FROM s WHERE x < 3) SELECT x, typeof(x) FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 'a' UNION SELECT upper(x) FROM s UNION SELECT lower(x) FROM s) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 'a' COLLATE NOCASE UNION SELECT upper(x) FROM s) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT 1 UNION SELECT x + 1 FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 2 UNION SELECT 1 UNION SELECT x + 1 FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION SELECT x + 1 FROM s WHERE x < 3 UNION SELECT x FROM s) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 INTERSECT SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 EXCEPT SELECT 2 UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (VALUES (1) UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (VALUES (1), (10) UNION ALL SELECT x + 1 FROM s WHERE x % 10 < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT DISTINCT x + 1 FROM s, (SELECT 1 UNION ALL SELECT 2) WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s s WHERE s.x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT y.x + 1 FROM s AS y WHERE y.x < 3) SELECT s.x FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < (SELECT 3)) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT (SELECT x + 1) FROM s WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s LEFT JOIN (SELECT 1) ON 1 WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM (SELECT 1) LEFT JOIN s ON 1 WHERE x < 3) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s, (SELECT 2 k) WHERE x < k) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT NULL UNION ALL SELECT 1 FROM s WHERE x IS NULL) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT 'a' UNION ALL SELECT x || 'a' FROM s WHERE length(x) < 4) SELECT * FROM s",
        "WITH RECURSIVE s AS MATERIALIZED (SELECT 1 n UNION ALL SELECT n + 1 FROM s WHERE n < 3) SELECT * FROM s",
        "WITH RECURSIVE f(n, v) AS (SELECT 1, 1 UNION ALL SELECT n + 1, v * (n + 1) FROM f WHERE n < 20) SELECT * FROM f",
        "WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a + b FROM fib WHERE b < 1000) SELECT a FROM fib",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 2000) SELECT count(*), sum(n), min(n), max(n) FROM c",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT x, (SELECT count(*) FROM s WHERE x <= s1.x) FROM s s1",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3), s2(y) AS (SELECT x * 2 FROM s UNION ALL SELECT y + 1 FROM s2 WHERE y < 3) SELECT * FROM s2",
        "WITH RECURSIVE o(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM o WHERE i < 3) SELECT i, (WITH RECURSIVE n(j) AS (SELECT 1 UNION ALL SELECT j + 1 FROM n WHERE j < o.i) SELECT group_concat(j) FROM n) FROM o",
        "WITH RECURSIVE o(i) AS (SELECT 1 UNION ALL SELECT i + (SELECT count(*) FROM (WITH RECURSIVE n(j) AS (SELECT 1 UNION ALL SELECT j + 1 FROM n WHERE j < o.i) SELECT * FROM n)) FROM o WHERE i < 20) SELECT * FROM o",
        "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 3) VALUES ((SELECT count(*) FROM s))",
    ];
    for (i, c) in cases.iter().enumerate() {
        assert!(same(&format!("rq{i}"), c).is_none(), "{c}");
    }
    // Over tables, and correlated to an outer row.
    let tabled = [
        "WITH RECURSIVE x(y) AS (SELECT 1 UNION ALL SELECT y + 1 FROM x WHERE y < 3) SELECT * FROM t WHERE a IN x",
        "WITH RECURSIVE x(y) AS (SELECT a FROM t WHERE a = 1 UNION ALL SELECT t.a FROM t, x WHERE t.a = x.y + 1) SELECT * FROM x",
        "WITH RECURSIVE x(y, path) AS (SELECT k, v FROM u WHERE k = 1 UNION ALL SELECT u.k, x.path || '>' || u.v FROM u JOIN x ON u.k = x.y * 2 WHERE x.y < 10) SELECT * FROM x",
        "WITH RECURSIVE s(x) AS (SELECT k FROM u UNION ALL SELECT x + 10 FROM s WHERE x < 20) SELECT * FROM s",
        "WITH RECURSIVE s(x) AS (SELECT k FROM u WHERE k > 100 UNION ALL SELECT x + 1 FROM s) SELECT * FROM s",
        "SELECT a, (WITH RECURSIVE c(n) AS (SELECT t.a UNION ALL SELECT n + 1 FROM c WHERE n < t.a + 2) SELECT group_concat(n) FROM c) FROM t",
        "SELECT a, (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < t.a) SELECT group_concat(n) FROM c) FROM t",
        "CREATE TABLE tt(a TEXT); INSERT INTO tt VALUES ('1'), ('2'); WITH RECURSIVE s(x) AS (SELECT a FROM tt UNION ALL SELECT x + 1 FROM s WHERE x < 3) SELECT x, typeof(x), x = 2, x = '2' FROM s",
        "WITH RECURSIVE s(x) AS (SELECT a FROM t UNION ALL SELECT x || '' FROM s WHERE length(x) < 3 AND x < 2) SELECT x, typeof(x), x = '1' FROM s",
        "CREATE VIEW r AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3) SELECT * FROM c; SELECT * FROM r; SELECT * FROM r JOIN t ON t.a = r.x",
    ];
    for (i, c) in tabled.iter().enumerate() {
        assert!(
            same(&format!("rt{i}"), &format!("{BASE} {c}")).is_none(),
            "{c}"
        );
    }
    // Joins of a recursive CTE with itself: order is the pin's
    // planner's choice, so compared as multisets.
    for (i, c) in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT * FROM c AS a, c AS b",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3) SELECT * FROM c a JOIN c b USING (x)",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3) SELECT * FROM c NATURAL JOIN c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 300) SELECT count(*) FROM c a, c b WHERE a.n = b.n",
    ]
    .iter()
    .enumerate()
    {
        same_unordered(&format!("rj{i}"), c);
    }
}

/// A generated corpus of recursive CTEs: setups of literal rows,
/// one or two recursive SELECTs with bounded depth, UNION or
/// UNION ALL, an optional queue ORDER BY, LIMIT and OFFSET, read
/// back in queue order (no outer ORDER BY: the queue's order is
/// the pin's defined output order).
#[test]
fn recursive_generated_corpus() {
    let mut rng = Rng(0x5eed_0016_c7e0);
    let steps = [
        "SELECT x + 1, d + 1 FROM r WHERE d < 3",
        "SELECT x * 2, d + 1 FROM r WHERE d < 3",
        "SELECT x - 3, d + 1 FROM r WHERE d < 2",
        "SELECT x % 4, d + 1 FROM r WHERE d < 4",
        "SELECT -x, d + 1 FROM r WHERE d < 2 AND x > 0",
        "SELECT x + 0.5, d + 1 FROM r WHERE d < 2",
        "SELECT CASE WHEN x > 2 THEN NULL ELSE x + 2 END, d + 1 FROM r WHERE d < 3",
        "SELECT x, d + 1 FROM r WHERE d < 3 AND x IS NOT NULL",
    ];
    let setups = ["1", "0", "5", "2", "-1", "NULL", "'a'", "2.5"];
    let orders = [
        "",
        "",
        " ORDER BY 1",
        " ORDER BY 1 DESC",
        " ORDER BY 2 DESC, 1",
        " ORDER BY 1 NULLS LAST",
        " ORDER BY 2",
    ];
    let limits = [
        "",
        "",
        " LIMIT 5",
        " LIMIT 3 OFFSET 2",
        " LIMIT -1 OFFSET 4",
        " LIMIT 0",
    ];
    let n: usize = std::env::var("HIPP_CTE_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let mut script = String::new();
    for i in 0..n {
        let op = if rng.below(2) == 0 {
            "UNION"
        } else {
            "UNION ALL"
        };
        let mut s = format!("WITH RECURSIVE r(x, d) AS (SELECT {}, 0", rng.pick(&setups));
        for _ in 0..rng.below(3) {
            s.push_str(&format!(" UNION ALL SELECT {}, 0", rng.pick(&setups)));
        }
        let nsteps = 1 + rng.below(2);
        for _ in 0..nsteps {
            s.push_str(&format!(" {op} {}", rng.pick(&steps)));
        }
        s.push_str(rng.pick(&orders));
        s.push_str(rng.pick(&limits));
        s.push_str(") SELECT x, d FROM r;");
        script.push_str(&s);
        // Keep each script a few statements long, so a failure
        // names a small one.
        if i % 6 == 5 || i + 1 == n {
            assert!(same(&format!("rg{i}"), &script).is_none(), "{script}");
            script.clear();
        }
    }
}

/// A generated corpus of views and CTEs over a random table: one
/// layer of filtering / projection, read through joins, IN,
/// compounds and aggregates.
#[test]
fn generated_view_cte_corpus() {
    let mut rng = Rng(0x5eed_0016_71e3);
    let mut base = String::from("CREATE TABLE g(a INTEGER, b TEXT, c REAL);");
    for _ in 0..30 {
        let a = rng.below(12) as i64 - 3;
        let b = ["'p'", "'q'", "'P'", "NULL", "'10'", "'r'"][rng.below(6)];
        let c = ["1.5", "NULL", "-2.0", "3", "0.25"][rng.below(5)];
        base.push_str(&format!("INSERT INTO g VALUES ({a}, {b}, {c});"));
    }
    let bodies = [
        "SELECT a, b FROM g WHERE a > 2",
        "SELECT a * 2 AS a, upper(b) AS b FROM g",
        "SELECT a, b FROM g WHERE b IS NOT NULL ORDER BY a LIMIT 7",
        "SELECT DISTINCT a % 3 AS a, b FROM g",
        "SELECT a, max(b) AS b FROM g GROUP BY a",
        "SELECT a, b FROM g UNION SELECT 100, 'z'",
        "SELECT a, b FROM g WHERE c > 0 UNION ALL SELECT a, b || '!' FROM g WHERE c < 0",
        "SELECT CAST(a AS TEXT) AS a, b FROM g",
    ];
    let readers = [
        "SELECT * FROM {v} ORDER BY 1, 2",
        "SELECT count(*), sum(a), group_concat(b) FROM (SELECT * FROM {v} ORDER BY a, b)",
        "SELECT g.a, {v}.b FROM g JOIN {v} ON g.a = {v}.a ORDER BY 1, 2, g.c",
        "SELECT * FROM g WHERE a IN (SELECT a FROM {v}) ORDER BY a, b, c",
        "SELECT a FROM {v} WHERE a = '4' ORDER BY a",
        "SELECT b, count(*) FROM {v} GROUP BY b ORDER BY b",
        "SELECT * FROM {v} NATURAL JOIN (SELECT a, b FROM g) ORDER BY 1, 2",
        "SELECT a FROM {v} INTERSECT SELECT a FROM g ORDER BY 1",
        "SELECT * FROM {v} LEFT JOIN g USING (a) ORDER BY 1, 2, 3, 4",
    ];
    for i in 0..40 {
        let body = rng.pick(&bodies);
        let mut reader = rng.pick(&readers);
        // A WHERE comparison over a compound whose cores differ in
        // affinity is planner-dependent on the pin (it pushes some
        // terms into each core, which then compare with that
        // core's affinity): not generated (see STATUS.md).
        while body.contains("UNION") && reader.contains("'4'") {
            reader = rng.pick(&readers);
        }
        let as_view = format!(
            "{base} CREATE VIEW vv AS {body}; {};",
            reader.replace("{v}", "vv")
        );
        assert!(same(&format!("gv{i}"), &as_view).is_none(), "{as_view}");
        let as_cte = format!(
            "{base} WITH cc AS ({body}) {};",
            reader.replace("{v}", "cc")
        );
        assert!(same(&format!("gc{i}"), &as_cte).is_none(), "{as_cte}");
    }
}

/// Bound parameters inside CTE bodies (and a WITH before a write)
/// bind like any other.
#[cfg(feature = "params")]
#[test]
fn parameters_in_ctes() {
    let path = common::temp_db("v16vc-params");
    block_on(async {
        let mut db = open(&path).await;
        let mut st = db
            .prepare("WITH RECURSIVE c(n) AS (SELECT ?1 UNION ALL SELECT n + 1 FROM c WHERE n < ?2) SELECT group_concat(n) FROM c")
            .await
            .expect("prepare");
        st.bind_i64(1, 3).expect("bind");
        st.bind_i64(2, 6).expect("bind");
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert_eq!(st.column_bytes(0), Some(&b"3,4,5,6"[..]));
        drop(st);
        let mut st = db.prepare("CREATE TABLE t(a)").await.expect("prepare");
        st.step().await.expect("step");
        drop(st);
        let mut st = db
            .prepare("WITH x(v) AS (SELECT :v) INSERT INTO t SELECT v * 2 FROM x")
            .await
            .expect("prepare");
        st.bind_i64(1, 21).expect("bind");
        assert_eq!(st.step().await.expect("step"), Step::Done);
        drop(st);
    });
    assert_eq!(common::cli(&path, "SELECT * FROM t;"), "42\n");
}

// ---------------- ungated fixes found on the way ----------------

/// Regression: `q.col` and `*` over two sources named `q` read the
/// first one where the pin reports an ambiguous column.
#[test]
fn same_named_sources_are_ambiguous() {
    for (i, s) in [
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT t.a FROM t, t",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT t.* FROM t, t",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT * FROM t, t",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT * FROM t, t AS T",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT * FROM t LEFT JOIN t ON 1",
        "CREATE TABLE t(a, b); CREATE TABLE u(a, c); SELECT * FROM t, u t",
        "CREATE TABLE t(a, b); SELECT * FROM t JOIN t USING (a)",
        "CREATE TABLE t(a, b); SELECT t.b FROM t JOIN t USING (a)",
        "SELECT * FROM (SELECT 1 x) q, (SELECT 2 x) q",
        "CREATE VIEW v AS SELECT 1 x; SELECT * FROM v, v",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("amb{i}"), s, SchemaKind::AmbiguousColumn);
    }
    for (i, s) in [
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT 1 FROM t, t",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT count(*) FROM t, t",
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES (1); INSERT INTO u VALUES (2); SELECT * FROM t, u t; SELECT t.* FROM t, u t",
        "CREATE TABLE t(a, b); CREATE TABLE u(a, c); INSERT INTO t VALUES (1, 2); INSERT INTO u VALUES (1, 3); SELECT t.c FROM t, u t",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2); SELECT t.a FROM t JOIN t USING (a); SELECT * FROM t JOIN t USING (a, b)",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT * FROM t NATURAL JOIN t",
        "SELECT * FROM (SELECT 1 x), (SELECT 2 x)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("namb{i}"), s).is_none(), "{s}");
    }
}

/// Regression: a derived compound's column took its leftmost
/// core's affinity; the pin demotes it across the cores (TEXT with
/// a numeric core, numeric with a text one, become none).
#[test]
fn derived_compound_affinity_is_demoted() {
    for (i, s) in [
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES ('1'), ('2'); SELECT x, x = 2, x = 5 FROM (SELECT a x FROM t UNION ALL SELECT 5)",
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES ('1'), ('2'); SELECT x, x = 2, x = '5' FROM (SELECT a x FROM t UNION ALL SELECT '5')",
        "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2); SELECT x, x = '1', x = 5 FROM (SELECT a x FROM t UNION ALL SELECT 'q')",
        "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2); SELECT x, x = '1' FROM (SELECT CAST(a AS INTEGER) x FROM t UNION ALL SELECT 3)",
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES ('1'), ('2'); SELECT x FROM (SELECT a x FROM t UNION SELECT 5) WHERE x = 2",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("aff{i}"), s).is_none(), "{s}");
    }
}

/// Regression (PRAGMA unit): a declared type that is one of the
/// pin's standard names prints in its spelling.
#[test]
fn table_info_standard_type_names() {
    assert!(
        same(
            "stdty",
            "CREATE TABLE x(a any, b blob, c real, d Text, e numeric, f InT, g integer, h varchar(3)); \
             CREATE VIEW v AS SELECT * FROM x; PRAGMA table_info(x); PRAGMA table_info(v)"
        )
        .is_none()
    );
}

/// A view's unaliased column references are named by their tables'
/// own columns (a rowid by the INTEGER PRIMARY KEY); a FROM
/// subquery's and a CTE's keep the spelling (the pin resolves only
/// a view's SELECT before naming its columns).
#[test]
fn view_and_cte_column_naming() {
    let t = "CREATE TABLE t(a, e INTEGER PRIMARY KEY); INSERT INTO t VALUES (1, 5);";
    for (i, s) in [
        "CREATE VIEW v AS SELECT rowid, A FROM t; SELECT e, a FROM v; PRAGMA table_info(v)",
        "CREATE VIEW v AS SELECT rowid FROM t; SELECT * FROM v NATURAL JOIN (SELECT 5 e)",
        "CREATE VIEW v AS SELECT A FROM t; CREATE TABLE z AS SELECT * FROM v; SELECT sql FROM sqlite_schema WHERE name = 'z'",
        "CREATE TABLE z AS SELECT * FROM (SELECT A FROM t); SELECT sql FROM sqlite_schema WHERE name = 'z'",
        "CREATE TABLE z AS WITH c AS (SELECT A FROM t) SELECT * FROM c; SELECT sql FROM sqlite_schema WHERE name = 'z'",
        "SELECT rowid FROM (SELECT rowid FROM t)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("nm{i}"), &format!("{t} {s}")).is_none(), "{s}");
    }
    for (i, s) in [
        "SELECT e FROM (SELECT rowid FROM t)",
        "WITH RECURSIVE c AS (SELECT rowid FROM t UNION ALL SELECT e + 1 FROM c WHERE e < 7) SELECT e FROM c",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("nme{i}"), &format!("{t} {s}"), SchemaKind::UnknownColumn);
    }
}

// ---------------- review fixes ----------------

/// Regression: every reference to a CTE pasted a fresh copy of its
/// body, so a body calling random() gave each reference its own
/// rows. The pin materializes a CTE once and shares it when it is
/// MATERIALIZED, or has no hint and is read more than once without
/// being flattened (no FROM clause, an aggregate, DISTINCT, a LIMIT
/// under a join or a bounded reader, a recursive CTE); a view named
/// twice in one FROM clause shares one materialization too
/// (`isSelfJoinView`), and a flattened reference is computed for
/// its reader alone. Each case prints only equalities and counts,
/// which are the same on every run.
#[test]
fn shared_materializations() {
    let t = "CREATE TABLE t(a); INSERT INTO t VALUES (1), (2), (3); \
             CREATE TABLE u(b); INSERT INTO u VALUES (5), (6);";
    for (i, s) in [
        // The review's cases.
        "WITH c AS (SELECT abs(random()) % 100000 r) SELECT count(*) FROM c WHERE r IN (SELECT r FROM c)",
        "WITH c AS MATERIALIZED (SELECT random() r) SELECT a.r = b.r FROM c a, c b",
        "WITH c AS MATERIALIZED (SELECT random() r) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a)) FROM t",
        "WITH RECURSIVE c(x) AS (SELECT abs(random()) % 1000 UNION ALL SELECT x FROM c LIMIT 2) SELECT a.x = b.x FROM c a, c b LIMIT 1",
        "CREATE VIEW v AS SELECT random() r; SELECT a.r = b.r FROM v a, v b",
        // CTEs read more than once and not flattened share.
        "WITH c AS (SELECT random() r) SELECT (SELECT r FROM c) = (SELECT r FROM c)",
        "WITH c AS (SELECT random() r) SELECT r = (SELECT r FROM c) FROM c",
        "WITH c AS (SELECT random() r) SELECT count(*) FROM c x, c y, c z WHERE x.r = y.r AND y.r = z.r",
        "WITH c AS (SELECT random() r), d AS (SELECT r FROM c) SELECT (SELECT r FROM d) = (SELECT r FROM d)",
        "WITH c AS (SELECT random() r FROM t LIMIT 1) SELECT (SELECT r FROM c LIMIT 1) = (SELECT r FROM c LIMIT 1)",
        "WITH c AS (SELECT random() r FROM t LIMIT 1) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a LIMIT 1)), (SELECT r FROM c) = (SELECT r FROM c) FROM t",
        "WITH c AS (SELECT random() r) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a)), (SELECT r FROM c) = (SELECT r FROM c) FROM t",
        "WITH c AS (SELECT random() r) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a)) FROM t, c",
        "WITH c AS MATERIALIZED (SELECT a, random() r FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS MATERIALIZED (SELECT a, random() r FROM t) SELECT count(*) FROM c x WHERE x.r IN (SELECT r FROM c)",
        "WITH c AS (SELECT a, random() r FROM t ORDER BY 1 LIMIT 10) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS (SELECT count(*), random() r FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS (SELECT random() r FROM t UNION ALL SELECT 5) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS (SELECT DISTINCT a, random() r FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS (SELECT a, random() r FROM t GROUP BY a) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH RECURSIVE c(x) AS (SELECT abs(random()) % 1000 UNION ALL SELECT x FROM c LIMIT 2) SELECT (SELECT x FROM c) = (SELECT x FROM c)",
        "WITH RECURSIVE c(x) AS MATERIALIZED (SELECT abs(random()) % 1000 UNION ALL SELECT x FROM c LIMIT 2) SELECT count(DISTINCT (SELECT x FROM c WHERE t.a = t.a)) FROM t",
        // A flattened, single, or NOT MATERIALIZED reference is
        // computed for its reader.
        "WITH c AS (SELECT a, random() r FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS (SELECT a, random() r FROM t) SELECT count(*) FROM c x WHERE x.r IN (SELECT r FROM c)",
        "WITH c AS (SELECT a, random() r FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r AND x.a > 1",
        "WITH c AS (SELECT a, random() r FROM t UNION ALL SELECT a, random() FROM t) SELECT count(*) FROM c x, c y WHERE x.r = y.r",
        "WITH c AS NOT MATERIALIZED (SELECT random() r) SELECT (SELECT r FROM c) = (SELECT r FROM c)",
        "WITH c AS NOT MATERIALIZED (SELECT random() r FROM t LIMIT 1) SELECT (SELECT r FROM c LIMIT 1) = (SELECT r FROM c LIMIT 1)",
        "WITH c AS (SELECT random() r FROM t LIMIT 1) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a LIMIT 1)) FROM t",
        "WITH c AS (SELECT random() r) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a)) FROM t",
        "WITH c AS (SELECT random() r UNION ALL SELECT 2) SELECT count(DISTINCT (SELECT r FROM c WHERE t.a = t.a)) FROM t",
        "WITH RECURSIVE c(x) AS (SELECT abs(random()) % 1000 UNION ALL SELECT x FROM c LIMIT 2) SELECT count(DISTINCT (SELECT x FROM c WHERE t.a = t.a)) FROM t",
        // Views: the same view twice in one FROM clause (or brought
        // there by a flattened view), unless flattened or a WHERE
        // term is pushed into one.
        "CREATE VIEW v AS SELECT random() r; SELECT count(*) FROM u, v a, v b WHERE a.r = b.r",
        "CREATE VIEW v AS SELECT random() r; SELECT count(*) FROM v a, u, v b WHERE a.r = b.r",
        "CREATE VIEW v AS SELECT random() r; SELECT count(*) FROM v a, v b, v c WHERE a.r = b.r AND b.r = c.r",
        "CREATE VIEW v AS SELECT random() r; CREATE VIEW w AS SELECT * FROM v; SELECT count(*) FROM w a, w b WHERE a.r = b.r",
        "CREATE VIEW v AS SELECT random() r; SELECT count(*) FROM v a, v b WHERE a.r = b.r AND a.r > 0",
        "CREATE VIEW v AS SELECT random() r; SELECT (SELECT r FROM v) = (SELECT r FROM v)",
        "CREATE VIEW w AS SELECT a, random() r FROM t; SELECT count(*) FROM w a, w b WHERE a.r = b.r",
        "SELECT count(*) FROM (SELECT random() r) a, (SELECT random() r) b WHERE a.r = b.r",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("mat{i}"), &format!("{t} {s}")).is_none(), "{s}");
    }
}

/// Regression: an endless recursive CTE read through a WHERE, a
/// join, a scalar subquery or EXISTS never ended (its rows grew
/// without bound); the pin reads such a first source as a
/// co-routine and stops at the reader's bound.
#[test]
fn endless_recursive_cte_stops_at_the_reader() {
    let c = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c)";
    for (i, s) in [
        "SELECT x FROM c WHERE x > 5 LIMIT 2",
        "SELECT x FROM c WHERE x > 5 LIMIT 2 OFFSET 3",
        "SELECT x FROM c WHERE x % 2 = 0 LIMIT 2 OFFSET 1",
        "SELECT (SELECT x FROM c LIMIT 1 OFFSET 5)",
        "SELECT (SELECT x FROM c WHERE x > 3)",
        "SELECT EXISTS (SELECT x FROM c WHERE x > 3)",
        "SELECT * FROM c, (SELECT 1) LIMIT 2",
        "SELECT * FROM c LEFT JOIN (SELECT 2 y) ON x = y LIMIT 3",
        "SELECT x + 1 FROM (SELECT x FROM c WHERE x > 3) LIMIT 2",
        "SELECT x FROM (SELECT x FROM c) WHERE x > 40 LIMIT 3",
        "SELECT x FROM c WHERE x IN (7, 9, 11) LIMIT 3",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            same(&format!("endless{i}"), &format!("{c} {s}")).is_none(),
            "{s}"
        );
    }
    // A finite CTE read the same ways reads it whole.
    let f = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10)";
    for (i, s) in [
        "SELECT x FROM c WHERE x > 5 LIMIT 20",
        "SELECT x FROM c WHERE x > 50 LIMIT 2",
        "SELECT (SELECT x FROM c WHERE x > 30)",
        "SELECT EXISTS (SELECT x FROM c WHERE x > 30)",
        "SELECT * FROM c, (SELECT 1 y) LIMIT 5 OFFSET 7",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            same(&format!("finite{i}"), &format!("{f} {s}")).is_none(),
            "{s}"
        );
    }
}

/// Run `f` on a thread with room for deeply nested views.
fn deep<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(f)
        .expect("thread")
        .join()
        .expect("join")
}

/// A script through hipp with the pin's nesting limit (and an arena
/// to expand it in), compared to the oracle.
fn same_deep(tag: &str, script: String, cfg: Config) -> Option<SchemaKind> {
    let h = common::temp_db(&format!("v16vcd-{tag}-h"));
    let o = common::temp_db(&format!("v16vcd-{tag}-o"));
    let (ook, ol) = oracle_script(&o, &script);
    let s2 = script.clone();
    let (hok, hl, he) = deep(move || {
        block_on(async {
            let mut db = Connection::open(FileStorage::open(&h).expect("fs"), cfg)
                .await
                .expect("open");
            let mut lines = Vec::new();
            for stmt in split(&s2) {
                let mut q = match db.prepare(&stmt).await {
                    Ok(q) => q,
                    Err(e) => return (false, lines, Some(format!("{e:?}"))),
                };
                loop {
                    match q.step().await {
                        Ok(Step::Row) => {
                            let mut l = String::new();
                            render(&q, &mut l);
                            lines.push(l);
                        }
                        Ok(Step::Done) => break,
                        Err(e) => return (false, lines, Some(format!("{e:?}"))),
                    }
                }
            }
            (true, lines, None)
        })
    });
    assert_eq!(hl, ol, "{tag}: hipp error {he:?}");
    assert_eq!(hok, ook, "{tag}: hipp error {he:?}");
    he.map(|e| {
        if e.contains("NestedTooDeep") {
            SchemaKind::NestedTooDeep
        } else {
            panic!("{tag}: {e}")
        }
    })
}

fn view_chain(n: usize) -> String {
    let mut s = String::from(
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); CREATE VIEW v0 AS SELECT a FROM t;",
    );
    for i in 1..n {
        s.push_str(&format!(
            " CREATE VIEW v{i} AS SELECT a + 1 AS a FROM v{};",
            i - 1
        ));
    }
    s.push_str(&format!(" SELECT * FROM v{};", n - 1));
    s
}

fn cte_chain(n: usize, sub: bool) -> String {
    let mut s = String::from("WITH c0 AS (SELECT 1 x)");
    for i in 1..n {
        if sub {
            s.push_str(&format!(
                ", c{i} AS (SELECT x + 1 x FROM (SELECT x FROM c{}))",
                i - 1
            ));
        } else {
            s.push_str(&format!(", c{i} AS (SELECT x + 1 x FROM c{})", i - 1));
        }
    }
    s.push_str(&format!(" SELECT * FROM c{}", n - 1));
    s
}

/// Regression: views or CTEs nested 63 deep were refused
/// (`RecursionLimit`); the pin reads 999 nested views or CTEs and
/// refuses 1000 ("VIEWs and/or subqueries nested too deep"), and
/// refuses a statement whose CTE references copy 1000 SELECTs (so
/// a CTE whose references double at each level fails fast instead
/// of growing exponentially). hipp's limit is
/// `Config::max_nesting` (default 250, for an 8 MiB stack).
#[test]
fn nesting_limits() {
    // The pin's nesting limit and (v0.16 depth unit) its
    // expression and parser depth limits.
    let pin = || {
        Config::default()
            .max_nesting(1000)
            .depth_limits(
                hipp::sql::parser::PIN_EXPR_DEPTH,
                hipp::sql::parser::PIN_PARSER_DEPTH,
            )
            .statement_arena(16 * 1024, 8 * 1024 * 1024)
    };
    for n in [63, 70, 300, 999] {
        assert_eq!(same_deep(&format!("vc{n}"), view_chain(n), pin()), None);
        assert_eq!(
            same_deep(&format!("cc{n}"), cte_chain(n, false), pin()),
            None
        );
    }
    assert_eq!(same_deep("cc500s", cte_chain(500, true), pin()), None);
    for (tag, s) in [
        ("vc1000", view_chain(1000)),
        ("cc1000", cte_chain(1000, false)),
        ("cc501s", cte_chain(501, true)),
    ] {
        assert_eq!(same_deep(tag, s, pin()), Some(SchemaKind::NestedTooDeep));
    }
    // CTE references copied side by side count too, views read
    // before a CTE included.
    let side = |n: usize, lead: &str| {
        let mut s = format!(
            "CREATE VIEW v AS SELECT 1 x; WITH c AS (SELECT 1 x) SELECT count(*) FROM (SELECT {lead}"
        );
        for i in 0..n {
            s.push_str(&format!("(SELECT x FROM c) a{i}, "));
        }
        s.push_str("1 z)");
        s
    };
    assert_eq!(same_deep("side900", side(900, ""), pin()), None);
    assert_eq!(
        same_deep("side1100", side(1100, ""), pin()),
        Some(SchemaKind::NestedTooDeep)
    );
    let views_first = {
        let mut s = String::from(
            "CREATE VIEW v AS SELECT 1 x; WITH c AS (SELECT 1 x) SELECT count(*) FROM (SELECT ",
        );
        for i in 0..1100 {
            s.push_str(&format!("(SELECT x FROM v) a{i}, "));
        }
        s.push_str("(SELECT x FROM c) z)");
        s
    };
    assert_eq!(
        same_deep("vfirst", views_first, pin()),
        Some(SchemaKind::NestedTooDeep)
    );
    for k in [9, 15, 30] {
        let mut s = String::from("WITH c0 AS (SELECT 1 x)");
        for i in 1..=k {
            s.push_str(&format!(
                ", c{i} AS (SELECT a.x + b.x x FROM c{} a, c{} b)",
                i - 1,
                i - 1
            ));
        }
        s.push_str(&format!(" SELECT * FROM c{k}"));
        assert_eq!(
            same_deep(&format!("exp{k}"), s, pin()),
            Some(SchemaKind::NestedTooDeep)
        );
    }
    // FROM subqueries nest freely (the expansion's old cap counted
    // them too).
    let mut d = String::from("SELECT * FROM ");
    for _ in 0..150 {
        d.push_str("(SELECT * FROM ");
    }
    d.push_str("(SELECT 1)");
    for _ in 0..150 {
        d.push(')');
    }
    assert_eq!(same_deep("derived150", d, pin()), None);
    // The default limit refuses cleanly.
    let h = common::temp_db("v16vcd-default");
    let r = deep(move || {
        block_on(async {
            let mut db = Connection::open(FileStorage::open(&h).expect("fs"), Config::default())
                .await
                .expect("open");
            for stmt in split(&view_chain(300)) {
                let mut q = match db.prepare(&stmt).await {
                    Ok(q) => q,
                    Err(e) => return Some(format!("{e:?}")),
                };
                while let Ok(Step::Row) = q.step().await {}
            }
            None
        })
    });
    assert!(r.is_some_and(|e| e.contains("NestedTooDeep")));
}

/// Regression: CREATE VIEW IF NOT EXISTS over an index's name did
/// nothing; the pin's "there is already an index named i".
#[test]
fn create_view_over_an_index_name() {
    let t = "CREATE TABLE t(a); CREATE INDEX i ON t(a);";
    refuse(
        "cvi0",
        &format!("{t} CREATE VIEW IF NOT EXISTS i AS SELECT 1"),
        SchemaKind::DuplicateIndex,
    );
    refuse(
        "cvi1",
        &format!("{t} CREATE VIEW i AS SELECT 1"),
        SchemaKind::DuplicateIndex,
    );
    assert!(
        same(
            "cvi2",
            &format!("{t} CREATE VIEW IF NOT EXISTS t AS SELECT 1; SELECT 5")
        )
        .is_none()
    );
    assert!(
        same(
            "cvi3",
            &format!("{t} CREATE VIEW v AS SELECT 1; CREATE VIEW IF NOT EXISTS v AS SELECT 2; SELECT * FROM v")
        )
        .is_none()
    );
}

/// Regression: a qualified rowid over two sources of that name with
/// a rowid read the first; the pin's "ambiguous column name".
#[test]
fn qualified_rowid_is_ambiguous() {
    let t =
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); CREATE TABLE u(b); INSERT INTO u VALUES (2);";
    for (i, s) in [
        "SELECT t.rowid FROM t, t",
        "SELECT t.oid FROM t, t",
        "SELECT t._rowid_ FROM t AS t, u AS t",
        "SELECT t.rowid FROM t JOIN t USING (a)",
        "SELECT rowid FROM t, t",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("rid{i}"),
            &format!("{t} {s}"),
            SchemaKind::AmbiguousColumn,
        );
    }
    for (i, s) in [
        "SELECT t.rowid FROM t, (SELECT 1 b) t",
        "SELECT t.rowid FROM (SELECT 1 b) t, t",
        "SELECT t.rowid FROM t, (SELECT 1 rowid) t",
        "SELECT t.rowid FROM (SELECT 7 rowid) t, t",
        "SELECT t.rowid, u.rowid FROM t, u",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            same(&format!("nrid{i}"), &format!("{t} {s}")).is_none(),
            "{s}"
        );
    }
}

/// Regression (fixed with the PRAGMA unit's review): a quoted
/// declared type prints dequoted, standard names in their spelling,
/// for tables and the views over them.
#[test]
fn table_info_quoted_type_names() {
    assert!(
        same(
            "qty",
            "CREATE TABLE t(a \"foo\", b [bar baz], c \"varchar\"(10), d 'text', e \"integer\" PRIMARY KEY, \
             f \"\", g `x y`, h \"a\"\"b\", i 'int'); PRAGMA table_info(t); \
             CREATE VIEW v AS SELECT * FROM t; PRAGMA table_info(v)"
        )
        .is_none()
    );
}

/// Regression: `SELECT *` with no FROM clause — in a view, a CTE, a
/// FROM subquery or anywhere — gave one row of no columns; the
/// pin's "no tables specified".
#[test]
fn star_without_from() {
    for (i, s) in [
        "SELECT *",
        "SELECT *, 1",
        "SELECT 1, *",
        "CREATE VIEW v AS SELECT *; SELECT count(*) FROM v",
        "CREATE VIEW v AS SELECT *; PRAGMA table_info(v)",
        "WITH c AS (SELECT *) SELECT * FROM c",
        "SELECT * FROM (SELECT *)",
        "SELECT (SELECT *)",
        "SELECT 1 WHERE EXISTS (SELECT *)",
        "VALUES (1) UNION SELECT *",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT (SELECT *) FROM t",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("star{i}"), s, SchemaKind::NoTablesSpecified);
    }
    assert!(
        same(
            "star-ok",
            "CREATE VIEW v AS SELECT *; SELECT count(*); SELECT 1"
        )
        .is_none()
    );
}

/// Regression: RETURNING ran a DELETE's subqueries before the row's
/// delete, and hoisted every uncorrelated subquery across the rows;
/// the pin evaluates RETURNING after each row's change, afresh, as
/// a trigger program — where a FROM subquery, view or CTE is
/// materialized once for the whole statement.
#[test]
fn returning_sees_each_change() {
    let t = "CREATE TABLE t(a PRIMARY KEY, b); INSERT INTO t VALUES (1, 0), (2, 0), (3, 0); \
             CREATE TABLE u(c); INSERT INTO u VALUES (1);";
    for (i, s) in [
        "DELETE FROM t WHERE a >= 2 RETURNING a, (SELECT count(*) FROM t)",
        "DELETE FROM t RETURNING a, (SELECT count(*) FROM t), (SELECT group_concat(a) FROM t)",
        "DELETE FROM t WHERE a = 2 RETURNING a, (SELECT count(*) FROM t WHERE a = 2), (SELECT max(a) FROM t)",
        "DELETE FROM t WHERE a >= 2 RETURNING a, EXISTS (SELECT 1 FROM t WHERE a = 3)",
        "CREATE VIEW v AS SELECT count(*) n FROM t; DELETE FROM t WHERE a = 2 RETURNING a, (SELECT n FROM v)",
        "DELETE FROM t WHERE a IN (SELECT a FROM t WHERE a > 1) RETURNING a, (SELECT count(*) FROM t)",
        "DELETE FROM t WHERE a >= 2 RETURNING a, (SELECT count(*) FROM t) + (SELECT count(*) FROM t)",
        "UPDATE t SET b = a * 10 RETURNING a, (SELECT sum(b) FROM t)",
        "UPDATE t SET b = (SELECT count(*) FROM t WHERE b > 0) + 1 RETURNING a, b",
        "UPDATE t SET b = 1 RETURNING a, (SELECT sum(b) FROM t), (SELECT sum(b) FROM t WHERE a > 0)",
        "INSERT INTO t VALUES (4, 0), (5, 0) RETURNING a, (SELECT count(*) FROM t)",
        "INSERT INTO t SELECT a + 10, b FROM t RETURNING a, (SELECT count(*) FROM t)",
        "INSERT INTO t VALUES (1, 0), (7, 0) ON CONFLICT (a) DO UPDATE SET b = 9 RETURNING a, b, (SELECT sum(b) FROM t)",
        // FROM subqueries, views and CTEs: once per statement.
        "DELETE FROM t WHERE a >= 2 RETURNING a, (WITH c AS (SELECT count(*) n FROM t) SELECT x.n + y.n FROM c x, c y)",
        "DELETE FROM t WHERE a >= 2 RETURNING a, (WITH c AS (SELECT count(*) n FROM t) SELECT x.n FROM c x)",
        "DELETE FROM t WHERE a >= 2 RETURNING a, (SELECT n FROM (SELECT count(*) n FROM t))",
        "DELETE FROM t WHERE a >= 2 RETURNING a, (SELECT count(*) FROM (SELECT * FROM t)), (SELECT count(*) FROM t)",
        "UPDATE t SET b = 1 RETURNING a, (SELECT sum(b) FROM (SELECT b FROM t LIMIT 5))",
        "UPDATE t SET b = 1 RETURNING a, (SELECT group_concat(x) FROM (SELECT b x FROM t))",
        "UPDATE t SET b = 1 RETURNING a, (SELECT count(*) FROM u, (SELECT b x FROM t) WHERE x = 1)",
        "CREATE VIEW v AS SELECT b FROM t; UPDATE t SET b = 1 RETURNING a, (SELECT sum(b) FROM v)",
        "WITH c AS NOT MATERIALIZED (SELECT b FROM t) UPDATE t SET b = 1 RETURNING a, (SELECT sum(b) FROM c)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("ret{i}"), &format!("{t} {s}")).is_none(), "{s}");
    }
}
