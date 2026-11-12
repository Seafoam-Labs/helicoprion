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

use crate::stats::{Spread, bootstrap_ci};

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
}

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
}

impl Default for Options {
    fn default() -> Self {
        Options { min_effect_pct: 5.0, unstable_spread_pct: 10.0 }
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
}

#[derive(Debug, Clone)]
pub struct MetricRow {
    pub workload: String,
    pub metric: String,
    pub unit: String,
    pub better: Better,
    /// Single-sample extreme (e.g. max): table only.
    pub extreme: bool,
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
        let median = change(self.better, b.spread.median, s.spread.median)?;
        let ci = bootstrap_ci(&b.values, &s.values, |mb, mc| change(self.better, mb, mc));
        let ratio = goodness_ratio(self.better, b.spread.median, s.spread.median);
        Some(Delta { median, ci, ratio })
    }

    /// Either side of the comparison is unstable.
    pub fn unstable_pair(&self, baseline: usize, i: usize) -> bool {
        [baseline, i].iter().any(|k| self.cells.get(*k).and_then(Option::as_ref).is_some_and(|c| c.unstable))
    }

    /// Whether series `i` belongs on the change chart: a scored metric that
    /// is not a single-sample extreme, comparable, and stable on both sides.
    pub fn chartable(&self, baseline: usize, i: usize) -> bool {
        !self.extreme
            && self.better != Better::Neither
            && !self.unstable_pair(baseline, i)
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
                if si != baseline {
                    if let Some(bw) = base.file.workloads.iter().find(|b| b.name == w.name) {
                        if bw.params != w.params {
                            warnings.push(format!(
                                "{}: {} ran with {} but the baseline used {}; not compared",
                                s.label, w.name, w.params, bw.params
                            ));
                        }
                    }
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
                        let ms: Vec<_> =
                            w.runs.iter().filter(|r| r.valid).filter_map(|r| r.metrics.get(&metric)).collect();
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
                        let spread = Spread::of(&values)?;
                        Some(Cell {
                            unstable: spread.n >= 2 && spread.rel_iqr_pct() / 2.0 > opts.unstable_spread_pct,
                            spread,
                            values,
                            total_runs: w.runs.len(),
                            comparable: units_ok && base_params.is_none_or(|p| *p == w.params),
                        })
                    })
                    .collect();
                MetricRow { workload, metric, unit, better, extreme, cells }
            })
            .collect::<Vec<_>>();

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
