//! v0.17 parity unit `result_gaps`, in the grammar the `query` feature
//! claims without `sql` (no subqueries): the OFFSET's rows and the
//! rows the pin's top-N sorter skips have no result column computed,
//! differentially against the pinned oracle (SQLite 3.54.1). The
//! default test run checks the async executor here and also builds
//! and runs this file with `--no-default-features --features
//! std,query` (`query_build_runs_these`), whose synchronous executor
//! (`project_rows`) computed an OFFSET row's columns before v0.17
//! (the recorded gap).
#![cfg(feature = "query")]

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};

/// One statement's rows in the shell's list rendering (integers and
/// text only here).
async fn collect(db: &mut Connection<FileStorage>, sql: &str) -> Result<String, Error<FileError>> {
    let mut q = db.prepare(sql).await?;
    let mut out = String::new();
    while q.step().await? == Step::Row {
        for i in 0..q.column_count() {
            if i > 0 {
                out.push('|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => out.push_str(&v.to_string()),
                Some(Value::Real(f)) => out.push_str(&format!("{f:?}")),
                Some(Value::Text(b)) | Some(Value::Blob(b)) => {
                    out.push_str(&String::from_utf8_lossy(b))
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// The overflowing row (`a` 1) comes third.
const T: &str = "CREATE TABLE t(a INTEGER, b, c TEXT); \
    INSERT INTO t VALUES (2,1,'x'),(3,2,'Y'),(1,5,'z'),(4,0,'w'),(5,3,NULL),(6,NULL,'v');";

const QS: &[&str] = &[
    // The OFFSET skips a row before its columns (no sorter).
    "SELECT $X FROM t LIMIT 1 OFFSET 2",
    "SELECT $X FROM t LIMIT 2 OFFSET 2",
    "SELECT $X, b FROM t WHERE a > 0 LIMIT 3 OFFSET 2",
    "SELECT $X FROM t ORDER BY rowid LIMIT 1 OFFSET 2",
    "SELECT a % 2, $X FROM t GROUP BY a LIMIT 2 OFFSET 0",
    "SELECT a, $X FROM t GROUP BY a LIMIT 3 OFFSET 1",
    "SELECT a, $X FROM t GROUP BY a LIMIT 3",
    "SELECT abs(-9223372036854775808) FROM t LIMIT 1 OFFSET 9",
    "SELECT count(*), abs(-9223372036854775808) FROM t LIMIT 1 OFFSET 1",
    // The top-N sorter skips a row whose key sorts past the LIMIT.
    "SELECT $X FROM t ORDER BY b LIMIT 1",
    "SELECT $X FROM t ORDER BY b LIMIT 2",
    "SELECT $X FROM t ORDER BY b LIMIT 1 OFFSET 1",
    "SELECT $X FROM t ORDER BY b LIMIT 3",
    "SELECT $X FROM t ORDER BY b DESC LIMIT 1",
    "SELECT $X, b FROM t ORDER BY b NULLS LAST LIMIT 2",
    "SELECT $X FROM t ORDER BY c LIMIT 2",
    "SELECT $X FROM t ORDER BY b >= 1 LIMIT 1",
    "SELECT $X AS x FROM t ORDER BY x LIMIT 1",
    "SELECT $X FROM t ORDER BY b",
    "SELECT DISTINCT $X FROM t ORDER BY b LIMIT 1",
    "SELECT b, $X FROM t GROUP BY b ORDER BY b + 0 LIMIT 1",
    "SELECT b, $X FROM t GROUP BY b ORDER BY sum(b) LIMIT 2",
    "SELECT a, $X FROM t GROUP BY a ORDER BY max(b) LIMIT 1",
    "SELECT t.b, $X FROM t, t AS u WHERE u.a = 2 ORDER BY t.b LIMIT 1",
];

/// A query, hipp's rows (or the error), and the pin's.
type Answer = (String, Result<String, String>, Result<String, String>);

/// Each query on `T`: hipp's rows (or the error), and the pin's.
fn answers(tag: &str) -> Vec<Answer> {
    let path = common::temp_db(&format!("v17oq-{tag}"));
    common::cli(&path, T);
    let qs: Vec<String> = QS
        .iter()
        .map(|q| {
            q.replace(
                "$X",
                "CASE WHEN a=1 THEN abs(-9223372036854775808) ELSE a END",
            )
        })
        .collect();
    let got: Vec<Result<String, String>> = block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        // (The frozen VM, hidden `vm`, keeps its own OFFSET / ORDER BY
        // handling: these run on today's executor.)
        #[cfg(feature = "vm")]
        let cfg = Config::default().vm_off(true);
        #[cfg(not(feature = "vm"))]
        let cfg = Config::default();
        let mut db = Connection::open(fs, cfg).await.expect("open");
        let mut out = Vec::new();
        for q in qs.iter() {
            out.push(collect(&mut db, q).await.map_err(|e| format!("{e:?}")));
        }
        out
    });
    qs.into_iter()
        .zip(got)
        .map(|(q, h)| {
            let o = std::process::Command::new(common::oracle())
                .arg(&path)
                .arg(&q)
                .output()
                .expect("oracle runs");
            let p = if o.status.success() && o.stderr.is_empty() {
                Ok(String::from_utf8_lossy(&o.stdout).to_string())
            } else {
                Err(String::from_utf8_lossy(&o.stderr).to_string())
            };
            (q, h, p)
        })
        .collect()
}

#[test]
fn skipped_rows_compute_no_column() {
    let all = answers("rows");
    let ok = all.iter().filter(|(_, _, p)| p.is_ok()).count();
    assert!(
        ok * 3 >= all.len(),
        "only {ok} of {} answer on the pin",
        all.len()
    );
    let bad: Vec<String> = all
        .iter()
        .filter(|(_, h, p)| match (h, p) {
            (Ok(a), Ok(b)) => a != b,
            (Err(_), Err(_)) => false,
            _ => true,
        })
        .map(|(q, h, p)| format!("{q}\n  pin : {p:?}\n  hipp: {h:?}"))
        .collect();
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The default run also builds this file without `sql` and runs it
/// there: the synchronous executor's OFFSET and top-N rows (it shares
/// the target directory of `depth_tiny_v16`'s tiny build).
#[cfg(feature = "sql")]
#[test]
fn query_build_runs_these() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = concat!(env!("CARGO_TARGET_TMPDIR"), "/tiny-depth");
    let out = std::process::Command::new(cargo)
        .args([
            "test",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            "-p",
            "hipp",
            "--no-default-features",
            "--features",
            "std,query",
            "--test",
            "offset_query_v17",
        ])
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "the query build's run failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
