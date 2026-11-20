//! Turn several result files into rows of comparable numbers.
//!
//! Two levels of compatibility checking against the baseline:
//! * workload parameters (`pairs`, `hogs`, …) differ -> different load, so
//!   that pair of cells gets no change value and no shared latency curve;
//! * a metric's unit or direction differs from the baseline's (or, if the
//!   baseline lacks the metric, from the first file that has it) -> that
//!   cell is not compared either;
//! * environment differs (CPU, CPU count, SMT, governor, EPP, kernel, tool
//!   version) -> compared, but listed as a warning. A different kernel is
//!   often the point of the comparison, so it never blocks.

use std::collections::BTreeMap;

use helicoprion_core::{Better, ResultFile, RunResult, SystemInfo};

use crate::stats::{MIN_ROUNDS_FOR_CI, Spread, bootstrap_ci, bootstrap_median_ci, median};
use crate::svg::fmt_share;

/// One input file as it appears in the report.
#[derive(Debug, Clone)]
pub struct Series {
    pub label: String,
    pub file: ResultFile,
    /// Categorical colour slot; fixed by input order so colour follows the file.
    pub slot: usize,
}

#[derive(Debug, Clone)]
pub struct Cell {
    /// Metric value of every valid run.
    pub values: Vec<f64>,
    pub spread: Spread,
    pub total_runs: usize,
    /// Workload parameters equal the baseline's and every run reports the
    /// metric in the row's unit and direction.
    pub comparable: bool,
    /// Run-to-run spread (half the relative IQR) exceeds
    /// `Options::unstable_spread_pct`: shown, but neither charted nor scored.
    pub unstable: bool,
    /// Interleaved (`matrix`) session this cell's file came from, if any.
    pub session: Option<SessionKey>,
    /// Median of the valid runs of each round, for interleaved sessions.
    pub rounds: BTreeMap<u32, f64>,
    /// Median over valid runs of the fraction (0–1) of the run's events the
    /// metric's population holds (`Metric::share`); `None` for metrics over
    /// everything.
    pub share: Option<f64>,
    /// For a metric that is a share of counted events (`event_counters`):
    /// the events of every valid run and what they are a share of, from
    /// the run's own counters. Empty for any other metric, and for a run
    /// that does not have the counters.
    pub events: Vec<EventRun>,
    /// For such a metric, the share over all valid runs together, in
    /// percent: their events out of their opportunities. It is what the
    /// cell shows, and what a change is worked out from, in place of the
    /// median of the runs' own shares, which is zero for three runs of
    /// five with events and hides them. `None` for any other metric, and
    /// where a run does not have the counters.
    pub pooled: Option<f64>,
}

impl Cell {
    /// The value the cell shows: for a share of counted events the share
    /// over all runs together, for anything else the median of the runs.
    pub fn shown(&self) -> f64 {
        self.pooled.unwrap_or(self.spread.median)
    }

    /// For a share of counted events, what `pooled` is made of: the
    /// events of all valid runs and what they are out of.
    pub fn counted(&self) -> Option<(u64, u64)> {
        self.pooled?;
        Some(self.events.iter().fold((0, 0), |(events, of), r| (events + r.events, of + r.of)))
    }
}

/// The events behind a share in one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRun {
    /// The round of an interleaved session the run belongs to.
    pub round: Option<u32>,
    pub events: u64,
    /// The events that count towards `MIN_EVENTS`: all of them, or for a
    /// share whose events come in groups the first of each group.
    pub qualifying: u64,
    /// Out of how many.
    pub of: u64,
}

/// The counters behind a metric that is a share of counted events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventCounters {
    /// The events: the share is their sum out of `of`.
    pub events: &'static [&'static str],
    /// The events counted towards `MIN_EVENTS`.
    pub qualifying: &'static str,
    /// What the events are a share of.
    pub of: &'static str,
}

/// The counters behind a metric that is a share of counted events and is
/// scored. Such a share is only as good as the number of events it rests
/// on, which the share itself does not show: 9 slow round trips in 33.6
/// million against none is "100% fewer" with an interval of no width,
/// and nothing. These metrics are compared from their counters and in no
/// other way (`MetricRow::share`).
///
/// A missed deadline is more than one event as a rule: a period that
/// ends late, and the periods skipped for it, one under some schedulers
/// and several under others. The share is of all of them, and what is
/// counted towards `MIN_EVENTS` is the late ones alone, as the nearer
/// count of how many times something went wrong; it is not a count of
/// independent episodes either.
pub fn event_counters(metric: &str) -> Option<EventCounters> {
    let one = |events: &'static [&'static str], of| Some(EventCounters { events, qualifying: events[0], of });
    match metric {
        "rtt_blocked_slow_pct" => one(&["roundtrips_slow"], "roundtrips_blocked"),
        "deadline_miss_pct" => one(&["periods_late", "periods_skipped"], "periods_due"),
        "overrun_pct" => one(&["wakeups_overrun"], "wakeups_due"),
        _ => None,
    }
}

/// A share of counted events is scored only when both sides have this
/// many of them, over the runs the comparison rests on.
///
/// Both, and not one: a side with a handful of events, or none, cannot
/// show how its events vary from run to run, and its zeros are then
/// taken for the zeros of independent events. Where its events come
/// rarely and many at a time, "none against thousands" is found in most
/// comparisons of two sides with the same share. In simulation a rule of
/// one side called 55 to 70% of such equal pairs different, and this
/// rule 2% at most (METHODOLOGY, "Shares of counted events"). So a
/// change to none, from none or to a few is shown as counts and is not
/// scored: no events seen is worth saying and is no proof of a share of
/// zero.
///
/// It admits a change to scoring and is no test of it: the interval and
/// the minimum effect still have to hold. Twenty is where a quantile
/// starts to be given too (`min_samples_for(0.5)`), and it does not
/// mean twenty independent events.
pub const MIN_EVENTS: u64 = 20;

/// What became of the comparison of a share of counted events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Share {
    /// Compared, from the counters.
    Scored(Delta),
    /// A side has under `MIN_EVENTS` events: counts, and no change.
    TooFewEvents,
    /// A run of a side has the metric and not its counters, as results
    /// of older versions do. Not compared: the change is not taken from
    /// the runs' shares in their place, which would make two reports
    /// that say the same of their method use two.
    NoCounters,
    /// One session, and no round that both sides have.
    NoSharedRounds,
}

/// Identifies one interleaved session: files with equal keys were measured
/// in the same rounds, so their rounds can be paired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey(String);

/// Thresholds that decide what a report highlights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// A change is only highlighted when its magnitude reaches this many
    /// percent (and its interval excludes zero). Covers drift between
    /// sessions that the within-session bootstrap cannot see; an A/A run of
    /// one scheduler is the way to calibrate it.
    pub min_effect_pct: f64,
    /// Cells whose run-to-run spread (± half the relative IQR) exceeds this
    /// many percent are marked unstable.
    pub unstable_spread_pct: f64,
    /// A population metric whose population holds less than this many
    /// percent of the run's events (on either side) is a minor population:
    /// compared in the table, never highlighted or charted.
    pub min_share_pct: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options { min_effect_pct: 5.0, unstable_spread_pct: 10.0, min_share_pct: 1.0 }
    }
}

/// Change vs baseline in percent, positive = better.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    /// From the two medians.
    pub median: f64,
    /// 95% bootstrap interval resampling runs of both sides; `None` with too few runs.
    pub ci: Option<(f64, f64)>,
    /// "Goodness" ratio of the medians: >1 better, <1 worse
    /// (baseline/candidate for lower-is-better, candidate/baseline otherwise).
    pub ratio: f64,
    /// Number of rounds the estimate was paired over (interleaved
    /// sessions), or `None` for an unpaired comparison of runs.
    pub paired_rounds: Option<usize>,
    /// Same interleaved session as the baseline, but no round in common:
    /// nothing could be paired, so this is a descriptive comparison of
    /// medians only, without an interval. `paired_rounds` is `None` then.
    pub unmatched_rounds: bool,
    /// Paired only: how consistent the effect is from round to round.
    pub paired_spread: Option<PairedSpread>,
    /// Paired only: shared rounds left out of the estimate because the
    /// baseline measured zero there, which leaves the percentage change
    /// undefined. The estimate then describes only the other rounds, so a
    /// change with any such round is never scored.
    pub undefined_rounds: usize,
    /// Direction of the metric, to turn a change back into a ratio.
    pub better: Better,
}

impl Delta {
    /// The interval lies entirely on one side of zero. A heuristic, not a
    /// formal significance test.
    pub fn ci_excludes_zero(&self) -> bool {
        self.ci.is_some_and(|(lo, hi)| lo > 0.0 || hi < 0.0)
    }

    /// Worth highlighting: interval excludes zero *and* the change is at
    /// least `min_effect_pct` in magnitude.
    pub fn notable(&self, min_effect_pct: f64) -> bool {
        self.ci_excludes_zero() && self.median.abs() >= min_effect_pct
    }

    /// Paired only: the per-round results disagree by more than
    /// `threshold` (`PairedSpread`, returned; percent or percentage points
    /// alike), and by too much for the size of the effect. The schedulers
    /// then did not differ by one factor under the session's conditions,
    /// so the median change is not scored.
    ///
    /// "Too much for the effect" matters for large effects. With a
    /// baseline that wobbles by 9% from round to round, "2.2× fewer, 95%
    /// interval 1.9× to 2.5×" has a spread of 14%, over a 10% threshold,
    /// and the same measurement read "clear" in one session and "unclear"
    /// in the next. A spread that the effect outweighs (`outweighs`) is
    /// not a reason to withhold the direction.
    pub fn uneven(&self, threshold: f64) -> Option<PairedSpread> {
        self.paired_spread.filter(|s| s.value() > threshold && !self.outweighs(*s))
    }

    /// The effect is larger than its own unevenness: even the cautious end
    /// of the 95% interval is farther from no change than the per-round
    /// spread. Both are on the same relative scale, as a factor minus one,
    /// in percent: a cautious end of "1.9× worse" is 90% away, against a
    /// spread of ±14%.
    ///
    /// Never without an interval, nor with one that includes no change.
    /// Never when an end of the interval has no usable ratio (zero or not
    /// finite): there is no distance to compare then, and the cautious
    /// reading stays. And never for a spread in percentage points: that is
    /// the fallback for sessions with a round that has no usable ratio (a
    /// zero on one side), where a relative distance does not exist for
    /// every round. An effect exactly as large as the spread does not
    /// outweigh it.
    fn outweighs(&self, spread: PairedSpread) -> bool {
        let (PairedSpread::Relative(spread_pct), Some((lo, hi))) = (spread, self.ci) else {
            return false;
        };
        if !self.ci_excludes_zero() {
            return false;
        }
        let distance_pct = |change: f64| {
            let r = self.ratio_of(change);
            (r > 0.0 && r.is_finite()).then(|| (r.max(1.0 / r) - 1.0) * 100.0)
        };
        match (distance_pct(lo), distance_pct(hi)) {
            (Some(a), Some(b)) => a.min(b) > spread_pct,
            _ => false,
        }
    }

    /// The goodness ratio (like `ratio`) that a change of `change_pct`
    /// percent stands for; the inverse of `change()`. Monotonic, so an
    /// interval of changes maps onto an interval of ratios.
    pub fn ratio_of(&self, change_pct: f64) -> f64 {
        ratio_of_change(self.better, change_pct)
    }
}

/// How far the per-round results of a paired comparison are from one
/// another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PairedSpread {
    /// In percent, between the per-round ratios of the two sides: half the
    /// IQR of their logarithms, as a factor minus one. Relative, so that
    /// "26× better" in every round is as even as "+5%" in every round,
    /// whichever direction the metric and the change have.
    Relative(f64),
    /// In percentage points, between the per-round changes: half their
    /// IQR. Used when some round has no usable ratio (zero or not finite:
    /// typically a zero on the candidate's side, possibly a ratio too large
    /// to represent), so that such a round still counts; a ratio scale
    /// would drop it.
    Points(f64),
}

impl PairedSpread {
    pub fn value(self) -> f64 {
        match self {
            PairedSpread::Relative(v) | PairedSpread::Points(v) => v,
        }
    }
}

impl std::fmt::Display for PairedSpread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairedSpread::Relative(v) => write!(f, "±{v:.1}%"),
            PairedSpread::Points(v) => write!(f, "±{v:.1} pp"),
        }
    }
}

/// The goodness ratio a change of `change_pct` percent stands for.
fn ratio_of_change(better: Better, change_pct: f64) -> f64 {
    match better {
        Better::Lower => 1.0 / (1.0 - change_pct / 100.0),
        Better::Higher => 1.0 + change_pct / 100.0,
        Better::Neither => 1.0,
    }
}

#[derive(Debug, Clone)]
pub struct MetricRow {
    pub workload: String,
    pub metric: String,
    pub unit: String,
    pub better: Better,
    /// Single-sample extreme (e.g. max): table only.
    pub extreme: bool,
    /// `Options::unstable_spread_pct` of the comparison this row belongs to.
    pub unstable_spread_pct: f64,
    /// `Options::min_share_pct` of the comparison this row belongs to.
    pub min_share_pct: f64,
    /// Indexed like `Comparison::series`; `None` if that file lacks the metric.
    pub cells: Vec<Option<Cell>>,
}

fn change(better: Better, base: f64, v: f64) -> Option<f64> {
    if base == 0.0 {
        return None;
    }
    match better {
        Better::Lower => Some((base - v) / base * 100.0),
        Better::Higher => Some((v - base) / base * 100.0),
        Better::Neither => None,
    }
}

fn goodness_ratio(better: Better, base: f64, v: f64) -> f64 {
    match better {
        Better::Lower => base / v,
        Better::Higher => v / base,
        Better::Neither => 1.0,
    }
}

impl MetricRow {
    /// Change of series `i` vs the baseline. `None` if either side is
    /// missing, the workload parameters differ, or the metric has no
    /// direction (informational).
    pub fn improvement(&self, baseline: usize, i: usize) -> Option<Delta> {
        let b = self.cells.get(baseline)?.as_ref()?;
        let s = self.cells.get(i)?.as_ref()?;
        if !s.comparable {
            return None;
        }
        // A share of counted events is compared from its counters, or
        // not at all.
        if event_counters(&self.metric).is_some() {
            return match self.share(baseline, i)? {
                Share::Scored(d) => Some(d),
                _ => None,
            };
        }
        if let Some(d) = self.paired(b, s) {
            return Some(d);
        }
        let median = change(self.better, b.spread.median, s.spread.median)?;
        let ci = bootstrap_ci(&b.values, &s.values, |mb, mc| change(self.better, mb, mc));
        let ratio = goodness_ratio(self.better, b.spread.median, s.spread.median);
        Some(Delta {
            median,
            ci,
            ratio,
            paired_rounds: None,
            unmatched_rounds: false,
            paired_spread: None,
            undefined_rounds: 0,
            better: self.better,
        })
    }

    /// Estimate for two cells from the same interleaved session, which is
    /// always paired: the runs of one session are not independent samples,
    /// so they are never fed to the per-run bootstrap (pseudoreplication).
    ///
    /// * at least `MIN_ROUNDS_FOR_CI` shared rounds: the change is computed within
    ///   each round (candidate vs baseline under the same conditions), the
    ///   estimate is the median of those per-round changes, and the interval
    ///   resamples whole rounds, so drift between rounds cancels;
    /// * 1-2 shared rounds: the same point estimate, no interval;
    /// * no shared rounds: the change of the medians, no interval.
    ///
    /// `None` only when the cells are not from one session.
    fn paired(&self, b: &Cell, s: &Cell) -> Option<Delta> {
        if b.session.is_none() || b.session != s.session {
            return None;
        }
        let common: Vec<(f64, f64)> =
            b.rounds.iter().filter_map(|(r, bv)| s.rounds.get(r).map(|sv| (*bv, *sv))).collect();
        // Rounds with a change (a non-zero baseline), with their ratio.
        let rounds: Vec<(f64, f64)> = common
            .iter()
            .filter_map(|(bv, sv)| Some((change(self.better, *bv, *sv)?, goodness_ratio(self.better, *bv, *sv))))
            .collect();
        let changes: Vec<f64> = rounds.iter().map(|(c, _)| *c).collect();
        let Some(spread) = Spread::of(&changes) else {
            return Some(Delta {
                median: change(self.better, b.spread.median, s.spread.median)?,
                ci: None,
                ratio: goodness_ratio(self.better, b.spread.median, s.spread.median),
                paired_rounds: None,
                unmatched_rounds: true,
                paired_spread: None,
                undefined_rounds: common.len(),
                better: self.better,
            });
        };
        let ratios: Vec<f64> = rounds.iter().map(|(_, r)| *r).filter(|r| r.is_finite() && *r > 0.0).collect();
        // A round where one side is zero has a change but no usable ratio.
        // Judging evenness on the remaining ratios would drop exactly the
        // rounds that differ most, so fall back to the changes themselves,
        // and take the ratio from the median change so the two agree.
        let (ratio, paired_spread) = if ratios.len() == changes.len() {
            (median(&ratios).unwrap_or(1.0), relative_spread_pct(&ratios).map(PairedSpread::Relative))
        } else {
            (ratio_of_change(self.better, spread.median), Some(PairedSpread::Points((spread.q3 - spread.q1) / 2.0)))
        };
        Some(Delta {
            median: spread.median,
            // `bootstrap_median_ci` returns None below MIN_ROUNDS_FOR_CI.
            ci: bootstrap_median_ci(&changes),
            ratio,
            paired_rounds: Some(changes.len()),
            unmatched_rounds: false,
            paired_spread,
            undefined_rounds: common.len() - rounds.len(),
            better: self.better,
        })
    }

    /// The comparison is too noisy to score. Unpaired: either side's
    /// run-to-run spread is above the threshold. Paired: the per-round
    /// changes disagree by more than the threshold (raw run-to-run spread
    /// there is mostly drift between rounds, which pairing removes).
    pub fn unstable_pair(&self, baseline: usize, i: usize) -> bool {
        // How much a share's runs differ is in its interval already, as
        // the dispersion its counts are divided by.
        if event_counters(&self.metric).is_some() {
            return false;
        }
        let d = self.improvement(baseline, i);
        if d.as_ref().is_some_and(|d| d.undefined_rounds > 0) {
            return true;
        }
        if let Some(d) = d.filter(|d| d.paired_spread.is_some()) {
            return d.uneven(self.unstable_spread_pct).is_some();
        }
        [baseline, i].iter().any(|k| self.cells.get(*k).and_then(Option::as_ref).is_some_and(|c| c.unstable))
    }

    /// Series `k`'s cell comes from a population that holds less than
    /// `min_share_pct` of its run's events.
    pub fn minor(&self, k: usize) -> bool {
        self.cells.get(k).and_then(Option::as_ref).and_then(|c| c.share).is_some_and(|s| s * 100.0 < self.min_share_pct)
    }

    /// For a share of rare events: the events and what they are a share
    /// of, for the baseline and for series `i`, summed over the runs the
    /// comparison rests on, which are runs that have the metric. Two
    /// cases, kept apart:
    ///
    /// * one session: the rounds both sides have, as for the change
    ///   itself, and nothing else. Where they share no round there is no
    ///   paired comparison for any count to belong to, and none is given;
    /// * anything else: every such run of each side.
    ///
    /// `None` also for any other metric, and where a side has no run
    /// with both the metric and its counters.
    pub fn events(&self, baseline: usize, i: usize) -> Option<[(u64, u64); 2]> {
        let sum = |runs: &Vec<EventRun>| runs.iter().fold((0, 0), |(events, of), e| (events + e.events, of + e.of));
        self.event_runs(baseline, i).map(|[b, s]| [sum(&b), sum(&s)])
    }

    /// The observations behind `events`, the baseline's and those of
    /// series `i`: what counts as one sample of a side.
    ///
    /// In a session that is a round, with the counters of its runs
    /// summed, and not a run: the rounds are what a session repeats, and
    /// two runs of one round are not two of them. Two rounds of two runs
    /// are two observations, too few for an interval, as two rounds are
    /// for any other metric. Outside a session every valid run is one.
    fn event_runs(&self, baseline: usize, i: usize) -> Option<[Vec<EventRun>; 2]> {
        let cell = |k: usize| self.cells.get(k).and_then(Option::as_ref).filter(|c| !c.events.is_empty());
        let (b, s) = (cell(baseline)?, cell(i)?);
        if b.session.is_none() || b.session != s.session {
            return Some([b.events.clone(), s.events.clone()]);
        }
        let rounds = |c: &Cell| c.events.iter().filter_map(|e| e.round).collect::<std::collections::BTreeSet<u32>>();
        let shared: Vec<u32> = rounds(b).intersection(&rounds(s)).copied().collect();
        if shared.is_empty() {
            return None;
        }
        let of = |c: &Cell| -> Vec<EventRun> {
            let mut rounds: BTreeMap<u32, EventRun> = BTreeMap::new();
            for e in &c.events {
                let Some(round) = e.round.filter(|r| shared.contains(r)) else { continue };
                let sum = rounds.entry(round).or_insert(EventRun { round: e.round, events: 0, qualifying: 0, of: 0 });
                sum.events += e.events;
                sum.qualifying += e.qualifying;
                sum.of += e.of;
            }
            rounds.into_values().collect()
        };
        Some([of(b), of(s)])
    }

    /// What became of the comparison of series `i` with the baseline for
    /// a share of counted events. `None` for any other metric, where a
    /// side is missing, and where the workload parameters differ.
    ///
    /// The change is that of the shares over the runs compared, each
    /// from the sums of its counters, with the effective-count
    /// approximate interval (`shares::change`). What is compared is what
    /// `events` counts: the rounds both sides have in a session, each
    /// one observation whatever its number of runs, and every valid run
    /// otherwise. An interval needs three observations a side, as it
    /// needs three rounds anywhere else.
    pub fn share(&self, baseline: usize, i: usize) -> Option<Share> {
        event_counters(&self.metric)?;
        let b = self.cells.get(baseline)?.as_ref()?;
        let s = self.cells.get(i)?.as_ref()?;
        if !s.comparable {
            return None;
        }
        if b.pooled.is_none() || s.pooled.is_none() {
            return Some(Share::NoCounters);
        }
        let Some([b_runs, s_runs]) = self.event_runs(baseline, i) else {
            return Some(Share::NoSharedRounds);
        };
        let qualifying = |runs: &[EventRun]| runs.iter().map(|r| r.qualifying).sum::<u64>();
        if qualifying(&b_runs).min(qualifying(&s_runs)) < MIN_EVENTS {
            return Some(Share::TooFewEvents);
        }
        let Some(found) = crate::shares::change(&b_runs, &s_runs) else {
            return Some(Share::TooFewEvents);
        };
        // The ratio of the shares as a change of this metric, and the
        // ends of its interval the same way, worse end first.
        let as_change = |ratio: f64| change(self.better, 1.0, ratio);
        let ci = found.interval.and_then(|(lo, hi)| {
            let (a, b) = (as_change(lo)?, as_change(hi)?);
            Some((a.min(b), a.max(b)))
        });
        Some(Share::Scored(Delta {
            median: as_change(found.ratio)?,
            ci,
            ratio: goodness_ratio(self.better, 1.0, found.ratio),
            // The rounds decide which runs are compared and are not
            // paired off: what a round shares is in the dispersion.
            paired_rounds: None,
            unmatched_rounds: false,
            paired_spread: None,
            undefined_rounds: 0,
            better: self.better,
        }))
    }

    /// The counts behind a share of rare events, the baseline's and
    /// then those of series `i`, each with what it is out of: "9 /
    /// 33.62M → 0 / 26.19M". The one wording of them, for the plain
    /// view, the table and the terminal. `None` where there are no
    /// counts to give (`events`): nothing is put in their place.
    pub fn event_counts(&self, baseline: usize, i: usize) -> Option<String> {
        let [(b_events, b_of), (events, of)] = self.events(baseline, i)?;
        let out_of = |n: u64| crate::svg::fmt_value(n as f64);
        Some(format!("{b_events} / {} → {events} / {}", out_of(b_of), out_of(of)))
    }

    /// A share of counted events with under `MIN_EVENTS` of them on a
    /// side: shown, with the counts, and not scored.
    pub fn too_few_events(&self, baseline: usize, i: usize) -> bool {
        self.share(baseline, i) == Some(Share::TooFewEvents)
    }

    /// Either side is a minor population. Its quantiles then describe a
    /// handful of unusual events (e.g. the few hand-offs that never slept),
    /// so a large "change" says the populations differ, not that one
    /// scheduler made the same events faster.
    pub fn minor_pair(&self, baseline: usize, i: usize) -> bool {
        self.minor(baseline) || self.minor(i)
    }

    /// Whether series `i` belongs on the change chart: a scored metric that
    /// is not a single-sample extreme, comparable, stable on both sides and
    /// not a minor population on either side.
    pub fn chartable(&self, baseline: usize, i: usize) -> bool {
        !self.extreme
            && self.better != Better::Neither
            && !self.unstable_pair(baseline, i)
            && !self.minor_pair(baseline, i)
            && !self.too_few_events(baseline, i)
            && self.improvement(baseline, i).is_some()
    }
}

/// Median quantile function per series for one workload population
/// (e.g. `rtt_blocked`): (quantile, value).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub workload: String,
    pub population: String,
    pub unit: String,
    pub curves: Vec<Option<Vec<(f64, f64)>>>,
    /// Indexed like `curves`: median over valid runs of the population's
    /// share (0–1) of the run's events, where known.
    pub shares: Vec<Option<f64>>,
}

impl Distribution {
    /// Series `k`'s population holds less than `min_share_pct` of its
    /// run's events (see `MetricRow::minor`).
    pub fn minor(&self, k: usize, min_share_pct: f64) -> bool {
        self.shares.get(k).copied().flatten().is_some_and(|s| s * 100.0 < min_share_pct)
    }
}

/// Diagnostic counters summed over the valid runs of one workload.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    pub series: usize,
    pub workload: String,
    pub valid_runs: usize,
    pub counters: BTreeMap<String, u64>,
    /// What the workload was set up with, as `name=value` pairs: the
    /// same in every run, and so not something to sum.
    pub params: String,
}

/// A workload's parameters on one line.
fn params_line(params: &serde_json::Value) -> String {
    match params.as_object() {
        Some(map) => {
            let value = |v: &serde_json::Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
            map.iter().map(|(k, v)| format!("{k}={}", value(v))).collect::<Vec<_>>().join(", ")
        }
        None => String::new(),
    }
}

