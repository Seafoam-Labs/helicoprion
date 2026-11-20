//! periodic-wake: a few threads that wake on a timer, do a short piece of
//! work and sleep until the next period, like a frame, an audio buffer or
//! an input handler. With `hogs > 0` every CPU is busy with something else
//! meanwhile.
//!
//! wake-pingpong asks how fast two threads can hand each other a token,
//! and its rate of work depends on the scheduler: one that lets replies
//! arrive before the receiver sleeps completes other round trips than one
//! that does not, and the two are hard to compare. Here the amount of
//! work is fixed by the clock. Every scheduler is asked for the same
//! thing at the same moments, and the question is only how late it
//! delivers.
//!
//! # What is measured
//!
//! A task's periods are released at fixed instants, `period` apart,
//! whatever happened before. For each period that the task runs:
//!
//! * `wake`: from the release instant to the task running again (the
//!   timer fires, the scheduler picks the task);
//! * `response`: from the release instant to the work being done. The
//!   work is a fixed amount of CPU time, so being preempted in the middle
//!   of it lengthens the response and not the work.
//!
//! A period is *on time* when its work is done before the next release.
//! If it is not, the releases that passed meanwhile are never started
//! (`periods_skipped`): the task resumes with the next release in the
//! future, as a frame loop drops frames instead of queueing them.
//! `deadline_miss_pct` is the share of all due periods that were late or
//! skipped, and is what gets scored: 1.0% against 0.1% is ten times fewer
//! missed deadlines. `on_time_pct`, its complement, is the same fact in
//! the form a reader expects and is informational: as a change of the
//! share on time, 99.0% against 99.9% is under one percent, and from
//! 95.3% on not even a perfect 100% would reach a 5% threshold.
//!
//! `task<i>_periods_missed` splits the late and skipped periods by task,
//! numbered in the order of their phases. The tasks do the same thing at
//! different instants: misses that are one task's and not the others'
//! come from when that task wakes, not from what it is.
//!
//! # Timing
//!
//! Releases are absolute (`clock_nanosleep` with `TIMER_ABSTIME` on
//! `CLOCK_MONOTONIC`), so a late period does not shift the following
//! ones. The tasks' phases are spread evenly over a period so that they
//! do not all wake at the same instant. Where on the clock the first
//! release falls is whenever the run started, unless `phase` places it:
//! the first task is then released at that much past every multiple of
//! the period. That is a place on the clock and nothing more. The
//! kernel's timer tick is timed by the same clock, so with a period that
//! is a multiple of the tick a task's releases keep their place against a
//! CPU's ticks for a whole run; where those ticks are is not known here
//! (`skew_tick` offsets them per CPU, and an idle CPU has none), so the
//! same phase need not be the same instant within a tick. Each task sets its timer slack to
//! 1 ns: the default of 50 µs lets the kernel fire a timer that much late
//! on purpose, which would be measured as wake-up latency. The setting is
//! read back, and a run whose tasks did not get it is invalid: the
//! parameters say 1 ns, and a run under another slack is another
//! measurement.
//!
//! # Start and failure
//!
//! Every task reports ready by counting itself in; the start instant is
//! fixed only when all have, or the run is abandoned after
//! `READY_TIMEOUT`. Nothing waits on a rendezvous that a thread which
//! could not be created would have to join, and whatever goes wrong,
//! every thread that was started is joined before the run returns, so
//! none is still working when the next run begins.
//!
//! Work is counted in the thread's own CPU time
//! (`CLOCK_THREAD_CPUTIME_ID`), read between short bursts of integer
//! mixing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use helicoprion_core::hist::Recorder;
use helicoprion_core::{Metric, RunResult, Window};
use serde_json::json;

use crate::Workload;
use crate::clock::{Alive, TIMER_SLACK_NS, burn, now_ns, ns, set_timer_slack, sleep_until};
use crate::hog::Hogs;
use crate::samples::population;
use crate::schedstat::{self, Collector};

pub const POP_WAKE: &str = "wake";
pub const POP_RESPONSE: &str = "response";

