//! The v0.16 scalar function library (`sql`): `printf()` /
//! `format()` and the value functions SQLite 3.54.1 ships beside
//! it — `quote()`, `char()`, `unicode()`, `zeroblob()`,
//! `randomblob()` / `random()`, `concat()`, `concat_ws()`,
//! `octet_length()`, `unhex()` — plus the deterministic PRNG the
//! random functions draw from.
//!
//! Everything here was fitted to the pin's observable output
//! (STATUS.md, "Scalar function library"): the printf engine
//! follows the pin's conversion grammar flag by flag, and its
//! REAL conversions round the pin's own 18 / 19 internal digits
//! (`lexer::pin_digits`, the digits `printf('%!.26g')` shows),
//! never a correctly rounded expansion. No float math beyond
//! core `f64` comparisons; no `core::fmt`.

use crate::arena::AVec;
use crate::error::{Error, Result, SchemaKind};
use crate::query::{Num, atoi_prefix, num_to_i64, to_number, to_text};
use crate::record::OwnedValue;

/// The pin's `SQLITE_LIMIT_LENGTH` (1,000,000,000): the largest
/// string or blob a function may return is one byte less for
/// printf (its accumulator counts the terminator) and exactly this
/// for `zeroblob` / `randomblob`.
pub const MAX_LENGTH: usize = 1_000_000_000;

fn too_big<E>() -> Error<E> {
    Error::Schema {
        kind: SchemaKind::TooBig,
    }
}

/// Reserve room for `n` more bytes in `out`, refusing anything that
/// would reach `MAX_LENGTH` (the pin's "string or blob too big")
/// and reporting an allocation failure as arena exhaustion instead
/// of aborting.
fn room<E>(out: &mut AVec<u8>, n: usize) -> Result<(), E> {
    match out.len().checked_add(n) {
        Some(t) if t < MAX_LENGTH => out.try_reserve(n).map_err(Error::from),
        _ => Err(too_big()),
    }
}

fn put<E>(out: &mut AVec<u8>, b: &[u8]) -> Result<(), E> {
    room::<E>(out, b.len())?;
    out.extend_from_slice(b);
    Ok(())
}

fn fill<E>(out: &mut AVec<u8>, c: u8, n: usize) -> Result<(), E> {
    room::<E>(out, n)?;
    for _ in 0..n {
        out.push(c);
    }
    Ok(())
}

/// A text value as the pin's `sqlite3_value_text` hands it to C
/// code: the bytes up to (not including) the first NUL.
fn c_str(t: &[u8]) -> &[u8] {
    match t.iter().position(|b| *b == 0) {
        Some(i) => t.get(..i).unwrap_or(t),
        None => t,
    }
}

/// The pin's `sqlite3_value_double`: a text / blob's numeric
/// prefix (no digits: 0.0), an integer converted, NULL 0.0.
pub fn real_value(v: &OwnedValue) -> f64 {
    match to_number(v) {
        Some(Num::Int(i)) => i as f64,
        Some(Num::Real(r)) => r,
        None => 0.0,
    }
}

/// The pin's `sqlite3_value_int64`.
pub fn int64_value(v: &OwnedValue) -> i64 {
    match v {
        OwnedValue::Null => 0,
        OwnedValue::Integer(i) => *i,
        OwnedValue::Real(r) => num_to_i64(Num::Real(*r)),
        OwnedValue::Text(t) | OwnedValue::Blob(t) => atoi_prefix(t.as_slice()),
    }
}

// ---------------------------------------------------------------
// printf()
// ---------------------------------------------------------------

/// The arguments after the format, consumed left to right; a
/// missing one reads as NULL (0, 0.0, no text), as on the pin.
struct Args<'a> {
    v: &'a [OwnedValue],
    used: usize,
}

impl Args<'_> {
    fn next(&mut self) -> Option<&OwnedValue> {
        let v = self.v.get(self.used)?;
        self.used += 1;
        Some(v)
    }
    fn int(&mut self) -> i64 {
        self.next().map_or(0, int64_value)
    }
    fn real(&mut self) -> f64 {
        self.next().map_or(0.0, real_value)
    }
    /// `sqlite3_value_text`: None for NULL / missing, else the
    /// text form up to its first NUL.
    fn text(&mut self) -> Option<AVec<u8>> {
        let t = to_text(self.next()?)?;
        let n = c_str(t.as_slice()).len();
        if n == t.len() {
            return Some(t);
        }
        let mut c = AVec::new();
        c.extend_from_slice(t.as_slice().get(..n).unwrap_or(&[]));
        Some(c)
    }
}

