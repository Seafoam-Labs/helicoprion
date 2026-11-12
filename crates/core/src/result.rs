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
pub const SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Lower,
    Higher,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub value: f64,
    pub unit: String,
    pub better: Better,
}

impl Metric {
    pub fn lower(value: f64, unit: &str) -> Self {
        Metric { value, unit: unit.into(), better: Better::Lower }
    }
    pub fn higher(value: f64, unit: &str) -> Self {
        Metric { value, unit: unit.into(), better: Better::Higher }
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
    pub workloads: Vec<WorkloadResult>,
}
