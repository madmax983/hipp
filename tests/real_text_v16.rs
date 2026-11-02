//! v0.16: REAL-to-text conversion, bit for bit against the oracle.
//!
//! `record::render_real` (CAST AS TEXT, `||`, TEXT affinity, and
//! the CLI's list mode, which prints a REAL exactly as this
//! conversion does) must give the pin's string for every double.
//! The pin's digit generation is approximate and its choice
//! between a short and a 17-digit rendering is its own; both were
//! fitted clean-room from oracle output (STATUS.md, v0.16 "CI vs
//! the real pin"). This corpus pins the fit: named examples from
//! the CI run against the real 3.54.1 pin, plus a deterministic
//! spread of random bit patterns, short decimals and their
//! neighbours, integers near 2^53 / 2^63, powers of ten,
//! subnormals and halfway cases. `HIPP_REAL_CORPUS=<n>` scales the
//! generated part (default 4000) for local soak runs.
#![cfg(feature = "v04")]
// Values are spelled as the pin prints them (17 digits), on purpose.
#![allow(clippy::excessive_precision)]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

/// Values and the strings CI's real 3.54.1 pin printed for them
/// (golden, recorded from the CI run; the generated corpus below
/// compares against the live oracle instead).
const PIN_GOLDEN: &[(f64, &str)] = &[
    (1e15, "1000000000000000.0"),
    (4503599627370497.0, "4503599627370497.0"),
    (9223372036854775808.0, "9.2233720368547758e+18"),
    (5833858491548785.0, "5833858491548785.0"),
    (-6776496294506.125, "-6776496294506.125"),
    (0.000123456789012345, "0.00012345678901234499"),
    (999999999999999.5, "999999999999999.5"),
    (574.423710258671, "574.42371025867101"),
    (-589569.9865969187, "-589569.98659691867"),
    (-3.521289658049935e18, "-3.5212896580499349e+18"),
];

/// A tiny deterministic generator (no dependencies).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (i128::from(hi) - i128::from(lo) + 1) as u64;
        (i128::from(lo) + i128::from(self.below(span))) as i64
    }
}

fn step(v: f64, up: bool) -> f64 {
    if v == 0.0 {
        return if up {
            f64::from_bits(1)
        } else {
            -f64::from_bits(1)
        };
    }
    let b = v.to_bits();
    let away = (v > 0.0) == up;
    f64::from_bits(if away { b + 1 } else { b - 1 })
}

fn decimal(rng: &mut Rng, digits: u32, exp: i64) -> f64 {
    let lo = 10u64.pow(digits - 1);
    let m = lo + rng.below(lo * 9);
    format!("{m}e{exp}").parse().expect("decimal parses")
}