/// Conversion types of the pin's format table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conv {
    /// Integer conversions: base, signed, digit case (upper),
    /// alternate-form prefix, ordinal suffix, thousands allowed.
    Int {
        base: u8,
        signed: bool,
        upper: bool,
        prefix: &'static [u8],
        ordinal: bool,
        commas: bool,
    },
    Float,
    Exp(bool),
    Generic(bool),
    Str,
    Char,
    Esc(u8),
    Json(bool),
    Percent,
    Size,
}

fn conv_of(c: u8) -> Option<Conv> {
    let int = |base, signed, upper, prefix, ordinal, commas| Conv::Int {
        base,
        signed,
        upper,
        prefix,
        ordinal,
        commas,
    };
    Some(match c {
        b'd' | b'i' => int(10, true, false, b"", false, true),
        b'u' => int(10, false, false, b"", false, true),
        b'x' => int(16, false, false, b"0x", false, false),
        b'X' => int(16, false, true, b"0X", false, false),
        b'o' => int(8, false, false, b"0", false, false),
        b'p' => int(16, false, true, b"0x", false, false),
        b'r' => int(10, true, false, b"", true, false),
        b'f' => Conv::Float,
        b'e' => Conv::Exp(false),
        b'E' => Conv::Exp(true),
        b'g' => Conv::Generic(false),
        b'G' => Conv::Generic(true),
        b's' | b'z' => Conv::Str,
        b'c' => Conv::Char,
        b'q' | b'Q' | b'w' => Conv::Esc(c),
        b'j' => Conv::Json(false),
        b'J' => Conv::Json(true),
        b'%' => Conv::Percent,
        b'n' => Conv::Size,
        _ => return None,
    })
}

/// One conversion's flags, width and precision.
#[derive(Clone, Copy, Default)]
struct Spec {
    left: bool,
    prefix: u8,
    thousand: bool,
    alt: bool,
    alt2: bool,
    zero: bool,
    width: i32,
    precision: i32,
}

/// `printf(FORMAT, ...)` / `format(FORMAT, ...)` — the pin's
/// `printfFunc` over its `sqlite3_str_vappendf` in SQL-function
/// mode. NULL without a format or with a NULL one; otherwise
/// TEXT. The format and text arguments end at their first NUL;
/// an unknown conversion (or `%T` / `%S`, which the pin keeps
/// for internal use) ends the output there; missing arguments
/// read as NULL. A result of 1,000,000,000 bytes or more is
/// `SchemaKind::TooBig`.
pub fn printf<E>(args: &[OwnedValue]) -> Result<OwnedValue, E> {
    let Some(first) = args.first() else {
        return Ok(OwnedValue::Null);
    };
    let Some(fmt) = to_text(first) else {
        return Ok(OwnedValue::Null);
    };
    let fmt = c_str(fmt.as_slice());
    let mut a = Args {
        v: args.get(1..).unwrap_or(&[]),
        used: 0,
    };
    let mut out: AVec<u8> = AVec::new();
    format_into::<E>(fmt, &mut a, &mut out)?;
    Ok(OwnedValue::Text(out))
}

