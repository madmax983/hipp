//! hipp-cli v0.16 differentials: executed views and common table
//! expressions must print byte-identically to the pinned oracle —
//! view rows through joins, compounds and subqueries, the
//! `.schema` view comments (the planned column names; none for a
//! view that cannot be read), PRAGMA table_info of views, WITH
//! before SELECT and writes, and recursive CTEs in the pin's queue
//! order (REAL and text rendering included). Each batch runs
//! one-shot and over stdin; hipp-written files must pass the
//! oracle's integrity check.

use std::path::{Path, PathBuf};
use std::process::Command;

fn hipp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hipp"))
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16vc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> (bool, Vec<u8>) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (out.status.success(), out.stdout)
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("cli finishes").stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let (hok, got) = run(&hipp_bin(), &a, batch);
    let (ook, want) = run(&oracle(), &b, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{tag}"
    );
    assert_eq!(hok, ook, "{tag}");
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    if hok {
        assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;").1, b"ok\n");
    }
}

const VIEWS: &str = "\
CREATE TABLE t(a INTEGER, b TEXT, c REAL);
INSERT INTO t VALUES (1, 'x', 1.5), (2, 'y', NULL), (3, NULL, -0.25), (4, 'x', 1e20);
CREATE TABLE u(k INTEGER PRIMARY KEY, v);
INSERT INTO u VALUES (1, 'one'), (2, 'two'), (5, 'five');
CREATE VIEW v AS SELECT a, b, c * 2 AS c2 FROM t WHERE a > 1;
CREATE VIEW w(p, q) AS SELECT k, v FROM u;
CREATE VIEW vv AS SELECT * FROM v NATURAL JOIN (SELECT k AS a, v FROM u);
CREATE VIEW vc AS WITH x AS (SELECT a FROM t) SELECT a, a * 1.5 AS h FROM x;
CREATE VIEW vr AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 4) SELECT n FROM r;
CREATE VIEW dup AS SELECT a, a, b AS a FROM t;
CREATE VIEW bad AS SELECT * FROM nosuch;
CREATE VIEW long(p, q, r) AS SELECT a FROM t;
CREATE VIEW \"odd name\"(\"a b\", \"select\") AS SELECT 1, 2;
SELECT * FROM v;
SELECT * FROM w WHERE p > 1;
SELECT * FROM vv;
SELECT * FROM vc;
SELECT * FROM vr;
SELECT * FROM dup;
SELECT a FROM t WHERE a IN vr;
SELECT * FROM v UNION ALL SELECT p, q, NULL FROM w;
SELECT (SELECT count(*) FROM v), (SELECT max(h) FROM vc);
SELECT t.a, w.q FROM t LEFT JOIN w ON w.p = t.a;
PRAGMA table_info(v);
PRAGMA table_info(vc);
PRAGMA table_info(dup);
PRAGMA table_info(long);
";

const CTES: &str = "\
CREATE TABLE t(a INTEGER, b TEXT);
INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, NULL);
WITH x AS (SELECT a, b FROM t WHERE a > 1), y(n) AS (SELECT count(*) FROM x) SELECT * FROM x, y;
WITH x(k) AS (SELECT a * 0.5 FROM t) SELECT k, typeof(k) FROM x;
WITH x AS (SELECT 10 AS a) INSERT INTO t(a) SELECT a FROM x;
WITH x AS (SELECT 2 AS a) UPDATE t SET b = 'upd' WHERE a IN x;
WITH x AS (SELECT 3 AS a) DELETE FROM t WHERE a IN (SELECT a FROM x);
SELECT * FROM t;
WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 6) SELECT n, n / 4.0 FROM c;
WITH RECURSIVE c(n, d) AS (SELECT 1, 0 UNION ALL SELECT n * 2, d + 1 FROM c WHERE d < 3 UNION ALL SELECT n * 2 + 1, d + 1 FROM c WHERE d < 3 ORDER BY 1 DESC) SELECT * FROM c;
WITH RECURSIVE s(x, tag) AS (SELECT 3, 'a' UNION ALL SELECT 3, 'b' UNION ALL SELECT x - 1, tag || '.' FROM s WHERE x > 1 ORDER BY 1) SELECT * FROM s;
WITH RECURSIVE s(x) AS (SELECT 'a' UNION SELECT upper(x) FROM s UNION SELECT x || 'b' FROM s WHERE length(x) < 3) SELECT * FROM s;
WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT n FROM c LIMIT 4 OFFSET 2;
WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 9 LIMIT 3 OFFSET 1) SELECT * FROM c;
WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a + b FROM fib WHERE b < 200) SELECT group_concat(a, ' ') FROM fib;
WITH RECURSIVE m(x) AS (SELECT 1.0 UNION ALL SELECT x / 3 FROM m WHERE x > 0.01) SELECT * FROM m;
";

#[test]
fn views_byte_identical() {
    byte_identical("views", VIEWS);
}

/// `.schema` over the views: each view's comment lists its planned
/// column names (a column list of any length as written, duplicates
/// renamed, odd names quoted), and a view that cannot be read gets
/// none. Over stdin, where dot-commands run.
#[test]
fn schema_view_comments_byte_identical() {
    let a = temp_db("schema-hipp");
    let b = temp_db("schema-ora");
    let input = format!("{VIEWS}.schema\nDROP TABLE t;\n.schema\n");
    let got = run_stdin(&hipp_bin(), &a, &input);
    let want = run_stdin(&oracle(), &b, &input);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want)
    );
    assert!(String::from_utf8_lossy(&got).contains("/* vv(a,b,c2,v) */"));
}

#[test]
fn ctes_byte_identical() {
    byte_identical("ctes", CTES);
}

/// A generated batch of recursive CTEs with queue ORDER BY,
/// UNION / UNION ALL and row windows, rendered by both shells.
#[test]
fn recursive_generated_byte_identical() {
    let mut s: u64 = 0x5eed_0016_c11e;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let setups = ["1", "2.5", "'q'", "NULL", "-3", "0"];
    let steps = [
        "SELECT x + 1, d + 1 FROM r WHERE d < 3",
        "SELECT x * 1.5, d + 1 FROM r WHERE d < 2",
        "SELECT x || 'z', d + 1 FROM r WHERE d < 2",
        "SELECT -x, d + 1 FROM r WHERE d < 3",
    ];
    let orders = ["", " ORDER BY 1", " ORDER BY 1 DESC", " ORDER BY 2 DESC"];
    let limits = ["", " LIMIT 4", " LIMIT 2 OFFSET 1"];
    let mut batch = String::new();
    for _ in 0..40 {
        let op = if next() % 2 == 0 {
            "UNION"
        } else {
            "UNION ALL"
        };
        batch.push_str(&format!(
            "WITH RECURSIVE r(x, d) AS (SELECT {}, 0 UNION ALL SELECT {}, 0 {op} {} {op} {}{}{}) SELECT * FROM r;\n",
            setups[(next() % 6) as usize],
            setups[(next() % 6) as usize],
            steps[(next() % 4) as usize],
            steps[(next() % 4) as usize],
            orders[(next() % 4) as usize],
            limits[(next() % 3) as usize],
        ));
    }
    byte_identical("recgen", &batch);
}
