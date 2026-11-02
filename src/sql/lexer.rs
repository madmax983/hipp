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
    /// A bound-parameter token (v0.16, feature `params`): `?`,
    /// `?NNN`, `:name`, `@name`, `$name` or `#name`, with the
    /// 1-based index the pin's `sqlite3ExprAssignVarNumber`
    /// gives it (see `ParamTable`) and the token's own text.
    #[cfg(feature = "params")]
    Param(u16, ABytes),
    /// The bitwise operators `&`, `|`, `<<`, `>>` and `~`
    /// (v0.16, `v16`).
    #[cfg(feature = "v16")]
    BitAnd,
    #[cfg(feature = "v16")]
    BitOr,
    #[cfg(feature = "v16")]
    Shl,
    #[cfg(feature = "v16")]
    Shr,
    #[cfg(feature = "v16")]
    BitNot,
    Eof,
}

/// The largest parameter index (v0.16, `params`): the pin's
/// `SQLITE_MAX_VARIABLE_NUMBER` default, 32766.
#[cfg(feature = "params")]
pub const MAX_PARAMS: u16 = 32766;

/// A statement's parameter numbering (v0.16, `params`) — the
/// pin's `Parse.nVar` and `pVList`, built in source order as
/// the lexer meets each parameter token: `?` takes the next
/// index (one past the largest so far); `?NNN` takes NNN (and
/// names that index `?NNN` unless it already has a name); a
/// named parameter (`:a`, `@a`, `$a`, `#a`) reuses the index of
/// an earlier token with exactly the same text, else takes the
/// next index.
#[cfg(feature = "params")]
#[derive(Debug, Clone, Default)]
pub struct ParamTable {
    /// The largest index assigned (`sqlite3_bind_parameter_count`).
    pub count: u16,
    /// `(name, index)` in registration order; the first entry
    /// for an index is that index's name.
    pub names: AVec<(ABytes, u16)>,
}

#[cfg(feature = "params")]
impl ParamTable {
    /// The index of the parameter spelled exactly `name`.
    pub fn index_of(&self, arena: &Arena, name: &[u8]) -> Option<u16> {
        self.names
            .iter()
            .find(|(h, _)| arena.slice_at(h.pos as usize, h.len as usize) == Some(name))
            .map(|(_, i)| *i)
    }

    /// The name of index `i` (`None` for a nameless `?`).
    pub fn name_of(&self, i: u16) -> Option<ABytes> {
        self.names.iter().find(|(_, x)| *x == i).map(|(h, _)| *h)
    }

    /// Assign the index of one parameter token `z` (the pin's
    /// `sqlite3ExprAssignVarNumber`).
    fn assign<E>(&mut self, arena: &Arena, z: &[u8], h: ABytes, off: usize) -> Result<u16, E> {
        let next = |c: u16| {
            c.checked_add(1)
                .filter(|n| *n <= MAX_PARAMS)
                .ok_or(perr(off, ParseKind::TooManyVariables))
        };
        if z.len() == 1 {
            self.count = next(self.count)?;
            return Ok(self.count);
        }
        if z.first() == Some(&b'?') {
            let mut v: u16 = 0;
            for d in z.iter().skip(1) {
                v = v
                    .checked_mul(10)
                    .and_then(|x| x.checked_add(u16::from(d.wrapping_sub(b'0'))))
                    .filter(|x| *x <= MAX_PARAMS)
                    .ok_or(perr(off, ParseKind::VariableNumber))?;
            }
            if v == 0 {
                return Err(perr(off, ParseKind::VariableNumber));
            }
            if v > self.count {
                self.count = v;
                self.names.push((h, v));
            } else if self.name_of(v).is_none() {
                self.names.push((h, v));
            }
            return Ok(v);
        }
        if let Some(x) = self.index_of(arena, z) {
            return Ok(x);
        }
        self.count = next(self.count)?;
        self.names.push((h, self.count));
        Ok(self.count)
    }
}

