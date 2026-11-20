//! `.help` (v0.17, `shell` unit): the pinned shell's help text for the
//! dot-commands hipp implements, verbatim (the pin's `azHelp` entries
//! and its `.mode` / `.output` / `.once` usage texts, captured from the
//! live pin), and its lookup rules.

/// The pin's help entries for the commands hipp has, in its order: a
/// first line (`.name ...`) and its detail lines (indented).
pub const ENTRIES: &str = r#".bail on|off             Stop after hitting an error.  Default OFF
.changes on|off          Show number of rows changed by SQL
.databases               List names and files of attached databases
.dump ?OBJECTS?          Render database content as SQL
   Options:
     --data-only            Output only INSERT statements
     --newlines             Allow unescaped newline characters in output
     --nosys                Omit system tables (ex: "sqlite_stat1")
     --preserve-rowids      Include ROWID values in the output
   OBJECTS is a LIKE pattern for tables, indexes, triggers or views to dump
   Additional LIKE patterns can be given in subsequent arguments
.echo on|off             Turn command echo on or off
.exit ?CODE?             Exit this program with return-code CODE
.fullschema ?--indent?   Show schema and the content of sqlite_stat tables
.headers on|off          Turn display of headers on or off
.help ?-all? ?PATTERN?   Show help text for PATTERN
.indexes ?PATTERN?       Show names of indexes matching PATTERN
   -a|--all                Also show system-generated indexes
   --expr                  Show only expression indexes
   --sys                   Show only system-generated indexes
.limit ?LIMIT? ?VAL?     Display or change the value of an SQLITE_LIMIT
.mode ?MODE? ?OPTIONS?   Set output mode
.nullvalue STRING        Use STRING in place of NULL values
.once ?OPTIONS? ?FILE?   Output for the next SQL command only to FILE
.open ?OPTIONS? ?FILE?   Close existing database and reopen FILE
     Options:
        --append        Use appendvfs to append database to the end of FILE
        --deserialize   Load into memory using sqlite3_deserialize()
        --hexdb         Load the output of "dbtotxt" as an in-memory db
        --ifexist       Only open if FILE already exists
        --maxsize N     Maximum size for --hexdb or --deserialized database
        --new           Initialize FILE to an empty database
        --normal        FILE is an ordinary SQLite database
        --nofollow      Do not follow symbolic links
        --readonly      Open FILE readonly
        --zip           FILE is a ZIP archive
.output ?FILE?           Send output to FILE or stdout if FILE is omitted
.parameter CMD ...       Manage SQL parameter bindings
   clear                   Erase all bindings
   init                    Initialize the TEMP table that holds bindings
   list                    List the current parameter bindings
   set PARAMETER VALUE     Given SQL parameter PARAMETER a value of VALUE
                           PARAMETER should start with one of: $ : @ ?
   unset PARAMETER         Remove PARAMETER from the binding table
.print STRING...         Print literal STRING
.quit                    Stop interpreting input stream, exit if primary.
.read FILE               Read input from FILE or command output
    If FILE begins with "|", it is a command that generates the input.
.schema ?PATTERN?        Show the CREATE statements matching PATTERN
   Options:
      --indent             Try to pretty-print the schema
      --nosys              Omit objects whose names start with "sqlite_"
.tables ?TABLE?          List names of tables matching LIKE pattern TABLE
.timer on|off|once       Turn SQL timer on or off.
"#;

/// `.help mode`: the pin's usage text.
pub const USAGE_MODE: &str = r#"USAGE: .mode [MODE] [OPTIONS]

Change the output mode to MODE and/or apply OPTIONS to the output mode.
Arguments are processed from left to right.  If no arguments, show the
current output mode and relevant options.  Use the --list option to get
a list of available MODE values.  Use -v to show all the current output
mode settings.

