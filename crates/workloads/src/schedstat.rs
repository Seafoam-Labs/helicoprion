//! What the kernel counted for each worker thread while it worked: how
//! long it ran, how long it waited for a CPU, how often it was switched
//! out and whether it gave the CPU up or had it taken, and how often it
//! changed CPU.
//!
//! The workloads measure what a task experiences, a latency or a share.
//! These counters say something about how the scheduler got there: a
//! long tail with tasks that waited on a run queue is another thing
//! than one with tasks that were preempted in the middle of their work,
//! or one with tasks that were moved from CPU to CPU.
//!
//! Each thread reads its own counters from `/proc/thread-self` when its
//! work begins and when it ends, outside of what is timed, and the
//! differences are summed over the threads. No tracing and no rights are
//! needed. The readings are not free, though, and they are taken by the
//! threads that are measured, so they are off unless asked for
//! (`--sched-diag`), and a run with them has another workload parameter
//! than one without: the two are never compared with each other.
//!
//! * `schedstat`: nanoseconds on a CPU, nanoseconds runnable and waiting
//!   for one, and the number of times the thread was given a CPU;
//! * `status`: voluntary switches (the thread slept or yielded) and
//!   involuntary ones (the CPU was taken from it);
//! * `sched`: `se.nr_migrations`, where the kernel provides the file.
//!
//! Everything derived from them is informational. They describe a
//! mechanism, and no mechanism is better as such: fewer migrations are
//! good where they keep caches warm and bad where they leave a task
//! waiting behind another.

use std::sync::Mutex;
use std::time::Instant;

use helicoprion_core::{Metric, RunResult};

/// One thread's counters at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stats {
    run_ns: u64,
    wait_ns: u64,
    slices: u64,
    voluntary: u64,
    involuntary: u64,
    /// `None` where the kernel does not say.
    migrations: Option<u64>,
}

/// `run_ns wait_ns slices`, the content of `schedstat`.
fn parse_schedstat(text: &str) -> Option<(u64, u64, u64)> {
    let mut n = text.split_whitespace().map(|v| v.parse::<u64>().ok());
    Some((n.next()??, n.next()??, n.next()??))
}

/// The number after `name` and a colon, on a line of its own.
fn field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == name).then(|| value.trim().parse().ok())?
    })
}

fn read() -> Result<Stats, String> {
    let file = |name: &str| {
        std::fs::read_to_string(format!("/proc/thread-self/{name}"))
            .map_err(|e| format!("/proc/thread-self/{name}: {e}"))
    };
    let (run_ns, wait_ns, slices) = parse_schedstat(&file("schedstat")?)
        .ok_or("/proc/thread-self/schedstat does not hold three numbers (a kernel without scheduler statistics?)")?;
    let status = file("status")?;
    let switches = |name: &str| field(&status, name).ok_or(format!("/proc/thread-self/status has no {name}"));
    Ok(Stats {
        run_ns,
        wait_ns,
        slices,
        voluntary: switches("voluntary_ctxt_switches")?,
        involuntary: switches("nonvoluntary_ctxt_switches")?,
        // Not every kernel has the file; the rest stands without it.
        migrations: file("sched").ok().and_then(|sched| field(&sched, "se.nr_migrations")),
    })
}

/// What one thread's counters grew by while it was watched.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Usage {
    stats: Stats,
    secs: f64,
}

fn grown(from: &Stats, to: &Stats, secs: f64) -> Usage {
    let d = |a: u64, b: u64| b.saturating_sub(a);
    Usage {
        stats: Stats {
            run_ns: d(from.run_ns, to.run_ns),
            wait_ns: d(from.wait_ns, to.wait_ns),
            slices: d(from.slices, to.slices),
            voluntary: d(from.voluntary, to.voluntary),
            involuntary: d(from.involuntary, to.involuntary),
            migrations: from.migrations.zip(to.migrations).map(|(a, b)| d(a, b)),
        },
        secs,
    }
}

