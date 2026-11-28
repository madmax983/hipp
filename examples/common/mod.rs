//! Shared helpers for the profile examples (`tiny`, `full`, `max`,
//! `xl`). Not an example itself: cargo only discovers
//! `examples/*.rs` and `examples/*/main.rs`.
//!
//! hipp's API is one prepared statement at a time: `prepare` one
//! SQL statement, `step` it until `Step::Done`, read columns while it
//! is on a `Step::Row`. These helpers wrap that loop so the examples
//! can show SQL next to the rows it returns, and check every answer.

#![allow(dead_code)] // each example uses a different subset

use hipp::{Connection, Step, Storage, Value};

/// Render one column value the way the `sqlite3` shell's list mode
/// does: integers and reals as numbers, text as-is, blobs as hex
/// literals, NULL as an empty string.
pub fn render(v: Option<Value<'_>>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(f)) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        Some(Value::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Some(Value::Blob(b)) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            format!("X'{hex}'")
        }
    }
}

/// Shorten long SQL for messages.
pub fn short(sql: &str) -> String {
    if sql.len() > 120 {
        format!("{}... ({} bytes)", &sql[..100], sql.len())
    } else {
        sql.to_string()
    }
}

/// Run one statement to completion and return its rows, each row
/// rendered as `a|b|c` (list mode). Panics with the SQL on error.
pub async fn rows<S>(db: &mut Connection<S>, sql: &str) -> Vec<String>
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let mut st = match db.prepare(sql).await {
        Ok(st) => st,
        Err(e) => panic!("prepare failed: {}\n  -> {e:?}", short(sql)),
    };
    let mut out = Vec::new();
    loop {
        match st.step().await {
            Ok(Step::Row) => {
                let n = st.column_count();
                let cells: Vec<String> = (0..n).map(|i| render(st.column_value(i))).collect();
                out.push(cells.join("|"));
            }
            Ok(Step::Done) => break,
            Err(e) => panic!("step failed: {}\n  -> {e:?}", short(sql)),
        }
    }
    out
}

/// Run a statement that returns no rows (DDL, DML, transaction
/// control). Panics if it fails.
pub async fn exec<S>(db: &mut Connection<S>, sql: &str)
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let got = rows(db, sql).await;
    assert!(got.is_empty(), "{} returned rows: {got:?}", short(sql));
}

/// Run several statements, one after another.
pub async fn exec_all<S>(db: &mut Connection<S>, sqls: &[&str])
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    for sql in sqls {
        exec(db, sql).await;
    }
}

/// Run a query, print it with its rows, and check the rows.
pub async fn check<S>(db: &mut Connection<S>, sql: &str, expected: &[&str])
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let got = rows(db, sql).await;
    println!("sqlite> {sql}");
    for r in &got {
        println!("{r}");
    }
    let want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
    // HIPP_EXAMPLE_KEEP_GOING=1 reports every mismatch instead of
    // stopping at the first (handy when editing an example).
    if got != want && std::env::var_os("HIPP_EXAMPLE_KEEP_GOING").is_some() {
        println!("!! MISMATCH for: {sql}\n!!   want {want:?}\n!!   got  {got:?}");
        return;
    }
    assert_eq!(got, want, "unexpected rows for: {sql}");
}

/// Run `EXPLAIN QUERY PLAN <sql>` and check its `detail` column (the
/// fourth). The `id` / `parent` numbers are hipp's own, not SQLite's
/// bytecode addresses, so only the plan text is compared.
pub async fn plan<S>(db: &mut Connection<S>, sql: &str, expected: &[&str])
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let eqp = format!("EXPLAIN QUERY PLAN {sql}");
    let got: Vec<String> = rows(db, &eqp)
        .await
        .into_iter()
        .map(|r| r.splitn(4, '|').nth(3).unwrap_or_default().to_string())
        .collect();
    println!("sqlite> {eqp}");
    for r in &got {
        println!("`--{r}");
    }
    let want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
    if got != want && std::env::var_os("HIPP_EXAMPLE_KEEP_GOING").is_some() {
        println!("!! MISMATCH for plan: {sql}\n!!   want {want:?}\n!!   got  {got:?}");
        return;
    }
    assert_eq!(got, want, "unexpected plan for: {sql}");
}

/// Run a statement that must fail, and return the error's debug text
/// (which names the remedy, e.g. `Schema(UniqueViolation)`).
pub async fn fails<S>(db: &mut Connection<S>, sql: &str) -> String
where
    S: Storage,
    S::Error: core::fmt::Debug,
{
    let res = match db.prepare(sql).await {
        Err(e) => Err(format!("{e:?}")),
        Ok(mut st) => loop {
            match st.step().await {
                Ok(Step::Row) => {}
                Ok(Step::Done) => break Ok(()),
                Err(e) => break Err(format!("{e:?}")),
            }
        },
    };
    match res {
        Ok(()) => panic!("expected an error from: {}", short(sql)),
        Err(e) => {
            println!("sqlite> {}\nError: {e}", short(sql));
            e
        }
    }
}

/// Print a section heading.
pub fn section(title: &str) {
    println!("\n== {title} ==");
}
