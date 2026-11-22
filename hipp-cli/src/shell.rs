//! The shell's input loop and dot-commands (v0.17, `shell` unit): the
//! pinned SQLite 3.54.1 shell's `process_input` and `do_meta_command`
//! for the commands hipp has, probed on the live pin (STATUS.md's
//! `shell` notes): dot-command lines and their argument parsing
//! (quotes, backslash escapes in `"..."`, a trailing `;` dropped),
//! unique command prefixes (`.h` is `.headers`), the error messages
//! (`Usage: ...`, the `line N: ` / `argv[N]: ` caret form), `.bail`,
//! `.echo`, `.changes`, `.timer`, `.output` / `.once`, `.read` and the
//! settings `.mode`, `.headers`, `.separator`, `.nullvalue` and
//! `.width` the renderer (`render`) reads.

use std::io::{BufRead, Write};

use hipp::storage::block_on;

use crate::render::{Mode, Settings, Titles};
use crate::{Db, Params};

/// Where results go: standard output, a file (`.output FILE`,
/// `.once FILE`), a pipe (`|command`) or nowhere (`.output off`).
pub enum Output {
    Stdout(std::io::BufWriter<std::io::Stdout>),
    File(std::io::BufWriter<std::fs::File>),
    Pipe(std::process::Child),
    Off,
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Output::Stdout(w) => w.write(buf),
            Output::File(w) => w.write(buf),
            Output::Pipe(c) => match c.stdin.as_mut() {
                Some(s) => s.write(buf),
                None => Ok(buf.len()),
            },
            Output::Off => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Output::Stdout(w) => w.flush(),
            Output::File(w) => w.flush(),
            Output::Pipe(c) => match c.stdin.as_mut() {
                Some(s) => s.flush(),
                None => Ok(()),
            },
            Output::Off => Ok(()),
        }
    }
}

impl Output {
    pub fn stdout() -> Output {
        Output::Stdout(std::io::BufWriter::new(std::io::stdout()))
    }

    /// End this output: a file flushed, a pipe's command waited for.
    fn close(self) {
        match self {
            Output::Stdout(mut w) => {
                let _ = w.flush();
            }
            Output::File(mut w) => {
                let _ = w.flush();
            }
            Output::Pipe(mut c) => {
                drop(c.stdin.take());
                let _ = c.wait();
            }
            Output::Off => {}
        }
    }
}

/// `.timer`: off, on, or on for the next SQL only.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    Off,
    On,
    Once,
}

/// Everything of the session but its database.
pub struct Io {
    pub set: Settings,
    pub out: Output,
    /// `.show`'s `output:` (the file name, or `stdout`).
    pub out_name: String,
    /// The pin's `outCount`: a `.once` output ends after the next SQL
    /// or dot-command.
    pub once: u8,
    pub echo: bool,
    pub bail: bool,
    pub changes: bool,
    pub timer: Timer,
    pub params: Params,
}

/// Where an input line came from, for the caret errors' prefix.
#[derive(Clone)]
pub enum Ctx {
    /// The N-th command-line argument (one-shot mode).
    Argv(usize),
    /// Line N of standard input (`None`) or of a `.read` file.
    Line(Option<String>, usize),
}

impl Ctx {
    fn prefix(&self) -> String {
        match self {
            Ctx::Argv(n) => format!("argv[{n}]: "),
            Ctx::Line(None, n) => format!("line {n}: "),
            Ctx::Line(Some(f), n) => format!("{f}:{n}: "),
        }
    }
}

/// Whether an open error says the file is not a database (the pin's
/// `SQLITE_NOTADB`, which it reports only once a page is read).
fn not_a_database(e: &str) -> bool {
    matches!(e, "BadMagic" | "TruncatedHeader" | "UnsupportedPageSize")
}

/// What the input loop does after a dot-command.
pub enum Flow {
    Continue,
    /// `.quit`: stop reading this input (the program, at the top).
    Quit,
    /// `.exit` / `.exit 0`: end the program with the session's status.
    Exit,
    /// `.exit N`: end the program with status N (its low byte).
    ExitWith(u8),
}

/// A dot-command failed; its message is printed already.
pub struct Failed;

/// The pin's `integerValue`: a decimal or `0x` hexadecimal prefix, an
/// optional size suffix (`KiB`, `MB`, ...), 0 for none.
pub fn integer_value(arg: &str) -> i64 {
    let b = arg.as_bytes();
    let mut i = 0;
    let neg = match b.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };
    let mut v: i64 = 0;
    if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        i += 2;
        while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(16)) {
            v = v.wrapping_mul(16).wrapping_add(i64::from(d));
            i += 1;
        }
    } else {
        while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(10)) {
            v = v.wrapping_mul(10).wrapping_add(i64::from(d));
            i += 1;
        }
        const SUFFIX: [(&str, i64); 9] = [
            ("KiB", 1024),
            ("MiB", 1024 * 1024),
            ("GiB", 1024 * 1024 * 1024),
            ("KB", 1000),
            ("MB", 1_000_000),
            ("GB", 1_000_000_000),
            ("K", 1000),
            ("M", 1_000_000),
            ("G", 1_000_000_000),
        ];
        let rest = &arg[i..];
        if let Some((_, m)) = SUFFIX.iter().find(|(s, _)| rest.eq_ignore_ascii_case(s)) {
            v = v.wrapping_mul(*m);
        }
    }
    if neg { v.wrapping_neg() } else { v }
}

