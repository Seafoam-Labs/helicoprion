//! idle-wake: a task woken by another on a machine that has gone to
//! sleep, and how long it takes to run.
//!
//! The other wake-up workloads keep the machine awake: wake-pingpong and
//! periodic-wake never leave a CPU idle for long, and wake-burst asks the
//! kernel outright to stay out of deep idle states, because the time to
//! leave one swamped what it measures. Here that time is the subject. A
//! CPU that has been idle for a few milliseconds is in a state it takes
//! tens or hundreds of microseconds to leave, and whether a wake-up pays
//! for that is partly the scheduler's choice: the task can be run on the
//! CPU of the thread that wakes it, which is awake, on the CPU it last
//! ran on, which is asleep, or on any other.
//!
//! # What is measured
//!
//! Some pairs of threads. The waker of a pair sleeps on a timer, long
//! enough for the machine to go idle (`period`, about 20 ms), then notes
//! the time and wakes its partner with one `FUTEX_WAKE`. The partner
//! notes the time as soon as it runs and goes back to sleep.
//!
//! * `wake`: from the waker's reading of the clock, right before its
//!   call, to the woken task running. Scored, lower is better.
//! * `timer_late`: how long after the instant its timer was set for the
//!   waker read the clock, for every wake-up it sent. This is the other
//!   wake-up of each round, by a timer and not by a thread, on a machine
//!   just as asleep, and where it was seen before (the waker of
//!   wake-burst) it was the larger of the two by far. Informational: it
//!   holds the timer's own lateness, the time to leave the idle state,
//!   the scheduler's time to run the waker and a few instructions of
//!   this workload, and how much of that a scheduler decides is not
//!   known. It is a population of its own. Its quantiles and those of
//!   `wake` are not to be added: the slowest of one need not come with
//!   the slowest of the other, and a sum of two p99 is no p99 of
//!   anything.
//! * `same_cpu_pct`: of the wake-ups measured, how many ran the task on
//!   the CPU its waker was on when it called. `prev_cpu_pct`: how many
//!   ran it on the CPU it had last run on. Informational: where a
//!   scheduler puts the task is how it gets to its delay, not a quality.
//!   Both are readings of the CPU at two instants and say where the
//!   threads were then, not what the kernel did in between.
//! * `idle_pct` and `idle_deepest_pct`: the share of the CPUs' time in
//!   the window that the kernel's idle accounting has as idle, and as
//!   spent in the idle state that takes longest to leave. Informational,
//!   and the check that the run is what it says: with a small share in
//!   the deepest state, the machine did not go to sleep and the wake-ups
//!   were not from deep idle. Absent where the kernel has no idle states
//!   to report, as in most virtual machines. The kernel adds a stay in
//!   an idle state to its count when the stay ends, so one that began
//!   before the window is counted whole and one still going on is not
//!   counted: the shares are rough, can come out a little over 100%,
//!   and say that the machine slept, not for exactly how long. Read
//!   before the first
//!   wake-up and after the last, so the half-idle moments of starting
//!   and stopping are in it, and nothing is read in between. It covers
//!   every CPU that reports idle states, also ones the benchmark may
//!   not run on: under a cpuset it says how deep the machine slept, not
//!   how deep the CPUs of the tasks did.
//!
//! The suite has two cases, which differ in what the waker does next:
//!
//! * `idle-wake/handoff`: the waker goes back to sleep at once, as one
//!   that hands a piece of work on. Its CPU is free a moment later, so
//!   running the woken task there costs little.
//! * `idle-wake/busy`: the waker works for a while after the call
//!   (`waker_work`), as one that hands out work and goes on with its
//!   own. Running the woken task on that CPU means taking it from the
//!   waker or waiting; sending it elsewhere means waking a CPU up.
//!
//! # Timing
//!
//! The wakers' timers are absolute and their slack is 1 ns, so that the
//! wake-ups come at the rate the parameters say; when a waker is late
//! is not measured, only what happens from its call on. The pairs'
//! phases are spread over the period, so that each wake-up finds the
//! machine as idle as the others left it. The period is no whole number
//! of the usual timer ticks (see periodic-wake for why that matters).
//!
//! A wake-up is measured when the task was asleep and is woken by this
//! call alone. One that found the task not asleep yet
//! (`wakes_not_asleep`) or that the task only saw together with the next
//! (`wakes_overrun`) is counted and left out: neither is a wake-up from
//! sleep with a time of its own. With 20 ms between them both should be
//! zero. The time a task takes for a wake-up is the one written for that
//! wake-up's number (see `Pair`), and one that would lie after the
//! task's own reading of the clock is not used (`wakes_mistimed`).
//!
//! # How many samples
//!
//! A long period is what lets the machine sleep, and it makes samples
//! scarce: about 49 wake-ups a second from each pair. The quantiles
//! follow the rule of every workload (`samples::min_samples_for`): a
//! p99 needs a thousand timed wake-ups, and a run with fewer has a
//! typical delay and a largest one, and no tail in between. That is not
//! relaxed here. A run gets more samples from more pairs, which the
//! suite takes from the number of CPUs, or from more time.
//!
//! The wake-ups are due at fixed instants, `wakes_planned` of them in
//! the window. A waker that comes back from its timer late sends fewer,
//! and one that comes back after the end of the window sends none for
//! it: `wakes_sent`, and `wakes_per_s`. That is shown and not limited.
//! A scheduler under which the wakers run late was asked for less, and
//! a reader should see it before comparing delays.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use helicoprion_core::hist::Recorder;
use helicoprion_core::{Metric, RunResult, Window};
use serde_json::json;

