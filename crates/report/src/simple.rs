//! The comparison in plain words, for readers who want to know which
//! scheduler did better and by roughly how much, not how it was measured.
//!
//! Nothing here is a second opinion: a point says "longer" or "fewer" only
//! where the full report colours the change (`Comparison::clear`), and the
//! verdict is the summary's.

use std::fmt::Write as _;

use crate::compare::{Comparison, Delta, MetricRow, UnclearReason, Verdict};
use crate::svg::{fmt_value, times};

/// One candidate against the baseline on one workload.
#[derive(Debug, Clone)]
pub struct Brief {
    /// Index into `Comparison::series`.
    pub series: usize,
    pub workload: String,
    /// The workload in plain words.
    pub title: String,
    pub verdict: Verdict,
    /// One line per thing a reader cares about, most important first.
    pub points: Vec<String>,
    /// What changes how the points should be read: the two schedulers did
    /// not produce the same mix of round trips.
    pub note: Option<String>,
}

/// What a workload measures, for someone who has not read the README.
fn title(workload: &str) -> String {
    match workload {
        "wake-pingpong" => "Idle system".into(),
        "wake-pingpong/loaded" => "Busy system (every CPU kept busy)".into(),
        other => other.to_string(),
    }
}

pub fn verdict_phrase(v: Verdict) -> &'static str {
    match v {
        Verdict::Better => "better",
        Verdict::Worse => "worse",
        Verdict::Mixed => "mixed: better in some ways, worse in others",
        // Not "about the same": too uncertain to name a winner is not
        // evidence that the two are equal.
        Verdict::NoClearDifference => "no clear difference",
    }
}

/// The size of a change the way one says it: "2.5×" or "37%", from its
/// goodness ratio and its percentage.
fn amount_of(ratio: f64, change_pct: f64) -> String {
    if ratio.is_finite() && ratio >= 2.0 {
        format!("{}×", times(ratio))
    } else if ratio > 0.0 && ratio <= 0.5 {
        format!("{}×", times(1.0 / ratio))
    } else {
        percent(change_pct)
    }
}

fn percent(change_pct: f64) -> String {
    if change_pct.abs() < 10.0 { format!("{:.1}%", change_pct.abs()) } else { format!("{:.0}%", change_pct.abs()) }
}

fn amount(d: &Delta) -> String {
    amount_of(d.ratio, d.median)
}

/// The interval of a change as a range of sizes, smaller first: "2.0× to
/// 2.9×", "39% to 97%". For an interval that lies on one side of no change.
/// This is the uncertainty of the estimate (the 95% interval of the median
/// effect), not the smallest and largest effect seen in single rounds.
///
/// Both ends are in one unit. Where one end is a multiple (2× or more),
/// the other is given as a multiple too, even below 2×: "1.2× to 62×", not
/// "19% to 62×", which reads as two different things.
fn amount_range(d: &Delta, lo: f64, hi: f64) -> String {
    let mut ends = [(d.ratio_of(lo), lo), (d.ratio_of(hi), hi)];
    let usable = |r: f64| r > 0.0 && r.is_finite();
    let size = |r: f64| if usable(r) { r.ln().abs() } else { f64::INFINITY };
    ends.sort_by(|a, b| size(a.0).total_cmp(&size(b.0)));
    let is_multiple = |r: f64| usable(r) && !(0.5 < r && r < 2.0);
    let as_multiples = ends.iter().any(|e| is_multiple(e.0)) && ends.iter().all(|e| usable(e.0));
    // Otherwise percentages for both: either neither end is a multiple, or
    // one end has no ratio at all (nothing left: 100% fewer).
    let end = |(ratio, change): (f64, f64)| {
        if as_multiples { format!("{}×", times(ratio.max(1.0 / ratio))) } else { percent(change) }
    };
    format!("{} to {}", end(ends[0]), end(ends[1]))
}

