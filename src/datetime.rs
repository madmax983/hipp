//! The date and time functions (v0.16, leaf feature `datetime`):
//! `date()`, `time()`, `datetime()`, `julianday()`, `unixepoch()`,
//! `strftime()`, `timediff()` and the `CURRENT_DATE` /
//! `CURRENT_TIME` / `CURRENT_TIMESTAMP` keywords, as SQLite 3.54.1
//! computes them.
//!
//! The engine is a faithful model of the pin's `date.c`, fitted
//! and checked against the pin's output (STATUS.md, "Date and time
//! functions"): a time value is the pin's `DateTime` record — an
//! integer-millisecond Julian day number (`ijd`) plus the civil
//! fields, each half valid or not — and every parser, modifier and
//! formatter updates those fields in the pin's order, with its
//! integer arithmetic (C truncating division and remainder), its
//! `double` arithmetic (core `f64` only: additions, products and
//! quotients, which IEEE 754 makes identical), and its
//! float-to-integer conversions as x86-64 performs them (an
//! out-of-range or NaN value converts to the "integer indefinite",
//! `i64::MIN` / `i32::MIN`). The quirks follow: a date that was
//! invalid before `'localtime'` comes back as a time in year 2000
//! or 0, a negative millisecond prints as the pin's punctuation,
//! a day-of-month overflow survives `'start of day'`.
//!
//! The wall clock and the time zone are caller-supplied hooks
//! (`Config::clock`, `Config::tz_offset`, read through
//! `query::Env`): `'now'` is the clock's instant, read once per
//! statement (the pin's `sqlite3StmtCurrentTime`), and
//! `'localtime'` / `'utc'` ask the zone hook for the offset at an
//! instant, as the pin asks `localtime_r` — on the same equivalent
//! year in 1970..=2037 for instants outside that range. Without a
//! zone hook local time is UTC. Inside a CHECK constraint, an
//! index expression or a generated column a function that would
//! read either fails the statement with
//! `SchemaKind::NonDeterministic` (the pin's "non-deterministic use
//! of date() in ...").

use crate::arena::AVec;
use crate::error::{Error, SchemaKind};
use crate::query::{Env, to_text};
use crate::record::OwnedValue;
use crate::sql::parser::eq_ci;

/// The date and time functions (v0.16, `datetime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateFn {
    Date,
    Time,
    DateTime,
    JulianDay,
    UnixEpoch,
    Strftime,
    TimeDiff,
    /// `CURRENT_TIME` (a keyword; never callable by name).
    CurrentTime,
    /// `CURRENT_DATE`.
    CurrentDate,
    /// `CURRENT_TIMESTAMP`.
    CurrentTimestamp,
}

impl DateFn {
    /// The function a name calls (case-insensitive). The
    /// `current_*` names are reached only from their keywords:
    /// the parser never makes a call of them otherwise (the pin's
    /// `current_date()` is a syntax error).
    pub fn lookup(name: &[u8]) -> Option<DateFn> {
        const NAMES: [(&[u8], DateFn); 10] = [
            (b"date", DateFn::Date),
            (b"time", DateFn::Time),
            (b"datetime", DateFn::DateTime),
            (b"julianday", DateFn::JulianDay),
            (b"unixepoch", DateFn::UnixEpoch),
            (b"strftime", DateFn::Strftime),
            (b"timediff", DateFn::TimeDiff),
            (b"current_time", DateFn::CurrentTime),
            (b"current_date", DateFn::CurrentDate),
            (b"current_timestamp", DateFn::CurrentTimestamp),
        ];
        NAMES.iter().find(|(n, _)| eq_ci(n, name)).map(|(_, f)| *f)
    }

    /// The argument counts the pin registers.
    pub fn arity(self) -> (usize, usize) {
        match self {
            DateFn::TimeDiff => (2, 2),
            DateFn::CurrentTime | DateFn::CurrentDate | DateFn::CurrentTimestamp => (0, 0),
            _ => (0, usize::MAX),
        }
    }

    /// Whether the pin registers the function non-deterministic
    /// (refused in generated columns and index expressions when
    /// the table is created): only the keywords. The others are
    /// "pure" and fail at run time only when they would read the
    /// clock or the zone there.
    pub fn nondeterministic(self) -> bool {
        matches!(
            self,
            DateFn::CurrentTime | DateFn::CurrentDate | DateFn::CurrentTimestamp
        )
    }
}

/// The caller-supplied time-zone hook (v0.16, `datetime`; see
/// `Config::tz_offset`): given an instant in Unix seconds, the
/// local time's offset east of UTC in seconds (`-18000` for New
/// York in winter), or `None` when local time is unavailable
/// (the statement then fails with
/// `SchemaKind::LocalTimeUnavailable`).
pub type TzOffset = fn(i64) -> Option<i64>;

/// Where a call is evaluated (v0.16, `datetime`): the connection
/// state holding the clock, the zone and the statement's instant,
/// and whether the context is one the pin evaluates "pure" — a
/// CHECK constraint, an index expression, a generated column —
/// where reading either is an error.
#[derive(Clone, Copy)]
pub struct TimeCtx<'a> {
    pub env: Option<&'a Env>,
    pub pure: bool,
}

/// Julian day 2440587.5 (1970-01-01 00:00:00 UTC) in milliseconds.
const UNIX_JD: i64 = 210_866_760_000_000;
/// The largest Julian day in milliseconds the pin formats
/// (9999-12-31 23:59:59.999).
const MAX_JD: i64 = 464_269_060_799_999;
const DAY_MS: i64 = 86_400_000;

/// The pin's `DateTime`.
#[derive(Clone, Copy, Default)]
struct Dt {
    ijd: i64,
    y: i64,
    mo: i64,
    d: i64,
    h: i64,
    mi: i64,
    /// Time-zone offset in minutes.
    tz: i64,
    s: f64,
    valid_jd: bool,
    valid_ymd: bool,
    valid_hms: bool,
    /// Days `'floor'` subtracts (a day-of-month overflow).
    n_floor: i64,
    /// `s` holds the raw number the value was given as.
    raw_s: bool,
    is_error: bool,
    use_subsec: bool,
    is_utc: bool,
    is_local: bool,
}

