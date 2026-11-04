//! hipp-cli v0.16 differentials: the date and time functions (leaf
//! feature `datetime`) must render byte-identically to the pinned
//! oracle in list mode — text results, julianday()'s REAL layout,
//! unixepoch()'s INTEGER / subsec REAL, NULLs — with both shells
//! run under the same `TZ` (UTC, America/New_York, Asia/Kolkata):
//! the CLI's default configuration reads the process's zone as the
//! C library does (`systime::local_offset`), so `'localtime'` /
//! `'utc'` agree across daylight-saving transitions. A table with
//! CURRENT_* defaults and date-valued CHECKs and indexes is
//! written by each shell, and the hipp-written file must pass the
//! oracle's integrity check. A generated corpus (xorshift64) mixes
//! random time values, modifier chains and strftime formats.
//! (`'now'` is only checked for shape: the two shells read the
//! clock at different instants.)

use std::path::{Path, PathBuf};
use std::process::Command;

fn hipp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hipp"))
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("hipp-cli-v16datetime-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, tz: &str, sql: &str) -> Vec<u8> {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .env("TZ", tz)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_stdin(bin: &Path, db: &Path, tz: &str, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .env("TZ", tz)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    // Write from another thread: the output of a long batch would
    // fill the pipe while this one is still writing.
    let mut stdin = child.stdin.take().expect("stdin");
    let bytes = input.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&bytes));
    let out = child.wait_with_output().expect("cli finishes");
    writer.join().expect("writer").expect("write stdin");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One batch in one zone, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, tz: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, tz, batch);
    let want = run(&oracle(), &b, tz, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{tag} in {tz}"
    );
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, tz, batch),
        run_stdin(&oracle(), &b2, tz, batch),
        "{tag} over stdin in {tz}"
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, tz, "PRAGMA integrity_check;"), b"ok\n");
}

const ZONES: [&str; 3] = ["UTC", "America/New_York", "Asia/Kolkata"];

/// The functions, their result types and the rendering of each.
const RENDER: &str = "\
SELECT date('2000-01-01'), time('12:34:56.789'), datetime('2000-01-01 12:34:56.789', 'subsec');
SELECT julianday('2000-01-01'), julianday('2000-01-01 00:00:00.001'), julianday(0), julianday('9999-12-31 23:59:59.999');
SELECT unixepoch('2000-01-01'), unixepoch('2000-01-01 00:00:00.5', 'subsec'), typeof(unixepoch('1970-01-01', 'subsec'));
SELECT strftime('%d|%e|%f|%F|%G|%g|%H|%I|%j|%J|%k|%l|%m|%M|%p|%P|%R|%s|%S|%T|%u|%U|%V|%w|%W|%Y|%%', '2024-03-05 07:08:09.123');
SELECT strftime('%y', '2024-03-05'), strftime('%Q', 'now'), strftime('abc%', '2000-01-01'), strftime('%Y', NULL);
SELECT timediff('2001-03-01', '2000-02-29'), timediff('2000-02-29', '2001-03-01'), timediff('12:00', '11:59:59.5');
SELECT date('2000-02-31'), date('2000-02-31', '+0 days'), datetime('2001-02-31 10:00', 'start of day');
SELECT time('24:00'), datetime('2000-01-31 24:00', '+1 month'), datetime('24:00', 'start of day'), time('24:00:01');
SELECT date('2000-01-01', 'weekday -1'), date('2000-01-05', 'weekday -0'), date('2000-01-01', 'weekday 7');
SELECT datetime('2000-02-17 13:14:15', 'end of month'), datetime('2000-05-17', 'end of year', 'subsec'), julianday('2000-05-17', 'end of day');
SELECT date('2001-01-31', '+1 month'), date('2001-01-31', '+1 month', 'floor'), datetime('2000-01-01', '-0001-00-00 01:00');
SELECT datetime(1700000000, 'unixepoch'), datetime(5373484.49, 'auto'), datetime(1700000000, 'auto'), datetime(2451545, 'julianday');
SELECT datetime('2000-01-01 00:00:00.5', '-3000000 days', 'localtime', 'subsec'), date('-5000-01-01', 'localtime');
SELECT datetime('2024-03-10 06:59:59', 'localtime'), datetime('2024-03-10 07:00:00', 'localtime'), datetime('2024-11-03 06:00:00', 'localtime');
SELECT datetime('2024-03-10 02:30:00', 'utc'), datetime('2024-11-03 01:30:00', 'utc'), datetime('1900-07-01 12:00', 'localtime');
SELECT datetime('12:00-00:00', 'utc'), datetime('12:00+01:00', 'utc'), datetime('2000-01-01 12:00+05:30');
SELECT length(datetime('now')), length(datetime('now', 'subsec')), typeof(julianday('now')), date('now') = current_date;
SELECT length(CURRENT_TIMESTAMP), length(current_time), length(Current_Date), datetime('now', 'localtime') IS NOT NULL;
CREATE TABLE ev(id INTEGER PRIMARY KEY, at TEXT DEFAULT CURRENT_TIMESTAMP, d DEFAULT (date('2000-01-01', '+' || 7 || ' days')), \
  CHECK (d >= date('1999-01-01')));