fn format_into<E>(fmt: &[u8], a: &mut Args<'_>, out: &mut AVec<u8>) -> Result<(), E> {
    let at = |i: usize| fmt.get(i).copied().unwrap_or(0);
    let mut i = 0usize;
    loop {
        let mut c = at(i);
        if c == 0 {
            return Ok(());
        }
        if c != b'%' {
            let end = fmt
                .get(i..)
                .and_then(|r| r.iter().position(|b| *b == b'%'))
                .map_or(fmt.len(), |p| i + p);
            put::<E>(out, fmt.get(i..end).unwrap_or(&[]))?;
            i = end;
            continue;
        }
        i += 1;
        c = at(i);
        if c == 0 {
            return put::<E>(out, b"%");
        }
        let mut sp = Spec {
            precision: -1,
            ..Spec::default()
        };
        loop {
            let mut done = false;
            match c {
                b'-' => sp.left = true,
                b'+' => sp.prefix = b'+',
                b' ' => sp.prefix = b' ',
                b'#' => sp.alt = true,
                b'!' => sp.alt2 = true,
                b'0' => sp.zero = true,
                b',' => sp.thousand = true,
                b'l' => {
                    i += 1;
                    c = at(i);
                    if c == b'l' {
                        i += 1;
                        c = at(i);
                    }
                    done = true;
                }
                b'1'..=b'9' => {
                    let mut wx = u32::from(c - b'0');
                    loop {
                        i += 1;
                        c = at(i);
                        if !c.is_ascii_digit() {
                            break;
                        }
                        wx = wx.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                    }
                    sp.width = (wx & 0x7fff_ffff) as i32;
                    if c != b'.' && c != b'l' {
                        done = true;
                    } else {
                        i -= 1;
                    }
                }
                b'*' => {
                    let w = a.int() as i32;
                    sp.width = w;
                    if w < 0 {
                        sp.left = true;
                        sp.width = if w >= -2_147_483_647 { -w } else { 0 };
                    }
                    let nx = at(i + 1);
                    if nx != b'.' && nx != b'l' {
                        i += 1;
                        c = at(i);
                        done = true;
                    }
                }
                b'.' => {
                    i += 1;
                    c = at(i);
                    if c == b'*' {
                        let p = a.int() as i32;
                        sp.precision = p;
                        if p < 0 {
                            sp.precision = if p >= -2_147_483_647 { -p } else { -1 };
                        }
                        i += 1;
                        c = at(i);
                    } else {
                        let mut px = 0u32;
                        while c.is_ascii_digit() {
                            px = px.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                            i += 1;
                            c = at(i);
                        }
                        sp.precision = (px & 0x7fff_ffff) as i32;
                    }
                    if c == b'l' {
                        i -= 1;
                    } else {
                        done = true;
                    }
                }
                _ => done = true,
            }
            if done {
                break;
            }
            i += 1;
            c = at(i);
            if c == 0 {
                break;
            }
        }
        // An unknown conversion — or %T / %S, internal-only on the
        // pin — ends the output.
        let Some(conv) = conv_of(c) else {
            return Ok(());
        };
        convert::<E>(conv, sp, a, out)?;
        i += 1;
    }
}

/// Emit `body` in a field of `width` bytes, space-padded on the
/// left (or the right when left-justified).
fn emit<E>(out: &mut AVec<u8>, body: &[u8], width: i32, left: bool) -> Result<(), E> {
    let pad = (width as i64 - body.len() as i64).max(0) as usize;
    if pad > 0 && !left {
        fill::<E>(out, b' ', pad)?;
    }
    put::<E>(out, body)?;
    if pad > 0 && left {
        fill::<E>(out, b' ', pad)?;
    }
    Ok(())
}

/// The pin's "!" width rule for strings: count the field in
/// characters by widening it by the body's continuation bytes.
fn utf8_width(sp: &Spec, body: &[u8]) -> i32 {
    let mut w = sp.width;
    if sp.alt2 && w > 0 {
        let extra = body.iter().filter(|b| (**b & 0xc0) == 0x80).count();
        w = w.saturating_add(i32::try_from(extra).unwrap_or(i32::MAX));
    }
    w
}

/// The byte length of the first `precision` bytes — or, with
/// `chars`, characters — of `s` (all of it for a negative
/// precision), stopping at a NUL.
fn prefix_len(s: &[u8], precision: i32, chars: bool) -> usize {
    let mut k = precision;
    let mut i = 0usize;
    while k != 0 {
        let Some(&ch) = s.get(i) else { break };
        if ch == 0 {
            break;
        }
        i += 1;
        if chars && (ch & 0xc0) == 0xc0 {
            while s.get(i).is_some_and(|b| (*b & 0xc0) == 0x80) {
                i += 1;
            }
        }
        k = k.saturating_sub(1);
    }
    i
}

