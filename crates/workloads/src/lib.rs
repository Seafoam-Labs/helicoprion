//! Workloads are written in-tree: no external benchmark programs, no
//! downloaded assets. Each one stresses a specific scheduler behaviour.

pub mod burst;
pub mod cgroup;
mod clock;
mod futex;
pub mod hog;
mod idle;
pub mod periodic;
pub mod runner;
pub mod samples;
mod schedstat;
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
    /// Every worker thread's scheduler statistics, in all workloads.
    pub sched_diag: bool,
    /// Another period for periodic-wake than `PERIODIC_PERIOD`.
    pub periodic_period: Option<Duration>,
    /// Where periodic-wake's releases lie on the clock (see
    /// `periodic::PeriodicWake::phase`).
    pub periodic_phase: Option<Duration>,
}

/// periodic-wake's period unless another is asked for, and its work.
///
/// 1010 µs and not 1 ms: a period that is a whole number of the kernel's
/// nominal timer ticks can keep its releases at one phase against the
/// tick for a whole run, and where that was measured the phase moved one
/// scheduler's missed deadlines more than tenfold and others' hardly
/// (METHODOLOGY.md, "The period and the kernel's tick"). 1010 µs is a
/// multiple of none of the usual ticks and returns to the same phase
/// against their nominal grid after about 0.1 to 1.01 s: 100 periods at
/// 1000 Hz, 400 at 250 Hz, 1000 at 300 and at 100 Hz. A run of five
/// seconds samples the phases about five times over or more; the tick is
/// still in the result, as many phases instead of one.
pub const PERIODIC_PERIOD: Duration = Duration::from_micros(1010);
pub const PERIODIC_WORK: Duration = Duration::from_micros(100);

impl Default for SuiteOpts {
    fn default() -> Self {
        SuiteOpts { cpu_diag: true, sched_diag: false, periodic_period: None, periodic_phase: None }
    }
}

pub fn suite(name: &str, cpus: usize, opts: SuiteOpts) -> Option<Vec<Box<dyn Workload>>> {
    let pairs = (cpus / 2).max(1);
    let pingpong = |hogs: usize| -> Box<dyn Workload> {
        Box::new(wake_pingpong::WakePingPong { pairs, hogs, cpu_diag: opts.cpu_diag, sched_diag: opts.sched_diag })
    };
    // A few periodic tasks whatever the machine: the question is how late
    // each wakes, not how many fit. About 1 ms and 100 µs: a tenth of a
    // CPU per task, and 19800 periods in a five-second run of four tasks,
    // enough for a p99.9.
    let periodic = |hogs: usize| -> Box<dyn Workload> {
        Box::new(periodic::PeriodicWake {
            tasks: (cpus / 4).clamp(1, 4),
            period: opts.periodic_period.unwrap_or(PERIODIC_PERIOD),
            work: PERIODIC_WORK,
            hogs,
            sched_diag: opts.sched_diag,
            phase: opts.periodic_phase,
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
            sched_diag: opts.sched_diag,
            shallow_idle: true,
        })
    };
    let bursts = || vec![burst("half", cpus / 2), burst("full", cpus), burst("double", cpus * 2)];
    // Two groups that could each use the whole machine. Once the same
    // weight and three times the threads, once the same threads and three
    // times the weight: a scheduler that treats every thread alike fails
    // the first, one that knows groups and ignores weights the second.
    let cgroups = || -> Vec<Box<dyn Workload>> {
        let (n, sched_diag) = (cpus.max(1), opts.sched_diag);
        vec![
            Box::new(cgroup::CgroupFair {
                case: "equal",
                weights: (100, 100),
                threads: (n, 3 * n),
                cpus: n,
                sched_diag,
            }),
            Box::new(cgroup::CgroupFair {
                case: "weighted",
                weights: (100, 300),
                threads: (n, n),
                cpus: n,
                sched_diag,
            }),
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
        // Scheduler statistics are off unless asked for, and a workload
        // that reads them says so in its parameters, all of them alike.
        for (on, marked) in [(false, None), (true, Some(true))] {
            let opts = SuiteOpts { sched_diag: on, ..SuiteOpts::default() };
            for w in super::suite("all", 4, opts).unwrap() {
                assert_eq!(w.params().get("sched_diag").and_then(|v| v.as_bool()), marked, "{}", w.name());
            }
        }
        // The loaded variant keeps every CPU busy; the tasks stay few.
        let w = super::suite("periodic", 16, SuiteOpts::default()).unwrap();
        assert_eq!(w[1].params()["hogs"], 16);
        assert_eq!((w[0].params()["tasks"].clone(), w[0].params()["hogs"].clone()), (4.into(), 0.into()));
        let small = super::suite("periodic", 2, SuiteOpts::default()).unwrap();
        assert_eq!(small[0].params()["tasks"], 1);
        // Period and phase are what they were unless asked for, and then
        // only periodic-wake has them.
        assert_eq!((w[0].params()["period_us"].clone(), w[0].params().get("phase_us")), (1010.into(), None));
        let opts = SuiteOpts {
            periodic_period: Some(Duration::from_micros(1000)),
            periodic_phase: Some(Duration::from_micros(300)),
            ..SuiteOpts::default()
        };
        for w in super::suite("all", 16, opts).unwrap() {
            let (p, periodic) = (w.params(), w.name().starts_with("periodic-wake"));
            assert_eq!(p.get("phase_us").and_then(|v| v.as_u64()), periodic.then_some(300), "{}", w.name());
            assert_eq!(p["period_us"] == 1000, periodic, "{}", w.name());
        }
    }
}
