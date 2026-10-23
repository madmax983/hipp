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

/// Render a real the way SQLite renders it for TEXT conversion:
/// shortest round-trip, with `.0` kept for integral values.
pub fn render_real(r: f64, out: &mut AVec<u8>) {
    // Rust's Display for f64 gives shortest-round-trip without an
    // exponent for typical magnitudes and matches SQLite's rendering
    // for the values v0.1 handles (1.5 -> "1.5", 1.0 -> "1.0").
    let mut buf = [0u8; 40];
    let s = format_real(r, &mut buf);
    for b in s.bytes() {
        out.push(b);
    }
}

struct BufWriter<'a> {
    buf: &'a mut [u8],
    n: usize,
}
impl core::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if self.n < self.buf.len() {
                self.buf[self.n] = b;
                self.n += 1;
            }
        }
        Ok(())
    }
}

fn format_real(r: f64, buf: &mut [u8; 40]) -> &str {
    use core::fmt::Write as _;
    let integral = is_integral_real(r);
    let mut w = BufWriter { buf, n: 0 };
    if integral {
        let _ = write!(w, "{r:.1}");
    } else {
        let _ = write!(w, "{r}");
    }
    let n = w.n;
    core::str::from_utf8(&w.buf[..n]).unwrap_or("0")
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
pub fn text_to_number(t: &[u8]) -> Option<OwnedValue> {
    let s = core::str::from_utf8(t).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(i) = s.parse::<i64>() {
        // parse::<i64> accepts a leading '+', SQLite does too.
        return Some(OwnedValue::Integer(i));
    }
    // Strict real check: must look like a number, not "inf"/"nan".
    let bytes = s.as_bytes();
    let mut has_digit = false;
    let mut has_dot_or_exp = false;
    for (i, c) in bytes.iter().enumerate() {
        match c {
            b'0'..=b'9' => has_digit = true,
            b'.' | b'e' | b'E' => has_dot_or_exp = true,
            b'+' | b'-' => {
                if i != 0 && !matches!(bytes[i - 1], b'e' | b'E') {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if has_digit
        && has_dot_or_exp
        && let Ok(r) = s.parse::<f64>()
    {
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
