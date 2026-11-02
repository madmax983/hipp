//! Aggregate folding (v0.4; reworked in v0.16).
//!
//! The executors materialise a group's rows and evaluate each
//! aggregate's inputs per row (`query::agg_row`: its FILTER first,
//! then its arguments and ORDER BY keys); `fold` turns those
//! values into the aggregate's result the way the pin's step /
//! finalize functions do:
//!
//! - DISTINCT keeps the first of equal values (under the argument's
//!   collation) in input order; with an aggregate ORDER BY (`v16`)
//!   the kept rows are then stably sorted by the keys (ties keep
//!   input order — the pin's sorter carries a sequence number).
//!   When the ORDER BY is the DISTINCT argument itself the pin
//!   deduplicates through the sorter, so the *last* of equal keys
//!   survives (`AggParts::unique`).
//! - `sum` / `total` / `avg` follow the pin's `sumStep`: an exact
//!   64-bit integer sum while every input is an INTEGER (numeric
//!   text counts as its number, but only text that is wholly an
//!   integer stays INTEGER), else a Kahan–Babuška–Neumaier REAL
//!   sum; an integer overflow that no later REAL input absorbed is
//!   "integer overflow" for `sum` (`SchemaKind::IntegerOverflow`)
//!   and a REAL for `total` / `avg`.
//! - `group_concat` / `string_agg` put each value's own separator
//!   before it (none before the first value, nothing for a NULL
//!   separator).
//! - `median` / `percentile` / `percentile_cont` /
//!   `percentile_disc` (`v16`) are the pin's percentile extension
//!   (built in with `SQLITE_ENABLE_PERCENTILE`): P is validated on
//!   every stepped row (NULL Y included) and must stay within
//!   0.001 of the first row's fraction; Y must be an INTEGER or a
//!   finite REAL (TEXT is refused, not converted); the result is
//!   the REAL at `P * (n - 1)` of the sorted values, interpolated
//!   linearly except for `percentile_disc`.
//!
//! Only core `f64` arithmetic; no float math intrinsics.

use crate::arena::AVec;
use crate::error::{Error, Result, SchemaKind};
use crate::query::{AggKind, AggParts, args_coll, parse_num_prefix, to_text, values_equal_coll};
use crate::record::{OwnedValue, compare_coll, fabs, text_to_number};

/// Fold one aggregate over the per-row input values of the rows
/// it steps (`rows`: one entry per row whose FILTER passed, each
/// holding `p.inputs()`' values — arguments, then ORDER BY keys).
pub fn fold<E>(p: &AggParts<'_>, rows: AVec<AVec<OwnedValue>>) -> Result<OwnedValue, E> {
    fold_rows(p, rows).map_err(|kind| Error::Schema { kind })
}

