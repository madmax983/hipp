//! Shared code for the async-runtime examples (`examples/tokio.rs`,
//! `examples/smol.rs`, `examples/embassy.rs`).
//!
//! hipp's API is `async` and runtime-agnostic: it never spawns, never
//! sleeps and ships no executor, so any executor can drive it. What
//! differs between runtimes is (a) how a `Storage` does its I/O, and
//! (b) how you share a `Connection`, which is `!Send` and allows one
//! statement at a time. The examples show both, per runtime.

use hipp::{Connection, Step, Storage, Value};

/// Render one value as the `sqlite3` shell's list mode does.
#[must_use]
pub fn render(v: Option<Value<'_>>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(f)) => format!("{f}"),
        Some(Value::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Some(Value::Blob(b)) => b.iter().map(|x| format!("{x:02X}")).collect(),
    }
}

/// Run one statement to completion. Rows come back as `a|b|c`. Errors
/// come back as their debug text, so a caller on another task or
/// thread can receive them.
///
/// # Errors
///
/// The statement's `hipp::Error`, formatted with `{:?}`.
pub async fn try_rows<S>(db: &mut Connection<S>, sql: &str) -> Result<Vec<String>, String>
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
    let mut out = Vec::new();
    while st.step().await.map_err(|e| format!("{e:?}"))? == Step::Row {
        let cells: Vec<String> = (0..st.column_count())
            .map(|i| render(st.column_value(i)))
            .collect();
        out.push(cells.join("|"));
    }
    Ok(out)
}

/// [`try_rows`], panicking with the SQL on error (for the examples'
/// own statements, which must succeed).
///
/// # Panics
///
/// If the statement fails.
pub async fn rows<S>(db: &mut Connection<S>, sql: &str) -> Vec<String>
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    match try_rows(db, sql).await {
        Ok(r) => r,
        Err(e) => panic!("{sql}\n  -> {e}"),
    }
}

/// Run a statement that returns no rows.
///
/// # Panics
///
/// If the statement fails or returns rows.
pub async fn exec<S>(db: &mut Connection<S>, sql: &str)
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let r = rows(db, sql).await;
    assert!(r.is_empty(), "{sql} returned rows: {r:?}");
}

/// The file name suffix SQLite gives each sidecar.
#[must_use]
pub fn sidecar_suffix(kind: hipp::storage::Sidecar) -> &'static str {
    use hipp::storage::Sidecar;
    match kind {
        Sidecar::RollbackJournal => "-journal",
        Sidecar::Wal => "-wal",
        Sidecar::Shm => "-shm",
    }
}

/// `path` with `suffix` appended (`app.db` + `-journal`).
#[must_use]
pub fn sibling(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// A fresh, unique path in the system temp directory, with any old
/// files there removed.
#[must_use]
pub fn temp_db(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("hipp-{name}-{}.db", std::process::id()));
    remove_db(&p);
    p
}

/// Remove a database file and its sidecars.
pub fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    for s in ["-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(sibling(path, s));
    }
}