/// x86-64's `(sqlite3_int64)x`: truncation, with the "integer
/// indefinite" for NaN and out-of-range values.
fn c_i64(x: f64) -> i64 {
    if x > -9_223_372_036_854_775_809.0 && x < 9_223_372_036_854_775_808.0 {
        x as i64
    } else {
        i64::MIN
    }
}

/// x86-64's `(int)x`.
fn c_i32(x: f64) -> i64 {
    if x > -2_147_483_649.0 && x < 2_147_483_648.0 {
        x as i64
    } else {
        i64::from(i32::MIN)
    }
}

/// C's truncating `/` (the divisors here are non-zero constants).
fn cdiv(a: i64, b: i64) -> i64 {
    a.checked_div(b).unwrap_or(0)
}

/// C's `%`.
fn crem(a: i64, b: i64) -> i64 {
    a.checked_rem(b).unwrap_or(0)
}

/// Byte `i` of a C string (0 past its end).
fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

fn tail(z: &[u8], i: usize) -> &[u8] {
    z.get(i..).unwrap_or(&[])
}

/// The pin's `sqlite3Isspace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// A text value as C code sees it: up to its first NUL.
fn c_str(t: &[u8]) -> &[u8] {
    match t.iter().position(|b| *b == 0) {
        Some(i) => t.get(..i).unwrap_or(t),
        None => t,
    }
}

/// ASCII case-insensitive prefix test.
fn starts_ci(z: &[u8], p: &[u8]) -> bool {
    z.get(..p.len()).is_some_and(|h| eq_ci(h, p))
}

