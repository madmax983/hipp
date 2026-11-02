//! hipp-cli v0.16 differentials: joins, ordering and
//! query-shape completion must render byte-identically to the
//! pinned oracle — RIGHT / FULL / NATURAL / USING joins (with
//! REAL and mixed-type join columns, so NULL padding, the
//! coalesced USING column and float rendering all show), NULLS
//! FIRST / LAST, LIMIT / OFFSET expressions, result-column
//! aliases in WHERE / GROUP BY / HAVING / ORDER BY, and
//! `x IN table`. Each batch runs one-shot and over stdin.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16qs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> Vec<u8> {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One batch, byte-compared one-shot and over stdin (raw bytes:
/// a blob column renders unconverted, so not always UTF-8).
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(got, run(&oracle(), &b, batch));
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

const JOINS: &str = "\
CREATE TABLE a(id, x REAL, k);
CREATE TABLE b(k, id, y TEXT);
CREATE TABLE c(id INTEGER PRIMARY KEY, z);
INSERT INTO a VALUES(1, 1.5, 10), (2, 2.0, 20), (3, -0.25, 30), (NULL, 1e100, 40), ('2', 7, 50);
INSERT INTO b VALUES(10, 1, 'b1'), (20, 2.0, 'b2'), (99, 4, 'b4'), (40, NULL, 'bn'), (50, '2', 'bs');
INSERT INTO c VALUES(1, 0.1), (4, 'c4'), (5, x'00ff');
SELECT * FROM a RIGHT JOIN b ON a.id = b.id;
SELECT * FROM a FULL JOIN b ON a.id = b.id;
SELECT * FROM a FULL OUTER JOIN b ON a.x > b.k / 20.0;
SELECT * FROM a NATURAL JOIN b;
SELECT * FROM a NATURAL FULL JOIN b;
SELECT * FROM a JOIN b USING (id);
SELECT * FROM a RIGHT JOIN b USING (id);
SELECT * FROM a FULL JOIN b USING (id);
SELECT id, a.id, b.id, x FROM a FULL JOIN b USING (id);
SELECT a.*, '|', b.* FROM a FULL JOIN b USING (id);
SELECT * FROM a FULL JOIN b USING (id) FULL JOIN c USING (id);
SELECT * FROM a LEFT JOIN b USING (id) RIGHT JOIN c USING (id);
SELECT id, typeof(id), count(*), total(x) FROM a NATURAL FULL JOIN b GROUP BY id ORDER BY id;
SELECT x, y FROM a JOIN b ON a.id = c.id JOIN c ON c.z > 0;
";

const ORDERING: &str = "\
CREATE TABLE t(a, b);
INSERT INTO t VALUES(2.5, 'x'), (NULL, 'y'), (1, NULL), (NULL, 'w'), (-3, 'z'), ('t', 1.0);
SELECT a, b FROM t ORDER BY a NULLS LAST;
SELECT a, b FROM t ORDER BY a DESC NULLS FIRST;
SELECT a, b FROM t ORDER BY b NULLS LAST, a DESC NULLS FIRST;
SELECT a FROM t UNION SELECT 0.5 ORDER BY 1 DESC NULLS FIRST;
SELECT * FROM (SELECT a FROM t ORDER BY a NULLS LAST LIMIT 4) ORDER BY 1 NULLS LAST;
SELECT a, count(*) FROM t GROUP BY a HAVING 1 ORDER BY a NULLS LAST;
SELECT a+1 AS b, count(*) FROM t GROUP BY b ORDER BY b NULLS LAST;
SELECT a * 2 AS d FROM t WHERE d > 0 ORDER BY d;
SELECT a AS v, b FROM t WHERE v IS NOT NULL GROUP BY v HAVING count(v) > 0 ORDER BY -v NULLS FIRST;
SELECT a FROM t ORDER BY a NULLS LAST LIMIT (SELECT count(*) FROM t) - 2 OFFSET '1';
SELECT a FROM t ORDER BY a LIMIT 2.0, -1;
";

const IN_TABLE: &str = "\
CREATE TABLE k(v REAL);
INSERT INTO k VALUES(1), (2.5), (NULL);
CREATE TABLE e(v);
CREATE TABLE s(w);
INSERT INTO s VALUES(1), (2.5), ('2.5'), (3), (NULL);
SELECT w, w IN k, w NOT IN k, w IN e, w NOT IN e FROM s;
SELECT w FROM s WHERE w IN main.k ORDER BY w;
";

#[test]
fn cli_v16_joins_byte_identical() {
    byte_identical("joins", JOINS);
}

#[test]
fn cli_v16_ordering_limits_aliases_byte_identical() {
    byte_identical("ordering", ORDERING);
}

#[test]
fn cli_v16_in_table_byte_identical() {
    byte_identical("in-table", IN_TABLE);
}
