//! v0.16 math functions (leaf feature `math`), differentially vs
//! the pinned oracle (SQLite 3.54.1, built with
//! SQLITE_ENABLE_MATH_FUNCTIONS over glibc's libm). The semantics
//! were probed on the pin first (STATUS.md, "Math functions"):
//!
//! - An argument is a number only if `sqlite3_value_numeric_type`
//!   makes it one: INTEGER, REAL, or TEXT that is entirely a number
//!   (to its first NUL, ASCII spaces around it); NULL, BLOB and
//!   other text make the result NULL — except `log(B, X)`'s X,
//!   read as `sqlite3_value_double` (a blob's or text's numeric
//!   prefix).
//! - `ceil` / `ceiling` / `floor` / `trunc` keep an INTEGER (also
//!   integer TEXT); everything else is REAL; `pi()` takes no
//!   argument.
//! - A NaN result is NULL (`sqrt(-1)`, `acos(2)`, `mod(1, 0)`,
//!   `sin(1e400)`, `pow(-8, 1.0/3)`); infinities stay (`exp(1000)`
//!   is Inf); the logarithms are NULL at or below zero, `log(B, X)`
//!   also for B at or below 1, and is `ln(X) / ln(B)` in double
//!   (`log(10, 1000)` is 2.9999999999999996).
//! - `degrees(x)` is `x * (180 / M_PI)`, `radians(x)` is `x *
//!   (M_PI / 180)` (fitted over 4,000 random doubles each).
//!
//! The numeric results are compared as IEEE bit patterns
//! (`hex(ieee754_to_blob(...))` on the pin). hipp's library is
//! correctly rounded (`sqrt`, `mod`, `ceil` / `floor` / `trunc`
//! exactly; the rest with double-double evaluation); glibc is not
//! everywhere, so a measured small fraction of results differ by
//! one or two units in the last place. The corpus test reports the
//! mismatch rate per function and fails above the bound STATUS.md
//! documents.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, MemStorage, Step, Value};

type HErr = Error<<FileStorage as hipp::Storage>::Error>;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

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

fn oracle_raw(path: &std::path::Path, sql: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// The oracle over stdin (for scripts too long for one argument).
fn oracle_stdin(sql: &str) -> Vec<String> {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(sql.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("oracle finishes");
    assert!(
        out.stderr.is_empty(),
        "oracle stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16math-{tag}-h"));
    let oracle = common::temp_db(&format!("v16math-{tag}-o"));
    common::cli(&oracle, if setup.is_empty() { "SELECT 1;" } else { setup });
    block_on(async {
        let mut db = open(&hipp).await;
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                try_collect(&mut db, stmt).await.expect("setup");
            }
        }
    });
    Twins { hipp, oracle }
}

/// Two rendered rows equal, or equal but for REAL fields one or
/// two ulps apart (glibc's own rounding; see the corpus test).
fn rows_close(got: &[String], want: &[String]) -> bool {
    got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| {
            let (gs, ws): (Vec<&str>, Vec<&str>) = (g.split('|').collect(), w.split('|').collect());
            gs.len() == ws.len()
                && gs.iter().zip(&ws).all(|(a, b)| {
                    a == b
                        || match (a.parse::<f64>(), b.parse::<f64>()) {
                            (Ok(x), Ok(y)) => {
                                x.is_finite()
                                    && y.is_finite()
                                    && x != 0.0
                                    && (x > 0.0) == (y > 0.0)
                                    && x.to_bits().abs_diff(y.to_bits()) <= 2
                            }
                            _ => false,
                        }
                })
        })
}

impl Twins {
    /// `same`, accepting REAL results within two ulps.
    fn close(&self, sql: &str) {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        })
        .unwrap_or_else(|e| panic!("hipp refused {sql}: {e:?}"));
        let (ok, want) = oracle_raw(&self.oracle, sql);
        assert!(ok, "oracle refused {sql}");
        assert!(
            rows_close(&got, &want),
            "statement: {sql}\nhipp: {got:?}\npin:  {want:?}"
        );
    }

    /// One statement on both twins: equal rows, or both refuse it.
    fn same(&self, sql: &str) -> Option<HErr> {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        });
        let (ok, want) = oracle_raw(&self.oracle, sql);
        match got {
            Ok(rows) => {
                assert!(ok, "oracle refused what hipp ran: {sql}\nhipp: {rows:?}");
                assert_eq!(rows, want, "statement: {sql}");
                None
            }
            Err(e) => {
                assert!(
                    !ok,
                    "hipp refused what the oracle ran: {sql}\nhipp: {e:?}\noracle: {want:?}"
                );
                Some(e)
            }
        }
    }

    fn all(&self, sqls: &[&str]) {
        for sql in sqls {
            self.same(sql);
        }
    }
}

