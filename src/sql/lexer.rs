//! Hand-written lexer for the v0.1 grammar (SPEC §5.6).
//!
//! String/blob literal bytes are copied into the statement arena
//! and handed back as `ABytes`; identifiers resolve against the
//! arena too, so tokens never borrow the input SQL text (a prepared
//! statement outlives the caller's `&str`).

use alloc::vec::Vec;

use crate::arena::{ABytes, AVec, Arena};
use crate::error::{Error, ParseKind, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok {
    /// Identifier or keyword text (arena handle) + was-quoted flag.
    /// Keywords are recognised by the parser, case-insensitively,
    /// and only when unquoted.
    Ident(ABytes, bool),
    Integer(i64),
    Real(f64),
    Text(ABytes),
    Blob(ABytes),
    LParen,
    RParen,
    Comma,
    Semi,
    Star,
    Minus,
    Plus,
    Dot,
    /// `=` (v0.2: WHERE rowid = <literal>).
    Eq,
    /// `<`, `>`, `<=`, `>=`, `!=`/`<>` (v0.4 expressions).
    Lt,
    Gt,
    Le,
    Ge,
    Ne,
    /// `/` and `%` (v0.4 expressions).
    Slash,
    Percent,
    /// `||` string concatenation (v0.4 expressions).
    Concat,
    Eof,
}

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub tok: Tok,
    pub offset: usize,
}

