//! v0.16 UTF-16 databases (leaf feature `utf16`), differentially vs
//! the pinned oracle (SQLite 3.54.1). The semantics were probed on the
//! pin before implementation (STATUS.md, "UTF-16 databases"); the
//! matrix the tests encode:
//!
//! - `PRAGMA encoding = 'UTF-16' | 'UTF-16le' | 'UTF-16be' | ...` on
//!   an empty database (`UTF-16` alone is little-endian, the pin's
//!   native order), reported as `UTF-16le` / `UTF-16be`; a silent
//!   no-op once the database has a schema or a TEMP database, an
//!   unknown name an error only while the encoding is still open; a
//!   file whose header field is still 0 may still choose.
//! - Text stored in the file's UTF-16 (serial types count UTF-16
//!   bytes; sqlite_schema's text too), read back as the pin's UTF-8;
//!   the header field (offset 56) as the pin writes it.
//! - The BINARY collation over the UTF-16 bytes (memcmp): ORDER BY,
//!   indexes (scans, ranges, lookups, UNIQUE), min / max, GROUP BY,
//!   compounds, IN, BETWEEN, windows; NOCASE and RTRIM over the
//!   UTF-8 form, as on the pin.
//! - The text functions as on the pin: hex() and CAST(... AS BLOB)
//!   give UTF-16 bytes, octet_length counts them, a blob read as text
//!   (CAST, ||, upper, LIKE, numbers from blobs) decodes from UTF-16,
//!   function results go through the pin's conversion.
//! - Invalid UTF-16 kept byte for byte (odd bytes dropped): an
//!   unpaired surrogate stays as stored through copies, index builds,
//!   `||`, CAST, equality and BINARY order, and is shown as the pin
//!   shows it (a surrogate followed by any unit combined with it).
//! - VACUUM keeps the encoding, files interoperate both ways (the
//!   pin's integrity_check on hipp's files and hipp's on the pin's,
//!   overflow pages and indexes included), and a generated corpus
//!   (xorshift PRNG) of random Unicode text — BMP, astral, combining,
//!   NUL-containing — in both byte orders against the live pin.

mod common;

use hipp::storage::{FileError, block_on};
use hipp::{Config, Connection, Error, FileStorage, Step, Value};
use std::path::{Path, PathBuf};

type HErr = Error<FileError>;

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> Vec<u8> {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    t.as_slice().to_vec()
}

/// The shell's byte rendering: control bytes but tab and newline in
/// caret notation; text up to its first NUL.
fn shell_bytes(b: &[u8], text: bool) -> Vec<u8> {
    let b = match (text, b.iter().position(|c| *c == 0)) {
        (true, Some(i)) => &b[..i],
        _ => b,
    };
    let mut out = Vec::with_capacity(b.len());
    for &c in b {
        if c < 0x20 && c != b'\t' && c != b'\n' {
            out.push(b'^');
            out.push(c + 0x40);
        } else {
            out.push(c);
        }
    }
    out
}

