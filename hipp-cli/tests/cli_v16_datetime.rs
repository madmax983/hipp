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

/// The instants every zone test converts, both ways, after a
/// warm-up conversion: the pin's glibc re-parses a rule-less
/// daylight zone on every conversion (see `systime`), and the first
/// one of a process differs from the steady state hipp models.
fn zone_batch() -> String {
    const AT: &[&str] = &[
        "1970-01-01 03:00",
        "1972-03-01 12:00",
        "1985-04-20 12:00",
        "1985-10-27 05:30",
        "1999-12-31 23:00",
        "2004-02-29 12:00",
        "2023-01-02 01:00",
        "2023-03-12 07:30",
        "2023-11-05 05:30",
        "2024-01-01 00:30",
        "2024-04-07 15:00",
        "2024-07-15 12:00",
        "2024-10-06 16:00",
        "2024-12-31 23:59",
        "2037-12-15 12:00",
        "2050-06-01 12:00",
        "1900-06-01 12:00",
    ];
    let mut s = String::from("SELECT datetime(0, 'unixepoch', 'localtime');\n");
    for t in AT {
        s.push_str(&format!(
            "SELECT datetime('{t}', 'localtime'), datetime('{t}', 'utc');\n"
        ));
    }
    s
}

/// Both shells under `TZ` (and `TZDIR`, when given), byte-compared
/// after the warm-up line; `None` when the pin did not finish (its
/// libc may abort on a hostile zone file).
fn zone_agrees(tz: &str, tzdir: Option<&Path>, batch: &str) -> Option<bool> {
    let go = |bin: &Path| {
        let mut c = Command::new(bin);
        c.arg(":memory:").arg(batch).env("TZ", tz);
        if let Some(d) = tzdir {
            c.env("TZDIR", d);
        }
        c.output().expect("cli runs")
    };
    let want = go(&oracle());
    if !want.status.success() {
        return None;
    }
    let got = go(&hipp_bin());
    let skip = |b: &[u8]| -> Vec<u8> {
        let at = b
            .iter()
            .position(|c| *c == b'\n')
            .map_or(b.len(), |i| i + 1);
        b[at..].to_vec()
    };
    Some(got.status.success() && skip(&got.stdout) == skip(&want.stdout))
}

/// A POSIX rule month outside 1..=12 (kept from a failed parse)
/// makes glibc read outside its month table: not modelled.
fn month_out_of_table(tz: &str) -> bool {
    let b = tz.as_bytes();
    b.iter().enumerate().any(|(i, c)| {
        if *c != b'M' {
            return false;
        }
        let mut j = i + 1;
        while b.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
            j += 1;
        }
        if matches!(b.get(j), Some(b'+' | b'-')) {
            return true;
        }
        let start = j;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        j > start
            && std::str::from_utf8(&b[start..j])
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .is_none_or(|m| !(1..=12).contains(&m))
    })
}

