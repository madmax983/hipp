//! UTF-16 databases (v0.16 leaf `utf16`): the text conversions of
//! a file whose header text encoding (offset 56) is 2 (UTF-16le) or
//! 3 (UTF-16be), fitted to the pin's observable behaviour.
//!
//! hipp keeps text as UTF-8 inside the engine whatever the file's
//! encoding; a UTF-16 file's records are translated at the B-tree
//! boundary (`transcode_record`): text read from a page becomes the
//! UTF-8 the pin's own UTF-16 → UTF-8 conversion gives (what its
//! `sqlite3_column_text` shows), and text written becomes UTF-16 in
//! the file's byte order. What the pin exposes of the UTF-16 bytes
//! themselves is rebuilt from the UTF-8 form by `units`:
//!
//! - the BINARY collation compares the stored UTF-16 bytes with
//!   memcmp (`cmp`), so code points beyond the BMP sort below
//!   U+E000..U+FFFF in UTF-16be and the low byte decides first in
//!   UTF-16le; NOCASE and RTRIM exist on the pin for UTF-8 only and
//!   compare the UTF-8 form as before;
//! - `hex(text)`, `CAST(text AS BLOB)` and `octet_length` see the
//!   UTF-16 bytes (`encode`), and a blob read as text is decoded
//!   from UTF-16 (`decode`).
//!
//! Invalid UTF-16 is held losslessly: a high surrogate followed by
//! a low one is the character they name, any other surrogate is kept
//! as its three-byte form (`ED A0..BF xx`), an odd trailing byte is
//! dropped (`decode_into`). So stored bytes survive any statement
//! that only moves them (a copy, an index build, `||`, CAST), and
//! comparisons see exactly the stored units. Where the pin reads
//! such text through its UTF-16 → UTF-8 conversion — which combines
//! any surrogate with the unit after it — hipp reads `view`: a
//! statement's result columns, function arguments, LIKE / GLOB,
//! NOCASE / RTRIM, JSON. Text a function builds is normalised as the
//! pin's UTF-8 → UTF-16 conversion of a function result normalises
//! it (`normalize`: surrogates become U+FFFD).

use core::cmp::Ordering;

use crate::arena::AVec;
use crate::error::{Error, Result};
use crate::record::{TextEnc, decode_varint, encode_varint, serial_body_len, varint_size};

/// Whether `enc` stores text big-endian (None: UTF-8, nothing to
/// convert).
pub fn byte_order(enc: TextEnc) -> Option<bool> {
    match enc {
        TextEnc::Utf8 => None,
        TextEnc::Utf16le => Some(false),
        TextEnc::Utf16be => Some(true),
    }
}

/// The pin's `WRITE_UTF8` of `c` (any value up to 0x1FFFFF; the
/// conversions here never produce more than 0x10FFFF).
fn put_utf8(c: u32, out: &mut AVec<u8>) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.push(0xC0 | (c >> 6) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    } else if c < 0x10000 {
        out.push(0xE0 | (c >> 12) as u8);
        out.push(0x80 | ((c >> 6) & 0x3F) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    } else {
        out.push(0xF0 | ((c >> 18) & 0x07) as u8);
        out.push(0x80 | ((c >> 12) & 0x3F) as u8);
        out.push(0x80 | ((c >> 6) & 0x3F) as u8);
        out.push(0x80 | (c & 0x3F) as u8);
    }
}

/// One UTF-16 code unit of `p` (a two-byte chunk) in the given
/// byte order.
fn unit(p: &[u8], be: bool) -> u32 {
    let (x, y) = match p {
        [x, y, ..] => (u32::from(*x), u32::from(*y)),
        _ => (0, 0),
    };
    if be { (x << 8) | y } else { (y << 8) | x }
}

/// UTF-16 bytes → hipp's lossless internal form, appended to `out`
/// (an odd trailing byte dropped). A high surrogate followed by a
/// low one is the supplementary character they name; any other
/// surrogate is kept as its three-byte form (`ED A0..BF xx`), so
/// `encode_into` gives the stored units back exactly and BINARY
/// order (`cmp`) is the pin's memcmp over them. What the pin's own
/// UTF-16 → UTF-8 conversion shows of such text is `view`.
pub fn decode_into(b: &[u8], be: bool, out: &mut AVec<u8>) {
    let mut chunks = b.chunks_exact(2).peekable();
    while let Some(p) = chunks.next() {
        let mut c = unit(p, be);
        if (0xD800..0xDC00).contains(&c)
            && let Some(q) = chunks.peek()
            && (0xDC00..0xE000).contains(&unit(q, be))
        {
            let c2 = unit(q, be);
            chunks.next();
            c = 0x10000 + ((c - 0xD800) << 10) + (c2 - 0xDC00);
        }
        put_utf8(c, out);
    }
}

