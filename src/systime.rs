//! The host's wall clock and time zone (feature `std`; v0.16):
//! the defaults `Config::default()` installs for `Config::clock`
//! and (with `datetime`) `Config::tz_offset`. The core itself has
//! neither — a `no_std` build supplies its own hooks or goes
//! without (`Error::NoClock`, UTC).
//!
//! The zone follows the C library's rules for `localtime_r` (as
//! glibc reads them, which is what the pin's `'localtime'` sees on
//! Linux): `TZ` unset is `/etc/localtime`; `TZ` empty is UTC; a
//! leading `:` is dropped; otherwise `TZ` names a TZif file
//! (relative names under `$TZDIR` or `/usr/share/zoneinfo`) or,
//! when no such file reads, is a POSIX TZ string
//! (`EST5EDT,M3.2.0,M11.1.0`); anything unreadable is UTC. A TZif
//! file is read with its 64-bit data and its footer rule for
//! instants past the last transition (RFC 8536). Nothing is cached
//! (there is no global state): each conversion reads the zone
//! afresh, as cheap as one small file read.

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
/// zone a `TZ` value names (`None`: `TZ` unset, the system zone).
pub fn zone_offset(tz: Option<&str>, t: i64) -> i64 {
    let spec = match tz {
        None => "/etc/localtime",
        Some("") => return 0,
        Some(s) => s.strip_prefix(':').unwrap_or(s),
    };
    if let Some(off) = read_tzfile(spec).and_then(|b| tzif_offset(&b, t)) {
        return off;
    }
    if tz.is_none() {
        return 0;
    }
    match parse_posix(spec.as_bytes()) {
        Some(rule) => rule.offset_at(t),
        None => 0,
    }
}

fn read_tzfile(spec: &str) -> Option<Vec<u8>> {
    if spec.is_empty() {
        return None;
    }
    let path = if spec.starts_with('/') {
        String::from(spec)
    } else {
        let dir = std::env::var("TZDIR").unwrap_or_else(|_| String::from("/usr/share/zoneinfo"));
        std::format!("{dir}/{spec}")
    };
    let b = std::fs::read(path).ok()?;
    b.starts_with(b"TZif").then_some(b)
}

fn be32(b: &[u8], at: usize) -> Option<i64> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(i64::from(i32::from_be_bytes([s[0], s[1], s[2], s[3]])))
}

fn be64(b: &[u8], at: usize) -> Option<i64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(i64::from_be_bytes(a))
}

/// The offset a TZif file gives at `t` (glibc's
/// `__tzfile_compute`): before the first transition, the first
/// standard-time type; at or after the last, the footer rule when
/// there is one, else the last type.
fn tzif_offset(b: &[u8], t: i64) -> Option<i64> {
    let header = |at: usize| -> Option<[usize; 6]> {
        let mut c = [0usize; 6];
        for (i, v) in c.iter_mut().enumerate() {
            *v = usize::try_from(be32(b, at + 20 + 4 * i)?).ok()?;
            // Every entry takes a byte at least: a larger count is
            // a malformed file (and keeps the offsets below small).
            if *v > b.len() {
                return None;
            }
        }
        Some(c)
    };
    let c1 = header(0)?;
    let v1_len = c1[3] * 5 + c1[4] * 6 + c1[5] + c1[2] * 8 + c1[1] + c1[0];
    let version = *b.get(4)?;
    let (base, c, tsize, footer) = if version >= b'2' {
        let h2 = 44 + v1_len;
        if b.get(h2..h2 + 4)? != b"TZif" {
            return None;
        }
        let c2 = header(h2)?;
        let data = h2 + 44;
        let len = c2[3] * 9 + c2[4] * 6 + c2[5] + c2[2] * 12 + c2[1] + c2[0];
        let foot = b.get(data + len..).and_then(|f| {
            let f = f.strip_prefix(b"\n")?;
            let end = f.iter().position(|x| *x == b'\n')?;
            f.get(..end).filter(|s| !s.is_empty())
        });
        (data, c2, 8usize, foot)
    } else {
        (44, c1, 4usize, None)
    };
    let (timecnt, typecnt) = (c[3], c[4]);
    let times = base;
    let idxs = times + timecnt * tsize;
    let types = idxs + timecnt;
    let ttinfo = |i: usize| -> Option<(i64, bool)> {
        let at = types + 6 * i;
        Some((be32(b, at)?, *b.get(at + 4)? != 0))
    };
    let trans = |i: usize| -> Option<i64> {
        if tsize == 8 {
            be64(b, times + 8 * i)
        } else {
            be32(b, times + 4 * i)
        }
    };
    if typecnt == 0 {
        return None;
    }
    if timecnt == 0 || t < trans(0)? {
        let mut i = 0;
        while i < typecnt && ttinfo(i)?.1 {
            i += 1;
        }
        if i == typecnt {
            i = 0;
        }
        return Some(ttinfo(i)?.0);
    }
    if t >= trans(timecnt - 1)?
        && let Some(rule) = footer.and_then(parse_posix)
    {
        return Some(rule.offset_at(t));
    }
    // The last transition at or before `t`.
    let (mut lo, mut hi) = (0usize, timecnt);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if trans(mid)? <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let ty = usize::from(*b.get(idxs + lo)?);
    Some(ttinfo(ty)?.0)
}

