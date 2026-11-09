//! v0.16 bound parameters (feature `params`), differentially vs
//! the pinned oracle (SQLite 3.54.1). The oracle CLI cannot bind,
//! so each case runs in hipp with its values bound through the
//! Statement API and in the oracle with every parameter replaced
//! by the literal of the same storage class (TEXT quoted, BLOB as
//! `X'..'`, REAL as an exactly round-tripping literal, INTEGER as
//! is). The pin gives a bound value no affinity and no collation,
//! exactly as it gives a literal, so the two must agree — except
//! in ORDER BY / GROUP BY terms, where an integer literal is an
//! ordinal and a parameter a constant (those cases carry their own
//! hand-written oracle SQL).
//!
//! Probed before implementation (C API of the system library and
//! the local 3.53.1 stand-in's Python binding; the numbering has
//! not changed in years): `?` takes one past the largest index so
//! far, `?NNN` takes NNN and names it unless named already, a
//! named parameter reuses an equal spelling's index else takes the
//! next; the largest index is 32766; unbound parameters are NULL;
//! a NaN binds NULL; parameters are refused in views, CHECK,
//! DEFAULT and generated columns but allowed in CREATE TABLE ... AS
//! SELECT.
#![cfg(feature = "params")]

mod common;

use hipp::error::ParseKind;
use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step, Value};

type FsError = <FileStorage as hipp::Storage>::Error;

/// A bound value, test side.
#[derive(Clone, Debug)]
enum B {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// The literal of `b`'s storage class (a leading space keeps a
/// negative number from forming `--` after a minus).
fn lit(b: &B) -> String {
    match b {
        B::Null => "NULL".into(),
        B::Int(i) if *i < 0 => format!(" {i}"),
        B::Int(i) => i.to_string(),
        B::Real(f) if f.is_nan() => "NULL".into(),
        B::Real(f) if f.is_infinite() => {
            if *f > 0.0 {
                "9e999".into()
            } else {
                " -9e999".into()
            }
        }
        B::Real(f) if f.is_sign_negative() => format!(" {f:?}"),
        B::Real(f) => format!("{f:?}"),
        B::Text(t) => format!("'{}'", t.replace('\'', "''")),
        B::Blob(v) => {
            let hex: String = v.iter().map(|c| format!("{c:02X}")).collect();
            format!("X'{hex}'")
        }
    }
}

/// The pin's numbering, test side (the spec the probes showed):
/// each parameter token of `sql` in source order with its index.
/// Returns (byte range, index) per token and the count.
fn number(sql: &str) -> (Vec<(usize, usize, usize)>, usize) {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut names: Vec<(String, usize)> = Vec::new();
    let mut count = 0usize;
    let mut i = 0usize;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'?' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let z = &sql[start..i];
                let idx = if z.len() == 1 {
                    count += 1;
                    count
                } else {
                    let v: usize = z[1..].parse().expect("index");
                    if v > count {
                        count = v;
                    }
                    if !names.iter().any(|(_, x)| *x == v) {
                        names.push((z.to_string(), v));
                    }
                    v
                };
                out.push((start, i, idx));
            }
            b':' | b'@' | b'$' | b'#' if b.get(i + 1).is_some_and(|c| ident(*c) || *c == b':') => {
                let start = i;
                i += 1;
                loop {
                    if i < b.len() && ident(b[i]) {
                        i += 1;
                    } else if b.get(i) == Some(&b':') && b.get(i + 1) == Some(&b':') {
                        i += 2;
                    } else if b.get(i) == Some(&b'(') {
                        while i < b.len() && b[i] != b')' {
                            i += 1;
                        }
                        i += 1;
                        break;
                    } else {
                        break;
                    }
                }
                let z = &sql[start..i];
                let idx = match names.iter().find(|(n, _)| n == z) {
                    Some((_, x)) => *x,
                    None => {
                        count += 1;
                        names.push((z.to_string(), count));
                        count
                    }
                };
                out.push((start, i, idx));
            }
            c if ident(c) => {
                while i < b.len() && ident(b[i]) {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    (out, count)
}

/// `sql` with every parameter replaced by its value's literal
/// (`vals[idx - 1]`, NULL when missing).
fn inline(sql: &str, vals: &[B]) -> String {
    let (toks, _) = number(sql);
    let mut out = String::new();
    let mut at = 0;
    for (s, e, idx) in toks {
        out.push_str(&sql[at..s]);
        out.push_str(&lit(vals.get(idx - 1).unwrap_or(&B::Null)));
        at = e;
    }
    out.push_str(&sql[at..]);
    out
}

/// The CLI's list rendering of a REAL: the core's REAL-to-text
/// conversion (the pin's CLI prints exactly what CAST AS TEXT
/// gives; one source of truth, `record::render_real`).
fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(f, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// The CLI's list rendering of one value (bytes: a blob need not
/// be UTF-8; TEXT stops at a NUL; control bytes as `^X`).
fn render(v: Value<'_>) -> Vec<u8> {
    let caret = |b: &[u8]| {
        let mut o = Vec::new();
        for &c in b {
            if c < 0x20 && c != b'\t' && c != b'\n' {
                o.push(b'^');
                o.push(c + 0x40);
            } else {
                o.push(c);
            }
        }
        o
    };
    match v {
        Value::Null => Vec::new(),
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Real(f) => fmt_real(f).into_bytes(),
        Value::Text(t) => caret(&t[..t.iter().position(|c| *c == 0).unwrap_or(t.len())]),
        Value::Blob(b) => caret(b),
    }
}

fn bind<S: hipp::Storage>(st: &mut hipp::Statement<'_, S>, i: usize, b: &B) {
    let r = match b {
        B::Null => st.bind_null(i),
        B::Int(v) => st.bind_i64(i, *v),
        B::Real(f) => st.bind_f64(i, *f),
        B::Text(t) => st.bind_text(i, t),
        B::Blob(v) => st.bind_blob(i, v),
    };
    assert!(r.is_ok(), "bind {i}");
}

/// Run `sql` in hipp with `vals` bound by index (1-based); rows in
/// the CLI rendering, joined by `|`.
async fn hipp_run<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
    vals: &[B],
) -> Result<Vec<Vec<u8>>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await?;
    let (_, count) = number(sql);
    assert_eq!(st.parameter_count(), count, "parameter count of {sql}");
    for (k, v) in vals.iter().enumerate() {
        bind(&mut st, k + 1, v);
    }
    let mut rows = Vec::new();
    while st.step().await? == Step::Row {
        let mut line = Vec::new();
        for i in 0..st.column_count() {
            if i > 0 {
                line.push(b'|');
            }
            line.extend(render(st.column_value(i).unwrap_or(Value::Null)));
        }
        rows.push(line);
    }
    Ok(rows)
}

/// One oracle batch: (success, stdout lines as bytes).
fn oracle_raw(path: &std::path::Path, sql: &str) -> (bool, Vec<Vec<u8>>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    let mut lines: Vec<Vec<u8>> = out
        .stdout
        .split(|c| *c == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    (out.status.success(), lines)
}

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// Twin databases: hipp writes one, the oracle the other.
struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let tw = Twins {
        hipp: common::temp_db(&format!("v16pa-{tag}-h")),
        oracle: common::temp_db(&format!("v16pa-{tag}-o")),
    };
    for stmt in setup.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            assert!(tw.case(stmt, &[]).is_none(), "setup {stmt}");
        }
    }
    tw
}

/// Answers recorded from the real 3.54.1 pin on CI for statements the
/// 3.53.1 stand-in answers differently (see tests/substr_pin_v16.rs:
/// the two-argument substr's implicit length is i64::MAX on the pin).
fn pin_golden(oracle_sql: &str) -> Option<&'static str> {
    const GOLDEN: &[(&str, &str)] = &[
        (
            "SELECT substr('abcdefghijklmnopqrstuvwxyz', '-9223372036854775808'), \
         substr('abcdefghijkl', 2, '-9223372036854775808')",
            "abcdefghijklmnopqrstuvwxy|a",
        ),
        (
            // Derived, not recorded: the text saturates to i64::MIN when
            // read as an integer (sqlite3_value_int64), so it takes the
            // recorded row above's path. CI re-checks it live on the pin.
            "SELECT substr('abcdefghijklmnopqrstuvwxyz', '-9223372036854775809'), \
         substr('abcdefghijkl', 2, '-9223372036854775809')",
            "abcdefghijklmnopqrstuvwxy|a",
        ),
        // Derived from the recorded grid in tests/substr_pin_v16.rs: every
        // start from -1000000001 down to i64::MIN + 1 keeps the whole
        // 26-character string on the pin (3.53 returns '').
        (
            "SELECT substr('abcdefghijklmnopqrstuvwxyz', '-2251799813685248.0'), \
         substr('abcdefghijkl', 2, '-2251799813685248.0')",
            "abcdefghijklmnopqrstuvwxyz|a",
        ),
        (
            "SELECT substr('abcdefghijklmnopqrstuvwxyz', '-2251799813685247.5'), \
         substr('abcdefghijkl', 2, '-2251799813685247.5')",
            "abcdefghijklmnopqrstuvwxyz|a",
        ),
    ];
    GOLDEN
        .iter()
        .find(|(q, _)| *q == oracle_sql)
        .map(|(_, a)| *a)
}