fn corpus(n: usize) -> Vec<f64> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut out: Vec<f64> = PIN_GOLDEN.iter().map(|(v, _)| *v).collect();
    out.extend([
        0.1,
        1.0 / 3.0,
        2.0 / 3.0,
        5e-324,
        2.2250738585072014e-308,
        1.7976931348623157e308,
        9.5e21,
        1e16,
        1e17,
        9.999999999999998e16,
        281474976710656.0625,
        2f64.powi(60),
        2f64.powi(63),
        0.5,
        1.5,
        2.5e-5,
        6.4352803473657501,
        8.5281530614695,
        1.2345678901234001,
        4.0690089522323998e-55,
        9.4174e-10,
        9.81388383e24,
    ]);
    while out.len() < n {
        let v = match rng.below(11) {
            0 => f64::from_bits(rng.next()),
            1 => f64::from_bits(rng.next() & ((1u64 << 54) - 1)),
            2 => {
                let d = rng.range(1, 17) as u32;
                let e = rng.range(-330, 290);
                decimal(&mut rng, d, e)
            }
            3 => {
                let d = rng.range(1, 17) as u32;
                let e = rng.range(-25, 25);
                let mut v = decimal(&mut rng, d, e);
                for _ in 0..rng.range(1, 3) {
                    v = step(v, rng.below(2) == 0);
                }
                v
            }
            4 => {
                let bases = [
                    2f64.powi(53),
                    2f64.powi(63),
                    2f64.powi(64),
                    1e15,
                    1e16,
                    1e17,
                    1e18,
                    1e19,
                ];
                let b = bases[rng.below(bases.len() as u64) as usize];
                let mut v = b + rng.range(-5000, 5000) as f64;
                for _ in 0..rng.range(0, 2) {
                    v = step(v, rng.below(2) == 0);
                }
                v
            }
            5 => {
                let e = rng.range(-323, 308);
                let mut v: f64 = format!("1e{e}").parse().expect("power of ten");
                for _ in 0..rng.range(0, 2) {
                    v = step(v, rng.below(2) == 0);
                }
                v
            }
            6 => {
                // 18-digit decimals ending in 5: halfway cases.
                let m = 10u64.pow(16) + rng.below(9 * 10u64.pow(16));
                let e = rng.range(-40, 40);
                format!("{m}5e{e}").parse().expect("halfway parses")
            }
            7 => {
                let num = rng.range(-1_000_000, 1_000_000) as f64;
                let den = [2.0, 4.0, 8.0, 16.0, 10.0, 100.0, 1000.0, 3.0, 7.0];
                num / den[rng.below(den.len() as u64) as usize]
            }
            8 => {
                let d = rng.range(12, 16) as u32;
                let e = rng.range(-30, 30);
                let v = decimal(&mut rng, d, e);
                if rng.below(2) == 0 {
                    step(v, rng.below(2) == 0)
                } else {
                    v
                }
            }
            9 => rng.range(i64::MIN / 2, i64::MAX / 2) as f64 * 2.0,
            _ => (rng.next() >> 11) as f64 / (1u64 << 33) as f64 - 1e6,
        };
        if !v.is_finite() {
            continue;
        }
        out.push(if rng.below(10) < 3 { -v } else { v });
    }
    out
}

/// An exactly round-tripping literal (both parsers read the
/// shortest representation exactly; the v0.12 corpus pins that).
fn lit(v: f64) -> String {
    if v.is_sign_negative() {
        format!("({v:?})")
    } else {
        format!("{v:?}")
    }
}

fn hipp_texts(vals: &[f64]) -> Vec<String> {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut out = Vec::new();
        for v in vals {
            let sql = format!("SELECT CAST({} AS TEXT)", lit(*v));
            let mut q = db.prepare(&sql).await.expect("prepare");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            let t = q.column_bytes(0).expect("text");
            out.push(String::from_utf8(t.to_vec()).expect("utf8"));
        }
        out
    })
}

fn oracle_texts(vals: &[f64], tag: &str) -> Vec<String> {
    let db = common::temp_db(tag);
    let sql: String = vals
        .iter()
        .map(|v| format!("SELECT CAST({} AS TEXT);\n", lit(*v)))
        .collect();
    common::cli(&db, &sql).lines().map(str::to_string).collect()
}

#[test]
fn named_examples_match_the_pin_golden() {
    let vals: Vec<f64> = PIN_GOLDEN.iter().map(|(v, _)| *v).collect();
    let got = hipp_texts(&vals);
    for ((v, want), got) in PIN_GOLDEN.iter().zip(got.iter()) {
        assert_eq!(got, want, "CAST({v:?} AS TEXT)");
    }
}

#[test]
fn render_real_matches_oracle_on_corpus() {
    let n = std::env::var("HIPP_REAL_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4000usize);
    let vals = corpus(n);
    let mut mismatches = Vec::new();
    for (i, chunk) in vals.chunks(2_000).enumerate() {
        let want = oracle_texts(chunk, &format!("v16-real-text-{i}"));
        assert_eq!(want.len(), chunk.len(), "one oracle line per value");
        let got = hipp_texts(chunk);
        for ((v, w), g) in chunk.iter().zip(want.iter()).zip(got.iter()) {
            if w != g {
                mismatches.push(format!("{v:?}: hipp {g} != oracle {w}"));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} reals render differently:\n{}",
        mismatches.len(),
        vals.len(),
        mismatches
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
