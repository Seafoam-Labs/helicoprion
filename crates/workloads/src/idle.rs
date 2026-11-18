//! Keeping the machine out of deep idle states for the length of a run.
//!
//! A CPU that has nothing to do goes to sleep, and the longer it expects
//! to sleep the deeper: clocks stop, voltages drop, and coming back
//! takes time that has nothing to do with the scheduler. Where a
//! workload lets the whole machine fall asleep between two events, that
//! time is in every measurement and can be most of it, and how deep the
//! machine sleeps depends on what the scheduler under test did a moment
//! before, so it is not even the same for both sides of a comparison.
//!
//! The kernel takes requests for a bound on that time (PM QoS,
//! `/dev/cpu_dma_latency`): while a process holds the file open with a
//! number of microseconds written to it, no CPU enters an idle state
//! that takes longer to leave. The request ends when the file is
//! closed, also when the process dies.
//!
//! The bound asked for is the exit latency of the shallowest idle state
//! the machine has in use, which leaves that state (a halted CPU, `C1`)
//! and rules out the deeper ones. Not zero: that would keep every CPU
//! spinning in the idle loop, a machine that is not idle at all. And not
//! a guess: where the states cannot be read there is no number that is
//! right for an unknown machine, one microsecond would rule out a first
//! state that takes two, and the request is not made.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::OnceLock;

const DEVICE: &str = "/dev/cpu_dma_latency";
const CPUIDLE: &str = "/sys/devices/system/cpu/cpu0/cpuidle";

/// The exit latency of the shallowest idle state that is not the idle
/// loop itself, in microseconds: the smallest one above zero among the
/// states listed in `dir` that are not switched off. An error where
/// there is no such state to read: the bound is not guessed.
fn shallowest(dir: &Path) -> Result<i32, String> {
    let states = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let latency = |state: &Path| -> Option<i32> {
        // A state that is switched off is not one the machine will use;
        // its latency must not set the bound.
        let off = fs::read_to_string(state.join("disable")).is_ok_and(|v| v.trim() != "0");
        let us = fs::read_to_string(state.join("latency")).ok()?.trim().parse::<i32>().ok()?;
        (!off && us > 0).then_some(us)
    };
    states
        .flatten()
        .filter_map(|state| latency(&state.path()))
        .min()
        .ok_or_else(|| format!("{} lists no idle state in use with an exit latency above zero", dir.display()))
}

/// The bound a run would ask for, if this process may ask and the
/// machine says what to ask for. Decided once, so that a workload's
/// parameters say the same thing before and after its run.
pub(crate) fn bound() -> Option<i32> {
    static BOUND: OnceLock<Option<i32>> = OnceLock::new();
    *BOUND.get_or_init(|| {
        // Opening the file alone asks for nothing.
        OpenOptions::new().read(true).write(true).open(DEVICE).ok()?;
        shallowest(Path::new(CPUIDLE)).ok()
    })
}

/// Tests that hold a request, one at a time: each looks at the bound in
/// force for the whole machine, which another one's request changes.
#[cfg(test)]
pub(crate) static REQUESTS: Mutex<()> = Mutex::new(());

/// A request for shallow idle states only, held until dropped.
#[derive(Debug)]
pub(crate) struct Shallow(#[allow(dead_code)] File);

impl Shallow {
    pub(crate) fn hold(limit_us: i32) -> Result<Self, String> {
        Self::hold_at(Path::new(DEVICE), limit_us)
    }

    fn hold_at(device: &Path, limit_us: i32) -> Result<Self, String> {
        let err = |what: &str, e: std::io::Error| format!("{what} {}: {e}", device.display());
        let file = OpenOptions::new().read(true).write(true).open(device).map_err(|e| err("opening", e))?;
        file.write_all_at(&limit_us.to_ne_bytes(), 0).map_err(|e| err("writing the idle latency bound to", e))?;
        // What is in force, not what was written: the bound of all
        // requests together, which may be tighter and must not be looser.
        let mut now = [0u8; 4];
        file.read_exact_at(&mut now, 0).map_err(|e| err("reading the idle latency bound from", e))?;
        let in_force = i32::from_ne_bytes(now);
        if in_force > limit_us {
            return Err(format!(
                "the idle latency bound is {in_force} µs after asking for {limit_us} µs through {}",
                device.display()
            ));
        }
        Ok(Shallow(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shallowest_state_in_use_sets_the_bound() {
        let dir = std::env::temp_dir().join(format!("helicoprion-cpuidle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let state = |name: &str, latency: &str, disable: &str| {
            fs::create_dir_all(dir.join(name)).unwrap();
            fs::write(dir.join(name).join("latency"), format!("{latency}\n")).unwrap();
            fs::write(dir.join(name).join("disable"), format!("{disable}\n")).unwrap();
        };
        // POLL, C1, C2, C3 as a Ryzen lists them.
        for (name, latency) in [("state0", "0"), ("state1", "1"), ("state2", "18"), ("state3", "350")] {
            state(name, latency, "0");
        }
        assert_eq!(shallowest(&dir), Ok(1));
        // Another machine: the first real state takes two microseconds,
        // and a bound of one would leave only the idle loop.
        state("state1", "2", "0");
        assert_eq!(shallowest(&dir), Ok(2));
        // That state switched off: the next one in use.
        state("state1", "2", "1");
        assert_eq!(shallowest(&dir), Ok(18));
        // A state without the switch counts as in use.
        fs::remove_file(dir.join("state1").join("disable")).unwrap();
        assert_eq!(shallowest(&dir), Ok(2));
        // Only the idle loop: nothing to keep, and no number made up.
        for name in ["state1", "state2", "state3"] {
            fs::remove_dir_all(dir.join(name)).unwrap();
        }
        assert!(shallowest(&dir).unwrap_err().contains("lists no idle state in use"));
        // No list at all.
        fs::remove_dir_all(&dir).unwrap();
        assert!(shallowest(&dir).unwrap_err().starts_with(&dir.display().to_string()));
    }

    /// Against the kernel, where the test may: the bound is in force
    /// while the request is held and gone when it is dropped.
    #[test]
    fn the_request_holds_while_it_is_held() {
        let _one = REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
        if OpenOptions::new().read(true).write(true).open(DEVICE).is_err() {
            // Not root: nothing to hold, and no bound to offer.
            assert_eq!(bound(), None);
            assert!(Shallow::hold(1).unwrap_err().contains("opening /dev/cpu_dma_latency"));
            return;
        }
        let in_force = || {
            let mut now = [0u8; 4];
            File::open(DEVICE).unwrap().read_exact_at(&mut now, 0).unwrap();
            i32::from_ne_bytes(now)
        };
        let before = in_force();
        {
            let _held = Shallow::hold(1).unwrap();
            assert!(in_force() <= 1, "{}", in_force());
        }
        // Other processes may hold their own bound; ours is gone. One of
        // them changing its request in these microseconds would fail
        // this, and has not been seen to.
        assert_eq!(in_force(), before);
        // What a run would ask for is one answer, whatever it is.
        assert_eq!(bound(), bound());
    }

    #[test]
    fn a_device_that_is_not_there_is_an_error_with_its_name() {
        let why = Shallow::hold_at(Path::new("/nonexistent/cpu_dma_latency"), 1).unwrap_err();
        assert!(why.starts_with("opening /nonexistent/cpu_dma_latency: "), "{why}");
    }
}