fn show(rows: &[Vec<u8>]) -> String {
    rows.iter()
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

impl Twins {
    /// `sql` bound with `vals` in hipp, inlined in the oracle:
    /// equal rows, or both refuse (hipp's error returned).
    fn case(&self, sql: &str, vals: &[B]) -> Option<Error<FsError>> {
        self.case_with(sql, vals, &inline(sql, vals))
    }

    /// As `case`, with the oracle's SQL given.
    fn case_with(&self, sql: &str, vals: &[B], oracle_sql: &str) -> Option<Error<FsError>> {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            hipp_run(&mut db, sql, vals).await
        });
        let (ok, want) = match pin_golden(oracle_sql) {
            // A statement whose answer changed between the local
            // stand-in (3.53.1) and the 3.54.1 pin: off the pin, the
            // recorded pin answer stands in for the live oracle.
            Some(g) if !common::oracle_is_pinned() => (true, vec![g.as_bytes().to_vec()]),
            _ => oracle_raw(&self.oracle, oracle_sql),
        };
        match got {
            Ok(rows) => {
                assert!(ok, "oracle refused {oracle_sql} (hipp ran {sql} {vals:?})");
                assert_eq!(
                    show(&rows),
                    show(&want),
                    "statement {sql} with {vals:?} (oracle: {oracle_sql})"
                );
                None
            }
            Err(e) => {
                assert!(
                    !ok,
                    "hipp refused {sql} {vals:?} with {e:?}; oracle ran {oracle_sql}"
                );
                Some(e)
            }
        }
    }

    /// The oracle's view of both files: `sql` (a read) run on
    /// each must print the same.
    fn same_files(&self, sql: &str) {
        let (a, ra) = oracle_raw(&self.hipp, sql);
        let (b, rb) = oracle_raw(&self.oracle, sql);
        assert!(a && b, "oracle read of {sql}");
        assert_eq!(show(&ra), show(&rb), "file contents via {sql}");
        let (ok, ic) = oracle_raw(&self.hipp, "PRAGMA integrity_check;");
        assert!(ok && show(&ic) == "ok", "integrity of the hipp file");
    }
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
        self.next() % n
    }
}

/// A random value of a random storage class (text kept free of
/// NUL so it can be inlined; reals round-trip exactly).
fn rand_value(rng: &mut Rng) -> B {
    match rng.below(9) {
        0 => B::Null,
        1 => B::Int(rng.below(21) as i64 - 10),
        2 => B::Int(match rng.below(4) {
            0 => i64::MAX,
            1 => i64::MIN,
            2 => 1 << 53,
            _ => rng.next() as i64,
        }),
        3 => B::Real((rng.below(2001) as f64 - 1000.0) / 8.0),
        // A REAL with at most 16 significant decimal digits
        // (n / 2^m, or n * 2^k): its text rendering is the same on
        // both engines. (The pin renders other REALs as TEXT with
        // up to 17 digits of its own decoding, which hipp does not
        // reproduce exactly — a known divergence, see STATUS.)
        4 => B::Real(if rng.below(2) == 0 {
            (rng.below(1 << 30) as f64 - (1u64 << 29) as f64) / (1u64 << rng.below(11)) as f64
        } else {
            (rng.below(1 << 30) as f64) * (1u64 << rng.below(20)) as f64
        }),
        5 => B::Text(
            [
                "5",
                " 5",
                "5.0",
                "-3",
                "abc",
                "",
                "1e3",
                "0x10",
                "9223372036854775808",
                "a'b",
            ][rng.below(10) as usize]
                .to_string(),
        ),
        6 => B::Text(format!("{}", rng.below(30) as i64 - 10)),
        7 => B::Blob((0..rng.below(4)).map(|_| rng.below(256) as u8).collect()),
        _ => B::Blob(b"5".to_vec()),
    }
}

// ---------------------------------------------------------------
// Numbering and the Statement API (hipp alone: the oracle CLI has
// no binding API; expectations are the probed C-API answers).
// ---------------------------------------------------------------

/// (sql, count, names of 1..=count, (name, index) lookups) — the
/// pin's `sqlite3_bind_parameter_count / _name / _index` answers.
#[cfg(feature = "query")]
#[allow(clippy::type_complexity)]
const NUMBERING: &[(&str, usize, &[Option<&str>], &[(&str, Option<usize>)])] = &[
    ("SELECT ?5", 5, &[None, None, None, None, Some("?5")], &[]),
    (
        "SELECT ?5, :a",
        6,
        &[None, None, None, None, Some("?5"), Some(":a")],
        &[(":a", Some(6)), ("?5", Some(5)), ("?6", None)],
    ),
    (
        "SELECT :a, ?1",
        1,
        &[Some(":a")],
        &[(":a", Some(1)), ("?1", None)],
    ),
    (
        "SELECT ?1, :a",
        2,
        &[Some("?1"), Some(":a")],
        &[("?1", Some(1)), (":a", Some(2))],
    ),
    (
        "SELECT ?2, ?",
        3,
        &[None, Some("?2"), None],
        &[("?2", Some(2)), ("?3", None)],
    ),
    (
        "SELECT ?01, ?1",
        1,
        &[Some("?01")],
        &[("?01", Some(1)), ("?1", None)],
    ),
    (
        "SELECT :a, :A",
        2,
        &[Some(":a"), Some(":A")],
        &[(":a", Some(1)), (":A", Some(2))],
    ),
    (
        "SELECT $a, @a, :a",
        3,
        &[Some("$a"), Some("@a"), Some(":a")],
        &[
            ("$a", Some(1)),
            ("@a", Some(2)),
            (":a", Some(3)),
            ("a", None),
        ],
    ),
    ("SELECT #a", 1, &[Some("#a")], &[("#a", Some(1))]),
    ("SELECT $a::b", 1, &[Some("$a::b")], &[("$a::b", Some(1))]),
    ("SELECT $a(x)", 1, &[Some("$a(x)")], &[("$a(x)", Some(1))]),
    ("SELECT :1", 1, &[Some(":1")], &[(":1", Some(1))]),
    ("SELECT @1a", 1, &[Some("@1a")], &[]),
    ("SELECT ?a", 1, &[None], &[]),
    ("SELECT ?1a", 1, &[Some("?1")], &[]),
    ("SELECT :a$b", 1, &[Some(":a$b")], &[]),
    ("SELECT :ä", 1, &[Some(":ä")], &[(":ä", Some(1))]),
    (
        "SELECT ?, :a, ?, :a, ?",
        4,
        &[None, Some(":a"), None, None],
        &[(":a", Some(2))],
    ),
    ("SELECT ?3, ?1", 3, &[Some("?1"), None, Some("?3")], &[]),
    ("SELECT ?1, ?1", 1, &[Some("?1")], &[]),
    (
        "SELECT :a, ?2, :b",
        3,
        &[Some(":a"), Some("?2"), Some(":b")],
        &[(":b", Some(3))],
    ),
    (
        "SELECT ?2, :a, ?1",
        3,
        &[Some("?1"), Some("?2"), Some(":a")],
        &[],
    ),
    ("SELECT ? + ?1", 1, &[Some("?1")], &[]),
    ("SELECT ?1+?", 2, &[Some("?1"), None], &[]),
    ("SELECT ?- ?", 2, &[None, None], &[]),
    ("SELECT ?+5", 1, &[None], &[]),
    (
        "SELECT ?00000000000000000000001",
        1,
        &[Some("?00000000000000000000001")],
        &[],
    ),
    ("SELECT :a,:a,:a", 1, &[Some(":a")], &[]),
    ("SELECT ?1 ,? ,?", 3, &[Some("?1"), None, None], &[]),
    ("SELECT 1", 0, &[], &[("?1", None)]),
    (
        "SELECT '?', \":a\" FROM (SELECT 1 AS \":a\") -- ?",
        0,
        &[],
        &[],
    ),
];

