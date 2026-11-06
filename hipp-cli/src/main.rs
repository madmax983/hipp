//! hipp-cli: a shell over the hipp library (SPEC §9.4, decision
//! D4). Interactive + one-shot modes over the library's SQL
//! subset, list-format output (`|` separator, NULL = empty) that
//! is byte-diffable against the pinned SQLite oracle CLI.
//!
//! Its first job is differential testing; demo surface second.
//! Dot-commands: `.tables`, `.schema`, `.dump`, `.open`, `.databases`,
//! `.parameter`, `.exit` (alias `.quit`). No line editing, no
//! `.import`/`.export`, no format/flag parity beyond diffable
//! basics.
//!
//! Bound parameters (v0.16) work as in the oracle shell: every
//! parameter of a statement is bound from the `.parameter set`
//! table by its name (a nameless `?` by `?N`), NULL when the
//! table has no such key.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, MemStorage, OwnedValue, Step, Value};

/// The `.parameter` bindings (v0.16): the oracle shell's
/// `temp.sqlite_parameters` table — key (the parameter's name as
/// written, `?N` for a nameless `?`) to value, in key order.
type Params = std::collections::BTreeMap<String, OwnedValue>;

/// The open database: a file or `:memory:`.
enum Db {
    File(Connection<FileStorage>),
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

/// `.dump`'s literal: `fmt_literal`, but a blob as the pin shell's
/// `x'hex'` (v0.16 review fix: hipp wrote `X'HEX'`).
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
        v => fmt_literal(v),
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

/// The oracle shell's EXPLAIN QUERY PLAN rendering (`eqp_render`):
/// `QUERY PLAN`, then each row under its parent, `|--` before a row
/// with a later sibling and `` `-- `` before the last, children
/// indented by `|  ` or three blanks; nothing for a plan with no
/// rows. Like the shell's 100-byte prefix buffer, it stops descending
/// past 31 levels.
fn eqp_render(out: &mut impl Write, rows: &[(i64, i64, Vec<u8>)]) {
    fn level(
        out: &mut impl Write,
        rows: &[(i64, i64, Vec<u8>)],
        parent: i64,
        prefix: &mut Vec<u8>,
    ) {
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

/// Run one SQL statement, printing any result rows. `Err` carries
/// the user-facing message (hipp's remedy-named Debug form).
async fn run_statement<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
    params: &Params,
    out: &mut impl Write,
) -> Result<(), String>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.map_err(|e| format!("{e:?}"))?;
    // The oracle shell's `bind_prepared_stmt`: each parameter
    // from the `.parameter` table by name, else NULL.
    for i in 1..=st.parameter_count() {
        let key = st
            .parameter_name(i)
            .map_or_else(|| format!("?{i}"), str::to_string);
        if let Some(v) = params.get(&key) {
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
        eqp_render(out, &rows);
        return Ok(());
    }
    // Step to completion, printing every row (a statement whose
    // planning waited for its bindings knows its columns only
    // once stepped).
    while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
        let vals: Vec<Vec<u8>> = (0..st.column_count())
            .map(|i| value_bytes(st.column_value(i).unwrap_or(Value::Null)))
            .collect();
        write_row(out, &vals);
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

/// What a dot-command wants the shell loop to do next.
enum Flow {
    Continue,
    Exit,
}

/// Split a dot-command line into arguments the way the oracle
/// shell does: whitespace-separated words, a word starting with
/// `'` or `"` running to the matching quote (the quotes
/// dropped).
fn shell_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&c) = chars.peek() else {
            return args;
        };
        let mut word = String::new();
        if c == '\'' || c == '"' {
            chars.next();
            for d in chars.by_ref() {
                if d == c {
                    break;
                }
                word.push(d);
            }
        } else {
            while let Some(d) = chars.next_if(|d| !d.is_whitespace()) {
                word.push(d);
            }
        }
        args.push(word);
    }
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

async fn dot_command<S: hipp::Storage>(
    db: &mut Connection<S>,
    line: &str,
    params: &mut Params,
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
        ".parameter" | ".param" => {
            let args = shell_args(arg);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            match args.as_slice() {
                ["clear"] => params.clear(),
                ["init"] => {}
                ["list"] => {
                    let len = params.keys().map(|k| k.len()).max().unwrap_or(0).min(40);
                    for (k, v) in params.iter() {
                        let _ = out.write_all(format!("{k:<len$} ").as_bytes());
                        let _ = out.write_all(&fmt_literal(v.as_value()));
                        let _ = out.write_all(b"\n");
                    }
                }
                ["set", key, value] => {
                    let v = parameter_value(db, value).await;
                    params.insert((*key).to_string(), v);
                }
                ["unset", key] => {
                    params.remove(*key);
                }
                _ => {
                    return Err(
                        "usage: .parameter clear | init | list | set NAME VALUE | unset NAME"
                            .to_string(),
                    );
                }
            }
            Ok(Flow::Continue)
        }
        ".tables" => {
            // `sqlite_*` tables (sqlite_sequence, sqlite_stat1) are
            // not listed, as in the oracle shell.
            for t in db.tables().iter() {
                if t.name.starts_with(b"sqlite_") {
                    continue;
                }
                let _ = out.write_all(t.name);
                let _ = out.write_all(b"\n");
            }
            for v in db.views().iter() {
                let _ = out.write_all(v.name);
                let _ = out.write_all(b"\n");
            }
            // The TEMP database's tables and views (v0.16), named
            // `temp.x` as the oracle shell lists them.
            if let Some(t) = db.temp_db() {
                let names = t.tables().iter().map(|m| m.name).collect::<Vec<_>>();
                let views = t.views().iter().map(|m| m.name).collect::<Vec<_>>();
                for n in names.iter().chain(views.iter()) {
                    if n.starts_with(b"sqlite_") {
                        continue;
                    }
                    let _ = out.write_all(b"temp.");
                    let _ = out.write_all(n);
                    let _ = out.write_all(b"\n");
                }
            }
            // The attached databases' (v0.16), as `name.x`.
            let mut i = 0;
            while let Some((an, a)) = db.attached_db(i) {
                let names = a.tables().iter().map(|m| m.name).collect::<Vec<_>>();
                let views = a.views().iter().map(|m| m.name).collect::<Vec<_>>();
                for n in names.iter().chain(views.iter()) {
                    if n.starts_with(b"sqlite_") {
                        continue;
                    }
                    let _ = out.write_all(an);
                    let _ = out.write_all(b".");
                    let _ = out.write_all(n);
                    let _ = out.write_all(b"\n");
                }
                i += 1;
            }
            Ok(Flow::Continue)
        }
        ".schema" => {
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
            Ok(Flow::Continue)
        }
        ".databases" => {
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
            Ok(Flow::Continue)
        }
        ".dump" => {
            // `.dump PATTERN...` (v0.16 review fix: the argument was
            // ignored): the pin shell's filter — a LIKE on the name,
            // a virtual table bringing its `name_*` shadow tables.
            let pats: Vec<String> = shell_args(arg)
                .into_iter()
                .filter(|a| !a.starts_with('-'))
                .collect();
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
                    if let Some(n) = st.column_bytes(0) {
                        keep.push(n.to_vec());
                    }
                }
            }
            // Snapshot the catalog first (names as bytes) so the
            // per-table SELECTs can borrow the connection freely.
            // A table with generated columns dumps only the
            // other columns (v0.16), as the oracle shell does:
            // its INSERT ... VALUES replays without them.
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
            // `sqlite_sequence` (AUTOINCREMENT, v0.16) comes last and
            // replays as `DELETE` + `INSERT`s, as in the oracle shell:
            // the table itself is re-created by the AUTOINCREMENT
            // table's CREATE (its own CREATE is a reserved name).
            let is_seq = |n: &[u8]| n == b"sqlite_sequence";
            // A virtual table (v0.16) replays as its schema row, under
            // `PRAGMA writable_schema=ON`, in schema order among the
            // tables; its shadow tables then take `IF NOT EXISTS`.
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
            // The pin shell's header for a script that writes
            // `sqlite_schema`, with the `foreign_keys` line it must
            // precede: replaying it, the shell turns defensive mode
            // off (v0.16 review fix: without it the pin refused the
            // replay).
            if !vtabs.is_empty() {
                let _ = out.write_all(
                    b"/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */\n\
                      PRAGMA foreign_keys=OFF;\n",
                );
            }
            let _ = out.write_all(b"BEGIN TRANSACTION;\n");
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
            for (name, sql, list) in ordered {
                // The virtual tables created before this table.
                let at = order.iter().position(|n| n == name).unwrap_or(usize::MAX);
                while let Some((vn, vq)) = vtabs.get(vt_done) {
                    if order.iter().position(|n| n == vn).unwrap_or(usize::MAX) > at {
                        break;
                    }
                    if !writable {
                        let _ = out.write_all(b"PRAGMA writable_schema=ON;\n");
                        writable = true;
                    }
                    let lit = |b: &[u8]| String::from_utf8_lossy(b).replace('\'', "''");
                    let _ = out.write_all(
                        format!(
                            "INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','{}','{}',0,'{}');\n",
                            lit(vn),
                            lit(vn),
                            lit(vq)
                        )
                        .as_bytes(),
                    );
                    vt_done += 1;
                }
                if sql.starts_with(b"CREATE TABLE \"") || sql.starts_with(b"CREATE TABLE '") {
                    let _ = out.write_all(b"CREATE TABLE IF NOT EXISTS ");
                    let _ = out.write_all(&sql[13..]);
                    let _ = out.write_all(b";\n");
                } else if is_seq(name) {
                    let _ = out.write_all(b"DELETE FROM sqlite_sequence;\n");
                } else {
                    let _ = out.write_all(sql);
                    let _ = out.write_all(b";\n");
                }
                let name = String::from_utf8_lossy(name);
                let mut st = db
                    .prepare(&format!("SELECT {list} FROM {name}"))
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                while let Step::Row = st.step().await.map_err(|e| format!("{e:?}"))? {
                    let mut line = format!("INSERT INTO {name} VALUES(").into_bytes();
                    for i in 0..st.column_count() {
                        if i > 0 {
                            line.extend_from_slice(b",");
                        }
                        line.extend_from_slice(&dump_literal(
                            st.column_value(i).unwrap_or(Value::Null),
                        ));
                    }
                    line.extend_from_slice(b");\n");
                    let _ = out.write_all(&line);
                }
            }
            // Non-table schema objects follow all table data, in
            // the oracle shell's .dump order (`ORDER BY type COLLATE
            // NOCASE DESC`): views, then triggers (v0.16: after the
            // data, so replaying it fires none), then explicit
            // indexes, each in schema order. A text holding `--`
            // ends with its `;` on a line of its own, as there.
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
            for (vn, vq) in vtabs.iter().skip(vt_done) {
                if !writable {
                    let _ = out.write_all(b"PRAGMA writable_schema=ON;\n");
                    writable = true;
                }
                let lit = |b: &[u8]| String::from_utf8_lossy(b).replace('\'', "''");
                let _ = out.write_all(
                    format!(
                        "INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','{}','{}',0,'{}');\n",
                        lit(vn),
                        lit(vn),
                        lit(vq)
                    )
                    .as_bytes(),
                );
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
            // Last, as the pin shell writes it.
            if writable {
                let _ = out.write_all(b"PRAGMA writable_schema=OFF;\n");
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
    params: &mut Params,
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
            // The pin's order: close the current database first (a
            // WAL-mode log is checkpointed and removed), then open
            // the new one. Opening first would leave two connections
            // on one file, and the old one's close would unlink the
            // log the new one appends to. If the open fails, the pin
            // carries on with a substitute in-memory database.
            let old = std::mem::replace(db, open_db(":memory:")?);
            let closed = close_db(old);
            let opened = open_db(path).map(|new| *db = new);
            closed?;
            opened?;
            return Ok(Flow::Continue);
        }
        match db {
            Db::File(c) => dot_command(c, trimmed, params, out).await,
            Db::Mem(c) => dot_command(c, trimmed, params, out).await,
        }
    } else {
        match db {
            // The statement as written (only leading space dropped):
            // its trailing text can belong in sqlite_schema.sql.
            Db::File(c) => run_statement(c, input.trim_start(), params, out)
                .await
                .map(|()| Flow::Continue),
            Db::Mem(c) => run_statement(c, input.trim_start(), params, out)
                .await
                .map(|()| Flow::Continue),
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: hipp <database|:memory:> [sql]");
    eprintln!("       hipp <database|:memory:>            (interactive / stdin batch)");
    ExitCode::from(2)
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
    let code = session(&mut db, &args);
    // The oracle shell closes the database however the session
    // ended (v0.16: a WAL-mode log is checkpointed and removed).
    if let Err(e) = close_db(db) {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    code
}

/// Run the one-shot SQL argument, or the interactive / stdin
/// session, against `db`.
fn session(db: &mut Db, args: &[String]) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut params = Params::new();

    // One-shot mode: run the SQL argument (possibly several
    // statements), stop at the first error.
    if let Some(sql) = args.get(1) {
        for stmt in split_statements(sql) {
            match block_on(dispatch(db, &stmt, &mut params, &mut out)) {
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
        println!("Enter SQL or dot-commands (.tables .schema .dump .open .parameter .exit)");
    }
    let mut lines = stdin.lock().lines();
    let mut buffer = String::new();
    // A statement or dot-command failed: the script goes on, and
    // the shell exits 1 at its end (or at `.exit`), as the pin's
    // `process_input` returns `errCnt > 0`.
    let mut failed = false;
    let status = |failed: bool| {
        if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    };
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
        // Lines keep their trailing spaces (the pin's input buffer
        // does, and a schema text can hold them).
        let line = line.as_str();
        if buffer.is_empty() && line.trim_start().starts_with('.') {
            match block_on(dispatch(db, line.trim(), &mut params, &mut out)) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return status(failed),
                Err(e) => {
                    eprintln!("Error: {e}");
                    failed = true;
                }
            }
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(line);
        if buffer.trim_end().ends_with(';') && split_script(&buffer).1 {
            let script = std::mem::take(&mut buffer);
            for stmt in split_statements(&script) {
                match block_on(dispatch(db, &stmt, &mut params, &mut out)) {
                    Ok(Flow::Continue) => {}
                    Ok(Flow::Exit) => return status(failed),
                    Err(e) => {
                        // The rest of this input chunk is skipped (the
                        // pin's shell runs a chunk as one exec, which
                        // stops at its first error).
                        eprintln!("Error: {e}");
                        failed = true;
                        break;
                    }
                }
            }
        }
    }
    // A trailing unterminated statement still runs at EOF.
    if !buffer.trim().is_empty() {
        for stmt in split_statements(&buffer) {
            match block_on(dispatch(db, &stmt, &mut params, &mut out)) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return status(failed),
                Err(e) => {
                    eprintln!("Error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    let _ = out.flush();
    status(failed)
}