/// The pin's `booleanValue`: digits (or `0x` hex) as a number, `on` /
/// `yes` true, `off` / `no` false; anything else is false with the
/// pin's warning on stderr.
pub fn boolean_value(arg: &str) -> bool {
    let b = arg.as_bytes();
    let digits = if b.starts_with(b"0x") {
        b[2..].iter().all(u8::is_ascii_hexdigit)
    } else {
        !b.is_empty() && b.iter().all(u8::is_ascii_digit)
    };
    if digits {
        return integer_value(arg) & 0xffff_ffff != 0;
    }
    if arg.eq_ignore_ascii_case("on") || arg.eq_ignore_ascii_case("yes") {
        return true;
    }
    if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("no") {
        return false;
    }
    eprintln!("ERROR: Not a boolean value: \"{arg}\". Assuming \"no\".");
    false
}

/// The pin's `resolve_backslashes`, for a `"..."` argument.
fn resolve_backslashes(z: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(z.len());
    let mut i = 0;
    while i < z.len() {
        let mut c = z[i];
        if c == b'\\' && i + 1 < z.len() {
            i += 1;
            c = z[i];
            c = match c {
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                b'x' => {
                    let mut v = 0u8;
                    let mut n = 0;
                    while n < 2 {
                        let Some(d) = z.get(i + 1).and_then(|c| (*c as char).to_digit(16)) else {
                            break;
                        };
                        v = (v << 4) | d as u8;
                        i += 1;
                        n += 1;
                    }
                    v
                }
                b'0'..=b'7' => {
                    let mut v = c - b'0';
                    for _ in 0..2 {
                        match z.get(i + 1) {
                            Some(d @ b'0'..=b'7') => {
                                v = (v << 3).wrapping_add(d - b'0');
                                i += 1;
                            }
                            _ => break,
                        }
                    }
                    v
                }
                c => c,
            };
        }
        out.push(c);
        i += 1;
    }
    out
}

/// A dot-command line's arguments as the pin's `do_meta_command` splits
/// them: blank-separated words after the leading `.` (so `. print x`
/// is `.print x`, and a line of only `.` has none); a word starting
/// with `'` or `"` runs to its closing quote (a `"..."` word with its
/// backslash escapes resolved, and cut at the first NUL one makes, as
/// the pin's C strings are). Each with the offset of its first
/// character in the line. A trailing `;` (after trailing blanks) is not
/// part of the command. The first word is the command's name, without
/// the `.`.
pub fn tokens(line: &str) -> Vec<(String, usize)> {
    raw_tokens(line)
        .into_iter()
        .map(|(w, o)| (String::from_utf8_lossy(&w).into_owned(), o))
        .collect()
}

/// `tokens` as bytes: a `"..."` escape can make any byte (`"\xff"`),
/// which `.separator`, `.nullvalue`, `.print` and `.mode`'s values keep.
pub fn raw_tokens(line: &str) -> Vec<(Vec<u8>, usize)> {
    let mut z = line.trim_end_matches(|c: char| c.is_ascii_whitespace());
    if let Some(s) = z.strip_suffix(';') {
        z = s;
    }
    let b = z.as_bytes();
    let mut out = Vec::new();
    let mut h = usize::from(b.first() == Some(&b'.'));
    loop {
        while h < b.len() && b[h].is_ascii_whitespace() {
            h += 1;
        }
        if h >= b.len() {
            return out;
        }
        if b[h] == b'\'' || b[h] == b'"' {
            let delim = b[h];
            h += 1;
            let start = h;
            while h < b.len() && b[h] != delim {
                if b[h] == b'\\' && delim == b'"' && h + 1 < b.len() {
                    h += 1;
                }
                h += 1;
            }
            let word = &b[start..h.min(b.len())];
            let word = if delim == b'"' {
                let mut w = resolve_backslashes(word);
                if let Some(nul) = w.iter().position(|c| *c == 0) {
                    w.truncate(nul);
                }
                w
            } else {
                word.to_vec()
            };
            out.push((word, start));
            if h < b.len() {
                h += 1;
            }
        } else {
            let start = h;
            while h < b.len() && !b[h].is_ascii_whitespace() {
                h += 1;
            }
            out.push((b[start..h].to_vec(), start));
        }
    }
}

/// The dot-commands hipp has and the shortest prefix of each the pin
/// takes (`.h` is `.headers`, `.hel` `.help`; probed one prefix at a
/// time on the pin).
const COMMANDS: &[(&str, usize)] = &[
    ("bail", 3),
    ("changes", 3),
    ("databases", 2),
    ("dump", 1),
    ("echo", 1),
    ("exit", 2),
    ("fullschema", 2),
    ("headers", 1),
    ("help", 3),
    ("indexes", 2),
    ("limits", 5),
    ("mode", 1),
    ("nullvalue", 1),
    ("once", 2),
    ("open", 2),
    ("output", 1),
    ("parameter", 3),
    ("print", 3),
    ("quit", 1),
    ("read", 3),
    ("schema", 3),
    ("separator", 2),
    ("show", 3),
    ("tables", 2),
    ("timer", 5),
    ("width", 2),
];

