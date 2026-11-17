//! Workloads are written in-tree: no external benchmark programs, no
//! downloaded assets. Each one stresses a specific scheduler behaviour.

pub mod burst;
pub mod cgroup;
mod clock;
mod futex;
pub mod hog;
pub mod periodic;
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
    let pingpong = |hogs: usize| -> Box<dyn Workload> {
        Box::new(wake_pingpong::WakePingPong { pairs, hogs, cpu_diag: opts.cpu_diag })
    };
    // A few periodic tasks whatever the machine: the question is how late
    // each wakes, not how many fit. 1 ms and 100 µs: a tenth of a CPU per
    // task, and 20000 periods in a five-second run of four tasks, enough
    // for a p99.9.
    let periodic = |hogs: usize| -> Box<dyn Workload> {
        Box::new(periodic::PeriodicWake {
            tasks: (cpus / 4).clamp(1, 4),
            period: std::time::Duration::from_millis(1),
            work: std::time::Duration::from_micros(100),
            hogs,
        })
    };
    // A group woken at once, in three sizes relative to the machine: it
    // fits with room to spare, it fits exactly, it does not fit. 2 ms and
    // 200 µs: a tenth of a CPU per task, and 2500 bursts in a five-second
    // run, enough for a p99 of the per-burst times.
    let burst = |size: &'static str, tasks: usize| -> Box<dyn Workload> {
        Box::new(burst::WakeBurst {
            size,
            tasks: tasks.max(1),
            period: std::time::Duration::from_millis(2),
            work: std::time::Duration::from_micros(200),
        })
    };
    let bursts = || vec![burst("half", cpus / 2), burst("full", cpus), burst("double", cpus * 2)];
    // Two groups that could each use the whole machine. Once the same
    // weight and three times the threads, once the same threads and three
    // times the weight: a scheduler that treats every thread alike fails
    // the first, one that knows groups and ignores weights the second.
    let cgroups = || -> Vec<Box<dyn Workload>> {
        let n = cpus.max(1);
        vec![
            Box::new(cgroup::CgroupFair { case: "equal", weights: (100, 100), threads: (n, 3 * n), cpus: n }),
            Box::new(cgroup::CgroupFair { case: "weighted", weights: (100, 300), threads: (n, n), cpus: n }),
        ]
    };
    match name {
        "quick" => Some(vec![pingpong(0), pingpong(cpus)]),
        "periodic" => Some(vec![periodic(0), periodic(cpus)]),
        "bursts" => Some(bursts()),
        "cgroups" => Some(cgroups()),
        "all" => {
            let mut all = vec![pingpong(0), pingpong(cpus), periodic(0), periodic(cpus)];
            all.extend(bursts());
            all.extend(cgroups());
            Some(all)
        }
        _ => None,
    }
}

/// `quick` is what it has always been, so that results stay comparable
/// with earlier ones; `periodic` is the timer-driven workload alone,
/// `bursts` the group wake-ups alone, `cgroups` the split between control
/// groups alone, and `all` runs everything.
pub const SUITES: &[&str] = &["quick", "periodic", "bursts", "cgroups", "all"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suites_hold_what_they_say() {
        let names = |suite: &str, cpus: usize| -> Vec<String> {
            super::suite(suite, cpus, SuiteOpts::default()).unwrap().iter().map(|w| w.name()).collect()
        };
        // `quick` must not change: results are compared with older ones.
        assert_eq!(names("quick", 16), ["wake-pingpong", "wake-pingpong/loaded"]);
        assert_eq!(names("periodic", 16), ["periodic-wake", "periodic-wake/loaded"]);
        assert_eq!(names("bursts", 16), ["wake-burst/half", "wake-burst/full", "wake-burst/double"]);
        assert_eq!(names("cgroups", 16), ["cgroup-fair/equal", "cgroup-fair/weighted"]);
        assert_eq!(names("all", 16).len(), 9);
        assert_eq!(names("all", 16)[..4], [names("quick", 16), names("periodic", 16)].concat());
        // Sizes follow the machine, and a single CPU still has a task.
        let tasks = |cpus: usize| -> Vec<u64> {
            let suite = super::suite("bursts", cpus, SuiteOpts::default()).unwrap();
            suite.iter().map(|w| w.params()["tasks"].as_u64().unwrap()).collect()
        };
        assert_eq!(tasks(16), [8, 16, 32]);
        assert_eq!(tasks(1), [1, 1, 2]);
        // Both groups can use every CPU; what differs is one thing each.
        let groups = super::suite("cgroups", 16, SuiteOpts::default()).unwrap();
        let p = |k: usize, name: &str| groups[k].params()[name].as_u64().unwrap();
        assert_eq!([p(0, "weight_a"), p(0, "weight_b"), p(0, "threads_a"), p(0, "threads_b")], [100, 100, 16, 48]);
        assert_eq!([p(1, "weight_a"), p(1, "weight_b"), p(1, "threads_a"), p(1, "threads_b")], [100, 300, 16, 16]);
        for name in SUITES {
            assert!(super::suite(name, 2, SuiteOpts::default()).is_some(), "{name}");
        }
        assert!(super::suite("nope", 2, SuiteOpts::default()).is_none());
        // The loaded variant keeps every CPU busy; the tasks stay few.
        let w = super::suite("periodic", 16, SuiteOpts::default()).unwrap();
        assert_eq!(w[1].params()["hogs"], 16);
        assert_eq!((w[0].params()["tasks"].clone(), w[0].params()["hogs"].clone()), (4.into(), 0.into()));
        let small = super::suite("periodic", 2, SuiteOpts::default()).unwrap();
        assert_eq!(small[0].params()["tasks"], 1);
    }
}