/// `fold`, generic-free (one copy whatever the storage error).
fn fold_rows(
    p: &AggParts<'_>,
    rows: AVec<AVec<OwnedValue>>,
) -> core::result::Result<OwnedValue, SchemaKind> {
    let coll = args_coll(p.args);
    let mut idx: AVec<usize> = AVec::with_capacity(rows.len());
    for i in 0..rows.len() {
        idx.push(i);
    }
    let first = |i: usize| rows.as_slice().get(i).and_then(|r| r.as_slice().first());
    // DISTINCT: the first of equal non-NULL values, in input order
    // (NULLs never count, so they are dropped here).
    if p.distinct && !p.unique {
        let mut kept: AVec<usize> = AVec::new();
        for &i in idx.iter() {
            let Some(v) = first(i).filter(|v| !matches!(v, OwnedValue::Null)) else {
                continue;
            };
            if kept
                .iter()
                .any(|&j| first(j).is_some_and(|w| values_equal_coll(w, v, coll)))
            {
                continue;
            }
            kept.push(i);
        }
        idx = kept;
    }
    #[cfg(feature = "v16")]
    if !p.order.is_empty() {
        let n = p.args.len();
        let keys = |i: usize| {
            rows.as_slice()
                .get(i)
                .and_then(|r| r.as_slice().get(n..))
                .unwrap_or(&[])
        };
        let cmp = |a: usize, b: usize| {
            crate::query::order_keys_cmp(keys(a), keys(b), p.order.iter().copied())
        };
        sort_stable(&mut idx, &|a, b| cmp(a, b) != core::cmp::Ordering::Greater);
        if p.unique {
            // Equal keys (under the key's collation) collapse to
            // the last one inserted.
            let mut kept: AVec<usize> = AVec::new();
            for &i in idx.iter() {
                match kept.as_mut_slice().last_mut() {
                    Some(l) if cmp(*l, i) == core::cmp::Ordering::Equal => *l = i,
                    _ => kept.push(i),
                }
            }
            idx = kept;
        }
    }
    let mut sel: AVec<&[OwnedValue]> = AVec::with_capacity(idx.len());
    for &i in idx.iter() {
        if let Some(r) = rows.as_slice().get(i) {
            sel.push(r.as_slice());
        }
    }
    let sel = sel.as_slice();
    let val = |r: &&[OwnedValue]| r.first().cloned().unwrap_or(OwnedValue::Null);
    match p.kind {
        AggKind::Count => {
            if p.star {
                return Ok(OwnedValue::Integer(
                    i64::try_from(sel.len()).unwrap_or(i64::MAX),
                ));
            }
            let n = sel
                .iter()
                .filter(|r| !matches!(r.first(), None | Some(OwnedValue::Null)))
                .count();
            Ok(OwnedValue::Integer(i64::try_from(n).unwrap_or(i64::MAX)))
        }
        AggKind::Sum | AggKind::Total | AggKind::Avg => {
            let mut s = SumCtx::default();
            for r in sel {
                s.step(r.first().unwrap_or(&OwnedValue::Null));
            }
            s.finish(p.kind)
        }
        AggKind::Min | AggKind::Max => {
            let mut best: Option<OwnedValue> = None;
            for r in sel {
                let v = val(r);
                if matches!(v, OwnedValue::Null) {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = compare_coll(&v.as_value(), &b.as_value(), coll);
                        let take = if p.kind == AggKind::Min {
                            ord == core::cmp::Ordering::Less
                        } else {
                            ord == core::cmp::Ordering::Greater
                        };
                        if take { v } else { b }
                    }
                });
            }
            Ok(best.unwrap_or(OwnedValue::Null))
        }
        AggKind::GroupConcat => {
            let mut out = AVec::new();
            let mut any = false;
            for r in sel {
                let Some(v) = r.first().filter(|v| !matches!(v, OwnedValue::Null)) else {
                    continue;
                };
                if any {
                    if p.args.len() > 1 {
                        // This row's own separator; a NULL one
                        // adds nothing (the pin: `ab3c`).
                        if let Some(sep) = r.get(1).and_then(to_text) {
                            out.extend_from_slice(sep.as_slice());
                        }
                    } else {
                        out.push(b',');
                    }
                }
                if let Some(t) = to_text(v) {
                    out.extend_from_slice(t.as_slice());
                }
                any = true;
            }
            Ok(if any {
                OwnedValue::Text(out)
            } else {
                OwnedValue::Null
            })
        }
        #[cfg(feature = "v16")]
        AggKind::Median
        | AggKind::Percentile
        | AggKind::PercentileCont
        | AggKind::PercentileDisc => percentile(p.kind, sel),
    }
}

/// The pin's `SumCtx`: the integer sum while it is exact, else
/// a Kahan–Babuška–Neumaier REAL sum (`r` + error term `err`).
#[derive(Default)]
struct SumCtx {
    r: f64,
    err: f64,
    i: i64,
    cnt: i64,
    approx: bool,
    ovrfl: bool,
}

/// What a value adds to a sum: its numeric type as the pin's
/// `sqlite3_value_numeric_type` sees it — INTEGER and REAL as
/// themselves; TEXT that is wholly an integer (surrounding spaces
/// allowed, up to a NUL) as that INTEGER, wholly a real as that
/// REAL; any other TEXT, and a BLOB, as the REAL of its numeric
/// prefix (0.0 without one). None for NULL.
enum SumIn {
    Int(i64),
    Real(f64),
}

