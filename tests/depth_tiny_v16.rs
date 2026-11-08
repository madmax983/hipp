//! v0.16 depth limits in the grammar the tiny profile claims
//! (CREATE TABLE / INSERT / scan SELECT), differentially vs the
//! pinned oracle (SQLite 3.54.1). Every test here compiles in the
//! tiny build too, and the default test run also builds and runs
//! them there (`tiny_build_runs_these`), against the tiny parser.
//!
//! The tiny grammar nests only one way — a run of unary signs
//! before a literal (`INSERT INTO t VALUES (- - 1)`, `WHERE rowid =
//! +-+1`), which hipp used to walk recursively, one stack frame per
//! sign: 100 000 signs overflowed any stack. The pin limits such a
//! run twice: one parser-stack symbol per sign (2500 in all, the
//! base depending on the position), and the tree height of its
//! sign nodes (1000; a `+` over a `+` node re-uses it). Tables,
//! result lists and VALUES rows stop at 2000 columns.

mod common;

use hipp::storage::{MemStorage, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    Rows(String),
    Err(String),
}

fn render(v: Option<Value<'_>>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(f)) => format!("{f:?}"),
        Some(Value::Text(b)) | Some(Value::Blob(b)) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn class<E: core::fmt::Debug>(e: &Error<E>) -> String {
    match e {
        Error::Parse { kind, .. } => format!("{kind:?}"),
        Error::Schema { kind } => format!("{kind:?}"),
        other => format!("other: {other:?}"),
    }
}

/// Run a `;`-separated script; the rows of every statement, or the
/// first error.
async fn script<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Out
where
    S::Error: core::fmt::Debug,
{
    let mut rows = Vec::new();
    for s in sql.split(";\n").filter(|s| !s.trim().is_empty()) {
        let mut st = match db.prepare(s).await {
            Ok(st) => st,
            Err(e) => return Out::Err(class(&e)),
        };
        loop {
            match st.step().await {
                Ok(Step::Row) => {
                    let line: Vec<String> = (0..st.column_count())
                        .map(|i| render(st.column_value(i)))
                        .collect();
                    rows.push(line.join("|"));
                }
                Ok(Step::Done) => break,
                Err(e) => return Out::Err(class(&e)),
            }
        }
    }
    Out::Rows(rows.join("\n"))
}

fn hipp(sql: &str) -> Out {
    let sql = sql.to_string();
    on_small_stack(move || {
        block_on(async move {
            let cfg = Config::default().statement_arena(16 * 1024, 64 * 1024 * 1024);
            // With the v0.4 grammar, the pin's limits (a sign run
            // is walked without recursion there too).
            #[cfg(feature = "query")]
            let cfg = cfg.depth_limits(
                hipp::sql::parser::PIN_EXPR_DEPTH,
                hipp::sql::parser::PIN_PARSER_DEPTH,
            );
            let mut db = Connection::open(MemStorage::new(), cfg)
                .await
                .expect("open");
            script(&mut db, &sql).await
        })
    })
}

fn pin(sql: &str) -> Out {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{sql};\n").as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        return Out::Err(if err.contains("Expression tree is too large") {
            "ExprTooDeep".into()
        } else if err.contains("Recursion limit") {
            "RecursionLimit".into()
        } else if err.contains("too many columns") {
            "TooManyColumns".into()
        } else {
            format!("other: {err}")
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Out::Rows(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

/// A small thread: hipp's own path to the parser needs about 320
/// KiB in a debug build before any nesting, 64 KiB in a release
/// one; the tiny grammar adds nothing per sign.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let stack = if cfg!(debug_assertions) {
        512 * 1024
    } else {
        256 * 1024
    };
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(f)
        .expect("thread spawns")
        .join()
        .expect("no panic")
}

/// hipp and the pin agree: the same rows, or both an error — of the
/// same class when it is a limit error.
fn agree(sql: &str) {
    let (got, want) = (hipp(sql), pin(sql));
    let ok = match (&got, &want) {
        (Out::Rows(a), Out::Rows(b)) => a == b,
        (Out::Err(a), Out::Err(b)) => a == b || (a.starts_with("other") || b.starts_with("other")),
        _ => false,
    };
    assert!(
        ok,
        "{}: hipp {got:?}, pin {want:?}",
        &sql[..sql.len().min(100)]
    );
}

/// The largest `n` the pin accepts (monotone, below `hi`).
fn pin_boundary(hi: usize, f: &dyn Fn(usize) -> String) -> usize {
    let (mut lo, mut hi) = (0usize, hi);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if matches!(pin(&f(mid)), Out::Rows(_)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

const T: &str = "CREATE TABLE t(a, b, c);\n";

/// Sign runs in every literal position of the tiny grammar, both
/// sides of the pin's boundary (searched live), and the rows they
/// wrote.
#[test]
fn sign_runs_match_the_oracle_at_their_boundaries() {
    let runs: [&str; 5] = ["+", "- ", "-+", "+-", "- +"];
    let contexts: [&str; 5] = [
        "INSERT INTO t VALUES ({}, 2, 3)",
        "INSERT INTO t VALUES (1, 2, {})",
        "INSERT INTO t VALUES (1, 2, 3), ({}, 2, 3)",
        "INSERT INTO t VALUES (1, 2, 3), (1, {}, 3)",
        "SELECT a FROM t WHERE rowid = {}",
    ];
    for c in contexts {
        for r in runs {
            let f = move |n: usize| {
                format!(
                    "{T}INSERT INTO t VALUES (7, 8, 9);\n{};\nSELECT a, b, c FROM t",
                    c.replace("{}", &format!("{}1", r.repeat(n)))
                )
            };
            let b = pin_boundary(3000, &f);
            assert!(b > 0, "{c} / {r}");
            agree(&f(b));
            agree(&f(b + 1));
        }
    }
}

/// Far past both limits: refused, no overflow.
#[test]
fn hostile_sign_runs_are_refused() {
    for r in ["+", "- ", "-+"] {
        for c in [
            "INSERT INTO t VALUES (1, 2, {})",
            "SELECT a FROM t WHERE rowid = {}",
        ] {
            let sql = format!("{T}{}", c.replace("{}", &format!("{}1", r.repeat(100_000))));
            assert!(matches!(hipp(&sql), Out::Err(_)), "{c} / {r}");
        }
    }
}

/// 2000 columns are a table, a result list, a VALUES row; 2001 are
/// the pin's "too many columns" (hipp made a 2001-column table the
/// pin could not open).
#[test]
fn column_counts_match_the_oracle() {
    let cols = |n: usize| {
        (0..n)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    for n in [2000, 2001] {
        agree(&format!("CREATE TABLE z({})", cols(n)));
        agree(&format!(
            "{T}INSERT INTO t VALUES (1, 2, 3);\nSELECT {} FROM t",
            vec!["a"; n].join(",")
        ));
    }
    // The 2000-column table hipp writes is one the pin reads.
    let db = common::temp_db("depth-tiny-cols");
    let sql = format!("CREATE TABLE z({})", cols(2000));
    let path = db.clone();
    on_small_stack(move || {
        block_on(async move {
            let fs = FileStorage::open(&path).expect("file");
            let mut c = Connection::open(fs, Config::default().statement_arena(16 * 1024, 1 << 22))
                .await
                .expect("open");
            assert_eq!(script(&mut c, &sql).await, Out::Rows(String::new()));
        })
    });
    assert_eq!(
        common::cli(&db, "SELECT count(*) FROM pragma_table_info('z');"),
        "2000\n"
    );
}

/// A parenthesised DEFAULT literal under a run of signs (v0.16
/// review fix): the parser folds it without the expression grammar,
/// and that path skipped both limits, so hipp stored a schema the
/// pin then refused to open ("malformed database schema"). Both
/// sides of the pin's boundary for the first column (7 symbols below
/// the run) and a later one (9), and the pin reads the file hipp
/// writes at the boundary.
#[test]
fn default_sign_runs_match_the_oracle() {
    for r in ["+ ", "- ", "+ - "] {
        for c in ["x DEFAULT ({}), y", "y, x DEFAULT ({})"] {
            let f = move |n: usize| {
                format!(
                    "CREATE TABLE u({});\nINSERT INTO u(y) VALUES (5);\nSELECT x, y FROM u",
                    c.replace("{}", &format!("{}1", r.repeat(n)))
                )
            };
            let b = pin_boundary(3000, &f);
            assert!(b > 0, "{c} / {r}");
            agree(&f(b));
            agree(&f(b + 1));
            let db = common::temp_db("depth-tiny-default");
            let create = format!(
                "CREATE TABLE u({})",
                c.replace("{}", &format!("{}1", r.repeat(b)))
            );
            let path = db.clone();
            on_small_stack(move || {
                block_on(async move {
                    let fs = FileStorage::open(&path).expect("file");
                    let cfg = Config::default().statement_arena(16 * 1024, 1 << 22);
                    #[cfg(feature = "query")]
                    let cfg = cfg.depth_limits(
                        hipp::sql::parser::PIN_EXPR_DEPTH,
                        hipp::sql::parser::PIN_PARSER_DEPTH,
                    );
                    let mut c = Connection::open(fs, cfg).await.expect("open");
                    assert_eq!(script(&mut c, &create).await, Out::Rows(String::new()));
                })
            });
            assert_eq!(common::cli(&db, "PRAGMA integrity_check;"), "ok\n");
        }
    }
    // Far past both: refused, no overflow.
    for r in ["+", "- "] {
        let sql = format!("CREATE TABLE u(x DEFAULT ({}1))", r.repeat(100_000));
        assert!(matches!(hipp(&sql), Out::Err(_)), "{r}");
    }
}

/// (page type, cell count) of every page of the file at `path`.
fn page_shapes(path: &std::path::Path) -> Vec<(u8, u16)> {
    let d = std::fs::read(path).expect("read db");
    let page = 4096;
    (0..d.len() / page)
        .map(|p| {
            let h = p * page + if p == 0 { 100 } else { 0 };
            (d[h], u16::from_be_bytes([d[h + 3], d[h + 4]]))
        })
        .collect()
}

/// A schema row that no longer fits page 1 beside the file header
/// (v0.16 review fix: inserting it panicked with a subtract overflow
/// when page 1 held no row yet). The pin moves page 1's rows down
/// into one child page when they fit there (page 1 becomes an
/// interior page with no cells): hipp writes the same pages, around
/// the sizes where the row stops fitting page 1, and the pin reads
/// the file.
#[test]
fn schema_rows_past_page_one_match_the_oracle() {
    for (pre, n) in [
        ("", 3900),
        ("", 3950),
        ("", 3990),
        ("", 4050),
        ("CREATE TABLE a1(x);\nCREATE TABLE a2(x);\n", 3850),
        ("CREATE TABLE a1(x);\nCREATE TABLE a2(x);\n", 3900),
        ("CREATE TABLE a1(x);\nCREATE TABLE a2(x);\n", 3950),
    ] {
        let sql = format!("{pre}CREATE TABLE u(a DEFAULT '{}')", "x".repeat(n));
        let (mine, theirs) = (
            common::temp_db("depth-tiny-page1-h"),
            common::temp_db("depth-tiny-page1-p"),
        );
        let (path, script_sql) = (mine.clone(), sql.clone());
        on_small_stack(move || {
            block_on(async move {
                let fs = FileStorage::open(&path).expect("file");
                let cfg = Config::default().statement_arena(16 * 1024, 1 << 22);
                let mut c = Connection::open(fs, cfg).await.expect("open");
                assert_eq!(script(&mut c, &script_sql).await, Out::Rows(String::new()));
            })
        });
        common::cli(&theirs, &format!("{sql};"));
        assert_eq!(
            page_shapes(&mine),
            page_shapes(&theirs),
            "{n} after {pre:?}"
        );
        assert_eq!(common::cli(&mine, "PRAGMA integrity_check;"), "ok\n");
        assert_eq!(
            common::cli(
                &mine,
                "SELECT length(sql) > 3800 FROM sqlite_schema WHERE name = 'u';"
            ),
            "1\n"
        );
    }
}

/// The default test run builds this file without the v0.4 grammar
/// (`--no-default-features --features std`, its own target
/// directory) and requires every test above to pass there.
#[cfg(feature = "query")]
#[test]
fn tiny_build_runs_these() {
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
            "std",
            "--test",
            "depth_tiny_v16",
        ])
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("run cargo");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the tiny build of depth_tiny_v16 failed:\n{stdout}\n{stderr}"
    );
    for name in [
        "sign_runs_match_the_oracle_at_their_boundaries",
        "hostile_sign_runs_are_refused",
        "column_counts_match_the_oracle",
        "default_sign_runs_match_the_oracle",
        "schema_rows_past_page_one_match_the_oracle",
    ] {
        assert!(
            stdout.contains(&format!("test {name} ... ok")),
            "{name} did not pass in the tiny build:\n{stdout}"
        );
    }
    assert!(stdout.contains("5 passed"), "{stdout}");
}