#[cfg(feature = "query")]
#[test]
fn numbering_matches_the_pin() {
    block_on(async {
        let mut db = Connection::open(hipp::MemStorage::new(), Config::default())
            .await
            .expect("open");
        for (sql, count, names, lookups) in NUMBERING {
            let st = db
                .prepare(sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            assert_eq!(st.parameter_count(), *count, "count of {sql}");
            for (k, n) in names.iter().enumerate() {
                assert_eq!(st.parameter_name(k + 1), *n, "name {} of {sql}", k + 1);
            }
            assert_eq!(st.parameter_name(0), None);
            assert_eq!(st.parameter_name(count + 1), None);
            for (name, want) in lookups.iter() {
                assert_eq!(st.parameter_index(name), *want, "index of {name} in {sql}");
            }
            let (_, test_count) = number(sql);
            assert_eq!(test_count, *count, "test-side numbering of {sql}");
        }
        // The largest index, and one past it.
        let st = db.prepare("SELECT ?32766").await.expect("max index");
        assert_eq!(st.parameter_count(), 32766);
        drop(st);
        let st = db.prepare("SELECT ?32765, ?").await.expect("max by ?");
        assert_eq!(st.parameter_count(), 32766);
    });
}

#[cfg(feature = "query")]
#[test]
fn numbering_errors_match_the_pin() {
    block_on(async {
        let mut db = Connection::open(
            hipp::MemStorage::new(),
            Config::default().statement_arena(1 << 20, 1 << 22),
        )
        .await
        .expect("open");
        let kind = |e: Error<_>| match e {
            Error::Parse { kind, .. } => Some(kind),
            _ => None,
        };
        for (sql, want) in [
            ("SELECT ?0", ParseKind::VariableNumber),
            ("SELECT ?32767", ParseKind::VariableNumber),
            ("SELECT ?99999999999999999999", ParseKind::VariableNumber),
            ("SELECT ?9223372036854775807", ParseKind::VariableNumber),
            ("SELECT ?32766, ?", ParseKind::TooManyVariables),
            ("SELECT ?32766, :a", ParseKind::TooManyVariables),
            ("SELECT #1", ParseKind::UnexpectedToken),
            ("SELECT #12a", ParseKind::UnexpectedToken),
            ("SELECT :", ParseKind::UnexpectedToken),
            ("SELECT @", ParseKind::UnexpectedToken),
            ("SELECT $", ParseKind::UnexpectedToken),
            ("SELECT #", ParseKind::UnexpectedToken),
            ("SELECT $a(x y)", ParseKind::UnexpectedToken),
            ("SELECT $a(", ParseKind::UnexpectedToken),
        ] {
            let e = db.prepare(sql).await.err().map(kind);
            assert_eq!(e, Some(Some(want)), "{sql}");
        }
        // A parameter where the grammar takes none (the pin's
        // "syntax error").
        for sql in ["SELECT 1?", "SELECT x'00' ?", "SELECT 1:a", "SELECT 1 AS ?"] {
            assert!(
                matches!(db.prepare(sql).await.err(), Some(Error::Parse { .. })),
                "{sql}"
            );
        }
        // 32766 nameless parameters fit; the 32767th does not.
        let mut sql = String::from("SELECT 1 WHERE 1 IN (");
        sql.push_str(&vec!["?"; 32766].join(","));
        sql.push(')');
        let st = db.prepare(&sql).await.expect("32766 parameters");
        assert_eq!(st.parameter_count(), 32766);
        drop(st);
        sql.insert_str(sql.len() - 1, ",?");
        let e = db.prepare(&sql).await.err().map(kind);
        assert_eq!(e, Some(Some(ParseKind::TooManyVariables)));
    });
}

#[cfg(feature = "query")]
#[test]
fn the_bind_api_contract() {
    block_on(async {
        let mut db = Connection::open(hipp::MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut st = db.prepare("SELECT ?1, ?2, :c").await.expect("prepare");
        // Unbound parameters are NULL.
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert!((0..3).all(|i| st.column_value(i) == Some(Value::Null)));
        // Binding a stepped statement needs a reset first.
        assert!(matches!(st.bind_i64(1, 5), Err(Error::ResetRequired)));
        st.reset();
        assert!(matches!(
            st.bind_i64(0, 5),
            Err(Error::BindIndexOutOfRange { index: 0 })
        ));
        assert!(matches!(
            st.bind_i64(4, 5),
            Err(Error::BindIndexOutOfRange { index: 4 })
        ));
        st.bind_i64(1, -7).expect("bind 1");
        st.bind_f64(2, f64::NAN).expect("bind NaN");
        st.bind_text(3, "x\0y").expect("bind text with NUL");
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert_eq!(st.column_value(0), Some(Value::Integer(-7)));
        // The pin stores a NaN double as NULL.
        assert_eq!(st.column_value(1), Some(Value::Null));
        assert_eq!(st.column_value(2), Some(Value::Text(b"x\0y")));
        assert_eq!(st.step().await.expect("done"), Step::Done);
        // reset keeps the bindings; clear_bindings drops them.
        st.reset();
        assert_eq!(st.step().await.expect("again"), Step::Row);
        assert_eq!(st.column_value(0), Some(Value::Integer(-7)));
        st.clear_bindings();
        st.reset();
        assert_eq!(st.step().await.expect("cleared"), Step::Row);
        assert_eq!(st.column_value(0), Some(Value::Null));
        st.reset();
        st.bind_value(1, Value::Blob(b"\x01\x02"))
            .expect("bind_value");
        st.bind_value(2, Value::Real(2.5)).expect("bind_value");
        st.bind_null(3).expect("bind_null");
        assert_eq!(st.step().await.expect("step"), Step::Row);
        assert_eq!(st.column_value(0), Some(Value::Blob(b"\x01\x02")));
        assert_eq!(st.column_value(1), Some(Value::Real(2.5)));
        assert_eq!(st.column_value(2), Some(Value::Null));
        drop(st);
        // A statement without parameters re-executes too.
        let mut st = db.prepare("SELECT 1").await.expect("prepare");
        assert_eq!(st.parameter_count(), 0);
        for _ in 0..3 {
            assert_eq!(st.step().await.expect("row"), Step::Row);
            assert_eq!(st.step().await.expect("done"), Step::Done);
            st.reset();
        }
    });
}

// ---------------------------------------------------------------
// The tiny grammar's literal positions (every profile).
// ---------------------------------------------------------------

/// The numbering and the bind / reset cycle through the tiny
/// grammar alone (INSERT VALUES and `WHERE rowid = ?`), as the
/// `params` leaf builds on the tiny profile.
#[test]
fn numbering_and_api_in_the_tiny_grammar() {
    block_on(async {
        let mut db = Connection::open(hipp::MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut st = db
            .prepare("CREATE TABLE t(a, b, c, d)")
            .await
            .expect("create");
        assert_eq!(st.step().await.expect("create"), Step::Done);
        drop(st);
        let mut st = db
            .prepare("INSERT INTO t VALUES (?3, :a, ?, :a)")
            .await
            .expect("prepare");
        // ?3 sets 3, :a takes 4, ? takes 5, :a reuses 4.
        assert_eq!(st.parameter_count(), 5);
        assert_eq!(st.parameter_name(1), None);
        assert_eq!(st.parameter_name(5), None);
        assert_eq!(st.parameter_name(3), Some("?3"));
        assert_eq!(st.parameter_name(4), Some(":a"));
        assert_eq!(st.parameter_index(":a"), Some(4));
        assert_eq!(st.parameter_index("?3"), Some(3));
        assert_eq!(st.parameter_index("?1"), None);
        assert!(matches!(
            st.bind_i64(6, 1),
            Err(Error::BindIndexOutOfRange { index: 6 })
        ));
        for k in 0..5i64 {
            st.bind_i64(3, k).expect("bind ?3");
            st.bind_text(4, "x").expect("bind :a");
            st.bind_f64(5, 0.5).expect("bind ?");
            assert_eq!(st.step().await.expect("insert"), Step::Done);
            assert!(matches!(st.bind_null(1), Err(Error::ResetRequired)));
            st.reset();
        }
        drop(st);
        let mut st = db
            .prepare("SELECT a, b, c, d FROM t WHERE rowid = :r")
            .await
            .expect("prepare select");
        assert_eq!(st.parameter_count(), 1);
        for (key, want) in [
            (B::Int(2), Some(1)),
            (B::Text("5".into()), Some(4)),
            (B::Null, None),
        ] {
            bind(&mut st, 1, &key);
            if let Some(a) = want {
                assert_eq!(st.step().await.expect("row"), Step::Row);
                assert_eq!(st.column_value(0), Some(Value::Integer(a)));
                assert_eq!(st.column_value(1), Some(Value::Text(b"x")));
                assert_eq!(st.column_value(2), Some(Value::Real(0.5)));
                assert_eq!(st.column_value(3), Some(Value::Text(b"x")));
            }
            assert_eq!(st.step().await.expect("done"), Step::Done);
            st.reset();
        }
        // Unbound is NULL: a NULL rowid matches nothing.
        st.clear_bindings();
        assert_eq!(st.step().await.expect("none"), Step::Done);
        drop(st);
        for (sql, kind) in [
            (
                "INSERT INTO t VALUES (?0, 1, 2, 3)",
                ParseKind::VariableNumber,
            ),
            (
                "INSERT INTO t VALUES (?32767, 1, 2, 3)",
                ParseKind::VariableNumber,
            ),
            (
                "INSERT INTO t VALUES (?32766, ?, 2, 3)",
                ParseKind::TooManyVariables,
            ),
            (
                "INSERT INTO t VALUES (#1, 1, 2, 3)",
                ParseKind::UnexpectedToken,
            ),
        ] {
            assert!(
                matches!(db.prepare(sql).await.err(), Some(Error::Parse { kind: k, .. }) if k == kind),
                "{sql}"
            );
        }
        // Parameters in a table definition are refused.
        assert!(matches!(
            db.prepare("CREATE TABLE u(a DEFAULT (?))").await.err(),
            Some(Error::Schema {
                kind: hipp::SchemaKind::ParameterNotAllowed
            }) | Some(Error::Parse { .. })
        ));
    });
}

#[test]
fn literal_positions_of_the_tiny_grammar() {
    let tw = twins(
        "tiny",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, i INTEGER, tx TEXT, n NUMERIC, r REAL, b BLOB, x)",
    );
    let mut rng = Rng(0x7e57_0001);
    for k in 0..60 {
        let mut vals = Vec::new();
        for _ in 0..6 {
            vals.push(rand_value(&mut rng));
        }
        vals[0] = B::Int(k + 1);
        let sql = match k % 3 {
            0 => "INSERT INTO t VALUES (?, ?, ?, ?, ?, ?, ?6)",
            1 => "INSERT INTO t (id, i, tx, n, r, b, x) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            _ => "INSERT INTO t VALUES (:id, :i, :t, :n, :r, :b, :b)",
        };
        assert!(tw.case(sql, &vals).is_none());
    }
    // Rowid seeks by every storage class of key.
    for v in [
        B::Int(3),
        B::Real(3.0),
        B::Real(3.5),
        B::Text("3".into()),
        B::Text(" 3".into()),
        B::Text("3.0".into()),
        B::Text("x".into()),
        B::Blob(b"3".to_vec()),
        B::Null,
        B::Int(1000),
    ] {
        assert!(
            tw.case("SELECT * FROM t WHERE rowid = ?", std::slice::from_ref(&v))
                .is_none()
        );
        assert!(tw.case("SELECT tx, b FROM t WHERE id = :k", &[v]).is_none());
    }
    // Rowid conversion of a bound key (MustBeInt) — errors agree.
    for v in [
        B::Text("100".into()),
        B::Real(101.0),
        B::Text("abc".into()),
        B::Real(102.5),
        B::Blob(b"103".to_vec()),
        B::Null,
    ] {
        tw.case("INSERT INTO t (id, x) VALUES (?, 'k')", &[v]);
    }
    tw.same_files("SELECT id, typeof(i), quote(i), typeof(tx), quote(tx), typeof(n), quote(n), typeof(r), quote(r), typeof(b), quote(b), typeof(x), quote(x) FROM t ORDER BY id;");
}

#[test]
fn a_prepared_insert_is_re_executed_with_new_bindings() {
    let tw = twins("reexec", "CREATE TABLE t(a INTEGER, b TEXT, c)");
    let mut rng = Rng(0x7e57_0002);
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows.push([
            rand_value(&mut rng),
            rand_value(&mut rng),
            rand_value(&mut rng),
        ]);
    }
    let high = block_on(async {
        let mut db = open(&tw.hipp).await;
        let mut st = db
            .prepare("INSERT INTO t VALUES (?, ?, ?)")
            .await
            .expect("prepare");
        let mut high = Vec::new();
        for r in &rows {
            for (k, v) in r.iter().enumerate() {
                bind(&mut st, k + 1, v);
            }
            assert_eq!(st.step().await.expect("insert"), Step::Done);
            st.reset();
            high.push(st.arena_high_water());
        }
        high
    });
    // Re-planning rewinds the statement arena: its high water is
    // bounded by the largest single execution, not their sum.
    let (early, last) = (high[19], *high.last().expect("ran"));
    assert!(
        last < early + 512 && last < 16 * 1024,
        "arena grew with executions: {early} -> {last}"
    );
    for r in &rows {
        let sql = inline("INSERT INTO t VALUES (?, ?, ?)", r);
        assert!(oracle_raw(&tw.oracle, &sql).0, "{sql}");
    }
    tw.same_files(
        "SELECT rowid, typeof(a), quote(a), typeof(b), quote(b), typeof(c), quote(c) FROM t;",
    );
}

// ---------------------------------------------------------------
// The full dialect: every expression position (v16 / full).
// ---------------------------------------------------------------

#[cfg(feature = "sql")]
const SETUP: &str = "CREATE TABLE t(i INTEGER, tx TEXT, n NUMERIC, r REAL, b BLOB, x);
    INSERT INTO t VALUES (1, '1', 1, 1.0, '1', 1);
    INSERT INTO t VALUES (5, '5', 5, 5.0, '5', '5');
    INSERT INTO t VALUES (NULL, 'abc', 'abc', NULL, x'61', x'05');
    INSERT INTO t VALUES (-3, '-3', 2.5, -3.5, 2.5, 2.5);
    INSERT INTO t VALUES (7, 'a''b', '7', 7, NULL, 'A');
    CREATE TABLE k(id INTEGER PRIMARY KEY, a TEXT UNIQUE, v);
    INSERT INTO k VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', NULL)";

#[cfg(feature = "sql")]
#[test]
fn affinity_of_bound_values_matches_literals() {
    let tw = twins("aff", SETUP);
    let vals = [
        B::Text("5".into()),
        B::Text(" 5".into()),
        B::Text("5.0".into()),
        B::Text("5e0".into()),
        B::Text("abc".into()),
        B::Text("0x10".into()),
        B::Text("9223372036854775808".into()),
        B::Int(5),
        B::Real(5.0),
        B::Real(5.5),
        B::Blob(b"5".to_vec()),
        B::Null,
        B::Text("1".into()),
        B::Int(1),
        B::Text("A".into()),
    ];
    for v in &vals {
        for col in [
            "i",
            "tx",
            "n",
            "r",
            "b",
            "x",
            "rowid",
            "+i",
            "i COLLATE NOCASE",
        ] {
            for op in ["=", "<", ">=", "IS", "!="] {
                let sql = format!("SELECT count(*) FROM t WHERE {col} {op} ?");
                assert!(tw.case(&sql, std::slice::from_ref(v)).is_none());
                let sql = format!("SELECT count(*) FROM t WHERE ? {op} {col}");
                assert!(tw.case(&sql, std::slice::from_ref(v)).is_none());
            }
            let sql = format!("SELECT {col} IN (?, 7), {col} BETWEEN ? AND 6 FROM t");
            assert!(tw.case(&sql, &[v.clone(), v.clone()]).is_none());
            // A bound value has no affinity, so `? IN (col, ...)`
            // compares without converting (the pin takes an IN
            // list's affinity from its left operand only).
            let sql = format!("SELECT ?1 IN ({col}), ?1 IN ({col}, 7), ?1 NOT IN ({col}) FROM t");
            assert!(tw.case(&sql, std::slice::from_ref(v)).is_none());
        }
        for sql in [
            "SELECT ?1 = 5, ?1 = '5', ?1 IS NULL, typeof(?1), ?1 + 0, ?1 || 'z'",
            "SELECT CASE ?1 WHEN 5 THEN 'five' WHEN '5' THEN 'text' ELSE 'other' END",
            "SELECT CAST(?1 AS INTEGER), CAST(?1 AS REAL), CAST(?1 AS TEXT), CAST(?1 AS NUMERIC)",
            "SELECT 1 WHERE ?1",
            "SELECT -?1, +?1, NOT ?1, abs(?1), length(?1), hex(?1), upper(?1)",
            "SELECT ?1 COLLATE NOCASE = 'a', 'A' IN (?1), ?1 LIKE 'A%', ?1 GLOB '[0-9]*'",
            "SELECT coalesce(?1, 'dflt'), nullif(?1, 5), iif(?1, 'y', 'n'), max(?1, 3), min(?1, 'z')",
        ] {
            assert!(tw.case(sql, std::slice::from_ref(v)).is_none());
        }
    }
    // Stored through each column's affinity.
    let mut id = 100;
    for v in &vals {
        id += 1;
        let sql = "INSERT INTO t (rowid, i, tx, n, r, b, x) VALUES (?7, ?1, ?1, ?1, ?1, ?1, ?1)";
        let mut row = vec![v.clone(); 6];
        row.push(B::Int(id));
        assert!(tw.case(sql, &row).is_none());
    }
    tw.same_files("SELECT rowid, typeof(i), quote(i), typeof(tx), quote(tx), typeof(n), quote(n), typeof(r), quote(r), typeof(b), quote(b), typeof(x), quote(x) FROM t;");
}

#[cfg(feature = "sql")]
#[test]
fn parameters_in_every_expression_position() {
    let tw = twins("pos", SETUP);
    let v = |x: i64| B::Int(x);
    let s = |x: &str| B::Text(x.into());
    let cases: Vec<(&str, Vec<B>)> = vec![
        ("SELECT i, tx FROM t WHERE i > ? ORDER BY i", vec![v(0)]),
        ("SELECT i FROM t WHERE tx = :t OR n = :t", vec![s("5")]),
        ("SELECT i FROM t LIMIT ?", vec![v(2)]),
        (
            "SELECT i FROM t LIMIT ? OFFSET ?",
            vec![s("2"), B::Real(1.0)],
        ),
        ("SELECT i FROM t LIMIT ?2, ?1", vec![v(2), v(1)]),
        ("SELECT i FROM t LIMIT ?", vec![v(-1)]),
        ("SELECT i FROM t LIMIT ?", vec![B::Real(2.5)]),
        ("SELECT i FROM t LIMIT ?", vec![B::Null]),
        ("SELECT i FROM t LIMIT ?", vec![s("abc")]),
        ("SELECT i FROM t LIMIT 1 OFFSET ?", vec![s(" 2 ")]),
        ("SELECT (SELECT a FROM k WHERE id = ?)", vec![v(2)]),
        (
            "SELECT i FROM t WHERE i IN (SELECT id FROM k WHERE v >= ?)",
            vec![v(20)],
        ),
        (
            "SELECT EXISTS (SELECT 1 FROM k WHERE a = ?)",
            vec![s("two")],
        ),
        ("SELECT a FROM k WHERE v = (SELECT ? * 10)", vec![v(1)]),
        (
            "SELECT t.i, k.a FROM t JOIN k ON k.id = t.i + ? ORDER BY 1, 2",
            vec![v(0)],
        ),
        (
            "SELECT t.i, k.a FROM t LEFT JOIN k ON k.id = t.i AND k.v > ? ORDER BY 1",
            vec![v(10)],
        ),
        (
            "SELECT s.c + ? FROM (SELECT i AS c FROM t WHERE i < ?) AS s ORDER BY 1",
            vec![v(100), v(6)],
        ),
        (
            "SELECT n, count(*) FROM t GROUP BY n HAVING count(*) >= ?",
            vec![v(1)],
        ),
        (
            "SELECT sum(i * ?), group_concat(tx, ?) FROM t",
            vec![v(2), s(";")],
        ),
        (
            "SELECT ? UNION SELECT ? UNION SELECT i FROM t ORDER BY 1",
            vec![v(3), s("x")],
        ),
        (
            "SELECT i FROM t WHERE i < ? EXCEPT SELECT ? ORDER BY 1",
            vec![v(6), v(1)],
        ),
        (
            "SELECT i FROM t INTERSECT SELECT id FROM k WHERE id > ?",
            vec![v(0)],
        ),
        (
            "VALUES (?, ?), (?3, ?1)",
            vec![v(1), s("b"), B::Blob(vec![0xff])],
        ),
        (
            "SELECT * FROM (VALUES (?1), (?2)) ORDER BY 1",
            vec![s("b"), s("a")],
        ),
        (
            "SELECT a FROM k WHERE a LIKE ? ESCAPE ?",
            vec![s("t!_%"), s("!")],
        ),
        ("SELECT a FROM k WHERE a GLOB ? ORDER BY 1", vec![s("t*")]),
        (
            "SELECT i FROM t WHERE i BETWEEN ? AND ?",
            vec![s("0"), B::Real(5.0)],
        ),
        (
            "SELECT CASE WHEN i > ? THEN ? ELSE ? END FROM t",
            vec![v(2), s("big"), B::Null],
        ),
        (
            "SELECT i FROM t WHERE x IN (?, ?, ?)",
            vec![s("5"), v(1), B::Blob(vec![5])],
        ),
        (
            "SELECT tx FROM t WHERE tx = ? COLLATE NOCASE",
            vec![s("ABC")],
        ),
        (
            "SELECT a FROM k WHERE id = :id AND a = :a",
            vec![v(1), s("one")],
        ),
        ("SELECT $x, @y, :z, #w", vec![v(1), v(2), v(3), v(4)]),
        (
            "SELECT a FROM k WHERE id IN (SELECT ?1 UNION SELECT ?1 + 1)",
            vec![v(1)],
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE i > k.id * ?) FROM k",
            vec![v(1)],
        ),
        ("SELECT i FROM t WHERE i IN k", vec![]),
    ];
    for (sql, vals) in &cases {
        tw.case(sql, vals);
    }
    // Writes: SET, WHERE, VALUES, upsert, RETURNING, INSERT ...
    // SELECT, UPDATE ... FROM, DELETE.
    let writes: Vec<(&str, Vec<B>)> = vec![
        (
            "INSERT INTO k (a, v) VALUES (?, ?) RETURNING id, a, v",
            vec![s("four"), B::Real(4.5)],
        ),
        (
            "INSERT INTO k (a, v) VALUES (?, ? + 1), (?, ?) RETURNING id",
            vec![s("five"), v(5), s("six"), B::Null],
        ),
        (
            "INSERT INTO k (a, v) VALUES (?1, ?2) ON CONFLICT (a) DO UPDATE SET v = v + ?2 WHERE v IS NOT ?3 RETURNING v",
            vec![s("one"), v(7), v(0)],
        ),
        (
            "INSERT INTO k (a, v) VALUES (:a, :v) ON CONFLICT DO NOTHING",
            vec![s("two"), v(0)],
        ),
        (
            "INSERT OR REPLACE INTO k (id, a, v) VALUES (?, ?, ?)",
            vec![v(3), s("tres"), s("3")],
        ),
        (
            "INSERT INTO k (a, v) SELECT tx || ?, i * ? FROM t WHERE i > ?",
            vec![s("!"), v(10), v(4)],
        ),
        (
            "UPDATE k SET v = ?, a = a || ? WHERE id = ? RETURNING *",
            vec![B::Blob(vec![1, 2]), s("+"), v(2)],
        ),
        (
            "UPDATE k SET v = coalesce(v, 0) + ? WHERE v < ? OR v IS NULL",
            vec![B::Real(0.5), v(15)],
        ),
        (
            "UPDATE k SET v = t.n FROM t WHERE t.i = k.id + ?",
            vec![v(0)],
        ),
        (
            "UPDATE t SET x = ?1, r = ?1, i = ?1 WHERE tx = ?2",
            vec![s("5.0"), s("abc")],
        ),
        ("DELETE FROM k WHERE a = ? RETURNING id, v", vec![s("five")]),
        ("DELETE FROM t WHERE i < ? AND i > ?", vec![v(0), v(-10)]),
        (
            "CREATE TABLE c AS SELECT ? AS p, ?, ? + 1, :n FROM k WHERE id < ?",
            vec![v(1), s("a"), B::Real(2.5), B::Null, v(3)],
        ),
    ];
    for (sql, vals) in &writes {
        assert!(tw.case(sql, vals).is_none(), "{sql}");
    }
    tw.same_files("SELECT id, quote(a), typeof(v), quote(v) FROM k ORDER BY id;");
    tw.same_files(
        "SELECT rowid, quote(i), quote(tx), quote(n), quote(r), quote(b), quote(x) FROM t;",
    );
    tw.same_files("SELECT * FROM c;");
    // CTAS names its columns by the parameter text, as the pin.
    let (ok, sql) = oracle_raw(&tw.hipp, "SELECT sql FROM sqlite_schema WHERE name = 'c';");
    assert!(ok);
    assert_eq!(show(&sql), "CREATE TABLE c(p,\"?\",\"? + 1\",\":n\")");
}

