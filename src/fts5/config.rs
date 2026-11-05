//! `CREATE VIRTUAL TABLE ... USING fts5(...)` arguments as the pin's
//! `sqlite3Fts5ConfigParse` reads them, and the runtime settings of
//! `%_config` (`sqlite3Fts5ConfigSetValue`).

use super::tok::{TokErr, Tokenizer};
use super::{Detail, Tuning};
use crate::arena::AVec;
use crate::record::OwnedValue;

/// Where the table's content lives (`content=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// `%_content`.
    Normal,
    /// `content=''`: none.
    None,
    /// `content=<table>`.
    External,
}

/// A parsed fts5 table definition.
#[derive(Debug, Clone)]
pub struct Config {
    pub cols: AVec<AVec<u8>>,
    pub unindexed: AVec<bool>,
    pub prefix: AVec<u16>,
    /// The `tokenize=` words (empty: unicode61).
    pub tok_args: AVec<AVec<u8>>,
    pub tok: Tokenizer,
    pub content: Content,
    pub content_tab: AVec<u8>,
    pub content_rowid: AVec<u8>,
    pub columnsize: bool,
    pub detail: Detail,
    pub contentless_delete: bool,
}

/// Why a definition is refused (the pin's messages, string-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfgErr {
    /// "parse error in ...", "unrecognized option", a malformed
    /// directive, "multiple ... directives".
    Syntax,
    /// "reserved fts5 column name" / "reserved fts5 table name".
    Reserved,
    /// "unrecognized column option".
    ColumnOption,
    /// "no such tokenizer".
    NoTokenizer,
    /// "error in tokenizer constructor".
    Tokenizer,
    /// "prefix length out of range", "too many prefix indexes".
    Prefix,
    /// "contentless_delete=1 requires a contentless table", "... is
    /// incompatible with columnsize=0".
    ContentlessDelete,
    /// No columns, or a column named twice ("vtable constructor
    /// failed").
    Columns,
}

/// `fts5_isBareword`.
pub fn is_bareword(c: u8) -> bool {
    c & 0x80 != 0 || c.is_ascii_alphanumeric() || c == b'_' || c == 0x1A
}

fn skip_ws(z: &[u8], mut i: usize) -> usize {
    while matches!(z.get(i), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)) {
        i += 1;
    }
    i
}

/// `sqlite3Fts5Dequote` of the quoted word at `z[0]`: its text and
/// the bytes it spans.
pub fn dequote(z: &[u8]) -> (AVec<u8>, usize) {
    let mut out = AVec::new();
    let q = match z.first() {
        Some(b'[') => b']',
        Some(c @ (b'\'' | b'"' | b'`')) => *c,
        _ => return (out, 0),
    };
    let mut i = 1;
    while let Some(&c) = z.get(i) {
        if c == q {
            if z.get(i + 1) == Some(&q) && q != b']' {
                out.push(q);
                i += 2;
                continue;
            }
            i += 1;
            return (out, i);
        }
        out.push(c);
        i += 1;
    }
    (out, i)
}

/// `fts5ConfigGobbleWord`: the word at `z[i..]` (quoted or bare),
/// whether it was quoted, and where it ends.
fn gobble(z: &[u8], i: usize) -> Option<(AVec<u8>, bool, usize)> {
    if matches!(z.get(i), Some(b'\'' | b'"' | b'`' | b'[')) {
        let (w, n) = dequote(&z[i..]);
        return Some((w, true, i + n));
    }
    let mut j = i;
    while z.get(j).is_some_and(|c| is_bareword(*c)) {
        j += 1;
    }
    if j == i {
        return None;
    }
    let mut w = AVec::new();
    w.extend_from_slice(&z[i..j]);
    Some((w, false, j))
}

