//! The SQL math functions (v0.16, leaf feature `math`): what
//! SQLite 3.54.1 provides when built with
//! `SQLITE_ENABLE_MATH_FUNCTIONS` — `acos` `acosh` `asin` `asinh`
//! `atan` `atan2` `atanh` `ceil` / `ceiling` `cos` `cosh` `degrees`
//! `exp` `floor` `ln` `log` (one or two arguments) `log10` `log2`
//! `mod` `pi` `pow` / `power` `radians` `sin` `sinh` `sqrt` `tan`
//! `tanh` `trunc` — and the in-crate floating-point library they
//! run on.
//!
//! There is no libm in `no_std` and hipp links none: every
//! function here is implemented from scratch over core `f64`
//! arithmetic (`+ - * /`, comparisons, bit casts) with clean-room
//! numerical methods — exact argument reduction (Payne–Hanek for
//! the trigonometric functions, with 1,472 bits of 2/π), then
//! Taylor / `atanh` series evaluated in double-double arithmetic
//! (about 104 significant bits) and rounded once. The results are
//! correctly rounded to nearest except with a probability of about
//! 2^-40 per call; `sqrt`, `fmod`, `ceil` / `floor` / `trunc` are
//! exact by construction (integer arithmetic). The pin calls its
//! platform libm (glibc), which is not correctly rounded in every
//! case, so a small fraction of results differ from the pin by one
//! unit in the last place (STATUS.md, "Math functions", records
//! the measured rates per function).
//!
//! The SQL layer (`MathFn`, `eval`) follows the pin's argument
//! handling: an argument counts only if `sqlite3_value_numeric_type`
//! makes it a number (INTEGER, REAL, or TEXT that is entirely a
//! number); anything else, and any NaN result, is NULL.

use crate::record::{OwnedValue, text_to_number};

// ---------------------------------------------------------------
// Double-double arithmetic
// ---------------------------------------------------------------

/// An unevaluated sum `hi + lo` with `|lo| <= ulp(hi) / 2`: about
/// 106 significant bits.
#[derive(Clone, Copy)]
struct DD {
    hi: f64,
    lo: f64,
}

const ONE: DD = DD { hi: 1.0, lo: 0.0 };

const fn dd(x: f64) -> DD {
    DD { hi: x, lo: 0.0 }
}

/// π as a double-double (computed offline from an exact integer
/// Machin evaluation; `hi` is the C `M_PI`).
const PI: DD = DD {
    hi: f64::from_bits(0x400921fb54442d18),
    lo: f64::from_bits(0x3ca1a62633145c07),
};
/// π / 2.
const PIO2: DD = DD {
    hi: f64::from_bits(0x3ff921fb54442d18),
    lo: f64::from_bits(0x3c91a62633145c07),
};
/// ln 2.
const LN2: DD = DD {
    hi: f64::from_bits(0x3fe62e42fefa39ef),
    lo: f64::from_bits(0x3c7abc9e3b39803f),
};

/// 2/π: its first 1,472 bits after the binary point, most
/// significant first (computed offline; the well-known prefix is
/// `0xa2f9836e4e44152...`).
const TWO_OVER_PI: [u64; 23] = [
    0xa2f9836e4e441529,
    0xfc2757d1f534ddc0,
    0xdb6295993c439041,
    0xfe5163abdebbc561,
    0xb7246e3a424dd2e0,
    0x06492eea09d1921c,
    0xfe1deb1cb129a73e,
    0xe88235f52ebb4484,
    0xe99c7026b45f7e41,
    0x3991d639835339f4,
    0x9c845f8bbdf9283b,
    0x1ff897ffde05980f,
    0xef2f118b5a0a6d1f,
    0x6d367ecf27cb09b7,
    0x4f463f669e5fea2d,
    0x7527bac7ebe5f17b,
    0x3d0739f78a5292ea,
    0x6bfb5fb11f8d5d08,
    0x56033046fc7b6bab,
    0xf0cfbc209af4361d,
    0xa9e391615ee61b08,
    0x6599855f14a06840,
    0x8dffd8804d732731,
];

fn two_sum(a: f64, b: f64) -> DD {
    let s = a + b;
    let bb = s - a;
    DD {
        hi: s,
        lo: (a - (s - bb)) + (b - bb),
    }
}

/// `two_sum` for `|a| >= |b|` (or `a == 0`).
fn quick_two_sum(a: f64, b: f64) -> DD {
    let s = a + b;
    DD {
        hi: s,
        lo: b - (s - a),
    }
}

/// Veltkamp's split: `a = h + l` with each half 26 bits wide.
/// Callers keep `|a|` far below 2^996.
fn split(a: f64) -> (f64, f64) {
    let c = 134_217_729.0 * a;
    let h = c - (c - a);
    (h, a - h)
}

/// Dekker's exact product (no FMA in core).
fn two_prod(a: f64, b: f64) -> DD {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    DD {
        hi: p,
        lo: ((ah * bh - p) + ah * bl + al * bh) + al * bl,
    }
}

fn add(a: DD, b: DD) -> DD {
    let s = two_sum(a.hi, b.hi);
    let t = two_sum(a.lo, b.lo);
    let u = quick_two_sum(s.hi, s.lo + t.hi);
    quick_two_sum(u.hi, t.lo + u.lo)
}

fn neg(a: DD) -> DD {
    DD {
        hi: -a.hi,
        lo: -a.lo,
    }
}

fn sub(a: DD, b: DD) -> DD {
    add(a, neg(b))
}

fn mul(a: DD, b: DD) -> DD {
    let p = two_prod(a.hi, b.hi);
    quick_two_sum(p.hi, p.lo + (a.hi * b.lo + a.lo * b.hi))
}

fn mul_f(a: DD, b: f64) -> DD {
    let p = two_prod(a.hi, b);
    quick_two_sum(p.hi, p.lo + a.lo * b)
}

/// `a * 2^k`, exact while the parts stay normal.
fn scale(a: DD, k: i32) -> DD {
    DD {
        hi: mul_pow2(a.hi, k),
        lo: mul_pow2(a.lo, k),
    }
}

fn div(a: DD, b: DD) -> DD {
    let q1 = a.hi / b.hi;
    let r = sub(a, mul_f(b, q1));
    let q2 = r.hi / b.hi;
    let r = sub(r, mul_f(b, q2));
    let q3 = r.hi / b.hi;
    add(quick_two_sum(q1, q2), dd(q3))
}

/// `a / d` for a small exact integer `d`.
fn div_f(a: DD, d: f64) -> DD {
    let q1 = a.hi / d;
    let p = two_prod(q1, d);
    let r = sub(a, p);
    let q2 = r.hi / d;
    let r = sub(r, mul_f(dd(d), q2));
    quick_two_sum(q1, q2 + r.hi / d)
}

/// `sqrt(a)` for `a >= 0`: one Newton step from the correctly
/// rounded double root.
fn sqrt_dd(a: DD) -> DD {
    if a.hi <= 0.0 {
        return dd(0.0);
    }
    let s = sqrt(a.hi);
    let r = sub(a, two_prod(s, s));
    quick_two_sum(s, r.hi / (2.0 * s))
}