#[cfg(feature = "sql")]
#[test]
fn order_and_group_by_parameters_are_constants() {
    let tw = twins("ord", SETUP);
    let one = || B::Int(1);
    // A bound integer is a constant term, not an ordinal: the pin
    // keeps scan order (`ORDER BY 1+0` is the same constant).
    for (sql, vals, ora) in [
        (
            "SELECT i FROM t ORDER BY ?",
            vec![one()],
            "SELECT i FROM t ORDER BY 1+0",
        ),
        (
            "SELECT i FROM t ORDER BY ? DESC",
            vec![B::Int(9)],
            "SELECT i FROM t ORDER BY 9+0 DESC",
        ),
        (
            "SELECT i FROM t ORDER BY ? COLLATE NOCASE, i",
            vec![one()],
            "SELECT i FROM t ORDER BY i",
        ),
        (
            "SELECT i FROM t ORDER BY -?",
            vec![one()],
            "SELECT i FROM t ORDER BY -(1+0)",
        ),
        (
            "SELECT i FROM t ORDER BY ?1 + 0, i DESC",
            vec![one()],
            "SELECT i FROM t ORDER BY i DESC",
        ),
        (
            "SELECT count(*) FROM t GROUP BY ?",
            vec![one()],
            "SELECT count(*) FROM t GROUP BY 1+0",
        ),
        (
            "SELECT count(*) FROM t WHERE 0 GROUP BY ?",
            vec![one()],
            "SELECT count(*) FROM t WHERE 0 GROUP BY 1+0",
        ),
        (
            "SELECT n, count(*) FROM t GROUP BY ?, n ORDER BY n",
            vec![B::Int(2)],
            "SELECT n, count(*) FROM t GROUP BY n ORDER BY n",
        ),
        (
            "SELECT ?1, i FROM t ORDER BY ?1",
            vec![one()],
            "SELECT 1, i FROM t ORDER BY 1+0",
        ),
        (
            "SELECT i FROM t GROUP BY i ORDER BY ?",
            vec![B::Int(2)],
            "SELECT i FROM t GROUP BY i ORDER BY 2+0",
        ),
        (
            "SELECT i FROM t ORDER BY ?",
            vec![B::Text("1".into())],
            "SELECT i FROM t ORDER BY '1'",
        ),
        (
            "SELECT i FROM t ORDER BY ?",
            vec![B::Int(5)],
            "SELECT i FROM t ORDER BY 5+0",
        ),
        // A compound ORDER BY parameter matches a result item
        // that is the same parameter (any core, the leftmost
        // first: the pin's resolveCompoundOrderBy resolves from
        // the first SELECT rightwards; probed with real bindings
        // on the 3.53.1 library).
        (
            "SELECT ?1 AS c UNION ALL SELECT 2 ORDER BY ?1",
            vec![B::Int(5)],
            "SELECT 5 AS c UNION ALL SELECT 2 ORDER BY 1",
        ),
        (
            "SELECT 9, ?1 UNION ALL SELECT 1, 2 ORDER BY ?1",
            vec![B::Int(5)],
            "SELECT 9, 5 UNION ALL SELECT 1, 2 ORDER BY 2",
        ),
        (
            "SELECT 1, 2 UNION ALL SELECT 9, ?1 ORDER BY ?1 DESC",
            vec![B::Int(5)],
            "SELECT 1, 2 UNION ALL SELECT 9, 5 ORDER BY 2 DESC",
        ),
        (
            "SELECT :a UNION ALL SELECT 2 ORDER BY :a",
            vec![B::Int(5)],
            "SELECT 5 UNION ALL SELECT 2 ORDER BY 1",
        ),
        (
            "SELECT ?1 UNION ALL SELECT 2 ORDER BY ?1 COLLATE NOCASE",
            vec![B::Int(5)],
            "SELECT 5 UNION ALL SELECT 2 ORDER BY 1 COLLATE NOCASE",
        ),
        // Regression: the leftmost core decides when the parameter
        // sits in different columns of different cores (hipp took
        // the rightmost).
        (
            "SELECT ?1, 2 UNION SELECT 3, ?1 ORDER BY ?1",
            vec![B::Int(9)],
            "SELECT 9, 2 UNION SELECT 3, 9 ORDER BY 1",
        ),
        (
            "SELECT 5, ?1 UNION ALL SELECT ?1, 2 ORDER BY ?1 DESC",
            vec![B::Int(0)],
            "SELECT 5, 0 UNION ALL SELECT 0, 2 ORDER BY 2 DESC",
        ),
        (
            "SELECT 5, 6 UNION ALL SELECT ?1, 2 UNION ALL SELECT 7, ?1 ORDER BY ?1",
            vec![B::Int(1)],
            "SELECT 5, 6 UNION ALL SELECT 1, 2 UNION ALL SELECT 7, 1 ORDER BY 1",
        ),
        (
            "VALUES (0, ?1) UNION ALL SELECT ?1, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (0, 1) UNION ALL SELECT 1, 3 ORDER BY 2",
        ),
        // A multi-row VALUES is laid out as the pin's
        // sqlite3MultiValues builds it. A row stays a core of its
        // own (and matches) when the row after it holds a subquery,
        // or when it has a subquery or a top-level CAST itself;
        // rows that share a co-routine never match.
        (
            "VALUES (CAST(1 AS INT), ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (CAST(1 AS INT), 1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 2",
        ),
        (
            "VALUES (CAST(1 AS INT) COLLATE NOCASE, ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (CAST(1 AS INT), 1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 2",
        ),
        (
            "VALUES (1 + (SELECT 1), ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (2, 1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 2",
        ),
        (
            "VALUES (1, 0), ((SELECT 2), ?1) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "SELECT 1, 0 UNION ALL SELECT 2, 1 UNION ALL SELECT 3, 3 ORDER BY 2",
        ),
        (
            "VALUES (CAST(1 AS INT), 0), (2, ?1), ((SELECT 3), 4) UNION ALL SELECT 5, 5 ORDER BY ?1",
            vec![B::Int(1)],
            "SELECT 1, 0 UNION ALL SELECT 2, 1 UNION ALL SELECT 3, 4 UNION ALL SELECT 5, 5 ORDER BY 2",
        ),
        (
            "VALUES (1, 0), (2, ?1), ((SELECT 3), 4), (5, ?1) UNION ALL SELECT 6, 6 ORDER BY ?1",
            vec![B::Int(2)],
            "SELECT 1, 0 UNION ALL SELECT 2, 2 UNION ALL SELECT 3, 4 UNION ALL SELECT 5, 2 \
             UNION ALL SELECT 6, 6 ORDER BY 2",
        ),
        (
            "VALUES (1, 0), ((SELECT 3), 4), (?1, 5) UNION ALL SELECT 7, 7 ORDER BY ?1",
            vec![B::Int(2)],
            "SELECT 1, 0 UNION ALL SELECT 3, 4 UNION ALL SELECT 2, 5 UNION ALL SELECT 7, 7 ORDER BY 1",
        ),
        (
            "SELECT 1, 2 UNION ALL VALUES (?1, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(0)],
            "SELECT 1, 2 UNION ALL VALUES (0, 0) UNION ALL SELECT 3, 3 ORDER BY 1",
        ),
        // ... and nothing else does: these are the pin's "does not
        // match any column" errors.
        // Regression: rows sharing a co-routine (hipp matched each
        // row of a multi-row VALUES).
        (
            "VALUES (1, ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (1, 1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "VALUES (2, 0), (1, ?1) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (2, 0), (1, 1) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "VALUES (+CAST(1 AS INT), ?1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (1, 1), (2, 0) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "VALUES (1, 0), ('a' LIKE 'b', ?1) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (1, 0), (0, 1) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "VALUES (CAST(1 AS INT), 0), (2, ?1), (3, 4) UNION ALL SELECT 5, 5 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (1, 0), (2, 1), (3, 4) UNION ALL SELECT 5, 5 ORDER BY 'nomatch'",
        ),
        (
            "VALUES (1, 0), ((SELECT 3), 4), (?1, 5), (6, 6) UNION ALL SELECT 7, 7 ORDER BY ?1",
            vec![B::Int(1)],
            "VALUES (1, 0), (3, 4), (1, 5), (6, 6) UNION ALL SELECT 7, 7 ORDER BY 'nomatch'",
        ),
        // A multi-row VALUES right of an operator is a subquery.
        (
            "SELECT 1, 2 UNION ALL VALUES (?1, 0), (4, 4) UNION ALL SELECT 3, 3 ORDER BY ?1",
            vec![B::Int(1)],
            "SELECT 1, 2 UNION ALL VALUES (1, 0), (4, 4) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "SELECT 1, 2 UNION ALL VALUES (CAST(4 AS INT), 4), (?1, 0) UNION ALL SELECT 3, 3 \
             ORDER BY ?1",
            vec![B::Int(1)],
            "SELECT 1, 2 UNION ALL VALUES (4, 4), (1, 0) UNION ALL SELECT 3, 3 ORDER BY 'nomatch'",
        ),
        (
            "SELECT :a UNION ALL SELECT 2 ORDER BY ?1",
            vec![B::Int(5)],
            "SELECT 5 UNION ALL SELECT 2 ORDER BY 'nomatch'",
        ),
        (
            "SELECT ? UNION ALL SELECT 2 ORDER BY ?",
            vec![B::Int(1), B::Int(1)],
            "SELECT 1 UNION ALL SELECT 2 ORDER BY 'nomatch'",
        ),
        (
            "SELECT ?1 UNION ALL SELECT 2 ORDER BY +?1",
            vec![B::Int(1)],
            "SELECT 1 UNION ALL SELECT 2 ORDER BY 'nomatch'",
        ),
        (
            "SELECT 3 UNION SELECT 1 ORDER BY ?",
            vec![B::Int(1)],
            "SELECT 3 UNION SELECT 1 ORDER BY 'nomatch'",
        ),
    ] {
        tw.case_with(sql, &vals, ora);
    }
}