#[derive(Debug, Clone)]
pub struct Comparison {
    pub series: Vec<Series>,
    pub baseline: usize,
    pub opts: Options,
    pub rows: Vec<MetricRow>,
    pub distributions: Vec<Distribution>,
    pub diagnostics: Vec<Diagnostics>,
    /// Invalid runs, undetectable restarts, parameter mismatches.
    pub warnings: Vec<String>,
    /// Environment differences vs the baseline (informational).
    pub env_notes: Vec<String>,
    /// Populations that are a very different part of the run for a
    /// candidate than for the baseline. Each is also among `warnings`.
    pub mixes: Vec<Mix>,
}

/// A population of events (e.g. `rtt_blocked`, round trips that slept)
/// whose share of the run differs by `MIX_FACTOR` or more between the
/// baseline and a candidate, and is minor on neither side. Its latencies
/// are compared, but are not the same events on the two sides.
#[derive(Debug, Clone, PartialEq)]
pub struct Mix {
    /// Index into `Comparison::series`.
    pub series: usize,
    pub workload: String,
    /// The metric name without its statistic: `rtt_blocked`.
    pub population: String,
    /// Share of the baseline's events, 0 to 1.
    pub baseline_share: f64,
    /// Share of the candidate's.
    pub share: f64,
}

/// Metrics that results written by earlier versions carry with a better
/// direction and that are informational now: read as such, so that old
/// files are scored like new ones and can be compared with them (a metric
/// whose direction differs between files is otherwise not compared).
///
/// `woken_per_s` of wake-pingpong, the threads actually woken per second,
/// was "higher is better". It is the rate of real wake-ups, and it follows
/// the mix of round trips: where most replies arrive before the pinger
/// sleeps, fewer threads are woken while more round trips complete.
/// Scored, that made a scheduler "worse" for not having to put its tasks
/// to sleep.
fn informational_in_older_files(f: &mut ResultFile) {
    for w in &mut f.workloads {
        if w.name != "wake-pingpong" && !w.name.starts_with("wake-pingpong/") {
            continue;
        }
        for r in &mut w.runs {
            if let Some(m) = r.metrics.get_mut("woken_per_s") {
                m.better = Better::Neither;
            }
        }
    }
}

/// Spread of positive ratios relative to one another, in percent: half the
/// IQR of their logarithms, turned back into a factor. Ratios of 1.00 and
/// 1.10 are as far apart as 20 and 22.
fn relative_spread_pct(ratios: &[f64]) -> Option<f64> {
    let logs: Vec<f64> = ratios.iter().map(|r| r.ln()).collect();
    let s = Spread::of(&logs)?;
    Some((((s.q3 - s.q1) / 2.0).exp() - 1.0) * 100.0)
}

/// A population whose share of the run's events differs between the two
/// sides by this factor or more gets a warning: its metrics are compared
/// and scored as usual, but they describe different parts of the two runs.
const MIX_FACTOR: f64 = 2.0;

/// How a candidate's clear changes on one workload add up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Some metrics clearly better, none clearly worse.
    Better,
    /// Some metrics clearly worse, none clearly better.
    Worse,
    /// Clearly better in some metrics and clearly worse in others.
    Mixed,
    /// No metric changed clearly either way.
    NoClearDifference,
}

/// One candidate against the baseline on one workload, for readers who
/// only want to know which result is better. It counts the metrics the
/// table colours, and is no more than that count: it does not weigh one
/// metric against another.
#[derive(Debug, Clone)]
pub struct Summary {
    /// Index into `Comparison::series`.
    pub series: usize,
    pub workload: String,
    /// Scored metrics both sides have, single-sample extremes aside.
    pub compared: usize,
    pub better: usize,
    pub worse: usize,
    /// Among the unclear ones: changes that would be clear, were they not
    /// uneven across rounds (`Delta::uneven`).
    pub uneven: usize,
    /// The largest clear changes (up to three), largest first.
    pub top: Vec<(String, Delta)>,
    /// The largest clear change for the better and the largest for the
    /// worse, of all of them and not of `top` alone: what the card puts
    /// next to a verdict. A change to nothing, where one side's value is
    /// zero, is not given here: it has a direction and no size ("+100%"
    /// whatever the baseline was). A share of counted events cannot be
    /// one, being compared only with events on both sides; this is for
    /// whatever else might.
    pub largest: [Option<Delta>; 2],
}

impl Summary {
    pub fn verdict(&self) -> Verdict {
        match (self.better > 0, self.worse > 0) {
            (true, false) => Verdict::Better,
            (false, true) => Verdict::Worse,
            (true, true) => Verdict::Mixed,
            (false, false) => Verdict::NoClearDifference,
        }
    }

    /// Compared metrics without a clear change: within the noise band, an
    /// interval that includes zero or is missing, unstable, or a minor
    /// population.
    pub fn unclear(&self) -> usize {
        self.compared - self.better - self.worse
    }
}

/// Why a compared change is not scored. One change can fail several
/// tests; the first that applies, in this order, is the reason given, so
/// that every part of the report names the same one.
///
/// The order goes from what the change is (a kind of number that is never
/// scored), over whether there is an estimate to speak of, to how good the
/// estimate is. Unevenness and instability come last: they are named only
/// for a change that would otherwise be scored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnclearReason {
    /// A single-sample extreme (a maximum): listed, never scored.
    SingleSample,
    /// The population behind the metric is under `min_share_pct` of the
    /// run's events on one side.
    MinorPopulation,
    /// The baseline measured zero in some shared rounds, where a
    /// percentage change does not exist.
    UndefinedRounds,
    /// No interval: fewer than three runs per side, or fewer than three
    /// rounds measured by both sides of a session.
    TooFewRounds,
    /// The 95% interval includes no change.
    IntervalIncludesNoChange,
    /// The interval excludes no change, but the change is smaller than
    /// `min_effect_pct`.
    BelowThreshold,
    /// Paired: the per-round results differ too much, and by more than the
    /// effect (`Delta::uneven`).
    Uneven(PairedSpread),
    /// Unpaired: a side's run-to-run spread is above `unstable_spread_pct`.
    Unstable,
}

impl Comparison {
    /// Why the change of series `i` in row `r` is not scored, or `None`
    /// when it is. The single decision behind `clear`, the uneven tag, the
    /// summary and the plain-words text.
    pub fn unclear_reason(&self, r: &MetricRow, i: usize, d: &Delta) -> Option<UnclearReason> {
        if r.extreme {
            return Some(UnclearReason::SingleSample);
        }
        if r.minor_pair(self.baseline, i) {
            return Some(UnclearReason::MinorPopulation);
        }
        // A share of counted events with too few of them never gets
        // here: it has no change to judge (`MetricRow::share`).
        if d.undefined_rounds > 0 {
            return Some(UnclearReason::UndefinedRounds);
        }
        if d.ci.is_none() {
            return Some(UnclearReason::TooFewRounds);
        }
        if !d.ci_excludes_zero() {
            return Some(UnclearReason::IntervalIncludesNoChange);
        }
        if !d.notable(self.opts.min_effect_pct) {
            return Some(UnclearReason::BelowThreshold);
        }
        if r.unstable_pair(self.baseline, i) {
            return Some(match d.uneven(self.opts.unstable_spread_pct) {
                Some(spread) => UnclearReason::Uneven(spread),
                None => UnclearReason::Unstable,
            });
        }
        None
    }

    /// What stands in place of the change of series `i` in row `r` where
    /// a share of counted events is not compared (`MetricRow::share`):
    /// the counts and that they are too few, or what else keeps the two
    /// from being compared. `None` for a share that is, for any other
    /// metric, for the baseline itself, and where the workload
    /// parameters differ, which is what is said then.
    ///
    /// "Too few events" is not "no clear difference": the first is a
    /// pair that was not admitted to a comparison, the second one that
    /// was and did not settle it. A change of "+100%" next to a note
    /// that it does not count would still be read as a number that
    /// does, so the table and the terminal show this and no percentage.
    pub fn share_note(&self, r: &MetricRow, i: usize) -> Option<String> {
        if i == self.baseline {
            return None;
        }
        match r.share(self.baseline, i)? {
            Share::Scored(_) => None,
            // Too few events are counted events: the counts are there.
            Share::TooFewEvents => {
                r.event_counts(self.baseline, i).map(|counts| format!("{counts}; too few events to score"))
            }
            Share::NoCounters => Some("no event counters in this result; not scored".into()),
            Share::NoSharedRounds => Some("no rounds in common; not scored".into()),
        }
    }

    /// The change of series `i` in row `r` is one the report colours and
    /// stars: not a single-sample extreme, stable, not a minor population
    /// on either side, interval excludes zero, at least the minimum effect.
    pub fn clear(&self, r: &MetricRow, i: usize, d: &Delta) -> bool {
        self.unclear_reason(r, i, d).is_none()
    }

    /// The change would be clear (`clear`), were its rounds not uneven:
    /// unevenness is the one reason it is not scored. The report tags
    /// exactly these and the summary counts exactly these, so the two
    /// agree. Not so for a single-sample extreme, a change without an
    /// interval or with one that includes zero, one below the minimum
    /// effect, a minor population, or rounds with an undefined change:
    /// there something else already keeps the change from being scored,
    /// and an "uneven" tag would explain nothing.
    pub fn held_back_by_unevenness(&self, r: &MetricRow, i: usize, d: &Delta) -> Option<PairedSpread> {
        match self.unclear_reason(r, i, d) {
            Some(UnclearReason::Uneven(spread)) => Some(spread),
            _ => None,
        }
    }

    /// Per candidate and workload, how many compared metrics are clearly
    /// better or worse than the baseline. Workloads the candidate cannot
    /// be compared on (missing, or different parameters) are left out.
    pub fn summary(&self) -> Vec<Summary> {
        let mut workloads: Vec<&str> = Vec::new();
        for r in &self.rows {
            if !workloads.contains(&r.workload.as_str()) {
                workloads.push(&r.workload);
            }
        }
        let mut out = Vec::new();
        for i in (0..self.series.len()).filter(|i| *i != self.baseline) {
            for w in &workloads {
                let mut s = Summary {
                    series: i,
                    workload: w.to_string(),
                    compared: 0,
                    better: 0,
                    worse: 0,
                    uneven: 0,
                    top: Vec::new(),
                    largest: [None, None],
                };
                for r in self.rows.iter().filter(|r| r.workload == *w && !r.extreme) {
                    let Some(d) = r.improvement(self.baseline, i) else { continue };
                    s.compared += 1;
                    if self.clear(r, i, &d) {
                        if d.median > 0.0 {
                            s.better += 1;
                        } else {
                            s.worse += 1;
                        }
                        s.top.push((r.metric.clone(), d));
                    } else if self.held_back_by_unevenness(r, i, &d).is_some() {
                        s.uneven += 1;
                    }
                }
                // Largest first, as the reader sees them: changes shown as a
                // multiple (2× or more) before those shown as a percentage,
                // multiples by their factor, percentages by their size. By
                // ratio alone "-20.4%" of a rate outranks "-25.1%" of a
                // latency, and the list looks unsorted.
                let size = |d: &Delta| {
                    let r = d.ratio;
                    if r.is_finite() && r > 0.0 && !(0.5 < r && r < 2.0) {
                        (1, r.ln().abs())
                    } else {
                        (0, d.median.abs())
                    }
                };
                s.top.sort_by(|a, b| {
                    let (a, b) = (size(&a.1), size(&b.1));
                    b.0.cmp(&a.0).then(b.1.total_cmp(&a.1))
                });
                // The first of each direction, before the list is cut:
                // its three can all point one way.
                let sized = |d: &&Delta| d.ratio.is_finite() && d.ratio > 0.0;
                let first = |better: bool| {
                    s.top.iter().map(|(_, d)| d).filter(sized).find(|d| (d.median > 0.0) == better).copied()
                };
                s.largest = [first(true), first(false)];
                s.top.truncate(3);
                if s.compared > 0 {
                    out.push(s);
                }
            }
        }
        out
    }

    /// Some candidate is compared with the baseline round by round.
    pub fn any_paired(&self) -> bool {
        self.rows.iter().any(|r| {
            (0..self.series.len())
                .filter(|i| *i != self.baseline)
                .any(|i| r.improvement(self.baseline, i).is_some_and(|d| d.paired_rounds.is_some()))
        })
    }

    pub fn build(files: Vec<ResultFile>, baseline: usize) -> Self {
        Self::build_with(files, baseline, Options::default())
    }

    pub fn build_with(files: Vec<ResultFile>, baseline: usize, opts: Options) -> Self {
        let mut c = Self::build_core(files, baseline, opts);
        // Two arms of one scheduler that differ clearly: said with the
        // other warnings, so that every output carries it.
        let alarms: Vec<String> = crate::calibration::calibrate(&c)
            .iter()
            .flat_map(|cal| cal.balance_warning().into_iter().chain(cal.warning()))
            .collect();
        c.warnings.extend(alarms);
        c
    }

    /// The comparison without the A/A check, which itself compares pairs
    /// of files with this.
    pub(crate) fn build_core(mut files: Vec<ResultFile>, baseline: usize, opts: Options) -> Self {
        for f in &mut files {
            informational_in_older_files(f);
        }
        let series: Vec<Series> = disambiguate(files)
            .into_iter()
            .enumerate()
            .map(|(slot, (label, file))| Series { label, file, slot })
            .collect();
        let base = &series[baseline];

        // Workload and metric order: first appearance across inputs.
        let mut order: Vec<(String, String)> = Vec::new();
        let mut meta: BTreeMap<(String, String), (String, Better, bool)> = BTreeMap::new();
        let mut warnings = Vec::new();
        let mut all_mixes = Vec::new();
        let mut diagnostics = Vec::new();
        warnings.extend(tick_aligned(&series));

        for (si, s) in series.iter().enumerate() {
            if !s.file.restart_detectable {
                warnings.push(format!(
                    "{}: {} exposes no enable_seq, so a crash-and-restart of the same scheduler between readings cannot be detected",
                    s.label,
                    s.file.scheduler.label()
                ));
            }
            for w in &s.file.workloads {
                for r in &w.runs {
                    for (k, m) in &r.metrics {
                        let key = (w.name.clone(), k.clone());
                        if let std::collections::btree_map::Entry::Vacant(e) = meta.entry(key.clone()) {
                            e.insert((m.unit.clone(), m.better, m.extreme));
                            order.push(key);
                        }
                    }
                }
                let invalid: Vec<_> = w.runs.iter().filter(|r| !r.valid).collect();
                if !invalid.is_empty() {
                    let why = invalid[0].invalid_reason.as_deref().unwrap_or("unknown");
                    warnings.push(format!(
                        "{}: {}: {}/{} runs invalid (first: {why})",
                        s.label,
                        w.name,
                        invalid.len(),
                        w.runs.len()
                    ));
                }
                if si != baseline
                    && let Some(bw) = base.file.workloads.iter().find(|b| b.name == w.name)
                    && bw.params != w.params
                {
                    warnings.push(format!(
                        "{}: {} ran with {} but the baseline used {}; not compared",
                        s.label, w.name, w.params, bw.params
                    ));
                }
                let mut counters: BTreeMap<String, u64> = BTreeMap::new();
                let mut valid_runs = 0;
                for r in w.runs.iter().filter(|r| r.valid) {
                    valid_runs += 1;
                    for (k, v) in &r.counters {
                        *counters.entry(k.clone()).or_default() += v;
                    }
                }
                if !counters.is_empty() {
                    diagnostics.push(Diagnostics {
                        series: si,
                        workload: w.name.clone(),
                        valid_runs,
                        counters,
                        params: params_line(&w.params),
                    });
                }
            }
        }

        // Reference unit/direction: the baseline's wherever it has the metric.
        for w in &base.file.workloads {
            for r in w.runs.iter().filter(|r| r.valid) {
                for (k, m) in &r.metrics {
                    meta.insert((w.name.clone(), k.clone()), (m.unit.clone(), m.better, m.extreme));
                }
            }
        }

        let env_notes = series
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != baseline)
            .flat_map(|(_, s)| env_diff(&base.file, &s.file).into_iter().map(move |d| format!("{}: {d}", s.label)))
            .collect();

