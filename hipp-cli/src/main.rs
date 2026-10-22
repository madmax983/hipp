//! hipp-cli: a shell over the hipp library (SPEC §9.4, decision
//! D4). Interactive + one-shot modes over the library's SQL
//! subset, list-format output (`|` separator, NULL = empty) that
//! is byte-diffable against the pinned SQLite oracle CLI.
//!
//! Its first job is differential testing; demo surface second.
//! Dot-commands: `.tables`, `.schema`, `.dump`, `.open`, `.exit`
//! (alias `.quit`). No line editing, no `.import`/`.export`, no
//! format/flag parity beyond diffable basics.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, MemStorage, Step, Value};

/// The open database: a file or `:memory:`.
enum Db {
    File(Connection<FileStorage>),
    Mem(Connection<MemStorage>),
}

fn open_db(path: &str) -> Result<Db, String> {
    block_on(async {
        if path == ":memory:" {
            let conn = Connection::open(MemStorage::new(), Config::default())
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok(Db::Mem(conn))
        } else {
            let fs = FileStorage::open(std::path::Path::new(path))
                .map_err(|e| format!("{e:?}"))?;
            let conn = Connection::open(fs, Config::default())
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok(Db::File(conn))
        }
    })
}

/// Format an `f64` the way SQLite's list mode prints reals
/// (`%!.15g`): 15 significant digits, fixed notation inside
/// [-4, 15) exponent range, at least one fractional digit.
fn fmt_real(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    if !f.is_finite() {
        // SQLite prints these as Inf / -Inf in list mode.
        return if f > 0.0 {
            "Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    // 15 significant digits in scientific form: d.dddddddddddddd e±X
    let sci = format!("{:.14e}", f.abs());
    let (mant, exp) = sci.split_once('e').expect("scientific form");
    let exp10: i32 = exp.parse().expect("exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let sign = if f < 0.0 { "-" } else { "" };
    if !(-4..15).contains(&exp10) {
        // Scientific: one digit, point, rest (at least one digit
        // after the point), e, signed two-digit exponent.
        let frac = if digits.len() > 1 { &digits[1..] } else { "0" };
        return format!("{sign}{}.{frac}e{exp10:+03}", &digits[0..1]);
    }
    if exp10 >= 0 {
        let split = (exp10 + 1) as usize;
        if digits.len() <= split {
            let int = format!("{digits:0<width$}", width = split);
            return format!("{sign}{int}.0");
        }
        format!("{sign}{}.{}", &digits[..split], &digits[split..])
    } else {
        let zeros = (-exp10 - 1) as usize;
        format!("{sign}0.{}{digits}", "0".repeat(zeros))
    }
}

/// SQL literal rendering for `.dump`: text quoted with `''`
/// doubling, blobs as `X'HEX'`, reals round-trip exact.
fn fmt_literal(v: Value<'_>) -> Vec<u8> {
    match v {
        Value::Null => b"NULL".to_vec(),
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Real(f) => format!("{f:?}").into_bytes(),
        Value::Text(b) => {
            let mut out = vec![b'\''];
            for &c in b {
                if c == b'\'' {
                    out.push(b'\'');
                }
                out.push(c);
            }
            out.push(b'\'');
            out
        }
        Value::Blob(b) => {
            let mut out = b"X'".to_vec();
            for &c in b {
                out.extend_from_slice(format!("{c:02X}").as_bytes());
            }
            out.push(b'\'');
            out
        }
    }
}

/// One output row in list format: `|` separators, NULL empty,
/// value bytes written raw (text/blob may be non-UTF-8).
fn write_row(out: &mut impl Write, vals: &[Vec<u8>]) {
    for (i, v) in vals.iter().enumerate() {
        if i > 0 {
            let _ = out.write_all(b"|");
        }
        let _ = out.write_all(v);
    }
    let _ = out.write_all(b"\n");
}

/// The oracle shell's list-mode byte rendering (verified byte by
/// byte against the pinned oracle): bytes below 0x20 render in
/// caret notation (`^` + byte+0x40), except tab and newline, which
/// pass through raw; everything else passes through raw. TEXT
/// values are treated as C strings and stop at the first NUL —
/// BLOBs render in full.
fn shell_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    for &c in b {
        if c < 0x20 && c != b'\t' && c != b'\n' {
            out.push(b'^');
            out.push(c + 0x40);
        } else {
            out.push(c);
        }
    }
    out
}

fn value_bytes(v: Value<'_>) -> Vec<u8> {
    match v {
        Value::Null => Vec::new(),
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Real(f) => fmt_real(f).into_bytes(),
        Value::Text(b) => {
            let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
            shell_bytes(&b[..end])
        }
        Value::Blob(b) => shell_bytes(b),
    }
}

/// Run one SQL statement, printing any result rows. `Err` carries
/// the user-facing message (hipp's remedy-named Debug form).
async fn run_statement<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
    out: &mut impl Write,
) -> Result<(), String>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
    if st.column_count() > 0 {
        loop {
            match st.step().await.map_err(|e| format!("{e:?}"))? {
                Step::Row => {
                    let vals: Vec<Vec<u8>> = (0..st.column_count())
                        .map(|i| value_bytes(st.column_value(i).unwrap_or(Value::Null)))
                        .collect();
                    write_row(out, &vals);
                }
                Step::Done => break,
            }
        }
        Ok(())
    } else {
        st.step().await.map_err(|e| format!("{e:?}"))?;
        Ok(())
    }
}

