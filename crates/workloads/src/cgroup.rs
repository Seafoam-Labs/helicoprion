//! cgroup-fair: two groups of busy threads in two control groups, and
//! whether each group gets the share of the CPUs it is entitled to.
//!
//! The other workloads ask how soon a task runs. This one asks who gets
//! the CPU when there is not enough of it, and by which rule: a desktop
//! session next to a build, or two services on one machine, are told
//! apart by the kernel as control groups, and `cpu.weight` says how the
//! CPUs are to be split between them. A scheduler can follow that, split
//! by group and ignore the weights, or know nothing of groups and treat
//! every thread alike. The suite runs two cases that tell these apart:
//!
//! * `cgroup-fair/equal`: equal weights, the second group with three
//!   times the threads. Scheduled as groups they get half each; a
//!   scheduler that treats every thread alike gives the second group
//!   three quarters.
//! * `cgroup-fair/weighted`: the same number of threads, the second
//!   group with three times the weight. It should get three quarters; a
//!   scheduler that ignores weights gives each half.
//!
//! Every thread is busy all the time and each group has at least as many
//! threads as there are CPUs, so each group could use the whole machine
//! and the split is the scheduler's alone.
//!
//! # What is measured
//!
//! The CPU time each thread got during the measured window, read from
//! the threads' own CPU clocks by the measuring thread at both ends of
//! the window, and summed per group.
//!
//! * `weight_accuracy_pct`: how close the ratio of the two groups' CPU
//!   time is to the ratio of their weights, as a percentage of it: 100%
//!   is exact, and getting it wrong by a factor of three is 33% in
//!   either direction. Scored, higher is better. A ratio and not a
//!   difference of shares, so that relative changes mean something: an
//!   error of 0.1 against 0.3 percentage points is not a threefold
//!   difference anyone cares about, 33% against 99% is.
//! * `cpu_used_pct`: the CPU time of both groups as a share of what the
//!   machine has. Scored, higher is better: with more busy threads than
//!   CPUs anything short of all of it is CPUs left idle.
//! * `share_a_pct`: the first group's share of the CPU time used,
//!   informational, with `expected_share_a_pct` (by the weights) and
//!   `flat_share_a_pct` (by the number of threads, what treating every
//!   thread alike gives) next to it, so that a reader sees which rule
//!   the split followed.
//! * `work_per_s`: integer mixing done by all threads, informational.
//!
//! # Control groups
//!
//! Needs cgroup v2 with the `cpu` controller available to children of
//! the root, and the right to create groups there (root). Where that is
//! missing the run is invalid and says what was missing; nothing is
//! measured without the groups.
//!
//! The threads of one process can only be in different groups in a
//! *threaded* subtree. For the run the benchmark creates
//! `helicoprion-<pid>` under the root with two threaded children, gives
//! them their weights, moves its own process into the new group and each
//! thread into its child. Afterwards the process goes back to the group
//! it came from and the three groups are removed, however the run ends.
//!
//! Moving the benchmark's own process changes where everything after it
//! is measured, so none of it is taken on trust. The run is invalid,
//! whatever it measured, when the move changes the CPUs the process may
//! use (the group it came from can have a cpuset that the new one has
//! not), when the process cannot be moved back, when it is not in the
//! group it started in afterwards, or when a group cannot be removed.
//! The runner checks the same after every run of every workload, so a
//! session does not go on unnoticed in another group.
//!
//! The number of CPUs the workload is sized for has to be the number
//! the threads can run on, or there is no contention to split: four
//! threads sized for two CPUs get a CPU each on sixteen, and a scheduler
//! that follows the weights to the letter gives both groups half. That
//! happens when the count was taken under a CPU quota, which the group
//! the benchmark came from has and the new one under the root has not.
//! A run whose CPU count differs from the CPUs in its affinity mask is
//! therefore refused, and one that used more CPU time than that many
//! CPUs have is invalid.

use std::fs;
use std::hint::black_box;
use std::os::unix::thread::JoinHandleExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use helicoprion_core::{Metric, RunResult, Window};
use serde_json::json;

use crate::Workload;
use crate::clock::Alive;

