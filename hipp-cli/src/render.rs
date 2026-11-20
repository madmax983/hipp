//! The pin shell's output modes (v0.17, `shell` unit): `.mode list`,
//! `csv`, `column`, `line`, `box`, `markdown`, `table`, `quote`,
//! `json`, `tabs`, `insert` and `ascii`, with `.headers`,
//! `.separator`, `.nullvalue` and `.width`, rendered byte for byte as
//! the pinned SQLite 3.54.1 shell renders them (its query result
//! formatter: probed on the live pin, see STATUS.md's `shell` notes).
//!
//! A result is rendered from owned values (`Val`) and the column
//! names (`names`), row by row for the streaming modes and all at once
//! for the columnar ones (`column`, `box`, `table`, `markdown`), which
//! size their columns from every row.

use std::io::Write;

/// One result value, owned (the statement's row is gone by the time a
/// columnar mode lays its rows out).
#[derive(Debug, Clone)]
pub enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Val {
    /// The owned copy of a statement's value.
    pub fn of(v: hipp::Value<'_>) -> Val {
        match v {
            hipp::Value::Null => Val::Null,
            hipp::Value::Integer(i) => Val::Int(i),
            hipp::Value::Real(f) => Val::Real(f),
            hipp::Value::Text(t) => Val::Text(t.to_vec()),
            hipp::Value::Blob(b) => Val::Blob(b.to_vec()),
        }
    }

    /// The borrowed form, for the `.dump` literal writer.
    pub fn as_value(&self) -> hipp::Value<'_> {
        match self {
            Val::Null => hipp::Value::Null,
            Val::Int(i) => hipp::Value::Integer(*i),
            Val::Real(f) => hipp::Value::Real(*f),
            Val::Text(t) => hipp::Value::Text(t),
            Val::Blob(b) => hipp::Value::Blob(b),
        }
    }

    fn is_num(&self) -> bool {
        matches!(self, Val::Int(_) | Val::Real(_))
    }
}

/// The output modes hipp renders (the pin's `.mode --list` has more:
/// `html`, `tcl`, `qbox`, `psql`, ... are refused by name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    List,
    Csv,
    Column,
    Line,
    Box,
    Markdown,
    Table,
    Quote,
    Json,
    Tabs,
    Insert,
    Ascii,
}

/// The pin's `.mode --list`, in its order.
pub const PIN_MODES: &[&str] = &[
    "ascii", "box", "c", "column", "count", "csv", "html", "insert", "jatom", "jobject", "json",
    "line", "list", "markdown", "off", "psql", "qbox", "quote", "split", "table", "tabs", "tcl",
    "batch", "tty",
];

impl Mode {
    /// The mode a `.mode` argument names (exact, case-sensitive, as
    /// on the pin), `None` for one hipp does not render.
    pub fn named(n: &str) -> Option<Mode> {
        Some(match n {
            "list" => Mode::List,
            "csv" => Mode::Csv,
            "column" => Mode::Column,
            "line" => Mode::Line,
            "box" => Mode::Box,
            "markdown" => Mode::Markdown,
            "table" => Mode::Table,
            "quote" => Mode::Quote,
            "json" => Mode::Json,
            "tabs" => Mode::Tabs,
            "insert" => Mode::Insert,
            "ascii" => Mode::Ascii,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::List => "list",
            Mode::Csv => "csv",
            Mode::Column => "column",
            Mode::Line => "line",
            Mode::Box => "box",
            Mode::Markdown => "markdown",
            Mode::Table => "table",
            Mode::Quote => "quote",
            Mode::Json => "json",
            Mode::Tabs => "tabs",
            Mode::Insert => "insert",
            Mode::Ascii => "ascii",
        }
    }

    /// The columnar modes, which show headers unless turned off.
    pub fn columnar(self) -> bool {
        matches!(
            self,
            Mode::Column | Mode::Box | Mode::Markdown | Mode::Table
        )
    }

    /// `(colsep, rowsep, nullvalue)` a `.mode` sets (the pin's
    /// `.show` after each `.mode`).
    fn defaults(self) -> (&'static [u8], &'static [u8], &'static [u8]) {
        match self {
            Mode::Csv => (b",", b"\r\n", b""),
            Mode::Tabs => (b"\t", b"\n", b""),
            Mode::Line => (b": ", b"\n", b""),
            Mode::Json => (b"|", b"\n", b"null"),
            Mode::Quote => (b",", b"\n", b"NULL"),
            Mode::Insert => (b"|", b"\n", b"NULL"),
            Mode::Ascii => (b"\x1f", b"\x1e", b""),
            _ => (b"|", b"\n", b""),
        }
    }
}

/// `.headers`: the pin's title setting. `Auto` (no `.headers` yet)
/// shows titles in the columnar modes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Titles {
    Auto,
    On,
    Off,
    Always,
}

