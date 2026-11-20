//! The change of a share of counted events, with an interval that does
//! not take the events for independent.
//!
//! A share such as "round trips of a millisecond or more" is events out
//! of opportunities, and the events come in clumps and at a rate that
//! differs from run to run: under one scheduler the count of a run
//! varied 700 times more than independent events would, under another on
//! the same workload 1.6 times. An interval that counts every event as
//! evidence of its own is then far too narrow, and a bootstrap of rounds
//! has no width at all where a side has none in every round.
//!
//! What is done here, the *effective-count approximate interval*:
//!
//! * the estimate is the ratio of the two shares, each from the sums of
//!   its events and its opportunities over the runs compared;
//! * the dispersion of each side is how much its runs' counts vary
//!   against what independent events at its share would (Pearson's
//!   statistic over its degrees of freedom). The larger of the two is
//!   taken, not under 1, and enlarged by an allowance for having been
//!   estimated from few runs;
//! * both counts are divided by it, and the interval is the one a
//!   binomial split of that many events between the two sides gives
//!   (beta quantiles, which take the fractional counts).
//!
//! It is approximate: not an exact interval of anything, and its
//! coverage is what simulation found it to be (METHODOLOGY, "Shares of
//! counted events"), under the models simulated. It rests on each side
//! showing its own dispersion, which a side with a handful of events
//! cannot, so it is given only where both sides have `MIN_EVENTS`.

//!
//! A "run" below is one observation of a side, which the caller makes:
//! a round of a session with the counters of its runs summed, or a run
//! of results that are not one session (`MetricRow::share`). The method
//! was checked with one observation a round, and is given no other.

use crate::compare::{EventRun, MIN_EVENTS};

/// Observations a side needs for its dispersion, and for an interval.
pub const MIN_RUNS: usize = 3;

/// The quantile of the dispersion's own uncertainty that the allowance
/// goes to: the estimate is scaled to what it could be with this
/// probability of being higher still, given its degrees of freedom.
const ALLOWANCE: f64 = 0.2;

/// ln Γ(x) for x > 0 (Lanczos, g = 7).
fn ln_gamma(x: f64) -> f64 {
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection, for the small first parameters of a beta.
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let t = x + 7.5;
    let sum = C.iter().enumerate().skip(1).fold(C[0], |sum, (i, c)| sum + c / (x + i as f64));
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

/// The regularized incomplete beta function I_x(a, b), by its continued
/// fraction (Lentz), from whichever end converges.
fn inc_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    let fraction = |a: f64, b: f64, x: f64| {
        const TINY: f64 = 1e-300;
        let mut c = 1.0;
        let mut d = 1.0 - (a + b) * x / (a + 1.0);
        d = 1.0 / if d.abs() < TINY { TINY } else { d };
        let mut h = d;
        for m in 1..=10_000 {
            let m = f64::from(m);
            for numerator in [
                m * (b - m) * x / ((a + 2.0 * m - 1.0) * (a + 2.0 * m)),
                -(a + m) * (a + b + m) * x / ((a + 2.0 * m) * (a + 2.0 * m + 1.0)),
            ] {
                d = 1.0 + numerator * d;
                d = 1.0 / if d.abs() < TINY { TINY } else { d };
                c = 1.0 + numerator / c;
                c = if c.abs() < TINY { TINY } else { c };
                h *= d * c;
            }
            if (d * c - 1.0).abs() < 1e-15 {
                break;
            }
        }
        h
    };
    if x < (a + 1.0) / (a + b + 2.0) {
        front * fraction(a, b, x) / a
    } else {
        1.0 - front * fraction(b, a, 1.0 - x) / b
    }
}

/// The `p` quantile of a beta distribution, by bisection on the
/// logarithm: the quantiles wanted here can be many orders of magnitude
/// under 1, and it is their ratio to 1 that is used.
pub(crate) fn beta_quantile(p: f64, a: f64, b: f64) -> f64 {
    if inc_beta(a, b, f64::MIN_POSITIVE) >= p {
        return 0.0;
    }
    let (mut lo, mut hi) = (f64::MIN_POSITIVE.ln(), 0.0);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if inc_beta(a, b, mid.exp()) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    ((lo + hi) / 2.0).exp()
}

/// The regularized lower incomplete gamma function P(a, x).
fn inc_gamma(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let front = (a * x.ln() - x - ln_gamma(a)).exp();
    if x < a + 1.0 {
        let (mut term, mut sum, mut n) = (1.0 / a, 1.0 / a, a);
        for _ in 0..10_000 {
            n += 1.0;
            term *= x / n;
            sum += term;
            if term < sum * 1e-16 {
                break;
            }
        }
        front * sum
    } else {
        const TINY: f64 = 1e-300;
        let mut b = x + 1.0 - a;
        let mut c = 1.0 / TINY;
        let mut d = 1.0 / b;
        let mut h = d;
        for i in 1..=10_000 {
            let i = f64::from(i);
            let an = -i * (i - a);
            b += 2.0;
            d = an * d + b;
            d = 1.0 / if d.abs() < TINY { TINY } else { d };
            c = b + an / c;
            c = if c.abs() < TINY { TINY } else { c };
            h *= d * c;
            if (d * c - 1.0).abs() < 1e-15 {
                break;
            }
        }
        1.0 - front * h
    }
}