/// `sqlite3AtoF(z, ...) > 0`: the whole text is one decimal number
/// (optional surrounding spaces, sign, digits with an optional
/// point, optional exponent), converted correctly rounded.
fn atof(z: &[u8]) -> Option<f64> {
    let mut a = 0;
    while is_space(at(z, a)) {
        a += 1;
    }
    let mut b = z.len();
    while b > a && is_space(at(z, b - 1)) {
        b -= 1;
    }
    let t = z.get(a..b)?;
    let mut i = usize::from(matches!(t.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while at(t, i).is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if at(t, i) == b'.' {
        i += 1;
        while at(t, i).is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(at(t, i), b'e' | b'E') {
        i += 1;
        if matches!(at(t, i), b'+' | b'-') {
            i += 1;
        }
        if !at(t, i).is_ascii_digit() {
            return None;
        }
        while at(t, i).is_ascii_digit() {
            i += 1;
        }
    }
    if i != t.len() {
        return None;
    }
    crate::sql::lexer::parse_real_signed(t)
}

/// The pin's `getDigits`: fields of `N` digits, each in
/// `min..=max` and followed by its separator, per 4-byte group of
/// `fmt` (count, minimum, max code `a`..`f`, separator). Returns
/// how many fields parsed.
fn get_digits(z: &[u8], fmt: &[u8], out: &mut [i64]) -> usize {
    const MX: [i64; 6] = [12, 14, 24, 31, 59, 14712];
    let (mut zi, mut fi, mut cnt) = (0usize, 0usize, 0usize);
    loop {
        let n = at(fmt, fi).wrapping_sub(b'0');
        let min = i64::from(at(fmt, fi + 1).wrapping_sub(b'0'));
        let max = MX
            .get(usize::from(at(fmt, fi + 2).wrapping_sub(b'a')))
            .copied()
            .unwrap_or(0);
        let next = at(fmt, fi + 3);
        let mut val = 0i64;
        for _ in 0..n {
            let c = at(z, zi);
            if !c.is_ascii_digit() {
                return cnt;
            }
            val = val * 10 + i64::from(c - b'0');
            zi += 1;
        }
        if val < min || val > max || (next != 0 && next != at(z, zi)) {
            return cnt;
        }
        match out.get_mut(cnt) {
            Some(o) => *o = val,
            None => return cnt,
        }
        zi += 1;
        cnt += 1;
        fi += 4;
        if next == 0 {
            return cnt;
        }
    }
}

/// The pin's `parseTimezone`: true on error.
fn parse_timezone(z: &[u8], p: &mut Dt) -> bool {
    let mut i = 0;
    while is_space(at(z, i)) {
        i += 1;
    }
    p.tz = 0;
    let sgn = match at(z, i) {
        b'-' => -1,
        b'+' => 1,
        b'Z' | b'z' => {
            i += 1;
            p.is_local = false;
            p.is_utc = true;
            0
        }
        c => return c != 0,
    };
    if sgn != 0 {
        i += 1;
        let mut v = [0i64; 2];
        if get_digits(tail(z, i), b"20b:20e", &mut v) != 2 {
            return true;
        }
        i += 5;
        p.tz = sgn * (v[1] + v[0] * 60);
        // An explicit zero offset marks the time UTC, as `Z` does
        // (a non-zero one does when it is applied, `compute_jd`).
        if p.tz == 0 {
            p.is_local = false;
            p.is_utc = true;
        }
    }
    while is_space(at(z, i)) {
        i += 1;
    }
    at(z, i) != 0
}

/// The pin's `parseHhMmSs`: `HH:MM[:SS[.FFF...]]` and a zone.
fn parse_hhmmss(z: &[u8], p: &mut Dt) -> bool {
    let mut hm = [0i64; 2];
    if get_digits(z, b"20c:20e", &mut hm) != 2 {
        return true;
    }
    let mut i = 5;
    let mut s = 0i64;
    let mut ms = 0.0f64;
    if at(z, i) == b':' {
        i += 1;
        let mut sv = [0i64; 1];
        if get_digits(tail(z, i), b"20e", &mut sv) != 1 {
            return true;
        }
        s = sv[0];
        i += 2;
        if at(z, i) == b'.' && at(z, i + 1).is_ascii_digit() {
            let mut scale = 1.0f64;
            i += 1;
            while at(z, i).is_ascii_digit() {
                ms = ms * 10.0 + f64::from(at(z, i) - b'0');
                scale *= 10.0;
                i += 1;
            }
            ms /= scale;
            // The pin truncates rather than round into the next
            // second.
            if ms > 0.999 {
                ms = 0.999;
            }
        }
    }
    // 3.54: hour 24 is only `24:00:00` (the next midnight).
    if hm[0] == 24 && (hm[1] != 0 || s != 0 || ms > 0.0) {
        return true;
    }
    p.valid_jd = false;
    p.raw_s = false;
    p.valid_hms = true;
    p.h = hm[0];
    p.mi = hm[1];
    p.s = s as f64 + ms;
    parse_timezone(tail(z, i), p)
}

fn datetime_error(p: &mut Dt) {
    *p = Dt::default();
    p.is_error = true;
}

/// The pin's `computeJD`.
fn compute_jd(p: &mut Dt) {
    if p.valid_jd {
        return;
    }
    let (mut y, mut m, d) = if p.valid_ymd {
        (p.y, p.mo, p.d)
    } else {
        (2000, 1, 1)
    };
    if !(-4713..=9999).contains(&y) || p.raw_s {
        datetime_error(p);
        return;
    }
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = cdiv(y + 4800, 100);
    let b = 38 - a + cdiv(a, 4);
    let x1 = cdiv(36525 * (y + 4716), 100);
    let x2 = cdiv(306_001 * (m + 1), 10_000);
    p.ijd = c_i64(((x1 + x2 + d + b) as f64 - 1524.5) * 86_400_000.0);
    p.valid_jd = true;
    if p.valid_hms {
        if p.h == 24 {
            // 3.54: a `24:00` value's day-of-month overflow is
            // folded into the next-midnight normalisation, so
            // `'floor'` has nothing left to move back.
            p.n_floor = 0;
        }
        let t = (p.h * 3_600_000 + p.mi * 60_000).wrapping_add(c_i64(p.s * 1000.0 + 0.5));
        p.ijd = p.ijd.wrapping_add(t);
        if p.tz != 0 {
            p.ijd = p.ijd.wrapping_sub(p.tz * 60_000);
            p.valid_ymd = false;
            p.valid_hms = false;
            p.tz = 0;
            p.is_utc = true;
            p.is_local = false;
        } else if p.h == 24 {
            // 3.54: `24:00` is normalised to the next day's
            // midnight (without the zone change's UTC mark).
            p.valid_ymd = false;
            p.valid_hms = false;
        }
    }
}

/// The pin's `computeFloor`: how far `'floor'` moves back a
/// day-of-month overflow.
fn compute_floor(p: &mut Dt) {
    // 0x15aa: the months of 31 days.
    p.n_floor = if p.d <= 28 || (1i64 << p.mo.clamp(0, 31)) & 0x15aa != 0 {
        0
    } else if p.mo != 2 {
        i64::from(p.d == 31)
    } else if crem(p.y, 4) != 0 || (crem(p.y, 100) == 0 && crem(p.y, 400) != 0) {
        p.d - 28
    } else {
        p.d - 29
    };
}

/// The pin's `parseYyyyMmDd`.
fn parse_yyyymmdd(z: &[u8], p: &mut Dt) -> bool {
    let neg = at(z, 0) == b'-';
    let mut i = usize::from(neg);
    let mut v = [0i64; 3];
    if get_digits(tail(z, i), b"40f-21a-21d", &mut v) != 3 {
        return true;
    }
    i += 10;
    while is_space(at(z, i)) || at(z, i) == b'T' {
        i += 1;
    }
    if !parse_hhmmss(tail(z, i), p) {
        // The time too.
    } else if at(z, i) == 0 {
        p.valid_hms = false;
    } else {
        return true;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -v[0] } else { v[0] };
    p.mo = v[1];
    p.d = v[2];
    compute_floor(p);
    if p.tz != 0 || (p.valid_hms && p.h == 24) {
        compute_jd(p);
    }
    false
}

/// The pin's `validJulianDay`.
fn valid_jd(ijd: i64) -> bool {
    (0..=MAX_JD).contains(&ijd)
}

/// The pin's `computeYMD`.
fn compute_ymd(p: &mut Dt) {
    if p.valid_ymd {
        return;
    }
    if !p.valid_jd {
        p.y = 2000;
        p.mo = 1;
        p.d = 1;
    } else if !valid_jd(p.ijd) {
        datetime_error(p);
        return;
    } else {
        let z = cdiv(p.ijd + 43_200_000, DAY_MS);
        let alpha = c_i32((z as f64 + 32044.75) / 36524.25) - 52;
        let a = z + 1 + alpha - cdiv(alpha + 100, 4) + 25;
        let b = a + 1524;
        let c = c_i32((b as f64 - 122.1) / 365.25);
        let d = cdiv(36525 * (c & 32767), 100);
        let e = c_i32((b - d) as f64 / 30.6001);
        let x1 = c_i32(30.6001 * e as f64);
        p.d = b - d - x1;
        p.mo = if e < 14 { e - 1 } else { e - 13 };
        p.y = if p.mo > 2 { c - 4716 } else { c - 4715 };
    }
    p.valid_ymd = true;
}

/// The pin's `computeHMS`.
fn compute_hms(p: &mut Dt) {
    if p.valid_hms {
        return;
    }
    compute_jd(p);
    let day_ms = crem(p.ijd.wrapping_add(43_200_000), DAY_MS);
    p.s = crem(day_ms, 60_000) as f64 / 1000.0;
    let day_min = cdiv(day_ms, 60_000);
    p.mi = crem(day_min, 60);
    p.h = cdiv(day_min, 60);
    p.raw_s = false;
    p.valid_hms = true;
}

fn compute_ymd_hms(p: &mut Dt) {
    compute_ymd(p);
    compute_hms(p);
}

fn clear_ymd_hms_tz(p: &mut Dt) {
    p.valid_ymd = false;
    p.valid_hms = false;
    p.tz = 0;
}

/// The pin's `setRawDateNumber`: a number is a Julian day when it
/// is one (`0 <= r < 5373484.5`); `'unixepoch'` / `'auto'` may
/// reinterpret it.
fn set_raw(p: &mut Dt, r: f64) {
    p.s = r;
    p.raw_s = true;
    if (0.0..5_373_484.5).contains(&r) {
        p.ijd = c_i64(r * 86_400_000.0 + 0.5);
        p.valid_jd = true;
    }
}

/// Why a call fails its statement (mapped to `Error` once, in
/// `eval`, so the engine is not generic over the storage error).
enum Fail {
    Kind(SchemaKind),
    NoClock,
}

type Res<T> = core::result::Result<T, Fail>;

fn nondeterministic() -> Fail {
    Fail::Kind(SchemaKind::NonDeterministic)
}

/// The pin's `sqlite3NotPureFunc`: reading the clock or the zone
/// is an error where the pin evaluates the call "pure" (and, here,
/// where no connection state is at hand — a generated column).
fn env_for<'a>(t: &TimeCtx<'a>) -> Res<&'a Env> {
    match t.env {
        Some(env) if !t.pure => Ok(env),
        _ => Err(nondeterministic()),
    }
}

/// The statement's instant as a Julian day in milliseconds (the
/// pin's `sqlite3StmtCurrentTime`: read once, kept until the
/// statement's next execution).
fn current(t: &TimeCtx<'_>) -> Res<i64> {
    let env = env_for(t)?;
    if let Some(j) = env.now.get() {
        return Ok(j);
    }
    let ms = env.clock.ok_or(Fail::NoClock)?();
    let j = ms.saturating_add(UNIX_JD);
    env.now.set(Some(j));
    Ok(j)
}

/// The pin's `setDateTimeToCurrent`: true when the clock reads
/// before the Julian epoch.
fn set_current(t: &TimeCtx<'_>, p: &mut Dt) -> Res<bool> {
    p.ijd = current(t)?;
    if p.ijd > 0 {
        p.valid_jd = true;
        p.is_utc = true;
        p.is_local = false;
        clear_ymd_hms_tz(p);
        Ok(false)
    } else {
        Ok(true)
    }
}

/// The pin's `parseDateOrTime`: true when the text is no time
/// value.
fn parse_date_or_time(t: &TimeCtx<'_>, z: &[u8], p: &mut Dt) -> Res<bool> {
    if !parse_yyyymmdd(z, p) || !parse_hhmmss(z, p) {
        return Ok(false);
    }
    if eq_ci(z, b"now") {
        return set_current(t, p);
    }
    if let Some(r) = atof(z) {
        set_raw(p, r);
        return Ok(false);
    }
    if eq_ci(z, b"subsec") || eq_ci(z, b"subsecond") {
        p.use_subsec = true;
        return set_current(t, p);
    }
    Ok(true)
}

/// Civil date and time of a count of seconds since 1970-01-01
/// (proleptic Gregorian; H. Hinnant's days-to-civil algorithm).
fn civil(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, sod / 3600, sod / 60 % 60, sod % 60)
}

