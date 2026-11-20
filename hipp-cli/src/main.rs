//! hipp-cli: a shell over the hipp library (SPEC §9.4, decision
//! D4). Interactive + one-shot modes over the library's SQL
//! subset, output byte-diffable against the pinned SQLite oracle
//! CLI (3.54.1).
//!
//! Its first job is differential testing; demo surface second.
//! Dot-commands (v0.17, the pin's text and errors; `shell`): `.bail`,
//! `.changes`, `.databases`, `.dump` (with `--data-only`, `--nosys`,
//! `--preserve-rowids`, `--newlines`), `.echo`, `.exit`, `.fullschema`,
//! `.headers`, `.help`, `.indexes`, `.limit`, `.mode` (`list`, `csv`,
//! `column`, `line`, `box`, `markdown`, `table`, `quote`, `json`,
//! `tabs`, `insert`, `ascii`; `render`), `.nullvalue`, `.once`,
//! `.open`, `.output`, `.parameter`, `.print`, `.quit`, `.read`,
//! `.schema`, `.separator`, `.show`, `.tables`, `.timer`, `.width`.
//! No line editing, no `.import`; the pin's other commands, modes and
//! options are refused by name.
//!
//! The command line is the pin shell's (v0.17): `[OPTIONS]
//! [FILENAME [SQL...]]`, the database `:memory:` when none is
//! named and opened only when the input first needs it; of the
//! options hipp takes `-help`, `-version` (each also as `--x`) and
//! `--` / `-`, which end them, and it refuses the pin's others.
//!
//! Bound parameters (v0.16) work as in the oracle shell: every
//! parameter of a statement is bound from the `.parameter set`
//! table by its name (a nameless `?` by `?N`), NULL when the
//! table has no such key.

mod help;
mod lazy;
mod names;
mod render;
mod shell;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, MemStorage, OwnedValue, Step, Value};

/// The `.parameter` bindings (v0.16): the oracle shell's
/// `temp.sqlite_parameters` table — key (the parameter's name as
/// written, `?N` for a nameless `?`) to value, in key order.
pub type Params = std::collections::BTreeMap<String, OwnedValue>;

/// The open database: a file or `:memory:`.
pub enum Db {
    /// A file, written only once something is (`lazy::LazyFile`, v0.17).
    File(Connection<lazy::LazyFile>),
    Mem(Connection<MemStorage>),
}

fn open_db(path: &str) -> Result<Db, String> {
    block_on(async {
        if path == ":memory:" {
            let conn = Connection::open(MemStorage::memory_db(), cli_config())
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok(Db::Mem(conn))
        } else {
            let fs = FileStorage::open(std::path::Path::new(path)).map_err(|e| format!("{e:?}"))?;
            let fs = lazy::LazyFile::new(fs);
            let conn = Connection::open(fs, cli_config())
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok(Db::File(conn))
        }
    })
}

/// Close the database as the oracle shell does at exit and on
/// `.open` (v0.16): in WAL mode the log is checkpointed and the
/// `-wal` file removed (`Connection::close`).
fn close_db(db: Db) -> Result<(), String> {
    block_on(async {
        match db {
            Db::File(c) => c.close().await.map_err(|e| format!("{e:?}")),
            Db::Mem(c) => c.close().await.map_err(|e| format!("{e:?}")),
        }
    })
}

/// The CLI's engine configuration: the defaults (under `std` they
/// include the system clock and, with `datetime`, the process's
/// time zone as `TZ` names it, so `'now'` and `'localtime'` read
/// what the pin's shell reads), the pin's nesting limit for views
/// and CTEs (999 levels read, 1000 are "nested too deep") and its
/// expression and parser depth
/// limits (1000, 2500), which `main`'s thread has the stack for,
/// with a statement arena big enough to expand them (the default
/// 64 KiB holds about 800 nested views, or 2500 tokens).
fn cli_config() -> Config {
    let d = Config::default();
    let (init, max) = (d.stmt_init, d.stmt_max.max(4 * 1024 * 1024));
    d.max_nesting(1000)
        .max_trigger_depth(1000)
        .depth_limits(
            hipp::sql::parser::PIN_EXPR_DEPTH,
            hipp::sql::parser::PIN_PARSER_DEPTH,
        )
        .statement_arena(init, max)
}

/// The stack of the thread the CLI runs on: room for
/// `cli_config`'s 1000 nested view / CTE levels in a debug build
/// (each costs the planner and executor tens of KiB), for the
/// pin's depth limits (about 13 MiB in a debug build) and for its
/// 999 nested trigger programs (about 250 MiB in a debug build,
/// 20 MiB in a release one). Only the
/// pages a statement touches are ever committed.
const STACK_BYTES: usize = 512 * 1024 * 1024;

/// A column name as the shell's `.schema` view comment prints it
/// (v0.16, CI against the pin: `/* v(a,"+b",b,Q) */`): bare when
/// it is a plain identifier (an ASCII letter or `_`, then letters,
/// digits, `_`) that is not an SQL keyword, else double-quoted
/// with `"` doubled. The keyword list is SQLite's documented one
/// (147 words, sqlite.org/lang_keywords.html); the keyword case is
/// inferred from the shell's quoting rule, not seen in CI.
/// `CREATE TABLE x` as `CREATE TABLE temp.x` (and likewise for an
/// index or a view): the oracle shell's `.schema` form of a TEMP
/// database object.
fn temp_text(sql: &[u8]) -> Vec<u8> {
    prefixed_text(sql, b"temp")
}

/// `CREATE TABLE x` as `CREATE TABLE <db>.x` (and likewise for an
/// index, a view or a trigger): the oracle shell's `.schema` form of
/// an object of a TEMP (`temp`) or attached database (its name,
/// quoted as `shell_quote_name` quotes).
fn prefixed_text(sql: &[u8], db: &[u8]) -> Vec<u8> {
    for p in [
        &b"CREATE TABLE "[..],
        b"CREATE UNIQUE INDEX ",
        b"CREATE INDEX ",
        b"CREATE VIEW ",
        b"CREATE TRIGGER ",
    ] {
        if sql.starts_with(p) {
            let mut v = p.to_vec();
            v.extend_from_slice(db);
            v.push(b'.');
            v.extend_from_slice(&sql[p.len()..]);
            return v;
        }
    }
    sql.to_vec()
}

