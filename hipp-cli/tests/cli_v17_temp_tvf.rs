//! hipp-cli v0.17 review fix (wave 15, `sync_readers` review):
//! `generate_series`, `dbstat` and pragma functions in statements
//! that write or read TEMP tables — byte-for-byte the pinned oracle's
//! stdout. hipp failed the `generate_series` / `dbstat` ones
//! (`UnknownFunction` / `UnknownTable`) and read nothing from a
//! pragma function naming a main table.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17tt-{tag}"));
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

const SETUP: &str = "CREATE TABLE m(x, y TEXT);
INSERT INTO m VALUES (1,'a'),(3,'c'),(5,NULL);
CREATE INDEX my ON m(y);
CREATE TEMP TABLE tt(a, b);
INSERT INTO tt VALUES (1,NULL),(2,'x');";

const SCRIPT: &str = "DELETE FROM tt WHERE a IN (SELECT value FROM generate_series(2,2));
SELECT * FROM tt;
INSERT INTO tt SELECT value, 'g' FROM generate_series(1,7,3);
UPDATE tt SET b = (SELECT group_concat(value, '-') FROM generate_series(1,a)) WHERE b = 'g';
INSERT INTO tt VALUES ((SELECT sum(value) FROM generate_series(1,4)), 'v');
WITH c(v) AS (SELECT value FROM generate_series(10,11)) INSERT INTO tt SELECT v, 'cte' FROM c;
SELECT rowid, a, b FROM tt;
SELECT a, value, a * value FROM tt, generate_series(1,2) WHERE a < 5 ORDER BY 1, 2;
CREATE TEMP TABLE g AS SELECT value AS v, value / 2.0 AS h FROM generate_series(1,3);
SELECT v, h, typeof(h) FROM g;
INSERT INTO tt SELECT name, pageno FROM dbstat ORDER BY name;
CREATE TEMP TABLE d AS SELECT name, path, pagetype FROM dbstat ORDER BY name;
SELECT * FROM d;
INSERT INTO tt SELECT name, type FROM pragma_table_info('m');
SELECT a, b FROM tt WHERE b = 'TEXT' OR b = '';
SELECT a, name FROM tt, pragma_index_list('m') WHERE a = 'y';
CREATE TEMP VIEW tv AS SELECT name FROM pragma_table_info('m');
SELECT * FROM tv;
SELECT count(*) FROM tt;";

#[test]
fn temp_writes_read_table_valued_functions_like_the_pin() {
    same("stdin", SETUP, SCRIPT);
    // One-shot: the whole batch as the argument.
    let h = temp_db("arg-h");
    let o = temp_db("arg-o");
    let sql = format!("{SETUP}\n{SCRIPT}");
    let run = |bin: &Path, db: &Path| {
        let out = Command::new(bin).arg(db).arg(&sql).output().expect("runs");
        (
            String::from_utf8(out.stdout).expect("utf8 stdout"),
            out.stderr.is_empty(),
        )
    };
    let (a, a_quiet) = run(&oracle(), &o);
    let (b, b_quiet) = run(&hipp_bin(), &h);
    assert_eq!(b, a, "one-shot:\n{sql}");
    assert!(a_quiet && b_quiet, "stderr: {sql}");
}