/// The `p` quantile of a chi-squared distribution with `df` degrees of
/// freedom, by bisection.
pub(crate) fn chi2_quantile(p: f64, df: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, df + 10.0 * (2.0 * df).sqrt() + 50.0);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if inc_gamma(df / 2.0, mid / 2.0) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

fn sums(runs: &[EventRun]) -> (f64, f64) {
    runs.iter().fold((0.0, 0.0), |(events, of), r| (events + r.events as f64, of + r.of as f64))
}

/// How much the counts of a side's runs vary, against what independent
/// events at the side's share would: Pearson's statistic over its
/// degrees of freedom, 1 for independent events at one rate. `None`
/// under `MIN_RUNS` runs or `MIN_EVENTS` events, where it says nothing.
///
/// It holds whatever makes runs differ: clumps of events, and a rate
/// that is not the same in every run, which grows with the count. It is
/// a property of these runs and of this many events, and not a clump
/// size that could be carried over to another side.
pub fn dispersion(runs: &[EventRun]) -> Option<f64> {
    let (events, of) = sums(runs);
    if runs.len() < MIN_RUNS || events < MIN_EVENTS as f64 || of <= 0.0 {
        return None;
    }
    let share = events / of;
    let pearson: f64 = runs
        .iter()
        .filter(|r| r.of > 0)
        .map(|r| {
            let expected = r.of as f64 * share;
            (r.events as f64 - expected).powi(2) / expected
        })
        .sum();
    Some(pearson / (runs.len() - 1) as f64)
}

/// The change of a share between two sides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    /// The two shares, baseline and then the other side, as fractions.
    pub shares: [f64; 2],
    /// The other side's share over the baseline's.
    pub ratio: f64,
    /// The 95% effective-count approximate interval of `ratio`; `None`
    /// under `MIN_RUNS` runs on a side.
    pub interval: Option<(f64, f64)>,
    /// The dispersion the counts were divided by, allowance included.
    pub dispersion: Option<f64>,
}

