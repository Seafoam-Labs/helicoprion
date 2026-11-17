//! wake-burst: one thread wakes a whole group of sleeping threads with a
//! single call, each does a short piece of work and goes back to sleep,
//! like a thread pool handed a batch, a parallel build step or a frame
//! fanned out to workers.
//!
//! wake-pingpong and periodic-wake wake one task at a time. Here the
//! scheduler gets many runnable tasks in the same instant and has to find
//! a CPU for each: whether it spreads them, how fast the last one runs
//! and what happens when there are more tasks than CPUs. The suite runs
//! it in three sizes, half as many tasks as CPUs, as many, and twice as
//! many.
//!
//! # What is measured
//!
//! Bursts are released by the clock, `period` apart, so every scheduler
//! is asked for the same work at the same moments. For each burst the
//! waker notes the time, right before the call that wakes the group
//! (`FUTEX_WAKE` for all waiters of one futex). From that instant:
//!
//! * `start`: to a woken task running, one sample per task and burst;
//! * `all_running`: to the last task of the group running, one sample
//!   per burst;
//! * `all_done`: to the last task of the group having done its work, one
//!   sample per burst. The work is a fixed amount of the thread's own CPU
//!   time, so a task preempted in the middle of it finishes later and
//!   does not work more.
//!
//! The kernel wakes the group one task after another inside the waker's
//! call, so `start` includes the time the waker needs to get to a task:
//! that is part of waking a group and the same for every scheduler only
//! as far as the scheduler makes it so.
//!
//! With twice as many tasks as CPUs half of the group has to wait for a
//! CPU; how long is the scheduler's choice (let the first ones finish, or
//! slice), and `all_running` and `all_done` show the two sides of it.
//!
//! # Every wake-up is accounted for
//!
//! A wake-up is only measured for a task that slept in `FUTEX_WAIT` when
//! its burst was released, and every such wake-up is measured: a task
//! woken by the last burst that gets a CPU only after the run is over
//! still takes its time and does its work, since leaving it out would
//! drop exactly the slowest wake-ups. Each task counts, for every burst
//! released, which of three things happened:
//!
//! * it was woken by the burst (`wakeups_ran`, one sample);
//! * it was still busy with an earlier burst, woken by it and waiting for
//!   a CPU or at its work (`wakeups_overrun`): it is not woken again,
//!   finds the burst gone when it is done and waits for the next;
//! * it was done with its work but not asleep yet
//!   (`wakeups_not_asleep`): the few instructions between the end of the
//!   work and the sleep, or the very first burst coming before the task
//!   got to sleep.
//!
//! What tells the second from the third is the time: a burst released
//! before the task finished its work found it busy, one released after
//! found it done. When the task gets to look does not matter, so a task
//! that loses its CPU right after its work is not taken for one that was
//! still working.
//!
//! The three add up to bursts times tasks for every task, or the run is
//! invalid. `overrun_pct` is the share of the second kind, scored, lower
//! is better; without it a scheduler that lets tasks overrun would be
//! judged on the bursts that went well. The third kind is counted and
//! shown, and not scored: it is not a way of being late. `all_running`
//! and `all_done` exist only for bursts in which every task took part
//! (`bursts_complete`).
//!
//! # The waker
//!
//! The waker runs with a timer slack of 1 ns, set and read back as in
//! periodic-wake, so that the bursts come when they are due. One reading
//! of the clock decides whether a release is still due, is the time the
//! tasks measure from, and gives how late the release was
//! (`release_late`, informational). From that reading to the return of
//! the wake call is `wake_call`, informational as well. The waker can
//! lose its CPU in that interval: before the tasks are woken, and the
//! time is in every `start` of the burst without being the tasks'
//! wake-up; or after, and only `wake_call` is long. A large value says
//! that the waker's side may have added to the start delays, not which
//! of them.
//!
//! A release that is already a whole period past when the waker gets to
//! it is left out instead of following the next one at once
//! (`bursts_skipped`).
//! Every scheduler is meant to get the same bursts, so a run in which
//! more than `MAX_SKIPPED_PCT` of them were left out is invalid: a
//! scheduler must not look better for having been handed less work.
//!
//! # Start and failure
//!
//! As in periodic-wake: every task counts itself in before the first
//! burst is released, the run is abandoned after `READY_TIMEOUT`, and
//! every thread that was started is joined before the run returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use helicoprion_core::hist::Recorder;
use helicoprion_core::{Metric, RunResult, Window};
use serde_json::json;

