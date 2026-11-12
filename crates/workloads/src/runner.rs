//! Runs workloads repeatedly and binds every run to the session's scheduler.
//!
//! A run is valid only if the scheduler read immediately before *and* after
//! it equals `RunConfig::expected` (the scheduler recorded in the result
//! file). That catches a crash/restart during a run (ops or `enable_seq`
//! change) as well as a scheduler swapped in between runs.

use std::time::Duration;

use helicoprion_core::{RunResult, Scheduler, WorkloadResult};

use crate::Workload;

pub struct RunConfig {
    /// The scheduler this session claims to measure.
    pub expected: Scheduler,
    pub runs: usize,
    pub warmup: usize,
    pub duration: Duration,
    /// Pause between runs to let clocks and caches settle.
    pub cooldown: Duration,
}

pub enum Event<'a> {
    Start { workload: &'a str, run: usize, total: usize, warmup: bool },
    Done { workload: &'a str, run: usize, warmup: bool, valid: bool, summary: String },
}

/// Mark `r` valid or invalid against the expected scheduler.
pub fn judge(r: &mut RunResult, expected: &Scheduler, before: Scheduler, after: Scheduler) {
    if r.invalid_reason.is_none() {
        r.invalid_reason = if before != *expected {
            Some(format!("scheduler before run was {before}, session expects {expected}"))
        } else if after != before {
            Some(format!("scheduler changed during run: {before} -> {after}"))
        } else {
            None
        };
    }
    r.valid = r.invalid_reason.is_none();
    r.sched_before = Some(before);
    r.sched_after = Some(after);
}

pub fn run_workload(w: &dyn Workload, cfg: &RunConfig, mut on: impl FnMut(Event<'_>)) -> WorkloadResult {
    let name = w.name();
    let mut runs = Vec::with_capacity(cfg.runs);

    for i in 0..cfg.warmup + cfg.runs {
        let warmup = i < cfg.warmup;
        let idx = if warmup { i } else { i - cfg.warmup };
        on(Event::Start { workload: &name, run: idx, total: cfg.runs, warmup });

        let before = Scheduler::detect();
        let mut r = w.run(cfg.duration);
        let after = Scheduler::detect();
        judge(&mut r, &cfg.expected, before, after);

        let summary = match &r.invalid_reason {
            Some(why) => format!("INVALID: {why}"),
            None => {
                let keys = w.headline_metrics();
                r.metrics
                    .iter()
                    .filter(|(k, _)| keys.is_empty() || keys.contains(&k.as_str()))
                    .take(if keys.is_empty() { 3 } else { keys.len() })
                    .map(|(k, m)| format!("{k}={} {}", short(m.value), m.unit))
                    .chain(w.progress_note(&r))
                    .collect::<Vec<_>>()
                    .join("  ")
            }
        };
        on(Event::Done { workload: &name, run: idx, warmup, valid: r.valid, summary });
        if !warmup {
            runs.push(r);
        }
        std::thread::sleep(cfg.cooldown);
    }

    WorkloadResult { name, description: w.description(), params: w.params(), runs }
}

fn short(v: f64) -> String {
    if v.abs() >= 1e4 { format!("{:.0}k", v / 1e3) } else { format!("{v:.1}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eevdf() -> Scheduler {
        Scheduler::Fair { class: "EEVDF".into(), scx_capable: true, bore: None }
    }
    fn lavd(seq: u64) -> Scheduler {
        Scheduler::Ext { name: "lavd".into(), ops: "lavd_1.0".into(), enable_seq: Some(seq) }
    }

    #[test]
    fn swapped_between_runs_is_invalid() {
        // Session started on EEVDF, someone attached lavd before this run.
        let mut r = RunResult::default();
        judge(&mut r, &eevdf(), lavd(1), lavd(1));
        assert!(!r.valid);
        assert!(r.invalid_reason.unwrap().contains("session expects"));
    }

    #[test]
    fn restart_during_run_is_invalid() {
        let mut r = RunResult::default();
        judge(&mut r, &lavd(1), lavd(1), lavd(2));
        assert!(!r.valid);
    }

    #[test]
    fn steady_is_valid_and_recorded() {
        let mut r = RunResult::default();
        judge(&mut r, &lavd(3), lavd(3), lavd(3));
        assert!(r.valid);
        assert_eq!(r.sched_before, Some(lavd(3)));
        assert_eq!(r.sched_after, Some(lavd(3)));
    }

    #[test]
    fn workload_error_stays_invalid() {
        let mut r = RunResult { invalid_reason: Some("boom".into()), ..Default::default() };
        judge(&mut r, &eevdf(), eevdf(), eevdf());
        assert!(!r.valid);
        assert_eq!(r.invalid_reason.as_deref(), Some("boom"));
    }
}
