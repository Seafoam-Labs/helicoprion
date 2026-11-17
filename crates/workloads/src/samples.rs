//! Which quantiles are stored in result files.

use std::collections::BTreeMap;

use helicoprion_core::Metric;
use helicoprion_core::hist::Recorder;

/// Quantiles stored in result files: 1..99% in 1% steps, then the tail.
pub fn quantile_grid() -> Vec<f64> {
    let mut q: Vec<f64> = (1..100).map(|p| f64::from(p) / 100.0).collect();
    q.extend([0.995, 0.999, 0.9995, 0.9999]);
    q
}

/// Samples needed before quantile `q` is reported: at least ten
/// observations beyond it (p50: 20, p99: 1000, p99.9: 10000).
pub fn min_samples_for(q: f64) -> u64 {
    (10.0 / (1.0 - q).max(1e-9)).ceil() as u64
}

/// p50/p99/p99.9/max of one population, each only with enough samples,
/// and its quantile curve likewise truncated.
pub(crate) fn population(m: &mut BTreeMap<String, Metric>, name: &str, rec: &Recorder) -> Vec<(f64, f64)> {
    let n = rec.count();
    let us = |q: f64| rec.quantile(q).unwrap_or(0.0) / 1000.0;
    for (q, suffix) in [(0.5, "p50"), (0.99, "p99"), (0.999, "p99.9")] {
        if n >= min_samples_for(q) {
            m.insert(format!("{name}_{suffix}"), Metric::lower(us(q), "us"));
        }
    }
    if let Some(max) = rec.max() {
        m.insert(format!("{name}_max"), Metric::lower(max as f64 / 1000.0, "us").extreme());
    }
    quantile_grid().into_iter().filter(|q| n >= min_samples_for(*q)).map(|q| (q, us(q))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_requirements() {
        assert_eq!(min_samples_for(0.5), 20);
        assert_eq!(min_samples_for(0.99), 1000);
        assert_eq!(min_samples_for(0.999), 10000);
    }

    #[test]
    fn grid_is_sorted() {
        let g = quantile_grid();
        assert!(g.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(*g.last().unwrap(), 0.9999);
    }
}