/// The one rounding of a double-double result.
fn round(a: DD) -> f64 {
    a.hi + a.lo
}

// ---------------------------------------------------------------
// Bit-level helpers
// ---------------------------------------------------------------

const NAN: f64 = f64::NAN;
const INF: f64 = f64::INFINITY;

/// 2^k for `k` in -1022 ..= 1023.
fn pow2(k: i32) -> f64 {
    let b = (k.clamp(-1022, 1023) + 1023) as u64;
    f64::from_bits(b << 52)
}

/// `x * 2^k` in two exact-or-correctly-rounded steps (k in
/// -2044 ..= 2046).
fn mul_pow2(x: f64, k: i32) -> f64 {
    let h = k / 2;
    x * pow2(h) * pow2(k - h)
}

fn abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !(1u64 << 63))
}

fn copysign(x: f64, s: f64) -> f64 {
    f64::from_bits((x.to_bits() & !(1u64 << 63)) | (s.to_bits() & (1u64 << 63)))
}

fn is_finite(x: f64) -> bool {
    x.to_bits() & 0x7ff0_0000_0000_0000 != 0x7ff0_0000_0000_0000
}

/// The unbiased binary exponent of a finite nonzero `x`
/// (`x = 1.f * 2^e`; subnormals give their true exponent).
fn exponent(x: f64) -> i32 {
    let b = x.to_bits();
    let e = ((b >> 52) & 0x7ff) as i32;
    if e == 0 {
        let m = b & ((1u64 << 52) - 1);
        // m != 0: leading_zeros >= 12.
        -1011 - m.leading_zeros() as i32
    } else {
        e - 1023
    }
}

/// `x = m * 2^e` with `m` an integer of at most 53 bits (finite,
/// nonzero `x`).
fn decompose(x: f64) -> (u64, i32) {
    let b = x.to_bits();
    let e = ((b >> 52) & 0x7ff) as i32;
    let m = b & ((1u64 << 52) - 1);
    if e == 0 {
        (m, -1074)
    } else {
        (m | (1u64 << 52), e - 1075)
    }
}

/// Correct rounding of `m * 2^k` to a double (round half to even),
/// including the subnormal range and overflow to infinity; `m` is
/// a positive double-double near 1.
fn scale_round(m: DD, k: i32) -> f64 {
    let v = round(m);
    let et = exponent(v).saturating_add(k);
    if et > 1023 {
        return INF;
    }
    if et >= -1022 {
        return mul_pow2(v, k);
    }
    if et < -1076 {
        return 0.0;
    }
    // Subnormal: round m * 2^(k + 1074) to an integer n, then
    // n * 2^-1074 is exact.
    let j = k + 1074;
    let h = mul_pow2(m.hi, j);
    let l = mul_pow2(m.lo, j);
    let f = h as u64;
    let a = h - f as f64;
    // The sign of (h + l) - (f + 1/2): `a - 0.5` is exact whenever
    // |l| could matter (a = 0 or a >= 1/4).
    let up = (a - 0.5) + l;
    let even_up = |lo: u64| if lo & 1 == 1 { lo + 1 } else { lo };
    let n = if up > 0.0 {
        f + 1
    } else if up == 0.0 {
        even_up(f)
    } else if a == 0.0 && l < -0.5 && f > 0 {
        f - 1
    } else if a == 0.0 && l == -0.5 && f > 0 {
        even_up(f - 1)
    } else {
        f
    };
    n as f64 * f64::from_bits(1)
}

// ---------------------------------------------------------------
// sqrt, fmod, ceil / floor / trunc (exact)
// ---------------------------------------------------------------

/// The integer square root of `n` and its remainder.
fn isqrt(n: u128) -> (u128, u128) {
    let mut bit: u128 = 1 << 126;
    while bit > n {
        bit >>= 2;
    }
    let (mut res, mut rem) = (0u128, n);
    while bit != 0 {
        if rem >= res + bit {
            rem -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    (res, rem)
}

/// The correctly rounded square root (IEEE 754 `squareRoot`):
/// NaN for a negative argument, `sqrt(-0.0)` is `-0.0`.
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return NAN;
    }
    if x == 0.0 || x == INF {
        return x;
    }
    let (m, mut e) = decompose(x);
    // Normalise m to 53 bits (subnormals).
    let lz = m.leading_zeros() as i32 - 11;
    let m = m << lz;
    e -= lz;
    let mut big = u128::from(m);
    if e & 1 != 0 {
        big <<= 1;
        e -= 1;
    }
    // big < 2^54; shift so the root carries 54 bits.
    let (r, rem) = isqrt(big << 54);
    e -= 54;
    let q = r >> 1;
    let q = if r & 1 == 1 && (rem != 0 || q & 1 == 1) {
        q + 1
    } else {
        q
    };
    (q as u64) as f64 * pow2(e / 2 + 1)
}

/// The C `fmod`: `x - n*y` for the `n` truncated toward zero,
/// computed exactly; NaN when `x` is infinite or `y` is zero.
pub fn fmod(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || !is_finite(x) || y == 0.0 {
        return NAN;
    }
    if !is_finite(y) || x == 0.0 || abs(x) < abs(y) {
        return x;
    }
    let (mx, ex) = decompose(abs(x));
    let (my, ey) = decompose(abs(y));
    // |x| >= |y|: reduce mx * 2^(ex - ey) modulo my, ten bits at a
    // time (r < my < 2^53, so r << 10 fits).
    let mut r = mx % my;
    // ex >= ey: |x| >= |y| and subnormals share the exponent -1074.
    let mut gap = ex - ey;
    while gap > 0 {
        let s = gap.min(10);
        r = (r << s) % my;
        gap -= s;
    }
    copysign(r as f64 * pow2_any(ey), x)
}

/// 2^k for any k in -1074 ..= 1023 (subnormal powers included).
fn pow2_any(k: i32) -> f64 {
    if k >= -1022 {
        pow2(k)
    } else {
        f64::from_bits(1u64 << (k + 1074).clamp(0, 51))
    }
}

/// Round toward zero to an integer (C `trunc`).
pub fn trunc(x: f64) -> f64 {
    let b = x.to_bits();
    let e = ((b >> 52) & 0x7ff) as i32 - 1023;
    if e >= 52 {
        return x;
    }
    if e < 0 {
        return copysign(0.0, x);
    }
    let mask = (1u64 << (52 - e)) - 1;
    f64::from_bits(b & !mask)
}

/// Round toward negative infinity (C `floor`).
pub fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if x < 0.0 && t != x { t - 1.0 } else { t }
}

/// Round toward positive infinity (C `ceil`).
pub fn ceil(x: f64) -> f64 {
    let t = trunc(x);
    if x > 0.0 && t != x { t + 1.0 } else { t }
}

// ---------------------------------------------------------------
// exp / log
// ---------------------------------------------------------------

