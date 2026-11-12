//! Minimal private futex wrappers. Used instead of std channels so a wakeup
//! is exactly one FUTEX_WAKE -> scheduler -> FUTEX_WAIT return, nothing more.
//! Return values are surfaced so callers can tell a real sleep from a
//! value-mismatch return and can reject unexpected errors.

use std::io;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The thread was put to sleep and later returned (woken, or rarely spurious).
    Slept,
    /// `*word != expected` at call time (EAGAIN): no sleep happened.
    ValueChanged,
    /// Interrupted by a signal (EINTR).
    Interrupted,
    /// The relative timeout expired (ETIMEDOUT).
    TimedOut,
}

/// Sleep while `*word == expected`.
pub fn wait(word: &AtomicU32, expected: u32) -> io::Result<Wait> {
    wait_inner(word, expected, None)
}

/// Like [`wait`], but give up after `timeout`.
pub fn wait_timeout(word: &AtomicU32, expected: u32, timeout: Duration) -> io::Result<Wait> {
    wait_inner(word, expected, Some(timeout))
}

fn wait_inner(word: &AtomicU32, expected: u32, timeout: Option<Duration>) -> io::Result<Wait> {
    let ts = timeout.map(|t| libc::timespec {
        tv_sec: t.as_secs().min(i64::MAX as u64) as libc::time_t,
        tv_nsec: libc::c_long::from(t.subsec_nanos() as i32),
    });
    let ts_ptr = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
    // SAFETY: `word` is a valid, aligned u32 for the duration of the call;
    // `ts_ptr` is null ("wait indefinitely") or points to a live timespec.
    let rc = unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG, expected, ts_ptr)
    };
    if rc == 0 {
        return Ok(Wait::Slept);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EAGAIN) => Ok(Wait::ValueChanged),
        Some(libc::EINTR) => Ok(Wait::Interrupted),
        Some(libc::ETIMEDOUT) => Ok(Wait::TimedOut),
        _ => Err(err),
    }
}

/// Wake up to `n` waiters; returns how many were actually woken.
pub fn wake(word: &AtomicU32, n: i32) -> io::Result<u32> {
    // SAFETY: as above; FUTEX_WAKE only uses the address as a key.
    let rc = unsafe { libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, n) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc as u32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_is_reported_not_slept() {
        let w = AtomicU32::new(1);
        assert_eq!(wait(&w, 0).unwrap(), Wait::ValueChanged);
    }

    #[test]
    fn timeout_expires() {
        let w = AtomicU32::new(0);
        assert_eq!(wait_timeout(&w, 0, Duration::from_millis(5)).unwrap(), Wait::TimedOut);
    }

    #[test]
    fn wake_without_waiters_wakes_nobody() {
        let w = AtomicU32::new(0);
        assert_eq!(wake(&w, 1).unwrap(), 0);
    }
}