/// The length of the parameter token at the start of `z`
/// (v0.16, `params`; the pin's tokenizer): `?` and its digits;
/// or `$ @ : #` and identifier characters, with the TCL forms
/// `::` and a trailing `(...)` group. `None` is the pin's
/// "unrecognized token" (no name characters, or an unclosed
/// group).
#[cfg(feature = "params")]
fn param_len(z: &[u8]) -> Option<usize> {
    if z.first() == Some(&b'?') {
        return Some(1 + z.iter().skip(1).take_while(|c| c.is_ascii_digit()).count());
    }
    let mut i = 1usize;
    let mut n = 0usize;
    while let Some(&c) = z.get(i) {
        if is_ident_char(c) {
            n += 1;
        } else if c == b'(' && n > 0 {
            loop {
                i += 1;
                match z.get(i) {
                    Some(b')') => return Some(i + 1),
                    Some(c) if *c != 0 && !c.is_ascii_whitespace() && *c != 0x0b => {}
                    _ => return None,
                }
            }
        } else if c == b':' && z.get(i + 1) == Some(&b':') {
            i += 1;
        } else {
            break;
        }
        i += 1;
    }
    (n > 0).then_some(i)
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
    Some(int_pow10_to_f64(acc, exp10))
}

/// The f64 nearest to `c * 10^x` (`c > 0`), ties to even — the
/// conversion `parse_real` ends in (`acc` holds the kept digits),
/// shared with `record::render_real`'s read-back test (v0.16).
pub fn int_pow10_to_f64(c: u64, x: i64) -> f64 {
    // Decimal magnitude: the value lies in [10^(mag-1), 10^mag).
    // Far outside the f64 range the answer needs no arithmetic:
    // >= 10^310 is past the overflow midpoint, and < 10^-324 is
    // below half of the smallest subnormal (2.47e-324).
    let mut mag = x;
    let mut t = c;
    while t > 0 {
        mag += 1;
        t /= 10;
    }
    if mag > 310 {
        return f64::INFINITY;
    }
    if mag < -323 {
        return 0.0;
    }
    // Exact rational P / Q equal to the literal.
    let mut p: BigUint = alloc::vec![c as u32, (c >> 32) as u32];
    big_trim(&mut p);
    let mut q: BigUint = alloc::vec![1];
    if x >= 0 {
        big_mul_pow10(&mut p, x as usize);
    } else {
        big_mul_pow10(&mut q, x.unsigned_abs() as usize);
    }
    big_rational_to_f64(&p, &q)
}

/// The pin's internal decimal digits of the finite `r > 0`
/// (v0.16, for `record::render_real`), as `(d, n)` with `d`, an
/// 18- or 19-digit integer, about `r * 10^n`. Fitted clean-room
/// to the pin's observable output (`printf('%!.26g', r)` shows
/// `d` with its trailing zeros dropped; matched on 120 000 random
/// doubles, then through `render_real` on millions): with
/// `r = m * 2^e` (53-bit `m`) and `p = floor(log10 2^(e+52))`,
/// `n = 17 - p` and `d = round_half_up(r * T)` where `T` is `10^n`
/// truncated to a 64-bit mantissa (exact for `0 <= n <= 27`, a
/// little low otherwise). Not correctly rounded: the truncation
/// shows in the last digit.
pub fn pin_digits(r: f64) -> (u64, i32) {
    let bits = r.to_bits();
    let be = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if be == 0 {
        // Subnormal: normalise so bit 52 is set.
        let sh = frac.leading_zeros().saturating_sub(11);
        (frac << sh, -1074 - sh as i32)
    } else {
        (frac | (1u64 << 52), be - 1075)
    };
    // floor(x * log10 2), exact for |x| < 2^16 (no float math).
    let n = 17 - (((e + 52) * 78913) >> 18);
    let (t, s) = pow10_mantissa(n);
    // d = m * t * 2^(e + s), rounded half up. The product is
    // below 2^117 and the shift between 54 and 61, so the low 53
    // bits only ever fall away: shift them off first and round in
    // 64 bits (no variable-width 128-bit shift).
    let y = ((u128::from(m) * u128::from(t)) >> 53) as u64;
    let sh = (-(e + s)).clamp(54, 116) as u32 - 53;
    let d = (y >> (sh - 1).min(63)).saturating_add(1) >> 1;
    (d, n)
}