fn sum_in(v: &OwnedValue) -> Option<SumIn> {
    let prefix = |t: &[u8]| SumIn::Real(parse_num_prefix(t).map(|n| n.as_f64()).unwrap_or(0.0));
    Some(match v {
        OwnedValue::Null => return None,
        OwnedValue::Integer(i) => SumIn::Int(*i),
        OwnedValue::Real(r) => SumIn::Real(*r),
        OwnedValue::Text(t) => {
            let t = until_nul(t.as_slice());
            match text_to_number(t) {
                Some(OwnedValue::Integer(i)) => SumIn::Int(i),
                Some(OwnedValue::Real(r)) => SumIn::Real(r),
                _ => prefix(t),
            }
        }
        OwnedValue::Blob(b) => prefix(b.as_slice()),
    })
}

fn until_nul(t: &[u8]) -> &[u8] {
    let end = t.iter().position(|c| *c == 0).unwrap_or(t.len());
    t.get(..end).unwrap_or(t)
}

/// |v| at or past 2^52: split before converting, so the REAL sum
/// does not lose the low bits twice (the pin's
/// `kahanBabuskaNeumaierStepInt64` / `...Init`).
fn split_big(v: i64) -> Option<(f64, f64)> {
    if v <= -4_503_599_627_370_496 || v >= 4_503_599_627_370_496 {
        // |sm| < 16384 with v's sign, so neither operation can
        // overflow.
        let sm = v.wrapping_rem(16384);
        Some((v.wrapping_sub(sm) as f64, sm as f64))
    } else {
        None
    }
}

impl SumCtx {
    fn kbn(&mut self, x: f64) {
        let s = self.r;
        let t = s + x;
        if fabs(s) > fabs(x) {
            self.err += (s - t) + x;
        } else {
            self.err += (x - t) + s;
        }
        self.r = t;
    }

    fn kbn_int(&mut self, v: i64) {
        match split_big(v) {
            Some((big, sm)) => {
                self.kbn(big);
                self.kbn(sm);
            }
            None => self.kbn(v as f64),
        }
    }

    /// Switch to the REAL sum, seeded with the integer sum so far.
    fn go_approx(&mut self) {
        let (r, err) = split_big(self.i).unwrap_or((self.i as f64, 0.0));
        self.r = r;
        self.err = err;
        self.approx = true;
    }

    fn step(&mut self, v: &OwnedValue) {
        let Some(x) = sum_in(v) else {
            return;
        };
        self.cnt = self.cnt.saturating_add(1);
        match (self.approx, x) {
            (false, SumIn::Real(r)) => {
                self.go_approx();
                self.kbn(r);
            }
            (false, SumIn::Int(i)) => match self.i.checked_add(i) {
                Some(s) => self.i = s,
                None => {
                    self.ovrfl = true;
                    self.go_approx();
                    self.kbn_int(i);
                }
            },
            (true, SumIn::Int(i)) => self.kbn_int(i),
            (true, SumIn::Real(r)) => {
                // A REAL input after an overflow turns the result
                // into an (approximate) REAL sum, as on the pin.
                self.ovrfl = false;
                self.kbn(r);
            }
        }
    }

    /// The REAL value of the sum (the error term added unless it
    /// overflowed).
    fn real(&self) -> f64 {
        if !self.approx {
            return self.i as f64;
        }
        if self.err.is_finite() {
            self.r + self.err
        } else {
            self.r
        }
    }

    fn finish(&self, kind: AggKind) -> core::result::Result<OwnedValue, SchemaKind> {
        Ok(match kind {
            AggKind::Total => OwnedValue::Real(self.real()),
            _ if self.cnt == 0 => OwnedValue::Null,
            AggKind::Avg => OwnedValue::Real(self.real() / self.cnt as f64),
            _ if self.approx && self.ovrfl => {
                return Err(SchemaKind::IntegerOverflow);
            }
            _ if self.approx => OwnedValue::Real(self.real()),
            _ => OwnedValue::Integer(self.i),
        })
    }
}