/// One statement's rows in the CLI's list rendering, or its error.
async fn collect(db: &mut Connection<FileStorage>, sql: &str) -> Result<Vec<u8>, HErr> {
    let mut q = db.prepare(sql).await?;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    while q.step().await? == Step::Row {
        if !first {
            out.push(b'\n');
        }
        first = false;
        for i in 0..q.column_count() {
            if i > 0 {
                out.push(b'|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => out.extend_from_slice(v.to_string().as_bytes()),
                Some(Value::Real(f)) => out.extend_from_slice(&fmt_real(f)),
                Some(Value::Text(b)) => out.extend(shell_bytes(b, true)),
                Some(Value::Blob(b)) => out.extend(shell_bytes(b, false)),
            }
        }
    }
    Ok(out)
}

fn fresh(tag: &str) -> PathBuf {
    common::temp_db(&format!("v16u16-{tag}"))
}

/// The statements through hipp on the file at `path`.
fn hipp_file(path: &Path, stmts: &[String]) -> Vec<Result<Vec<u8>, HErr>> {
    block_on(async {
        let fs = FileStorage::open(path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut out = Vec::new();
        for s in stmts {
            out.push(collect(&mut db, s).await);
        }
        out
    })
}

/// The statements through the oracle CLI on the file at `path`: per
/// statement, its output (`Some`), or None when the pin reported an
/// error for it.
fn oracle_file(path: &Path, stmts: &[String]) -> Vec<Option<Vec<u8>>> {
    let mut script = String::new();
    for (i, s) in stmts.iter().enumerate() {
        assert!(!s.contains('\n'), "statements are one line each: {s}");
        script.push_str(&format!("SELECT '#@#{i}';\n{s};\n"));
    }
    let spath = path.with_extension("sql");
    std::fs::write(&spath, script.as_bytes()).expect("write script");
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::fs::File::open(&spath).expect("script"))
        .output()
        .expect("oracle runs");
    let _ = std::fs::remove_file(&spath);
    let mut res: Vec<Option<Vec<u8>>> = vec![Some(Vec::new()); stmts.len()];
    let o = &out.stdout;
    let mut marks: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0usize;
    while at < o.len() {
        if o[at..].starts_with(b"#@#") && (at == 0 || o[at - 1] == b'\n') {
            let end = o[at..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(o.len(), |k| at + k);
            if let Some(k) = std::str::from_utf8(&o[at + 3..end])
                .ok()
                .and_then(|t| t.parse::<usize>().ok())
            {
                marks.push((k, at, (end + 1).min(o.len())));
            }
            at = end;
        }
        at += 1;
    }
    for (m, &(k, _, body)) in marks.iter().enumerate() {
        let stop = marks.get(m + 1).map_or(o.len(), |n| n.1);
        let mut chunk = o[body.min(stop)..stop].to_vec();
        if chunk.last() == Some(&b'\n') {
            chunk.pop();
        }
        if k < res.len() {
            res[k] = Some(chunk);
        }
    }
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        let Some(at) = line.find("near line ") else {
            continue;
        };
        let n: usize = line[at + 10..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if n.is_multiple_of(2) && n / 2 >= 1 && n / 2 <= stmts.len() {
            res[n / 2 - 1] = None;
        }
    }
    res
}

/// Every statement agrees with the pin, each engine on its own fresh
/// file: equal output, or both fail; then the pin's integrity_check
/// passes on the file hipp wrote, and the two files' text encodings
/// (header offset 56) are equal. Returns how many statements failed
/// on both, and the hipp file.
fn agree(tag: &str, stmts: &[String]) -> (usize, PathBuf) {
    let hp = fresh(&format!("{tag}-h"));
    let op = fresh(&format!("{tag}-o"));
    let got = hipp_file(&hp, stmts);
    let want = oracle_file(&op, stmts);
    let mut bad = Vec::new();
    let mut both_err = 0;
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        let ok = match (g, w) {
            (Ok(a), Some(b)) => a == b,
            (Err(_), None) => {
                both_err += 1;
                true
            }
            _ => false,
        };
        if !ok {
            bad.push(format!(
                "{}\n  hipp:   {:?}\n  oracle: {:?}",
                stmts[i],
                g.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
                w.as_ref().map(|b| String::from_utf8_lossy(b).into_owned())
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{tag}: {} of {} differ from the pin:\n{}",
        bad.len(),
        stmts.len(),
        bad.iter().take(10).cloned().collect::<Vec<_>>().join("\n"),
    );
    let ic = common::cli(&hp, "PRAGMA integrity_check;");
    assert_eq!(
        ic.trim(),
        "ok",
        "{tag}: the pin's integrity_check on hipp's file"
    );
    let (h, o) = (std::fs::read(&hp), std::fs::read(&op));
    if let (Ok(h), Ok(o)) = (h, o)
        && h.len() >= 100
        && o.len() >= 100
    {
        assert_eq!(h[56..60], o[56..60], "{tag}: header text encoding");
    }
    (both_err, hp)
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(|l| l.trim_end_matches(';').to_string())
        .collect()
}

/// The pin's integrity_check and hipp's agree that `p` is sound.
fn both_ok(p: &Path) {
    assert_eq!(
        common::cli(p, "PRAGMA integrity_check;").trim(),
        "ok",
        "pin check"
    );
    let got = hipp_file(p, &["PRAGMA integrity_check".to_string()]);
    assert_eq!(
        got[0].as_ref().map(|b| b.clone()).ok(),
        Some(b"ok".to_vec()),
        "hipp check"
    );
}

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
}

fn corpus_size(default: usize) -> usize {
    std::env::var("HIPP_UTF16_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

const ORDERS: [&str; 2] = ["UTF-16le", "UTF-16be"];

/// Characters whose UTF-8, UTF-16le and UTF-16be orders disagree:
/// ASCII, Latin-1, U+0100, CJK, the private-use start U+E000, U+FFFD,
/// U+FFFE, astral (U+10000, U+1F600, U+10FFFF), combining marks.
const ALPHABET: [&str; 18] = [
    "a",
    "B",
    "z",
    " ",
    "\u{e9}",
    "\u{df}",
    "\u{100}",
    "\u{1ff}",
    "\u{4e2d}",
    "\u{e000}",
    "\u{fb01}",
    "\u{fffd}",
    "\u{fffe}",
    "\u{10000}",
    "\u{1f600}",
    "\u{10ffff}",
    "e\u{301}",
    "\u{d7ff}",
];

/// A random text value as an SQL expression: a quoted literal of
/// alphabet characters, sometimes with a NUL spliced in (`char(0)`)
/// or a random code point (`char(N)`, surrogates included).
fn gen_text(r: &mut Rng) -> String {
    let n = r.below(5) as usize;
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(ALPHABET[r.below(ALPHABET.len() as u64) as usize]);
    }
    let lit = format!("'{}'", s.replace('\'', "''"));
    match r.below(12) {
        0 => format!("{lit} || char(0) || 'x'"),
        1 => format!("{lit} || char({})", r.below(0x11_0000)),
        2 => format!("char({}, {})", r.below(0x3_0000), r.below(0x1_0000)),
        3 | 4 => format!("{lit} || {}", raw_units(r)),
        _ => lit,
    }
}

/// Raw UTF-16 as text (`CAST(x'...' AS TEXT)`), surrogate-heavy: lone
/// and reversed surrogates, pairs, and the units around them, read in
/// either byte order.
fn raw_units(r: &mut Rng) -> String {
    const UNITS: [u16; 10] = [
        0xD800, 0xDBFF, 0xDC00, 0xDFFF, 0xD83D, 0xDE00, 0x0061, 0xE000, 0x00E9, 0x6100,
    ];
    let mut h = String::new();
    for _ in 0..(1 + r.below(4)) {
        h.push_str(&format!(
            "{:04X}",
            UNITS[r.below(UNITS.len() as u64) as usize]
        ));
    }
    format!("CAST(x'{h}' AS TEXT)")
}

// ------------------------------------------------------------ PRAGMA

#[test]
fn pragma_encoding_names_and_reporting() {
    for (i, name) in [
        "UTF-16", "utf16", "UTF-16le", "utf16LE", "UTF-16be", "UTF16BE", "UTF-8", "utf8",
    ]
    .iter()
    .enumerate()
    {
        let stmts = lines(&format!(
            "PRAGMA encoding;
             PRAGMA encoding = '{name}';
             PRAGMA encoding;
             SELECT * FROM pragma_encoding;
             CREATE TABLE t(a TEXT, b);
             PRAGMA encoding;
             PRAGMA encoding = 'UTF-8';
             PRAGMA encoding = 'UTF-16be';
             PRAGMA encoding = 'nonsense';
             PRAGMA encoding;
             INSERT INTO t VALUES ('h\u{e9}llo \u{1f600}', 1);
             SELECT a, hex(a), length(a), octet_length(a), typeof(a) FROM t;
             SELECT sql, hex(name) FROM sqlite_schema"
        ));
        agree(&format!("names{i}"), &stmts);
    }
}

#[test]
fn pragma_encoding_when_fixed() {
    // The encoding stays open until the schema exists: reads, a
    // BEGIN, a page-size change and repeated choices do not fix it;
    // the last choice wins.
    for (k, script) in [
        "SELECT 1; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "SELECT count(*) FROM sqlite_schema; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "PRAGMA encoding; PRAGMA encoding = 'UTF-16be'; CREATE TABLE t(a); PRAGMA encoding",
        "PRAGMA page_size = 1024; PRAGMA encoding = 'UTF-16be'; CREATE TABLE t(a); PRAGMA encoding; PRAGMA page_size",
        "PRAGMA encoding = 'UTF-16be'; PRAGMA encoding = 'UTF-16le'; PRAGMA encoding = 'UTF-8'; PRAGMA encoding = 'UTF-16'; CREATE TABLE t(a); PRAGMA encoding",
        "BEGIN; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding; CREATE TABLE t(a); COMMIT; PRAGMA encoding",
        // A TEMP database fixes it; one created after the choice
        // stores text as main does.
        "CREATE TEMP TABLE x(a); PRAGMA encoding = 'UTF-16be'; PRAGMA encoding; CREATE TABLE t(a); PRAGMA encoding",
        "PRAGMA encoding = 'UTF-16be'; CREATE TEMP TABLE x(a); INSERT INTO x VALUES ('\u{1f600}'), ('\u{e000}'); SELECT a, hex(a) FROM x ORDER BY a; PRAGMA encoding; CREATE TABLE t(a); PRAGMA encoding",
        // Once fixed, any name is ignored, even an unknown one.
        "CREATE TABLE t(a); PRAGMA encoding = 'bogus'; PRAGMA encoding; PRAGMA encoding = 'UTF-16'; PRAGMA encoding",
        "PRAGMA encoding = 'UTF-16le'; CREATE TABLE t(a); DROP TABLE t; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
    ]
    .iter()
    .enumerate()
    {
        let stmts: Vec<String> = script.split("; ").map(str::to_string).collect();
        agree(&format!("fixed{k}"), &stmts);
    }
    // An unknown name while the encoding is open is an error there.
    let (errs, _) = agree(
        "unknown",
        &[
            "PRAGMA encoding = 'bogus'",
            "PRAGMA encoding = 'UTF-17'",
            "PRAGMA encoding",
        ]
        .map(str::to_string),
    );
    assert_eq!(errs, 2);
}

/// A file whose encoding field is still 0 (the pin's file after only a
/// header write) may still choose, in a later connection.
#[test]
fn header_field_unset_reopened() {
    let p = fresh("unset");
    common::cli(&p, "PRAGMA user_version = 3;");
    let d = std::fs::read(&p).expect("read");
    assert_eq!(&d[56..60], &[0, 0, 0, 0], "the pin leaves the field 0");
    let stmts = lines(
        "PRAGMA encoding = 'UTF-16be';
         PRAGMA encoding;
         CREATE TABLE t(a);
         INSERT INTO t VALUES ('\u{1f600}');
         PRAGMA encoding;
         PRAGMA user_version",
    );
    let got = hipp_file(&p, &stmts);
    let rows: Vec<Vec<u8>> = got.into_iter().map(|r| r.expect("ok")).collect();
    assert_eq!(rows[1], b"UTF-16be");
    assert_eq!(rows[4], b"UTF-16be");
    assert_eq!(rows[5], b"3");
    let d = std::fs::read(&p).expect("read");
    assert_eq!(&d[56..60], &[0, 0, 0, 3]);
    assert_eq!(
        common::cli(&p, "PRAGMA encoding; SELECT hex(a) FROM t;"),
        "UTF-16be\nD83DDE00\n"
    );
    both_ok(&p);
}

// ---------------------------------------------------------- ordering

/// The BINARY order of every query shape over the bytes the pin
/// compares, in both byte orders, with and without an index.
#[test]
fn binary_order_everywhere() {
    for (k, order) in ORDERS.iter().enumerate() {
        let mut vals = String::new();
        for (i, c) in ALPHABET.iter().enumerate() {
            if i > 0 {
                vals.push_str(", ");
            }
            vals.push_str(&format!("('{c}', '{c}', '{c}', {i})"));
        }
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE t(x, y COLLATE NOCASE, z COLLATE RTRIM, n);
             INSERT INTO t VALUES {vals};
             INSERT INTO t VALUES ('a ', 'A', 'a  ', 100), ('', '', '', 101), ('ab', 'AB', 'ab', 102);
             SELECT x, hex(x) FROM t ORDER BY x, n;
             SELECT x FROM t ORDER BY x DESC, n;
             SELECT y FROM t ORDER BY y, n;
             SELECT hex(z) FROM t ORDER BY z, n;
             SELECT x FROM t ORDER BY x COLLATE NOCASE, n;
             SELECT x FROM t ORDER BY x COLLATE RTRIM DESC, n;
             SELECT min(x), max(x), min(y), max(y), min(z), max(z) FROM t;
             SELECT min(x, '\u{e000}'), max(x, '\u{1f600}') FROM t ORDER BY n;
             SELECT x, count(*) FROM t GROUP BY x;
             SELECT x FROM t UNION SELECT y FROM t;
             SELECT x FROM t INTERSECT SELECT z FROM t;
             SELECT x FROM t EXCEPT SELECT '\u{e000}';
             SELECT n FROM t WHERE x > '\u{e000}' ORDER BY n;
             SELECT n FROM t WHERE x < '\u{1f600}' ORDER BY n;
             SELECT n FROM t WHERE x BETWEEN 'a' AND '\u{fffd}' ORDER BY n;
             SELECT n FROM t WHERE x IN ('\u{1f600}', '\u{e000}', 'a') ORDER BY n;
             SELECT x < y, x > '\u{100}', '\u{1f600}' < '\u{e000}' FROM t ORDER BY n;
             SELECT group_concat(x, ',') OVER (ORDER BY x, n) FROM t ORDER BY n;
             SELECT x, row_number() OVER (PARTITION BY y ORDER BY x DESC) FROM t ORDER BY n;
             SELECT group_concat(x ORDER BY x) FROM t;
             CREATE INDEX ix ON t(x);
             CREATE INDEX iy ON t(y);
             CREATE INDEX iz ON t(z DESC);
             SELECT x FROM t WHERE x > '' ORDER BY x;
             SELECT x FROM t WHERE x >= 'a' AND x < '\u{e000}';
             SELECT x FROM t WHERE x < '\u{100}';
             SELECT x FROM t WHERE x IN ('\u{1f600}', '\u{e000}', 'a', '\u{10000}');
             SELECT n FROM t WHERE x = '\u{1f600}';
             SELECT y FROM t WHERE y > 'a';
             SELECT z FROM t WHERE z > 'a';
             SELECT min(x), max(x) FROM t;
             SELECT x FROM t ORDER BY x DESC LIMIT 5;
             SELECT DISTINCT x FROM t ORDER BY 1;
             CREATE UNIQUE INDEX ux ON t(n, x);
             INSERT INTO t VALUES ('\u{1f600}', 'q', 'q', 0);
             SELECT count(*) FROM t;
             PRAGMA integrity_check"
        ));
        agree(&format!("order{k}"), &stmts);
    }
}

// --------------------------------------------------------- functions

#[test]
fn text_functions() {
    for (k, order) in ORDERS.iter().enumerate() {
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE t(x);
             INSERT INTO t VALUES ('abc'), ('\u{c4}BC'), ('na\u{ef}ve'), ('x\u{1f600}y'), (''), ('e\u{301}'), (' \u{3000}pad  '), ('\u{10ffff}\u{e000}');
             SELECT x, length(x), octet_length(x), hex(x), upper(x), lower(x), substr(x, 2, 2), substr(x, -1), instr(x, 'b'), instr(x, '\u{1f600}') FROM t ORDER BY rowid;
             SELECT replace(x, 'b', '\u{df}'), trim(x), ltrim(x, ' a'), rtrim(x, 'y'), quote(x), unicode(x), char(unicode(x)), printf('%5s|%-4.2s|', x, x), x || '\u{e9}' FROM t ORDER BY rowid;
             SELECT x LIKE 'a%', x LIKE '%\u{1f600}%', x GLOB '*\u{1f600}*', x GLOB '[a-z]*', like('%E%', x) FROM t ORDER BY rowid;
             SELECT hex(CAST(x AS BLOB)), length(CAST(x AS BLOB)), typeof(CAST(x AS BLOB)), hex(x || x), concat(x, 1, 2.5), concat_ws('-', x, x) FROM t ORDER BY rowid;
             SELECT json_quote(x), json_array(x, length(x)), json_extract(json_object('k', x), '$.k') FROM t ORDER BY rowid;
             SELECT group_concat(x, '\u{b7}'), string_agg(hex(x), ','), count(DISTINCT x) FROM t;
             SELECT hex(12), hex(1.5), hex(CAST(12 AS BLOB)), hex(CAST(1.5 AS BLOB)), octet_length(12), octet_length(-1.5), octet_length(NULL), octet_length(x'00');
             SELECT hex(char(65535)), hex(char(65533)), hex(char(55296)), hex(char(1114111)), unicode(char(65534)), unicode(char(55296))"
        ));
        agree(&format!("funcs{k}"), &stmts);
    }
}