/// The pin's `toLocaltime`: the zone hook stands in for
/// `localtime_r`, asked about an instant in 1970..2038 (an instant
/// outside is moved to the year with the same leap-year phase in
/// 1997..=2003 and back, as the pin does).
fn to_localtime(p: &mut Dt, env: &Env) -> Res<()> {
    compute_jd(p);
    let (t, year_diff) = if p.ijd < 2_108_667_600 * 100_000 || p.ijd > 2_130_141_456 * 100_000 {
        let mut x = *p;
        compute_ymd_hms(&mut x);
        let diff = (2000 + crem(x.y, 4)) - x.y;
        x.y += diff;
        x.valid_jd = false;
        compute_jd(&mut x);
        (cdiv(x.ijd, 1000) - 210_866_760_000, diff)
    } else {
        (cdiv(p.ijd, 1000) - 210_866_760_000, 0)
    };
    let off = match env.tz {
        None => 0,
        Some(f) => f(t).ok_or(Fail::Kind(SchemaKind::LocalTimeUnavailable))?,
    };
    let (y, mo, d, h, mi, s) = civil(t.saturating_add(off));
    p.y = y - year_diff;
    p.mo = mo;
    p.d = d;
    p.h = h;
    p.mi = mi;
    p.s = s as f64 + crem(p.ijd, 1000) as f64 * 0.001;
    p.valid_ymd = true;
    p.valid_hms = true;
    p.valid_jd = false;
    p.raw_s = false;
    p.tz = 0;
    p.is_error = false;
    Ok(())
}

/// The pin's `aXformType`: unit name, the `float` limit on its
/// count, and its length in seconds.
const XFORM: [(&[u8], f32, f32); 6] = [
    (b"second", 4.6427e14, 1.0),
    (b"minute", 7.7379e12, 60.0),
    (b"hour", 1.2897e11, 3600.0),
    (b"day", 5_373_485.0, 86_400.0),
    (b"month", 176_546.0, 2_592_000.0),
    (b"year", 14_713.0, 31_536_000.0),
];

/// Normalise a month count into 1..=12, carrying into the year
/// (the pin's `x = M>0 ? (M-1)/12 : (M-12)/12`).
fn carry_months(p: &mut Dt) {
    let x = if p.mo > 0 {
        cdiv(p.mo - 1, 12)
    } else {
        cdiv(p.mo - 12, 12)
    };
    p.y += x;
    p.mo -= x * 12;
}

/// 23:59:59.999, the time `'end of day'` and `'end of year'` set.
fn end_of_day(p: &mut Dt) {
    p.h = 23;
    p.mi = 59;
    p.s = 59.999;
}