/// xorshift64: a hand-rolled deterministic PRNG.
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
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    /// e^U(lo, hi) with a random sign.
    fn logscale(&mut self, lo: f64, hi: f64) -> f64 {
        let m = self.range(lo, hi).exp();
        if self.next() & 1 == 1 { -m } else { m }
    }
    /// A random finite bit pattern.
    fn bits(&mut self) -> f64 {
        loop {
            let x = f64::from_bits(self.next());
            if x.is_finite() {
                return x;
            }
        }
    }
}

/// An SQL literal for a double that both parsers read back
/// exactly (shortest round-trip digits; infinities as 1e400).
fn lit(x: f64) -> String {
    if x == f64::INFINITY {
        "1e400".into()
    } else if x == f64::NEG_INFINITY {
        "-1e400".into()
    } else {
        format!("{x:e}")
    }
}

/// The value `sql` computes for each `(x, y)` row, as IEEE bits
/// (or "NULL"), on hipp and on the pin. `sql` names the columns
/// `x` and `y`. The pin also echoes `x` / `y` so a literal that
/// parses differently fails loudly.
fn bits_both(sql: &str, rows: &[(f64, f64)]) -> (Vec<String>, Vec<String>) {
    let ins: Vec<String> = rows
        .chunks(100)
        .map(|ch| {
            let v: Vec<String> = ch
                .iter()
                .map(|(x, y)| format!("({},{})", lit(*x), lit(*y)))
                .collect();
            format!("INSERT INTO c(x, y) VALUES {}", v.join(","))
        })
        .collect();
    let hex = |e: &str| format!("coalesce(nullif(hex(ieee754_to_blob({e})),''),'NULL')");
    let script = format!(
        "CREATE TABLE c(x, y);\n{};\nSELECT {}, {}, {} FROM c ORDER BY rowid;\n",
        ins.join(";\n"),
        hex("x"),
        hex("y"),
        hex(sql)
    );
    let pin = oracle_stdin(&script);
    let mut want = Vec::new();
    for (line, (x, y)) in pin.iter().zip(rows) {
        let p: Vec<&str> = line.split('|').collect();
        assert_eq!(p.len(), 3, "pin line {line}");
        assert_eq!(p[0], format!("{:016X}", x.to_bits()), "literal {}", lit(*x));
        assert_eq!(p[1], format!("{:016X}", y.to_bits()), "literal {}", lit(*y));
        want.push(p[2].to_string());
    }
    assert_eq!(want.len(), rows.len(), "pin rows for {sql}");
    let got = block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        for s in std::iter::once("CREATE TABLE c(x, y)").chain(ins.iter().map(String::as_str)) {
            let mut q = db.prepare(s).await.expect("prepare");
            while q.step().await.expect("step") == Step::Row {}
        }
        let mut q = db
            .prepare(&format!("SELECT {sql} FROM c ORDER BY rowid"))
            .await
            .expect("prepare");
        let mut out = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            out.push(match q.column_value(0) {
                Some(Value::Real(f)) => format!("{:016X}", f.to_bits()),
                Some(Value::Null) | None => "NULL".into(),
                other => format!("{other:?}"),
            });
        }
        out
    });
    (got, want)
}

/// Units in the last place between two bit patterns of equal sign.
fn ulps(a: &str, b: &str) -> Option<u64> {
    let a = u64::from_str_radix(a, 16).ok()?;
    let b = u64::from_str_radix(b, 16).ok()?;
    // Zeros, infinities and signs must agree exactly.
    let special = |v: u64| v << 1 == 0 || (v << 1) >> 53 == 0x7ff;
    if (a >> 63) != (b >> 63) || special(a) || special(b) {
        return None;
    }
    Some(a.abs_diff(b))
}