/// The output settings `.mode`, `.headers`, `.separator`,
/// `.nullvalue` and `.width` change.
#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub titles: Titles,
    pub colsep: Vec<u8>,
    pub rowsep: Vec<u8>,
    pub null: Vec<u8>,
    pub widths: Vec<i64>,
    /// `.mode insert TABLE`'s table (`tab` when empty).
    pub table: Vec<u8>,
}

impl Default for Settings {
    fn default() -> Settings {
        let mut s = Settings {
            mode: Mode::List,
            titles: Titles::Auto,
            colsep: Vec::new(),
            rowsep: Vec::new(),
            null: Vec::new(),
            widths: Vec::new(),
            table: Vec::new(),
        };
        s.set_mode(Mode::List);
        s
    }
}

impl Settings {
    /// `.mode NAME`: the mode's separators and NULL text (titles,
    /// widths and the insert table stay).
    pub fn set_mode(&mut self, m: Mode) {
        let (c, r, n) = m.defaults();
        self.mode = m;
        self.colsep = c.to_vec();
        self.rowsep = r.to_vec();
        self.null = n.to_vec();
    }

    /// Whether a result shows its titles: `Some(always)`, where
    /// `always` also shows them over no rows.
    fn shows_titles(&self) -> Option<bool> {
        match self.titles {
            Titles::On => Some(false),
            Titles::Always => Some(true),
            Titles::Off => None,
            Titles::Auto => self.mode.columnar().then_some(true),
        }
    }

    /// Whether a result needs its column names: shown as titles, or
    /// written by the json and line modes.
    pub fn wants_names(&self) -> bool {
        self.shows_titles().is_some() || matches!(self.mode, Mode::Json | Mode::Line)
    }

    /// `.show`'s `headers:` word.
    pub fn titles_word(&self) -> &'static str {
        match self.titles {
            Titles::On => "on",
            Titles::Off => "off",
            Titles::Always => "always",
            Titles::Auto if self.mode.columnar() => "always",
            Titles::Auto => "off",
        }
    }

    /// `.mode` with no argument: the mode and the settings that differ
    /// from its defaults, as the pin prints them.
    pub fn describe(&self) -> Vec<u8> {
        let (c, r, n) = self.mode.defaults();
        let mut out = b".mode ".to_vec();
        out.extend_from_slice(self.mode.name().as_bytes());
        let opt = |out: &mut Vec<u8>, name: &str, v: &[u8]| {
            out.extend_from_slice(b" --");
            out.extend_from_slice(name.as_bytes());
            out.push(b' ');
            out.extend_from_slice(&c_string(v));
        };
        let seps = !matches!(self.mode, Mode::Json | Mode::Insert) && !self.mode.columnar();
        if seps && self.colsep != c {
            opt(&mut out, "colsep", &self.colsep);
        }
        if self.null != n {
            opt(&mut out, "null", &self.null);
        }
        if seps && self.rowsep != r {
            opt(&mut out, "rowsep", &self.rowsep);
        }
        if self.mode == Mode::Insert {
            opt(&mut out, "tablename", &self.table);
        }
        if !matches!(self.mode, Mode::Line | Mode::Json) {
            let def = if self.mode.columnar() {
                Titles::Always
            } else {
                Titles::Off
            };
            let t = if self.titles == Titles::Auto {
                def
            } else {
                self.titles
            };
            if t != def {
                out.extend_from_slice(b" --titles ");
                out.extend_from_slice(match t {
                    Titles::On => b"on".as_slice(),
                    Titles::Off => b"off",
                    _ => b"always",
                });
            }
        }
        if self.mode.columnar() && !self.widths.is_empty() {
            out.extend_from_slice(b" --widths ");
            let w: Vec<String> = self.widths.iter().map(i64::to_string).collect();
            out.extend_from_slice(w.join(",").as_bytes());
        }
        out
    }
}

/// A string as the pin's `.show` / `.mode` print one: in `"..."`, with
/// `\` and `"` escaped, `\t`, `\n`, `\r` named and other control bytes
/// in octal.
pub fn c_string(s: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        // (A whole UTF-8 character passes as is; a stray byte does not.)
        let (n, _) = next_char(s, i);
        if c >= 0x80 && n > 1 {
            out.extend_from_slice(&s[i..i + n]);
            i += n;
            continue;
        }
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            c if !(0x20..0x7f).contains(&c) => {
                out.extend_from_slice(format!("\\{c:03o}").as_bytes())
            }
            c => out.push(c),
        }
        i += 1;
    }
    out.push(b'"');
    out
}