/// Split a script into statements on `;` outside of string
/// literals, quoted identifiers and `--` comments.
fn split_statements(script: &str) -> Vec<String> {
    let b = script.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' | b'`' => {
                let close = b[i];
                i += 1;
                while i < b.len() && b[i] != close {
                    i += 1;
                }
                i = (i + 1).min(b.len());
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b';' => {
                let s = script[start..=i].trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let tail = script[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// What a dot-command wants the shell loop to do next.
enum Flow {
    Continue,
    Exit,
}

async fn dot_command<S: hipp::Storage>(
    db: &mut Connection<S>,
    line: &str,
    out: &mut impl Write,
) -> Result<Flow, String>
where
    S::Error: core::fmt::Debug,
{
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).unwrap_or("");
    match cmd {
        ".exit" | ".quit" => Ok(Flow::Exit),
        ".tables" => {
            for t in db.tables().iter() {
                let _ = out.write_all(t.name);
                let _ = out.write_all(b"\n");
            }
            Ok(Flow::Continue)
        }
        ".schema" => {
            for t in db.tables().iter() {
                if !arg.is_empty() && !t.name.eq_ignore_ascii_case(arg.as_bytes()) {
                    continue;
                }
                let _ = out.write_all(t.sql);
                let _ = out.write_all(b";\n");
            }
            Ok(Flow::Continue)
        }
        ".dump" => {
            let _ = out.write_all(b"BEGIN TRANSACTION;\n");
            // Snapshot the catalog first (names as bytes) so the
            // per-table SELECTs can borrow the connection freely.
            let tables: Vec<(Vec<u8>, Vec<u8>)> = db
                .tables()
                .iter()
                .map(|t| (t.name.to_vec(), t.sql.to_vec()))
                .collect();
            for (name, sql) in &tables {
                let _ = out.write_all(sql);
                let _ = out.write_all(b";\n");
                let name = String::from_utf8_lossy(name);
                let mut st = db
                    .prepare(&format!("SELECT * FROM {name}"))
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                loop {
                    match st.step().await.map_err(|e| format!("{e:?}"))? {
                        Step::Row => {
                            let mut line = format!("INSERT INTO {name} VALUES(").into_bytes();
                            for i in 0..st.column_count() {
                                if i > 0 {
                                    line.extend_from_slice(b",");
                                }
                                line.extend_from_slice(&fmt_literal(
                                    st.column_value(i).unwrap_or(Value::Null),
                                ));
                            }
                            line.extend_from_slice(b");\n");
                            let _ = out.write_all(&line);
                        }
                        Step::Done => break,
                    }
                }
            }
            let _ = out.write_all(b"COMMIT;\n");
            Ok(Flow::Continue)
        }
        _ => Err(format!("unknown command: {cmd}")),
    }
}

/// Execute one input unit (statement or dot-command) against the
/// current database, handling `.open` by swapping `db`.
async fn dispatch(
    db: &mut Db,
    input: &str,
    out: &mut impl Write,
) -> Result<Flow, String> {
    let trimmed = input.trim();
    if trimmed.starts_with('.') {
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        if parts.next() == Some(".open") {
            let path = parts.next().map(str::trim).unwrap_or("");
            if path.is_empty() {
                return Err("usage: .open <path|:memory:>".to_string());
            }
            *db = open_db(path)?;
            return Ok(Flow::Continue);
        }
        match db {
            Db::File(c) => dot_command(c, trimmed, out).await,
            Db::Mem(c) => dot_command(c, trimmed, out).await,
        }
    } else {
        match db {
            Db::File(c) => run_statement(c, trimmed, out).await.map(|()| Flow::Continue),
            Db::Mem(c) => run_statement(c, trimmed, out).await.map(|()| Flow::Continue),
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: hipp <database|:memory:> [sql]");
    eprintln!("       hipp <database|:memory:>            (interactive / stdin batch)");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.len() > 2 {
        return usage();
    }
    let mut db = match open_db(&args[0]) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();

    // One-shot mode: run the SQL argument (possibly several
    // statements), stop at the first error.
    if let Some(sql) = args.get(1) {
        for stmt in split_statements(sql) {
            match block_on(dispatch(&mut db, &stmt, &mut out)) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    // Interactive / stdin-batch mode.
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!("hipp — reads and writes SQLite database files (format pin: SQLite 3.54.1)");
        println!("Enter SQL or dot-commands (.tables .schema .dump .open .exit)");
    }
    let mut lines = stdin.lock().lines();
    let mut buffer = String::new();
    loop {
        if interactive {
            print!("{}", if buffer.is_empty() { "hipp> " } else { "...> " });
            let _ = std::io::stdout().flush();
        }
        let line = match lines.next() {
            Some(Ok(l)) => l,
            Some(Err(e)) => {
                eprintln!("Error: {e}");
                return ExitCode::FAILURE;
            }
            None => break,
        };
        let line = line.trim_end();
        if buffer.is_empty() && line.trim_start().starts_with('.') {
            match block_on(dispatch(&mut db, line.trim(), &mut out)) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return ExitCode::SUCCESS,
                Err(e) => eprintln!("Error: {e}"),
            }
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(line);
        if buffer.trim_end().ends_with(';') {
            let script = std::mem::take(&mut buffer);
            for stmt in split_statements(&script) {
                match block_on(dispatch(&mut db, &stmt, &mut out)) {
                    Ok(Flow::Continue) => {}
                    Ok(Flow::Exit) => return ExitCode::SUCCESS,
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
        }
    }
    // A trailing unterminated statement still runs at EOF.
    if !buffer.trim().is_empty() {
        for stmt in split_statements(&buffer) {
            match block_on(dispatch(&mut db, &stmt, &mut out)) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    let _ = out.flush();
    ExitCode::SUCCESS
}