/// `exp(z) = m * 2^k` with `m` near 1 (`|z| <= ~1100`).
fn exp_parts(z: DD) -> (DD, i32) {
    let (t, k) = expm1_parts(z);
    (add(ONE, t), k)
}

/// `exp(z) = (1 + t) * 2^k`, with `t` = expm1 of the reduced
/// argument (full relative precision when `k` is 0).
fn expm1_parts(z: DD) -> (DD, i32) {
    let kf = trunc(z.hi * core::f64::consts::LOG2_E + copysign(0.5, z.hi));
    let r = sub(z, mul_f(LN2, kf));
    // expm1(r / 1024) by Taylor (|s| < 3.5e-4, nine terms), then
    // ten squarings of 1 + t kept as t: (1 + t)^2 - 1 = 2t + t^2.
    let s = scale(r, -10);
    let mut p = ONE;
    let mut n = 9.0;
    while n >= 2.0 {
        p = add(ONE, div_f(mul(s, p), n));
        n -= 1.0;
    }
    let mut t = mul(s, p);
    for _ in 0..10 {
        t = add(mul_f(t, 2.0), mul(t, t));
    }
    (t, kf as i32)
}

/// `e^x - 1`, correctly rounded (accurate for tiny `x`).
pub fn expm1(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 710.0 {
        return INF;
    }
    if x < -40.0 {
        return -1.0;
    }
    if abs(x) < pow2(-54) {
        return x;
    }
    let (t, k) = expm1_parts(dd(x));
    if k == 0 {
        return round(t);
    }
    if k > 60 {
        return scale_round(add(ONE, t), k);
    }
    round(sub(scale(add(ONE, t), k), ONE))
}

/// e^x, correctly rounded.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 710.0 {
        return INF;
    }
    if x < -746.0 {
        return 0.0;
    }
    let (m, k) = exp_parts(dd(x));
    scale_round(m, k)
}

/// `ln(1 + u)` for `|u| <= ~0.42`: `2 atanh(s)`, `s = u / (2 + u)`.
fn log1p_series(u: DD) -> DD {
    let s = div(u, add(dd(2.0), u));
    let s2 = mul(s, s);
    // sum_{k=0}^{22} s2^k / (2k + 1), |s2| <= 0.0295.
    let mut acc = div_f(ONE, 45.0);
    let mut k = 21.0;
    while k >= 0.0 {
        acc = add(mul(acc, s2), div_f(ONE, 2.0 * k + 1.0));
        k -= 1.0;
    }
    mul_f(mul(s, acc), 2.0)
}

/// `ln(w)` split as `e * ln 2 + ln(1 + f)`: returns `(e, f)` with
/// `1 + f` in `[sqrt(1/2), sqrt(2)]` (positive finite `w`).
fn log_reduce(w: DD) -> (i32, DD) {
    let mut w = w;
    let mut e = 0;
    if w.hi < f64::MIN_POSITIVE {
        w = scale(w, 54);
        e = -54;
    }
    let k = exponent(w.hi);
    let mut m = scale(w, -k);
    e += k;
    if m.hi > core::f64::consts::SQRT_2 {
        m = scale(m, -1);
        e += 1;
    }
    (e, sub(m, ONE))
}

/// `ln(w)` for a positive finite double-double.
fn ln_dd(w: DD) -> DD {
    let (e, f) = log_reduce(w);
    add(mul_f(LN2, f64::from(e)), log1p_series(f))
}

/// `ln(1 + u)` for `u > -1`.
fn log1p_dd(u: DD) -> DD {
    if abs(u.hi) < 0.4 {
        log1p_series(u)
    } else {
        ln_dd(add(ONE, u))
    }
}

/// `ln(1 + x)`, correctly rounded (accurate for tiny `x`; -inf at
/// -1, NaN below).
pub fn log1p(x: f64) -> f64 {
    if x.is_nan() || x < -1.0 {
        return NAN;
    }
    if x == -1.0 {
        return -INF;
    }
    if x == INF {
        return x;
    }
    if abs(x) < pow2(-54) {
        return x;
    }
    round(log1p_dd(dd(x)))
}

/// Domain handling shared by the logarithms: `Some(result)` for
/// the special arguments.
fn log_special(x: f64) -> Option<f64> {
    if x.is_nan() || x < 0.0 {
        Some(NAN)
    } else if x == 0.0 {
        Some(-INF)
    } else if x == INF {
        Some(INF)
    } else {
        None
    }
}

/// The natural logarithm, correctly rounded (`ln(0)` is -inf,
/// negative arguments NaN).
pub fn ln(x: f64) -> f64 {
    log_special(x).unwrap_or_else(|| round(ln_dd(dd(x))))
}

/// The base-2 logarithm, correctly rounded (exact for powers of
/// two).
pub fn log2(x: f64) -> f64 {
    log_special(x).unwrap_or_else(|| {
        let (e, f) = log_reduce(dd(x));
        round(add(dd(f64::from(e)), div(log1p_series(f), LN2)))
    })
}

/// log10(2) split for exact products with an exponent: `hi` has
/// its low 20 mantissa bits clear, `lo` is the rest (rounded).
const LOG10_2HI: f64 = f64::from_bits(0x3fd3441350900000);
const LOG10_2LO: f64 = f64::from_bits(0x3dcef3fde623e256);
/// 1 / ln(10), rounded.
const INV_LN10: f64 = f64::from_bits(0x3fdbcb7b1526e50e);

/// The base-10 logarithm as the pin computes it (fitted to its
/// output, 0 mismatches over 145,000 random doubles through its
/// own `ln`): `x = m * 2^k` with `m` in `[1, 2)` (in `[1/2, 1)`,
/// and `k + 1`, below 1), then `(k * lo + ln(m) / ln 10) + k * hi`
/// in double arithmetic over the correctly rounded `ln`. Not
/// correctly rounded itself (up to 2 ulp from the true value,
/// exact for 10^0 .. 10^22).
pub fn log10(x: f64) -> f64 {
    log_special(x).unwrap_or_else(|| {
        let mut k = exponent(x);
        let mut m = mul_pow2(x, -k);
        if k < 0 {
            m *= 0.5;
            k += 1;
        }
        let y = f64::from(k);
        let z = y * LOG10_2LO + INV_LN10 * ln(m);
        z + y * LOG10_2HI
    })
}

// ---------------------------------------------------------------
// pow
// ---------------------------------------------------------------

fn is_int(y: f64) -> bool {
    is_finite(y) && trunc(y) == y
}

fn is_odd_int(y: f64) -> bool {
    is_int(y) && abs(y) < 9_007_199_254_740_992.0 && trunc(y * 0.5) != y * 0.5
}