/// The pin's `parseModifier` (3.54.1, with `'end of'` and the
/// backward `'weekday -N'`): true when the modifier does not apply
/// (the result is NULL).
fn parse_modifier(t: &TimeCtx<'_>, z: &[u8], p: &mut Dt, idx: usize) -> Res<bool> {
    let mut rc = true;
    match at(z, 0).to_ascii_lowercase() {
        b'a' => {
            if eq_ci(z, b"auto") {
                if idx > 1 {
                    return Ok(true);
                }
                if !p.raw_s || p.valid_jd {
                    rc = false;
                    p.raw_s = false;
                } else if p.s >= -210_866_760_000.0 && p.s <= 253_402_300_799.0 {
                    let r = p.s * 1000.0 + 210_866_760_000_000.0;
                    clear_ymd_hms_tz(p);
                    p.ijd = c_i64(r + 0.5);
                    p.valid_jd = true;
                    p.raw_s = false;
                    rc = false;
                }
            }
        }
        b'c' => {
            if eq_ci(z, b"ceiling") {
                compute_jd(p);
                clear_ymd_hms_tz(p);
                rc = false;
                p.n_floor = 0;
            }
        }
        b'e' => {
            if starts_ci(z, b"end of ") && (p.valid_jd || p.valid_ymd || p.valid_hms) {
                let unit = tail(z, 7);
                compute_ymd(p);
                p.valid_hms = true;
                p.raw_s = false;
                p.tz = 0;
                p.valid_jd = false;
                if eq_ci(unit, b"month") {
                    // The pin's 'end of month' is the next month's
                    // first midnight less one millisecond: in
                    // December 9999 that midnight is out of range
                    // (an error), and an invalid value falls back to
                    // 2000-01-01 before the subtraction.
                    p.h = 0;
                    p.mi = 0;
                    p.s = 0.0;
                    p.d = 1;
                    p.mo += 1;
                    carry_months(p);
                    compute_jd(p);
                    p.ijd = p.ijd.wrapping_sub(1);
                    clear_ymd_hms_tz(p);
                    p.n_floor = 0;
                    rc = false;
                } else if eq_ci(unit, b"year") {
                    end_of_day(p);
                    p.mo = 12;
                    p.d = 31;
                    rc = false;
                } else if eq_ci(unit, b"day") {
                    end_of_day(p);
                    rc = false;
                }
            }
        }
        b'f' => {
            if eq_ci(z, b"floor") {
                compute_jd(p);
                p.ijd = p.ijd.wrapping_sub(p.n_floor * DAY_MS);
                clear_ymd_hms_tz(p);
                rc = false;
            }
        }
        b'j' => {
            if eq_ci(z, b"julianday") {
                if idx > 1 {
                    return Ok(true);
                }
                if p.valid_jd && p.raw_s {
                    rc = false;
                    p.raw_s = false;
                }
            }
        }
        b'l' => {
            if eq_ci(z, b"localtime") {
                let env = env_for(t)?;
                if !p.is_local {
                    to_localtime(p, env)?;
                }
                rc = false;
                p.is_utc = false;
                p.is_local = true;
            }
        }
        b'u' => {
            if eq_ci(z, b"unixepoch") && p.raw_s {
                if idx > 1 {
                    return Ok(true);
                }
                let r = p.s * 1000.0 + 210_866_760_000_000.0;
                if (0.0..464_269_060_800_000.0).contains(&r) {
                    clear_ymd_hms_tz(p);
                    p.ijd = c_i64(r + 0.5);
                    p.valid_jd = true;
                    p.raw_s = false;
                    rc = false;
                }
            } else if eq_ci(z, b"utc") {
                let env = env_for(t)?;
                if !p.is_utc {
                    compute_jd(p);
                    let orig = p.ijd;
                    let mut guess = orig;
                    let mut err = 0i64;
                    let mut cnt = 0;
                    loop {
                        let mut new = Dt::default();
                        guess = guess.wrapping_sub(err);
                        new.ijd = guess;
                        new.valid_jd = true;
                        to_localtime(&mut new, env)?;
                        compute_jd(&mut new);
                        err = new.ijd.wrapping_sub(orig);
                        if err == 0 || cnt >= 3 {
                            break;
                        }
                        cnt += 1;
                    }
                    *p = Dt::default();
                    p.ijd = guess;
                    p.valid_jd = true;
                    p.is_utc = true;
                    p.is_local = false;
                }
                rc = false;
            }
        }
        b'w' => {
            if starts_ci(z, b"weekday ")
                && let Some(r) = atof(tail(z, 8))
                && r > -7.0
                && r < 7.0
                && (r as i64) as f64 == r
            {
                let n = (r as i64).abs();
                compute_ymd_hms(p);
                p.tz = 0;
                p.valid_jd = false;
                compute_jd(p);
                let mut w = crem(cdiv(p.ijd.wrapping_add(129_600_000), DAY_MS), 7);
                if r.is_sign_negative() {
                    // 3.54: back to the latest such weekday.
                    if w < n {
                        w += 7;
                    }
                    p.ijd = p.ijd.wrapping_sub((w - n) * DAY_MS);
                } else {
                    if w > n {
                        w -= 7;
                    }
                    p.ijd = p.ijd.wrapping_add((n - w) * DAY_MS);
                }
                clear_ymd_hms_tz(p);
                rc = false;
            }
        }
        b's' => {
            if !starts_ci(z, b"start of ") {
                if eq_ci(z, b"subsec") || eq_ci(z, b"subsecond") {
                    p.use_subsec = true;
                    rc = false;
                }
            } else if p.valid_jd || p.valid_ymd || p.valid_hms {
                let unit = tail(z, 9);
                compute_ymd(p);
                p.valid_hms = true;
                p.h = 0;
                p.mi = 0;
                p.s = 0.0;
                p.raw_s = false;
                p.tz = 0;
                p.valid_jd = false;
                if eq_ci(unit, b"month") {
                    p.d = 1;
                    rc = false;
                } else if eq_ci(unit, b"year") {
                    p.mo = 1;
                    p.d = 1;
                    rc = false;
                } else if eq_ci(unit, b"day") {
                    rc = false;
                }
            }
        }
        b'+' | b'-' | b'0'..=b'9' => rc = numeric_modifier(z, p),
        _ => {}
    }
    Ok(rc)
}