/// A blob read as text (or as a number through text) decodes from
/// the database's UTF-16.
#[test]
fn blobs_read_as_text() {
    for (k, (order, a, one)) in [
        ("UTF-16le", "61006200", "3100"),
        ("UTF-16be", "00610062", "0031"),
    ]
    .iter()
    .enumerate()
    {
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             SELECT CAST(x'{a}' AS TEXT), hex(CAST(x'{a}' AS TEXT)), length(CAST(x'{a}' AS TEXT)), CAST(x'{one}' AS INTEGER), CAST(x'{one}' AS REAL), CAST(x'{one}' AS NUMERIC);
             SELECT x'{one}' + 1, x'{one}' * 2, typeof(x'{one}' + 0), x'{a}' || 'c', hex(x'{a}' || ''), length(x'{a}'), octet_length(x'{a}');
             SELECT upper(x'{a}'), lower(x'{a}'), trim(x'{a}'), replace(x'{a}', 'a', 'z'), instr(x'{a}', 'b'), instr('ab', x'{a}'), substr(x'{a}', 2), typeof(substr(x'{a}', 2));
             SELECT 'ab' LIKE x'{a}', x'{a}' = 'ab', quote(x'{a}'), printf('%s', x'{a}'), unicode(x'{a}'), abs(x'{one}'), round(x'{one}');
             SELECT CASE WHEN x'{one}' THEN 'T' ELSE 'F' END, zeroblob(x'{one}') = x'00', length(randomblob(x'{one}'));
             SELECT CAST(x'41' AS TEXT), length(CAST(x'410042' AS TEXT)), hex(CAST(x'410042' AS TEXT));
             SELECT group_concat(x'{a}'), hex(string_agg(x'{a}', x'{one}'))"
        ));
        agree(&format!("blobs{k}"), &stmts);
    }
}