CREATE INDEX ev_day ON ev(date(at, 'start of month'));
INSERT INTO ev(id) VALUES (1), (2);
INSERT INTO ev(id, at) VALUES (3, '2024-02-29 23:59:59'), (4, '2023-12-31 00:00:00');
SELECT id, length(at), d FROM ev WHERE id < 3;
SELECT id, date(at, 'start of month'), strftime('%j', at), timediff(at, '2023-01-01') FROM ev WHERE id > 2 ORDER BY date(at, 'start of month');
SELECT count(*) FROM ev WHERE date(at, 'start of month') = '2024-02-01';
";

#[test]
fn render_matches_pin_in_each_zone() {
    for (i, tz) in ZONES.iter().enumerate() {
        byte_identical(&format!("render{i}"), tz, RENDER);
    }
}

/// xorshift64 over the time-value forms, modifiers and formats.
fn corpus(seed: u64) -> String {
    let mut s = seed;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    const MODS: &[&str] = &[
        "'+1 day'",
        "'-36 hours'",
        "'+1.5 months'",
        "'-13 months'",
        "'+2 years'",
        "'start of month'",
        "'start of year'",
        "'end of month'",
        "'end of day'",
        "'weekday 0'",
        "'weekday -3'",
        "'localtime'",
        "'utc'",
        "'subsec'",
        "'floor'",
        "'ceiling'",
        "'+12:30'",
        "'-0001-02-03 04:05'",
        "'+90 minutes'",
        "'-0.5 seconds'",
    ];
    const FMTS: &[&str] = &[
        "'%Y-%m-%d %H:%M:%f'",
        "'%G-W%V-%u'",
        "'%j %U %W %w'",
        "'%s %J'",
        "'%I:%M %p %P %k %l'",
        "'%F %T %R %e %d'",
        "'[%%] %g %S'",
    ];
    let mut out = String::new();
    for _ in 0..400 {
        let r = next();
        let value = match r % 5 {
            0 => format!(
                "'{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}'",
                next() % 10000,
                next() % 12 + 1,
                next() % 31 + 1,
                next() % 24,
                next() % 60,
                next() % 60,
                next() % 1000
            ),
            1 => format!("{}", next() % 5_373_484),
            2 => format!(
                "{}, 'unixepoch'",
                (next() % 4_000_000_000) as i64 - 1_000_000_000
            ),
            3 => format!("'{:02}:{:02}'", next() % 25, next() % 60),
            _ => format!(
                "'{:04}-{:02}-{:02}'",
                1960 + next() % 100,
                next() % 12 + 1,
                next() % 31 + 1
            ),
        };
        let mut args = value;
        for _ in 0..next() % 4 {
            args.push_str(", ");
            args.push_str(MODS[(next() % MODS.len() as u64) as usize]);
        }
        let f = FMTS[(next() % FMTS.len() as u64) as usize];
        out.push_str(&format!(
            "SELECT datetime({args}), julianday({args}), strftime({f}, {args}), unixepoch({args});\n"
        ));
    }
    out
}

#[test]
fn generated_corpus_matches_pin_in_each_zone() {
    for (i, tz) in ZONES.iter().enumerate() {
        byte_identical(&format!("corpus{i}"), tz, &corpus(0x5eed_da7e + i as u64));
    }
}