/// `x^y` (C99 Annex F special cases), correctly rounded; exact
/// powers (and their halfway cases) are computed exactly.
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return NAN;
    }
    let odd = is_odd_int(y);
    if !is_finite(y) {
        let ax = abs(x);
        if ax == 1.0 {
            return 1.0;
        }
        return if (ax < 1.0) == (y > 0.0) { 0.0 } else { INF };
    }
    if x == 0.0 {
        return if y < 0.0 {
            if odd { copysign(INF, x) } else { INF }
        } else if odd {
            x
        } else {
            0.0
        };
    }
    if !is_finite(x) {
        let r = if y < 0.0 { 0.0 } else { INF };
        return if x < 0.0 && odd { -r } else { r };
    }
    if x < 0.0 && !is_int(y) {
        return NAN;
    }
    let r = pow_pos(abs(x), y);
    if x < 0.0 && odd { -r } else { r }
}

/// `(a * 2^e)^y` for odd `a > 1` when the result is a dyadic
/// rational of at most 106 significant bits, computed exactly
/// and rounded once (so halfway results round to even); `None`
/// when it is not, so the result is irrational or needs more
/// bits and cannot be a halfway case. Writing `y = n / 2^k` with
/// `n` odd, the power is rational only for `n > 0`, `a = t^(2^k)`
/// and `2^k | e n`; `t >= 3` and `t^(2^k) < 2^53` bound `k <= 5`,
/// and `t^n < 2^106` bounds `n <= 66`.
fn pow_exact(a: u64, e: i32, y: f64) -> Option<f64> {
    if y <= 0.0 || y > 106.0 {
        return None;
    }
    let mut k = 0u32;
    let mut yk = y;
    while !is_int(yk) {
        if k == 5 {
            return None;
        }
        yk *= 2.0;
        k += 1;
    }
    // 1 <= yk <= 106 * 32: an exact small integer.
    let n = yk as u32;
    // t = the exact 2^k-th root of a.
    let mut t = u128::from(a);
    for _ in 0..k {
        let (r, rem) = isqrt(t);
        if rem != 0 {
            return None;
        }
        t = r;
    }
    let en = e.checked_mul(i32::try_from(n).ok()?)?;
    let d = 1i32 << k;
    if en.rem_euclid(d) != 0 {
        return None;
    }
    let ey = en.div_euclid(d);
    let mut p: u128 = 1;
    for _ in 0..n {
        p = p.checked_mul(t).filter(|v| *v < (1u128 << 106))?;
    }
    let bits = 128 - p.leading_zeros() as i32;
    let sh = (bits - 53).max(0) as u32;
    // Both halves fit 53 bits: exact u64 -> f64 conversions.
    let hi = ((p >> sh) as u64) as f64 * pow2(sh as i32);
    let lo = ((p & ((1u128 << sh) - 1)) as u64) as f64;
    // p * 2^ey, with p scaled to [1, 2).
    let v = scale(quick_two_sum(hi, lo), 1 - bits);
    let ex = ey.checked_add(bits - 1)?;
    if ex > 2000 {
        return Some(INF);
    }
    if ex < -2000 {
        return Some(0.0);
    }
    Some(scale_round(v, ex))
}

/// `ax^y` for finite `ax > 0`, finite nonzero `y`.
fn pow_pos(ax: f64, y: f64) -> f64 {
    let (m, e) = decompose(ax);
    let tz = m.trailing_zeros() as i32;
    let (a, e) = (m >> tz, e + tz);
    if a == 1 {
        // A power of two: exact whenever e * y is an integer.
        if e == 0 {
            return 1.0;
        }
        if abs(y) > pow2(20) {
            return if (e > 0) == (y > 0.0) { INF } else { 0.0 };
        }
        let p = two_prod(f64::from(e), y);
        if p.lo == 0.0 && is_int(p.hi) {
            if p.hi > 2000.0 {
                return INF;
            }
            if p.hi < -2000.0 {
                return 0.0;
            }
            return scale_round(ONE, p.hi as i32);
        }
    } else if let Some(r) = pow_exact(a, e, y) {
        return r;
    }
    let l = ln_dd(dd(ax));
    let z0 = l.hi * y;
    if z0 > 720.0 {
        return INF;
    }
    if z0 < -760.0 {
        return 0.0;
    }
    let (m, k) = exp_parts(mul_f(l, y));
    scale_round(m, k)
}

// ---------------------------------------------------------------
// Trigonometric functions
// ---------------------------------------------------------------

/// 64 bits of 2/π starting at bit `p` (0 = the 2^-1 place).
fn two_over_pi_bits(p: usize) -> u64 {
    let i = p / 64;
    let off = (p % 64) as u32;
    let a = TWO_OVER_PI.get(i).copied().unwrap_or(0);
    if off == 0 {
        return a;
    }
    let b = TWO_OVER_PI.get(i + 1).copied().unwrap_or(0);
    (a << off) | (b >> (64 - off))
}

/// 64 bits of the little-endian big integer `p` starting at bit
/// `pos`.
fn bits_at(p: &[u64; 6], pos: usize) -> u64 {
    let i = pos / 64;
    let off = (pos % 64) as u32;
    let a = p.get(i).copied().unwrap_or(0);
    if off == 0 {
        return a;
    }
    let b = p.get(i + 1).copied().unwrap_or(0);
    (a >> off) | (b << (64 - off))
}

/// Payne–Hanek reduction of a finite `ax >= π/4`: `ax = (4j + q)
/// π/2 + r` with `|r| <= π/4`.
fn reduce(ax: f64) -> (DD, u32) {
    let (m, e) = decompose(ax);
    // Bits s .. s+319 of 2/π (1-based places); those before s
    // contribute multiples of 4 and drop out.
    let s = if e >= 2 { (e - 1) as usize } else { 1 };
    let mut w = [0u64; 5];
    for (j, slot) in w.iter_mut().enumerate() {
        *slot = two_over_pi_bits(s - 1 + 64 * j);
    }
    // P = m * W, little-endian.
    let mut p = [0u64; 6];
    let mut carry: u128 = 0;
    for j in 0..5 {
        let limb = w.get(4 - j).copied().unwrap_or(0);
        let t = u128::from(m) * u128::from(limb) + carry;
        if let Some(slot) = p.get_mut(j) {
            *slot = t as u64;
        }
        carry = t >> 64;
    }
    if let Some(slot) = p.get_mut(5) {
        *slot = carry as u64;
    }
    // ax * 2/π = P * 2^-f.
    let f = (s as i64 + 319 - i64::from(e)) as usize;
    let int = bits_at(&p, f) & 3;
    // The 192 fraction bits as g2:g1:g0 (most significant first).
    let (mut g0, mut g1, mut g2) = (
        bits_at(&p, f - 192),
        bits_at(&p, f - 128),
        bits_at(&p, f - 64),
    );
    let negate = g2 >> 63 == 1;
    if negate {
        // 2^192 - g.
        let (v0, c0) = (!g0).overflowing_add(1);
        let (v1, c1) = (!g1).overflowing_add(u64::from(c0));
        let v2 = (!g2).wrapping_add(u64::from(c1));
        (g0, g1, g2) = (v0, v1, v2);
    }
    let q = ((int + u64::from(negate)) & 3) as u32;
    // Normalise the 192-bit fraction and take its top 106 bits.
    let mut lz = 0i32;
    while g2 == 0 && lz < 192 {
        (g0, g1, g2) = (0, g0, g1);
        lz += 64;
    }
    if lz >= 192 {
        return (dd(0.0), q);
    }
    let z = g2.leading_zeros();
    if z > 0 {
        (g1, g2) = ((g1 << z) | (g0 >> (64 - z)), (g2 << z) | (g1 >> (64 - z)));
    }
    lz += z as i32;
    let top = (u128::from(g2) << 64) | u128::from(g1);
    let hi = ((top >> 75) as u64) as f64 * pow2(-53);
    let lo = (((top >> 22) as u64) & ((1u64 << 53) - 1)) as f64 * pow2(-106);
    let frac = scale(quick_two_sum(hi, lo), -lz);
    let r = mul(frac, PIO2);
    (if negate { neg(r) } else { r }, q)
}

