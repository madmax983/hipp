//! The host's wall clock and time zone (feature `std`; v0.16):
//! the defaults `Config::default()` installs for `Config::clock`
//! and (with `datetime`) `Config::tz_offset`. The core itself has
//! neither — a `no_std` build supplies its own hooks or goes
//! without (`Error::NoClock`, UTC).
//!
//! The zone is a model of glibc's `localtime_r` (2.39), which is
//! what the pin's `'localtime'` sees on Linux (`time/tzset.c` and
//! `time/tzfile.c`):
//!
//! - `TZ` unset is `/etc/localtime`; `TZ` empty is `Universal`; a
//!   leading `:` is dropped. The value first names a TZif file
//!   (relative names under `$TZDIR`, or `/usr/share/zoneinfo` when
//!   that is unset or empty), read with glibc's checks — a file
//!   glibc rejects (a bad type index, a DST flag other than 0/1,
//!   an abbreviation index past the names, counts that overrun the
//!   file, …) is not used. Its 64-bit data, leap-second
//!   corrections and footer rule (for instants at or past the last
//!   transition) apply, and the transition search is glibc's.
//! - Otherwise the value is a POSIX TZ string, parsed the way
//!   `__tzset_parse_tz` parses it, partial results included: a
//!   malformed or missing rule keeps the offsets already parsed
//!   and whatever the rule parser filled in before it stopped (a
//!   zeroed rule is "January 1st, 00:00"), a missing daylight name
//!   leaves a daylight offset of zero, and a daylight name with no
//!   rules takes the `posixrules` file's transitions shifted to
//!   the string's offsets (falling back to the US rules when that
//!   file does not read).
//! - The daylight-time decision is `__tz_compute`'s: both changes
//!   are computed for the instant's UTC year, and a start later
//!   than the end is a southern-hemisphere rule.
//!
//! Nothing is cached (there is no global state): each conversion
//! reads the zone afresh, as cheap as one small file read.

use std::string::String;
use std::vec::Vec;

/// The system clock in Unix milliseconds (0 if it reads before
/// 1970): the `Config::clock` default under `std`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The process's local-time offset east of UTC, in seconds, at
/// Unix time `t`, per `TZ` (see the module docs): the
/// `Config::tz_offset` default under `std` with `datetime`. Always
/// `Some` (an unreadable zone is UTC, as for the C library).
pub fn local_offset(t: i64) -> Option<i64> {
    let tz = std::env::var_os("TZ").map(|v| v.to_string_lossy().into_owned());
    Some(zone_offset(tz.as_deref(), t))
}

/// The offset east of UTC, in seconds, at Unix time `t` for the
/// zone a `TZ` value names (`None`: `TZ` unset, the system zone),
/// as glibc's `tzset_internal` resolves it.
pub fn zone_offset(tz: Option<&str>, t: i64) -> i64 {
    let spec = match tz {
        None => "/etc/localtime",
        Some("") => "Universal",
        Some(s) => s.strip_prefix(':').unwrap_or(s),
    };
    if let Some(f) = load_tzfile(spec) {
        return f.offset_at(t);
    }
    if tz.is_none() || spec.is_empty() || spec == "/etc/localtime" {
        return 0;
    }
    parse_tz(spec.as_bytes(), false).offset_at(t)
}

// ---------------------------------------------------------------
// TZif files (glibc's `__tzfile_read` / `__tzfile_compute`).
// ---------------------------------------------------------------

#[derive(Clone, Copy)]
struct TType {
    off: i64,
    isdst: bool,
    isstd: bool,
    isgmt: bool,
}

/// A TZif file as glibc keeps it.
struct TzFile {
    trans: Vec<i64>,
    /// The type of each transition (checked to be in range).
    idx: Vec<usize>,
    types: Vec<TType>,
    /// Leap-second records: (transition, cumulative correction).
    leaps: Vec<(i64, i64)>,
    /// The footer TZ string (64-bit data only).
    footer: Option<Vec<u8>>,
}