/// The `tokenize=` value as words: barewords and '...' literals.
fn tok_words(v: &[u8]) -> Option<AVec<AVec<u8>>> {
    let mut out = AVec::new();
    let mut i = 0;
    loop {
        i = skip_ws(v, i);
        if i >= v.len() {
            break;
        }
        if v[i] == b'\'' {
            let (w, n) = dequote(&v[i..]);
            if i + n > v.len() || v.get(i + n - 1) != Some(&b'\'') {
                return None;
            }
            out.push(w);
            i += n;
        } else {
            let mut j = i;
            while v.get(j).is_some_and(|c| is_bareword(*c)) {
                j += 1;
            }
            if j == i {
                return None;
            }
            let mut w = AVec::new();
            w.extend_from_slice(&v[i..j]);
            out.push(w);
            i = j;
        }
    }
    Some(out)
}

/// `sqlite3_strnicmp(name, cmd, strlen(cmd)) == 0`: `cmd` is a
/// prefix of `name`, any case.
fn abbrev(name: &[u8], cmd: &[u8]) -> bool {
    cmd.len() <= name.len() && name[..cmd.len()].eq_ignore_ascii_case(cmd)
}

impl Config {
    /// Parse table `name`'s module arguments.
    pub fn parse(name: &[u8], args: &[&[u8]]) -> Result<Config, CfgErr> {
        if name.eq_ignore_ascii_case(b"rank") {
            return Err(CfgErr::Reserved);
        }
        let mut c = Config {
            cols: AVec::new(),
            unindexed: AVec::new(),
            prefix: AVec::new(),
            tok_args: AVec::new(),
            tok: Tokenizer::create(&[]).map_err(|_| CfgErr::Tokenizer)?,
            content: Content::Normal,
            content_tab: AVec::new(),
            content_rowid: AVec::new(),
            columnsize: true,
            detail: Detail::Full,
            contentless_delete: false,
        };
        let mut have_tok = false;
        let mut have_rowid = false;
        for a in args.iter() {
            let (one, quoted, mut i) = gobble(a, 0).ok_or(CfgErr::Syntax)?;
            i = skip_ws(a, i);
            let option = a.get(i) == Some(&b'=');
            if option {
                if quoted {
                    return Err(CfgErr::Syntax);
                }
                i += 1;
            }
            i = skip_ws(a, i);
            let mut two = None;
            if i < a.len() {
                let (w, _, j) = gobble(a, i).ok_or(CfgErr::Syntax)?;
                if j < a.len() {
                    return Err(CfgErr::Syntax);
                }
                two = Some(w);
            }
            if option {
                let v = two.unwrap_or_default();
                c.special(one.as_slice(), v.as_slice(), &mut have_tok, &mut have_rowid)?;
            } else {
                if one.as_slice().eq_ignore_ascii_case(b"rank")
                    || one.as_slice().eq_ignore_ascii_case(b"rowid")
                {
                    return Err(CfgErr::Reserved);
                }
                let un = match two {
                    None => false,
                    Some(w) if w.as_slice().eq_ignore_ascii_case(b"unindexed") => true,
                    Some(_) => return Err(CfgErr::ColumnOption),
                };
                c.cols.push(one);
                c.unindexed.push(un);
            }
        }
        if c.contentless_delete && c.content != Content::None {
            return Err(CfgErr::ContentlessDelete);
        }
        if c.contentless_delete && !c.columnsize {
            return Err(CfgErr::ContentlessDelete);
        }
        if c.cols.is_empty() {
            return Err(CfgErr::Columns);
        }
        for (i, x) in c.cols.iter().enumerate() {
            let dup = c.cols.as_slice()[..i]
                .iter()
                .any(|y| y.as_slice().eq_ignore_ascii_case(x.as_slice()));
            if dup || x.as_slice().eq_ignore_ascii_case(name) {
                return Err(CfgErr::Columns);
            }
        }
        if !have_rowid {
            c.content_rowid.extend_from_slice(b"rowid");
        }
        Ok(c)
    }