/// UTF-16 bytes → the UTF-8 the pin's `sqlite3VdbeMemTranslate`
/// gives (its `READ_UTF16`: any surrogate followed by another unit
/// combines with it into one supplementary code point; a surrogate
/// that ends the text keeps its three-byte form), appended to
/// `out`. This is what the pin's `sqlite3_value_text` and its CLI
/// show of a UTF-16 text.
pub fn decode_pin_into(b: &[u8], be: bool, out: &mut AVec<u8>) {
    let mut chunks = b.chunks_exact(2);
    while let Some(p) = chunks.next() {
        let mut c = unit(p, be);
        if (0xD800..0xE000).contains(&c)
            && let Some(q) = chunks.next()
        {
            let c2 = unit(q, be);
            c = (c2 & 0x3FF) + ((c & 0x3F) << 10) + (((c & 0x3C0) + 0x40) << 10);
        }
        put_utf8(c, out);
    }
}

/// Whether internal text `t` holds a surrogate's three-byte form
/// (an unpaired surrogate, or one ending the text).
pub fn has_surrogate(t: &[u8]) -> bool {
    t.windows(2)
        .any(|w| matches!(w, [0xED, b] if (0xA0..=0xBF).contains(b)))
}

/// What the pin shows of internal text `t` (`decode_pin_into` of
/// its UTF-16): None when that is `t` itself (no unpaired
/// surrogate). Applied where the pin reads a UTF-16 value through
/// `sqlite3_value_text` — a statement's result columns, function
/// arguments, LIKE / GLOB, the NOCASE and RTRIM collations, JSON.
pub fn view(t: &[u8]) -> Option<AVec<u8>> {
    if !has_surrogate(t) {
        return None;
    }
    let mut u = AVec::new();
    encode_into(t, true, &mut u);
    let mut out = AVec::new();
    decode_pin_into(u.as_slice(), true, &mut out);
    Some(out)
}

/// Internal text `t` in its canonical form (`decode_into` of its
/// UTF-16): None when it is already canonical. Two texts joined
/// byte-wise (`||`) can leave a high surrogate's three-byte form
/// right before a low one's; the pin's concatenation of their UTF-16
/// bytes makes them a pair.
pub fn canon(t: &[u8]) -> Option<AVec<u8>> {
    let split_pair = t.windows(5).any(|w| {
        matches!(w, [0xED, h, _, 0xED, l] if (0xA0..=0xAF).contains(h) && (0xB0..=0xBF).contains(l))
    });
    if !split_pair {
        return None;
    }
    let mut u = AVec::new();
    encode_into(t, true, &mut u);
    let mut out = AVec::new();
    decode_into(u.as_slice(), true, &mut out);
    if out.as_slice() == t { None } else { Some(out) }
}

/// `decode_into` as a fresh vector.
pub fn decode(b: &[u8], enc: TextEnc) -> AVec<u8> {
    let mut out = AVec::new();
    match byte_order(enc) {
        Some(be) => decode_into(b, be, &mut out),
        None => out.extend_from_slice(b),
    }
    out
}

/// `decode_pin_into` as a fresh vector (UTF-8: the bytes
/// unchanged).
pub fn decode_pin(b: &[u8], enc: TextEnc) -> AVec<u8> {
    let mut out = AVec::new();
    match byte_order(enc) {
        Some(be) => decode_pin_into(b, be, &mut out),
        None => out.extend_from_slice(b),
    }
    out
}

/// The leading-byte payload bits of the pin's `sqlite3Utf8Trans1`.
fn trans1(b: u8) -> u32 {
    let b = u32::from(b);
    match b {
        0xC0..=0xDF => b & 0x1F,
        0xE0..=0xEF => b & 0x0F,
        0xF0..=0xF7 => b & 0x07,
        0xF8..=0xFB => b & 0x03,
        0xFC..=0xFD => b & 0x01,
        _ => 0,
    }
}

/// The UTF-16 code units of a UTF-8 text, as the pin's `READ_UTF8`
/// / `WRITE_UTF16` loop produces them (total over any bytes). With
/// `replace` a surrogate code point becomes U+FFFD (the pin's
/// conversion of a function's result); without it, it is written as
/// the unit it names (text read from a UTF-16 file and passed on).
pub struct Units<'a> {
    s: &'a [u8],
    i: usize,
    pending: Option<u16>,
    replace: bool,
}