fn convert<E>(conv: Conv, mut sp: Spec, a: &mut Args<'_>, out: &mut AVec<u8>) -> Result<(), E> {
    match conv {
        Conv::Percent => emit::<E>(out, b"%", sp.width, sp.left),
        Conv::Size => Ok(()),
        Conv::Int {
            base,
            signed,
            upper,
            prefix,
            ordinal,
            commas,
        } => {
            let thousand = sp.thousand && commas;
            let (mut val, sign) = if signed {
                let v = a.int();
                if v < 0 {
                    (v.unsigned_abs(), b'-')
                } else {
                    (v as u64, sp.prefix)
                }
            } else {
                (a.int() as u64, 0)
            };
            let mut alt = sp.alt;
            if val == 0 {
                alt = false;
            }
            let has_sign = i32::from(sign != 0);
            let mut precision = sp.precision;
            if sp.zero && precision < sp.width.saturating_sub(has_sign) {
                precision = sp.width.saturating_sub(has_sign);
            }
            // Digits, least significant first, then reversed.
            let mut digs: AVec<u8> = AVec::new();
            if ordinal {
                let x = (val % 10) as usize;
                let x = if x >= 4 || (val / 10) % 10 == 1 { 0 } else { x };
                let suf: &[u8] = b"thstndrd";
                digs.push(suf.get(x * 2 + 1).copied().unwrap_or(b'h'));
                digs.push(suf.get(x * 2).copied().unwrap_or(b't'));
            }
            let set: &[u8] = if upper {
                b"0123456789ABCDEF"
            } else {
                b"0123456789abcdef"
            };
            let b = u64::from(base);
            loop {
                digs.push(set.get((val % b) as usize).copied().unwrap_or(b'0'));
                val /= b;
                if val == 0 {
                    break;
                }
            }
            let want = usize::try_from(precision.max(0)).unwrap_or(0);
            if want > digs.len() {
                let more = want - digs.len();
                room::<E>(&mut digs, more)?;
                while digs.len() < want {
                    digs.push(b'0');
                }
            }
            let len = digs.len();
            let mut body: AVec<u8> = AVec::new();
            room::<E>(&mut body, len + len / 3 + 3)?;
            if alt {
                body.extend_from_slice(prefix);
            }
            if sign != 0 {
                body.push(sign);
            }
            // Digits most significant first, `,` between groups of
            // three (zero padding included, as on the pin). Only
            // unsigned conversions carry an alternate prefix, so it
            // and a sign never meet.
            for (k, d) in digs.as_slice().iter().rev().enumerate() {
                body.push(*d);
                if thousand && k + 1 < len && (len - 1 - k).is_multiple_of(3) {
                    body.push(b',');
                }
            }
            emit::<E>(out, body.as_slice(), sp.width, sp.left)
        }
        Conv::Float | Conv::Exp(_) | Conv::Generic(_) => float::<E>(conv, sp, a.real(), out),
        Conv::Str => {
            let t = a.text();
            let s: &[u8] = t.as_ref().map_or(&[], |t| t.as_slice());
            let len = if sp.precision >= 0 {
                prefix_len(s, sp.precision, sp.alt2)
            } else {
                s.len()
            };
            let body = s.get(..len).unwrap_or(s);
            emit::<E>(out, body, utf8_width(&sp, body), sp.left)
        }
        Conv::Char => {
            let t = a.text();
            let mut buf = [0u8; 4];
            let mut length = 1usize;
            if let Some(t) = &t {
                let s = t.as_slice();
                let c0 = s.first().copied().unwrap_or(0);
                buf[0] = c0;
                if (c0 & 0xc0) == 0xc0 {
                    while length < 4 {
                        match s.get(length) {
                            Some(b) if (*b & 0xc0) == 0x80 => {
                                if let Some(slot) = buf.get_mut(length) {
                                    *slot = *b;
                                }
                                length += 1;
                            }
                            _ => break,
                        }
                    }
                }
            }
            let one = buf.get(..length).unwrap_or(&[]);
            if sp.precision > 1 {
                sp.width = sp.width.saturating_sub(sp.precision - 1);
                if sp.width > 1 && !sp.left {
                    fill::<E>(out, b' ', (sp.width - 1) as usize)?;
                    sp.width = 0;
                }
                let copies = (sp.precision - 1) as usize;
                room::<E>(out, copies.saturating_mul(length))?;
                for _ in 0..copies {
                    out.extend_from_slice(one);
                }
            }
            sp.alt2 = true;
            emit::<E>(out, one, utf8_width(&sp, one), sp.left)
        }
        Conv::Esc(kind) => {
            let t = a.text();
            let q = if kind == b'w' { b'"' } else { b'\'' };
            let isnull = t.is_none();
            let s: &[u8] = match &t {
                Some(t) => t.as_slice(),
                None if kind == b'Q' => b"NULL",
                None => b"(NULL)",
            };
            let n = prefix_len(s, sp.precision, sp.alt2);
            let src = s.get(..n).unwrap_or(s);
            // The `#` form (3.50+): control characters become
            // `\u00XX` escapes and backslashes double — always for
            // %q, and for %Q only when a control character is
            // present, which then wraps the literal in unistr(...).
            let ctrl = src.iter().any(|b| *b <= 0x1f);
            let uni = sp.alt && (kind == b'q' || (kind == b'Q' && ctrl));
            let quote = !isnull && kind == b'Q';
            let mut body: AVec<u8> = AVec::new();
            room::<E>(&mut body, n.saturating_mul(2) + 12)?;
            if quote {
                if uni && ctrl {
                    body.extend_from_slice(b"unistr(");
                }
                body.push(q);
            }
            for &ch in src {
                if uni && ch == b'\\' {
                    body.extend_from_slice(b"\\\\");
                } else if uni && ch <= 0x1f {
                    hex_escape::<E>(&mut body, ch)?;
                } else {
                    body.push(ch);
                    if ch == q {
                        body.push(ch);
                    }
                }
            }
            if quote {
                body.push(q);
                if uni && ctrl {
                    body.push(b')');
                }
            }
            emit::<E>(
                out,
                body.as_slice(),
                utf8_width(&sp, body.as_slice()),
                sp.left,
            )
        }
        Conv::Json(quoted) => {
            let t = a.text();
            let mut body: AVec<u8> = AVec::new();
            match &t {
                None => {
                    if quoted {
                        body.extend_from_slice(b"null");
                    }
                }
                Some(t) => {
                    let s = t.as_slice();
                    let n = prefix_len(s, sp.precision, sp.alt2);
                    let src = s.get(..n).unwrap_or(s);
                    room::<E>(&mut body, n.saturating_mul(2) + 2)?;
                    if quoted {
                        body.push(b'"');
                    }
                    for &ch in src {
                        json_escape::<E>(&mut body, ch)?;
                    }
                    if quoted {
                        body.push(b'"');
                    }
                }
            }
            emit::<E>(
                out,
                body.as_slice(),
                utf8_width(&sp, body.as_slice()),
                sp.left,
            )
        }
    }
}

