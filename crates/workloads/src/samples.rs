//! Which quantiles are stored in result files.

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