impl<'a> Units<'a> {
    pub fn new(s: &'a [u8], replace: bool) -> Units<'a> {
        Units {
            s,
            i: 0,
            pending: None,
            replace,
        }
    }
}

impl Iterator for Units<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        if let Some(u) = self.pending.take() {
            return Some(u);
        }
        let b = *self.s.get(self.i)?;
        self.i += 1;
        let mut c = u32::from(b);
        if b >= 0xC0 {
            c = trans1(b);
            while let Some(n) = self.s.get(self.i).filter(|n| **n & 0xC0 == 0x80) {
                c = (c << 6).wrapping_add(u32::from(*n & 0x3F));
                self.i += 1;
            }
            let surrogate = c & 0xFFFF_F800 == 0xD800;
            if c < 0x80 || (surrogate && self.replace) {
                c = 0xFFFD;
            }
        }
        if c <= 0xFFFF {
            return Some(c as u16);
        }
        // The pin's WRITE_UTF16 arithmetic, masks and all.
        let d = c.wrapping_sub(0x10000);
        let lo0 = ((c >> 10) & 0x3F) + ((d >> 10) & 0xC0);
        let hi0 = 0xD8 + ((d >> 18) & 0x03);
        let lo1 = c & 0xFF;
        let hi1 = 0xDC + ((c >> 8) & 0x03);
        self.pending = Some((((hi1 & 0xFF) << 8) | lo1) as u16);
        Some((((hi0 & 0xFF) << 8) | (lo0 & 0xFF)) as u16)
    }
}

/// UTF-8 text → UTF-16 bytes in the given order, appended to `out`
/// (lone surrogates kept; see `Units`).
pub fn encode_into(t: &[u8], be: bool, out: &mut AVec<u8>) {
    for u in Units::new(t, false) {
        let [h, l] = u.to_be_bytes();
        if be {
            out.push(h);
            out.push(l);
        } else {
            out.push(l);
            out.push(h);
        }
    }
}

/// `encode_into` as a fresh vector (UTF-8: the bytes unchanged).
pub fn encode(t: &[u8], enc: TextEnc) -> AVec<u8> {
    let mut out = AVec::new();
    match byte_order(enc) {
        Some(be) => encode_into(t, be, &mut out),
        None => out.extend_from_slice(t),
    }
    out
}

/// The number of UTF-16 bytes `t` encodes to (`octet_length`).
pub fn encoded_len(t: &[u8]) -> usize {
    Units::new(t, false).count().saturating_mul(2)
}

/// The BINARY collation of a UTF-16 database: memcmp of the two
/// texts' UTF-16 bytes, then the length.
pub fn cmp(a: &[u8], b: &[u8], be: bool) -> Ordering {
    // ASCII-only prefixes compare alike in every encoding.
    let n = a
        .iter()
        .zip(b.iter())
        .take_while(|(x, y)| x == y && **x < 0x80)
        .count();
    let (a, b) = (a.get(n..).unwrap_or(&[]), b.get(n..).unwrap_or(&[]));
    let key = |u: u16| if be { u } else { u.swap_bytes() };
    let mut x = Units::new(a, false);
    let mut y = Units::new(b, false);
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => match key(p).cmp(&key(q)) {
                Ordering::Equal => {}
                o => return o,
            },
        }
    }
}

/// A function's text result as the pin stores it in a UTF-16
/// database (its UTF-8 → UTF-16 conversion, read back): None when
/// `t` is unchanged by that round trip (all ASCII, or no surrogate
/// and well-formed).
pub fn normalize(t: &[u8]) -> Option<AVec<u8>> {
    if t.iter().all(|b| *b < 0x80) {
        return None;
    }
    let mut bytes = AVec::new();
    for u in Units::new(t, true) {
        let [h, l] = u.to_be_bytes();
        bytes.push(h);
        bytes.push(l);
    }
    let mut out = AVec::new();
    decode_into(bytes.as_slice(), true, &mut out);
    if out.as_slice() == t { None } else { Some(out) }
}