Options:
  --align STRING           Set the alignment of text in columnar modes
                           String consists of characters 'L', 'C', 'R'
                           meaning "left", "centered", and "right", with
                           one letter per column starting from the left.
                           Unspecified alignment defaults to 'L'.
  --blob-quote ARG         ARG can be "auto", "text", "sql", "hex", "tcl",
                           "json", or "size".  Default is "auto".
  --border on|off          Show outer border on "box" and "table" modes.
  --charlimit N            Set the maximum number of output characters to
                           show for any single SQL value to N. Longer values
                           truncated. Zero means "no limit".
  --colsep STRING          Use STRING as the column separator
  --escape ESC             Enable/disable escaping of control characters
                           found in the output. ESC can be "auto", "off",
                           "ascii", or "symbol".  Default is "auto"
  --fpfmt STRING           String is a printf-style format string used to
                           render floating-point values.
  --ifmt STRING            String is a printf-style format string used to
                           render integer values.
  --linelimit N            Set the maximum number of output lines to show for
                           any single SQL value to N. Longer values are
                           truncated. Zero means "no limit". Only works
                           in "line" mode and in columnar modes.
  --limits L,C,T           Shorthand for "--linelimit L --charlimit C
                           --titlelimit T". The ",T" can be omitted in which
                           case the --titlelimit is unchanged.  The argument
                           can also be "off" to mean "0,0,0" or "on" to
                           mean "5,300,20".
  --list                   List available modes
  --multiinsert N          In "insert" mode, put multiple rows on a single
                           INSERT statement until the size exceeds N bytes.
  --null STRING            Render SQL NULL values as the given string
  --once                   Setting changes to the right are reverted after
                           the next SQL command.
  --quote ARG              Enable/disable quoting of text. ARG can be
                           "off", "on", "sql", "relaxed", "csv", "html",
                           "tcl", or "json". "off" means show the text as-is.
                           "on" is an alias for "sql".
  --reset                  Changes all mode settings back to their default.
  --rowcount BOOLEAN       Show "(N rows)" at the end of query results.
  --rowsep STRING          Use STRING as the row separator
  --sw|--screenwidth N     Declare the screen width of the output device
                           to be N characters.  An attempt may be made to
                           wrap output text to fit within this limit. Zero
                           means "no limit".  Or N can be "auto" to set the
                           width automatically.
  --tablename NAME         Set the name of the table for "insert" mode.
  --tag NAME               Save current settings as a new mode called NAME.
  --textjsonb BOOLEAN      If enabled, JSONB text is displayed as text JSON.
  --title ARG              Whether or not to show column headers, and if so
                           how to encode them.  ARG can be "off", "on",
                           "always", "sql", "csv", "html", "tcl", or "json".
  --titlelimit N           Limit the length of column titles to N characters.
  -v|--verbose             Verbose output
  --widths LIST            Set the columns widths for columnar modes. The
                           argument is a list of integers, one for each
                           column. A "0" width means use a dynamic width
                           based on the actual width of data. If there are
                           fewer entries in LIST than columns, "0" is used
                           for the unspecified widths.
  --wordwrap BOOLEAN       Enable/disable word wrapping
  --wrap N                 Wrap columns wider than N characters
  --ww                     Shorthand for "--wordwrap on"
"#;

/// `.help output`: the pin's usage text.
pub const USAGE_OUTPUT: &str = r#"USAGE: .output [OPTIONS] [FILE]

Begin redirecting output to FILE.  Or if FILE is omitted, revert
to sending output to the console.  If FILE begins with "|" then
the remainder of file is taken as a pipe and output is directed
into that pipe.  If FILE is "memory" then output is captured in an
internal memory buffer.  If FILE is "off" then output is redirected
into /dev/null or the equivalent.

Options:
  --bom             Prepend a byte-order mark to the output
  -e                Accumulate output in a temporary text file then
                    launch a text editor when the redirection ends.
  --error-prefix X  Use X as the left-margin prefix for error messages.
                    Set to an empty string to restore the default.
  --keep            Keep redirecting output to its current destination.
                    Use this option in combination with --show or
                    with --error-prefix when you do not want to stop
                    a current redirection.
  --plain           Use plain text rather than HTML tables with -w
  --show            Show output text captured by .testcase or by
                    redirecting to "memory".
  -w                Show the output in a web browser.  Output is
                    written into a temporary HTML file until the
                    redirect ends, then the web browser is launched.
                    Query results  are shown as HTML tables, unless
                    the --plain is used too.
  -x                Show the output in a spreadsheet.  Output is
                    written to a temp file as CSV then the spreadsheet
                    is launched when
"#;

/// `.help once`: the pin's usage text.
pub const USAGE_ONCE: &str = r#"USAGE: .once [OPTIONS] FILE ...

Write the output for the next line of SQL or the next dot-command into
FILE.  If FILE begins with "|" then it is a program into which output
is written. The FILE argument should be omitted if one of the -e, -w,
or -x options is used.

Options:
  -e                Capture output into a temporary file then bring up
                    a text editor on that temporary file.
  --plain           Use plain text rather than HTML tables with -w
  -w                Capture output into an HTML file then bring up that
                    file in a web browser
  -x                Show the output in a spreadsheet.  Output is
                    written to a temp file as CSV then the spreadsheet
                    is launched when
"#;

/// The extended usage text a command has instead of its entry.
fn usage_of(first_line: &str) -> Option<&'static str> {
    let name = first_line.split(' ').next().unwrap_or("");
    match name {
        ".mode" => Some(USAGE_MODE),
        ".output" => Some(USAGE_OUTPUT),
        ".once" => Some(USAGE_ONCE),
        _ => None,
    }
}

/// The entries: each a first line and its detail lines.
fn entries() -> Vec<Vec<&'static str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    for l in ENTRIES.lines() {
        if l.starts_with('.') || out.is_empty() {
            out.push(vec![l]);
        } else if let Some(e) = out.last_mut() {
            e.push(l);
        }
    }
    out
}