/// A factor for an "at least" claim: cut off at the digits shown, never
/// rounded, so that the claim holds ("at least 1.49×" for 1.4993, not
/// "1.5×"). Two decimals below 2×, where one would often leave "1.0×".
fn floor_times(x: f64) -> String {
    let cut = |digits: i32| {
        let scale = 10f64.powi(digits);
        (x * scale).floor() / scale
    };
    if x >= 10.0 {
        format!("{:.0}×", cut(0))
    } else if x >= 2.0 {
        format!("{:.1}×", cut(1))
    } else {
        format!("{:.2}×", cut(2))
    }
}

/// The same for a percentage.
fn floor_percent(change_pct: f64) -> String {
    let p = change_pct.abs();
    if p < 10.0 { format!("{:.1}%", (p * 10.0).floor() / 10.0) } else { format!("{:.0}%", p.floor()) }
}

/// ", at least 1.49×" for a scored change whose estimate is far above the
/// cautious end of its 95% interval: "54× shorter" alone sounds surer than
/// an interval of 1.5× to 55× allows. Far means that the estimate is at
/// least twice as far from no change as the cautious end (as a factor
/// minus one: 54× is 53 away, 1.5× is 0.5 away). In the unit of the
/// estimate: a multiple next to a multiple, a percentage next to a
/// percentage; cut off, not rounded, since "at least" must not overstate
/// the end. `None` where the interval is tight on that side, or where
/// there is none or an end has no usable ratio.
fn at_least(d: &Delta) -> Option<String> {
    let (lo, hi) = d.ci?;
    // Distance from no change as a factor of 1 or more. A ratio that is
    // zero, not finite, or so small that its inverse overflows gives none.
    let factor = |ratio: f64| {
        let f = if ratio > 0.0 { ratio.max(1.0 / ratio) } else { f64::NAN };
        f.is_finite().then_some(f)
    };
    let estimate = factor(d.ratio)?;
    let cautious = [lo, hi]
        .into_iter()
        .map(|change| Some((factor(d.ratio_of(change))?, change)))
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
    if estimate - 1.0 < 2.0 * (cautious.0 - 1.0) {
        return None;
    }
    let shown = if estimate >= 2.0 { floor_times(cautious.0) } else { floor_percent(cautious.1) };
    Some(format!(", at least {shown}"))
}

fn unit(u: &str) -> &str {
    match u {
        "us" => " µs",
        "1/s" => " per second",
        _ => "",
    }
}

fn medians(row: &MetricRow, baseline: usize, i: usize) -> Option<(f64, f64)> {
    let m = |k: usize| row.cells.get(k).and_then(Option::as_ref).map(|c| c.spread.median);
    Some((m(baseline)?, m(i)?))
}

/// "Label: 2.6× longer (14.0 → 36.1 µs)", or "no reliable difference"
/// with the reason where the report does not score the change, however
/// far apart the two medians look. `words` are (better, worse).
///
/// In between is a change the report holds back only because its size
/// differed from round to round (`Comparison::held_back_by_unevenness`).
/// Its interval excludes no change, so the direction has the same support
/// as a scored one's; only the size is not one number. Saying "no reliable
/// difference" next to "1.23M → 521.2k" would be false. The line gives
/// three things without mixing them up: the direction, that the effect
/// was too uneven between rounds to score, and the estimate's 95%
/// interval, named as such, since it is not the range of the rounds. This
/// does not score the change: the verdict and the tally still leave it out.
fn point(c: &Comparison, workload: &str, i: usize, metric: &str, label: &str, words: (&str, &str)) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == metric)?;
    let d = row.improvement(c.baseline, i)?;
    let (b, v) = medians(row, c.baseline, i)?;
    let values = format!("{} → {}{}", fmt_value(b), fmt_value(v), unit(&row.unit));
    let word = if d.median > 0.0 { words.0 } else { words.1 };
    Some(match (c.unclear_reason(row, i, &d), d.ci) {
        (None, _) => format!("{label}: {} {word}{} ({values})", amount(&d), at_least(&d).unwrap_or_default()),
        (Some(UnclearReason::Uneven(_)), Some((lo, hi))) => format!(
            "{label}: {word}; the effect varied too much between rounds to score (estimated effect, 95% interval: {}; {values})",
            amount_range(&d, lo, hi)
        ),
        (Some(reason), _) => format!("{label}: no reliable difference; {} ({values})", why(c, &d, reason)),
    })
}

