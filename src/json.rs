//! JSON functions (v0.16, leaf feature `json`): the pin's JSON1
//! support — text JSON and JSON5 input, the JSONB binary format
//! (byte for byte the pin's blobs), paths, the edit functions,
//! `json_patch`, the aggregates' text accumulation and the
//! `json_each` / `json_tree` table-valued functions.
//!
//! Everything works on JSONB: a text argument is translated to a
//! blob first (the pin's `jsonTranslateTextToBlob`), edits splice
//! the blob, and a text result renders it back. The layouts are
//! the pin's documented format: one header byte whose low nibble
//! is the element type and whose high nibble is the payload size
//! (0..=11), or 12 / 13 / 14 / 15 for a 1 / 2 / 4 / 8-byte
//! big-endian size following it; numbers and strings keep their
//! source text as payload (so `json('1e5')` renders `1e5`).
//!
//! Every walk over a document is iterative with an explicit stack
//! (no recursion on input-derived depth): the pin's nesting limit
//! of 1000 bounds the parser, the renderers ("JSON nested too
//! deep"), path lookups ("JSON path too deep") and `json_patch`.
//! Nothing here indexes unchecked or panics on input; reads past
//! a buffer's end see NUL, as the pin's NUL-terminated buffers do.

use crate::arena::AVec;
use crate::error::SchemaKind;
use crate::record::OwnedValue;

type R<T> = core::result::Result<T, SchemaKind>;

/// The pin's `JSON_MAX_DEPTH`.
const MAX_DEPTH: usize = 1000;

const T_NULL: u8 = 0;
const T_TRUE: u8 = 1;
const T_FALSE: u8 = 2;
const T_INT: u8 = 3;
const T_INT5: u8 = 4;
const T_FLOAT: u8 = 5;
const T_FLOAT5: u8 = 6;
const T_TEXT: u8 = 7;
const T_TEXTJ: u8 = 8;
const T_TEXT5: u8 = 9;
const T_TEXTRAW: u8 = 10;
const T_ARRAY: u8 = 11;
const T_OBJECT: u8 = 12;

/// The code point an invalid escape decodes to (the pin's
/// `JSON_INVALID_CHAR`): dropped from a decoded string.
const INVALID_CHAR: u32 = 0x99999;

/// `json_type()` names per element type.
const TYPE_NAMES: [&[u8]; 16] = [
    b"null", b"true", b"false", b"integer", b"integer", b"real", b"real", b"text", b"text",
    b"text", b"text", b"array", b"object", b"", b"", b"",
];

/// Byte `i` of `b`, or NUL past its end.
#[inline]
fn g(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}
fn is_xdigit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}
fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}
/// The pin's `jsonIsspace`.
fn is_jspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}
/// The pin's `sqlite3Isspace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
/// A byte a JSON string may hold unescaped (the pin's `jsonIsOk`).
fn is_ok(c: u8) -> bool {
    c >= 0x20 && c != b'"' && c != b'\\' && c != b'\''
}
/// The pin's `sqlite3JsonId1` / `sqlite3JsonId2`.
fn id1(c: u8) -> bool {
    is_alpha(c) || c == b'_' || c == b'$' || c >= 0x80
}
fn id2(c: u8) -> bool {
    is_alnum(c) || c == b'_' || c == b'$' || c >= 0x80
}
/// The pin's `jsonHexToInt`: a hex digit's value (any byte maps
/// to its low nibble, letters adjusted).
fn hex_val(c: u8) -> u32 {
    let h = u32::from(c);
    (h + 9 * ((h >> 6) & 1)) & 0xf
}
fn is_4hex(z: &[u8], i: usize) -> bool {
    (0..4).all(|k| is_xdigit(g(z, i + k)))
}
fn hex4(z: &[u8], i: usize) -> u32 {
    (0..4).fold(0, |a, k| (a << 4) | hex_val(g(z, i + k)))
}
fn starts_ci(z: &[u8], i: usize, w: &[u8]) -> bool {
    w.iter()
        .enumerate()
        .all(|(k, c)| g(z, i + k).eq_ignore_ascii_case(c))
}
fn push_dec(out: &mut AVec<u8>, mut u: u64) {
    let mut d = [0u8; 20];
    let mut n = 0;
    loop {
        if let Some(s) = d.get_mut(n) {
            *s = b'0' + (u % 10) as u8;
        }
        n += 1;
        u /= 10;
        if u == 0 || n >= 20 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out.push(g(&d, n));
    }
}
fn slice(b: &[u8], from: usize, len: usize) -> &[u8] {
    let end = from.saturating_add(len).min(b.len());
    b.get(from.min(end)..end).unwrap_or(&[])
}

// ---------------------------------------------------------------
// JSONB headers.

/// The pin's `jsonbPayloadSize`: (header bytes, payload bytes) of
/// the element at `i`, or (0, 0) when the header is truncated or
/// the payload runs past the blob.
fn payload(b: &[u8], i: usize) -> (usize, usize) {
    payload_x(b, i, 0)
}

/// `payload`, the payload allowed to run `extra` bytes past the
/// blob's end.
fn payload_x(b: &[u8], i: usize, extra: usize) -> (usize, usize) {
    let len = b.len();
    let x = g(b, i) >> 4;
    let (n, sz) = match x {
        0..=11 => (1usize, usize::from(x)),
        12 => {
            if i.saturating_add(1) >= len {
                return (0, 0);
            }
            (2, usize::from(g(b, i + 1)))
        }
        13 => {
            if i.saturating_add(2) >= len {
                return (0, 0);
            }
            (3, usize::from(g(b, i + 1)) << 8 | usize::from(g(b, i + 2)))
        }
        14 => {
            if i.saturating_add(4) >= len {
                return (0, 0);
            }
            (5, be32(b, i + 1))
        }
        _ => {
            if i.saturating_add(8) >= len || (1..5).any(|k| g(b, i + k) != 0) {
                return (0, 0);
            }
            (9, be32(b, i + 5))
        }
    };
    if i.saturating_add(n).saturating_add(sz) > len.saturating_add(extra) {
        return (0, 0);
    }
    (n, sz)
}

/// `payload` while an edit has shrunk the blob by `-delta` bytes
/// and left the enclosing headers stale: the pin bounds the payload
/// by the blob's size before the edit too.
fn payload_d(b: &[u8], i: usize, delta: isize) -> (usize, usize) {
    payload_x(b, i, if delta < 0 { delta.unsigned_abs() } else { 0 })
}

fn be32(b: &[u8], i: usize) -> usize {
    (0..4).fold(0usize, |a, k| (a << 8) | usize::from(g(b, i + k)))
}

/// The payload size a header states, unchecked against the blob
/// (the pin reads it with the blob's allocation as its bound while
/// an edit has left the enclosing headers stale).
fn raw_size(b: &[u8], i: usize) -> usize {
    match g(b, i) >> 4 {
        x @ 0..=11 => usize::from(x),
        12 => usize::from(g(b, i + 1)),
        13 => usize::from(g(b, i + 1)) << 8 | usize::from(g(b, i + 2)),
        14 => be32(b, i + 1),
        _ => be32(b, i + 5),
    }
}

fn header_extra(sz: usize) -> usize {
    if sz <= 11 {
        0
    } else if sz <= 0xff {
        1
    } else if sz <= 0xffff {
        2
    } else {
        4
    }
}

/// The pin's `jsonBlobAppendNode`: the smallest header, then the
/// payload when given.
fn append_node(b: &mut AVec<u8>, t: u8, sz: usize, payload: Option<&[u8]>) {
    match header_extra(sz) {
        0 => b.push(t | ((sz as u8) << 4)),
        1 => {
            b.push(t | 0xc0);
            b.push(sz as u8);
        }
        2 => {
            b.push(t | 0xd0);
            b.push((sz >> 8) as u8);
            b.push(sz as u8);
        }
        _ => {
            b.push(t | 0xe0);
            for k in (0..4).rev() {
                b.push((sz >> (8 * k)) as u8);
            }
        }
    }
    if let Some(p) = payload {
        b.extend_from_slice(p);
    }
}

/// Replace `del` bytes at `at` by `ins`, in place (the pin's
/// memmove: only the bytes after the range move, so closing a
/// container while parsing costs its own payload, not the blob).
fn splice(b: &mut AVec<u8>, at: usize, del: usize, ins: &[u8]) {
    b.replace_range(at, del, ins);
}

/// The pin's `jsonBlobChangePayloadSize`: rewrite the header at `i`
/// for payload size `sz` with the fewest bytes; returns how many
/// bytes the header grew (negative: shrank).
fn change_payload_size(b: &mut AVec<u8>, i: usize, sz: usize) -> isize {
    let x = g(b.as_slice(), i);
    let extra = match x >> 4 {
        0..=11 => 0usize,
        12 => 1,
        13 => 2,
        14 => 4,
        _ => 8,
    };
    let need = header_extra(sz);
    let t = x & 0x0f;
    let mut h: AVec<u8> = AVec::new();
    append_node(&mut h, t, sz, None);
    splice(b, i, 1 + extra, h.as_slice());
    need as isize - extra as isize
}

/// The pin's `jsonBlobOverwrite`: write `ins`, enlarged by `d`
/// (1..=8) bytes through a denormalized size field, over the `d`
/// longer element at `at`; false when no such header exists.
fn overwrite(b: &mut AVec<u8>, at: usize, ins: &[u8], d: usize) -> bool {
    let x = g(ins, 0);
    if x & 0x0f <= T_FALSE {
        return false;
    }
    let (newhdr, szhdr) = match x >> 4 {
        12 => {
            if !matches!(d, 1 | 3 | 7) {
                return false;
            }
            (d + 2, 2)
        }
        13 => {
            if d != 2 && d != 6 {
                return false;
            }
            (d + 3, 3)
        }
        14 => {
            if d != 4 {
                return false;
            }
            (9, 5)
        }
        15 => return false,
        _ => {
            if !matches!(d, 1 | 2 | 4 | 8) {
                return false;
            }
            (d + 1, 1)
        }
    };
    let top = match newhdr {
        2 => 0xc0,
        3 => 0xd0,
        5 => 0xe0,
        _ => 0xf0,
    };
    let body = ins.get(szhdr..).unwrap_or(&[]);
    let mut w: AVec<u8> = AVec::with_capacity(newhdr + body.len());
    w.push((x & 0x0f) | top);
    let n = body.len();
    for k in (0..newhdr - 1).rev() {
        w.push(((n as u64) >> (8 * k)) as u8);
    }
    w.extend_from_slice(body);
    let len = w.len();
    splice(b, at, len, w.as_slice());
    true
}

/// The pin's `jsonBlobEdit`: remove `del` bytes at `at`, insert
/// `ins` (or `nins` placeholder bytes when `ins` is None);
/// `delta` accumulates the size change.
fn blob_edit(
    b: &mut AVec<u8>,
    at: usize,
    del: usize,
    ins: Option<&[u8]>,
    nins: usize,
    delta: &mut isize,
) {
    let d = nins as isize - del as isize;
    if let Some(i) = ins
        && (-8..0).contains(&d)
        && overwrite(b, at, i, (-d) as usize)
    {
        return;
    }
    match ins {
        Some(i) => splice(b, at, del, i),
        None => {
            let mut z = AVec::with_capacity(nins);
            for _ in 0..nins {
                z.push(0u8);
            }
            splice(b, at, del, z.as_slice());
        }
    }
    *delta += d;
}

/// The pin's `jsonAfterEditSizeAdjust` for the container at `i`.
fn adjust(b: &mut AVec<u8>, i: usize, delta: &mut isize) {
    let sz = raw_size(b.as_slice(), i) as isize + *delta;
    *delta += change_payload_size(b, i, sz.max(0) as usize);
}

/// The pin's `jsonbArrayCount`.
fn array_count(b: &[u8], root: usize) -> u32 {
    let (mut n, sz) = payload(b, root);
    let end = root + n + sz;
    let mut i = root + n;
    let mut k = 0u32;
    while n > 0 && i < end {
        let (n2, sz2) = payload(b, i);
        n = n2;
        i += n2 + sz2;
        k = k.wrapping_add(1);
    }
    k
}

// ---------------------------------------------------------------
// Text JSON / JSON5 to JSONB.

/// A JSON document as JSONB, and whether its text used JSON5.
#[derive(Debug, Clone)]
struct Doc {
    /// The JSONB.
    b: AVec<u8>,
    /// The text used JSON5 (`json_valid(X, 1)` is 0).
    nonstd: bool,
}

/// One open container of the text parser (the pin's recursion
/// frame), with the state its loop resumes in.
#[derive(Clone, Copy)]
struct PFrame {
    obj: bool,
    st: u8,
    this: usize,
    start: usize,
    j: usize,
    iblob: usize,
}

const S_ELEM: u8 = 0;
const S_PROBE: u8 = 1;
const S_LABEL: u8 = 2;
const S_COLON: u8 = 3;
const S_VALUE: u8 = 4;

enum Scan {
    Open(bool, usize),
    Done(i64),
}

enum Cont {
    Parse(usize),
    Pop(i64),
}

struct TextParser<'a> {
    z: &'a [u8],
    b: AVec<u8>,
    nonstd: bool,
    err: usize,
    depth: usize,
}

