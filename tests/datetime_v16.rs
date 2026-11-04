//! v0.16 date and time functions (leaf feature `datetime`),
//! differentially vs the pinned oracle (SQLite 3.54.1). The
//! semantics were probed on the pin first (STATUS.md, "Date and
//! time functions"); every result is compared byte for byte as
//! `quote(...)` text, with the time zone given to both sides — the
//! oracle through `TZ`, hipp through `Config::tz_offset` over
//! `systime::zone_offset` for the same zone — in UTC,
//! America/New_York, Asia/Kolkata and Australia/Lord_Howe (a
//! 30-minute daylight shift):
//!
//! - a fixed corpus of every time-value form, modifier and
//!   `strftime` conversion, the pin's boundaries (years -4713 /
//!   9999, Julian days 0 / 5373484.5, the unit limits, `24:00`),
//!   its quirks (a date made invalid before `'localtime'`, the
//!   punctuation of a negative millisecond, day-of-month overflow
//!   kept by `'start of day'`), and a regression case for each bug
//!   found while fitting (named in the corpus);
//! - generated corpora (a deterministic PRNG) of random time values
//!   in all input forms, random modifier chains and random
//!   `strftime` formats, and of instants around daylight-saving
//!   transitions through `'localtime'` / `'utc'`;
//! - `'now'` (`Config::clock`, read once per statement: a ticking
//!   clock shows one instant per statement, DEFAULTs and triggers
//!   included), `Error::NoClock` without a clock, the "pure"
//!   contexts (CHECK, index expressions, generated columns) where
//!   reading the clock or the zone fails on both engines, the
//!   CURRENT_* keywords against a table's own "current_date"
//!   column, and `SchemaKind::LocalTimeUnavailable`.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, MemStorage, SchemaKind, Step, Value};

type MErr = Error<<MemStorage as hipp::Storage>::Error>;

fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// Step a statement to completion, collecting its rows in the
/// CLI's list rendering, or the error.
async fn try_collect<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await?;
    let mut rows = Vec::new();
    while q.step().await? == Step::Row {
        let mut line = String::new();
        for i in 0..q.column_count() {
            if i > 0 {
                line.push('|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => line.push_str(&v.to_string()),
                Some(Value::Real(f)) => line.push_str(&fmt_real(f)),
                Some(Value::Text(b)) | Some(Value::Blob(b)) => {
                    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
                    line.push_str(&String::from_utf8_lossy(&b[..end]));
                }
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

/// The oracle over stdin with `TZ` set; stdout lines (statements
/// that fail print nothing there).
fn oracle_tz(tz: &str, db: &str, sql: &str) -> Vec<String> {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(db)
        .env("TZ", tz)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle spawns");
    // Write from another thread: a long script's output would fill
    // the pipe while this one is still writing.
    let mut stdin = child.stdin.take().expect("stdin");
    let input = sql.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().expect("oracle finishes");
    writer.join().expect("writer").expect("write stdin");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// A fixed instant for 'now': 2023-11-14 22:13:20.123 UTC.
fn fixed_clock() -> i64 {
    1_700_000_000_123
}

fn zone_utc(t: i64) -> Option<i64> {
    Some(hipp::systime::zone_offset(Some("UTC"), t))
}
fn zone_ny(t: i64) -> Option<i64> {
    Some(hipp::systime::zone_offset(Some("America/New_York"), t))
}
fn zone_kolkata(t: i64) -> Option<i64> {
    Some(hipp::systime::zone_offset(Some("Asia/Kolkata"), t))
}
fn zone_lord_howe(t: i64) -> Option<i64> {
    Some(hipp::systime::zone_offset(Some("Australia/Lord_Howe"), t))
}

/// A zone hook (`Config::tz_offset`).
type Zone = fn(i64) -> Option<i64>;

/// The zones every corpus runs in: the `TZ` value and the hook
/// that names the same zone.
const ZONES: [(&str, Zone); 4] = [
    ("UTC", zone_utc),
    ("America/New_York", zone_ny),
    ("Asia/Kolkata", zone_kolkata),
    ("Australia/Lord_Howe", zone_lord_howe),
];

async fn mem_db(zone: Zone) -> Connection<MemStorage> {
    let cfg = Config::default().clock(fixed_clock).tz_offset(Some(zone));
    Connection::open(MemStorage::new(), cfg)
        .await
        .expect("open")
}

/// One mismatch: the expression, the pin's and hipp's `quote()`
/// (`None` an error).
type Mismatch = (String, Option<String>, Option<String>);

/// Evaluate each expression as `quote(expr)` on both engines in
/// `tz`; the mismatches.
fn differ(tz: &str, zone: Zone, exprs: &[String]) -> Vec<Mismatch> {
    let mut script = String::new();
    for (i, e) in exprs.iter().enumerate() {
        script.push_str(&format!("SELECT {i}, quote({e});\n"));
    }
    let mut pin: Vec<Option<String>> = vec![None; exprs.len()];
    for line in oracle_tz(tz, ":memory:", &script) {
        if let Some((i, v)) = line.split_once('|')
            && let Ok(i) = i.parse::<usize>()
            && i < exprs.len()
        {
            pin[i] = Some(v.to_string());
        }
    }
    let mut bad = Vec::new();
    block_on(async {
        let mut db = mem_db(zone).await;
        for (i, e) in exprs.iter().enumerate() {
            let got = try_collect(&mut db, &format!("SELECT quote({e})"))
                .await
                .ok()
                .and_then(|r| r.into_iter().next());
            if got != pin[i] {
                bad.push((e.clone(), pin[i].clone(), got));
            }
        }
    });
    bad
}

fn assert_same(tz: &str, zone: Zone, exprs: &[String]) {
    let bad = differ(tz, zone, exprs);
    assert!(
        bad.is_empty(),
        "{} of {} differ in {tz}, first: {:?}",
        bad.len(),
        exprs.len(),
        &bad[..bad.len().min(8)]
    );
}

/// The fixed corpus (no 'now': those are compared separately).
const FIXED: &[&str] = &[
    // Time-value forms.
    "date('2000-01-01')",
    "datetime('2000-01-01 12:34')",
    "datetime('2000-01-01 12:34:56')",
    "datetime('2000-01-01 12:34:56.789', 'subsec')",
    "datetime('2000-01-01T12:00')",
    "datetime('2000-01-01TT12:00')",
    "datetime('2000-01-01   12:00')",
    "datetime('2000-1-01')",
    "time('12:00')",
    "time('1:00')",
    "time('23:60')",
    "time(' 12:00')",
    "time('12:00 ')",
    "date(' 2000-01-01')",
    "date('2000-01-01 ')",
    "date('2000-01-01 12:00 ')",
    "date('2000-01-01'||char(10))",
    "date('2000-01-01'||char(0)||'x')",
    "date('-0001-01-01')",
    "date('0000-01-01')",
    "date('9999-12-31 23:59:59.999')",
    "date('10000-01-01')",
    "date('-4713-11-24 11:59')",
    "date('-4713-11-24 12:00')",
    "date('2000-02-31')",
    "date('2000-02-31', '+0 days')",
    "datetime('2001-02-31 10:00', 'start of day')",
    "datetime('2001-02-31 10:00', 'end of day')",
    "datetime('2001-02-31 10:00', '+0 hours')",
    // Zones in the value.
    "datetime('2000-01-01 12:00+05:30')",
    "datetime('2000-01-01 12:00 -05:30 ')",
    "datetime('2000-01-01 12:00Z')",
    "datetime('12:00z')",
    "time('12:00 +24:00')",
    "time('12:00+14:59')",
    "time('12:00+15:00')",
    "datetime('2000-01-01 12:00+05:00 x')",
    // Fractional seconds: truncated at .999, rounded otherwise.
    "time('12:00:00.9999')",
    "strftime('%f', '12:00:59.9999')",
    "julianday('2000-01-01 00:00:00.9996')",
    "time('00:00:01.2345', 'subsecond')",
    "datetime('2000-01-01 00:00:01.9999', 'subsec')",
    // Regression: a fraction of 400 digits is NaN on the pin (an
    // invalid date), never second 0.
    "datetime('2000-01-01 12:00:00.' || printf('%.*c', 400, '1'))",
    // 3.54: hour 24 is exactly 24:00:00, the next midnight.
    "time('24:00')",
    "time('24:00:00.000')",
    "time('24:00:01')",
    "time('24:01')",
    "time('24:00:00.0001')",
    "time('24:00:00.5', 'subsec')",
    "julianday('24:00')",
    "datetime('2000-01-01 24:00')",
    "datetime('2000-01-31 24:00')",
    "datetime('2000-02-30 24:00', 'subsec')",
    "datetime('9999-12-31 24:00')",
    "datetime('-4713-11-24 24:00')",
    "datetime('24:00', 'start of day')",
    "datetime('24:00', 'end of day')",
    "datetime('24:00', '+1 month')",
    "datetime('24:00', '+0000-01-00')",
    "datetime('24:00', 'weekday 0')",
    "datetime('2000-01-31 24:00', '+1 month')",
    "datetime('2000-01-31 24:00', 'start of month')",
    "datetime('2000-01-31 24:00', 'end of month')",
    "datetime('2000-01-31 24:00', 'weekday 1')",
    "datetime('2000-01-31 24:00', 'julianday')",
    "datetime('2000-01-31 24:00', 'utc')",
    "datetime('2000-02-29 24:00', '+1 year', 'floor')",
    "strftime('%j %d %H', '2000-12-31 24:00', 'subsec')",
    "strftime('%d %H', '24:00')",
    "timediff('24:00', '2000-01-01')",
    "datetime('2000-01-01', '+24:00')",
    "datetime('2000-01-01', '+24:01')",
    "datetime('2000-01-01', '+0000-00-00 24:00')",
    // Regression: an explicit zero offset marks the value UTC.
    "datetime('12:00-00:00', 'utc')",
    "datetime('12:00+00:00', 'utc')",
    "datetime('12:00+01:00', 'utc')",
    "datetime('2000-01-01 12:00+00:00', 'utc')",
    "datetime('12:00+00:00', '+1 hour', 'utc')",
    // Numbers.
    "date(2451545)",
    "date(2451545.0)",
    "date(2451545.5)",
    "datetime(2451545.25)",
    "datetime('2451545.25')",
    "date(' 2451545 ')",
    "date('2451545.')",
    "date('.5e7')",
    "date('2_451_545')",
    "date('0x10')",
    "date('2451545e0')",
    "julianday('1e400')",
    "julianday('-0')",
    "julianday('+5')",
    "julianday(' +5 ')",
    "julianday('5e')",
    "julianday('5.e3')",
    "julianday('inf')",
    "julianday('.')",
    "julianday('-.5')",
    "date(5373484.4)",
    "date(5373484.5)",
    "datetime(0)",
    "datetime(-1)",
    "julianday(-1)",
    "datetime(0.1234, 'subsec')",
    "datetime(5373484.4999999, 'subsec')",
    "date(x'323030302d30312d3031')",
    "date(x'32303030')",
    "date(NULL)",
    "date('2000-01-01', NULL)",
    "date('')",
    "strftime('', '')",
    // unixepoch / julianday / auto.
    "datetime(1700000000.5, 'unixepoch')",
    "datetime('1700000000', 'unixepoch')",
    "datetime(1700000000, 'unixepoch', 'unixepoch')",
    "datetime('2000-01-01', 'unixepoch')",
    "datetime(-210866760000, 'unixepoch')",
    "datetime(-210866760001, 'unixepoch')",
    "datetime(253402300799.999, 'unixepoch', 'subsec')",
    "datetime(253402300800, 'unixepoch')",
    "datetime(1e10, 'unixepoch')",
    "datetime(1700000000123, 'unixepoch')",
    "datetime(1700000000.1234, 'unixepoch', 'subsec')",
    "datetime(2451545, 'julianday')",
    "datetime('2451545', 'julianday')",
    "datetime('2000-01-01', 'julianday')",
    "datetime(2451545, '+1 day', 'julianday')",
    "datetime(-1, 'julianday')",
    "datetime(6000000, 'julianday')",
    "datetime(0, 'auto')",
    "datetime(5373484.49, 'auto')",
    "datetime(5373484.5, 'auto')",
    "datetime(-210866760000, 'auto')",
    "datetime(253402300799, 'auto')",
    "datetime(253402300800, 'auto')",
    "datetime(1700000000, 'unixepoch', 'auto')",
    "datetime('2000-01-31 24:00', 'auto')",
    "unixepoch('1970-01-01')",
    "unixepoch('1969-12-31 23:59:59.5')",
    "unixepoch('1969-12-31 23:59:59.5', 'subsec')",
    "unixepoch(-1.5, 'unixepoch')",
    "unixepoch(-1.5, 'unixepoch', 'subsec')",
    "unixepoch('9999-12-31 23:59:59.999', 'subsec')",
    "unixepoch('2000-01-01 00:00:01.234', 'subsec')",
    "unixepoch('2000-01-01 00:00:01.234')",
    "typeof(unixepoch('2000-01-01', 'subsec'))",
    "julianday('2000-01-01')",
    "typeof(julianday('2000-01-01'))",
    "julianday('2000-01-01 00:00:00.001')",
    "julianday('-4713-11-24 12:00')",
    // NNN units.
    "datetime('2000-01-01', '+1.5 seconds', 'subsec')",
    "datetime('2000-01-01', '+90 minutes')",
    "datetime('2000-01-01', '-1.25 hours')",
    "datetime('2000-01-01', '+1.5 years')",
    "datetime('2000-01-01', '+1 ye')",
    "datetime('2000-01-01', '+1 secondss')",
    "datetime('2000-01-01', '+1  days')",
    "datetime('2000-01-01', '+1days')",
    "datetime('2000-01-01', '+1 DAYS')",
    "datetime('2000-01-01', '+1 Day')",
    "datetime('2000-01-01', '1e1 days')",
    "datetime('2000-01-01', '.5 days')",
    "datetime('2000-01-01', '5. days')",
    "datetime('2000-01-01', '+1_0 days')",
    "datetime('2000-01-01', '+10\tdays')",
    "datetime('2000-01-01', '-0.5 seconds', 'subsec')",
    "datetime('2000-01-01', '-0.0004 seconds', 'subsec')",
    "datetime('2000-01-01', '+0.0005 minutes', 'subsec')",
    "datetime('2000-01-01', '+5')",
    "datetime('2000-01-01', '5 days ')",
    "datetime('2000-01-01', ' 5 days')",
    "datetime('2000-01-01', '+5373485 days')",
    "datetime('2000-01-01', '+5373484 days')",
    "datetime('2000-01-01', '+176546 months')",
    "datetime('2000-01-01', '+14713 years')",
    "datetime('2000-01-01', '-14712 years')",
    "datetime('2000-01-01', '+464269060799999 seconds')",
    "datetime('2000-01-01', '+464270000000000 seconds')",
    "datetime('2000-01-01', '+1e400 days')",
    "datetime('2000-01-01', '+20000 years')",
    "date('2000-01-01', 5)",
    "date('2000-01-01', 1.5)",
    "date('2000-01-01', x'2b31206461792b')",
    "date('2000-01-01', '+1 day'||char(0)||'zz')",
    // Months and years, ceiling / floor.
    "date('2001-01-31', '+1 month')",
    "date('2001-01-31', '+1 month', 'floor')",
    "date('2001-01-31', 'floor', '+1 month')",
    "date('2000-02-29', '+1 year')",
    "date('2000-02-29', '+1 year', 'floor')",
    "date('2000-01-31', '+1.5 month')",
    "datetime('2001-01-31', '+1 month', 'end of month', 'floor')",
    "datetime('2001-01-31', '+1 month', 'start of month', 'floor')",
    "datetime('2001-01-31', '+1 month', 'floor', 'floor')",
    "datetime('2001-01-31', '+1 month', '+0 days', 'floor')",
    "datetime('2001-01-31', '+1 month', 'ceiling', 'floor')",
    "datetime('2001-03-31', 'floor')",
    "datetime('2000-02-30', 'floor')",
    "datetime('2000-02-30', 'ceiling')",
    "datetime('2001-01-31', '+1 month', 'weekday 1', 'floor')",
    "datetime('2001-01-31', '+1 month', '12:00', 'floor')",
    "datetime('2001-01-31', '+1 month', 'subsec', 'floor')",
    "datetime('2001-01-31', '+1 month', '-0000-00-00', 'floor')",
    "datetime('2001-01-31', '+0000-01-00', 'floor')",
    "datetime('2001-01-31', '+1 year', 'floor')",
    "datetime('2000-02-29', '+1 year', '+1 day', 'floor')",
    // (+|-)YYYY-MM-DD [HH:MM[:SS]] and HH:MM modifiers.
    "datetime('2000-01-01 00:00', '+0001-02-03 04:05')",
    "datetime('2000-01-01', '-0001-00-00 01:00')",
    "datetime('2000-03-31', '+0000-01-00')",
    "datetime('2000-01-01', '+0000-00-00 01:00:30.5')",
    "datetime('2000-01-01', '+10000-00-00')",
    "datetime('2000-01-01', '+14712-00-00')",
    "datetime('2000-01-01', '-01000-00-00')",
    "datetime('2000-01-01', '+1-00-00')",
    "datetime('2000-01-01', '+12345-01-01 12:00')",
    "datetime('2000-01-01', '+0000-12-00')",
    "datetime('2000-01-01', '+0000-00-31')",
    "datetime('2000-01-01', '+0000-00-30')",
    "datetime('2000-01-01', '0000-00-01')",
    "datetime('2000-01-01', '+0000-00-01x')",
    "datetime('2000-01-01', '+0000-00-01 x')",
    "datetime('2000-01-01', '+0000-00-01 12:00:00.5', 'subsec')",
    "datetime('2000-01-01', '+0000-00-01  12:00')",
    "datetime('2000-01-01', '12:30')",
    "datetime('2000-01-01', '-12:30')",
    "datetime('2000-01-01', '+12:00:00.5', 'subsec')",
    "datetime('2000-01-01', '-00:00:00.001', 'subsec')",
    "datetime('2000-01-01', '+23:59:59.9999', 'subsec')",
    "datetime('2000-01-01', '+12:00+01:00')",
    "datetime('2000-01-01', '+12:00 x')",
    "datetime('2000-01-01', '12:00:00 ')",
    "datetime('2000-01-01', '+1:00')",
    "datetime('2000-01-01', '+001:00')",
    "time('24:00', '12:00')",
    // start of / end of (3.54: 'end of').
    "datetime('2000-05-17 13:14:15', 'start of day')",
    "datetime('2000-05-17 13:14:15', 'start of month')",
    "datetime('2000-05-17 13:14:15', 'start of year')",
    "datetime('2000-05-17', 'start of week')",
    "datetime('12:34', 'start of day')",
    "datetime(2451545.7, 'start of day')",
    "datetime(1700000000, 'start of day')",
    "date('2000-01-01', 'START OF MONTH')",
    "datetime('2000-05-17', 'start of ')",
    "datetime('2000-05-17', 'start of day', 'start of day')",
    "datetime('2000-05-17 13:14:15.5', 'end of day', 'subsec')",
    "datetime('2000-05-17 13:14:15', 'end of month', 'subsec')",
    "datetime('2000-02-17 13:14:15', 'end of month')",
    "datetime('2001-02-17', 'end of month')",
    "datetime('2000-05-17 13:14:15', 'end of year', 'subsec')",
    "datetime('2000-05-17', 'end of week')",
    "datetime('12:34', 'end of day')",
    "datetime(2451545.7, 'end of day')",
    "datetime(1700000000, 'end of day')",
    "datetime('2000-05-17', 'END OF MONTH')",
    "julianday('2000-05-17', 'end of day')",
    "datetime('9999-12-31', 'end of day', 'subsec')",
    "datetime('9999-12-31', 'end of year')",
    "datetime('2000-05-17 13:14:15+02:00', 'end of day')",
    "datetime('2000-01-31', 'end of month', '+1 month')",
    "datetime('2000-01-31', '+1 month', 'end of month')",
    "datetime('2000-05-17', 'end of  month')",
    "datetime('2000-05-17', 'end of month ')",
    "datetime('2000-05-17', 'end of')",
    "datetime('2001-02-31', 'end of day', 'floor')",
    // Review regressions: 'end of month' is the next month's first
    // midnight less 1 ms (an error in December 9999; an invalid
    // value falls back to 2000-01-01 before the subtraction).
    "datetime('9999-12-15', 'end of month')",
    "datetime('9999-12-15', 'end of month', 'localtime')",
    "datetime('9999-12-15', 'end of month', 'utc')",
    "datetime(5, '-25 days', 'end of month', 'utc')",
    "datetime(5, '-25 days', 'end of month', 'localtime')",
    "datetime(5, '-25 days', 'end of month')",
    "unixepoch('9999-12-31 23:59:59.999', 'subsecond', 'end of month')",
    "julianday('9999-11-30', '+1 month', 'end of month')",
    "strftime('%f %J', '9999-12-01', 'start of month', 'end of month')",
    "datetime('9999-11-15', 'end of month', 'subsec')",
    "datetime('2024-02-30', 'end of month', 'floor')",
    "datetime('2024-02-30', 'end of month', 'floor', 'floor')",
    "datetime('-4713-11-24 12:00', 'end of month')",
    "datetime(5, '-25 days', 'end of year', 'utc')",
    "datetime(5, '-25 days', 'end of day', 'utc')",
    // Review regressions: a `24:00` value's day-of-month overflow is
    // folded into the next midnight; 'floor' has nothing to undo.
    "datetime('2024-02-30 24:00:00', 'floor')",
    "datetime('2024-02-30 24:00:00', 'end of day', 'floor')",
    "datetime('2024-02-30 24:00:00', 'end of year', 'floor')",
    "datetime('2024-02-30 24:00:00', 'start of day', 'floor')",
    "datetime('2024-02-30 24:00:00', 'start of month', 'floor')",
    "datetime('2024-02-30 24:00:00+01:00', 'floor')",
    "datetime('2024-02-30 24:00:00Z', 'floor')",
    "datetime('2023-02-31 24:00', 'end of year', 'floor')",
    "datetime('2024-04-31 24:00:00.000', 'floor')",
    "datetime('2024-02-30 12:00:00+01:00', 'floor')",
    "datetime('2001-02-31', 'end of month', 'floor')",
    "datetime('2001-02-31', 'end of year', 'floor')",
    "datetime('2001-02-31', 'start of day', 'floor')",
    "datetime('2001-02-31', 'start of year', 'floor')",
    "datetime('2001-02-31', 'end of month')",
    "datetime('2001-02-31', 'start of month')",
    "datetime('2000-02-10', 'end of month')",
    "datetime('1900-02-10', 'end of month')",
    "datetime('2400-02-10', 'end of month')",
    "datetime('-0004-02-10', 'end of month')",
    "datetime('-0100-02-10', 'end of month')",
    "datetime('2000-01-01', 'end of day', '+1 second', 'subsec')",
    "datetime('2000-01-01', 'end of day', '+0.0005 seconds', 'subsec')",
    "strftime('%f %s %J', '2000-01-01', 'end of day')",
    "datetime('12:00', 'end of month')",
    "time('12:00', 'end of day')",
    "datetime(2451545, 'end of year')",
    // weekday N, and 3.54's backward weekday -N.
    "date('2000-01-01', 'weekday 0')",
    "date('2000-01-01', 'weekday 6')",
    "date('2000-01-01', 'weekday 7')",
    "date('2000-01-01', 'weekday 1.0')",
    "date('2000-01-01', 'weekday 1.5')",
    "date('2000-01-01', 'weekday  1')",
    "date('2000-01-01', 'weekday 1 ')",
    "date('2000-01-01', 'weekday')",
    "date('2000-01-01', 'weekday 3e0')",
    "date('2000-01-01', 'Weekday 0')",
    "date('2000-01-01', 'weekday -0')",
    "date('2000-01-05', 'weekday -0')",
    "date('2000-01-02', 'weekday -0')",
    "date('2000-01-01', 'weekday -0.0')",
    "date('2000-01-01', 'weekday -1')",
    "date('2000-01-02', 'weekday -1')",
    "date('2000-01-05', 'weekday -6')",
    "date('2000-01-01', 'weekday -5')",
    "date('2000-01-01', 'weekday -7')",
    "date('2000-01-01', 'weekday - 1')",
    "date('2000-01-01', 'weekday -0.5')",
    "date('2000-01-01', 'weekday  -1')",
    "date('2000-01-01', 'weekday 0x1')",
    "date('2000-01-01', 'weekday 6.9999999999999999')",
    "datetime('2000-01-02 12:34:56.789', 'weekday 3', 'subsec')",
    "datetime('2000-01-02 12:34:56.789+01:00', 'weekday -3', 'subsec')",
    // A chain.
    "datetime('2000-01-01', '+1 day', '-2 hours', 'start of month', '+1.25 months', \
     'weekday 3', 'end of day')",
    // Regression: a date invalid before 'localtime' comes back in
    // year 2000 (or year 0), its negative milliseconds as the pin's
    // punctuation.
    "date('-5000-01-01', 'localtime')",
    "datetime('-5000-01-01 10:00', 'localtime')",
    "datetime('2000-01-01', '-3000000 days', 'localtime')",
    "datetime('2000-01-01 00:00:00.5', '-3000000 days', 'localtime', 'subsec')",
    "datetime('2000-01-01', '+20000 years', 'localtime')",
    // localtime / utc around the transitions and out of 1970..2037.
    "datetime('2024-03-10 06:59:59', 'localtime')",
    "datetime('2024-03-10 07:00:00', 'localtime')",
    "datetime('2024-11-03 05:59:59', 'localtime')",
    "datetime('2024-11-03 06:00:00', 'localtime')",
    "datetime('2024-03-10 02:30:00', 'utc')",
    "datetime('2024-03-10 01:59:59', 'utc')",
    "datetime('2024-11-03 01:30:00', 'utc')",
    "datetime('2024-11-03 00:59:59', 'utc')",
    "datetime('1969-07-20 20:17:40', 'localtime')",
    "datetime('1900-01-01 12:00', 'localtime')",
    "datetime('2100-07-01 12:00', 'localtime')",
    "datetime('-4713-11-25 12:00', 'localtime')",
    "datetime('9999-12-31 23:00', 'localtime')",
    "datetime('2038-01-18 12:00', 'localtime')",
    "datetime('2038-01-19 12:00', 'localtime')",
    "datetime('1970-01-01 04:00', 'localtime')",
    "datetime('2024-06-01 12:00:00.567', 'localtime', 'subsec')",
    "datetime('2024-06-01 12:00', 'localtime', 'localtime')",
    "datetime('2024-06-01 12:00', 'utc', 'utc')",
    "datetime('2024-06-01 12:00', 'localtime', 'utc')",
    "datetime('2024-06-01 12:00Z', 'utc')",
    "datetime(1717243200, 'unixepoch', 'utc')",
    "datetime('2000-01-01', 'localtime', 'start of day', 'utc')",
    "datetime('9999-12-31 23:59:59', 'utc')",
    "datetime('-4713-11-24 12:00', 'utc')",
    "datetime('0000-01-01', 'localtime')",
    "strftime('%s', '2024-06-01', 'localtime')",
    "unixepoch('2024-06-01', 'utc')",
    "datetime('2000-01-01', 'LocalTime')",
    // strftime: every conversion, the unknown ones and %y (unknown
    // on 3.54.1), negative years.
    "strftime('%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%', \
     '2024-03-05 07:08:09.123')",
    "strftime('%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%', \
     '-0012-03-05 17:08:09.123')",
    "strftime('%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%', \
     '2021-01-01 00:00:00.5', 'subsec')",
    "strftime('%y', '2024-03-05')",
    "strftime('%a', '2000-01-01')",
    "strftime('%Q', '2000-01-01')",
    "strftime('abc%', '2000-01-01')",
    "strftime('%', '2000-01-01')",
    "strftime('x%%y', '2000-01-01')",
    "strftime(NULL, '2000-01-01')",
    "strftime('%Y', NULL)",
    "strftime(5, '2000-01-01')",
    "strftime('%Y'||char(0)||'%Q', '2000-01-01')",
    "strftime('%Y %F %G %g', '-0001-03-01')",
    "strftime('%J %s %j %U %V %W %u %w', '-0012-03-01')",
    "strftime('%Y', '-4713-11-24 12:00')",
    "strftime('%G %V', '2021-01-03')",
    "strftime('%G %V', '2020-12-31')",
    "strftime('%G %V', '2024-12-30')",
    "strftime('%I %l %p', '2000-01-01 00:30')",
    "strftime('%I %l %p', '2000-01-01 12:30')",
    "strftime('%I %l %P', '2000-01-01 13:30')",
    "strftime('é%Y→', '2000-01-01')",
    // timediff.
    "timediff('2001-03-01', '2000-02-29')",
    "timediff('2000-02-29', '2001-03-01')",
    "timediff('2000-01-01', '2000-01-01')",
    "timediff('2000-01-31 12:00', '2000-03-01')",
    "timediff('2000-03-01', '2000-01-31 12:00')",
    "timediff('12:00', '11:59:59.5')",
    "timediff(2451545, 0)",
    "timediff('9999-12-31 23:59:59.999', '-4713-11-24 12:00')",
    "timediff('-4713-11-24 12:00', '9999-12-31 23:59:59.999')",
    "timediff(NULL, '2000-01-01')",
    "timediff('x', '2000-01-01')",
    "timediff('2023-02-15', '2023-03-31')",
    "timediff('2023-03-31', '2023-02-15')",
    "timediff('2024-02-29', '2023-02-28')",
    "timediff('2023-02-28', '2024-02-29')",
    "timediff('2000-01-31', '2000-03-01')",
    "timediff('2000-03-31', '2000-02-01')",
    "timediff('2000-01-01 00:00:00.001', '2000-01-01')",
    "timediff('2000-01-01', '2000-01-01 00:00:00.001')",
    "timediff('2000-02-31', '2000-01-01')",
    "timediff('2000-12-31 23:00', '2000-01-31 23:30')",
    "timediff('2000-01-31 23:30', '2000-12-31 23:00')",
    "timediff('9999-12-31', '9999-11-30 12:00')",
    "timediff('9999-11-30 12:00', '9999-12-31')",
    "timediff('-4713-11-24 12:00', '-4713-12-24 11:00')",
    // In expressions: comparisons, affinity, aggregates.
    "date('2000-01-01') = '2000-01-01'",
    "julianday('2000-01-02') - julianday('2000-01-01')",
    "date('2000-01-01', '+' || 10 || ' days')",
    "(SELECT group_concat(date('2000-01-01', '+' || value || ' months'), ',') \
     FROM json_each('[0,1,2,3,13,-1]'))",
];

#[test]
fn fixed_corpus_matches_pin_in_every_zone() {
    let exprs: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    for (tz, zone) in ZONES {
        assert_same(tz, zone, &exprs);
    }
}

/// xorshift64 (as `fuzz_smoke.rs`).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    /// A double in `lo..hi` with 6 fraction digits.
    fn real(&mut self, lo: i64, hi: i64) -> String {
        format!("{}.{:06}", self.range(lo, hi - 1), self.below(1_000_000))
    }
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn gen_year(r: &mut Rng) -> String {
    match r.below(10) {
        0 => r
            .pick(&[
                "0000", "9999", "-4713", "-0001", "1970", "2000", "1900", "2100", "2038", "1969",
            ])
            .to_string(),
        1 => format!("-{:04}", r.range(0, 4713)),
        _ => format!("{:04}", r.range(0, 9999)),
    }
}

fn gen_frac(r: &mut Rng) -> String {
    let n = r.range(1, 6);
    let mut s = String::from(".");
    for _ in 0..n {
        s.push((b'0' + r.below(10) as u8) as char);
    }
    s
}

fn gen_hms(r: &mut Rng) -> String {
    let h = match r.below(4) {
        0 => 24,
        1 => r.range(0, 30),
        _ => r.range(0, 23),
    };
    let m = match r.below(6) {
        0 => 0,
        1 => 60,
        _ => r.range(0, 59),
    };
    let mut s = format!("{h:02}:{m:02}");
    if r.chance(70) {
        let sec = match r.below(6) {
            0 => 59,
            1 => 60,
            _ => r.range(0, 59),
        };
        s.push_str(&format!(":{sec:02}"));
        if r.chance(40) {
            s.push_str(&gen_frac(r));
        }
    }
    if r.chance(20) {
        s.push_str(r.pick(&[
            "Z", "z", "+05:30", "-08:00", " +01:00", "+14:00", "-00:00", "+15:00", "+5:00",
        ]));
    }
    s
}

fn gen_value(r: &mut Rng) -> String {
    match r.below(20) {
        0..=8 => {
            let mut s = format!(
                "{}-{:02}-{:02}",
                gen_year(r),
                if r.chance(90) {
                    r.range(1, 12)
                } else {
                    r.range(0, 13)
                },
                match r.below(4) {
                    0 => r.range(29, 31),
                    1 => r.range(0, 32),
                    _ => r.range(1, 28),
                }
            );
            if r.chance(60) {
                s.push_str(r.pick(&[" ", "T", "  "]));
                s.push_str(&gen_hms(r));
            }
            q(&s)
        }
        9 | 10 => q(&gen_hms(r)),
        11 | 12 => match r.below(4) {
            0 => r.real(0, 5_373_484),
            1 => r.real(-10, 10),
            2 => r.real(2_440_000, 2_470_000),
            _ => r.range(0, 5_373_484).to_string(),
        },
        13 | 14 => match r.below(3) {
            0 => r.range(-210_866_760_000, 253_402_300_799).to_string(),
            1 => r.range(-1_000_000_000_000, 1_000_000_000_000).to_string(),
            _ => r.real(-100_000_000_000, 100_000_000_000),
        },
        15 => q(&r.real(0, 3_000_000)),
        16 => q(r.pick(&[
            "2000",
            "12",
            "x",
            "",
            "  2451545.5 ",
            "1e400",
            "-1",
            "2000-01-01x",
            "2000-01-01 ",
            "2000-01-01T",
            "12:00 x",
        ])),
        17 => r
            .pick(&[
                "NULL",
                "x'323030302d30312d3031'",
                "'2024-02-29'",
                "'1900-02-29'",
            ])
            .to_string(),
        _ => q(&format!(
            "{}-{:02}-{:02}",
            gen_year(r),
            r.range(1, 12),
            r.range(1, 31)
        )),
    }
}

fn gen_num(r: &mut Rng) -> String {
    match r.below(10) {
        0..=4 => r.range(-1000, 1000).to_string(),
        5 | 6 => format!("{}.{}", r.range(-500, 499), r.below(10_000)),
        7 => format!("{}{}", r.pick(&["+", "-", ""]), r.range(0, 10_000_000)),
        8 => r
            .pick(&[
                "1e3",
                "-2.5e1",
                "+0",
                "-0",
                "0.0005",
                "176545",
                "14712",
                "5373484",
                "464269060799",
            ])
            .to_string(),
        _ => format!("{}.{:06}", r.range(-3, 2), r.below(1_000_000)),
    }
}

fn gen_modifier(r: &mut Rng) -> String {
    const UNITS: &[&str] = &[
        "day", "days", "hour", "hours", "minute", "minutes", "second", "seconds", "month",
        "months", "year", "years", "DAYS", "Month", "ye", "secondss",
    ];
    match r.below(100) {
        0..=34 => q(&format!(
            "{}{}{}",
            gen_num(r),
            r.pick(&[" ", " ", "  ", "\t"]),
            r.pick(UNITS)
        )),
        35..=44 => q(r.pick(&[
            "start of day",
            "start of month",
            "start of year",
            "end of day",
            "end of month",
            "end of year",
            "START OF MONTH",
            "start of week",
            "end of  day",
        ])),
        45..=51 => q(&format!(
            "weekday {}",
            r.pick(&[
                "0", "1", "2", "3", "4", "5", "6", "-0", "-1", "-3", "-6", "7", "-7", "1.5", "2.0",
                " 3",
            ])
        )),
        52..=61 => q(r.pick(&[
            "unixepoch",
            "julianday",
            "auto",
            "localtime",
            "utc",
            "subsec",
            "subsecond",
            "ceiling",
            "floor",
            "LOCALTIME",
        ])),
        62..=74 => {
            let frac = gen_frac(r);
            let tail = match r.below(3) {
                0 => String::new(),
                1 => format!(":{:02}", r.range(0, 59)),
                _ => format!(":{:02}{frac}", r.range(0, 59)),
            };
            q(&format!(
                "{}{:02}:{:02}{tail}",
                r.pick(&["+", "-", ""]),
                r.range(0, 30),
                r.range(0, 70)
            ))
        }
        75..=87 => {
            let y = match r.below(3) {
                0 => format!("{:04}", r.range(0, 30)),
                1 => format!("{:04}", r.range(0, 9999)),
                _ => format!("{:05}", r.range(0, 15000)),
            };
            let mut s = format!(
                "{}{y}-{:02}-{:02}",
                r.pick(&["+", "-", ""]),
                r.range(0, 13),
                r.range(0, 32)
            );
            if r.chance(30) {
                s.push_str(&format!(
                    " {:02}:{:02}{}",
                    r.range(0, 25),
                    r.range(0, 59),
                    r.pick(&["", ":30", ":59.5"])
                ));
            }
            q(&s)
        }
        _ => r
            .pick(&["NULL", "5", "1.5", "'+1 day'", "' +1 day'", "'+1day'"])
            .to_string(),
    }
}

fn gen_format(r: &mut Rng) -> String {
    const KNOWN: &str = "deFfGgHIjJklmMpPRsSTuUVwWY%";
    const ANY: &str = "dDeEfFgGHIjJkKlmMpPRsStTuUVwWyY%aQ";
    let mut s = String::new();
    for _ in 0..r.range(1, 6) {
        if r.chance(75) {
            let set = if r.chance(90) { KNOWN } else { ANY };
            s.push('%');
            s.push(set.as_bytes()[r.below(set.len() as u64) as usize] as char);
        } else {
            s.push_str(r.pick(&[" ", "-", ":", "x", "é", "T", "/"]));
        }
    }
    if r.chance(3) {
        s.push('%');
    }
    q(&s)
}

fn gen_case(r: &mut Rng) -> String {
    let n = [0, 0, 1, 1, 2, 3, 4][r.below(7) as usize];
    let mut args = vec![gen_value(r)];
    for _ in 0..n {
        args.push(gen_modifier(r));
    }
    let args = args.join(", ");
    match r.below(100) {
        0..=14 => format!("date({args})"),
        15..=24 => format!("time({args})"),
        25..=44 => format!("datetime({args})"),
        45..=54 => format!("julianday({args})"),
        55..=61 => format!("unixepoch({args})"),
        62..=89 => format!("strftime({}, {args})", gen_format(r)),
        _ => format!("timediff({}, {})", gen_value(r), gen_value(r)),
    }
}

/// How many generated cases each zone runs (`HIPP_DATETIME_CORPUS`
/// scales it).
fn corpus_size(default: usize) -> usize {
    std::env::var("HIPP_DATETIME_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn generated_corpus_matches_pin_in_every_zone() {
    for (k, (tz, zone)) in ZONES.into_iter().enumerate() {
        let mut r = Rng(0x0d47_e71e_5eed_0001 ^ (k as u64) << 32);
        let n = corpus_size(if tz == "UTC" { 6000 } else { 2500 });
        let exprs: Vec<String> = (0..n).map(|_| gen_case(&mut r)).collect();
        assert_same(tz, zone, &exprs);
    }
}

/// Every combination of a base value, a modifier before, an `'end
/// of'` modifier and up to two modifiers after it (review
/// regression: `'end of month'` in December 9999 and on invalid
/// values, and `'floor'` after a `24:00` overflow, differed).
#[test]
fn end_of_combinations_match_pin() {
    const BASES: &[&str] = &[
        "'2024-02-30 24:00:00'",
        "'2024-01-31 24:00'",
        "'2023-02-29'",
        "'2024-02-29 12:34:56.789'",
        "'2000-12-31 23:59:59.999'",
        "'9999-12-15'",
        "'9999-12-31 23:59:59.999'",
        "'9999-11-30 12:00'",
        "'-4713-11-24 12:00'",
        "'0000-01-01'",
        "5",
        "2460000.25",
        "'12:00'",
        "'2001-06-15 10:00+05:30'",
    ];
    const BEFORE: &[&str] = &[
        "",
        "'-25 days'",
        "'+1 month'",
        "'localtime'",
        "'utc'",
        "'start of month'",
        "'subsec'",
        "'auto'",
        "'+10 years'",
    ];
    const END: &[&str] = &["'end of day'", "'end of month'", "'end of year'"];
    const AFTER: &[&str] = &[
        "",
        "'floor'",
        "'ceiling'",
        "'utc'",
        "'localtime'",
        "'+1 day'",
        "'subsec'",
        "'start of day'",
        "'weekday 0'",
    ];
    let funcs = ["datetime", "julianday", "unixepoch"];
    let mut exprs = Vec::new();
    for (bi, b) in BASES.iter().enumerate() {
        for pre in BEFORE {
            for (ei, e) in END.iter().enumerate() {
                for a1 in AFTER {
                    for a2 in ["", "'floor'", "'utc'"] {
                        let mut args = vec![b.to_string()];
                        args.extend(
                            [*pre, *e, *a1, a2]
                                .iter()
                                .filter(|m| !m.is_empty())
                                .map(|m| m.to_string()),
                        );
                        let f = funcs[(bi + ei + exprs.len()) % funcs.len()];
                        exprs.push(format!("{f}({})", args.join(", ")));
                    }
                }
            }
        }
    }
    // UTC throughout; New York over every third case.
    assert_same("UTC", zone_utc, &exprs);
    let ny: Vec<String> = exprs.iter().step_by(3).cloned().collect();
    assert_same("America/New_York", zone_ny, &ny);
}

/// Instants around the daylight-saving transitions (and in years
/// the pin maps into 1970..2037), through 'localtime' and 'utc'.
#[test]
fn dst_corpus_matches_pin() {
    let mut r = Rng(0x0d57_0000_0000_0001);
    let mut exprs = Vec::new();
    for _ in 0..1500 {
        let y = match r.below(3) {
            0 => r.range(1970, 2037),
            1 => r.range(1, 9999),
            _ => r.range(1900, 2100),
        };
        let mo = match r.below(5) {
            0 => 3,
            1 => 4,
            2 => 10,
            3 => 11,
            _ => r.range(1, 12),
        };
        let t = format!(
            "'{y:04}-{mo:02}-{:02} {:02}:{:02}:{:02}'",
            r.range(1, 31),
            r.range(0, 23),
            [0, 30, 59, r.range(0, 59)][r.below(4) as usize],
            r.range(0, 59)
        );
        let mods = r.pick(&[
            "'localtime'",
            "'utc'",
            "'localtime', 'utc'",
            "'utc', 'localtime'",
            "'utc', 'utc'",
            "'localtime', '+1 hour', 'utc'",
            "'subsec', 'localtime'",
        ]);
        exprs.push(format!("datetime({t}, {mods})"));
        exprs.push(format!("strftime('%s %H:%M %j', {t}, {mods})"));
    }
    for (tz, zone) in ZONES {
        assert_same(tz, zone, &exprs);
    }
}

/// 'now' is the clock's instant: the same text the pin gives for
/// that instant written as a number.
#[test]
fn now_reads_the_clock_hook() {
    let pairs = [
        (
            "datetime('now', 'subsec')",
            "datetime(1700000000.123, 'unixepoch', 'subsec')",
        ),
        ("datetime('now')", "datetime(1700000000.123, 'unixepoch')"),
        ("date()", "date(1700000000.123, 'unixepoch')"),
        ("time()", "time(1700000000.123, 'unixepoch')"),
        ("datetime()", "datetime(1700000000.123, 'unixepoch')"),
        ("julianday('now')", "julianday(1700000000.123, 'unixepoch')"),
        ("unixepoch()", "unixepoch(1700000000.123, 'unixepoch')"),
        (
            "unixepoch('subsec')",
            "unixepoch(1700000000.123, 'unixepoch', 'subsec')",
        ),
        (
            "datetime('subsecond')",
            "datetime(1700000000.123, 'unixepoch', 'subsec')",
        ),
        (
            "strftime('%Y %j %f %s')",
            "strftime('%Y %j %f %s', 1700000000.123, 'unixepoch')",
        ),
        ("strftime('x')", "'x'"),
        (
            "datetime('NOW', 'localtime', 'start of day')",
            "datetime(1700000000.123, 'unixepoch', 'localtime', 'start of day')",
        ),
        ("CURRENT_TIMESTAMP", "datetime(1700000000.123, 'unixepoch')"),
        ("current_date", "date(1700000000.123, 'unixepoch')"),
        ("Current_Time", "time(1700000000.123, 'unixepoch')"),
        (
            "timediff('now', '2000-01-01')",
            "timediff(datetime(1700000000.123, 'unixepoch', 'subsec'), '2000-01-01')",
        ),
    ];
    for (tz, zone) in ZONES {
        let pin: Vec<String> = pairs.iter().map(|(_, p)| p.to_string()).collect();
        let mut script = String::new();
        for (i, e) in pin.iter().enumerate() {
            script.push_str(&format!("SELECT {i}, quote({e});\n"));
        }
        let want = oracle_tz(tz, ":memory:", &script);
        block_on(async {
            let mut db = mem_db(zone).await;
            for (i, (h, _)) in pairs.iter().enumerate() {
                let got = try_collect(&mut db, &format!("SELECT {i}, quote({h})"))
                    .await
                    .expect(h);
                let want_line = want.iter().find(|l| l.starts_with(&format!("{i}|")));
                assert_eq!(Some(&got[0]), want_line, "{h} in {tz}");
            }
        });
    }
}

/// A clock that moves one second per read (test-only state).
static TICKS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
fn ticking_clock() -> i64 {
    1_700_000_000_000 + 1000 * TICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// One instant per statement execution (the pin's
/// `sqlite3StmtCurrentTime`): every 'now', CURRENT_* and DEFAULT of
/// a statement — its triggers too — reads it; the next statement
/// reads the clock again.
#[test]
fn now_is_stable_within_a_statement() {
    block_on(async {
        let cfg = Config::default().clock(ticking_clock).tz_offset(None);
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        let same = try_collect(
            &mut db,
            "SELECT datetime('now') = datetime('now') AND CURRENT_TIMESTAMP = datetime() \
             AND julianday() = julianday('now') AND unixepoch() = unixepoch('now')",
        )
        .await
        .unwrap();
        assert_eq!(same, ["1"]);
        let rows = try_collect(
            &mut db,
            "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 50) \
             SELECT count(DISTINCT strftime('%s', 'now')) FROM r",
        )
        .await
        .unwrap();
        assert_eq!(rows, ["1"]);
        let a = try_collect(&mut db, "SELECT unixepoch()").await.unwrap();
        let b = try_collect(&mut db, "SELECT unixepoch()").await.unwrap();
        assert_ne!(a, b, "a new statement reads the clock again");
        for s in [
            "CREATE TABLE t(a DEFAULT CURRENT_TIMESTAMP, b DEFAULT (datetime('now')), c, d)",
            "CREATE TABLE log(x)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             INSERT INTO log VALUES (datetime('now')); END",
            "INSERT INTO t(c) VALUES (datetime('now')), (CURRENT_TIMESTAMP)",
        ] {
            try_collect(&mut db, s).await.expect(s);
        }
        let rows = try_collect(
            &mut db,
            "SELECT count(DISTINCT a), count(DISTINCT b), count(DISTINCT c), \
             (SELECT count(DISTINCT x) FROM log), (SELECT x FROM log LIMIT 1) = a FROM t",
        )
        .await
        .unwrap();
        assert_eq!(rows, ["1|1|1|1|1"]);
        // A prepared statement re-executed reads the clock again.
        let mut q = db.prepare("SELECT unixepoch()").await.unwrap();
        assert_eq!(q.step().await.unwrap(), Step::Row);
        let first = q.column_value(0).map(|v| format!("{v:?}"));
        while q.step().await.unwrap() == Step::Row {}
        q.reset();
        assert_eq!(q.step().await.unwrap(), Step::Row);
        let second = q.column_value(0).map(|v| format!("{v:?}"));
        assert_ne!(first, second);
    });
}

#[test]
fn no_clock_is_an_error_only_when_read() {
    block_on(async {
        let cfg = Config::default().no_clock();
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        for s in [
            "SELECT date('now')",
            "SELECT date()",
            "SELECT CURRENT_DATE",
            "SELECT strftime('%Y')",
            "SELECT datetime('subsec')",
        ] {
            let e: Result<Vec<String>, MErr> = try_collect(&mut db, s).await;
            assert!(matches!(e, Err(Error::NoClock)), "{s}: {e:?}");
        }
        let ok = try_collect(&mut db, "SELECT date('2000-01-01', 'localtime')")
            .await
            .unwrap();
        assert_eq!(ok, ["2000-01-01"]);
    });
}

fn unavailable(_: i64) -> Option<i64> {
    None
}

#[test]
fn a_zone_hook_without_an_answer_fails_the_statement() {
    block_on(async {
        let cfg = Config::default()
            .clock(fixed_clock)
            .tz_offset(Some(unavailable));
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        for s in [
            "SELECT datetime('2000-01-01', 'localtime')",
            "SELECT datetime('2000-01-01', 'utc')",
        ] {
            let e = try_collect(&mut db, s).await;
            assert!(
                matches!(
                    e,
                    Err(Error::Schema {
                        kind: SchemaKind::LocalTimeUnavailable
                    })
                ),
                "{s}: {e:?}"
            );
        }
        // Without 'localtime' / 'utc' the zone is never asked.
        let ok = try_collect(&mut db, "SELECT datetime('2000-01-01 12:00+01:00')")
            .await
            .unwrap();
        assert_eq!(ok, ["2000-01-01 11:00:00"]);
        // No hook at all is UTC.
        let cfg = Config::default().clock(fixed_clock).tz_offset(None);
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        let utc = try_collect(&mut db, "SELECT datetime('2000-01-01', 'localtime')")
            .await
            .unwrap();
        assert_eq!(utc, ["2000-01-01 00:00:00"]);
    });
}

/// Run each `;`-separated statement of `sql` (no `;` inside its
/// literals), the rows of all of them, or the first error.
async fn run_all<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut rows = Vec::new();
    for s in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        rows.extend(try_collect(db, s).await?);
    }
    Ok(rows)
}

/// Statement scripts run on both engines (file databases, `TZ` =
/// New York): each statement's success or failure and the final
/// query results must agree.
fn script_agrees(tag: &str, stmts: &[&str], check: &str) {
    let hp = common::temp_db(&format!("v16dt-{tag}-h"));
    let op = common::temp_db(&format!("v16dt-{tag}-o"));
    let mut pin_ok = Vec::new();
    for s in stmts {
        let out = std::process::Command::new(common::oracle())
            .arg(&op)
            .arg(s)
            .env("TZ", "America/New_York")
            .output()
            .expect("oracle runs");
        pin_ok.push(out.status.success() && out.stderr.is_empty());
    }
    let want = oracle_tz("America/New_York", op.to_str().unwrap(), check);
    block_on(async {
        let fs = hipp::FileStorage::open(&hp).expect("fs");
        let cfg = Config::default().tz_offset(Some(zone_ny));
        let mut db = Connection::open(fs, cfg).await.expect("open");
        for (s, ok) in stmts.iter().zip(&pin_ok) {
            let r = run_all(&mut db, s).await;
            assert_eq!(r.is_ok(), *ok, "{tag}: {s}: {r:?}");
        }
        let got = run_all(&mut db, check).await.expect(check);
        assert_eq!(got, want, "{tag}");
    });
    assert_eq!(
        oracle_tz("UTC", hp.to_str().unwrap(), "PRAGMA integrity_check;"),
        ["ok"]
    );
}

/// The pin's "pure" contexts: a date function reading the clock or
/// the zone fails the statement in a CHECK, an index expression, a
/// partial index's WHERE or a generated column; the CURRENT_*
/// keywords are refused in generated columns and indexes when the
/// table is created and are allowed in a CHECK.
#[test]
fn pure_contexts_refuse_the_clock_and_zone() {
    script_agrees(
        "pure",
        &[
            "CREATE TABLE t(a, CHECK (a < date('now')))",
            "INSERT INTO t VALUES ('1999-01-01')",
            "CREATE TABLE t1(a CHECK (date(a, '+1 day') > '2000'))",
            "INSERT INTO t1 VALUES ('2000-01-01')",
            "CREATE TABLE t1b(a CHECK (datetime(a, 'localtime') IS NOT NULL))",
            "INSERT INTO t1b VALUES ('2000-01-01')",
            "CREATE TABLE t2(a, b AS (date(a)), c AS (date(a, 'localtime')))",
            "INSERT INTO t2(a) VALUES ('2000-01-01')",
            "CREATE TABLE t2b(a, b AS (date(a)))",
            "INSERT INTO t2b(a) VALUES ('2000-01-01')",
            "CREATE TABLE t3(a)",
            "INSERT INTO t3 VALUES ('2000-01-01')",
            "CREATE INDEX i3 ON t3(date(a, '+1 day'))",
            "CREATE INDEX i4 ON t3(date(a, 'utc'))",
            "CREATE INDEX i5 ON t3(a) WHERE a < date('now')",
            "CREATE INDEX i6 ON t3(datetime(a, 'subsec'))",
            "CREATE INDEX i7 ON t3(datetime('subsec'))",
            "CREATE INDEX i8 ON t3(unixepoch())",
            "CREATE TABLE t4(a CHECK (a <> current_date))",
            "INSERT INTO t4 VALUES (1)",
            "CREATE TABLE t5(a, b AS (current_date))",
            "CREATE INDEX i9 ON t3(a) WHERE current_date > a",
            "INSERT INTO t3 VALUES ('2001-01-01')",
        ],
        "SELECT (SELECT count(*) FROM t), (SELECT count(*) FROM t1), \
         (SELECT count(*) FROM t4), (SELECT group_concat(b) FROM t2b), \
         (SELECT group_concat(name) FROM sqlite_schema WHERE type = 'index'), \
         (SELECT count(*) FROM t3 WHERE date(a, '+1 day') = '2000-01-02');",
    );
}

/// The CURRENT_* keywords are calls (the pin's CTIME_KW), never a
/// table's own "current_date" column (regression: hipp resolved
/// the bare keyword as that column), and take no parentheses.
#[test]
fn clock_keywords_never_read_a_column() {
    script_agrees(
        "ctime",
        &[
            "CREATE TABLE x(\"current_date\", \"current_time\", b)",
            "INSERT INTO x VALUES (5, 6, 7)",
            "SELECT current_date()",
            "SELECT current_timestamp(1)",
            "CREATE TABLE y(a DEFAULT CURRENT_TIMESTAMP, b DEFAULT (CURRENT_DATE || '!'), \
             c DEFAULT current_time, z)",
            "INSERT INTO y(z) VALUES (1)",
            "ALTER TABLE y ADD COLUMN d DEFAULT CURRENT_TIME",
            "CREATE TABLE z(a DEFAULT (\"current_date\"))",
            // Review regression: a bare keyword never qualifies a
            // column (hipp read `current_date.x` as one).
            "CREATE TABLE current_date(x)",
            "INSERT INTO current_date VALUES (1)",
            "SELECT current_date.x FROM current_date",
            "SELECT current_date.* FROM current_date",
            "SELECT current_time.x",
            "SELECT current_date . x FROM x AS current_date",
            "SELECT x FROM current_date",
            "SELECT \"current_date\".x, \"current_date\".* FROM current_date",
            "SELECT x.current_date FROM x",
            "CREATE TABLE w(a DEFAULT current_date.x)",
            "CREATE TABLE w2(a CHECK (current_date.x))",
            "INSERT INTO x VALUES (1, 2, 3) RETURNING current_date.*",
        ],
        "SELECT current_date = 5, \"current_date\", x.current_time, current_time = 6, \
         typeof(CURRENT_TIME), length(current_timestamp) FROM x; \
         SELECT length(a), length(b), length(c), typeof(a) FROM y;",
    );
}

#[test]
fn timediff_takes_two_arguments() {
    script_agrees(
        "tdargs",
        &[
            "SELECT timediff('2000-01-01')",
            "SELECT timediff('2000-01-01', '2000-01-01', 1)",
            "SELECT timediff()",
            "SELECT date('2000-01-01', '+1 day', '+1 day', '+1 day')",
        ],
        "SELECT timediff('2000-01-01', '1999-01-01'), strftime(), julianday(NULL);",
    );
}

/// Inside a UTF-16 database the functions read text through the
/// pin's conversions (a BLOB is UTF-16 text there).
#[test]
fn utf16_database_values() {
    script_agrees(
        "utf16",
        &[
            "PRAGMA encoding = 'UTF-16le'; CREATE TABLE t(a, b BLOB)",
            "INSERT INTO t VALUES ('2000-02-29 12:00', x'3200300030003000')",
        ],
        "SELECT date(a, '+1 year'), strftime('%j|%H', a), quote(date(b)), \
         quote(date(CAST(b AS TEXT))), timediff(a, '2000-01-01') FROM t;",
    );
}