use crate::Workload;
use crate::clock::{Alive, TIMER_SLACK_NS, burn, now_ns, ns, set_timer_slack, sleep_until};
use crate::futex::{self, Wait};
use crate::idle::Residency;
use crate::samples::{min_samples_for, population};
use crate::schedstat::{self, Collector};

pub const POP_WAKE: &str = "wake";
pub const POP_TIMER_LATE: &str = "timer_late";

/// Start not fixed yet.
const NOT_STARTED: u64 = 0;
/// The run was abandoned before it started; wakers exit.
const ABANDONED: u64 = u64::MAX;
/// From fixing the start to the first wake-up: time for every thread to
/// go to sleep, and for the machine to go idle once.
const LEAD: Duration = Duration::from_millis(50);
/// How long the threads may take to report ready before the run is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Set in a pair's futex word to end its task.
const STOP: u32 = 1 << 31;

/// Test hooks.
#[derive(Clone, Default)]
struct Hooks {
    /// Creating the thread with this index fails (wakers and tasks
    /// alternate, the waker of a pair first).
    fail_spawn_at: Option<usize>,
    /// Shorter `READY_TIMEOUT`.
    ready_timeout: Option<Duration>,
    /// Threads running.
    alive: Arc<AtomicUsize>,
}

pub struct IdleWake {
    /// Which of the suite's cases this is: part of the name.
    pub case: &'static str,
    /// Pairs of a waker and a task.
    pub pairs: usize,
    /// Between two wake-ups of one pair.
    pub period: Duration,
    /// CPU time the waker works for after its call; zero to sleep at once.
    pub waker_work: Duration,
    /// Read every thread's scheduler statistics.
    pub sched_diag: bool,
}

impl Workload for IdleWake {
    fn name(&self) -> String {
        format!("idle-wake/{}", self.case)
    }

    fn description(&self) -> String {
        let after = if self.waker_work.is_zero() {
            "goes back to sleep".to_string()
        } else {
            format!("works for {} µs", self.waker_work.as_micros())
        };
        format!(
            "{} pairs of threads on an idle machine: every {} µs one wakes the other and {after}; how long the \
             woken one takes to run",
            self.pairs,
            self.period.as_micros()
        )
    }

    fn params(&self) -> serde_json::Value {
        let params = json!({
            "pairs": self.pairs,
            "period_us": self.period.as_micros() as u64,
            "waker_work_us": self.waker_work.as_micros() as u64,
            "timer_slack_ns": TIMER_SLACK_NS,
        });
        schedstat::mark(params, self.sched_diag)
    }

    fn headline_metrics(&self) -> &'static [&'static str] {
        &["wake_p50", "wake_p99", "same_cpu_pct"]
    }

    fn run(&self, duration: Duration) -> RunResult {
        self.run_with(duration, &Hooks::default())
    }
}

/// What a waker and its task share.
///
/// The number of a wake-up and its time are two words, and the task
/// reads them at some later moment: the time it reads has to be the one
/// of the wake-up it counts. So the times are kept by number, in two
/// places used in turn. Wake-up `n` writes place `n % 2` before the word
/// becomes `n`, and that place is next written for `n + 2`, after the
/// word has become `n + 1`. A task that reads the word, then the place,
/// then the word again and finds it unchanged has the time of that
/// wake-up; if the word has moved, it may not, and takes no time.
#[derive(Default)]
struct Pair {
    /// Wake-ups sent so far; `STOP` ends the task.
    word: AtomicU32,
    /// When the waker read the clock for a wake-up, and the CPU it was
    /// on then, by the wake-up's number.
    released_at: [AtomicU64; 2],
    waker_cpu: [AtomicU32; 2],
}

impl Pair {
    /// Announce the next wake-up: its time and CPU, then its number.
    /// Returns the number.
    fn release(&self, at: u64, cpu: u32) -> u32 {
        let n = (self.word.load(Ordering::Relaxed) & !STOP).wrapping_add(1);
        let place = (n % 2) as usize;
        self.waker_cpu[place].store(cpu, Ordering::Relaxed);
        self.released_at[place].store(at, Ordering::Release);
        self.word.fetch_add(1, Ordering::Release);
        n
    }

