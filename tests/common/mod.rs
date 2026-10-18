//! Shared test harness: oracle location + CLI runner (§8 oracle
//! discipline). The pinned oracle is SQLite 3.54.1 (D1); harness
//! results against anything else are pre-pin and do not count as
//! claim tests — `oracle_version()` lets tests report which ran.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

pub fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

pub fn oracle_version() -> String {
    let out = Command::new(oracle())
        .arg("--version")
        .output()
        .expect("oracle runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn oracle_is_pinned() -> bool {
    oracle_version().starts_with("3.54.1")
}

/// Run the oracle CLI against `db` with one SQL batch; returns
/// stdout (list mode, `|` separator). Panics on CLI failure.
pub fn cli(db: &std::path::Path, sql: &str) -> String {
    let out = Command::new(oracle())
        .arg(db)
        .arg(sql)
        .output()
        .expect("oracle runs");
    assert!(
        out.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Create a fresh temp directory path for one test database.
pub fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}