/// The pin's `json5Whitespace`: bytes of JSON5 white space and
/// comments at `i`.
fn json5_ws(z: &[u8], i: usize) -> usize {
    let mut n = 0usize;
    loop {
        let c = g(z, i + n);
        match c {
            0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x20 => n += 1,
            b'/' => {
                if g(z, i + n + 1) == b'*' && g(z, i + n + 2) != 0 {
                    let mut j = n + 3;
                    loop {
                        if g(z, i + j) == b'/' && g(z, i + j - 1) == b'*' {
                            break;
                        }
                        if g(z, i + j) == 0 {
                            return n;
                        }
                        j += 1;
                    }
                    n = j + 1;
                } else if g(z, i + n + 1) == b'/' {
                    let mut j = n + 2;
                    loop {
                        let c = g(z, i + j);
                        if c == 0 || c == b'\n' || c == b'\r' {
                            break;
                        }
                        if c == 0xe2
                            && g(z, i + j + 1) == 0x80
                            && matches!(g(z, i + j + 2), 0xa8 | 0xa9)
                        {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    n = j;
                    if g(z, i + n) != 0 {
                        n += 1;
                    }
                } else {
                    return n;
                }
            }
            0xc2 if g(z, i + n + 1) == 0xa0 => n += 2,
            0xe1 if g(z, i + n + 1) == 0x9a && g(z, i + n + 2) == 0x80 => n += 3,
            0xe2 => {
                let c1 = g(z, i + n + 1);
                let c2 = g(z, i + n + 2);
                if c1 == 0x80 {
                    if c2 < 0x80 {
                        return n;
                    }
                    if c2 <= 0x8a || c2 == 0xa8 || c2 == 0xa9 || c2 == 0xaf {
                        n += 3;
                    } else {
                        return n;
                    }
                } else if c1 == 0x81 && c2 == 0x9f {
                    n += 3;
                } else {
                    return n;
                }
            }
            0xe3 if g(z, i + n + 1) == 0x80 && g(z, i + n + 2) == 0x80 => n += 3,
            0xef if g(z, i + n + 1) == 0xbb && g(z, i + n + 2) == 0xbf => n += 3,
            _ => return n,
        }
    }
}

fn strspn_spaces(z: &[u8], mut i: usize) -> usize {
    while is_jspace(g(z, i)) {
        i += 1;
    }
    i
}

impl<'a> TextParser<'a> {
    fn new(z: &'a [u8]) -> TextParser<'a> {
        TextParser {
            z,
            b: AVec::new(),
            nonstd: false,
            err: 0,
            depth: 0,
        }
    }

    fn at(&self, i: usize) -> u8 {
        g(self.z, i)
    }

    /// The pin's `jsonTranslateTextToBlob` from `i0`: > 0 the index
    /// after the value, 0 end of input, -1 an error at `self.err`,
    /// -2..=-5 a `}` `]` `,` `:` at `self.err`.
    fn translate(&mut self, i0: usize) -> i64 {
        let mut st: AVec<PFrame> = AVec::new();
        let mut req = i0;
        loop {
            let mut x = match self.scan(req) {
                Scan::Open(obj, at) => {
                    let this = self.b.len();
                    let t = if obj { T_OBJECT } else { T_ARRAY };
                    append_node(&mut self.b, t, self.z.len().saturating_sub(at), None);
                    self.depth += 1;
                    if self.depth > MAX_DEPTH {
                        self.err = at;
                        -1
                    } else {
                        let start = self.b.len();
                        st.push(PFrame {
                            obj,
                            st: if obj { S_LABEL } else { S_ELEM },
                            this,
                            start,
                            j: at + 1,
                            iblob: start,
                        });
                        req = at + 1;
                        continue;
                    }
                }
                Scan::Done(x) => x,
            };
            loop {
                let Some(f) = st.as_mut_slice().last_mut() else {
                    return x;
                };
                let mut fr = *f;
                let c = self.cont(&mut fr, x);
                if let Some(f) = st.as_mut_slice().last_mut() {
                    *f = fr;
                }
                match c {
                    Cont::Parse(p) => {
                        req = p;
                        break;
                    }
                    Cont::Pop(r) => {
                        st.pop();
                        x = r;
                    }
                }
            }
        }
    }

    /// Close the container `f` at `j` (its closing bracket).
    fn close(&mut self, f: &PFrame, j: usize) -> Cont {
        let sz = self.b.len().saturating_sub(f.start);
        change_payload_size(&mut self.b, f.this, sz);
        self.depth = self.depth.saturating_sub(1);
        Cont::Pop((j + 1) as i64)
    }

    fn next_member(&mut self, f: &mut PFrame, j: usize) -> Cont {
        f.j = j;
        if f.obj {
            f.iblob = self.b.len();
            f.st = S_LABEL;
        } else {
            f.st = S_ELEM;
        }
        Cont::Parse(j)
    }

    /// After a member's value at `j`: a separator, the close, or
    /// the pin's probe of what follows.
    fn after_value(&mut self, f: &mut PFrame, mut j: usize) -> Cont {
        let close = if f.obj { b'}' } else { b']' };
        let c = self.at(j);
        if c == b',' {
            return self.next_member(f, j + 1);
        }
        if c == close {
            return self.close(f, j);
        }
        if is_jspace(c) {
            j = strspn_spaces(self.z, j + 1);
            let c = self.at(j);
            if c == b',' {
                return self.next_member(f, j + 1);
            }
            if c == close {
                return self.close(f, j);
            }
        }
        f.j = j;
        f.st = S_PROBE;
        Cont::Parse(j)
    }

    fn cont(&mut self, f: &mut PFrame, x: i64) -> Cont {
        let fail = Cont::Pop(-1);
        match f.st {
            S_ELEM => {
                if x <= 0 {
                    if x == -3 {
                        let j = self.err;
                        if self.b.len() != f.start {
                            self.nonstd = true;
                        }
                        let fr = *f;
                        return self.close(&fr, j);
                    }
                    if x != -1 {
                        self.err = f.j;
                    }
                    return fail;
                }
                self.after_value(f, x as usize)
            }
            S_PROBE => {
                let close_code = if f.obj { -2 } else { -3 };
                if x == -4 {
                    let j = self.err;
                    return self.next_member(f, j + 1);
                }
                if x == close_code {
                    let j = self.err;
                    let fr = *f;
                    return self.close(&fr, j);
                }
                self.err = f.j;
                fail
            }
            S_LABEL => {
                let mut x = x;
                if x <= 0 {
                    if x == -2 {
                        let j = self.err;
                        if self.b.len() != f.start {
                            self.nonstd = true;
                        }
                        let fr = *f;
                        return self.close(&fr, j);
                    }
                    let j = f.j + json5_ws(self.z, f.j);
                    let mut op = T_TEXT;
                    let c = self.at(j);
                    if id1(c) || (c == b'\\' && self.is4hexb(j + 1, &mut op)) {
                        let mut k = j + 1;
                        loop {
                            let c = self.at(k);
                            if (id2(c) && json5_ws(self.z, k) == 0)
                                || (c == b'\\' && self.is4hexb(k + 1, &mut op))
                            {
                                k += 1;
                            } else {
                                break;
                            }
                        }
                        let key = slice(self.z, j, k - j);
                        append_node(&mut self.b, op, k - j, Some(key));
                        self.nonstd = true;
                        x = k as i64;
                    } else {
                        if x != -1 {
                            self.err = j;
                        }
                        return fail;
                    }
                }
                let t = g(self.b.as_slice(), f.iblob) & 0x0f;
                if !(T_TEXT..=T_TEXTRAW).contains(&t) {
                    self.err = f.j;
                    return fail;
                }
                let mut j = x as usize;
                if self.at(j) == b':' {
                    return self.value(f, j + 1);
                }
                if is_jspace(self.at(j)) {
                    j = strspn_spaces(self.z, j + 1);
                    if self.at(j) == b':' {
                        return self.value(f, j + 1);
                    }
                }
                f.j = j;
                f.st = S_COLON;
                Cont::Parse(j)
            }
            S_COLON => {
                if x != -5 {
                    if x != -1 {
                        self.err = f.j;
                    }
                    return fail;
                }
                let j = self.err + 1;
                self.value(f, j)
            }
            _ => {
                if x <= 0 {
                    if x != -1 {
                        self.err = f.j;
                    }
                    return fail;
                }
                self.after_value(f, x as usize)
            }
        }
    }

    fn value(&mut self, f: &mut PFrame, j: usize) -> Cont {
        f.j = j;
        f.st = S_VALUE;
        Cont::Parse(j)
    }

    fn is4hexb(&self, i: usize, op: &mut u8) -> bool {
        if self.at(i) != b'u' || !is_4hex(self.z, i + 1) {
            return false;
        }
        *op = T_TEXTJ;
        true
    }

    /// One token or scalar value at `i` (white space skipped), or
    /// the opening of a container.
    fn scan(&mut self, mut i: usize) -> Scan {
        loop {
            let c = self.at(i);
            return Scan::Done(match c {
                b'{' => return Scan::Open(true, i),
                b'[' => return Scan::Open(false, i),
                b'\'' => {
                    self.nonstd = true;
                    self.string(i)
                }
                b'"' => self.string(i),
                b't' => self.word(i, b"true", T_TRUE),
                b'f' => self.word(i, b"false", T_FALSE),
                b'+' => {
                    self.nonstd = true;
                    self.number(i, 0)
                }
                b'.' => {
                    if is_digit(self.at(i + 1)) {
                        self.nonstd = true;
                        self.number_2(i, 3)
                    } else {
                        self.err = i;
                        -1
                    }
                }
                b'-' | b'0'..=b'9' => self.number(i, 0),
                b'}' => {
                    self.err = i;
                    -2
                }
                b']' => {
                    self.err = i;
                    -3
                }
                b',' => {
                    self.err = i;
                    -4
                }
                b':' => {
                    self.err = i;
                    -5
                }
                0 => 0,
                0x09 | 0x0a | 0x0d | 0x20 => {
                    i = strspn_spaces(self.z, i + 1);
                    continue;
                }
                0x0b | 0x0c | b'/' | 0xc2 | 0xe1 | 0xe2 | 0xe3 | 0xef => {
                    let j = json5_ws(self.z, i);
                    if j > 0 {
                        i += j;
                        self.nonstd = true;
                        continue;
                    }
                    self.err = i;
                    -1
                }
                b'n' if self.lit(i, b"null") => {
                    self.b.push(T_NULL);
                    (i + 4) as i64
                }
                _ => self.nan_inf(i),
            });
        }
    }

    fn lit(&self, i: usize, w: &[u8]) -> bool {
        slice(self.z, i, w.len()) == w && !is_alnum(self.at(i + w.len()))
    }

    fn word(&mut self, i: usize, w: &[u8], t: u8) -> i64 {
        if self.lit(i, w) {
            self.b.push(t);
            (i + w.len()) as i64
        } else {
            self.err = i;
            -1
        }
    }

    fn nan_inf(&mut self, i: usize) -> i64 {
        const NAMES: [(&[u8], bool); 5] = [
            (b"inf", true),
            (b"infinity", true),
            (b"nan", false),
            (b"qnan", false),
            (b"snan", false),
        ];
        let c = self.at(i).to_ascii_lowercase();
        for (w, inf) in NAMES {
            if g(w, 0) != c || !starts_ci(self.z, i, w) || is_alnum(self.at(i + w.len())) {
                continue;
            }
            if inf {
                append_node(&mut self.b, T_FLOAT, 5, Some(b"9e999"));
            } else {
                self.b.push(T_NULL);
            }
            self.nonstd = true;
            return (i + w.len()) as i64;
        }
        self.err = i;
        -1
    }

    fn string(&mut self, i: usize) -> i64 {
        let delim = self.at(i);
        let mut op = T_TEXT;
        let mut j = i + 1;
        loop {
            let mut c = self.at(j);
            if is_ok(c) {
                j += 1;
                continue;
            }
            if c == delim {
                break;
            } else if c == b'\\' {
                j += 1;
                c = self.at(j);
                if matches!(c, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't')
                    || (c == b'u' && is_4hex(self.z, j + 1))
                {
                    if op == T_TEXT {
                        op = T_TEXTJ;
                    }
                } else if c == b'\''
                    || c == b'v'
                    || c == b'\n'
                    || (c == b'0' && !is_digit(self.at(j + 1)))
                    || (c == 0xe2
                        && self.at(j + 1) == 0x80
                        && matches!(self.at(j + 2), 0xa8 | 0xa9))
                    || (c == b'x' && is_xdigit(self.at(j + 1)) && is_xdigit(self.at(j + 2)))
                {
                    op = T_TEXT5;
                    self.nonstd = true;
                } else if c == b'\r' {
                    if self.at(j + 1) == b'\n' {
                        j += 1;
                    }
                    op = T_TEXT5;
                    self.nonstd = true;
                } else {
                    self.err = j;
                    return -1;
                }
            } else if c <= 0x1f {
                if c == 0 {
                    self.err = j;
                    return -1;
                }
                op = T_TEXT5;
                self.nonstd = true;
            } else if c == b'"' {
                op = T_TEXT5;
            }
            j += 1;
        }
        let body = slice(self.z, i + 1, j - 1 - i);
        append_node(&mut self.b, op, j - 1 - i, Some(body));
        (j + 1) as i64
    }

    fn number(&mut self, i: usize, t: u8) -> i64 {
        let mut t = t;
        let c = self.at(i);
        if c <= b'0' {
            if c == b'0' {
                if matches!(self.at(i + 1), b'x' | b'X') && is_xdigit(self.at(i + 2)) {
                    self.nonstd = true;
                    let mut j = i + 3;
                    while is_xdigit(self.at(j)) {
                        j += 1;
                    }
                    return self.number_finish(i, j, 1);
                } else if is_digit(self.at(i + 1)) {
                    self.err = i + 1;
                    return -1;
                }
            } else {
                if !is_digit(self.at(i + 1)) {
                    if matches!(self.at(i + 1), b'I' | b'i') && starts_ci(self.z, i + 1, b"inf") {
                        self.nonstd = true;
                        if c == b'-' {
                            append_node(&mut self.b, T_FLOAT, 6, Some(b"-9e999"));
                        } else {
                            append_node(&mut self.b, T_FLOAT, 5, Some(b"9e999"));
                        }
                        let n = if starts_ci(self.z, i + 4, b"inity") {
                            9
                        } else {
                            4
                        };
                        return (i + n) as i64;
                    }
                    if self.at(i + 1) == b'.' {
                        self.nonstd = true;
                        t |= 1;
                        return self.number_2(i, t);
                    }
                    self.err = i;
                    return -1;
                }
                if self.at(i + 1) == b'0' {
                    if is_digit(self.at(i + 2)) {
                        self.err = i + 1;
                        return -1;
                    } else if matches!(self.at(i + 2), b'x' | b'X') && is_xdigit(self.at(i + 3)) {
                        self.nonstd = true;
                        t |= 1;
                        let mut j = i + 4;
                        while is_xdigit(self.at(j)) {
                            j += 1;
                        }
                        return self.number_finish(i, j, t);
                    }
                }
            }
        }
        self.number_2(i, t)
    }

    /// The pin's `parse_number_2`: digits, one `.`, one exponent.
    fn number_2(&mut self, i: usize, mut t: u8) -> i64 {
        let mut seen_e = false;
        let mut j = i + 1;
        loop {
            let c = self.at(j);
            if is_digit(c) {
                j += 1;
                continue;
            }
            if c == b'.' {
                if t & 2 != 0 {
                    self.err = j;
                    return -1;
                }
                t |= 2;
                j += 1;
                continue;
            }
            if c == b'e' || c == b'E' {
                if self.at(j - 1) < b'0' {
                    if self.at(j - 1) == b'.' && j >= i + 2 && is_digit(self.at(j - 2)) {
                        self.nonstd = true;
                        t |= 1;
                    } else {
                        self.err = j;
                        return -1;
                    }
                }
                if seen_e {
                    self.err = j;
                    return -1;
                }
                t |= 2;
                seen_e = true;
                let mut c = self.at(j + 1);
                if c == b'+' || c == b'-' {
                    j += 1;
                    c = self.at(j + 1);
                }
                if !is_digit(c) {
                    self.err = j;
                    return -1;
                }
                j += 1;
                continue;
            }
            break;
        }
        if self.at(j - 1) < b'0' {
            if self.at(j - 1) == b'.' && j >= i + 2 && is_digit(self.at(j - 2)) {
                self.nonstd = true;
                t |= 1;
            } else {
                self.err = j;
                return -1;
            }
        }
        self.number_finish(i, j, t)
    }

    fn number_finish(&mut self, mut i: usize, j: usize, t: u8) -> i64 {
        if self.at(i) == b'+' {
            i += 1;
        }
        let body = slice(self.z, i, j - i);
        append_node(&mut self.b, T_INT + t, j - i, Some(body));
        j as i64
    }
}

/// The pin's `jsonConvertTextToBlob`: the whole text as one JSON
/// value (white space, JSON5 comments around it). Err carries the
/// byte offset of the error (`json_error_position`).
fn text_to_doc(z: &[u8]) -> core::result::Result<Doc, usize> {
    let mut p = TextParser::new(z);
    let i = p.translate(0);
    if i > 0 {
        let mut i = strspn_spaces(z, i as usize);
        if g(z, i) != 0 {
            i += json5_ws(z, i);
            if g(z, i) != 0 {
                return Err(p.err);
            }
            p.nonstd = true;
        }
        return Ok(Doc {
            b: p.b,
            nonstd: p.nonstd,
        });
    }
    Err(p.err)
}

/// The pin's `jsonReturnStringAsBlob`: JSON text this module built
/// as JSONB, whatever the parse made of it (it only fails past the
/// nesting limit, and then the pin returns the partial blob).
fn text_to_blob_loose(z: &[u8]) -> AVec<u8> {
    let mut p = TextParser::new(z);
    let _ = p.translate(0);
    p.b
}

// ---------------------------------------------------------------
// JSONB to text.

/// The pin's `jsonAppendControlChar`.
fn control_char(out: &mut AVec<u8>, c: u8) {
    let short = match c {
        8 => b'b',
        9 => b't',
        10 => b'n',
        12 => b'f',
        13 => b'r',
        _ => 0,
    };
    out.push(b'\\');
    if short != 0 {
        out.push(short);
    } else {
        out.extend_from_slice(b"u00");
        let h = b"0123456789abcdef";
        out.push(g(h, usize::from(c >> 4)));
        out.push(g(h, usize::from(c & 0xf)));
    }
}

/// The pin's `jsonAppendString`: `z` as a quoted JSON string.
fn append_string(out: &mut AVec<u8>, z: &[u8]) {
    out.push(b'"');
    for &c in z {
        if is_ok(c) || c == b'\'' {
            out.push(c);
        } else if c == b'"' || c == b'\\' {
            out.push(b'\\');
            out.push(c);
        } else {
            control_char(out, c);
        }
    }
    out.push(b'"');
}

fn render_text5(out: &mut AVec<u8>, z: &[u8]) -> R<()> {
    out.push(b'"');
    let mut k = 0usize;
    let n = z.len();
    while k < n {
        let c = g(z, k);
        if is_ok(c) || c == b'\'' {
            out.push(c);
            k += 1;
            continue;
        }
        if c == b'"' {
            out.extend_from_slice(b"\\\"");
            k += 1;
            continue;
        }
        if c <= 0x1f {
            control_char(out, c);
            k += 1;
            continue;
        }
        // A backslash.
        if n - k < 2 {
            return Err(SchemaKind::JsonMalformed);
        }
        let mut adv = 2usize;
        match g(z, k + 1) {
            b'\'' => out.push(b'\''),
            b'v' => out.extend_from_slice(b"\x5cu000b"),
            b'x' => {
                if n - k < 4 {
                    return Err(SchemaKind::JsonMalformed);
                }
                out.extend_from_slice(b"\x5cu00");
                out.push(g(z, k + 2));
                out.push(g(z, k + 3));
                adv = 4;
            }
            b'0' => out.extend_from_slice(b"\x5cu0000"),
            b'\r' => {
                if n - k > 2 && g(z, k + 2) == b'\n' {
                    adv = 3;
                }
            }
            b'\n' => {}
            0xe2 => {
                if n - k < 4 || g(z, k + 2) != 0x80 || !matches!(g(z, k + 3), 0xa8 | 0xa9) {
                    return Err(SchemaKind::JsonMalformed);
                }
                adv = 4;
            }
            _ => {
                out.push(c);
                out.push(g(z, k + 1));
            }
        }
        k += adv;
    }
    out.push(b'"');
    Ok(())
}

/// One primitive element (`t`, payload at `p` of `sz` bytes) as
/// JSON text.
fn render_scalar(out: &mut AVec<u8>, b: &[u8], t: u8, p: usize, sz: usize) -> R<()> {
    let z = slice(b, p, sz);
    match t {
        T_NULL => out.extend_from_slice(b"null"),
        T_TRUE => out.extend_from_slice(b"true"),
        T_FALSE => out.extend_from_slice(b"false"),
        T_INT | T_FLOAT => {
            if sz == 0 {
                return Err(SchemaKind::JsonMalformed);
            }
            out.extend_from_slice(z);
        }
        T_INT5 => {
            if sz == 0 {
                return Err(SchemaKind::JsonMalformed);
            }
            let mut k = 2usize;
            match g(z, 0) {
                b'-' => {
                    out.push(b'-');
                    k += 1;
                }
                b'+' => k += 1,
                _ => {}
            }
            let mut u = 0u64;
            let mut over = false;
            while k < sz {
                let c = g(z, k);
                if !is_xdigit(c) {
                    return Err(SchemaKind::JsonMalformed);
                } else if u >> 60 != 0 {
                    over = true;
                } else {
                    u = u * 16 + u64::from(hex_val(c));
                }
                k += 1;
            }
            if over {
                out.extend_from_slice(b"9.0e999");
            } else {
                push_dec(out, u);
            }
        }
        T_FLOAT5 => {
            if sz == 0 {
                return Err(SchemaKind::JsonMalformed);
            }
            let mut k = 0usize;
            if g(z, 0) == b'-' {
                out.push(b'-');
                k = 1;
                if sz == 1 {
                    return Err(SchemaKind::JsonMalformed);
                }
            }
            // The pin reads the byte after the payload here.
            if g(b, p + k) == b'.' {
                out.push(b'0');
            }
            while k < sz {
                let c = g(z, k);
                out.push(c);
                if c == b'.' && (k + 1 == sz || !is_digit(g(z, k + 1))) {
                    out.push(b'0');
                }
                k += 1;
            }
        }
        T_TEXT | T_TEXTJ => {
            out.push(b'"');
            out.extend_from_slice(z);
            out.push(b'"');
        }
        T_TEXT5 => render_text5(out, z)?,
        T_TEXTRAW => append_string(out, z),
        _ => return Err(SchemaKind::JsonMalformed),
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RFrame {
    obj: bool,
    end: usize,
    x: u32,
}

/// The pin's `jsonTranslateBlobToText` of the element at `start`:
/// returns the index after it. Containers nest at most 1000 deep
/// ("JSON nested too deep").
fn render(b: &[u8], start: usize, out: &mut AVec<u8>) -> R<usize> {
    let mut st: AVec<RFrame> = AVec::new();
    let mut i = start;
    loop {
        let (n, sz) = payload(b, i);
        if n == 0 {
            return Err(SchemaKind::JsonMalformed);
        }
        let t = g(b, i) & 0x0f;
        let mut next = match t {
            T_NULL | T_TRUE | T_FALSE => {
                render_scalar(out, b, t, i + n, sz)?;
                i + 1
            }
            T_ARRAY | T_OBJECT => {
                let obj = t == T_OBJECT;
                out.push(if obj { b'{' } else { b'[' });
                if st.len() >= MAX_DEPTH {
                    return Err(SchemaKind::JsonNestedTooDeep);
                }
                if sz > 0 {
                    st.push(RFrame {
                        obj,
                        end: i + n + sz,
                        x: 0,
                    });
                    i += n;
                    continue;
                }
                out.push(if obj { b'}' } else { b']' });
                i + n
            }
            _ => {
                render_scalar(out, b, t, i + n, sz)?;
                i + n + sz
            }
        };
        loop {
            let Some(f) = st.as_mut_slice().last_mut() else {
                return Ok(next);
            };
            if f.obj {
                out.push(if f.x & 1 == 1 { b',' } else { b':' });
                f.x += 1;
            } else {
                out.push(b',');
            }
            if next < f.end {
                i = next;
                break;
            }
            if next > f.end || (f.obj && f.x & 1 == 1) {
                return Err(SchemaKind::JsonMalformed);
            }
            out.pop();
            out.push(if f.obj { b'}' } else { b']' });
            next = f.end;
            st.pop();
        }
    }
}

fn render_doc(b: &[u8], i: usize) -> R<AVec<u8>> {
    let mut out = AVec::new();
    render(b, i, &mut out)?;
    Ok(out)
}

#[derive(Clone, Copy)]
struct PrFrame {
    obj: bool,
    j: usize,
    end: usize,
}

fn indent(out: &mut AVec<u8>, ind: &[u8], n: usize) {
    for _ in 0..n {
        out.extend_from_slice(ind);
    }
}

/// The pin's `jsonTranslateBlobToPrettyText`.
fn pretty(b: &[u8], ind: &[u8]) -> R<AVec<u8>> {
    let mut out = AVec::new();
    let mut st: AVec<PrFrame> = AVec::new();
    let mut i = 0usize;
    loop {
        // Pretty-print the element at `i` (a pretty call at depth
        // `st.len() + 1`).
        if st.len() >= MAX_DEPTH {
            return Err(SchemaKind::JsonNestedTooDeep);
        }
        let (n, sz) = payload(b, i);
        if n == 0 {
            return Err(SchemaKind::JsonMalformed);
        }
        let t = g(b, i) & 0x0f;
        let mut done = if t == T_ARRAY || t == T_OBJECT {
            let obj = t == T_OBJECT;
            let j = i + n;
            let end = j + sz;
            out.push(if obj { b'{' } else { b'[' });
            if j < end {
                out.push(b'\n');
                st.push(PrFrame { obj, j, end });
                indent(&mut out, ind, st.len());
                if obj {
                    let k = render(b, j, &mut out)?;
                    if k > end {
                        return Err(SchemaKind::JsonMalformed);
                    }
                    out.extend_from_slice(b": ");
                    i = k;
                } else {
                    i = j;
                }
                continue;
            }
            out.push(if obj { b'}' } else { b']' });
            end
        } else {
            render(b, i, &mut out)?
        };
        loop {
            let Some(f) = st.as_mut_slice().last_mut() else {
                return Ok(out);
            };
            f.j = done;
            if f.j < f.end {
                out.extend_from_slice(b",\n");
                let depth = st.len();
                indent(&mut out, ind, depth);
                let Some(f) = st.as_slice().last() else {
                    return Ok(out);
                };
                if f.obj {
                    let k = render(b, f.j, &mut out)?;
                    if k > f.end {
                        return Err(SchemaKind::JsonMalformed);
                    }
                    out.extend_from_slice(b": ");
                    i = k;
                } else {
                    i = f.j;
                }
                break;
            }
            let (obj, end) = (f.obj, f.end);
            st.pop();
            out.push(b'\n');
            indent(&mut out, ind, st.len());
            out.push(if obj { b'}' } else { b']' });
            done = end;
        }
    }
}

// ---------------------------------------------------------------
// JSONB to SQL values.

/// The pin's `sqlite3Utf8ReadLimited`.
fn utf8_read(z: &[u8]) -> (usize, u32) {
    let c = u32::from(g(z, 0));
    if c < 0xc0 {
        return (1, c);
    }
    let mut v = match c {
        0xc0..=0xdf => c - 0xc0,
        0xe0..=0xef => c - 0xe0,
        0xf0..=0xf7 => c - 0xf0,
        0xf8..=0xfb => c - 0xf8,
        0xfc..=0xfd => c - 0xfc,
        _ => 0,
    };
    let n = z.len().min(4);
    let mut i = 1;
    while i < n && g(z, i) & 0xc0 == 0x80 {
        v = (v << 6) + u32::from(g(z, i) & 0x3f);
        i += 1;
    }
    (i, v)
}

/// The pin's `jsonBytesToBypass`: escaped line continuations.
fn bytes_to_bypass(z: &[u8]) -> usize {
    let n = z.len();
    let mut i = 0usize;
    while i + 1 < n {
        if g(z, i) != b'\\' {
            return i;
        }
        let c = g(z, i + 1);
        if c == b'\n' {
            i += 2;
            continue;
        }
        if c == b'\r' {
            if i + 2 < n && g(z, i + 2) == b'\n' {
                i += 3;
            } else {
                i += 2;
            }
            continue;
        }
        if c == 0xe2 && i + 3 < n && g(z, i + 2) == 0x80 && matches!(g(z, i + 3), 0xa8 | 0xa9) {
            i += 4;
            continue;
        }
        break;
    }
    i
}

/// The pin's `jsonUnescapeOneChar` over `z` (starting at a
/// backslash): (bytes consumed, code point).
fn unescape_one(z: &[u8]) -> (usize, u32) {
    let n = z.len();
    let mut z = z;
    let mut skipped = 0usize;
    loop {
        if n - skipped < 2 {
            return (n, INVALID_CHAR);
        }
        let c = g(z, 1);
        let r = match c {
            b'u' => {
                if z.len() < 6 {
                    return (n, INVALID_CHAR);
                }
                let v = hex4(z, 2);
                if v & 0xfc00 == 0xd800
                    && z.len() >= 12
                    && g(z, 6) == b'\\'
                    && g(z, 7) == b'u'
                    && hex4(z, 8) & 0xfc00 == 0xdc00
                {
                    let lo = hex4(z, 8);
                    (12, ((v & 0x3ff) << 10) + (lo & 0x3ff) + 0x10000)
                } else {
                    (6, v)
                }
            }
            b'b' => (2, 8),
            b'f' => (2, 12),
            b'n' => (2, 10),
            b'r' => (2, 13),
            b't' => (2, 9),
            b'v' => (2, 11),
            b'0' => (
                2,
                if z.len() > 2 && is_digit(g(z, 2)) {
                    INVALID_CHAR
                } else {
                    0
                },
            ),
            b'\'' | b'"' | b'/' | b'\\' => (2, u32::from(c)),
            b'x' => {
                if z.len() < 4 {
                    return (n, INVALID_CHAR);
                }
                (4, (hex_val(g(z, 2)) << 4) | hex_val(g(z, 3)))
            }
            0xe2 | b'\r' | b'\n' => {
                let skip = bytes_to_bypass(z);
                if skip == 0 {
                    return (n, INVALID_CHAR);
                }
                if skip == z.len() {
                    return (n, 0);
                }
                if g(z, skip) == b'\\' {
                    skipped += skip;
                    z = z.get(skip..).unwrap_or(&[]);
                    continue;
                }
                let (k, v) = utf8_read(z.get(skip..).unwrap_or(&[]));
                (skip + k, v)
            }
            _ => (2, INVALID_CHAR),
        };
        return (skipped + r.0, r.1);
    }
}

fn push_utf8(out: &mut AVec<u8>, v: u32) {
    if v <= 0x7f {
        out.push(v as u8);
    } else if v <= 0x7ff {
        out.push((0xc0 | (v >> 6)) as u8);
        out.push((0x80 | (v & 0x3f)) as u8);
    } else if v < 0x10000 {
        out.push((0xe0 | (v >> 12)) as u8);
        out.push((0x80 | ((v >> 6) & 0x3f)) as u8);
        out.push((0x80 | (v & 0x3f)) as u8);
    } else if v == INVALID_CHAR {
    } else {
        out.push((0xf0 | (v >> 18)) as u8);
        out.push((0x80 | ((v >> 12) & 0x3f)) as u8);
        out.push((0x80 | ((v >> 6) & 0x3f)) as u8);
        out.push((0x80 | (v & 0x3f)) as u8);
    }
}

/// A TEXTJ / TEXT5 payload decoded to raw text.
fn unescape(z: &[u8]) -> AVec<u8> {
    let mut out = AVec::with_capacity(z.len());
    let mut i = 0usize;
    while i < z.len() {
        let c = g(z, i);
        if c == b'\\' {
            let (k, v) = unescape_one(z.get(i..).unwrap_or(&[]));
            push_utf8(&mut out, v);
            i += k.max(1);
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The pin's `sqlite3Atoi64` over `z` (an unsigned decimal):
/// (code, value) — 0 ok, 1 extra text, 2 too big, 3 exactly
/// 9223372036854775808, -1 no digits.
fn atoi64(z: &[u8], neg: bool) -> (i32, i64) {
    let mut i = 0usize;
    while i < z.len() && is_space(g(z, i)) {
        i += 1;
    }
    let mut neg = neg;
    if g(z, i) == b'-' && i < z.len() {
        neg = !neg;
        i += 1;
    } else if g(z, i) == b'+' && i < z.len() {
        i += 1;
    }
    let start = i;
    while i < z.len() && g(z, i) == b'0' {
        i += 1;
    }
    let digits_at = i;
    let mut u = 0u64;
    let mut nd = 0usize;
    while digits_at + nd < z.len() && is_digit(g(z, digits_at + nd)) {
        u = u
            .wrapping_mul(10)
            .wrapping_add(u64::from(g(z, digits_at + nd) - b'0'));
        nd += 1;
    }
    let mut v = if u > i64::MAX as u64 {
        if neg { i64::MIN } else { i64::MAX }
    } else if neg {
        -(u as i64)
    } else {
        u as i64
    };
    let mut rc = 0;
    if nd == 0 && start == digits_at {
        rc = -1;
    } else if digits_at + nd < z.len() && (digits_at + nd..z.len()).any(|k| !is_space(g(z, k))) {
        rc = 1;
    }
    if nd < 19 {
        return (rc, v);
    }
    let c = if nd > 19 {
        1
    } else {
        let d = slice(z, digits_at, 19);
        let p = b"9223372036854775808";
        match d.cmp(&p[..]) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }
    };
    if c < 0 {
        return (rc, v);
    }
    v = if neg { i64::MIN } else { i64::MAX };
    if c > 0 {
        (2, v)
    } else if neg {
        (rc, v)
    } else {
        (3, v)
    }
}

/// The pin's `sqlite3AtoF` used as a validity test: the whole text
/// (ASCII spaces around it allowed) must be one decimal number.
fn atof(z: &[u8]) -> Option<f64> {
    let mut a = 0usize;
    let mut e = z.len();
    while a < e && is_space(g(z, a)) {
        a += 1;
    }
    while e > a && is_space(g(z, e - 1)) {
        e -= 1;
    }
    let t = slice(z, a, e - a);
    let mut i = 0usize;
    if matches!(g(t, 0), b'+' | b'-') && !t.is_empty() {
        i = 1;
    }
    let mut digits = 0;
    while i < t.len() && is_digit(g(t, i)) {
        i += 1;
        digits += 1;
    }
    if i < t.len() && g(t, i) == b'.' {
        i += 1;
        while i < t.len() && is_digit(g(t, i)) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < t.len() && matches!(g(t, i), b'e' | b'E') {
        let mut j = i + 1;
        if j < t.len() && matches!(g(t, j), b'+' | b'-') {
            j += 1;
        }
        let s = j;
        while j < t.len() && is_digit(g(t, j)) {
            j += 1;
        }
        if j == s {
            return None;
        }
        i = j;
    }
    if i != t.len() {
        return None;
    }
    crate::sql::lexer::parse_real_signed(t)
}

/// The pin's `jsonReturnFromBlob` for the element at `i`: the SQL
/// value (an array / object as JSON text, or as JSONB when
/// `blob`).
fn sql_value(b: &[u8], i: usize, blob: bool) -> R<OwnedValue> {
    let (n, sz) = payload(b, i);
    if n == 0 {
        return Err(SchemaKind::JsonMalformed);
    }
    let p = i + n;
    let z = slice(b, p, sz);
    Ok(match g(b, i) & 0x0f {
        T_NULL => OwnedValue::Null,
        T_TRUE => OwnedValue::Integer(1),
        T_FALSE => OwnedValue::Integer(0),
        T_INT | T_INT5 => {
            if sz == 0 {
                return Err(SchemaKind::JsonMalformed);
            }
            let (neg, digits) = if g(z, 0) == b'-' {
                if sz < 2 {
                    return Err(SchemaKind::JsonMalformed);
                }
                (true, z.get(1..).unwrap_or(&[]))
            } else {
                (false, z)
            };
            // The payload is read as a C string.
            let digits = match digits.iter().position(|c| *c == 0) {
                Some(k) => digits.get(..k).unwrap_or(&[]),
                None => digits,
            };
            match dec_or_hex(digits) {
                (0, v) if v < 0 => {
                    let r = (v as u64) as f64;
                    OwnedValue::Real(if neg { -r } else { r })
                }
                (0, v) => OwnedValue::Integer(if neg { v.wrapping_neg() } else { v }),
                (3, _) if neg => OwnedValue::Integer(i64::MIN),
                (1, _) => return Err(SchemaKind::JsonMalformed),
                _ => match atof(cstr(z)) {
                    Some(r) => OwnedValue::Real(r),
                    None => return Err(SchemaKind::JsonMalformed),
                },
            }
        }
        T_FLOAT | T_FLOAT5 => {
            if sz == 0 {
                return Err(SchemaKind::JsonMalformed);
            }
            match atof(cstr(z)) {
                Some(r) => OwnedValue::Real(r),
                None => return Err(SchemaKind::JsonMalformed),
            }
        }
        T_TEXT | T_TEXTRAW => {
            let mut t = AVec::new();
            t.extend_from_slice(z);
            OwnedValue::Text(t)
        }
        T_TEXTJ | T_TEXT5 => OwnedValue::Text(unescape(z)),
        T_ARRAY | T_OBJECT => {
            if blob {
                let mut t = AVec::new();
                t.extend_from_slice(slice(b, i, n + sz));
                OwnedValue::Blob(t)
            } else {
                OwnedValue::Text(render_doc(b, i)?)
            }
        }
        _ => return Err(SchemaKind::JsonMalformed),
    })
}

fn cstr(z: &[u8]) -> &[u8] {
    match z.iter().position(|c| *c == 0) {
        Some(k) => z.get(..k).unwrap_or(&[]),
        None => z,
    }
}

/// The pin's `sqlite3DecOrHexToI64`.
fn dec_or_hex(z: &[u8]) -> (i32, i64) {
    if g(z, 0) == b'0' && matches!(g(z, 1), b'x' | b'X') {
        let mut i = 2;
        while g(z, i) == b'0' {
            i += 1;
        }
        let mut k = i;
        let mut u = 0u64;
        while is_xdigit(g(z, k)) {
            u = u.wrapping_mul(16).wrapping_add(u64::from(hex_val(g(z, k))));
            k += 1;
        }
        let v = u as i64;
        if k - i > 16 {
            return (2, v);
        }
        if k < z.len() {
            return (1, v);
        }
        return (0, v);
    }
    let mut n = 0;
    while n < z.len() && matches!(g(z, n), b'+' | b'-' | b' ' | b'\n' | b'\t' | b'0'..=b'9') {
        n += 1;
    }
    if n < z.len() {
        n += 1;
    }
    atoi64(slice(z, 0, n), false)
}

// ---------------------------------------------------------------
// Labels and paths.

/// The pin's `jsonLabelCompare`.
fn label_eq(l: &[u8], lraw: bool, r: &[u8], rraw: bool) -> bool {
    if lraw && rraw {
        return l == r;
    }
    let next = |z: &mut &[u8], raw: bool| -> u32 {
        if z.is_empty() {
            return 0;
        }
        if raw || g(z, 0) != b'\\' {
            let c = g(z, 0);
            if c >= 0xc0 {
                let (k, v) = utf8_read(z);
                *z = z.get(k..).unwrap_or(&[]);
                v
            } else {
                *z = z.get(1..).unwrap_or(&[]);
                u32::from(c)
            }
        } else {
            let (k, v) = unescape_one(z);
            *z = z.get(k.min(z.len())..).unwrap_or(&[]);
            v
        }
    };
    let (mut a, mut b) = (l, r);
    loop {
        let ca = next(&mut a, lraw);
        let cb = next(&mut b, rraw);
        if ca != cb {
            return false;
        }
        if ca == 0 {
            return true;
        }
    }
}

/// What a path lookup ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Found,
    NotFound,
    PathErr,
    Malformed,
    TooDeep,
    NotArray,
}

/// How a lookup edits what it finds (the pin's `JEDIT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Del,
    Repl,
    Ins,
    Set,
    AIns,
}

impl Edit {
    fn inserts(self) -> bool {
        matches!(self, Edit::Ins | Edit::Set | Edit::AIns)
    }
}

/// One step of a path: an object key, or an array element.
enum Step<'p> {
    Key(&'p [u8], bool),
    Index,
}

#[derive(Clone, Copy)]
enum IndexSpec {
    At(u32),
    /// `[#]` or `[#-N]`.
    FromEnd(u32),
}

/// Parse the key step after a `.` at `p` (the pin's rules):
/// (key bytes, raw, bytes consumed after the dot) or a path error.
fn parse_key(path: &[u8], p: usize) -> Option<(&[u8], bool, usize)> {
    if g(path, p) == b'"' {
        let mut i = 1usize;
        loop {
            let c = g(path, p + i);
            if c == 0 || c == b'"' {
                break;
            }
            if c == b'\\' && g(path, p + i + 1) != 0 {
                i += 1;
            }
            i += 1;
        }
        let nkey = i - 1;
        if g(path, p + i) == 0 {
            return None;
        }
        let key = slice(path, p + 1, nkey);
        Some((key, !key.contains(&b'\\'), i + 1))
    } else {
        let mut i = 0usize;
        while !matches!(g(path, p + i), 0 | b'.' | b'[') {
            i += 1;
        }
        if i == 0 {
            return None;
        }
        Some((slice(path, p, i), true, i))
    }
}

/// Parse an index step at `p` (a `[`): (spec, bytes consumed) or
/// a path error.
fn parse_index(path: &[u8], p: usize) -> Option<(IndexSpec, usize)> {
    let mut k = 0u32;
    let mut i = 1usize;
    while is_digit(g(path, p + i)) {
        k = k
            .wrapping_mul(10)
            .wrapping_add(u32::from(g(path, p + i) - b'0'));
        i += 1;
    }
    if i < 2 || g(path, p + i) != b']' {
        if g(path, p + 1) != b'#' {
            return None;
        }
        i = 2;
        let mut nn = 0u32;
        if g(path, p + 2) == b'-' && is_digit(g(path, p + 3)) {
            i = 3;
            while is_digit(g(path, p + i)) {
                nn = nn
                    .wrapping_mul(10)
                    .wrapping_add(u32::from(g(path, p + i) - b'0'));
                i += 1;
            }
        }
        if g(path, p + i) != b']' {
            return None;
        }
        return Some((IndexSpec::FromEnd(nn), i + 1));
    }
    Some((IndexSpec::At(k), i + 1))
}

/// A missing member an inserting edit would create: the container,
/// the insertion point, the key (its offset in the path, length and
/// rawness; None for an array element), the offset of the path's
/// rest.
type Missing = (usize, usize, Option<(usize, usize, bool)>, usize);

/// The result of walking a path (the pin's `jsonLookupStep` up to
/// the point where it would edit).
struct Walk {
    look: Look,
    /// The element found, and its label (0 when in an array).
    root: usize,
    label: usize,
    /// The containers above `root` (outermost first).
    anc: AVec<usize>,
    /// A missing member that an inserting edit would create: the
    /// container, the insertion point, the key (None: an array
    /// element), and the path after this step.
    missing: Option<Missing>,
    /// The last path byte before the end (`zPath[-1]`).
    last: u8,
    depth: usize,
}

/// Walk `path` (after its `$`) through `b`, `depth` lookup calls
/// deep already.
fn walk(b: &[u8], path: &[u8], depth0: usize) -> Walk {
    let mut w = Walk {
        look: Look::Found,
        root: 0,
        label: 0,
        anc: AVec::new(),
        missing: None,
        last: b'$',
        depth: depth0,
    };
    let mut p = 0usize;
    loop {
        w.depth += 1;
        if w.depth > MAX_DEPTH {
            w.look = Look::TooDeep;
            return w;
        }
        let c = g(path, p);
        if c == 0 {
            w.look = Look::Found;
            return w;
        }
        let root = w.root;
        if c == b'.' {
            let x = g(b, root) & 0x0f;
            let Some((key, raw, used)) = parse_key(path, p + 1) else {
                w.look = Look::PathErr;
                return w;
            };
            if x != T_OBJECT {
                w.look = Look::NotFound;
                return w;
            }
            let (n, sz) = payload(b, root);
            let mut j = root + n;
            let end = j + sz;
            let mut hit = None;
            while j < end {
                let x = g(b, j) & 0x0f;
                if !(T_TEXT..=T_TEXTRAW).contains(&x) {
                    w.look = Look::Malformed;
                    return w;
                }
                let (n, sz) = payload(b, j);
                if n == 0 {
                    w.look = Look::Malformed;
                    return w;
                }
                let k = j + n;
                if k + sz >= end {
                    w.look = Look::Malformed;
                    return w;
                }
                let lraw = x == T_TEXT || x == T_TEXTRAW;
                if label_eq(key, raw, slice(b, k, sz), lraw) {
                    let v = k + sz;
                    if g(b, v) & 0x0f > T_OBJECT {
                        w.look = Look::Malformed;
                        return w;
                    }
                    let (n2, sz2) = payload(b, v);
                    if n2 == 0 || v + n2 + sz2 > end {
                        w.look = Look::Malformed;
                        return w;
                    }
                    hit = Some((v, j));
                    break;
                }
                j = k + sz;
                if g(b, j) & 0x0f > T_OBJECT {
                    w.look = Look::Malformed;
                    return w;
                }
                let (n2, sz2) = payload(b, j);
                if n2 == 0 {
                    w.look = Look::Malformed;
                    return w;
                }
                j += n2 + sz2;
            }
            match hit {
                Some((v, lab)) => {
                    w.anc.push(root);
                    w.root = v;
                    w.label = lab;
                    p += 1 + used;
                    w.last = g(path, p - 1);
                    continue;
                }
                None => {
                    if j > end {
                        w.look = Look::Malformed;
                        return w;
                    }
                    let kpos = if g(path, p + 1) == b'"' { p + 2 } else { p + 1 };
                    w.missing = Some((root, j, Some((kpos, key.len(), raw)), p + 1 + used));
                    w.look = Look::NotFound;
                    return w;
                }
            }
        } else if c == b'[' {
            let x = g(b, root) & 0x0f;
            if x != T_ARRAY {
                w.look = Look::NotFound;
                return w;
            }
            let (n, sz) = payload(b, root);
            let Some((spec, used)) = parse_index(path, p) else {
                w.look = Look::PathErr;
                return w;
            };
            let mut k = match spec {
                IndexSpec::At(k) => k,
                IndexSpec::FromEnd(nn) => {
                    let cnt = array_count(b, root);
                    if nn > cnt {
                        w.look = Look::NotFound;
                        return w;
                    }
                    cnt - nn
                }
            };
            let mut j = root + n;
            let end = j + sz;
            let mut hit = None;
            while j < end {
                if k == 0 {
                    hit = Some(j);
                    break;
                }
                k -= 1;
                let (n2, sz2) = payload(b, j);
                if n2 == 0 {
                    w.look = Look::Malformed;
                    return w;
                }
                j += n2 + sz2;
            }
            match hit {
                Some(e) => {
                    w.anc.push(root);
                    w.root = e;
                    w.label = 0;
                    p += used;
                    w.last = g(path, p - 1);
                    continue;
                }
                None => {
                    if j > end {
                        w.look = Look::Malformed;
                        return w;
                    }
                    if k > 0 {
                        w.look = Look::NotFound;
                        return w;
                    }
                    w.missing = Some((root, j, None, p + used));
                    w.last = b']';
                    w.look = Look::NotFound;
                    return w;
                }
            }
        } else {
            w.look = Look::PathErr;
            return w;
        }
    }
}

/// A read-only lookup: (outcome, element, label).
fn lookup(b: &[u8], path: &[u8]) -> (Look, usize, usize) {
    let w = walk(b, path, 0);
    (w.look, w.root, w.label)
}

/// The substructure an inserting edit creates for the path `tail`
/// (the pin's `jsonCreateEditSubstructure`), its first lookup call
/// at depth `depth + 1`: the containers the remaining steps name,
/// minimal headers, `ins` innermost.
fn substructure(path: &[u8], tail: usize, ins: &[u8], depth: usize) -> Result<AVec<u8>, Look> {
    // The steps, outermost first.
    let mut steps: AVec<Step<'_>> = AVec::new();
    let mut p = tail;
    let mut d = depth;
    while g(path, p) != 0 {
        d += 1;
        if d > MAX_DEPTH {
            return Err(Look::TooDeep);
        }
        if g(path, p) == b'.' {
            let Some((key, raw, used)) = parse_key(path, p + 1) else {
                return Err(Look::PathErr);
            };
            steps.push(Step::Key(key, raw));
            p += 1 + used;
        } else if g(path, p) == b'[' {
            let Some((spec, used)) = parse_index(path, p) else {
                return Err(Look::PathErr);
            };
            match spec {
                IndexSpec::At(0) | IndexSpec::FromEnd(0) => {}
                _ => return Err(Look::NotFound),
            }
            steps.push(Step::Index);
            p += used;
        } else {
            return Err(Look::PathErr);
        }
    }
    let mut inner: AVec<u8> = AVec::new();
    inner.extend_from_slice(ins);
    for s in steps.iter().rev() {
        let mut body: AVec<u8> = AVec::new();
        let t = match s {
            Step::Key(key, raw) => {
                append_node(
                    &mut body,
                    if *raw { T_TEXTRAW } else { T_TEXT5 },
                    key.len(),
                    Some(key),
                );
                T_OBJECT
            }
            Step::Index => T_ARRAY,
        };
        body.extend_from_slice(inner.as_slice());
        let mut c = AVec::new();
        append_node(&mut c, t, body.len(), Some(body.as_slice()));
        inner = c;
    }
    Ok(inner)
}

/// Walk `path` through `doc` and apply `edit` with the value
/// `ins` (the pin's `jsonLookupStep` in edit mode).
fn edit_at(doc: &mut AVec<u8>, path: &[u8], edit: Edit, ins: &[u8]) -> Look {
    let w = walk(doc.as_slice(), path, 0);
    let mut delta: isize = 0;
    match w.look {
        Look::Found => {
            let (n, sz) = payload(doc.as_slice(), w.root);
            let mut at = w.root;
            let mut len = n + sz;
            match edit {
                Edit::Del => {
                    if w.label > 0 {
                        len += w.root - w.label;
                        at = w.label;
                    }
                    blob_edit(doc, at, len, None, 0, &mut delta);
                }
                Edit::Ins => {}
                Edit::AIns => {
                    if w.last != b']' {
                        return Look::NotArray;
                    }
                    blob_edit(doc, at, 0, Some(ins), ins.len(), &mut delta);
                }
                Edit::Repl | Edit::Set => blob_edit(doc, at, len, Some(ins), ins.len(), &mut delta),
            }
        }
        Look::NotFound if edit.inserts() && w.missing.is_some() => {
            let Some((container, at, key, tail)) = w.missing else {
                return Look::NotFound;
            };
            // json_array_insert() creates only array elements: the
            // pin checks the path's last byte before anything else.
            if edit == Edit::AIns && cstr(path).last() != Some(&b']') {
                return Look::NotArray;
            }
            let v = if g(path, tail) == 0 {
                let mut v = AVec::new();
                v.extend_from_slice(ins);
                v
            } else {
                match substructure(path, tail, ins, w.depth) {
                    Ok(v) => v,
                    Err(l) => return l,
                }
            };
            let mut add: AVec<u8> = AVec::new();
            if let Some((kpos, klen, raw)) = key {
                append_node(
                    &mut add,
                    if raw { T_TEXTRAW } else { T_TEXT5 },
                    klen,
                    Some(slice(path, kpos, klen)),
                );
            }
            add.extend_from_slice(v.as_slice());
            blob_edit(doc, at, 0, Some(add.as_slice()), add.len(), &mut delta);
            if delta != 0 {
                adjust(doc, container, &mut delta);
            }
            for a in w.anc.iter().rev() {
                if delta != 0 {
                    adjust(doc, *a, &mut delta);
                }
            }
            return Look::Found;
        }
        other => return other,
    }
    for a in w.anc.iter().rev() {
        if delta != 0 {
            adjust(doc, *a, &mut delta);
        }
    }
    Look::Found
}

// ---------------------------------------------------------------
// Validity.

/// The pin's `jsonFuncArgMightBeBinary`: the blob's root header
/// spans it exactly.
fn might_be_binary(b: &[u8]) -> bool {
    if b.is_empty() || g(b, 0) & 0x0f > T_OBJECT {
        return false;
    }
    let (n, sz) = payload(b, 0);
    if n == 0 || n + sz != b.len() {
        return false;
    }
    !(g(b, 0) & 0x0f <= T_FALSE && sz > 0)
}

/// The pin's `jsonArgIsJsonb`: `might_be_binary`, and a short blob
/// that reads like JSON text must also pass the strict check.
fn is_jsonb(b: &[u8]) -> bool {
    if !might_be_binary(b) {
        return false;
    }
    let c = g(b, 0);
    let (_, sz) = payload(b, 0);
    sz > 7 || (c != 0x7b && c != 0x5b && !is_digit(c)) || validity(b) == 0
}

#[derive(Clone, Copy)]
struct VFrame {
    obj: bool,
    j: usize,
    k: usize,
    cnt: u32,
    depth: usize,
}

/// The pin's `jsonbValidityCheck` of the whole blob: 0, or one
/// more than the offset of the first problem.
fn validity(b: &[u8]) -> usize {
    let mut st: AVec<VFrame> = AVec::new();
    let (mut i, mut iend, mut depth) = (0usize, b.len(), 1usize);
    loop {
        match check_node(b, i, iend, depth) {
            Err(p) => return p,
            Ok(Some((obj, j, k))) => st.push(VFrame {
                obj,
                j,
                k,
                cnt: 0,
                depth,
            }),
            Ok(None) => {}
        }
        loop {
            let Some(f) = st.as_mut_slice().last_mut() else {
                return 0;
            };
            if f.j < f.k {
                let (n, sz) = payload(b, f.j);
                if n == 0 || f.j + n + sz > f.k {
                    return f.j + 1;
                }
                if f.obj && f.cnt & 1 == 0 {
                    let x = g(b, f.j) & 0x0f;
                    if !(T_TEXT..=T_TEXTRAW).contains(&x) {
                        return f.j + 1;
                    }
                }
                i = f.j;
                iend = f.j + n + sz;
                depth = f.depth + 1;
                f.cnt += 1;
                f.j += n + sz;
                break;
            }
            if f.obj && f.cnt & 1 != 0 {
                return f.j + 1;
            }
            st.pop();
        }
    }
}

/// One element's own check: Err(position + 1), or the container's
/// payload range to descend into.
fn check_node(
    b: &[u8],
    i: usize,
    iend: usize,
    depth: usize,
) -> Result<Option<(bool, usize, usize)>, usize> {
    if depth > MAX_DEPTH {
        return Err(i + 1);
    }
    let (n, sz) = payload(b, i);
    if n == 0 || i + n + sz != iend {
        return Err(i + 1);
    }
    let x = g(b, i) & 0x0f;
    let z = b;
    match x {
        T_NULL | T_TRUE | T_FALSE => {
            if n + sz == 1 {
                Ok(None)
            } else {
                Err(i + 1)
            }
        }
        T_INT => {
            if sz < 1 {
                return Err(i + 1);
            }
            let mut j = i + n;
            if g(z, j) == b'-' {
                j += 1;
                if sz < 2 {
                    return Err(i + 1);
                }
            }
            let k = i + n + sz;
            while j < k {
                if is_digit(g(z, j)) {
                    j += 1;
                } else {
                    return Err(j + 1);
                }
            }
            Ok(None)
        }
        T_INT5 => {
            if sz < 3 {
                return Err(i + 1);
            }
            let mut j = i + n;
            if g(z, j) == b'-' {
                if sz < 4 {
                    return Err(i + 1);
                }
                j += 1;
            }
            if g(z, j) != b'0' {
                return Err(i + 1);
            }
            if !matches!(g(z, j + 1), b'x' | b'X') {
                return Err(j + 2);
            }
            j += 2;
            let k = i + n + sz;
            while j < k {
                if is_xdigit(g(z, j)) {
                    j += 1;
                } else {
                    return Err(j + 1);
                }
            }
            Ok(None)
        }
        T_FLOAT | T_FLOAT5 => {
            let mut seen = 0u8;
            if sz < 2 {
                return Err(i + 1);
            }
            let mut j = i + n;
            let k = j + sz;
            if g(z, j) == b'-' {
                j += 1;
                if sz < 3 {
                    return Err(i + 1);
                }
            }
            if g(z, j) == b'.' {
                if x == T_FLOAT || !is_digit(g(z, j + 1)) {
                    return Err(j + 1);
                }
                j += 2;
                seen = 1;
            } else if g(z, j) == b'0' && x == T_FLOAT {
                if j + 3 > k {
                    return Err(j + 1);
                }
                if !matches!(g(z, j + 1), b'.' | b'e' | b'E') {
                    return Err(j + 1);
                }
                j += 1;
            }
            while j < k {
                let c = g(z, j);
                if is_digit(c) {
                    j += 1;
                    continue;
                }
                if c == b'.' {
                    if seen > 0 {
                        return Err(j + 1);
                    }
                    if x == T_FLOAT && (j == k - 1 || !is_digit(g(z, j + 1))) {
                        return Err(j + 1);
                    }
                    seen = 1;
                    j += 1;
                    continue;
                }
                if c == b'e' || c == b'E' {
                    if seen == 2 || j == k - 1 {
                        return Err(j + 1);
                    }
                    if matches!(g(z, j + 1), b'+' | b'-') {
                        j += 1;
                        if j == k - 1 {
                            return Err(j + 1);
                        }
                    }
                    seen = 2;
                    j += 1;
                    continue;
                }
                return Err(j + 1);
            }
            if seen == 0 {
                return Err(i + 1);
            }
            Ok(None)
        }
        T_TEXT => {
            let mut j = i + n;
            let k = j + sz;
            while j < k {
                let c = g(z, j);
                if !is_ok(c) && c != b'\'' {
                    return Err(j + 1);
                }
                j += 1;
            }
            Ok(None)
        }
        T_TEXTJ | T_TEXT5 => {
            let mut j = i + n;
            let k = j + sz;
            while j < k {
                let c = g(z, j);
                if !is_ok(c) && c != b'\'' {
                    if c == b'"' || c <= 0x1f {
                        // Raw in a JSON5 string only.
                        if x == T_TEXTJ {
                            return Err(j + 1);
                        }
                    } else if c != b'\\' || j + 1 >= k {
                        return Err(j + 1);
                    } else if matches!(
                        g(z, j + 1),
                        0 | b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        j += 1;
                    } else if g(z, j + 1) == b'u' {
                        if j + 5 >= k || !is_4hex(z, j + 2) {
                            return Err(j + 1);
                        }
                        j += 1;
                    } else if x != T_TEXT5 {
                        return Err(j + 1);
                    } else {
                        let (sz_c, cp) = unescape_one(slice(z, j, k - j));
                        if cp == INVALID_CHAR {
                            return Err(j + 1);
                        }
                        j += sz_c.saturating_sub(1);
                    }
                }
                j += 1;
            }
            Ok(None)
        }
        T_TEXTRAW => Ok(None),
        T_ARRAY | T_OBJECT => Ok(Some((x == T_OBJECT, i + n, i + n + sz))),
        _ => Err(i + 1),
    }
}

// ---------------------------------------------------------------
// json_patch.

#[derive(Clone, Copy)]
struct MFrame {
    target: usize,
    pc: usize,
    pend: usize,
    tstart: usize,
    tend_be: usize,
    saved: isize,
}

/// The pin's `jsonMergePatch` (RFC 7396) of `p` into `t`.
fn merge_patch(t: &mut AVec<u8>, p: &[u8]) -> R<()> {
    let bad = SchemaKind::JsonMalformed;
    let mut st: AVec<MFrame> = AVec::new();
    let mut delta: isize = 0;
    let mut call = Some((0usize, 0usize));
    let mut returned = false;
    loop {
        if let Some((it, ip)) = call.take() {
            if st.len() > MAX_DEPTH {
                return Err(SchemaKind::JsonNestedTooDeep);
            }
            if g(p, ip) & 0x0f != T_OBJECT {
                let (n, sz) = payload(p, ip);
                let patch = slice(p, ip, n + sz);
                let (n, sz) = payload_d(t.as_slice(), it, delta);
                blob_edit(t, it, n + sz, Some(patch), patch.len(), &mut delta);
                returned = true;
            } else {
                if g(t.as_slice(), it) & 0x0f != T_OBJECT {
                    let (n, sz) = payload_d(t.as_slice(), it, delta);
                    blob_edit(t, it + n, sz, None, 0, &mut delta);
                    let x = g(t.as_slice(), it);
                    if let Some(h) = t.as_mut_slice().get_mut(it) {
                        *h = (x & 0xf0) | T_OBJECT;
                    }
                }
                let (n, sz) = payload(p, ip);
                if n == 0 {
                    return Err(bad);
                }
                let pc = ip + n;
                let pend = pc + sz;
                let (n, sz) = payload_d(t.as_slice(), it, delta);
                if n == 0 {
                    return Err(bad);
                }
                st.push(MFrame {
                    target: it,
                    pc,
                    pend,
                    tstart: it + n,
                    tend_be: it + n + sz,
                    saved: 0,
                });
                returned = false;
            }
        }
        let Some(f) = st.as_mut_slice().last_mut() else {
            return Ok(());
        };
        if returned {
            delta += f.saved;
            returned = false;
        }
        if f.pc >= f.pend {
            let target = f.target;
            st.pop();
            if delta != 0 {
                adjust(t, target, &mut delta);
            }
            returned = true;
            continue;
        }
        // The next patch member.
        let plabel = f.pc;
        let ep = g(p, plabel) & 0x0f;
        if !(T_TEXT..=T_TEXTRAW).contains(&ep) {
            return Err(bad);
        }
        let (npl, szpl) = payload(p, plabel);
        if npl == 0 {
            return Err(bad);
        }
        let pvalue = plabel + npl + szpl;
        if pvalue >= f.pend {
            return Err(bad);
        }
        let (npv, szpv) = payload(p, pvalue);
        if npv == 0 {
            return Err(bad);
        }
        f.pc = pvalue + npv + szpv;
        if f.pc > f.pend {
            return Err(bad);
        }
        let tend = (f.tend_be as isize + delta).max(0) as usize;
        let mut tc = f.tstart;
        let tb = t.as_slice();
        let mut found = None;
        while tc < tend {
            let tlabel = tc;
            let et = g(tb, tc) & 0x0f;
            if !(T_TEXT..=T_TEXTRAW).contains(&et) {
                return Err(bad);
            }
            let (ntl, sztl) = payload_d(tb, tc, delta);
            if ntl == 0 {
                return Err(bad);
            }
            let tvalue = tlabel + ntl + sztl;
            if tvalue >= tend {
                return Err(bad);
            }
            let (ntv, sztv) = payload_d(tb, tvalue, delta);
            if ntv == 0 || tvalue + ntv + sztv > tend {
                return Err(bad);
            }
            if label_eq(
                slice(p, plabel + npl, szpl),
                ep == T_TEXT || ep == T_TEXTRAW,
                slice(tb, tlabel + ntl, sztl),
                et == T_TEXT || et == T_TEXTRAW,
            ) {
                found = Some((tlabel, ntl + sztl, tvalue, ntv + sztv));
                break;
            }
            tc = tvalue + ntv + sztv;
        }
        let x = g(p, pvalue) & 0x0f;
        match found {
            Some((tlabel, llen, tvalue, vlen)) => {
                if x == T_NULL {
                    blob_edit(t, tlabel, llen + vlen, None, 0, &mut delta);
                } else {
                    f.saved = delta;
                    delta = 0;
                    call = Some((tvalue, pvalue));
                }
            }
            None if x != T_NULL => {
                let label = slice(p, plabel, npl + szpl);
                if x != T_OBJECT {
                    let mut add = AVec::new();
                    add.extend_from_slice(label);
                    add.extend_from_slice(slice(p, pvalue, npv + szpv));
                    blob_edit(t, tend, 0, Some(add.as_slice()), add.len(), &mut delta);
                } else {
                    let mut add = AVec::new();
                    add.extend_from_slice(label);
                    add.push(T_NULL);
                    blob_edit(t, tend, 0, Some(add.as_slice()), add.len(), &mut delta);
                    f.saved = delta;
                    delta = 0;
                    call = Some((tend + label.len(), pvalue));
                }
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------
// SQL values into JSON.

fn text_of(v: &OwnedValue) -> AVec<u8> {
    crate::query::to_text(v).unwrap_or_default()
}

/// The pin's `jsonAppendSqlValue`: one SQL value as JSON text
/// (`sub`: it carries the JSON subtype).
fn append_sql_value(out: &mut AVec<u8>, v: &OwnedValue, sub: bool) -> R<()> {
    match v {
        OwnedValue::Null => out.extend_from_slice(b"null"),
        OwnedValue::Real(r) => {
            if r.is_infinite() {
                if *r < 0.0 {
                    out.push(b'-');
                }
                out.extend_from_slice(b"9.0e+999");
            } else if r.is_nan() {
                out.extend_from_slice(b"null");
            } else {
                crate::record::render_real(*r, out);
            }
        }
        OwnedValue::Integer(i) => crate::record::render_int(*i, out),
        OwnedValue::Text(t) => {
            if sub {
                out.extend_from_slice(t.as_slice());
            } else {
                append_string(out, t.as_slice());
            }
        }
        OwnedValue::Blob(b) => {
            if is_jsonb(b.as_slice()) {
                render(b.as_slice(), 0, out)?;
            } else {
                return Err(SchemaKind::JsonBlobValue);
            }
        }
    }
    Ok(())
}

/// The pin's `jsonAppendSeparator`.
fn separator(out: &mut AVec<u8>) {
    match out.as_slice().last() {
        None | Some(b'[') | Some(b'{') => {}
        _ => out.push(b','),
    }
}

/// The pin's `jsonFunctionArgToBlob`: one SQL value as a JSONB
/// element.
fn arg_to_blob(v: &OwnedValue, sub: bool) -> R<AVec<u8>> {
    let mut b = AVec::new();
    match v {
        OwnedValue::Null => b.push(T_NULL),
        OwnedValue::Blob(x) => {
            if is_jsonb(x.as_slice()) {
                b.extend_from_slice(x.as_slice());
            } else {
                return Err(SchemaKind::JsonBlobValue);
            }
        }
        OwnedValue::Text(t) => {
            if sub {
                return text_to_doc(t.as_slice())
                    .map(|d| d.b)
                    .map_err(|_| SchemaKind::JsonMalformed);
            }
            append_node(&mut b, T_TEXTRAW, t.len(), Some(t.as_slice()));
        }
        OwnedValue::Real(r) => {
            if r.is_nan() {
                b.push(T_NULL);
            } else {
                let z = text_of(v);
                let z = z.as_slice();
                if g(z, 0) == b'I' {
                    append_node(&mut b, T_FLOAT, 5, Some(b"9e999"));
                } else if g(z, 0) == b'-' && g(z, 1) == b'I' {
                    append_node(&mut b, T_FLOAT, 6, Some(b"-9e999"));
                } else {
                    let _ = r;
                    append_node(&mut b, T_FLOAT, z.len(), Some(z));
                }
            }
        }
        OwnedValue::Integer(_) => {
            let z = text_of(v);
            append_node(&mut b, T_INT, z.len(), Some(z.as_slice()));
        }
    }
    Ok(b)
}

/// The pin's `jsonParseFuncArg`: a JSON argument as a document
/// (None for NULL); text that is not JSON is "malformed JSON".
fn parse_arg(v: &OwnedValue) -> R<Option<Doc>> {
    match parse_arg_keep(v) {
        None => Ok(None),
        Some(Ok(d)) => Ok(Some(d)),
        Some(Err(_)) => Err(SchemaKind::JsonMalformed),
    }
}

fn parse_arg_keep(v: &OwnedValue) -> Option<core::result::Result<Doc, usize>> {
    if let OwnedValue::Blob(b) = v
        && is_jsonb(b.as_slice())
    {
        let mut c = AVec::new();
        c.extend_from_slice(b.as_slice());
        return Some(Ok(Doc {
            b: c,
            nonstd: false,
        }));
    }
    let t = crate::query::to_text(v)?;
    if t.is_empty() {
        return Some(Err(0));
    }
    Some(text_to_doc(t.as_slice()))
}

/// A path argument as C text (None: NULL).
fn path_arg(v: &OwnedValue) -> Option<AVec<u8>> {
    let t = crate::query::to_text(v)?;
    let mut out = AVec::new();
    out.extend_from_slice(cstr(t.as_slice()));
    Some(out)
}

fn look_err(l: Look) -> SchemaKind {
    match l {
        Look::Malformed => SchemaKind::JsonMalformed,
        Look::TooDeep => SchemaKind::JsonPathTooDeep,
        Look::NotArray => SchemaKind::JsonNotArrayElement,
        _ => SchemaKind::JsonBadPath,
    }
}

fn text_result(t: AVec<u8>) -> (OwnedValue, bool) {
    (OwnedValue::Text(t), true)
}

/// The pin's `jsonReturnParse`: a document as the result.
fn return_doc(d: Doc, blob: bool) -> R<(OwnedValue, bool)> {
    if blob {
        return Ok((OwnedValue::Blob(d.b), false));
    }
    Ok(text_result(render_doc(d.b.as_slice(), 0)?))
}

/// The pin's `jsonReturnString`: built JSON text as the result.
fn return_text(t: AVec<u8>, blob: bool) -> (OwnedValue, bool) {
    if blob {
        return (OwnedValue::Blob(text_to_blob_loose(t.as_slice())), false);
    }
    text_result(t)
}

// ---------------------------------------------------------------
// The SQL functions.

/// What a JSON scalar function computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `json(X)` / `jsonb(X)`.
    Json,
    /// `json_array(...)`.
    Array,
    /// `json_object(label, value, ...)`.
    Object,
    /// `json_extract(X, path, ...)`.
    Extract,
    /// `->`.
    Arrow,
    /// `->>`.
    Arrow2,
    /// `json_insert(X, path, value, ...)`.
    Insert,
    /// `json_replace(X, path, value, ...)`.
    Replace,
    /// `json_set(X, path, value, ...)`.
    Set,
    /// `json_remove(X, path, ...)`.
    Remove,
    /// `json_array_insert(X, path, value, ...)`.
    ArrayInsert,
    /// `json_patch(T, P)`.
    Patch,
    /// `json_type(X[, path])`.
    Type,
    /// `json_valid(X[, flags])`.
    Valid,
    /// `json_quote(X)`.
    Quote,
    /// `json_array_length(X[, path])`.
    ArrayLength,
    /// `json_error_position(X)`.
    ErrorPosition,
    /// `json_pretty(X[, indent])`.
    Pretty,
    /// A `json_each` / `json_tree` `value` column (argument 0, its
    /// `type` column argument 1): the value, with the JSON subtype
    /// when it is an array or object.
    EachValue,
    /// A `json_each` / `json_tree` `key` column: an object member's
    /// label decoded.
    EachKey,
    /// A `json_each` / `json_tree` hidden `json` column: the call's
    /// first argument.
    EachJson,
    /// A value that carries the JSON subtype (a JSON aggregate's
    /// window result).
    Subtyped,
    /// One `json_group_array` row's element text.
    Frag,
    /// One `json_group_object` row's `"label":value` text (NULL
    /// for a NULL label).
    ObjFrag,
}

/// A JSON scalar function: what it does and whether it is the
/// `jsonb_` form (JSONB results).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonFn {
    /// What it computes.
    pub op: Op,
    /// The `jsonb_` form: a JSON result is JSONB.
    pub blob: bool,
}

impl Op {
    /// A `json_each` / `json_tree` column read (`each_column`).
    pub fn is_each_column(self) -> bool {
        matches!(self, Op::EachValue | Op::EachKey | Op::EachJson)
    }
}

impl JsonFn {
    /// The function a name calls (case-insensitive).
    pub fn lookup(name: &[u8]) -> Option<JsonFn> {
        const NAMES: &[(&[u8], Op, bool)] = &[
            (b"json", Op::Json, false),
            (b"jsonb", Op::Json, true),
            (b"json_array", Op::Array, false),
            (b"jsonb_array", Op::Array, true),
            (b"json_object", Op::Object, false),
            (b"jsonb_object", Op::Object, true),
            (b"json_extract", Op::Extract, false),
            (b"jsonb_extract", Op::Extract, true),
            (b"->", Op::Arrow, false),
            (b"->>", Op::Arrow2, false),
            (b"json_insert", Op::Insert, false),
            (b"jsonb_insert", Op::Insert, true),
            (b"json_replace", Op::Replace, false),
            (b"jsonb_replace", Op::Replace, true),
            (b"json_set", Op::Set, false),
            (b"jsonb_set", Op::Set, true),
            (b"json_remove", Op::Remove, false),
            (b"jsonb_remove", Op::Remove, true),
            (b"json_array_insert", Op::ArrayInsert, false),
            (b"jsonb_array_insert", Op::ArrayInsert, true),
            (b"json_patch", Op::Patch, false),
            (b"jsonb_patch", Op::Patch, true),
            (b"json_type", Op::Type, false),
            (b"json_valid", Op::Valid, false),
            (b"json_quote", Op::Quote, false),
            (b"json_array_length", Op::ArrayLength, false),
            (b"json_error_position", Op::ErrorPosition, false),
            (b"json_pretty", Op::Pretty, false),
        ];
        NAMES
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, op, blob)| JsonFn {
                op: *op,
                blob: *blob,
            })
    }

    /// Whether every text result carries the JSON subtype (an
    /// aggregate min / max of the call keeps it too).
    pub fn always_json(self) -> bool {
        !self.blob
            && matches!(
                self.op,
                Op::Json
                    | Op::Array
                    | Op::Object
                    | Op::Arrow
                    | Op::Insert
                    | Op::Replace
                    | Op::Set
                    | Op::Remove
                    | Op::ArrayInsert
                    | Op::Patch
                    | Op::Quote
            )
    }

    /// The argument counts the pin accepts (outside them:
    /// "wrong number of arguments" at prepare).
    pub fn arity(self) -> (usize, usize) {
        match self.op {
            Op::Json
            | Op::Quote
            | Op::ErrorPosition
            | Op::Subtyped
            | Op::Frag
            | Op::EachKey
            | Op::EachJson => (1, 1),
            Op::Arrow | Op::Arrow2 | Op::Patch | Op::EachValue | Op::ObjFrag => (2, 2),
            Op::Type | Op::Valid | Op::ArrayLength | Op::Pretty => (1, 2),
            _ => (0, usize::MAX),
        }
    }
}

/// Evaluate a JSON scalar function over its arguments (each with
/// whether it carries the JSON subtype): the result and its
/// subtype.
pub fn eval(f: JsonFn, args: &[(OwnedValue, bool)]) -> R<(OwnedValue, bool)> {
    let null = (OwnedValue::Null, false);
    let arg = |i: usize| args.get(i).map(|a| &a.0).unwrap_or(&OwnedValue::Null);
    let r = match f.op {
        Op::Json => match parse_arg(arg(0))? {
            None => null,
            Some(d) => return_doc(d, f.blob)?,
        },
        Op::Array => {
            let mut out = AVec::new();
            out.push(b'[');
            for (v, s) in args {
                separator(&mut out);
                append_sql_value(&mut out, v, *s)?;
            }
            out.push(b']');
            return_text(out, f.blob)
        }
        Op::Object => {
            if args.len() % 2 == 1 {
                return Err(SchemaKind::JsonObjectOddArgs);
            }
            let mut out = AVec::new();
            out.push(b'{');
            for pair in args.chunks(2) {
                let (Some((k, _)), Some((v, s))) = (pair.first(), pair.get(1)) else {
                    break;
                };
                let OwnedValue::Text(k) = k else {
                    return Err(SchemaKind::JsonObjectLabel);
                };
                separator(&mut out);
                append_string(&mut out, k.as_slice());
                out.push(b':');
                append_sql_value(&mut out, v, *s)?;
            }
            out.push(b'}');
            return_text(out, f.blob)
        }
        Op::Extract | Op::Arrow | Op::Arrow2 => extract(f, args)?,
        Op::Insert | Op::Replace | Op::Set | Op::ArrayInsert => {
            let edit = match f.op {
                Op::Insert => Edit::Ins,
                Op::Replace => Edit::Repl,
                Op::Set => Edit::Set,
                _ => Edit::AIns,
            };
            if args.is_empty() {
                return Ok(null);
            }
            if args.len().is_multiple_of(2) {
                return Err(SchemaKind::JsonOddArgs);
            }
            let Some(mut d) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            for pair in args.get(1..).unwrap_or(&[]).chunks(2) {
                let (Some((pv, _)), Some((v, s))) = (pair.first(), pair.get(1)) else {
                    break;
                };
                let Some(path) = path_arg(pv) else {
                    continue;
                };
                if g(path.as_slice(), 0) != b'$' {
                    return Err(SchemaKind::JsonBadPath);
                }
                let ins = arg_to_blob(v, *s)?;
                if g(path.as_slice(), 1) == 0 {
                    if matches!(edit, Edit::Repl | Edit::Set) {
                        let mut delta = 0;
                        let len = d.b.len();
                        blob_edit(
                            &mut d.b,
                            0,
                            len,
                            Some(ins.as_slice()),
                            ins.len(),
                            &mut delta,
                        );
                    }
                    continue;
                }
                match edit_at(
                    &mut d.b,
                    path.as_slice().get(1..).unwrap_or(&[]),
                    edit,
                    ins.as_slice(),
                ) {
                    Look::Found | Look::NotFound => {}
                    l => return Err(look_err(l)),
                }
            }
            return_doc(d, f.blob)?
        }
        Op::Remove => {
            let Some(mut d) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            for (pv, _) in args.get(1..).unwrap_or(&[]) {
                let Some(path) = path_arg(pv) else {
                    return Ok(null);
                };
                if g(path.as_slice(), 0) != b'$' {
                    return Err(SchemaKind::JsonBadPath);
                }
                if g(path.as_slice(), 1) == 0 {
                    return Ok(null);
                }
                match edit_at(
                    &mut d.b,
                    path.as_slice().get(1..).unwrap_or(&[]),
                    Edit::Del,
                    &[],
                ) {
                    Look::Found | Look::NotFound => {}
                    l => return Err(look_err(l)),
                }
            }
            return_doc(d, f.blob)?
        }
        Op::Patch => {
            let Some(mut t) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            let Some(p) = parse_arg(arg(1))? else {
                return Ok(null);
            };
            merge_patch(&mut t.b, p.b.as_slice())?;
            return_doc(t, f.blob)?
        }
        Op::Type => {
            let Some(d) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            let mut at = 0;
            if args.len() == 2 {
                let Some(path) = path_arg(arg(1)) else {
                    return Ok(null);
                };
                if g(path.as_slice(), 0) != b'$' {
                    return Err(SchemaKind::JsonBadPath);
                }
                match lookup(d.b.as_slice(), path.as_slice().get(1..).unwrap_or(&[])) {
                    (Look::Found, i, _) => at = i,
                    (Look::NotFound, ..) => return Ok(null),
                    (l, ..) => return Err(look_err(l)),
                }
            }
            let t = TYPE_NAMES
                .get(usize::from(g(d.b.as_slice(), at) & 0x0f))
                .copied()
                .unwrap_or(b"");
            let mut v = AVec::new();
            v.extend_from_slice(t);
            (OwnedValue::Text(v), false)
        }
        Op::Valid => valid(args)?,
        Op::Quote => {
            let mut out = AVec::new();
            let (v, s) = args.first().cloned().unwrap_or((OwnedValue::Null, false));
            append_sql_value(&mut out, &v, s)?;
            text_result(out)
        }
        Op::ArrayLength => {
            let Some(d) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            let mut at = 0;
            if args.len() == 2 {
                let Some(path) = path_arg(arg(1)) else {
                    return Ok(null);
                };
                let p: &[u8] = if g(path.as_slice(), 0) == b'$' {
                    path.as_slice().get(1..).unwrap_or(&[])
                } else {
                    b"@"
                };
                match lookup(d.b.as_slice(), p) {
                    (Look::Found, i, _) => at = i,
                    (Look::NotFound, ..) => return Ok(null),
                    (l, ..) => return Err(look_err(l)),
                }
            }
            let n = if g(d.b.as_slice(), at) & 0x0f == T_ARRAY {
                array_count(d.b.as_slice(), at)
            } else {
                0
            };
            (OwnedValue::Integer(i64::from(n)), false)
        }
        Op::ErrorPosition => {
            let v = arg(0);
            if let OwnedValue::Blob(b) = v
                && is_jsonb(b.as_slice())
            {
                return Ok((OwnedValue::Integer(validity(b.as_slice()) as i64), false));
            }
            let Some(t) = crate::query::to_text(v) else {
                return Ok(null);
            };
            let pos = match text_to_doc(t.as_slice()) {
                Ok(_) => 0i64,
                Err(e) => {
                    let mut k = 0i64;
                    for c in t.as_slice().iter().take(e) {
                        if *c == 0 {
                            break;
                        }
                        if c & 0xc0 != 0x80 {
                            k += 1;
                        }
                    }
                    k + 1
                }
            };
            (OwnedValue::Integer(pos), false)
        }
        Op::Pretty => {
            let Some(d) = parse_arg(arg(0))? else {
                return Ok(null);
            };
            let ind = match args.get(1).and_then(|a| crate::query::to_text(&a.0)) {
                Some(t) => {
                    let mut v = AVec::new();
                    v.extend_from_slice(cstr(t.as_slice()));
                    v
                }
                None => {
                    let mut v = AVec::new();
                    v.extend_from_slice(b"    ");
                    v
                }
            };
            (
                OwnedValue::Text(pretty(d.b.as_slice(), ind.as_slice())?),
                false,
            )
        }
        // Read through `each_column`, with the call's document.
        Op::EachValue | Op::EachKey | Op::EachJson => return Err(SchemaKind::JsonMalformed),
        Op::Subtyped => (arg(0).clone(), matches!(arg(0), OwnedValue::Text(_))),
        Op::Frag => {
            let (v, s) = args.first().cloned().unwrap_or((OwnedValue::Null, false));
            let mut out = AVec::new();
            append_sql_value(&mut out, &v, s)?;
            (OwnedValue::Text(out), false)
        }
        Op::ObjFrag => {
            let Some(k) = crate::query::to_text(arg(0)) else {
                return Ok(null);
            };
            let mut out = AVec::new();
            append_string(&mut out, cstr(k.as_slice()));
            out.push(b':');
            let (v, s) = args.get(1).cloned().unwrap_or((OwnedValue::Null, false));
            append_sql_value(&mut out, &v, s)?;
            (OwnedValue::Text(out), false)
        }
    };
    Ok(r)
}

/// `json_valid(X[, FLAGS])`.
fn valid(args: &[(OwnedValue, bool)]) -> R<(OwnedValue, bool)> {
    let mut flags = 1i64;
    if let Some((fv, _)) = args.get(1) {
        let f = crate::func::int64_value(fv);
        if !(1..=15).contains(&f) {
            return Err(SchemaKind::JsonValidFlags);
        }
        flags = f;
    }
    let v = args.first().map(|a| &a.0).unwrap_or(&OwnedValue::Null);
    if matches!(v, OwnedValue::Null) {
        return Ok((OwnedValue::Null, false));
    }
    let mut res = 0;
    let mut text = true;
    if let OwnedValue::Blob(b) = v
        && is_jsonb(b.as_slice())
    {
        text = false;
        if flags & 4 != 0 {
            res = 1;
        } else if flags & 8 != 0 {
            res = i64::from(validity(b.as_slice()) == 0);
        }
    }
    if text && flags & 3 != 0 {
        match parse_arg_keep(v) {
            Some(Ok(d)) => {
                if flags & 2 != 0 || !d.nonstd {
                    res = 1;
                }
            }
            _ => res = 0,
        }
    }
    Ok((OwnedValue::Integer(res), false))
}

/// `json_extract` / `jsonb_extract` / `->` / `->>`.
fn extract(f: JsonFn, args: &[(OwnedValue, bool)]) -> R<(OwnedValue, bool)> {
    let null = (OwnedValue::Null, false);
    if args.len() < 2 {
        return Ok(null);
    }
    let Some(d) = parse_arg(args.first().map(|a| &a.0).unwrap_or(&OwnedValue::Null))? else {
        return Ok(null);
    };
    let b = d.b.as_slice();
    let multi = args.len() > 2;
    let mut out = AVec::new();
    if multi {
        out.push(b'[');
    }
    let abbrev = matches!(f.op, Op::Arrow | Op::Arrow2);
    for (pv, _) in args.get(1..).unwrap_or(&[]) {
        let Some(path) = path_arg(pv) else {
            return Ok(null);
        };
        let path = path.as_slice();
        let full: AVec<u8>;
        let p: &[u8] = if g(path, 0) == b'$' {
            path.get(1..).unwrap_or(&[])
        } else if abbrev {
            let mut x = AVec::new();
            if matches!(pv, OwnedValue::Integer(_)) {
                x.push(b'[');
                if g(path, 0) == b'-' {
                    x.push(b'#');
                }
                x.extend_from_slice(path);
                x.push(b']');
            } else if path.iter().all(|c| is_alnum(*c) || *c == b'_') {
                x.push(b'.');
                x.extend_from_slice(path);
            } else if g(path, 0) == b'[' && path.len() >= 3 && path.last() == Some(&b']') {
                x.extend_from_slice(path);
            } else {
                x.extend_from_slice(b".\"");
                x.extend_from_slice(path);
                x.push(b'"');
            }
            full = x;
            full.as_slice()
        } else {
            return Err(SchemaKind::JsonBadPath);
        };
        match lookup(b, p) {
            (Look::Found, j, _) => {
                if !multi {
                    if f.op == Op::Arrow {
                        return Ok(text_result(render_doc(b, j)?));
                    }
                    let v = sql_value(b, j, f.blob)?;
                    let container = g(b, j) & 0x0f >= T_ARRAY;
                    let sub = container && !f.blob && f.op == Op::Extract;
                    return Ok((v, sub));
                }
                separator(&mut out);
                render(b, j, &mut out)?;
            }
            (Look::NotFound, ..) => {
                if !multi {
                    return Ok(null);
                }
                separator(&mut out);
                out.extend_from_slice(b"null");
            }
            (l, ..) => return Err(look_err(l)),
        }
    }
    out.push(b']');
    Ok(return_text(out, f.blob))
}

// ---------------------------------------------------------------
// Aggregates.

/// The JSON aggregates: `json_group_array` / `json_group_object`
/// and their `jsonb_` forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonAgg {
    /// `json_group_object` (else `json_group_array`).
    pub object: bool,
    /// The `jsonb_` form.
    pub blob: bool,
}

impl JsonAgg {
    /// The aggregate a name calls (case-insensitive).
    pub fn lookup(name: &[u8]) -> Option<JsonAgg> {
        const NAMES: &[(&[u8], bool, bool)] = &[
            (b"json_group_array", false, false),
            (b"jsonb_group_array", false, true),
            (b"json_group_object", true, false),
            (b"jsonb_group_object", true, true),
        ];
        NAMES
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, object, blob)| JsonAgg {
                object: *object,
                blob: *blob,
            })
    }

    /// The argument count.
    pub fn nargs(self) -> usize {
        if self.object { 2 } else { 1 }
    }

    /// The per-row wrapper of the arguments: each stepped row's
    /// JSON text (`Frag` / `ObjFrag`), which the fold joins.
    pub fn frag_fn(self) -> JsonFn {
        JsonFn {
            op: if self.object { Op::ObjFrag } else { Op::Frag },
            blob: false,
        }
    }

    /// The result over the stepped rows' fragments (`Frag` /
    /// `ObjFrag` values, in step order): the pin's accumulated
    /// text, as JSONB for the `jsonb_` forms. The text result
    /// carries the JSON subtype.
    pub fn fold<'a>(self, frags: impl Iterator<Item = &'a OwnedValue>) -> OwnedValue {
        let mut out = AVec::new();
        out.push(if self.object { b'{' } else { b'[' });
        let mut first = true;
        for v in frags {
            let OwnedValue::Text(t) = v else {
                continue;
            };
            if !first {
                out.push(b',');
            }
            first = false;
            out.extend_from_slice(t.as_slice());
        }
        out.push(if self.object { b'}' } else { b']' });
        if self.blob {
            OwnedValue::Blob(text_to_blob_loose(out.as_slice()))
        } else {
            OwnedValue::Text(out)
        }
    }
}

// ---------------------------------------------------------------
// json_each / json_tree.

/// A table-valued function's column names, the last two hidden.
pub const TVF_COLUMNS: [&[u8]; 10] = [
    b"key", b"value", b"type", b"atom", b"id", b"parent", b"fullkey", b"path", b"json", b"root",
];

/// The number of `TVF_COLUMNS` a `*` shows.
pub const TVF_VISIBLE: usize = 8;

/// `json_each` (1), `json_tree` (2), `jsonb_each` (5), `jsonb_tree`
/// (6) by name; 0 for any other.
pub fn tvf_kind(name: &[u8]) -> u8 {
    const NAMES: &[(&[u8], u8)] = &[
        (b"json_each", 1),
        (b"json_tree", 2),
        (b"jsonb_each", 5),
        (b"jsonb_tree", 6),
    ];
    NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map_or(0, |(_, k)| *k)
}

/// One `json_each` / `json_tree` call's document, held while the
/// statement that made the call runs (`Env::json_docs`): its rows
/// name it by index rather than carry it, so a call over a document
/// of n elements costs O(n) memory, not O(n²).
#[derive(Debug)]
pub struct TvfDoc {
    /// The document as JSONB.
    doc: AVec<u8>,
    /// The `json` column when the argument was text (its C-string
    /// prefix); None when it was JSONB, which `doc` holds as given.
    text: Option<AVec<u8>>,
}

/// The documents of the running statement's `json_each` /
/// `json_tree` calls, in call order.
pub type TvfDocs = core::cell::RefCell<AVec<TvfDoc>>;

/// A row's reference to byte `off` of document `id`: 16 bytes,
/// read back only by `each_column` (the key, value, atom and json
/// columns hold nothing else).
fn handle(id: usize, off: usize) -> OwnedValue {
    let mut h = AVec::with_capacity(16);
    h.extend_from_slice(&(id as u64).to_be_bytes());
    h.extend_from_slice(&(off as u64).to_be_bytes());
    OwnedValue::Blob(h)
}

fn unhandle(v: &OwnedValue) -> Option<(usize, usize)> {
    let OwnedValue::Blob(h) = v else {
        return None;
    };
    let h = h.as_slice();
    let id = u64::from_be_bytes(h.get(..8)?.try_into().ok()?);
    let off = u64::from_be_bytes(h.get(8..16)?.try_into().ok()?);
    Some((usize::try_from(id).ok()?, usize::try_from(off).ok()?))
}

/// Read a `json_each` / `json_tree` column (`Op::EachValue`,
/// `EachKey`, `EachJson`; argument 0 the row's value, a handle
/// into `docs`, or a plain value; `EachValue`'s argument 1 the
/// row's `type`): the element decodes now, as the pin's columns are
/// computed only when a query reads them (`count(*)` over a
/// document too deep to render answers). A value that is an array
/// or object keeps the JSON subtype.
pub fn each_column(
    f: JsonFn,
    args: &[(OwnedValue, bool)],
    docs: Option<&TvfDocs>,
) -> R<(OwnedValue, bool)> {
    let arg = |i: usize| args.get(i).map(|a| &a.0).unwrap_or(&OwnedValue::Null);
    let Some((id, off)) = unhandle(arg(0)) else {
        return Ok((arg(0).clone(), false));
    };
    let docs = docs
        .ok_or(SchemaKind::JsonMalformed)?
        .try_borrow()
        .map_err(|_| SchemaKind::JsonMalformed)?;
    let d = docs.as_slice().get(id).ok_or(SchemaKind::JsonMalformed)?;
    let b = d.doc.as_slice();
    if f.op == Op::EachJson {
        let mut t = AVec::new();
        return Ok(match &d.text {
            Some(z) => {
                t.extend_from_slice(z.as_slice());
                (OwnedValue::Text(t), false)
            }
            None => {
                t.extend_from_slice(b);
                (OwnedValue::Blob(t), false)
            }
        });
    }
    let (n, sz) = payload(b, off);
    let elem = slice(b, off, n + sz);
    let container =
        matches!(arg(1), OwnedValue::Text(t) if matches!(t.as_slice(), b"array" | b"object"));
    Ok(if f.op == Op::EachValue && container {
        if f.blob {
            let mut t = AVec::new();
            t.extend_from_slice(elem);
            (OwnedValue::Blob(t), false)
        } else {
            text_result(render_doc(elem, 0)?)
        }
    } else {
        (sql_value(elem, 0, false)?, false)
    })
}

#[derive(Clone, Copy)]
struct Parent {
    head: usize,
    value: usize,
    end: usize,
    key: i64,
    npath: usize,
}

struct Cursor<'a> {
    b: &'a [u8],
    /// The document's index in `TvfDocs`.
    id: usize,
    i: usize,
    end: usize,
    etype: u8,
    parents: AVec<Parent>,
    path: AVec<u8>,
    nroot: usize,
    rowid: i64,
    recursive: bool,
}

impl Cursor<'_> {
    fn skip_label(&self) -> usize {
        if self.etype == T_OBJECT {
            let (n, sz) = payload(self.b, self.i);
            self.i + n + sz
        } else {
            self.i
        }
    }

    /// The pin's `jsonAppendPathName`.
    fn append_path_name(&self, out: &mut AVec<u8>) {
        let Some(top) = self.parents.as_slice().last() else {
            return;
        };
        if self.etype == T_ARRAY {
            out.push(b'[');
            let mut t = AVec::new();
            crate::record::render_int(top.key, &mut t);
            out.extend_from_slice(t.as_slice());
            out.push(b']');
        } else {
            let (n, sz) = payload(self.b, self.i);
            let z = slice(self.b, self.i + n, sz);
            let quote = sz == 0 || !is_alpha(g(z, 0)) || z.iter().any(|c| !is_alnum(*c));
            let z = cstr(z);
            if quote {
                out.extend_from_slice(b".\"");
                out.extend_from_slice(z);
                out.push(b'"');
            } else {
                out.push(b'.');
                out.extend_from_slice(z);
            }
        }
    }

    /// The pin's `jsonEachPathLength`.
    fn path_length(&self) -> usize {
        let z = self.path.as_slice();
        let mut n = z.len();
        if self.rowid == 0 && self.recursive && n >= 2 {
            while n > 1 {
                n -= 1;
                let c = g(z, n);
                if c == b'[' || c == b'.' {
                    let pre = slice(z, 1, n - 1);
                    let (l, x, _) = lookup(self.b, pre);
                    if l != Look::Found {
                        continue;
                    }
                    let (hn, _) = payload(self.b, x);
                    if x + hn == self.i {
                        break;
                    }
                }
            }
        }
        n
    }

    /// Step to the next row; an element inside the document whose
    /// header does not decode is "malformed JSON" (the pin's cursor
    /// could not move past it).
    fn next(&mut self) -> R<()> {
        if self.recursive {
            let mut level = false;
            let i = self.skip_label();
            let x = g(self.b, i) & 0x0f;
            let (n, sz) = payload(self.b, i);
            if n == 0 && i < self.end {
                return Err(SchemaKind::JsonMalformed);
            }
            if x == T_OBJECT || x == T_ARRAY {
                level = true;
                let par = Parent {
                    head: self.i,
                    value: i,
                    end: i + n + sz,
                    key: -1,
                    npath: self.path.len(),
                };
                if self.etype != 0 && !self.parents.is_empty() {
                    let mut name = AVec::new();
                    self.append_path_name(&mut name);
                    self.path.extend_from_slice(name.as_slice());
                }
                self.parents.push(par);
                self.i = i + n;
            } else {
                self.i = i + n + sz;
            }
            while let Some(top) = self.parents.as_slice().last().copied() {
                if self.i < top.end {
                    break;
                }
                self.parents.pop();
                self.path.truncate(top.npath);
                level = true;
            }
            if level {
                self.etype = match self.parents.as_slice().last() {
                    Some(p) => g(self.b, p.value) & 0x0f,
                    None => 0,
                };
            }
        } else {
            let i = self.skip_label();
            let (n, sz) = payload(self.b, i);
            if n == 0 && i < self.end {
                return Err(SchemaKind::JsonMalformed);
            }
            self.i = i + n + sz;
        }
        if let Some(p) = self.parents.as_mut_slice().last_mut() {
            p.key += 1;
        }
        self.rowid += 1;
        Ok(())
    }

    fn text(z: &[u8]) -> OwnedValue {
        let mut t = AVec::new();
        t.extend_from_slice(z);
        OwnedValue::Text(t)
    }

    fn row(&self) -> R<AVec<OwnedValue>> {
        let mut r = AVec::with_capacity(10);
        // key
        r.push(match self.parents.as_slice().last() {
            None => {
                if self.nroot == 1 {
                    OwnedValue::Null
                } else {
                    let j = self.path_length();
                    let n = self.nroot.saturating_sub(j);
                    let z = self.path.as_slice();
                    if n == 0 {
                        OwnedValue::Null
                    } else if g(z, j) == b'[' {
                        OwnedValue::Integer(atoi64(slice(z, j + 1, n - 1), false).1)
                    } else if g(z, j + 1) == b'"' {
                        Self::text(slice(z, j + 2, n.saturating_sub(3)))
                    } else {
                        Self::text(slice(z, j + 1, n - 1))
                    }
                }
            }
            Some(top) => {
                if self.etype == T_OBJECT {
                    // The label, decoded when read (`Op::EachKey`).
                    handle(self.id, self.i)
                } else {
                    OwnedValue::Integer(top.key)
                }
            }
        });
        let vi = self.skip_label();
        let t = g(self.b, vi) & 0x0f;
        // value: the element, decoded when read (`Op::EachValue`),
        // as the pin's columns are computed only when a query reads
        // them. An object whose last label has no value: nothing to
        // read (the pin reads past the blob's end there).
        let elem = if vi >= self.b.len() {
            OwnedValue::Null
        } else {
            handle(self.id, vi)
        };
        r.push(elem.clone());
        // type
        r.push(Self::text(
            TYPE_NAMES.get(usize::from(t)).copied().unwrap_or(b""),
        ));
        // atom
        r.push(if t < T_ARRAY { elem } else { OwnedValue::Null });
        // id
        r.push(OwnedValue::Integer(self.i as i64));
        // parent
        r.push(match self.parents.as_slice().last() {
            Some(p) if self.recursive => OwnedValue::Integer(p.head as i64),
            _ => OwnedValue::Null,
        });
        // fullkey
        let mut fk = AVec::new();
        fk.extend_from_slice(self.path.as_slice());
        if !self.parents.is_empty() {
            self.append_path_name(&mut fk);
        }
        r.push(OwnedValue::Text(fk));
        // path
        r.push(Self::text(slice(
            self.path.as_slice(),
            0,
            self.path_length(),
        )));
        // json (`Op::EachJson`)
        r.push(handle(self.id, 0));
        // root
        r.push(Self::text(slice(self.path.as_slice(), 0, self.nroot)));
        Ok(r)
    }
}