/// Start not fixed yet.
const NOT_STARTED: u64 = 0;
/// The run was abandoned before it started; tasks exit.
const ABANDONED: u64 = u64::MAX;
/// From fixing the start to the first release: time for every task to
/// read it and go to sleep.
const LEAD: Duration = Duration::from_millis(5);
/// How long the tasks may take to report ready before the run is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Test hooks: failures on purpose, and a count of the tasks alive.
#[derive(Clone, Default)]
struct Hooks {
    /// Creating the task with this index fails.
    fail_spawn_at: Option<usize>,
    /// The task with this index panics once the run has started.
    panic_task: Option<usize>,
    /// The task with this index never reports ready.
    stall_task: Option<usize>,
    /// Shorter `READY_TIMEOUT`.
    ready_timeout: Option<Duration>,
    /// Tasks running: up on entry, down on exit, however they exit.
    alive: Arc<AtomicUsize>,
}

pub struct PeriodicWake {
    pub tasks: usize,
    pub period: Duration,
    /// CPU time of work per period.
    pub work: Duration,
    pub hogs: usize,
    /// Read every task's scheduler statistics.
    pub sched_diag: bool,
    /// Where the releases lie on the clock: those of the first task at
    /// this much past a multiple of the period. `None` leaves it to the
    /// moment the run happens to start.
    pub phase: Option<Duration>,
}

impl Workload for PeriodicWake {
    fn name(&self) -> String {
        if self.hogs == 0 { "periodic-wake".into() } else { "periodic-wake/loaded".into() }
    }

    fn description(&self) -> String {
        let base = format!(
            "{} threads waking every {} µs for {} µs of work; wake-up latency, response time and periods finished on time",
            self.tasks,
            self.period.as_micros(),
            self.work.as_micros()
        );
        if self.hogs == 0 { base } else { format!("{base}, with {} CPU hog threads in the background", self.hogs) }
    }

    fn params(&self) -> serde_json::Value {
        let mut params = json!({
            "tasks": self.tasks,
            "period_us": self.period.as_micros() as u64,
            "work_us": self.work.as_micros() as u64,
            "hogs": self.hogs,
            "timer_slack_ns": TIMER_SLACK_NS,
        });
        // Only when set: a run with its releases placed is another
        // measurement than one that starts whenever it starts.
        if let Some(phase) = self.phase {
            params["phase_us"] = (phase.as_micros() as u64).into();
        }
        schedstat::mark(params, self.sched_diag)
    }

    fn headline_metrics(&self) -> &'static [&'static str] {
        &["wake_p50", "wake_p99", "response_p99"]
    }

    fn progress_note(&self, r: &RunResult) -> Option<String> {
        Some(format!("on time={:.1}%", r.metrics.get("on_time_pct")?.value))
    }

    fn run(&self, duration: Duration) -> RunResult {
        self.run_with(duration, &Hooks::default())
    }
}

