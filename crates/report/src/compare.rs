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

use helicoprion_core::{Better, ResultFile, SystemInfo};

use crate::stats::{MIN_ROUNDS_FOR_CI, Spread, bootstrap_ci, bootstrap_median_ci, median};

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
    /// Paired only: half the IQR of the per-round changes, in percentage
    /// points; how consistent the effect is from round to round.
    pub paired_spread_pp: Option<f64>,
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

    /// The goodness ratio (like `ratio`) that a change of `change_pct`
    /// percent stands for; the inverse of `change()`. Monotonic, so an
    /// interval of changes maps onto an interval of ratios.
    pub fn ratio_of(&self, change_pct: f64) -> f64 {
        match self.better {
            Better::Lower => 1.0 / (1.0 - change_pct / 100.0),
            Better::Higher => 1.0 + change_pct / 100.0,
            Better::Neither => 1.0,
        }
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
            paired_spread_pp: None,
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
        let changes: Vec<f64> = common.iter().filter_map(|(bv, sv)| change(self.better, *bv, *sv)).collect();
        let Some(spread) = Spread::of(&changes) else {
            return Some(Delta {
                median: change(self.better, b.spread.median, s.spread.median)?,
                ci: None,
                ratio: goodness_ratio(self.better, b.spread.median, s.spread.median),
                paired_rounds: None,
                unmatched_rounds: true,
                paired_spread_pp: None,
                better: self.better,
            });
        };
        let ratios: Vec<f64> = common
            .iter()
            .map(|(bv, sv)| goodness_ratio(self.better, *bv, *sv))
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();
        Some(Delta {
            median: spread.median,
            // `bootstrap_median_ci` returns None below MIN_ROUNDS_FOR_CI.
            ci: bootstrap_median_ci(&changes),
            ratio: median(&ratios).unwrap_or(1.0),
            paired_rounds: Some(changes.len()),
            unmatched_rounds: false,
            paired_spread_pp: Some((spread.q3 - spread.q1) / 2.0),
            better: self.better,
        })
    }

    /// The comparison is too noisy to score. Unpaired: either side's
    /// run-to-run spread is above the threshold. Paired: the per-round
    /// changes disagree by more than the threshold (raw run-to-run spread
    /// there is mostly drift between rounds, which pairing removes).
    pub fn unstable_pair(&self, baseline: usize, i: usize) -> bool {
        if let Some(pp) = self.improvement(baseline, i).and_then(|d| d.paired_spread_pp) {
            return pp > self.unstable_spread_pct;
        }
        [baseline, i].iter().any(|k| self.cells.get(*k).and_then(Option::as_ref).is_some_and(|c| c.unstable))
    }

    /// Series `k`'s cell comes from a population that holds less than
    /// `min_share_pct` of its run's events.
    pub fn minor(&self, k: usize) -> bool {
        self.cells.get(k).and_then(Option::as_ref).and_then(|c| c.share).is_some_and(|s| s * 100.0 < self.min_share_pct)
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
}

/// Diagnostic counters summed over the valid runs of one workload.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    pub series: usize,
    pub workload: String,
    pub valid_runs: usize,
    pub counters: BTreeMap<String, u64>,
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
}

impl Comparison {
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
        let mut diagnostics = Vec::new();

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
                    diagnostics.push(Diagnostics { series: si, workload: w.name.clone(), valid_runs, counters });
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
                            .filter_map(|r| {
                                let m = r.metrics.get(&metric)?;
                                m.share.or_else(|| legacy_share(&metric, &r.counters))
                            })
                            .filter(|v| v.is_finite())
                            .collect();
                        let spread = Spread::of(&values)?;
                        Some(Cell {
                            unstable: spread.n >= 2 && spread.rel_iqr_pct() / 2.0 > opts.unstable_spread_pct,
                            spread,
                            values,
                            total_runs: w.runs.len(),
                            comparable: units_ok && base_params.is_none_or(|p| *p == w.params),
                            session: session_key(&s.file),
                            rounds,
                            share: median(&shares),
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
            match deltas.iter().filter_map(|d| d.paired_rounds).min() {
                Some(n) if n < MIN_ROUNDS_FOR_CI => warnings.push(format!(
                    "{}: paired with the baseline over only {n} round(s); point estimates only, no intervals (use >= {MIN_ROUNDS_FOR_CI} rounds)",
                    ser.label
                )),
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

        Comparison { series, baseline, opts, rows, distributions, diagnostics, warnings, env_notes }
    }
}

/// The interleaved session a file belongs to, if it came from `matrix`.
///
/// Uses the session's random id; files written before it existed fall back
/// to start time (seconds) plus scheduler list, which two sessions started
/// in the same second with the same list would share.
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

pub fn session_key(f: &ResultFile) -> Option<SessionKey> {
    let d = f.design.as_ref().filter(|d| d.kind == "interleaved")?;
    Some(SessionKey(match &d.session_id {
        Some(id) => format!("id:{id}"),
        None => format!("legacy:{}:{}", f.started_at, d.schedulers.join("\u{1f}")),
    }))
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
    curves.iter().any(Option::is_some).then(|| Distribution {
        workload: workload.to_string(),
        population: population.to_string(),
        unit: reference,
        curves,
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
    use helicoprion_core::{Metric, RunResult, Scheduler, WorkloadResult};
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
            system: SystemInfo {
                kernel: "6.18".into(),
                cpu_model: None,
                usable_cpus: 4,
                online_cpus: None,
                smt_active: None,
                governor: None,
                epp: None,
                platform_profile: None,
                thp: None,
                loadavg_1m: None,
            },
            scheduler: Scheduler::Fair { class: "EEVDF".into(), scx_capable: false, bore: None },
            restart_detectable: true,
            design: None,
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

    /// `matrix --rounds 2 --runs-per-visit 5`: ten runs per scheduler but
    /// only two independent rounds. No per-run bootstrap (pseudoreplication):
    /// a paired point estimate, no interval, never highlighted.
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
        assert!(c.warnings.iter().any(|w| w.contains("only 2 round")));
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
