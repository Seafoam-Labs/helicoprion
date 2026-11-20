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
        "periodic-wake" => "Tasks on a timer, idle system".into(),
        "periodic-wake/loaded" => "Tasks on a timer, busy system (every CPU kept busy)".into(),
        "wake-burst/half" => "A group of tasks woken at once, half as many as CPUs".into(),
        "wake-burst/full" => "A group of tasks woken at once, as many as CPUs".into(),
        "wake-burst/double" => "A group of tasks woken at once, twice as many as CPUs".into(),
        "cgroup-fair/equal" => "Two control groups, same weight, one with three times the tasks".into(),
        "cgroup-fair/weighted" => "Two control groups, same tasks, one with three times the weight".into(),
        "idle-wake/handoff" => "A task woken on a sleeping machine, by one that sleeps again".into(),
        "idle-wake/busy" => "A task woken on a sleeping machine, by one that goes on working".into(),
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
    let word = if d.median > 0.0 { words.0 } else { words.1 };
    // A share in percent: "35% fewer" for 1.0% against 0.65% would be a
    // percentage of a percentage, and read as points. The two shares say
    // it, with the direction, and with the factor where it is a multiple.
    if row.unit == "%" {
        let values = format!("{}% → {}% {}", fmt_value(b), fmt_value(v), share_of(metric));
        let factor = d.ratio.max(1.0 / d.ratio);
        let size = if factor.is_finite() && factor >= 2.0 { format!("{}× ", times(factor)) } else { String::new() };
        return Some(match c.unclear_reason(row, i, &d) {
            None => format!("{label}: {size}{word} ({values})"),
            Some(UnclearReason::Uneven(_)) => {
                format!("{label}: {word}; the effect varied too much between rounds to score ({values})")
            }
            Some(reason) => format!("{label}: no reliable difference; {} ({values})", why(c, &d, reason)),
        });
    }
    let values = format!("{} → {}{}", fmt_value(b), fmt_value(v), unit(&row.unit));
    Some(match (c.unclear_reason(row, i, &d), d.ci) {
        (None, _) => format!("{label}: {} {word}{} ({values})", amount(&d), at_least(&d).unwrap_or_default()),
        (Some(UnclearReason::Uneven(_)), Some((lo, hi))) => format!(
            "{label}: {word}; the effect varied too much between rounds to score (estimated effect, 95% interval: {}; {values})",
            amount_range(&d, lo, hi)
        ),
        (Some(reason), _) => format!("{label}: no reliable difference; {} ({values})", why(c, &d, reason)),
    })
}

/// What a share in percent is a share of.
fn share_of(metric: &str) -> &'static str {
    match metric {
        "overrun_pct" => "of wake-ups",
        "rtt_blocked_slow_pct" => "of the round trips that needed a wake-up",
        "weight_accuracy_pct" => "of the intended ratio",
        "cpu_used_pct" => "of the machine's CPU time",
        _ => "of periods",
    }
}

/// A share of failures that is scored: periodic-wake's missed deadlines,
/// wake-burst's wake-ups lost to overrun.
struct Failures {
    metric: &'static str,
    label: &'static str,
    /// "missed" in "no run missed any" and "single runs missed up to".
    verb: &'static str,
}

const DEADLINES: Failures = Failures { metric: "deadline_miss_pct", label: "Deadlines missed", verb: "missed" };
const SLOW_TRIPS: Failures =
    Failures { metric: "rtt_blocked_slow_pct", label: "Round trips of a millisecond or more", verb: "had" };
const OVERRUNS: Failures =
    Failures { metric: "overrun_pct", label: "Wake-ups lost because the task was still busy", verb: "lost" };