fn load_tzfile(spec: &str) -> Option<TzFile> {
    if spec.is_empty() {
        return None;
    }
    let path = if spec.starts_with('/') {
        String::from(spec)
    } else {
        let dir = std::env::var("TZDIR")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| String::from("/usr/share/zoneinfo"));
        std::format!("{dir}/{spec}")
    };
    let b = std::fs::read(path).ok()?;
    read_tzif(&b)
}

fn be32(b: &[u8], at: usize) -> Option<i64> {
    let s = b.get(at..at.checked_add(4)?)?;
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    Some(i64::from(i32::from_be_bytes(a)))
}

fn be64(b: &[u8], at: usize) -> Option<i64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(i64::from_be_bytes(a))
}

/// A byte cursor over the file: every read past the end fails
/// (glibc's short `fread`, which loses the file).
struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).and_then(|s| s.first().copied())
    }

    fn int(&mut self, width: usize) -> Option<i64> {
        let s = self.take(width)?;
        if width == 8 { be64(s, 0) } else { be32(s, 0) }
    }
}

/// glibc's `__tzfile_read`, with its checks: `None` where glibc
/// "loses" the file.
fn read_tzif(b: &[u8]) -> Option<TzFile> {
    let mut pos = 0usize;
    let mut width = 4usize;
    let counts = loop {
        let head = b.get(pos..pos.checked_add(44)?)?;
        if head.get(..4)? != b"TZif" {
            return None;
        }
        // `(size_t) decode (...)`: a negative count is huge.
        let mut c = [0u64; 6];
        for (k, v) in c.iter_mut().enumerate() {
            *v = be32(head, 20 + 4 * k)? as u64;
        }
        let [isgmt, isstd, leaps, times, types, chars] = c;
        if isstd > types || isgmt > types {
            return None;
        }
        pos += 44;
        if width == 4 && head.get(4).is_some_and(|v| *v != 0) {
            // Skip the 32-bit data to the second header, as
            // `fseek (f, to_skip, SEEK_CUR)` does (wrapping size_t
            // arithmetic, then a `long` offset).
            width = 8;
            let skip = times
                .wrapping_mul(5)
                .wrapping_add(types.wrapping_mul(6))
                .wrapping_add(chars)
                .wrapping_add(leaps.wrapping_mul(8))
                .wrapping_add(isstd)
                .wrapping_add(isgmt) as i64;
            let to = i64::try_from(pos).ok()?.checked_add(skip)?;
            pos = usize::try_from(to).ok()?;
            continue;
        }
        break c;
    };
    // Every count of the data actually read must fit in the file.
    let mut n = [0usize; 6];
    for (v, c) in n.iter_mut().zip(counts) {
        *v = usize::try_from(c).ok().filter(|v| *v <= b.len())?;
    }
    let [n_isgmt, n_isstd, n_leaps, n_times, n_types, n_chars] = n;
    let mut footer_len = 0usize;
    if width == 8 {
        let rem = b.len().checked_sub(pos)?;
        let data = n_times * 9 + n_types * 6 + n_chars;
        let mut tl = rem.checked_sub(data)?;
        tl = tl.checked_sub(n_leaps * 12)?;
        tl = tl.checked_sub(n_isstd)?;
        if tl == 0 || tl - 1 < n_isgmt {
            return None;
        }
        footer_len = tl - n_isgmt - 1;
        // No byte for the footer's text at all loses the file (a
        // bare "\n\n" is an empty footer, which is merely unused).
        if footer_len == 0 {
            return None;
        }
    }
    let mut c = Cursor { b, pos };
    let mut trans = Vec::with_capacity(n_times);
    for _ in 0..n_times {
        trans.push(c.int(width)?);
    }
    let mut idx = Vec::with_capacity(n_times);
    for _ in 0..n_times {
        let i = usize::from(c.byte()?);
        if i >= n_types {
            return None;
        }
        idx.push(i);
    }
    let mut types = Vec::with_capacity(n_types);
    for _ in 0..n_types {
        let off = c.int(4)?;
        let isdst = c.byte()?;
        let abbr = c.byte()?;
        if isdst > 1 || usize::from(abbr) > n_chars {
            return None;
        }
        types.push(TType {
            off,
            isdst: isdst == 1,
            isstd: false,
            isgmt: false,
        });
    }
    c.take(n_chars)?;
    let mut leaps = Vec::with_capacity(n_leaps);
    for _ in 0..n_leaps {
        let at = c.int(width)?;
        let change = c.int(4)?;
        leaps.push((at, change));
    }
    for ty in types.iter_mut().take(n_isstd) {
        ty.isstd = c.byte()? != 0;
    }
    for ty in types.iter_mut().take(n_isgmt) {
        ty.isgmt = c.byte()? != 0;
    }
    let mut footer = None;
    if footer_len > 0 && c.byte()? == b'\n' {
        let s = c.take(footer_len - 1)?;
        let s = s.split(|x| *x == 0).next().unwrap_or(&[]);
        if !s.is_empty() {
            footer = Some(s.to_vec());
        }
    }
    // glibc reads `types[0]` of a file with no types (or aborts on
    // an assertion); such a file is not modelled.
    if types.is_empty() {
        return None;
    }
    Some(TzFile {
        trans,
        idx,
        types,
        leaps,
        footer,
    })
}

