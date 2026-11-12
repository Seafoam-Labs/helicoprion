//! wake-pingpong: pairs of threads hand a token back and forth through a
//! futex. With `hogs > 0` the same measurement runs while every CPU is busy,
//! which is where schedulers differ the most.
//!
//! # Two populations
//!
//! A hand-off is only a real sleep + wakeup if the receiver was already in
//! FUTEX_WAIT. Under load many are not: the pinger finds the token already
//! back, or FUTEX_WAIT returns EAGAIN. Mixing both into one distribution
//! makes fast hand-offs look like excellent wakeup latency, so every round
//! trip is classified:
//!
//! * `rtt_blocked` — the pinger slept in FUTEX_WAIT at least once;
//! * `rtt_fast`    — it never slept (the reply arrived before it blocked).
//!
//! A round trip where a wait was interrupted (EINTR) without any completed
//! sleep cannot be classified either way; it is counted as
//! `roundtrips_ambiguous` and kept out of both histograms.
//!
//! Each population gets its own histogram and metrics. Counters record the
//! share of each, and `roundtrips_two_sided` counts round trips where the
//! pinger slept *and* its FUTEX_WAKE really woke the ponger.
//!
//! # Timing
//!
//! Every worker acknowledges readiness; only then is a single start instant
//! fixed and the gate opened, so all pingers share one start and one
//! deadline. Hog work is snapshotted immediately before the start instant
//! is fixed and again as soon as the main thread wakes at the deadline; the
//! actual boundaries of both windows are stored in `RunResult::windows`.
//!
//! # Failure handling
//!
//! Any worker failure (unexpected futex error or panic, caught at the
//! worker boundary) triggers a global abort: every token is set to ABORT
//! and every waiter on the gate, the ready counter and the tokens is woken,
//! so the run ends promptly and is reported invalid.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use helicoprion_core::hist::Recorder;
use helicoprion_core::{Metric, RunResult, Window};
use serde_json::json;

use crate::Workload;
use crate::futex::{self, Wait};
use crate::hog::Hogs;
use crate::samples::{min_samples_for, quantile_grid};

/// Token is held by the pinger.
const PING: u32 = 0;
/// Token is held by the ponger.
const PONG: u32 = 1;
/// Shutting down; both sides exit.
const ABORT: u32 = 2;

/// How long workers may take to report ready before the run is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

pub const POP_BLOCKED: &str = "rtt_blocked";
pub const POP_FAST: &str = "rtt_fast";

pub struct WakePingPong {
    pub pairs: usize,
    pub hogs: usize,
}

impl Workload for WakePingPong {
    fn name(&self) -> String {
        if self.hogs == 0 { "wake-pingpong".into() } else { "wake-pingpong/loaded".into() }
    }

    fn description(&self) -> String {
        let base = format!(
            "{} thread pairs passing a token via futex; round trips split into blocked (pinger slept) and fast",
            self.pairs
        );
        if self.hogs == 0 { base } else { format!("{base}, with {} CPU hog threads in the background", self.hogs) }
    }

    fn params(&self) -> serde_json::Value {
        json!({ "pairs": self.pairs, "hogs": self.hogs })
    }

    fn headline_metrics(&self) -> &'static [&'static str] {
        &["rtt_blocked_p50", "rtt_blocked_p99", "rtt_fast_p50"]
    }

    fn progress_note(&self, r: &RunResult) -> Option<String> {
        let (b, t) = (r.counters.get("roundtrips_blocked")?, r.counters.get("roundtrips")?);
        (*t > 0).then(|| format!("blocked={:.0}%", *b as f64 / *t as f64 * 100.0))
    }

    fn run(&self, duration: Duration) -> RunResult {
        self.run_with(duration, Fault::None)
    }
}

