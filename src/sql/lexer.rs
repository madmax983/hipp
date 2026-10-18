//! Hand-written lexer for the v0.1 grammar (SPEC §5.6).
//!
//! String/blob literal bytes are copied into the statement arena
//! and handed back as `ABytes`; identifiers resolve against the
//! arena too, so tokens never borrow the input SQL text (a prepared
//! statement outlives the caller's `&str`).

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

/// Parse a decimal floating literal. Deliberately simple: up to 17
/// significant digits are accumulated exactly into a `u64`, then
/// scaled by powers of ten. This is correctly rounded for the
/// values differential tests use (SQLite's own parser is exact;
/// pathological 17-digit mantissas may differ by 1 ulp — a noted
/// v0.1 limitation, RISKS R5 territory).
pub fn parse_real(text: &[u8]) -> Option<f64> {
    let mut mant: u64 = 0;
    let mut sig = 0usize;
    let mut exp10: i64 = 0;
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
        if sig < 17 {
            mant = mant * 10 + u64::from(b - b'0');
            sig += 1;
            if seen_dot {
                exp10 -= 1;
            }
        } else if !seen_dot {
            exp10 += 1;
        }
        i += 1;
    }
    if !seen_digit {
        return None;
    }
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
            e = (e * 10 + i64::from(text[i] - b'0')).min(10_000);
            i += 1;
        }
        if !any {
            return None;
        }
        exp10 += if neg { -e } else { e };
    }
    let mut v = mant as f64;
    // Scale in chunks of 1e16 to bound loop count and error growth.
    let mut e = exp10;
    while e > 0 {
        let step = e.min(16);
        let mut f = 1.0f64;
        for _ in 0..step {
            f *= 10.0;
        }
        v *= f;
        e -= step;
    }
    while e < 0 {
        let step = (-e).min(16);
        let mut f = 1.0f64;
        for _ in 0..step {
            f *= 10.0;
        }
        v /= f;
        e += step;
    }
    Some(v)
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