fn hex_escape<E>(body: &mut AVec<u8>, ch: u8) -> Result<(), E> {
    let h: &[u8] = b"0123456789abcdef";
    put::<E>(
        body,
        &[
            b'\\',
            b'u',
            b'0',
            b'0',
            h.get(usize::from(ch >> 4)).copied().unwrap_or(b'0'),
            h.get(usize::from(ch & 0xf)).copied().unwrap_or(b'0'),
        ],
    )
}

/// One byte of a JSON string literal (`%j` / `%J`, new in 3.54):
/// `"` and `\` escaped, the C0 controls as `\b \f \n \r \t` or
/// `\u00xx`, every other byte — DEL and UTF-8 included — as is.
fn json_escape<E>(body: &mut AVec<u8>, ch: u8) -> Result<(), E> {
    let short = match ch {
        b'"' => b'"',
        b'\\' => b'\\',
        0x08 => b'b',
        0x0c => b'f',
        b'\n' => b'n',
        b'\r' => b'r',
        b'\t' => b't',
        c if c <= 0x1f => return hex_escape::<E>(body, c),
        c => return put::<E>(body, &[c]),
    };
    put::<E>(body, &[b'\\', short])
}

/// The REAL conversions `%f %e %E %g %G`, as the pin lays them out
/// from its rounded digits (see `fp_decode`).
fn float<E>(conv: Conv, sp: Spec, r: f64, out: &mut AVec<u8>) -> Result<(), E> {
    // The pin's SQLITE_FP_PRECISION_LIMIT: at most 100,000,000
    // digits after the point.
    let mut precision = if sp.precision < 0 {
        6
    } else {
        sp.precision.min(100_000_000)
    };
    let mut exp_kind = match conv {
        Conv::Exp(u) | Conv::Generic(u) => Some(u),
        _ => None,
    };
    let iround = match conv {
        Conv::Float => -precision,
        Conv::Generic(_) => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
        _ => precision.saturating_add(1),
    };
    let mut f = crate::record::fp_decode(r, iround, if sp.alt2 { 26 } else { 16 });
    if f.special == 2 {
        let body: &[u8] = if sp.zero { b"null" } else { b"NaN" };
        return emit::<E>(out, body, sp.width, sp.left);
    }
    if f.special == 1 {
        if sp.zero {
            if let Some(z) = f.z.get_mut(f.s) {
                *z = b'9';
            }
            f.n = 1;
            f.idp = 1000;
        } else {
            let mut body: AVec<u8> = AVec::new();
            if f.neg {
                body.push(b'-');
            } else if sp.prefix != 0 {
                body.push(sp.prefix);
            }
            body.extend_from_slice(b"Inf");
            return emit::<E>(out, body.as_slice(), sp.width, sp.left);
        }
    }
    let mut prefix = if f.neg { b'-' } else { sp.prefix };
    let exp = f.idp - 1;
    let rtz;
    if let Conv::Generic(upper) = conv {
        precision -= 1;
        rtz = !sp.alt;
        if exp < -4 || exp > precision {
            exp_kind = Some(upper);
        } else {
            precision -= exp;
            exp_kind = None;
        }
    } else {
        rtz = sp.alt2;
    }
    let mut e2 = if exp_kind.is_some() { 0 } else { f.idp - 1 };
    let mut body: AVec<u8> = AVec::new();
    let need = (e2.max(0) as usize)
        .saturating_add(precision.max(0) as usize)
        .saturating_add(sp.width.max(0) as usize)
        .saturating_add(15);
    room::<E>(&mut body, need)?;
    let dp = precision > 0 || sp.alt || sp.alt2;
    if prefix != 0 {
        body.push(prefix);
    }
    let mut j = 0usize;
    if e2 < 0 {
        body.push(b'0');
    } else {
        while e2 >= 0 {
            body.push(f.digit(j));
            j += 1;
            if sp.thousand && e2 % 3 == 0 && e2 > 1 {
                body.push(b',');
            }
            e2 -= 1;
        }
    }
    if dp {
        body.push(b'.');
    }
    e2 += 1;
    while e2 < 0 && precision > 0 {
        body.push(b'0');
        precision -= 1;
        e2 += 1;
    }
    while precision > 0 {
        body.push(f.digit(j));
        j += 1;
        precision -= 1;
    }
    if rtz && dp {
        while body.as_slice().last() == Some(&b'0') {
            body.pop();
        }
        if body.as_slice().last() == Some(&b'.') {
            if sp.alt2 {
                body.push(b'0');
            } else {
                body.pop();
            }
        }
    }
    if let Some(upper) = exp_kind {
        let mut e = f.idp - 1;
        body.push(if upper { b'E' } else { b'e' });
        if e < 0 {
            body.push(b'-');
            e = -e;
        } else {
            body.push(b'+');
        }
        if e >= 100 {
            body.push(b'0' + (e / 100) as u8);
            e %= 100;
        }
        body.push(b'0' + (e / 10) as u8);
        body.push(b'0' + (e % 10) as u8);
    }
    // The `#` form drops the sign of a negative value that prints
    // as zero (`%#.0f` of -0.4 is `0.`) unless a `+` / space flag
    // asks for one — as the pin prints it.
    if sp.alt
        && sp.prefix == 0
        && prefix == b'-'
        && body
            .as_slice()
            .iter()
            .all(|b| matches!(b, b'-' | b'0' | b'.' | b','))
    {
        let mut rest: AVec<u8> = AVec::new();
        rest.extend_from_slice(body.as_slice().get(1..).unwrap_or(&[]));
        body = rest;
        prefix = 0;
    }
    if sp.zero && !sp.left && (body.len() as i64) < i64::from(sp.width) {
        let pad = (i64::from(sp.width) - body.len() as i64) as usize;
        let at = usize::from(prefix != 0);
        let mut z: AVec<u8> = AVec::new();
        room::<E>(&mut z, body.len() + pad)?;
        z.extend_from_slice(body.as_slice().get(..at).unwrap_or(&[]));
        for _ in 0..pad {
            z.push(b'0');
        }
        z.extend_from_slice(body.as_slice().get(at..).unwrap_or(&[]));
        return put::<E>(out, z.as_slice());
    }
    emit::<E>(out, body.as_slice(), sp.width, sp.left)
}