/// The pin's numeric modifiers: `NNN units`, `[+-]HH:MM[:SS[.F]]`,
/// `[+-]YYYY-MM-DD[ HH:MM[:SS[.F]]]`. True when it does not apply.
fn numeric_modifier(z: &[u8], p: &mut Dt) -> bool {
    let z0 = at(z, 0);
    let mut n = 1usize;
    let mut scratch = [0i64; 3];
    while at(z, n) != 0 {
        let c = at(z, n);
        if c == b':' || is_space(c) {
            break;
        }
        if c == b'-'
            && ((n == 5 && get_digits(tail(z, 1), b"40f", &mut scratch) == 1)
                || (n == 6 && get_digits(tail(z, 1), b"50f", &mut scratch) == 1))
        {
            break;
        }
        n += 1;
    }
    let Some(mut r) = atof(z.get(..n).unwrap_or(z)) else {
        return true;
    };
    // Where an `HH:MM` part starts (`z2` in the pin).
    let mut z2 = 0usize;
    if at(z, n) == b'-' {
        if z0 != b'+' && z0 != b'-' {
            return true;
        }
        let mut v = [0i64; 3];
        let mut zo = 0usize;
        if n == 5 {
            if get_digits(tail(z, 1), b"40f-20a-20d", &mut v) != 3 {
                return true;
            }
        } else {
            if get_digits(tail(z, 1), b"50f-20a-20d", &mut v) != 3 {
                return true;
            }
            zo = 1;
        }
        let [yy, mm, mut dd] = v;
        if mm >= 12 || dd >= 31 {
            return true;
        }
        compute_ymd_hms(p);
        p.valid_jd = false;
        if z0 == b'-' {
            p.y -= yy;
            p.mo -= mm;
            dd = -dd;
        } else {
            p.y += yy;
            p.mo += mm;
        }
        carry_months(p);
        compute_floor(p);
        compute_jd(p);
        p.valid_hms = false;
        p.valid_ymd = false;
        p.ijd = p.ijd.wrapping_add(dd * DAY_MS);
        if at(z, zo + 11) == 0 {
            return false;
        }
        let mut hm = [0i64; 2];
        if is_space(at(z, zo + 11)) && get_digits(tail(z, zo + 12), b"20c:20e", &mut hm) == 2 {
            z2 = zo + 12;
            n = 2;
        } else {
            return true;
        }
    }
    if at(z, z2 + n) == b':' {
        let start = if at(z, z2).is_ascii_digit() {
            z2
        } else {
            z2 + 1
        };
        let mut tx = Dt::default();
        if parse_hhmmss(tail(z, start), &mut tx) {
            return true;
        }
        compute_jd(&mut tx);
        tx.ijd = tx.ijd.wrapping_sub(43_200_000);
        let day = cdiv(tx.ijd, DAY_MS);
        tx.ijd = tx.ijd.wrapping_sub(day.wrapping_mul(DAY_MS));
        if z0 == b'-' {
            tx.ijd = tx.ijd.wrapping_neg();
        }
        compute_jd(p);
        clear_ymd_hms_tz(p);
        p.ijd = p.ijd.wrapping_add(tx.ijd);
        return false;
    }
    // `NNN units`.
    let mut u = n;
    while is_space(at(z, u)) {
        u += 1;
    }
    let unit = tail(z, u);
    let mut len = unit.len();
    if !(3..=10).contains(&len) {
        return true;
    }
    if at(unit, len - 1).eq_ignore_ascii_case(&b's') {
        len -= 1;
    }
    let unit = unit.get(..len).unwrap_or(unit);
    compute_jd(p);
    let rounder = if r < 0.0 { -0.5 } else { 0.5 };
    p.n_floor = 0;
    let mut rc = true;
    for (i, (name, limit, xform)) in XFORM.iter().enumerate() {
        let limit = f64::from(*limit);
        if name.len() == len && eq_ci(name, unit) && r > -limit && r < limit {
            if i == 4 {
                compute_ymd_hms(p);
                p.mo += c_i32(r);
                carry_months(p);
                compute_floor(p);
                p.valid_jd = false;
                r -= c_i32(r) as f64;
            } else if i == 5 {
                let y = c_i32(r);
                compute_ymd_hms(p);
                p.y += y;
                compute_floor(p);
                p.valid_jd = false;
                r -= c_i32(r) as f64;
            }
            compute_jd(p);
            p.ijd = p
                .ijd
                .wrapping_add(c_i64(r * 1000.0 * f64::from(*xform) + rounder));
            rc = false;
            break;
        }
    }
    clear_ymd_hms_tz(p);
    rc
}

/// The text a modifier or format argument reads
/// (`sqlite3_value_text`): None for NULL.
fn value_text(v: &OwnedValue) -> Option<AVec<u8>> {
    to_text(v)
}

/// The pin's `isDate`: the time value and its modifiers. `Ok(true)`
/// is a NULL result.
fn is_date(t: &TimeCtx<'_>, args: &[OwnedValue], p: &mut Dt) -> Res<bool> {
    *p = Dt::default();
    let Some(first) = args.first() else {
        return set_current(t, p);
    };
    match first {
        OwnedValue::Null => return Ok(true),
        OwnedValue::Integer(i) => set_raw(p, *i as f64),
        OwnedValue::Real(r) => set_raw(p, *r),
        OwnedValue::Text(b) | OwnedValue::Blob(b) => {
            if parse_date_or_time(t, c_str(b.as_slice()), p)? {
                return Ok(true);
            }
        }
    }
    for (i, a) in args.iter().enumerate().skip(1) {
        let Some(z) = value_text(a) else {
            return Ok(true);
        };
        if parse_modifier(t, c_str(z.as_slice()), p, i)? {
            return Ok(true);
        }
    }
    compute_jd(p);
    if p.is_error || !valid_jd(p.ijd) {
        return Ok(true);
    }
    if args.len() == 1 && p.valid_ymd && p.d > 28 {
        // A lone YYYY-MM-DD is normalised (2023-02-31 is March 3).
        p.valid_ymd = false;
    }
    Ok(false)
}

/// `'0' + v % 10` as the pin's `char` arithmetic computes it (a
/// negative digit is punctuation).
fn digit(v: i64) -> u8 {
    (48 + crem(v, 10)) as u8
}

fn push_date(out: &mut AVec<u8>, x: &Dt) {
    if x.y < 0 {
        out.push(b'-');
    }
    let y = x.y.wrapping_abs();
    for v in [y / 1000, y / 100, y / 10, y] {
        out.push(digit(v));
    }
    out.push(b'-');
    out.push(digit(cdiv(x.mo, 10)));
    out.push(digit(x.mo));
    out.push(b'-');
    out.push(digit(cdiv(x.d, 10)));
    out.push(digit(x.d));
}