/// POSIX TZ strings as glibc parses them (review regression: a
/// malformed or partial rule part made hipp fall back to UTC, where
/// glibc keeps the offsets and whatever the rule parser had filled
/// in), then mutations of well-formed strings (xorshift64).
#[test]
fn tz_strings_match_pin() {
    const FIXED: &[&str] = &[
        "EST5EDT,M3.2.0",
        "EST5EDT,M3.2.0,M11.1.0x",
        "EST5EDT,J0/2,J365",
        "EST5EDT,366,0",
        "NZST-12NZDT,M9.5.0,M4.1.0/",
        "EST5,M3.2.0,M11.1.0",
        "EST5x",
        "EST5:",
        "EST+-5",
        "EST5EDT 4,M3.2.0,M11.1.0",
        "EST5EDT,M3.2,M11.1.0",
        "EST5EDT,M3.2.0/x,M11.1.0",
        "EST5EDT,M3.2.0/-1:30,M11.1.0/26",
        "EST5EDT,J60/1:30,300/-2",
        "<+0330>-3:30<+0430>,J79/24,J263/24",
        "<A1>5<B2>4,M3.2.0,M11.1.0",
        "IST-5:30",
        "XST3XDT",
        "XST3XDT,",
        "XST3XDT2",
        "AAA3BBB",
        "Universal",
        ":America/New_York",
        "",
        ":",
    ];
    let batch = zone_batch();
    let mut bad = Vec::new();
    for tz in FIXED {
        if zone_agrees(tz, None, &batch) == Some(false) {
            bad.push(tz.to_string());
        }
    }
    const SEEDS: &[&str] = &[
        "EST5EDT,M3.2.0,M11.1.0",
        "NZST-12NZDT,M9.5.0,M4.1.0/3",
        "<+1030>-10:30<+11>-11,M10.1.0,M4.1.0",
        "CET-1CEST,M3.5.0,M10.5.0/3",
        "AAA3BBB,J60/1:30,300/-2",
        "PST8PDT,M3.2.0/2:00:00,M11.1.0/2:00:00",
        "<-03>3<-02>,M3.5.0/-2,M10.5.0/-1",
        "WART4WARST,J1/0,J365/25",
    ];
    const ALPHA: &[u8] = b"EST5DT,M.0123456789/:+-<>J x";
    let mut r = 0x7a5e_ed00_0000_0001u64;
    let mut next = move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    };
    let mut ran = 0;
    while ran < 150 {
        let mut s: Vec<u8> = SEEDS[(next() % SEEDS.len() as u64) as usize].into();
        for _ in 0..1 + next() % 3 {
            let at = (next() % (s.len() as u64 + 1)) as usize;
            let c = ALPHA[(next() % ALPHA.len() as u64) as usize];
            match next() % 4 {
                0 if at < s.len() => {
                    s.remove(at);
                }
                1 => s.insert(at, c),
                2 if at < s.len() => s[at] = c,
                _ => s.truncate(at),
            }
        }
        let tz = String::from_utf8(s).expect("ascii");
        if month_out_of_table(&tz) || tz.contains('/') && tz.starts_with('/') {
            continue;
        }
        ran += 1;
        if zone_agrees(&tz, None, &batch) == Some(false) {
            bad.push(tz);
        }
    }
    assert!(bad.is_empty(), "{} TZ strings differ: {bad:?}", bad.len());
}

/// A minimal TZif v2 file (both data blocks) with a footer.
fn tzif(
    trans: &[i64],
    idx: &[u8],
    types: &[(i32, u8, u8)],
    leaps: &[(i64, i32)],
    tail: &[u8],
) -> Vec<u8> {
    let names = b"AAA\0BBB\0";
    let block = |wide: bool| {
        let mut b = b"TZif2".to_vec();
        b.extend([0u8; 15]);
        for n in [0, 0, leaps.len(), trans.len(), types.len(), names.len()] {
            b.extend((n as i32).to_be_bytes());
        }
        for t in trans {
            if wide {
                b.extend(t.to_be_bytes());
            } else {
                b.extend((*t as i32).to_be_bytes());
            }
        }
        b.extend(idx);
        for (off, dst, abbr) in types {
            b.extend(off.to_be_bytes());
            b.extend([*dst, *abbr]);
        }
        b.extend(names);
        for (at, c) in leaps {
            if wide {
                b.extend(at.to_be_bytes());
            } else {
                b.extend((*at as i32).to_be_bytes());
            }
            b.extend(c.to_be_bytes());
        }
        b
    };
    let mut f = block(false);
    f.extend(block(true));
    f.extend(tail);
    f
}