/// The change of a share from the runs of the baseline and of another
/// side. `None` where a side has no events or no opportunities: there
/// is no ratio then, and the caller does not score such a pair anyway.
pub fn change(baseline: &[EventRun], other: &[EventRun]) -> Option<Change> {
    let ((b, b_of), (s, s_of)) = (sums(baseline), sums(other));
    if b <= 0.0 || s <= 0.0 || b_of <= 0.0 || s_of <= 0.0 {
        return None;
    }
    let shares = [b / b_of, s / s_of];
    let ratio = shares[1] / shares[0];
    let runs = baseline.len().min(other.len());
    if runs < MIN_RUNS {
        return Some(Change { shares, ratio, interval: None, dispersion: None });
    }
    let own = [dispersion(baseline), dispersion(other)].into_iter().flatten().fold(1.0, f64::max);
    let df = (runs - 1) as f64;
    let dispersion = (own * df / chi2_quantile(ALLOWANCE, df)).max(1.0);
    let (b, s) = (b / dispersion, s / dispersion);
    // The other side's part of the effective events of both, and from
    // it the ratio of the shares.
    let ratio_at = |part: f64| part / (1.0 - part) * b_of / s_of;
    let interval = (ratio_at(beta_quantile(0.025, s, b + 1.0)), ratio_at(beta_quantile(0.975, s + 1.0, b)));
    Some(Change { shares, ratio, interval: Some(interval), dispersion: Some(dispersion) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(got: f64, want: f64) -> bool {
        (got - want).abs() <= want.abs() * 1e-7 + 1e-300
    }

    /// Against the values SciPy gives (`beta.ppf`, `chi2.ppf`).
    #[test]
    fn quantiles_agree_with_reference_values() {
        for (p, a, b, want) in [
            (0.025, 0.5, 3.0, 0.000_177_819_932_934_757_78),
            (0.975, 1.0, 119.0, 0.030_523_444_853_796_18),
            (0.025, 12.5, 40.2, 0.133_606_867_812_177_65),
            (0.975, 13.5, 39.2, 0.380_955_627_991_128_1),
            (0.5, 2.0, 2.0, 0.5),
            (0.975, 250.0, 4000.0, 0.066_090_886_663_040_97),
            (0.025, 0.119, 33.9, 6.343_330_230_631_389e-16),
        ] {
            let got = beta_quantile(p, a, b);
            assert!(close(got, want), "beta({p}, {a}, {b}): {got} against {want}");
        }
        // A quantile under the smallest number there is, is zero.
        assert_eq!(beta_quantile(0.025, 0.003, 7.1), 0.0);
        for (p, df, want) in [
            (0.2, 1.0, 0.064_184_754_667_301_6),
            (0.2, 2.0, 0.446_287_102_628_419_5),
            (0.2, 4.0, 1.648_776_618_065_969),
            (0.2, 11.0, 6.988_673_512_230_545),
            (0.5, 4.0, 3.356_693_980_033_322_4),
            (0.2, 49.0, 40.534_417_406_052_79),
        ] {
            let got = chi2_quantile(p, df);
            assert!(close(got, want), "chi2({p}, {df}): {got} against {want}");
        }
    }

    fn runs(events: &[u64], of: &[u64]) -> Vec<EventRun> {
        events.iter().zip(of).map(|(e, of)| EventRun { round: None, events: *e, qualifying: *e, of: *of }).collect()
    }

    /// The same numbers as the script the method was calibrated with
    /// (calibration/sim3.py, `phi_pair` and `interval`), for counts of
    /// the kinds a session has.
    #[test]
    fn the_interval_is_the_calibrated_one() {
        // Many events that vary a lot from run to run, against few.
        let c = change(
            &runs(&[3613, 4569, 3394, 5844, 5756], &[6_332_107, 6_100_000, 5_900_000, 6_200_000, 5_959_389]),
            &runs(&[62, 68, 74, 67, 47], &[2_598_423, 2_600_000, 2_610_000, 2_590_000, 2_618_200]),
        )
        .unwrap();
        let (lo, hi) = c.interval.unwrap();
        assert!(close(c.dispersion.unwrap(), 709.023_808_879_611_5), "{c:?}");
        assert!(close(c.ratio, 0.032_141_714_847_456_02), "{c:?}");
        assert!(close(lo, 1.433_078_781_362_190_3e-5) && close(hi, 0.349_612_809_046_337_5), "{c:?}");
        // A large share, as of missed deadlines.
        let c =
            change(&runs(&[311, 303, 328, 309, 365], &[19_802; 5]), &runs(&[4, 20, 6, 6, 6], &[19_802; 5])).unwrap();
        let (lo, hi) = c.interval.unwrap();
        assert!(close(c.dispersion.unwrap(), 12.361_257_527_329_226), "{c:?}");
        assert!(close(c.ratio, 0.025_990_099_009_900_992), "{c:?}");
        assert!(close(lo, 0.006_013_844_586_119_585) && close(hi, 0.073_248_599_326_390_34), "{c:?}");
        // More events on the other side, six runs, counts as steady as
        // independent events: the allowance alone is what widens it.
        let c =
            change(&runs(&[10, 12, 9, 11, 8, 10], &[1_000_000; 6]), &runs(&[30, 28, 33, 29, 31, 30], &[1_000_000; 6]))
                .unwrap();
        let (lo, hi) = c.interval.unwrap();
        assert!(close(c.dispersion.unwrap(), 2.134_440_459_434_244), "{c:?}");
        assert!(close(c.ratio, 3.016_666_666_666_666_6), "{c:?}");
        assert!(close(lo, 1.948_608_020_539_846_2) && close(hi, 4.800_856_686_579_59), "{c:?}");
    }

    #[test]
    fn a_dispersion_needs_runs_and_events() {
        // Independent events at one rate are about 1; here exactly even.
        assert_eq!(dispersion(&runs(&[10, 10, 10], &[1000; 3])), Some(0.0));
        // One run with everything: far from independent.
        assert!(dispersion(&runs(&[0, 0, 150, 0, 0], &[1000; 5])).unwrap() > 100.0);
        assert_eq!(dispersion(&runs(&[10, 10], &[1000; 2])), None);
        assert_eq!(dispersion(&runs(&[6, 6, 7], &[1000; 3])), None);
        assert_eq!(dispersion(&runs(&[7, 7, 7], &[0; 3])), None);
    }

    #[test]
    fn no_ratio_without_events_and_no_interval_without_runs() {
        let some = runs(&[30, 28, 33], &[1_000_000; 3]);
        assert_eq!(change(&runs(&[0, 0, 0], &[1_000_000; 3]), &some), None);
        assert_eq!(change(&some, &runs(&[0, 0, 0], &[1_000_000; 3])), None);
        assert_eq!(change(&some, &runs(&[1, 2, 3], &[0; 3])), None);
        let two = change(&some[..2], &some[..2]).unwrap();
        assert_eq!((two.ratio, two.interval, two.dispersion), (1.0, None, None));
        // Equal sides: the ratio is 1 and the interval holds it.
        let same = change(&some, &some).unwrap();
        let (lo, hi) = same.interval.unwrap();
        assert!(same.ratio == 1.0 && lo < 1.0 && 1.0 < hi, "{same:?}");
    }
}