/// The pin's display escape: a byte below 0x20 other than tab and
/// newline in caret notation (`^A`), everything else as is.
fn escape(b: &[u8]) -> Vec<u8> {
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

/// A text value as the pin reads it: up to its first NUL.
fn c_text(t: &[u8]) -> &[u8] {
    &t[..t.iter().position(|c| *c == 0).unwrap_or(t.len())]
}

/// A value as the display modes show it (list, csv, line, the
/// columnar ones): NULL as `.nullvalue`, a real as the core's
/// REAL-to-text, text and blob bytes escaped.
fn display(v: &Val, null: &[u8]) -> Vec<u8> {
    match v {
        Val::Null => null.to_vec(),
        Val::Int(i) => i.to_string().into_bytes(),
        Val::Real(f) => crate::fmt_real(*f).into_bytes(),
        Val::Text(t) => escape(c_text(t)),
        Val::Blob(b) => escape(b),
    }
}

/// Whether the csv / tabs modes quote a text: it is empty, holds a
/// blank or control byte, a quote of either kind, a byte past 0x7e, or
/// the column separator.
fn csv_needs_quote(t: &[u8], colsep: &[u8]) -> bool {
    t.is_empty()
        || t.iter()
            .any(|&c| c <= 0x20 || c == b'"' || c == b'\'' || c >= 0x7f)
        || (!colsep.is_empty() && t.windows(colsep.len()).any(|w| w == colsep))
}

fn csv_text(t: &[u8], colsep: &[u8]) -> Vec<u8> {
    let e = escape(t);
    if !csv_needs_quote(t, colsep) {
        return e;
    }
    let mut out = vec![b'"'];
    for c in e {
        if c == b'"' {
            out.push(b'"');
        }
        out.push(c);
    }
    out.push(b'"');
    out
}

/// A value as `.mode quote` writes it: SQL literals, except a real as
/// the display form and NULL as `.nullvalue`.
fn quote_value(v: &Val, null: &[u8]) -> Vec<u8> {
    match v {
        Val::Null => null.to_vec(),
        Val::Real(f) => crate::fmt_real(*f).into_bytes(),
        v => crate::dump_literal(v.as_value()),
    }
}

/// A JSON string literal as the pin's json mode writes one.
fn json_string(t: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &c in t {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

fn json_value(v: &Val, out: &mut Vec<u8>) {
    match v {
        Val::Null => out.extend_from_slice(b"null"),
        Val::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Val::Real(_) => out.extend_from_slice(&crate::dump_literal(v.as_value())),
        Val::Text(t) => json_string(c_text(t), out),
        Val::Blob(b) => {
            out.push(b'"');
            for c in b {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
            }
            out.push(b'"');
        }
    }
}

/// One result being written: the streaming modes write each row as it
/// comes, the columnar ones keep the rows until `finish`.
pub struct Emit<'s> {
    set: &'s Settings,
    names: Vec<Vec<u8>>,
    rows: Vec<Vec<Val>>,
    count: usize,
}

impl<'s> Emit<'s> {
    pub fn new(set: &'s Settings, names: Vec<Vec<u8>>) -> Emit<'s> {
        Emit {
            set,
            names,
            rows: Vec::new(),
            count: 0,
        }
    }

    /// Write one row (or keep it, for a columnar mode).
    pub fn row(&mut self, vals: Vec<Val>, out: &mut dyn Write) {
        let first = self.count == 0;
        self.count += 1;
        if self.set.mode.columnar() {
            self.rows.push(vals);
            return;
        }
        let mut b = Vec::new();
        if first {
            self.head(&mut b);
        }
        self.stream_row(&vals, first, &mut b);
        let _ = out.write_all(&b);
    }

    /// The end of the result: a columnar mode's whole table, a json
    /// mode's closing bracket, or the titles of an empty result.
    pub fn finish(self, out: &mut dyn Write) {
        let mut b = Vec::new();
        if self.set.mode.columnar() {
            columnar(self.set, &self.names, &self.rows, &mut b);
        } else if self.count == 0 {
            if self.set.shows_titles() == Some(true) {
                self.head(&mut b);
            }
        } else if self.set.mode == Mode::Json {
            b.extend_from_slice(b"]\n");
        }
        let _ = out.write_all(&b);
    }

    /// The title line of a streaming mode.
    fn head(&self, b: &mut Vec<u8>) {
        if self.set.shows_titles().is_none() {
            return;
        }
        let s = self.set;
        match s.mode {
            Mode::List | Mode::Ascii | Mode::Csv | Mode::Tabs | Mode::Quote => {
                for (i, n) in self.names.iter().enumerate() {
                    if i > 0 {
                        b.extend_from_slice(&s.colsep);
                    }
                    match s.mode {
                        Mode::Csv | Mode::Tabs => b.extend_from_slice(&csv_text(n, &s.colsep)),
                        Mode::Quote => {
                            b.extend_from_slice(&crate::dump_literal(hipp::Value::Text(n)))
                        }
                        _ => b.extend_from_slice(&escape(n)),
                    }
                }
                b.extend_from_slice(&s.rowsep);
            }
            _ => {}
        }
    }

    fn stream_row(&self, vals: &[Val], first: bool, b: &mut Vec<u8>) {
        let s = self.set;
        match s.mode {
            Mode::List | Mode::Ascii | Mode::Csv | Mode::Tabs | Mode::Quote => {
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        b.extend_from_slice(&s.colsep);
                    }
                    match (s.mode, v) {
                        (Mode::Csv | Mode::Tabs, Val::Text(t)) => {
                            b.extend_from_slice(&csv_text(c_text(t), &s.colsep))
                        }
                        (Mode::Quote, v) => b.extend_from_slice(&quote_value(v, &s.null)),
                        (_, v) => b.extend_from_slice(&display(v, &s.null)),
                    }
                }
                b.extend_from_slice(&s.rowsep);
            }
            Mode::Insert => {
                b.extend_from_slice(b"INSERT INTO ");
                if s.table.is_empty() {
                    b.extend_from_slice(b"tab");
                } else {
                    b.extend_from_slice(&crate::dump_name(&s.table));
                }
                if s.shows_titles().is_some() {
                    b.push(b'(');
                    for (i, n) in self.names.iter().enumerate() {
                        if i > 0 {
                            b.push(b',');
                        }
                        b.extend_from_slice(&crate::dump_name(n));
                    }
                    b.push(b')');
                }
                b.extend_from_slice(b" VALUES(");
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        b.push(b',');
                    }
                    b.extend_from_slice(&crate::dump_literal(v.as_value()));
                }
                b.extend_from_slice(b");\n");
            }
            Mode::Json => {
                b.extend_from_slice(if first { b"[{" } else { b",\n{" });
                for (i, (n, v)) in self.names.iter().zip(vals).enumerate() {
                    if i > 0 {
                        b.push(b',');
                    }
                    json_string(n, b);
                    b.push(b':');
                    json_value(v, b);
                }
                b.push(b'}');
            }
            Mode::Line => {
                let w = self
                    .names
                    .iter()
                    .map(|n| width(&escape(n)))
                    .max()
                    .unwrap_or(0);
                if !first {
                    b.push(b'\n');
                }
                let indent = w + width(&s.colsep);
                for (n, v) in self.names.iter().zip(vals) {
                    let n = escape(n);
                    b.resize(b.len() + w.saturating_sub(width(&n)), b' ');
                    b.extend_from_slice(&n);
                    b.extend_from_slice(&s.colsep);
                    let d = display(v, &s.null);
                    for (i, l) in lines(&d).iter().enumerate() {
                        if i > 0 {
                            b.resize(b.len() + indent, b' ');
                        }
                        b.extend_from_slice(l);
                        b.push(b'\n');
                    }
                }
            }
            _ => {}
        }
    }
}