fn push_time(out: &mut AVec<u8>, x: &Dt) {
    out.push(digit(cdiv(x.h, 10)));
    out.push(digit(x.h));
    out.push(b':');
    out.push(digit(cdiv(x.mi, 10)));
    out.push(digit(x.mi));
    out.push(b':');
    if x.use_subsec {
        let s = c_i32(1000.0 * x.s + 0.5);
        out.push(digit(cdiv(s, 10_000)));
        out.push(digit(cdiv(s, 1000)));
        out.push(b'.');
        out.push(digit(cdiv(s, 100)));
        out.push(digit(cdiv(s, 10)));
        out.push(digit(s));
    } else {
        let s = c_i32(x.s);
        out.push(digit(cdiv(s, 10)));
        out.push(digit(s));
    }
}

/// `v` as C's `%0Nd` (`zero`) or `%Nd`: at least `width`
/// characters, the sign counted.
fn push_int(out: &mut AVec<u8>, v: i64, width: usize, zero: bool) {
    let mut digits = [0u8; 20];
    let mut k = digits.len();
    let mut u = v.unsigned_abs();
    loop {
        k -= 1;
        if let Some(d) = digits.get_mut(k) {
            *d = b'0' + (u % 10) as u8;
        }
        u /= 10;
        if u == 0 || k == 0 {
            break;
        }
    }
    let digits = digits.get(k..).unwrap_or(&[]);
    let neg = v < 0;
    let len = digits.len() + usize::from(neg);
    let pad = width.saturating_sub(len);
    if !zero {
        for _ in 0..pad {
            out.push(b' ');
        }
    }
    if neg {
        out.push(b'-');
    }
    if zero {
        for _ in 0..pad {
            out.push(b'0');
        }
    }
    out.extend_from_slice(digits);
}

/// `r` through the pin's printf with a one-conversion format
/// (`%06.3f`, `%.16g`, `%.3f`).
fn push_printf(out: &mut AVec<u8>, fmt: &[u8], r: f64) -> Res<()> {
    let mut f = AVec::new();
    f.extend_from_slice(fmt);
    let args = [OwnedValue::Text(f), OwnedValue::Real(r)];
    let v = crate::func::printf::<()>(&args).map_err(|_| Fail::Kind(SchemaKind::TooBig))?;
    if let OwnedValue::Text(t) = v {
        out.extend_from_slice(t.as_slice());
    }
    Ok(())
}

/// The pin's `daysAfterJan01`.
fn days_after_jan01(x: &Dt) -> i64 {
    let mut j = *x;
    j.valid_jd = false;
    j.mo = 1;
    j.d = 1;
    compute_jd(&mut j);
    cdiv(x.ijd.wrapping_sub(j.ijd).wrapping_add(43_200_000), DAY_MS)
}

/// The pin's `daysAfterMonday` (0 = Monday).
fn days_after_monday(x: &Dt) -> i64 {
    crem(cdiv(x.ijd.wrapping_add(43_200_000), DAY_MS), 7)
}

/// The pin's `daysAfterSunday` (0 = Sunday).
fn days_after_sunday(x: &Dt) -> i64 {
    crem(cdiv(x.ijd.wrapping_add(129_600_000), DAY_MS), 7)
}

/// The Thursday of `x`'s ISO week (for `%G`, `%g`, `%V`).
fn iso_thursday(x: &Dt) -> Dt {
    let mut y = *x;
    y.ijd = y
        .ijd
        .wrapping_add((3 - days_after_monday(x)).wrapping_mul(DAY_MS));
    y.valid_ymd = false;
    compute_ymd(&mut y);
    y
}

/// The pin's `strftimeFunc` body: None for an unknown conversion
/// (the result is NULL — 3.54.1 knows no `%y`).
fn strftime(fmt: &[u8], x: &mut Dt) -> Res<Option<AVec<u8>>> {
    compute_jd(x);
    compute_ymd_hms(x);
    let mut out = AVec::new();
    let mut i = 0usize;
    while i < fmt.len() {
        let c = at(fmt, i);
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        let cf = at(fmt, i + 1);
        i += 2;
        match cf {
            b'd' => push_int(&mut out, x.d, 2, true),
            b'e' => push_int(&mut out, x.d, 2, false),
            b'f' => {
                let s = if x.s > 59.999 { 59.999 } else { x.s };
                push_printf(&mut out, b"%06.3f", s)?;
            }
            b'F' => {
                push_int(&mut out, x.y, 4, true);
                out.push(b'-');
                push_int(&mut out, x.mo, 2, true);
                out.push(b'-');
                push_int(&mut out, x.d, 2, true);
            }
            b'G' | b'g' => {
                let y = iso_thursday(x);
                if cf == b'g' {
                    push_int(&mut out, crem(y.y, 100), 2, true);
                } else {
                    push_int(&mut out, y.y, 4, true);
                }
            }
            b'H' => push_int(&mut out, x.h, 2, true),
            b'k' => push_int(&mut out, x.h, 2, false),
            b'I' | b'l' => {
                let mut h = x.h;
                if h > 12 {
                    h -= 12;
                }
                if h == 0 {
                    h = 12;
                }
                push_int(&mut out, h, 2, cf == b'I');
            }
            b'j' => push_int(&mut out, days_after_jan01(x) + 1, 3, true),
            b'J' => push_printf(&mut out, b"%.16g", x.ijd as f64 / 86_400_000.0)?,
            b'm' => push_int(&mut out, x.mo, 2, true),
            b'M' => push_int(&mut out, x.mi, 2, true),
            b'p' | b'P' => {
                let pm = x.h >= 12;
                out.extend_from_slice(match (pm, cf == b'p') {
                    (true, true) => b"PM",
                    (true, false) => b"pm",
                    (false, true) => b"AM",
                    (false, false) => b"am",
                });
            }
            b'R' => {
                push_int(&mut out, x.h, 2, true);
                out.push(b':');
                push_int(&mut out, x.mi, 2, true);
            }
            b's' => {
                if x.use_subsec {
                    push_printf(
                        &mut out,
                        b"%.3f",
                        x.ijd.wrapping_sub(UNIX_JD) as f64 / 1000.0,
                    )?;
                } else {
                    push_int(&mut out, cdiv(x.ijd, 1000) - 210_866_760_000, 0, false);
                }
            }
            b'S' => push_int(&mut out, c_i32(x.s), 2, true),
            b'T' => {
                push_int(&mut out, x.h, 2, true);
                out.push(b':');
                push_int(&mut out, x.mi, 2, true);
                out.push(b':');
                push_int(&mut out, c_i32(x.s), 2, true);
            }
            b'u' | b'w' => {
                let mut d = days_after_sunday(x);
                if d == 0 && cf == b'u' {
                    d = 7;
                }
                out.push(digit(d));
            }
            b'U' => push_int(
                &mut out,
                cdiv(days_after_jan01(x) - days_after_sunday(x) + 7, 7),
                2,
                true,
            ),
            b'V' => {
                let y = iso_thursday(x);
                push_int(&mut out, cdiv(days_after_jan01(&y), 7) + 1, 2, true);
            }
            b'W' => push_int(
                &mut out,
                cdiv(days_after_jan01(x) - days_after_monday(x) + 7, 7),
                2,
                true,
            ),
            b'Y' => push_int(&mut out, x.y, 4, true),
            b'%' => out.push(b'%'),
            _ => return Ok(None),
        }
        if out.len() >= crate::func::MAX_LENGTH {
            return Err(Fail::Kind(SchemaKind::TooBig));
        }
    }
    Ok(Some(out))
}