/// How long the threads may take to join their groups before the run is
/// abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// From letting the threads run to the start of the window: time for the
/// scheduler to spread them and for the split to settle.
const SETTLE: Duration = Duration::from_millis(500);
/// CPU time used, as a share of what the CPUs the run was sized for
/// have, above which those were not the CPUs it ran on. Some over 100:
/// the two ends of the window are not read in one instant.
const MAX_CPU_USED_PCT: f64 = 105.0;

const WAIT: u8 = 0;
const RUN: u8 = 1;
const STOP: u8 = 2;

/// Test hooks.
#[derive(Clone, Default)]
struct Hooks {
    /// Run without the cpu controller: the groups and the threads in
    /// them are real, the weights are not set. For machines whose cpu
    /// controller is not on cgroup v2.
    no_cpu_controller: bool,
    /// Creating the thread with this index fails.
    fail_spawn_at: Option<usize>,
    /// The thread with this index never reports ready.
    stall_thread: Option<usize>,
    /// Shorter `READY_TIMEOUT`.
    ready_timeout: Option<Duration>,
    /// Shorter `SETTLE`.
    settle: Option<Duration>,
    /// Where the process is sent back to, instead of where it came from.
    send_home_to: Option<PathBuf>,
    /// Threads running.
    alive: Arc<AtomicUsize>,
}

pub struct CgroupFair {
    /// Which of the suite's cases this is: part of the name.
    pub case: &'static str,
    /// `cpu.weight` of the two groups.
    pub weights: (u32, u32),
    /// Busy threads in the two groups.
    pub threads: (usize, usize),
    /// CPUs of the machine: what `cpu_used_pct` is a share of.
    pub cpus: usize,
}

impl Workload for CgroupFair {
    fn name(&self) -> String {
        format!("cgroup-fair/{}", self.case)
    }

    fn description(&self) -> String {
        format!(
            "two control groups of busy threads, {} with cpu.weight {} and {} with cpu.weight {}; how the CPU time \
             is split between them",
            self.threads.0, self.weights.0, self.threads.1, self.weights.1
        )
    }

    fn params(&self) -> serde_json::Value {
        json!({
            "weight_a": self.weights.0,
            "weight_b": self.weights.1,
            "threads_a": self.threads.0,
            "threads_b": self.threads.1,
            // What `cpu_used_pct` is a share of.
            "cpus": self.cpus,
        })
    }

    fn headline_metrics(&self) -> &'static [&'static str] {
        &["weight_accuracy_pct", "share_a_pct", "cpu_used_pct"]
    }

    fn run(&self, duration: Duration) -> RunResult {
        self.run_with(duration, &Hooks::default())
    }
}

/// The cgroup v2 mount point, from the text of `/proc/self/mountinfo`.
fn v2_mount(mountinfo: &str) -> Option<PathBuf> {
    mountinfo.lines().find_map(|line| {
        // "... <mount point> <options> [optional fields] - <type> ..."
        let (before, after) = line.split_once(" - ")?;
        (after.split(' ').next()? == "cgroup2").then(|| before.split(' ').nth(4).map(PathBuf::from))?
    })
}

/// This process's group in the v2 hierarchy, relative to its mount
/// point, from the text of `/proc/self/cgroup`.
fn own_group(cgroup: &str) -> Option<&str> {
    cgroup.lines().find_map(|line| line.strip_prefix("0::")).map(|path| path.trim_start_matches('/'))
}

fn write(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, value).map_err(|e| format!("writing {value:?} to {}: {e}", path.display()))
}

/// The CPUs this process may run on.
fn allowed_cpus() -> Result<Vec<usize>, String> {
    // SAFETY: an all-zero cpu_set_t is a valid empty set, and the call
    // writes at most its size into it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        return Err(format!("reading the allowed CPUs: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `cpu` is below CPU_SETSIZE.
    Ok((0..libc::CPU_SETSIZE as usize).filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) }).collect())
}

/// The groups of one run. `restore` puts the process back where it was
/// and removes them, and says so if it could not; dropping without it
/// tries the same and can tell nobody.
struct Groups {
    parent: PathBuf,
    /// The group the process came from, relative to the mount point.
    home: String,
    /// `cgroup.procs` of that group.
    home_procs: PathBuf,
    /// Whether the process was moved into `parent`.
    moved: bool,
    restored: bool,
}