/// The command a dot-command's first word (its name, without the `.`)
/// names, if hipp has it.
pub fn command(w: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .find(|(name, min)| w.len() >= *min && name.starts_with(w))
        .map(|(name, _)| *name)
}

/// The pin's one-line usage errors.
fn usage(text: &str) -> Result<Flow, Failed> {
    eprintln!("{text}");
    Err(Failed)
}

/// The quick scan of one input line (the pin's `quickscan`): whether
/// it leaves the text inside a string, quoted name or block comment,
/// whether it holds anything but blanks and comments, and whether it
/// ends (before blanks and comments) on a `;`.
#[derive(Clone, Copy, Default)]
pub struct Scan {
    wait: u8,
    dark: bool,
    semi: bool,
}

impl Scan {
    pub fn line(mut self, l: &str) -> Scan {
        let b = l.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if self.wait != 0 {
                let c = b[i];
                i += 1;
                if self.wait == b'*' {
                    if c == b'*' && b.get(i) == Some(&b'/') {
                        i += 1;
                        self.wait = 0;
                    }
                } else if c == self.wait {
                    if self.wait != b']' && b.get(i) == Some(&self.wait) {
                        i += 1;
                    } else {
                        self.wait = 0;
                    }
                }
                continue;
            }
            let c = b[i];
            i += 1;
            match c {
                c if c.is_ascii_whitespace() => {}
                b'-' if b.get(i) == Some(&b'-') => return self,
                b';' => self.semi = true,
                b'/' if b.get(i) == Some(&b'*') => {
                    i += 1;
                    self.wait = b'*';
                }
                b'[' => {
                    self.wait = b']';
                    self.dark = true;
                    self.semi = false;
                }
                b'`' | b'\'' | b'"' => {
                    self.wait = c;
                    self.dark = true;
                    self.semi = false;
                }
                _ => {
                    self.dark = true;
                    self.semi = false;
                }
            }
        }
        self
    }

    /// Nothing but blanks and comments, outside any of them.
    pub fn white(&self) -> bool {
        self.wait == 0 && !self.dark && !self.semi
    }

    /// Ends on a `;`, outside any string or comment.
    pub fn semi_term(&self) -> bool {
        self.wait == 0 && self.semi
    }
}

/// Whether a chunk of SQL ends its last statement (only blanks and
/// comments after the last `;`; a trigger's body taken whole).
fn complete(sql: &str) -> bool {
    let (stmts, done) = crate::split_script(sql);
    done || stmts.last().is_some_and(|s| crate::no_sql(s))
}