// ---------------------------------------------------------------
// The value functions
// ---------------------------------------------------------------

/// `quote(X)`: an SQL literal for X — NULL, an integer, a REAL as
/// its text form (`±9.0e+999` for an infinity, which reads back
/// as one), `'...'` with quotes doubled and the text ending at its
/// first NUL, `X'..'` upper-case hex for a BLOB.
pub fn quote<E>(v: &OwnedValue) -> Result<OwnedValue, E> {
    let mut out: AVec<u8> = AVec::new();
    match v {
        OwnedValue::Null => out.extend_from_slice(b"NULL"),
        OwnedValue::Integer(i) => crate::record::render_int(*i, &mut out),
        OwnedValue::Real(r) => {
            if crate::record::fabs(*r) == f64::INFINITY {
                if *r < 0.0 {
                    out.push(b'-');
                }
                out.extend_from_slice(b"9.0e+999");
            } else {
                crate::record::render_real(*r, &mut out);
            }
        }
        OwnedValue::Text(t) => {
            let s = c_str(t.as_slice());
            room::<E>(&mut out, s.len().saturating_mul(2) + 2)?;
            out.push(b'\'');
            for &b in s {
                out.push(b);
                if b == b'\'' {
                    out.push(b);
                }
            }
            out.push(b'\'');
        }
        OwnedValue::Blob(b) => {
            room::<E>(&mut out, b.len().saturating_mul(2) + 3)?;
            out.extend_from_slice(b"X'");
            push_hex(&mut out, b.as_slice());
            out.push(b'\'');
        }
    }
    Ok(OwnedValue::Text(out))
}

fn push_hex(out: &mut AVec<u8>, b: &[u8]) {
    let h: &[u8] = b"0123456789ABCDEF";
    for x in b {
        out.push(h.get(usize::from(x >> 4)).copied().unwrap_or(b'0'));
        out.push(h.get(usize::from(x & 0xf)).copied().unwrap_or(b'0'));
    }
}

