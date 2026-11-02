//! Varints, the record format, values, comparison, and affinity
//! (SPEC §5.5). All decoders are check-first: a `.db` file is
//! untrusted input (RISKS R8) — no length is ever trusted before it is
//! checked against the bytes actually present.

use crate::arena::AVec;
use crate::error::{Error, Result};

/// A decoded value. Text and blob are byte slices — hipp carries text
/// as bytes end-to-end (SPEC §5.5); UTF-8 interpretation happens only
/// at API boundaries that ask for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

/// An owned value (the one place heap vectors appear at the API
/// surface, on request — SPEC §5.5). Used by the executor as the
/// materialised form of literals before affinity + encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(AVec<u8>),
    Blob(AVec<u8>),
}

impl OwnedValue {
    pub fn as_value(&self) -> Value<'_> {
        match self {
            OwnedValue::Null => Value::Null,
            OwnedValue::Integer(i) => Value::Integer(*i),
            OwnedValue::Real(r) => Value::Real(*r),
            OwnedValue::Text(t) => Value::Text(t.as_slice()),
            OwnedValue::Blob(b) => Value::Blob(b.as_slice()),
        }
    }
    pub fn from_value(v: Value<'_>) -> OwnedValue {
        match v {
            Value::Null => OwnedValue::Null,
            Value::Integer(i) => OwnedValue::Integer(i),
            Value::Real(r) => OwnedValue::Real(r),
            Value::Text(t) => {
                let mut a = AVec::new();
                for b in t {
                    a.push(*b);
                }
                OwnedValue::Text(a)
            }
            Value::Blob(t) => {
                let mut a = AVec::new();
                for b in t {
                    a.push(*b);
                }
                OwnedValue::Blob(a)
            }
        }
    }
}

// ---------- varints (SPEC §5.4 / SQFMT §1.6) ----------
// 1–9 bytes: high bit = continuation for the first eight; the ninth
// byte contributes all eight bits.

pub fn decode_varint<E>(buf: &[u8]) -> Result<(u64, usize), E> {
    if buf.is_empty() {
        return Err(Error::BadVarint);
    }
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = *buf.get(i).ok_or(Error::BadVarint)?;
        v = (v << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    // Ninth byte: all 8 bits.
    let b = *buf.get(8).ok_or(Error::BadVarint)?;
    v = (v << 8) | u64::from(b);
    Ok((v, 9))
}

pub fn encode_varint(mut v: u64, out: &mut AVec<u8>) {
    if v <= 0x7f {
        out.push(v as u8);
        return;
    }
    // Collect 7-bit groups; the 9-byte form needs special handling:
    // when v needs more than 56 bits, the first eight bytes carry 7
    // bits each of the high 56 and the ninth byte carries the low 8.
    let mut tmp = [0u8; 9];
    if v > 0x00ff_ffff_ffff_ffff {
        tmp[8] = (v & 0xff) as u8;
        v >>= 8;
        for i in (0..8).rev() {
            tmp[i] = ((v & 0x7f) as u8) | 0x80;
            v >>= 7;
        }
        for b in tmp {
            out.push(b);
        }
        return;
    }
    let mut n = 0;
    let mut x = v;
    while x > 0 {
        tmp[8 - n] = (x & 0x7f) as u8;
        x >>= 7;
        n += 1;
    }
    for i in 0..n {
        let mut b = tmp[9 - n + i];
        if i + 1 < n {
            b |= 0x80;
        }
        out.push(b);
    }
}

// ---------- serial types & records (SQFMT §2.1) ----------

pub fn serial_body_len<E>(serial: u64) -> Result<usize, E> {
    Ok(match serial {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return Err(Error::BadSerialType),
        n if n >= 12 && n % 2 == 0 => ((n - 12) / 2) as usize,
        n => ((n - 13) / 2) as usize,
    })
}

fn int_serial(v: i64) -> u64 {
    match v {
        0 => 8,
        1 => 9,
        -128..=127 => 1,
        -32768..=32767 => 2,
        -8_388_608..=8_388_607 => 3,
        -2_147_483_648..=2_147_483_647 => 4,
        -140_737_488_355_328..=140_737_488_355_327 => 5,
        _ => 6,
    }
}

/// Encode a record: header (varint header-length incl. itself, then
/// one varint serial type per column) followed by the body.
pub fn encode_record(values: &[Value<'_>], out: &mut AVec<u8>) {
    let mut serials = AVec::new();
    for v in values {
        serials.push(match v {
            Value::Null => 0,
            Value::Integer(i) => int_serial(*i),
            Value::Real(_) => 7,
            Value::Text(t) => 13 + 2 * t.len() as u64,
            Value::Blob(b) => 12 + 2 * b.len() as u64,
        });
    }
    // Header length includes its own varint — iterate to a fixpoint.
    let body_len: usize = serials.iter().map(|s| varint_size(*s)).sum();
    let mut hlen = body_len + 1;
    loop {
        let need = body_len + varint_size(hlen as u64);
        if need == hlen {
            break;
        }
        hlen = need;
    }
    encode_varint(hlen as u64, out);
    for s in serials.iter() {
        encode_varint(*s, out);
    }
    // Body.
    for v in values {
        match v {
            Value::Null => {}
            Value::Integer(i) => {
                let serial = int_serial(*i);
                let nbytes = serial_body_len::<()>(serial).unwrap_or(0);
                let bits = *i as u64;
                for k in 0..nbytes {
                    let shift = 8 * (nbytes - 1 - k);
                    out.push((bits >> shift) as u8);
                }
            }
            Value::Real(r) => {
                for b in r.to_be_bytes() {
                    out.push(b);
                }
            }
            Value::Text(t) => {
                for b in t.iter() {
                    out.push(*b);
                }
            }
            Value::Blob(b) => {
                for x in b.iter() {
                    out.push(*x);
                }
            }
        }
    }
}

fn varint_size(v: u64) -> usize {
    let mut n = 1;
    let mut x = v >> 7;
    while x > 0 {
        n += 1;
        x >>= 7;
    }
    if v > 0x00ff_ffff_ffff_ffff {
        return 9;
    }
    n
}

/// Decode a record payload into values borrowing the payload bytes.
/// At most `max_cols` serial types are read; a record with fewer
/// values than the schema is legal (caller materialises defaults).
pub fn decode_record<'a, E>(payload: &'a [u8], max_cols: usize) -> Result<AVec<Value<'a>>, E> {
    let mut out = AVec::new();
    decode_record_into(payload, max_cols, &mut out)?;
    Ok(out)
}