impl Groups {
    fn create(weights: (u32, u32), cpu: bool) -> Result<Self, String> {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
        let mount = v2_mount(&mountinfo).ok_or("cgroup v2 is not mounted")?;
        let own = fs::read_to_string("/proc/self/cgroup").map_err(|e| format!("/proc/self/cgroup: {e}"))?;
        let home = own_group(&own).ok_or("this process has no cgroup v2 group")?.to_string();
        let home_procs = mount.join(&home).join("cgroup.procs");
        let parent = mount.join(format!("helicoprion-{}", std::process::id()));
        fs::create_dir(&parent).map_err(|e| {
            format!(
                "creating the control group {}: {e} (cgroup-fair needs root, or a delegated cgroup)",
                parent.display()
            )
        })?;
        // From here on the groups exist: `groups` cleans up.
        let mut groups = Groups { parent, home, home_procs, moved: false, restored: false };
        match groups.set_up(&mount, weights, cpu) {
            Ok(()) => Ok(groups),
            // The reason for giving up comes first; that the clean-up
            // failed as well is said with it.
            Err(why) => Err(match groups.restore() {
                Ok(()) => why,
                Err(also) => format!("{why}; and {also}"),
            }),
        }
    }

    fn set_up(&mut self, mount: &Path, weights: (u32, u32), cpu: bool) -> Result<(), String> {
        if cpu {
            let available = fs::read_to_string(self.parent.join("cgroup.controllers")).unwrap_or_default();
            if !available.split_whitespace().any(|c| c == "cpu") {
                return Err(format!(
                    "the cpu controller is not available to control groups under {} (not enabled for the children \
                     of the root, or bound to a cgroup v1 hierarchy)",
                    mount.display()
                ));
            }
        }
        for group in ["a", "b"] {
            let dir = self.parent.join(group);
            fs::create_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            // Threads of one process in different groups: threaded only.
            write(&dir.join("cgroup.type"), "threaded")?;
        }
        if cpu {
            write(&self.parent.join("cgroup.subtree_control"), "+cpu")?;
            for (group, weight) in [("a", weights.0), ("b", weights.1)] {
                let file = self.parent.join(group).join("cpu.weight");
                write(&file, &weight.to_string())?;
                let got = fs::read_to_string(&file).unwrap_or_default();
                if got.trim() != weight.to_string() {
                    return Err(format!("{} is {:?} after writing {weight}", file.display(), got.trim()));
                }
            }
        }
        // The group the process comes from can limit its CPUs where the
        // new one, under the root, does not: then the threads would run
        // on other CPUs than the ones the session was set up for.
        let before = allowed_cpus()?;
        write(&self.parent.join("cgroup.procs"), &std::process::id().to_string())?;
        self.moved = true;
        let after = allowed_cpus()?;
        if after != before {
            return Err(format!(
                "moving the benchmark to its own control group changed the CPUs it may use, from {} to {}: not the \
                 machine the session was set up for",
                before.len(),
                after.len()
            ));
        }
        Ok(())
    }

    /// Move the calling thread into group `a` or `b`.
    fn join(parent: &Path, group: &str) -> Result<(), String> {
        // SAFETY: gettid takes no arguments and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        write(&parent.join(group).join("cgroup.threads"), &tid.to_string())
    }

    /// Put the process back into the group it came from, check that it
    /// is there, and remove the groups of the run. Every thread that
    /// was moved into them must be gone. Says everything that failed.
    fn restore(&mut self) -> Result<(), String> {
        self.restored = true;
        let mut failed: Vec<String> = Vec::new();
        if self.moved {
            if let Err(e) = fs::write(&self.home_procs, std::process::id().to_string()) {
                failed.push(format!("moving the benchmark back to {}: {e}", self.home_procs.display()));
            }
            // Not what the write returned: where the process is.
            match fs::read_to_string("/proc/self/cgroup") {
                Ok(own) if own_group(&own) == Some(self.home.as_str()) => {}
                Ok(own) => failed.push(format!(
                    "the benchmark is in the control group /{} and not back in /{}",
                    own_group(&own).unwrap_or("?"),
                    self.home
                )),
                Err(e) => failed.push(format!("/proc/self/cgroup: {e}")),
            }
        }
        // The children before the parent; a group that is not there was
        // never created.
        for dir in [self.parent.join("a"), self.parent.join("b"), self.parent.clone()] {
            match fs::remove_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => failed.push(format!("removing the control group {}: {e}", dir.display())),
            }
        }
        if failed.is_empty() { Ok(()) } else { Err(failed.join("; ")) }
    }
}

