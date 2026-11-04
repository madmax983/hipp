//! hipp-cli v0.14 differentials: delete merge/redistribution.
//! Delete-heavy batches run through the hipp CLI on an
//! oracle-built file must leave the same contents AND the same
//! file shape (page_count / freelist_count, read back through
//! the pinned oracle) as the same batches run through the
//! pinned oracle — the pin redistributes underflow windows of
//! up to three siblings once free space passes two thirds of
//! usable, merges when the window fits fewer pages, and
//! collapses a one-child root; hipp now does the same.

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
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v14-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

const SETUP: &str = "PRAGMA page_size=512; CREATE TABLE t (x TEXT); CREATE INDEX ix ON t(x); WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<200) INSERT INTO t SELECT printf('r%03dxxxxxx', n) FROM c;";

/// Run `script` through the hipp CLI on one oracle-built twin
/// and through the oracle on the other; contents, shape, and
/// integrity must all agree afterwards.
fn assert_delete_script(tag: &str, script: &str) {
    let h = temp_db(&format!("{tag}-hipp"));
    let o = temp_db(&format!("{tag}-ora"));
    run(&oracle(), &h, SETUP);
    run(&oracle(), &o, SETUP);
    run_stdin(&hipp_bin(), &h, script);
    run_stdin(&oracle(), &o, script);
    assert_eq!(
        run(&oracle(), &h, "SELECT * FROM t ORDER BY rowid;"),
        run(&oracle(), &o, "SELECT * FROM t ORDER BY rowid;"),
        "{tag}: contents differ"
    );
    for pragma in ["PRAGMA page_count;", "PRAGMA freelist_count;"] {
        assert_eq!(
            run(&oracle(), &h, pragma),
            run(&oracle(), &o, pragma),
            "{tag}: {pragma} differs"
        );
    }
    assert_eq!(
        run(&oracle(), &h, "PRAGMA integrity_check;"),
        "ok\n",
        "{tag}: integrity_check on the hipp file"
    );
}

#[test]
fn cli_v14_tail_and_scatter_deletes() {
    assert_delete_script(
        "tail",
        "DELETE FROM t WHERE rowid > 120;\nDELETE FROM t WHERE rowid % 2 = 1;\n",
    );
}

#[test]
fn cli_v14_delete_all_collapses() {
    assert_delete_script("all", "DELETE FROM t;\n");
}

#[test]
fn cli_v14_middle_band_and_update_churn() {
    assert_delete_script(
        "band",
        "DELETE FROM t WHERE rowid BETWEEN 40 AND 160;\nUPDATE t SET x = x || 'yy' WHERE rowid > 190;\nDELETE FROM t WHERE rowid > 190;\n",
    );
}