/// sin(r) for |r| <= π/4: r (1 - r²/(2·3) (1 - r²/(4·5) (...))).
fn sin_poly(r: DD) -> DD {
    let r2 = mul(r, r);
    let mut acc = ONE;
    let mut k = 14.0;
    while k >= 1.0 {
        acc = sub(ONE, div_f(mul(r2, acc), (2.0 * k) * (2.0 * k + 1.0)));
        k -= 1.0;
    }
    mul(r, acc)
}

/// cos(r) for |r| <= π/4: 1 - r²/(1·2) (1 - r²/(3·4) (...)).
fn cos_poly(r: DD) -> DD {
    let r2 = mul(r, r);
    let mut acc = ONE;
    let mut k = 15.0;
    while k >= 1.0 {
        acc = sub(ONE, div_f(mul(r2, acc), (2.0 * k - 1.0) * (2.0 * k)));
        k -= 1.0;
    }
    acc
}

/// The reduced argument and quadrant of a finite `ax >= 0`.
fn reduced(ax: f64) -> (DD, u32) {
    if ax <= core::f64::consts::FRAC_PI_4 {
        (dd(ax), 0)
    } else {
        reduce(ax)
    }
}

/// The sine, correctly rounded (NaN for infinities).
pub fn sin(x: f64) -> f64 {
    if !is_finite(x) {
        return NAN;
    }
    let ax = abs(x);
    if ax < pow2(-27) {
        return x;
    }
    let (r, q) = reduced(ax);
    let v = match q {
        0 => sin_poly(r),
        1 => cos_poly(r),
        2 => neg(sin_poly(r)),
        _ => neg(cos_poly(r)),
    };
    copysign(1.0, x) * round(v)
}

/// The cosine, correctly rounded (NaN for infinities).
pub fn cos(x: f64) -> f64 {
    if !is_finite(x) {
        return NAN;
    }
    let ax = abs(x);
    if ax < pow2(-60) {
        return 1.0;
    }
    let (r, q) = reduced(ax);
    round(match q {
        0 => cos_poly(r),
        1 => neg(sin_poly(r)),
        2 => neg(cos_poly(r)),
        _ => sin_poly(r),
    })
}

/// The tangent, correctly rounded (NaN for infinities).
pub fn tan(x: f64) -> f64 {
    if !is_finite(x) {
        return NAN;
    }
    let ax = abs(x);
    if ax < pow2(-27) {
        return x;
    }
    let (r, q) = reduced(ax);
    let (s, c) = (sin_poly(r), cos_poly(r));
    let v = if q & 1 == 0 {
        div(s, c)
    } else {
        neg(div(c, s))
    };
    copysign(1.0, x) * round(v)
}

/// atan(t) for a double-double `t >= 0`.
fn atan_dd(t: DD) -> DD {
    if t.hi > pow2(60) {
        // π/2 - 1/t (+ 1/(3t³) < 2^-180).
        return sub(PIO2, dd(1.0 / t.hi));
    }
    let inv = t.hi > 1.0;
    let mut t = if inv { div(ONE, t) } else { t };
    // Three angle halvings: t / (1 + sqrt(1 + t²)) → |t| <= 0.0985.
    for _ in 0..3 {
        let w = sqrt_dd(add(ONE, mul(t, t)));
        t = div(t, add(ONE, w));
    }
    let t2 = mul(t, t);
    // sum_{k=0}^{17} (-1)^k t2^k / (2k + 1).
    let mut acc = div_f(ONE, 35.0);
    let mut k = 16.0;
    while k >= 0.0 {
        acc = sub(div_f(ONE, 2.0 * k + 1.0), mul(acc, t2));
        k -= 1.0;
    }
    let a = mul_f(mul(t, acc), 8.0);
    if inv { sub(PIO2, a) } else { a }
}

/// The arctangent, correctly rounded.
pub fn atan(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let ax = abs(x);
    if ax < pow2(-27) {
        return x;
    }
    if ax == INF {
        return copysign(PIO2.hi, x);
    }
    copysign(round(atan_dd(dd(ax))), x)
}

/// `atan2(y, x)` (C99 Annex F special cases), correctly rounded.
pub fn atan2(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return NAN;
    }
    let xneg = x.to_bits() >> 63 == 1;
    if y == 0.0 {
        return if xneg { copysign(PI.hi, y) } else { y };
    }
    if x == 0.0 {
        return copysign(PIO2.hi, y);
    }
    let (ax, ay) = (abs(x), abs(y));
    if ax == INF {
        if ay == INF {
            let v = if xneg {
                round(mul_f(PI, 0.75))
            } else {
                round(scale(PI, -2))
            };
            return copysign(v, y);
        }
        return if xneg {
            copysign(PI.hi, y)
        } else {
            copysign(0.0, y)
        };
    }
    if ay == INF {
        return copysign(PIO2.hi, y);
    }
    let d = exponent(ay) - exponent(ax);
    let v = if d > 64 {
        // atan(t) = π/2 - 1/t + ..., 1/t < 2^-63.
        let t = dd(ax / ay);
        let a = sub(PIO2, t);
        if xneg { add(PIO2, t) } else { a }
    } else if d < -64 {
        // atan(t) = t - t³/3 (t < 2^-63): the rounded quotient, as
        // on the pin (an exact tie in the subnormal range rounds to
        // even, where the true value lies just below it).
        if xneg {
            sub(PI, dd(ay / ax))
        } else {
            return copysign(ay / ax, y);
        }
    } else {
        // Scale both to near 1 (exact), then atan of the quotient.
        let k = -exponent(ax);
        let t = div(dd(mul_pow2(ay, k)), dd(mul_pow2(ax, k)));
        let a = atan_dd(t);
        if xneg { sub(PI, a) } else { a }
    };
    copysign(round(v), y)
}

/// The arcsine, correctly rounded (NaN outside -1 ..= 1).
pub fn asin(x: f64) -> f64 {
    let ax = abs(x);
    if x.is_nan() || ax > 1.0 {
        return NAN;
    }
    if ax < pow2(-27) {
        return x;
    }
    if ax == 1.0 {
        return copysign(PIO2.hi, x);
    }
    let w = mul(two_sum(1.0, -ax), two_sum(1.0, ax));
    copysign(round(atan_dd(div(dd(ax), sqrt_dd(w)))), x)
}