/// TZif files as glibc reads them: crafted footers (missing,
/// empty, truncated, with a NUL), leap-second corrections, version
/// bytes, and (review regression: hipp accepted files glibc
/// rejects) truncations and byte corruptions of real zone files.
#[test]
fn tzif_files_match_pin() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hipp-cli-v16datetime-tzif");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let ny = [1_678_604_400, 1_699_164_000, 1_900_000_000];
    let types = [(-18000, 0, 0), (-14400, 1, 4)];
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (
            "full".into(),
            tzif(&ny, &[1, 0, 0], &types, &[], b"\nIST-5:30\n"),
        ),
        ("nofoot".into(), tzif(&ny, &[1, 0, 0], &types, &[], b"")),
        ("nl".into(), tzif(&ny, &[1, 0, 0], &types, &[], b"\n")),
        ("nlnl".into(), tzif(&ny, &[1, 0, 0], &types, &[], b"\n\n")),
        (
            "trunc".into(),
            tzif(&ny, &[1, 0, 0], &types, &[], b"\nIST-5:30"),
        ),
        (
            "nul".into(),
            tzif(&ny, &[1, 0, 0], &types, &[], b"\nIST-5\0:30\n"),
        ),
        (
            "nonl".into(),
            tzif(&ny, &[1, 0, 0], &types, &[], b"XIST-5:30\n"),
        ),
        (
            "barefoot".into(),
            tzif(&ny, &[1, 0, 0], &types, &[], b"\nCET-1CEST\n"),
        ),
        (
            "leap".into(),
            tzif(
                &ny,
                &[1, 0, 0],
                &types,
                &[(1_600_000_000, 1), (1_650_000_000, 2), (1_950_000_000, 3)],
                b"\nEST5EDT,M3.2.0,M11.1.0\n",
            ),
        ),
        (
            "badidx".into(),
            tzif(&ny, &[1, 2, 0], &types, &[], b"\nUTC0\n"),
        ),
        (
            "baddst".into(),
            tzif(
                &ny,
                &[1, 0, 0],
                &[(-18000, 0, 0), (-14400, 2, 4)],
                &[],
                b"\nUTC0\n",
            ),
        ),
        (
            "badabbr".into(),
            tzif(
                &ny,
                &[1, 0, 0],
                &[(-18000, 0, 0), (-14400, 1, 9)],
                &[],
                b"\nUTC0\n",
            ),
        ),
    ];
    let mut v1 = files[0].1.clone();
    v1[4] = b'1';
    files.push(("ver1".into(), v1));
    let mut r = 0x7a1f_0000_0000_0001u64;
    let mut next = move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    };
    for (k, z) in [
        "America/New_York",
        "Europe/Berlin",
        "Australia/Lord_Howe",
        "Asia/Kolkata",
        "America/Sao_Paulo",
        "Pacific/Chatham",
    ]
    .iter()
    .cycle()
    .take(150)
    .enumerate()
    {
        let Ok(mut b) = std::fs::read(Path::new("/usr/share/zoneinfo").join(z)) else {
            continue;
        };
        if next() % 4 == 0 {
            b.truncate((next() % (b.len() as u64 + 1)) as usize);
        } else {
            for _ in 0..1 + next() % 3 {
                let at = (next() % b.len() as u64) as usize;
                b[at] = if next() % 2 == 0 {
                    next() as u8
                } else {
                    b[at] ^ (1 << (next() % 8))
                };
            }
        }
        files.push((format!("fz{k}"), b));
    }
    let batch = zone_batch();
    let mut bad = Vec::new();
    for (name, bytes) in &files {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write");
        let tz = p.to_str().expect("utf-8 path");
        if zone_agrees(tz, None, &batch) == Some(false) {
            bad.push(name.clone());
        }
    }
    assert!(bad.is_empty(), "{} zone files differ: {bad:?}", bad.len());
}

/// A daylight name without rules takes `posixrules` (from `TZDIR`),
/// its transitions shifted to the string's offsets as glibc shifts
/// them.
#[test]
fn rule_less_zones_use_posixrules() {
    let batch = zone_batch();
    let mut bad = Vec::new();
    for z in ["America/Chicago", "Europe/Berlin", "Australia/Sydney"] {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "hipp-cli-v16datetime-posixrules-{}",
            z.replace('/', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let Ok(b) = std::fs::read(Path::new("/usr/share/zoneinfo").join(z)) else {
            continue;
        };
        std::fs::write(dir.join("posixrules"), b).expect("write");
        for tz in [
            "XST5XDT",
            "XST3XDT",
            "XST6XDT",
            "XST-1XDT",
            "XST-10XDT,",
            "XST5XDT3",
        ] {
            if zone_agrees(tz, Some(&dir), &batch) == Some(false) {
                bad.push(format!("{z}: {tz}"));
            }
        }
    }
    for tz in ["XST3XDT", "AAA3BBB", "XST5XDT6"] {
        if zone_agrees(tz, None, &batch) == Some(false) {
            bad.push(tz.to_string());
        }
    }
    assert!(bad.is_empty(), "{bad:?}");
}