/// Invalid UTF-16 in stored data reads as on the pin, and a
/// surrogate followed by a unit that does not pair with it (`g`)
/// keeps its stored bytes through copies, updates and an index.
#[test]
fn invalid_utf16_reads_as_the_pin() {
    for (k, (order, bad, mix)) in [
        (
            "UTF-16le",
            [
                "00D8",
                "00DC",
                "FFDB",
                "4100",
                "410042",
                "6100000062",
                "00D800DC",
            ],
            "00D86100",
        ),
        (
            "UTF-16be",
            [
                "D800",
                "DC00",
                "DBFF",
                "0041",
                "004100",
                "0061000000",
                "D800DC00",
            ],
            "DC000061",
        ),
    ]
    .iter()
    .enumerate()
    {
        let mut ins = String::new();
        for (i, b) in bad.iter().enumerate() {
            if i > 0 {
                ins.push_str(", ");
            }
            ins.push_str(&format!("(CAST(x'{b}' AS TEXT))"));
        }
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE t(x);
             INSERT INTO t VALUES {ins};
             SELECT x FROM t ORDER BY rowid;
             SELECT hex(x), length(x), octet_length(x), typeof(x), unicode(x), hex(CAST(x AS BLOB)) FROM t ORDER BY rowid;
             SELECT hex(upper(x)), hex(substr(x, 1)), hex(x || ''), hex(coalesce(x, 1)), hex(quote(x)), hex(trim(x)), hex(lower(x)) FROM t ORDER BY rowid;
             SELECT hex(group_concat(x)) FROM t;
             CREATE TABLE u AS SELECT x FROM t;
             SELECT hex(x) FROM u ORDER BY rowid;
             SELECT hex(x) FROM t ORDER BY x;
             CREATE TABLE g(x);
             INSERT INTO g VALUES (CAST(x'{mix}' AS TEXT));
             SELECT x, length(x), unicode(x), length(x || 'z') FROM g;
             SELECT hex(x), hex(x || 'z'), hex(CAST(x AS BLOB)), x || 'z' FROM g;
             INSERT INTO g SELECT x FROM g;
             UPDATE g SET x = x WHERE rowid = 1;
             CREATE INDEX gi ON g(x);
             SELECT rowid, hex(x) FROM g WHERE x = CAST(x'{mix}' AS TEXT);
             SELECT count(*) FROM g WHERE x = char(unicode(x));
             PRAGMA integrity_check"
        ));
        agree(&format!("invalid{k}"), &stmts);
    }
}