    /// `fts5ConfigParseSpecial`.
    fn special(
        &mut self,
        cmd: &[u8],
        v: &[u8],
        have_tok: &mut bool,
        have_rowid: &mut bool,
    ) -> Result<(), CfgErr> {
        let bit = |v: &[u8]| match v {
            b"0" => Ok(false),
            b"1" => Ok(true),
            _ => Err(CfgErr::Syntax),
        };
        if abbrev(b"prefix", cmd) {
            let mut i = 0;
            let mut first = true;
            loop {
                while v.get(i) == Some(&b' ') {
                    i += 1;
                }
                if !first && v.get(i) == Some(&b',') {
                    i += 1;
                    while v.get(i) == Some(&b' ') {
                        i += 1;
                    }
                } else if i >= v.len() {
                    break;
                }
                if !v.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    return Err(CfgErr::Syntax);
                }
                if self.prefix.len() == 31 {
                    return Err(CfgErr::Prefix);
                }
                let mut n = 0u32;
                while let Some(d) = v.get(i).filter(|c| c.is_ascii_digit())
                    && n < 1000
                {
                    n = n * 10 + u32::from(d - b'0');
                    i += 1;
                }
                if n == 0 || n >= 1000 {
                    return Err(CfgErr::Prefix);
                }
                self.prefix.push(n as u16);
                first = false;
            }
            Ok(())
        } else if abbrev(b"tokenize", cmd) {
            if *have_tok {
                return Err(CfgErr::Syntax);
            }
            let words = tok_words(v).ok_or(CfgErr::Syntax)?;
            self.tok = Tokenizer::create(words.as_slice()).map_err(|e| match e {
                TokErr::NoSuch => CfgErr::NoTokenizer,
                TokErr::Args => CfgErr::Tokenizer,
            })?;
            self.tok_args = words;
            *have_tok = true;
            Ok(())
        } else if abbrev(b"content", cmd) {
            if self.content != Content::Normal {
                return Err(CfgErr::Syntax);
            }
            if v.is_empty() {
                self.content = Content::None;
            } else {
                self.content = Content::External;
                self.content_tab.extend_from_slice(v);
            }
            Ok(())
        } else if abbrev(b"contentless_delete", cmd) {
            self.contentless_delete = bit(v)?;
            Ok(())
        } else if abbrev(b"contentless_unindexed", cmd) {
            // Accepted only in its default form.
            if bit(v)? {
                return Err(CfgErr::Syntax);
            }
            Ok(())
        } else if abbrev(b"content_rowid", cmd) {
            if *have_rowid {
                return Err(CfgErr::Syntax);
            }
            self.content_rowid.extend_from_slice(v);
            *have_rowid = true;
            Ok(())
        } else if abbrev(b"columnsize", cmd) {
            self.columnsize = bit(v)?;
            Ok(())
        } else if abbrev(b"locale", cmd) {
            if bit(v)? {
                return Err(CfgErr::Syntax);
            }
            Ok(())
        } else if abbrev(b"detail", cmd) {
            let mut hit = None;
            for (n, d) in [
                (b"none" as &[u8], Detail::None),
                (b"full", Detail::Full),
                (b"columns", Detail::Column),
            ] {
                if abbrev(n, v) {
                    if hit.is_some() {
                        return Err(CfgErr::Syntax);
                    }
                    hit = Some(d);
                }
            }
            self.detail = hit.ok_or(CfgErr::Syntax)?;
            Ok(())
        } else if abbrev(b"tokendata", cmd) {
            if bit(v)? {
                return Err(CfgErr::Syntax);
            }
            Ok(())
        } else {
            Err(CfgErr::Syntax)
        }
    }

    /// Whether the table has a `%_content` table.
    pub fn has_content(&self) -> bool {
        self.content == Content::Normal
    }
}

/// The runtime settings read from `%_config` (`Fts5Config`'s pgsz,
/// automerge, ..., rank).
#[derive(Debug, Clone, Default)]
pub struct Runtime {
    pub t: Tuning,
    pub version: i64,
    /// `rank` (`name(args)`), empty for the default `bm25()`.
    pub rank: AVec<u8>,
    /// `secure-delete` (`bSecureDelete`): hipp cannot remove index
    /// entries in place, so deletes are refused while it is set.
    pub secure_delete: bool,
}