/// `decode_record` into a caller-owned, reusable output vector —
/// the steady-state scan path allocates nothing once the vector has
/// reached its working capacity (SPEC §4.1).
pub fn decode_record_into<'a, E>(
    payload: &'a [u8],
    max_cols: usize,
    out: &mut AVec<Value<'a>>,
) -> Result<(), E> {
    out.clear();
    let (hlen, n) = decode_varint::<E>(payload)?;
    let hlen = hlen as usize;
    if hlen < n || hlen > payload.len() {
        return Err(Error::CorruptRecord);
    }
    let mut serials = AVec::new();
    let mut off = n;
    while off < hlen && serials.len() < max_cols {
        let (s, used) = decode_varint::<E>(&payload[off..hlen])?;
        serials.push(s);
        off += used;
    }
    if off != hlen {
        // More serial types in the header than the schema has
        // columns: a record from a wider table version. Trailing
        // columns are not ours to interpret; validate their serial
        // types are at least well-formed, then ignore them.
        let mut rest = off;
        while rest < hlen {
            let (s, used) = decode_varint::<E>(&payload[rest..hlen])?;
            let _ = serial_body_len::<E>(s)?;
            rest += used;
        }
    }
    let mut body = hlen;
    for s in serials.iter() {
        let s = *s;
        let blen = serial_body_len::<E>(s)?;
        let end = body.checked_add(blen).ok_or(Error::CorruptRecord)?;
        if end > payload.len() {
            return Err(Error::CorruptRecord);
        }
        let bytes = &payload[body..end];
        let v = match s {
            0 => Value::Null,
            8 => Value::Integer(0),
            9 => Value::Integer(1),
            1..=6 => {
                let mut x: i64 = if bytes[0] & 0x80 != 0 { -1 } else { 0 };
                for b in bytes {
                    x = (x << 8) | i64::from(*b);
                }
                Value::Integer(x)
            }
            7 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(bytes);
                Value::Real(f64::from_be_bytes(b))
            }
            n if n >= 12 && n % 2 == 0 => Value::Blob(bytes),
            _ => Value::Text(bytes),
        };
        out.push(v);
        body = end;
    }
    Ok(())
}

// ---------- comparison (SQFMT §2.2) ----------