/// The arccosine, correctly rounded (NaN outside -1 ..= 1).
pub fn acos(x: f64) -> f64 {
    if x.is_nan() || abs(x) > 1.0 {
        return NAN;
    }
    if x == 1.0 {
        return 0.0;
    }
    if x == -1.0 {
        return PI.hi;
    }
    // 2 atan(sqrt((1 - x) / (1 + x))).
    let u = div(two_sum(1.0, -x), two_sum(1.0, x));
    round(mul_f(atan_dd(sqrt_dd(u)), 2.0))
}

// ---------------------------------------------------------------
// Hyperbolic functions
// ---------------------------------------------------------------

// The pin's hyperbolic functions behave exactly as the textbook
// identities over expm1 / exp / log1p / log / sqrt evaluated in
// double arithmetic (fitted to its output: 0 to 6 mismatches in
// 60,000 random arguments through its own building blocks). hipp
// evaluates the same identities over its correctly rounded
// building blocks, so the results agree wherever the pin's
// building blocks are correctly rounded too.

/// ln(2), rounded.
const LN2_D: f64 = f64::from_bits(0x3fe62e42fefa39ef);
/// Where the pin's `sinh` / `cosh` stop calling `exp(|x|)` and
/// square `exp(|x|/2)` instead: 709.7822265625, ln(DBL_MAX) with
/// its low 32 bits clear (fitted: 0 mismatches over 32,768
/// arguments around it, 28 with ln(DBL_MAX) itself).
const EXP_SPLIT: f64 = f64::from_bits(0x4086_2e42_0000_0000);

/// The hyperbolic sine: `sign * (t + t/(t+1)) / 2` with `t =
/// expm1(|x|)` (`2t - t²/(t+1)` below 1), `e^|x| / 2` from 22 on.
pub fn sinh(x: f64) -> f64 {
    if !is_finite(x) {
        return x;
    }
    let h = copysign(0.5, x);
    let a = abs(x);
    if a < 22.0 {
        if a < pow2(-28) {
            return x;
        }
        let t = expm1(a);
        if a < 1.0 {
            return h * (2.0 * t - t * t / (t + 1.0));
        }
        return h * (t + t / (t + 1.0));
    }
    if a < EXP_SPLIT {
        return h * exp(a);
    }
    if a <= 710.475_860_073_943_9 {
        let w = exp(0.5 * a);
        return h * w * w;
    }
    copysign(INF, x)
}

/// The hyperbolic cosine: `1 + t²/(2(1+t))` (`t = expm1(|x|)`)
/// below about ln(2)/2, `e^|x|/2 + 1/(2 e^|x|)` below 22, `e^|x| / 2`
/// beyond.
pub fn cosh(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let a = abs(x);
    // ln(2)/2 rounded up to its high 32 bits (fitted).
    if a < f64::from_bits(0x3fd6_2e43_0000_0000) {
        let t = expm1(a);
        let w = 1.0 + t;
        if a < pow2(-55) {
            return w;
        }
        return 1.0 + (t * t) / (w + w);
    }
    if a < 22.0 {
        let t = exp(a);
        return 0.5 * t + 0.5 / t;
    }
    if a < EXP_SPLIT {
        return 0.5 * exp(a);
    }
    if a <= 710.475_860_073_943_9 {
        let w = exp(0.5 * a);
        return 0.5 * w * w;
    }
    INF
}

/// The hyperbolic tangent: `-t/(t+2)` with `t = expm1(-2|x|)`
/// below 1, `1 - 2/(t+2)` with `t = expm1(2|x|)` below 22, else
/// ±1.
pub fn tanh(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let a = abs(x);
    let z = if a < 22.0 {
        if a < pow2(-55) {
            return x * (1.0 + x);
        }
        if a >= 1.0 {
            let t = expm1(2.0 * a);
            1.0 - 2.0 / (t + 2.0)
        } else {
            let t = expm1(-2.0 * a);
            -t / (t + 2.0)
        }
    } else {
        1.0
    };
    copysign(z, x)
}

/// The inverse hyperbolic sine: `ln(|x|) + ln 2` beyond about
/// 2^28, `ln(2|x| + 1/(sqrt(x²+1) + |x|))` beyond about 2, else
/// `log1p(|x| + x²/(1 + sqrt(1 + x²)))`.
pub fn asinh(x: f64) -> f64 {
    if !is_finite(x) {
        return x;
    }
    let a = abs(x);
    if a < pow2(-28) {
        return x;
    }
    // The pin's branch points compare the high 32 bits: "beyond
    // 2^28" starts at 2^28 + 2^-20 * 2^28, "beyond 2" at 2 + 2^-19
    // (fitted: 0 mismatches over 4,207 arguments around each,
    // 1,052 and 176 with the plain values).
    let w = if a >= f64::from_bits(0x41b0_0001_0000_0000) {
        ln(a) + LN2_D
    } else if a >= f64::from_bits(0x4000_0001_0000_0000) {
        ln(2.0 * a + 1.0 / (sqrt(x * x + 1.0) + a))
    } else {
        let t = x * x;
        log1p(a + t / (1.0 + sqrt(1.0 + t)))
    };
    copysign(w, x)
}

/// The inverse hyperbolic cosine (NaN below 1): `ln(x) + ln 2`
/// from 2^28, `ln(2x - 1/(x + sqrt(x² - 1)))` beyond 2, else
/// `log1p(t + sqrt(2t + t²))` with `t = x - 1`.
pub fn acosh(x: f64) -> f64 {
    if x.is_nan() || x < 1.0 {
        return NAN;
    }
    if x >= pow2(28) {
        return if x == INF { x } else { ln(x) + LN2_D };
    }
    if x == 1.0 {
        return 0.0;
    }
    if x > 2.0 {
        let t = x * x;
        return ln(2.0 * x - 1.0 / (x + sqrt(t - 1.0)));
    }
    let t = x - 1.0;
    log1p(t + sqrt(2.0 * t + t * t))
}

/// The inverse hyperbolic tangent (±inf at ±1, NaN beyond):
/// `log1p(2|x| + 2|x|²/(1-|x|)) / 2` below 1/2, else
/// `log1p(2|x|/(1-|x|)) / 2`.
pub fn atanh(x: f64) -> f64 {
    let a = abs(x);
    if x.is_nan() || a > 1.0 {
        return NAN;
    }
    if a == 1.0 {
        return copysign(INF, x);
    }
    if a < pow2(-28) {
        return x;
    }
    let t = if a < 0.5 {
        let t = a + a;
        0.5 * log1p(t + t * a / (1.0 - a))
    } else {
        0.5 * log1p((a + a) / (1.0 - a))
    };
    copysign(t, x)
}

