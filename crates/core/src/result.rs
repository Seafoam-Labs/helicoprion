//! On-disk result format (JSON). One file = one benchmark session on one
//! scheduler; `compare` takes several of them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hist::Histogram;
use crate::{Scheduler, SystemInfo};

/// 2: per-run scheduler readings, latency histogram, diagnostic counters,
///    cpufreq summaries over all policies.
/// 3: latency histograms and quantile curves per population (e.g.
///    `rtt_blocked` / `rtt_fast`) instead of a single mixed distribution.
/// 4: `Better::Neither` (informational metrics), `Metric::extreme`,
///    `RunResult::visit`/`round`, `ResultFile::design`. Schema 3 files still read.
///    `Metric::share` was added later as an optional field; files without it
///    are still schema 4.
pub const SCHEMA_VERSION: u32 = 4;
/// Oldest schema `compare` can read.
pub const MIN_READABLE_SCHEMA: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Lower,
    Higher,
    /// Informational: no direction is "better" (e.g. how CPU time was split
    /// between competing threads). Shown, never scored.
    Neither,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub value: f64,
    pub unit: String,
    pub better: Better,
    /// A single-sample extreme (e.g. max). Too noisy to chart or score;
    /// kept in tables.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extreme: bool,
    /// For a metric of one population of events (e.g. blocked round trips):
    /// the fraction (0–1) of all the run's events that population holds.
    /// `None` for metrics over everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
}

impl Metric {
    pub fn lower(value: f64, unit: &str) -> Self {
        Metric { value, unit: unit.into(), better: Better::Lower, extreme: false, share: None }
    }
    pub fn higher(value: f64, unit: &str) -> Self {
        Metric { value, unit: unit.into(), better: Better::Higher, extreme: false, share: None }
    }
    pub fn info(value: f64, unit: &str) -> Self {
        Metric { value, unit: unit.into(), better: Better::Neither, extreme: false, share: None }
    }
    /// Mark as a single-sample extreme.
    pub fn extreme(mut self) -> Self {
        self.extreme = true;
        self
    }
    /// Computed from a population holding `share` (0–1) of the run's events.
    pub fn share(mut self, share: f64) -> Self {
        self.share = Some(share);
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunResult {
    /// True only if the scheduler matched the session's expected scheduler
    /// both before and after the run, and the workload reported no error.
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    /// Scheduler read immediately before / after the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sched_before: Option<Scheduler>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sched_after: Option<Scheduler>,
    /// Index of the scheduler visit this run belongs to in an interleaved
    /// (`matrix`) session; absent for plain `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit: Option<u32>,
    /// Round of an interleaved session (every scheduler is visited once per
    /// round), for paired analysis; absent for plain `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Length of the measurement window (excludes thread setup/teardown).
    pub duration_s: f64,
    /// Actual boundaries of each measurement window, in seconds relative
    /// to the run's common start instant (e.g. `latency`, `hog`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub windows: BTreeMap<String, Window>,
    /// Compared across files.
    pub metrics: BTreeMap<String, Metric>,
    /// Diagnostics (not compared), e.g. how many waits really blocked.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, u64>,
    /// Full latency distribution of each population measured by the run,
    /// keyed by population name (e.g. `rtt_blocked`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub latency_histograms: BTreeMap<String, Histogram>,
    /// Quantile function per population, (quantile, value in
    /// `quantile_unit`), derived from `latency_histograms`. Only quantiles
    /// the sample count supports are included.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quantile_curves: BTreeMap<String, Vec<(f64, f64)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantile_unit: Option<String>,
}

/// A measurement interval relative to the run's common start (may start
/// slightly before 0 when a snapshot is taken just before the start).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub start_s: f64,
    pub end_s: f64,
}

/// An interleaved session: schedulers were switched between visits so that
/// slow drift (thermal, background state) affects every scheduler alike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Design {
    /// e.g. `interleaved`
    pub kind: String,
    pub rounds: u32,
    /// Every scheduler spec of the session, in first-round order.
    pub schedulers: Vec<String>,
    /// The spec this file holds results for.
    pub spec: String,
    /// Random identifier shared by every file of one session; pairing by
    /// round requires equal ids. Files without it (written before the field
    /// existed) are identified by start time and scheduler list instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadResult {
    pub name: String,
    pub description: String,
    /// Workload parameters; results are only comparable when these match.
    pub params: serde_json::Value,
    pub runs: Vec<RunResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub tool_version: String,
    pub label: String,
    /// Unix seconds.
    pub started_at: u64,
    pub system: SystemInfo,
    /// The scheduler every run was required to execute on.
    pub scheduler: Scheduler,
    /// False when a restart of the same scheduler could go unnoticed
    /// (sched_ext without `enable_seq`).
    pub restart_detectable: bool,
    /// How the session was run; absent means a plain `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<Design>,
    pub workloads: Vec<WorkloadResult>,
}