use crate::Workload;
use crate::clock::{Alive, PreciseTimers, TIMER_SLACK_NS, burn, now_ns, ns, sleep_until};
use crate::futex::{self, Wait};
use crate::samples::{min_samples_for, population};

pub const POP_START: &str = "start";
pub const POP_ALL_RUNNING: &str = "all_running";
pub const POP_ALL_DONE: &str = "all_done";

/// From every task being ready to the first burst: time for all of them
/// to go to sleep.
const LEAD: Duration = Duration::from_millis(5);
/// How long the tasks may take to report ready before the run is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// The share of the planned bursts the waker may leave out, in percent,
/// before the run no longer is the load that was asked for.
const MAX_SKIPPED_PCT: f64 = 1.0;
/// Set in the futex word when the run is over; the rest of the word is
/// the number of bursts released. One word for both: a task reads how
/// many bursts there were and whether more will come in one piece.
const STOP: u32 = 1 << 31;

/// Test hooks: failures on purpose, and a count of the tasks alive.
#[derive(Clone, Default)]
struct Hooks {
    /// Creating the task with this index fails.
    fail_spawn_at: Option<usize>,
    /// The task with this index panics at its first burst.
    panic_task: Option<usize>,
    /// The task with this index never reports ready.
    stall_task: Option<usize>,
    /// Shorter `READY_TIMEOUT`.
    ready_timeout: Option<Duration>,
    /// The task with this index, woken by a burst, gets to run only this
    /// much later: a wake-up the scheduler was slow to act on.
    slow_task: Option<(usize, Duration)>,
    /// The task with this index loses its CPU for this long right after
    /// its work, before it looks for new bursts.
    slow_after_work: Option<(usize, Duration)>,
    /// The waker oversleeps this release (counted from 0) by this much.
    slow_waker: Option<(u64, Duration)>,
    /// Tasks running: up on entry, down on exit, however they exit.
    alive: Arc<AtomicUsize>,
}

pub struct WakeBurst {
    /// Which of the suite's sizes this is: part of the name.
    pub size: &'static str,
    /// Tasks woken by each burst.
    pub tasks: usize,
    pub period: Duration,
    /// CPU time of work per task and burst.
    pub work: Duration,
}

impl Workload for WakeBurst {
    fn name(&self) -> String {
        format!("wake-burst/{}", self.size)
    }

    fn description(&self) -> String {
        format!(
            "{} threads woken at once every {} µs for {} µs of work each; time until each runs, until all run and \
             until all are done",
            self.tasks,
            self.period.as_micros(),
            self.work.as_micros()
        )
    }

    fn params(&self) -> serde_json::Value {
        json!({
            "tasks": self.tasks,
            "period_us": self.period.as_micros() as u64,
            "work_us": self.work.as_micros() as u64,
            "timer_slack_ns": TIMER_SLACK_NS,
        })
    }

    fn headline_metrics(&self) -> &'static [&'static str] {
        &["start_p50", "start_p99", "all_done_p99"]
    }

    fn progress_note(&self, r: &RunResult) -> Option<String> {
        Some(format!("overrun={:.1}%", r.metrics.get("overrun_pct")?.value))
    }

    fn run(&self, duration: Duration) -> RunResult {
        if self.tasks == 0 || self.period.is_zero() || self.work >= self.period {
            return RunResult {
                invalid_reason: Some("wake-burst needs at least one task and work shorter than the period".into()),
                ..Default::default()
            };
        }
        self.run_with(duration, &Hooks::default())
    }
}

/// What the waker and the tasks share.
struct Shared {
    /// The futex every task sleeps on: the number of bursts released,
    /// and `STOP` once no more will come.
    word: AtomicU32,
    /// When burst `b` was released, at index `b`; written before `word`
    /// becomes `b`. Index 0 is unused.
    released: Box<[AtomicU64]>,
    ready: AtomicUsize,
}

impl Shared {
    /// Tell every task that no more bursts will come, asleep or not.
    fn stop(&self) {
        self.word.fetch_or(STOP, Ordering::Release);
        let _ = futex::wake(&self.word, i32::MAX);
    }

    /// Bursts released so far, and whether that is all.
    fn bursts(&self) -> (u32, bool) {
        let word = self.word.load(Ordering::Acquire);
        (word & !STOP, word & STOP != 0)
    }
}