/// The rows of `json_each` / `json_tree` (`kind` as `tvf_kind`)
/// over its arguments (the JSON, then the optional root path):
/// each row's ten `TVF_COLUMNS` values and its rowid, at most
/// `limit` of them. The document joins `docs`, where the rows'
/// key, value, atom and json columns point (`each_column`).
#[allow(clippy::type_complexity)]
pub fn tvf_rows(
    kind: u8,
    args: &[OwnedValue],
    limit: Option<usize>,
    docs: &TvfDocs,
) -> R<AVec<(AVec<OwnedValue>, Option<i64>)>> {
    let mut out = AVec::new();
    let Some(input) = args.first() else {
        return Ok(out);
    };
    let recursive = kind & 3 == 2;
    let (doc, text) = match input {
        OwnedValue::Blob(b) if is_jsonb(b.as_slice()) => {
            let mut c = AVec::new();
            c.extend_from_slice(b.as_slice());
            (c, None)
        }
        _ => {
            let Some(t) = crate::query::to_text(input) else {
                return Ok(out);
            };
            let d = text_to_doc(t.as_slice()).map_err(|_| SchemaKind::JsonMalformed)?;
            let mut z = AVec::new();
            z.extend_from_slice(cstr(t.as_slice()));
            (d.b, Some(z))
        }
    };
    let id = docs
        .try_borrow()
        .map_err(|_| SchemaKind::JsonMalformed)?
        .len();
    let b = doc.as_slice();
    let mut cur = Cursor {
        b,
        id,
        i: 0,
        end: 0,
        etype: 0,
        parents: AVec::new(),
        path: AVec::new(),
        nroot: 1,
        rowid: 0,
        recursive,
    };
    let i;
    if let Some(rv) = args.get(1) {
        let Some(root) = path_arg(rv) else {
            return Ok(out);
        };
        let root = root.as_slice();
        if g(root, 0) != b'$' {
            return Err(SchemaKind::JsonBadPath);
        }
        cur.nroot = root.len();
        if g(root, 1) == 0 {
            i = 0;
        } else {
            match lookup(b, root.get(1..).unwrap_or(&[])) {
                (Look::Found, x, label) => {
                    i = x;
                    if label != 0 {
                        cur.i = label;
                        cur.etype = T_OBJECT;
                    } else {
                        cur.i = x;
                        cur.etype = T_ARRAY;
                    }
                }
                (Look::NotFound, ..) => return Ok(out),
                _ => return Err(SchemaKind::JsonBadPath),
            }
        }
        cur.path.extend_from_slice(root);
    } else {
        i = 0;
        cur.path.push(b'$');
    }
    let (n, sz) = payload(b, i);
    cur.end = i + n + sz;
    let t = g(b, i) & 0x0f;
    if t >= T_ARRAY && !recursive {
        cur.i = i + n;
        cur.etype = t;
        cur.parents.push(Parent {
            head: cur.i,
            value: i,
            end: cur.end,
            key: 0,
            npath: 0,
        });
    }
    while cur.i < cur.end {
        let row = cur.row()?;
        out.push((row, Some(cur.rowid)));
        // A reader that stops early never steps the pin's cursor on.
        if limit.is_some_and(|l| out.len() >= l) {
            break;
        }
        cur.next()?;
    }
    if !out.is_empty() {
        docs.try_borrow_mut()
            .map_err(|_| SchemaKind::JsonMalformed)?
            .push(TvfDoc { doc, text });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexs(b: &[u8]) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for c in b {
            let h = b"0123456789ABCDEF";
            s.push(char::from(g(h, usize::from(c >> 4))));
            s.push(char::from(g(h, usize::from(c & 15))));
        }
        s
    }

    fn blob(t: &str) -> AVec<u8> {
        text_to_doc(t.as_bytes()).expect("valid").b
    }

    #[test]
    fn blobs_match_the_pin() {
        assert_eq!(hexs(blob("[1,2,3]").as_slice()), "6B133113321333");
        assert_eq!(hexs(blob("{\"a\":1}").as_slice()), "4C17611331");
        assert_eq!(hexs(blob("0x1F").as_slice()), "4430783146");
        assert_eq!(hexs(blob("+5").as_slice()), "1335");
        assert_eq!(hexs(blob("-Infinity").as_slice()), "652D3965393939");
        assert_eq!(hexs(blob("{a:1}").as_slice()), "4C17611331");
    }

    #[test]
    fn render_round_trip() {
        let b = blob("[0x1F, +5, .5, 5., -.5, 1e5]");
        let t = render_doc(b.as_slice(), 0).expect("render");
        assert_eq!(t.as_slice(), b"[31,5,0.5,5.0,-0.5,1e5]");
    }

    #[test]
    fn error_offsets() {
        assert_eq!(text_to_doc(b"[1 [x]").err(), Some(3));
        assert_eq!(text_to_doc(b"{\"a\" /**/ [x]}").err(), Some(11));
        assert_eq!(text_to_doc(b"{  ").err(), Some(3));
    }

    #[test]
    fn overwrite_denormalizes() {
        let mut d = blob("[12345]");
        let ins = arg_to_blob(&OwnedValue::Integer(1), false).expect("blob");
        assert_eq!(
            edit_at(&mut d, b"[0]", Edit::Set, ins.as_slice()),
            Look::Found
        );
        assert_eq!(hexs(d.as_slice()), "6BE30000000131");
    }
}