#[cfg(feature = "sql")]
#[test]
fn errors_and_deferred_value_errors() {
    let tw = twins(
        "err",
        "CREATE TABLE nn(id INTEGER PRIMARY KEY, a INTEGER NOT NULL CHECK (a < 100), u TEXT UNIQUE);
         CREATE TABLE st(a INTEGER, b TEXT) STRICT;
         INSERT INTO nn VALUES (1, 1, 'x')",
    );
    for (sql, vals) in [
        ("INSERT INTO nn (a) VALUES (?)", vec![B::Null]),
        ("INSERT INTO nn (a) VALUES (?)", vec![B::Int(5)]),
        ("INSERT INTO nn (a) VALUES (?)", vec![B::Int(500)]),
        ("INSERT INTO nn (a) VALUES (?)", vec![B::Text("50".into())]),
        (
            "INSERT INTO nn (a, u) VALUES (1, ?)",
            vec![B::Text("x".into())],
        ),
        (
            "INSERT OR IGNORE INTO nn (a, u) VALUES (?, ?)",
            vec![B::Null, B::Text("y".into())],
        ),
        (
            "INSERT INTO nn (id, a) VALUES (?, 1)",
            vec![B::Text("abc".into())],
        ),
        ("INSERT INTO nn (id, a) VALUES (?, 1)", vec![B::Real(9.0)]),
        ("UPDATE nn SET a = ? WHERE id = 1", vec![B::Null]),
        ("UPDATE nn SET a = ? WHERE id = 1", vec![B::Int(1000)]),
        (
            "INSERT INTO st VALUES (?, ?)",
            vec![B::Text("5".into()), B::Int(5)],
        ),
        (
            "INSERT INTO st VALUES (?, ?)",
            vec![B::Text("x".into()), B::Int(5)],
        ),
        ("SELECT 1 LIMIT ?", vec![B::Real(2.5)]),
        ("SELECT abs(?)", vec![B::Int(i64::MIN)]),
        ("SELECT 'a' LIKE 'a' ESCAPE ?", vec![B::Text("ab".into())]),
    ] {
        tw.case(sql, &vals);
    }
    tw.same_files("SELECT id, quote(a), quote(u) FROM nn;");
    tw.same_files("SELECT quote(a), quote(b) FROM st;");
    block_on(async {
        let mut db = open(&tw.hipp).await;
        // A value error is deferred to step: the NULL default
        // fails NOT NULL, the bound value does not.
        let mut st = db
            .prepare("INSERT INTO nn (a) VALUES (?)")
            .await
            .expect("prepare defers the NULL-binding error");
        assert!(matches!(st.step().await, Err(Error::Schema { .. })));
        st.reset();
        st.bind_i64(1, 9).expect("bind");
        assert_eq!(st.step().await.expect("insert"), Step::Done);
        drop(st);
        // Name errors are not deferred.
        assert!(db.prepare("SELECT nope FROM nn WHERE a = ?").await.is_err());
        assert!(db.prepare("SELECT * FROM nope WHERE a = ?").await.is_err());
        // Regression: only value errors are deferred; any other
        // planning error is prepare's, as the pin's sqlite3_prepare
        // reports it (CREATE TABLE ... AS SELECT onto an existing
        // table used to fail only at step).
        for (sql, want) in [
            (
                "CREATE TABLE nn AS SELECT ?",
                hipp::SchemaKind::DuplicateTable,
            ),
            (
                "SELECT ? UNION SELECT 1, 2",
                hipp::SchemaKind::ColumnCountMismatch,
            ),
            (
                "CREATE TABLE nn AS SELECT nope",
                hipp::SchemaKind::DuplicateTable,
            ),
        ] {
            let got = db.prepare(sql).await.err();
            assert!(
                matches!(got, Some(Error::Schema { kind }) if kind == want),
                "{sql}: {got:?}"
            );
        }
        // Under IF NOT EXISTS a taken name makes the statement a
        // no-op before its SELECT is resolved (the pin's
        // sqlite3StartTable; hipp used to refuse the SELECT).
        let mut st = db
            .prepare("CREATE TABLE IF NOT EXISTS nn AS SELECT nope, ?")
            .await
            .expect("a no-op");
        assert_eq!(st.step().await.expect("no-op"), Step::Done);
        drop(st);
        // Parameters in stored definitions are refused (probed:
        // the pin's views / CHECK / DEFAULT / generated errors).
        for sql in [
            "CREATE VIEW v AS SELECT ?",
            "CREATE VIEW v AS SELECT a FROM nn WHERE a = :a",
            "CREATE TABLE c1(a CHECK (a > ?))",
            "CREATE TABLE c2(a DEFAULT (?))",
            "CREATE TABLE c4(a, b AS (a + ?))",
            "ALTER TABLE nn ADD COLUMN z DEFAULT (?)",
            "ALTER TABLE nn ADD COLUMN z CHECK (z > ?)",
        ] {
            assert!(
                matches!(
                    db.prepare(sql).await.err(),
                    Some(Error::Schema {
                        kind: hipp::SchemaKind::ParameterNotAllowed
                    })
                ),
                "{sql}"
            );
        }
        // `DEFAULT ?` is a syntax error on the pin.
        assert!(matches!(
            db.prepare("CREATE TABLE c3(a DEFAULT ?)").await.err(),
            Some(Error::Parse { .. })
        ));
    });
}