// ---------------------------------------------------------------
// The SQL functions
// ---------------------------------------------------------------

/// The math functions (`ScalarFn::Math`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum MathFn {
    Acos,
    Acosh,
    Asin,
    Asinh,
    Atan,
    Atan2,
    Atanh,
    Ceil,
    Cos,
    Cosh,
    Degrees,
    Exp,
    Floor,
    Ln,
    Log,
    Log10,
    Log2,
    Mod,
    Pi,
    Pow,
    Radians,
    Sin,
    Sinh,
    Sqrt,
    Tan,
    Tanh,
    Trunc,
}

const NAMES: &[(&[u8], MathFn)] = &[
    (b"acos", MathFn::Acos),
    (b"acosh", MathFn::Acosh),
    (b"asin", MathFn::Asin),
    (b"asinh", MathFn::Asinh),
    (b"atan", MathFn::Atan),
    (b"atan2", MathFn::Atan2),
    (b"atanh", MathFn::Atanh),
    (b"ceil", MathFn::Ceil),
    (b"ceiling", MathFn::Ceil),
    (b"cos", MathFn::Cos),
    (b"cosh", MathFn::Cosh),
    (b"degrees", MathFn::Degrees),
    (b"exp", MathFn::Exp),
    (b"floor", MathFn::Floor),
    (b"ln", MathFn::Ln),
    (b"log", MathFn::Log),
    (b"log10", MathFn::Log10),
    (b"log2", MathFn::Log2),
    (b"mod", MathFn::Mod),
    (b"pi", MathFn::Pi),
    (b"pow", MathFn::Pow),
    (b"power", MathFn::Pow),
    (b"radians", MathFn::Radians),
    (b"sin", MathFn::Sin),
    (b"sinh", MathFn::Sinh),
    (b"sqrt", MathFn::Sqrt),
    (b"tan", MathFn::Tan),
    (b"tanh", MathFn::Tanh),
    (b"trunc", MathFn::Trunc),
];

impl MathFn {
    /// The function a name calls (ASCII case-insensitive).
    pub fn lookup(name: &[u8]) -> Option<MathFn> {
        NAMES
            .iter()
            .find(|(n, _)| name.eq_ignore_ascii_case(n))
            .map(|(_, f)| *f)
    }

    /// The argument counts the pin accepts: `pi()` none, `log` one
    /// or two, `atan2` / `mod` / `pow` two, the rest one.
    pub fn arity(self) -> (usize, usize) {
        match self {
            MathFn::Pi => (0, 0),
            MathFn::Log => (1, 2),
            MathFn::Atan2 | MathFn::Mod | MathFn::Pow => (2, 2),
            _ => (1, 1),
        }
    }
}

/// A number as `sqlite3_value_numeric_type` sees it: INTEGER and
/// REAL as they are, TEXT only when the whole text (to its first
/// NUL, ASCII spaces around it allowed) is a number; NULL and BLOB
/// are not numbers.
enum Arg {
    Int(i64),
    Real(f64),
}

fn numeric(v: &OwnedValue) -> Option<Arg> {
    match v {
        OwnedValue::Integer(i) => Some(Arg::Int(*i)),
        OwnedValue::Real(r) => Some(Arg::Real(*r)),
        OwnedValue::Text(t) => match text_to_number(t.as_slice())? {
            OwnedValue::Integer(i) => Some(Arg::Int(i)),
            OwnedValue::Real(r) => Some(Arg::Real(r)),
            _ => None,
        },
        _ => None,
    }
}

fn real(v: &OwnedValue) -> Option<f64> {
    Some(match numeric(v)? {
        Arg::Int(i) => i as f64,
        Arg::Real(r) => r,
    })
}

/// `sqlite3_value_double`: any value, a text's or blob's numeric
/// prefix (0.0 without one), NULL 0.0.
fn value_double(v: &OwnedValue) -> f64 {
    match crate::query::to_number(v) {
        Some(crate::query::Num::Int(i)) => i as f64,
        Some(crate::query::Num::Real(r)) => r,
        None => 0.0,
    }
}

fn result(r: f64) -> OwnedValue {
    if r.is_nan() {
        OwnedValue::Null
    } else {
        OwnedValue::Real(r)
    }
}