/// Translate a record payload's TEXT values between UTF-8 and the
/// UTF-16 of `be` (`to16`: UTF-8 → UTF-16), into `out` (cleared
/// first). Every other value, and any bytes after the last column,
/// are copied as they are. Check-first like `decode_record`.
pub fn transcode_record<E>(
    payload: &[u8],
    be: bool,
    to16: bool,
    out: &mut AVec<u8>,
) -> Result<(), E> {
    out.clear();
    let (hlen, n) = decode_varint::<E>(payload)?;
    let hlen = usize::try_from(hlen).map_err(|_| Error::CorruptRecord)?;
    let head = payload.get(n..hlen).ok_or(Error::CorruptRecord)?;
    let mut serials: AVec<u64> = AVec::new();
    let mut off = 0usize;
    while off < head.len() {
        let (s, used) = decode_varint::<E>(head.get(off..).unwrap_or(&[]))?;
        serials.push(s);
        off += used;
    }
    let mut body: AVec<u8> = AVec::new();
    let mut pos = hlen;
    let mut text = false;
    for s in serials.as_mut_slice().iter_mut() {
        let blen = serial_body_len::<E>(*s)?;
        let end = pos.checked_add(blen).ok_or(Error::CorruptRecord)?;
        let bytes = payload.get(pos..end).ok_or(Error::CorruptRecord)?;
        pos = end;
        if *s >= 13 && *s % 2 == 1 {
            text = true;
            let at = body.len();
            if to16 {
                encode_into(bytes, be, &mut body);
            } else {
                decode_into(bytes, be, &mut body);
            }
            let len = (body.len() - at) as u64;
            *s = len
                .checked_mul(2)
                .and_then(|x| x.checked_add(13))
                .ok_or(Error::CorruptRecord)?;
        } else {
            body.extend_from_slice(bytes);
        }
    }
    if !text {
        out.extend_from_slice(payload);
        return Ok(());
    }
    let sizes: usize = serials.iter().map(|s| varint_size(*s)).sum();
    let mut h = sizes + 1;
    loop {
        let need = sizes + varint_size(h as u64);
        if need == h {
            break;
        }
        h = need;
    }
    encode_varint(h as u64, out);
    for s in serials.iter() {
        encode_varint(*s, out);
    }
    out.extend_from_slice(body.as_slice());
    out.extend_from_slice(payload.get(pos..).unwrap_or(&[]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(b: &[u8], be: bool) -> AVec<u8> {
        let mut out = AVec::new();
        decode_into(b, be, &mut out);
        out
    }

    fn enc(t: &[u8], be: bool) -> AVec<u8> {
        let mut out = AVec::new();
        encode_into(t, be, &mut out);
        out
    }

    /// The pin's translations, as its `hex()` / CLI show them.
    #[test]
    fn pin_translation() {
        // Valid text both ways, both byte orders.
        let s = "a\u{e9}\u{100}\u{1f600}\u{e000}\u{10ffff}".as_bytes();
        for be in [false, true] {
            let u = enc(s, be);
            assert_eq!(dec(u.as_slice(), be).as_slice(), s);
            assert_eq!(encoded_len(s), u.len());
        }
        assert_eq!(
            enc("\u{1f600}".as_bytes(), false).as_slice(),
            &[0x3d, 0xd8, 0, 0xde]
        );
        assert_eq!(
            enc("\u{1f600}".as_bytes(), true).as_slice(),
            &[0xd8, 0x3d, 0xde, 0]
        );
        // An odd trailing byte is dropped.
        assert_eq!(dec(&[0x41, 0, 0x42], false).as_slice(), b"A");
        // Held losslessly: an unpaired surrogate keeps its 3-byte
        // form and encodes back to itself, whatever follows it.
        for (u, be) in [
            (&[0u8, 0xd8, 0x61, 0][..], false),
            (&[0xd8, 0, 0, 0x61][..], true),
            (&[0, 0xdc, 0, 0xd8][..], false),
            (&[0, 0xd8, 0, 0xd8, 0, 0xdc][..], false),
            (&[0, 0xd8][..], false),
        ] {
            let t = dec(u, be);
            assert_eq!(enc(t.as_slice(), be).as_slice(), u);
        }
        assert_eq!(
            dec(&[0, 0xd8, 0x61, 0], false).as_slice(),
            &[0xed, 0xa0, 0x80, b'a']
        );
        assert_eq!(dec(&[0, 0xd8], false).as_slice(), &[0xed, 0xa0, 0x80]);
        // The pin's own conversion (`view`): a surrogate followed by
        // any unit combines with it; one ending the text stays.
        let shown = |u: &[u8]| {
            let t = dec(u, false);
            view(t.as_slice()).map_or(t.as_slice().to_vec(), |v| v.as_slice().to_vec())
        };
        assert_eq!(shown(&[0, 0xd8, 0x61, 0]), "\u{10061}".as_bytes());
        assert_eq!(shown(&[0, 0xdc, 0x61, 0]), "\u{10061}".as_bytes());
        assert_eq!(shown(&[0, 0xdc, 0, 0xd8]), "\u{10000}".as_bytes());
        assert_eq!(shown(&[0, 0xd8]), &[0xed, 0xa0, 0x80]);
        assert!(view("a\u{1f600}".as_bytes()).is_none());
        // `||` across a split pair joins it (`canon`).
        let mut j = dec(&[0, 0xd8], false);
        j.extend_from_slice(dec(&[0, 0xdc], false).as_slice());
        assert_eq!(
            canon(j.as_slice()).map(|v| v.as_slice().to_vec()),
            Some("\u{10000}".as_bytes().to_vec())
        );
        assert!(canon(dec(&[0, 0xdc, 0, 0xd8], false).as_slice()).is_none());
        // A function result's surrogate becomes U+FFFD.
        assert_eq!(
            normalize(&[b'x', 0xed, 0xa0, 0x80]).map(|v| v.as_slice().to_vec()),
            Some("x\u{fffd}".as_bytes().to_vec())
        );
        assert!(normalize("\u{fffe}\u{1f600}".as_bytes()).is_none());
        assert!(normalize(b"plain").is_none());
    }

    /// BINARY is memcmp over the UTF-16 bytes (oracle-probed
    /// orders).
    #[test]
    fn binary_order() {
        let le = |a: &str, b: &str| cmp(a.as_bytes(), b.as_bytes(), false);
        let be = |a: &str, b: &str| cmp(a.as_bytes(), b.as_bytes(), true);
        // UTF-16le: the low byte first — U+0100 (00 01) sorts below
        // 'a' (61 00), U+1F600 (3D D8 ..) too.
        assert_eq!(le("\u{100}", "a"), Ordering::Less);
        assert_eq!(le("\u{1f600}", "a"), Ordering::Less);
        assert_eq!(le("a", "b"), Ordering::Less);
        assert_eq!(le("ab", "a"), Ordering::Greater);
        // UTF-16be: code units — astral (D83D) below U+E000.
        assert_eq!(be("\u{1f600}", "\u{e000}"), Ordering::Less);
        assert_eq!(be("a", "\u{100}"), Ordering::Less);
        assert_eq!(be("", "\u{0}"), Ordering::Less);
        assert_eq!(be("x\u{e9}", "x\u{e9}"), Ordering::Equal);
        // Unpaired surrogates order by their stored units: D800 0061
        // is not the pair D800 DC61.
        let lone = dec(&[0xd8, 0, 0, 0x61], true);
        let pair = dec(&[0xd8, 0, 0xdc, 0x61], true);
        assert_eq!(cmp(lone.as_slice(), pair.as_slice(), true), Ordering::Less);
        assert_ne!(lone.as_slice(), pair.as_slice());
    }

    /// Records translate both ways, other values untouched.
    #[test]
    fn records() {
        use crate::record::{Value, decode_record, encode_record};
        let vals = [
            Value::Integer(7),
            Value::Text("h\u{e9}llo\u{1f600}".as_bytes()),
            Value::Null,
            Value::Blob(&[1, 2, 3]),
            Value::Real(1.5),
            Value::Text(b""),
        ];
        let mut rec = AVec::new();
        encode_record(&vals, &mut rec);
        for be in [false, true] {
            let mut w = AVec::new();
            transcode_record::<()>(rec.as_slice(), be, true, &mut w).unwrap_or_default();
            let stored = decode_record::<()>(w.as_slice(), 9).unwrap_or_default();
            assert_eq!(stored.len(), vals.len());
            assert_eq!(
                stored.as_slice()[1],
                Value::Text(enc(vals[1].text(), be).as_slice())
            );
            let mut r = AVec::new();
            transcode_record::<()>(w.as_slice(), be, false, &mut r).unwrap_or_default();
            assert_eq!(r.as_slice(), rec.as_slice());
        }
        // Damaged records are errors, never panics.
        let mut w = AVec::new();
        assert!(transcode_record::<()>(&[0x05, 0x21], false, false, &mut w).is_err());
        assert!(transcode_record::<()>(&[], false, false, &mut w).is_err());
    }

    trait TextOf {
        fn text(&self) -> &[u8];
    }

    impl TextOf for crate::record::Value<'_> {
        fn text(&self) -> &[u8] {
            match self {
                crate::record::Value::Text(t) => t,
                _ => &[],
            }
        }
    }
}