#[cfg(feature = "sql")]
#[test]
fn re_execution_follows_schema_changes() {
    let tw = twins("schema", "CREATE TABLE a(x); CREATE TABLE b(x)");
    block_on(async {
        let mut db = open(&tw.hipp).await;
        // DROP TABLE re-executed: the second run is "no such table".
        let mut st = db.prepare("DROP TABLE a").await.expect("prepare");
        assert_eq!(st.step().await.expect("drop"), Step::Done);
        st.reset();
        assert!(matches!(
            st.step().await,
            Err(Error::Schema {
                kind: hipp::SchemaKind::UnknownTable
            })
        ));
        drop(st);
        // CREATE TABLE ... AS SELECT re-executed: "already exists".
        let mut st = db
            .prepare("CREATE TABLE c AS SELECT ? AS v")
            .await
            .expect("ctas");
        st.bind_text(1, "one").expect("bind");
        assert_eq!(st.step().await.expect("ctas"), Step::Done);
        st.reset();
        assert!(st.step().await.is_err());
        drop(st);
        // A prepared query sees rows written between executions
        // by itself (a prepared write) — and a ROLLBACK.
        let mut st = db
            .prepare("INSERT INTO b VALUES (?) RETURNING x")
            .await
            .expect("prepare");
        for k in 0..3 {
            st.bind_i64(1, k).expect("bind");
            assert_eq!(st.step().await.expect("row"), Step::Row);
            assert_eq!(st.column_value(0), Some(Value::Integer(k)));
            assert_eq!(st.step().await.expect("done"), Step::Done);
            st.reset();
        }
    });
    assert!(
        oracle_raw(
            &tw.oracle,
            "DROP TABLE a; CREATE TABLE c AS SELECT 'one' AS v; INSERT INTO b VALUES (0), (1), (2);"
        )
        .0
    );
    tw.same_files("SELECT name, sql FROM sqlite_schema ORDER BY name;");
    tw.same_files("SELECT * FROM b; SELECT * FROM c;");
}