#[derive(Default)]
struct TaskOut {
    start: Recorder,
    /// Per burst, at its index: time to running and to done, plus one;
    /// zero for a burst this task took no part in.
    running: Vec<u64>,
    done: Vec<u64>,
    /// Bursts this task was woken by.
    ran: u64,
    /// Bursts released while it was still busy with an earlier one.
    overrun: u64,
    /// Bursts released while it was neither busy nor asleep.
    not_asleep: u64,
}

impl WakeBurst {
    fn run_with(&self, duration: Duration, hooks: &Hooks) -> RunResult {
        let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
        let (period, work) = (ns(self.period), ns(self.work));
        // Whole periods: the last burst has one to finish in.
        let bursts = (ns(duration) / period).max(1) as usize;
        let shared = Arc::new(Shared {
            word: AtomicU32::new(0),
            released: (0..bursts + 1).map(|_| AtomicU64::new(0)).collect(),
            ready: AtomicUsize::new(0),
        });
        let mut handles = Vec::with_capacity(self.tasks);
        let mut failed: Option<String> = None;
        for i in 0..self.tasks {
            let (sh, hooks) = (Arc::clone(&shared), hooks.clone());
            let body = move || -> Result<TaskOut, String> {
                let _alive = Alive::new(&hooks.alive);
                if hooks.stall_task != Some(i) {
                    sh.ready.fetch_add(1, Ordering::Release);
                }
                let mine = |hook: Option<(usize, Duration)>| hook.and_then(|(task, by)| (task == i).then_some(by));
                let slow = Slow { after_wake: mine(hooks.slow_task), after_work: mine(hooks.slow_after_work) };
                task(&sh, work, hooks.panic_task == Some(i), slow)
            };
            let spawned = if hooks.fail_spawn_at == Some(i) {
                Err(std::io::Error::other("cannot create a thread (test)"))
            } else {
                std::thread::Builder::new().name(format!("burst-{i}")).spawn(body)
            };
            match spawned {
                Ok(h) => handles.push(h),
                Err(e) => {
                    failed = Some(format!("spawning burst threads: {e}"));
                    break;
                }
            }
        }

        let give_up = Instant::now() + hooks.ready_timeout.unwrap_or(READY_TIMEOUT);
        while failed.is_none() && shared.ready.load(Ordering::Acquire) < self.tasks {
            if handles.iter().any(|h| h.is_finished()) {
                failed = Some("a burst thread stopped before the run started".into());
            } else if Instant::now() >= give_up {
                failed = Some("burst threads never became ready".into());
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let mut waker = Waker::default();
        if failed.is_none() {
            match wake_bursts(&shared, bursts, period, hooks.slow_waker) {
                Ok(w) => waker = w,
                Err(why) => failed = Some(why),
            }
        }
        shared.stop();

        // Every thread that was started is joined, whatever the others
        // did: none may still be at work when the next run begins.
        let mut outs = Vec::with_capacity(handles.len());
        for h in handles {
            match h.join() {
                Ok(Ok(out)) => outs.push(out),
                Ok(Err(why)) => failed = failed.or(Some(why)),
                Err(_) => failed = failed.or(Some("a burst thread panicked".into())),
            }
        }
        match failed {
            Some(why) => invalid(why),
            None => summarize(&outs, &waker, bursts as u64, period),
        }
    }
}

/// What the waker did.
#[derive(Default)]
struct Waker {
    /// Bursts released.
    released: u64,
    /// Releases that were already a period past when the waker got to
    /// them, and were left out.
    skipped: u64,
    /// Tasks the wake calls found asleep.
    woken: u64,
    /// How late the waker was for the bursts it released.
    late: Recorder,
    /// From the time a burst is released at to the wake call returning.
    call: Recorder,
}

/// Release `bursts` bursts, `period` apart, and wait one more period for
/// the last one.
fn wake_bursts(sh: &Shared, bursts: usize, period: u64, slow: Option<(u64, Duration)>) -> Result<Waker, String> {
    // Bursts when they are due, not up to 50 µs later; the calling
    // thread gets its own slack back afterwards.
    let _precise = PreciseTimers::new()?;
    let mut w = Waker::default();
    let first = now_ns() + ns(LEAD);
    for k in 0..bursts as u64 {
        let release = first + k * period;
        sleep_until(release);
        if let Some((_, by)) = slow.filter(|(at, _)| *at == k) {
            std::thread::sleep(by);
        }
        // The waker itself can be late. A release a whole period past
        // would follow the next one at once: leave it out.
        let at = now_ns();
        if at >= release + period {
            w.skipped += 1;
            continue;
        }
        // `at` is the release: what was checked, what is recorded as
        // late and what the tasks measure from are one reading.
        w.late.record(at.saturating_sub(release));
        w.released += 1;
        // The time first, then the number: a task that sees burst `b`
        // finds its time.
        sh.released[w.released as usize].store(at, Ordering::Release);
        sh.word.store(w.released as u32, Ordering::Release);
        w.woken += u64::from(futex::wake(&sh.word, i32::MAX).map_err(|e| format!("futex wake: {e}"))?);
        w.call.record(now_ns().saturating_sub(at));
    }
    sleep_until(first + bursts as u64 * period);
    Ok(w)
}

/// Delays on purpose, for tests.
#[derive(Clone, Copy, Default)]
struct Slow {
    after_wake: Option<Duration>,
    after_work: Option<Duration>,
}

/// One task: sleep until a burst is released, work, sleep again. Every
/// burst released ends up in exactly one of `ran`, `overrun` and
/// `not_asleep`.
fn task(sh: &Shared, work: u64, panic: bool, slow: Slow) -> Result<TaskOut, String> {
    let bursts = sh.released.len();
    let mut out = TaskOut { running: vec![0; bursts], done: vec![0; bursts], ..Default::default() };
    // The bursts this task knows of: what it sleeps on.
    let mut seen = 0u32;
    loop {
        // With `STOP` set the word differs from `seen` and this returns
        // at once, whenever the stop came: there is no moment at which a
        // task could go to sleep with nobody left to wake it.
        let slept = match futex::wait(&sh.word, seen) {
            Ok(Wait::Slept) => true,
            // The word changed before this task got to sleep.
            Ok(Wait::ValueChanged) => false,
            Ok(Wait::Interrupted | Wait::TimedOut) => continue,
            Err(e) => return Err(format!("futex wait: {e}")),
        };
        let (now, stop) = sh.bursts();
        if now == seen {
            // No burst since: the stop, or woken for nothing.
            if stop {
                return Ok(out);
            }
            continue;
        }
        if !slept {
            // Done with its work and not asleep yet when they were
            // released: not woken, and not late either.
            out.not_asleep += u64::from(now - seen);
            seen = now;
            if stop {
                return Ok(out);
            }
            continue;
        }
        // Asleep on `seen`, so woken by the burst after it, however many
        // have been released since and whether or not the run is over by
        // now: a wake-up that took long is the one not to lose.
        if let Some(by) = slow.after_wake {
            std::thread::sleep(by);
        }
        let at = now_ns();
        if panic {
            panic!("burst task failing on purpose");
        }
        let burst = seen + 1;
        let released = sh.released[burst as usize].load(Ordering::Acquire);
        let running = at.saturating_sub(released);
        burn(work);
        let finished = now_ns();
        let done = finished.saturating_sub(released);
        out.start.record(running);
        out.running[burst as usize] = running + 1;
        out.done[burst as usize] = done + 1;
        out.ran += 1;
        if let Some(by) = slow.after_work {
            std::thread::sleep(by);
        }
        // Bursts released since this one. Those released before the work
        // was finished found the task busy with it; those released after
        // found it done and not asleep yet. By their time, not by when
        // this task gets to look: it can lose its CPU right here.
        let (now, stop) = sh.bursts();
        for b in burst + 1..=now {
            if sh.released[b as usize].load(Ordering::Acquire) <= finished {
                out.overrun += 1;
            } else {
                out.not_asleep += 1;
            }
        }
        seen = now;
        if stop {
            return Ok(out);
        }
    }
}

fn summarize(outs: &[TaskOut], waker: &Waker, planned: u64, period: u64) -> RunResult {
    let window = (planned * period) as f64 / 1e9;
    let mut r = RunResult { duration_s: window, ..Default::default() };
    // The load is the bursts: a run that got clearly fewer of them than
    // planned is not the run another scheduler got.
    if waker.skipped as f64 * 100.0 > planned as f64 * MAX_SKIPPED_PCT {
        r.invalid_reason = Some(format!(
            "the waker was a whole period late for {} of {planned} bursts and left them out: not the load that was \
             asked for",
            waker.skipped
        ));
        return r;
    }
    let mut start = Recorder::default();
    let (mut ran, mut overrun, mut not_asleep) = (0, 0, 0);
    for o in outs {
        // Every burst released is one of the three for every task.
        if o.ran + o.overrun + o.not_asleep != waker.released {
            r.invalid_reason = Some(format!(
                "a task accounts for {} + {} + {} of {} bursts",
                o.ran, o.overrun, o.not_asleep, waker.released
            ));
            return r;
        }
        start.merge(&o.start);
        ran += o.ran;
        overrun += o.overrun;
        not_asleep += o.not_asleep;
    }
    if ran == 0 {
        r.invalid_reason = Some("no task was woken by a burst".into());
        return r;
    }
    // Per burst, only where the whole group took part.
    let (mut all_running, mut all_done) = (Recorder::default(), Recorder::default());
    let mut complete = 0u64;
    for b in 1..=waker.released as usize {
        if outs.iter().all(|o| o.running[b] != 0) {
            complete += 1;
            all_running.record(outs.iter().map(|o| o.running[b] - 1).max().unwrap_or(0));
            all_done.record(outs.iter().map(|o| o.done[b] - 1).max().unwrap_or(0));
        }
    }
    let due = waker.released * outs.len() as u64;
    let pops = [(POP_START, &start), (POP_ALL_RUNNING, &all_running), (POP_ALL_DONE, &all_done)];
    for (name, rec) in pops {
        let curve = population(&mut r.metrics, name, rec);
        if rec.count() > 0 {
            r.latency_histograms.insert(name.into(), rec.to_histogram("ns"));
        }
        if !curve.is_empty() {
            r.quantile_curves.insert(name.into(), curve);
        }
    }
    // Scored, like periodic-wake's missed deadlines: zero is a value, and
    // a baseline of zero leaves the change undefined.
    r.metrics.insert("overrun_pct".into(), Metric::lower(overrun as f64 / due as f64 * 100.0, "%"));
    // How late the waker was, and how long its wake calls took: what
    // the load looked like, not a quality of the tasks' wake-ups.
    for (name, rec) in [("release_late", &waker.late), ("wake_call", &waker.call)] {
        for (q, suffix) in [(0.5, "p50"), (0.99, "p99")] {
            if rec.count() >= min_samples_for(q) {
                let us = rec.quantile(q).unwrap_or(0.0) / 1000.0;
                r.metrics.insert(format!("{name}_{suffix}"), Metric::info(us, "us"));
            }
        }
        if let Some(max) = rec.max() {
            r.metrics.insert(format!("{name}_max"), Metric::info(max as f64 / 1000.0, "us"));
        }
    }
    r.windows.insert("latency".into(), Window { start_s: 0.0, end_s: window });
    let c = &mut r.counters;
    c.insert("bursts_planned".into(), planned);
    c.insert("bursts".into(), waker.released);
    c.insert("bursts_complete".into(), complete);
    c.insert("bursts_skipped".into(), waker.skipped);
    c.insert("wakeups_due".into(), due);
    c.insert("wakeups_ran".into(), ran);
    c.insert("wakeups_overrun".into(), overrun);
    c.insert("wakeups_not_asleep".into(), not_asleep);
    c.insert("tasks_found_asleep".into(), waker.woken);
    r.quantile_unit = Some("us".into());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::Better;

    fn quick(tasks: usize) -> WakeBurst {
        WakeBurst { size: "full", tasks, period: Duration::from_millis(2), work: Duration::from_micros(50) }
    }

    /// A run for a test of something else than the waker's lateness: on
    /// a busy test machine the waker can be late on its own, and such a
    /// run is rightly invalid. Try again a few times.
    fn run(w: &WakeBurst, duration: Duration, hooks: &Hooks) -> RunResult {
        let mut r = w.run_with(duration, hooks);
        for _ in 0..5 {
            if !r.invalid_reason.as_deref().is_some_and(|why| why.contains("left them out")) {
                break;
            }
            r = w.run_with(duration, hooks);
        }
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        r
    }

    /// Every burst released is, for every task, one of three things.
    fn accounted(r: &RunResult) {
        let c = &r.counters;
        assert_eq!(c["bursts"] + c["bursts_skipped"], c["bursts_planned"], "{c:?}");
        assert_eq!(c["wakeups_due"], c["wakeups_ran"] + c["wakeups_overrun"] + c["wakeups_not_asleep"], "{c:?}");
        // What the wake calls woke is what the tasks measured.
        assert_eq!(c["tasks_found_asleep"], c["wakeups_ran"], "{c:?}");
        assert_eq!(r.latency_histograms[POP_START].count, c["wakeups_ran"]);
    }

    #[test]
    fn bursts_are_counted_and_timed() {
        let w = quick(2);
        let r = run(&w, Duration::from_millis(300), &Hooks::default());
        let c = &r.counters;
        // 300 ms, a burst every 2 ms: 150 planned whatever the machine does.
        assert_eq!(c["bursts_planned"], 150);
        assert_eq!(c["wakeups_due"], c["bursts"] * 2);
        accounted(&r);
        // One sample per complete burst.
        assert!(c["bursts_complete"] <= c["bursts"]);
        assert_eq!(r.latency_histograms[POP_ALL_RUNNING].count, c["bursts_complete"]);
        assert_eq!(r.latency_histograms[POP_ALL_DONE].count, c["bursts_complete"]);
        // The whole group is running no sooner than its typical member,
        // and done no sooner than running plus the work.
        let m = |name: &str| r.metrics[name].value;
        assert!(m("all_running_p50") >= m("start_p50"), "{:?}", r.metrics);
        assert!(m("all_done_p50") >= 50.0 && m("all_done_p50") >= m("all_running_p50"), "{:?}", r.metrics);
        assert_eq!(r.metrics["start_p50"].better, Better::Lower);
        assert!(r.metrics["start_max"].extreme && r.metrics["all_done_max"].extreme);
        // Scored: only the wake-ups that found the task busy with an
        // earlier burst.
        let overrun = &r.metrics["overrun_pct"];
        assert_eq!((overrun.better, overrun.unit.as_str()), (Better::Lower, "%"));
        let expected = c["wakeups_overrun"] as f64 / c["wakeups_due"] as f64 * 100.0;
        assert!((overrun.value - expected).abs() < 1e-9 && (0.0..=100.0).contains(&overrun.value));
        // How late the waker was describes the load, and is not scored.
        for name in ["release_late_p50", "release_late_max", "wake_call_p50", "wake_call_max"] {
            assert_eq!((r.metrics[name].better, r.metrics[name].unit.as_str()), (Better::Neither, "us"), "{name}");
        }
        assert!(m("release_late_p50") < 2000.0, "a release a period late is left out");
        // 300 starts carry a p50 and not a p99; 150 bursts likewise.
        assert!(!r.metrics.contains_key("start_p99") && !r.metrics.contains_key("all_done_p99"));
        assert!(!r.metrics.contains_key("release_late_p99") && !r.metrics.contains_key("wake_call_p99"));
        // The tasks measure from before the call, so none starts in less
        // than nothing, and the group is not running before the call is
        // well under way.
        assert!(m("wake_call_max") >= m("wake_call_p50") && m("wake_call_p50") > 0.0, "{:?}", r.metrics);
        assert!(r.quantile_curves[POP_START].iter().any(|(q, _)| *q == 0.5));
        assert_eq!(r.quantile_unit.as_deref(), Some("us"));
        assert_eq!(r.duration_s, 0.3);
    }

    #[test]
    fn parameters_identify_the_workload() {
        let w = WakeBurst { size: "double", ..quick(4) };
        assert_eq!(w.name(), "wake-burst/double");
        assert_eq!(w.params(), json!({ "tasks": 4, "period_us": 2000, "work_us": 50, "timer_slack_ns": 1 }));
        assert_eq!(w.headline_metrics(), ["start_p50", "start_p99", "all_done_p99"]);
        // Work that does not fit its period is refused, not measured.
        let bad = WakeBurst { work: Duration::from_millis(2), ..quick(1) };
        assert!(bad.run(Duration::from_millis(10)).invalid_reason.unwrap().contains("shorter than the period"));
        // A duration under one period still plans one burst.
        let r = run(&quick(1), Duration::from_millis(1), &Hooks::default());
        assert_eq!((r.counters["bursts_planned"], r.counters["bursts"]), (1, 1));
    }

    /// The waker's thread gets the timer slack it came with back.
    #[test]
    fn the_calling_thread_keeps_its_timer_slack() {
        std::thread::spawn(|| {
            // SAFETY: PR_GET_TIMERSLACK takes no further argument.
            let slack = || unsafe { libc::prctl(libc::PR_GET_TIMERSLACK) };
            let before = slack();
            run(&quick(1), Duration::from_millis(10), &Hooks::default());
            assert_eq!(slack(), before);
        })
        .join()
        .unwrap();
    }

    /// A task still at work when the next burst is released is not woken
    /// by it: the burst is counted as overrun, and no latency is made up
    /// for it.
    #[test]
    fn a_task_at_work_is_not_woken() {
        // 9 ms of CPU time per burst, a burst every 2 ms: while it works
        // on one, four more are released. `run` refuses these parameters;
        // the mechanism is what is tested. The bounds leave room for a
        // waker that is late itself on a busy test machine.
        let w = WakeBurst { size: "full", tasks: 1, period: Duration::from_millis(2), work: Duration::from_millis(9) };
        let r = run(&w, Duration::from_millis(150), &Hooks::default());
        let c = &r.counters;
        assert!(c["wakeups_ran"] >= 3, "{c:?}");
        assert!(c["wakeups_overrun"] >= c["wakeups_ran"], "{c:?}");
        accounted(&r);
        assert!(r.metrics["overrun_pct"].value >= 50.0);
        // Only the bursts it slept for have a sample: the time since a
        // burst that passed during the work is never taken for a wake-up.
        assert_eq!(c["bursts_complete"], c["wakeups_ran"]);
        assert!(r.metrics["all_done_max"].value >= 9000.0);
    }

    /// The run ends while a task is at work: the task must not go back to
    /// sleep with nobody left to wake it.
    #[test]
    fn a_task_at_work_when_the_run_ends_leaves() {
        let w = WakeBurst { size: "full", tasks: 1, period: Duration::from_millis(2), work: Duration::from_millis(40) };
        let hooks = Hooks::default();
        let r = run(&w, Duration::from_millis(6), &hooks);
        assert_eq!(hooks.alive.load(Ordering::SeqCst), 0);
        // Woken by the first burst, and still at it when the others came.
        let c = &r.counters;
        assert_eq!((c["wakeups_ran"], c["wakeups_overrun"], c["wakeups_not_asleep"]), (1, 2, 0), "{c:?}");
        accounted(&r);
        assert!(r.metrics["all_done_max"].value >= 40_000.0);
    }

    /// A task woken by a burst that gets a CPU only after the run is over
    /// was woken all the same: its wake-up is measured, long as it was,
    /// and is not counted as a task that was still at work. Losing it
    /// would lose exactly the wake-ups the benchmark is after.
    #[test]
    fn a_wakeup_acted_on_after_the_run_is_over_is_measured() {
        let w = quick(2);
        // Three bursts in 6 ms; the first task runs 30 ms after it was
        // woken by the first of them.
        let hooks = Hooks { slow_task: Some((0, Duration::from_millis(30))), ..Default::default() };
        let r = run(&w, Duration::from_millis(6), &hooks);
        assert_eq!(hooks.alive.load(Ordering::SeqCst), 0);
        let c = &r.counters;
        assert_eq!((c["bursts"], c["wakeups_due"]), (3, 6), "{c:?}");
        accounted(&r);
        // The slow one: woken once, and the two bursts that came while it
        // waited for its CPU found it busy. The other: three wake-ups,
        // unless one found it between its work and its sleep.
        assert_eq!(c["wakeups_overrun"], 2, "{c:?}");
        assert_eq!(c["wakeups_ran"] + c["wakeups_not_asleep"], 4, "{c:?}");
        // Its 30 ms are in the results, for the task and for its group.
        assert!(r.metrics["start_max"].value >= 30_000.0, "{:?}", r.metrics["start_max"]);
        assert!(c["bursts_complete"] >= 1 && r.metrics["all_running_max"].value >= 30_000.0, "{c:?}");
    }

    /// A burst released after the task finished its work, and before it
    /// got to look for one, found a task that was done: not asleep yet,
    /// and not an overrun, which is scored. A task that loses its CPU
    /// between its work and its look must not pass for one still working.
    #[test]
    fn a_burst_after_the_work_was_done_is_not_an_overrun() {
        // Three bursts, 20 ms apart: wide enough that neither 50 µs of
        // work nor the waker being late on a busy test machine reaches
        // the next burst by itself. Woken by the first, the task is done
        // at once and off its CPU for 30 ms: the second burst comes
        // meanwhile.
        let w = WakeBurst { period: Duration::from_millis(20), ..quick(1) };
        let hooks = Hooks { slow_after_work: Some((0, Duration::from_millis(30))), ..Default::default() };
        let r = run(&w, Duration::from_millis(60), &hooks);
        let c = &r.counters;
        accounted(&r);
        assert_eq!(c["wakeups_overrun"], 0, "{c:?}");
        assert!(c["wakeups_not_asleep"] >= 1 && c["wakeups_ran"] >= 1, "{c:?}");
        assert_eq!(r.metrics["overrun_pct"].value, 0.0);

        // The same delay before the work is done is an overrun: what
        // differs is only when the burst came.
        let hooks = Hooks { slow_task: Some((0, Duration::from_millis(30))), ..Default::default() };
        let r = run(&w, Duration::from_millis(60), &hooks);
        accounted(&r);
        assert!(r.counters["wakeups_overrun"] >= 1, "{:?}", r.counters);
    }

    /// A waker that is late itself delivers less load. A little of that
    /// is counted and shown; more makes the run invalid instead of
    /// letting it pass for a run with fewer bursts and nothing lost.
    #[test]
    fn bursts_the_waker_leaves_out_are_not_a_better_result() {
        let w = quick(2);
        // 300 bursts planned. The waker oversleeps one release by more
        // than a period: that one is left out, the next comes late.
        let hooks = Hooks { slow_waker: Some((5, Duration::from_micros(2500))), ..Default::default() };
        let r = run(&w, Duration::from_millis(600), &hooks);
        let c = &r.counters;
        assert_eq!(c["bursts_planned"], 300);
        assert!((1..=3).contains(&c["bursts_skipped"]), "{c:?}");
        // What was not released is not due, and is not hidden either.
        assert_eq!(c["wakeups_due"], c["bursts"] * 2);
        accounted(&r);
        assert!(r.metrics["release_late_max"].value >= 400.0, "{:?}", r.metrics["release_late_max"]);

        // 50 ms: some 25 of 300 releases gone. Not the load asked for.
        let hooks = Hooks { slow_waker: Some((5, Duration::from_millis(50))), ..Default::default() };
        let r = w.run_with(Duration::from_millis(600), &hooks);
        let why = r.invalid_reason.expect("invalid");
        assert!(why.contains("of 300 bursts and left them out"), "{why}");
        assert!(r.metrics.is_empty(), "nothing to compare: {:?}", r.metrics);
        assert_eq!(hooks.alive.load(Ordering::SeqCst), 0);
    }

    /// Whatever fails, the run returns, says why, and leaves no thread
    /// behind.
    #[test]
    fn failures_end_the_run_and_leave_no_thread_behind() {
        let w = quick(4);
        let run = |hooks: Hooks| {
            let alive = Arc::clone(&hooks.alive);
            let began = Instant::now();
            let r = w.run_with(Duration::from_millis(100), &hooks);
            (r.invalid_reason.expect("invalid"), began.elapsed(), alive.load(Ordering::SeqCst))
        };

        // The third thread cannot be created: the two asleep are woken,
        // told to leave and joined.
        let (why, took, alive) = run(Hooks { fail_spawn_at: Some(2), ..Default::default() });
        assert!(why.contains("spawning burst threads"), "{why}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(alive, 0);
        let (why, _, alive) = run(Hooks { fail_spawn_at: Some(0), ..Default::default() });
        assert!(why.contains("spawning burst threads") && alive == 0, "{why}");

        // One task panics at its first burst: the others run to the end
        // and are joined before the result is returned.
        let (why, took, alive) = run(Hooks { panic_task: Some(1), ..Default::default() });
        assert_eq!(why, "a burst thread panicked");
        assert!(took >= Duration::from_millis(100), "the others ran to the end: {took:?}");
        assert_eq!(alive, 0);

        // A task that never reports ready: abandoned after the timeout,
        // and the stalled task, asleep like the others, leaves too.
        let hooks = Hooks { stall_task: Some(3), ready_timeout: Some(Duration::from_millis(50)), ..Default::default() };
        let (why, took, alive) = run(hooks);
        assert_eq!(why, "burst threads never became ready");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(alive, 0);

        // And a run that works leaves none behind either.
        let hooks = Hooks::default();
        let alive = Arc::clone(&hooks.alive);
        super::tests::run(&w, Duration::from_millis(50), &hooks);
        assert_eq!(alive.load(Ordering::SeqCst), 0);
    }
}