/// A workload's parameters with the mark of a run that read these
/// counters. Only when it did, so that a run without them compares with
/// results recorded before the option existed.
pub(crate) fn mark(mut params: serde_json::Value, on: bool) -> serde_json::Value {
    if on {
        params["sched_diag"] = true.into();
    }
    params
}

/// Collects what the worker threads of one run report.
pub(crate) struct Collector {
    on: bool,
    threads: Mutex<Vec<Result<Usage, String>>>,
}

impl Collector {
    pub(crate) fn new(on: bool) -> Self {
        Collector { on, threads: Mutex::new(Vec::new()) }
    }

    /// Start watching the calling thread; the counters are read again
    /// when the result is dropped, however the thread leaves its work.
    /// Does nothing where the collector is off.
    pub(crate) fn watch(&self) -> Watching<'_> {
        Watching { to: self, from: self.on.then(|| (read(), Instant::now())) }
    }

    /// Add what the threads reported to `r`, as informational metrics
    /// and counters. A thread whose counters could not be read makes
    /// the run invalid: they were asked for.
    pub(crate) fn record(&self, r: &mut RunResult) {
        if !self.on || r.invalid_reason.is_some() {
            return;
        }
        let threads = std::mem::take(&mut *self.threads.lock().unwrap_or_else(|e| e.into_inner()));
        let mut usages = Vec::with_capacity(threads.len());
        for t in threads {
            match t {
                Ok(u) => usages.push(u),
                Err(why) => {
                    r.invalid_reason =
                        Some(format!("scheduler statistics were asked for and could not be read: {why}"));
                    return;
                }
            }
        }
        // A kernel without scheduler statistics (CONFIG_SCHED_INFO)
        // still has the file, and it reads "0 0 0". Three numbers, and
        // nothing counted. Only known after the work: a thread that has
        // just started has zeros on any kernel.
        if !usages.is_empty() && usages.iter().all(|u| u.stats.run_ns == 0 && u.stats.slices == 0) {
            r.invalid_reason = Some(
                "scheduler statistics were asked for and this kernel does not keep them: no thread has any time on \
                 a CPU in /proc/thread-self/schedstat after its work (CONFIG_SCHED_INFO?)"
                    .into(),
            );
            return;
        }
        summarize(r, &usages);
    }
}

pub(crate) struct Watching<'a> {
    to: &'a Collector,
    from: Option<(Result<Stats, String>, Instant)>,
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        if let Some((from, at)) = self.from.take() {
            let usage = from.and_then(|from| Ok(grown(&from, &read()?, at.elapsed().as_secs_f64())));
            self.to.threads.lock().unwrap_or_else(|e| e.into_inner()).push(usage);
        }
    }
}

