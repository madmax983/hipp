//! The pin's built-in FTS5 tokenizers (`fts5_tokenize.c`):
//! `unicode61` (`remove_diacritics`, `tokenchars`, `separators`,
//! `categories`), `ascii` (`tokenchars`, `separators`), `porter
//! [inner ...]` and `trigram` (`case_sensitive`, `remove_diacritics`),
//! with the pin's UTF-8 reading (`READ_UTF8`: lone continuation
//! bytes are code points, surrogates and U+FFFE / U+FFFF read as
//! U+FFFD) and its Unicode tables (`unicode`).

use super::unicode::{CAT_INDEX, CAT_RUNS, FOLD, RM1, RM2};
use crate::arena::AVec;

/// A configured tokenizer.
#[derive(Debug, Clone)]
pub enum Tokenizer {
    Unicode61(U61),
    /// `ascii`: its ASCII token characters.
    Ascii([bool; 128]),
    /// `porter` over another tokenizer.
    Porter(alloc::boxed::Box<Tokenizer>),
    /// `trigram`: fold case (and the `remove_diacritics` level).
    Trigram {
        fold: bool,
        rd: u8,
    },
}

/// `unicode61`'s settings.
#[derive(Debug, Clone)]
pub struct U61 {
    rd: u8,
    ascii: [bool; 128],
    /// One bit per category (bit 0: the unclassified class).
    cats: u32,
    /// Code points whose class `tokenchars` / `separators` flipped.
    exc: AVec<u32>,
}

fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The category of `c` (the pin's `sqlite3Fts5UnicodeCategory`).
pub fn category(c: u32) -> u8 {
    if c >= 0x11_0000 {
        return 0;
    }
    let k = CAT_INDEX.partition_point(|e| e.0 <= c).saturating_sub(1);
    let Some(&(mut start, off)) = CAT_INDEX.get(k) else {
        return 0;
    };
    let mut i = usize::from(off);
    while i < CAT_RUNS.len() {
        let cat = CAT_RUNS[i];
        i += 1;
        let mut n = 0u32;
        let mut sh = 0;
        while let Some(b) = CAT_RUNS.get(i) {
            i += 1;
            n |= u32::from(b & 0x7F) << sh;
            sh += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        if c < start + n {
            return cat;
        }
        start += n;
    }
    0
}

/// `sqlite3Fts5UnicodeIsdiacritic`.
pub fn is_diacritic(c: u32) -> bool {
    const M0: u32 = 0x0802_9FDF;
    const M1: u32 = 0x0003_61F8;
    if !(768..=817).contains(&c) {
        return false;
    }
    if c < 768 + 32 {
        M0 & (1 << (c - 768)) != 0
    } else {
        M1 & (1 << (c - 768 - 32)) != 0
    }
}

/// `sqlite3Fts5UnicodeFold(c, rd)`.
pub fn fold(c: u32, rd: u8) -> u32 {
    if c < 128 {
        return if (65..=90).contains(&c) { c + 32 } else { c };
    }
    let mut r = c;
    let k = FOLD.partition_point(|e| e.0 <= c);
    if k > 0 {
        let (s, nb, d) = FOLD[k - 1];
        let stride = if nb & 0x80 != 0 { 2 } else { 1 };
        let n = u32::from(nb & 0x7F);
        if c < s + n * stride && (c - s).is_multiple_of(stride) {
            r = (c as i64 + i64::from(d)) as u32;
        }
    }
    if rd > 0 && r < 0x1_0000 {
        let key = r as u16;
        if rd == 2
            && let Ok(i) = RM2.binary_search_by_key(&key, |e| e.0)
        {
            return u32::from(RM2[i].1);
        }
        if let Ok(i) = RM1.binary_search_by_key(&key, |e| e.0) {
            return u32::from(RM1[i].1);
        }
    }
    r
}

/// `READ_UTF8`: the code point at `a[*i]`.
pub fn read_utf8(a: &[u8], i: &mut usize) -> u32 {
    let Some(&b) = a.get(*i) else {
        return 0;
    };
    *i += 1;
    let b = u32::from(b);
    if b < 0xC0 {
        return b;
    }
    let mut c = match b {
        0xC0..=0xDF => b & 0x1F,
        0xE0..=0xEF => b & 0x0F,
        0xF0..=0xF7 => b & 0x07,
        0xF8..=0xFB => b & 0x03,
        0xFC..=0xFD => b & 0x01,
        _ => 0,
    };
    while let Some(&n) = a.get(*i) {
        if n & 0xC0 != 0x80 {
            break;
        }
        c = c.wrapping_shl(6).wrapping_add(u32::from(n & 0x3F));
        *i += 1;
    }
    if c < 0x80 || (c & 0xFFFF_F800) == 0xD800 || (c & 0xFFFF_FFFE) == 0xFFFE {
        c = 0xFFFD;
    }
    c
}

/// `WRITE_UTF8`.
pub fn write_utf8(out: &mut AVec<u8>, c: u32) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.push(0xC0 + ((c >> 6) & 0x1F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    } else if c < 0x1_0000 {
        out.push(0xE0 + ((c >> 12) & 0x0F) as u8);
        out.push(0x80 + ((c >> 6) & 0x3F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    } else {
        out.push(0xF0 + ((c >> 18) & 0x07) as u8);
        out.push(0x80 + ((c >> 12) & 0x3F) as u8);
        out.push(0x80 + ((c >> 6) & 0x3F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    }
}

/// `sqlite3Fts5UnicodeCatParse`: the bits of category name `z` (its
/// first two bytes), or `None`.
fn cat_bits(z: &[u8]) -> Option<u32> {
    let a = z.first().copied().unwrap_or(0);
    let b = z.get(1).copied().unwrap_or(0);
    let one = |n: u32| Some(1u32 << n);
    let set = |ns: &[u32]| Some(ns.iter().fold(0u32, |m, n| m | (1 << n)));
    match (a, b) {
        (b'C', b'c') => one(1),
        (b'C', b'f') => one(2),
        (b'C', b'n') => one(3),
        (b'C', b's') => one(4),
        (b'C', b'o') => one(31),
        (b'C', b'*') => set(&[1, 2, 3, 4, 31]),
        (b'L', b'l') => one(5),
        (b'L', b'm') => one(6),
        (b'L', b'o') => one(7),
        (b'L', b't') => one(8),
        (b'L', b'u') => one(9),
        (b'L', b'C') => one(30),
        (b'L', b'*') => set(&[5, 6, 7, 8, 9, 30]),
        (b'M', b'c') => one(10),
        (b'M', b'e') => one(11),
        (b'M', b'n') => one(12),
        (b'M', b'*') => set(&[10, 11, 12]),
        (b'N', b'd') => one(13),
        (b'N', b'l') => one(14),
        (b'N', b'o') => one(15),
        (b'N', b'*') => set(&[13, 14, 15]),
        (b'P', b'c') => one(16),
        (b'P', b'd') => one(17),
        (b'P', b'e') => one(18),
        (b'P', b'f') => one(19),
        (b'P', b'i') => one(20),
        (b'P', b'o') => one(21),
        (b'P', b's') => one(22),
        (b'P', b'*') => set(&[16, 17, 18, 19, 20, 21, 22]),
        (b'S', b'c') => one(23),
        (b'S', b'k') => one(24),
        (b'S', b'm') => one(25),
        (b'S', b'o') => one(26),
        (b'S', b'*') => set(&[23, 24, 25, 26]),
        (b'Z', b'l') => one(27),
        (b'Z', b'p') => one(28),
        (b'Z', b's') => one(29),
        (b'Z', b'*') => set(&[27, 28, 29]),
        _ => None,
    }
}

/// Why a tokenizer could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokErr {
    /// "no such tokenizer".
    NoSuch,
    /// "error in tokenizer constructor".
    Args,
}

impl Tokenizer {
    /// The tokenizer `args[0]` with arguments `args[1..]` (the
    /// `tokenize=` directive's words).
    pub fn create(args: &[AVec<u8>]) -> Result<Tokenizer, TokErr> {
        let Some(name) = args.first() else {
            return Ok(Tokenizer::Unicode61(U61::new(&[])?));
        };
        let rest = args.get(1..).unwrap_or(&[]);
        let n = name.as_slice();
        if eq_ci(n, b"unicode61") {
            Ok(Tokenizer::Unicode61(U61::new(rest)?))
        } else if eq_ci(n, b"ascii") {
            if rest.len() % 2 != 0 {
                return Err(TokErr::Args);
            }
            let mut a = [false; 128];
            for (i, x) in a.iter_mut().enumerate() {
                *x = (i as u8).is_ascii_alphanumeric();
            }
            for p in rest.chunks(2) {
                let on = if eq_ci(p[0].as_slice(), b"tokenchars") {
                    true
                } else if eq_ci(p[0].as_slice(), b"separators") {
                    false
                } else {
                    return Err(TokErr::Args);
                };
                for b in p[1].iter() {
                    if *b < 0x80 {
                        a[usize::from(*b)] = on;
                    }
                }
            }
            Ok(Tokenizer::Ascii(a))
        } else if eq_ci(n, b"porter") {
            let inner = if rest.is_empty() {
                Tokenizer::Unicode61(U61::new(&[])?)
            } else {
                Tokenizer::create(rest).map_err(|_| TokErr::Args)?
            };
            Ok(Tokenizer::Porter(alloc::boxed::Box::new(inner)))
        } else if eq_ci(n, b"trigram") {
            if rest.len() % 2 != 0 {
                return Err(TokErr::Args);
            }
            let (mut fold, mut rd) = (true, 0u8);
            for p in rest.chunks(2) {
                let v = p[1].as_slice();
                let bit = match v {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(TokErr::Args),
                };
                if eq_ci(p[0].as_slice(), b"case_sensitive") {
                    fold = !bit;
                } else if eq_ci(p[0].as_slice(), b"remove_diacritics") {
                    rd = if bit { 2 } else { 0 };
                } else {
                    return Err(TokErr::Args);
                }
            }
            if rd != 0 && !fold {
                return Err(TokErr::Args);
            }
            Ok(Tokenizer::Trigram { fold, rd })
        } else {
            Err(TokErr::NoSuch)
        }
    }

    /// Tokenize `text`: `cb(token, start, end)` per token, in order.
    pub fn tokenize(&self, text: &[u8], cb: &mut dyn FnMut(&[u8], usize, usize)) {
        match self {
            Tokenizer::Unicode61(u) => u.tokenize(text, cb),
            Tokenizer::Ascii(a) => ascii_tokenize(a, text, cb),
            Tokenizer::Porter(inner) => {
                let mut buf = AVec::new();
                inner.tokenize(text, &mut |t, s, e| {
                    if t.len() > 64 || t.len() < 3 {
                        cb(t, s, e);
                    } else {
                        buf.clear();
                        buf.extend_from_slice(t);
                        porter(&mut buf);
                        cb(buf.as_slice(), s, e);
                    }
                })
            }
            Tokenizer::Trigram { fold: f, rd } => trigram(*f, *rd, text, cb),
        }
    }
}

impl U61 {
    fn new(args: &[AVec<u8>]) -> Result<U61, TokErr> {
        if !args.len().is_multiple_of(2) {
            return Err(TokErr::Args);
        }
        let mut zcat: &[u8] = b"L* N* Co";
        for p in args.chunks(2) {
            if eq_ci(p[0].as_slice(), b"categories") {
                zcat = p[1].as_slice();
            }
        }
        let mut cats = 0u32;
        let mut i = 0;
        while i < zcat.len() {
            while matches!(zcat.get(i), Some(b' ' | b'\t')) {
                i += 1;
            }
            if i < zcat.len() {
                cats |= 1 | cat_bits(&zcat[i..]).ok_or(TokErr::Args)?;
            }
            while i < zcat.len() && !matches!(zcat[i], b' ' | b'\t') {
                i += 1;
            }
        }
        let mut u = U61 {
            rd: 1,
            ascii: [false; 128],
            cats,
            exc: AVec::new(),
        };
        for (c, x) in u.ascii.iter_mut().enumerate() {
            *x = cats & (1 << category(c as u32)) != 0;
        }
        u.ascii[0] = false;
        for p in args.chunks(2) {
            let (k, v) = (p[0].as_slice(), p[1].as_slice());
            if eq_ci(k, b"remove_diacritics") {
                u.rd = match v {
                    b"0" => 0,
                    b"1" => 1,
                    b"2" => 2,
                    _ => return Err(TokErr::Args),
                };
            } else if eq_ci(k, b"tokenchars") || eq_ci(k, b"separators") {
                let on = eq_ci(k, b"tokenchars");
                let mut j = 0;
                while j < v.len() {
                    let c = read_utf8(v, &mut j);
                    if c < 128 {
                        u.ascii[c as usize] = on;
                    } else {
                        let tok = u.cats & (1 << category(c)) != 0;
                        if tok != on && !is_diacritic(c) {
                            let at = u.exc.as_slice().partition_point(|x| *x <= c);
                            u.exc.insert_at(at, c);
                        }
                    }
                }
            } else if !eq_ci(k, b"categories") {
                return Err(TokErr::Args);
            }
        }
        Ok(u)
    }

    fn is_alnum(&self, c: u32) -> bool {
        (self.cats & (1 << category(c)) != 0) ^ self.exc.as_slice().binary_search(&c).is_ok()
    }

    /// `fts5UnicodeTokenize`.
    fn tokenize(&self, text: &[u8], cb: &mut dyn FnMut(&[u8], usize, usize)) {
        let mut out = AVec::new();
        let mut i = 0usize;
        'outer: loop {
            out.clear();
            let is;
            // Skip separators; the first token character starts the
            // token.
            loop {
                let Some(&b) = text.get(i) else {
                    break 'outer;
                };
                if b & 0x80 != 0 {
                    let at = i;
                    let c = read_utf8(text, &mut i);
                    if self.is_alnum(c) {
                        is = at;
                        let f = fold(c, self.rd);
                        if f != 0 {
                            write_utf8(&mut out, f);
                        }
                        break;
                    }
                } else if self.ascii[usize::from(b)] {
                    is = i;
                    out.push(b.to_ascii_lowercase());
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            let mut ie = i;
            while let Some(&b) = text.get(i) {
                if b & 0x80 != 0 {
                    let c = read_utf8(text, &mut i);
                    if self.is_alnum(c) || is_diacritic(c) {
                        let f = fold(c, self.rd);
                        if f != 0 {
                            write_utf8(&mut out, f);
                        }
                    } else {
                        break;
                    }
                } else if !self.ascii[usize::from(b)] {
                    break;
                } else {
                    out.push(b.to_ascii_lowercase());
                    i += 1;
                }
                ie = i;
            }
            cb(out.as_slice(), is, ie);
        }
    }
}

/// `fts5AsciiTokenize`.
fn ascii_tokenize(a: &[bool; 128], text: &[u8], cb: &mut dyn FnMut(&[u8], usize, usize)) {
    let tokc = |b: u8| b & 0x80 != 0 || a[usize::from(b)];
    let mut is = 0usize;
    let mut out = AVec::new();
    while is < text.len() {
        while is < text.len() && !tokc(text[is]) {
            is += 1;
        }
        if is == text.len() {
            break;
        }
        let mut ie = is + 1;
        while ie < text.len() && tokc(text[ie]) {
            ie += 1;
        }
        out.clear();
        for b in &text[is..ie] {
            out.push(b.to_ascii_lowercase());
        }
        cb(out.as_slice(), is, ie);
        is = ie + 1;
    }
}

/// `fts5TriTokenize`.
fn trigram(f: bool, rd: u8, text: &[u8], cb: &mut dyn FnMut(&[u8], usize, usize)) {
    let mut i = 0usize;
    let mut buf: AVec<u8> = AVec::new();
    let mut starts = [0usize; 3];
    let next = |i: &mut usize| -> Option<(usize, u32)> {
        loop {
            let at = *i;
            if *i >= text.len() {
                return None;
            }
            let mut c = read_utf8(text, i);
            if f {
                c = fold(c, rd);
            }
            if c != 0 {
                return Some((at, c));
            }
        }
    };
    for s in starts.iter_mut() {
        let Some((at, c)) = next(&mut i) else {
            return;
        };
        *s = at;
        write_utf8(&mut buf, c);
    }
    loop {
        let r = next(&mut i);
        let inext = r.map_or(text.len(), |x| x.0);
        cb(buf.as_slice(), starts[0], inext);
        let Some((at, c)) = r else {
            break;
        };
        // Drop the first character, append the next.
        let mut k = 1;
        if buf[0] >= 0xC0 {
            while k < buf.len() && buf[k] & 0xC0 == 0x80 {
                k += 1;
            }
        }
        buf.remove_front(k);
        write_utf8(&mut buf, c);
        starts[0] = starts[1];
        starts[1] = starts[2];
        starts[2] = at;
    }
}

// ---------------------------------------------------------------
// The Porter stemmer (fts5PorterCb)
// ---------------------------------------------------------------

fn is_vowel(c: u8, y_is_vowel: bool) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u') || (y_is_vowel && c == b'y')
}

/// `fts5PorterGobbleVC`.
fn gobble_vc(z: &[u8], prev_cons: bool) -> usize {
    let mut cons = prev_cons;
    let mut i = 0;
    while i < z.len() {
        cons = !is_vowel(z[i], cons);
        if !cons {
            break;
        }
        i += 1;
    }
    i += 1;
    while i < z.len() {
        cons = !is_vowel(z[i], cons);
        if cons {
            return i + 1;
        }
        i += 1;
    }
    0
}

fn m_gt0(z: &[u8]) -> bool {
    gobble_vc(z, false) != 0
}

fn m_gt1(z: &[u8]) -> bool {
    let n = gobble_vc(z, false);
    n != 0 && gobble_vc(z.get(n..).unwrap_or(&[]), true) != 0
}

fn m_eq1(z: &[u8]) -> bool {
    let n = gobble_vc(z, false);
    n != 0 && gobble_vc(z.get(n..).unwrap_or(&[]), true) == 0
}

/// `fts5Porter_Ostar`.
fn ostar(z: &[u8]) -> bool {
    match z.last() {
        None | Some(b'w' | b'x' | b'y') => false,
        _ => {
            let mut mask = 0u32;
            let mut cons = false;
            for c in z.iter() {
                cons = !is_vowel(*c, cons);
                mask = (mask << 1) + u32::from(cons);
            }
            mask & 7 == 5
        }
    }
}

fn has_vowel(z: &[u8]) -> bool {
    z.iter().enumerate().any(|(i, c)| is_vowel(*c, i > 0))
}

/// The condition of a rule (`fts5PorterStepN`'s generated tests).
#[derive(Clone, Copy)]
enum Cond {
    None,
    MGt0,
    MGt1,
    MGt1St,
    Vowel,
}

fn cond(c: Cond, z: &[u8]) -> bool {
    match c {
        Cond::None => true,
        Cond::MGt0 => m_gt0(z),
        Cond::MGt1 => m_gt1(z),
        Cond::MGt1St => matches!(z.last(), Some(b's' | b't')) && m_gt1(z),
        Cond::Vowel => has_vowel(z),
    }
}

/// One generated step: the rules whose suffix's second-to-last
/// letter is the word's, tried in order; the first suffix that
/// matches decides (its condition may still fail). Returns whether
/// a rule fired.
fn step(buf: &mut AVec<u8>, rules: &[(&[u8], &[u8], Cond)]) -> bool {
    let n = buf.len();
    let c2 = if n >= 2 { buf[n - 2] } else { 0 };
    for (suf, rep, c) in rules {
        let l = suf.len();
        if suf[l - 2] != c2 {
            continue;
        }
        if n > l && buf.as_slice().ends_with(suf) {
            if cond(*c, &buf.as_slice()[..n - l]) {
                buf.truncate(n - l);
                buf.extend_from_slice(rep);
                return true;
            }
            return false;
        }
    }
    false
}

use Cond::{MGt0, MGt1, MGt1St, Vowel};

const STEP1B: &[(&[u8], &[u8], Cond)] = &[
    (b"eed", b"ee", MGt0),
    (b"ed", b"", Vowel),
    (b"ing", b"", Vowel),
];
const STEP1B2: &[(&[u8], &[u8], Cond)] = &[
    (b"at", b"ate", Cond::None),
    (b"bl", b"ble", Cond::None),
    (b"iz", b"ize", Cond::None),
];
const STEP2: &[(&[u8], &[u8], Cond)] = &[
    (b"ational", b"ate", MGt0),
    (b"tional", b"tion", MGt0),
    (b"enci", b"ence", MGt0),
    (b"anci", b"ance", MGt0),
    (b"izer", b"ize", MGt0),
    (b"logi", b"log", MGt0),
    (b"bli", b"ble", MGt0),
    (b"alli", b"al", MGt0),
    (b"entli", b"ent", MGt0),
    (b"eli", b"e", MGt0),
    (b"ousli", b"ous", MGt0),
    (b"ization", b"ize", MGt0),
    (b"ation", b"ate", MGt0),
    (b"ator", b"ate", MGt0),
    (b"alism", b"al", MGt0),
    (b"iveness", b"ive", MGt0),
    (b"fulness", b"ful", MGt0),
    (b"ousness", b"ous", MGt0),
    (b"aliti", b"al", MGt0),
    (b"iviti", b"ive", MGt0),
    (b"biliti", b"ble", MGt0),
];
const STEP3: &[(&[u8], &[u8], Cond)] = &[
    (b"ical", b"ic", MGt0),
    (b"ness", b"", MGt0),
    (b"icate", b"ic", MGt0),
    (b"iciti", b"ic", MGt0),
    (b"ful", b"", MGt0),
    (b"ative", b"", MGt0),
    (b"alize", b"al", MGt0),
];
const STEP4: &[(&[u8], &[u8], Cond)] = &[
    (b"al", b"", MGt1),
    (b"ance", b"", MGt1),
    (b"ence", b"", MGt1),
    (b"er", b"", MGt1),
    (b"ic", b"", MGt1),
    (b"able", b"", MGt1),
    (b"ible", b"", MGt1),
    (b"ant", b"", MGt1),
    (b"ement", b"", MGt1),
    (b"ment", b"", MGt1),
    (b"ent", b"", MGt1),
    (b"ion", b"", MGt1St),
    (b"ou", b"", MGt1),
    (b"ism", b"", MGt1),
    (b"ate", b"", MGt1),
    (b"iti", b"", MGt1),
    (b"ous", b"", MGt1),
    (b"ive", b"", MGt1),
    (b"ize", b"", MGt1),
];

/// Stem the token in `b` (3 to 64 bytes) as `fts5PorterCb` does.
fn porter(b: &mut AVec<u8>) {
    // Step 1A.
    let n = b.len();
    if b[n - 1] == b's' {
        if b[n - 2] == b'e' {
            if (n > 4 && b[n - 4] == b's' && b[n - 3] == b's') || (n > 3 && b[n - 3] == b'i') {
                b.truncate(n - 2);
            } else {
                b.truncate(n - 1);
            }
        } else if b[n - 2] != b's' {
            b.truncate(n - 1);
        }
    }
    // Step 1B.
    let fired = {
        let n = b.len();
        // "eed" with a failing condition stops the step.
        if n > 3 && b.as_slice().ends_with(b"eed") {
            if m_gt0(&b.as_slice()[..n - 3]) {
                b.truncate(n - 1);
            }
            false
        } else {
            step(b, &STEP1B[1..])
        }
    };
    if fired && !step(b, STEP1B2) {
        let n = b.len();
        let c = b[n - 1];
        if !is_vowel(c, false) && c != b'l' && c != b's' && c != b'z' && n >= 2 && c == b[n - 2] {
            b.truncate(n - 1);
        } else if m_eq1(b.as_slice()) && ostar(b.as_slice()) {
            b.push(b'e');
        }
    }
    // Step 1C.
    let n = b.len();
    if b[n - 1] == b'y' && has_vowel(&b.as_slice()[..n - 1]) {
        b[n - 1] = b'i';
    }
    step(b, STEP2);
    step(b, STEP3);
    step(b, STEP4);
    // Step 5a.
    let n = b.len();
    if b[n - 1] == b'e'
        && (m_gt1(&b.as_slice()[..n - 1])
            || (m_eq1(&b.as_slice()[..n - 1]) && !ostar(&b.as_slice()[..n - 1])))
    {
        b.truncate(n - 1);
    }
    // Step 5b.
    let n = b.len();
    if n > 1 && b[n - 1] == b'l' && b[n - 2] == b'l' && m_gt1(&b.as_slice()[..n - 1]) {
        b.truncate(n - 1);
    }
}