/// A cell's lines: split at each newline, a final newline ending the
/// last line rather than starting an empty one.
fn lines(d: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = d.split(|c| *c == b'\n').collect();
    if v.len() > 1 && v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// The next character of `b` at `i`: its byte length and display
/// width. A complete UTF-8 sequence is one character, sized by the
/// pin's width table; any other byte is a character of width 1.
fn next_char(b: &[u8], i: usize) -> (usize, usize) {
    let c = b[i];
    let need = match c {
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        _ => return (1, 1),
    };
    let tail = b.get(i + 1..i + 1 + need);
    let Some(tail) = tail.filter(|t| t.iter().all(|x| x & 0xc0 == 0x80)) else {
        return (1, 1);
    };
    let mut cp = u32::from(c) & (0x3f >> need);
    for x in tail {
        cp = (cp << 6) | u32::from(x & 0x3f);
    }
    (need + 1, char_width(cp))
}

/// The display width of a code point (the pin's table: Markus Kuhn's
/// zero-width combining marks and East Asian wide ranges).
fn char_width(cp: u32) -> usize {
    let within = |t: &[(u32, u32)]| {
        t.binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
    };
    if cp < 0x300 {
        1
    } else if within(&ZERO_WIDTH) {
        0
    } else if within(&WIDE) {
        2
    } else {
        1
    }
}

/// The display width of a line.
pub fn width(b: &[u8]) -> usize {
    let mut i = 0;
    let mut w = 0;
    while i < b.len() {
        let (n, cw) = next_char(b, i);
        i += n;
        w += cw;
    }
    w
}

/// A line with each tab expanded to the next multiple of 8 columns.
fn expand_tabs(l: &[u8]) -> Vec<u8> {
    if !l.contains(&b'\t') {
        return l.to_vec();
    }
    let mut out = Vec::new();
    let mut col = 0;
    let mut i = 0;
    while i < l.len() {
        if l[i] == b'\t' {
            let n = 8 - col % 8;
            out.resize(out.len() + n, b' ');
            col += n;
            i += 1;
            continue;
        }
        let (n, cw) = next_char(l, i);
        out.extend_from_slice(&l[i..i + n]);
        col += cw;
        i += n;
    }
    out
}

/// A line wrapped to `w` columns as the pin wraps a fixed-width
/// column (probed on the live pin; `h` is half the bytes that fit).
/// Each line takes as much as fits (a tab counting to the next
/// multiple of 8).
/// Unless a blank follows, the line then ends earlier: before the last
/// blank at least `h` bytes in; else at the last change between word
/// characters (ASCII letters, digits, `_`) and other non-blank ones, or
/// word start after a blank, at least `h` bytes in (the cut itself
/// included); else at the cut. Blanks at the start of the next line are
/// dropped; tabs are expanded within each line.
fn wrap(l: &[u8], w: usize, out: &mut Vec<Vec<u8>>) {
    let w = w.max(1);
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let blank = |c: u8| c == b' ' || c == b'\t';
    let mut i = 0;
    loop {
        if i >= l.len() {
            return;
        }
        let mut j = i;
        let mut cols = 0;
        while j < l.len() {
            let (n, cw) = if l[j] == b'\t' {
                (1, 8 - cols % 8)
            } else {
                next_char(l, j)
            };
            if cols + cw > w {
                break;
            }
            cols += cw;
            j += n;
        }
        if j == i {
            if blank(l[i]) {
                // A tab wider than the column: an empty line.
                out.push(Vec::new());
                while i < l.len() && blank(l[i]) {
                    i += 1;
                }
                continue;
            }
            // A character wider than the column: alone on its line.
            j += next_char(l, i).0;
        }
        if j >= l.len() {
            out.push(expand_tabs(&l[i..]));
            return;
        }
        let thr = (j - i) / 2;
        // A change between word and other non-blank characters, or a
        // word's start after a blank.
        let change = |p: usize| {
            let (x, y) = (l[p - 1], l[p]);
            if blank(x) {
                word(y)
            } else {
                !blank(y) && word(x) != word(y)
            }
        };
        let end = if blank(l[j]) {
            j
        } else if let Some(k) = (i..j).rev().find(|&k| blank(l[k]) && k - i >= thr) {
            k
        } else if let Some(p) = (i + 1..=j).rev().find(|&p| change(p) && p - i >= thr) {
            p
        } else {
            j
        };
        out.push(expand_tabs(&l[i..end]));
        i = end;
        while i < l.len() && blank(l[i]) {
            i += 1;
        }
    }
}

/// A cell before layout: its lines (escaped, tabs expanded), whether
/// it aligns right, and whether its text held a newline (even a final
/// one, which makes no line of its own: the pin then rules off every
/// row).
struct Cell {
    lines: Vec<Vec<u8>>,
    right: bool,
    newline: bool,
}

impl Cell {
    fn new(d: &[u8], right: bool) -> Cell {
        Cell {
            lines: lines(d).into_iter().map(<[u8]>::to_vec).collect(),
            right,
            newline: d.contains(&b'\n'),
        }
    }

    fn width(&self) -> usize {
        self.lines
            .iter()
            .map(|l| width(&expand_tabs(l)))
            .max()
            .unwrap_or(0)
    }

    /// The lines wrapped to a fixed width.
    fn wrap(&mut self, w: usize) {
        let mut out = Vec::new();
        for l in &self.lines {
            if width(&expand_tabs(l)) > w {
                let before = out.len();
                wrap(l, w, &mut out);
                if out.len() == before {
                    out.push(Vec::new());
                }
            } else {
                out.push(expand_tabs(l));
            }
        }
        self.lines = out;
    }
}

/// `text` padded to `w` columns: left, right or centered.
fn pad(text: &[u8], w: usize, how: u8, b: &mut Vec<u8>) {
    let tw = width(text);
    let room = w.saturating_sub(tw);
    let (l, r) = match how {
        b'R' => (room, 0),
        b'C' => (room / 2, room - room / 2),
        _ => (0, room),
    };
    b.resize(b.len() + l, b' ');
    b.extend_from_slice(text);
    b.resize(b.len() + r, b' ');
}

/// The columnar modes: every row laid out against the widest cell of
/// its column (or the `.width` given).
fn columnar(set: &Settings, names: &[Vec<u8>], rows: &[Vec<Val>], b: &mut Vec<u8>) {
    let titles = set.shows_titles();
    if rows.is_empty() && titles != Some(true) {
        return;
    }
    let ncol = names.len();
    let fixed = |i: usize| -> (usize, bool) {
        let w = set.widths.get(i).copied().unwrap_or(0);
        (
            usize::try_from(w.unsigned_abs()).unwrap_or(usize::MAX),
            w < 0,
        )
    };
    let mut head: Vec<Cell> = (0..ncol)
        .map(|i| Cell::new(&escape(&names[i]), false))
        .collect();
    let mut body: Vec<Vec<Cell>> = rows
        .iter()
        .map(|r| {
            (0..ncol)
                .map(|i| {
                    let v = r.get(i).unwrap_or(&Val::Null);
                    Cell::new(&display(v, &set.null), fixed(i).1 || v.is_num())
                })
                .collect()
        })
        .collect();
    // Every row is ruled off when a cell holds a newline, or is wider
    // than its column's `.width` (and so wraps; a width of 1 that a
    // cell overflows becomes 2).
    let mut multi = body.iter().any(|r| r.iter().any(|c| c.newline))
        || (titles.is_some() && head.iter().any(|c| c.newline));
    let mut widths = Vec::with_capacity(ncol);
    for i in 0..ncol {
        let (fw, _) = fixed(i);
        let mut w = if titles.is_some() { head[i].width() } else { 0 };
        for r in &body {
            w = w.max(r[i].width());
        }
        if fw == 0 {
            // (Wrapping to no limit only expands the tabs.)
            head[i].wrap(usize::MAX);
            for r in body.iter_mut() {
                r[i].wrap(usize::MAX);
            }
            widths.push(w);
            continue;
        }
        let over = w > fw;
        multi |= over;
        let fw = if fw < 2 && w > fw { 2 } else { fw };
        head[i].wrap(fw);
        for r in body.iter_mut() {
            r[i].wrap(fw);
        }
        widths.push(fw);
    }
    let mode = set.mode;
    // (left edge, between cells, right edge) of a text line, and the
    // rule lines' (left, fill, joint, right).
    let (le, mid, re): (&[u8], &[u8], &[u8]) = match mode {
        Mode::Box => ("│ ".as_bytes(), " │ ".as_bytes(), " │".as_bytes()),
        Mode::Table | Mode::Markdown => (b"| ", b" | ", b" |"),
        _ => (b"", b"  ", b""),
    };
    let rule = |b: &mut Vec<u8>, l: &str, f: &str, j: &str, r: &str| {
        b.extend_from_slice(l.as_bytes());
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                b.extend_from_slice(j.as_bytes());
            }
            for _ in 0..w + 2 {
                b.extend_from_slice(f.as_bytes());
            }
        }
        b.extend_from_slice(r.as_bytes());
        b.push(b'\n');
    };
    let text_rows = |b: &mut Vec<u8>, cells: &[Cell], center: bool| {
        let h = cells
            .iter()
            .map(|c| c.lines.len())
            .max()
            .unwrap_or(1)
            .max(1);
        for k in 0..h {
            let start = b.len();
            b.extend_from_slice(le);
            for (i, c) in cells.iter().enumerate() {
                if i > 0 {
                    b.extend_from_slice(mid);
                }
                let t = c.lines.get(k).map_or(&[][..], Vec::as_slice);
                let how = if center {
                    b'C'
                } else if c.right {
                    b'R'
                } else {
                    b'L'
                };
                pad(t, widths[i], how, b);
            }
            b.extend_from_slice(re);
            if mode == Mode::Column {
                while b.len() > start && b.last() == Some(&b' ') {
                    b.pop();
                }
            }
            b.push(b'\n');
        }
    };
    match mode {
        Mode::Box => rule(b, "╭", "─", "┬", "╮"),
        Mode::Table => rule(b, "+", "-", "+", "+"),
        _ => {}
    }
    if titles.is_some() {
        text_rows(b, &head, true);
        match mode {
            Mode::Box if rows.is_empty() => rule(b, "╘", "═", "╧", "╛"),
            Mode::Box => rule(b, "╞", "═", "╪", "╡"),
            Mode::Table => rule(b, "+", "-", "+", "+"),
            Mode::Markdown => rule(b, "|", "-", "|", "|"),
            _ => {
                let start = b.len();
                for (i, w) in widths.iter().enumerate() {
                    if i > 0 {
                        b.extend_from_slice(b"  ");
                    }
                    b.resize(b.len() + w, b'-');
                }
                while b.len() > start && b.last() == Some(&b' ') {
                    b.pop();
                }
                b.push(b'\n');
            }
        }
    }
    for (n, r) in body.iter().enumerate() {
        if n > 0 && multi {
            match mode {
                Mode::Box => rule(b, "├", "─", "┼", "┤"),
                Mode::Table => rule(b, "+", "-", "+", "+"),
                Mode::Column => b.push(b'\n'),
                _ => {}
            }
        }
        text_rows(b, r, false);
    }
    if !rows.is_empty() || titles.is_none() {
        match mode {
            Mode::Box => rule(b, "╰", "─", "┴", "╯"),
            Mode::Table => rule(b, "+", "-", "+", "+"),
            _ => {}
        }
    } else if mode == Mode::Table {
        // (An empty table's title rule is also its bottom.)
    }
}

