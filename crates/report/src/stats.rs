//! Small robust statistics over per-run values.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    pub median: f64,
    pub q1: f64,
    pub q3: f64,
    pub n: usize,
}

impl Spread {
    pub fn of(values: &[f64]) -> Option<Self> {
        let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        Some(Spread { median: interp(&v, 0.5), q1: interp(&v, 0.25), q3: interp(&v, 0.75), n: v.len() })
    }

    /// Interquartile range relative to the median, in percent.
    /// Relative to a zero median any spread is unbounded: `[0, 0, 0, 100,
    /// 100]` must not look stable. Zero spread around zero stays 0.
    pub fn rel_iqr_pct(&self) -> f64 {
        let iqr = self.q3 - self.q1;
        if self.median == 0.0 { if iqr == 0.0 { 0.0 } else { f64::INFINITY } } else { iqr / self.median.abs() * 100.0 }
    }
}

/// Fewer runs than this on either side: no interval is reported.
pub const MIN_RUNS_FOR_CI: usize = 3;
const BOOTSTRAP_ITERS: usize = 4000;

/// Percentile-bootstrap 95% interval of `f(median(base), median(cand))`,
/// resampling the runs of *both* sides. Deterministic (fixed seed) so the
/// same inputs always give the same report. `None` with too few runs.
pub fn bootstrap_ci(base: &[f64], cand: &[f64], f: impl Fn(f64, f64) -> Option<f64>) -> Option<(f64, f64)> {
    if base.len() < MIN_RUNS_FOR_CI || cand.len() < MIN_RUNS_FOR_CI {
        return None;
    }
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |n: usize| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % n as u64) as usize
    };
    let mut buf_b = vec![0.0; base.len()];
    let mut buf_c = vec![0.0; cand.len()];
    let mut stats = Vec::with_capacity(BOOTSTRAP_ITERS);
    for _ in 0..BOOTSTRAP_ITERS {
        for x in &mut buf_b {
            *x = base[next(base.len())];
        }
        for x in &mut buf_c {
            *x = cand[next(cand.len())];
        }
        let (mb, mc) = (median_in_place(&mut buf_b), median_in_place(&mut buf_c));
        if let Some(v) = f(mb, mc).filter(|v| v.is_finite()) {
            stats.push(v);
        }
    }
    if stats.len() < BOOTSTRAP_ITERS / 2 {
        return None;
    }
    stats.sort_by(f64::total_cmp);
    Some((interp(&stats, 0.025), interp(&stats, 0.975)))
}

fn median_in_place(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    interp(v, 0.5)
}

/// Linear-interpolated quantile on a sorted slice.
fn interp(sorted: &[f64], q: f64) -> f64 {
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_basic() {
        let s = Spread::of(&[5.0, 1.0, 3.0, 2.0, 4.0]).unwrap();
        assert_eq!(s.median, 3.0);
        assert_eq!(s.q1, 2.0);
        assert_eq!(s.q3, 4.0);
        assert_eq!(s.n, 5);
    }

    #[test]
    fn bootstrap_needs_runs_and_brackets_effect() {
        let ratio = |b: f64, c: f64| Some((c - b) / b * 100.0);
        assert!(bootstrap_ci(&[1.0, 2.0], &[1.0, 2.0, 3.0], ratio).is_none());
        // Clear 2x shift with small noise: interval excludes zero and contains ~100%.
        let base = [10.0, 10.2, 9.9, 10.1, 9.8];
        let cand = [20.0, 20.3, 19.8, 20.1, 19.9];
        let (lo, hi) = bootstrap_ci(&base, &cand, ratio).unwrap();
        assert!(lo > 0.0 && lo <= 100.0 && hi >= 100.0, "{lo}..{hi}");
        // Same distribution: interval straddles zero.
        let (lo, hi) = bootstrap_ci(&base, &base, ratio).unwrap();
        assert!(lo <= 0.0 && hi >= 0.0);
    }

    #[test]
    fn zero_median_with_spread_is_unbounded() {
        let s = Spread::of(&[0.0, 0.0, 0.0, 100.0, 100.0]).unwrap();
        assert_eq!(s.median, 0.0);
        assert!(s.rel_iqr_pct().is_infinite());
        assert_eq!(Spread::of(&[0.0, 0.0, 0.0]).unwrap().rel_iqr_pct(), 0.0);
    }

    #[test]
    fn spread_single_and_empty() {
        let s = Spread::of(&[7.0]).unwrap();
        assert_eq!((s.median, s.q1, s.q3), (7.0, 7.0, 7.0));
        assert!(Spread::of(&[]).is_none());
        assert!(Spread::of(&[f64::NAN]).is_none());
    }
}