/// One function's corpus: (got, want) compared; returns the number
/// of mismatches after checking each is a finite ulp difference
/// within `max_ulp` (NULL-ness, infinities and zero signs must
/// agree exactly).
fn compare(
    name: &str,
    rows: &[(f64, f64)],
    got: &[String],
    want: &[String],
    max_ulp: u64,
) -> usize {
    assert_eq!(got.len(), want.len(), "{name}: row counts");
    let mut bad = 0;
    for ((g, w), (x, y)) in got.iter().zip(want).zip(rows) {
        if g == w {
            continue;
        }
        bad += 1;
        let d = ulps(g, w);
        assert!(
            d.is_some_and(|d| d <= max_ulp),
            "{name}({x:e}, {y:e}): hipp {g} pin {w} (ulps {d:?})"
        );
    }
    bad
}

type Gen = fn(&mut Rng) -> (f64, f64);

/// The generated corpus: per function, its argument generator, the
/// largest ulp difference accepted, and the mismatch bound (out of
/// `N`) — 0 for the exact functions; for the rest, a few times the
/// rate measured over 10^6 arguments (STATUS.md), which is glibc's
/// own deviation from correct rounding.
const N: usize = 1500;

fn corpus() -> Vec<(&'static str, Gen, u64, usize)> {
    fn mixed(r: &mut Rng, lo: f64, hi: f64) -> f64 {
        match r.next() % 4 {
            0 => r.bits(),
            1 => r.logscale(-40.0, 5.0),
            _ => r.range(lo, hi),
        }
    }
    vec![
        ("sqrt(x)", |r| (mixed(r, 0.0, 1e6).abs(), 0.0), 0, 0),
        ("ceil(x)", |r| (mixed(r, -1e6, 1e6), 0.0), 0, 0),
        ("floor(x)", |r| (mixed(r, -1e6, 1e6), 0.0), 0, 0),
        ("trunc(x)", |r| (mixed(r, -1e6, 1e6), 0.0), 0, 0),
        ("degrees(x)", |r| (mixed(r, -1e3, 1e3), 0.0), 0, 0),
        ("radians(x)", |r| (mixed(r, -1e5, 1e5), 0.0), 0, 0),
        (
            "mod(x, y)",
            |r| (mixed(r, -1e6, 1e6), mixed(r, -100.0, 100.0)),
            0,
            0,
        ),
        ("exp(x)", |r| (r.range(-745.5, 710.0), 0.0), 1, 6),
        ("ln(x)", |r| (mixed(r, 0.0, 100.0).abs(), 0.0), 1, 6),
        ("log(x)", |r| (mixed(r, 0.0, 100.0).abs(), 0.0), 2, 6),
        ("log10(x)", |r| (mixed(r, 0.0, 100.0).abs(), 0.0), 2, 6),
        ("log2(x)", |r| (mixed(r, 0.0, 100.0).abs(), 0.0), 1, 6),
        (
            "log(x, y)",
            |r| (mixed(r, 0.0, 30.0).abs(), mixed(r, 0.0, 1e4).abs()),
            2,
            12,
        ),
        (
            "pow(x, y)",
            |r| (r.range(0.0, 20.0), r.range(-30.0, 30.0)),
            1,
            6,
        ),
        (
            "power(-x, y)",
            |r| (r.range(0.0, 20.0), r.range(-20.0, 20.0).round()),
            1,
            6,
        ),
        ("sin(x)", |r| (mixed(r, -100.0, 100.0), 0.0), 1, 8),
        ("cos(x)", |r| (mixed(r, -100.0, 100.0), 0.0), 1, 8),
        ("tan(x)", |r| (mixed(r, -100.0, 100.0), 0.0), 1, 10),
        ("asin(x)", |r| (r.range(-1.0, 1.0), 0.0), 1, 8),
        ("acos(x)", |r| (r.range(-1.0, 1.0), 0.0), 1, 8),
        ("atan(x)", |r| (mixed(r, -10.0, 10.0), 0.0), 1, 8),
        (
            "atan2(x, y)",
            |r| (mixed(r, -10.0, 10.0), mixed(r, -10.0, 10.0)),
            1,
            8,
        ),
        ("sinh(x)", |r| (r.range(-30.0, 30.0), 0.0), 2, 260),
        ("cosh(x)", |r| (r.range(-30.0, 30.0), 0.0), 1, 20),
        ("tanh(x)", |r| (r.range(-3.0, 3.0), 0.0), 3, 400),
        ("asinh(x)", |r| (mixed(r, -10.0, 10.0), 0.0), 1, 300),
        (
            "acosh(x)",
            |r| (1.0 + mixed(r, 0.0, 50.0).abs(), 0.0),
            1,
            30,
        ),
        ("atanh(x)", |r| (r.range(-1.0, 1.0), 0.0), 1, 300),
    ]
}