#[cfg(feature = "sql")]
#[test]
fn generated_expressions_with_random_bindings() {
    let tw = twins("prng", SETUP);
    let mut rng = Rng(0x7e57_0003);
    let ops = [
        "+", "-", "*", "/", "%", "||", "=", "<", "<>", "IS", "IS NOT", "AND", "OR",
    ];
    let cols = ["i", "tx", "n", "r", "b", "x", "?", "?1", ":p"];
    for _ in 0..400 {
        let a = cols[rng.below(cols.len() as u64) as usize];
        let c = cols[rng.below(cols.len() as u64) as usize];
        let op = ops[rng.below(ops.len() as u64) as usize];
        let shape = rng.below(5);
        let sql = match shape {
            0 => format!("SELECT {a} {op} {c} FROM t"),
            1 => format!("SELECT count(*) FROM t WHERE {a} {op} {c}"),
            2 => format!("SELECT typeof({a} {op} {c}), {a} FROM t ORDER BY rowid"),
            3 => format!("SELECT max({a}), min({c}) FROM t WHERE {a} IS NOT NULL"),
            _ => format!("SELECT DISTINCT {a} FROM t WHERE {c} {op} ? ORDER BY 1"),
        };
        let (_, count) = number(&sql);
        let vals: Vec<B> = (0..count).map(|_| rand_value(&mut rng)).collect();
        tw.case(&sql, &vals);
    }
}