/// `.schema [name]` for one database (`temp`: the TEMP database):
/// every object with SQL text in schema (rowid) order, as the oracle
/// shell prints them — `name` matches an object's name or its
/// table's — each view followed by its `/* name(columns) */` comment.
/// The column names are the view's planned ones (v0.16: views
/// execute); a view that cannot be planned — a missing table, a
/// column list of the wrong length, a cycle — gets no comment, as in
/// the oracle shell.
///
/// `attached`: the attached database at that index (v0.16), printed
/// as the oracle shell prints one — every name prefixed with its
/// database's name, quoted as a column name would be.
async fn print_schema<S: hipp::Storage>(
    db: &mut Connection<S>,
    arg: &str,
    out: &mut dyn Write,
    temp: bool,
    attached: Option<usize>,
) {
    let att_name = attached.and_then(|i| db.attached_db(i).map(|(n, _)| n.to_vec()));
    let quoted = att_name.as_deref().map(shell_quote_name);
    let att_sql = att_name.as_ref().map(|n| {
        let mut q = String::from("SELECT type, name, tbl_name, sql FROM \"");
        q.push_str(&String::from_utf8_lossy(n).replace('"', "\"\""));
        q.push_str("\".sqlite_schema");
        q
    });
    let sql = if let Some(q) = att_sql.as_deref() {
        q
    } else if temp {
        "SELECT type, name, tbl_name, sql FROM sqlite_temp_schema"
    } else {
        "SELECT type, name, tbl_name, sql FROM main.sqlite_schema"
    };
    // (type, name, tbl_name, sql) per schema row.
    type SchemaRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);
    let mut rows: Vec<SchemaRow> = Vec::new();
    if let Ok(mut st) = db.prepare(sql).await {
        while let Ok(Step::Row) = st.step().await {
            let col = |i| st.column_bytes(i).map(<[u8]>::to_vec);
            if let (Some(t), Some(n), Some(tb), Some(q)) = (col(0), col(1), col(2), col(3)) {
                rows.push((t, n, tb, q));
            }
        }
    }
    for (ty, name, tbl, sql) in rows.iter() {
        let a = arg.as_bytes();
        if !arg.is_empty() && !name.eq_ignore_ascii_case(a) && !tbl.eq_ignore_ascii_case(a) {
            continue;
        }
        let shown = match &quoted {
            Some(q) => prefixed_text(sql, q),
            None if temp => temp_text(sql),
            None => sql.clone(),
        };
        let _ = out.write_all(&shown);
        let _ = out.write_all(b";\n");
        // A virtual table (v0.16) gets the view comment too: its
        // declared columns, as the oracle shell lists them.
        let vtab = ty.as_slice() == b"table"
            && sql.len() >= 20
            && sql[..20].eq_ignore_ascii_case(b"CREATE VIRTUAL TABLE");
        if ty.as_slice() != b"view" && !vtab {
            continue;
        }
        let cols = if vtab {
            if attached.is_some() || temp {
                continue;
            }
            let q = format!(
                "PRAGMA table_info(\"{}\")",
                String::from_utf8_lossy(name).replace('"', "\"\"")
            );
            let mut cols = Vec::new();
            if let Ok(mut st) = db.prepare(&q).await {
                while let Ok(Step::Row) = st.step().await {
                    cols.push(st.column_bytes(1).unwrap_or_default().to_vec());
                }
            }
            if cols.is_empty() {
                continue;
            }
            Some(cols)
        } else if let Some(i) = attached {
            db.attached_db(i)
                .and_then(|(_, a)| a.view_columns(name))
                .map(|v| v.iter().map(|c| c.as_slice().to_vec()).collect())
        } else if temp {
            db.temp_db()
                .and_then(|t| t.view_columns(name))
                .map(|v| v.iter().map(|c| c.as_slice().to_vec()).collect())
        } else {
            db.view_columns(name)
                .map(|v| v.iter().map(|c| c.as_slice().to_vec()).collect())
        };
        let Some(cols): Option<Vec<Vec<u8>>> = cols else {
            continue;
        };
        let _ = out.write_all(b"/* ");
        if let Some(q) = &quoted {
            let _ = out.write_all(q);
            let _ = out.write_all(b".");
        } else if temp {
            let _ = out.write_all(b"temp.");
        }
        let _ = out.write_all(&shell_quote_name(name));
        let _ = out.write_all(b"(");
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                let _ = out.write_all(b",");
            }
            let _ = out.write_all(&shell_quote_name(c.as_slice()));
        }
        let _ = out.write_all(b") */\n");
    }
}

fn shell_quote_name(c: &[u8]) -> Vec<u8> {
    let plain = c
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && c.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if plain
        && !SQL_KEYWORDS
            .iter()
            .any(|k| k.as_bytes().eq_ignore_ascii_case(c))
    {
        return c.to_vec();
    }
    let mut q = vec![b'"'];
    for &b in c {
        if b == b'"' {
            q.push(b'"');
        }
        q.push(b);
    }
    q.push(b'"');
    q
}

/// SQLite's keywords (sqlite.org/lang_keywords.html), for
/// `shell_quote_name`.
const SQL_KEYWORDS: [&str; 147] = [
    "ABORT",
    "ACTION",
    "ADD",
    "AFTER",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "ATTACH",
    "AUTOINCREMENT",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DO",
    "DROP",
    "EACH",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUSIVE",
    "EXISTS",
    "EXPLAIN",
    "FAIL",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GENERATED",
    "GLOB",
    "GROUP",
    "GROUPS",
    "HAVING",
    "IF",
    "IGNORE",
    "IMMEDIATE",
    "IN",
    "INDEX",
    "INDEXED",
    "INITIALLY",
    "INNER",
    "INSERT",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATCH",
    "MATERIALIZED",
    "NATURAL",
    "NO",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OTHERS",
    "OUTER",
    "OVER",
    "PARTITION",
    "PLAN",
    "PRAGMA",
    "PRECEDING",
    "PRIMARY",
    "QUERY",
    "RAISE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TIES",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
];

/// Format an `f64` the way SQLite's list mode prints reals: the
/// pin's CLI renders a REAL exactly as its core REAL-to-text
/// conversion (CAST AS TEXT) does — CI showed it (v0.16) — so this
/// is the core's `render_real`, the one source of truth.
fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(f, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// `.dump`'s literal, as the pin shell's insert mode writes it: a
/// blob as `x'hex'` (v0.16 review fix: hipp wrote `X'HEX'`); a real
/// as `%.16g` (`1`, `1e+300`, `3e-05`), an infinity as `9e+999`; a
/// text up to its first NUL, quoted with `''` doubling, and as
/// `unistr('...')` — each control character `\uXXXX`, each `\`
/// doubled — when it holds one (v0.17 fix: hipp wrote `1.0`, `1e300`,
/// raw newlines).
fn dump_literal(v: Value<'_>) -> Vec<u8> {
    match v {
        Value::Blob(b) => {
            let mut out = b"x'".to_vec();
            for &c in b {
                out.extend_from_slice(format!("{c:02x}").as_bytes());
            }
            out.push(b'\'');
            out
        }
        Value::Real(f) if f.is_infinite() => {
            if f > 0.0 {
                b"9e+999".to_vec()
            } else {
                b"-9e+999".to_vec()
            }
        }
        Value::Real(f) => {
            let mut fmt = hipp::arena::AVec::new();
            fmt.extend_from_slice(b"%.16g");
            match hipp::func::printf::<()>(&[OwnedValue::Text(fmt), OwnedValue::Real(f)]) {
                Ok(OwnedValue::Text(t)) => t.as_slice().to_vec(),
                _ => fmt_real(f).into_bytes(),
            }
        }
        Value::Text(b) => {
            let b = &b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())];
            let esc = b.iter().any(|c| *c < 0x20);
            let mut out: Vec<u8> = if esc {
                b"unistr('".to_vec()
            } else {
                b"'".to_vec()
            };
            for &c in b {
                match c {
                    b'\'' => out.extend_from_slice(b"''"),
                    b'\\' if esc => out.extend_from_slice(b"\\\\"),
                    c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
                    c => out.push(c),
                }
            }
            out.extend_from_slice(if esc { b"')" } else { b"'" });
            out
        }
        v => fmt_literal(v),
    }
}

/// A name as the pin shell's `.dump` writes it after `INSERT INTO`
/// (its `quoteChar`): in `"..."` unless it is a letter or `_` then
/// letters, digits and `_`, and no keyword (v0.17 fix: hipp wrote
/// every name bare, and could not read a table back whose name needs
/// quoting).
fn dump_name(n: &[u8]) -> Vec<u8> {
    let plain = n
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && n.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
        && !KEYWORDS
            .iter()
            .any(|k| k.as_bytes().eq_ignore_ascii_case(n));
    if plain {
        return n.to_vec();
    }
    let mut out = vec![b'"'];
    for &c in n {
        if c == b'"' {
            out.push(b'"');
        }
        out.push(c);
    }
    out.push(b'"');
    out
}