/// Why a change is not scored, for a reader who sees "1434 → 42.2 µs" next
/// to "no reliable difference" and wonders whether the benchmark is broken.
/// The reason is `Comparison::unclear_reason`'s, the one the scoring
/// itself went by; this only puts it in words.
fn why(c: &Comparison, d: &Delta, reason: UnclearReason) -> String {
    match reason {
        UnclearReason::SingleSample => "a single extreme value is never scored".into(),
        UnclearReason::MinorPopulation => "too few relevant wake-ups".into(),
        UnclearReason::UndefinedRounds => "the baseline measured zero in some rounds".into(),
        UnclearReason::TooFewRounds if d.paired_rounds.is_some() || d.unmatched_rounds => {
            "too few paired rounds".into()
        }
        UnclearReason::TooFewRounds => "too few runs".into(),
        UnclearReason::IntervalIncludesNoChange => "the interval includes no change".into(),
        UnclearReason::BelowThreshold => {
            format!("difference below the reporting threshold of {}%", c.opts.min_effect_pct)
        }
        UnclearReason::Uneven(_) => "the effect varied too much between rounds to score".into(),
        UnclearReason::Unstable => "results were too unstable to compare reliably".into(),
    }
}

/// Work the CPU hogs got done next to the test: the other half of the
/// trade-off under load. It is informational in the report and passes no
/// test of its own, so the line says "observed, not scored".
fn background(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == "hog_work_per_s")?;
    let comparable = |k: usize| row.cells.get(k).and_then(Option::as_ref).is_some_and(|c| c.comparable);
    let (b, v) = medians(row, c.baseline, i)?;
    if !(b > 0.0 && comparable(c.baseline) && comparable(i)) {
        return None;
    }
    let pct = (v - b) / b * 100.0;
    let what = if pct.abs() >= c.opts.min_effect_pct {
        format!("{:.0}% {}", pct.abs(), if pct > 0.0 { "higher" } else { "lower" })
    } else {
        "about the same".to_string()
    };
    Some(format!("Background CPU work: {what} (observed, not scored)"))
}

/// Per candidate and workload, in the summary's order.
pub fn briefs(c: &Comparison) -> Vec<Brief> {
    c.summary()
        .into_iter()
        .map(|s| {
            let (w, i) = (s.workload.as_str(), s.series);
            let slowest = point(c, w, i, "rtt_blocked_p99.9", "Slowest wake-ups (1 in 1000)", ("shorter", "longer"))
                .or_else(|| point(c, w, i, "rtt_blocked_p99", "Slow wake-ups (1 in 100)", ("shorter", "longer")));
            let mut points: Vec<String> = [
                // Round trips, not wake-ups: one round trip can take one wake-up
                // or two, or none, and `woken_per_s` counts those.
                point(c, w, i, "roundtrips_per_s", "Round trips completed", ("more", "fewer")),
                point(c, w, i, "rtt_blocked_p50", "Typical wake-up delay", ("shorter", "longer")),
                slowest,
                background(c, w, i),
            ]
            .into_iter()
            .flatten()
            .collect();
            if points.is_empty() {
                // A workload this module has no words for: its largest clear changes.
                points = s.top.iter().map(|(m, d)| format!("{m}: {}", crate::svg::fmt_delta(d))).collect();
            }
            points.push(format!(
                "Of {} measurements, {} clearly better and {} clearly worse",
                s.compared, s.better, s.worse
            ));
            let note = mix_note(c, w, i);
            Brief { series: i, workload: s.workload.clone(), title: title(w), verdict: s.verdict(), points, note }
        })
        .collect()
}