/// The pin's `timediffFunc` once both values are valid.
fn timediff(d1: &mut Dt, d2: &mut Dt) -> Res<AVec<u8>> {
    compute_ymd_hms(d1);
    compute_ymd_hms(d2);
    let sign;
    let (mut y, mut m);
    if d1.ijd >= d2.ijd {
        sign = b'+';
        y = d1.y - d2.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(d2);
        }
        m = d1.mo - d2.mo;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.mo = d1.mo;
            d2.valid_jd = false;
            compute_jd(d2);
        }
        while d1.ijd < d2.ijd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.mo -= 1;
            if d2.mo < 1 {
                d2.mo = 12;
                d2.y -= 1;
            }
            d2.valid_jd = false;
            compute_jd(d2);
        }
        d1.ijd = d1.ijd.wrapping_sub(d2.ijd);
    } else {
        sign = b'-';
        y = d2.y - d1.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(d2);
        }
        m = d2.mo - d1.mo;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.mo = d1.mo;
            d2.valid_jd = false;
            compute_jd(d2);
        }
        while d1.ijd > d2.ijd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.mo += 1;
            if d2.mo > 12 {
                d2.mo = 1;
                d2.y += 1;
            }
            d2.valid_jd = false;
            compute_jd(d2);
        }
        d1.ijd = d2.ijd.wrapping_sub(d1.ijd);
    }
    d1.ijd = d1.ijd.wrapping_add(148_699_540_800_000);
    clear_ymd_hms_tz(d1);
    compute_ymd_hms(d1);
    let mut out = AVec::new();
    out.push(sign);
    push_int(&mut out, y, 4, true);
    out.push(b'-');
    push_int(&mut out, m, 2, true);
    out.push(b'-');
    push_int(&mut out, d1.d - 1, 2, true);
    out.push(b' ');
    push_int(&mut out, d1.h, 2, true);
    out.push(b':');
    push_int(&mut out, d1.mi, 2, true);
    out.push(b':');
    push_printf(&mut out, b"%06.3f", d1.s)?;
    Ok(out)
}

/// Evaluate a date and time function over its argument values.
pub fn eval<E>(f: DateFn, v: &[OwnedValue], t: TimeCtx<'_>) -> crate::error::Result<OwnedValue, E> {
    eval_inner(f, v, t).map_err(|e| match e {
        Fail::Kind(kind) => Error::Schema { kind },
        Fail::NoClock => Error::NoClock,
    })
}

#[inline(never)]
fn eval_inner(f: DateFn, v: &[OwnedValue], t: TimeCtx<'_>) -> Res<OwnedValue> {
    let mut x = Dt::default();
    let text = |b: AVec<u8>| Ok(OwnedValue::Text(b));
    let mut t = t;
    let args = match f {
        // The keywords are ordinary (not "pure") calls on the pin:
        // a CHECK constraint may read the clock through them.
        DateFn::CurrentTime | DateFn::CurrentDate | DateFn::CurrentTimestamp => {
            t.pure = false;
            &[]
        }
        DateFn::Strftime => v.get(1..).unwrap_or(&[]),
        _ => v,
    };
    match f {
        DateFn::Strftime => {
            let Some(fmt) = v.first().and_then(value_text) else {
                return Ok(OwnedValue::Null);
            };
            if is_date(&t, args, &mut x)? {
                return Ok(OwnedValue::Null);
            }
            match strftime(c_str(fmt.as_slice()), &mut x)? {
                Some(out) => text(out),
                None => Ok(OwnedValue::Null),
            }
        }
        DateFn::TimeDiff => {
            let mut d2 = Dt::default();
            if is_date(&t, v.get(..1).unwrap_or(&[]), &mut x)?
                || is_date(&t, v.get(1..2).unwrap_or(&[]), &mut d2)?
            {
                return Ok(OwnedValue::Null);
            }
            text(timediff(&mut x, &mut d2)?)
        }
        _ => {
            if is_date(&t, args, &mut x)? {
                return Ok(OwnedValue::Null);
            }
            let mut out = AVec::new();
            match f {
                DateFn::Date | DateFn::CurrentDate => {
                    compute_ymd(&mut x);
                    push_date(&mut out, &x);
                }
                DateFn::Time | DateFn::CurrentTime => {
                    compute_hms(&mut x);
                    push_time(&mut out, &x);
                }
                DateFn::JulianDay => {
                    compute_jd(&mut x);
                    return Ok(OwnedValue::Real(x.ijd as f64 / 86_400_000.0));
                }
                DateFn::UnixEpoch => {
                    compute_jd(&mut x);
                    return Ok(if x.use_subsec {
                        OwnedValue::Real(x.ijd.wrapping_sub(UNIX_JD) as f64 / 1000.0)
                    } else {
                        OwnedValue::Integer(cdiv(x.ijd, 1000) - 210_866_760_000)
                    });
                }
                _ => {
                    compute_ymd_hms(&mut x);
                    push_date(&mut out, &x);
                    out.push(b' ');
                    push_time(&mut out, &x);
                }
            }
            text(out)
        }
    }
}