/// A POSIX TZ rule's change date.
#[derive(Clone, Copy)]
enum Day {
    /// `Jn`: 1..=365, February 29 never counted.
    J1(i64),
    /// `n`: 0..=365, February 29 counted.
    J0(i64),
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of
    /// month `m`.
    M(i64, i64, i64),
}

/// A parsed POSIX TZ string (offsets east of UTC, in seconds).
struct Posix {
    std_off: i64,
    /// The daylight offset and the start / end rules with their
    /// local times of day.
    dst: Option<(i64, Day, i64, Day, i64)>,
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days from 1970-01-01 to January 1st of `y`.
fn days_to_year(y: i64) -> i64 {
    let mut d = 0;
    if y >= 1970 {
        for k in 1970..y {
            d += if is_leap(k) { 366 } else { 365 };
        }
    } else {
        for k in y..1970 {
            d -= if is_leap(k) { 366 } else { 365 };
        }
    }
    d
}

const MON_YDAY: [[i64; 13]; 2] = [
    [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
    [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
];

impl Posix {
    /// The UTC instant of a change in year `y` (glibc's
    /// `compute_change`), `off` being the offset in force before it.
    fn change(day: Day, secs: i64, off: i64, y: i64) -> i64 {
        let mut t = days_to_year(y) * 86_400;
        let leap = usize::from(is_leap(y));
        match day {
            Day::J1(d) => {
                t += (d - 1) * 86_400;
                if d >= 60 && leap == 1 {
                    t += 86_400;
                }
            }
            Day::J0(d) => t += d * 86_400,
            Day::M(m, w, wd) => {
                let mi = usize::try_from(m.clamp(1, 12)).unwrap_or(1);
                let yd = MON_YDAY[leap];
                t += yd[mi - 1] * 86_400;
                // Zeller's congruence: the weekday of the 1st.
                let m1 = (m + 9) % 12 + 1;
                let yy0 = if m <= 2 { y - 1 } else { y };
                let (yy1, yy2) = (yy0 / 100, yy0 % 100);
                let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
                if dow < 0 {
                    dow += 7;
                }
                let mut d = wd - dow;
                if d < 0 {
                    d += 7;
                }
                let mut i = 1;
                while i < w {
                    if d + 7 >= yd[mi] - yd[mi - 1] {
                        break;
                    }
                    d += 7;
                    i += 1;
                }
                t += d * 86_400;
            }
        }
        t - off + secs
    }

    /// The offset at Unix time `t` (glibc's `__tz_compute`, the
    /// rules taken for `t`'s UTC year).
    fn offset_at(&self, t: i64) -> i64 {
        let Some((dst_off, sd, ss, ed, es)) = self.dst else {
            return self.std_off;
        };
        let y = 1970 + t.div_euclid(86_400 * 365);
        // Correct the estimate to `t`'s UTC year.
        let mut y = y;
        while days_to_year(y) * 86_400 > t {
            y -= 1;
        }
        while days_to_year(y + 1) * 86_400 <= t {
            y += 1;
        }
        let start = Self::change(sd, ss, self.std_off, y);
        let end = Self::change(ed, es, dst_off, y);
        let dst = if start > end {
            t < end || t >= start
        } else {
            t >= start && t < end
        };
        if dst { dst_off } else { self.std_off }
    }
}

/// A zone name: `<...>` or three or more letters.
fn tz_name(s: &[u8], i: &mut usize) -> Option<()> {
    if s.get(*i) == Some(&b'<') {
        let end = s.get(*i..)?.iter().position(|c| *c == b'>')?;
        if end < 4 {
            return None;
        }
        *i += end + 1;
        return Some(());
    }
    let start = *i;
    while s.get(*i).is_some_and(|c| c.is_ascii_alphabetic()) {
        *i += 1;
    }
    (*i - start >= 3).then_some(())
}

/// `[+-]hh[:mm[:ss]]` as seconds (sign kept as written); `hours`
/// bounds the hour field.
fn tz_hms(s: &[u8], i: &mut usize, hours: i64) -> Option<i64> {
    let neg = match s.get(*i) {
        Some(b'-') => {
            *i += 1;
            true
        }
        Some(b'+') => {
            *i += 1;
            false
        }
        _ => false,
    };
    let num = |i: &mut usize| -> Option<i64> {
        let start = *i;
        let mut v = 0i64;
        while let Some(c) = s.get(*i).filter(|c| c.is_ascii_digit()) {
            v = v.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
            *i += 1;
        }
        (*i > start).then_some(v)
    };
    let h = num(i)?.min(hours);
    let mut v = h * 3600;
    if s.get(*i) == Some(&b':') {
        *i += 1;
        v += num(i)?.min(59) * 60;
        if s.get(*i) == Some(&b':') {
            *i += 1;
            v += num(i)?.min(59);
        }
    }
    Some(if neg { -v } else { v })
}

fn tz_rule(s: &[u8], i: &mut usize) -> Option<(Day, i64)> {
    let num = |i: &mut usize| -> Option<i64> {
        let start = *i;
        let mut v = 0i64;
        while let Some(c) = s.get(*i).filter(|c| c.is_ascii_digit()) {
            v = v.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
            *i += 1;
        }
        (*i > start).then_some(v)
    };
    let day = match s.get(*i)? {
        b'J' => {
            *i += 1;
            let d = num(i)?;
            if !(1..=365).contains(&d) {
                return None;
            }
            Day::J1(d)
        }
        b'M' => {
            *i += 1;
            let m = num(i)?;
            if s.get(*i) != Some(&b'.') {
                return None;
            }
            *i += 1;
            let w = num(i)?;
            if s.get(*i) != Some(&b'.') {
                return None;
            }
            *i += 1;
            let d = num(i)?;
            if !(1..=12).contains(&m) || !(1..=5).contains(&w) || d > 6 {
                return None;
            }
            Day::M(m, w, d)
        }
        c if c.is_ascii_digit() => {
            let d = num(i)?;
            if d > 365 {
                return None;
            }
            Day::J0(d)
        }
        _ => return None,
    };
    let secs = if s.get(*i) == Some(&b'/') {
        *i += 1;
        tz_hms(s, i, 167)?
    } else {
        7200
    };
    Some((day, secs))
}

/// Parse a POSIX TZ string (glibc's `__tzset_parse_tz`). A
/// daylight name without rules takes the US rules (glibc's
/// fallback when no `posixrules` data applies).
fn parse_posix(s: &[u8]) -> Option<Posix> {
    let mut i = 0;
    tz_name(s, &mut i)?;
    // POSIX offsets are west-positive.
    let std_off = -tz_hms(s, &mut i, 24)?;
    if i == s.len() {
        return Some(Posix { std_off, dst: None });
    }
    tz_name(s, &mut i)?;
    let dst_off = match s.get(i) {
        Some(c) if c.is_ascii_digit() || *c == b'+' || *c == b'-' => -tz_hms(s, &mut i, 24)?,
        _ => std_off + 3600,
    };
    let ((sd, ss), (ed, es)) = if i == s.len() || (s.get(i) == Some(&b',') && i + 1 == s.len()) {
        ((Day::M(3, 2, 0), 7200), (Day::M(11, 1, 0), 7200))
    } else {
        if s.get(i) != Some(&b',') {
            return None;
        }
        i += 1;
        let a = tz_rule(s, &mut i)?;
        if s.get(i) != Some(&b',') {
            return None;
        }
        i += 1;
        let b = tz_rule(s, &mut i)?;
        if i != s.len() {
            return None;
        }
        (a, b)
    };
    Some(Posix {
        std_off,
        dst: Some((dst_off, sd, ss, ed, es)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_rules() {
        let ny = parse_posix(b"EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2024-03-10 06:59:59 UTC is EST, 07:00:00 EDT.
        assert_eq!(ny.offset_at(1_710_053_999), -18_000);
        assert_eq!(ny.offset_at(1_710_054_000), -14_400);
        // 2024-11-03 05:59:59 UTC is EDT, 06:00:00 EST.
        assert_eq!(ny.offset_at(1_730_613_599), -14_400);
        assert_eq!(ny.offset_at(1_730_613_600), -18_000);
        let ist = parse_posix(b"IST-5:30").unwrap();
        assert_eq!(ist.offset_at(0), 19_800);
        let lh = parse_posix(b"<+1030>-10:30<+11>-11,M10.1.0,M4.1.0").unwrap();
        // Southern hemisphere: daylight time in January.
        assert_eq!(lh.offset_at(1_704_067_200), 39_600);
        assert_eq!(lh.offset_at(1_719_792_000), 37_800);
        assert!(parse_posix(b"X5").is_none());
        assert_eq!(zone_offset(Some(""), 0), 0);
    }
}