/// How often a workload's tasks failed in the way `f` names. Zero is
/// common here, and a baseline of zero leaves no change to take, so the
/// line is worded for those cases instead of being left out.
///
/// What is compared are medians over the runs. A median of zero is not
/// "no deadline missed": three clean runs and two with misses have a
/// median of zero, and a benchmark for rare lateness must not call that
/// none. "None" is said only when every run of a side has none; otherwise
/// the line says "median" and how much single runs missed.
fn failures(c: &Comparison, workload: &str, i: usize, f: &Failures) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == f.metric)?;
    let (b, v) = medians(row, c.baseline, i)?;
    let base = &c.series[c.baseline].label;
    let (label, verb, of) = (f.label, f.verb, share_of(f.metric));
    // The most any single run of a side missed.
    let worst = |k: usize| {
        row.cells.get(k).and_then(Option::as_ref).map_or(0.0, |cell| cell.values.iter().copied().fold(0.0, f64::max))
    };
    let (worst_b, worst_v) = (worst(c.baseline), worst(i));
    let runs = |worst: f64| {
        if worst == 0.0 {
            format!("no run {verb} any")
        } else {
            format!("single runs {verb} up to {}%", fmt_value(worst))
        }
    };
    match (b == 0.0, v == 0.0) {
        (true, true) if worst_b == 0.0 && worst_v == 0.0 => Some(format!("{label}: none in any run under either")),
        (true, true) => Some(format!(
            "{label}: a median of 0% {of} under both ({base}: {}; here: {})",
            runs(worst_b),
            runs(worst_v)
        )),
        (true, false) => Some(format!(
            "{label}: a median of {}% {of}, and a median of 0% under {base} ({}); a change from zero is not scored",
            fmt_value(v),
            runs(worst_b)
        )),
        _ => point(c, workload, i, f.metric, label, ("fewer", "more")),
    }
}

/// How many bursts wake-burst released under each side. Each burst is
/// due a period after the one before, so a scheduler under which the
/// waker runs later is asked for less work. Not scored, and no limit is
/// drawn: both rates are said, so that a reader sees whether the two
/// sides were given the same thing to do.
fn burst_rate(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == "bursts_per_s")?;
    let (b, v) = medians(row, c.baseline, i)?;
    Some(format!(
        "Bursts released: {} a second, {} under {} (observed, not scored; with fewer bursts a scheduler was asked \
         for less work)",
        fmt_value(v),
        fmt_value(b),
        c.series[c.baseline].label
    ))
}

/// Where idle-wake's woken task ran: on the CPU of the thread that woke
/// it, which is awake, or elsewhere, which may have to be woken up
/// first. Observed, not scored: it is how a scheduler gets to its delay.
/// With how deep the machine slept, where the kernel says: a run in
/// which it did not is not what the part's title says.
fn placement(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let median = |metric: &str, k: usize| {
        let row = c.rows.iter().find(|r| r.workload == workload && r.metric == metric)?;
        row.cells.get(k).and_then(Option::as_ref).map(|c| c.spread.median)
    };
    let (base, here) = (c.series[c.baseline].label.as_str(), median("same_cpu_pct", i)?);
    let mut line = format!("Ran on the CPU of the thread that woke it: {here:.0}% of wake-ups");
    if let Some(b) = median("same_cpu_pct", c.baseline) {
        let _ = write!(line, " ({b:.0}% under {base})");
    }
    if let Some(deep) = median("idle_deepest_pct", i) {
        let _ = write!(line, "; the CPUs were in their deepest idle state {deep:.0}% of the time");
        if let Some(b) = median("idle_deepest_pct", c.baseline) {
            let _ = write!(line, " ({b:.0}% under {base})");
        }
    }
    line.push_str(" (observed, not scored)");
    Some(line)
}

/// How many wake-ups idle-wake's wakers sent under each side. They are
/// due at fixed instants, so a scheduler under which the wakers come
/// back from their timers late, or after the end of the window, was
/// asked for less. Not scored, and no limit is drawn: both rates are
/// said, as for wake-burst's bursts.
fn wake_rate(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == "wakes_per_s")?;
    let (b, v) = medians(row, c.baseline, i)?;
    Some(format!(
        "Wake-ups sent: {} a second, {} under {} (observed, not scored; with fewer a scheduler was asked for less)",
        fmt_value(v),
        fmt_value(b),
        c.series[c.baseline].label
    ))
}

/// How late idle-wake's wakers were for their timers: the other wake-up
/// of each round, on the same sleeping machine. Not scored: how much of
/// it is the scheduler's is not known, and it is said so that a reader
/// does not take the task's delay for the whole of waking up.
fn timer_late(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let row = c.rows.iter().find(|r| r.workload == workload && r.metric == "timer_late_p50")?;
    let (b, v) = medians(row, c.baseline, i)?;
    Some(format!(
        "The waking thread itself ran {} µs after its timer, typically ({} µs under {}; observed, not scored)",
        fmt_value(v),
        fmt_value(b),
        c.series[c.baseline].label
    ))
}