/// The pin's keywords (`sqlite3_keyword_check`).
const KEYWORDS: &[&str] = &[
    "ABORT",
    "ACTION",
    "ADD",
    "AFTER",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "ATTACH",
    "AUTOINCREMENT",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DO",
    "DROP",
    "EACH",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUSIVE",
    "EXISTS",
    "EXPLAIN",
    "FAIL",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GENERATED",
    "GLOB",
    "GROUP",
    "GROUPS",
    "HAVING",
    "IF",
    "IGNORE",
    "IMMEDIATE",
    "IN",
    "INDEX",
    "INDEXED",
    "INITIALLY",
    "INNER",
    "INSERT",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATCH",
    "MATERIALIZED",
    "NATURAL",
    "NO",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OTHERS",
    "OUTER",
    "OVER",
    "PARTITION",
    "PLAN",
    "PRAGMA",
    "PRECEDING",
    "PRIMARY",
    "QUERY",
    "RAISE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TIES",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
];

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

/// The oracle shell's EXPLAIN QUERY PLAN rendering (`eqp_render`):
/// `QUERY PLAN`, then each row under its parent, `|--` before a row
/// with a later sibling and `` `-- `` before the last, children
/// indented by `|  ` or three blanks; nothing for a plan with no
/// rows. Like the shell's 100-byte prefix buffer, it stops descending
/// past 31 levels.
fn eqp_render(out: &mut dyn Write, rows: &[(i64, i64, Vec<u8>)]) {
    fn level(out: &mut dyn Write, rows: &[(i64, i64, Vec<u8>)], parent: i64, prefix: &mut Vec<u8>) {
        let kids: Vec<&(i64, i64, Vec<u8>)> = rows.iter().filter(|r| r.1 == parent).collect();
        for (i, r) in kids.iter().enumerate() {
            let more = i + 1 < kids.len();
            let _ = out.write_all(prefix);
            let _ = out.write_all(if more { b"|--" } else { b"`--" });
            let _ = out.write_all(&r.2);
            let _ = out.write_all(b"\n");
            if prefix.len() < 100 - 7 && r.0 != parent {
                let n = prefix.len();
                prefix.extend_from_slice(if more { b"|  " } else { b"   " });
                level(out, rows, r.0, prefix);
                prefix.truncate(n);
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    let _ = out.write_all(b"QUERY PLAN\n");
    level(out, rows, 0, &mut Vec::new());
}

/// Run one SQL statement, writing its rows in the current output mode
/// (v0.17: `.mode`, `.headers`, `.nullvalue`, ...; `render`). `Err`
/// carries the user-facing message (hipp's remedy-named Debug form).
async fn run_statement<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
    io: &mut shell::Io,
) -> Result<(), String>
where
    S::Error: core::fmt::Debug,
{
    // The names come from the text and the catalog (`names`), read
    // before the statement holds the connection; only the modes that
    // show them pay for them.
    let names = if io.set.wants_names() {
        names::result_names(db, sql)
    } else {
        Vec::new()
    };
    let mut st = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
    // The oracle shell's `bind_prepared_stmt`: each parameter
    // from the `.parameter` table by name, else NULL.
    for i in 1..=st.parameter_count() {
        let key = st
            .parameter_name(i)
            .map_or_else(|| format!("?{i}"), str::to_string);
        if let Some(v) = io.params.get(&key) {
            st.bind_value(i, v.as_value())
                .map_err(|e| format!("{e:?}"))?;
        }
    }
    // EXPLAIN QUERY PLAN (v0.16): the oracle shell's tree.
    if st.is_explain() == 2 {
        let mut rows: Vec<(i64, i64, Vec<u8>)> = Vec::new();
        while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
            rows.push((
                st.column_i64(0).unwrap_or(0),
                st.column_i64(1).unwrap_or(0),
                st.column_bytes(3).unwrap_or(&[]).to_vec(),
            ));
        }
        eqp_render(&mut io.out, &rows);
        return Ok(());
    }
    // Step to completion, writing every row (a statement whose
    // planning waited for its bindings knows its columns only once
    // stepped, so the names are fitted to its width then).
    let shell::Io { set, out, .. } = io;
    let mut emit: Option<render::Emit<'_>> = None;
    let fit = |mut n: Vec<Vec<u8>>, w: usize| {
        n.truncate(w);
        while n.len() < w {
            n.push(format!("column{}", n.len() + 1).into_bytes());
        }
        n
    };
    while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
        let w = st.column_count();
        let vals: Vec<render::Val> = (0..w)
            .map(|i| render::Val::of(st.column_value(i).unwrap_or(Value::Null)))
            .collect();
        let e = emit.get_or_insert_with(|| render::Emit::new(set, fit(names.clone(), w)));
        e.row(vals, out);
    }
    let w = st.column_count();
    if w > 0 {
        emit.unwrap_or_else(|| render::Emit::new(set, fit(names, w)))
            .finish(out);
    }
    Ok(())
}

/// Split a script into statements on `;` outside of string
/// literals, quoted identifiers and comments. A `CREATE [TEMP]
/// TRIGGER` statement runs to a `;` that follows `; END` (blanks and
/// comments between), exactly as the oracle shell's
/// `sqlite3_complete` state machine reads it.
fn split_statements(script: &str) -> Vec<String> {
    split_script(script).0
}

/// `split_statements`, also returning whether the script ends on a
/// complete statement (nothing but blanks after the last one).
fn split_script(script: &str) -> (Vec<String>, bool) {
    // The states of the pin's `sqlite3_complete`.
    const START: u8 = 1;
    const NORMAL: u8 = 2;
    const EXPLAIN: u8 = 3;
    const CREATE: u8 = 4;
    const TRIGGER: u8 = 5;
    const SEMI: u8 = 6;
    const END: u8 = 7;
    // Tokens: 0 `;`, 1 blank / comment, 2 other, 3 EXPLAIN, 4
    // CREATE, 5 TEMP / TEMPORARY, 6 TRIGGER, 7 END.
    const TRANS: [[u8; 8]; 8] = [
        [START, 0, NORMAL, EXPLAIN, CREATE, NORMAL, NORMAL, NORMAL],
        [
            START, START, NORMAL, EXPLAIN, CREATE, NORMAL, NORMAL, NORMAL,
        ],
        [
            START, NORMAL, NORMAL, NORMAL, NORMAL, NORMAL, NORMAL, NORMAL,
        ],
        [
            START, EXPLAIN, EXPLAIN, NORMAL, CREATE, NORMAL, NORMAL, NORMAL,
        ],
        [
            START, CREATE, NORMAL, NORMAL, NORMAL, CREATE, TRIGGER, NORMAL,
        ],
        [
            SEMI, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER,
        ],
        [SEMI, SEMI, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER, END],
        [
            START, END, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER, TRIGGER,
        ],
    ];
    let b = script.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut state = START;
    while i < b.len() {
        let tok: usize = match b[i] {
            b';' => {
                i += 1;
                0
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                1
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                1
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                1
            }
            q @ (b'\'' | b'"' | b'`' | b'[') => {
                let close = if q == b'[' { b']' } else { q };
                i += 1;
                while i < b.len() && b[i] != close {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                2
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                let a = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'_'
                        || b[i] == b'$'
                        || b[i] >= 0x80)
                {
                    i += 1;
                }
                let w = &b[a..i];
                let is = |k: &str| w.eq_ignore_ascii_case(k.as_bytes());
                if is("EXPLAIN") {
                    3
                } else if is("CREATE") {
                    4
                } else if is("TEMP") || is("TEMPORARY") {
                    5
                } else if is("TRIGGER") {
                    6
                } else if is("END") {
                    7
                } else {
                    2
                }
            }
            _ => {
                i += 1;
                2
            }
        };
        state = TRANS[usize::from(state)][tok];
        if tok == 0 && state == START {
            let s = script[start..i].trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            start = i;
        }
    }
    // An unterminated tail keeps its trailing whitespace: a CREATE
    // TABLE with options or a CREATE INDEX stores it, as on the pin.
    let tail = script[start..].trim_start();
    let complete = tail.trim_end().is_empty();
    if !complete {
        out.push(tail.to_string());
    }
    (out, complete)
}