    /// The time and CPU of wake-up `n`, if `n` is still the latest one.
    fn released(&self, n: u32) -> Option<(u64, u32)> {
        let place = (n % 2) as usize;
        let at = self.released_at[place].load(Ordering::Acquire);
        let cpu = self.waker_cpu[place].load(Ordering::Relaxed);
        (self.word.load(Ordering::Acquire) == n).then_some((at, cpu))
    }
}

#[derive(Default)]
struct WakerOut {
    /// Wake-ups due in the window, had the waker never been late.
    planned: u64,
    sent: u64,
    /// `FUTEX_WAKE` woke nobody: the task was not waiting.
    woke_nobody: u64,
    /// How long after the instant its timer was set for the waker read
    /// the clock, for every wake-up it sent.
    timer_late: Recorder,
}

#[derive(Default)]
struct TaskOut {
    wake: Recorder,
    not_asleep: u64,
    overrun: u64,
    /// The time read for a wake-up was later than the task's own
    /// reading: not a time of that wake-up. Should not happen.
    mistimed: u64,
    same_cpu: u64,
    prev_cpu: u64,
}

fn current_cpu() -> u32 {
    // SAFETY: sched_getcpu takes no arguments.
    let cpu = unsafe { libc::sched_getcpu() };
    u32::try_from(cpu).unwrap_or(u32::MAX)
}

/// How many wake-ups a waker that is never late sends: those due at
/// `first + k * period` before `end`.
fn planned(first: u64, end: u64, period: u64) -> u64 {
    end.saturating_sub(first).div_ceil(period)
}

/// One waker: wake-ups at `first + k * period` until `end`.
fn waker(pair: &Pair, first: u64, end: u64, period: u64, work: u64) -> Result<WakerOut, String> {
    let mut out = WakerOut { planned: planned(first, end, period), ..Default::default() };
    let mut k = 0u64;
    loop {
        let release = first + k * period;
        if release >= end {
            return Ok(out);
        }
        sleep_until(release);
        let cpu = current_cpu();
        // The clock last: nothing but the call lies between this
        // reading and the task being woken.
        let now = now_ns();
        // A waker that comes back after the end of the window does not
        // send what was due inside it: the window is what is measured.
        if now >= end {
            return Ok(out);
        }
        let late = now.saturating_sub(release);
        pair.release(now, cpu);
        let woken = futex::wake(&pair.word, 1).map_err(|e| format!("futex wake: {e}"))?;
        out.sent += 1;
        out.woke_nobody += u64::from(woken == 0);
        if work > 0 {
            burn(work);
        }
        // Noted down here and not where it was read: between the
        // reading of the clock and the call lies the time `wake`
        // measures, and before the work the time a busy waker's CPU is
        // free, and neither is to be longer for this bookkeeping. For
        // the wake-ups that are sent, and only those: one that was
        // skipped or fell past the end did not happen and has no time.
        out.timer_late.record(late);
        // A waker that was late by a period or more skips what it missed.
        k = (now_ns().saturating_sub(first) / period).max(k) + 1;
    }
}

/// What became of the wake-ups a task found on waking.
#[derive(Debug, PartialEq)]
enum Found {
    /// One, from sleep, with its own time: so long after the waker's
    /// reading of the clock, on a CPU that is or is not the waker's.
    Timed { delay: u64, waker_cpu: u32 },
    /// So many that the task was not asleep for.
    NotAsleep(u64),
    /// So many that have no time of their own: more than one went by,
    /// or another was sent while this one's time was being read.
    Overrun(u64),
    /// One whose time is after the task's own reading of the clock.
    Mistimed,
}

/// Sort the wake-ups numbered after `seen` up to `word`, found by a
/// task that returned from its wait at `now`; `slept` says whether it
/// had been asleep. `released` is `Pair::released`.
fn found(slept: bool, seen: u32, word: u32, now: u64, released: impl FnOnce(u32) -> Option<(u64, u32)>) -> Found {
    let new = u64::from(word.wrapping_sub(seen));
    if !slept {
        return Found::NotAsleep(new);
    }
    if new != 1 {
        return Found::Overrun(new);
    }
    match released(word) {
        // The word moved on while the time was read: the next wake-up
        // has been sent, and this one's place may be the next one's by
        // now. The task finds that one on its next wait, not asleep.
        None => Found::Overrun(1),
        Some((at, _)) if at > now => Found::Mistimed,
        Some((at, waker_cpu)) => Found::Timed { delay: now - at, waker_cpu },
    }
}