impl WakePingPong {
    fn run_with(&self, duration: Duration, fault: Fault) -> RunResult {
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

        let mut crew = Crew::new(fault);
        for i in 0..self.pairs {
            if let Err(e) = crew.spawn_pair(i, duration) {
                // `crew` and `hogs` are dropped on return: everything stops.
                return invalid(format!("spawning ping/pong threads: {e}"));
            }
        }

        // First hog snapshot: immediately before the start instant is fixed
        // and the gate opens, so it cannot miss the beginning of the window.
        let mut hog_t0: Option<(u64, Instant)> = None;
        let Some(start) = crew.start(|| hog_t0 = hogs.as_ref().map(|h| (h.work(), Instant::now()))) else {
            return invalid(crew.finish().err().unwrap_or_else(|| "workers never became ready".into()));
        };
        crew.sleep_until(start + duration);
        // Second snapshot: as soon as the main thread wakes at the deadline.
        let hog = match (hogs.as_ref(), hog_t0) {
            (Some(h), Some((w0, t0))) => {
                let (w1, t1) = (h.work(), Instant::now());
                Some(HogWindow { units: w1 - w0, t0, t1 })
            }
            _ => None,
        };

        let outcome = crew.finish();
        drop(hogs);
        match outcome {
            Ok((pings, pongs)) => summarize(&pings, &pongs, start, hog),
            Err(why) => invalid(why),
        }
    }
}

/// Hog work between two snapshots and when they were taken.
struct HogWindow {
    units: u64,
    t0: Instant,
    t1: Instant,
}

/// Seconds from `start` to `t` (negative if `t` is earlier).
fn offset(start: Instant, t: Instant) -> f64 {
    if t >= start { (t - start).as_secs_f64() } else { -(start - t).as_secs_f64() }
}

/// How a round trip is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// The pinger's FUTEX_WAIT returned 0: it slept and was woken.
    Blocked,
    /// The pinger never slept: the reply arrived before it blocked.
    Fast,
    /// A wait was interrupted (EINTR) and no completed sleep was seen: the
    /// pinger may or may not have slept. Counted, but in neither histogram.
    Ambiguous,
}

fn classify(slept: bool, interrupted: bool) -> Class {
    match (slept, interrupted) {
        (true, _) => Class::Blocked,
        (false, true) => Class::Ambiguous,
        (false, false) => Class::Fast,
    }
}

fn invalid(why: String) -> RunResult {
    RunResult { invalid_reason: Some(why), ..Default::default() }
}

/// Test hook: make a worker fail on purpose after some hand-offs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    #[cfg_attr(not(test), allow(dead_code))]
    PongError,
    #[cfg_attr(not(test), allow(dead_code))]
    PongPanic,
    #[cfg_attr(not(test), allow(dead_code))]
    PingPanic,
}
const FAULT_AFTER: u64 = 1000;

#[derive(Default, Clone, Copy)]
struct Counts {
    waits_slept: u64,
    waits_eagain: u64,
    waits_eintr: u64,
    wake_calls: u64,
    woken: u64,
}

impl Counts {
    fn on_wait(&mut self, w: Wait) {
        match w {
            Wait::Slept | Wait::TimedOut => self.waits_slept += 1,
            Wait::ValueChanged => self.waits_eagain += 1,
            Wait::Interrupted => self.waits_eintr += 1,
        }
    }
    fn add(&mut self, b: &Counts) {
        self.waits_slept += b.waits_slept;
        self.waits_eagain += b.waits_eagain;
        self.waits_eintr += b.waits_eintr;
        self.wake_calls += b.wake_calls;
        self.woken += b.woken;
    }
}

#[derive(Default)]
struct PingOut {
    blocked: Recorder,
    fast: Recorder,
    counts: Counts,
    /// Pinger slept and its FUTEX_WAKE woke the ponger.
    two_sided: u64,
    /// Round trips that could not be classified (EINTR without a completed sleep).
    ambiguous: u64,
    end: Option<Instant>,
}

struct Shared {
    gate: AtomicU32,
    ready: AtomicU32,
    abort: AtomicBool,
    start: OnceLock<Instant>,
    tokens: Mutex<Vec<Arc<AtomicU32>>>,
    error: Mutex<Option<String>>,
    fault: Fault,
}

impl Shared {
    /// Record the first failure and abort everything.
    fn fail(&self, what: String) {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(what);
        self.abort_all();
    }

    /// Release every waiter: gate, ready counter and all tokens.
    fn abort_all(&self) {
        self.abort.store(true, Ordering::Release);
        self.gate.store(1, Ordering::Release);
        let _ = futex::wake(&self.gate, i32::MAX);
        let _ = futex::wake(&self.ready, i32::MAX);
        for t in self.tokens.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            t.store(ABORT, Ordering::Release);
            let _ = futex::wake(t, i32::MAX);
        }
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

/// Run a worker body; a panic becomes a recorded failure plus global abort.
fn guarded<T: Default>(sh: &Shared, who: &str, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            sh.fail(format!("{who} panicked: {msg}"));
            T::default()
        }
    }
}