/// `char(X1, ..., XN)`: the characters with these code points
/// (each read as an integer — NULL is 0, a NUL character), UTF-8
/// encoded; outside 0 ..= 0x10FFFF is U+FFFD; surrogates are
/// encoded as they are, as on the pin.
pub fn char_fn<E>(v: &[OwnedValue]) -> Result<OwnedValue, E> {
    let mut out: AVec<u8> = AVec::new();
    room::<E>(&mut out, v.len().saturating_mul(4))?;
    for x in v {
        let c = int64_value(x);
        let c = if (0..=0x10ffff).contains(&c) {
            c as u32
        } else {
            0xfffd
        };
        if c < 0x80 {
            out.push(c as u8);
        } else if c < 0x800 {
            out.push(0xc0 | (c >> 6) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        } else if c < 0x10000 {
            out.push(0xe0 | (c >> 12) as u8);
            out.push(0x80 | ((c >> 6) & 0x3f) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        } else {
            out.push(0xf0 | (c >> 18) as u8);
            out.push(0x80 | ((c >> 12) & 0x3f) as u8);
            out.push(0x80 | ((c >> 6) & 0x3f) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        }
    }
    Ok(OwnedValue::Text(out))
}

/// The pin's `sqlite3Utf8Read`: the code point starting the bytes
/// (a lead byte takes every continuation byte after it; overlong
/// forms and surrogates are U+FFFD, a stray continuation byte is
/// itself).
fn utf8_read(s: &[u8]) -> u32 {
    let Some(&c0) = s.first() else {
        return 0;
    };
    let mut c = u32::from(c0);
    if c >= 0xc0 {
        // The pin's lead-byte table: the payload bits of 2-, 3-
        // and 4-byte leads, and of the obsolete 5- / 6-byte ones.
        c = u32::from(match c0 {
            0xc0..=0xdf => c0 - 0xc0,
            0xe0..=0xef => c0 - 0xe0,
            0xf0..=0xf7 => c0 - 0xf0,
            0xf8..=0xfb => c0 - 0xf8,
            0xfc..=0xfd => c0 - 0xfc,
            _ => 0,
        });
        for b in s.get(1..).unwrap_or(&[]) {
            if (b & 0xc0) != 0x80 {
                break;
            }
            c = (c << 6).wrapping_add(u32::from(b & 0x3f));
        }
        // U+FFFE / U+FFFF read as themselves on the 3.54.1 pin
        // (v0.16 fix, ungated: `unicode(char(65534))` is 65534).
        if c < 0x80 || (c & 0xffff_f800) == 0xd800 {
            c = 0xfffd;
        }
    }
    c
}

/// `unicode(X)`: the code point of X's first character (X as
/// text), NULL for NULL or an empty / NUL-led text.
pub fn unicode(v: &OwnedValue) -> OwnedValue {
    let Some(t) = to_text(v) else {
        return OwnedValue::Null;
    };
    match t.as_slice().first() {
        None | Some(0) => OwnedValue::Null,
        Some(_) => OwnedValue::Integer(i64::from(utf8_read(t.as_slice()))),
    }
}

/// `zeroblob(N)`: N zero bytes (N read as an integer; negative is
/// 0). More than 1,000,000,000 is `SchemaKind::TooBig`.
pub fn zeroblob<E>(v: &OwnedValue) -> Result<OwnedValue, E> {
    let n = usize::try_from(int64_value(v).max(0)).unwrap_or(usize::MAX);
    if n > MAX_LENGTH {
        return Err(too_big());
    }
    let mut out: AVec<u8> = AVec::new();
    out.try_reserve(n).map_err(Error::from)?;
    for _ in 0..n {
        out.push(0);
    }
    Ok(OwnedValue::Blob(out))
}

/// `octet_length(X)`: the byte length of X's text (or blob) form;
/// NULL for NULL.
pub fn octet_length(v: &OwnedValue) -> OwnedValue {
    match v {
        OwnedValue::Null => OwnedValue::Null,
        OwnedValue::Text(t) | OwnedValue::Blob(t) => OwnedValue::Integer(t.len() as i64),
        _ => OwnedValue::Integer(to_text(v).map_or(0, |t| t.len()) as i64),
    }
}

/// `concat(X, ...)`: the text forms of the non-NULL arguments run
/// together (always TEXT, `''` when all are NULL).
/// `concat_ws(SEP, X, ...)`: the same with SEP between them; NULL
/// when SEP is NULL.
pub fn concat<E>(sep: Option<&OwnedValue>, v: &[OwnedValue]) -> Result<OwnedValue, E> {
    let sep = match sep {
        None => None,
        Some(s) => match to_text(s) {
            None => return Ok(OwnedValue::Null),
            Some(t) => Some(t),
        },
    };
    let mut out: AVec<u8> = AVec::new();
    let mut first = true;
    for x in v {
        let Some(t) = to_text(x) else { continue };
        if !first && let Some(s) = &sep {
            put::<E>(&mut out, s.as_slice())?;
        }
        first = false;
        put::<E>(&mut out, t.as_slice())?;
    }
    Ok(OwnedValue::Text(out))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `unhex(X[, Y])`: the blob whose hex digits X spells (upper or
/// lower case, in pairs); characters of Y may stand between the
/// pairs. NULL for a NULL argument or any other character, or an
/// odd digit count.
pub fn unhex(x: &OwnedValue, y: Option<&OwnedValue>) -> OwnedValue {
    let Some(t) = to_text(x) else {
        return OwnedValue::Null;
    };
    let pass: AVec<u8> = match y {
        None => AVec::new(),
        Some(y) => match to_text(y) {
            None => return OwnedValue::Null,
            Some(p) => p,
        },
    };
    let pass = c_str(pass.as_slice());
    let s = c_str(t.as_slice());
    let mut out: AVec<u8> = AVec::new();
    let mut i = 0usize;
    while i < s.len() {
        let Some(&c) = s.get(i) else { break };
        let Some(hi) = hex_val(c) else {
            // A character of Y (a whole UTF-8 character) skips.
            let mut end = i + 1;
            if c >= 0xc0 {
                while s.get(end).is_some_and(|b| (*b & 0xc0) == 0x80) {
                    end += 1;
                }
            }
            let ch = s.get(i..end).unwrap_or(&[]);
            if !contains_char(pass, ch) {
                return OwnedValue::Null;
            }
            i = end;
            continue;
        };
        let Some(lo) = s.get(i + 1).copied().and_then(hex_val) else {
            return OwnedValue::Null;
        };
        out.push((hi << 4) | lo);
        i += 2;
    }
    OwnedValue::Blob(out)
}

/// Whether the UTF-8 character `ch` occurs, as a whole character,
/// in `set`.
fn contains_char(set: &[u8], ch: &[u8]) -> bool {
    let mut i = 0usize;
    while i < set.len() {
        let mut end = i + 1;
        if set.get(i).is_some_and(|b| *b >= 0xc0) {
            while set.get(end).is_some_and(|b| (*b & 0xc0) == 0x80) {
                end += 1;
            }
        }
        if set.get(i..end) == Some(ch) {
            return true;
        }
        i = end;
    }
    false
}

// ---------------------------------------------------------------
// The connection's random stream
// ---------------------------------------------------------------

/// The deterministic generator behind `random()` / `randomblob()`
/// (v0.16): splitmix64 over a per-connection state seeded from
/// `Config::nonce_seed` (the core has no entropy of its own; a
/// target with real entropy seeds that). Not the pin's ChaCha20
/// stream — only the types, lengths and ranges match.
#[derive(Debug)]
pub struct Prng {
    state: core::cell::Cell<u64>,
}

impl Prng {
    pub fn new(seed: u64) -> Prng {
        Prng {
            state: core::cell::Cell::new(seed ^ 0x7261_6e64_6f6d_2121),
        }
    }

    /// The next 64 random bits.
    pub fn next_u64(&self) -> u64 {
        let s = self.state.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.state.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `random()`: a 64-bit integer, a negative draw folded as the
    /// pin folds it (`-(r & i64::MAX)`), so never i64::MIN.
    pub fn random(&self) -> i64 {
        let r = self.next_u64() as i64;
        if r < 0 { -(r & i64::MAX) } else { r }
    }

    /// `randomblob(N)`: N random bytes (N read as an integer; less
    /// than 1 is 1). More than 1,000,000,000 is
    /// `SchemaKind::TooBig`.
    pub fn blob<E>(&self, v: &OwnedValue) -> Result<OwnedValue, E> {
        let n = usize::try_from(int64_value(v).max(1)).unwrap_or(usize::MAX);
        if n > MAX_LENGTH {
            return Err(too_big());
        }
        let mut out: AVec<u8> = AVec::new();
        out.try_reserve(n).map_err(Error::from)?;
        while out.len() < n {
            for b in self.next_u64().to_le_bytes() {
                if out.len() < n {
                    out.push(b);
                }
            }
        }
        Ok(OwnedValue::Blob(out))
    }
}