/// `.tables` / `.indexes`: names in columns down then across, as many
/// columns as fit 80 with two blanks between them, the gap widened up
/// to five when there is room; a name wider than 80 wraps, one per
/// line with a blank line between.
pub fn grid(names: &[Vec<u8>], b: &mut Vec<u8>) {
    if names.is_empty() {
        return;
    }
    let ws: Vec<usize> = names.iter().map(|n| width(n)).collect();
    if ws.iter().any(|w| *w > 80) {
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                b.push(b'\n');
            }
            let mut out = Vec::new();
            hard_wrap(n, 80, &mut out);
            for l in out {
                b.extend_from_slice(&l);
                b.push(b'\n');
            }
        }
        return;
    }
    let n = names.len();
    for nrow in 1..=n {
        let ncol = n.div_ceil(nrow);
        let cw: Vec<usize> = (0..ncol)
            .map(|c| {
                (c * nrow..((c + 1) * nrow).min(n))
                    .map(|k| ws[k])
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let sum: usize = cw.iter().sum();
        if ncol > 1 && sum + 2 * (ncol - 1) > 80 {
            continue;
        }
        let gap = if ncol > 1 {
            ((80 - sum) / (ncol - 1)).min(5)
        } else {
            0
        };
        for r in 0..nrow {
            let start = b.len();
            for (c, w) in cw.iter().enumerate() {
                let k = c * nrow + r;
                if k >= n {
                    break;
                }
                if c > 0 {
                    b.resize(b.len() + gap, b' ');
                }
                pad(&names[k], *w, b'L', b);
            }
            while b.len() > start && b.last() == Some(&b' ') {
                b.pop();
            }
            b.push(b'\n');
        }
        return;
    }
}

/// Cut a name into lines of at most `w` columns.
fn hard_wrap(l: &[u8], w: usize, out: &mut Vec<Vec<u8>>) {
    let mut i = 0;
    while i < l.len() {
        let mut j = i;
        let mut cols = 0;
        while j < l.len() {
            let (n, cw) = next_char(l, j);
            if cols + cw > w && j > i {
                break;
            }
            cols += cw;
            j += n;
        }
        out.push(l[i..j].to_vec());
        i = j;
    }
}

/// Zero-width code points of the pin's width table (Markus Kuhn's
/// combining characters), measured on the live pin one code point at a
/// time.
const ZERO_WIDTH: [(u32, u32); 141] = [
    (0x300, 0x36f),
    (0x483, 0x486),
    (0x488, 0x489),
    (0x591, 0x5bd),
    (0x5bf, 0x5bf),
    (0x5c1, 0x5c2),
    (0x5c4, 0x5c5),
    (0x5c7, 0x5c7),
    (0x600, 0x603),
    (0x610, 0x615),
    (0x64b, 0x65e),
    (0x670, 0x670),
    (0x6d6, 0x6e4),
    (0x6e7, 0x6e8),
    (0x6ea, 0x6ed),
    (0x70f, 0x70f),
    (0x711, 0x711),
    (0x730, 0x74a),
    (0x7a6, 0x7b0),
    (0x7eb, 0x7f3),
    (0x901, 0x902),
    (0x93c, 0x93c),
    (0x941, 0x948),
    (0x94d, 0x94d),
    (0x951, 0x954),
    (0x962, 0x963),
    (0x981, 0x981),
    (0x9bc, 0x9bc),
    (0x9c1, 0x9c4),
    (0x9cd, 0x9cd),
    (0x9e2, 0x9e3),
    (0xa01, 0xa02),
    (0xa3c, 0xa3c),
    (0xa41, 0xa42),
    (0xa47, 0xa48),
    (0xa4b, 0xa4d),
    (0xa70, 0xa71),
    (0xa81, 0xa82),
    (0xabc, 0xabc),
    (0xac1, 0xac5),
    (0xac7, 0xac8),
    (0xacd, 0xacd),
    (0xae2, 0xae3),
    (0xb01, 0xb01),
    (0xb3c, 0xb3c),
    (0xb3f, 0xb3f),
    (0xb41, 0xb43),
    (0xb4d, 0xb4d),
    (0xb56, 0xb56),
    (0xb82, 0xb82),
    (0xbc0, 0xbc0),
    (0xbcd, 0xbcd),
    (0xc3e, 0xc40),
    (0xc46, 0xc48),
    (0xc4a, 0xc4d),
    (0xc55, 0xc56),
    (0xcbc, 0xcbc),
    (0xcbf, 0xcbf),
    (0xcc6, 0xcc6),
    (0xccc, 0xccd),
    (0xce2, 0xce3),
    (0xd41, 0xd43),
    (0xd4d, 0xd4d),
    (0xdca, 0xdca),
    (0xdd2, 0xdd4),
    (0xdd6, 0xdd6),
    (0xe31, 0xe31),
    (0xe34, 0xe3a),
    (0xe47, 0xe4e),
    (0xeb1, 0xeb1),
    (0xeb4, 0xeb9),
    (0xebb, 0xebc),
    (0xec8, 0xecd),
    (0xf18, 0xf19),
    (0xf35, 0xf35),
    (0xf37, 0xf37),
    (0xf39, 0xf39),
    (0xf71, 0xf7e),
    (0xf80, 0xf84),
    (0xf86, 0xf87),
    (0xf90, 0xf97),
    (0xf99, 0xfbc),
    (0xfc6, 0xfc6),
    (0x102d, 0x1030),
    (0x1032, 0x1032),
    (0x1036, 0x103a),
    (0x1058, 0x1059),
    (0x1160, 0x11ff),
    (0x135f, 0x135f),
    (0x1712, 0x1714),
    (0x1732, 0x1734),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17b4, 0x17b5),
    (0x17b7, 0x17bd),
    (0x17c6, 0x17c6),
    (0x17c9, 0x17d3),
    (0x17dd, 0x17dd),
    (0x180b, 0x180d),
    (0x18a9, 0x18a9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193b),
    (0x1a17, 0x1a18),
    (0x1b00, 0x1b03),
    (0x1b34, 0x1b34),
    (0x1b36, 0x1b3a),
    (0x1b3c, 0x1b3c),
    (0x1b42, 0x1b42),
    (0x1b6b, 0x1b73),
    (0x1dc0, 0x1dca),
    (0x1dfe, 0x1dff),
    (0x200b, 0x200f),
    (0x202a, 0x202e),
    (0x2060, 0x2063),
    (0x206a, 0x206f),
    (0x20d0, 0x20ef),
    (0x302a, 0x302f),
    (0x3099, 0x309a),
    (0xa806, 0xa806),
    (0xa80b, 0xa80b),
    (0xa825, 0xa826),
    (0xfb1e, 0xfb1e),
    (0xfe00, 0xfe0f),
    (0xfe20, 0xfe23),
    (0xfeff, 0xfeff),
    (0xfff9, 0xfffb),
    (0x10a01, 0x10a03),
    (0x10a05, 0x10a06),
    (0x10a0c, 0x10a0f),
    (0x10a38, 0x10a3a),
    (0x10a3f, 0x10a3f),
    (0x1d167, 0x1d169),
    (0x1d173, 0x1d182),
    (0x1d185, 0x1d18b),
    (0x1d1aa, 0x1d1ad),
    (0x1d242, 0x1d244),
    (0xe0001, 0xe0001),
    (0xe0020, 0xe007f),
    (0xe0100, 0xe01ef),
];

/// Double-width code points of the pin's width table (East Asian
/// wide), measured on the live pin.
const WIDE: [(u32, u32); 14] = [
    (0x1100, 0x115f),
    (0x2329, 0x232a),
    (0x2e80, 0x3029),
    (0x3030, 0x303e),
    (0x3040, 0x3098),
    (0x309b, 0xa4cf),
    (0xac00, 0xd7a3),
    (0xf900, 0xfaff),
    (0xfe10, 0xfe19),
    (0xfe30, 0xfe6f),
    (0xff00, 0xff60),
    (0xffe0, 0xffe6),
    (0x20000, 0x2fffd),
    (0x30000, 0x3fffd),
];
