//! Workloads are written in-tree: no external benchmark programs, no
//! downloaded assets. Each one stresses a specific scheduler behaviour.

mod futex;
pub mod hog;
pub mod runner;
pub mod samples;
pub mod wake_pingpong;

use std::time::Duration;

use helicoprion_core::RunResult;

pub trait Workload: Send + Sync {
    /// Stable identifier used in result files, e.g. `wake-pingpong`.
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn params(&self) -> serde_json::Value;
    /// Metrics worth printing in progress output; empty means "first three".
    fn headline_metrics(&self) -> &'static [&'static str] {
        &[]
    }
    /// Extra text for the progress line (e.g. a diagnostic share).
    fn progress_note(&self, _r: &RunResult) -> Option<String> {
        None
    }
    /// Run once for roughly `duration`. `valid` is filled in by the runner.
    fn run(&self, duration: Duration) -> RunResult;
}

/// Named sets of workloads.
/// Options that change how a suite measures.
#[derive(Debug, Clone, Copy)]
pub struct SuiteOpts {
    /// Same-/cross-CPU diagnostics in wake-pingpong.
    pub cpu_diag: bool,
}

impl Default for SuiteOpts {
    fn default() -> Self {
        SuiteOpts { cpu_diag: true }
    }
}

pub fn suite(name: &str, cpus: usize, opts: SuiteOpts) -> Option<Vec<Box<dyn Workload>>> {
    let pairs = (cpus / 2).max(1);
    match name {
        "quick" => Some(vec![
            Box::new(wake_pingpong::WakePingPong { pairs, hogs: 0, cpu_diag: opts.cpu_diag }),
            Box::new(wake_pingpong::WakePingPong { pairs, hogs: cpus, cpu_diag: opts.cpu_diag }),
        ]),
        _ => None,
    }
}

pub const SUITES: &[&str] = &["quick"];