impl TzFile {
    fn ty(&self, i: usize) -> TType {
        self.types
            .get(i)
            .or(self.types.first())
            .copied()
            .unwrap_or(TType {
                off: 0,
                isdst: false,
                isstd: false,
                isgmt: false,
            })
    }

    fn at(&self, i: usize) -> i64 {
        self.trans.get(i).copied().unwrap_or(i64::MAX)
    }

    fn type_of(&self, i: usize) -> TType {
        self.ty(self.idx.get(i).copied().unwrap_or(0))
    }

    /// glibc's `__tzfile_compute` and the leap correction
    /// `__tz_convert` subtracts.
    fn offset_at(&self, t: i64) -> i64 {
        let n = self.trans.len();
        let off = if n == 0 || t < self.at(0) {
            // The first standard-time type (or the first type).
            let i = self.types.iter().position(|x| !x.isdst).unwrap_or(0);
            self.ty(i).off
        } else if t >= self.at(n - 1) {
            match &self.footer {
                Some(f) => parse_tz(f, true).offset_at(t),
                None => self.type_of(n - 1).off,
            }
        } else {
            self.type_of(self.search(t) - 1).off
        };
        // The last leap record at or before `t`.
        let corr = self
            .leaps
            .iter()
            .rev()
            .find(|(at, _)| t >= *at)
            .map_or(0, |(_, c)| *c);
        off.wrapping_sub(corr)
    }

    /// glibc's transition search for `transitions[0] <= t <
    /// transitions[n-1]`: a guess from half a Gregorian year per
    /// transition, a linear walk near it, else a binary search.
    /// The first transition after `t` (1..n).
    fn search(&self, t: i64) -> usize {
        let n = self.trans.len();
        let (mut lo, mut hi) = (0usize, n - 1);
        let guess = (self.at(n - 1).wrapping_sub(t) / 15_778_476) as u64;
        if guess < n as u64 {
            let mut i = n - 1 - guess as usize;
            if t < self.at(i) {
                if i < 10 || t >= self.at(i - 10) {
                    while i > 1 && t < self.at(i - 1) {
                        i -= 1;
                    }
                    return i;
                }
                hi = i - 10;
            } else {
                if i + 10 >= n || t < self.at(i + 10) {
                    while i < n - 1 && t >= self.at(i) {
                        i += 1;
                    }
                    return i;
                }
                lo = i + 10;
            }
        }
        while lo + 1 < hi {
            let i = lo + (hi - lo) / 2;
            if t < self.at(i) {
                hi = i;
            } else {
                lo = i;
            }
        }
        hi.max(1)
    }
}