// ------------------------------------------------------ maintenance

#[test]
fn vacuum_alter_temp_and_without_rowid() {
    for (k, order) in ORDERS.iter().enumerate() {
        let big = "\u{1f600}\u{e9}".repeat(700);
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE \"t\u{e9}\"(x TEXT, y UNIQUE);
             CREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID;
             INSERT INTO \"t\u{e9}\" VALUES ('{big}', 'a'), ('\u{e000}', '\u{1f600}'), ('z', '\u{e000}');
             INSERT INTO w VALUES ('\u{1f600}', 1), ('\u{e000}', 2), ('a', 3), ('\u{100}', 4), ('{big}', 5);
             SELECT length(x), octet_length(x), y FROM \"t\u{e9}\" ORDER BY y;
             SELECT k = '{big}', v FROM w;
             UPDATE w SET k = k || '!' WHERE v < 3;
             SELECT hex(k), v FROM w WHERE length(k) < 10;
             DELETE FROM w WHERE k = '\u{100}';
             SELECT v FROM w;
             VACUUM;
             PRAGMA encoding;
             SELECT y FROM \"t\u{e9}\" ORDER BY y;
             SELECT v FROM w;
             ALTER TABLE \"t\u{e9}\" RENAME TO \"\u{1f600}\";
             ALTER TABLE \"\u{1f600}\" ADD COLUMN \"c\u{e9}\" DEFAULT '\u{10ffff}';
             SELECT \"c\u{e9}\", hex(\"c\u{e9}\") FROM \"\u{1f600}\" ORDER BY y;
             SELECT name, sql FROM sqlite_schema ORDER BY name;
             CREATE TEMP TABLE tt(a);
             INSERT INTO tt VALUES ('\u{1f600}'), ('\u{e000}'), ('a');
             SELECT a, hex(a) FROM tt ORDER BY a;
             CREATE INDEX temp.ti ON tt(a);
             SELECT a FROM tt WHERE a > 'a';
             PRAGMA integrity_check"
        ));
        let (_, hp) = agree(&format!("maint{k}"), &stmts);
        both_ok(&hp);
    }
}

// ---------------------------------------------------- file interop

/// Each engine reads and extends the other's UTF-16 files: overflow
/// pages, indexes, both byte orders; both integrity checks pass.
#[test]
fn files_interoperate() {
    for (k, order) in ORDERS.iter().enumerate() {
        let mut r = Rng(0x5eed_0000_u64 + k as u64 * 7919);
        let mut rows = Vec::new();
        for i in 0..120 {
            let mut s = String::new();
            let n = if i % 17 == 0 {
                600
            } else {
                r.below(12) as usize
            };
            for _ in 0..n {
                s.push_str(ALPHABET[r.below(ALPHABET.len() as u64) as usize]);
            }
            rows.push(format!("({i}, '{s}')"));
        }
        let setup = format!(
            "PRAGMA encoding = '{order}'; CREATE TABLE t(n INTEGER PRIMARY KEY, x TEXT); CREATE INDEX ix ON t(x); INSERT INTO t VALUES {};",
            rows.join(", ")
        );
        let q = "SELECT n, hex(x) FROM t ORDER BY x, n";
        // hipp writes, the pin reads.
        let hp = fresh(&format!("io{k}-h"));
        let mut stmts = lines(&setup.replace("; ", ";\n"));
        stmts.push(q.to_string());
        let got = hipp_file(&hp, &stmts);
        assert!(got.iter().all(|g| g.is_ok()), "{order}: hipp wrote");
        let hipp_rows = got
            .last()
            .and_then(|g| g.as_ref().ok())
            .cloned()
            .expect("rows");
        both_ok(&hp);
        let pin_rows = common::cli(&hp, &format!("{q};"));
        assert_eq!(
            pin_rows.trim_end().as_bytes(),
            hipp_rows.as_slice(),
            "{order}: the pin reads hipp's file"
        );
        // The pin writes, hipp reads and extends, the pin checks.
        let op = fresh(&format!("io{k}-o"));
        common::cli(&op, &setup);
        let more = lines(
            "SELECT n, hex(x) FROM t ORDER BY x, n;
             INSERT INTO t SELECT n + 1000, x || '\u{1f600}' FROM t WHERE n % 3 = 0;
             UPDATE t SET x = '\u{e000}' || x WHERE n % 5 = 0;
             DELETE FROM t WHERE x < 'a';
             SELECT count(*) FROM t;
             PRAGMA integrity_check",
        );
        let got = hipp_file(&op, &more);
        assert_eq!(
            got[0].as_ref().ok().map(|b| b.as_slice()),
            Some(pin_rows.trim_end().as_bytes()),
            "{order}: hipp reads the pin's file"
        );
        assert!(got.iter().all(|g| g.is_ok()), "{order}: hipp extended");
        both_ok(&op);
        let after = common::cli(&op, &format!("{q};"));
        let mine = hipp_file(&op, &[q.to_string()]);
        assert_eq!(
            mine[0].as_ref().ok().map(|b| b.as_slice()),
            Some(after.trim_end().as_bytes()),
            "{order}: after hipp's writes"
        );
    }
}

// -------------------------------------------------- generated corpus