/// `.help [PATTERN]` as the pin answers it: every first line with no
/// pattern (or `-a`, `-all`, `--all`); the entries whose first line
/// globs `.PATTERN*` (case-sensitive), with the details (or the usage
/// text) when only one does; else every entry with a line LIKE
/// `%PATTERN%`; else `Nothing matches`.
pub fn help(pattern: Option<&str>) -> String {
    let all = entries();
    let mut out = String::new();
    let pat = match pattern {
        None | Some("-a" | "-all" | "--all") => {
            for e in &all {
                out.push_str(e[0]);
                out.push('\n');
            }
            return out;
        }
        Some(p) => p.strip_prefix('.').unwrap_or(p),
    };
    let glob_pat = format!(".{pat}*");
    let hits: Vec<&Vec<&str>> = all
        .iter()
        .filter(|e| glob(glob_pat.as_bytes(), e[0].as_bytes()))
        .collect();
    if hits.len() == 1 {
        let e = hits[0];
        if let Some(u) = usage_of(e[0]) {
            return u.to_string();
        }
        for l in e {
            out.push_str(l);
            out.push('\n');
        }
        return out;
    }
    if !hits.is_empty() {
        for e in hits {
            out.push_str(e[0]);
            out.push('\n');
        }
        return out;
    }
    let like_pat = format!("%{pat}%");
    for e in &all {
        if e.iter().any(|l| like(like_pat.as_bytes(), l.as_bytes())) {
            for l in e {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    if out.is_empty() {
        out = format!("Nothing matches '{pat}'\n");
    }
    out
}

/// The characters of `b` as SQLite's pattern matcher reads them: a
/// complete UTF-8 sequence as its code point, any other byte as itself.
fn chars(b: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let need = match c {
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf7 => 3,
            _ => 0,
        };
        let tail = b
            .get(i + 1..i + 1 + need)
            .filter(|t| t.iter().all(|x| x & 0xc0 == 0x80));
        match tail {
            Some(t) if need > 0 => {
                let mut cp = u32::from(c) & (0x3f >> need);
                for x in t {
                    cp = (cp << 6) | u32::from(x & 0x3f);
                }
                out.push(cp);
                i += need + 1;
            }
            _ => {
                out.push(u32::from(c));
                i += 1;
            }
        }
    }
    out
}

/// SQLite's GLOB (`*`, `?`, `[...]` with ranges and `^`), case
/// sensitive, one character a UTF-8 sequence.
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    glob_cp(&chars(p), &chars(s))
}

fn glob_cp(p: &[u32], s: &[u32]) -> bool {
    const STAR: u32 = b'*' as u32;
    const ANY: u32 = b'?' as u32;
    const OPEN: u32 = b'[' as u32;
    const CLOSE: u32 = b']' as u32;
    match p.first() {
        None => s.is_empty(),
        Some(&STAR) => (0..=s.len()).any(|i| glob_cp(&p[1..], &s[i..])),
        Some(&ANY) => !s.is_empty() && glob_cp(&p[1..], &s[1..]),
        Some(&OPEN) => {
            let Some(&c) = s.first() else {
                return false;
            };
            let mut i = 1;
            let neg = p.get(i) == Some(&u32::from(b'^'));
            if neg {
                i += 1;
            }
            let mut hit = false;
            let mut first = true;
            while i < p.len() && (first || p[i] != CLOSE) {
                first = false;
                if p.get(i + 1) == Some(&u32::from(b'-'))
                    && p.get(i + 2).is_some_and(|e| *e != CLOSE)
                {
                    if p[i] <= c && c <= p[i + 2] {
                        hit = true;
                    }
                    i += 3;
                } else {
                    if p[i] == c {
                        hit = true;
                    }
                    i += 1;
                }
            }
            if i >= p.len() {
                return false;
            }
            hit != neg && glob_cp(&p[i + 1..], &s[1..])
        }
        Some(&c) => s.first() == Some(&c) && glob_cp(&p[1..], &s[1..]),
    }
}

/// SQLite's LIKE (`%`, `_`), ASCII case-insensitive, no escape.
pub fn like(p: &[u8], s: &[u8]) -> bool {
    like_esc(p, s, None)
}

/// SQLite's LIKE with an `ESCAPE` character: `%` any run, `_` one
/// character (a UTF-8 sequence), ASCII letters matching either case,
/// the escape character making the next one literal (a pattern that
/// ends on it matches nothing).
pub fn like_esc(p: &[u8], s: &[u8], esc: Option<u8>) -> bool {
    like_cp(&chars(p), &chars(s), esc.map(u32::from))
}

fn like_cp(p: &[u32], s: &[u32], esc: Option<u32>) -> bool {
    let fold = |c: u32| {
        if (u32::from(b'A')..=u32::from(b'Z')).contains(&c) {
            c + 32
        } else {
            c
        }
    };
    let same = |a: u32, b: Option<&u32>| b.is_some_and(|&b| fold(a) == fold(b));
    match p.first() {
        None => s.is_empty(),
        Some(&c) if Some(c) == esc => match p.get(1) {
            None => false,
            Some(&l) => same(l, s.first()) && like_cp(&p[2..], &s[1..], esc),
        },
        Some(&c) if c == u32::from(b'%') => (0..=s.len()).any(|i| like_cp(&p[1..], &s[i..], esc)),
        Some(&c) if c == u32::from(b'_') => !s.is_empty() && like_cp(&p[1..], &s[1..], esc),
        Some(&c) => same(c, s.first()) && like_cp(&p[1..], &s[1..], esc),
    }
}