/// How cgroup-fair's two groups shared the CPU time, next to what their
/// weights ask for, and under the baseline. Observed, not scored: the
/// score is in how closely the split follows the weights. Where the
/// split is the one the number of threads gives, the line says so: that
/// is a scheduler that does not know the groups, or their weights, and
/// not one that knows them badly.
fn split(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let median = |metric: &str, k: usize| {
        let row = c.rows.iter().find(|r| r.workload == workload && r.metric == metric)?;
        row.cells.get(k).and_then(Option::as_ref).map(|c| c.spread.median)
    };
    let pair = |a: f64| format!("{a:.1}% : {:.1}%", 100.0 - a);
    let share = median("share_a_pct", i)?;
    let mut line = format!("CPU time of the two groups: {}", pair(share));
    if let Some(expected) = median("expected_share_a_pct", i) {
        let _ = write!(line, " (the weights ask for {})", pair(expected));
        // Nearer to the split by threads than to the one by weights, and
        // within five points of it.
        if let Some(flat) = median("flat_share_a_pct", i)
            && (share - flat).abs() < (share - expected).abs()
            && (share - flat).abs() <= 5.0
        {
            let ignored = if workload.ends_with("/equal") { "the groups were" } else { "the weights were" };
            let _ = write!(line, ", which is the split by number of tasks, as if {ignored} not there");
        }
    }
    if let Some(base) = median("share_a_pct", c.baseline) {
        let _ = write!(line, "; under {}: {}", c.series[c.baseline].label, pair(base));
    }
    Some(line)
}