        let rows = order
            .into_iter()
            .map(|(workload, metric)| {
                let (unit, better, extreme) = meta[&(workload.clone(), metric.clone())].clone();
                let base_params = base.file.workloads.iter().find(|w| w.name == workload).map(|w| &w.params);
                let cells = series
                    .iter()
                    .map(|s| {
                        let w = s.file.workloads.iter().find(|w| w.name == workload)?;
                        let with_round: Vec<_> = w
                            .runs
                            .iter()
                            .filter(|r| r.valid)
                            .filter_map(|r| r.metrics.get(&metric).map(|m| (r.round, m)))
                            .collect();
                        let ms: Vec<_> = with_round.iter().map(|(_, m)| *m).collect();
                        let mut by_round: BTreeMap<u32, Vec<f64>> = BTreeMap::new();
                        for (round, m) in &with_round {
                            if let Some(r) = round
                                && m.value.is_finite()
                            {
                                by_round.entry(*r).or_default().push(m.value);
                            }
                        }
                        let rounds: BTreeMap<u32, f64> =
                            by_round.into_iter().filter_map(|(r, v)| median(&v).map(|m| (r, m))).collect();
                        let units_ok = ms.iter().all(|m| m.unit == unit && m.better == better);
                        if !units_ok {
                            let (u, b) = ms
                                .iter()
                                .find(|m| m.unit != unit || m.better != better)
                                .map(|m| (m.unit.clone(), m.better))
                                .expect("mismatch exists");
                            warnings.push(format!(
                                "{}: {workload} · {metric} is in {u} ({b:?}), expected {unit} ({better:?}); not compared",
                                s.label
                            ));
                        }
                        let values: Vec<f64> = ms.iter().map(|m| m.value).filter(|v| v.is_finite()).collect();
                        let shares: Vec<f64> = w
                            .runs
                            .iter()
                            .filter(|r| r.valid)
                            .filter_map(|r| metric_share(r, &metric))
                            .filter(|v| v.is_finite())
                            .collect();
                        let events: Vec<EventRun> = event_counters(&metric)
                            .map(|counters| {
                                w.runs
                                    .iter()
                                    // The runs the metric's value comes
                                    // from, and no others: a run can have
                                    // the counters and not the metric,
                                    // which is given from a number of
                                    // samples on, and its events are then
                                    // behind nothing that is compared.
                                    .filter(|r| r.valid && r.metrics.get(&metric).is_some_and(|m| m.value.is_finite()))
                                    .filter_map(|r| {
                                        let count = |name: &str| r.counters.get(name).copied();
                                        let mut events = 0;
                                        for name in counters.events {
                                            events += count(name)?;
                                        }
                                        Some(EventRun {
                                            round: r.round,
                                            events,
                                            qualifying: count(counters.qualifying)?,
                                            of: count(counters.of)?,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let spread = Spread::of(&values)?;
                        // Every run with the metric has its counters,
                        // or the share over all of them is not known.
                        let (counted, out_of) = events.iter().fold((0, 0), |(e, of), r| (e + r.events, of + r.of));
                        let pooled = (!events.is_empty() && events.len() == values.len() && out_of > 0)
                            .then(|| counted as f64 / out_of as f64 * 100.0);
                        Some(Cell {
                            unstable: spread.n >= 2 && spread.rel_iqr_pct() / 2.0 > opts.unstable_spread_pct,
                            spread,
                            values,
                            total_runs: w.runs.len(),
                            comparable: units_ok && base_params.is_none_or(|p| *p == w.params),
                            session: session_key(&s.file),
                            rounds,
                            share: median(&shares),
                            events,
                            pooled,
                        })
                    })
                    .collect();
                MetricRow {
                    workload,
                    metric,
                    unit,
                    better,
                    extreme,
                    unstable_spread_pct: opts.unstable_spread_pct,
                    min_share_pct: opts.min_share_pct,
                    cells,
                }
            })
            .collect::<Vec<_>>();

        // Paired comparisons without enough rounds for an interval.
        for (i, ser) in series.iter().enumerate() {
            if i == baseline {
                continue;
            }
            let deltas: Vec<Delta> = rows.iter().filter_map(|r| r.improvement(baseline, i)).collect();
            if deltas.iter().any(|d| d.unmatched_rounds) {
                warnings.push(format!(
                    "{}: same matrix session as the baseline but some metrics share no round with it; those are \
descriptive comparisons of medians only, not paired, without intervals",
                    ser.label
                ));
            }
            // Populations that are a very different part of the run on the
            // two sides, e.g. blocked round trips: nearly all of one
            // scheduler's, under half of the other's. Their latencies are
            // then not the same events, however stable each side is. This
            // is about latencies that are compared: a population one side
            // lacks altogether has no comparison to qualify, and none here.
            let mut mixes: BTreeMap<&str, BTreeMap<&str, (f64, f64)>> = BTreeMap::new();
            for r in &rows {
                let share = |k: usize| r.cells.get(k).and_then(Option::as_ref).and_then(|c| c.share);
                let (Some(b), Some(s), Some((population, _stat))) =
                    (share(baseline), share(i), r.metric.rsplit_once('_'))
                else {
                    continue;
                };
                if r.improvement(baseline, i).is_some()
                    && !r.minor_pair(baseline, i)
                    // Not two empty populations (possible with --min-share 0).
                    && b.max(s) > 0.0
                    && b.max(s) >= MIX_FACTOR * b.min(s)
                {
                    mixes.entry(&r.workload).or_default().insert(population, (b, s));
                }
            }
            for (workload, populations) in mixes {
                let list: Vec<String> = populations
                    .iter()
                    .map(|(p, (b, s))| {
                        format!("{p} ({} of the baseline's round trips, {} here)", fmt_share(*b), fmt_share(*s))
                    })
                    .collect();
                // The same finding as data, for the plain-words view.
                all_mixes.extend(populations.iter().map(|(p, (b, s))| Mix {
                    series: i,
                    workload: workload.to_string(),
                    population: p.to_string(),
                    baseline_share: *b,
                    share: *s,
                }));
                warnings.push(format!(
                    "{}: in {workload} the two schedulers produce a different mix of round trips: {}. Latencies of \
such a population describe different parts of the two runs; read them together with roundtrips_per_s, which covers \
all round trips",
                    ser.label,
                    list.join(", ")
                ));
            }
            let undefined = deltas.iter().filter(|d| d.undefined_rounds > 0).count();
            if undefined > 0 {
                warnings.push(format!(
                    "{}: for {undefined} metric(s) the baseline measured zero in some shared rounds, where a percentage \
change is undefined; those rounds are left out of the estimate, so these metrics are not scored",
                    ser.label
                ));
            }
            match deltas.iter().filter_map(|d| d.paired_rounds).min() {
                Some(n) if n < MIN_ROUNDS_FOR_CI => {
                    let paired = deltas.iter().filter(|d| d.paired_rounds.is_some()).count();
                    let few = deltas.iter().filter(|d| d.paired_rounds.is_some_and(|k| k < MIN_ROUNDS_FOR_CI)).count();
                    // All of them: the session is too short. Some: those metrics
                    // exist in few rounds only (e.g. a population that is usually empty).
                    let which =
                        if few == paired { "all metrics".to_string() } else { format!("{few} of {paired} metrics") };
                    warnings.push(format!(
                        "{}: {which} paired with the baseline over fewer than {MIN_ROUNDS_FOR_CI} rounds (as few as {n}); point estimates only for those, no intervals (a metric needs observations in at least {MIN_ROUNDS_FOR_CI} shared rounds)",
                        ser.label
                    ));
                }
                _ => {}
            }
        }

        let mut workloads: Vec<String> = Vec::new();
        for r in &rows {
            if !workloads.contains(&r.workload) {
                workloads.push(r.workload.clone());
            }
        }
        let distributions = workloads
            .iter()
            .flat_map(|name| {
                let mut pops: Vec<&String> = series
                    .iter()
                    .flat_map(|s| s.file.workloads.iter().filter(|w| &w.name == name))
                    .flat_map(|w| w.runs.iter().flat_map(|r| r.quantile_curves.keys()))
                    .collect();
                pops.sort();
                pops.dedup();
                pops.into_iter()
                    .filter_map(|p| distribution(&series, baseline, name, p, &mut warnings))
                    .collect::<Vec<_>>()
            })
            .collect();

        Comparison { series, baseline, opts, rows, distributions, diagnostics, warnings, env_notes, mixes: all_mixes }
    }
}

/// Share of one population in a run: from any of its own metrics
/// (`<population>_<stat>`, not a sub-population's `<population>_<sub>_<stat>`),
/// else from the counters as for `legacy_share`.
fn population_share(r: &RunResult, population: &str) -> Option<f64> {
    r.metrics
        .iter()
        .filter(|(k, _)| {
            k.strip_prefix(population).and_then(|s| s.strip_prefix('_')).is_some_and(|stat| !stat.contains('_'))
        })
        .find_map(|(_, m)| m.share)
        .or_else(|| legacy_share(&format!("{population}_max"), &r.counters))
}

/// Share of the population behind `metric` (`<population>_<stat>`) in a
/// run, whether or not the run has that metric. A run with too few events
/// for, say, a p99 still counts: taking only the runs that report the
/// metric would take the ones where the population happened to be largest,
/// and give two rows of one population different shares. `None` for
/// metrics over everything.
fn metric_share(r: &RunResult, metric: &str) -> Option<f64> {
    r.metrics.get(metric).and_then(|m| m.share).or_else(|| population_share(r, metric.rsplit_once('_')?.0))
}

/// Population share for wake-pingpong results written before
/// `Metric::share` existed, so that they can be re-rendered: metric
/// `rtt_<population>_<stat>` is backed by counter `roundtrips_<population>`
/// out of `roundtrips`.
fn legacy_share(metric: &str, counters: &BTreeMap<String, u64>) -> Option<f64> {
    let (population, _stat) = metric.strip_prefix("rtt_")?.rsplit_once('_')?;
    let n = *counters.get(&format!("roundtrips_{population}"))?;
    let total = *counters.get("roundtrips")?;
    (total > 0).then(|| n as f64 / total as f64)
}

/// The interleaved session a file belongs to, if it came from `matrix`.
///
/// Uses the session's random id; files written before it existed fall back
/// to start time (seconds) plus scheduler list, which two sessions started
/// in the same second with the same list would share.
pub fn session_key(f: &ResultFile) -> Option<SessionKey> {
    let d = f.design.as_ref().filter(|d| d.kind == "interleaved")?;
    Some(SessionKey(match &d.session_id {
        Some(id) => format!("id:{id}"),
        None => format!("legacy:{}:{}", f.started_at, d.schedulers.join("\u{1f}")),
    }))
}

/// A warning for every period of periodic-wake that is a whole number of
/// the nominal timer ticks of the kernel it ran on, naming the results it
/// is in.
///
/// Such a period can keep the releases at one phase against the tick for
/// a whole run. Where it did, under a scheduler that lets a woken task
/// wait for the tick, missed deadlines differed tenfold between runs by
/// nothing but the moment a run started, and hardly at all under other
/// schedulers (METHODOLOGY.md, "The period and the kernel's tick"). This
/// is about the nominal grid: where a CPU's ticks really are, offset by
/// `skew_tick` or stopped while it is idle, is not in a result file.
/// Files that do not record the tick rate, those of 0.5.0 among them,
/// cannot be checked and get no warning.
fn tick_aligned(series: &[Series]) -> Vec<String> {
    /// Workloads and labels.
    type Where<'a> = (Vec<&'a str>, Vec<&'a str>);
    // By period in µs, tick rate and whether a phase was set.
    let mut found: BTreeMap<(u64, u32, bool), Where> = BTreeMap::new();
    for s in series {
        let Some(hz) = s.file.system.tick_hz.filter(|hz| *hz > 0) else { continue };
        for w in s.file.workloads.iter().filter(|w| w.name.starts_with("periodic-wake")) {
            let Some(period) = w.params.get("period_us").and_then(|v| v.as_u64()) else { continue };
            // A whole number of ticks: period × rate is a whole number
            // of seconds. In 128 bits, where the product cannot overflow:
            // a saturated one would lose what is being asked of it.
            if period == 0 || !(u128::from(period) * u128::from(hz)).is_multiple_of(1_000_000) {
                continue;
            }
            let (workloads, labels) = found.entry((period, hz, w.params.get("phase_us").is_some())).or_default();
            if !workloads.contains(&w.name.as_str()) {
                workloads.push(&w.name);
            }
            if !labels.contains(&s.label.as_str()) {
                labels.push(&s.label);
            }
        }
    }
    found
        .into_iter()
        .map(|((period, hz, phase), (workloads, labels))| {
            let which = if phase {
                "Here the phase was set, and the results are for that phase"
            } else {
                "Here the phase is whatever each run happened to start with, and runs can differ for that reason"
            };
            format!(
                "{}: the period of {period} µs is a whole number of the kernel's nominal timer ticks ({hz} Hz) in \
                 {}. This can keep the releases at one phase against the tick for a whole run and make missed \
                 deadlines and the slowest wake-ups follow that phase, under some schedulers far more than under \
                 others. {which}. A period that is no whole number of ticks, such as 1010 µs, moves through the \
                 phases within a run, which limits this kind of dependence",
                workloads.join(", "),
                labels.join(", ")
            )
        })
        .collect()
}

/// Human-readable differences that can affect results.
pub fn env_diff(base: &ResultFile, other: &ResultFile) -> Vec<String> {
    let (a, b): (&SystemInfo, &SystemInfo) = (&base.system, &other.system);
    let mut out = Vec::new();
    let mut cmp = |what: &str, x: String, y: String| {
        if x != y {
            out.push(format!("{what} differs ({y} vs baseline {x})"));
        }
    };
    let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "?".into());
    cmp("CPU model", opt(&a.cpu_model), opt(&b.cpu_model));
    cmp("usable CPUs", a.usable_cpus.to_string(), b.usable_cpus.to_string());
    cmp("SMT", format!("{:?}", a.smt_active), format!("{:?}", b.smt_active));
    cmp("governor", opt(&a.governor), opt(&b.governor));
    cmp("EPP", opt(&a.epp), opt(&b.epp));
    cmp("platform profile", opt(&a.platform_profile), opt(&b.platform_profile));
    cmp("kernel", a.kernel.clone(), b.kernel.clone());
    cmp("tool version", base.tool_version.clone(), other.tool_version.clone());
    // What newer files also record. A file without a field (written
    // before it existed, or by a user who may not read it) says nothing,
    // and nothing is not a difference.
    let mut known = |what: &str, x: Option<String>, y: Option<String>| {
        if let (Some(x), Some(y)) = (x, y) {
            cmp(what, x, y);
        }
    };
    let words = |v: &[String]| if v.is_empty() { "none".to_string() } else { v.join(" ") };
    let power = |ac: bool| if ac { "mains" } else { "battery" }.to_string();
    let switch = |on: bool| if on { "on" } else { "off" }.to_string();
    known("kernel build", a.kernel_build.clone(), b.kernel_build.clone());
    known("preemption model", a.preempt.clone(), b.preempt.clone());
    known("timer tick", a.tick_hz.map(|hz| format!("{hz} Hz")), b.tick_hz.map(|hz| format!("{hz} Hz")));
    // The parameters are read wherever the build is.
    known(
        "kernel parameters",
        a.kernel_build.as_ref().map(|_| words(&a.cmdline)),
        b.kernel_build.as_ref().map(|_| words(&b.cmdline)),
    );
    known("CPUs the benchmark may use", a.allowed_cpus.clone(), b.allowed_cpus.clone());
    known("CPU topology", a.topology.as_ref().map(|t| format!("{t:?}")), b.topology.as_ref().map(|t| format!("{t:?}")));
    known(
        "isolated CPUs",
        a.topology.as_ref().map(|_| opt(&a.isolated_cpus)),
        b.topology.as_ref().map(|_| opt(&b.isolated_cpus)),
    );
    known("cpufreq driver", a.cpufreq_driver.clone(), b.cpufreq_driver.clone());
    known("frequency boost", a.boost.map(switch), b.boost.map(switch));
    known("power source", a.on_ac.map(power), b.on_ac.map(power));
    let taken = |f: &ResultFile| {
        f.protocol.as_ref().map(|p| {
            format!(
                "{} runs of {} s, {} warm-up, {} s pause, suite {}",
                p.runs, p.duration_s, p.warmup, p.cooldown_s, p.suite
            )
        })
    };
    known("how the runs were taken", taken(base), taken(other));
    out
}

/// The quantile unit shared by every valid run of `s` that has a curve for
/// `population`. `Err(())` if those runs disagree; `None` if there are none.
fn curve_unit(s: &Series, workload: &str, population: &str) -> Option<Result<String, ()>> {
    let w = s.file.workloads.iter().find(|w| w.name == workload)?;
    let mut units = w
        .runs
        .iter()
        .filter(|r| r.valid && r.quantile_curves.contains_key(population))
        .map(|r| r.quantile_unit.clone().unwrap_or_default());
    let first = units.next()?;
    Some(if units.all(|u| u == first) { Ok(first) } else { Err(()) })
}

/// Median of each quantile across valid runs, for one population.
///
/// Reference unit: the baseline's, if it has a consistent unit for this
/// curve, otherwise the first series that does. A series is left out (with a
/// warning) when its runs disagree on the unit or it differs from the
/// reference, and when its workload parameters differ from the baseline's.
/// A quantile is only plotted where every contributing run supports it.
fn distribution(
    series: &[Series],
    baseline: usize,
    workload: &str,
    population: &str,
    warnings: &mut Vec<String>,
) -> Option<Distribution> {
    let base_params = series[baseline].file.workloads.iter().find(|w| w.name == workload).map(|w| &w.params);
    let units: Vec<Option<Result<String, ()>>> = series.iter().map(|s| curve_unit(s, workload, population)).collect();
    let reference = match &units[baseline] {
        Some(Ok(u)) => Some(u.clone()),
        _ => units.iter().find_map(|u| u.as_ref().and_then(|u| u.as_ref().ok()).cloned()),
    }?;

    let curves: Vec<Option<Vec<(f64, f64)>>> = series
        .iter()
        .zip(&units)
        .map(|(s, unit)| {
            let w = s.file.workloads.iter().find(|w| w.name == workload)?;
            if base_params.is_some_and(|p| *p != w.params) {
                return None;
            }
            match unit.as_ref()? {
                Ok(u) if *u == reference => {}
                Ok(u) => {
                    warnings.push(format!(
                        "{}: {workload} · {population} quantiles are in {u}, expected {reference}; left out of the chart",
                        s.label
                    ));
                    return None;
                }
                Err(()) => {
                    warnings.push(format!(
                        "{}: {workload} · {population} runs use different quantile units; left out of the chart",
                        s.label
                    ));
                    return None;
                }
            }
            let runs: Vec<_> = w.runs.iter().filter(|r| r.valid).collect();
            let per_run: Vec<&Vec<(f64, f64)>> =
                runs.iter().filter_map(|r| r.quantile_curves.get(population)).collect();
            if per_run.is_empty() || per_run.len() < runs.len() {
                return None;
            }
            // Quantiles present in every run's curve.
            let shortest = per_run.iter().map(|c| c.len()).min().unwrap_or(0);
            let curve: Vec<(f64, f64)> = (0..shortest)
                .filter_map(|i| {
                    let q = per_run[0][i].0;
                    let vals: Vec<f64> = per_run.iter().map(|c| c[i].1).collect();
                    Spread::of(&vals).map(|s| (q, s.median))
                })
                .collect();
            (!curve.is_empty()).then_some(curve)
        })
        .collect();
    let shares = series
        .iter()
        .map(|s| {
            let w = s.file.workloads.iter().find(|w| w.name == workload)?;
            let v: Vec<f64> = w
                .runs
                .iter()
                .filter(|r| r.valid)
                .filter_map(|r| population_share(r, population))
                .filter(|v| v.is_finite())
                .collect();
            median(&v)
        })
        .collect();
    curves.iter().any(Option::is_some).then(|| Distribution {
        workload: workload.to_string(),
        population: population.to_string(),
        unit: reference,
        curves,
        shares,
    })
}

/// Make labels unique by appending `#2`, `#3`… to repeats.
fn disambiguate(files: Vec<ResultFile>) -> Vec<(String, ResultFile)> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    files
        .into_iter()
        .map(|f| {
            let n = seen.entry(f.label.clone()).or_insert(0);
            *n += 1;
            let label = if *n == 1 { f.label.clone() } else { format!("{} #{n}", f.label) };
            (label, f)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::{Metric, Scheduler, WorkloadResult};
    use serde_json::json;

    fn file(label: &str, p99: &[f64], pairs: u32) -> ResultFile {
        let runs = p99
            .iter()
            .map(|v| {
                let mut r = RunResult { valid: true, duration_s: 1.0, ..Default::default() };
                r.metrics.insert("p99".into(), Metric::lower(*v, "us"));
                r.metrics.insert("ops".into(), Metric::higher(*v * 10.0, "1/s"));
                r.counters.insert("roundtrips".into(), 10);
                r
            })
            .collect();
        ResultFile {
            schema: helicoprion_core::SCHEMA_VERSION,
            tool_version: "test".into(),
            label: label.into(),
            started_at: 0,
            system: SystemInfo { kernel: "6.18".into(), usable_cpus: 4, ..Default::default() },
            scheduler: Scheduler::Fair { class: "EEVDF".into(), scx_capable: false, bore: None },
            restart_detectable: true,
            design: None,
            protocol: None,
            workloads: vec![WorkloadResult {
                name: "w".into(),
                description: String::new(),
                params: json!({ "pairs": pairs }),
                runs,
            }],
        }
    }

    #[test]
    fn improvement_respects_direction() {
        let c = Comparison::build(vec![file("base", &[100.0], 2), file("cand", &[80.0], 2)], 0);
        let p99 = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        let ops = c.rows.iter().find(|r| r.metric == "ops").unwrap();
        // 80 vs 100 us latency: 20% better.
        assert!((p99.improvement(0, 1).unwrap().median - 20.0).abs() < 1e-9);
        // 800 vs 1000 ops/s: 20% worse.
        assert!((ops.improvement(0, 1).unwrap().median + 20.0).abs() < 1e-9);
        // One run each: no interval.
        assert!(p99.improvement(0, 1).unwrap().ci.is_none());
    }

    #[test]
    fn ci_with_enough_runs() {
        let c = Comparison::build(
            vec![file("base", &[100.0, 101.0, 99.0, 100.5], 2), file("cand", &[80.0, 81.0, 79.0, 80.5], 2)],
            0,
        );
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert!(d.ci_excludes_zero(), "{d:?}");
    }

    #[test]
    fn different_params_are_not_compared() {
        let c = Comparison::build(vec![file("base", &[100.0], 1), file("cand", &[80.0], 8)], 0);
        assert!(c.rows[0].improvement(0, 1).is_none());
        assert!(c.warnings.iter().any(|w| w.contains("not compared")));
    }

    #[test]
    fn unit_mismatch_is_not_compared() {
        let base = file("base", &[100.0], 1);
        let mut other = file("other", &[100_000.0], 1);
        for r in &mut other.workloads[0].runs {
            r.metrics.get_mut("p99").unwrap().unit = "ns".into();
        }
        let c = Comparison::build(vec![base, other], 0);
        let p99 = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        assert!(p99.improvement(0, 1).is_none());
        assert!(c.warnings.iter().any(|w| w.contains("expected us")));
    }

    #[test]
    fn units_follow_the_chosen_baseline() {
        // First file uses ns; the baseline (index 1) and the third use us.
        let mut first = file("first", &[100_000.0], 1);
        for r in &mut first.workloads[0].runs {
            r.metrics.get_mut("p99").unwrap().unit = "ns".into();
        }
        let c = Comparison::build(vec![first, file("base", &[100.0], 1), file("third", &[80.0], 1)], 1);
        let p99 = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        assert_eq!(p99.unit, "us");
        assert!(p99.improvement(1, 2).is_some(), "two consistent series must be compared");
        assert!(p99.improvement(1, 0).is_none());
    }

    #[test]
    fn curves_skip_mismatched_params() {
        let c = Comparison::build(
            vec![
                with_curve(file("b", &[1.0], 1), "us"),
                with_curve(file("same", &[1.0], 1), "us"),
                with_curve(file("diff", &[1.0], 8), "us"),
            ],
            0,
        );
        let d = &c.distributions[0];
        assert_eq!(d.population, "rtt_blocked");
        assert!(d.curves[0].is_some() && d.curves[1].is_some());
        assert!(d.curves[2].is_none());
    }

    fn with_curve(mut f: ResultFile, unit: &str) -> ResultFile {
        for r in &mut f.workloads[0].runs {
            r.quantile_curves.insert("rtt_blocked".into(), vec![(0.5, 10.0), (0.9, 20.0)]);
            r.quantile_unit = Some(unit.into());
        }
        f
    }

    #[test]
    fn curve_units_follow_the_chosen_baseline() {
        // First file in ns; baseline (index 1) and third in us.
        let c = Comparison::build(
            vec![
                with_curve(file("first", &[1.0], 1), "ns"),
                with_curve(file("base", &[1.0], 1), "us"),
                with_curve(file("third", &[1.0], 1), "us"),
            ],
            1,
        );
        let d = &c.distributions[0];
        assert_eq!(d.unit, "us");
        assert!(d.curves[0].is_none());
        assert!(d.curves[1].is_some() && d.curves[2].is_some());
        assert!(c.warnings.iter().any(|w| w.contains("first") && w.contains("expected us")));
    }

    #[test]
    fn mixed_curve_units_within_a_series_are_left_out() {
        let mut mixed = with_curve(file("mixed", &[1.0, 2.0], 1), "us");
        mixed.workloads[0].runs[1].quantile_unit = Some("ns".into());
        let c = Comparison::build(vec![with_curve(file("base", &[1.0], 1), "us"), mixed], 0);
        assert!(c.distributions[0].curves[1].is_none());
        assert!(c.warnings.iter().any(|w| w.contains("different quantile units")));
    }

    #[test]
    fn info_metrics_are_never_scored() {
        let mut base = file("base", &[1.0, 1.0, 1.0], 1);
        let mut cand = file("cand", &[1.0, 1.0, 1.0], 1);
        for f in [&mut base, &mut cand] {
            for (i, r) in f.workloads[0].runs.iter_mut().enumerate() {
                r.metrics.insert("hog".into(), Metric::info(100.0 + i as f64, "units/s"));
            }
        }
        let c = Comparison::build(vec![base, cand], 0);
        let hog = c.rows.iter().find(|r| r.metric == "hog").unwrap();
        assert!(hog.improvement(0, 1).is_none());
        assert!(!hog.chartable(0, 1));
    }

    #[test]
    fn extremes_and_unstable_cells_stay_off_the_chart() {
        let mut base = file("base", &[10.0, 10.0, 10.0, 10.0], 1);
        let mut cand = file("cand", &[1.0, 10.0, 30.0, 90.0], 1); // wildly unstable
        for f in [&mut base, &mut cand] {
            for r in &mut f.workloads[0].runs {
                let v = r.metrics["p99"].value;
                r.metrics.insert("max".into(), Metric::lower(v, "us").extreme());
            }
        }
        let c = Comparison::build(vec![base, cand], 0);
        let p99 = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        assert!(p99.cells[1].as_ref().unwrap().unstable);
        assert!(p99.improvement(0, 1).is_some(), "still in the table");
        assert!(!p99.chartable(0, 1));
        let max = c.rows.iter().find(|r| r.metric == "max").unwrap();
        assert!(max.extreme && !max.chartable(0, 1));
    }

    /// Blocked/fast populations whose shares are given per side: `blocked`
    /// out of 1000 round trips, the rest fast. `share` says whether the
    /// metrics carry `Metric::share` or only the counters do (older files).
    fn populations(label: &str, blocked: u64, p50: f64, share: bool) -> ResultFile {
        let mut f = file(label, &[1.0; 5], 1);
        for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
            let jitter = 1.0 + k as f64 * 1e-3;
            for (pop, n) in [("blocked", blocked), ("fast", 1000 - blocked)] {
                let s = n as f64 / 1000.0;
                let m = Metric::lower(p50 * jitter, "us");
                r.metrics.insert(format!("rtt_{pop}_p50"), if share { m.share(s) } else { m });
                r.counters.insert(format!("roundtrips_{pop}"), n);
            }
            r.counters.insert("roundtrips".into(), 1000);
        }
        f
    }

    #[test]
    fn minor_populations_are_compared_but_not_scored() {
        for share in [true, false] {
            // Candidate: only 5 of 1000 round trips are fast, at half the latency.
            let c =
                Comparison::build(vec![populations("base", 500, 10.0, share), populations("cand", 995, 5.0, share)], 0);
            let row = |m: &str| c.rows.iter().find(|r| r.metric == m).unwrap();
            let fast = row("rtt_fast_p50");
            assert!((fast.cells[1].as_ref().unwrap().share.unwrap() - 0.005).abs() < 1e-12, "share={share}");
            assert!(!fast.minor(0) && fast.minor(1) && fast.minor_pair(0, 1));
            let d = fast.improvement(0, 1).expect("still in the table");
            assert!(d.notable(5.0), "large, tight change...");
            assert!(!fast.chartable(0, 1), "...but a minor population is not charted");
            let blocked = row("rtt_blocked_p50");
            assert!(!blocked.minor_pair(0, 1) && blocked.chartable(0, 1));
            // Whole-run metrics have no share.
            assert!(row("p99").cells[0].as_ref().unwrap().share.is_none());
            // Rendered: tagged, never starred or coloured.
            let line = |m: &str| {
                crate::html::text_table(&c).lines().find(|l| l.starts_with(&format!("w·{m} "))).unwrap().to_string()
            };
            assert!(line("rtt_fast_p50").contains("°") && !line("rtt_fast_p50").contains('*'));
            assert!(!line("rtt_blocked_p50").contains("°"));
            let html = crate::html::report(&c);
            assert!(html.contains(">minor 0.5%</span>"));
            // Explanations are folded away; each section keeps a one-line caption.
            assert_eq!(html.matches("<details class=help>").count(), html.matches("<p class=cap>").count());
            assert!(
                !html.contains("<p class=sub>Bars:")
                    && html.contains("<summary>How to read this</summary><p>For most metrics: median")
            );
        }
        let opts = Options { min_share_pct: 0.0, ..Options::default() };
        let c = Comparison::build_with(
            vec![populations("base", 500, 10.0, true), populations("cand", 995, 5.0, true)],
            0,
            opts,
        );
        let fast = c.rows.iter().find(|r| r.metric == "rtt_fast_p50").unwrap();
        assert!(!fast.minor_pair(0, 1) && fast.chartable(0, 1), "threshold 0 turns it off");
    }

    /// 1000 round trips: 900 blocked, of which `same` on the same CPU, and
    /// 100 fast. Sub-population names extend their parent's
    /// (`rtt_blocked_same_cpu_p50` vs `rtt_blocked_p50`) and sort before it.
    fn sub_populations(label: &str, same: u64, share: bool) -> ResultFile {
        let mut f = file(label, &[1.0; 5], 1);
        for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
            let jitter = 1.0 + k as f64 * 1e-3;
            for (pop, n) in
                [("blocked", 900), ("blocked_same_cpu", same), ("blocked_cross_cpu", 900 - same), ("fast", 100)]
            {
                let m = Metric::lower(10.0 * jitter, "us");
                r.metrics.insert(format!("rtt_{pop}_p50"), if share { m.share(n as f64 / 1000.0) } else { m });
                r.counters.insert(format!("roundtrips_{pop}"), n);
            }
            r.counters.insert("roundtrips".into(), 1000);
        }
        f
    }

    #[test]
    fn sub_population_shares_are_not_mixed_with_their_parent() {
        for share in [true, false] {
            let c = Comparison::build(vec![sub_populations("base", 450, share), sub_populations("cand", 4, share)], 0);
            let row = |m: &str| c.rows.iter().find(|r| r.metric == m).unwrap();
            let cand = |m: &str| row(m).cells[1].as_ref().unwrap().share.unwrap();
            let near = |a: f64, b: f64| (a - b).abs() < 1e-12;
            assert!(near(cand("rtt_blocked_p50"), 0.9), "share={share}");
            assert!(near(cand("rtt_blocked_same_cpu_p50"), 0.004), "share={share}");
            assert!(near(cand("rtt_blocked_cross_cpu_p50"), 0.896), "share={share}");
            assert!(row("rtt_blocked_same_cpu_p50").minor_pair(0, 1));
            assert!(!row("rtt_blocked_p50").minor_pair(0, 1) && !row("rtt_blocked_cross_cpu_p50").minor_pair(0, 1));

            // The share of a whole population, as the quantile charts take it.
            let run = &c.series[1].file.workloads[0].runs[0];
            for (pop, want) in [
                ("rtt_blocked", 0.9),
                ("rtt_blocked_same_cpu", 0.004),
                ("rtt_blocked_cross_cpu", 0.896),
                ("rtt_fast", 0.1),
            ] {
                assert!(population_share(run, pop).is_some_and(|s| near(s, want)), "{pop}, share={share}");
            }
            assert!(population_share(run, "rtt").is_none() && population_share(run, "rtt_blocked_same").is_none());
        }
    }

    #[test]
    fn summary_counts_only_clear_changes() {
        // One workload, lower is better: p50 and p99 (the helper's metrics).
        let c = Comparison::build(
            vec![
                file("base", &[100.0, 101.0, 99.0, 100.5, 99.5], 1),
                file("same", &[100.2, 100.8, 99.4, 100.1, 99.9], 1),
                file("slow", &[150.0, 151.0, 149.0, 150.5, 149.5], 1),
                file("other", &[50.0, 50.5, 49.5, 50.2, 49.8], 8),
            ],
            0,
        );
        let s = c.summary();
        let of = |label: &str| s.iter().find(|s| c.series[s.series].label == label);
        let same = of("same").unwrap();
        assert_eq!((same.verdict(), same.better, same.worse), (Verdict::NoClearDifference, 0, 0));
        assert_eq!(same.unclear(), same.compared);
        // The helper's two metrics move together: a higher p99 (worse) comes
        // with more ops (better), so the count does not pick a side.
        let slow = of("slow").unwrap();
        assert_eq!((slow.verdict(), slow.better, slow.worse, slow.unclear()), (Verdict::Mixed, 1, 1, 0));
        assert_eq!(slow.top.len(), 2);
        assert!(of("other").is_none(), "different parameters: nothing to compare");

        // A minor population's change is compared, never counted as clear.
        let c = Comparison::build(vec![populations("base", 500, 10.0, true), populations("cand", 995, 5.0, true)], 0);
        let s = &c.summary()[0];
        assert!(!s.top.iter().any(|(m, _)| m == "rtt_fast_p50"));
        assert!(s.top.iter().any(|(m, _)| m == "rtt_blocked_p50"));
        assert_eq!((s.verdict(), s.better, s.worse), (Verdict::Better, 1, 0));
        // The same two files with the other one as baseline read as worse.
        let flipped =
            Comparison::build(vec![populations("base", 500, 10.0, true), populations("cand", 995, 5.0, true)], 1);
        assert_eq!(flipped.summary()[0].verdict(), Verdict::Worse);

        // Rendered in both outputs.
        let html = crate::html::report(&c);
        assert!(html.contains("<h2>Summary</h2>") && html.contains("class=good>better</strong>"));
        assert!(crate::html::text_table(&c).contains("cand vs base · w: better"));
    }

    #[test]
    fn a_population_has_one_share_whichever_runs_report_the_metric() {
        for share in [true, false] {
            // 5 runs; the population is 0.1% of round trips except in the last
            // run (4%), the only one with enough events for a p99.
            let mut f = file("a", &[1.0; 5], 1);
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let n = if k == 4 { 40 } else { 1 };
                let m = |v: f64| {
                    let m = Metric::lower(v, "us");
                    if share { m.share(n as f64 / 1000.0) } else { m }
                };
                r.metrics.insert("rtt_blocked_same_cpu_p50".into(), m(5.0));
                if k == 4 {
                    r.metrics.insert("rtt_blocked_same_cpu_p99".into(), m(60.0));
                }
                r.counters.insert("roundtrips_blocked_same_cpu".into(), n);
                r.counters.insert("roundtrips".into(), 1000);
            }
            let c = Comparison::build(vec![f.clone(), f], 0);
            let s = |m: &str| c.rows.iter().find(|r| r.metric == m).unwrap().cells[0].as_ref().unwrap().share;
            assert_eq!(s("rtt_blocked_same_cpu_p50"), Some(0.001), "share={share}");
            assert_eq!(s("rtt_blocked_same_cpu_p99"), Some(0.001), "share={share}: not 4% from the one run");
            assert_eq!(s("p99"), None);
        }
    }

    #[test]
    fn a_different_mix_of_populations_is_warned_about() {
        let mix = |c: &Comparison| c.warnings.iter().find(|w| w.contains("different mix")).cloned();
        for share in [true, false] {
            // Blocked round trips: 30% of the baseline's, 90% of the candidate's.
            let c =
                Comparison::build(vec![populations("base", 300, 10.0, share), populations("cand", 900, 5.0, share)], 0);
            let w = mix(&c).expect("warning");
            assert!(w.starts_with("cand: in w the two schedulers produce a different mix"), "{w}");
            assert!(w.contains("rtt_blocked (30% of the baseline's round trips, 90% here)"), "{w}");
            assert!(w.contains("rtt_fast (70% of the baseline's round trips, 10% here)"), "{w}");
            // A warning only: the change is still scored.
            let row = c.rows.iter().find(|r| r.metric == "rtt_blocked_p50").unwrap();
            assert!(c.clear(row, 1, &row.improvement(0, 1).unwrap()));

            // A similar mix, and a population that is minor on one side
            // (tagged as such already), raise nothing.
            for (b, s) in [(500, 600), (500, 995)] {
                let c =
                    Comparison::build(vec![populations("base", b, 10.0, share), populations("cand", s, 5.0, share)], 0);
                assert_eq!(mix(&c), None, "{b} vs {s}");
            }

            // The threshold is a factor of exactly 2, inclusive: 20% vs 40%
            // warns (the other population, 80% vs 60%, does not), 20% vs
            // 39.9% does not.
            let at = |b, s| {
                mix(&Comparison::build(
                    vec![populations("base", b, 10.0, share), populations("cand", s, 5.0, share)],
                    0,
                ))
            };
            let w = at(200, 400).expect("exactly 2x");
            assert!(
                w.contains("rtt_blocked (20% of the baseline's round trips, 40% here)") && !w.contains("rtt_fast"),
                "{w}"
            );
            assert_eq!(at(200, 399), None);

            // With --min-share 0 nothing is minor, so a zero share gets this
            // far: zero on both sides is the same mix, zero on one side only
            // is as different as a mix gets. This holds where the latency
            // is compared at all, as with this helper, which writes a p50
            // even for an empty population. The workload does not: a
            // population with no round trips in any run has no quantiles,
            // so no row is compared and nothing is warned about. A zero
            // median share with the metric present comes from a population
            // that is empty in most runs but not all.
            let no_minor = Options { min_share_pct: 0.0, ..Options::default() };
            let at = |b, s| {
                let files = vec![populations("base", b, 10.0, share), populations("cand", s, 5.0, share)];
                mix(&Comparison::build_with(files, 0, no_minor))
            };
            assert_eq!(at(0, 0), None);
            let w = at(0, 300).expect("zero share on one side only, metric present");
            assert!(w.contains("rtt_blocked (0.00% of the baseline's round trips, 30% here)"), "{w}");
        }
    }

    /// The plain-words view says what the full report scores, no more.
    #[test]
    fn in_short_says_it_in_plain_words() {
        // A busy-system session shaped like scx_cake vs scx_pandemonium.
        let session = |workload: &str, cand: [f64; 4]| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = workload.into();
                for r in &mut f.workloads[0].runs {
                    // The helper's own drift and jitter, without its 10% effect.
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    let x = |base: f64, k: usize| (if is_cand { cand[k] } else { base }) * wobble;
                    r.metrics.clear();
                    r.metrics.insert("roundtrips_per_s".into(), Metric::higher(x(542.4e3, 0), "1/s"));
                    r.metrics.insert("rtt_blocked_p50".into(), Metric::lower(x(14.0, 1), "us"));
                    r.metrics.insert("rtt_blocked_p99.9".into(), Metric::lower(x(45.6, 2), "us"));
                    r.metrics.insert("hog_work_per_s".into(), Metric::info(x(1.61e6, 3), "units/s"));
                }
            }
            Comparison::build(files, 0)
        };
        let c = session("wake-pingpong/loaded", [216.5e3, 36.1, 1335.0, 2.18e6]);
        let text = crate::simple::text(&c);
        for want in [
            "In short, compared with base:",
            "\ncand\n  Busy system (every CPU kept busy): WORSE\n",
            "• Round trips completed: 2.5× fewer (",
            "• Typical wake-up delay: 2.6× longer (",
            "• Slowest wake-ups (1 in 1000): 29× longer (",
            " µs)",
            " per second)",
            "• Background CPU work: 35% higher (observed, not scored)",
            "• Of 3 measurements, 0 clearly better and 3 clearly worse",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
        // The same words open the HTML report; --simple folds the rest away.
        let full = crate::html::report(&c);
        let simple = crate::html::report_with(&c, true);
        for html in [&full, &simple] {
            let (short, summary) = (html.find("<h2>In short</h2>").unwrap(), html.find("<h2>Summary</h2>").unwrap());
            assert!(short < summary && html.contains("<li>Typical wake-up delay: 2.6× longer ("));
        }
        assert!(!full.contains("<details class=full>"));
        let folded = simple.find("<details class=full>").unwrap();
        assert!(
            simple.find("<h2>In short</h2>").unwrap() < folded && folded < simple.find("<h2>Summary</h2>").unwrap()
        );
        assert!(simple.contains("</details><footer>"), "everything up to the footer is folded");

        // No clear change is said as such, never as a small "better".
        let c = session("wake-pingpong", [545.0e3, 14.1, 45.0, 1.62e6]);
        let text = crate::simple::text(&c);
        assert!(text.contains("\ncand\n  Idle system: NO CLEAR DIFFERENCE\n"), "{text}");
        assert!(text.contains("• Round trips completed: no reliable difference; "), "{text}");
        assert!(text.contains("• Background CPU work: about the same (observed, not scored)"), "{text}");
        assert!(!text.contains("fewer") && !text.contains("longer"), "{text}");
        // Background work is left out unless both sides are comparable.
        let mut files = drifting_session();
        for f in &mut files {
            f.workloads[0].name = "wake-pingpong/loaded".into();
            for r in &mut f.workloads[0].runs {
                r.metrics.insert("hog_work_per_s".into(), Metric::info(1.0e6, "units/s"));
            }
        }
        files[0].workloads[0].runs.iter_mut().for_each(|r| {
            r.metrics.insert("hog_work_per_s".into(), Metric::info(1.0e6, "other-unit"));
        });
        assert!(!crate::simple::text(&Comparison::build(files, 0)).contains("Background CPU work"));

        // Warnings are folded away with the full report, but not silently.
        let mut c = session("wake-pingpong/loaded", [216.5e3, 36.1, 1335.0, 2.18e6]);
        assert_eq!(crate::simple::caution(&c), None);
        assert!(!crate::html::report_with(&c, true).contains("require caution"));
        c.warnings.push("cand: something to know".into());
        let caution = crate::simple::caution(&c).unwrap();
        assert_eq!(caution, "Some measurements require caution: see the 1 warning in the full report.");
        let simple = crate::html::report_with(&c, true);
        let (said, folded) = (simple.find(&caution).unwrap(), simple.find("<details class=full>").unwrap());
        assert!(said < folded && folded < simple.find("something to know").unwrap());
        assert!(!crate::html::report(&c).contains("require caution"), "the full report shows the warnings themselves");

        // The same for results taken on different setups, said apart from
        // the warnings: it is not a flaw in a measurement.
        assert_eq!(crate::simple::environment(&c), None);
        c.env_notes.extend(["cand: kernel differs".to_string(), "cand: governor differs".to_string()]);
        let environment = crate::simple::environment(&c).unwrap();
        assert_eq!(
            environment,
            "The results were not all taken on the same setup: see the 2 notes in the full report."
        );
        assert_eq!(crate::simple::caution(&c).unwrap(), caution, "notes are not counted as warnings");
        let simple = crate::html::report_with(&c, true);
        let (said, folded) = (simple.find(&environment).unwrap(), simple.find("<details class=full>").unwrap());
        assert!(said < folded && folded < simple.find("governor differs").unwrap());
        assert!(!crate::html::report(&c).contains("not all taken on the same setup"));

        // A change held back only because its size varied between rounds
        // by more than the effect's cautious end (here 1.2× to 3× fewer
        // round trips, alternating) still has a supported direction: it is
        // said with its interval, not as "no reliable difference". It stays
        // out of the verdict and the tally.
        let mut files = drifting_session();
        for (f, is_cand) in files.iter_mut().zip([false, true]) {
            f.workloads[0].name = "wake-pingpong/loaded".into();
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                let factor = match (is_cand, (k / 2) % 2 == 0) {
                    (false, _) => 1.0,
                    (true, true) => 1.0 / 1.2,
                    (true, false) => 1.0 / 3.0,
                };
                r.metrics.clear();
                r.metrics.insert("roundtrips_per_s".into(), Metric::higher(1.2e6 * factor * wobble, "1/s"));
                r.metrics.insert("rtt_blocked_p50".into(), Metric::lower(10.0 * wobble, "us"));
            }
        }
        let c = Comparison::build(files, 0);
        let row = c.rows.iter().find(|r| r.metric == "roundtrips_per_s").unwrap();
        let d = row.improvement(0, 1).unwrap();
        assert!(!c.clear(row, 1, &d) && c.held_back_by_unevenness(row, 1, &d).is_some(), "{d:?}");
        let text = crate::simple::text(&c);
        assert!(
            text.contains(
                "• Round trips completed: fewer; the effect varied too much between rounds to score \
                 (estimated effect, 95% interval: 1.2× to 3.0×; "
            ),
            "{text}"
        );
        assert!(text.contains("\ncand\n  Busy system (every CPU kept busy): NO CLEAR DIFFERENCE\n"), "{text}");
        assert!(text.contains("• Of 2 measurements, 0 clearly better and 0 clearly worse"), "{text}");
        assert!(text.contains("• Typical wake-up delay: no reliable difference; "), "{text}");

        // Two workloads of one scheduler share a card: the scheduler once,
        // then a part per workload.
        let mut files = drifting_session();
        for f in &mut files {
            let mut idle = f.workloads[0].clone();
            idle.name = "wake-pingpong".into();
            f.workloads[0].name = "wake-pingpong/loaded".into();
            f.workloads.insert(0, idle);
        }
        let c = Comparison::build(files, 0);
        let briefs = crate::simple::briefs(&c);
        assert_eq!(briefs.len(), 2);
        let groups = crate::simple::by_scheduler(&briefs);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].1.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(),
            ["Idle system", "Busy system (every CPU kept busy)"]
        );
        let text = crate::simple::text(&c);
        assert_eq!(text.matches("\ncand\n").count(), 1, "{text}");
        let (idle, busy) = (
            text.find("  Idle system: BETTER\n").unwrap(),
            text.find("  Busy system (every CPU kept busy): BETTER\n").unwrap(),
        );
        assert!(idle < busy && text[idle..busy].contains("    • p99: +10.0%"), "{text}");
        let html = crate::html::report(&c);
        assert_eq!(html.matches("<div class=brief>").count(), 1);
        assert_eq!(html.matches("<h4>").count(), 2);
        assert!(html.contains("</i>cand</h3><h4>Idle system: <span class=good>better</span></h4>"), "{html}");

        // A workload without plain words still gets its largest changes.
        let c = Comparison::build(drifting_session(), 0);
        let text = crate::simple::text(&c);
        assert!(text.contains("\ncand\n  w: BETTER\n") && text.contains("• p99: +10.0%"), "{text}");
        assert!(text.contains("• Of 1 measurements, 1 clearly better and 0 clearly worse"), "{text}");

        // One file: nothing to compare, said plainly.
        let one = Comparison::build(vec![file("solo", &[1.0; 5], 1)], 0);
        let why = "Nothing to compare with solo yet: a comparison needs results from two schedulers.";
        assert_eq!(crate::simple::text(&one).trim_end(), why);
        // --simple folds the full report away, so the page says it too; the
        // full report shows what there is and needs no such line.
        let simple = crate::html::report_with(&one, true);
        let said = simple.find(&format!("<h2>In short</h2><p class=sub>{why}</p>")).expect("explained");
        assert!(said < simple.find("<details class=full>").unwrap());
        assert!(!crate::html::report(&one).contains("In short"));

        // Two files that share nothing comparable (other workload
        // parameters): a different reason, not "needs two schedulers".
        let apart = Comparison::build(vec![file("one", &[1.0; 5], 1), file("other", &[2.0; 5], 8)], 0);
        assert!(crate::simple::briefs(&apart).is_empty());
        let why = crate::simple::nothing_to_compare(&apart);
        assert!(why.starts_with("The results cannot be compared with one: they were not measured the same way."));
        assert!(crate::html::report_with(&apart, true).contains(&why) && crate::simple::text(&apart).contains(&why));
    }

    /// "at least" belongs to a scored change only. Through the whole path:
    /// a tail 54× shorter in five rounds of six and 1.5× in one is scored,
    /// with a wide interval; the same numbers on a minor population, or with
    /// rounds too uneven to score, are not, and must not read as if they were.
    #[test]
    fn at_least_is_said_only_for_scored_changes() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Case {
            Scored,
            Minor,
            Uneven,
        }
        let session = |case: Case| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "wake-pingpong/loaded".into();
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    let shorter = match (case, k / 2) {
                        // Half the rounds 54×, half 1.5×: nothing like one factor.
                        (Case::Uneven, round) if round % 2 == 1 => 1.5,
                        (Case::Scored | Case::Minor, 2) => 1.5,
                        _ => 54.0,
                    };
                    let v = 1434.0 * wobble / if is_cand { shorter } else { 1.0 };
                    let m = Metric::lower(v, "us");
                    r.metrics.clear();
                    r.metrics.insert("rtt_blocked_p99.9".into(), if case == Case::Minor { m.share(0.001) } else { m });
                }
            }
            Comparison::build(files, 0)
        };
        let line = |c: &Comparison| {
            crate::simple::text(c).lines().find(|l| l.contains("Slowest wake-ups")).unwrap().trim().to_string()
        };

        let c = session(Case::Scored);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert!(c.clear(&c.rows[0], 1, &d), "{d:?}");
        let said = line(&c);
        assert!(said.starts_with("• Slowest wake-ups (1 in 1000): 54× shorter, at least "), "{said}");
        // What is claimed holds for the interval's cautious end.
        let claimed: f64 = said.split("at least ").nth(1).unwrap().split('×').next().unwrap().parse().unwrap();
        let (lo, hi) = d.ci.unwrap();
        let cautious = d.ratio_of(lo).min(d.ratio_of(hi));
        assert!(claimed <= cautious && cautious < 27.0, "{said} vs {cautious}");
        // And it is said what "at least" is: an end of an interval, no promise.
        let explained = "not a guaranteed minimum";
        assert!(crate::simple::text(&c).contains(explained) && crate::html::report(&c).contains(explained));

        for case in [Case::Minor, Case::Uneven] {
            let c = session(case);
            let d = c.rows[0].improvement(0, 1).unwrap();
            assert!(!c.clear(&c.rows[0], 1, &d), "{case:?}: {d:?}");
            let said = line(&c);
            assert!(!said.contains("at least"), "{case:?}: {said}");
            assert!(!said.contains("54× shorter"), "{case:?} must not read as scored: {said}");
            assert!(!crate::simple::text(&c).contains(explained) && !crate::html::report(&c).contains(explained));
        }
    }

    /// `clear` is "no reason not to score", and the reasons cover exactly
    /// what the scoring rule excludes: checked against the rule written
    /// out, for every compared cell of sessions of every kind.
    #[test]
    fn unclear_reasons_agree_with_the_scoring_rule() {
        let mut comparisons = vec![
            Comparison::build(drifting_session(), 0),
            Comparison::build(vec![populations("base", 500, 10.0, true), populations("cand", 995, 5.0, true)], 0),
            Comparison::build(vec![sub_populations("base", 450, true), sub_populations("cand", 4, true)], 0),
            Comparison::build(
                vec![
                    file("base", &[100.0, 101.0, 99.0, 100.5, 99.5], 1),
                    file("same", &[100.2, 100.8, 99.4, 100.1, 99.9], 1),
                    file("slow", &[150.0, 151.0, 149.0, 150.5, 149.5], 1),
                    // Far from the baseline in every run, but all over the place.
                    file("wild", &[400.0, 900.0, 450.0, 1200.0, 700.0], 1),
                    file("short", &[150.0, 151.0], 1),
                ],
                0,
            ),
        ];
        // An uneven session, and one with a round the baseline measured as zero.
        let mut uneven = drifting_session();
        for (k, r) in uneven[1].workloads[0].runs.iter_mut().enumerate() {
            if (k / 2) % 2 == 0 {
                r.metrics.get_mut("p99").unwrap().value *= 0.5 / 0.9;
            }
        }
        comparisons.push(Comparison::build(uneven, 0));
        let mut undefined = drifting_session();
        undefined[0].workloads[0].runs[0].metrics.get_mut("p99").unwrap().value = 0.0;
        undefined[0].workloads[0].runs[1].metrics.get_mut("p99").unwrap().value = 0.0;
        comparisons.push(Comparison::build(undefined, 0));

        let mut seen = Vec::new();
        let mut scored = 0;
        for c in &comparisons {
            for r in &c.rows {
                for i in (0..c.series.len()).filter(|i| *i != c.baseline) {
                    let Some(d) = r.improvement(c.baseline, i) else { continue };
                    let rule = !r.extreme
                        && !r.unstable_pair(c.baseline, i)
                        && !r.minor_pair(c.baseline, i)
                        && d.notable(c.opts.min_effect_pct);
                    let reason = c.unclear_reason(r, i, &d);
                    assert_eq!(reason.is_none(), rule, "{} {}: {reason:?}", r.metric, c.series[i].label);
                    assert_eq!(c.clear(r, i, &d), rule);
                    assert_eq!(
                        c.held_back_by_unevenness(r, i, &d).is_some(),
                        matches!(reason, Some(UnclearReason::Uneven(_)))
                    );
                    match reason {
                        Some(reason) => seen.push(std::mem::discriminant(&reason)),
                        None => scored += 1,
                    }
                }
            }
        }
        // The sessions above reach every reason but a single-sample extreme
        // (covered by the uneven-tag test), and scored changes too.
        assert!(scored > 0);
        for reason in [
            UnclearReason::MinorPopulation,
            UnclearReason::UndefinedRounds,
            UnclearReason::TooFewRounds,
            UnclearReason::IntervalIncludesNoChange,
            UnclearReason::Uneven(PairedSpread::Relative(0.0)),
            UnclearReason::Unstable,
        ] {
            assert!(seen.contains(&std::mem::discriminant(&reason)), "no case reached {reason:?}");
        }
    }

    /// Every reason has its own words in the plain view, and they are the
    /// reason the scoring went by.
    #[test]
    fn in_short_says_why_a_change_is_not_scored() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Case {
            IncludesNoChange,
            BelowThreshold,
            Minor,
            TwoRounds,
            UndefinedRound,
        }
        let session = |case: Case| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "wake-pingpong".into();
                if case == Case::TwoRounds {
                    f.workloads[0].runs.retain(|r| r.round.is_some_and(|k| k < 2));
                }
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    let round = k / 2;
                    let longer = match (case, is_cand) {
                        (_, false) => 1.0,
                        // 34× shorter in most rounds, longer in others.
                        (Case::IncludesNoChange, true) => [1.0 / 34.0, 1.3, 1.0 / 34.0, 1.2, 1.0 / 34.0, 1.4][round],
                        (Case::BelowThreshold, true) => 1.03,
                        (_, true) => 2.0,
                    };
                    let mut v = 1434.0 * wobble * longer;
                    if case == Case::UndefinedRound && !is_cand && round == 0 {
                        v = 0.0;
                    }
                    let m = Metric::lower(v, "us");
                    r.metrics.clear();
                    r.metrics.insert("rtt_blocked_p99.9".into(), if case == Case::Minor { m.share(0.001) } else { m });
                }
            }
            Comparison::build(files, 0)
        };
        for (case, reason, words) in [
            (Case::IncludesNoChange, UnclearReason::IntervalIncludesNoChange, "the interval includes no change"),
            (Case::BelowThreshold, UnclearReason::BelowThreshold, "difference below the reporting threshold of 5%"),
            (Case::Minor, UnclearReason::MinorPopulation, "too few relevant wake-ups"),
            (Case::TwoRounds, UnclearReason::TooFewRounds, "too few paired rounds"),
            (Case::UndefinedRound, UnclearReason::UndefinedRounds, "the baseline measured zero in some rounds"),
        ] {
            let c = session(case);
            let d = c.rows[0].improvement(0, 1).unwrap();
            assert_eq!(c.unclear_reason(&c.rows[0], 1, &d), Some(reason), "{case:?}: {d:?}");
            let text = crate::simple::text(&c);
            let line = text.lines().find(|l| l.contains("Slowest wake-ups")).unwrap().trim();
            assert!(
                line.starts_with(&format!("• Slowest wake-ups (1 in 1000): no reliable difference; {words} (")),
                "{case:?}: {line}"
            );
            assert!(!line.contains("longer") && !line.contains("shorter"), "{case:?}: {line}");
        }

        // Unpaired files: too few runs, and a side too unstable to compare.
        let c = Comparison::build(
            vec![
                file("base", &[100.0, 101.0, 99.0, 100.5, 99.5], 1),
                file("short", &[150.0, 151.0], 1),
                file("wild", &[400.0, 900.0, 450.0, 1200.0, 700.0], 1),
            ],
            0,
        );
        let row = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        let reason = |i: usize| c.unclear_reason(row, i, &row.improvement(0, i).unwrap());
        assert_eq!(reason(1), Some(UnclearReason::TooFewRounds));
        assert_eq!(reason(2), Some(UnclearReason::Unstable));
    }

    /// The mix warning is repeated, in plain words, in the part of "In
    /// short" whose numbers it qualifies: there and nowhere else, from the
    /// detection the warning itself comes from.
    #[test]
    fn in_short_notes_a_different_mix_where_it_applies() {
        let note = |c: &Comparison| crate::simple::briefs(c).into_iter().map(|b| b.note).collect::<Vec<_>>();
        for share in [true, false] {
            let build = |b, s| {
                Comparison::build(vec![populations("base", b, 10.0, share), populations("cand", s, 5.0, share)], 0)
            };

            // Blocked round trips: 99% of the baseline's, 11% of the candidate's.
            let c = build(990, 110);
            assert_eq!(c.mixes.len(), 2, "{:?}", c.mixes);
            assert!(c.mixes.iter().all(|m| m.series == 1 && m.workload == "w"));
            let said = "Only 11% of round trips needed a wake-up, versus 99% for base. The wake-up delays here do \
not describe the same mix of round trips.";
            assert_eq!(note(&c), [Some(said.to_string())]);
            let text = crate::simple::text(&c);
            assert!(text.contains(&format!("    Note: {said}\n")), "{text}");
            // After the points it is about, within the part.
            assert!(text.find("Typical wake-up delay").unwrap() < text.find("Note:").unwrap());
            assert!(crate::html::report(&c).contains(&format!("<p class=note>{said}</p>")));
            // The full report's warning is still there.
            assert!(c.warnings.iter().any(|w| w.contains("different mix")));

            // The other way round, without "Only".
            let c = build(300, 900);
            assert_eq!(
                note(&c)[0].as_deref().map(|n| n.split('.').next().unwrap()),
                Some("90% of round trips needed a wake-up, versus 30% for base")
            );

            // No warning, no note: a similar mix, and a population minor on
            // one side (which is tagged, and not scored, already).
            for (b, s) in [(500, 600), (500, 995)] {
                let c = build(b, s);
                assert!(c.mixes.is_empty() && note(&c) == [None], "{b} vs {s}");
                assert!(!crate::simple::text(&c).contains("Note:"));
                assert!(!crate::html::report(&c).contains("<p class=note>"));
            }

            // Only the fast population differs by the factor (20% vs 60%
            // blocked is 3×; here 80% vs 60% blocked, 20% vs 40% fast).
            let c = build(800, 600);
            assert_eq!(c.mixes.len(), 1);
            let n = note(&c)[0].clone().unwrap();
            // The delays this part shows are those of blocked round trips,
            // so the note does not call them, or any others, incomparable.
            assert_eq!(
                n,
                "40% of round trips needed no wake-up, versus 20% for base. The mix of round trips differs, so read \
the round trips completed and the wake-up delays together."
            );
        }

        // Same-CPU and cross-CPU wake-ups are both inside the delays shown:
        // the note says that, not that some other delays are incomparable.
        // Blocked as a whole is 90% on both sides and raises nothing; of
        // 1000 round trips 400 vs 100 are on the same CPU.
        let c = Comparison::build(vec![sub_populations("base", 400, true), sub_populations("cand", 100, true)], 0);
        assert_eq!(c.mixes.iter().map(|m| m.population.as_str()).collect::<Vec<_>>(), ["rtt_blocked_same_cpu"]);
        assert_eq!(
            note(&c),
            [Some(
                "Only 10% of round trips needed a wake-up with the two tasks on one CPU, versus 40% for base. The \
wake-up delays here cover wake-ups on one CPU and across CPUs together, in different proportions for the two."
                    .to_string()
            )]
        );

        // Several populations differ: blocked round trips are named whenever
        // they are among them; otherwise the one whose share moved the most,
        // not the first in a fixed order. Shares per side, in the order
        // blocked, same CPU, cross CPU, fast.
        let mixed = |label: &str, shares: [f64; 4]| {
            let mut f = file(label, &[1.0; 5], 1);
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let names = ["rtt_blocked", "rtt_blocked_same_cpu", "rtt_blocked_cross_cpu", "rtt_fast"];
                for (pop, share) in names.into_iter().zip(shares) {
                    let m = Metric::lower(10.0 * (1.0 + k as f64 * 1e-3), "us").share(share);
                    r.metrics.insert(format!("{pop}_p50"), m);
                }
            }
            f
        };
        let named = |base: [f64; 4], cand: [f64; 4]| {
            let c = Comparison::build(vec![mixed("base", base), mixed("cand", cand)], 0);
            let populations: Vec<String> = c.mixes.iter().map(|m| m.population.clone()).collect();
            (populations.len(), note(&c)[0].clone().unwrap())
        };
        // cake under load: cross CPU 5.3% → 81% (76 points), same CPU 87% →
        // 16% (71), fast 8.6% → 1.9% (6.7). Fast used to be named.
        let (n, said) = named([0.914, 0.87, 0.053, 0.086], [0.981, 0.16, 0.81, 0.019]);
        assert_eq!(n, 3);
        assert!(
            said.starts_with("81% of round trips needed a wake-up with the two tasks on different CPUs, versus 5.3%"),
            "{said}"
        );
        // The larger move wins either way round: here fast, 5% → 60%,
        // against cross CPU, 5% → 15%.
        let (n, said) = named([0.95, 0.90, 0.05, 0.05], [0.40, 0.25, 0.15, 0.60]);
        assert_eq!(n, 4, "blocked differs too");
        assert!(
            said.starts_with("Only 40% of round trips needed a wake-up, versus 95%"),
            "blocked comes first: {said}"
        );
        let (n, said) = named([0.95, 0.90, 0.05, 0.05], [0.60, 0.50, 0.10, 0.40]);
        assert_eq!(n, 2, "cross CPU and fast");
        assert!(said.starts_with("40% of round trips needed no wake-up, versus 5.0%"), "{said}");
        // Equal moves (half of all round trips each, exact in binary): the
        // fixed order decides, cross CPU before same CPU.
        let (n, said) = named([0.875, 0.75, 0.125, 0.125], [0.875, 0.25, 0.625, 0.125]);
        assert_eq!(n, 2);
        assert!(said.starts_with("62% of round trips needed a wake-up with the two tasks on different CPUs"), "{said}");

        // Per scheduler and workload: a note in one card says nothing in another.
        let c = Comparison::build(
            vec![
                populations("base", 900, 10.0, true),
                populations("same", 880, 5.0, true),
                populations("other", 110, 5.0, true),
            ],
            0,
        );
        let notes: Vec<(usize, bool)> =
            crate::simple::briefs(&c).iter().map(|b| (b.series, b.note.is_some())).collect();
        assert_eq!(notes, [(1, false), (2, true)]);
    }

    /// `woken_per_s` is listed, not scored: a scheduler under which most
    /// round trips need no sleep wakes fewer threads, and that alone must
    /// not make it "worse". Also for files that still call it "higher is
    /// better", alone or next to new ones.
    #[test]
    fn woken_throughput_is_informational() {
        // 1000 round trips per run at the same latencies and the same rate;
        // `blocked` of them sleep, and each of those wakes two threads.
        let run = |label: &str, blocked: u64, direction: Option<Better>, workload: &str| {
            let mut f = populations(label, blocked, 10.0, true);
            f.workloads[0].name = workload.into();
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let jitter = 1.0 + k as f64 * 1e-3;
                r.metrics.insert("roundtrips_per_s".into(), Metric::higher(1.0e6 * jitter, "1/s"));
                if let Some(better) = direction {
                    let mut m = Metric::info(2.0e3 * blocked as f64 * jitter, "1/s");
                    m.better = better;
                    r.metrics.insert("woken_per_s".into(), m);
                }
            }
            f
        };
        let (old, new) = (Some(Better::Higher), Some(Better::Neither));
        for (base, cand) in [(new, new), (old, old), (old, new), (new, old)] {
            for workload in ["wake-pingpong", "wake-pingpong/loaded"] {
                // 90% of the baseline's round trips sleep, 30% of the candidate's.
                let c = Comparison::build(vec![run("base", 900, base, workload), run("cand", 300, cand, workload)], 0);
                assert!(c.warnings.iter().all(|w| !w.contains("not compared")), "{:?}", c.warnings);
                let row = c.rows.iter().find(|r| r.metric == "woken_per_s").expect("still listed");
                assert_eq!(row.better, Better::Neither);
                assert!(row.improvement(0, 1).is_none(), "no change to score");
                // Its values are in the table, three times apart.
                let (b, s) = (row.cells[0].as_ref().unwrap(), row.cells[1].as_ref().unwrap());
                assert!((b.spread.median / s.spread.median - 3.0).abs() < 1e-9);

                // The shift in the mix is reported, and nothing is worse.
                assert!(c.mixes.iter().any(|m| m.population == "rtt_fast"));
                let s = &c.summary()[0];
                assert_eq!((s.verdict(), s.better, s.worse), (Verdict::NoClearDifference, 0, 0), "{base:?} {cand:?}");
                // Round trips, the two p50 and the helper's p99 and ops.
                assert_eq!(s.compared, 5, "woken_per_s is not among them");
                assert!(s.top.iter().all(|(m, _)| m != "woken_per_s"));
                let text = crate::simple::text(&c);
                assert!(text.contains(": NO CLEAR DIFFERENCE"), "{text}");
                assert!(text.contains("Of 5 measurements, 0 clearly better and 0 clearly worse"), "{text}");
                let html = crate::html::report(&c);
                assert!(html.contains("woken_per_s") && !html.contains("woken_per_s 3.0×"), "listed, not ranked");
            }
        }

        // The reading is this workload's: elsewhere a metric of that name
        // keeps the direction its file gives it.
        let c = Comparison::build(vec![run("base", 900, old, "other"), run("cand", 300, old, "other")], 0);
        let row = c.rows.iter().find(|r| r.metric == "woken_per_s").unwrap();
        assert_eq!(row.better, Better::Higher);
        assert_eq!(c.summary()[0].verdict(), Verdict::Worse);
    }

    /// periodic-wake has its own words in the plain view: how late the
    /// tasks ran and how often their work was done in time, and what the
    /// test does. The token-passing sentence is not said of it.
    #[test]
    fn periodic_wake_is_described_in_plain_words() {
        let session = |workload: &str| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = workload.into();
                for r in f.workloads[0].runs.iter_mut() {
                    // The drift of the round, without the candidate's 10%.
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    r.metrics.clear();
                    let wake = if is_cand { 18.0 } else { 20.0 };
                    r.metrics.insert("wake_p50".into(), Metric::lower(wake * wobble, "us"));
                    r.metrics.insert("wake_p99".into(), Metric::lower(40.0 * wake * wobble, "us"));
                    r.metrics.insert("response_p50".into(), Metric::lower((100.0 + wake) * wobble, "us"));
                    r.metrics.insert("response_p99".into(), Metric::lower(60.0 * wake * wobble, "us"));
                    let missed = if is_cand { 1.0 } else { 10.0 };
                    counted(r, "deadline_miss_pct", missed * wobble);
                    r.metrics.insert("on_time_pct".into(), Metric::info(100.0 - missed * wobble, "%"));
                    r.metrics.insert("hog_work_per_s".into(), Metric::info(1e6 * wobble, "units/s"));
                }
            }
            Comparison::build(files, 0)
        };
        let c = session("periodic-wake/loaded");
        let text = crate::simple::text(&c);
        assert!(text.contains("  Tasks on a timer, busy system (every CPU kept busy): BETTER\n"), "{text}");
        assert!(text.contains("• Typical wake-up delay: 10% shorter (23.0 → 20.7 µs)"), "{text}");
        // Too few samples for 1 in 1000: the p99 stands in.
        assert!(text.contains("• Slow wake-ups (1 in 100): 10% shorter ("), "{text}");
        // The response has a line of its own: it can differ where the
        // wake-up does not.
        assert!(text.contains("• Slow responses (1 in 100), work included: 10% shorter ("), "{text}");
        // Two shares and a factor, not a percentage of a percentage.
        // The counts over all runs together, which are what is compared.
        assert!(text.contains("• Deadlines missed: 10× fewer (28000 / 240.0k → 2794 / 240.0k periods)"), "{text}");
        assert!(!text.contains("on time"), "informational, and the same fact: {text}");
        assert!(text.contains("• Background CPU work: about the same (observed, not scored)"), "{text}");
        // The typical response moves by under 2%: below the threshold.
        assert!(text.contains("• Of 5 measurements, 4 clearly better and 0 clearly worse"), "{text}");

        // Deadlines missed by neither, by the candidate only and by the
        // baseline only: none is said where there is none, and a side
        // without events is counts and no change, to none as from none.
        let zeros = |base: f64, cand: f64| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "periodic-wake".into();
                for r in f.workloads[0].runs.iter_mut() {
                    r.metrics.clear();
                    r.metrics.insert("wake_p50".into(), Metric::lower(20.0, "us"));
                    counted(r, "deadline_miss_pct", if is_cand { cand } else { base });
                }
            }
            crate::simple::text(&Comparison::build(files, 0))
        };
        assert!(zeros(0.0, 0.0).contains("• Deadlines missed: none in any run under either\n"), "{}", zeros(0.0, 0.0));
        let from_zero = zeros(0.0, 2.5);
        assert!(
            from_zero.contains("• Deadlines missed: 0 / 240.0k → 6000 / 240.0k periods; too few events to score\n"),
            "{from_zero}"
        );
        let to_zero = zeros(2.5, 0.0);
        assert!(
            to_zero.contains("• Deadlines missed: 6000 / 240.0k → 0 / 240.0k periods; too few events to score\n"),
            "{to_zero}"
        );
        assert!(!to_zero.contains("fewer") && !from_zero.contains("more"), "{to_zero}{from_zero}");

        // A few misses are not "none", whatever the median of the runs
        // is. Five runs a side: three clean, two with misses. The counts
        // say what there was, and that it is too little to compare.
        let rare = |base: [f64; 5], cand: [f64; 5]| {
            let mk = |label: &str, missed: [f64; 5]| {
                let mut f = file(label, &[1.0; 5], 1);
                f.workloads[0].name = "periodic-wake".into();
                for (r, m) in f.workloads[0].runs.iter_mut().zip(missed) {
                    r.metrics.clear();
                    r.metrics.insert("wake_p50".into(), Metric::lower(20.0, "us"));
                    counted(r, "deadline_miss_pct", m);
                }
                f
            };
            crate::simple::text(&Comparison::build(vec![mk("base", base), mk("cand", cand)], 0))
        };
        let said = rare([0.0, 0.0, 0.0, 0.02, 0.05], [0.0, 0.0, 0.0, 0.04, 0.08]);
        assert!(
            said.contains("• Deadlines missed: 14 / 100.0k → 24 / 100.0k periods; too few events to score\n"),
            "{said}"
        );
        assert!(!said.contains("none") && !said.contains("median"), "{said}");
        // Many on one side do not make up for a handful on the other.
        let said = rare([0.0, 0.0, 0.0, 0.02, 0.05], [2.0, 2.5, 3.0, 2.5, 2.5]);
        assert!(
            said.contains("• Deadlines missed: 14 / 100.0k → 2500 / 100.0k periods; too few events to score\n"),
            "{said}"
        );
        // One side clean throughout, the other with rare misses.
        let said = rare([0.0; 5], [0.0, 0.0, 0.0, 0.04, 0.08]);
        assert!(
            said.contains("• Deadlines missed: 0 / 100.0k → 24 / 100.0k periods; too few events to score\n"),
            "{said}"
        );
        assert!(text.ends_with(&format!("\n{}\n", crate::simple::WHAT_PERIODIC)), "{text}");
        assert!(!text.contains("pairs of tasks"), "{text}");
        assert!(crate::simple::text(&session("periodic-wake")).contains("  Tasks on a timer, idle system: BETTER"));
        let html = crate::html::report(&c);
        assert!(html.contains(&crate::esc(crate::simple::WHAT_PERIODIC)) && !html.contains("pairs of tasks wake"));

        // Both workloads in one report: both sentences, joined.
        let mut files = drifting_session();
        for f in &mut files {
            let mut second = f.workloads[0].clone();
            f.workloads[0].name = "wake-pingpong".into();
            second.name = "periodic-wake".into();
            f.workloads.push(second);
        }
        let what = crate::simple::what(&Comparison::build(files, 0));
        assert!(what.starts_with("The test makes pairs of tasks wake each other up"), "{what}");
        assert!(what.contains(" It also wakes a few tasks on a timer"), "{what}");
        // A workload without words gets no sentence about another one.
        assert_eq!(crate::simple::what(&Comparison::build(drifting_session(), 0)), "");
    }

    /// wake-burst in the plain view: one task, the whole group, the work
    /// done, and the wake-ups a task sat out because it was still busy.
    #[test]
    fn wake_burst_is_described_in_plain_words() {
        let session = |workload: &str, overrun: (f64, f64)| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = workload.into();
                for r in f.workloads[0].runs.iter_mut() {
                    // The drift of the round, without the candidate's 10%.
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    r.metrics.clear();
                    let k = if is_cand { 0.5 } else { 1.0 } * wobble;
                    r.metrics.insert("start_p50".into(), Metric::lower(40.0 * k, "us"));
                    r.metrics.insert("start_p99.9".into(), Metric::lower(900.0 * k, "us"));
                    r.metrics.insert("all_running_p50".into(), Metric::lower(120.0 * k, "us"));
                    r.metrics.insert("all_done_p50".into(), Metric::lower(200.0 + 120.0 * k, "us"));
                    r.metrics.insert("all_done_p99".into(), Metric::lower(200.0 + 800.0 * k, "us"));
                    let lost = if is_cand { overrun.1 } else { overrun.0 };
                    counted(r, "overrun_pct", lost * wobble);
                    r.metrics.insert("bursts_per_s".into(), Metric::info(if is_cand { 498.0 } else { 393.0 }, "1/s"));
                    let overrun = (lost * 10.0) as u64;
                    for (name, n) in [
                        ("bursts_planned", 252),
                        ("bursts", 250),
                        ("bursts_complete", 250 - overrun),
                        ("wakeups_due", 1000),
                        ("wakeups_ran", 999 - overrun),
                        ("wakeups_not_asleep", 1),
                        ("wakeups_overrun", overrun),
                    ] {
                        r.counters.insert(name.into(), n);
                    }
                }
            }
            Comparison::build(files, 0)
        };
        let c = session("wake-burst/double", (4.0, 1.0));
        let text = crate::simple::text(&c);
        assert!(text.contains("  A group of tasks woken at once, twice as many as CPUs: BETTER\n"), "{text}");
        assert!(text.contains("• Typical wake-up delay: 2.0× shorter (45.9 → 23.0 µs)"), "{text}");
        assert!(text.contains("• Slowest wake-ups (1 in 1000): 2.0× shorter ("), "{text}");
        assert!(text.contains("• Until the whole group runs, typically: 2.0× shorter ("), "{text}");
        // The work is in it, so it shrinks by less than the wait does.
        assert!(text.contains("• Until the whole group is done, typically: 20% shorter (338 → 269 µs)"), "{text}");
        assert!(text.contains("• Slow bursts (1 in 100), until the whole group is done: 41% shorter ("), "{text}");
        // A share of the wake-ups, not of periods, and no percentage of a
        // percentage.
        assert!(
            text.contains(
                "• Wake-ups lost because the task was still busy: 4.0× fewer (480 / 12.0k → 120 / 12.0k wake-ups)"
            ),
            "{text}"
        );
        // How much work each side was asked for, said and not scored.
        assert!(
            text.contains(
                "• Bursts released: 498 a second, 393 under base (observed, not scored; with fewer bursts a scheduler \
                 was asked for less work)\n"
            ),
            "{text}"
        );
        assert!(text.ends_with(&format!("\n{}\n", crate::simple::WHAT_BURST)), "{text}");
        for (name, title) in [("wake-burst/half", "half as many as CPUs"), ("wake-burst/full", "as many as CPUs")] {
            let text = crate::simple::text(&session(name, (0.0, 0.0)));
            assert!(text.contains(&format!("  A group of tasks woken at once, {title}: BETTER\n")), "{text}");
            // Zero is the usual case, and is said, not left out.
            assert!(
                text.contains("• Wake-ups lost because the task was still busy: none in any run under either\n"),
                "{text}"
            );
        }
        let from_zero = crate::simple::text(&session("wake-burst/full", (0.0, 2.0)));
        let line = from_zero.lines().find(|l| l.contains("Wake-ups lost")).unwrap();
        assert!(line.contains("still busy: 0 / ") && line.ends_with(" wake-ups; too few events to score"), "{line}");
        // The report explains the populations and counters it shows.
        let html = crate::html::report(&c);
        assert!(html.contains(&crate::esc(crate::simple::WHAT_BURST)), "{html}");
        assert!(html.contains("<code>wakeups_overrun</code>") && html.contains("<code>wake_call</code>"));
        assert!(html.contains("<code>idle_latency_us</code>"));
        {
            // The bound on idle latency is a parameter of the workload. Five
            // runs with a bound of 1 µs are shown with 1, once, and not with
            // the 5 a counter summed over the runs would give.
            let mut files = drifting_session();
            for f in &mut files {
                f.workloads[0].name = "wake-burst/half".into();
                f.workloads[0].params = serde_json::json!({ "tasks": 8, "idle": "shallow", "idle_latency_us": 1 });
                assert!(f.workloads[0].runs.len() >= 5);
                for r in f.workloads[0].runs.iter_mut() {
                    r.counters.insert("bursts".into(), 2490);
                }
            }
            let shallow = Comparison::build(files.clone(), 0);
            assert_eq!(shallow.diagnostics[0].params, "idle=shallow, idle_latency_us=1, tasks=8");
            let html = crate::html::report(&shallow);
            assert!(html.contains("<p class=sub>Parameters: idle=shallow, idle_latency_us=1, tasks=8</p>"), "{html}");
            assert!(!html.contains("<td>idle_latency_us</td>"), "not among the counters: {html}");
            // Another bound is another experiment: said, and not compared.
            files[1].workloads[0].params = serde_json::json!({ "tasks": 8, "idle": "shallow", "idle_latency_us": 2 });
            let mixed = Comparison::build(files, 0);
            let html = crate::html::report(&mixed);
            assert!(html.contains("<p class=sub>Parameters differ:"), "{html}");
            assert!(
                html.contains("idle_latency_us=1, tasks=8;") && html.contains("idle_latency_us=2, tasks=8;"),
                "{html}"
            );
            assert!(mixed.warnings.iter().any(|w| w.contains("not compared")), "{:?}", mixed.warnings);
        }
        let share = |label: &str, base: &str, cand: &str| {
            format!("<td><strong>{label}</strong></td><td><strong>{base}</strong></td><td><strong>{cand}</strong></td>")
        };
        assert!(html.contains(&share("wake-ups lost because the task was still busy", "4.0%", "1.0%")), "{html}");
        assert!(html.contains(&share("wake-ups measured, of those due", "95.9%", "98.9%")), "{html}");
        // Not asleep yet is its own row, not part of the overruns.
        assert!(
            html.contains(&share("wake-ups that found the task done but not asleep yet", "0.1%", "0.1%")),
            "{html}"
        );
        assert!(
            html.contains(&share("bursts released, of those a punctual waker would release", "99.2%", "99.2%")),
            "{html}"
        );
        assert!(html.contains(&share("bursts in which every task took part", "84.0%", "96.0%")), "{html}");

        // All three workloads in one report: one test with three parts.
        let mut files = drifting_session();
        for f in &mut files {
            let (mut second, mut third) = (f.workloads[0].clone(), f.workloads[0].clone());
            f.workloads[0].name = "wake-pingpong".into();
            second.name = "periodic-wake".into();
            third.name = "wake-burst/full".into();
            f.workloads.extend([second, third]);
        }
        let all = Comparison::build(files, 0);
        let what = crate::simple::what(&all);
        assert!(what.starts_with("The test makes pairs of tasks wake each other up"), "{what}");
        assert!(what.contains(" It also wakes a few tasks on a timer"), "{what}");
        assert!(what.contains(" It also wakes a whole group of tasks at once"), "{what}");
        assert_eq!(what.matches("The test ").count(), 1, "{what}");
        // Alone, it is introduced as the test.
        assert!(crate::simple::what(&c).starts_with("The test wakes a whole group"));
    }

    /// The share of slow wake-ups in wake-pingpong: what a percentile on
    /// a step cannot say. Zero is the usual value and is said.
    #[test]
    fn slow_wakeups_are_a_share_in_plain_words() {
        let session = |slow: (f64, f64)| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "wake-pingpong/loaded".into();
                for r in f.workloads[0].runs.iter_mut() {
                    counted(r, "rtt_blocked_slow_pct", if is_cand { slow.1 } else { slow.0 });
                }
            }
            crate::simple::text(&Comparison::build(files, 0))
        };
        let text = session((0.12, 0.03));
        assert!(
            text.contains(
                "• Round trips of a millisecond or more: 4.0× fewer (28800 / 24.00M → 7200 / 24.00M blocked round \
                 trips)"
            ),
            "{text}"
        );
        let none = session((0.0, 0.0));
        assert!(none.contains("• Round trips of a millisecond or more: none in any run under either\n"), "{none}");
        let from_zero = session((0.0, 0.5));
        assert!(
            from_zero.contains(
                "• Round trips of a millisecond or more: 0 / 24.00M → 120000 / 24.00M blocked round trips; too few \
                 events to score\n"
            ),
            "{from_zero}"
        );
        // A result without the counters, as older versions wrote them:
        // not compared, and said so; the runs' own shares are not taken
        // in their place.
        let mut files = drifting_session();
        for (f, pct) in files.iter_mut().zip([0.12, 0.03]) {
            f.workloads[0].name = "wake-pingpong/loaded".into();
            for r in f.workloads[0].runs.iter_mut() {
                r.metrics.insert("rtt_blocked_slow_pct".into(), Metric::lower(pct, "%"));
            }
        }
        let c = Comparison::build(files, 0);
        let row = c.rows.iter().find(|r| r.metric == "rtt_blocked_slow_pct").unwrap();
        assert_eq!(row.share(0, 1), Some(Share::NoCounters));
        assert_eq!(row.improvement(0, 1), None);
        let old = crate::simple::text(&c);
        assert!(
            old.contains("• Round trips of a millisecond or more: no event counters in this result; not scored\n"),
            "{old}"
        );
        assert!(crate::html::text_table(&c).contains("(no event counters in this result; not scored)"));
    }

    /// A share of counted events is compared from its counters, and only
    /// where both sides have twenty events. The case that showed the
    /// need: 9 slow round trips in 33.6 million against none counted as
    /// "100% fewer, clearly better" and turned a workload that was worse
    /// in everything else into "mixed". Then simulation: none against
    /// many is found in most comparisons of two equal sides when the
    /// events of the side with none come rarely and many at a time.
    #[test]
    fn a_share_of_counted_events_needs_events_on_both_sides() {
        // Slow round trips of the baseline and of the candidate, summed
        // over the 12 runs of a side, each of 2.8 million blocked round
        // trips. The other metric of the workload is clearly worse.
        const BLOCKED: u64 = 2_800_000;
        let session = |slow: (u64, u64)| {
            let mut files = drifting_session();
            for (f, total) in files.iter_mut().zip([slow.0, slow.1]) {
                f.workloads[0].name = "wake-pingpong".into();
                let worse = if f.label == "cand" { 1.5 } else { 1.0 };
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    r.metrics.get_mut("p99").unwrap().value *= worse;
                    // Spread over the runs as evenly as whole events go.
                    let events = total / 12 + u64::from((k as u64) < total % 12);
                    r.counters.insert("roundtrips_slow".into(), events);
                    r.counters.insert("roundtrips_blocked".into(), BLOCKED);
                    let pct = events as f64 / BLOCKED as f64 * 100.0;
                    r.metrics.insert("rtt_blocked_slow_pct".into(), Metric::lower(pct, "%"));
                }
            }
            files
        };
        fn slow(c: &Comparison) -> &MetricRow {
            c.rows.iter().find(|r| r.metric == "rtt_blocked_slow_pct").unwrap()
        }
        let scored = |c: &Comparison| matches!(slow(c).share(0, 1), Some(Share::Scored(_)));
        let verdict = |c: &Comparison| crate::simple::briefs(c)[0].verdict;
        let build = |slow| Comparison::build(session(slow), 0);
        let line_of = |text: &str| text.lines().find(|l| l.contains("rtt_blocked_slow_pct")).unwrap().to_string();
        // The row of the table of all metrics, not the summary's.
        let html_line = |c: &Comparison| {
            let html = crate::html::report(c).replace("</tr>", "</tr>\n");
            html.lines().find(|l| l.contains(" · rtt_blocked_slow_pct ")).unwrap().to_string()
        };
        let terminal_line = |c: &Comparison| line_of(&crate::html::text_table(c));

        // The report that showed it. Not compared, and the workload is
        // worse and not mixed: in the briefs, which the plain view and
        // the card are made of, and in the summary's counts.
        let c = build((9, 0));
        let row = slow(&c);
        assert_eq!(row.share(0, 1), Some(Share::TooFewEvents));
        assert!(row.too_few_events(0, 1) && row.improvement(0, 1).is_none() && !row.chartable(0, 1));
        assert_eq!(verdict(&c), Verdict::Worse);
        let summary = &c.summary()[0];
        assert_eq!((summary.compared, summary.better, summary.worse, summary.verdict()), (1, 0, 1, Verdict::Worse));
        let card = crate::card::svg(&c).unwrap();
        // In its cell, and twice in the legend, as the sample cell and
        // as its meaning; "mixed" only as the legend's sample cell.
        assert_eq!((card.matches(">worse<").count(), card.matches(">mixed<").count()), (3, 1));
        // With the size of what is worse: p99, 35% above the baseline's.
        assert_eq!(summary.largest.map(|d| d.map(|d| d.median.round())), [None, Some(-35.0)]);
        assert!(card.contains(">35%</text>"), "{card}");
        let text = crate::simple::text(&c);
        assert!(
            text.contains(
                "• Round trips of a millisecond or more: 9 / 33.60M → 0 / 33.60M blocked round trips; too few events \
                 to score\n"
            ),
            "{text}"
        );
        assert_eq!(row.events(0, 1), Some([(9, 12 * BLOCKED), (0, 12 * BLOCKED)]));
        // The table and the terminal give the counts where a change
        // would stand, in the same words, and no percentage or interval.
        let counts = "9 / 33.60M → 0 / 33.60M";
        assert_eq!(row.event_counts(0, 1).as_deref(), Some(counts));
        assert_eq!(c.share_note(row, 1), Some(format!("{counts}; too few events to score")));
        assert_eq!(c.share_note(row, 0), None);
        let (html, terminal) = (html_line(&c), terminal_line(&c));
        assert!(html.contains(&format!(">{counts}; too few events to score</span>")), "{html}");
        assert!(terminal.contains(&format!("({counts}; too few events to score)")), "{terminal}");
        for line in [&html, &terminal] {
            assert!(!["+100", "-100", "inf", "NaN", " … ", "[+", "[-"].iter().any(|n| line.contains(n)), "{line}");
        }
        // A cell shows the share over all its runs, and what it is made
        // of in place of a spread of the runs.
        assert!(html.contains(">9 / 33.60M</span>") && html.contains(">0 / 33.60M</span>"), "{html}");
        assert!(!html.contains('±') && !html.contains("unstable"), "{html}");
        // The other metric is untouched by all this: its change and its
        // interval are shown as they were, and it has no counts.
        let table = crate::html::text_table(&c);
        let p99 = table.lines().find(|l| l.contains("·p99 ")).unwrap();
        assert!(p99.contains("%*)") && !p99.contains("too few events"), "{p99}");
        assert_eq!(crate::html::report(&c).matches("too few events to score</span>").count(), 1);
        let other = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        assert_eq!((other.share(0, 1), other.event_counts(0, 1), other.events(0, 1)), (None, None, None));
        assert_eq!(c.share_note(other, 1), None);
        assert!(!other.too_few_events(0, 1) && other.cells[0].as_ref().unwrap().pooled.is_none());
        assert_eq!(c.unclear_reason(other, 1, &other.improvement(0, 1).unwrap()), None);

        // The line is drawn at twenty on both sides. None against many
        // is what an improvement looks like, and is what two equal sides
        // look like where one has its events rarely and many at a time:
        // it is counts, and no change.
        for (events, compared) in [
            ((19, 19), false),
            ((19, 20), false),
            ((20, 19), false),
            ((20, 20), true),
            ((20, 0), false),
            ((0, 20), false),
            ((0, 0), false),
            ((32_475, 0), false),
            ((32_475, 1), false),
            ((1, 32_475), false),
            ((32_475, 19), false),
            ((32_475, 20), true),
            ((20, 32_475), true),
        ] {
            let c = build(events);
            assert_eq!(scored(&c), compared, "{events:?}");
            assert_eq!(slow(&c).improvement(0, 1).is_some(), compared, "{events:?}");
            assert_eq!(c.share_note(slow(&c), 1).is_none(), compared, "{events:?}");
            // Not compared: the other metric alone decides.
            if !compared {
                assert_eq!(verdict(&c), Verdict::Worse, "{events:?}");
                assert_eq!(slow(&c).share(0, 1), Some(Share::TooFewEvents), "{events:?}");
            }
        }
        let c = build((32_475, 0));
        let (html, terminal) = (html_line(&c), terminal_line(&c));
        let note = "32475 / 33.60M → 0 / 33.60M; too few events to score";
        assert!(html.contains(note) && terminal.contains(note), "{html}\n{terminal}");
        assert!(!html.contains("+100") && !terminal.contains("+100"), "{html}\n{terminal}");
        assert!(crate::simple::text(&c).contains("32475 / 33.60M → 0 / 33.60M blocked round trips; too few events"));

        // Enough on both sides: compared, from the sums. Ten times
        // fewer in every run is clear, and the workload is mixed.
        let c = build((240, 24));
        let row = slow(&c);
        let d = row.improvement(0, 1).unwrap();
        assert!((d.median - 90.0).abs() < 1e-9 && (d.ratio - 10.0).abs() < 1e-9, "{d:?}");
        // The rounds choose the runs; the change is not one of rounds
        // paired off, and is not said to be.
        assert_eq!((d.paired_rounds, d.undefined_rounds, d.paired_spread), (None, 0, None));
        let (lo, hi) = d.ci.unwrap();
        assert!(60.0 < lo && lo < 90.0 && 90.0 < hi && hi < 100.0, "{d:?}");
        assert_eq!(c.unclear_reason(row, 1, &d), None);
        assert_eq!(verdict(&c), Verdict::Mixed);
        let (html, terminal) = (html_line(&c), terminal_line(&c));
        assert!(html.contains("10× better") && html.contains(">240 / 33.60M</span>"), "{html}");
        assert!(terminal.contains("(10× better*)") && !terminal.contains("too few"), "{terminal}");
        let text = crate::simple::text(&c);
        assert!(text.contains("• Round trips of a millisecond or more: 10× fewer ("), "{text}");
        // A round is one observation, whatever its number of runs: this
        // session has six rounds of two runs, and the interval is that
        // of six sums, not of twelve runs.
        let rounds = |events: u64| -> Vec<EventRun> {
            (0..6).map(|r| EventRun { round: Some(r), events, qualifying: events, of: 2 * BLOCKED }).collect()
        };
        let six = crate::shares::change(&rounds(40), &rounds(4)).unwrap().interval.unwrap();
        assert!(
            (lo - (1.0 - six.1) * 100.0).abs() < 1e-9 && (hi - (1.0 - six.0) * 100.0).abs() < 1e-9,
            "{six:?} {d:?}"
        );
        let twelve: Vec<EventRun> = rounds(20).into_iter().chain(rounds(20)).collect();
        let cand: Vec<EventRun> = rounds(2).into_iter().chain(rounds(2)).collect();
        let runs = crate::shares::change(&twelve, &cand).unwrap().interval.unwrap();
        assert!(runs != six, "{runs:?}");
        // Two rounds of two runs are four runs and two observations:
        // a change, and no interval, so nothing clear. Three rounds
        // have one.
        let first = |n: u32| {
            let mut files = session((2400, 240));
            for f in files.iter_mut() {
                f.workloads[0].runs.retain(|r| r.round.is_some_and(|round| round < n));
            }
            Comparison::build(files, 0)
        };
        let c2 = first(2);
        assert_eq!(c2.rows[0].cells[0].as_ref().unwrap().values.len(), 4);
        let d2 = slow(&c2).improvement(0, 1).unwrap();
        assert!((d2.ratio - 10.0).abs() < 1e-9 && d2.ci.is_none(), "{d2:?}");
        assert_eq!(c2.unclear_reason(slow(&c2), 1, &d2), Some(UnclearReason::TooFewRounds));
        assert!(slow(&first(3)).improvement(0, 1).unwrap().ci.is_some());
        // The other way round it is clearly worse.
        let c = build((24, 240));
        let d = slow(&c).improvement(0, 1).unwrap();
        assert!((d.median + 900.0).abs() < 1e-6 && d.ci.unwrap().1 < 0.0, "{d:?}");
        assert_eq!(c.unclear_reason(slow(&c), 1, &d), None);
        // The same on both sides: compared, and not settled. That is
        // "no clear difference", which too few events are not.
        let c = build((240, 240));
        let d = slow(&c).improvement(0, 1).unwrap();
        assert_eq!(d.median, 0.0);
        assert_eq!(c.unclear_reason(slow(&c), 1, &d), Some(UnclearReason::IntervalIncludesNoChange));
        assert_eq!(c.share_note(slow(&c), 1), None);

        // None on either side is none, said as such.
        let c = build((0, 0));
        let text = crate::simple::text(&c);
        assert!(text.contains("• Round trips of a millisecond or more: none in any run under either\n"), "{text}");
        // The tables give the counts for it, and for none against a few.
        for (events, counts) in [((0, 0), "0 / 33.60M → 0 / 33.60M"), ((0, 9), "0 / 33.60M → 9 / 33.60M")] {
            let c = build(events);
            assert_eq!(c.share_note(slow(&c), 1), Some(format!("{counts}; too few events to score")), "{events:?}");
            assert!(html_line(&c).contains(&format!(">{counts}; too few events to score</span>")));
            assert!(terminal_line(&c).contains(&format!("({counts}; too few events to score)")));
            assert_eq!(crate::html::report(&c).matches("too few events to score</span>").count(), 1);
        }
        // Where the parameters differ nothing is compared, events
        // included, and that is what is said.
        let mut files = session((240, 24));
        files[1].workloads[0].params = serde_json::json!({ "pairs": 3 });
        let c = Comparison::build(files, 0);
        assert_eq!((slow(&c).share(0, 1), c.share_note(slow(&c), 1)), (None, None));
        assert!(html_line(&c).contains("params differ") && !html_line(&c).contains("too few events"));
        assert!(terminal_line(&c).contains("(params differ)") && !terminal_line(&c).contains("too few events"));

        // The events are those of the rounds both sides have: 24 a
        // side, 2 a run. With a round missing on one side 20 are left
        // on both, with two rounds missing 16.
        let lacking = |rounds: u32| {
            let mut files = session((24, 24));
            files[1].workloads[0].runs.retain(|r| r.round.is_some_and(|round| round >= rounds));
            Comparison::build(files, 0)
        };
        let events = |c: &Comparison| slow(c).events(0, 1).unwrap().map(|(events, _)| events);
        assert_eq!((events(&lacking(0)), events(&lacking(1)), events(&lacking(2))), ([24, 24], [20, 20], [16, 16]));
        assert!(scored(&lacking(1)) && !scored(&lacking(2)));
        // A run can have the counters and not the metric, which is only
        // given from 10000 blocked round trips on. Its events are behind
        // nothing that is compared and are not counted: 20 in all, one
        // of them in a run that now has 30 and no metric, leave 19, and
        // the 30 do not carry the share over the line.
        let mut files = session((20, 20));
        let short = files[0].workloads[0].runs.last_mut().unwrap();
        assert_eq!(short.counters["roundtrips_slow"], 1);
        short.metrics.remove("rtt_blocked_slow_pct");
        short.counters.insert("roundtrips_slow".into(), 30);
        short.counters.insert("roundtrips_blocked".into(), 5_000);
        let c = Comparison::build(files, 0);
        // Its round has the other run of the baseline left, so all six
        // rounds are shared and the candidate's runs all count.
        assert_eq!(slow(&c).events(0, 1), Some([(19, 11 * BLOCKED), (20, 12 * BLOCKED)]));
        assert_eq!(slow(&c).share(0, 1), Some(Share::TooFewEvents));
        // One session whose two sides share no round: no paired
        // comparison, so no counts that would pass for one, and nothing
        // is compared.
        let mut files = session((240, 24));
        files[1].workloads[0].runs.iter_mut().for_each(|r| r.round = r.round.map(|round| round + 100));
        let c = Comparison::build(files, 0);
        assert_eq!((slow(&c).events(0, 1), slow(&c).share(0, 1)), (None, Some(Share::NoSharedRounds)));
        assert!(!slow(&c).too_few_events(0, 1) && slow(&c).improvement(0, 1).is_none());
        assert_eq!(c.share_note(slow(&c), 1).as_deref(), Some("no rounds in common; not scored"));
        assert!(crate::simple::text(&c).contains("a millisecond or more: no rounds in common; not scored\n"));
        // Results that are not one session: every valid run of a side.
        let mut files = session((240, 24));
        files[1].design = None;
        files[1].workloads[0].runs.truncate(4);
        let c = Comparison::build(files, 0);
        assert_eq!(events(&c), [240, 8]);
        assert_eq!(slow(&c).share(0, 1), Some(Share::TooFewEvents));
        // Two runs a side have a change and no interval, as two rounds
        // have anywhere else, and that is not clear.
        let mut files = session((2400, 240));
        for f in files.iter_mut() {
            f.design = None;
            f.workloads[0].runs.truncate(2);
        }
        let c = Comparison::build(files, 0);
        let d = slow(&c).improvement(0, 1).unwrap();
        assert_eq!((d.ci, d.paired_rounds), (None, None));
        assert_eq!(c.unclear_reason(slow(&c), 1, &d), Some(UnclearReason::TooFewRounds));

        // A result without the counters, as older versions wrote them,
        // is not compared, and neither is one where a single run lacks
        // them: the runs' own shares are not taken in their place.
        for missing in [12, 1] {
            let mut files = session((240, 24));
            files[1].workloads[0].runs.iter_mut().take(missing).for_each(|r| r.counters.clear());
            let c = Comparison::build(files, 0);
            assert_eq!(slow(&c).share(0, 1), Some(Share::NoCounters), "{missing}");
            assert_eq!(slow(&c).improvement(0, 1), None, "{missing}");
            assert_eq!(c.share_note(slow(&c), 1).as_deref(), Some("no event counters in this result; not scored"));
            assert_eq!(verdict(&c), Verdict::Worse, "{missing}");
            // The cell falls back to what it has, the median of the
            // runs, and says nothing of counts.
            let cell = slow(&c).cells[1].as_ref().unwrap();
            assert_eq!((cell.pooled, cell.counted()), (None, None));
            assert!(html_line(&c).contains("no event counters in this result; not scored"));
        }
    }

    /// Missed deadlines come in groups, a late period and those skipped
    /// for it. The share is of all; what admits it to a comparison is
    /// twenty late ones a side. And how much a share's runs differ is in
    /// its interval, so a side whose runs differ a lot is not "unstable".
    #[test]
    fn missed_deadlines_qualify_by_the_late_ones() {
        let side = |label: &str, late: [u64; 5]| {
            let mut f = file(label, &[1.0; 5], 1);
            f.workloads[0].name = "periodic-wake/loaded".into();
            for (r, late) in f.workloads[0].runs.iter_mut().zip(late) {
                r.metrics.clear();
                r.counters.insert("periods_late".into(), late);
                r.counters.insert("periods_skipped".into(), late);
                r.counters.insert("periods_due".into(), 20_000);
                r.metrics.insert("deadline_miss_pct".into(), Metric::lower(2.0 * late as f64 / 200.0, "%"));
            }
            f
        };
        let build = |base, cand| Comparison::build(vec![side("base", base), side("cand", cand)], 0);
        // 19 late and 19 skipped are 38 events and 19 times it went wrong.
        let c = build([300; 5], [4, 4, 4, 4, 3]);
        let row = &c.rows[0];
        assert_eq!(row.events(0, 1), Some([(3000, 100_000), (38, 100_000)]));
        assert_eq!(row.share(0, 1), Some(Share::TooFewEvents));
        assert_eq!(c.share_note(row, 1).as_deref(), Some("3000 / 100.0k → 38 / 100.0k; too few events to score"));
        // Twenty are enough, and the change is that of all events.
        let c = build([300; 5], [4; 5]);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert!((d.ratio - 75.0).abs() < 1e-9 && d.ci.is_some_and(|(lo, _)| lo > 0.0), "{d:?}");
        assert_eq!(c.rows[0].cells[0].as_ref().unwrap().pooled, Some(3.0));
        // Runs that differ a lot: the cell is not marked, the pair is
        // not unstable, and the interval is the wider for it.
        let even = d.ci.unwrap();
        let c = build([360, 240, 360, 240, 300], [4; 5]);
        let row = &c.rows[0];
        assert!(row.cells[0].as_ref().unwrap().unstable && !row.unstable_pair(0, 1));
        let d = row.improvement(0, 1).unwrap();
        let uneven = d.ci.unwrap();
        assert!(uneven.0 < even.0 && c.unclear_reason(row, 1, &d).is_none(), "{uneven:?} {even:?}");
        assert!(!crate::html::report(&c).contains(">unstable<"));
        // Wake-ups lost to overrun are counted the same way.
        assert_eq!(
            event_counters("overrun_pct"),
            Some(EventCounters { events: &["wakeups_overrun"], qualifying: "wakeups_overrun", of: "wakeups_due" })
        );
        assert_eq!(event_counters("deadline_miss_pct").unwrap().qualifying, "periods_late");
        assert_eq!(event_counters("on_time_pct"), None);
    }

    /// A burst with more tasks than CPUs has no median of the start
    /// delay, and the plain view shows its two quartiles as quartiles.
    /// Files from before the change still have their median line.
    #[test]
    fn a_crowded_burst_is_described_by_its_quartiles() {
        let session = |quartiles: bool| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "wake-burst/double".into();
                for r in f.workloads[0].runs.iter_mut() {
                    r.metrics.clear();
                    let k = if is_cand { 0.5 } else { 1.0 };
                    if quartiles {
                        r.metrics.insert("start_p25".into(), Metric::lower(30.0 * k, "us"));
                        r.metrics.insert("start_p75".into(), Metric::lower(230.0 * k, "us"));
                    } else {
                        r.metrics.insert("start_p50".into(), Metric::lower(130.0 * k, "us"));
                    }
                }
            }
            crate::simple::text(&Comparison::build(files, 0))
        };
        let text = session(true);
        assert!(text.contains("• Wake-up delay, lower quartile: 2.0× shorter (30.0 → 15.0 µs)"), "{text}");
        assert!(text.contains("• Wake-up delay, upper quartile: 2.0× shorter (230 → 115 µs)"), "{text}");
        assert!(!text.contains("Typical wake-up delay"), "{text}");
        let old = session(false);
        assert!(old.contains("• Typical wake-up delay: 2.0× shorter (130 → 65.0 µs)"), "{old}");
        assert!(!old.contains("quartile"), "{old}");
    }

    /// A session of `schedulers` candidates against "base" on `workloads`
    /// workloads, every metric of `metrics` (name, the candidate's value
    /// as a multiple of the baseline's) lower-is-better and the same in
    /// every round but for the drift both sides share.
    fn card_session(schedulers: usize, workloads: usize, metrics: &[(&str, f64)]) -> Vec<ResultFile> {
        let pair = drifting_session();
        let mut files = vec![pair[0].clone()];
        files.extend((0..schedulers).map(|k| {
            let mut f = pair[1].clone();
            f.label = format!("scx_{k}");
            f
        }));
        for (k, f) in files.iter_mut().enumerate() {
            for r in f.workloads[0].runs.iter_mut() {
                let drifted = r.metrics.remove("p99").unwrap().value / if k == 0 { 1.0 } else { 0.9 };
                for (name, factor) in metrics {
                    let factor = if k == 0 { 1.0 } else { *factor };
                    r.metrics.insert((*name).into(), Metric::lower(drifted * factor, "us"));
                }
            }
            let first = f.workloads[0].clone();
            f.workloads = (0..workloads)
                .map(|w| {
                    let mut one = first.clone();
                    one.name = format!("load-{w}/case");
                    one
                })
                .collect();
        }
        files
    }

    /// The size next to a mark is the largest clear change that way of
    /// all of them, not of the three the summary lists: those can all
    /// point one way, and a mixed cell would lose the other.
    #[test]
    fn the_largest_change_each_way_is_not_limited_to_the_listed_three() {
        let files = card_session(1, 1, &[("a", 8.0), ("b", 6.0), ("c", 4.0), ("d", 1.0 / 3.0)]);
        let c = Comparison::build(files, 0);
        let s = &c.summary()[0];
        assert_eq!((s.better, s.worse, s.verdict()), (1, 3, Verdict::Mixed));
        let top: Vec<(&str, String)> = s.top.iter().map(|(m, d)| (m.as_str(), crate::svg::fmt_delta(d))).collect();
        assert_eq!(
            top,
            [("a", "8.0× worse".to_string()), ("b", "6.0× worse".to_string()), ("c", "4.0× worse".to_string())]
        );
        let [better, worse] = s.largest.map(|d| d.map(|d| crate::svg::fmt_delta(&d)));
        assert_eq!((better.as_deref(), worse.as_deref()), (Some("3.0× better"), Some("8.0× worse")));
        // And the card has both, side by side in a card this small.
        let card = crate::card::svg(&c).unwrap();
        assert!(card.contains(">3.0×</text>") && card.contains(">8.0×</text>"), "{card}");
        // One direction only: the other has none, and is not made up.
        let c = Comparison::build(card_session(1, 1, &[("a", 8.0), ("b", 6.0)]), 0);
        assert_eq!(c.summary()[0].largest.map(|d| d.is_some()), [false, true]);
    }

    /// The card at its limits, as markup: every size inside its own
    /// cell by the widths the card reckons with, and the rule on which
    /// sizes are drawn the same for every cell of a card.
    #[test]
    fn a_full_card_keeps_its_sizes_inside_their_cells() {
        // (x, y, text) of every size on a card, and its cells' boxes.
        let sizes = |card: &str| -> Vec<(f64, f64, String)> {
            card.split("<text ")
                .skip(1)
                .filter(|t| t.contains(r#"font-size="13""#))
                .map(|t| {
                    let attr = |name: &str| -> f64 {
                        let from = t.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
                        t[from..from + t[from..].find('"').unwrap()].parse().unwrap()
                    };
                    let body = &t[t.find('>').unwrap() + 1..t.find("</text>").unwrap()];
                    (attr("x"), attr("y"), body.to_string())
                })
                .collect()
        };
        let boxes = |card: &str| -> Vec<[f64; 4]> {
            card.split("<rect ")
                .skip(2)
                .map(|t| {
                    // Its own attributes come first, the first of them
                    // at the very start.
                    let t = format!(" {t}");
                    let attr = |name: &str| -> f64 {
                        let from = t.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
                        t[from..from + t[from..].find('"').unwrap()].parse().unwrap()
                    };
                    [attr("x"), attr("y"), attr("width"), attr("height")]
                })
                .collect()
        };
        // Widths as the card estimates them, for the widest font.
        let width = |s: &str| -> f64 {
            s.chars()
                .map(|c| match c {
                    '.' => 4.2,
                    '%' => 12.4,
                    '×' | '&' => 10.9,
                    _ => 8.3,
                })
                .sum()
        };
        let inside = |card: &str| {
            let boxes = boxes(card);
            for (x, y, s) in sizes(card) {
                let s = s.replace("&gt;", "&");
                let (right, top) = (x + width(&s), y - 10.0);
                assert!(
                    boxes
                        .iter()
                        .any(|[bx, by, w, h]| *bx <= x - 17.0 && right <= bx + w && *by <= top && y + 3.0 <= by + h),
                    "{s} at {x}, {y}"
                );
            }
        };
        let mixed = [("fast", 1.0 / 63.2), ("slow", 2.9)];

        // Twelve workloads, five schedulers, every cell mixed: the two
        // sizes one above the other, in all sixty cells.
        let card = crate::card::svg(&Comparison::build(card_session(5, 12, &mixed), 0)).unwrap();
        let drawn = sizes(&card);
        assert_eq!(drawn.len(), 120);
        assert_eq!(drawn.iter().filter(|(_, _, s)| s == "63×").count(), 60);
        assert_eq!(drawn.iter().filter(|(_, _, s)| s == "2.9×").count(), 60);
        // One above the other: the two of a cell share their x.
        assert!(drawn.chunks(2).all(|two| two[0].0 == two[1].0 && two[0].1 < two[1].1), "{drawn:?}");
        inside(&card);
        assert!(card.contains("Numbers: the largest clear change in that direction<"));

        // Seven schedulers on twelve workloads: no room for two sizes in
        // any mixed cell, and none in any; the line says nothing of
        // numbers, since there are none.
        let card = crate::card::svg(&Comparison::build(card_session(7, 12, &mixed), 0)).unwrap();
        assert_eq!(card.matches(">mixed<").count(), 84 + 1);
        assert!(sizes(&card).is_empty());
        assert!(card.contains("Compared with the baseline<"));

        // The same grid with one direction only, and the widest size
        // there is: single sizes fit the narrowest column the card has.
        let worse = [("slow", 3377.0)];
        let card = crate::card::svg(&Comparison::build(card_session(7, 12, &worse), 0)).unwrap();
        let drawn = sizes(&card);
        assert_eq!(drawn.len(), 84);
        assert!(drawn.iter().all(|(_, _, s)| s == "&gt;999×"), "{drawn:?}");
        inside(&card);

        // More than fits: what is missing is said, and the sizes of what
        // is drawn stay.
        let card = crate::card::svg(&Comparison::build(card_session(8, 13, &worse), 0)).unwrap();
        assert!(card.contains(
            ">Not shown: 1 more scheduler and 1 more workload. Every change and its certainty are in the report.<"
        ));
        assert_eq!(sizes(&card).len(), 84);
        inside(&card);
    }

    /// A share of counted events in a run, with the counters it is made
    /// of: `pct` percent of 20 000 periods or wake-ups, or of two million
    /// blocked round trips. A missed deadline is a late period and a
    /// skipped one, half each.
    fn counted(r: &mut RunResult, metric: &str, pct: f64) {
        let counters = event_counters(metric).unwrap();
        let of: u64 = if metric == "rtt_blocked_slow_pct" { 2_000_000 } else { 20_000 };
        let events = (pct / 100.0 * of as f64).round() as u64;
        r.counters.insert(counters.of.into(), of);
        for name in counters.events {
            r.counters.insert((*name).into(), events / counters.events.len() as u64);
        }
        r.metrics.insert(metric.into(), Metric::lower(pct, "%"));
    }

    /// The card says what the plain view's headings say, for every
    /// scheduler and workload, and nothing where there is nothing to say.
    #[test]
    fn the_card_holds_the_plain_views_verdicts() {
        let mut files = drifting_session();
        // A second workload, named as one the card has a short name for.
        for f in files.iter_mut() {
            let mut second = f.workloads[0].clone();
            second.name = "wake-burst/double".into();
            f.workloads.push(second);
        }
        files[1].label = "a <scheduler> with a very long label".into();
        let c = Comparison::build(files.clone(), 0);
        let card = crate::card::svg(&c).expect("a card");
        assert!(card.starts_with("<svg ") && card.ends_with("</svg>"));
        assert!(card.contains(r#"viewBox="0 0 1200 630""#) && !card.contains("<style") && !card.contains("<script"));
        assert!(card.contains(">Schedulers compared with base<"));
        // And what the baseline is, from the scheduler its file records.
        let what = crate::card::svg(&c).unwrap();
        let said = format!("The baseline, base, is {}: ", c.series[c.baseline].file.scheduler.label());
        assert!(what.contains(&said), "{said}");
        // Escaped and cut, not broken markup.
        assert!(card.contains(">a &lt;scheduler&gt; wi…<"));
        assert!(card.contains(">burst<") && card.contains(">double<"));
        // One cell per brief, with the brief's own word.
        let briefs = crate::simple::briefs(&c);
        assert_eq!(briefs.len(), 2);
        for b in &briefs {
            let word = match b.verdict {
                Verdict::Better => "better",
                Verdict::Worse => "worse",
                Verdict::Mixed => "mixed",
                Verdict::NoClearDifference => "unclear",
            };
            // In a cell, and once more in the legend.
            assert!(card.matches(&format!(">{word}<")).count() >= 2, "{word}");
        }
        // The line on how to read it and the one on what it leaves out
        // do not share a height, however long the second gets.
        for (line, y) in [("Rows: schedulers", crate::card::READING), ("Every change", crate::card::CAVEAT)] {
            assert!(
                card.contains(&format!(r#"x="48.0" y="{y:.1}" font-family="sans-serif" font-size="15""#)),
                "{line}"
            );
            assert_eq!(card.matches(&format!(r#" y="{y:.1}""#)).count(), 1, "{line}");
        }
        // The candidate is 10% better in both, and the cells say so
        // next to their marks, in the ink of the text; the line on how
        // to read the card says what the number is.
        assert_eq!(briefs.iter().map(|b| b.verdict).collect::<Vec<_>>(), [Verdict::Better; 2]);
        assert_eq!(card.matches(r##"font-size="13" fill="#0b0b0b">10%</text>"##).count(), 2, "{card}");
        assert!(card.contains(
            ">Rows: schedulers  ·  Columns: workloads  ·  Numbers: the largest clear change in that direction<"
        ));
        // The legend's sample cells have none.
        assert_eq!(card.matches(r#"font-size="13""#).count(), 2);
        // Where no cell has a number the line does not speak of any.
        let mut same = files.clone();
        same[1] = same[0].clone();
        same[1].label = "cand".into();
        let none = crate::card::svg(&Comparison::build(same, 0)).unwrap();
        assert!(!none.contains(r#"font-size="13""#) && none.contains(">unclear<"), "{none}");
        assert!(none.contains(">Rows: schedulers  ·  Columns: workloads  ·  Compared with the baseline<"));
        // What it leaves out is said on it.
        assert!(card.contains("are in the report.<"));
        // A single result has nothing to compare, and no card.
        let alone = Comparison::build(vec![files.remove(0)], 0);
        assert!(crate::card::svg(&alone).is_none());
    }

    /// idle-wake in the plain view: the delay, where the task was put,
    /// and whether the machine slept.
    #[test]
    fn idle_wake_is_described_in_plain_words() {
        // (wake-up delay, share on the waker's CPU, share of deep idle),
        // baseline then candidate; no idle accounting where `None`.
        let session = |workload: &str, base: (f64, f64, Option<f64>), cand: (f64, f64, Option<f64>)| {
            let mut files = drifting_session();
            for (f, (wake, same, deep)) in files.iter_mut().zip([base, cand]) {
                f.workloads[0].name = workload.into();
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    r.metrics.clear();
                    r.metrics.insert("wake_p50".into(), Metric::lower(wake * (1.0 + 0.01 * k as f64), "us"));
                    r.metrics.insert("same_cpu_pct".into(), Metric::info(same, "%"));
                    r.metrics.insert("prev_cpu_pct".into(), Metric::info(100.0 - same, "%"));
                    r.metrics
                        .insert("wakes_per_s".into(), Metric::info(if same > 50.0 { 150.5 } else { 197.0 }, "1/s"));
                    let late = if same > 50.0 { 12.0 } else { 640.0 };
                    r.metrics.insert("timer_late_p50".into(), Metric::info(late, "us"));
                    if let Some(deep) = deep {
                        r.metrics.insert("idle_deepest_pct".into(), Metric::info(deep, "%"));
                    }
                }
            }
            Comparison::build(files, 0)
        };
        let c = session("idle-wake/handoff", (400.0, 2.0, Some(91.0)), (40.0, 97.0, Some(88.0)));
        let text = crate::simple::text(&c);
        assert!(text.contains("  A task woken on a sleeping machine, by one that sleeps again: BETTER\n"), "{text}");
        assert!(text.contains("• Typical wake-up delay: "), "{text}");
        assert!(
            text.contains(
                "• Ran on the CPU of the thread that woke it: 97% of wake-ups (2% under base); the CPUs were in \
                 their deepest idle state 88% of the time (91% under base) (observed, not scored)"
            ),
            "{text}"
        );
        // The waker's own wake-up is said next to the task's, unscored.
        assert!(
            text.contains(
                "• The waking thread itself ran 12.0 µs after its timer, typically (640 µs under base; observed, not \
                 scored)"
            ),
            "{text}"
        );
        // What each side was asked for, where it differs and where not.
        assert!(
            text.contains(
                "• Wake-ups sent: 150 a second, 197 under base (observed, not scored; with fewer a scheduler was \
                 asked for less)"
            ),
            "{text}"
        );
        assert!(text.ends_with(&format!("\n{}\n", crate::simple::WHAT_IDLE)), "{text}");
        // Where the kernel reports no idle states, nothing is said of them.
        let text = crate::simple::text(&session("idle-wake/busy", (400.0, 2.0, None), (900.0, 1.0, None)));
        assert!(text.contains("  A task woken on a sleeping machine, by one that goes on working: WORSE\n"), "{text}");
        assert!(
            text.contains(
                "• Ran on the CPU of the thread that woke it: 1% of wake-ups (2% under base) (observed, not scored)"
            ),
            "{text}"
        );
        // Where the task ran is not scored: equal delays, no verdict from it.
        let text = crate::simple::text(&session("idle-wake/busy", (400.0, 2.0, None), (400.0, 98.0, None)));
        assert!(!text.contains("BETTER") && !text.contains("WORSE"), "{text}");
    }

    /// cgroup-fair in the plain view: the split, what the weights ask
    /// for, and whether the split is the one a scheduler without groups
    /// or without weights gives.
    #[test]
    fn cgroup_fair_is_described_in_plain_words() {
        // Shares of the first group: (baseline, candidate).
        let session = |workload: &str, share: (f64, f64), expected: f64, flat: f64| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = workload.into();
                for r in f.workloads[0].runs.iter_mut() {
                    r.metrics.clear();
                    let a = if is_cand { share.1 } else { share.0 };
                    let off = (a / (100.0 - a)) / (expected / (100.0 - expected));
                    r.metrics.insert("weight_accuracy_pct".into(), Metric::higher(off.min(1.0 / off) * 100.0, "%"));
                    r.metrics.insert("cpu_used_pct".into(), Metric::higher(99.0, "%"));
                    r.metrics.insert("share_a_pct".into(), Metric::info(a, "%"));
                    r.metrics.insert("expected_share_a_pct".into(), Metric::info(expected, "%"));
                    r.metrics.insert("flat_share_a_pct".into(), Metric::info(flat, "%"));
                    r.counters.insert("cpu_ms_a".into(), (a * 100.0) as u64);
                    r.counters.insert("cpu_ms_b".into(), ((100.0 - a) * 100.0) as u64);
                }
            }
            Comparison::build(files, 0)
        };
        // The baseline follows the weights; the candidate ignores them.
        let c = session("cgroup-fair/weighted", (25.5, 49.5), 25.0, 50.0);
        let text = crate::simple::text(&c);
        assert!(text.contains("  Two control groups, same tasks, one with three times the weight: WORSE\n"), "{text}");
        assert!(
            text.contains(
                "• CPU time of the two groups: 49.5% : 50.5% (the weights ask for 25.0% : 75.0%), which is the split \
                 by number of tasks, as if the weights were not there; under base: 25.5% : 74.5%"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "• How closely the split follows the weights: 2.9× further (97.4% → 34.0% of the intended ratio)"
            ),
            "{text}"
        );
        assert!(
            text.contains("• CPU time used: no reliable difference; the interval includes no change (99.0% → 99.0% of the machine's CPU time)"),
            "{text}"
        );
        assert!(text.ends_with(&format!("\n{}\n", crate::simple::WHAT_CGROUP)), "{text}");

        // No group scheduling at all: the case with unequal thread counts.
        let text = crate::simple::text(&session("cgroup-fair/equal", (50.0, 25.5), 50.0, 25.0));
        assert!(text.contains("  Two control groups, same weight, one with three times the tasks: WORSE\n"), "{text}");
        assert!(
            text.contains("which is the split by number of tasks, as if the groups were not there; under base"),
            "{text}"
        );

        // Off, but not by the rule of threads: no such remark. The
        // scheduler knows the weights and misses them.
        let text = crate::simple::text(&session("cgroup-fair/weighted", (25.0, 35.0), 25.0, 50.0));
        assert!(
            text.contains(
                "• CPU time of the two groups: 35.0% : 65.0% (the weights ask for 25.0% : 75.0%); under base"
            ),
            "{text}"
        );
        assert!(!text.contains("as if"), "{text}");
        // And one that follows them gets none either.
        let text = crate::simple::text(&session("cgroup-fair/weighted", (49.5, 25.2), 25.0, 50.0));
        assert!(text.contains("BETTER") && !text.contains("as if"), "{text}");
        assert!(text.contains("• How closely the split follows the weights: 2.9× closer ("), "{text}");

        let html = crate::html::report(&c);
        assert!(html.contains(&crate::esc(crate::simple::WHAT_CGROUP)), "{html}");
        assert!(html.contains("<code>cpu_ms_a</code>"));
        // With another workload it is one more part of the test.
        let mut files = drifting_session();
        for f in &mut files {
            let mut second = f.workloads[0].clone();
            f.workloads[0].name = "wake-pingpong".into();
            second.name = "cgroup-fair/equal".into();
            f.workloads.push(second);
        }
        let what = crate::simple::what(&Comparison::build(files, 0));
        assert!(what.contains(" It also runs two control groups of busy tasks"), "{what}");
    }

    /// A session run with `--sched-diag`: the plain view says how the
    /// scheduler got to its numbers, without scoring any of it, and the
    /// diagnostics show the shares.
    #[test]
    fn scheduler_statistics_are_shown_and_not_scored() {
        let session = |diag: bool| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                f.workloads[0].name = "wake-burst/double".into();
                for r in f.workloads[0].runs.iter_mut() {
                    r.metrics.clear();
                    r.metrics.insert("start_p50".into(), Metric::lower(if is_cand { 20.0 } else { 40.0 }, "us"));
                    if diag {
                        let (wait, preempt, moves) = if is_cand { (56.4, 0.1, 281.0) } else { (31.8, 12.0, 1500.0) };
                        r.metrics.insert("sched_wait_pct".into(), Metric::info(wait, "%"));
                        r.metrics.insert("sched_preempt_pct".into(), Metric::info(preempt, "%"));
                        r.metrics.insert("sched_migrations_per_s".into(), Metric::info(moves, "1/s"));
                        for (name, n) in [
                            ("sched_wait_ms", if is_cand { 564 } else { 318 }),
                            ("sched_runnable_ms", 1000),
                            ("sched_involuntary", if is_cand { 1 } else { 120 }),
                            ("sched_switches", 1000),
                        ] {
                            r.counters.insert(name.into(), n);
                        }
                    }
                }
            }
            Comparison::build(files, 0)
        };
        let c = session(true);
        let text = crate::simple::text(&c);
        assert!(
            text.contains(
                "• How the scheduler got there (observed, not scored; base in brackets): the tasks waited for a CPU \
                 56% of the time they wanted one (32%); 0.1% of their switches took the CPU away from them (12%); they \
                 changed CPU 281 times a second (1500)\n"
            ),
            "{text}"
        );
        // One measurement is scored, as without the statistics.
        assert!(text.contains("• Of 1 measurements, 1 clearly better and 0 clearly worse"), "{text}");
        assert_eq!(c.summary()[0].verdict(), Verdict::Better);
        let html = crate::html::report(&c);
        let share = |label: &str, base: &str, cand: &str| {
            format!("<td><strong>{label}</strong></td><td><strong>{base}</strong></td><td><strong>{cand}</strong></td>")
        };
        assert!(html.contains(&share("time the threads wanted a CPU and waited for one", "31.8%", "56.4%")), "{html}");
        assert!(html.contains(&share("switches that took the CPU away from a thread", "12.0%", "0.1%")), "{html}");
        assert!(html.contains("<code>sched_runnable_ms</code>"));
        // Without them the line is not there.
        assert!(!crate::simple::text(&session(false)).contains("How the scheduler got there"));
    }

    /// The summary's largest changes read largest first: multiples before
    /// percentages, each by the number shown.
    #[test]
    fn largest_clear_changes_are_in_the_order_they_read() {
        let mut files = drifting_session();
        for (f, cand) in files.iter_mut().zip([false, true]) {
            for r in &mut f.workloads[0].runs {
                let v = r.metrics["p99"].value;
                // Candidate: latency 25.1% higher, rate 20.4% lower, wait 3× longer.
                let (p99, ops, wait) = if cand { (1.251 / 0.9, 0.796 / 0.9, 3.0 / 0.9) } else { (1.0, 1.0, 1.0) };
                r.metrics.insert("p99".into(), Metric::lower(v * p99, "us"));
                r.metrics.insert("ops".into(), Metric::higher(v * ops, "1/s"));
                r.metrics.insert("wait".into(), Metric::lower(v * wait, "us"));
            }
        }
        let c = Comparison::build(files, 0);
        let s = &c.summary()[0];
        let top: Vec<(&str, String)> = s.top.iter().map(|(m, d)| (m.as_str(), crate::svg::fmt_delta(d))).collect();
        assert_eq!(
            top,
            [
                ("wait", "3.0× worse".to_string()),
                ("p99", "\u{2212}25.1%".to_string()),
                ("ops", "\u{2212}20.4%".to_string())
            ]
        );
    }

    #[test]
    fn minor_populations_are_dashed_on_quantile_charts() {
        for share in [true, false] {
            let with_curves = |mut f: ResultFile| {
                for r in &mut f.workloads[0].runs {
                    for pop in ["rtt_blocked", "rtt_fast"] {
                        r.quantile_curves.insert(pop.into(), vec![(0.5, 10.0), (0.9, 20.0), (0.99, 40.0)]);
                    }
                    r.quantile_unit = Some("us".into());
                }
                f
            };
            let c = Comparison::build(
                vec![
                    with_curves(populations("base", 500, 10.0, share)),
                    with_curves(populations("cand", 995, 5.0, share)),
                ],
                0,
            );
            let d = |p: &str| c.distributions.iter().find(|d| d.population == p).unwrap();
            let fast = d("rtt_fast");
            assert!(!fast.minor(0, 1.0) && fast.minor(1, 1.0), "share={share}");
            assert!((fast.shares[1].unwrap() - 0.005).abs() < 1e-12);
            let svg = crate::svg::tail_chart(&c, fast).unwrap();
            assert_eq!(svg.matches("stroke-dasharray").count(), 1, "only the candidate's curve");
            assert!(svg.contains("cand (minor 0.5%)") && svg.contains("dashed: minor population"));
            // The note is one fixed line: series are named in labels and hover text only.
            let note = svg.split("dashed: minor population").nth(1).unwrap().split('<').next().unwrap();
            assert!(!note.contains("cand") && !note.contains("0.5%"), "{note}");
            let svg = crate::svg::tail_chart(&c, d("rtt_blocked")).unwrap();
            assert!(!svg.contains("stroke-dasharray") && !svg.contains("minor"));
        }
    }

    #[test]
    fn zero_median_metrics_can_be_unstable() {
        let c = Comparison::build(vec![file("base", &[0.0, 0.0, 0.0, 100.0, 100.0], 1)], 0);
        assert!(c.rows.iter().find(|r| r.metric == "p99").unwrap().cells[0].as_ref().unwrap().unstable);
    }

    #[test]
    fn small_effects_are_not_notable() {
        // ~2% shift with tight runs: interval excludes zero, but below 5%.
        let c = Comparison::build(
            vec![
                file("base", &[100.0, 100.1, 99.9, 100.0, 100.05], 1),
                file("cand", &[98.0, 98.1, 97.9, 98.0, 98.05], 1),
            ],
            0,
        );
        let d = c.rows.iter().find(|r| r.metric == "p99").unwrap().improvement(0, 1).unwrap();
        assert!(d.ci_excludes_zero());
        assert!(!d.notable(5.0));
        assert!(d.notable(1.0));
        assert!((d.ratio - 100.0 / 98.0).abs() < 1e-9);
    }

    /// Two schedulers from one interleaved session with strong drift between
    /// rounds and a constant 10% difference within every round.
    fn drifting_session() -> Vec<ResultFile> {
        let drift = [100.0, 140.0, 80.0, 160.0, 90.0, 130.0];
        let mk = |label: &str, factor: f64| {
            let mut f = file(label, &[], 1);
            f.started_at = 42;
            f.design = Some(helicoprion_core::Design {
                kind: "interleaved".into(),
                rounds: drift.len() as u32,
                schedulers: vec!["base".into(), "cand".into()],
                spec: label.into(),
                session_id: Some("0123456789abcdef".into()),
                ..Default::default()
            });
            for (round, d) in drift.iter().enumerate() {
                for jitter in [0.99, 1.01] {
                    let mut r = RunResult { valid: true, round: Some(round as u32), ..Default::default() };
                    r.metrics.insert("p99".into(), Metric::lower(d * factor * jitter, "us"));
                    f.workloads[0].runs.push(r);
                }
            }
            f
        };
        vec![mk("base", 1.0), mk("cand", 0.9)]
    }

    #[test]
    fn paired_rounds_cancel_drift() {
        let c = Comparison::build(drifting_session(), 0);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert_eq!(d.paired_rounds, Some(6));
        let (lo, hi) = d.ci.unwrap();
        // Every round shows exactly a 10% improvement: tight, excludes zero.
        assert!((d.median - 10.0).abs() < 1.0, "{d:?}");
        assert!(lo > 8.0 && hi < 12.0, "{d:?}");
        assert!(c.any_paired());
        // Raw runs swing with the drift (unstable per run), but the paired
        // effect is consistent, so it is charted and can be highlighted.
        assert!(c.rows[0].cells[1].as_ref().unwrap().unstable);
        assert!(!c.rows[0].unstable_pair(0, 1));
        assert!(c.rows[0].chartable(0, 1));

        // The same numbers without the session link: the drift swamps the effect.
        let mut files = drifting_session();
        files[1].design.as_mut().unwrap().session_id = Some("another-session".into());
        let u = Comparison::build(files, 0).rows[0].improvement(0, 1).unwrap();
        assert_eq!(u.paired_rounds, None);
        let (ulo, uhi) = u.ci.unwrap();
        assert!(ulo < 0.0 && uhi > 0.0, "unpaired interval should straddle zero: {u:?}");
    }

    #[test]
    fn uneven_paired_changes_are_tagged_not_scored() {
        // The candidate is 10% better in every other round and 50% better
        // in the rest: always better, but not by one amount.
        let mut files = drifting_session();
        for (k, r) in files[1].workloads[0].runs.iter_mut().enumerate() {
            if (k / 2) % 2 == 0 {
                r.metrics.get_mut("p99").unwrap().value *= 0.5 / 0.9;
            }
        }
        let c = Comparison::build(files, 0);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert!(d.notable(c.opts.min_effect_pct), "interval excludes zero, large: {d:?}");
        // Ratios of 1.11 and 2.0: about ±34% around their middle.
        let uneven = d.uneven(c.opts.unstable_spread_pct).expect("uneven");
        assert!(matches!(uneven, PairedSpread::Relative(s) if (30.0..40.0).contains(&s)), "{uneven:?}");
        assert!(c.rows[0].unstable_pair(0, 1) && !c.clear(&c.rows[0], 1, &d) && !c.rows[0].chartable(0, 1));
        let s = &c.summary()[0];
        assert_eq!((s.verdict(), s.better, s.uneven, s.unclear()), (Verdict::NoClearDifference, 0, 1, 1));
        // Both outputs say why it is not scored.
        let html = crate::html::report(&c);
        assert!(html.contains(&format!(">uneven {uneven}</span>")) && html.contains("(1 uneven)"), "{html}");
        let text = crate::html::text_table(&c);
        assert!(text.contains("≠)") && text.contains("1 unclear, 1 of them uneven across rounds"), "{text}");

        // The even session carries no tag.
        let c = Comparison::build(drifting_session(), 0);
        assert!(c.rows[0].improvement(0, 1).unwrap().uneven(c.opts.unstable_spread_pct).is_none());
        assert!(
            !crate::html::report(&c).contains("class=bad title=\"the per-round ratios")
                && !crate::html::text_table(&c).contains("≠)")
        );
        assert_eq!(c.summary()[0].uneven, 0);
    }

    /// A metric that exists in one round only (a population that is usually
    /// empty) must not make the warning read as if the session were short.
    #[test]
    fn few_rounds_warning_says_how_many_metrics() {
        let mut files = drifting_session();
        for f in &mut files {
            for r in f.workloads[0].runs.iter_mut().filter(|r| r.round == Some(0)) {
                r.metrics.insert("rare_p50".into(), Metric::lower(1.0, "us"));
            }
        }
        let c = Comparison::build(files, 0);
        let w = c.warnings.iter().find(|w| w.contains("fewer than")).expect("warning");
        assert!(w.contains("cand: 1 of 2 metrics paired") && w.contains("as few as 1"), "{w}");
        assert!(!Comparison::build(drifting_session(), 0).warnings.iter().any(|w| w.contains("fewer than")));
    }

    /// A change of many multiples that is the same multiple in every round
    /// is even, in either direction and for either kind of metric. In
    /// percentage points its rounds are hundreds apart.
    #[test]
    fn steady_multiples_are_not_uneven() {
        let mut files = drifting_session();
        for f in &mut files {
            for r in &mut f.workloads[0].runs {
                let v = r.metrics["p99"].value;
                r.metrics.insert("ops".into(), Metric::higher(v, "1/s"));
            }
        }
        // Candidate: p99 40× higher (worse), ops 26× higher (better), each
        // within ±4% from round to round.
        for (k, r) in files[1].workloads[0].runs.iter_mut().enumerate() {
            let wobble = if (k / 2) % 2 == 0 { 1.04 } else { 0.96 };
            r.metrics.get_mut("p99").unwrap().value *= 40.0 / 0.9 * wobble;
            r.metrics.get_mut("ops").unwrap().value *= 26.0 / 0.9 * wobble;
        }
        let c = Comparison::build(files, 0);
        for (metric, ratio) in [("p99", 1.0 / 40.0), ("ops", 26.0)] {
            let row = c.rows.iter().find(|r| r.metric == metric).unwrap();
            let d = row.improvement(0, 1).unwrap();
            assert!((d.ratio / ratio - 1.0).abs() < 0.05, "{metric}: {d:?}");
            assert!((d.median.abs()) > 2000.0, "{metric}: a change of thousands of percent: {d:?}");
            let spread = d.paired_spread.unwrap();
            assert!(matches!(spread, PairedSpread::Relative(s) if (3.0..6.0).contains(&s)), "{metric}: {spread:?}");
            assert!(d.uneven(c.opts.unstable_spread_pct).is_none(), "{metric}");
            assert!(c.clear(row, 1, &d) && row.chartable(0, 1), "{metric}");
        }
        let s = &c.summary()[0];
        assert_eq!((s.verdict(), s.better, s.worse, s.uneven), (Verdict::Mixed, 1, 1, 0));
    }

    /// An effect larger than its own unevenness is scored. The same
    /// measurement must not read "clear" in one session and "unclear" in
    /// the next only because the baseline wobbled a little more.
    #[test]
    fn an_effect_larger_than_its_unevenness_is_scored() {
        // Round trips per second, higher is better: the candidate has `a`
        // times fewer in even rounds and `b` times fewer in odd ones.
        let session = |a: f64, b: f64| {
            let mut files = drifting_session();
            for (f, is_cand) in files.iter_mut().zip([false, true]) {
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    let wobble = r.metrics["p99"].value / if is_cand { 90.0 } else { 100.0 };
                    let factor = match (is_cand, (k / 2) % 2 == 0) {
                        (false, _) => 1.0,
                        (true, true) => 1.0 / a,
                        (true, false) => 1.0 / b,
                    };
                    r.metrics.clear();
                    r.metrics.insert("roundtrips_per_s".into(), Metric::higher(1.2e6 * factor * wobble, "1/s"));
                }
            }
            Comparison::build(files, 0)
        };
        let threshold = Options::default().unstable_spread_pct;

        // 2× to 3× fewer: the rounds differ by about ±22%, over the
        // threshold, but the cautious end is 2×, 100% away from no change.
        let c = session(2.0, 3.0);
        let row = &c.rows[0];
        let d = row.improvement(0, 1).unwrap();
        assert!(matches!(d.paired_spread, Some(PairedSpread::Relative(s)) if (20.0..25.0).contains(&s)), "{d:?}");
        assert_eq!(d.uneven(threshold), None, "{d:?}");
        assert!(!row.unstable_pair(0, 1) && c.clear(row, 1, &d) && row.chartable(0, 1));
        assert_eq!(c.held_back_by_unevenness(row, 1, &d), None);
        let s = &c.summary()[0];
        assert_eq!((s.verdict(), s.worse, s.uneven), (Verdict::Worse, 1, 0));
        assert!(!crate::html::report(&c).contains("class=bad title=\"the per-round ratios"));
        assert!(crate::simple::text(&c).contains("• Round trips completed: 2.4× fewer ("));
        // The chart's own help says so too, with the session's threshold.
        assert!(crate::html::report(&c).contains("differ by more than ±10% and by more than the effect itself"));

        // 1.2× to 3× fewer: about ±58%, against a cautious end of 1.2×,
        // 20% away. The size is anyone's guess: still held back.
        let c = session(1.2, 3.0);
        let row = &c.rows[0];
        let d = row.improvement(0, 1).unwrap();
        assert!(d.notable(c.opts.min_effect_pct), "{d:?}");
        assert!(matches!(d.uneven(threshold), Some(PairedSpread::Relative(s)) if s > 50.0), "{d:?}");
        assert!(row.unstable_pair(0, 1) && !c.clear(row, 1, &d));
        assert_eq!(c.summary()[0].uneven, 1);

        // A steady effect has nothing to outweigh.
        let c = session(2.0, 2.0);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert_eq!(d.uneven(threshold), None);
        assert!(c.clear(&c.rows[0], 1, &d));
    }

    /// The edges of "the effect outweighs its unevenness", on `Delta`
    /// itself: both directions of metric, an interval across no change, an
    /// effect exactly as large as the spread, and ends without a ratio.
    #[test]
    fn outweighing_is_cautious_at_its_edges() {
        let d = |better, ci: (f64, f64), spread| Delta {
            median: (ci.0 + ci.1) / 2.0,
            ci: Some(ci),
            ratio: 1.0,
            paired_rounds: Some(5),
            unmatched_rounds: false,
            paired_spread: Some(spread),
            undefined_rounds: 0,
            better,
        };
        let rel = PairedSpread::Relative;
        let uneven = |d: &Delta| d.uneven(10.0).is_some();

        // Higher is better. 1.9× to 2.5× fewer is -47.4% to -60%.
        assert!(!uneven(&d(Better::Higher, (-60.0, -47.4), rel(14.6))));
        // Lower is better: 1.9× to 2.5× longer is -90% to -150%; the
        // cautious end is 90% away. Shorter by 20% to 60% is 25% away.
        assert!(!uneven(&d(Better::Lower, (-150.0, -90.0), rel(14.6))));
        assert!(uneven(&d(Better::Lower, (-150.0, -12.0), rel(14.6))), "cautious end only 12% away");
        assert!(!uneven(&d(Better::Lower, (20.0, 60.0), rel(14.6))));
        assert!(uneven(&d(Better::Lower, (20.0, 60.0), rel(30.0))), "25% away against ±30%");
        // The same change of a rate and of a latency is not the same
        // distance: -20% of a rate is 25% away (1/0.8), -20% of a latency 20%.
        assert!(!uneven(&d(Better::Higher, (-40.0, -20.0), rel(22.0))));
        assert!(uneven(&d(Better::Lower, (-40.0, -20.0), rel(22.0))));

        // An interval across no change supports no direction.
        assert!(uneven(&d(Better::Higher, (-10.0, 300.0), rel(14.6))));
        assert!(uneven(&d(Better::Lower, (-300.0, 10.0), rel(14.6))));
        assert!(uneven(&d(Better::Higher, (0.0, 300.0), rel(14.6))));

        // Exactly as far as the spread: not outweighed. +25% of a rate is
        // a factor of 1.25, 25% away.
        assert!(uneven(&d(Better::Higher, (25.0, 80.0), rel(25.0))));
        assert!(!uneven(&d(Better::Higher, (25.0, 80.0), rel(24.9))));

        // An end without a usable ratio (a rate down by 100%: nothing left;
        // a latency down by 100%: an infinite ratio) gives no distance.
        assert!(uneven(&d(Better::Higher, (-100.0, -60.0), rel(14.6))));
        assert!(uneven(&d(Better::Lower, (60.0, 100.0), rel(14.6))));
        assert!(uneven(&d(Better::Lower, (f64::NAN, 60.0), rel(14.6))));

        // A spread in percentage points is never outweighed, and a spread
        // under the threshold never holds anything back.
        assert!(uneven(&d(Better::Higher, (-60.0, -47.4), PairedSpread::Points(14.6))));
        assert!(!uneven(&d(Better::Lower, (-150.0, -12.0), rel(9.9))));
        // No interval, nothing to outweigh with.
        let mut no_ci = d(Better::Higher, (-60.0, -47.4), rel(14.6));
        no_ci.ci = None;
        assert!(uneven(&no_ci));
    }

    /// The uneven tag explains why a change that looks clear is not scored,
    /// so it is shown only where unevenness is the one reason, and the
    /// summary counts the same changes.
    #[test]
    fn uneven_tag_is_shown_only_where_it_is_the_reason() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Case {
            Plain,
            Extreme,
            TwoRounds,
            IntervalIncludesZero,
            BelowMinEffect,
            Minor,
            UndefinedRound,
        }
        let session = |case: Case| {
            let mut files = drifting_session();
            // Candidate's extra factor in even and odd rounds, on top of the
            // helper's steady 0.9 (10% better).
            let (even, odd) = match case {
                Case::IntervalIncludesZero => (0.6, 1.6),
                _ => (0.5 / 0.9, 1.0),
            };
            for (f, cand) in files.iter_mut().zip([false, true]) {
                if case == Case::TwoRounds {
                    f.workloads[0].runs.retain(|r| r.round.is_some_and(|k| k < 2));
                }
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    let round = k / 2;
                    let m = r.metrics.get_mut("p99").unwrap();
                    if cand {
                        m.value *= if round % 2 == 0 { even } else { odd };
                    }
                    m.extreme = case == Case::Extreme;
                    if case == Case::Minor {
                        m.share = Some(0.001);
                    }
                    if case == Case::UndefinedRound && !cand && round == 0 {
                        m.value = 0.0;
                    }
                }
            }
            let opts = Options {
                // 10% vs 50% better: a median of 30%, below a 40% minimum.
                min_effect_pct: if case == Case::BelowMinEffect { 40.0 } else { 5.0 },
                ..Options::default()
            };
            Comparison::build_with(files, 0, opts)
        };
        for case in [
            Case::Plain,
            Case::Extreme,
            Case::TwoRounds,
            Case::IntervalIncludesZero,
            Case::BelowMinEffect,
            Case::Minor,
            Case::UndefinedRound,
        ] {
            let c = session(case);
            let d = c.rows[0].improvement(0, 1).unwrap();
            // The rule itself sees the unevenness in every case, and none is scored.
            assert!(d.uneven(c.opts.unstable_spread_pct).is_some(), "{case:?}: {d:?}");
            assert!(c.rows[0].unstable_pair(0, 1) && !c.clear(&c.rows[0], 1, &d), "{case:?}");
            // One reason each, the first that applies, and the same one
            // wherever the report gives it.
            let reason = c.unclear_reason(&c.rows[0], 1, &d).expect("not scored");
            let expected = match case {
                Case::Plain => matches!(reason, UnclearReason::Uneven(PairedSpread::Relative(_))),
                Case::Extreme => reason == UnclearReason::SingleSample,
                Case::TwoRounds => reason == UnclearReason::TooFewRounds,
                Case::IntervalIncludesZero => reason == UnclearReason::IntervalIncludesNoChange,
                Case::BelowMinEffect => reason == UnclearReason::BelowThreshold,
                Case::Minor => reason == UnclearReason::MinorPopulation,
                Case::UndefinedRound => reason == UnclearReason::UndefinedRounds,
            };
            assert!(expected, "{case:?}: {reason:?}");
            // Tag, terminal mark and summary count agree: only the plain case.
            let want = case == Case::Plain;
            assert_eq!(c.held_back_by_unevenness(&c.rows[0], 1, &d).is_some(), want, "{case:?}: {d:?}");
            assert_eq!(crate::html::report(&c).contains("class=bad title=\"the per-round ratios"), want, "{case:?}");
            assert_eq!(crate::html::text_table(&c).contains("≠)"), want, "{case:?}");
            let counted: usize = c.summary().iter().map(|s| s.uneven).sum();
            assert_eq!(counted, usize::from(want), "{case:?}");
        }
    }

    /// A round where the candidate measured zero has a change (-100%) but
    /// no usable ratio. It must still count: on a ratio scale the remaining
    /// rounds (all -50%) would look perfectly even.
    #[test]
    fn rounds_without_a_ratio_still_count_as_uneven() {
        let mut files = drifting_session();
        for (f, cand) in files.iter_mut().zip([false, true]) {
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let v = match (cand, (k / 2) % 2 == 0) {
                    (false, _) => 100.0,
                    (true, true) => 0.0,
                    (true, false) => 50.0,
                };
                r.metrics.insert("ops".into(), Metric::higher(v, "1/s"));
            }
        }
        let c = Comparison::build(files, 0);
        let row = c.rows.iter().find(|r| r.metric == "ops").unwrap();
        let d = row.improvement(0, 1).unwrap();
        assert_eq!(d.paired_rounds, Some(6));
        assert!((d.median + 75.0).abs() < 1e-9, "{d:?}");
        assert!(d.notable(c.opts.min_effect_pct), "tight interval, large change: {d:?}");
        // In percentage points of the change, all six rounds counted.
        assert_eq!(d.paired_spread, Some(PairedSpread::Points(25.0)));
        assert_eq!(d.uneven(c.opts.unstable_spread_pct), Some(PairedSpread::Points(25.0)));
        assert!(row.unstable_pair(0, 1) && !c.clear(row, 1, &d) && !row.chartable(0, 1));
        // The label agrees with the median change (4× worse), not with the
        // median of the three ratios that exist (2× worse).
        assert!((d.ratio - 0.25).abs() < 1e-9, "{d:?}");
        assert!(crate::html::report(&c).contains(">uneven ±25.0 pp</span>"));

        // A zero on the candidate's side of a lower-is-better metric (an
        // infinite ratio) takes the same path.
        let mut files = drifting_session();
        for (k, r) in files[1].workloads[0].runs.iter_mut().enumerate() {
            if (k / 2) % 2 == 0 {
                r.metrics.get_mut("p99").unwrap().value = 0.0;
            }
        }
        let d = Comparison::build(files, 0).rows[0].improvement(0, 1).unwrap();
        assert!(matches!(d.paired_spread, Some(PairedSpread::Points(_))), "{d:?}");
    }

    /// A round where the baseline measured zero has no percentage change at
    /// all. Dropping it silently would leave three rounds of a steady -50%
    /// out of six, in three of which the candidate did better than nothing.
    #[test]
    fn rounds_with_a_zero_baseline_block_scoring() {
        let mut files = drifting_session();
        for (f, cand) in files.iter_mut().zip([false, true]) {
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                let v = match (cand, (k / 2) % 2 == 0) {
                    (true, _) => 50.0,
                    (false, true) => 0.0,
                    (false, false) => 100.0,
                };
                r.metrics.insert("ops".into(), Metric::higher(v, "1/s"));
            }
        }
        let c = Comparison::build(files, 0);
        let row = c.rows.iter().find(|r| r.metric == "ops").unwrap();
        let d = row.improvement(0, 1).unwrap();
        // What the three usable rounds say, taken alone: a tight -50%.
        assert_eq!((d.paired_rounds, d.undefined_rounds), (Some(3), 3));
        assert!((d.median + 50.0).abs() < 1e-9 && d.notable(c.opts.min_effect_pct), "{d:?}");
        assert_eq!(d.paired_spread, Some(PairedSpread::Relative(0.0)));
        assert!(d.uneven(c.opts.unstable_spread_pct).is_none(), "not an unevenness problem");
        // Not scored anywhere, and the report says why.
        assert!(row.unstable_pair(0, 1) && !c.clear(row, 1, &d) && !row.chartable(0, 1));
        let s = &c.summary()[0];
        assert_eq!((s.worse, s.uneven), (0, 0), "{s:?}");
        assert!(c.warnings.iter().any(|w| w.contains("cand: for 1 metric(s) the baseline measured zero")));
        assert!(crate::html::report(&c).contains(">3 of 6 rounds undefined</span>"));
        let text = crate::html::text_table(&c);
        assert!(text.contains("worse?)") && text.contains("? baseline measured zero"), "{text}");

        // p99 of the same session has a change in every round: untouched.
        let p99 = c.rows.iter().find(|r| r.metric == "p99").unwrap();
        let d = p99.improvement(0, 1).unwrap();
        assert_eq!(d.undefined_rounds, 0);
        assert!(c.clear(p99, 1, &d));
    }

    /// Two sessions started in the same second with the same scheduler list
    /// are still two experiments: their rounds must not be paired.
    #[test]
    fn sessions_are_told_apart_by_id_not_start_time() {
        let mut files = drifting_session();
        files[1].design.as_mut().unwrap().session_id = Some("fedcba9876543210".into());
        assert_eq!(files[0].started_at, files[1].started_at);
        let d = Comparison::build(files, 0).rows[0].improvement(0, 1).unwrap();
        assert_eq!(d.paired_rounds, None);
        assert!(!d.unmatched_rounds, "different sessions: an ordinary unpaired comparison");

        // Files from before the id existed still pair by start time + list.
        let mut legacy = drifting_session();
        for f in &mut legacy {
            f.design.as_mut().unwrap().session_id = None;
        }
        assert_eq!(Comparison::build(legacy, 0).rows[0].improvement(0, 1).unwrap().paired_rounds, Some(6));
    }

    /// `matrix --rounds 2 --runs-per-visit 5`: ten runs per scheduler but
    /// only two independent rounds. No per-run bootstrap (pseudoreplication):
    /// a paired point estimate, no interval, never highlighted.
    #[test]
    fn two_rounds_give_a_point_estimate_without_interval() {
        let mut files = drifting_session();
        for f in &mut files {
            f.workloads[0].runs.retain(|r| r.round.unwrap() < 2);
            let extra: Vec<_> = f.workloads[0].runs.clone();
            for _ in 0..4 {
                f.workloads[0].runs.extend(extra.iter().cloned());
            }
        }
        assert_eq!(files[0].workloads[0].runs.len(), 20);
        let c = Comparison::build(files, 0);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert_eq!(d.paired_rounds, Some(2));
        assert!(d.ci.is_none());
        assert!((d.median - 10.0).abs() < 1.0);
        assert!(!d.notable(0.0));
        assert!(c.warnings.iter().any(|w| w.contains("all metrics paired") && w.contains("as few as 2")));
    }

    #[test]
    fn same_session_without_shared_rounds_never_uses_per_run_bootstrap() {
        let mut files = drifting_session();
        files[0].workloads[0].runs.retain(|r| r.round.unwrap() < 3);
        files[1].workloads[0].runs.retain(|r| r.round.unwrap() >= 3);
        let c = Comparison::build(files, 0);
        let d = c.rows[0].improvement(0, 1).unwrap();
        assert_eq!(d.paired_rounds, None, "nothing was paired");
        assert!(d.unmatched_rounds);
        assert!(d.ci.is_none());
        assert!(!c.any_paired());
        assert!(c.warnings.iter().any(|w| w.contains("share no round") && w.contains("not paired")));
        let html = crate::html::report(&c);
        assert!(html.contains("no matching rounds"), "report must not call it paired");
        assert!(!html.contains("paired, 0"));
    }

    #[test]
    fn env_differences_are_reported() {
        let base = file("base", &[1.0], 1);
        let mut other = file("other", &[1.0], 1);
        other.system.usable_cpus = 16;
        other.system.kernel = "6.19".into();
        let c = Comparison::build(vec![base, other], 0);
        assert!(c.env_notes.iter().any(|n| n.contains("usable CPUs")));
        assert!(c.env_notes.iter().any(|n| n.contains("kernel")));
        // Environment differences do not block the comparison.
        assert!(c.rows[0].improvement(0, 1).is_some());
    }

    /// The report shows when each scheduler was measured and the state of
    /// the machine then, and the rest of what the files record, without
    /// inventing either for files that have neither.
    #[test]
    fn conditions_and_details_are_shown_when_recorded() {
        use helicoprion_core::{Freq, Reading};
        let old = Comparison::build(drifting_session(), 0);
        let html = crate::html::report(&old);
        assert!(!html.contains("<h2>Conditions</h2>") && !html.contains("More about the systems"), "old files");

        let mut files = drifting_session();
        for (i, f) in files.iter_mut().enumerate() {
            f.system.preempt = Some("full".into());
            f.system.temp_sensor = Some("k10temp/Tctl".into());
            f.design.as_mut().unwrap().options = (i == 1).then(|| vec!["--slice-us".into(), "5000".into()]);
            for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                // Rounds a minute apart, the candidate 20 s after the baseline.
                let at = 1.8e9 + 60.0 * (k / 2) as f64 + 20.0 * i as f64 + 6.0 * (k % 2) as f64;
                let reading = |at: f64, temp: f64, mhz: f64| Reading {
                    at,
                    loadavg_1m: Some(0.25),
                    temp_c: Some(temp),
                    freq_mhz: Some(Freq { min: mhz, mean: mhz, max: mhz }),
                };
                r.before = Some(reading(at, 55.0 + k as f64, 2000.0));
                r.after = Some(reading(at + 5.0, 70.0 + k as f64, 4250.0));
            }
        }
        let html = crate::html::report(&Comparison::build(files, 0));
        assert!(html.contains("<h2>Conditions</h2>"));
        assert!(html.contains(
            "<strong>Session started 2027-01-15 08:00 UTC</strong>: 5:31 from the first run to the last, 71–81 °C \
after a visit's hottest run, 4.25 GHz right after the runs, load average at most 0.25."
        ));
        assert!(!html.contains("Runs taken separately") && !html.contains("sessions; each is shown"));
        // Round 2 of the candidate: started 1:20 in, 57 °C before its
        // first run, 73 °C after its second.
        assert!(html.contains("<td>round 2</td>"));
        assert!(html.contains("<td><span class=m>+1:20</span><br>57→73 °C<br>4.25 GHz<br>load 0.25</td>"));
        // Shared by both files: one cell; per file where they differ.
        assert!(html.contains("<tr><td>Preemption</td><td class=l colspan=2>full</td></tr>"));
        assert!(
            html.contains(
                "<tr><td>Scheduler options</td><td class=\"l m\">—</td><td class=l>--slice-us 5000</td></tr>"
            )
        );
        // Nothing recorded, no row.
        assert!(!html.contains("Kernel build") && !html.contains("<td>Power</td>"));

        // The same two schedulers measured again a day later, compared
        // with the first session: two sessions, two tables, two clocks.
        // Round 2 of one is not laid next to round 2 of the other.
        let with_readings = |id: &str, shift: f64| {
            let mut files = drifting_session();
            for (i, f) in files.iter_mut().enumerate() {
                f.design.as_mut().unwrap().session_id = Some(id.into());
                for (k, r) in f.workloads[0].runs.iter_mut().enumerate() {
                    let at = 1.8e9 + shift + 60.0 * (k / 2) as f64 + 20.0 * i as f64 + 6.0 * (k % 2) as f64;
                    let reading =
                        |at: f64, temp: f64| Reading { at, loadavg_1m: None, temp_c: Some(temp), freq_mhz: None };
                    r.before = Some(reading(at, 50.0));
                    r.after = Some(reading(at + 5.0, 60.0 + shift / 86_400.0 * 20.0));
                }
            }
            files
        };
        let mut files = with_readings("monday", 0.0);
        files.extend(with_readings("tuesday", 86_400.0));
        // And a plain run from yet another day.
        let mut plain = file("by hand", &[100.0], 1);
        let at = 1.8e9 + 3.0 * 86_400.0;
        plain.workloads[0].runs[0].before = Some(Reading { at, loadavg_1m: Some(0.5), temp_c: None, freq_mhz: None });
        plain.workloads[0].runs[0].after =
            Some(Reading { at: at + 5.0, loadavg_1m: Some(0.75), temp_c: None, freq_mhz: None });
        files.push(plain);
        let html = crate::html::report(&Comparison::build(files, 0));
        let section = &html[html.find("<h2>Conditions</h2>").unwrap()..html.find("<h2>Change vs baseline").unwrap()];
        assert!(section.contains("The files come from 3 sessions; each is shown on its own"), "{section}");
        assert!(section.contains("<strong>Session started 2027-01-15 08:00 UTC</strong>: 5:31 from the first run"));
        assert!(section.contains("<strong>Session started 2027-01-16 08:00 UTC</strong>: 5:31 from the first run"));
        assert_eq!(section.matches("<td>round 2</td>").count(), 2, "one per session");
        // Each table has its own two schedulers, and its own temperatures.
        assert_eq!(section.matches("<th>base</th><th>cand</th>").count(), 1);
        assert_eq!(section.matches("<th>base #2</th><th>cand #2</th>").count(), 1);
        assert!(section.contains("50→60 °C") && section.contains("50→80 °C"));
        // Nothing counts from the other session's start: no "+24:01:20".
        assert!(!section.contains("+24:"), "{section}");
        assert!(
            section.contains("<tr><td>by hand</td><td>2027-01-18 08:00 UTC</td><td>0:05</td><td>load 0.75</td></tr>")
        );
    }

    /// What newer files record about the machine and the protocol is
    /// compared too, where both files have it; a file without a field is
    /// not "different".
    /// A period of periodic-wake that is a whole number of the kernel's
    /// ticks is warned about, once, with the results it is in; one that
    /// is not, or a file that cannot tell, is left alone.
    #[test]
    fn a_period_of_whole_ticks_is_warned_about() {
        let periodic = |label: &str, hz: Option<u32>, params: serde_json::Value| {
            let mut f = file(label, &[1.0], 1);
            f.system.tick_hz = hz;
            let mut loaded = f.workloads[0].clone();
            (f.workloads[0].name, loaded.name) = ("periodic-wake".into(), "periodic-wake/loaded".into());
            (f.workloads[0].params, loaded.params) = (params.clone(), params);
            f.workloads.push(loaded);
            f
        };
        let about = |files: Vec<ResultFile>| -> Vec<String> {
            let c = Comparison::build(files, 0);
            c.warnings.into_iter().filter(|w| w.contains("timer ticks")).collect()
        };
        let at = |us: u64| json!({ "tasks": 4, "period_us": us });

        // 1 ms with a tick of 1 ms, in both files: one warning for both
        // workloads and both results.
        let w = about(vec![periodic("a", Some(1000), at(1000)), periodic("b", Some(1000), at(1000))]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("periodic-wake, periodic-wake/loaded: the period of 1000 µs is a whole number"));
        assert!(w[0].contains("(1000 Hz) in a, b.") && w[0].contains("whatever each run happened to start with"));
        // What can happen, not what does: the ticks of the CPUs are not
        // in the file, and schedulers differ in how much they follow it.
        assert!(w[0].contains("nominal timer ticks") && w[0].contains("This can keep") && !w[0].contains("avoids"));
        // The suite's period is no multiple of any tick.
        for hz in [100, 250, 300, 1000] {
            assert!(about(vec![periodic("a", Some(hz), at(1010))]).is_empty(), "{hz}");
        }
        // 4 ms with a tick of 4 ms is, and so is 2 ms with one of 1 ms;
        // 1 ms with a tick of 4 ms is not.
        assert_eq!(about(vec![periodic("a", Some(250), at(4000))]).len(), 1);
        assert_eq!(about(vec![periodic("a", Some(1000), at(2000))]).len(), 1);
        assert!(about(vec![periodic("a", Some(250), at(1000))]).is_empty());
        // A tick of a third of 10 ms: 10 ms is three of them.
        assert_eq!(about(vec![periodic("a", Some(300), at(10_000))]).len(), 1);
        // No tick rate in the file, as in those of 0.5.0: nothing to say.
        assert!(about(vec![periodic("a", None, at(1000))]).is_empty());
        // Only the result that has it is named.
        let w = about(vec![periodic("a", None, at(1000)), periodic("b", Some(1000), at(1000))]);
        assert!(w.len() == 1 && w[0].contains("(1000 Hz) in b."), "{w:?}");
        // With a phase set the result is that phase's, which is said.
        let w = about(vec![periodic("a", Some(1000), json!({ "period_us": 1000, "phase_us": 150 }))]);
        assert!(w.len() == 1 && w[0].contains("Here the phase was set"), "{w:?}");
        // A product that does not fit 64 bits is still divided exactly.
        let huge = 1_000_000 * (u64::MAX / 1_000_000);
        assert_eq!(about(vec![periodic("a", Some(1000), at(huge))]).len(), 1);
        assert!(about(vec![periodic("a", Some(1000), at(huge + 1))]).is_empty());
        assert!(about(vec![periodic("a", Some(3), at(u64::MAX))]).is_empty());
        // Other workloads have periods too, and are not this.
        let mut burst = periodic("a", Some(1000), at(1000));
        burst.workloads.iter_mut().for_each(|w| w.name = w.name.replace("periodic-wake", "wake-burst"));
        assert!(about(vec![burst]).is_empty());
    }

    #[test]
    fn recorded_protocol_and_machine_differences_are_noted() {
        use helicoprion_core::{Protocol, Topology};
        let described = |label: &str| {
            let mut f = file(label, &[1.0], 1);
            f.system.kernel_build = Some("#1 SMP PREEMPT_DYNAMIC Mon, 16 Nov 2026".into());
            f.system.preempt = Some("full".into());
            f.system.tick_hz = Some(1000);
            f.system.cmdline = vec!["mitigations=auto".into()];
            f.system.topology = Some(Topology { packages: 1, cores: 8, l3_domains: Some(1), numa_nodes: Some(1) });
            f.system.cpufreq_driver = Some("amd-pstate-epp".into());
            f.system.allowed_cpus = Some("0-7".into());
            f.system.boost = Some(true);
            f.system.on_ac = Some(true);
            f.protocol = Some(Protocol { suite: "quick".into(), runs: 5, warmup: 1, duration_s: 5.0, cooldown_s: 1.0 });
            f
        };
        let notes = |base: ResultFile, other: ResultFile| Comparison::build(vec![base, other], 0).env_notes;
        assert_eq!(notes(described("a"), described("b")), Vec::<String>::new());
        // An older file next to a newer one: nothing to compare, no note.
        assert_eq!(notes(file("old", &[1.0], 1), described("new")), Vec::<String>::new());
        assert_eq!(notes(described("new"), file("old", &[1.0], 1)), Vec::<String>::new());

        let mut other = described("b");
        other.system.kernel_build = Some("#2 SMP PREEMPT_DYNAMIC Tue, 17 Nov 2026".into());
        other.system.preempt = Some("lazy".into());
        other.system.tick_hz = Some(250);
        other.system.cmdline = vec![];
        other.system.isolated_cpus = Some("2-3".into());
        // Eight CPUs as well, but other ones.
        other.system.allowed_cpus = Some("8-15".into());
        other.system.boost = Some(false);
        other.system.on_ac = Some(false);
        other.protocol.as_mut().unwrap().cooldown_s = 0.0;
        let n = notes(described("a"), other).join("\n");
        for expected in [
            "kernel build differs",
            "preemption model differs (lazy vs baseline full)",
            "timer tick differs (250 Hz vs baseline 1000 Hz)",
            "kernel parameters differs (none vs baseline mitigations=auto)",
            "isolated CPUs differs (2-3 vs baseline ?)",
            "CPUs the benchmark may use differs (8-15 vs baseline 0-7)",
            "frequency boost differs (off vs baseline on)",
            "power source differs (battery vs baseline mains)",
            "how the runs were taken differs (5 runs of 5 s, 1 warm-up, 0 s pause, suite quick vs baseline",
        ] {
            assert!(n.contains(expected), "{expected}\n{n}");
        }
        assert!(!n.contains("topology") && !n.contains("cpufreq driver"), "{n}");
    }

    #[test]
    fn undetectable_restart_warns() {
        let mut f = file("x", &[1.0], 1);
        f.restart_detectable = false;
        let c = Comparison::build(vec![f], 0);
        assert!(c.warnings.iter().any(|w| w.contains("enable_seq")));
    }

    #[test]
    fn duplicate_labels_are_made_unique() {
        let c = Comparison::build(vec![file("x", &[1.0], 1), file("x", &[1.0], 1)], 0);
        assert_eq!(c.series[1].label, "x #2");
    }

    #[test]
    fn diagnostics_are_summed() {
        let c = Comparison::build(vec![file("x", &[1.0, 2.0], 1)], 0);
        assert_eq!(c.diagnostics[0].counters["roundtrips"], 20);
        assert_eq!(c.diagnostics[0].valid_runs, 2);
    }
}