impl PeriodicWake {
    fn run_with(&self, duration: Duration, hooks: &Hooks) -> RunResult {
        let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
        if self.tasks == 0 || self.period.is_zero() || self.work >= self.period {
            return invalid("periodic-wake needs at least one task and work shorter than the period".into());
        }
        if self.phase.is_some_and(|phase| phase >= self.period) {
            return invalid("periodic-wake needs a phase shorter than the period".into());
        }
        let hogs = if self.hogs > 0 {
            match Hogs::start(self.hogs) {
                Ok(h) => {
                    // Let hogs spread out before measuring.
                    std::thread::sleep(Duration::from_millis(200));
                    Some(h)
                }
                Err(e) => return invalid(format!("spawning hog threads: {e}")),
            }
        } else {
            None
        };

        let stats = Arc::new(Collector::new(self.sched_diag));
        let start = Arc::new(AtomicU64::new(NOT_STARTED));
        let ready = Arc::new(AtomicUsize::new(0));
        let (period, work, span) = (ns(self.period), ns(self.work), ns(duration));
        let mut handles = Vec::with_capacity(self.tasks);
        let mut failed: Option<String> = None;
        for i in 0..self.tasks {
            let (task_start, task_ready, hooks) = (Arc::clone(&start), Arc::clone(&ready), hooks.clone());
            let stats = Arc::clone(&stats);
            // Phases spread over one period.
            let phase = period * i as u64 / self.tasks as u64;
            let body = move || -> Result<TaskOut, String> {
                let _alive = Alive::new(&hooks.alive);
                set_timer_slack()?;
                if hooks.stall_task != Some(i) {
                    task_ready.fetch_add(1, Ordering::Release);
                }
                let t0 = loop {
                    match task_start.load(Ordering::Acquire) {
                        NOT_STARTED => std::thread::sleep(Duration::from_micros(200)),
                        ABANDONED => return Ok(TaskOut::default()),
                        t0 => break t0,
                    }
                };
                if hooks.panic_task == Some(i) {
                    panic!("periodic task {i} failing on purpose");
                }
                // From here to the end of its last period; the first
                // release is still `LEAD` away.
                let _watching = stats.watch();
                Ok(task(t0 + phase, t0 + span, period, work))
            };
            let spawned = if hooks.fail_spawn_at == Some(i) {
                Err(std::io::Error::other("cannot create a thread (test)"))
            } else {
                std::thread::Builder::new().name(format!("periodic-{i}")).spawn(body)
            };
            match spawned {
                Ok(h) => handles.push(h),
                Err(e) => {
                    failed = Some(format!("spawning periodic threads: {e}"));
                    break;
                }
            }
        }

        // Wait for every task to count itself in. A task that failed
        // before that (its timer slack) never does: the handle is done.
        let give_up = Instant::now() + hooks.ready_timeout.unwrap_or(READY_TIMEOUT);
        while failed.is_none() && ready.load(Ordering::Acquire) < self.tasks {
            if handles.iter().any(|h| h.is_finished()) {
                failed = Some("a periodic thread stopped before the run started".into());
            } else if Instant::now() >= give_up {
                failed = Some("periodic threads never became ready".into());
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let mut hog = None;
        let mut began = Instant::now();
        if failed.is_some() {
            start.store(ABANDONED, Ordering::Release);
        } else {
            // Hog snapshot right before the start is fixed, and again as
            // soon as the window is over.
            let hog_t0 = hogs.as_ref().map(|h| (h.work(), Instant::now()));
            let now = now_ns();
            let t0 = first_release(now + ns(LEAD), period, self.phase.map(ns));
            began = Instant::now() + Duration::from_nanos(t0 - now);
            start.store(t0, Ordering::Release);
            sleep_until(t0 + span);
            hog = hogs.as_ref().zip(hog_t0).map(|(h, (w0, at0))| (h.work() - w0, at0, Instant::now()));
        }

        // Every thread that was started is joined, whatever the others
        // did: none may still be at work when the next run begins.
        let mut outs = Vec::with_capacity(handles.len());
        for h in handles {
            match h.join() {
                Ok(Ok(out)) => outs.push(out),
                Ok(Err(why)) => failed = failed.or(Some(why)),
                Err(_) => failed = failed.or(Some("a periodic thread panicked".into())),
            }
        }
        drop(hogs);
        match failed {
            Some(why) => invalid(why),
            None => {
                let mut r = summarize(&outs, duration, began, hog);
                stats.record(&mut r);
                r
            }
        }
    }
}

#[derive(Default)]
struct TaskOut {
    wake: Recorder,
    response: Recorder,
    /// Periods that were started.
    ran: u64,
    /// Of those: work done after the next release.
    late: u64,
    /// Releases that passed while an earlier period was still at work.
    skipped: u64,
}

/// The first release of the first task: `earliest`, or with a phase the
/// first instant from there on that is `phase` past a multiple of the
/// period on the clock the releases are timed by.
fn first_release(earliest: u64, period: u64, phase: Option<u64>) -> u64 {
    match phase {
        None => earliest,
        Some(phase) => (earliest.saturating_sub(phase)).next_multiple_of(period) + phase,
    }
}

/// Where a task goes after finishing period `k` at `done` (nanoseconds
/// after its first release): the next period to run, and how many
/// releases in between it has to skip because they are already past.
fn next_period(k: u64, done: u64, period: u64) -> (u64, u64) {
    // The period whose release was the last one at or before `done`.
    let current = done / period;
    if current > k { (current + 1, current - k) } else { (k + 1, 0) }
}

/// One task: periods released at `first + k * period` until `end`.
fn task(first: u64, end: u64, period: u64, work: u64) -> TaskOut {
    let mut out = TaskOut::default();
    let mut k = 0u64;
    loop {
        let release = first + k * period;
        if release >= end {
            return out;
        }
        sleep_until(release);
        out.wake.record(now_ns().saturating_sub(release));
        burn(work);
        let done = now_ns();
        out.response.record(done.saturating_sub(release));
        out.ran += 1;
        let (next, skipped) = next_period(k, done.saturating_sub(first), period);
        if skipped > 0 {
            out.late += 1;
            // The releases skipped are the ones before `next`, without
            // those at or after the end, which were never due.
            let due_until = (end - first).div_ceil(period);
            out.skipped += skipped.min(due_until.saturating_sub(k + 1));
        }
        k = next;
    }
}

fn summarize(outs: &[TaskOut], duration: Duration, began: Instant, hog: Option<(u64, Instant, Instant)>) -> RunResult {
    let (mut wake, mut response) = (Recorder::default(), Recorder::default());
    let (mut ran, mut late, mut skipped) = (0, 0, 0);
    for o in outs {
        wake.merge(&o.wake);
        response.merge(&o.response);
        ran += o.ran;
        late += o.late;
        skipped += o.skipped;
    }
    let window = duration.as_secs_f64();
    let mut r = RunResult { duration_s: window, ..Default::default() };
    if ran == 0 {
        r.invalid_reason = Some("no period ran".into());
        return r;
    }
    let wake_curve = population(&mut r.metrics, POP_WAKE, &wake);
    let response_curve = population(&mut r.metrics, POP_RESPONSE, &response);
    let due = ran + skipped;
    // Scored: missed deadlines, where ten times fewer is ten times fewer.
    // Zero is a value here, not a gap: a side without a single miss has
    // 0%, and a baseline of zero leaves the change undefined, which the
    // report says instead of scoring.
    let missed = late + skipped;
    r.metrics.insert("deadline_miss_pct".into(), Metric::lower(missed as f64 / due as f64 * 100.0, "%"));
    // The same fact the way a reader expects it; informational, since a
    // relative change of a share near 100% says nothing (see the module
    // documentation).
    r.metrics.insert("on_time_pct".into(), Metric::info((due - missed) as f64 / due as f64 * 100.0, "%"));
    r.windows.insert("latency".into(), Window { start_s: 0.0, end_s: window });
    if let Some((units, at0, at1)) = hog {
        let secs = (at1 - at0).as_secs_f64();
        if secs > 0.0 {
            // Informational, as in wake-pingpong: how much the hogs got is
            // a split of CPU time, not a quality.
            r.metrics.insert("hog_work_per_s".into(), Metric::info(units as f64 / secs, "units/s"));
        }
        let offset = |t: Instant| if t >= began { (t - began).as_secs_f64() } else { -(began - t).as_secs_f64() };
        r.windows.insert("hog".into(), Window { start_s: offset(at0), end_s: offset(at1) });
    }
    let c = &mut r.counters;
    c.insert("periods_due".into(), due);
    c.insert("periods_ran".into(), ran);
    c.insert("periods_late".into(), late);
    c.insert("periods_skipped".into(), skipped);
    // Per task, in the order of their phases: whether the misses are
    // every task's or one's. Counted from what each task returns, nothing
    // is added to what it does per period.
    for (i, o) in outs.iter().enumerate() {
        c.insert(format!("task{i}_periods_missed"), o.late + o.skipped);
    }
    for (name, rec, curve) in [(POP_WAKE, &wake, wake_curve), (POP_RESPONSE, &response, response_curve)] {
        r.latency_histograms.insert(name.into(), rec.to_histogram("ns"));
        if !curve.is_empty() {
            r.quantile_curves.insert(name.into(), curve);
        }
    }
    r.quantile_unit = Some("us".into());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::Better;

    fn quick(hogs: usize) -> PeriodicWake {
        PeriodicWake {
            tasks: 2,
            period: Duration::from_millis(1),
            work: Duration::from_micros(50),
            hogs,
            sched_diag: false,
            phase: None,
        }
    }

    #[test]
    fn a_phase_places_the_releases_on_the_clock() {
        // Without one, the run starts when it starts.
        assert_eq!(first_release(12_345, 1000, None), 12_345);
        // With one: the first instant not before that with this remainder.
        assert_eq!(first_release(12_345, 1000, Some(0)), 13_000);
        assert_eq!(first_release(12_345, 1000, Some(300)), 13_300);
        assert_eq!(first_release(12_345, 1000, Some(345)), 12_345);
        assert_eq!(first_release(12_345, 1000, Some(999)), 12_999);
        assert_eq!(first_release(100, 1000, Some(300)), 300);
        // Never earlier, and less than a period later.
        for (earliest, phase) in [(0, 0), (1, 0), (999, 1), (7_000_001, 250), (7_000_250, 250), (7_000_251, 250)] {
            let at = first_release(earliest, 1000, Some(phase));
            assert!(at >= earliest && at - earliest < 1000 && at % 1000 == phase, "{earliest} {phase} {at}");
        }

        // A run with a phase says so in its parameters, one without does
        // not have the key at all, and the periods are what they were.
        let w = PeriodicWake { phase: Some(Duration::from_micros(300)), ..quick(0) };
        assert_eq!(w.params()["phase_us"], 300);
        assert!(quick(0).params().get("phase_us").is_none());
        let r = w.run(Duration::from_millis(100));
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        assert!((198..=200).contains(&r.counters["periods_due"]), "{:?}", r.counters);
        // The hog window still starts before the first release.
        let r = PeriodicWake { phase: Some(Duration::from_micros(999)), ..quick(1) }.run(Duration::from_millis(100));
        let window = r.windows["hog"];
        assert!(window.start_s <= 0.0 && window.end_s >= 0.1, "{window:?}");
        // A phase of a whole period or more is another phase's name.
        let bad = PeriodicWake { phase: Some(Duration::from_millis(1)), ..quick(0) };
        assert!(bad.run(Duration::from_millis(10)).invalid_reason.unwrap().contains("phase shorter than the period"));
    }

    #[test]
    fn late_periods_skip_the_releases_they_overran() {
        let period = 1000;
        // Done within its own period: the next one, nothing skipped.
        assert_eq!(next_period(0, 300, period), (1, 0));
        assert_eq!(next_period(7, 7999, period), (8, 0));
        // Done just after the next release: that release is gone.
        assert_eq!(next_period(0, 1000, period), (2, 1));
        assert_eq!(next_period(7, 8001, period), (9, 1));
        // Done three and a half periods late: three releases are gone.
        assert_eq!(next_period(4, 8500, period), (9, 4));
        // Woken early enough to finish before its release would be odd, but
        // must not go backwards.
        assert_eq!(next_period(5, 100, period), (6, 0));
    }

    #[test]
    fn periods_are_counted_and_timed() {
        let w = quick(0);
        let r = w.run(Duration::from_millis(300));
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        let c = &r.counters;
        let (due, ran) = (c["periods_due"], c["periods_ran"]);
        // Two tasks, a period of 1 ms, 300 ms: 600 periods due, whatever
        // the machine did; this is what makes schedulers comparable.
        assert_eq!(due, ran + c["periods_skipped"]);
        assert!((598..=600).contains(&due), "{due}");
        assert!(c["periods_late"] <= ran);
        // One wake-up and one response per period that ran.
        assert_eq!(r.latency_histograms[POP_WAKE].count, ran);
        assert_eq!(r.latency_histograms[POP_RESPONSE].count, ran);
        // The response includes the work: at least 50 µs, and never
        // shorter than the wake-up.
        let (wake, response) = (r.metrics["wake_p50"].value, r.metrics["response_p50"].value);
        assert!(response >= 50.0 && response >= wake, "{wake} {response}");
        assert_eq!(r.metrics["wake_p50"].better, Better::Lower);
        assert!(r.metrics["wake_max"].extreme && r.metrics["response_max"].extreme);
        // Missed deadlines are scored, lower is better; the share on time
        // is its complement and informational.
        let (missed, on_time) = (&r.metrics["deadline_miss_pct"], &r.metrics["on_time_pct"]);
        assert_eq!((missed.better, missed.unit.as_str()), (Better::Lower, "%"));
        assert_eq!((on_time.better, on_time.unit.as_str()), (Better::Neither, "%"));
        let expected = (c["periods_late"] + c["periods_skipped"]) as f64 / due as f64 * 100.0;
        // And split by task: one count each, together all of them.
        let by_task: Vec<u64> = c.iter().filter(|(k, _)| k.starts_with("task")).map(|(_, v)| *v).collect();
        assert!(c.contains_key("task0_periods_missed") && c.contains_key("task1_periods_missed"));
        assert_eq!((by_task.len(), by_task.iter().sum::<u64>()), (2, c["periods_late"] + c["periods_skipped"]));
        assert!((missed.value - expected).abs() < 1e-9 && (0.0..=100.0).contains(&missed.value));
        assert!((missed.value + on_time.value - 100.0).abs() < 1e-9);
        // 600 samples carry a p50 and not a p99.9.
        assert!(!r.metrics.contains_key("wake_p99.9"));
        assert!(r.quantile_curves[POP_WAKE].iter().any(|(q, _)| *q == 0.5));
        assert_eq!(r.quantile_unit.as_deref(), Some("us"));
        assert_eq!(r.duration_s, 0.3);
        assert!(!r.metrics.contains_key("hog_work_per_s"));
    }

    #[test]
    fn loaded_variant_reports_the_hogs_share() {
        let w = quick(1);
        assert_eq!(w.name(), "periodic-wake/loaded");
        let r = w.run(Duration::from_millis(150));
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        let hog = &r.metrics["hog_work_per_s"];
        assert!(hog.value > 0.0 && hog.better == Better::Neither);
        let window = r.windows["hog"];
        assert!(window.start_s <= 0.0 && window.end_s >= 0.15, "{window:?}");
        // Still the same number of periods due as without load.
        assert!((298..=300).contains(&r.counters["periods_due"]), "{:?}", r.counters);
    }

    /// With scheduler statistics: one set of counters per task, from its
    /// first release to its last period, and a parameter that keeps such
    /// a run apart from one without.
    #[test]
    fn scheduler_statistics_cover_every_task() {
        let w = PeriodicWake { sched_diag: true, ..quick(0) };
        assert_eq!(w.params()["sched_diag"], true);
        assert!(quick(0).params().get("sched_diag").is_none());
        let r = w.run(Duration::from_millis(200));
        if !schedstat::kept(&r) {
            return;
        }
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        let c = &r.counters;
        assert_eq!(c["sched_threads"], 2);
        // Both tasks are in the counters, and the counters agree with
        // each other. How often a task slept is not asked: on an idle
        // machine once per period, but the periods do not say so. One
        // that was done in time and lost its CPU before it could sleep
        // goes on without a sleep and is not late, and one that was late
        // may sleep all the same, for the release after those it
        // skipped. That is the scheduler's doing and no fault of the
        // statistics.
        assert!(c["sched_slices"] >= 2, "{c:?}");
        assert_eq!(c["sched_switches"], c["sched_voluntary"] + c["sched_involuntary"], "{c:?}");
        // 50 µs of work per period that ran: that much time on a CPU,
        // give or take, and far from the 200 ms the run lasted per task.
        let worked_ms = c["periods_ran"] / 20;
        assert!(c["sched_run_ms"] >= worked_ms / 2 && c["sched_run_ms"] < 200, "{c:?}");
        assert!(r.metrics.contains_key("sched_wait_pct") && r.metrics.contains_key("sched_preempt_pct"));
        // Without the option nothing of it is there.
        let r = quick(0).run(Duration::from_millis(50));
        assert!(!r.counters.keys().chain(r.metrics.keys()).any(|k| k.starts_with("sched_")));
    }

    #[test]
    fn parameters_identify_the_workload() {
        let w = quick(0);
        assert_eq!(w.name(), "periodic-wake");
        assert_eq!(w.params(), json!({ "tasks": 2, "period_us": 1000, "work_us": 50, "hogs": 0, "timer_slack_ns": 1 }));
        assert_eq!(w.headline_metrics(), ["wake_p50", "wake_p99", "response_p99"]);
        // Work that does not fit its period is refused, not measured.
        let bad = PeriodicWake { work: Duration::from_millis(1), ..quick(0) };
        assert!(bad.run(Duration::from_millis(10)).invalid_reason.unwrap().contains("shorter than the period"));
    }

    /// Whatever fails, the run returns, says why, and leaves no thread
    /// behind: a thread that cannot be created must not leave the others
    /// waiting for it, and a panic in one must not let the rest outlive
    /// the run.
    #[test]
    fn failures_end_the_run_and_leave_no_thread_behind() {
        let w = PeriodicWake { tasks: 4, ..quick(0) };
        let run = |hooks: Hooks| {
            let alive = Arc::clone(&hooks.alive);
            let began = Instant::now();
            let r = w.run_with(Duration::from_millis(100), &hooks);
            (r.invalid_reason.expect("invalid"), began.elapsed(), alive.load(Ordering::SeqCst))
        };

        // The third thread cannot be created: the two that exist are told
        // to leave and are joined. Nothing is spawned to help.
        let (why, took, alive) = run(Hooks { fail_spawn_at: Some(2), ..Default::default() });
        assert!(why.contains("spawning periodic threads"), "{why}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(alive, 0);
        // The very first one, too.
        let (why, _, alive) = run(Hooks { fail_spawn_at: Some(0), ..Default::default() });
        assert!(why.contains("spawning periodic threads") && alive == 0, "{why}");

        // One task panics after the start: the others finish their run
        // and are joined before the result is returned.
        let (why, took, alive) = run(Hooks { panic_task: Some(1), ..Default::default() });
        assert_eq!(why, "a periodic thread panicked");
        assert!(took >= Duration::from_millis(100), "the others ran to the end: {took:?}");
        assert_eq!(alive, 0);

        // A task that never reports ready: abandoned after the timeout.
        let hooks = Hooks { stall_task: Some(3), ready_timeout: Some(Duration::from_millis(50)), ..Default::default() };
        let (why, took, alive) = run(hooks);
        assert_eq!(why, "periodic threads never became ready");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(alive, 0);

        // And a run that works leaves none behind either.
        let hooks = Hooks::default();
        let alive = Arc::clone(&hooks.alive);
        let r = w.run_with(Duration::from_millis(50), &hooks);
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        assert_eq!(alive.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_task_that_overruns_is_late_and_skips() {
        // Work of 2.5 periods, measured in CPU time: every period that
        // runs is late and two releases pass meanwhile.
        let (period, work) = (2_000_000, 5_000_000);
        let first = now_ns() + 1_000_000;
        let out = task(first, first + 60_000_000, period, work);
        // How many periods run depends on how much CPU the test gets.
        assert!(out.ran >= 2, "{}", out.ran);
        assert_eq!(out.late, out.ran);
        assert!(out.skipped >= 2 * (out.ran - 1), "{} skipped for {} ran", out.skipped, out.ran);
        // Nothing beyond the end is counted as due.
        assert!(out.ran + out.skipped <= 30, "{} + {}", out.ran, out.skipped);
        assert!(out.response.max().unwrap() >= work);
    }
}