/// glibc's `__tzfile_default`: a daylight-time TZ string without
/// rules takes `posixrules`' transitions, shifted from the file's
/// offsets to the string's (`std_off` / `dst_off`, east of UTC).
fn tzfile_default(std_off: i64, dst_off: i64) -> Option<TzFile> {
    let mut f = load_tzfile("posixrules")?;
    if f.types.len() < 2 {
        return None;
    }
    // The standard offset the file's rules use: that of the latest
    // transition to standard time (or of the only type). The
    // daylight correction is relative to the string's own daylight
    // offset (no shift): the pin's glibc (2.39) takes it from the
    // `rule_dstoff` its previous `__tzfile_default` left behind, and
    // re-parses the zone on every conversion (`localtime`, with
    // `old_tz` freed on this path), so every conversion but the
    // first of a process sees that. (The first sees 0; fitted on
    // hand-made `posixrules` files.)
    let rule_std = if f.trans.is_empty() {
        f.ty(0).off
    } else {
        (0..f.trans.len())
            .rev()
            .map(|i| f.type_of(i))
            .find(|t| !t.isdst)
            .map_or(0, |t| t.off)
    };
    let rule_dst = dst_off;
    let mut isdst = false;
    for i in 0..f.trans.len() {
        let ty = f.type_of(i);
        if let Some(x) = f.idx.get_mut(i) {
            *x = usize::from(ty.isdst);
        }
        let shift = if ty.isgmt {
            0
        } else if isdst && !ty.isstd {
            dst_off - rule_dst
        } else {
            std_off - rule_std
        };
        if let Some(x) = f.trans.get_mut(i) {
            *x = x.wrapping_add(shift);
        }
        isdst = ty.isdst;
    }
    f.types.truncate(2);
    if let [s, d] = f.types.as_mut_slice() {
        s.off = std_off;
        s.isdst = false;
        d.off = dst_off;
        d.isdst = true;
    }
    Some(f)
}

// ---------------------------------------------------------------
// POSIX TZ strings (glibc's `__tzset_parse_tz` / `__tz_compute`).
// ---------------------------------------------------------------

/// A rule's change-date form; the zeroed rule is `J0` day 0.
#[derive(Clone, Copy, Default, PartialEq)]
enum Kind {
    /// `n`: 0..=365, February 29 counted.
    #[default]
    J0,
    /// `Jn`: 1..=365, February 29 never counted.
    J1,
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of
    /// month `m`.
    M,
}

/// glibc's `tz_rule` (the parts that decide the offset).
#[derive(Clone, Copy, Default)]
struct Rule {
    /// Offset east of UTC, seconds.
    offset: i64,
    kind: Kind,
    m: i64,
    n: i64,
    d: i64,
    /// The local time of day of the change, seconds.
    secs: i64,
}

/// What a TZ string comes to: rules, or (a daylight name without
/// rules) the shifted `posixrules` file.
enum Zone {
    Rules([Rule; 2]),
    File(TzFile),
}