/// `.parameter set KEY VALUE`'s value (v0.16): VALUE as an SQL
/// expression when it is one (`5`, `2.5`, `x'01'`, `'text'`,
/// `1+2`), else the text itself — the oracle shell's two-try
/// `REPLACE INTO temp.sqlite_parameters ... VALUES(%Q,%s)`.
async fn parameter_value<S: hipp::Storage>(db: &mut Connection<S>, text: &str) -> OwnedValue
where
    S::Error: core::fmt::Debug,
{
    if let Ok(mut st) = db.prepare(&format!("SELECT {text}")).await
        && st.column_count() == 1
        && st.parameter_count() == 0
        && let Ok(Step::Row) = st.step().await
    {
        return OwnedValue::from_value(st.column_value(0).unwrap_or(Value::Null));
    }
    OwnedValue::from_value(Value::Text(text.as_bytes()))
}

/// The pin's limits as `.limit` lists them (`%20s %d`), in its order:
/// hipp enforces the ones it has at these values (the pin's defaults)
/// and models no VDBE, worker threads or trigger-step count.
const LIMITS: [(&str, i64); 15] = [
    ("length", 1_000_000_000),
    ("sql_length", 1_000_000_000),
    ("column", 2000),
    ("expr_depth", 1000),
    ("parser_depth", 2500),
    ("compound_select", 500),
    ("vdbe_op", 250_000_000),
    ("function_arg", 1000),
    ("attached", 10),
    ("like_pattern_length", 50000),
    ("variable_number", 32766),
    ("trigger_depth", 1000),
    ("worker_threads", 0),
    ("schema", 10_000_000),
    ("trigger_steps", 65000),
];

