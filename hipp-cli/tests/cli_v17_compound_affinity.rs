//! hipp-cli v0.17 review fix (wave 14, unit `vm_flip` retry): a
//! FROM subquery / view / CTE column with REAL affinity across a
//! compound reads another core's integers as reals (`1.0`, the pin's
//! `OP_RealAffinity`), the compound's affinity comes from the first
//! core whose item has one, and a scalar subquery's from its
//! rightmost core — byte-for-byte the pinned oracle's stdout (hipp
//! printed `1`).

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
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17caff-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

/// A script on stdin: stdout and stderr.
fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, String) {
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
    let out = child.wait_with_output().expect("cli finishes");
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// `setup` then `script`, each engine on its own fresh file: stdout
/// identical and nothing on stderr.
fn same(tag: &str, setup: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let input = format!("{setup}\n{script}");
    let (a, ae) = run_stdin(&oracle(), &o, &input);
    let (b, be) = run_stdin(&hipp_bin(), &h, &input);
    assert_eq!(b, a, "script:\n{input}");
    assert!(ae.is_empty(), "pin stderr {ae}: {input}");
    assert!(be.is_empty(), "hipp stderr {be}: {input}");
}

const SETUP: &str = "CREATE TABLE t(c REAL);
INSERT INTO t VALUES (0.0);
CREATE TABLE u(i INTEGER, s TEXT);
INSERT INTO u VALUES (1,'a'),(2,'b');
CREATE VIEW v AS SELECT c x FROM t UNION SELECT 1;";

const QUERIES: &str = "SELECT x, typeof(x) FROM (SELECT c x FROM t UNION ALL SELECT 1);
SELECT x, typeof(x) FROM (SELECT 1 x UNION SELECT c FROM t);
SELECT max(x), typeof(max(x)) FROM (SELECT c x FROM t UNION ALL SELECT 1);
SELECT x, typeof(x) FROM v;
SELECT typeof(x) FROM (SELECT c x FROM t UNION ALL VALUES(1));
WITH w(x) AS (SELECT c FROM t UNION ALL SELECT 9223372036854775807) SELECT x FROM w;
WITH RECURSIVE r(x, n) AS (SELECT 1, 0 UNION ALL SELECT (SELECT c FROM t), n + 1 FROM r WHERE n < 2) SELECT * FROM r;
SELECT x, i FROM (SELECT c x FROM t UNION ALL SELECT i FROM u) JOIN u ON 1 ORDER BY 1, 2;
CREATE TABLE ct AS SELECT (SELECT c FROM t UNION ALL SELECT 1) y, (SELECT 1 UNION ALL SELECT c FROM t) z;
SELECT sql FROM sqlite_schema WHERE name = 'ct';";

#[test]
fn derived_real_affinity_renders_like_the_pin() {
    same("list", SETUP, QUERIES);
}