fn class_rank(v: &Value<'_>) -> u8 {
    match v {
        Value::Null => 0,
        Value::Integer(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    }
}

/// Exact integer-vs-real comparison (the f64 cast route loses the
/// low bits of large i64s: 9007199254740993 vs 9007199254740992.0
/// must not compare equal). Mirrors the documented semantics:
/// compare against the real's truncation, then its fraction.
fn cmp_int_real(i: i64, r: f64) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if r.is_nan() {
        // SQLite stores NaN as NULL, so a real NaN never reaches a
        // comparison from a file; keep a total order anyway.
        return Ordering::Greater;
    }
    if r >= TWO_63 {
        return Ordering::Less; // every i64 is below 2^63 <= r
    }
    if r < -TWO_63 {
        return Ordering::Greater;
    }
    let ri = r as i64; // truncates toward zero; in range per above
    match i.cmp(&ri) {
        Ordering::Equal => {
            // Same integral part: the fraction decides. r - ri has
            // the sign of the fraction (both are exactly
            // representable differences here).
            let frac = r - ri as f64;
            if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    }
}

/// SQLite comparison order: NULL < numeric (int/real compare
/// numerically across types) < text (BINARY = memcmp) < blob (memcmp).
pub fn compare_values(a: &Value<'_>, b: &Value<'_>) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let (ra, rb) = (class_rank(a), class_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Integer(x), Value::Real(y)) => cmp_int_real(*x, *y),
        (Value::Real(x), Value::Integer(y)) => cmp_int_real(*y, *x).reverse(),
        (Value::Text(x), Value::Text(y)) | (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        _ => Ordering::Equal,
    }
}

// ---------- collating sequences (v0.16) ----------

/// A collating sequence: how two TEXT values compare (v0.16).
/// SQLite's three built-ins — `BINARY` (memcmp, then length),
/// `NOCASE` (ASCII `A`-`Z` folded to lower case, then memcmp and
/// length; other bytes, UTF-8 included, compare as-is) and
/// `RTRIM` (trailing U+0020 spaces ignored, then BINARY). A
/// collation only affects TEXT-vs-TEXT comparisons; every other
/// pairing is `compare_values`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Coll {
    #[default]
    Binary,
    NoCase,
    Rtrim,
}

impl Coll {
    /// The built-in collation named `name` (case-insensitive, as
    /// the pin matches collation names), or None for an unknown
    /// name ("no such collation sequence" on the pin).
    pub fn named(name: &[u8]) -> Option<Coll> {
        let is = |w: &[u8]| name.eq_ignore_ascii_case(w);
        if is(b"BINARY") {
            Some(Coll::Binary)
        } else if is(b"NOCASE") {
            Some(Coll::NoCase)
        } else if is(b"RTRIM") {
            Some(Coll::Rtrim)
        } else {
            None
        }
    }
}

/// Compare two TEXT byte strings under a collation (v0.16).
pub fn compare_text(a: &[u8], b: &[u8], coll: Coll) -> core::cmp::Ordering {
    match coll {
        Coll::Binary => a.cmp(b),
        Coll::NoCase => {
            let n = a.len().min(b.len());
            for (x, y) in a[..n].iter().zip(b[..n].iter()) {
                let c = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if c != core::cmp::Ordering::Equal {
                    return c;
                }
            }
            a.len().cmp(&b.len())
        }
        Coll::Rtrim => {
            let trim = |s: &[u8]| -> usize {
                let mut e = s.len();
                while e > 0 && s[e - 1] == b' ' {
                    e -= 1;
                }
                e
            };
            a[..trim(a)].cmp(&b[..trim(b)])
        }
    }
}

/// `compare_values` under a collation (v0.16): TEXT against
/// TEXT compares by `coll`, everything else as before.
pub fn compare_coll(a: &Value<'_>, b: &Value<'_>, coll: Coll) -> core::cmp::Ordering {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => compare_text(x, y, coll),
        _ => compare_values(a, b),
    }
}

/// One index key column's ordering (v0.16): its direction and
/// its collating sequence (an index on a NOCASE column, or
/// `CREATE INDEX ... (c COLLATE NOCASE)`, orders and finds its
/// keys by NOCASE — exactly the pin's file layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyOrder {
    pub desc: bool,
    pub coll: Coll,
}