/// Owns every ping/pong thread. Dropping it aborts and joins whatever is left.
struct Crew {
    shared: Arc<Shared>,
    pingers: Vec<JoinHandle<PingOut>>,
    pongers: Vec<JoinHandle<Counts>>,
}

impl Crew {
    fn new(fault: Fault) -> Self {
        Crew {
            shared: Arc::new(Shared {
                gate: AtomicU32::new(0),
                ready: AtomicU32::new(0),
                abort: AtomicBool::new(false),
                start: OnceLock::new(),
                tokens: Mutex::new(Vec::new()),
                error: Mutex::new(None),
                fault,
            }),
            pingers: Vec::new(),
            pongers: Vec::new(),
        }
    }

    fn spawn_pair(&mut self, i: usize, duration: Duration) -> std::io::Result<()> {
        let token = Arc::new(AtomicU32::new(if self.shared.aborted() { ABORT } else { PING }));
        self.shared.tokens.lock().unwrap_or_else(|p| p.into_inner()).push(Arc::clone(&token));
        let (t, sh) = (Arc::clone(&token), Arc::clone(&self.shared));
        self.pongers.push(
            std::thread::Builder::new()
                .name(format!("pong-{i}"))
                .spawn(move || guarded(&sh, "ponger", || pong(&t, &sh)))?,
        );
        let sh = Arc::clone(&self.shared);
        self.pingers.push(
            std::thread::Builder::new()
                .name(format!("ping-{i}"))
                .spawn(move || guarded(&sh, "pinger", || ping(&token, &sh, duration)))?,
        );
        Ok(())
    }

    /// Wait until every worker is ready, fix the common start, open the gate.
    /// `None` if the run was aborted or workers never became ready.
    /// `before_start` runs right before the start instant is fixed (used for
    /// snapshots that must not miss the beginning of the window).
    fn start(&self, before_start: impl FnOnce()) -> Option<Instant> {
        let sh = &self.shared;
        let need = (self.pingers.len() + self.pongers.len()) as u32;
        let give_up = Instant::now() + READY_TIMEOUT;
        loop {
            if sh.aborted() {
                return None;
            }
            let ready = sh.ready.load(Ordering::Acquire);
            if ready >= need {
                break;
            }
            if Instant::now() >= give_up {
                sh.fail(format!("only {ready}/{need} workers ready after {READY_TIMEOUT:?}"));
                return None;
            }
            if let Err(e) = futex::wait_timeout(&sh.ready, ready, Duration::from_millis(100)) {
                sh.fail(format!("FUTEX_WAIT on ready counter: {e}"));
                return None;
            }
        }
        before_start();
        let start = *sh.start.get_or_init(Instant::now);
        sh.gate.store(1, Ordering::Release);
        if let Err(e) = futex::wake(&sh.gate, i32::MAX) {
            sh.fail(format!("FUTEX_WAKE on start gate: {e}"));
            return None;
        }
        Some(start)
    }

    /// Sleep until `deadline`, returning early on abort.
    fn sleep_until(&self, deadline: Instant) {
        while !self.shared.aborted() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }

    /// Join everything; `Err` if any worker failed.
    fn finish(mut self) -> Result<(Vec<PingOut>, Vec<Counts>), String> {
        let mut pings = Vec::new();
        for h in std::mem::take(&mut self.pingers) {
            match h.join() {
                Ok(p) => pings.push(p),
                // Unreachable with `guarded`, but never hang or lose it.
                Err(_) => self.shared.fail("pinger thread panicked outside guard".into()),
            }
        }
        let mut pongs = Vec::new();
        for h in std::mem::take(&mut self.pongers) {
            match h.join() {
                Ok(c) => pongs.push(c),
                Err(_) => self.shared.fail("ponger thread panicked outside guard".into()),
            }
        }
        match self.shared.error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(e) => Err(e),
            None => Ok((pings, pongs)),
        }
    }
}

impl Drop for Crew {
    fn drop(&mut self) {
        if self.pingers.is_empty() && self.pongers.is_empty() {
            return;
        }
        self.shared.abort_all();
        for h in self.pingers.drain(..) {
            let _ = h.join();
        }
        for h in self.pongers.drain(..) {
            let _ = h.join();
        }
    }
}