/// `sqlite3_value_numeric_type(v) == SQLITE_INTEGER` and the value.
pub(crate) fn int_value(v: &OwnedValue) -> Option<i64> {
    match v {
        OwnedValue::Integer(i) => Some(*i),
        OwnedValue::Text(t) => match crate::record::text_to_number(t.as_slice()) {
            Some(OwnedValue::Integer(i)) => Some(i),
            _ => None,
        },
        _ => None,
    }
}

/// A `%_config` key or value `Runtime::set` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadSetting;

impl Runtime {
    pub fn new() -> Runtime {
        Runtime {
            t: Tuning::default(),
            version: 4,
            rank: AVec::new(),
            secure_delete: false,
        }
    }

    /// `sqlite3Fts5ConfigSetValue`: `Err(BadSetting)` for a bad key or
    /// value.
    pub fn set(&mut self, key: &[u8], v: &OwnedValue) -> Result<(), BadSetting> {
        let k = |n: &[u8]| key.eq_ignore_ascii_case(n);
        let int = int_value(v);
        // sqlite3_value_int: 32-bit.
        let i32v = int.map(|i| i as i32 as i64);
        if k(b"pgsz") {
            match i32v {
                Some(p) if (32..=65536).contains(&p) => self.t.pgsz = p as usize,
                _ => return Err(BadSetting),
            }
        } else if k(b"hashsize") {
            match i32v {
                Some(p) if p > 0 => self.t.hashsize = p,
                _ => return Err(BadSetting),
            }
        } else if k(b"automerge") {
            match i32v {
                Some(p) if (0..=64).contains(&p) => {
                    self.t.automerge = if p == 1 { 4 } else { p };
                }
                _ => return Err(BadSetting),
            }
        } else if k(b"usermerge") {
            match i32v {
                Some(p) if (2..=16).contains(&p) => self.t.usermerge = p,
                _ => return Err(BadSetting),
            }
        } else if k(b"crisismerge") {
            match i32v {
                Some(p) if p >= 0 => {
                    let p = if p <= 1 { 16 } else { p.min(1999) };
                    self.t.crisismerge = p;
                }
                _ => return Err(BadSetting),
            }
        } else if k(b"deletemerge") {
            let Some(mut p) = i32v else {
                return Err(BadSetting);
            };
            if p < 0 {
                p = 10;
            }
            if p > 100 {
                p = 0;
            }
            self.t.deletemerge = p;
        } else if k(b"secure-delete") {
            match i32v {
                Some(p) if p >= 0 => self.secure_delete = p != 0,
                _ => return Err(BadSetting),
            }
        } else if k(b"rank") {
            let t = match v {
                OwnedValue::Text(t) | OwnedValue::Blob(t) => t.clone(),
                _ => return Err(BadSetting),
            };
            parse_rank(t.as_slice()).ok_or(BadSetting)?;
            self.rank = t;
        } else {
            return Err(BadSetting);
        }
        Ok(())
    }
}

/// `sqlite3Fts5ConfigParseRank`: the function name and its argument
/// list text (`bm25(10.0, 5)` → `bm25`, `10.0, 5`).
pub fn parse_rank(z: &[u8]) -> Option<(AVec<u8>, AVec<u8>)> {
    let mut i = skip_ws(z, 0);
    let s = i;
    while z.get(i).is_some_and(|c| is_bareword(*c)) {
        i += 1;
    }
    if i == s {
        return None;
    }
    let mut name = AVec::new();
    name.extend_from_slice(&z[s..i]);
    i = skip_ws(z, i);
    if z.get(i) != Some(&b'(') {
        return None;
    }
    i += 1;
    i = skip_ws(z, i);
    let a = i;
    let mut end = None;
    let mut j = i;
    while j < z.len() {
        if z[j] == b')' {
            end = Some(j);
            break;
        }
        j += 1;
    }
    let e = end?;
    if skip_ws(z, e + 1) != z.len() {
        return None;
    }
    let mut args = AVec::new();
    args.extend_from_slice(z.get(a..e).unwrap_or(&[]));
    Some((name, args))
}
