//! The comparison in plain words, for readers who want to know which
//! scheduler did better and by roughly how much, not how it was measured.
//!
//! Nothing here is a second opinion: a point says "longer" or "fewer" only
//! where the full report colours the change (`Comparison::clear`), and the
//! verdict is the summary's.

use std::fmt::Write as _;

use crate::compare::{Comparison, Delta, MetricRow, Verdict};
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

/// The size of a change the way one says it: "2.5×" or "37%".
fn amount(d: &Delta) -> String {
    let r = d.ratio;
    if r.is_finite() && r >= 2.0 {
        format!("{}×", times(r))
    } else if r > 0.0 && r <= 0.5 {
        format!("{}×", times(1.0 / r))
    } else if d.median.abs() < 10.0 {
        format!("{:.1}%", d.median.abs())
    } else {
        format!("{:.0}%", d.median.abs())
    }
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
/// where the report does not score the change, however far apart the two
/// medians look. `words` are (better, worse).
fn point(c: &Comparison, workload: &str, i: usize, metric: &str, label: &str, words: (&str, &str)) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == metric)?;
    let d = row.improvement(c.baseline, i)?;
    let (b, v) = medians(row, c.baseline, i)?;
    let values = format!("{} → {}{}", fmt_value(b), fmt_value(v), unit(&row.unit));
    Some(if c.clear(row, i, &d) {
        let word = if d.median > 0.0 { words.0 } else { words.1 };
        format!("{label}: {} {word} ({values})", amount(&d))
    } else {
        format!("{label}: no reliable difference ({values})")
    })
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
            Brief { series: i, workload: s.workload.clone(), title: title(w), verdict: s.verdict(), points }
        })
        .collect()
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
    for b in &briefs {
        let _ =
            writeln!(out, "\n{} · {}: {}", c.series[b.series].label, b.title, verdict_phrase(b.verdict).to_uppercase());
        for p in &b.points {
            let _ = writeln!(out, "  • {p}");
        }
    }
    let _ = writeln!(out, "\n{WHAT}");
    out
}
