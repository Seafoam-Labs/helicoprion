//! Log-linear latency histogram: constant memory however long a run lasts.
//!
//! Values below 64 are exact. Above that, every power of two is split into
//! 64 equal sub-buckets, so a bucket's width is at most 1/64 (≈1.6%) of its
//! lower bound. Quantiles are reported as the bucket midpoint, clamped to the
//! exact observed min/max.

use serde::{Deserialize, Serialize};

const SUB_BITS: u32 = 6;
const SUB: u64 = 1 << SUB_BITS;
/// Enough buckets for any u64.
const N_BUCKETS: usize = ((64 - SUB_BITS as usize) + 1) * SUB as usize;

pub const ENCODING: &str = "log-linear: exact below 64, then 64 sub-buckets per power of two";

/// Recording side: dense counts, cheap `record`.
#[derive(Clone)]
pub struct Recorder {
    counts: Vec<u64>,
    count: u64,
    min: u64,
    max: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder { counts: vec![0; N_BUCKETS], count: 0, min: u64::MAX, max: 0 }
    }
}

fn index(v: u64) -> usize {
    if v < SUB {
        return v as usize;
    }
    let msb = 63 - v.leading_zeros();
    let shift = msb - SUB_BITS;
    let mantissa = v >> shift; // in [SUB, 2*SUB)
    ((shift as u64 + 1) * SUB + (mantissa - SUB)) as usize
}

/// Inclusive [lower, upper] value range of a bucket.
fn bounds(i: usize) -> (u64, u64) {
    let i = i as u64;
    if i < SUB {
        return (i, i);
    }
    let shift = i / SUB - 1;
    let m = i % SUB + SUB;
    let lower = m << shift;
    (lower, lower + ((1u64 << shift) - 1))
}

impl Recorder {
    pub fn record(&mut self, v: u64) {
        self.counts[index(v)] += 1;
        self.count += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, other: &Recorder) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Nearest-rank quantile; `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        // The extremes are known exactly.
        if rank == 1 {
            return Some(self.min as f64);
        }
        if rank == self.count {
            return Some(self.max as f64);
        }
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                let (lo, hi) = bounds(i);
                let mid = (lo as f64 + hi as f64) / 2.0;
                return Some(mid.clamp(self.min as f64, self.max as f64));
            }
        }
        Some(self.max as f64)
    }

    pub fn max(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max)
    }

    /// How many values lie in the bucket that holds `v` or in a higher
    /// one. Every value of `v` or more is counted, `v` itself included,
    /// and with them the values that share its bucket from below: the
    /// threshold is `v` to within the width of a bucket, at most 1.6%
    /// under it and exact below 64. `threshold_of` says where it is.
    pub fn count_from_bucket_of(&self, v: u64) -> u64 {
        self.counts[index(v)..].iter().sum()
    }

    /// The smallest value `count_from_bucket_of(v)` counts.
    pub fn threshold_of(v: u64) -> u64 {
        bounds(index(v)).0
    }

    /// Sparse, self-describing form for result files.
    pub fn to_histogram(&self, unit: &str) -> Histogram {
        Histogram {
            unit: unit.into(),
            encoding: ENCODING.into(),
            count: self.count,
            min: if self.count > 0 { self.min } else { 0 },
            max: self.max,
            buckets: self
                .counts
                .iter()
                .enumerate()
                .filter(|(_, c)| **c > 0)
                .map(|(i, c)| {
                    let (lo, hi) = bounds(i);
                    [lo, hi, *c]
                })
                .collect(),
        }
    }
}

/// Stored form: only non-empty buckets, each `[lower, upper, count]` (inclusive bounds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    pub unit: String,
    pub encoding: String,
    pub count: u64,
    pub min: u64,
    pub max: u64,
    pub buckets: Vec<[u64; 3]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_at_or_above_a_threshold_are_counted() {
        let mut r = Recorder::default();
        for v in [3, 40, 999_000, 1_000_000, 1_400_000, 1_400_000, 5_000_000] {
            r.record(v);
        }
        let from = |v: u64| r.count_from_bucket_of(v);
        // Exact below 64.
        assert_eq!((from(0), from(3), from(4), from(41)), (7, 7, 6, 5));
        // Above, from the lower edge of the bucket that holds the
        // threshold. A value of exactly a millisecond is counted, as
        // every value above it is; 999 µs lies in the bucket below and
        // is not.
        assert_eq!(from(1_000_000), 4);
        assert_eq!(Recorder::threshold_of(1_000_000), 999_424);
        assert_eq!(from(999_424), 4);
        assert_eq!(from(999_423), 5);
        assert_eq!(from(2_000_000), 1);
        assert_eq!(Recorder::default().count_from_bucket_of(0), 0);
        // Never above the threshold asked for, and within 1.6% of it.
        for v in [64, 1000, 1_000_000, 123_456_789] {
            let edge = Recorder::threshold_of(v);
            assert!(edge <= v && (v - edge) as f64 <= v as f64 / 64.0, "{v}: {edge}");
        }
    }

    #[test]
    fn index_bounds_roundtrip() {
        for v in [0, 1, 63, 64, 65, 127, 128, 1000, 123_456, u32::MAX as u64, u64::MAX] {
            let (lo, hi) = bounds(index(v));
            assert!(lo <= v && v <= hi, "{v} not in [{lo}, {hi}]");
            if v >= SUB {
                assert!((hi - lo) as f64 / lo as f64 <= 1.0 / SUB as f64);
            }
        }
        assert!(index(u64::MAX) < N_BUCKETS);
    }

    #[test]
    fn quantiles_close_to_exact() {
        let mut r = Recorder::default();
        for v in 1..=100_000u64 {
            r.record(v);
        }
        for (q, exact) in [(0.5, 50_000.0), (0.99, 99_000.0), (0.999, 99_900.0)] {
            let got = r.quantile(q).unwrap();
            assert!((got - exact).abs() / exact < 0.02, "q{q}: {got} vs {exact}");
        }
        assert_eq!(r.quantile(1.0).unwrap(), 100_000.0);
        assert_eq!(r.quantile(0.0).unwrap(), 1.0);
    }

    #[test]
    fn merge_and_sparse() {
        let mut a = Recorder::default();
        let mut b = Recorder::default();
        a.record(10);
        b.record(10);
        b.record(5000);
        a.merge(&b);
        let h = a.to_histogram("ns");
        assert_eq!(h.count, 3);
        assert_eq!((h.min, h.max), (10, 5000));
        assert_eq!(h.buckets.len(), 2);
        assert_eq!(h.buckets[0], [10, 10, 2]);
    }
}