/// One task: sleep until woken, note when and where it runs, again.
fn task(pair: &Pair) -> Result<TaskOut, String> {
    let mut out = TaskOut::default();
    let mut seen = 0u32;
    let mut last_cpu = current_cpu();
    loop {
        let slept = futex::wait(&pair.word, seen).map_err(|e| format!("futex wait: {e}"))?;
        // The clock first: this is the instant the task runs.
        let now = now_ns();
        let word = pair.word.load(Ordering::Acquire);
        if word & STOP != 0 {
            return Ok(out);
        }
        if word == seen {
            // Woken for nothing, or interrupted.
            continue;
        }
        let cpu = current_cpu();
        match found(slept == Wait::Slept, seen, word, now, |n| pair.released(n)) {
            Found::Timed { delay, waker_cpu } => {
                out.wake.record(delay);
                out.same_cpu += u64::from(cpu == waker_cpu);
                out.prev_cpu += u64::from(cpu == last_cpu);
            }
            Found::NotAsleep(n) => out.not_asleep += n,
            Found::Overrun(n) => out.overrun += n,
            Found::Mistimed => out.mistimed += 1,
        }
        (seen, last_cpu) = (word, cpu);
    }
}

impl IdleWake {
    fn run_with(&self, duration: Duration, hooks: &Hooks) -> RunResult {
        let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
        if self.pairs == 0 || self.period.is_zero() || self.waker_work >= self.period {
            return invalid("idle-wake needs at least one pair and a waker that works for less than the period".into());
        }
        let stats = Arc::new(Collector::new(self.sched_diag));
        let start = Arc::new(AtomicU64::new(NOT_STARTED));
        let ready = Arc::new(AtomicUsize::new(0));
        let pairs: Arc<[Pair]> = (0..self.pairs).map(|_| Pair::default()).collect();
        let (period, work, span) = (ns(self.period), ns(self.waker_work), ns(duration));
        let mut wakers = Vec::with_capacity(self.pairs);
        let mut tasks = Vec::with_capacity(self.pairs);
        let mut failed: Option<String> = None;
        let spawn_fails = |index: usize| hooks.fail_spawn_at == Some(index);
        for i in 0..self.pairs {
            // Phases spread over one period.
            let phase = period * i as u64 / self.pairs as u64;
            let (w_pairs, w_start, w_ready, w_stats, w_alive) =
                (Arc::clone(&pairs), Arc::clone(&start), Arc::clone(&ready), Arc::clone(&stats), hooks.alive.clone());
            let waker_body = move || -> Result<WakerOut, String> {
                let _alive = Alive::new(&w_alive);
                set_timer_slack()?;
                w_ready.fetch_add(1, Ordering::Release);
                let t0 = loop {
                    match w_start.load(Ordering::Acquire) {
                        NOT_STARTED => std::thread::sleep(Duration::from_micros(200)),
                        ABANDONED => return Ok(WakerOut::default()),
                        t0 => break t0,
                    }
                };
                let _watching = w_stats.watch();
                waker(&w_pairs[i], t0 + phase, t0 + span, period, work)
            };
            let (t_pairs, t_ready, t_stats, t_alive) =
                (Arc::clone(&pairs), Arc::clone(&ready), Arc::clone(&stats), hooks.alive.clone());
            let task_body = move || -> Result<TaskOut, String> {
                let _alive = Alive::new(&t_alive);
                t_ready.fetch_add(1, Ordering::Release);
                let _watching = t_stats.watch();
                task(&t_pairs[i])
            };
            let spawned = (|| -> std::io::Result<()> {
                let fail = || std::io::Error::other("cannot create a thread (test)");
                if spawn_fails(2 * i) {
                    return Err(fail());
                }
                wakers.push(std::thread::Builder::new().name(format!("idle-waker-{i}")).spawn(waker_body)?);
                if spawn_fails(2 * i + 1) {
                    return Err(fail());
                }
                tasks.push(std::thread::Builder::new().name(format!("idle-task-{i}")).spawn(task_body)?);
                Ok(())
            })();
            if let Err(e) = spawned {
                failed = Some(format!("spawning idle-wake threads: {e}"));
                break;
            }
        }

        let give_up = Instant::now() + hooks.ready_timeout.unwrap_or(READY_TIMEOUT);
        while failed.is_none() && ready.load(Ordering::Acquire) < 2 * self.pairs {
            if wakers.iter().any(|h| h.is_finished()) || tasks.iter().any(|h| h.is_finished()) {
                failed = Some("an idle-wake thread stopped before the run started".into());
            } else if Instant::now() >= give_up {
                failed = Some("idle-wake threads never became ready".into());
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let mut idle = None;
        if failed.is_some() {
            start.store(ABANDONED, Ordering::Release);
        } else {
            // Idle accounting from before the start to after the end:
            // not read while wake-ups are being measured.
            let before = Residency::read();
            let t0 = now_ns() + ns(LEAD);
            start.store(t0, Ordering::Release);
            sleep_until(t0 + span);
            idle = before.zip(Residency::read()).and_then(|(a, b)| b.since(&a));
        }

        // The wakers end by themselves; then every task is told to.
        let mut waker_outs = Vec::with_capacity(wakers.len());
        for h in wakers {
            match h.join() {
                Ok(Ok(out)) => waker_outs.push(out),
                Ok(Err(why)) => failed = failed.or(Some(why)),
                Err(_) => failed = failed.or(Some("an idle-wake thread panicked".into())),
            }
        }
        for pair in pairs.iter() {
            pair.word.fetch_or(STOP, Ordering::Release);
            let _ = futex::wake(&pair.word, i32::MAX);
        }
        let mut task_outs = Vec::with_capacity(tasks.len());
        for h in tasks {
            match h.join() {
                Ok(Ok(out)) => task_outs.push(out),
                Ok(Err(why)) => failed = failed.or(Some(why)),
                Err(_) => failed = failed.or(Some("an idle-wake thread panicked".into())),
            }
        }
        match failed {
            Some(why) => invalid(why),
            None => {
                let mut r = summarize(&waker_outs, &task_outs, duration, idle);
                r.windows.insert("latency".into(), Window { start_s: 0.0, end_s: duration.as_secs_f64() });
                stats.record(&mut r);
                r
            }
        }
    }
}

fn summarize(wakers: &[WakerOut], tasks: &[TaskOut], duration: Duration, idle: Option<(f64, f64)>) -> RunResult {
    let mut wake = Recorder::default();
    let (mut not_asleep, mut overrun, mut mistimed, mut same_cpu, mut prev_cpu) = (0, 0, 0, 0, 0);
    for t in tasks {
        wake.merge(&t.wake);
        not_asleep += t.not_asleep;
        overrun += t.overrun;
        mistimed += t.mistimed;
        same_cpu += t.same_cpu;
        prev_cpu += t.prev_cpu;
    }
    let sent: u64 = wakers.iter().map(|w| w.sent).sum();
    let planned: u64 = wakers.iter().map(|w| w.planned).sum();
    let woke_nobody: u64 = wakers.iter().map(|w| w.woke_nobody).sum();
    let measured = wake.count();
    let mut r = RunResult { duration_s: duration.as_secs_f64(), ..Default::default() };
    if measured == 0 {
        r.invalid_reason = Some("no wake-up was measured".into());
        return r;
    }
    let curve = population(&mut r.metrics, POP_WAKE, &wake);
    let pct = |part: u64| part as f64 / measured as f64 * 100.0;
    // Where the task ran: how a scheduler gets to its delay, not a
    // quality of it.
    r.metrics.insert("same_cpu_pct".into(), Metric::info(pct(same_cpu), "%"));
    r.metrics.insert("prev_cpu_pct".into(), Metric::info(pct(prev_cpu), "%"));
    // How much was asked of the scheduler: a waker that runs late sends
    // fewer wake-ups, and one that is late past the end of the window
    // does not send at all. Shown, so that two sides can be seen to have
    // been given the same thing to do; no limit is drawn.
    r.metrics.insert("wakes_per_s".into(), Metric::info(sent as f64 / duration.as_secs_f64(), "1/s"));
    // The other wake-up of every round: the waker's own, by its timer,
    // on a machine just as asleep. Shown and not scored: how much of it
    // is a scheduler's doing is what the numbers are collected to find
    // out.
    let mut timer_late = Recorder::default();
    for w in wakers {
        timer_late.merge(&w.timer_late);
    }
    for (q, suffix) in [(0.5, "p50"), (0.99, "p99")] {
        if timer_late.count() >= min_samples_for(q) {
            let us = timer_late.quantile(q).unwrap_or(0.0) / 1000.0;
            r.metrics.insert(format!("{POP_TIMER_LATE}_{suffix}"), Metric::info(us, "us"));
        }
    }
    if let Some(max) = timer_late.max() {
        r.metrics.insert(format!("{POP_TIMER_LATE}_max"), Metric::info(max as f64 / 1000.0, "us"));
    }
    if let Some((any, deepest)) = idle {
        r.metrics.insert("idle_pct".into(), Metric::info(any, "%"));
        r.metrics.insert("idle_deepest_pct".into(), Metric::info(deepest, "%"));
    }
    let c = &mut r.counters;
    c.insert("wakes_planned".into(), planned);
    c.insert("wakes_sent".into(), sent);
    c.insert("wakes_measured".into(), measured);
    c.insert("wakes_not_asleep".into(), not_asleep);
    c.insert("wakes_overrun".into(), overrun);
    c.insert("wakes_mistimed".into(), mistimed);
    c.insert("wakes_same_cpu".into(), same_cpu);
    c.insert("wakes_prev_cpu".into(), prev_cpu);
    c.insert("wake_calls_woke_nobody".into(), woke_nobody);
    r.latency_histograms.insert(POP_WAKE.into(), wake.to_histogram("ns"));
    if timer_late.count() > 0 {
        r.latency_histograms.insert(POP_TIMER_LATE.into(), timer_late.to_histogram("ns"));
    }
    if !curve.is_empty() {
        r.quantile_curves.insert(POP_WAKE.into(), curve);
    }
    r.quantile_unit = Some("us".into());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::Better;

    fn quick(waker_work: Duration) -> IdleWake {
        IdleWake {
            case: if waker_work.is_zero() { "handoff" } else { "busy" },
            pairs: 2,
            period: Duration::from_millis(5),
            waker_work,
            sched_diag: false,
        }
    }

    #[test]
    fn wake_ups_are_counted_timed_and_placed() {
        for work in [Duration::ZERO, Duration::from_micros(200)] {
            let w = quick(work);
            let r = w.run(Duration::from_millis(300));
            assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
            let c = &r.counters;
            // Two pairs, 5 ms, 300 ms: 120 wake-ups planned, whatever
            // the machine did, and sent less what a late waker skipped.
            assert_eq!(c["wakes_planned"], 120);
            assert!((100..=120).contains(&c["wakes_sent"]), "{c:?}");
            let rate = &r.metrics["wakes_per_s"];
            assert!((rate.value - c["wakes_sent"] as f64 / 0.3).abs() < 1e-6 && rate.better == Better::Neither);
            assert_eq!(c["wakes_mistimed"], 0);
            // One reading of the waker's lateness for every wake-up
            // sent, none for one that was not; shown and not scored.
            assert_eq!(r.latency_histograms[POP_TIMER_LATE].count, c["wakes_sent"]);
            for name in ["timer_late_p50", "timer_late_max"] {
                let m = &r.metrics[name];
                assert!(m.better == Better::Neither && m.unit == "us" && m.value >= 0.0, "{name} {m:?}");
            }
            assert!(!r.metrics.contains_key("timer_late_p99"));
            assert!(r.metrics["timer_late_max"].value >= r.metrics["timer_late_p50"].value);
            // Every one is measured or accounted for; a wake-up the
            // task had not acted on when the run ended is neither.
            let accounted = c["wakes_measured"] + c["wakes_not_asleep"] + c["wakes_overrun"];
            assert!(accounted <= c["wakes_sent"] && accounted + 2 >= c["wakes_sent"], "{c:?}");
            // 5 ms apart and nothing to do in between: asleep each time.
            assert!(c["wakes_measured"] >= c["wakes_sent"] * 9 / 10, "{c:?}");
            assert_eq!(r.latency_histograms[POP_WAKE].count, c["wakes_measured"]);
            assert!(c["wakes_same_cpu"] <= c["wakes_measured"] && c["wakes_prev_cpu"] <= c["wakes_measured"]);
            let wake = &r.metrics["wake_p50"];
            assert!(wake.value > 0.0 && wake.better == Better::Lower && wake.unit == "us");
            assert!(r.metrics["wake_max"].extreme);
            // 120 samples carry a p50 and not a p99.
            assert!(!r.metrics.contains_key("wake_p99"));
            for name in ["same_cpu_pct", "prev_cpu_pct"] {
                let m = &r.metrics[name];
                assert!((0.0..=100.0).contains(&m.value) && m.better == Better::Neither, "{name} {m:?}");
            }
            // Idle accounting where the kernel has it, and then both.
            assert_eq!(r.metrics.contains_key("idle_pct"), r.metrics.contains_key("idle_deepest_pct"));
            if let Some(idle) = r.metrics.get("idle_pct") {
                assert!(idle.better == Better::Neither && (0.0..=101.0).contains(&idle.value), "{idle:?}");
                assert!(r.metrics["idle_deepest_pct"].value <= idle.value + 1e-9);
            }
            assert_eq!(r.duration_s, 0.3);
            assert_eq!(r.quantile_unit.as_deref(), Some("us"));
        }
    }

    #[test]
    fn parameters_identify_the_workload() {
        let w = quick(Duration::ZERO);
        assert_eq!(w.name(), "idle-wake/handoff");
        assert_eq!(w.params(), json!({ "pairs": 2, "period_us": 5000, "waker_work_us": 0, "timer_slack_ns": 1 }));
        assert!(w.description().contains("goes back to sleep"));
        let busy = quick(Duration::from_micros(200));
        assert_eq!((busy.name().as_str(), busy.params()["waker_work_us"].as_u64()), ("idle-wake/busy", Some(200)));
        assert!(busy.description().contains("works for 200 µs"));
        assert_eq!(w.headline_metrics(), ["wake_p50", "wake_p99", "same_cpu_pct"]);
        // A waker that never sleeps leaves no idle machine to wake on.
        let bad = IdleWake { waker_work: Duration::from_millis(5), ..quick(Duration::ZERO) };
        assert!(bad.run(Duration::from_millis(10)).invalid_reason.unwrap().contains("less than the period"));
        let none = IdleWake { pairs: 0, ..quick(Duration::ZERO) };
        assert!(none.run(Duration::from_millis(10)).invalid_reason.is_some());
    }

    /// Which wake-ups get a time, decided on what the task found:
    /// no threads, no timing.
    #[test]
    fn only_a_wake_up_from_sleep_alone_is_timed() {
        let at = |t: u64, cpu: u32| move |_: u32| Some((t, cpu));
        // Asleep, one new wake-up, its time before the task ran.
        assert_eq!(found(true, 4, 5, 1_000, at(400, 3)), Found::Timed { delay: 600, waker_cpu: 3 });
        assert_eq!(found(true, 0, 1, 1_000, at(1_000, 0)), Found::Timed { delay: 0, waker_cpu: 0 });
        // Not asleep: however many there were, none is from sleep.
        assert_eq!(found(false, 4, 5, 1_000, at(400, 3)), Found::NotAsleep(1));
        assert_eq!(found(false, 4, 7, 1_000, at(400, 3)), Found::NotAsleep(3));
        // Asleep through more than one: none has a time of its own, and
        // the time kept is not looked at.
        assert_eq!(found(true, 4, 6, 1_000, |_| unreachable!()), Found::Overrun(2));
        // The next was sent while this one's time was read.
        assert_eq!(found(true, 4, 5, 1_000, |_| None), Found::Overrun(1));
        // A time after the task's own is no time of this wake-up, and is
        // not turned into a delay of zero.
        assert_eq!(found(true, 4, 5, 1_000, at(1_001, 3)), Found::Mistimed);
        // The count wraps with the word.
        assert_eq!(
            found(true, u32::MAX >> 1, (u32::MAX >> 1).wrapping_add(1), 9, at(4, 1)),
            Found::Timed { delay: 5, waker_cpu: 1 }
        );
    }

    /// The time of a wake-up is read by its number: a waker that has
    /// written the time of the next one, and not yet its number, has not
    /// touched this one's.
    #[test]
    fn a_wake_up_keeps_its_time_while_the_next_is_announced() {
        let pair = Pair::default();
        assert_eq!(pair.release(100, 7), 1);
        assert_eq!(pair.released(1), Some((100, 7)));
        // The waker is half way into wake-up 2: time and CPU written,
        // the number not yet. Wake-up 1 still reads as it was.
        pair.waker_cpu[0].store(9, Ordering::Relaxed);
        pair.released_at[0].store(200, Ordering::Release);
        assert_eq!(pair.released(1), Some((100, 7)));
        // The number follows: wake-up 1 is no longer the latest, and a
        // reader that had its time takes none, since from wake-up 3 on
        // its place holds another.
        pair.word.fetch_add(1, Ordering::Release);
        assert_eq!(pair.released(1), None);
        assert_eq!(pair.released(2), Some((200, 9)));
        assert_eq!(pair.release(300, 4), 3);
        assert_eq!((pair.released(3), pair.released(2), pair.released(1)), (Some((300, 4)), None, None));
        // A stopped pair has no latest wake-up to read.
        pair.word.fetch_or(STOP, Ordering::Release);
        assert_eq!(pair.released(3), None);
    }

    #[test]
    fn wake_ups_past_the_end_of_the_window_are_not_sent() {
        // Planned: those due before the end.
        assert_eq!(planned(1_000, 1_000, 100), 0);
        assert_eq!(planned(1_000, 1_001, 100), 1);
        assert_eq!(planned(1_000, 1_100, 100), 1);
        assert_eq!(planned(1_000, 1_101, 100), 2);
        assert_eq!(planned(2_000, 1_000, 100), 0);
        // A window that is over when the waker gets to its first
        // wake-up: due inside it, and not sent after it.
        let pair = Pair::default();
        let now = now_ns();
        let out = waker(&pair, now - 3_000_000, now - 1_000_000, 1_000_000, 0).unwrap();
        assert_eq!((out.planned, out.sent, out.timer_late.count()), (2, 0, 0));
        assert_eq!(pair.word.load(Ordering::Acquire), 0);
        // One that ends in a while gets what is due before then: three
        // wake-ups 100 ms apart, of which a waker kept from its CPU for
        // some of that time, on a busy test machine, sends fewer.
        let now = now_ns();
        let out = waker(&pair, now + 5_000_000, now + 250_000_000, 100_000_000, 0).unwrap();
        assert_eq!(out.planned, 3);
        assert!((1..=3).contains(&out.sent) && out.woke_nobody == out.sent, "{} sent", out.sent);
        assert_eq!(out.timer_late.count(), out.sent);
        // Lateness is against the instant the wake-up was due. One due
        // 2 ms ago, the only one of a long window: sent at once, and
        // late by those 2 ms and whatever getting to the clock took.
        // That is bounded by the clock itself, read before and after,
        // and not by a guess at how busy the machine is: the waker read
        // its clock between the two readings here.
        const DUE_AGO: u64 = 2_000_000;
        let (window, period) = (10_000_000_000, 100_000_000_000);
        let before = now_ns();
        let out = waker(&pair, before - DUE_AGO, before + window, period, 0).unwrap();
        let after = now_ns();
        assert_eq!((out.planned, out.sent, out.timer_late.count()), (1, 1, 1));
        let late = out.timer_late.max().unwrap();
        assert!((DUE_AGO..=after - before + DUE_AGO).contains(&late), "{late} ns");
        // With work after the call, the lateness is still the one read
        // before it. The work takes its 20 ms of CPU time at least, so
        // the waker's reading lay that much before the second one here:
        // lateness that held the work would be over this bound.
        const WORK: u64 = 20_000_000;
        let before = now_ns();
        let out = waker(&pair, before - DUE_AGO, before + window, period, WORK).unwrap();
        let after = now_ns();
        assert!(after - before >= WORK, "worked for {} ns", after - before);
        let late = out.timer_late.max().unwrap();
        assert!((DUE_AGO..=after - before - WORK + DUE_AGO).contains(&late), "{late} ns of {}", after - before);
    }

    /// The same with a task that really sleeps. The test waits for what
    /// it needs to have happened, and does not sleep and hope: the task
    /// is asleep when the kernel says its thread is.
    #[test]
    fn a_sleeping_task_is_timed_and_one_that_is_not_is_counted() {
        let pair = Pair::default();
        let tid = AtomicU32::new(0);
        // Asleep in its wait: the thread sleeps nowhere else.
        let asleep = || {
            let give_up = Instant::now() + Duration::from_secs(30);
            loop {
                let id = tid.load(Ordering::Acquire);
                let stat = std::fs::read_to_string(format!("/proc/self/task/{id}/stat")).unwrap_or_default();
                // "<tid> (<name>) <state> ...": the state after the name.
                if id != 0 && stat.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('S')) {
                    return;
                }
                assert!(Instant::now() < give_up, "the task never went to sleep: {stat}");
                std::thread::yield_now();
            }
        };
        // Not asleep: the word moved before the task waited on it.
        pair.release(now_ns(), u32::MAX);
        std::thread::scope(|s| {
            let t = s.spawn(|| {
                // SAFETY: gettid takes no arguments and cannot fail.
                tid.store(unsafe { libc::syscall(libc::SYS_gettid) } as u32, Ordering::Release);
                task(&pair)
            });
            // It has taken that one without sleeping and sleeps now.
            // Two before it can run: an overrun of two.
            asleep();
            pair.release(now_ns(), u32::MAX);
            pair.release(now_ns(), u32::MAX);
            assert_eq!(futex::wake(&pair.word, 1).unwrap(), 1);
            // Asleep again, and one alone: timed.
            asleep();
            pair.release(now_ns(), u32::MAX);
            assert_eq!(futex::wake(&pair.word, 1).unwrap(), 1);
            asleep();
            pair.word.fetch_or(STOP, Ordering::Release);
            futex::wake(&pair.word, 1).unwrap();
            let out = t.join().unwrap().unwrap();
            assert_eq!((out.not_asleep, out.overrun, out.mistimed, out.wake.count()), (1, 2, 0, 1));
            // No waker was on a CPU with that number.
            assert_eq!(out.same_cpu, 0);
        });
    }

    #[test]
    fn scheduler_statistics_cover_every_thread() {
        let w = IdleWake { sched_diag: true, ..quick(Duration::ZERO) };
        assert_eq!(w.params()["sched_diag"], true);
        let r = w.run(Duration::from_millis(200));
        if !schedstat::kept(&r) {
            return;
        }
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        // Two wakers and two tasks.
        assert_eq!(r.counters["sched_threads"], 4);
        let r = quick(Duration::ZERO).run(Duration::from_millis(50));
        assert!(!r.counters.keys().chain(r.metrics.keys()).any(|k| k.starts_with("sched_")));
    }

    #[test]
    fn failures_end_the_run_and_leave_no_thread_behind() {
        let w = quick(Duration::ZERO);
        for index in 0..4 {
            let hooks = Hooks { fail_spawn_at: Some(index), ..Default::default() };
            let began = Instant::now();
            let why = w.run_with(Duration::from_millis(100), &hooks).invalid_reason.expect("invalid");
            assert!(why.contains("spawning idle-wake threads"), "{why}");
            assert!(began.elapsed() < Duration::from_secs(5));
            assert_eq!(hooks.alive.load(Ordering::SeqCst), 0, "thread {index}");
        }
        let hooks = Hooks::default();
        let r = w.run_with(Duration::from_millis(50), &hooks);
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        assert_eq!(hooks.alive.load(Ordering::SeqCst), 0);
    }
}