impl Zone {
    fn offset_at(&self, t: i64) -> i64 {
        match self {
            Zone::File(f) => f.offset_at(t),
            Zone::Rules(r) => {
                let y = utc_year(t);
                let c0 = change(&r[0], y);
                let c1 = change(&r[1], y);
                let dst = if c0 > c1 {
                    t < c1 || t >= c0
                } else {
                    t >= c0 && t < c1
                };
                r[usize::from(dst)].offset
            }
        }
    }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days from 1970-01-01 to January 1st of `y` (proleptic
/// Gregorian, any year).
fn days_before_year(y: i64) -> i64 {
    let y1 = y.wrapping_sub(1);
    365i64
        .wrapping_mul(y.wrapping_sub(1970))
        .wrapping_add(y1.div_euclid(4) - 492)
        .wrapping_sub(y1.div_euclid(100) - 19)
        .wrapping_add(y1.div_euclid(400) - 4)
}

/// The UTC year of Unix time `t` (`__offtime (t, 0)`'s).
fn utc_year(t: i64) -> i64 {
    let days = t.div_euclid(86_400);
    let mut y = 1970 + (i128::from(days) * 400 / 146_097) as i64;
    while days_before_year(y) > days {
        y -= 1;
    }
    while days_before_year(y + 1) <= days {
        y += 1;
    }
    y
}

/// glibc's `__mon_yday`, flattened as its two rows sit in memory:
/// a rule month outside 1..=12 (kept from a failed parse) reads
/// past its row, as glibc's does. Reads before the table are 0
/// (as observed on the pin's libc); reads past it are not
/// modelled (0).
const MON_YDAY: [i64; 26] = [
    0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365, //
    0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366,
];

fn mon_yday(k: i64) -> i64 {
    usize::try_from(k)
        .ok()
        .and_then(|k| MON_YDAY.get(k))
        .copied()
        .unwrap_or(0)
}

/// The UTC instant of a rule's change in year `y` (glibc's
/// `compute_change`; January 1st 1970 for years up to 1970).
fn change(rule: &Rule, y: i64) -> i64 {
    let mut t = if y > 1970 {
        days_before_year(y).wrapping_mul(86_400)
    } else {
        0
    };
    match rule.kind {
        Kind::J1 => {
            t = t.wrapping_add((rule.d - 1) * 86_400);
            if rule.d >= 60 && is_leap(y) {
                t = t.wrapping_add(86_400);
            }
        }
        Kind::J0 => t = t.wrapping_add(rule.d * 86_400),
        Kind::M => {
            let row = if is_leap(y) { 13 } else { 0 };
            let cur = mon_yday(row + rule.m);
            let prev = mon_yday(row + rule.m - 1);
            t = t.wrapping_add(prev * 86_400);
            // Zeller's congruence: the weekday of the 1st.
            let m1 = (rule.m + 9) % 12 + 1;
            let yy0 = if rule.m <= 2 { y - 1 } else { y };
            let (yy1, yy2) = (yy0 / 100, yy0 % 100);
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = rule.d - dow;
            if d < 0 {
                d += 7;
            }
            let mut i = 1;
            while i < rule.n {
                if d + 7 >= cur - prev {
                    break;
                }
                d += 7;
                i += 1;
            }
            t = t.wrapping_add(d * 86_400);
        }
    }
    t.wrapping_sub(rule.offset).wrapping_add(rule.secs)
}

/// The byte at `i`, NUL past the end (the C string's terminator).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `sscanf`'s `%hu` at `p`: blanks, a sign, digits (`strtoul`,
/// saturating, a minus negating), truncated to 16 bits; the value
/// and the position after it.
fn scan_hu(s: &[u8], p: usize) -> Option<(i64, usize)> {
    let mut q = p;
    while is_c_space(at(s, q)) {
        q += 1;
    }
    let neg = match at(s, q) {
        b'-' => {
            q += 1;
            true
        }
        b'+' => {
            q += 1;
            false
        }
        _ => false,
    };
    if !at(s, q).is_ascii_digit() {
        return None;
    }
    let mut v = Some(0u64);
    while at(s, q).is_ascii_digit() {
        let d = u64::from(at(s, q) - b'0');
        v = v.and_then(|v| v.checked_mul(10)?.checked_add(d));
        q += 1;
    }
    let v = match v {
        None => u64::MAX,
        Some(v) if neg => v.wrapping_neg(),
        Some(v) => v,
    };
    Some((i64::from(v as u16), q))
}

/// `sscanf (s, "%hu%n:%hu%n:%hu%n", ...)` at `p`: the three fields
/// (those not scanned keep `init`), how many were scanned, and the
/// characters consumed by the last one.
fn scan_hms(s: &[u8], p: usize, init: [i64; 3]) -> ([i64; 3], usize, usize) {
    let mut v = init;
    let mut q = p;
    let mut n = 0;
    let mut end = p;
    for (k, slot) in v.iter_mut().enumerate() {
        if k > 0 {
            if at(s, q) != b':' {
                break;
            }
            q += 1;
        }
        let Some((x, e)) = scan_hu(s, q) else {
            break;
        };
        *slot = x;
        q = e;
        end = e;
        n = k + 1;
    }
    (v, n, end - p)
}

/// glibc's `parse_tzname`: three or more letters, or `<...>` of
/// three or more letters, digits and signs.
fn parse_tzname(s: &[u8], i: &mut usize) -> bool {
    let mut p = *i;
    while at(s, p).is_ascii_alphabetic() {
        p += 1;
    }
    if p - *i < 3 {
        p = *i;
        if at(s, p) != b'<' {
            return false;
        }
        p += 1;
        let start = p;
        while at(s, p).is_ascii_alphanumeric() || matches!(at(s, p), b'+' | b'-') {
            p += 1;
        }
        if at(s, p) != b'>' || p - start < 3 {
            return false;
        }
        p += 1;
    }
    *i = p;
    true
}

/// glibc's `parse_offset`: POSIX offsets are west-positive, kept
/// east-positive.
fn parse_offset(s: &[u8], i: &mut usize, which: usize, r: &mut [Rule; 2]) -> bool {
    let mut p = *i;
    let c = at(s, p);
    if which == 0 && (c == 0 || (c != b'+' && c != b'-' && !c.is_ascii_digit())) {
        return false;
    }
    let sign = if c == b'+' || c == b'-' {
        p += 1;
        if c == b'-' { 1 } else { -1 }
    } else {
        -1
    };
    *i = p;
    let ([hh, mm, ss], n, consumed) = scan_hms(s, p, [0, 0, 0]);
    if n > 0 {
        r[which].offset = sign * (ss.min(59) + mm.min(59) * 60 + hh.min(24) * 3600);
    } else if which == 0 {
        r[0].offset = 0;
        return false;
    } else {
        r[1].offset = r[0].offset + 3600;
    }
    *i = p + consumed;
    true
}

/// glibc's `parse_rule`: what it fills in before failing stays.
fn parse_rule(s: &[u8], i: &mut usize, which: usize, rules: &mut [Rule; 2]) -> bool {
    let mut p = *i;
    if at(s, p) == b',' {
        p += 1;
    }
    let r = &mut rules[which];
    let c = at(s, p);
    if c == b'J' || c.is_ascii_digit() {
        r.kind = if c == b'J' { Kind::J1 } else { Kind::J0 };
        if r.kind == Kind::J1 {
            p += 1;
            if !at(s, p).is_ascii_digit() {
                return false;
            }
        }
        // `strtoul`, saturating.
        let mut d = Some(0u64);
        while at(s, p).is_ascii_digit() {
            let x = u64::from(at(s, p) - b'0');
            d = d.and_then(|d| d.checked_mul(10)?.checked_add(x));
            p += 1;
        }
        let d = d.unwrap_or(u64::MAX);
        if d > 365 || (r.kind == Kind::J1 && d == 0) {
            return false;
        }
        r.d = d as i64;
    } else if c == b'M' {
        r.kind = Kind::M;
        // `sscanf (tz, "M%hu.%hu.%hu%n", ...)`.
        let mut q = p + 1;
        let mut n = 0;
        for k in 0..3 {
            if k > 0 {
                if at(s, q) != b'.' {
                    break;
                }
                q += 1;
            }
            let Some((v, e)) = scan_hu(s, q) else {
                break;
            };
            match k {
                0 => r.m = v,
                1 => r.n = v,
                _ => r.d = v,
            }
            q = e;
            n = k + 1;
        }
        if n != 3 || !(1..=12).contains(&r.m) || !(1..=5).contains(&r.n) || r.d > 6 {
            return false;
        }
        p = q;
    } else if c == 0 {
        // The US rules: M3.2.0 / M11.1.0.
        r.kind = Kind::M;
        (r.m, r.n, r.d) = if which == 0 { (3, 2, 0) } else { (11, 1, 0) };
    } else {
        return false;
    }
    match at(s, p) {
        0 | b',' => r.secs = 7200,
        b'/' => {
            p += 1;
            if at(s, p) == 0 {
                return false;
            }
            let neg = at(s, p) == b'-';
            if neg {
                p += 1;
            }
            let ([hh, mm, ss], _, consumed) = scan_hms(s, p, [2, 0, 0]);
            p += consumed;
            let v = hh * 3600 + mm * 60 + ss;
            r.secs = if neg { -v } else { v };
        }
        _ => return false,
    }
    *i = p;
    true
}

/// glibc's `__tzset_parse_tz` over a zeroed rule pair (UTC). A
/// daylight name without rules takes the `posixrules` file
/// (`__tzfile_default`) when that file reads. For a TZif footer
/// (`footer`), glibc still loads that file but then computes with
/// the rules as they stand — zeroed, both changes near January 1st
/// — and that is modelled; that it also replaces the zone's file
/// for later conversions is not.
fn parse_tz(s: &[u8], footer: bool) -> Zone {
    let mut r = [Rule::default(); 2];
    let mut i = 0;
    if parse_tzname(s, &mut i) && parse_offset(s, &mut i, 0, &mut r) {
        if at(s, i) != 0 {
            if parse_tzname(s, &mut i) {
                parse_offset(s, &mut i, 1, &mut r);
                let bare = at(s, i) == 0 || (at(s, i) == b',' && at(s, i + 1) == 0);
                if bare && let Some(f) = tzfile_default(r[0].offset, r[1].offset) {
                    return if footer {
                        Zone::Rules(r)
                    } else {
                        Zone::File(f)
                    };
                }
            }
            if parse_rule(s, &mut i, 0, &mut r) {
                parse_rule(s, &mut i, 1, &mut r);
            }
        } else {
            r[1].offset = r[0].offset;
        }
    }
    Zone::Rules(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(tz: &str, t: i64) -> i64 {
        parse_tz(tz.as_bytes(), false).offset_at(t)
    }

    #[test]
    fn posix_rules() {
        let ny = "EST5EDT,M3.2.0,M11.1.0";
        // 2024-03-10 06:59:59 UTC is EST, 07:00:00 EDT.
        assert_eq!(off(ny, 1_710_053_999), -18_000);
        assert_eq!(off(ny, 1_710_054_000), -14_400);
        // 2024-11-03 05:59:59 UTC is EDT, 06:00:00 EST.
        assert_eq!(off(ny, 1_730_613_599), -14_400);
        assert_eq!(off(ny, 1_730_613_600), -18_000);
        assert_eq!(off("IST-5:30", 0), 19_800);
        let lh = "<+1030>-10:30<+11>-11,M10.1.0,M4.1.0";
        // Southern hemisphere: daylight time in January.
        assert_eq!(off(lh, 1_704_067_200), 39_600);
        assert_eq!(off(lh, 1_719_792_000), 37_800);
        assert_eq!(off("X5", 0), 0);
        assert_eq!(zone_offset(Some(""), 0), 0);
    }

    #[test]
    fn partial_rules_keep_the_offsets() {
        // 2024-01-15 and 2024-07-15, 12:00 UTC.
        let (jan, jul) = (1_705_320_000, 1_721_044_800);
        // A missing end rule is the US default.
        assert_eq!(off("EST5EDT,M3.2.0", jan), -18_000);
        assert_eq!(off("EST5EDT,M3.2.0", jul), -14_400);
        // Trailing junk after the end rule: kept, at 00:00.
        assert_eq!(off("EST5EDT,M3.2.0,M11.1.0x", jul), -14_400);
        // A failed start rule: both changes near January 1st.
        assert_eq!(off("EST5EDT,J0/2,J365", jul), -18_000);
        assert_eq!(off("EST5EDT,366,0", jul), -14_400);
        // No daylight name: the daylight offset is UTC.
        assert_eq!(off("EST5,M3.2.0,M11.1.0", jul), 0);
        assert_eq!(off("EST5,M3.2.0,M11.1.0", jan), -18_000);
        assert_eq!(off("NZST-12NZDT,M9.5.0,M4.1.0/", jan), 46_800);
    }

    #[test]
    fn hostile_files_never_panic() {
        // Truncations and corruptions of a well-formed v2 file.
        let Ok(b) = std::fs::read("/usr/share/zoneinfo/America/New_York") else {
            return;
        };
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for k in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut m = b.clone();
            if k % 3 == 0 {
                m.truncate((seed as usize) % (b.len() + 1));
            } else if let Some(x) = m.get_mut((seed as usize) % b.len()) {
                *x ^= (seed >> 32) as u8 | 1;
            }
            if let Some(f) = read_tzif(&m) {
                for t in [i64::MIN, -1, 0, 1_700_000_000, 4_000_000_000, i64::MAX] {
                    let _ = f.offset_at(t);
                }
            }
        }
        for t in [i64::MIN, i64::MAX] {
            let _ = off("EST5EDT,M3.2.0,M11.1.0", t);
            let _ = off("A5B,M0.9.9,M99.9.9", t);
        }
    }
}