/// The CPU seconds this process has used, user and system (Linux:
/// `/proc/self/stat`; elsewhere 0).
fn cpu_times() -> (f64, f64) {
    let Ok(s) = std::fs::read_to_string("/proc/self/stat") else {
        return (0.0, 0.0);
    };
    let Some(rest) = s.rsplit_once(')').map(|(_, r)| r) else {
        return (0.0, 0.0);
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 100.0;
    (tick(11), tick(12))
}

/// The session: the database (opened when input first needs it), the
/// settings and where output goes.
pub struct Shell {
    pub db: Option<Db>,
    pub path: String,
    pub io: Io,
    pub ctx: Ctx,
    depth: usize,
    /// The input levels below the first `.read`: 1 when the session
    /// reads standard input (the pin counts that `process_input` toward
    /// its nesting limit of 25), 0 for the command-line arguments.
    pub base: usize,
    /// Standard input is a terminal: prompts before each line.
    pub interactive: bool,
    /// The exit status a failed one-shot argument ends with, when not
    /// 1 (`.fullschema` on a file that is not a database: 26).
    pub fail_code: Option<u8>,
    /// The databases (`main`, attached names) a write transaction the
    /// pin would hold has touched (`track_txn`).
    wtx: Vec<String>,
    /// The line of the input being read (kept under a command-line
    /// argument too, whose errors are named by the argument).
    line: usize,
}

impl Shell {
    pub fn new(path: String) -> Shell {
        Shell {
            db: None,
            path,
            io: Io {
                set: Settings::default(),
                out: Output::stdout(),
                out_name: "stdout".to_string(),
                once: 0,
                echo: false,
                bail: false,
                changes: false,
                timer: Timer::Off,
                params: Params::new(),
            },
            ctx: Ctx::Line(None, 0),
            depth: 0,
            base: 0,
            interactive: false,
            fail_code: None,
            wtx: Vec::new(),
            line: 0,
        }
    }

    /// The database, opened now if no input has needed it yet.
    fn db(&mut self) -> Result<&mut Db, String> {
        if self.db.is_none() {
            self.db = Some(crate::open_db(&self.path)?);
        }
        self.db.as_mut().ok_or_else(String::new)
    }

    /// Print `line` when `.echo` is on.
    pub fn echo(&mut self, line: &str) {
        if self.io.echo {
            let _ = self.io.out.write_all(line.as_bytes());
            let _ = self.io.out.write_all(b"\n");
        }
    }

    /// The caret error: the line, a caret under the argument at
    /// `offset` with `msg`, and the hint line if any.
    fn caret(&self, line: &str, offset: usize, msg: &str, hint: Option<&str>) -> Failed {
        let p = self.ctx.prefix();
        eprintln!("{p}{line}");
        eprintln!("{p}{}^--- {msg}", " ".repeat(offset));
        if let Some(h) = hint {
            eprintln!("{p}{h}");
        }
        Failed
    }

    /// Point the output somewhere else (`.output` / `.once`).
    fn redirect(&mut self, target: &str, once: bool) -> Result<(), Failed> {
        self.reset_output();
        if target.is_empty() || (!once && target == "stdout") {
            return Ok(());
        }
        if !once && target == "off" {
            self.io.out = Output::Off;
            self.io.out_name = "off".to_string();
            return Ok(());
        }
        let _ = self.io.out.flush();
        if let Some(cmd) = target.strip_prefix('|') {
            match std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => {
                    self.io.out = Output::Pipe(c);
                    self.io.out_name = target.to_string();
                }
                Err(_) => {
                    eprintln!("Error: cannot open pipe \"{cmd}\"");
                    return Err(Failed);
                }
            }
        } else {
            match std::fs::File::create(target) {
                Ok(f) => {
                    self.io.out = Output::File(std::io::BufWriter::new(f));
                    self.io.out_name = target.to_string();
                }
                Err(_) => {
                    eprintln!("Error: cannot open \"{target}\"");
                    eprintln!("Error: cannot write to \"{target}\"");
                    return Err(Failed);
                }
            }
        }
        if once {
            self.io.once = 2;
        }
        Ok(())
    }

    /// Back to standard output (the pin's `output_reset`).
    fn reset_output(&mut self) {
        if !matches!(self.io.out, Output::Stdout(_)) {
            let old = std::mem::replace(&mut self.io.out, Output::stdout());
            old.close();
        }
        self.io.out_name = "stdout".to_string();
        self.io.once = 0;
    }

    /// The end of a session: output flushed (a file closed).
    pub fn finish(&mut self) {
        self.reset_output();
        let _ = self.io.out.flush();
    }

    /// After a dot-command: a `.once` output ends after the command
    /// that follows it.
    fn after_meta(&mut self) {
        if self.io.once > 0 {
            self.io.once -= 1;
            if self.io.once == 0 {
                self.reset_output();
            }
        }
    }

    /// Run one chunk of SQL (the pin's `runOneSqlLine`): its
    /// statements in order up to the first error, timed by `.timer`,
    /// followed by the `.changes` line. `Err` when one failed (its
    /// message printed).
    pub fn sql(&mut self, chunk: &str) -> Result<(), Failed> {
        let stmts = crate::split_statements(chunk);
        if let Err(e) = self.db() {
            eprintln!("Error: {e}");
            return Err(Failed);
        }
        let t0 = (self.io.timer != Timer::Off).then(|| (std::time::Instant::now(), cpu_times()));
        let mut res = Ok(());
        for s in &stmts {
            if crate::no_sql(s) {
                continue;
            }
            let r = match self.db.as_mut() {
                Some(Db::File(c)) => {
                    block_on(crate::run_statement(c, s.trim_start(), &mut self.io))
                }
                Some(Db::Mem(c)) => block_on(crate::run_statement(c, s.trim_start(), &mut self.io)),
                None => Ok(()),
            };
            self.track_txn(s.trim_start(), r.is_ok());
            if let Err(e) = r {
                eprintln!("Error: {e}");
                res = Err(Failed);
                break;
            }
        }
        if let Some((t, (u0, s0))) = t0 {
            let (u1, s1) = cpu_times();
            let line = format!(
                "Run Time: real {:.6} user {:.6} sys {:.6}\n",
                t.elapsed().as_secs_f64(),
                (u1 - u0).max(0.0),
                (s1 - s0).max(0.0)
            );
            let _ = self.io.out.write_all(line.as_bytes());
            if self.io.timer == Timer::Once {
                self.io.timer = Timer::Off;
            }
        }
        if res.is_ok() && self.io.changes {
            let (c, t) = match self.db.as_ref() {
                Some(Db::File(c)) => (c.changes(), c.total_changes()),
                Some(Db::Mem(c)) => (c.changes(), c.total_changes()),
                None => (0, 0),
            };
            let _ = writeln!(self.io.out, "changes: {c}   total_changes: {t}");
        }
        if self.io.once > 0 {
            self.reset_output();
        }
        res
    }

    /// Follow the write transactions the pin opens where hipp writes
    /// nothing, so a fresh file gets its page 1 as on the pin when one
    /// commits (`lazy::LazyFile::commit_empty`): `BEGIN IMMEDIATE` /
    /// `EXCLUSIVE` (every database file), `PRAGMA [schema.]
    /// incremental_vacuum`, and `schema_version` / `default_cache_size`
    /// set (that schema's file). Ended by a `ROLLBACK` (or a failed
    /// statement) they write nothing.
    fn track_txn(&mut self, stmt: &str, ok: bool) {
        let Some(Db::File(c)) = self.db.as_mut() else {
            return;
        };
        let word: String = stmt
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        if ok && (word == "begin" || word == "pragma") {
            let arena = hipp::arena::Arena::new(hipp::Scope::Statement, 0, 1 << 20);
            let ast = hipp::sql::parser::parse::<()>(
                stmt.as_bytes(),
                &arena,
                hipp::sql::parser::Limits::PIN,
            );
            match ast {
                Ok(hipp::sql::ast::Ast::BeginImmediate) => {
                    self.wtx.push("main".to_string());
                    let mut i = 0;
                    while let Some((n, _)) = c.attached_db(i) {
                        self.wtx
                            .push(String::from_utf8_lossy(n).to_ascii_lowercase());
                        i += 1;
                    }
                }
                Ok(hipp::sql::ast::Ast::Pragma {
                    schema,
                    name,
                    value,
                    ..
                }) => {
                    let text = |h: hipp::arena::ABytes| {
                        arena
                            .slice_at(h.pos as usize, h.len as usize)
                            .unwrap_or(&[])
                            .to_ascii_lowercase()
                    };
                    let name = text(name);
                    let writes = name == b"incremental_vacuum"
                        || (value.is_some()
                            && (name == b"schema_version" || name == b"default_cache_size"));
                    let db = schema.map_or_else(
                        || "main".to_string(),
                        |s| String::from_utf8_lossy(&text(s)).into_owned(),
                    );
                    if writes && db != "temp" {
                        self.wtx.push(db);
                    }
                }
                _ => {}
            }
        }
        if self.wtx.is_empty() || c.in_explicit_transaction() {
            return;
        }
        let dbs = std::mem::take(&mut self.wtx);
        if !ok || word == "rollback" {
            return;
        }
        for d in dbs {
            if d == "main" {
                // (Its page size and encoding are fixed from now on, as
                // after the pin's commit.)
                c.pager_mut().fix_layout();
                c.storage().commit_empty();
                continue;
            }
            let mut i = 0;
            while let Some((n, a)) = c.attached_db(i) {
                if n.eq_ignore_ascii_case(d.as_bytes()) {
                    a.storage().commit_empty();
                }
                i += 1;
            }
        }
    }

    /// One dot-command line.
    pub fn meta(&mut self, line: &str) -> Result<Flow, Failed> {
        let r = self.meta_inner(line);
        self.after_meta();
        r
    }

    fn meta_inner(&mut self, line: &str) -> Result<Flow, Failed> {
        let args = tokens(line);
        let Some((word, _)) = args.first() else {
            return Ok(Flow::Continue);
        };
        let Some(cmd) = command(word) else {
            eprintln!(
                "Error: unknown command or invalid arguments:  \"{word}\". Enter \".help\" for help"
            );
            return Err(Failed);
        };
        let a: Vec<&str> = args.iter().map(|(s, _)| s.as_str()).collect();
        let raw = raw_tokens(line);
        let n = a.len();
        match cmd {
            "bail" | "changes" | "echo" => {
                if n != 2 {
                    return usage(&format!("Usage: .{cmd} on|off"));
                }
                let v = boolean_value(a[1]);
                match cmd {
                    "bail" => self.io.bail = v,
                    "changes" => self.io.changes = v,
                    _ => self.io.echo = v,
                }
                Ok(Flow::Continue)
            }
            "timer" => {
                if n != 2 {
                    return usage("Usage: .timer on|off|once");
                }
                self.io.timer = if a[1].eq_ignore_ascii_case("once") {
                    Timer::Once
                } else if boolean_value(a[1]) {
                    Timer::On
                } else {
                    Timer::Off
                };
                Ok(Flow::Continue)
            }
            "headers" => {
                if n != 2 {
                    return usage("Usage: .headers off|on|always");
                }
                self.io.set.titles = if a[1].eq_ignore_ascii_case("always") {
                    Titles::Always
                } else if boolean_value(a[1]) {
                    Titles::On
                } else {
                    Titles::Off
                };
                Ok(Flow::Continue)
            }
            "nullvalue" => {
                if n != 2 {
                    return usage("Usage: .nullvalue STRING");
                }
                self.io.set.null = raw[1].0.clone();
                Ok(Flow::Continue)
            }
            "separator" => {
                if n >= 2 {
                    self.io.set.colsep = raw[1].0.clone();
                }
                if n >= 3 {
                    self.io.set.rowsep = raw[2].0.clone();
                }
                if !(2..=3).contains(&n) {
                    return usage("Usage: .separator COL ?ROW?");
                }
                Ok(Flow::Continue)
            }
            "width" => {
                self.io.set.widths = a[1..].iter().map(|w| integer_value(w)).collect();
                Ok(Flow::Continue)
            }
            "print" => {
                let words: Vec<&[u8]> = raw[1..].iter().map(|(w, _)| w.as_slice()).collect();
                let _ = self.io.out.write_all(&words.join(&b' '));
                let _ = self.io.out.write_all(b"\n");
                Ok(Flow::Continue)
            }
            "help" => {
                let text = crate::help::help(a.get(1).copied());
                let _ = self.io.out.write_all(text.as_bytes());
                Ok(Flow::Continue)
            }
            "show" => {
                if n != 1 {
                    return usage("Usage: .show");
                }
                self.show();
                Ok(Flow::Continue)
            }
            "mode" => self.mode(line, &args),
            "output" | "once" => {
                let once = cmd == "once";
                let mut file: Option<&(String, usize)> = None;
                for t in &args[1..] {
                    if t.0.starts_with('-') && file.is_none() {
                        // The pin's options (a spreadsheet, an editor, a
                        // browser, a byte-order mark, ...), which hipp
                        // does not take; any other is unknown.
                        let o = t.0.trim_start_matches('-');
                        let known = t.0.len() > 1
                            && matches!(
                                o,
                                "bom"
                                    | "e"
                                    | "error-prefix"
                                    | "keep"
                                    | "plain"
                                    | "show"
                                    | "w"
                                    | "x"
                            );
                        let msg = if known {
                            "option not supported by hipp"
                        } else {
                            "unknown option"
                        };
                        return Err(self.caret(line, t.1, msg, None));
                    }
                    if file.is_some() {
                        return Err(self.caret(line, t.1, "surplus argument", None));
                    }
                    file = Some(t);
                }
                let target = file.map_or("", |t| t.0.as_str());
                self.redirect(target, once && !target.is_empty())?;
                Ok(Flow::Continue)
            }
            "read" => {
                if n != 2 {
                    return usage("Usage: .read FILE");
                }
                self.read(a[1])
            }
            "exit" => match integer_value(a.get(1).copied().unwrap_or("")) {
                0 => Ok(Flow::Exit),
                v => Ok(Flow::ExitWith(v as u8)),
            },
            "quit" => Ok(Flow::Quit),
            "open" => self.open(&args[1..]),
            _ => self.db_meta(cmd, line, &args),
        }
    }

    /// `.show`.
    fn show(&mut self) {
        let s = &self.io.set;
        let mut b: Vec<u8> = Vec::new();
        let on = |v: bool| if v { "on" } else { "off" };
        let _ = writeln!(b, "{:>12}: {}", "echo", on(self.io.echo));
        let _ = writeln!(b, "{:>12}: off", "eqp");
        let _ = writeln!(b, "{:>12}: auto", "explain");
        let _ = writeln!(b, "{:>12}: {}", "headers", s.titles_word());
        let _ = writeln!(b, "{:>12}: {}", "mode", s.mode.name());
        b.extend_from_slice(b"   nullvalue: ");
        b.extend_from_slice(&crate::render::c_string(&s.null));
        b.push(b'\n');
        let _ = writeln!(b, "{:>12}: {}", "output", self.io.out_name);
        b.extend_from_slice(b"colseparator: ");
        b.extend_from_slice(&crate::render::c_string(&s.colsep));
        b.extend_from_slice(b"\nrowseparator: ");
        b.extend_from_slice(&crate::render::c_string(&s.rowsep));
        b.push(b'\n');
        let _ = writeln!(b, "{:>12}: off", "stats");
        b.extend_from_slice(b"       width: ");
        for w in &s.widths {
            let _ = write!(b, "{w} ");
        }
        b.push(b'\n');
        let _ = writeln!(b, "{:>12}: {}", "filename", self.path);
        let _ = self.io.out.write_all(&b);
    }

    /// `.mode [MODE] [OPTIONS]`.
    fn mode(&mut self, line: &str, args: &[(String, usize)]) -> Result<Flow, Failed> {
        let raw = raw_tokens(line);
        const HINT: Option<&str> = Some("Use \".help .mode\" for more info");
        if args.len() == 1 {
            let mut d = self.io.set.describe();
            d.push(b'\n');
            let _ = self.io.out.write_all(&d);
            return Ok(Flow::Continue);
        }
        let mut words = 0;
        let mut i = 1;
        while i < args.len() {
            let (t, off) = (&args[i].0, args[i].1);
            if t.starts_with('-') && t.len() > 1 {
                let opt = t.strip_prefix("--").unwrap_or(&t[1..]);
                let mut value = || -> Result<(&str, &[u8]), Failed> {
                    i += 1;
                    match (args.get(i), raw.get(i)) {
                        (Some((v, _)), Some((r, _))) => Ok((v.as_str(), r.as_slice())),
                        _ => Err(self.caret(line, off, "missing argument", None)),
                    }
                };
                match opt {
                    "list" => {
                        let _ = writeln!(
                            self.io.out,
                            "available modes: {}",
                            crate::render::PIN_MODES.join(" ")
                        );
                    }
                    "titles" => {
                        let v = value()?.0;
                        self.io.set.titles = match v {
                            "on" => Titles::On,
                            "off" => Titles::Off,
                            "always" => Titles::Always,
                            "auto" => Titles::Auto,
                            "plain" | "sql" | "csv" | "html" | "tcl" | "json" => {
                                return Err(self.caret(
                                    line,
                                    args[i].1,
                                    "--titles value not supported by hipp",
                                    None,
                                ));
                            }
                            _ => {
                                return Err(self.caret(
                                    line,
                                    args[i].1,
                                    "bad --titles value",
                                    Some(
                                        "should be off, on, always, auto, plain, sql, csv, html, tcl, or json",
                                    ),
                                ));
                            }
                        };
                    }
                    "colsep" => self.io.set.colsep = value()?.1.to_vec(),
                    "rowsep" => self.io.set.rowsep = value()?.1.to_vec(),
                    "null" => self.io.set.null = value()?.1.to_vec(),
                    "tablename" => self.io.set.table = value()?.1.to_vec(),
                    "widths" => {
                        let v = value()?.0;
                        let mut ws = Vec::new();
                        for w in v.split([',', ' ']).filter(|w| !w.is_empty()) {
                            let ok = w.strip_prefix('-').unwrap_or(w);
                            if ok.is_empty() || !ok.bytes().all(|c| c.is_ascii_digit()) {
                                eprintln!(
                                    "{}should be a comma-separated list if integers",
                                    self.ctx.prefix()
                                );
                                return Err(Failed);
                            }
                            ws.push(integer_value(w));
                        }
                        self.io.set.widths = ws;
                    }
                    "align" | "border" | "blob-quote" | "charlimit" | "escape" | "fpfmt"
                    | "ifmt" | "linelimit" | "limits" | "multiinsert" | "once" | "quote"
                    | "reset" | "rowcount" | "sw" | "screenwidth" | "tag" | "textjsonb"
                    | "titlelimit" | "v" | "verbose" | "wordwrap" | "wrap" | "ww" => {
                        return Err(self.caret(line, off, "option not supported by hipp", None));
                    }
                    _ => return Err(self.caret(line, off, "bad option", HINT)),
                }
                i += 1;
                continue;
            }
            words += 1;
            if words == 1 {
                match Mode::named(t) {
                    Some(m) => self.io.set.set_mode(m),
                    None if crate::render::PIN_MODES.contains(&t.as_str()) => {
                        return Err(self.caret(line, off, "mode not supported by hipp", None));
                    }
                    None => return Err(self.caret(line, off, "unknown mode", HINT)),
                }
            } else if words == 2 {
                if self.io.set.mode == Mode::Insert {
                    self.io.set.table = raw[i].0.clone();
                }
            } else {
                return Err(self.caret(line, off, "bad argument", HINT));
            }
            i += 1;
        }
        Ok(Flow::Continue)
    }

    /// `.open [FILE]`: close the database (a WAL-mode log checkpointed
    /// and removed), open the new one at once.
    fn open(&mut self, args: &[(String, usize)]) -> Result<Flow, Failed> {
        // The pin's `.open` options, which hipp does not take; any
        // other is the pin's "unknown option".
        const OPTS: &[&str] = &[
            "append",
            "deserialize",
            "hexdb",
            "ifexist",
            "ifexists",
            "maxsize",
            "new",
            "normal",
            "nofollow",
            "readonly",
            "zip",
        ];
        if let Some((o, _)) = args.iter().find(|(a, _)| a.starts_with('-')) {
            let name = o.trim_start_matches('-');
            if OPTS.contains(&name) {
                eprintln!("Error: .open option not supported by hipp: {o}");
            } else {
                eprintln!("unknown option: {o}");
            }
            return Err(Failed);
        }
        if args.len() > 1 {
            eprintln!("extra argument: \"{}\"", args[1].0);
            return Err(Failed);
        }
        let path = args
            .first()
            .map_or(":memory:", |(p, _)| p.as_str())
            .to_string();
        let closed = self.db.take().map(crate::close_db).unwrap_or(Ok(()));
        let opened = crate::open_db(&path);
        self.path = path.clone();
        let res = match opened {
            Ok(d) => {
                self.db = Some(d);
                Ok(())
            }
            // (The pin's `.open` reads no page: the file's error comes
            // with the first command that does, as at start-up.)
            Err(e) if not_a_database(&e) => Ok(()),
            Err(e) => {
                // The pin carries on with an in-memory database.
                self.db = crate::open_db(":memory:").ok();
                Err(e)
            }
        };
        if let Err(e) = closed.and(res) {
            eprintln!("Error: {e}");
            return Err(Failed);
        }
        Ok(Flow::Continue)
    }

    /// A dot-command that reads the database.
    fn db_meta(&mut self, cmd: &str, line: &str, args: &[(String, usize)]) -> Result<Flow, Failed> {
        crate::db_precheck(cmd, args, &|off, msg| self.caret(line, off, msg, None))?;
        if let Err(e) = self.db() {
            // A file that is not a database: the pin opens it without
            // reading it, so the commands that need no page of it, or
            // shrug off the error, carry on (v0.17 review fix).
            if not_a_database(&e) {
                match cmd {
                    "tables" | "indexes" => return Ok(Flow::Continue),
                    "limits" => {
                        let a: Vec<&str> = args.iter().map(|(s, _)| s.as_str()).collect();
                        return crate::limits(&a, &mut self.io.out).map(|()| Flow::Continue);
                    }
                    "databases" => {
                        // (The file's full name as `FileStorage` makes it.)
                        let full = std::fs::canonicalize(&self.path)
                            .unwrap_or_else(|_| std::path::PathBuf::from(&self.path));
                        let _ = writeln!(self.io.out, "main: {} r/w", full.display());
                        return Ok(Flow::Continue);
                    }
                    "fullschema" => {
                        // (Its statistics query fails: SQLITE_NOTADB,
                        // 26, the one-shot exit status.)
                        let _ = self.io.out.write_all(b"/* No STAT tables available */\n");
                        self.fail_code = Some(26);
                        return Err(Failed);
                    }
                    _ => {}
                }
            }
            eprintln!("Error: {e}");
            return Err(Failed);
        }
        let ctx = self.ctx.clone();
        let r = match self.db.as_mut() {
            Some(Db::File(c)) => {
                block_on(crate::db_command(c, cmd, line, args, &mut self.io, &ctx))
            }
            Some(Db::Mem(c)) => block_on(crate::db_command(c, cmd, line, args, &mut self.io, &ctx)),
            None => Ok(()),
        };
        r.map(|()| Flow::Continue)
    }

    /// `.read FILE`: the file as input, its errors this command's.
    fn read(&mut self, file: &str) -> Result<Flow, Failed> {
        if let Some(cmd) = file.strip_prefix('|') {
            let _ = self.io.out.flush();
            let out = std::process::Command::new("sh").arg("-c").arg(cmd).output();
            let Ok(out) = out else {
                eprintln!("Error: cannot open \"{cmd}\"");
                return Err(Failed);
            };
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            return self.read_from(std::io::Cursor::new(text), file);
        }
        let Ok(f) = std::fs::File::open(file) else {
            eprintln!("Error: cannot open \"{file}\"");
            return Err(Failed);
        };
        self.read_from(std::io::BufReader::new(f), file)
    }

    fn read_from(&mut self, input: impl BufRead, name: &str) -> Result<Flow, Failed> {
        if self.depth + self.base >= 25 {
            // (The line of the `.read` in the file that holds it, also
            // under a command-line argument.)
            let at = match &self.ctx {
                Ctx::Line(_, n) => *n,
                Ctx::Argv(_) => self.line.max(1),
            };
            eprintln!("{name}: Input nesting limit (25) reached at line {at}. Check recursion.");
            return Err(Failed);
        }
        let saved = self.ctx.clone();
        let saved_line = self.line;
        self.depth += 1;
        let file_ctx = match &saved {
            Ctx::Argv(_) => None,
            _ => Some(name.to_string()),
        };
        let (errs, flow) = self.process(input, file_ctx);
        self.depth -= 1;
        self.ctx = saved;
        self.line = saved_line;
        match flow {
            Flow::Exit | Flow::ExitWith(_) => Ok(flow),
            _ if errs > 0 => Err(Failed),
            _ => Ok(Flow::Continue),
        }
    }

    /// The pin's `process_input` over `input`: dot-command lines (a
    /// first character `.` between statements), `#` lines skipped, SQL
    /// gathered into chunks that end on a `;` and run as one, a line
    /// of only blanks and comments dropped; `.echo` prints each as it
    /// is read. `file`: the `.read` file the lines come from (`None`:
    /// standard input, or a `.read` under a command-line argument,
    /// whose errors name the argument). Returns the errors seen and
    /// how the input ended.
    pub fn process(&mut self, input: impl BufRead, file: Option<String>) -> (usize, Flow) {
        let argv = matches!(self.ctx, Ctx::Argv(_));
        let mut errs = 0usize;
        let mut sql = String::new();
        let mut scan = Scan::default();
        let mut lineno = 0usize;
        let mut start = 0usize;
        let mut lines = input.lines();
        loop {
            if errs > 0 && self.io.bail {
                break;
            }
            if self.interactive && self.depth == 0 {
                let _ = self
                    .io
                    .out
                    .write_all(if sql.is_empty() { b"hipp> " } else { b"...> " });
            }
            let _ = self.io.out.flush();
            let line = match lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    eprintln!("Error: {e}");
                    errs += 1;
                    break;
                }
                None => break,
            };
            lineno += 1;
            self.line = lineno;
            if !argv {
                self.ctx = Ctx::Line(file.clone(), lineno);
            }
            // (`lines` drops the `\n` and one `\r` before it, as the
            // pin's `local_getline` does.)
            if sql.is_empty() {
                let s = Scan::default().line(&line);
                if s.white() {
                    self.echo(&line);
                    continue;
                }
                if line.starts_with('.') || line.starts_with('#') {
                    self.echo(&line);
                    if line.starts_with('#') {
                        continue;
                    }
                    let _ = self.io.out.flush();
                    match self.meta(&line) {
                        Ok(Flow::Continue) => {}
                        Ok(f) => return (errs, f),
                        Err(Failed) => errs += 1,
                    }
                    continue;
                }
                start = lineno;
                sql = line.trim_start().to_string();
            } else {
                sql.push('\n');
                sql.push_str(&line);
            }
            scan = scan.line(&line);
            if scan.semi_term() && complete(&sql) {
                let chunk = std::mem::take(&mut sql);
                scan = Scan::default();
                if !argv {
                    self.ctx = Ctx::Line(file.clone(), start);
                }
                self.echo(&chunk);
                if self.sql(&chunk).is_err() {
                    errs += 1;
                }
            } else if scan.white() {
                let chunk = std::mem::take(&mut sql);
                scan = Scan::default();
                self.echo(&chunk);
            }
        }
        if !sql.is_empty() && !(errs > 0 && self.io.bail) {
            self.echo(&sql);
            if self.sql(&sql).is_err() {
                errs += 1;
            }
        }
        (errs, Flow::Continue)
    }
}