impl Drop for Groups {
    fn drop(&mut self) {
        // Only where `restore` was not reached, as on a panic.
        if !self.restored {
            let _ = self.restore();
        }
    }
}

/// One counter per cache line.
#[repr(align(64))]
#[derive(Default)]
struct Slot(AtomicU64);

/// CPU time a thread has used, in nanoseconds, read from outside it.
fn cpu_ns(thread: &JoinHandle<Result<(), String>>) -> Result<u64, String> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: the handle is not joined yet, so the pthread exists;
    // `clock` is a valid place for the result.
    let rc = unsafe { libc::pthread_getcpuclockid(thread.as_pthread_t(), &mut clock) };
    if rc != 0 {
        return Err(format!("no CPU clock for a thread: {}", std::io::Error::from_raw_os_error(rc)));
    }
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write to.
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return Err(format!("reading a thread's CPU clock: {}", std::io::Error::last_os_error()));
    }
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

/// How close the ratio of CPU time `b : a` is to the ratio of the
/// weights, in percent of it: 100 is exact, off by a factor of three is
/// 33.3 whichever group got too much.
fn accuracy(cpu: (u64, u64), weights: (u32, u32)) -> f64 {
    if cpu.0 == 0 || cpu.1 == 0 {
        return 0.0;
    }
    let got = cpu.1 as f64 / cpu.0 as f64;
    let wanted = f64::from(weights.1) / f64::from(weights.0);
    let off = got / wanted;
    off.min(1.0 / off) * 100.0
}

impl CgroupFair {
    fn run_with(&self, duration: Duration, hooks: &Hooks) -> RunResult {
        let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
        if self.threads.0 == 0 || self.threads.1 == 0 || self.weights.0 == 0 || self.weights.1 == 0 || self.cpus == 0 {
            return invalid("cgroup-fair needs threads and a weight in both groups".into());
        }
        // Sized for the CPUs it will run on, or the groups do not
        // compete. The count comes from outside and can be one taken
        // under a CPU quota that the run's own group will not have.
        match allowed_cpus() {
            Ok(allowed) if allowed.len() != self.cpus => {
                return invalid(format!(
                    "cgroup-fair was sized for {} CPUs, but the benchmark may run on {}: check for a CPU quota on \
                     the control group it was started in",
                    self.cpus,
                    allowed.len()
                ));
            }
            Ok(_) => {}
            Err(why) => return invalid(why),
        }
        let mut groups = match Groups::create(self.weights, !hooks.no_cpu_controller) {
            Ok(g) => g,
            Err(why) => return invalid(why),
        };
        if let Some(elsewhere) = &hooks.send_home_to {
            groups.home_procs = elsewhere.clone();
        }
        let total = self.threads.0 + self.threads.1;
        let state = Arc::new(AtomicU8::new(WAIT));
        let ready = Arc::new(AtomicUsize::new(0));
        let slots: Arc<[Slot]> = (0..total).map(|_| Slot::default()).collect();
        let mut handles = Vec::with_capacity(total);
        let mut failed: Option<String> = None;
        for i in 0..total {
            let group = if i < self.threads.0 { "a" } else { "b" };
            let (state, ready, slots, hooks) =
                (Arc::clone(&state), Arc::clone(&ready), Arc::clone(&slots), hooks.clone());
            let parent = groups.parent.clone();
            let body = move || -> Result<(), String> {
                let _alive = Alive::new(&hooks.alive);
                Groups::join(&parent, group)?;
                if hooks.stall_thread != Some(i) {
                    ready.fetch_add(1, Ordering::Release);
                }
                while state.load(Ordering::Acquire) == WAIT {
                    std::thread::sleep(Duration::from_micros(200));
                }
                burn(&state, &slots[i].0, i as u64);
                Ok(())
            };
            let spawned = if hooks.fail_spawn_at == Some(i) {
                Err(std::io::Error::other("cannot create a thread (test)"))
            } else {
                std::thread::Builder::new().name(format!("cgroup-{group}-{i}")).spawn(body)
            };
            match spawned {
                Ok(h) => handles.push(h),
                Err(e) => {
                    failed = Some(format!("spawning cgroup threads: {e}"));
                    break;
                }
            }
        }

        let give_up = Instant::now() + hooks.ready_timeout.unwrap_or(READY_TIMEOUT);
        while failed.is_none() && ready.load(Ordering::Acquire) < total {
            if handles.iter().any(|h| h.is_finished()) {
                // Its reason comes out of the join below.
                failed = Some("a cgroup thread stopped before the run started".into());
            } else if Instant::now() >= give_up {
                failed = Some("cgroup threads never became ready".into());
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let mut measured = None;
        if failed.is_none() {
            state.store(RUN, Ordering::Release);
            std::thread::sleep(hooks.settle.unwrap_or(SETTLE));
            match self.measure(&handles, &slots, duration) {
                Ok(m) => measured = Some(m),
                Err(why) => failed = Some(why),
            }
        }
        state.store(STOP, Ordering::Release);
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                // Why a thread stopped says more than that one did.
                Ok(Err(why)) => failed = Some(why),
                Err(_) => failed = failed.or(Some("a cgroup thread panicked".into())),
            }
        }
        // Every thread is gone: the process can go home and the groups
        // can be removed. If that fails the run does not count, whatever
        // it measured: what comes after it runs somewhere else.
        let restored = groups.restore();
        match (failed, measured) {
            (Some(why), _) => invalid(match restored {
                Ok(()) => why,
                Err(also) => format!("{why}; and {also}"),
            }),
            (None, _) if restored.is_err() => invalid(restored.unwrap_err()),
            (None, Some(m)) => self.summarize(&m),
            (None, None) => invalid("nothing was measured".into()),
        }
    }