#[test]
fn generated_corpus_matches_pin_bits() {
    let mut rng = Rng(0x005e_ed0f_3a7b);
    let mut report = Vec::new();
    for (sql, gen_, max_ulp, bound) in corpus() {
        let rows: Vec<(f64, f64)> = (0..N).map(|_| gen_(&mut rng)).collect();
        // power(-x, y): negative bases with integer exponents.
        let (got, want) = bits_both(sql, &rows);
        let bad = compare(sql, &rows, &got, &want, max_ulp);
        report.push(format!("{sql}: {bad}/{N}"));
        assert!(
            bad <= bound,
            "{sql}: {bad} of {N} results differ from the pin (bound {bound})"
        );
    }
    eprintln!("math mismatch rates vs the pin: {}", report.join(", "));
}

/// Structural edge cases must agree bit for bit: zeros and their
/// signs, subnormals, the largest doubles, infinities, every
/// NaN-producing argument (NULL), domain boundaries, overflow and
/// underflow thresholds.
#[test]
fn edge_cases_match_pin_bits() {
    const SUB: f64 = 5e-324;
    const MIN: f64 = 2.2250738585072014e-308;
    const MAX: f64 = f64::MAX;
    const INF: f64 = f64::INFINITY;
    let common1: Vec<f64> = vec![
        0.0, -0.0, SUB, -SUB, 1e-310, -1e-310, MIN, -MIN, 1e-300, MAX, -MAX, INF, -INF, 1.0, -1.0,
        0.5, -0.5, 2.0, -2.0, 1e-20, -1e-20, 1e300, -1e300,
    ];
    let cases: Vec<(&str, Vec<(f64, f64)>)> = vec![
        (
            "sqrt(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(4.0, 0.0), (2.0, 0.0)])
                .collect(),
        ),
        (
            "ceil(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(-0.5, 0.0), (4.5e15, 0.0)])
                .collect(),
        ),
        (
            "floor(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(0.5, 0.0), (-4.5e15, 0.0)])
                .collect(),
        ),
        (
            "trunc(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(-0.9, 0.0), (2.5, 0.0)])
                .collect(),
        ),
        (
            "exp(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain(
                    [
                        709.78,
                        709.79,
                        709.782712893384,
                        -708.4,
                        -745.13,
                        -745.14,
                        -746.0,
                        1000.0,
                    ]
                    .map(|x| (x, 0.0)),
                )
                .collect(),
        ),
        (
            "ln(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(std::f64::consts::E, 0.0)])
                .collect(),
        ),
        (
            "log10(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain((0..23).map(|k| (10f64.powi(k), 0.0)))
                .collect(),
        ),
        (
            "log2(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([
                    (1024.0, 0.0),
                    (2f64.powi(-1074), 0.0),
                    (2f64.powi(-1022), 0.0),
                ])
                .collect(),
        ),
        ("sin(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("cos(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("tan(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("asin(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("acos(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("atan(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        (
            "sinh(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([710.0, 710.47, 710.48, 711.0, -22.0, 22.0].map(|x| (x, 0.0)))
                .collect(),
        ),
        (
            "cosh(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([710.0, 710.47, 710.48, 711.0, 22.0].map(|x| (x, 0.0)))
                .collect(),
        ),
        (
            "tanh(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(22.0, 0.0), (-23.0, 0.0)])
                .collect(),
        ),
        ("asinh(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        (
            "acosh(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(1.5, 0.0)])
                .collect(),
        ),
        (
            "atanh(x)",
            common1
                .iter()
                .map(|x| (*x, 0.0))
                .chain([(0.9, 0.0)])
                .collect(),
        ),
        ("degrees(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
        ("radians(x)", common1.iter().map(|x| (*x, 0.0)).collect()),
    ];
    // Finite nonzero results may differ where glibc is not
    // correctly rounded (e.g. atanh(0.5): glibc's log1p(2) is one
    // ulp low); at most two such cases per function, one ulp each.
    let mut report = Vec::new();
    for (sql, rows) in &cases {
        let (got, want) = bits_both(sql, rows);
        let bad = compare(sql, rows, &got, &want, 1);
        assert!(bad <= 2, "{sql}: {bad} edge cases differ");
        if bad > 0 {
            report.push(format!("{sql}: {bad}"));
        }
    }
    // Two-argument special values: the C99 Annex F table for pow,
    // atan2's signed zeros and infinities, fmod's NaN cases.
    let specials = [
        0.0, -0.0, 1.0, -1.0, 2.0, -2.0, 0.5, -0.5, 3.0, -3.0, 1.5, INF, -INF, SUB, MAX, 1e-300,
    ];
    let pairs: Vec<(f64, f64)> = specials
        .iter()
        .flat_map(|x| specials.iter().map(move |y| (*x, *y)))
        .chain([
            (2.0, 1023.0),
            (2.0, 1024.0),
            (2.0, -1074.0),
            (2.0, -1075.0),
            (4.0, -537.5),
            (4.0, 0.5),
            (-8.0, 1.0 / 3.0),
            (-1.0, 1e300),
            (134217727.0, 2.0),
            (10.0, 22.0),
            (10.0, -5.0),
            (3.0, 33.0),
            (-3.0, 33.0),
            (1.0000001, 1e10),
        ])
        .collect();
    for sql in ["pow(x, y)", "atan2(x, y)", "mod(x, y)"] {
        let (got, want) = bits_both(sql, &pairs);
        let bad = compare(sql, &pairs, &got, &want, 1);
        assert!(bad <= 2, "{sql}: {bad} special pairs differ");
        if bad > 0 {
            report.push(format!("{sql}: {bad}"));
        }
    }
    eprintln!("edge cases off by one ulp: {report:?}");
}

/// Argument coercion over every storage class, result types and
/// NULL handling, compared as rendered rows (typeof + value).
#[test]
fn coercion_and_types_match_pin() {
    let setup = "CREATE TABLE v(a); INSERT INTO v VALUES (NULL), (0), (1), (-1), (5), (-7), \
        (9223372036854775807), (-9223372036854775808), (9007199254740993), (0.0), (-0.0), (0.5), \
        (-0.5), (1.5), (-2.5), (2.0), (1e300), (-1e-300), (1e400), (-1e400), ('1.5'), (' 4 '), \
        ('4x'), (''), (' '), ('0x10'), ('1e400'), ('-0.0'), ('-0'), ('5.0'), ('+5'), ('.5'), \
        ('5.'), ('1e'), ('inf'), ('nan'), ('1_000'), ('9223372036854775808'), \
        ('-9223372036854775808'), (char(160) || '5'), ('5' || char(0) || 'x'), (char(9) || '3'), \
        ('3' || char(11)), (x'34'), (x''), (x'312e35'), ('abc'), ('2.0e0'), ('  -1.25e+2  ')";
    let tw = twins("coerce", setup);
    for f in [
        "acos", "acosh", "asin", "asinh", "atan", "atanh", "ceil", "ceiling", "cos", "cosh",
        "degrees", "exp", "floor", "ln", "log", "log10", "log2", "radians", "sin", "sinh", "sqrt",
        "tan", "tanh", "trunc",
    ] {
        tw.close(&format!(
            "SELECT typeof({f}(a)), {f}(a) FROM v ORDER BY rowid"
        ));
    }
    for f in ["atan2", "mod", "pow", "power", "log"] {
        tw.close(&format!(
            "SELECT typeof({f}(p.a, q.a)), {f}(p.a, q.a) FROM v p, v q \
             WHERE p.rowid % 3 = 0 OR q.rowid % 4 = 1 ORDER BY p.rowid, q.rowid"
        ));
    }
    tw.all(&[
        "SELECT pi(), typeof(pi()), pi(*), -pi(), PI(), Sin(1), CEILING(1.2), POWER(2, 3), LoG(2, 8)",
        "SELECT ceil(5), typeof(ceil(5)), floor(-5), trunc(7), typeof(trunc('7')), typeof(trunc('7.0'))",
        "SELECT log(2, '8abc'), log(2, x'38'), log(2, x''), log(2, NULL), log(NULL, 8), log(1, 8), log(0.5, 8)",
        "SELECT log(10, 1000), log(3, 81), log(1.5, 2.25), log(8, 2), log(2, 1), log(2, 0)",
        "SELECT sin(DISTINCT 1), ceil(DISTINCT 1.5), max(sqrt(4), 1), sqrt(2) * sqrt(2), pow(2, 0.5) = sqrt(2)",
        "SELECT sum(sin(value)), avg(exp(value)), group_concat(round(ln(value), 3)) FROM (SELECT 1 value UNION SELECT 2 UNION SELECT 3)",
    ]);
}

/// Wrong argument counts are refused by both at prepare.
#[test]
fn arity_errors_agree() {
    let tw = twins("arity", "");
    for (f, bad) in [
        ("pi", &["1", "1, 2"][..]),
        ("sin", &["", "1, 2"]),
        ("sqrt", &["", "1, 2"]),
        ("ceil", &["", "1, 2"]),
        ("log", &["", "1, 2, 3"]),
        ("atan2", &["", "1", "1, 2, 3"]),
        ("mod", &["", "1", "1, 2, 3"]),
        ("pow", &["", "1"]),
        ("power", &["1, 2, 3"]),
        ("trunc", &["1, 2"]),
        ("degrees", &["", "1, 2"]),
    ] {
        for a in bad {
            let e = tw.same(&format!("SELECT {f}({a})"));
            assert!(e.is_some(), "{f}({a}) should be refused");
        }
    }
    assert!(tw.same("SELECT sin(*)").is_some());
    assert!(tw.same("SELECT sin(1) FILTER (WHERE 1)").is_some());
}

/// Math functions are deterministic: they work in generated
/// columns, CHECK constraints, DEFAULTs, WHERE / ORDER BY and
/// write paths, and the file stays valid for the pin.
#[test]
fn math_in_schema_and_writes() {
    let setup = "CREATE TABLE g(a, b AS (sqrt(a)), c AS (ceil(a)) STORED, \
        d CHECK (d IS NULL OR ln(d) < 3), e DEFAULT (pi() * 2)); \
        INSERT INTO g(a, d) VALUES (4, 1), (2.25, 10), (-1, NULL), ('9', 20)";
    let tw = twins("schema", setup);
    tw.all(&[
        "SELECT a, b, c, d, e, typeof(c) FROM g ORDER BY rowid",
        "SELECT a FROM g WHERE sqrt(a) > 1.4 ORDER BY sin(a)",
        "SELECT count(*) FROM g WHERE b IS NULL",
    ]);
    // A CHECK the value fails, on both.
    assert!(tw.same("INSERT INTO g(a, d) VALUES (1, 100)").is_some());
    let tw = twins(
        "writes",
        "CREATE TABLE w(x REAL, y); INSERT INTO w VALUES (1, 2), (3, 4)",
    );
    tw.same("UPDATE w SET y = pow(x, y) + mod(y, 3)");
    tw.same("SELECT x, y FROM w ORDER BY rowid");
    let (ok, _) = oracle_raw(&tw.hipp, "PRAGMA integrity_check");
    assert!(ok);
}

/// Regression (ungated, tiny-claimed path): text → number
/// conversion read Unicode whitespace as spaces and refused a
/// number followed by a NUL; the pin trims only ASCII spaces and
/// reads text up to its first NUL. Column affinity, comparison
/// affinity and the math functions share it.
#[test]
fn text_to_number_whitespace_and_nul() {
    let setup = "CREATE TABLE t(a NUMERIC, b INTEGER, c REAL); \
        INSERT INTO t VALUES (char(160) || '5', char(160) || '5', char(160) || '5'), \
        ('5' || char(133), '5' || char(133), '5' || char(133)), \
        ('5' || char(0) || 'x', '5' || char(0) || 'x', '5' || char(0) || 'x'), \
        (char(12) || '5', '5' || char(11), char(13) || '1.5' || char(9)), \
        ('1.5' || char(0) || 'zz', char(0) || '5', '9223372036854775808' || char(0))";
    let tw = twins("ttn", setup);
    tw.all(&[
        "SELECT typeof(a), typeof(b), typeof(c), hex(a), hex(b), hex(c) FROM t ORDER BY rowid",
        "SELECT a = 5, b = 5, c = 5.0, a > 4 FROM t ORDER BY rowid",
        "SELECT sin(a), sqrt(b), ceil(c) FROM t ORDER BY rowid",
    ]);
}
