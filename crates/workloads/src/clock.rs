//! What the timer-driven workloads share: the monotonic clock in
//! nanoseconds, sleeping until an absolute instant, timers that fire on
//! time, a fixed amount of CPU time, and a count of the threads alive.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Counts a task as alive until dropped, also when it panics.
pub(crate) struct Alive(Arc<AtomicUsize>);

impl Alive {
    pub(crate) fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Alive(Arc::clone(count))
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) fn clock_ns(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write to; both clocks used here
    // exist on every Linux this builds for.
    unsafe {
        libc::clock_gettime(clock, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub(crate) fn now_ns() -> u64 {
    clock_ns(libc::CLOCK_MONOTONIC)
}

/// Sleep until the monotonic clock reads `at`; returns at once if it
/// already does.
pub(crate) fn sleep_until(at: u64) {
    let ts =
        libc::timespec { tv_sec: (at / 1_000_000_000) as libc::time_t, tv_nsec: (at % 1_000_000_000) as libc::c_long };
    loop {
        // SAFETY: `ts` is a valid timespec; with TIMER_ABSTIME the
        // remaining-time pointer is unused and may be null.
        let r = unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
        // Interrupted by a signal: the deadline is absolute, so go again.
        if r != libc::EINTR {
            return;
        }
    }
}

/// Use `work` nanoseconds of this thread's CPU time.
pub(crate) fn burn(work: u64) {
    let from = clock_ns(libc::CLOCK_THREAD_CPUTIME_ID);
    let mut x = from | 1;
    while clock_ns(libc::CLOCK_THREAD_CPUTIME_ID) - from < work {
        // Well under a microsecond of integer mixing between two readings.
        for _ in 0..256 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        black_box(x);
    }
}

/// The timer slack every thread that sleeps until a release runs with, in
/// nanoseconds, as recorded in the workloads' parameters.
pub(crate) const TIMER_SLACK_NS: u64 = 1;

fn timer_slack() -> libc::c_int {
    // SAFETY: PR_GET_TIMERSLACK takes no further argument and returns
    // the calling thread's slack.
    unsafe { libc::prctl(libc::PR_GET_TIMERSLACK) }
}

/// Ask for timers that fire on time, and check that this thread got
/// them: the default slack of 50 µs lets the kernel fire a timer that
/// much late on purpose, and the parameters of the run promise
/// `TIMER_SLACK_NS`.
pub(crate) fn set_timer_slack() -> Result<(), String> {
    // SAFETY: PR_SET_TIMERSLACK takes one integer argument and affects
    // only the calling thread.
    let set = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, TIMER_SLACK_NS as libc::c_ulong) };
    if set != 0 {
        return Err(format!("setting the timer slack failed: {}", std::io::Error::last_os_error()));
    }
    let got = timer_slack();
    if i64::from(got) != TIMER_SLACK_NS as i64 {
        return Err(format!("the timer slack is {got} ns after asking for {TIMER_SLACK_NS} ns"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_slack_is_set_and_checked() {
        std::thread::spawn(|| {
            assert_eq!(set_timer_slack(), Ok(()));
            assert_eq!(timer_slack(), 1);
        })
        .join()
        .unwrap();
    }
}