/// Evaluate a math function over its (already counted) arguments,
/// as the pin does: a non-numeric argument or a NaN result is
/// NULL; `ceil` / `floor` / `trunc` keep an INTEGER; the
/// logarithms are NULL at or below zero (`log(B, X)` also for a
/// base at or below 1, reading X as `sqlite3_value_double`).
pub fn eval(f: MathFn, args: &[OwnedValue]) -> OwnedValue {
    let null = OwnedValue::Null;
    let a0 = args.first().unwrap_or(&null);
    match f {
        MathFn::Pi => return OwnedValue::Real(PI.hi),
        MathFn::Ceil | MathFn::Floor | MathFn::Trunc => {
            return match numeric(a0) {
                None => OwnedValue::Null,
                Some(Arg::Int(i)) => OwnedValue::Integer(i),
                Some(Arg::Real(r)) => OwnedValue::Real(match f {
                    MathFn::Ceil => ceil(r),
                    MathFn::Floor => floor(r),
                    _ => trunc(r),
                }),
            };
        }
        MathFn::Atan2 | MathFn::Mod | MathFn::Pow => {
            let a1 = args.get(1).unwrap_or(&null);
            let (Some(x), Some(y)) = (real(a0), real(a1)) else {
                return OwnedValue::Null;
            };
            return result(match f {
                MathFn::Atan2 => atan2(x, y),
                MathFn::Mod => fmod(x, y),
                _ => pow(x, y),
            });
        }
        _ => {}
    }
    let Some(x) = real(a0) else {
        return OwnedValue::Null;
    };
    if matches!(f, MathFn::Ln | MathFn::Log | MathFn::Log10 | MathFn::Log2) {
        if x <= 0.0 {
            return OwnedValue::Null;
        }
        if f == MathFn::Log {
            if let Some(a1) = args.get(1) {
                let b = ln(x);
                if b <= 0.0 {
                    return OwnedValue::Null;
                }
                let v = value_double(a1);
                if v <= 0.0 {
                    return OwnedValue::Null;
                }
                return result(ln(v) / b);
            }
            return result(log10(x));
        }
    }
    result(match f {
        MathFn::Acos => acos(x),
        MathFn::Acosh => acosh(x),
        MathFn::Asin => asin(x),
        MathFn::Asinh => asinh(x),
        MathFn::Atan => atan(x),
        MathFn::Atanh => atanh(x),
        MathFn::Cos => cos(x),
        MathFn::Cosh => cosh(x),
        MathFn::Degrees => x * (180.0 / PI.hi),
        MathFn::Exp => exp(x),
        MathFn::Ln => ln(x),
        MathFn::Log10 => log10(x),
        MathFn::Log2 => log2(x),
        MathFn::Radians => x * (PI.hi / 180.0),
        MathFn::Sin => sin(x),
        MathFn::Sinh => sinh(x),
        MathFn::Sqrt => sqrt(x),
        MathFn::Tan => tan(x),
        _ => tanh(x),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 6381956970095103 * 2^797, the classic worst case for
    /// argument reduction: cos(x) ~ -4.687e-19. Bits checked
    /// against a 1,300-digit Decimal reduction (glibc, the pin, is
    /// 8 and 14 ulp off for cos and tan here).
    #[test]
    fn hard_reduction_argument() {
        let x = 6381956970095103.0 * pow2(797);
        assert_eq!(cos(x).to_bits(), 0xbc21_4ae7_2e6b_a22f);
        assert_eq!(tan(x).to_bits(), 0xc3bd_9ba9_a797_5636);
        assert_eq!(sin(x).to_bits(), 0x3ff0_0000_0000_0000);
    }

    /// Exact powers with fractional exponents round half to even
    /// (`68661293089^1.5 = 262033^3`, a 54-bit odd integer).
    #[test]
    fn pow_exact_ties() {
        assert_eq!(pow(68661293089.0, 1.5), 17991524611989936.0);
        assert_eq!(pow(590211519630.0625, 1.25), 517320396955749.75);
        assert_eq!(pow(9.0, 17.0), 16677181699666568.0);
        assert_eq!(pow(-17.0, 13.0), -9904578032905936.0);
        // Not exact (1/3 is not a double): the general path.
        assert_eq!(pow(8.0, 1.0 / 3.0), 2.0);
        assert_eq!(pow(3.0, 0.5), sqrt(3.0));
        assert_eq!(pow(1e-200, 1.5), 1e-300);
    }

    /// Exact and special results (C99 Annex F) every build must
    /// reproduce bit for bit; the differential suite
    /// (`tests/math_v16.rs`) compares the rest with the pin.
    #[test]
    #[allow(clippy::float_cmp)]
    fn exact_and_special_values() {
        assert_eq!(sqrt(2.0).to_bits(), 0x3ff6a09e667f3bcd);
        assert_eq!(sqrt(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(sqrt(f64::from_bits(1)).to_bits(), 0x1e60000000000000);
        assert!(sqrt(-1.0).is_nan());
        assert_eq!(fmod(7.5, 2.0), 1.5);
        assert_eq!(fmod(-7.0, 3.0), -1.0);
        assert_eq!(fmod(1e308, 3.0), 2.0);
        assert!(fmod(1.0, 0.0).is_nan());
        assert_eq!(trunc(-0.5).to_bits(), (-0.0f64).to_bits());
        assert_eq!(ceil(-0.5).to_bits(), (-0.0f64).to_bits());
        assert_eq!(floor(-0.5), -1.0);
        assert_eq!(ceil(4.5e15), 4.5e15);
        assert_eq!(pow(2.0, -1074.0).to_bits(), 1);
        assert_eq!(pow(2.0, -1075.0), 0.0);
        assert_eq!(pow(4.0, -537.5), 0.0);
        assert_eq!(pow(4.0, 0.5), 2.0);
        assert_eq!(pow(10.0, 22.0), 1e22);
        assert_eq!(pow(-1.0, f64::MAX), 1.0);
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert!(pow(-8.0, 1.0 / 3.0).is_nan());
        assert_eq!(pow(0.0, -1.0), f64::INFINITY);
        assert_eq!(pow(-0.0, -1.0), f64::NEG_INFINITY);
        assert_eq!(pow(134217727.0, 2.0), 134217727.0 * 134217727.0);
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(exp(-1000.0), 0.0);
        assert_eq!(ln(1.0), 0.0);
        assert_eq!(log2(1024.0), 10.0);
        assert_eq!(log2(f64::from_bits(1)), -1074.0);
        for k in 0..23 {
            let mut p = 1.0;
            for _ in 0..k {
                p *= 10.0;
            }
            assert_eq!(log10(p), f64::from(k));
        }
        assert_eq!(sin(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(cos(0.0), 1.0);
        assert_eq!(atan2(0.0, -1.0), PI.hi);
        assert_eq!(atan2(-0.0, -1.0), -PI.hi);
        assert_eq!(atan2(1.0, 0.0), PIO2.hi);
        assert_eq!(atan(f64::INFINITY), PIO2.hi);
        assert_eq!(acos(-1.0), PI.hi);
        assert_eq!(asin(1.0), PIO2.hi);
        assert_eq!(atanh(1.0), f64::INFINITY);
        assert!(atanh(2.0).is_nan());
        assert_eq!(acosh(1.0), 0.0);
        assert_eq!(tanh(30.0), 1.0);
        assert_eq!(sinh(-711.0), f64::NEG_INFINITY);
        assert!(sin(f64::INFINITY).is_nan());
        // A few correctly rounded values (Python's Decimal at 400
        // digits).
        assert_eq!(exp(1.0).to_bits(), 0x4005bf0a8b145769);
        assert_eq!(sin(1.0).to_bits(), 0x3feaed548f090cee);
        assert_eq!(ln(10.0).to_bits(), 0x40026bb1bbb55516);
        assert_eq!(atan(1.0).to_bits(), 0x3fe921fb54442d18);
    }

    #[test]
    fn sql_argument_rules() {
        let text = |s: &[u8]| {
            let mut t = crate::arena::AVec::new();
            t.extend_from_slice(s);
            OwnedValue::Text(t)
        };
        assert!(matches!(
            eval(MathFn::Ceil, &[OwnedValue::Integer(5)]),
            OwnedValue::Integer(5)
        ));
        assert!(matches!(
            eval(MathFn::Ceil, &[text(b" 5 ")]),
            OwnedValue::Integer(5)
        ));
        assert!(matches!(
            eval(MathFn::Ceil, &[text(b"1.5")]),
            OwnedValue::Real(r) if r == 2.0
        ));
        assert!(matches!(
            eval(MathFn::Sin, &[text(b"4x")]),
            OwnedValue::Null
        ));
        assert!(matches!(
            eval(MathFn::Sqrt, &[OwnedValue::Real(-1.0)]),
            OwnedValue::Null
        ));
        assert!(matches!(
            eval(MathFn::Ln, &[OwnedValue::Integer(0)]),
            OwnedValue::Null
        ));
        assert!(matches!(
            eval(MathFn::Log, &[OwnedValue::Integer(2), text(b"8abc")]),
            OwnedValue::Real(r) if r == 3.0
        ));
        assert!(matches!(
            eval(
                MathFn::Log,
                &[OwnedValue::Integer(1), OwnedValue::Integer(8)]
            ),
            OwnedValue::Null
        ));
        assert!(matches!(eval(MathFn::Pi, &[]), OwnedValue::Real(r) if r == PI.hi));
        assert_eq!(MathFn::lookup(b"CEILING"), Some(MathFn::Ceil));
        assert_eq!(MathFn::lookup(b"power"), Some(MathFn::Pow));
        assert_eq!(MathFn::lookup(b"sign"), None);
        assert_eq!(MathFn::Log.arity(), (1, 2));
    }
}