/// Found by the generated corpus (ungated fixes on the v0.4
/// evaluator, with or without parameters): `%` with a REAL or
/// real-looking operand is a REAL computed on the operands'
/// integer values (a text operand's integer *prefix*), CAST(text
/// AS INTEGER) reads only the integer prefix, and CAST(text AS
/// NUMERIC) is an INTEGER only for an integer that fits or an
/// integral value below 2^51.
#[cfg(feature = "sql")]
#[test]
fn cast_and_remainder_follow_the_pin() {
    let tw = twins("cast", "CREATE TABLE z(a)");
    let texts = [
        "1e18",
        "1e15",
        "5",
        "5.0",
        "5.",
        ".5",
        "12abc",
        " 7 ",
        "1.5e3x",
        "0x10",
        "",
        "abc",
        "-0.0",
        "+5",
        "- 5",
        "0009",
        "1e",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "-9223372036854775809",
        "99999999999999999999",
        "2251799813685248.0",
        "2251799813685247.0",
        "-2251799813685248.0",
        "-2251799813685247.5",
        "9223372036854775808.0",
        "1e400",
        " 1e5 ",
    ];
    for t in texts {
        let v = [B::Text(t.into())];
        for sql in [
            "SELECT typeof(CAST(?1 AS INTEGER)), CAST(?1 AS INTEGER)",
            "SELECT typeof(CAST(?1 AS NUMERIC)), CAST(?1 AS NUMERIC)",
            "SELECT typeof(CAST(?1 AS REAL)), CAST(?1 AS REAL)",
            "SELECT typeof(?1 % 2), ?1 % 2, ?1 % 2.0, 7 % ?1, 7.5 % ?1",
            // Regression: substr() and round() read their integer
            // arguments as the pin's sqlite3_value_int64 does (the
            // integer prefix: '1e1' is 1), not as a full number.
            "SELECT substr('abcdefghijklmnopqrstuvwxyz', ?1), substr('abcdefghijkl', 2, ?1)",
            "SELECT substr('abcdefghijkl', -2, ?1), round(1.23456789, ?1), round(?1), round(?1, 1)",
        ] {
            assert!(tw.case(sql, &v).is_none());
        }
        let blob = [B::Blob(t.as_bytes().to_vec())];
        assert!(
            tw.case(
                "SELECT CAST(?1 AS INTEGER), CAST(?1 AS NUMERIC), ?1 % 3",
                &blob
            )
            .is_none()
        );
    }
    for sql in [
        "SELECT 5.5 % 2, 5 % 2.0, -7.5 % 2, 7 % -2.5, 9e999 % 7, -9e999 % 7, 5 % 0.5, 5.0 % 0",
        "SELECT typeof(5 % 2.0), typeof(5 % 2), 'abc' % 2.0, -9223372036854775808 % -1.0",
        // Regression: substr() coordinates saturate at the i64
        // bounds (hipp's arithmetic overflowed and panicked).
        "SELECT substr('abcdefghijkl', -2, 9223372036854775807), \
         substr('abcdefghijkl', 2, -9223372036854775808), \
         substr('abcdefghijkl', -9223372036854775808, 9223372036854775807), \
         substr('abcdefghijkl', 9223372036854775807, -9223372036854775807), \
         substr('abcdef', -1, -9223372036854775807)",
    ] {
        assert!(tw.case(sql, &[]).is_none());
    }
}

/// Regression: round() clamps its digit count to 0..=30 (a huge
/// count used to loop for as many iterations) and rounds the exact
/// decimal expansion as the pin's `%!.*f` does (hipp scaled by a
/// power of ten, which overflowed past 18 digits and misrounded
/// values such as 2.675 at 2 places).
#[cfg(feature = "sql")]
#[test]
fn round_follows_the_pin() {
    let tw = twins("round", "CREATE TABLE z(a)");
    for (x, n) in [
        (B::Real(2.675), B::Int(2)),
        (B::Real(1.5), B::Int(100)),
        (B::Real(1.5), B::Int(1_000_000_000_000)),
        (B::Real(1.5), B::Int(i64::MAX)),
        (B::Real(1.23456), B::Int(4_294_967_298)),
        (B::Real(1.23456), B::Int(-4_294_967_294)),
        (B::Real(1e300), B::Int(2)),
        (B::Real(5e-301), B::Int(300)),
        (B::Real(0.1), B::Int(30)),
        (B::Real(123.456), B::Int(20)),
        (B::Real(-0.001), B::Int(2)),
        (B::Real(-2.5), B::Int(0)),
        (B::Real(4503599627370497.0), B::Int(0)),
        (B::Real(0.49999999999999994), B::Int(0)),
        (B::Int(7), B::Int(3)),
        (B::Text("1e1".into()), B::Text("1e1".into())),
        (B::Real(1.23456), B::Real(2.9)),
        (B::Null, B::Int(1)),
        (B::Real(1.0), B::Null),
    ] {
        assert!(tw.case("SELECT round(?1, ?2)", &[x.clone(), n]).is_none());
        assert!(tw.case("SELECT round(?1)", &[x]).is_none());
    }
    // Random short decimals and random magnitudes at every count.
    let mut rng = Rng(0x0dd5_eed5);
    for _ in 0..300 {
        let digits = (rng.next() % 7) as i32;
        let mag = (rng.next() % 2_000_000) as f64 / 1000.0 - 1000.0;
        let x = format!("{mag:.*}", digits as usize)
            .parse::<f64>()
            .unwrap_or(0.0);
        let n = (rng.next() % 32) as i64;
        assert!(
            tw.case("SELECT round(?1, ?2)", &[B::Real(x), B::Int(n)])
                .is_none()
        );
        let scale = 10f64.powi((rng.next() % 30) as i32 - 15);
        let y = (rng.next() % 1_000_000) as f64 / 7.0 * scale;
        assert!(
            tw.case("SELECT round(?1, ?2)", &[B::Real(y), B::Int(n)])
                .is_none()
        );
    }
}