// ---------- affinity (SPEC §5.5, SQFMT §2.3) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// Column affinity from the declared type string, SQLite's rules.
pub fn affinity_of(decl: &[u8]) -> Affinity {
    // ASCII uppercase the declaration for substring checks.
    let upper_contains = |needle: &str| -> bool {
        let n = needle.as_bytes();
        if decl.len() < n.len() {
            return false;
        }
        decl.windows(n.len()).any(|w| {
            w.iter()
                .zip(n.iter())
                .all(|(a, b)| a.to_ascii_uppercase() == *b)
        })
    };
    if upper_contains("INT") {
        Affinity::Integer
    } else if upper_contains("CHAR") || upper_contains("CLOB") || upper_contains("TEXT") {
        Affinity::Text
    } else if upper_contains("BLOB") || decl.is_empty() {
        Affinity::Blob
    } else if upper_contains("REAL") || upper_contains("FLOA") || upper_contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Render a real the way SQLite renders it for TEXT conversion
/// (`||`, CAST AS TEXT, TEXT affinity, and the CLI's list mode,
/// which prints a REAL exactly as this conversion does), bit for
/// bit as the pin. Fitted clean-room to the pin's output (v0.16,
/// STATUS.md "CI vs the real pin"): the pin's approximate 18 or
/// 19 digits `d` (`lexer::pin_digits`), then the first of
///
/// 1. the first 13 digits, truncated, when they read back as `r`
///    and digits 14..=16 are `000` or the digits needed a downward
///    decimal scale (`r` about 1e17 or more);
/// 2. when digits 15 and 16 are `99`, the 14-digit rounding (half
///    up) when it reads back as `r`;
/// 3. the 17-digit rounding (half up: double rounding from `d`).
///
/// Trailing zeros dropped; laid out as `%!.17g`: fixed notation
/// for decimal exponents -4 ..= 16 with `.0` kept on integral
/// values, else `d.ddde±XX`. `-0.0` is `0.0`, infinities `Inf` /
/// `-Inf`.
/// The pin's 17-significant-digit rounding of the finite `r > 0`
/// whose pin digits are `(d, scale)` (`lexer::pin_digits`), with
/// its shortening: `(c, n, e10)` — the coefficient `c` of `n`
/// digits (one more after a rounding carry) and the decimal
/// exponent `e10` of the first of the `n`. Used by REAL → TEXT
/// and by printf's `%!.17g` (any `!` conversion rounding to
/// exactly 17 digits), which the pin computes the same way.
pub fn short17(r: f64, d: u64, scale: i32) -> (u64, i32, i32) {
    let p = |k: u32| 10u64.pow(k);
    // 18 or 19 digits.
    let l: u32 = if d >= p(18) { 19 } else { 18 };
    // Decimal exponent of d's first digit.
    let e10 = l as i32 - 1 - scale;
    let lead16 = d / p(l - 16);
    let c13 = d / p(l - 13);
    let c14 = (d + 5 * p(l - 15)) / p(l - 14);
    let (c, n) = if (scale < 0 || lead16.is_multiple_of(1000)) && reads_back(c13, e10 - 12, r) {
        (c13, 13)
    } else if lead16 % 100 == 99 && reads_back(c14, e10 - 13, r) {
        (c14, 14)
    } else {
        ((d + 5 * p(l - 18)) / p(l - 17), 17)
    };
    (c, n, e10)
}

#[inline(never)]
pub fn render_real(r: f64, out: &mut AVec<u8>) {
    if r < 0.0 {
        out.push(b'-');
    }
    let r = fabs(r);
    if r == f64::INFINITY {
        out.extend_from_slice(b"Inf");
        return;
    }
    if r == 0.0 || r.is_nan() {
        out.extend_from_slice(b"0.0");
        return;
    }
    let (d, scale) = crate::sql::lexer::pin_digits(r);
    let (c, n, e10) = short17(r, d, scale);
    let p = |k: u32| 10u64.pow(k);
    let mut nd = 1usize;
    while nd < 20 && c >= p(nd as u32) {
        nd += 1;
    }
    let mut dig = [0u8; 20];
    let mut x = c;
    for k in (0..nd).rev() {
        if let Some(slot) = dig.get_mut(k) {
            *slot = b'0' + (x % 10) as u8;
        }
        x /= 10;
    }
    // A rounding carry adds a digit.
    let exp = e10 + nd as i32 - n;
    while nd > 1 && dig.get(nd - 1) == Some(&b'0') {
        nd -= 1;
    }
    let digits = dig.get(..nd).unwrap_or(&[]);
    // Fixed: the integer part (at least `0`), `.`, the fraction
    // (at least `0`). Exponential: one digit, `.`, the rest (at
    // least `0`), `e`, the signed exponent (at least two digits).
    let fixed = (-4..=16).contains(&exp);
    let (int, lead) = if fixed {
        (exp.max(0) as usize + 1, (-exp).max(0) as usize)
    } else {
        (1, 0)
    };
    for k in 0..int {
        out.push(if lead > 0 {
            b'0'
        } else {
            digits.get(k).copied().unwrap_or(b'0')
        });
    }
    out.push(b'.');
    for _ in 1..lead {
        out.push(b'0');
    }
    let from = if lead > 0 { 0 } else { int };
    match digits.get(from..) {
        Some(rest) if !rest.is_empty() => out.extend_from_slice(rest),
        _ => out.push(b'0'),
    }
    if !fixed {
        out.push(b'e');
        out.push(if exp < 0 { b'-' } else { b'+' });
        let e = exp.unsigned_abs();
        if e >= 100 {
            out.push(b'0' + (e / 100) as u8);
        }
        out.push(b'0' + (e / 10 % 10) as u8);
        out.push(b'0' + (e % 10) as u8);
    }
}

#[cfg(feature = "v04")]
/// A decoded REAL: `n` significant digits `z[s..s + n]` with the
/// decimal point after digit `idp` (`0.d1d2... * 10^idp`).
pub struct Fp {
    pub neg: bool,
    /// 0 finite, 1 infinite, 2 NaN.
    pub special: u8,
    pub z: [u8; 26],
    pub s: usize,
    pub n: usize,
    pub idp: i32,
}

#[cfg(feature = "v04")]
impl Fp {
    pub fn digit(&self, j: usize) -> u8 {
        if j < self.n {
            self.z.get(self.s + j).copied().unwrap_or(b'0')
        } else {
            b'0'
        }
    }
}

#[cfg(feature = "v04")]
/// Decode `r` into the pin's digits, rounded half up to `iround`
/// significant digits (or, for `iround <= 0`, to `-iround` digits
/// after the point) and to at most `mx` digits (16, or 26 with the
/// `!` flag) — the pin's `sqlite3FpDecode`.
pub fn fp_decode(r: f64, mut iround: i32, mx: i32) -> Fp {
    let mut f = Fp {
        neg: false,
        special: 0,
        z: [b'0'; 26],
        s: 3,
        n: 1,
        idp: 1,
    };
    if r.is_nan() {
        f.special = 2;
        return f;
    }
    if r == 0.0 {
        return f;
    }
    f.neg = r < 0.0;
    let a = fabs(r);
    if a == f64::INFINITY {
        f.special = 1;
        return f;
    }
    let (mut d, scale) = crate::sql::lexer::pin_digits(a);
    let d0 = d;
    // %e / %g pass a significant-digit count; %f passes `-precision`.
    let short = iround > 0 && iround < 17;
    let mut nd = 0usize;
    let mut tmp = [b'0'; 20];
    while d > 0 && nd < tmp.len() {
        if let Some(slot) = tmp.get_mut(nd) {
            *slot = b'0' + (d % 10) as u8;
        }
        d /= 10;
        nd += 1;
    }
    for k in 0..nd {
        if let (Some(dst), Some(src)) = (f.z.get_mut(f.s + k), tmp.get(nd - 1 - k)) {
            *dst = *src;
        }
    }
    f.n = nd;
    f.idp = nd as i32 - scale;
    if iround <= 0 {
        iround = f.idp.saturating_sub(iround);
        if iround == 0 && f.digit(0) >= b'5' {
            iround = 1;
            f.s = f.s.saturating_sub(1);
            if let Some(z) = f.z.get_mut(f.s) {
                *z = b'0';
            }
            f.n += 1;
            f.idp += 1;
        }
    }
    // A short significant-digit rounding (%e / %g to fewer than 17
    // digits) reads the digits from the pin's floor at that
    // precision instead (`lexer::pin_digits_floor`); %f rounds the
    // 18 / 19 digits (`printf('%.3f', 166162.7745)` is .775 there
    // while `%.8e` shows ...774).
    if short {
        let (mut v, n2) = crate::sql::lexer::pin_digits_floor(a, iround);
        let mut nd = 0usize;
        let mut tmp = [b'0'; 20];
        while v > 0 && nd < tmp.len() {
            if let Some(slot) = tmp.get_mut(nd) {
                *slot = b'0' + (v % 10) as u8;
            }
            v /= 10;
            nd += 1;
        }
        for k in 0..nd {
            if let (Some(dst), Some(src)) = (f.z.get_mut(f.s + k), tmp.get(nd - 1 - k)) {
                *dst = *src;
            }
        }
        f.n = nd;
        f.idp = nd as i32 - n2;
    }
    if iround > 0 && ((iround as usize) < f.n || f.n as i32 > mx) {
        if iround > mx {
            iround = mx;
        }
        // Exactly 17 digits (only reachable with `!`): the pin's
        // shortened 17-digit form, as REAL -> TEXT has it
        // (`printf('%!.17g', 0.1)` is 0.1).
        if iround == 17 && f.s == 3 {
            let (c, n, e10) = short17(a, d0, scale);
            let mut nd = 0usize;
            let mut x = c;
            let mut tmp = [b'0'; 20];
            while x > 0 && nd < tmp.len() {
                if let Some(slot) = tmp.get_mut(nd) {
                    *slot = b'0' + (x % 10) as u8;
                }
                x /= 10;
                nd += 1;
            }
            for k in 0..nd {
                if let (Some(dst), Some(src)) = (f.z.get_mut(f.s + k), tmp.get(nd - 1 - k)) {
                    *dst = *src;
                }
            }
            f.n = nd;
            f.idp = e10 + 1 + (nd as i32 - n);
            while f.n > 0 && f.digit(f.n - 1) == b'0' {
                f.n -= 1;
            }
            return f;
        }
        let ir = iround as usize;
        let round_up = f.digit(ir) >= b'5';
        f.n = ir;
        if round_up {
            let mut j = ir;
            loop {
                if j == 0 {
                    f.s = f.s.saturating_sub(1);
                    if let Some(z) = f.z.get_mut(f.s) {
                        *z = b'1';
                    }
                    f.n += 1;
                    f.idp += 1;
                    break;
                }
                j -= 1;
                let Some(z) = f.z.get_mut(f.s + j) else {
                    break;
                };
                if *z < b'9' {
                    *z += 1;
                    break;
                }
                *z = b'0';
            }
        }
    }
    while f.n > 0 && f.digit(f.n - 1) == b'0' {
        f.n -= 1;
    }
    f
}

/// `round(r, n)` for `n` in 1..=30 as the pin computes it (v0.16
/// fit): its `roundFunc` prints `%!.*f` — the pin's own digits
/// (`fp_decode`, at most 26 of them) rounded half up at `n`
/// places, a value too small to reach the `n`th place printing as
/// zero — and reads the text back. (v0.15 rounded the exact
/// binary expansion instead, which differs where the pin's
/// approximate digits do.)
#[cfg(feature = "v04")]
pub fn round_digits(r: f64, n: i64) -> f64 {
    let n = n.clamp(0, 30) as i32;
    let f = fp_decode(r, -n, 26);
    if f.special != 0 {
        return r;
    }
    let kept = if f.idp.saturating_add(n) <= 0 { 0 } else { f.n };
    let mut c: u64 = 0;
    for j in 0..kept {
        c = c
            .saturating_mul(10)
            .saturating_add(u64::from(f.digit(j).saturating_sub(b'0')));
    }
    // (int_pow10_to_f64 takes a positive coefficient.)
    let x = if c == 0 {
        0.0
    } else {
        crate::sql::lexer::int_pow10_to_f64(c, i64::from(f.idp) - kept as i64)
    };
    if f.neg { -x } else { x }
}

/// Whether `c * 10^x` reads back (correctly rounded, like the
/// pin's own text-to-real conversion) as `r`.
fn reads_back(c: u64, x: i32, r: f64) -> bool {
    crate::sql::lexer::int_pow10_to_f64(c, i64::from(x)) == r
}

/// `f64::fract`/`abs` are not in `core`; spell the two checks we need.
pub fn fabs(r: f64) -> f64 {
    if r < 0.0 { -r } else { r }
}

pub fn is_integral_real(r: f64) -> bool {
    if fabs(r) >= 9.223372036854776e18 {
        return false;
    }
    r == (r as i64) as f64
}

/// Render an integer as decimal text.
pub fn render_int(i: i64, out: &mut AVec<u8>) {
    let mut tmp = [0u8; 20];
    let mut n = 0usize;
    let mut x = i.unsigned_abs();
    if x == 0 {
        tmp[n] = b'0';
        n += 1;
    }
    let mut digits = [0u8; 20];
    let mut d = 0usize;
    while x > 0 {
        digits[d] = b'0' + (x % 10) as u8;
        d += 1;
        x /= 10;
    }
    if i < 0 {
        tmp[n] = b'-';
        n += 1;
    }
    while d > 0 {
        d -= 1;
        tmp[n] = digits[d];
        n += 1;
    }
    for b in &tmp[..n] {
        out.push(*b);
    }
}

/// Try to interpret text as a SQLite number (for NUMERIC affinity).
/// Returns Integer if the text is a well-formed integer literal,
/// Real if a well-formed real literal, else None. SQLite requires
/// the whole string (modulo surrounding spaces) to be numeric.
///
/// The text is read as the pin reads it: up to its first NUL
/// (`'5' || char(0) || 'x'` is 5), with only the ASCII spaces
/// `sqlite3Isspace` knows (space, `\t` `\n` `\v` `\f` `\r`)
/// trimmed — `char(160) || '5'` stays text (v0.16 fix, ungated:
/// Unicode whitespace used to be trimmed and a NUL refused the
/// number).
pub fn text_to_number(t: &[u8]) -> Option<OwnedValue> {
    let t = t.split(|c| *c == 0).next()?;
    let u = core::str::from_utf8(t).ok()?;
    let s = u.trim();
    // Only the ASCII spaces may go: `trim` takes Unicode ones too,
    // and a number holds no space inside.
    let ascii = u
        .bytes()
        .filter(|c| matches!(c, b' ' | b'\t'..=b'\r'))
        .count();
    if s.is_empty() || u.len().saturating_sub(s.len()) != ascii {
        return None;
    }
    if let Ok(i) = s.parse::<i64>() {
        // parse::<i64> accepts a leading '+', SQLite does too.
        return Some(OwnedValue::Integer(i));
    }
    // Strict real check: must look like a number, not "inf"/"nan".
    let bytes = s.as_bytes();
    let mut has_digit = false;
    for (i, c) in bytes.iter().enumerate() {
        match c {
            b'0'..=b'9' => has_digit = true,
            b'.' | b'e' | b'E' => {}
            b'+' | b'-' => {
                if i != 0 && !matches!(bytes[i - 1], b'e' | b'E') {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if has_digit && let Some(r) = crate::sql::lexer::parse_real_signed(bytes) {
        // The exact decimal conversion (v0.12) serves both the
        // real-literal shape and the pure-integer shape: integer
        // text reaches here only when it overflowed i64 above,
        // and the oracle converts such text to a REAL as well.
        return Some(OwnedValue::Real(r));
    }
    None
}

/// Apply a column affinity to a value (SPEC §5.5's minimum honest
/// subset): conversions happen on INSERT, mirroring SQLite.
pub fn apply_affinity(v: OwnedValue, aff: Affinity) -> OwnedValue {
    match aff {
        Affinity::Blob => v,
        Affinity::Text => match v {
            OwnedValue::Integer(i) => {
                let mut t = AVec::new();
                render_int(i, &mut t);
                OwnedValue::Text(t)
            }
            OwnedValue::Real(r) => {
                let mut t = AVec::new();
                render_real(r, &mut t);
                OwnedValue::Text(t)
            }
            other => other,
        },
        Affinity::Integer | Affinity::Numeric => match v {
            OwnedValue::Text(t) => {
                if let Some(n) = text_to_number(t.as_slice()) {
                    match n {
                        OwnedValue::Real(r) => {
                            if is_integral_real(r) {
                                OwnedValue::Integer(r as i64)
                            } else {
                                OwnedValue::Real(r)
                            }
                        }
                        other => other,
                    }
                } else {
                    OwnedValue::Text(t)
                }
            }
            OwnedValue::Real(r) => {
                if is_integral_real(r) {
                    OwnedValue::Integer(r as i64)
                } else {
                    OwnedValue::Real(r)
                }
            }
            other => other,
        },
        Affinity::Real => match v {
            OwnedValue::Integer(i) => OwnedValue::Real(i as f64),
            OwnedValue::Text(t) => match text_to_number(t.as_slice()) {
                Some(OwnedValue::Integer(i)) => OwnedValue::Real(i as f64),
                Some(OwnedValue::Real(r)) => OwnedValue::Real(r),
                _ => OwnedValue::Text(t),
            },
            other => other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin's REAL -> TEXT strings (CI's 3.54.1 run and the
    /// fitted corpus; `tests/real_text_v16.rs` checks millions
    /// against the oracle). Here so miri walks every branch.
    #[test]
    #[allow(clippy::excessive_precision)]
    fn render_real_pin_strings() {
        for (v, want) in [
            (1e15, "1000000000000000.0"),
            (9223372036854775808.0, "9.2233720368547758e+18"),
            (0.000123456789012345, "0.00012345678901234499"),
            (574.423710258671, "574.42371025867101"),
            (-589569.9865969187, "-589569.98659691867"),
            (-3.521289658049935e18, "-3.5212896580499349e+18"),
            (8.5281530614695, "8.5281530614695"),
            (9.5e21, "9.5e+21"),
            (9.4174e-10, "9.417400000000001e-10"),
            (1e-14, "1.0e-14"),
            (0.1, "0.1"),
            (5e-324, "4.9406564584124654e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (-0.0, "0.0"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            let mut out = AVec::new();
            render_real(v, &mut out);
            assert_eq!(out.as_slice(), want.as_bytes(), "{v:?}");
        }
    }

    #[test]
    fn varint_roundtrip() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16384,
            1 << 21,
            1 << 28,
            1 << 35,
            1 << 42,
            1 << 49,
            1 << 56,
            u64::MAX,
            0x00ff_ffff_ffff_ffff,
            0x0100_0000_0000_0000,
        ] {
            let mut out = AVec::new();
            encode_varint(v, &mut out);
            let (back, used) = decode_varint::<()>(out.as_slice()).expect("decode");
            assert_eq!(back, v, "value {v}");
            assert_eq!(used, out.len(), "len for {v}");
        }
    }

    #[test]
    fn record_roundtrip_widths() {
        let vals = [
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(127),
            Value::Integer(128),
            Value::Integer(-129),
            Value::Integer(32767),
            Value::Integer(32768),
            Value::Integer(i64::MIN),
            Value::Integer(9_007_199_254_740_993),
            Value::Real(-0.0),
            Value::Real(1.5),
            Value::Text(b""),
            Value::Blob(b""),
            Value::Text(b"hello"),
            Value::Null,
        ];
        let mut rec = AVec::new();
        encode_record(&vals, &mut rec);
        let back = decode_record::<()>(rec.as_slice(), vals.len()).expect("decode");
        assert_eq!(back.len(), vals.len());
        for (a, b) in vals.iter().zip(back.iter()) {
            match (a, b) {
                (Value::Real(x), Value::Real(y)) => {
                    assert_eq!(x.to_bits(), y.to_bits())
                }
                _ => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn int_real_compare_is_exact() {
        use core::cmp::Ordering::*;
        let c = |a: Value<'_>, b: Value<'_>| compare_values(&a, &b);
        // The f64-cast trap: 2^53+1 as an integer vs 2^53 as a real.
        assert_eq!(
            c(
                Value::Integer(9_007_199_254_740_993),
                Value::Real(9_007_199_254_740_992.0)
            ),
            Greater
        );
        assert_eq!(c(Value::Integer(3), Value::Real(3.5)), Less);
        assert_eq!(c(Value::Integer(3), Value::Real(3.0)), Equal);
        assert_eq!(c(Value::Integer(-3), Value::Real(-3.5)), Greater);
        assert_eq!(c(Value::Integer(-4), Value::Real(-3.5)), Less);
        assert_eq!(
            c(Value::Integer(i64::MAX), Value::Real(9.223372036854776e18)),
            Less
        );
        assert_eq!(c(Value::Integer(0), Value::Real(-0.0)), Equal);
        assert_eq!(c(Value::Null, Value::Integer(i64::MIN)), Less);
        assert_eq!(c(Value::Text(b"a"), Value::Blob(b"a")), Less);
    }

    #[test]
    fn affinity_rules() {
        assert_eq!(affinity_of(b"INTEGER"), Affinity::Integer);
        assert_eq!(affinity_of(b"VARCHAR(10)"), Affinity::Text);
        assert_eq!(affinity_of(b""), Affinity::Blob);
        assert_eq!(affinity_of(b"DOUBLE PRECISION"), Affinity::Real);
        assert_eq!(affinity_of(b"DECIMAL(10,2)"), Affinity::Numeric);
        assert_eq!(affinity_of(b"UNSIGNED BIG INT"), Affinity::Integer);
    }
}