/// The full report's warning about a different mix of round trips, next
/// to the result it qualifies: "11% of round trips needed a wake-up, 99%
/// for the baseline" changes what "wake-up delay 2× shorter" means, and a
/// reader of the plain view does not open the warnings. Nothing is
/// detected here: this puts `Comparison::mixes` in words, one population
/// per part.
///
/// Which one: round trips that needed a wake-up (`rtt_blocked`) whenever
/// their share differs, since the delays shown are theirs. Otherwise the
/// population whose share moved the most, in percentage points of all
/// round trips: with 5.3% against 81% of round trips woken across CPUs
/// and 8.6% against 1.9% needing no wake-up, the first is what the two
/// runs differ in. Equal moves keep the order of the table below. The
/// note names a difference in behaviour; it does not say that the
/// difference causes the delays.
///
/// What follows from the finding is said about the numbers the part
/// shows, which are the delays of all round trips that needed a wake-up
/// (`rtt_blocked`). Only a different share of exactly those makes the
/// delays shown "not the same mix". A different share of round trips
/// without a wake-up is about throughput and delays together, and the
/// same-CPU and cross-CPU kinds are both inside the delays shown, in
/// different proportions.
fn mix_note(c: &Comparison, workload: &str, series: usize) -> Option<String> {
    // (population, what such a round trip did, what it means for the part)
    const KINDS: [(&str, &str, &str); 4] = [
        ("rtt_blocked", "needed a wake-up", "The wake-up delays here do not describe the same mix of round trips."),
        (
            "rtt_fast",
            "needed no wake-up",
            "The mix of round trips differs, so read the round trips completed and the wake-up delays together.",
        ),
        (
            "rtt_blocked_cross_cpu",
            "needed a wake-up with the two tasks on different CPUs",
            "The wake-up delays here cover wake-ups on one CPU and across CPUs together, in different proportions \
for the two.",
        ),
        (
            "rtt_blocked_same_cpu",
            "needed a wake-up with the two tasks on one CPU",
            "The wake-up delays here cover wake-ups on one CPU and across CPUs together, in different proportions \
for the two.",
        ),
    ];
    let of = |population: &str| {
        c.mixes.iter().find(|m| m.series == series && m.workload == workload && m.population == population)
    };
    let moved = |m: &crate::compare::Mix| (m.share - m.baseline_share).abs();
    let mut found = KINDS.iter().filter_map(|(p, did, means)| Some((of(p)?, *did, *means)));
    let first = found.next()?;
    let (m, did, means) = if first.0.population == KINDS[0].0 {
        first
    } else {
        // The largest move; the earlier in `KINDS` of equal ones.
        found.fold(first, |best, next| if moved(next.0) > moved(best.0) { next } else { best })
    };
    let share = crate::svg::fmt_share;
    Some(format!(
        "{}{} of round trips {did}, versus {} for {}. {means}",
        if m.share < m.baseline_share { "Only " } else { "" },
        share(m.share),
        share(m.baseline_share),
        c.series[c.baseline].label
    ))
}

/// For `--simple`, where warnings are folded away with the full report:
/// that there are some, without the diagnostics themselves. A warning can
/// change how a result should be read (e.g. the two schedulers produced a
/// different mix of round trips), so its existence is never hidden.
pub fn caution(c: &Comparison) -> Option<String> {
    let n = c.warnings.len();
    (n > 0).then(|| {
        format!(
            "Some measurements require caution: see the {n} warning{} in the full report.",
            if n == 1 { "" } else { "s" }
        )
    })
}

/// For `--simple`, like `caution`, for the notes on how the systems behind
/// the results differ (another kernel, CPU count, governor…). Such a
/// difference is not a flaw in a measurement, so it is said apart from the
/// warnings; but results from different setups are not a comparison of
/// schedulers alone, and the reader should know before trusting a verdict.
pub fn environment(c: &Comparison) -> Option<String> {
    let n = c.env_notes.len();
    (n > 0).then(|| {
        format!(
            "The results were not all taken on the same setup: see the {n} note{} in the full report.",
            if n == 1 { "" } else { "s" }
        )
    })
}

/// Why there is no "In short": said in the terminal and, with `--simple`,
/// in the report, where the full report is folded away and a page without
/// any explanation would be left.
pub fn nothing_to_compare(c: &Comparison) -> String {
    let base = &c.series[c.baseline].label;
    if c.series.len() < 2 {
        format!("Nothing to compare with {base} yet: a comparison needs results from two schedulers.")
    } else {
        // Two or more files, yet no metric both sides have under the same
        // conditions (different workload parameters, or no workload in common).
        format!(
            "The results cannot be compared with {base}: they were not measured the same way. The full report says what differs."
        )
    }
}