fn perr<E>(offset: usize, kind: ParseKind) -> Error<E> {
    Error::Parse { offset, kind }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Parse a decimal floating literal the way the pinned
/// SQLite 3.54.1 oracle does (RISKS R5; before v0.12 this
/// accumulated 17 digits into a `u64` and scaled by inexact
/// powers of ten, which diverged from the oracle by as much
/// as ~41 ulp at `1e308`).
///
/// The oracle's conversion is NOT textbook correctly
/// rounded on every decimal, and the difference is observable
/// — so v0.12 reproduces the oracle's algorithm, probed
/// black-box before implementation:
///
/// 1. Significant digits are accumulated into a `u64`. The
///    first digit that would overflow `u64` (checked
///    multiply-and-add) ends the accumulation; that digit
///    and every later digit are dropped, with the decimal
///    exponent increased by one per dropped digit. Digits
///    past the horizon are therefore *ignored entirely* —
///    no rounding, no sticky bit. (Evidence: sig digit 20
///    of the 1.0 midpoint flips the result, sig digit 21
///    does not; the 20-digit integer `18446744073709553665`
///    converts as `...53660`, because accumulating its 20th
///    digit overflows `u64`; `2.4703282292062327209e-324`
///    converts to 0, because its truncated form sits below
///    the subnormal midpoint.)
/// 2. The truncated decimal is then converted exactly,
///    round-to-nearest, ties to even. (Evidence: every
///    short-string exact tie probed — `72057594037927960`,
///    `36028797018963972`, `9007199254740993` — lands on
///    the even neighbour; long-digit exact ties land on
///    the lower neighbour only because their truncated form
///    is strictly below the tie.)
///
/// Method for step 2: the truncated literal is an exact
/// rational `D * 10^exp10` with `D` a big integer. For
/// `exp10 < 0` the value is the fraction `D / 10^-exp10`.
/// The binary exponent comes from bit-length comparison,
/// then the 53-bit (or subnormal) mantissa is one
/// big-integer division whose remainder decides rounding
/// exactly: above half rounds up, below half rounds down,
/// exactly half is a true tie and rounds to the even
/// neighbour. Overflow lands on infinity (a value at or
/// above the midpoint between the largest finite float and
/// 2^1024), underflow on zero. Cost is quadratic in the
/// digit count in the worst case, which is the right trade
/// for a literal parser (literals are short; correctness
/// is the claim).
///
/// The text must be an unsigned decimal literal as lexed from
/// a token (`digits`, `digits.digits`, `.digits`, optional
/// `e`/`E` exponent with optional sign). For text conversions
/// that may carry a sign, see `parse_real_signed`.
pub fn parse_real(text: &[u8]) -> Option<f64> {
    let mut digits: Vec<u8> = Vec::new();
    let mut frac_digits: i64 = 0;
    let mut i = 0usize;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while i < text.len() {
        let b = text[i];
        if b == b'.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
            i += 1;
            continue;
        }
        if b == b'e' || b == b'E' {
            break;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        seen_digit = true;
        digits.push(b - b'0');
        if seen_dot {
            frac_digits += 1;
        }
        i += 1;
    }
    if !seen_digit {
        return None;
    }
    let mut exp10: i64 = -frac_digits;
    if i < text.len() {
        // exponent part
        i += 1;
        let mut neg = false;
        if i < text.len() && (text[i] == b'-' || text[i] == b'+') {
            neg = text[i] == b'-';
            i += 1;
        }
        let mut e: i64 = 0;
        let mut any = false;
        while i < text.len() {
            if !text[i].is_ascii_digit() {
                return None;
            }
            any = true;
            e = (e * 10 + i64::from(text[i] - b'0')).min(1_000_000);
            i += 1;
        }
        if !any {
            return None;
        }
        exp10 += if neg { -e } else { e };
    }
    // Strip leading zeros; an all-zero mantissa is 0.0 whatever
    // the exponent says (the oracle agrees: `0e999` is 0.0).
    let mut start = 0usize;
    while start < digits.len() && digits[start] == 0 {
        start += 1;
    }
    if start == digits.len() {
        return Some(0.0);
    }
    // Strip trailing zeros into the exponent (keeps the big
    // integers small on inputs like `1000...0.0`).
    let mut end = digits.len();
    while digits[end - 1] == 0 {
        end -= 1;
    }
    exp10 += (digits.len() - end) as i64;
    let digits = &digits[start..end];
    // The oracle's digit horizon (step 1 of the module
    // comment): accumulate the significant digits into a
    // `u64`; the first digit that would overflow ends the
    // input, and it and every later digit are dropped with
    // one exponent unit each. The conversion below therefore
    // runs on the *truncated* decimal, exactly as the pin's
    // does.
    let mut acc: u64 = 0;
    let mut kept = 0usize;
    for (idx, &d) in digits.iter().enumerate() {
        match acc
            .checked_mul(10)
            .and_then(|a| a.checked_add(u64::from(d)))
        {
            Some(a) => {
                acc = a;
                kept = idx + 1;
            }
            None => break,
        }
    }
    let dropped = digits.len() - kept;
    if dropped > 0 {
        exp10 += dropped as i64;
    }
    let digits = &digits[..kept];
    // Decimal magnitude: the value lies in [10^(mag-1), 10^mag).
    // Far outside the f64 range the answer needs no arithmetic:
    // >= 10^310 is past the overflow midpoint, and < 10^-324 is
    // below half of the smallest subnormal (2.47e-324).
    let mag = exp10 + digits.len() as i64;
    if mag > 310 {
        return Some(f64::INFINITY);
    }
    if mag < -323 {
        return Some(0.0);
    }
    // Exact rational P / Q equal to the literal.
    let mut p: BigUint = Vec::new();
    for &d in digits {
        big_mul_small(&mut p, 10);
        big_add_small(&mut p, u32::from(d));
    }
    let mut q: BigUint = Vec::new();
    q.push(1);
    if exp10 >= 0 {
        big_mul_pow10(&mut p, exp10 as usize);
    } else {
        big_mul_pow10(&mut q, (-exp10) as usize);
    }
    Some(big_rational_to_f64(&p, &q))
}

/// `parse_real` for text that may carry one leading `+`/`-`
/// (CAST and affinity text-to-number conversions, where the
/// sign is part of the text rather than a unary operator).
pub fn parse_real_signed(text: &[u8]) -> Option<f64> {
    match text.split_first() {
        Some((b'-', rest)) => parse_real(rest).map(|f| -f),
        Some((b'+', rest)) => parse_real(rest),
        _ => parse_real(text),
    }
}

/// A non-negative big integer as little-endian `u32` limbs
/// with no trailing zero limbs; the empty vector is zero.
type BigUint = Vec<u32>;

fn big_trim(v: &mut BigUint) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn big_mul_small(v: &mut BigUint, m: u32) {
    let mut carry = 0u64;
    for limb in v.iter_mut() {
        let cur = u64::from(*limb) * u64::from(m) + carry;
        *limb = cur as u32;
        carry = cur >> 32;
    }
    while carry > 0 {
        v.push(carry as u32);
        carry >>= 32;
    }
}

fn big_add_small(v: &mut BigUint, a: u32) {
    let mut carry = u64::from(a);
    let mut i = 0usize;
    while carry > 0 {
        if i == v.len() {
            v.push(0);
        }
        let cur = u64::from(v[i]) + carry;
        v[i] = cur as u32;
        carry = cur >> 32;
        i += 1;
    }
}

fn big_mul_pow10(v: &mut BigUint, mut e: usize) {
    while e >= 9 {
        big_mul_small(v, 1_000_000_000);
        e -= 9;
    }
    if e > 0 {
        big_mul_small(v, 10u32.pow(e as u32));
    }
}

fn big_bit_len(v: &BigUint) -> usize {
    match v.last() {
        None => 0,
        Some(&top) => (v.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
    }
}

fn big_cmp(a: &BigUint, b: &BigUint) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn big_shl(v: &BigUint, shift: usize) -> BigUint {
    let mut out: BigUint = Vec::new();
    if v.is_empty() {
        return out;
    }
    let limbs = shift / 32;
    let bits = shift % 32;
    out = alloc::vec![0; limbs];
    if bits == 0 {
        out.extend_from_slice(v);
    } else {
        let mut carry = 0u32;
        for &x in v.iter() {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
        if carry > 0 {
            out.push(carry);
        }
    }
    out
}

/// `a -= b`; requires `a >= b`.
fn big_sub(a: &mut BigUint, b: &BigUint) {
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let bi = if i < b.len() { i64::from(b[i]) } else { 0 };
        let cur = i64::from(a[i]) - bi - borrow;
        if cur < 0 {
            a[i] = (cur + (1i64 << 32)) as u32;
            borrow = 1;
        } else {
            a[i] = cur as u32;
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0);
    big_trim(a);
}

/// Floor division with remainder, by binary long division.
/// In this module the quotient is always mantissa-sized
/// (~53 bits), so the loop runs ~54 compare/subtract rounds.
fn big_div_rem(num: &BigUint, den: &BigUint) -> (BigUint, BigUint) {
    use core::cmp::Ordering;
    if big_cmp(num, den) == Ordering::Less {
        return (Vec::new(), num.clone());
    }
    let shift = big_bit_len(num) - big_bit_len(den);
    let mut quot: BigUint = alloc::vec![0; shift / 32 + 1];
    let mut rem = num.clone();
    for i in (0..=shift).rev() {
        let ds = big_shl(den, i);
        if big_cmp(&rem, &ds) != Ordering::Less {
            big_sub(&mut rem, &ds);
            quot[i / 32] |= 1u32 << (i % 32);
        }
    }
    big_trim(&mut quot);
    (quot, rem)
}

/// `round(num / den)` to the nearest integer, ties to even,
/// returned as a `u64` (the caller guarantees it fits).
fn big_rounded_div(num: &BigUint, den: &BigUint) -> u64 {
    use core::cmp::Ordering;
    let (quot, rem) = big_div_rem(num, den);
    let mut m = u64::from(quot.first().copied().unwrap_or(0))
        | (u64::from(quot.get(1).copied().unwrap_or(0)) << 32);
    debug_assert!(quot.len() <= 2);
    let twice = big_shl(&rem, 1);
    match big_cmp(&twice, den) {
        Ordering::Greater => m += 1,
        Ordering::Equal => {
            if m % 2 == 1 {
                m += 1;
            }
        }
        Ordering::Less => {}
    }
    m
}

/// The f64 nearest to the exact rational `p / q` (`p > 0`,
/// `q > 0`), ties to even, saturating to infinity / zero at
/// the range ends exactly as IEEE rounding prescribes.
fn big_rational_to_f64(p: &BigUint, q: &BigUint) -> f64 {
    use core::cmp::Ordering;
    // Binary exponent: value lies in [2^bin_exp, 2^(bin_exp+1)).
    let cand = big_bit_len(p) as i64 - big_bit_len(q) as i64;
    let ge = if cand >= 0 {
        big_cmp(p, &big_shl(q, cand as usize)) != Ordering::Less
    } else {
        big_cmp(&big_shl(p, (-cand) as usize), q) != Ordering::Less
    };
    let bin_exp = if ge { cand } else { cand - 1 };
    if bin_exp > 1023 {
        return f64::INFINITY;
    }
    if bin_exp < -1075 {
        // Strictly below half of the smallest subnormal.
        return 0.0;
    }
    if bin_exp >= -1022 {
        // Normal: mantissa = round(value * 2^(52 - bin_exp)).
        let shift = 52 - bin_exp;
        let (num, den) = if shift >= 0 {
            (big_shl(p, shift as usize), q.clone())
        } else {
            (p.clone(), big_shl(q, (-shift) as usize))
        };
        let mut m = big_rounded_div(&num, &den);
        let mut e = bin_exp;
        if m == (1u64 << 53) {
            // Rounding carried out of the mantissa (this is
            // also the overflow-midpoint case at e == 1023).
            m >>= 1;
            e += 1;
            if e > 1023 {
                return f64::INFINITY;
            }
        }
        let bits = (((e + 1023) as u64) << 52) | (m - (1u64 << 52));
        f64::from_bits(bits)
    } else {
        // Subnormal (or zero): value = m * 2^-1074 with
        // m <= 2^52; m == 2^52 is the smallest normal, whose
        // bit pattern is exactly m, so one from_bits serves.
        let num = big_shl(p, 1074);
        let m = big_rounded_div(&num, q);
        f64::from_bits(m)
    }
}

/// An integer literal that overflowed i64 becomes a real (SQLite's
/// rule). Returns Integer when it fits.
fn number_token<E>(text: &[u8], offset: usize) -> core::result::Result<Tok, Error<E>> {
    let is_float = text.iter().any(|b| matches!(b, b'.' | b'e' | b'E'));
    if !is_float {
        let mut v: i64 = 0;
        let mut overflow = false;
        for b in text {
            let d = i64::from(b - b'0');
            match v.checked_mul(10).and_then(|x| x.checked_add(d)) {
                Some(x) => v = x,
                None => {
                    overflow = true;
                    break;
                }
            }
        }
        if !overflow {
            return Ok(Tok::Integer(v));
        }
    }
    match parse_real(text) {
        Some(f) => Ok(Tok::Real(f)),
        None => Err(perr(offset, ParseKind::InvalidLiteral)),
    }
}

pub fn lex<E>(sql: &[u8], arena: &Arena) -> Result<AVec<Token>, E> {
    let mut toks = AVec::new();
    let mut i = 0usize;
    let n = sql.len();
    while i < n {
        let b = sql[i];
        let off = i;
        match b {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => {
                i += 1;
            }
            b'-' if i + 1 < n && sql[i + 1] == b'-' => {
                while i < n && sql[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                toks.push(Token {
                    tok: Tok::LParen,
                    offset: off,
                });
                i += 1;
            }
            b')' => {
                toks.push(Token {
                    tok: Tok::RParen,
                    offset: off,
                });
                i += 1;
            }
            b',' => {
                toks.push(Token {
                    tok: Tok::Comma,
                    offset: off,
                });
                i += 1;
            }
            b';' => {
                toks.push(Token {
                    tok: Tok::Semi,
                    offset: off,
                });
                i += 1;
            }
            b'*' => {
                toks.push(Token {
                    tok: Tok::Star,
                    offset: off,
                });
                i += 1;
            }
            b'=' => {
                toks.push(Token {
                    tok: Tok::Eq,
                    offset: off,
                });
                i += 1;
            }
            b'<' => {
                if i + 1 < n && sql[i + 1] == b'=' {
                    toks.push(Token {
                        tok: Tok::Le,
                        offset: off,
                    });
                    i += 2;
                } else if i + 1 < n && sql[i + 1] == b'>' {
                    toks.push(Token {
                        tok: Tok::Ne,
                        offset: off,
                    });
                    i += 2;
                } else {
                    toks.push(Token {
                        tok: Tok::Lt,
                        offset: off,
                    });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < n && sql[i + 1] == b'=' {
                    toks.push(Token {
                        tok: Tok::Ge,
                        offset: off,
                    });
                    i += 2;
                } else {
                    toks.push(Token {
                        tok: Tok::Gt,
                        offset: off,
                    });
                    i += 1;
                }
            }
            b'!' if i + 1 < n && sql[i + 1] == b'=' => {
                toks.push(Token {
                    tok: Tok::Ne,
                    offset: off,
                });
                i += 2;
            }
            b'/' => {
                if i + 1 < n && sql[i + 1] == b'*' {
                    // C-style comment (SQLite accepts these).
                    i += 2;
                    while i + 1 < n && !(sql[i] == b'*' && sql[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                } else {
                    toks.push(Token {
                        tok: Tok::Slash,
                        offset: off,
                    });
                    i += 1;
                }
            }
            b'%' => {
                toks.push(Token {
                    tok: Tok::Percent,
                    offset: off,
                });
                i += 1;
            }
            b'|' if i + 1 < n && sql[i + 1] == b'|' => {
                toks.push(Token {
                    tok: Tok::Concat,
                    offset: off,
                });
                i += 2;
            }
            b'-' => {
                toks.push(Token {
                    tok: Tok::Minus,
                    offset: off,
                });
                i += 1;
            }
            b'+' => {
                toks.push(Token {
                    tok: Tok::Plus,
                    offset: off,
                });
                i += 1;
            }
            b'.' if !(i + 1 < n && sql[i + 1].is_ascii_digit()) => {
                toks.push(Token {
                    tok: Tok::Dot,
                    offset: off,
                });
                i += 1;
            }
            b'\'' => {
                // String literal with '' escaping.
                i += 1;
                let mut buf = AVec::new();
                loop {
                    if i >= n {
                        return Err(perr(off, ParseKind::UnexpectedEof));
                    }
                    if sql[i] == b'\'' {
                        if i + 1 < n && sql[i + 1] == b'\'' {
                            buf.push(b'\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        buf.push(sql[i]);
                        i += 1;
                    }
                }
                let h = arena.alloc_copy_handle(buf.as_slice())?;
                toks.push(Token {
                    tok: Tok::Text(h),
                    offset: off,
                });
            }
            b'"' | b'`' => {
                let close = b;
                i += 1;
                let mut buf = AVec::new();
                loop {
                    if i >= n {
                        return Err(perr(off, ParseKind::UnexpectedEof));
                    }
                    if sql[i] == close {
                        if i + 1 < n && sql[i + 1] == close {
                            buf.push(close);
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        buf.push(sql[i]);
                        i += 1;
                    }
                }
                let h = arena.alloc_copy_handle(buf.as_slice())?;
                toks.push(Token {
                    tok: Tok::Ident(h, true),
                    offset: off,
                });
            }
            b'[' => {
                i += 1;
                let start = i;
                while i < n && sql[i] != b']' {
                    i += 1;
                }
                if i >= n {
                    return Err(perr(off, ParseKind::UnexpectedEof));
                }
                let h = arena.alloc_copy_handle(&sql[start..i])?;
                i += 1;
                toks.push(Token {
                    tok: Tok::Ident(h, true),
                    offset: off,
                });
            }
            b'x' | b'X' if i + 1 < n && sql[i + 1] == b'\'' => {
                i += 2;
                let start = i;
                while i < n && sql[i] != b'\'' {
                    i += 1;
                }
                if i >= n {
                    return Err(perr(off, ParseKind::UnexpectedEof));
                }
                let hex = &sql[start..i];
                if !hex.len().is_multiple_of(2) {
                    return Err(perr(off, ParseKind::InvalidLiteral));
                }
                let mut buf = AVec::new();
                let mut j = 0;
                while j < hex.len() {
                    let hi = hex_val(hex[j]);
                    let lo = hex_val(hex[j + 1]);
                    let (Some(hi), Some(lo)) = (hi, lo) else {
                        return Err(perr(off, ParseKind::InvalidLiteral));
                    };
                    buf.push(hi * 16 + lo);
                    j += 2;
                }
                i += 1;
                let h = arena.alloc_copy_handle(buf.as_slice())?;
                toks.push(Token {
                    tok: Tok::Blob(h),
                    offset: off,
                });
            }
            b'0'..=b'9' | b'.' => {
                // Number (b'.' only reaches here before a digit).
                if b == b'0' && i + 1 < n && (sql[i + 1] == b'x' || sql[i + 1] == b'X') {
                    i += 2;
                    let mut v: i64 = 0;
                    let mut any = false;
                    while i < n && hex_val(sql[i]).is_some() {
                        any = true;
                        let d = i64::from(hex_val(sql[i]).expect("checked"));
                        v = v
                            .checked_mul(16)
                            .and_then(|x| x.checked_add(d))
                            .ok_or(perr(off, ParseKind::InvalidLiteral))?;
                        i += 1;
                    }
                    if !any {
                        return Err(perr(off, ParseKind::InvalidLiteral));
                    }
                    toks.push(Token {
                        tok: Tok::Integer(v),
                        offset: off,
                    });
                    continue;
                }
                let start = i;
                let mut seen_dot = false;
                while i < n {
                    let c = sql[i];
                    if c.is_ascii_digit() {
                        i += 1;
                    } else if c == b'.' && !seen_dot {
                        seen_dot = true;
                        i += 1;
                    } else if c == b'e' || c == b'E' {
                        i += 1;
                        if i < n && (sql[i] == b'-' || sql[i] == b'+') {
                            i += 1;
                        }
                        while i < n && sql[i].is_ascii_digit() {
                            i += 1;
                        }
                        break;
                    } else {
                        break;
                    }
                }
                let tok = number_token::<E>(&sql[start..i], off)?;
                toks.push(Token { tok, offset: off });
            }
            _ if is_ident_start(b) => {
                let start = i;
                while i < n && is_ident_char(sql[i]) {
                    i += 1;
                }
                let h = arena.alloc_copy_handle(&sql[start..i])?;
                toks.push(Token {
                    tok: Tok::Ident(h, false),
                    offset: off,
                });
            }
            _ => return Err(perr(off, ParseKind::UnexpectedToken)),
        }
    }
    toks.push(Token {
        tok: Tok::Eof,
        offset: n,
    });
    Ok(toks)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