/// Random Unicode text in both byte orders: random writes through
/// both engines on their own files, then every query shape compared,
/// and the pin's integrity_check on hipp's file.
#[test]
fn generated_corpus() {
    let n = corpus_size(24);
    for case in 0..n {
        let order = ORDERS[case % 2];
        let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ (case as u64 + 1).wrapping_mul(0x100_0193));
        let mut stmts = vec![
            format!("PRAGMA encoding = '{order}'"),
            "CREATE TABLE t(x TEXT, y TEXT COLLATE NOCASE, z COLLATE RTRIM, n INTEGER)".to_string(),
        ];
        if r.below(2) == 0 {
            stmts.push("CREATE INDEX ix ON t(x)".to_string());
        }
        if r.below(2) == 0 {
            stmts.push("CREATE INDEX iyz ON t(y, z DESC)".to_string());
        }
        for i in 0..(20 + r.below(30)) {
            let (a, b, c) = (gen_text(&mut r), gen_text(&mut r), gen_text(&mut r));
            stmts.push(format!("INSERT INTO t VALUES ({a}, {b}, {c}, {i})"));
        }
        for _ in 0..4 {
            let v = gen_text(&mut r);
            stmts.push(match r.below(4) {
                0 => format!("UPDATE t SET x = x || {v} WHERE n % 3 = {}", r.below(3)),
                1 => format!("DELETE FROM t WHERE x < {v}"),
                2 => format!("UPDATE t SET y = {v} WHERE x > {}", gen_text(&mut r)),
                _ => format!("INSERT INTO t SELECT z, x, y, n + 100 FROM t WHERE y >= {v}"),
            });
        }
        let probe = gen_text(&mut r);
        for q in [
            "SELECT n, hex(x), length(x), octet_length(x) FROM t ORDER BY x, n".to_string(),
            "SELECT x FROM t ORDER BY x DESC, n".to_string(),
            "SELECT n FROM t ORDER BY y, z, n".to_string(),
            "SELECT n FROM t ORDER BY z DESC, n".to_string(),
            "SELECT min(x), max(x), min(y), max(z), count(DISTINCT x) FROM t".to_string(),
            "SELECT hex(x), count(*) FROM t GROUP BY x".to_string(),
            "SELECT x FROM t UNION SELECT z FROM t".to_string(),
            format!("SELECT n FROM t WHERE x > {probe} ORDER BY n"),
            format!("SELECT x FROM t WHERE x <= {probe}"),
            format!("SELECT n FROM t WHERE y = {probe} ORDER BY n"),
            "SELECT hex(upper(x)), hex(lower(y)), hex(substr(x, 2, 2)), instr(x, 'a'), hex(replace(x, 'a', '\u{e9}')), unicode(z) FROM t ORDER BY n".to_string(),
            "SELECT hex(trim(z)), quote(y), x LIKE '%\u{e9}%', z GLOB '*\u{1f600}*', hex(x || y) FROM t ORDER BY n".to_string(),
            "SELECT n, rank() OVER (ORDER BY x) FROM t ORDER BY n".to_string(),
            "SELECT hex(group_concat(x ORDER BY x DESC)) FROM t".to_string(),
            "PRAGMA integrity_check".to_string(),
        ] {
            stmts.push(q);
        }
        agree(&format!("gen{case}"), &stmts);
    }
}

// ------------------------------------------------ review regressions

/// A pin file whose text holds unpaired surrogates (stored through
/// `CAST(blob AS TEXT)`): hipp's index builds write the table rows'
/// exact bytes, its lookups find the pin's entries, and both
/// integrity checks pass (it rebuilt them from re-encoded text).
#[test]
fn regression_unpaired_surrogates_in_pin_files() {
    for (k, (order, lone, pair)) in [
        (
            "UTF-16le",
            ["00D86100", "00DC00D8", "00D800D8", "6100DFFF"],
            "00D800DC",
        ),
        (
            "UTF-16be",
            ["D8000061", "DC00D800", "D800D800", "0061DFFF"],
            "D800DC00",
        ),
    ]
    .iter()
    .enumerate()
    {
        let mut vals: Vec<String> = lone
            .iter()
            .enumerate()
            .map(|(i, h)| format!("(CAST(x'{h}' AS TEXT), {i})"))
            .collect();
        vals.push("(char(0x10000), 10), ('z', 11), (char(0x10061), 12)".to_string());
        for (j, with_index) in [true, false].iter().enumerate() {
            let p = fresh(&format!("lone{k}{j}"));
            common::cli(
                &p,
                &format!(
                    "PRAGMA encoding = '{order}'; CREATE TABLE t(a TEXT, b); {} INSERT INTO t VALUES {};",
                    if *with_index {
                        "CREATE INDEX ia ON t(a);"
                    } else {
                        ""
                    },
                    vals.join(", ")
                ),
            );
            let mut stmts = vec![
                "PRAGMA integrity_check".to_string(),
                format!("SELECT b FROM t WHERE a = CAST(x'{pair}' AS TEXT)"),
                "SELECT b FROM t WHERE a = char(0x10061) ORDER BY b".to_string(),
                "SELECT b, hex(a) FROM t ORDER BY a, b".to_string(),
            ];
            for h in lone.iter() {
                stmts.push(format!("SELECT b FROM t WHERE a = CAST(x'{h}' AS TEXT)"));
            }
            if *with_index {
                stmts.push("REINDEX".to_string());
            } else {
                stmts.push("CREATE INDEX ia ON t(a)".to_string());
                stmts.push("CREATE UNIQUE INDEX ua ON t(a, b)".to_string());
            }
            stmts.push("SELECT b FROM t WHERE a > 'a' ORDER BY a".to_string());
            stmts.push("PRAGMA integrity_check".to_string());
            // Each engine on its own copy of the pin's file.
            let q = fresh(&format!("lone{k}{j}-pin"));
            std::fs::copy(&p, &q).expect("copy");
            let got = hipp_file(&p, &stmts);
            let want = oracle_file(&q, &stmts[..stmts.len() - 1]);
            assert_eq!(
                got[0].as_ref().ok().map(|b| b.as_slice()),
                Some(&b"ok"[..]),
                "{order}: hipp's integrity_check on the pin's file"
            );
            for (i, w) in want.iter().enumerate().skip(1) {
                assert_eq!(
                    got[i].as_ref().ok(),
                    w.as_ref(),
                    "{order} (index {with_index}): {}",
                    stmts[i]
                );
            }
            both_ok(&p);
        }
    }
    // The same through hipp's own writes, compared statement by
    // statement: equality, hex, order and UNIQUE see the stored units.
    for (k, (order, rev, pair)) in [
        ("UTF-16le", "00DC00D8", "00D800DC"),
        ("UTF-16be", "DC00D800", "D800DC00"),
    ]
    .iter()
    .enumerate()
    {
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE t(a TEXT UNIQUE);
             CREATE TABLE w(k TEXT PRIMARY KEY) WITHOUT ROWID;
             INSERT INTO t VALUES (char(0x10000));
             INSERT INTO t VALUES (CAST(x'{rev}' AS TEXT));
             INSERT INTO t VALUES (CAST(x'{pair}' AS TEXT));
             INSERT INTO w SELECT a FROM t;
             SELECT hex(a) FROM t ORDER BY 1;
             SELECT hex(k), length(k) FROM w ORDER BY k;
             SELECT CAST(x'{rev}' AS TEXT) = char(0x10000), hex(CAST(x'{rev}' AS TEXT)), length(CAST(x'{rev}' AS TEXT));
             SELECT CAST(x'{rev}' AS TEXT) = CAST(x'{pair}' AS TEXT) COLLATE NOCASE;
             SELECT hex(substr(CAST(x'{rev}' AS TEXT), 1, 1)), hex(upper(CAST(x'{rev}' AS TEXT)));
             SELECT hex(CAST(x'{}' AS TEXT) || CAST(x'{}' AS TEXT)), length(CAST(x'{}' AS TEXT) || CAST(x'{}' AS TEXT));
             SELECT count(DISTINCT a), count(*) FROM t;
             SELECT a FROM t ORDER BY a;
             PRAGMA integrity_check",
            &pair[..4], &pair[4..], &pair[..4], &pair[4..],
        ));
        let (errs, hp) = agree(&format!("lone-own{k}"), &stmts);
        assert_eq!(errs, 1, "{order}: only the UNIQUE duplicate fails");
        both_ok(&hp);
    }
}