fn summarize(r: &mut RunResult, usages: &[Usage]) {
    if usages.is_empty() {
        return;
    }
    let sum = |f: fn(&Stats) -> u64| usages.iter().map(|u| f(&u.stats)).sum::<u64>();
    let (run, wait, slices) = (sum(|s| s.run_ns), sum(|s| s.wait_ns), sum(|s| s.slices));
    let (voluntary, involuntary) = (sum(|s| s.voluntary), sum(|s| s.involuntary));
    // Only if every thread has them: a sum over some would be too low.
    let migrations = usages.iter().map(|u| u.stats.migrations).sum::<Option<u64>>();
    // How long a thread was watched, on average: what "per second" means
    // for counts summed over threads that all ran side by side.
    let secs = usages.iter().map(|u| u.secs).sum::<f64>() / usages.len() as f64;
    let switches = voluntary + involuntary;
    let pct = |part: u64, whole: u64| part as f64 / whole as f64 * 100.0;
    let mut info = |name: &str, value: f64, unit: &str| {
        r.metrics.insert(name.into(), Metric::info(value, unit));
    };
    if run + wait > 0 {
        info("sched_wait_pct", pct(wait, run + wait), "%");
    }
    if slices > 0 {
        info("sched_slice_us", run as f64 / slices as f64 / 1000.0, "us");
    }
    if switches > 0 {
        info("sched_preempt_pct", pct(involuntary, switches), "%");
    }
    if secs > 0.0 {
        info("sched_switches_per_s", switches as f64 / secs, "1/s");
        if let Some(m) = migrations {
            info("sched_migrations_per_s", m as f64 / secs, "1/s");
        }
    }
    let c = &mut r.counters;
    c.insert("sched_threads".into(), usages.len() as u64);
    c.insert("sched_run_ms".into(), run / 1_000_000);
    c.insert("sched_wait_ms".into(), wait / 1_000_000);
    c.insert("sched_runnable_ms".into(), (run + wait) / 1_000_000);
    c.insert("sched_slices".into(), slices);
    c.insert("sched_switches".into(), switches);
    c.insert("sched_voluntary".into(), voluntary);
    c.insert("sched_involuntary".into(), involuntary);
    if let Some(m) = migrations {
        c.insert("sched_migrations".into(), m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::Better;

    #[test]
    fn the_files_are_parsed() {
        assert_eq!(parse_schedstat("123456789 4567 89\n"), Some((123_456_789, 4567, 89)));
        assert_eq!(parse_schedstat("12 34\n"), None);
        assert_eq!(parse_schedstat(""), None);
        let status = "Name:\tburst-3\nvoluntary_ctxt_switches:\t2501\nnonvoluntary_ctxt_switches:\t17\n";
        assert_eq!(field(status, "voluntary_ctxt_switches"), Some(2501));
        assert_eq!(field(status, "nonvoluntary_ctxt_switches"), Some(17));
        let sched = "burst-3 (4242, #threads: 9)\n\
                     -------------------------------------------------------------------\n\
                     se.exec_start                                :      12345.678901\n\
                     se.nr_migrations                             :                   42\n\
                     nr_switches                                  :                 2518\n";
        assert_eq!(field(sched, "se.nr_migrations"), Some(42));
        assert_eq!(field(sched, "se.nr_wakeups"), None);
        // Not a number: no value, and no panic.
        assert_eq!(field(sched, "se.exec_start"), None);
    }

    #[test]
    fn a_thread_reads_its_own_counters() {
        let c = Collector::new(true);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _watching = c.watch();
                // Some time on a CPU, and some sleeps to give it up.
                let until = Instant::now() + std::time::Duration::from_millis(20);
                let mut x = 1u64;
                while Instant::now() < until {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
        });
        let mut r = RunResult::default();
        c.record(&mut r);
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        let n = &r.counters;
        assert_eq!(n["sched_threads"], 1);
        // It slept five times of its own accord.
        assert!(n["sched_voluntary"] >= 5, "{n:?}");
        assert_eq!(n["sched_switches"], n["sched_voluntary"] + n["sched_involuntary"]);
        assert!(n["sched_runnable_ms"] >= n["sched_run_ms"].max(n["sched_wait_ms"]), "{n:?}");
        // Everything derived is informational.
        assert!(!r.metrics.is_empty());
        assert!(
            r.metrics.iter().all(|(name, m)| name.starts_with("sched_") && m.better == Better::Neither),
            "{:?}",
            r.metrics
        );
        assert!(r.metrics["sched_switches_per_s"].value > 0.0);
    }

    #[test]
    fn off_means_nothing_is_read_or_recorded() {
        let c = Collector::new(false);
        drop(c.watch());
        assert!(c.threads.lock().unwrap().is_empty());
        let mut r = RunResult::default();
        c.record(&mut r);
        assert!(r.metrics.is_empty() && r.counters.is_empty());
    }

    #[test]
    fn counters_are_summed_and_shares_derived() {
        let thread = |run_ms: u64, wait_ms: u64, slices, voluntary, involuntary, migrations| Usage {
            stats: Stats {
                run_ns: run_ms * 1_000_000,
                wait_ns: wait_ms * 1_000_000,
                slices,
                voluntary,
                involuntary,
                migrations,
            },
            secs: 2.0,
        };
        let mut r = RunResult::default();
        summarize(&mut r, &[thread(300, 100, 1000, 900, 100, Some(40)), thread(500, 100, 1000, 700, 300, Some(60))]);
        let v = |name: &str| r.metrics[name].value;
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // 200 ms waiting of 1000 ms runnable; 400 of 2000 switches forced.
        assert!(near(v("sched_wait_pct"), 20.0) && near(v("sched_preempt_pct"), 20.0), "{:?}", r.metrics);
        // 800 ms on a CPU in 2000 stints.
        assert!(near(v("sched_slice_us"), 400.0));
        // Two threads side by side for two seconds: per second of that.
        assert!(near(v("sched_switches_per_s"), 1000.0) && near(v("sched_migrations_per_s"), 50.0));
        assert_eq!(r.metrics["sched_slice_us"].unit, "us");
        let n = &r.counters;
        assert_eq!((n["sched_run_ms"], n["sched_wait_ms"], n["sched_runnable_ms"]), (800, 200, 1000));
        assert_eq!((n["sched_voluntary"], n["sched_involuntary"], n["sched_switches"]), (1600, 400, 2000));
        assert_eq!((n["sched_threads"], n["sched_slices"], n["sched_migrations"]), (2, 2000, 100));

        // One thread without migrations: no sum that would be too low.
        let mut r = RunResult::default();
        summarize(&mut r, &[thread(300, 100, 1000, 900, 100, Some(40)), thread(500, 100, 1000, 700, 300, None)]);
        assert!(!r.metrics.contains_key("sched_migrations_per_s") && !r.counters.contains_key("sched_migrations"));
        assert!(r.metrics.contains_key("sched_wait_pct"));

        // A thread whose counters could not be read: the run does not
        // count, and says why. An invalid run is left as it is.
        let c = Collector::new(true);
        c.threads.lock().unwrap().push(Err("/proc/thread-self/schedstat: gone".into()));
        let mut r = RunResult::default();
        c.record(&mut r);
        assert_eq!(
            r.invalid_reason.as_deref(),
            Some("scheduler statistics were asked for and could not be read: /proc/thread-self/schedstat: gone")
        );
        // A kernel that keeps no such statistics: the file is there and
        // holds zeros. Every thread worked and none has any time on a
        // CPU: said, not passed off as a run with nothing to report.
        let c = Collector::new(true);
        let zeros = Usage { stats: Stats { voluntary: 5000, involuntary: 3, ..Stats::default() }, secs: 5.0 };
        c.threads.lock().unwrap().extend([Ok(zeros), Ok(zeros)]);
        let mut r = RunResult::default();
        c.record(&mut r);
        assert!(
            r.invalid_reason.as_deref().unwrap().contains("this kernel does not keep them"),
            "{:?}",
            r.invalid_reason
        );
        assert!(r.metrics.is_empty() && r.counters.is_empty());
        // One thread with time on a CPU is enough to know they are kept.
        let c = Collector::new(true);
        let ran = Usage { stats: Stats { run_ns: 1_000_000, slices: 3, ..Stats::default() }, secs: 5.0 };
        c.threads.lock().unwrap().extend([Ok(zeros), Ok(ran)]);
        let mut r = RunResult::default();
        c.record(&mut r);
        assert!(r.invalid_reason.is_none() && r.counters["sched_threads"] == 2, "{:?}", r.invalid_reason);

        let mut r = RunResult { invalid_reason: Some("boom".into()), ..Default::default() };
        Collector::new(true).record(&mut r);
        assert_eq!(r.invalid_reason.as_deref(), Some("boom"));
    }
}