/// Announce readiness, then wait for the gate. False if aborted.
fn ready_then_wait_gate(sh: &Shared) -> bool {
    sh.ready.fetch_add(1, Ordering::AcqRel);
    let _ = futex::wake(&sh.ready, 1);
    while sh.gate.load(Ordering::Acquire) == 0 {
        if let Err(e) = futex::wait(&sh.gate, 0) {
            sh.fail(format!("FUTEX_WAIT on start gate: {e}"));
            return false;
        }
    }
    !sh.aborted()
}

fn ping(token: &AtomicU32, sh: &Shared, duration: Duration) -> PingOut {
    let mut out = PingOut::default();
    if ready_then_wait_gate(sh) {
        let start = *sh.start.get().expect("start is set before the gate opens");
        let deadline = start + duration;
        let mut n = 0u64;
        'run: while !sh.aborted() && Instant::now() < deadline {
            if sh.fault == Fault::PingPanic && n == FAULT_AFTER {
                panic!("injected ping panic");
            }
            n += 1;
            let t0 = Instant::now();
            if token.compare_exchange(PING, PONG, Ordering::AcqRel, Ordering::Acquire).is_err() {
                break; // aborted
            }
            let woke_peer = match futex::wake(token, 1) {
                Ok(n) => {
                    out.counts.wake_calls += 1;
                    out.counts.woken += u64::from(n);
                    n > 0
                }
                Err(e) => {
                    sh.fail(format!("FUTEX_WAKE (ping): {e}"));
                    break;
                }
            };
            let mut slept = false;
            let mut interrupted = false;
            loop {
                match token.load(Ordering::Acquire) {
                    PONG => match futex::wait(token, PONG) {
                        Ok(w) => {
                            slept |= w == Wait::Slept;
                            interrupted |= w == Wait::Interrupted;
                            out.counts.on_wait(w);
                        }
                        Err(e) => {
                            sh.fail(format!("FUTEX_WAIT (ping): {e}"));
                            break 'run;
                        }
                    },
                    PING => break,
                    _ => break 'run, // ABORT
                }
            }
            let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            match classify(slept, interrupted) {
                Class::Blocked => {
                    out.blocked.record(ns);
                    out.two_sided += u64::from(woke_peer);
                }
                Class::Fast => out.fast.record(ns),
                Class::Ambiguous => out.ambiguous += 1,
            }
        }
        out.end = Some(Instant::now());
    }
    // Release the ponger.
    token.store(ABORT, Ordering::Release);
    let _ = futex::wake(token, i32::MAX);
    out
}

fn pong(token: &AtomicU32, sh: &Shared) -> Counts {
    let mut c = Counts::default();
    if !ready_then_wait_gate(sh) {
        return c;
    }
    let mut handoffs = 0u64;
    loop {
        match token.load(Ordering::Acquire) {
            PING => match futex::wait(token, PING) {
                Ok(w) => c.on_wait(w),
                Err(e) => {
                    sh.fail(format!("FUTEX_WAIT (pong): {e}"));
                    return c;
                }
            },
            PONG => {
                if handoffs == FAULT_AFTER {
                    match sh.fault {
                        // Leave the pinger asleep on PONG, as a real failure would.
                        Fault::PongError => {
                            sh.fail("injected pong futex error".into());
                            return c;
                        }
                        Fault::PongPanic => panic!("injected pong panic"),
                        _ => {}
                    }
                }
                // CAS so an ABORT stored concurrently is never overwritten.
                if token.compare_exchange(PONG, PING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    handoffs += 1;
                    match futex::wake(token, 1) {
                        Ok(n) => {
                            c.wake_calls += 1;
                            c.woken += u64::from(n);
                        }
                        Err(e) => {
                            sh.fail(format!("FUTEX_WAKE (pong): {e}"));
                            return c;
                        }
                    }
                }
            }
            _ => return c, // ABORT
        }
    }
}