/// What the test does, in one sentence.
pub const WHAT: &str = "The test makes pairs of tasks wake each other up in turn, as fast as they can: \
what a snappy desktop, a game or a busy server depends on.";

/// What "at least" stands for, where a line says it: the cautious end of
/// an estimated interval reads like a promise otherwise.
pub const AT_LEAST: &str = "\"At least\" is the cautious end of the 95% interval around the estimate: how small \
the change may plausibly be, not a guaranteed minimum.";

/// `AT_LEAST`, if any of the briefs has such a line.
pub fn at_least_note(briefs: &[Brief]) -> Option<&'static str> {
    briefs.iter().flat_map(|b| &b.points).any(|p| p.contains(", at least ")).then_some(AT_LEAST)
}

/// The briefs of one scheduler together, schedulers in report order: one
/// card per scheduler, with a part per workload.
pub fn by_scheduler(briefs: &[Brief]) -> Vec<(usize, Vec<&Brief>)> {
    let mut groups: Vec<(usize, Vec<&Brief>)> = Vec::new();
    for b in briefs {
        match groups.iter_mut().find(|(series, _)| *series == b.series) {
            Some((_, of)) => of.push(b),
            None => groups.push((b.series, vec![b])),
        }
    }
    groups
}

/// For the terminal.
pub fn text(c: &Comparison) -> String {
    let briefs = briefs(c);
    let base = &c.series[c.baseline].label;
    let mut out = String::new();
    if briefs.is_empty() {
        let _ = writeln!(out, "{}", nothing_to_compare(c));
        return out;
    }
    let _ = writeln!(out, "In short, compared with {base}:");
    for (series, of) in by_scheduler(&briefs) {
        let _ = writeln!(out, "\n{}", c.series[series].label);
        for b in of {
            let _ = writeln!(out, "  {}: {}", b.title, verdict_phrase(b.verdict).to_uppercase());
            for p in &b.points {
                let _ = writeln!(out, "    • {p}");
            }
            if let Some(note) = &b.note {
                let _ = writeln!(out, "    Note: {note}");
            }
        }
    }
    if let Some(note) = at_least_note(&briefs) {
        let _ = writeln!(out, "\n{note}");
    }
    let _ = writeln!(out, "\n{WHAT}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(series: usize, workload: &str) -> Brief {
        Brief {
            series,
            workload: workload.into(),
            title: title(workload),
            verdict: Verdict::NoClearDifference,
            points: Vec::new(),
            note: None,
        }
    }

    /// A scored change far above the cautious end of its interval says so.
    #[test]
    fn a_wide_interval_adds_the_cautious_end() {
        use helicoprion_core::Better;
        let d = |better, median: f64, ratio: f64, ci| Delta {
            median,
            ci,
            ratio,
            paired_rounds: Some(5),
            unmatched_rounds: false,
            paired_spread: None,
            undefined_rounds: 0,
            better,
        };
        let lower = |median, ratio, ci| at_least(&d(Better::Lower, median, ratio, Some(ci)));
        let higher = |median, ratio, ci| at_least(&d(Better::Higher, median, ratio, Some(ci)));
        // 1434 -> 26.8 µs, interval about 1.5× to 55× shorter. A change of
        // 33.3% is a factor of 1.4993: "at least 1.5×" would overstate it.
        assert_eq!(lower(98.1, 54.0, (33.3, 98.2)).as_deref(), Some(", at least 1.49×"));
        assert_eq!(lower(98.1, 54.0, (31.5, 98.2)).as_deref(), Some(", at least 1.45×"));
        // Close to no change the second decimal keeps the claim useful.
        assert_eq!(lower(98.1, 54.0, (3.8, 98.2)).as_deref(), Some(", at least 1.03×"));
        // Larger factors: one decimal, then none, always cut off.
        assert_eq!(lower(98.1, 54.0, (79.0, 98.2)).as_deref(), Some(", at least 4.7×"));
        assert_eq!(lower(99.5, 200.0, (94.9, 99.6)).as_deref(), Some(", at least 19×"));
        // 1434 -> 2802 µs: 98% longer, interval 91% to 68×. The estimate
        // sits at the cautious end; the wide side is the other one.
        assert_eq!(lower(-97.7, 1.0 / 1.977, (-6783.1, -91.1)), None);
        // Tight intervals: 2.2× fewer (1.9× to 2.5×), 13% fewer (12% to 13.5%).
        assert_eq!(higher(-54.5, 1.0 / 2.2, (-60.0, -47.4)), None);
        assert_eq!(higher(-12.7, 0.873, (-13.5, -12.0)), None);
        // In percent next to a percentage: 50% more, interval 6% to 90%.
        assert_eq!(higher(50.0, 1.5, (6.0, 90.0)).as_deref(), Some(", at least 6.0%"));
        assert_eq!(higher(50.0, 1.5, (6.79, 90.0)).as_deref(), Some(", at least 6.7%"));
        assert_eq!(higher(90.0, 1.9, (19.9, 95.0)).as_deref(), Some(", at least 19%"));
        // 9× better, interval 4.6× to 9.7×: 8 away against 3.6, over twice.
        assert_eq!(lower(88.9, 9.0, (78.3, 89.7)).as_deref(), Some(", at least 4.6×"));
        // No interval, or an end without a ratio: nothing to add.
        assert_eq!(at_least(&d(Better::Lower, 98.1, 54.0, None)), None);
        assert_eq!(higher(-80.0, 0.2, (-100.0, -20.0)), None);
        // An estimate so small that its inverse overflows.
        assert_eq!(higher(-99.9, f64::MIN_POSITIVE / 4.0, (-99.95, -20.0)), None);
    }

    #[test]
    fn interval_ends_share_one_unit() {
        use helicoprion_core::Better;
        let d = |better| Delta {
            median: 0.0,
            ci: None,
            ratio: 1.0,
            paired_rounds: None,
            unmatched_rounds: false,
            paired_spread: None,
            undefined_rounds: 0,
            better,
        };
        // Lower is better. 1434 -> 27.0 µs: about 16% to 98.4% shorter.
        assert_eq!(amount_range(&d(Better::Lower), 16.7, 98.39), "1.2× to 62×");
        // 1434 -> 2802 µs: 73.6% to 8254.3% longer.
        assert_eq!(amount_range(&d(Better::Lower), -8254.3, -73.6), "1.7× to 84×");
        // Both below 2×: percentages, as before.
        assert_eq!(amount_range(&d(Better::Lower), -83.9, -42.6), "43% to 84%");
        // Both multiples, higher is better: 1.23M -> 521.2k round trips.
        assert_eq!(amount_range(&d(Better::Higher), -65.5, -50.0), "2.0× to 2.9×");
        assert_eq!(amount_range(&d(Better::Higher), 40.0, 250.0), "1.4× to 3.5×");
        // An end with no usable ratio (nothing left: -100%) cannot be a
        // multiple, so the range stays in percent.
        assert_eq!(amount_range(&d(Better::Higher), -100.0, -60.0), "60% to 100%");
    }

    /// `briefs()` yields a scheduler's workloads one after another, but the
    /// grouping must not depend on that: entries of two schedulers that
    /// arrive interleaved still end up one card each, schedulers and their
    /// workloads in order of first appearance.
    #[test]
    fn interleaved_briefs_are_grouped_by_scheduler() {
        let (cake, lavd) = (2, 1);
        let briefs = [
            brief(cake, "wake-pingpong"),
            brief(lavd, "wake-pingpong"),
            brief(cake, "wake-pingpong/loaded"),
            brief(lavd, "wake-pingpong/loaded"),
        ];
        let groups: Vec<(usize, Vec<&str>)> = by_scheduler(&briefs)
            .into_iter()
            .map(|(series, of)| (series, of.into_iter().map(|b| b.workload.as_str()).collect()))
            .collect();
        assert_eq!(
            groups,
            [
                (cake, vec!["wake-pingpong", "wake-pingpong/loaded"]),
                (lavd, vec!["wake-pingpong", "wake-pingpong/loaded"]),
            ]
        );
        assert!(by_scheduler(&[]).is_empty());
    }
}