/// The argument errors of a database command, reported before the
/// database is opened (the pin opens it only once they pass).
fn db_precheck(
    cmd: &str,
    args: &[(String, usize)],
    caret: &dyn Fn(usize, &str) -> shell::Failed,
) -> Result<(), shell::Failed> {
    match cmd {
        "fullschema" => match args.get(1).map(|a| a.0.as_str()) {
            None => Ok(()),
            Some("--indent" | "-indent") if args.len() == 2 => {
                eprintln!("Error: .fullschema --indent is not supported by hipp");
                Err(shell::Failed)
            }
            Some(_) => {
                eprintln!("Usage: .fullschema ?--indent?");
                Err(shell::Failed)
            }
        },
        "indexes" => {
            let mut pat = false;
            for (t, off) in &args[1..] {
                match t.as_str() {
                    "-a" | "-all" | "--all" | "-sys" | "--sys" | "-expr" | "--expr" => {}
                    _ if t.starts_with('-') => return Err(caret(*off, "unknown option")),
                    _ if pat => return Err(caret(*off, "unknown argument")),
                    _ => pat = true,
                }
            }
            Ok(())
        }
        "dump" => {
            for (t, off) in &args[1..] {
                if let Some(o) = t.strip_prefix('-')
                    && !matches!(
                        o.strip_prefix('-').unwrap_or(o),
                        "data-only" | "nosys" | "preserve-rowids" | "newlines"
                    )
                {
                    return Err(caret(*off, "unknown option"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A dot-command that reads the database (v0.17: `.tables`,
/// `.indexes`, `.schema`, `.fullschema`, `.databases`, `.dump`,
/// `.parameter`, `.limit`). `Err` when it failed (its message
/// printed).
async fn db_command<S: hipp::Storage>(
    db: &mut Connection<S>,
    cmd: &str,
    line: &str,
    args: &[(String, usize)],
    io: &mut shell::Io,
    ctx: &shell::Ctx,
) -> Result<(), shell::Failed>
where
    S::Error: core::fmt::Debug,
{
    let a: Vec<&str> = args.iter().map(|(s, _)| s.as_str()).collect();
    let caret = |off: usize, msg: &str| {
        let p = match ctx {
            shell::Ctx::Argv(n) => format!("argv[{n}]: "),
            shell::Ctx::Line(None, n) => format!("line {n}: "),
            shell::Ctx::Line(Some(f), n) => format!("{f}:{n}: "),
        };
        eprintln!("{p}{line}");
        eprintln!("{p}{}^--- {msg}", " ".repeat(off));
        shell::Failed
    };
    let out = &mut io.out;
    match cmd {
        "parameter" => {
            match &a[1..] {
                ["clear"] => io.params.clear(),
                ["init"] => {}
                ["list"] => {
                    let len = io.params.keys().map(|k| k.len()).max().unwrap_or(0).min(40);
                    for (k, v) in io.params.iter() {
                        let _ = out.write_all(format!("{k:<len$} ").as_bytes());
                        let _ = out.write_all(&fmt_literal(v.as_value()));
                        let _ = out.write_all(b"\n");
                    }
                }
                ["set", key, value] => {
                    let v = parameter_value(db, value).await;
                    io.params.insert((*key).to_string(), v);
                }
                ["unset", key] => {
                    io.params.remove(*key);
                }
                // (v0.17: as the pin, its help text, and no error.)
                _ => {
                    let _ = out.write_all(help::help(Some("parameter")).as_bytes());
                }
            }
            Ok(())
        }
        "tables" => {
            // The pin's `.tables ?PATTERN?`: the tables and views of
            // every database (`temp.x`, `aux.x` for the others; no
            // `sqlite_` names) whose name is LIKE the pattern, sorted,
            // in its column grid.
            let pat = a.get(1).copied();
            let mut names: Vec<Vec<u8>> = Vec::new();
            if a.len() <= 2 {
                let mut add = |prefix: &[u8], n: &[u8]| {
                    if n.starts_with(b"sqlite_") {
                        return;
                    }
                    if let Some(p) = pat
                        && !help::like(p.as_bytes(), n)
                    {
                        return;
                    }
                    let mut v = prefix.to_vec();
                    v.extend_from_slice(n);
                    names.push(v);
                };
                for t in db.tables().iter() {
                    add(b"", t.name);
                }
                for v in db.views().iter() {
                    add(b"", v.name);
                }
                if let Some(t) = db.temp_db() {
                    for m in t.tables().iter() {
                        add(b"temp.", m.name);
                    }
                    for m in t.views().iter() {
                        add(b"temp.", m.name);
                    }
                }
                let mut i = 0;
                while let Some((an, at)) = db.attached_db(i) {
                    let mut pre = an.to_vec();
                    pre.push(b'.');
                    for m in at.tables().iter() {
                        add(&pre, m.name);
                    }
                    for m in at.views().iter() {
                        add(&pre, m.name);
                    }
                    i += 1;
                }
            }
            names.sort();
            let mut b = Vec::new();
            render::grid(&names, &mut b);
            let _ = out.write_all(&b);
            Ok(())
        }
        "indexes" => {
            // The pin's `.indexes ?PATTERN?`: the indexes on the tables
            // LIKE the pattern; `-a` / `--all` adds the automatic ones,
            // `--sys` shows only those, `--expr` only expression
            // indexes.
            let (mut all, mut sys, mut expr) = (false, false, false);
            let mut pat: Option<&str> = None;
            for (t, off) in &args[1..] {
                match t.as_str() {
                    "-a" | "-all" | "--all" => all = true,
                    "-sys" | "--sys" => sys = true,
                    "-expr" | "--expr" => expr = true,
                    _ if t.starts_with('-') => return Err(caret(*off, "unknown option")),
                    _ if pat.is_some() => return Err(caret(*off, "unknown argument")),
                    _ => pat = Some(t),
                }
            }
            let mut dbs: Vec<String> = vec!["main".into()];
            if db.temp_db().is_some() {
                dbs.push("temp".into());
            }
            let mut i = 0;
            while let Some((an, _)) = db.attached_db(i) {
                dbs.push(String::from_utf8_lossy(an).into_owned());
                i += 1;
            }
            let mut names: Vec<Vec<u8>> = Vec::new();
            for d in &dbs {
                let q = d.replace('"', "\"\"");
                let sql = format!(
                    "SELECT name, tbl_name, sql FROM \"{q}\".sqlite_schema WHERE type = 'index'"
                );
                let mut found: Vec<(Vec<u8>, Vec<u8>, bool)> = Vec::new();
                if let Ok(mut st) = db.prepare(&sql).await {
                    while let Ok(Step::Row) = st.step().await {
                        found.push((
                            st.column_bytes(0).unwrap_or_default().to_vec(),
                            st.column_bytes(1).unwrap_or_default().to_vec(),
                            st.column_bytes(2).is_none(),
                        ));
                    }
                }
                for (name, tbl, auto) in found {
                    if sys && !auto || !(all || sys) && auto {
                        continue;
                    }
                    // (The pin's pattern, `%` appended: LIKE the index
                    // name, or for an automatic index its table's.)
                    if let Some(p) = pat {
                        let p = format!("{p}%");
                        let hit = help::like(p.as_bytes(), &name)
                            || (auto && help::like(p.as_bytes(), &tbl));
                        if !hit {
                            continue;
                        }
                    }
                    if expr {
                        let x = format!(
                            "PRAGMA \"{q}\".index_xinfo(\"{}\")",
                            String::from_utf8_lossy(&name).replace('"', "\"\"")
                        );
                        let mut has = false;
                        if let Ok(mut st) = db.prepare(&x).await {
                            while let Ok(Step::Row) = st.step().await {
                                if st.column_i64(1) == Some(-2) {
                                    has = true;
                                }
                            }
                        }
                        if !has {
                            continue;
                        }
                    }
                    let mut v = Vec::new();
                    if d != "main" {
                        v.extend_from_slice(d.as_bytes());
                        v.push(b'.');
                    }
                    v.extend_from_slice(&name);
                    names.push(v);
                }
            }
            names.sort();
            let mut b = Vec::new();
            render::grid(&names, &mut b);
            let _ = out.write_all(&b);
            Ok(())
        }
        "schema" => {
            let arg = a.get(1).copied().unwrap_or("");
            print_schema(db, arg, out, false, None).await;
            // The TEMP database's objects follow, named `temp.`, as
            // the oracle shell prints them.
            if db.temp_db().is_some() {
                print_schema(db, arg, out, true, None).await;
            }
            // Then each attached database's (v0.16), `name.`-prefixed.
            let mut i = 0;
            while db.attached_db(i).is_some() {
                print_schema(db, arg, out, false, Some(i)).await;
                i += 1;
            }
            Ok(())
        }
        "fullschema" => {
            // The pin's `.fullschema`: every schema text but the
            // `sqlite_` tables', main and TEMP together in rowid
            // order, then the statistics as `.dump` writes them.
            // (Its arguments are checked by `db_precheck`.)
            // (Main and TEMP read apart and merged by rowid, main first:
            // hipp reads no compound of the two schema tables.)
            let mut rows: Vec<(i64, u8, Vec<u8>)> = Vec::new();
            let mut srcs = vec![(0u8, "main.sqlite_schema")];
            if db.temp_db().is_some() {
                srcs.push((1, "sqlite_temp_schema"));
            }
            for (k, tbl) in srcs {
                let sql = format!(
                    "SELECT rowid, sql FROM {tbl} WHERE type != 'meta' AND sql NOTNULL \
                     AND name NOT LIKE 'sqlite_%'"
                );
                if let Ok(mut st) = db.prepare(&sql).await {
                    while let Ok(Step::Row) = st.step().await {
                        rows.push((
                            st.column_i64(0).unwrap_or(0),
                            k,
                            st.column_bytes(1).unwrap_or_default().to_vec(),
                        ));
                    }
                }
            }
            rows.sort_by_key(|r| (r.0, r.1));
            let texts: Vec<Vec<u8>> = rows.into_iter().map(|r| r.2).collect();
            for t in &texts {
                let _ = out.write_all(t);
                let comment = t.windows(2).any(|w| w == b"--");
                let _ = out.write_all(if comment { b"\n;\n" } else { b";\n" });
            }
            let stats = db
                .tables()
                .iter()
                .any(|t| matches!(t.name, b"sqlite_stat1" | b"sqlite_stat3" | b"sqlite_stat4"));
            if !stats {
                let _ = out.write_all(b"/* No STAT tables available */\n");
                return Ok(());
            }
            let _ = out.write_all(b"ANALYZE sqlite_schema;\n");
            if db.tables().iter().any(|t| t.name == b"sqlite_stat1")
                && let Ok(mut st) = db.prepare("SELECT * FROM sqlite_stat1").await
            {
                while let Ok(Step::Row) = st.step().await {
                    let mut l = b"INSERT INTO sqlite_stat1 VALUES(".to_vec();
                    for i in 0..st.column_count() {
                        if i > 0 {
                            l.push(b',');
                        }
                        l.extend_from_slice(&dump_literal(
                            st.column_value(i).unwrap_or(Value::Null),
                        ));
                    }
                    l.extend_from_slice(b");\n");
                    let _ = out.write_all(&l);
                }
            }
            let _ = out.write_all(b"ANALYZE sqlite_schema;\n");
            Ok(())
        }
        "databases" => {
            // `name: file r/w`, `""` for a database without a file,
            // as the oracle shell prints `PRAGMA database_list`.
            if let Ok(mut st) = db.prepare("PRAGMA database_list").await {
                while let Ok(Step::Row) = st.step().await {
                    let name = st.column_bytes(1).unwrap_or_default().to_vec();
                    let file = st.column_bytes(2).unwrap_or_default().to_vec();
                    let _ = out.write_all(&name);
                    let _ = out.write_all(b": ");
                    if file.is_empty() {
                        let _ = out.write_all(b"\"\"");
                    } else {
                        let _ = out.write_all(&file);
                    }
                    let _ = out.write_all(b" r/w\n");
                }
            }
            Ok(())
        }
        "limits" => {
            if a.len() > 3 {
                eprintln!("Usage: .limit NAME ?NEW-VALUE?");
                return Err(shell::Failed);
            }
            let Some(name) = a.get(1) else {
                for (n, v) in LIMITS {
                    let _ = writeln!(out, "{n:>20} {v}");
                }
                return Ok(());
            };
            let hits: Vec<&(&str, i64)> = LIMITS
                .iter()
                .filter(|(n, _)| {
                    n.len() >= name.len()
                        && n.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
                })
                .collect();
            let (n, v) = match hits.as_slice() {
                [one] => **one,
                [] => {
                    eprintln!("unknown limit: \"{name}\"");
                    eprintln!("enter \".limits\" with no arguments for a list.");
                    return Err(shell::Failed);
                }
                _ => {
                    eprintln!("ambiguous limit: \"{name}\"");
                    return Err(shell::Failed);
                }
            };
            if a.len() == 3 {
                eprintln!("Error: .limit cannot change a limit in hipp");
                return Err(shell::Failed);
            }
            let _ = writeln!(out, "{n:>20} {v}");
            Ok(())
        }
        "dump" => dump(db, line, args, out, &caret).await,
        _ => Ok(()),
    }
}

/// How `.dump` writes one table's rows: the column list of its
/// `INSERT`s (with `--preserve-rowids`) and the select that reads them.
async fn dump_rows<S: hipp::Storage>(
    db: &mut Connection<S>,
    name: &[u8],
    sql: &[u8],
    list: &str,
    keep_rowid: bool,
    out: &mut shell::Output,
) -> Result<(), String>
where
    S::Error: core::fmt::Debug,
{
    let quoted = format!("\"{}\"", String::from_utf8_lossy(name).replace('"', "\"\""));
    let mut head = [b"INSERT INTO ".as_slice(), &dump_name(name)].concat();
    let mut select = format!("SELECT {list} FROM {quoted}");
    if keep_rowid {
        // The pin's `--preserve-rowids`: the column list written out,
        // the rowid first (as `rowid`) unless the table has an INTEGER
        // PRIMARY KEY or no rowid; the rowid read through the first of
        // `rowid`, `_rowid_`, `oid` no column is named.
        let mut cols: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
        let ti = format!("PRAGMA table_xinfo({quoted})");
        if let Ok(mut st) = db.prepare(&ti).await {
            while let Ok(Step::Row) = st.step().await {
                let hidden = st.column_i64(6).unwrap_or(0);
                if hidden == 0 {
                    cols.push((
                        st.column_bytes(1).unwrap_or_default().to_vec(),
                        st.column_bytes(2).unwrap_or_default().to_vec(),
                        st.column_i64(5).unwrap_or(0),
                    ));
                }
            }
        }
        let pks: Vec<&(Vec<u8>, Vec<u8>, i64)> = cols.iter().filter(|c| c.2 > 0).collect();
        let ipk = matches!(pks.as_slice(), [c] if c.1.eq_ignore_ascii_case(b"INTEGER"));
        let lower = sql.to_ascii_lowercase();
        let tail = lower.rsplit(|c| *c == b')').next().unwrap_or(&[]);
        let no_rowid = tail
            .split(|c| !c.is_ascii_alphanumeric() && *c != b'_')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w == [b"without".as_slice(), b"rowid".as_slice()]);
        let alias = ["rowid", "_rowid_", "oid"]
            .into_iter()
            .find(|r| !cols.iter().any(|c| c.0.eq_ignore_ascii_case(r.as_bytes())));
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut exprs: Vec<String> = Vec::new();
        if !ipk
            && !no_rowid
            && let Some(r) = alias
        {
            names.push(b"rowid".to_vec());
            exprs.push(r.to_string());
        }
        for c in &cols {
            names.push(c.0.clone());
            exprs.push(format!(
                "\"{}\"",
                String::from_utf8_lossy(&c.0).replace('"', "\"\"")
            ));
        }
        head.push(b'(');
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                head.push(b',');
            }
            head.extend_from_slice(&dump_name(n));
        }
        head.push(b')');
        select = format!("SELECT {} FROM {quoted}", exprs.join(", "));
    }
    head.extend_from_slice(b" VALUES(");
    let mut st = db.prepare(&select).await.map_err(|e| format!("{e:?}"))?;
    while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
        let mut line = head.clone();
        for i in 0..st.column_count() {
            if i > 0 {
                line.extend_from_slice(b",");
            }
            line.extend_from_slice(&dump_literal(st.column_value(i).unwrap_or(Value::Null)));
        }
        line.extend_from_slice(b");\n");
        let _ = out.write_all(&line);
    }
    Ok(())
}

/// `.dump [OPTIONS] [PATTERN...]` (v0.17: `--data-only`, `--nosys`,
/// `--preserve-rowids`, `--newlines` — which the pin 3.54.1 takes and
/// ignores — and LIKE patterns), byte for byte as the pin writes it.
async fn dump<S: hipp::Storage>(
    db: &mut Connection<S>,
    _line: &str,
    args: &[(String, usize)],
    out: &mut shell::Output,
    caret: &dyn Fn(usize, &str) -> shell::Failed,
) -> Result<(), shell::Failed>
where
    S::Error: core::fmt::Debug,
{
    let (mut data_only, mut nosys, mut rowids) = (false, false, false);
    let mut pats: Vec<&str> = Vec::new();
    for (t, off) in &args[1..] {
        if let Some(o) = t.strip_prefix('-') {
            match o.strip_prefix('-').unwrap_or(o) {
                "data-only" => data_only = true,
                "nosys" => nosys = true,
                "preserve-rowids" => rowids = true,
                "newlines" => {}
                _ => return Err(caret(*off, "unknown option")),
            }
        } else {
            pats.push(t);
        }
    }
    match dump_body(db, &pats, data_only, nosys, rowids, out).await {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("Error: {e}");
            Err(shell::Failed)
        }
    }
}

async fn dump_body<S: hipp::Storage>(
    db: &mut Connection<S>,
    pats: &[&str],
    data_only: bool,
    nosys: bool,
    rowids: bool,
    out: &mut shell::Output,
) -> Result<(), String>
where
    S::Error: core::fmt::Debug,
{
    // `.dump PATTERN...` (v0.16 review fix: the argument was
    // ignored): the pin shell's filter — a LIKE on the name, a
    // virtual table bringing its `name_*` shadow tables.
    let filter = if pats.is_empty() {
        "1".to_string()
    } else {
        pats.iter()
            .map(|p| {
                let q = format!("'{}'", p.replace('\'', "''"));
                format!(
                    "name LIKE {q} ESCAPE '\\' OR EXISTS (SELECT 1 FROM sqlite_schema \
                     WHERE name LIKE {q} ESCAPE '\\' AND sql LIKE 'CREATE VIRTUAL TABLE%' \
                     AND substr(o.name, 1, length(name)+1) == (name||'_'))"
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    let mut keep: Vec<Vec<u8>> = Vec::new();
    if let Ok(mut st) = db
        .prepare(&format!(
            "SELECT name FROM main.sqlite_schema AS o WHERE ({filter}) AND type = 'table' AND sql NOT NULL"
        ))
        .await
    {
        while let Ok(Step::Row) = st.step().await {
            if let Some(n) = st.column_bytes(0)
                && !(nosys && n.starts_with(b"sqlite_"))
            {
                keep.push(n.to_vec());
            }
        }
    }
    // Snapshot the catalog first (names as bytes) so the per-table
    // SELECTs can borrow the connection freely. A table with
    // generated columns dumps only the other columns (v0.16), as the
    // oracle shell does: its INSERT ... VALUES replays without them.
    let tables: Vec<(Vec<u8>, Vec<u8>, String)> = db
        .tables()
        .iter()
        .map(|t| {
            let list = if t.generated.iter().any(|g| *g) {
                let cols: Vec<String> = t
                    .columns
                    .iter()
                    .zip(t.generated.iter())
                    .filter(|(_, g)| !**g)
                    .map(|(c, _)| {
                        format!("\"{}\"", String::from_utf8_lossy(c).replace('"', "\"\""))
                    })
                    .collect();
                cols.join(", ")
            } else {
                "*".to_string()
            };
            (t.name.to_vec(), t.sql.to_vec(), list)
        })
        .collect();
    // `sqlite_sequence` (AUTOINCREMENT, v0.16) comes last and replays
    // as `DELETE` + `INSERT`s, as in the oracle shell: the table
    // itself is re-created by the AUTOINCREMENT table's CREATE (its
    // own CREATE is a reserved name).
    let is_seq = |n: &[u8]| n == b"sqlite_sequence";
    // A virtual table (v0.16) replays as its schema row, under `PRAGMA
    // writable_schema=ON`, in schema order among the tables; its
    // shadow tables then take `IF NOT EXISTS`. With `--data-only` its
    // rows are dumped there instead.
    let mut vtabs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut order: Vec<Vec<u8>> = Vec::new();
    if let Ok(mut st) = db
        .prepare("SELECT name, sql FROM main.sqlite_schema WHERE type = 'table' AND sql NOT NULL")
        .await
    {
        while let Ok(Step::Row) = st.step().await {
            if let (Some(n), Some(q)) = (st.column_bytes(0), st.column_bytes(1)) {
                if !keep.iter().any(|k| k == n) {
                    continue;
                }
                if q.len() >= 20 && q[..20].eq_ignore_ascii_case(b"CREATE VIRTUAL TABLE") {
                    vtabs.push((n.to_vec(), q.to_vec()));
                }
                order.push(n.to_vec());
            }
        }
    }
    // The pin shell's header for a script that writes `sqlite_schema`,
    // with the `foreign_keys` line it must precede: replaying it, the
    // shell turns defensive mode off (v0.16 review fix: without it the
    // pin refused the replay). (v0.17 fix: the PRAGMA line is always
    // there; `--data-only` writes neither it nor BEGIN / COMMIT.)
    if !vtabs.is_empty() {
        let _ = out.write_all(
            b"/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */\n",
        );
    }
    if !data_only {
        let _ = out.write_all(b"PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n");
    }
    let mut writable = false;
    let ordered = tables
        .iter()
        .filter(|t| keep.contains(&t.0))
        .filter(|t| !is_seq(&t.0))
        .chain(
            tables
                .iter()
                .filter(|t| keep.contains(&t.0))
                .filter(|t| is_seq(&t.0)),
        );
    let mut ordered: Vec<&(Vec<u8>, Vec<u8>, String)> = ordered.collect();
    if !vtabs.is_empty() {
        ordered.sort_by_key(|t| order.iter().position(|n| *n == t.0).unwrap_or(usize::MAX));
    }
    let mut vt_done = 0usize;
    let vtab_line = |vn: &[u8], vq: &[u8]| {
        let lit = |b: &[u8]| String::from_utf8_lossy(b).replace('\'', "''");
        format!(
            "INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','{}','{}',0,'{}');\n",
            lit(vn),
            lit(vn),
            lit(vq)
        )
    };
    for (name, sql, list) in ordered {
        // The virtual tables created before this table.
        let at = order.iter().position(|n| n == name).unwrap_or(usize::MAX);
        while let Some((vn, vq)) = vtabs.get(vt_done) {
            if order.iter().position(|n| n == vn).unwrap_or(usize::MAX) > at {
                break;
            }
            if data_only {
                dump_rows(db, vn, vq, "*", rowids, out).await?;
            } else {
                if !writable {
                    let _ = out.write_all(b"PRAGMA writable_schema=ON;\n");
                    writable = true;
                }
                let _ = out.write_all(vtab_line(vn, vq).as_bytes());
            }
            vt_done += 1;
        }
        if is_seq(name) {
            // (v0.17 fix: the pin re-creates it first.)
            if !writable {
                let _ = out.write_all(b"PRAGMA writable_schema=ON;\n");
                writable = true;
            }
            let _ = out.write_all(
                b"CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);\n\
                  DELETE FROM sqlite_sequence;\n",
            );
        } else if name.starts_with(b"sqlite_stat") && name.len() == 12 {
            // (v0.17 fix: ANALYZE's tables replay their rows.)
            if !data_only {
                let _ = out.write_all(b"ANALYZE sqlite_schema;\n");
            }
        } else if name.starts_with(b"sqlite_") {
            continue;
        } else if data_only {
        } else if sql.starts_with(b"CREATE TABLE \"") || sql.starts_with(b"CREATE TABLE '") {
            let _ = out.write_all(b"CREATE TABLE IF NOT EXISTS ");
            let _ = out.write_all(&sql[13..]);
            let _ = out.write_all(b";\n");
        } else {
            let _ = out.write_all(sql);
            let _ = out.write_all(b";\n");
        }
        dump_rows(db, name, sql, list, rowids, out).await?;
    }
    for (vn, vq) in vtabs.iter().skip(vt_done) {
        if data_only {
            dump_rows(db, vn, vq, "*", rowids, out).await?;
            continue;
        }
        if !writable {
            let _ = out.write_all(b"PRAGMA writable_schema=ON;\n");
            writable = true;
        }
        let _ = out.write_all(vtab_line(vn, vq).as_bytes());
    }
    // Non-table schema objects follow all table data, in the oracle
    // shell's .dump order (`ORDER BY type COLLATE NOCASE DESC`):
    // views, then triggers (v0.16: after the data, so replaying it
    // fires none), then explicit indexes, each in schema order. A
    // text holding `--` ends with its `;` on a line of its own, as
    // there.
    if !data_only {
        let mut objs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        if let Ok(mut st) = db
            .prepare(&format!(
                "SELECT type, sql FROM main.sqlite_schema AS o WHERE ({filter}) AND sql NOT NULL"
            ))
            .await
        {
            while let Ok(Step::Row) = st.step().await {
                if let (Some(t), Some(q)) = (st.column_bytes(0), st.column_bytes(1)) {
                    objs.push((t.to_vec(), q.to_vec()));
                }
            }
        }
        for ty in [&b"view"[..], b"trigger", b"index"] {
            for (t, sql) in objs.iter() {
                if !t.eq_ignore_ascii_case(ty) {
                    continue;
                }
                let _ = out.write_all(sql);
                let comment = sql.windows(2).any(|w| w == b"--");
                let _ = out.write_all(if comment { b"\n;\n" } else { b";\n" });
            }
        }
    }
    // Last, as the pin shell writes it.
    if writable {
        let _ = out.write_all(b"PRAGMA writable_schema=OFF;\n");
    }
    if !data_only {
        let _ = out.write_all(b"COMMIT;\n");
    }
    Ok(())
}

/// Whether `sql` holds nothing but blanks and comments (the pin
/// shell's `_all_whitespace`: such input is skipped, `;` is not).
fn no_sql(sql: &str) -> bool {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            return rest.is_empty();
        }
    }
}

/// `-help` (v0.17): the pin shell's usage text, to stderr, byte for
/// byte (its option list too: hipp refuses the options it does not
/// take, `PIN_OPTIONS`, rather than reading one as a file name).
fn usage(argv0: &str) {
    eprintln!("Usage: {argv0} [OPTIONS] [FILENAME [SQL...]]");
    eprint!("{}", PIN_USAGE);
}

/// The pin's `-help` text after its first line.
const PIN_USAGE: &str = "FILENAME is the name of an SQLite database. A new database is created
if the file does not previously exist. Defaults to :memory:.
OPTIONS include:
   --                   treat no subsequent arguments as options
   -append              append the database to the end of the file
   -ascii               set '.mode ascii'
   -bail                stop after hitting an error
   -batch               force batch I/O
   -box                 set '.mode box'
   -cmd COMMAND         run \"COMMAND\" before reading stdin
   -cmdline-edit BOOL   enable or disable command-line editing
   -column              set '.mode column'
   -csv                 set '.mode csv -limits off'
   -deserialize         open the database using sqlite3_deserialize()
   -echo                print inputs before execution
   -escape T            ctrl-char escape; T is one of: symbol, ascii, off
   -init FILENAME       read/process named file
   -[no]header          turn headers on or off
   -help                show this message
   -html                set '.mode html'
   -ifexists            only open if database already exists
   -interactive         force interactive I/O
   -json                set '.mode json'
   -line                set '.mode line'
   -list                set '.mode list'
   -lookaside SIZE N    use N entries of SZ bytes for lookaside memory
   -markdown            set '.mode markdown'
   -maxsize N           maximum size for a --deserialize database
   -memtrace            trace all memory allocations and deallocations
   -mmap N              default mmap size set to N
   -newline SEP         set output row separator. Default: '\\n'
   -nofollow            refuse to open symbolic links to database files
   -noinit              Do not read the ~/.sqliterc file at startup
   -nonce STRING        set the safe-mode escape nonce
   -no-rowid-in-view    Disable rowid-in-view using sqlite3_config()
   -nullvalue TEXT      set text string for NULL values. Default ''
   -pagecache SIZE N    use N slots of SZ bytes each for page cache memory
   -pcachetrace         trace all page cache operations
   -quote               set '.mode quote'
   -readonly            open the database read-only
   -safe                enable safe-mode
   -screenwidth N       use N as the default screenwidth 
   -separator SEP       set output column separator. Default: '|'
   -stats               print memory stats before each finalize
   -table               set '.mode table'
   -tabs                set '.mode tabs'
   -unsafe-testing      allow unsafe commands and modes for testing
   -version             show SQLite version
   -vfs NAME            use NAME as the default VFS
   -vfstrace            enable tracing of all VFS calls
";
/// The pin shell's other options (its `-help` list and the ones it
/// takes unlisted): hipp takes none of them, and says so rather than
/// reading one as a file name.
const PIN_OPTIONS: &[&str] = &[
    "-A",
    "-append",
    "-ascii",
    "-bail",
    "-batch",
    "-box",
    "-cmd",
    "-cmdline-edit",
    "-column",
    "-csv",
    "-deserialize",
    "-echo",
    "-escape",
    "-header",
    "-heap",
    "-html",
    "-ifexists",
    "-init",
    "-interactive",
    "-json",
    "-line",
    "-list",
    "-lookaside",
    "-markdown",
    "-maxsize",
    "-memtrace",
    "-mmap",
    "-multiplex",
    "-newline",
    "-no-rowid-in-view",
    "-nofollow",
    "-noheader",
    "-noinit",
    "-nonce",
    "-nullvalue",
    "-pagecache",
    "-pcachetrace",
    "-quote",
    "-readonly",
    "-safe",
    "-screenwidth",
    "-separator",
    "-sorterref",
    "-stats",
    "-table",
    "-tabs",
    "-unsafe-testing",
    "-utf8",
    "-vfs",
    "-vfstrace",
    "-zip",
];

/// The command line as the pin's shell reads it (v0.17): options
/// (`-x`, or `--x`) anywhere before `-` / `--`, which ends them; the
/// first other argument is the database (`:memory:` when there is
/// none), the rest SQL texts run in order, each with its index in the
/// command line (the `argv[N]: ` of an error). `Err`: the exit status
/// `-help`, `-version` or a bad option ends with, having printed
/// what the pin prints (no database is opened, so no file made).
fn parse_args(
    argv0: &str,
    args: impl Iterator<Item = String>,
) -> Result<(String, Vec<(usize, String)>), ExitCode> {
    let mut file: Option<String> = None;
    let mut cmds: Vec<(usize, String)> = Vec::new();
    let mut opts = true;
    for (k, a) in args.enumerate() {
        if !opts || !a.starts_with('-') {
            match file {
                None => file = Some(a),
                Some(_) => cmds.push((k + 1, a)),
            }
            continue;
        }
        let z = if a.starts_with("--") {
            &a[1..]
        } else {
            a.as_str()
        };
        match z {
            "-" => opts = false,
            "-help" => {
                usage(argv0);
                return Err(ExitCode::SUCCESS);
            }
            "-version" => {
                // (v0.17: the pin's layout, with hipp's name where
                // the pin prints its source id.)
                println!("{}", version_line());
                return Err(ExitCode::SUCCESS);
            }
            _ if PIN_OPTIONS.contains(&z) => {
                eprintln!("{argv0}: Error: option not supported by hipp: {z}");
                return Err(ExitCode::FAILURE);
            }
            _ => {
                eprintln!("{argv0}: Error: unknown option: {z}");
                eprintln!("Use -help for a list of options.");
                return Err(ExitCode::FAILURE);
            }
        }
    }
    Ok((file.unwrap_or_else(|| String::from(":memory:")), cmds))
}

/// `-version` (v0.17): the pin's `VERSION DATE TIME SOURCE-ID
/// (NN-bit)` with hipp's name and version in the source id's place —
/// the first word, the SQLite version hipp is pinned to, stays the
/// pin's, so tools that read it see 3.54.1.
fn version_line() -> String {
    format!(
        "3.54.1 2026-10-16 14:45:20 hipp-{} ({}-bit)",
        env!("CARGO_PKG_VERSION"),
        usize::BITS
    )
}

fn main() -> ExitCode {
    // Run on a thread with the stack `cli_config` needs (in place
    // if the host cannot make one).
    match std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(run)
    {
        Ok(h) => h.join().unwrap_or(ExitCode::FAILURE),
        Err(_) => run(),
    }
}

fn run() -> ExitCode {
    let mut argv = std::env::args();
    let argv0 = argv.next().unwrap_or_else(|| String::from("hipp"));
    let (file, cmds) = match parse_args(&argv0, argv) {
        Ok(x) => x,
        Err(code) => return code,
    };
    let mut sh = shell::Shell::new(file);
    let code = session(&mut sh, &cmds);
    sh.finish();
    // The oracle shell closes the database however the session
    // ended (v0.16: a WAL-mode log is checkpointed and removed).
    if let Some(db) = sh.db.take()
        && let Err(e) = close_db(db)
    {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    code
}

/// Run the one-shot arguments, or the interactive / stdin session.
fn session(sh: &mut shell::Shell, cmds: &[(usize, String)]) -> ExitCode {
    // One-shot mode: run the arguments in order, stop at the first
    // error. An argument whose first character is `.` is one
    // dot-command; any other is SQL (possibly several statements),
    // for which the pin opens the database even when it is blank
    // (v0.17: a dot-command opens it only if it reads it).
    if !cmds.is_empty() {
        for (k, arg) in cmds {
            sh.ctx = shell::Ctx::Argv(*k);
            sh.echo(arg);
            let r = if arg.starts_with('.') {
                match sh.meta(arg.trim_end()) {
                    Ok(shell::Flow::Continue) => Ok(()),
                    Ok(shell::Flow::Quit | shell::Flow::Exit) => return ExitCode::SUCCESS,
                    Ok(shell::Flow::ExitWith(n)) => return ExitCode::from(n),
                    Err(e) => Err(e),
                }
            } else {
                sh.sql(arg)
            };
            if r.is_err() {
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    // Interactive / stdin-batch mode.
    let stdin = std::io::stdin();
    sh.interactive = stdin.is_terminal();
    if sh.interactive {
        println!("hipp — reads and writes SQLite database files (format pin: SQLite 3.54.1)");
        println!("Enter SQL or dot-commands (\".help\" lists them)");
    }
    // A statement or dot-command failed: the script goes on (unless
    // `.bail on`), and the shell exits 1 at its end (or at `.exit`),
    // as the pin's `process_input` returns `errCnt > 0`.
    let (errs, flow) = sh.process(stdin.lock(), None);
    match flow {
        shell::Flow::ExitWith(n) => ExitCode::from(n),
        _ if errs > 0 => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}