/// The percentile family over the stepped rows (`v16`): see the
/// module notes.
#[cfg(feature = "v16")]
fn percentile(
    kind: AggKind,
    sel: &[&[OwnedValue]],
) -> core::result::Result<OwnedValue, SchemaKind> {
    let err = Err;
    let (max_frac, disc) = match kind {
        AggKind::Percentile => (Some(100.0), false),
        AggKind::PercentileCont => (Some(1.0), false),
        AggKind::PercentileDisc => (Some(1.0), true),
        _ => (None, false),
    };
    let mut pct: Option<f64> = None;
    let mut ys: AVec<f64> = AVec::new();
    for r in sel {
        let rp = match max_frac {
            None => 0.5,
            Some(m) => {
                // P's numeric type as `sqlite3_value_numeric_type`
                // gives it: numeric text counts, a blob does not.
                let x = match r.get(1) {
                    Some(OwnedValue::Integer(i)) => Some(*i as f64),
                    Some(OwnedValue::Real(x)) => Some(*x),
                    Some(OwnedValue::Text(t)) => match text_to_number(until_nul(t.as_slice())) {
                        Some(OwnedValue::Integer(i)) => Some(i as f64),
                        Some(OwnedValue::Real(x)) => Some(x),
                        _ => None,
                    },
                    _ => None,
                };
                match x.map(|x| x / m) {
                    Some(q) if (0.0..=1.0).contains(&q) => q,
                    _ => return err(SchemaKind::PercentileFraction),
                }
            }
        };
        match pct {
            None => pct = Some(rp),
            Some(q) => {
                let d = q - rp;
                if !(-0.001..=0.001).contains(&d) {
                    return err(SchemaKind::PercentileFractionVaries);
                }
            }
        }
        let y = match r.first() {
            None | Some(OwnedValue::Null) => continue,
            Some(OwnedValue::Integer(i)) => *i as f64,
            Some(OwnedValue::Real(x)) => *x,
            Some(_) => return err(SchemaKind::PercentileNotNumeric),
        };
        if y.is_infinite() {
            return err(SchemaKind::PercentileInfinite);
        }
        ys.push(y);
    }
    let (Some(q), Some(last)) = (pct, ys.len().checked_sub(1)) else {
        return Ok(OwnedValue::Null);
    };
    let mut order: AVec<usize> = AVec::with_capacity(ys.len());
    for i in 0..ys.len() {
        order.push(i);
    }
    let y = |i: usize| ys.as_slice().get(i).copied().unwrap_or(0.0);
    sort_stable(&mut order, &|a, b| y(a) <= y(b));
    let mut sorted: AVec<f64> = AVec::with_capacity(ys.len());
    for &i in order.iter() {
        sorted.push(y(i));
    }
    let ys = sorted.as_slice();
    let ix = q * last as f64;
    // 0 <= ix <= last: the truncation stays in range.
    let i1 = (ix as usize).min(last);
    let v1 = ys.get(i1).copied().unwrap_or(0.0);
    if disc {
        return Ok(OwnedValue::Real(v1));
    }
    let i2 = if ix == i1 as f64 || i1 == last {
        i1
    } else {
        i1.saturating_add(1).min(last)
    };
    let v2 = ys.get(i2).copied().unwrap_or(v1);
    Ok(OwnedValue::Real(v1 + (v2 - v1) * (ix - i1 as f64)))
}

/// A stable bottom-up merge sort of `v` under `le` ("a sorts no
/// later than b"): one small, non-generic routine for every
/// aggregate sort (the aggregate ORDER BY, the percentile values),
/// instead of a `slice::sort_by` instantiation per call site.
#[cfg(feature = "v16")]
fn sort_stable(v: &mut AVec<usize>, le: &dyn Fn(usize, usize) -> bool) {
    let n = v.len();
    let mut src: AVec<usize> = v.clone();
    let mut dst: AVec<usize> = v.clone();
    let mut width = 1usize;
    while width < n {
        let mut lo = 0usize;
        while lo < n {
            let mid = lo.saturating_add(width).min(n);
            let hi = mid.saturating_add(width).min(n);
            let (mut i, mut j) = (lo, mid);
            for k in lo..hi {
                let a = src.as_slice().get(i).copied();
                let b = src.as_slice().get(j).copied();
                let take_left = match (a, b) {
                    (Some(x), Some(y)) => i < mid && (j >= hi || le(x, y)),
                    _ => i < mid,
                };
                let pick = if take_left {
                    i += 1;
                    a
                } else {
                    j += 1;
                    b
                };
                if let (Some(slot), Some(x)) = (dst.as_mut_slice().get_mut(k), pick) {
                    *slot = x;
                }
            }
            lo = hi;
        }
        core::mem::swap(&mut src, &mut dst);
        width = width.saturating_mul(2);
    }
    *v = src;
}