/// In a UTF-16 database the math functions and sign() give NULL for a
/// BLOB, as in UTF-8; a BLOB read as a number (unary minus, log's
/// second argument, randomblob, sum / avg / total, windows too) reads
/// its UTF-16 text; sign() of a text that is not wholly a number is
/// NULL (that last also in UTF-8).
#[test]
fn regression_blobs_as_numbers() {
    for (k, (order, b)) in [
        ("UTF-8", "322E35"),
        ("UTF-16le", "32002E003500"),
        ("UTF-16be", "0032002E0035"),
    ]
    .iter()
    .enumerate()
    {
        let x = format!("x'{b}'");
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             SELECT sqrt({x}), ln({x}), ceil({x}), floor({x}), pow({x}, 2), exp({x}), log10({x}), trunc({x}), mod({x}, 3), sign({x});
             SELECT atan2({x}, 1), atan2(1, {x}), log({x}), log({x}, 8), log(2, {x}), radians({x}), degrees({x}), acos({x}), pi();
             SELECT sign('2x'), sign(' 3 '), sign('-1e3'), sign(''), sign('abc'), sign(-0.0), sign(NULL), sign(5);
             SELECT -{x}, -'2.5', length(randomblob({x})), abs({x}), round({x}), {x} * 2, CAST({x} AS REAL);
             CREATE TABLE t(v, o);
             INSERT INTO t VALUES ({x}, 1), ({x}, 2), (1, 3);
             SELECT sum(v), avg(v), total(v), group_concat(v), string_agg(v, {x}) FROM t;
             SELECT sum(v) OVER (ORDER BY o), avg(v) OVER (ROWS 1 PRECEDING), total(v) OVER (), group_concat(v) OVER (ORDER BY o), string_agg(v, '-') OVER (ORDER BY o ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY o;
             SELECT sum(v) OVER (ORDER BY o GROUPS 1 PRECEDING EXCLUDE CURRENT ROW), group_concat(v, v) OVER (ORDER BY o RANGE BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE TIES) FROM t ORDER BY o"
        ));
        agree(&format!("num{k}"), &stmts);
    }
}

/// JSON functions in a UTF-16 database read a BLOB that is not JSONB
/// as UTF-16 text where they read text (documents, paths), reject it
/// where they take a value, and their text results go through the
/// pin's conversion (a lone surrogate becomes U+FFFD).
#[test]
fn regression_json_blob_arguments() {
    for (k, (order, doc, arr, path, lone)) in [
        ("UTF-8", "7B7D", "5B315D", "24", "D800"),
        (
            "UTF-16le",
            "7B007D00",
            "5B0031005D00",
            "24005B0030005D00",
            "00D8",
        ),
        (
            "UTF-16be",
            "007B007D",
            "005B0031005D",
            "0024005B0030005D",
            "D800",
        ),
    ]
    .iter()
    .enumerate()
    {
        let stmts = lines(&format!(
            "PRAGMA encoding = '{order}';
             CREATE TABLE t(j);
             INSERT INTO t VALUES (CAST('{{\"a\":[1,2]}}' AS BLOB));
             SELECT json_extract(j, '$.a[1]'), json_valid(j), json(j), json_type(j), j ->> '$.a', json_array_length(j, '$.a') FROM t;
             SELECT json_valid(x'{doc}'), json_valid(x'7b007d00'), json_valid(x'007b007d'), json_valid(x'7b7d'), json_valid(x'{doc}', 1), json_valid(x'{doc}', 8);
             SELECT json(x'{doc}'), json_type(x'{arr}'), json_array_length(x'{arr}'), x'{arr}' ->> 0, json_extract('[5]', x'{path}');
             SELECT json_insert(x'{doc}', '$.a', 1), json_patch(x'{doc}', '{{}}'), json_patch('{{}}', x'{doc}'), json_remove(x'{arr}', '$[0]'), json_pretty(x'{doc}'), hex(jsonb(x'{doc}'));
             SELECT json_error_position(x'7b00'), json_error_position(x'{doc}');
             SELECT count(*) FROM json_each(x'{arr}');
             SELECT count(*) FROM json_tree(x'{arr}');
             SELECT group_concat(value) FROM json_each('[[7,8]]', x'{path}');
             SELECT json_array(x'3100');
             SELECT json_quote(x'3100');
             SELECT json_set(x'{doc}', '$.a', x'3100');
             SELECT json_object(x'6100', 1);
             SELECT json_quote(CAST(x'{lone}' AS TEXT)), json_array(CAST(x'{lone}' AS TEXT)), json_group_array(CAST(x'{lone}' AS TEXT)), json_quote(CAST(x'{lone}6100' AS TEXT));
             SELECT json(CAST(x'{arr}' AS TEXT))"
        ));
        agree(&format!("json{k}"), &stmts);
    }
}