/// What the kernel counted for the worker threads, where the session
/// was run with `--sched-diag`: how the scheduler got to the numbers
/// above. Observed, not scored, and each figure with the baseline's in
/// brackets, since on its own none of them is good or bad.
fn mechanism(c: &Comparison, workload: &str, i: usize) -> Option<String> {
    let both = |metric: &str| {
        let row = c.rows.iter().find(|r| r.workload == workload && r.metric == metric)?;
        medians(row, c.baseline, i)
    };
    let pct = |v: f64| if v < 10.0 { format!("{v:.1}%") } else { format!("{v:.0}%") };
    let parts: Vec<String> = [
        both("sched_wait_pct")
            .map(|(b, v)| format!("the tasks waited for a CPU {} of the time they wanted one ({})", pct(v), pct(b))),
        both("sched_preempt_pct")
            .map(|(b, v)| format!("{} of their switches took the CPU away from them ({})", pct(v), pct(b))),
        both("sched_migrations_per_s")
            .map(|(b, v)| format!("they changed CPU {} times a second ({})", fmt_value(v), fmt_value(b))),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "How the scheduler got there (observed, not scored; {} in brackets): {}",
            c.series[c.baseline].label,
            parts.join("; ")
        )
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
            // The same two lines for either workload's wake-ups: the tail
            // at 1 in 1000 where the runs have the samples for it.
            let delays = ("shorter", "longer");
            let slowest = |population: &str, what: &str, more: &str| {
                point(c, w, i, &format!("{population}_p99.9"), &format!("Slowest {what} (1 in 1000){more}"), delays)
                    .or_else(|| {
                        point(c, w, i, &format!("{population}_p99"), &format!("Slow {what} (1 in 100){more}"), delays)
                    })
            };
            let mut points: Vec<String> = [
                // Round trips, not wake-ups: one round trip can take one wake-up
                // or two, or none, and `woken_per_s` counts those.
                point(c, w, i, "roundtrips_per_s", "Round trips completed", ("more", "fewer")),
                point(c, w, i, "rtt_blocked_p50", "Typical wake-up delay", delays),
                slowest("rtt_blocked", "wake-ups", ""),
                // What the slowest wake-ups are a matter of: how many
                // are slow, where their percentile sits on a step. A
                // share of the round trips that needed a wake-up, not of
                // all: where most hand-offs need none, that is far fewer.
                failures(c, w, i, &SLOW_TRIPS),
                // periodic-wake: the same amount of work for every
                // scheduler, so only how late it ran.
                point(c, w, i, "wake_p50", "Typical wake-up delay", delays),
                slowest("wake", "wake-ups", ""),
                // Two schedulers can wake a task equally fast and differ
                // in how often they take the CPU away during its work:
                // only the response shows that.
                slowest("response", "responses", ", work included"),
                failures(c, w, i, &DEADLINES),
                // wake-burst: a whole group woken at once. How soon one
                // of them runs, how soon all of them do, and how soon
                // their work is done; the last is what whoever handed
                // out the batch waits for.
                point(c, w, i, "start_p50", "Typical wake-up delay", delays),
                // More tasks than CPUs: no median, which would sit on the
                // step between the tasks that run at once and those that
                // wait. The quartiles are of all wake-ups together, and
                // are called that: the tasks are not sorted into kinds.
                point(c, w, i, "start_p25", "Wake-up delay, lower quartile", delays),
                point(c, w, i, "start_p75", "Wake-up delay, upper quartile", delays),
                slowest("start", "wake-ups", ""),
                point(c, w, i, "all_running_p50", "Until the whole group runs, typically", delays),
                point(c, w, i, "all_done_p50", "Until the whole group is done, typically", delays),
                slowest("all_done", "bursts", ", until the whole group is done"),
                failures(c, w, i, &OVERRUNS),
                burst_rate(c, w, i),
                // cgroup-fair: who got the CPUs, whether that is what
                // the weights ask for, and whether any were left idle.
                split(c, w, i),
                point(
                    c,
                    w,
                    i,
                    "weight_accuracy_pct",
                    "How closely the split follows the weights",
                    ("closer", "further"),
                ),
                point(c, w, i, "cpu_used_pct", "CPU time used", ("more", "less")),
                // idle-wake: its delays are `wake_*`, above; here where
                // the task was put, and whether the machine slept.
                placement(c, w, i),
                timer_late(c, w, i),
                wake_rate(c, w, i),
                // Any workload, where it was recorded.
                mechanism(c, w, i),
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
            // A repeated arm of the baseline: whatever the verdict says,
            // it is not about the scheduler.
            let again = crate::calibration::pairs(c).iter().any(|p| p.first == c.baseline && p.twin == i).then(|| {
                format!(
                    "This is {} again, measured as a separate arm: a difference here is not the scheduler, it is \
                         what the measurement finds between two runs of the same thing.",
                    c.series[c.baseline].label
                )
            });
            let note = match (again, mix_note(c, w, i)) {
                (Some(a), Some(m)) => Some(format!("{a} {m}")),
                (a, m) => a.or(m),
            };
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

/// The same for periodic-wake.
pub const WHAT_PERIODIC: &str = "The test wakes a few tasks on a timer for a short piece of work, as a frame \
or an audio buffer would, and measures how late they run.";

/// The same for wake-burst.
pub const WHAT_BURST: &str = "The test wakes a whole group of tasks at once for a short piece of work, as a \
thread pool given a batch would, and measures how soon they run and how soon all of them are done.";

/// The same for cgroup-fair.
pub const WHAT_CGROUP: &str = "The test runs two control groups of busy tasks that compete for every CPU, and \
measures whether the CPU time is split between them the way their weights say.";

/// The same for idle-wake.
pub const WHAT_IDLE: &str = "The test lets the machine go to sleep and has one task wake another, and measures how \
long the woken one takes to run.";

/// What the workloads of this comparison do, in a sentence each.
pub fn what(c: &Comparison) -> String {
    let has = |prefix: &str| c.rows.iter().any(|r| r.workload.starts_with(prefix));
    let parts: Vec<String> = [
        ("wake-pingpong", WHAT),
        ("periodic-wake", WHAT_PERIODIC),
        ("wake-burst", WHAT_BURST),
        ("cgroup-fair", WHAT_CGROUP),
        ("idle-wake", WHAT_IDLE),
    ]
    .into_iter()
    .filter(|(prefix, _)| has(prefix))
    .enumerate()
    // One test with several parts: the first sentence introduces
    // it, the others add to it.
    .map(|(k, (_, words))| if k == 0 { words.to_string() } else { words.replacen("The test ", "It also ", 1) })
    .collect();
    // Results this module has no words for: a sentence about another
    // workload would be wrong, and none is better than that.
    parts.join(" ")
}

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
    let what = what(c);
    if !what.is_empty() {
        let _ = writeln!(out, "\n{what}");
    }
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