    /// CPU time per group and work done over `duration`.
    fn measure(
        &self,
        handles: &[JoinHandle<Result<(), String>>],
        slots: &[Slot],
        duration: Duration,
    ) -> Result<Measured, String> {
        let read = || -> Result<(Vec<u64>, u64, Instant), String> {
            let cpu = handles.iter().map(cpu_ns).collect::<Result<Vec<_>, _>>()?;
            Ok((cpu, slots.iter().map(|s| s.0.load(Ordering::Relaxed)).sum(), Instant::now()))
        };
        let (cpu0, work0, at0) = read()?;
        std::thread::sleep(duration);
        let (cpu1, work1, at1) = read()?;
        let used: Vec<u64> = cpu1.iter().zip(&cpu0).map(|(b, a)| b.saturating_sub(*a)).collect();
        let (a, b) = used.split_at(self.threads.0);
        Ok(Measured { cpu: (a.iter().sum(), b.iter().sum()), work: work1 - work0, window: at1 - at0 })
    }

    fn summarize(&self, m: &Measured) -> RunResult {
        let secs = m.window.as_secs_f64();
        let mut r = RunResult { duration_s: secs, ..Default::default() };
        let used = m.cpu.0 + m.cpu.1;
        if used == 0 || secs <= 0.0 {
            r.invalid_reason = Some("the threads used no CPU time".into());
            return r;
        }
        let pct = |part: f64, whole: f64| part / whole * 100.0;
        let (w, t) = (self.weights, self.threads);
        let cpu_used = pct(used as f64 / 1e9, secs * self.cpus as f64);
        if cpu_used > MAX_CPU_USED_PCT {
            // More than the CPUs it was sized for can give: it ran on
            // more, and the groups did not have to share.
            r.invalid_reason = Some(format!(
                "the threads used {cpu_used:.0}% of the CPU time of {} CPUs: they ran on more CPUs than the run was \
                 sized for",
                self.cpus
            ));
            return r;
        }
        r.metrics.insert("weight_accuracy_pct".into(), Metric::higher(accuracy(m.cpu, w), "%"));
        r.metrics.insert("cpu_used_pct".into(), Metric::higher(cpu_used, "%"));
        // Which rule the split followed: the weights, or the number of
        // threads. Both constant for the workload; next to the share so
        // that a report can say it without knowing the parameters.
        r.metrics.insert("share_a_pct".into(), Metric::info(pct(m.cpu.0 as f64, used as f64), "%"));
        r.metrics.insert("expected_share_a_pct".into(), Metric::info(pct(f64::from(w.0), f64::from(w.0 + w.1)), "%"));
        r.metrics.insert("flat_share_a_pct".into(), Metric::info(pct(t.0 as f64, (t.0 + t.1) as f64), "%"));
        r.metrics.insert("work_per_s".into(), Metric::info(m.work as f64 / secs, "units/s"));
        r.windows.insert("cpu".into(), Window { start_s: 0.0, end_s: secs });
        r.counters.insert("cpu_ms_a".into(), m.cpu.0 / 1_000_000);
        r.counters.insert("cpu_ms_b".into(), m.cpu.1 / 1_000_000);
        r
    }
}