/// A header-only write (`PRAGMA user_version` / `application_id`) does
/// not fix the encoding, in the same connection or on reopen, and
/// leaves the header's schema-format and encoding fields 0 as the
/// pin's does; a schema change, even rolled back, fixes it.
#[test]
fn regression_header_write_then_encoding() {
    for (k, script) in [
        "PRAGMA user_version = 5; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding; CREATE TABLE t(a); INSERT INTO t VALUES ('\u{e9}'); SELECT hex(a) FROM t",
        "PRAGMA encoding = 'UTF-16le'; PRAGMA user_version = 3; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding; CREATE TABLE t(a); INSERT INTO t VALUES ('\u{e9}'); SELECT hex(a) FROM t",
        "PRAGMA application_id = 9; PRAGMA page_size = 1024; PRAGMA encoding = 'UTF-16le'; PRAGMA encoding; PRAGMA page_size",
        "BEGIN; CREATE TABLE x(a); ROLLBACK; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "PRAGMA user_version = 1; BEGIN; CREATE TABLE x(a); ROLLBACK; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "BEGIN; SAVEPOINT a; CREATE TABLE x(a); ROLLBACK TO a; COMMIT; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "BEGIN IMMEDIATE; COMMIT; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "CREATE TABLE x(a); DROP TABLE x; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
    ]
    .iter()
    .enumerate()
    {
        let stmts: Vec<String> = script.split("; ").map(str::to_string).collect();
        agree(&format!("hdr{k}"), &stmts);
    }
    // The header bytes of a file that only got a header write.
    let (h, o) = (fresh("hdr-bytes-h"), fresh("hdr-bytes-o"));
    hipp_file(&h, &["PRAGMA user_version = 5".to_string()]);
    common::cli(&o, "PRAGMA user_version = 5;");
    let (hb, ob) = (
        std::fs::read(&h).expect("read"),
        std::fs::read(&o).expect("read"),
    );
    assert_eq!(hb[44..48], ob[44..48], "schema format field");
    assert_eq!(hb[56..64], ob[56..64], "encoding and user_version fields");
    // Each engine chooses the other's such file's encoding on reopen.
    for (p, writer_is_hipp) in [(&h, true), (&o, false)] {
        let stmts = lines(
            "PRAGMA encoding = 'UTF-16be';
             CREATE TABLE t(a);
             INSERT INTO t VALUES ('\u{1f600}')",
        );
        if writer_is_hipp {
            let out = common::cli(p, &format!("{};", stmts.join(";\n")));
            assert_eq!(out, "", "the pin extends hipp's file");
        } else {
            assert!(hipp_file(p, &stmts).iter().all(|r| r.is_ok()));
        }
        assert_eq!(
            common::cli(p, "PRAGMA encoding; SELECT hex(a) FROM t;"),
            "UTF-16be\nD83DDE00\n"
        );
        let got = hipp_file(
            p,
            &[
                "PRAGMA encoding".to_string(),
                "SELECT hex(a) FROM t".to_string(),
            ],
        );
        assert_eq!(
            got[0].as_ref().ok().map(|b| b.as_slice()),
            Some(&b"UTF-16be"[..])
        );
        assert_eq!(
            got[1].as_ref().ok().map(|b| b.as_slice()),
            Some(&b"D83DDE00"[..])
        );
        both_ok(p);
    }
}

/// `PRAGMA temp.encoding` is the connection's encoding, as on the pin;
/// only a TEMP schema change (even rolled back) fixes it, not reading
/// the TEMP database or writing its header.
#[test]
fn regression_temp_encoding_pragma() {
    for (k, script) in [
        "PRAGMA temp.encoding = 'UTF-16be'; PRAGMA encoding; CREATE TABLE t(a); INSERT INTO t VALUES ('\u{e9}'); SELECT hex(a) FROM t",
        "PRAGMA temp.encoding = 'UTF-16be'; PRAGMA temp.encoding; PRAGMA main.encoding; PRAGMA encoding = 'UTF-16le'; PRAGMA encoding",
        "PRAGMA temp.encoding = 'UTF-16be'; CREATE TEMP TABLE x(a); INSERT INTO x VALUES ('\u{e9}'); SELECT hex(a) FROM x; PRAGMA encoding",
        "PRAGMA temp.user_version = 3; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "PRAGMA temp.page_size = 1024; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "SELECT count(*) FROM temp.sqlite_master; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "PRAGMA temp.integrity_check; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "CREATE TEMP TABLE x(a); DROP TABLE x; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "BEGIN; CREATE TEMP TABLE x(a); ROLLBACK; PRAGMA encoding = 'UTF-16be'; PRAGMA encoding",
        "CREATE TEMP VIEW v AS SELECT 1; PRAGMA temp.encoding = 'UTF-16be'; PRAGMA encoding",
        "PRAGMA encoding = 'UTF-16be'; SELECT count(*) FROM temp.sqlite_master; PRAGMA encoding = 'UTF-16le'; CREATE TEMP TABLE y(a); INSERT INTO y VALUES ('\u{e9}'); SELECT hex(a) FROM y; CREATE TABLE t(a); INSERT INTO t VALUES ('\u{e9}'); SELECT hex(a) FROM t; PRAGMA encoding",
    ]
    .iter()
    .enumerate()
    {
        let stmts: Vec<String> = script.split("; ").map(str::to_string).collect();
        agree(&format!("tempenc{k}"), &stmts);
    }
    let (errs, _) = agree(
        "tempenc-bogus",
        &["PRAGMA temp.encoding = 'bogus'", "PRAGMA encoding"].map(str::to_string),
    );
    assert_eq!(errs, 1);
}