/// Insert p50/p99/p99.9/max for one population, each only if the sample
/// count supports it; returns the quantile curve likewise truncated.
fn population_metrics(m: &mut BTreeMap<String, Metric>, name: &str, rec: &Recorder) -> Vec<(f64, f64)> {
    let n = rec.count();
    let us = |q: f64| rec.quantile(q).unwrap_or(0.0) / 1000.0;
    for (q, suffix) in [(0.5, "p50"), (0.99, "p99"), (0.999, "p99.9")] {
        if n >= min_samples_for(q) {
            m.insert(format!("{name}_{suffix}"), Metric::lower(us(q), "us"));
        }
    }
    if let Some(max) = rec.max() {
        m.insert(format!("{name}_max"), Metric::lower(max as f64 / 1000.0, "us"));
    }
    quantile_grid().into_iter().filter(|q| n >= min_samples_for(*q)).map(|q| (q, us(q))).collect()
}

fn summarize(pings: &[PingOut], pongs: &[Counts], start: Instant, hog: Option<HogWindow>) -> RunResult {
    let mut blocked = Recorder::default();
    let mut fast = Recorder::default();
    let mut ping_c = Counts::default();
    let mut two_sided = 0;
    let mut ambiguous = 0;
    for p in pings {
        blocked.merge(&p.blocked);
        fast.merge(&p.fast);
        ping_c.add(&p.counts);
        two_sided += p.two_sided;
        ambiguous += p.ambiguous;
    }
    let mut pong_c = Counts::default();
    for c in pongs {
        pong_c.add(c);
    }
    // Common window: shared start to the last pinger's end.
    let end = pings.iter().filter_map(|p| p.end).max().unwrap_or(start);
    let window = (end - start).as_secs_f64();
    let roundtrips = blocked.count() + fast.count() + ambiguous;

    let mut r = RunResult { duration_s: window, ..Default::default() };
    if roundtrips == 0 || window <= 0.0 {
        r.invalid_reason = Some("no samples collected".into());
        return r;
    }

    let blocked_curve = population_metrics(&mut r.metrics, POP_BLOCKED, &blocked);
    let fast_curve = population_metrics(&mut r.metrics, POP_FAST, &fast);
    let m = &mut r.metrics;
    m.insert("roundtrips_per_s".into(), Metric::higher(roundtrips as f64 / window, "1/s"));
    // Threads FUTEX_WAKE actually woke, not an assumed two per round trip.
    let woken = ping_c.woken + pong_c.woken;
    m.insert("woken_per_s".into(), Metric::higher(woken as f64 / window, "1/s"));
    r.windows.insert("latency".into(), Window { start_s: 0.0, end_s: window });
    if let Some(h) = hog {
        let secs = (h.t1 - h.t0).as_secs_f64();
        if secs > 0.0 {
            m.insert("hog_work_per_s".into(), Metric::higher(h.units as f64 / secs, "units/s"));
        }
        r.windows.insert("hog".into(), Window { start_s: offset(start, h.t0), end_s: offset(start, h.t1) });
    }

    let c = &mut r.counters;
    c.insert("roundtrips".into(), roundtrips);
    c.insert("roundtrips_blocked".into(), blocked.count());
    c.insert("roundtrips_fast".into(), fast.count());
    c.insert("roundtrips_ambiguous".into(), ambiguous);
    c.insert("roundtrips_two_sided".into(), two_sided);
    c.insert("ping_waits_slept".into(), ping_c.waits_slept);
    c.insert("ping_waits_eagain".into(), ping_c.waits_eagain);
    c.insert("ping_waits_eintr".into(), ping_c.waits_eintr);
    c.insert("pong_waits_slept".into(), pong_c.waits_slept);
    c.insert("pong_waits_eagain".into(), pong_c.waits_eagain);
    c.insert("pong_waits_eintr".into(), pong_c.waits_eintr);
    c.insert("futex_wake_calls".into(), ping_c.wake_calls + pong_c.wake_calls);
    c.insert("futex_woken".into(), woken);

    for (name, rec, curve) in [(POP_BLOCKED, &blocked, blocked_curve), (POP_FAST, &fast, fast_curve)] {
        if rec.count() > 0 {
            r.latency_histograms.insert(name.into(), rec.to_histogram("ns"));
        }
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
    use std::sync::mpsc;

    /// Run on another thread; fail the test instead of hanging forever.
    fn run_bounded(w: WakePingPong, duration: Duration, fault: Fault, limit: Duration) -> RunResult {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(w.run_with(duration, fault));
        });
        rx.recv_timeout(limit).expect("workload did not finish in time (hang)")
    }

    #[test]
    fn populations_partition_round_trips() {
        let r = run_bounded(
            WakePingPong { pairs: 2, hogs: 0 },
            Duration::from_millis(150),
            Fault::None,
            Duration::from_secs(10),
        );
        assert!(r.invalid_reason.is_none(), "{:?}", r.invalid_reason);
        let c = &r.counters;
        assert_eq!(c["roundtrips"], c["roundtrips_blocked"] + c["roundtrips_fast"] + c["roundtrips_ambiguous"]);
        assert!(r.windows.contains_key("latency"));
        assert!(c["roundtrips_two_sided"] <= c["roundtrips_blocked"]);
        assert!(c["futex_woken"] <= c["futex_wake_calls"]);
        let total: u64 = r.latency_histograms.values().map(|h| h.count).sum();
        // Ambiguous (EINTR) round trips are counted but deliberately in neither histogram.
        assert_eq!(total + c["roundtrips_ambiguous"], c["roundtrips"]);
        // Pingers run until the common deadline, so the window cannot be shorter
        // than requested. How much longer is host-dependent and recorded in
        // `windows`, so it is not bounded here.
        assert!(r.duration_s >= 0.15, "{}", r.duration_s);
    }

    #[test]
    fn hog_rate_is_reported() {
        let r = run_bounded(
            WakePingPong { pairs: 1, hogs: 1 },
            Duration::from_millis(100),
            Fault::None,
            Duration::from_secs(10),
        );
        assert!(r.metrics["hog_work_per_s"].value > 0.0);
        let hog = r.windows["hog"];
        let lat = r.windows["latency"];
        // Ordering only; actual offsets are host-dependent and recorded in the
        // result, so no wall-clock tolerances here (a loaded CI host would
        // make them flaky). The first hog snapshot is taken before the common
        // start is fixed; both windows end no earlier than the nominal deadline.
        assert_eq!(lat.start_s, 0.0);
        assert!(hog.start_s <= lat.start_s, "{hog:?} vs {lat:?}");
        assert!(hog.end_s >= 0.1 && lat.end_s >= 0.1, "{hog:?} vs {lat:?}");
        assert!(hog.start_s < hog.end_s);
    }

    #[test]
    fn eintr_without_completed_sleep_is_ambiguous() {
        assert_eq!(classify(true, false), Class::Blocked);
        assert_eq!(classify(true, true), Class::Blocked);
        assert_eq!(classify(false, false), Class::Fast);
        assert_eq!(classify(false, true), Class::Ambiguous);
    }

    #[test]
    fn tail_quantiles_need_enough_samples() {
        let mut m = BTreeMap::new();
        let mut rec = Recorder::default();
        for v in 0..500 {
            rec.record(1000 + v);
        }
        let curve = population_metrics(&mut m, "x", &rec);
        assert!(m.contains_key("x_p50"));
        assert!(!m.contains_key("x_p99"), "500 samples cannot support p99");
        assert!(curve.iter().all(|(q, _)| *q <= 0.98));
    }

    fn assert_fault_aborts_promptly(fault: Fault, needle: &str) {
        // 60 s requested: only the abort path can finish within the limit.
        let r =
            run_bounded(WakePingPong { pairs: 2, hogs: 0 }, Duration::from_secs(60), fault, Duration::from_secs(10));
        let why = r.invalid_reason.expect("run must be invalid");
        assert!(why.contains(needle), "{why}");
    }

    #[test]
    fn pong_error_aborts_promptly() {
        assert_fault_aborts_promptly(Fault::PongError, "injected pong futex error");
    }

    #[test]
    fn pong_panic_aborts_promptly() {
        assert_fault_aborts_promptly(Fault::PongPanic, "ponger panicked: injected pong panic");
    }

    #[test]
    fn ping_panic_aborts_promptly() {
        assert_fault_aborts_promptly(Fault::PingPanic, "pinger panicked: injected ping panic");
    }

    #[test]
    fn crew_drop_without_start_does_not_hang() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut crew = Crew::new(Fault::None);
            crew.spawn_pair(0, Duration::from_secs(60)).unwrap();
            crew.spawn_pair(1, Duration::from_secs(60)).unwrap();
            drop(crew); // gate never opened: Drop must release and join.
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("Crew::drop hung");
    }
}