struct Measured {
    /// CPU time of the two groups in the window, nanoseconds.
    cpu: (u64, u64),
    work: u64,
    window: Duration,
}

fn burn(state: &AtomicU8, slot: &AtomicU64, seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut units = 0u64;
    while state.load(Ordering::Relaxed) != STOP {
        // A few microseconds of integer mixing per unit.
        for _ in 0..4096 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        black_box(x);
        units += 1;
        slot.store(units, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::Better;

    /// Sized for the machine the test runs on, as a run has to be.
    fn quick() -> CgroupFair {
        CgroupFair { case: "weighted", weights: (100, 300), threads: (2, 2), cpus: allowed_cpus().unwrap().len() }
    }

    fn fast() -> Hooks {
        Hooks { settle: Some(Duration::from_millis(20)), ..Default::default() }
    }

    fn own() -> String {
        fs::read_to_string("/proc/self/cgroup").unwrap()
    }

    /// The tests that move the test process between groups, one at a
    /// time: each compares where the process is before and after.
    static MOVES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn mount_point_and_own_group_are_found() {
        let mountinfo = "36 23 0:28 / /sys/fs/cgroup rw,relatime - tmpfs tmpfs rw,mode=755\n\
                         38 36 0:30 / /sys/fs/cgroup/cpuacct rw,relatime - cgroup cgroup rw,cpuacct\n\
                         46 36 0:38 / /sys/fs/cgroup/unified rw,relatime shared:9 - cgroup2 cgroup2 rw\n";
        assert_eq!(v2_mount(mountinfo), Some(PathBuf::from("/sys/fs/cgroup/unified")));
        let pure =
            "30 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:4 - cgroup2 cgroup2 rw,nsdelegate\n";
        assert_eq!(v2_mount(pure), Some(PathBuf::from("/sys/fs/cgroup")));
        assert_eq!(v2_mount("38 36 0:30 / /sys/fs/cgroup/cpu rw - cgroup cgroup rw,cpu\n"), None);

        assert_eq!(
            own_group("0::/user.slice/user-1000.slice/session-2.scope\n"),
            Some("user.slice/user-1000.slice/session-2.scope")
        );
        // Hybrid: the v1 hierarchies come first; the root is the mount point itself.
        assert_eq!(own_group("1:cpu:/\n0::/\n"), Some(""));
        assert_eq!(own_group("1:cpu:/\n"), None);
    }

    #[test]
    fn accuracy_is_the_ratio_against_the_weights() {
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // Exact, whatever the weights.
        assert!(near(accuracy((250, 750), (100, 300)), 100.0));
        assert!(near(accuracy((500, 500), (100, 100)), 100.0));
        // Weights ignored: half each where it should be 1 : 3.
        assert!(near(accuracy((500, 500), (100, 300)), 100.0 / 3.0));
        // Groups ignored: 1 : 3 by threads where it should be half each.
        assert!(near(accuracy((250, 750), (100, 100)), 100.0 / 3.0));
        // Too much to the heavy group is as wrong as too little.
        assert!(near(accuracy((100, 900), (100, 300)), 100.0 / 3.0));
        // A group that got nothing.
        assert_eq!(accuracy((0, 1000), (100, 300)), 0.0);
        assert_eq!(accuracy((1000, 0), (100, 300)), 0.0);
    }

    #[test]
    fn parameters_identify_the_workload() {
        let w = CgroupFair { cpus: 2, ..quick() };
        assert_eq!(w.name(), "cgroup-fair/weighted");
        assert_eq!(w.params(), json!({ "weight_a": 100, "weight_b": 300, "threads_a": 2, "threads_b": 2, "cpus": 2 }));
        let none = CgroupFair { threads: (0, 2), ..quick() };
        assert!(none.run(Duration::from_millis(10)).invalid_reason.unwrap().contains("needs threads"));
    }

    #[test]
    fn metrics_say_which_rule_the_split_followed() {
        let w = CgroupFair { case: "equal", weights: (100, 100), threads: (4, 12), cpus: 4 };
        // Three seconds on four CPUs; the first group got a quarter.
        let m = Measured { cpu: (3_000_000_000, 9_000_000_000), work: 6000, window: Duration::from_secs(3) };
        let r = w.summarize(&m);
        assert!(r.invalid_reason.is_none());
        let v = |name: &str| r.metrics[name].value;
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // By the weights it should be half; it is what the threads give.
        assert!(
            near(v("share_a_pct"), 25.0) && near(v("expected_share_a_pct"), 50.0) && near(v("flat_share_a_pct"), 25.0)
        );
        assert!(near(v("weight_accuracy_pct"), 100.0 / 3.0));
        assert!(near(v("cpu_used_pct"), 100.0) && near(v("work_per_s"), 2000.0));
        for (name, better) in [
            ("weight_accuracy_pct", Better::Higher),
            ("cpu_used_pct", Better::Higher),
            ("share_a_pct", Better::Neither),
            ("expected_share_a_pct", Better::Neither),
            ("flat_share_a_pct", Better::Neither),
            ("work_per_s", Better::Neither),
        ] {
            assert_eq!(r.metrics[name].better, better, "{name}");
        }
        assert_eq!((r.counters["cpu_ms_a"], r.counters["cpu_ms_b"]), (3000, 9000));
        assert_eq!(r.duration_s, 3.0);
        let idle = Measured { cpu: (0, 0), work: 0, window: Duration::from_secs(1) };
        assert!(w.summarize(&idle).invalid_reason.unwrap().contains("no CPU time"));
    }

    /// With real groups, where the machine lets the test create them
    /// (root and cgroup v2). The cpu controller is left out: it is often
    /// not there for a test, and what is tested is that the threads are
    /// put into their groups, measured, and that nothing is left behind.
    #[test]
    fn threads_run_in_their_groups_and_nothing_is_left_behind() {
        let _one = MOVES.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let hooks = Hooks { no_cpu_controller: true, ..fast() };
        let w = quick();
        let r = w.run_with(Duration::from_millis(200), &hooks);
        if let Some(why) = &r.invalid_reason {
            // No right to create groups here: nothing to test, and
            // nothing may have been changed either.
            assert!(why.contains("creating the control group") || why.contains("not mounted"), "{why}");
            assert_eq!(own(), before);
            return;
        }
        let v = |name: &str| r.metrics[name].value;
        assert!(v("share_a_pct") > 0.0 && v("share_a_pct") < 100.0, "{:?}", r.metrics);
        assert!((0.0..=100.0).contains(&v("weight_accuracy_pct")));
        // Four busy threads: CPU time was used, and no more than the
        // CPUs the run was sized for have.
        assert!(v("cpu_used_pct") > 0.0 && v("cpu_used_pct") <= MAX_CPU_USED_PCT, "{:?}", r.metrics);
        assert!(r.counters["cpu_ms_a"] > 0 && r.counters["cpu_ms_b"] > 0);
        assert!(v("work_per_s") > 0.0);
        // Back where it was, and the groups are gone.
        assert_eq!(own(), before);
        assert_eq!(hooks.alive.load(Ordering::SeqCst), 0);
        let mount = v2_mount(&fs::read_to_string("/proc/self/mountinfo").unwrap()).unwrap();
        assert!(!mount.join(format!("helicoprion-{}", std::process::id())).exists());

        // Whatever fails, the same holds.
        for broken in [
            Hooks { fail_spawn_at: Some(2), ..hooks.clone() },
            Hooks { stall_thread: Some(1), ready_timeout: Some(Duration::from_millis(50)), ..hooks.clone() },
        ] {
            let r = w.run_with(Duration::from_millis(50), &broken);
            let why = r.invalid_reason.expect("invalid");
            assert!(why.contains("spawning cgroup threads") || why.contains("never became ready"), "{why}");
            assert_eq!(own(), before);
            assert_eq!(broken.alive.load(Ordering::SeqCst), 0);
            assert!(!mount.join(format!("helicoprion-{}", std::process::id())).exists());
        }
    }

    /// A process that cannot be put back where it came from: the run
    /// says so and does not count, whatever it measured.
    #[test]
    fn a_process_that_is_not_put_back_invalidates_the_run() {
        let _one = MOVES.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let mount = v2_mount(&fs::read_to_string("/proc/self/mountinfo").unwrap_or_default());
        let nowhere = PathBuf::from("/nonexistent/cgroup.procs");
        let hooks = Hooks { no_cpu_controller: true, send_home_to: Some(nowhere), ..fast() };
        let r = quick().run_with(Duration::from_millis(50), &hooks);
        let why = r.invalid_reason.expect("invalid");
        if why.contains("creating the control group") || why.contains("not mounted") {
            return;
        }
        assert!(why.contains("moving the benchmark back to /nonexistent/cgroup.procs"), "{why}");
        // Where it is, not only that a write failed; and the group it
        // still sits in could not be removed.
        let stuck = format!("helicoprion-{}", std::process::id());
        assert!(why.contains(&format!("the benchmark is in the control group /{stuck} and not back in /")), "{why}");
        assert!(
            why.contains(&format!("removing the control group {}", mount.as_ref().unwrap().join(&stuck).display())),
            "{why}"
        );
        assert!(r.metrics.is_empty(), "nothing to compare: {:?}", r.metrics);
        // The runner would flag every later run too.
        assert!(crate::runner::moved(Some(&before), Some(&own())).is_some());

        // Put the test process back by hand.
        let home = mount.as_ref().unwrap().join(own_group(&before).unwrap());
        fs::write(home.join("cgroup.procs"), std::process::id().to_string()).unwrap();
        fs::remove_dir(mount.unwrap().join(&stuck)).unwrap();
        assert_eq!(own(), before);
    }

    /// A count of CPUs that is not the CPUs the threads can run on: a
    /// quota on the group the benchmark was started in gives a small
    /// count, the run's own group has no quota, and then nothing has to
    /// be shared. Such a run is refused before anything is moved, and
    /// one that used more CPU time than its CPUs have does not count.
    #[test]
    fn a_run_sized_for_other_cpus_than_it_runs_on_is_refused() {
        let before = own();
        let real = allowed_cpus().unwrap().len();
        for cpus in [real + 1, real + 14] {
            let w = CgroupFair { cpus, ..quick() };
            let why = w.run_with(Duration::from_millis(50), &fast()).invalid_reason.expect("invalid");
            assert_eq!(
                why,
                format!(
                    "cgroup-fair was sized for {cpus} CPUs, but the benchmark may run on {real}: check for a CPU \
                     quota on the control group it was started in"
                )
            );
        }
        // Nothing was created or moved for it.
        assert_eq!(own(), before);

        // Sized for two CPUs under a quota, run on sixteen: four threads
        // with a CPU each. Both groups get half, as they would under a
        // scheduler that follows the weights, and it must not be scored.
        let w = CgroupFair { case: "weighted", weights: (100, 300), threads: (2, 2), cpus: 2 };
        let m = Measured { cpu: (2_000_000_000, 2_000_000_000), work: 4000, window: Duration::from_secs(1) };
        let r = w.summarize(&m);
        let why = r.invalid_reason.expect("invalid");
        assert_eq!(
            why,
            "the threads used 200% of the CPU time of 2 CPUs: they ran on more CPUs than the run was sized for"
        );
        assert!(r.metrics.is_empty(), "{:?}", r.metrics);
        // Within what two readings of the clocks can differ by: counts.
        let m = Measured { cpu: (510_000_000, 1_530_000_000), work: 4000, window: Duration::from_secs(1) };
        let r = w.summarize(&m);
        assert!(r.invalid_reason.is_none() && r.metrics["cpu_used_pct"].value <= MAX_CPU_USED_PCT, "{:?}", r.metrics);
    }

    #[test]
    fn allowed_cpus_are_read() {
        let cpus = allowed_cpus().unwrap();
        assert!(!cpus.is_empty() && cpus.windows(2).all(|w| w[0] < w[1]), "{cpus:?}");
    }

    /// Without the controller the run says so and measures nothing.
    #[test]
    fn a_machine_without_the_cpu_controller_is_told_so() {
        let _one = MOVES.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let r = quick().run_with(Duration::from_millis(50), &fast());
        if let Some(why) = r.invalid_reason {
            assert!(
                why.contains("cpu controller is not available")
                    || why.contains("creating the control group")
                    || why.contains("not mounted")
                    || why.contains("writing"),
                "{why}"
            );
            assert!(r.metrics.is_empty());
        }
        assert_eq!(own(), before);
    }
}