/// `10^n` as `(t, s)`: `t` in `[2^63, 2^64)`, the largest with
/// `t * 2^s <= 10^n` (a truncated 64-bit mantissa).
fn pow10_mantissa(n: i32) -> (u64, i32) {
    let mut p: BigUint = alloc::vec![1];
    big_mul_pow10(&mut p, n.unsigned_abs() as usize);
    let bl = big_bit_len(&p) as i32;
    let one: BigUint = alloc::vec![1];
    // 10^n = (a / b) * 2^s with a / b in [2^63, 2^64): for n >= 0
    // a = 10^n * 2^64, b = 2^bl; else a = 2^(bl+63), b = 10^-n.
    let neg = n < 0;
    let (top, bot) = if neg { (&one, &p) } else { (&p, &one) };
    let a = big_shl(top, if neg { bl + 63 } else { 64 } as usize);
    let b = big_shl(bot, if neg { 0 } else { bl } as usize);
    let s = if neg { -(bl + 63) } else { bl - 64 };
    let (q, _) = big_div_rem(&a, &b);
    let limb = |i: usize| u64::from(q.get(i).copied().unwrap_or(0));
    (limb(0) | (limb(1) << 32), s)
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
/// (~53 to 64 bits), so the loop runs at most ~65
/// compare/subtract rounds.
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

/// Lex one numeric literal at the start of `z` (the pin's
/// tokenizer, `CC_DIGIT`): returns the token and its length.
///
/// - `0x` / `0X` followed by a hex digit is a hexadecimal
///   integer: leading zeros are skipped and up to 16
///   significant digits are read as a 64-bit two's-complement
///   value (`0xffffffffffffffff` is -1); more is the pin's
///   "hex literal too big" (v0.16 fix: hipp refused anything
///   past `0x7fffffffffffffff`).
/// - Otherwise digits, an optional `.` fraction and an
///   optional exponent (`e` / `E`, optional sign, at least one
///   digit) — `1e` is not an exponent.
/// - With the evaluator (v0.16 fix; `v04`): a `_` digit
///   separator is allowed between two digits (hex digits in a
///   hex literal), as in SQLite 3.46+, and a literal followed
///   directly by an identifier character is the pin's
///   "unrecognized token" (`1_000` used to lex as `1` aliased
///   `_000`; `1a` as `1` aliased `a`). The tiny profile has
///   no position where either reading is accepted, so it keeps
///   the shorter scan.
#[cfg(feature = "v04")]
fn number_at<E>(z: &[u8], off: usize) -> core::result::Result<(Tok, usize), Error<E>> {
    let at = |k: usize| z.get(k).copied().unwrap_or(0);
    let sep = |c: u8| cfg!(feature = "v04") && c == b'_';
    let mut i;
    let hex = at(0) == b'0' && matches!(at(1), b'x' | b'X') && hex_val(at(2)).is_some();
    if hex {
        i = 2;
        while hex_val(at(i)).is_some() || sep(at(i)) {
            i += 1;
        }
    } else {
        i = 0;
        while at(i).is_ascii_digit() || sep(at(i)) {
            i += 1;
        }
        if at(i) == b'.' {
            i += 1;
            while at(i).is_ascii_digit() || sep(at(i)) {
                i += 1;
            }
        }
        if matches!(at(i), b'e' | b'E')
            && (at(i + 1).is_ascii_digit()
                || (matches!(at(i + 1), b'+' | b'-') && at(i + 2).is_ascii_digit()))
        {
            i += 2;
            while at(i).is_ascii_digit() || sep(at(i)) {
                i += 1;
            }
        }
    }
    let text = z.get(..i).unwrap_or(&[]);
    #[cfg(feature = "v04")]
    {
        if is_ident_char(at(i)) {
            return Err(perr(off, ParseKind::InvalidLiteral));
        }
        if text.contains(&b'_') {
            // Every separator sits between two digits (hex
            // digits in a hex literal); the digits are then read
            // without them.
            let mut buf: AVec<u8> = AVec::new();
            for (k, &c) in text.iter().enumerate() {
                if c != b'_' {
                    buf.push(c);
                    continue;
                }
                let ok = |d: u8| {
                    if hex {
                        hex_val(d).is_some()
                    } else {
                        d.is_ascii_digit()
                    }
                };
                let prev = k.checked_sub(1).map_or(0, at);
                if !ok(prev) || !ok(at(k + 1)) {
                    return Err(perr(off, ParseKind::InvalidLiteral));
                }
            }
            let tok = if hex {
                hex_token::<E>(buf.as_slice(), off)?
            } else {
                number_token::<E>(buf.as_slice(), off)?
            };
            return Ok((tok, i));
        }
    }
    let tok = if hex {
        hex_token::<E>(text, off)?
    } else {
        number_token::<E>(text, off)?
    };
    Ok((tok, i))
}

/// The value of a hexadecimal literal `0x<digits>` (v0.16): the
/// pin's `sqlite3DecOrHexToI64` — leading zeros skipped, at most
/// 16 significant digits, read as two's complement.
#[cfg(feature = "v04")]
fn hex_token<E>(text: &[u8], off: usize) -> core::result::Result<Tok, Error<E>> {
    let mut v: u64 = 0;
    for &c in text.get(2..).unwrap_or(&[]) {
        // A 17th significant digit overflows: "hex literal too
        // big" (leading zeros never do).
        if v >> 60 != 0 {
            return Err(perr(off, ParseKind::HexLiteralTooBig));
        }
        v = (v << 4) | u64::from(hex_val(c).unwrap_or(0));
    }
    Ok(Tok::Integer(v as i64))
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
    #[cfg(feature = "params")]
    {
        lex_params::<E>(sql, arena, &mut ParamTable::default())
    }
    #[cfg(not(feature = "params"))]
    {
        lex_inner::<E>(sql, arena)
    }
}

/// `lex`, numbering parameter tokens into `params` (v0.16,
/// feature `params`).
#[cfg(feature = "params")]
pub fn lex_params<E>(sql: &[u8], arena: &Arena, params: &mut ParamTable) -> Result<AVec<Token>, E> {
    lex_inner::<E>(sql, arena, params)
}

fn lex_inner<E>(
    sql: &[u8],
    arena: &Arena,
    #[cfg(feature = "params")] params: &mut ParamTable,
) -> Result<AVec<Token>, E> {
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
            #[cfg(feature = "v16")]
            b'<' | b'>' if sql.get(i + 1) == Some(&b) => {
                toks.push(Token {
                    tok: if b == b'<' { Tok::Shl } else { Tok::Shr },
                    offset: off,
                });
                i += 2;
            }
            #[cfg(feature = "v16")]
            b'&' | b'~' | b'|' if !(b == b'|' && sql.get(i + 1) == Some(&b'|')) => {
                toks.push(Token {
                    tok: match b {
                        b'&' => Tok::BitAnd,
                        b'|' => Tok::BitOr,
                        _ => Tok::BitNot,
                    },
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
            // The tiny profile's number scan: hex up to
            // 0x7fffffffffffffff, refused beyond (the evaluator's
            // `number_at` reads all 16 digits, digit separators
            // and the pin's trailing-character rule).
            #[cfg(not(feature = "v04"))]
            b'0'..=b'9' | b'.' => {
                // Number (b'.' only reaches here before a digit).
                if b == b'0' && i + 1 < n && (sql[i + 1] == b'x' || sql[i + 1] == b'X') {
                    i += 2;
                    let mut v: i64 = 0;
                    let mut any = false;
                    while i < n && hex_val(sql[i]).is_some() {
                        any = true;
                        let d = i64::from(hex_val(sql[i]).unwrap_or(0));
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
            #[cfg(feature = "v04")]
            b'0'..=b'9' | b'.' => {
                // Number (b'.' only reaches here before a digit).
                let (tok, len) = number_at::<E>(sql.get(i..).unwrap_or(&[]), off)?;
                toks.push(Token { tok, offset: off });
                i += len;
            }
            #[cfg(feature = "params")]
            b'?' | b':' | b'@' | b'$' | b'#' => {
                let rest = sql.get(i..).unwrap_or(&[]);
                let len = param_len(rest).ok_or(perr(off, ParseKind::UnexpectedToken))?;
                let z = rest.get(..len).unwrap_or(&[]);
                // `#` and a digit is the pin's register syntax,
                // a syntax error outside its nested parses.
                if b == b'#' && z.get(1).is_some_and(u8::is_ascii_digit) {
                    return Err(perr(off, ParseKind::UnexpectedToken));
                }
                let h = arena.alloc_copy_handle(z)?;
                let idx = params.assign::<E>(arena, z, h, off)?;
                toks.push(Token {
                    tok: Tok::Param(idx, h),
                    offset: off,
                });
                i += len;
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
