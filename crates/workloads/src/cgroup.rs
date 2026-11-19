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
//! `helicoprion-<pid>` under the root with two threaded children and
//! gives them their weights. Afterwards the three groups are removed,
//! however the run ends.
//!
//! # A process of its own
//!
//! The threads do not belong to the benchmark's process. Putting threads
//! into the children of a group needs their process in that group, and
//! moving a process moves every thread it has: the benchmark itself
//! would change control groups for the run, and everything measured
//! after it would depend on it having been put back, into a group that
//! may by then refuse it. So the benchmark starts itself a second time
//! (`/proc/self/exe`, told what to run by `CHILD_ENV`), and that process
//! moves into the group, starts the threads, measures them and writes
//! its result as one line. It never moves back: it ends, and with it
//! every thread it had, whatever went wrong inside. The benchmark stays
//! where it was started and has nothing to restore.
//!
//! What the benchmark does itself is create the groups before and
//! remove them after, when the other process is gone. A run is invalid
//! when that process gives no result (it could not be started, was
//! killed, or wrote something else), when the move changed the CPUs it
//! may use (the group the benchmark was started in can have a cpuset
//! that the new one has not), or when a group cannot be removed. The
//! process ends itself if the benchmark dies, and after a time no run
//! can take; it checks that the benchmark is still its parent before it
//! moves or starts anything, since a parent that died earlier is not
//! reported. A benchmark that is killed cannot remove the groups: the
//! measuring process ends, and three empty groups stay until someone
//! removes them or the machine restarts. An interrupt from the terminal does not reach it: the
//! benchmark gets it and stops after the run in hand, as with any other
//! workload. The runner still checks after every run of every workload
//! that the benchmark is in the groups it started in: nothing in a run
//! moves it now, so if it has moved, something else did.
//!
//! A program that runs this workload has to call `child_main` before
//! anything else, as the `helicoprion` binary does: it is what turns
//! the second start into the measuring process.
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
use std::os::unix::process::CommandExt;
use std::os::unix::thread::JoinHandleExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use helicoprion_core::{Metric, RunResult, Window};
use serde_json::{Value, json};

use crate::Workload;
use crate::schedstat::{self, Collector};

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

/// On top of everything a run waits for, before the measuring process
/// ends itself: no run takes this long.
const CHILD_MARGIN: Duration = Duration::from_secs(60);

/// Set for the measuring process: what it is to run, as JSON. A program
/// started with it runs that and nothing else (`child_main`).
pub const CHILD_ENV: &str = "HELICOPRION_CGROUP_FAIR_CHILD";
/// What the line with the measuring process's result begins with. The
/// rest of what it writes is not the benchmark's to interpret.
const RESULT_PREFIX: &str = "helicoprion-cgroup-fair-result ";

const WAIT: u8 = 0;
const RUN: u8 = 1;
const STOP: u8 = 2;

/// Test hooks for the measuring process; they travel with its orders.
#[derive(Clone, Default, PartialEq, Debug)]
struct ChildHooks {
    /// Creating the thread with this index fails.
    fail_spawn_at: Option<usize>,
    /// The thread with this index never reports ready.
    stall_thread: Option<usize>,
    /// Shorter `READY_TIMEOUT`.
    ready_timeout: Option<Duration>,
    /// Shorter `SETTLE`.
    settle: Option<Duration>,
    /// The process is killed while its threads are at work, in their
    /// groups: as by the kernel for lack of memory.
    die_at_work: bool,
}

/// Test hooks.
#[derive(Clone, Default)]
struct Hooks {
    /// Run without the cpu controller: the groups and the threads in
    /// them are real, the weights are not set. For machines whose cpu
    /// controller is not on cgroup v2.
    no_cpu_controller: bool,
    /// The program to start as the measuring process, and its
    /// arguments, instead of this one.
    launch: Option<Vec<String>>,
    child: ChildHooks,
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
    /// Read every thread's scheduler statistics.
    pub sched_diag: bool,
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
        let params = json!({
            "weight_a": self.weights.0,
            "weight_b": self.weights.1,
            "threads_a": self.threads.0,
            "threads_b": self.threads.1,
            // What `cpu_used_pct` is a share of.
            "cpus": self.cpus,
        });
        schedstat::mark(params, self.sched_diag)
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

/// How long a group may stay busy after its last thread was joined.
const GROUP_BUSY: Duration = Duration::from_secs(2);

/// Remove a group that no longer has members. A thread that was joined
/// has finished, but the kernel takes it out of its group a moment
/// later, when the task is released, and until then removing the group
/// fails as busy. Try again for a while before taking that for final.
fn remove_group(dir: &Path) -> std::io::Result<()> {
    let give_up = Instant::now() + GROUP_BUSY;
    loop {
        match fs::remove_dir(dir) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) && Instant::now() < give_up => {
                std::thread::sleep(Duration::from_millis(1));
            }
            done => return done,
        }
    }
}

/// The groups of one run: `helicoprion-<pid>` under the root, with the
/// threaded children `a` and `b`. `remove` takes them away again and
/// says so if it could not; dropping without it tries the same and can
/// tell nobody.
struct Groups {
    parent: PathBuf,
    removed: bool,
}

impl Groups {
    fn create(weights: (u32, u32), cpu: bool) -> Result<Self, String> {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
        let mount = v2_mount(&mountinfo).ok_or("cgroup v2 is not mounted")?;
        let parent = mount.join(format!("helicoprion-{}", std::process::id()));
        fs::create_dir(&parent).map_err(|e| {
            format!(
                "creating the control group {}: {e} (cgroup-fair needs root, or a delegated cgroup)",
                parent.display()
            )
        })?;
        // From here on the groups exist: `groups` cleans up.
        let mut groups = Groups { parent, removed: false };
        match groups.set_up(&mount, weights, cpu) {
            Ok(()) => Ok(groups),
            // The reason for giving up comes first; that the clean-up
            // failed as well is said with it.
            Err(why) => Err(match groups.remove() {
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
        Ok(())
    }

    /// Move the calling thread into group `a` or `b`.
    fn join(parent: &Path, group: &str) -> Result<(), String> {
        // SAFETY: gettid takes no arguments and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        write(&parent.join(group).join("cgroup.threads"), &tid.to_string())
    }

    /// Remove the groups of the run. The process that was in them must
    /// be gone. Says everything that failed.
    fn remove(&mut self) -> Result<(), String> {
        self.removed = true;
        let mut failed: Vec<String> = Vec::new();
        // The children before the parent; a group that is not there was
        // never created.
        for dir in [self.parent.join("a"), self.parent.join("b"), self.parent.clone()] {
            match remove_group(&dir) {
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
        // Only where `remove` was not reached, as on a panic.
        if !self.removed {
            let _ = self.remove();
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

/// What the measuring process is to run: the workload, for how long,
/// and in which group.
struct Orders {
    weights: (u32, u32),
    threads: (usize, usize),
    cpus: usize,
    sched_diag: bool,
    duration: Duration,
    /// The run's group: the process moves into it, its threads into the
    /// children.
    parent: PathBuf,
    /// The benchmark that gives the orders: the process the measuring
    /// one must be a child of, and must not outlive.
    benchmark: u32,
    hooks: ChildHooks,
}

impl Orders {
    fn to_json(&self) -> Value {
        let ms = |d: Option<Duration>| d.map(|d| d.as_millis() as u64);
        json!({
            "weights": [self.weights.0, self.weights.1],
            "threads": [self.threads.0, self.threads.1],
            "cpus": self.cpus,
            "sched_diag": self.sched_diag,
            "duration_ns": self.duration.as_nanos() as u64,
            "parent": self.parent,
            "benchmark": self.benchmark,
            "fail_spawn_at": self.hooks.fail_spawn_at,
            "stall_thread": self.hooks.stall_thread,
            "ready_timeout_ms": ms(self.hooks.ready_timeout),
            "settle_ms": ms(self.hooks.settle),
            "die_at_work": self.hooks.die_at_work,
        })
    }

    fn from_json(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let number = |key: &str| v.get(key).and_then(Value::as_u64).ok_or(format!("no number {key:?}"));
        let pair = |key: &str| -> Result<(u64, u64), String> {
            let of = |i: usize| v.get(key).and_then(|p| p.get(i)).and_then(Value::as_u64);
            of(0).zip(of(1)).ok_or(format!("no pair {key:?}"))
        };
        let optional = |key: &str| v.get(key).and_then(Value::as_u64);
        let (weights, threads) = (pair("weights")?, pair("threads")?);
        Ok(Orders {
            weights: (weights.0 as u32, weights.1 as u32),
            threads: (threads.0 as usize, threads.1 as usize),
            cpus: number("cpus")? as usize,
            sched_diag: v.get("sched_diag").and_then(Value::as_bool).ok_or("no \"sched_diag\"")?,
            duration: Duration::from_nanos(number("duration_ns")?),
            parent: v.get("parent").and_then(Value::as_str).map(PathBuf::from).ok_or("no \"parent\"")?,
            benchmark: u32::try_from(number("benchmark")?).map_err(|_| "\"benchmark\" is no process id")?,
            hooks: ChildHooks {
                fail_spawn_at: optional("fail_spawn_at").map(|i| i as usize),
                stall_thread: optional("stall_thread").map(|i| i as usize),
                ready_timeout: optional("ready_timeout_ms").map(Duration::from_millis),
                settle: optional("settle_ms").map(Duration::from_millis),
                die_at_work: v.get("die_at_work").and_then(Value::as_bool).unwrap_or(false),
            },
        })
    }
}

/// If this process was started as the measuring process of a cgroup-fair
/// run, the line with its result; `None` in any other process.
fn child_report() -> Option<String> {
    let orders = std::env::var(CHILD_ENV).ok()?;
    let r = match Orders::from_json(&orders) {
        Ok(orders) => measure_here(&orders),
        Err(why) => RunResult { invalid_reason: Some(format!("{CHILD_ENV}: {why}")), ..Default::default() },
    };
    let json = serde_json::to_string(&r).unwrap_or_else(|e| {
        json!({ "valid": false, "invalid_reason": format!("writing the result: {e}"), "duration_s": 0.0, "metrics": {} })
            .to_string()
    });
    Some(format!("{RESULT_PREFIX}{json}"))
}

/// To be called first thing by a program that runs this workload. In the
/// process that the workload starts to do its measuring this runs the
/// measurement, writes the result and ends the process; in any other it
/// returns at once.
pub fn child_main() {
    if let Some(line) = child_report() {
        println!("{line}");
        std::process::exit(0);
    }
}

/// The measuring process: move into the run's group, put the threads
/// into its children, measure them. Nothing is undone at the end: the
/// process ends, and the benchmark that started it removes the groups.
fn measure_here(o: &Orders) -> RunResult {
    let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
    let hooks = &o.hooks;
    let (ready_timeout, settle) = (hooks.ready_timeout.unwrap_or(READY_TIMEOUT), hooks.settle.unwrap_or(SETTLE));
    // Not without the benchmark, and not for ever: busy threads on
    // every CPU are not to be left behind by a benchmark that was
    // killed, or by a fault in here.
    // SAFETY: both calls take plain numbers and change only this
    // process: a signal when the parent dies, and one after a time.
    let asked = unsafe {
        libc::alarm((o.duration + ready_timeout + settle + CHILD_MARGIN).as_secs().min(u64::from(u32::MAX)) as u32);
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong)
    };
    if asked != 0 {
        return invalid(format!(
            "the measuring process cannot ask to end with the benchmark: {}",
            std::io::Error::last_os_error()
        ));
    }
    // The signal is for a parent that dies from now on. One that died
    // between starting this process and the call above is not reported
    // any more: look. Nothing has been moved or started yet.
    // SAFETY: getppid takes no arguments and cannot fail.
    let parent = unsafe { libc::getppid() };
    if u32::try_from(parent) != Ok(o.benchmark) {
        return invalid(format!(
            "the measuring process is the child of process {parent} and not of the benchmark, process {}: the \
             benchmark is gone, or this was not started by it",
            o.benchmark
        ));
    }
    // The group the benchmark was started in can limit its CPUs where
    // the new one, under the root, does not: then the threads would run
    // on other CPUs than the ones the session was set up for.
    let before = match allowed_cpus() {
        Ok(cpus) => cpus,
        Err(why) => return invalid(why),
    };
    if let Err(why) = write(&o.parent.join("cgroup.procs"), &std::process::id().to_string()) {
        return invalid(why);
    }
    // Not what the write returned: where the process is.
    match fs::read_to_string("/proc/self/cgroup") {
        Ok(own) if own_group(&own).is_some_and(|g| Path::new(g).file_name() == o.parent.file_name()) => {}
        Ok(own) => {
            return invalid(format!(
                "the measuring process is in the control group /{} and not in {}",
                own_group(&own).unwrap_or("?"),
                o.parent.display()
            ));
        }
        Err(e) => return invalid(format!("/proc/self/cgroup: {e}")),
    }
    match allowed_cpus() {
        Ok(after) if after != before => {
            return invalid(format!(
                "moving into the run's control group changed the CPUs the threads may use, from {} to {}: not the \
                 machine the session was set up for",
                before.len(),
                after.len()
            ));
        }
        Ok(after) if after.len() != o.cpus => {
            return invalid(format!(
                "cgroup-fair was sized for {} CPUs, but its threads may run on {}",
                o.cpus,
                after.len()
            ));
        }
        Ok(_) => {}
        Err(why) => return invalid(why),
    }

    let total = o.threads.0 + o.threads.1;
    let state = Arc::new(AtomicU8::new(WAIT));
    let ready = Arc::new(AtomicUsize::new(0));
    let slots: Arc<[Slot]> = (0..total).map(|_| Slot::default()).collect();
    let stats = Arc::new(Collector::new(o.sched_diag));
    let mut handles = Vec::with_capacity(total);
    let mut failed: Option<String> = None;
    for i in 0..total {
        let group = if i < o.threads.0 { "a" } else { "b" };
        let (state, ready, slots) = (Arc::clone(&state), Arc::clone(&ready), Arc::clone(&slots));
        let (parent, stats, stall) = (o.parent.clone(), Arc::clone(&stats), hooks.stall_thread == Some(i));
        let body = move || -> Result<(), String> {
            Groups::join(&parent, group)?;
            if !stall {
                ready.fetch_add(1, Ordering::Release);
            }
            while state.load(Ordering::Acquire) == WAIT {
                std::thread::sleep(Duration::from_micros(200));
            }
            // All of its work, the time before the window included.
            let _watching = stats.watch();
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

    let give_up = Instant::now() + ready_timeout;
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

    let workload =
        CgroupFair { case: "", weights: o.weights, threads: o.threads, cpus: o.cpus, sched_diag: o.sched_diag };
    let mut measured = None;
    if failed.is_none() {
        state.store(RUN, Ordering::Release);
        std::thread::sleep(settle);
        if hooks.die_at_work {
            // SAFETY: a signal to this process; it does not return.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
        }
        match workload.measure(&handles, &slots, o.duration) {
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
    match (failed, measured) {
        (Some(why), _) => invalid(why),
        (None, Some(m)) => {
            let mut r = workload.summarize(&m);
            stats.record(&mut r);
            r
        }
        (None, None) => invalid("nothing was measured".into()),
    }
}

/// What a process wrote, cut to its end: enough to say why it failed.
fn tail(text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    let text = text.trim();
    let from = text.char_indices().rev().nth(399).map_or(0, |(i, _)| i);
    text[from..].replace('\n', " | ")
}

/// Start the measuring process, wait for it to end and take its result.
fn launch(orders: &Orders, hooks: &Hooks) -> Result<RunResult, String> {
    // This program again, by the file it runs from and not by a path
    // that a package update may have put another version under.
    let own = ["/proc/self/exe".to_string()];
    let command = hooks.launch.as_deref().unwrap_or(&own);
    let out = Command::new(&command[0])
        .args(&command[1..])
        .env(CHILD_ENV, orders.to_json().to_string())
        .stdin(Stdio::null())
        // A process group of its own: Ctrl-C at the terminal goes to the
        // benchmark, which finishes the run in hand before it stops, and
        // not to the run itself, which would end it without a result.
        .process_group(0)
        .output()
        .map_err(|e| format!("starting the measuring process of cgroup-fair ({}): {e}", command[0]))?;
    // Waited for: the process and all of its threads are gone.
    let said = String::from_utf8_lossy(&out.stdout);
    match said.lines().rev().find_map(|line| line.strip_prefix(RESULT_PREFIX)) {
        // A result from a process that then did not end as it should is
        // not one: it was killed, or failed, between writing and ending,
        // and what it wrote before is not to be trusted with the rest.
        Some(_) if !out.status.success() => Err(format!(
            "the measuring process of cgroup-fair wrote a result and then did not end as it should ({})",
            out.status
        )),
        Some(json) => serde_json::from_str(json)
            .map_err(|e| format!("the result of the measuring process of cgroup-fair could not be read: {e}")),
        None => {
            let wrote = tail(&out.stderr);
            Err(format!(
                "the measuring process of cgroup-fair gave no result ({}){}",
                out.status,
                if wrote.is_empty() { String::new() } else { format!(": {wrote}") }
            ))
        }
    }
}

impl CgroupFair {
    fn run_with(&self, duration: Duration, hooks: &Hooks) -> RunResult {
        let invalid = |why: String| RunResult { invalid_reason: Some(why), ..Default::default() };
        if self.threads.0 == 0 || self.threads.1 == 0 || self.weights.0 == 0 || self.weights.1 == 0 || self.cpus == 0 {
            return invalid("cgroup-fair needs threads and a weight in both groups".into());
        }
        // A measuring process that got here was not turned into one:
        // the program did not call `child_main`. Starting another would
        // start one more each time.
        if std::env::var_os(CHILD_ENV).is_some() {
            return invalid(format!(
                "cgroup-fair was started inside its own measuring process ({CHILD_ENV} is set): the program has \
                 to call cgroup::child_main() before anything else"
            ));
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
        let orders = Orders {
            weights: self.weights,
            threads: self.threads,
            cpus: self.cpus,
            sched_diag: self.sched_diag,
            duration,
            parent: groups.parent.clone(),
            benchmark: std::process::id(),
            hooks: hooks.child.clone(),
        };
        let outcome = launch(&orders, hooks);
        // The measuring process has ended, with a result or without:
        // the groups are empty and can go. If one cannot, the run does
        // not count, whatever it measured: something is left behind.
        let removed = groups.remove();
        let also = |why: String| match &removed {
            Ok(()) => why,
            Err(left) => format!("{why}; and {left}"),
        };
        match outcome {
            Err(why) => invalid(also(why)),
            Ok(r) => match (&r.invalid_reason, &removed) {
                (Some(why), _) => invalid(also(why.clone())),
                (None, Err(left)) => invalid(left.clone()),
                (None, Ok(())) => r,
            },
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
        CgroupFair {
            case: "weighted",
            weights: (100, 300),
            threads: (2, 2),
            cpus: allowed_cpus().unwrap().len(),
            sched_diag: false,
        }
    }

    /// The test binary as the measuring process: the one test in it
    /// that is one.
    fn as_test() -> Vec<String> {
        // The names as the test harness has them: without the crate.
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let name = format!("{module}::measuring_process");
        [exe.as_str(), "--ignored", "--exact", name.as_str(), "--nocapture", "--test-threads=1"]
            .map(String::from)
            .into()
    }

    /// Real groups without the cpu controller, which is often not there
    /// for a test; the measuring process is the test binary.
    fn fast() -> Hooks {
        Hooks {
            no_cpu_controller: true,
            launch: Some(as_test()),
            child: ChildHooks { settle: Some(Duration::from_millis(20)), ..Default::default() },
        }
    }

    /// Another program as the measuring process.
    fn sh(script: &str) -> Hooks {
        Hooks { launch: Some(["/bin/sh", "-c", script].map(String::from).into()), ..fast() }
    }

    fn own() -> String {
        fs::read_to_string("/proc/self/cgroup").unwrap()
    }

    /// Where the groups of a run of this process would be.
    fn run_group() -> Option<PathBuf> {
        let mount = v2_mount(&fs::read_to_string("/proc/self/mountinfo").ok()?)?;
        Some(mount.join(format!("helicoprion-{}", std::process::id())))
    }

    /// No right to create groups here: nothing to test.
    fn no_groups(why: &str) -> bool {
        why.contains("creating the control group") || why.contains("not mounted")
    }

    /// The groups of a run are named after the process that creates
    /// them, and the tests are one process: one run at a time.
    static GROUPS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Not a test of its own: what the test binary does when a test
    /// starts it as the measuring process, as `child_main` does in the
    /// benchmark. Run in any other way it does nothing.
    #[test]
    #[ignore = "the measuring process of the tests below"]
    fn measuring_process() {
        if let Some(line) = child_report() {
            // On a line of its own: the harness has begun one.
            println!("\n{line}");
        }
    }

    /// Not a test of its own either: a program that did not call
    /// `child_main` and so runs the workload inside the measuring
    /// process. Says what the run answered.
    #[test]
    #[ignore = "run by a_program_that_does_not_hand_over_is_stopped"]
    fn workload_inside_the_measuring_process() {
        if std::env::var_os(CHILD_ENV).is_some() {
            println!("answer: {:?}", quick().run(Duration::from_millis(10)).invalid_reason);
        }
    }

    #[test]
    fn a_program_that_does_not_hand_over_is_stopped() {
        let mut command = as_test();
        command[3] = command[3].replace("measuring_process", "workload_inside_the_measuring_process");
        let out = Command::new(&command[0]).args(&command[1..]).env(CHILD_ENV, "{}").output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        // It does not start a process of its own again, and says why.
        assert!(said.contains("answer: Some(") && said.contains("has to call cgroup::child_main()"), "{said}");
    }

    #[test]
    fn orders_reach_the_measuring_process_as_they_were_given() {
        let orders = Orders {
            weights: (100, 300),
            threads: (16, 48),
            cpus: 16,
            sched_diag: true,
            duration: Duration::from_nanos(5_000_000_001),
            parent: PathBuf::from("/sys/fs/cgroup/helicoprion-4242"),
            benchmark: 4242,
            hooks: ChildHooks {
                fail_spawn_at: Some(2),
                stall_thread: None,
                ready_timeout: Some(Duration::from_millis(50)),
                settle: Some(Duration::from_millis(20)),
                die_at_work: true,
            },
        };
        let back = Orders::from_json(&orders.to_json().to_string()).unwrap();
        assert_eq!((back.weights, back.threads, back.cpus, back.sched_diag), ((100, 300), (16, 48), 16, true));
        assert_eq!((back.duration, &back.parent, back.benchmark), (orders.duration, &orders.parent, 4242));
        assert_eq!(back.hooks, orders.hooks);
        // Without hooks, as the benchmark sends them.
        let plain = Orders { hooks: ChildHooks::default(), ..orders };
        assert_eq!(Orders::from_json(&plain.to_json().to_string()).unwrap().hooks, ChildHooks::default());
        // Orders without the test's part, as another version would send.
        let mut bare = plain.to_json();
        bare.as_object_mut().unwrap().retain(|k, _| !["die_at_work", "settle_ms"].contains(&k.as_str()));
        assert_eq!(Orders::from_json(&bare.to_string()).unwrap().hooks, ChildHooks::default());
        // Orders that are not orders are refused, with what is missing.
        assert!(Orders::from_json("nope").err().unwrap().starts_with("not JSON"));
        assert_eq!(Orders::from_json("{}").err().unwrap(), "no pair \"weights\"");
        let mut partial = plain.to_json();
        partial.as_object_mut().unwrap().remove("parent");
        assert_eq!(Orders::from_json(&partial.to_string()).err().unwrap(), "no \"parent\"");
        let mut huge = plain.to_json();
        huge["benchmark"] = json!(u64::from(u32::MAX) + 1);
        assert_eq!(Orders::from_json(&huge.to_string()).err().unwrap(), "\"benchmark\" is no process id");

        // Orders from a benchmark that is not the parent of the process
        // that gets them, as when the benchmark died while the process
        // was starting: refused before anything is moved or started.
        let orphan = Orders { parent: PathBuf::from("/nonexistent/helicoprion-1"), benchmark: 1, ..plain };
        let command = as_test();
        let out = Command::new(&command[0])
            .args(&command[1..])
            .env(CHILD_ENV, orphan.to_json().to_string())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        let line = said.lines().find_map(|l| l.strip_prefix(RESULT_PREFIX)).expect("a result line");
        let r: RunResult = serde_json::from_str(line).unwrap();
        let why = r.invalid_reason.unwrap();
        // Not the group that is not there: it never got that far.
        assert_eq!(
            why,
            format!(
                "the measuring process is the child of process {} and not of the benchmark, process 1: the \
                 benchmark is gone, or this was not started by it",
                std::process::id()
            )
        );

        // A process started with orders it cannot read says so in its
        // result, and measures nothing.
        let command = as_test();
        let out = Command::new(&command[0]).args(&command[1..]).env(CHILD_ENV, "nope").output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        let line = said.lines().find_map(|l| l.strip_prefix(RESULT_PREFIX)).expect("a result line");
        let r: RunResult = serde_json::from_str(line).unwrap();
        assert!(r.invalid_reason.unwrap().starts_with(&format!("{CHILD_ENV}: not JSON")) && r.metrics.is_empty());
        // And a process that was not given any is no measuring process.
        assert!(child_report().is_none());
    }

    #[test]
    fn what_a_failed_process_wrote_is_cut_to_its_end() {
        assert_eq!(tail(b"  one\ntwo\n"), "one | two");
        assert_eq!(tail(b""), "");
        let long = "x".repeat(1000) + "the end";
        assert_eq!(tail(long.as_bytes()).chars().count(), 400);
        assert!(tail(long.as_bytes()).ends_with("the end"));
        // Not cut inside a character.
        assert_eq!(tail("ż".repeat(500).as_bytes()), "ż".repeat(400));
    }

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
        let w = CgroupFair { case: "equal", weights: (100, 100), threads: (4, 12), cpus: 4, sched_diag: false };
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
    /// (root and cgroup v2): the threads are put into their groups by a
    /// process of their own and measured there, the test process stays
    /// where it is, and nothing is left behind.
    #[test]
    fn threads_run_in_a_process_of_their_own_and_nothing_is_left_behind() {
        let _one = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let w = quick();
        let r = w.run_with(Duration::from_millis(200), &fast());
        // Whatever happened, this process did not move.
        assert_eq!(own(), before);
        if let Some(why) = &r.invalid_reason {
            assert!(no_groups(why), "{why}");
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
        // The window is the measuring process's own, and came across.
        assert!(r.duration_s >= 0.2 && r.windows["cpu"].end_s == r.duration_s, "{:?}", r.windows);
        // The groups are gone.
        let group = run_group().unwrap();
        assert!(!group.exists());

        // Whatever fails in the measuring process, the same holds.
        for (child, expected) in [
            (ChildHooks { fail_spawn_at: Some(2), ..fast().child }, "spawning cgroup threads"),
            (
                ChildHooks { stall_thread: Some(1), ready_timeout: Some(Duration::from_millis(50)), ..fast().child },
                "never became ready",
            ),
        ] {
            let r = w.run_with(Duration::from_millis(50), &Hooks { child, ..fast() });
            let why = r.invalid_reason.expect("invalid");
            assert!(why.contains(expected), "{why}");
            assert!(r.metrics.is_empty());
            assert_eq!(own(), before);
            assert!(!group.exists());
        }
    }

    /// With scheduler statistics, where the machine lets the test
    /// create groups: busy threads that never sleep, so what they have
    /// is time on a CPU, time waiting for one and preemptions. Read in
    /// the measuring process, where the threads are.
    #[test]
    fn scheduler_statistics_cover_every_thread() {
        let _one = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        let w = CgroupFair { sched_diag: true, ..quick() };
        assert_eq!(w.params()["sched_diag"], true);
        assert!(quick().params().get("sched_diag").is_none());
        let r = w.run_with(Duration::from_millis(200), &fast());
        // No groups here, or a kernel without the statistics.
        if !schedstat::kept(&r) || r.invalid_reason.is_some() {
            return;
        }
        let c = &r.counters;
        assert_eq!(c["sched_threads"], 4);
        assert!(c["sched_run_ms"] > 0 && c["sched_runnable_ms"] >= c["sched_run_ms"], "{c:?}");
        assert!(r.metrics.contains_key("sched_wait_pct"));
    }

    /// A measuring process that gives no result, for whatever reason:
    /// the run says what it knows and does not count, the groups are
    /// removed and the benchmark is where it was.
    #[test]
    fn a_measuring_process_without_a_result_invalidates_the_run() {
        let _one = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let run = |hooks: Hooks| {
            let r = quick().run_with(Duration::from_millis(50), &hooks);
            assert!(r.metrics.is_empty(), "nothing to compare: {:?}", r.metrics);
            assert_eq!(own(), before);
            assert!(run_group().is_none_or(|g| !g.exists()));
            r.invalid_reason.expect("invalid")
        };
        // It ends without one.
        let why = run(sh("echo something went wrong >&2; exit 3"));
        if no_groups(&why) {
            return;
        }
        assert_eq!(why, "the measuring process of cgroup-fair gave no result (exit status: 3): something went wrong");
        // It is killed, as by the kernel for lack of memory.
        let why = run(sh("kill -9 $$"));
        assert_eq!(why, "the measuring process of cgroup-fair gave no result (signal: 9 (SIGKILL))");
        // The real one, killed while its threads are at work in their
        // groups: no result, the threads are gone with it, and the
        // groups they were in can be removed.
        let why = run(Hooks { child: ChildHooks { die_at_work: true, ..fast().child }, ..fast() });
        assert_eq!(why, "the measuring process of cgroup-fair gave no result (signal: 9 (SIGKILL))");
        // It writes a result and is killed before it ends: what it
        // wrote does not count.
        let done = serde_json::to_string(&RunResult { duration_s: 1.0, ..Default::default() }).unwrap();
        let why = run(sh(&format!("echo '{RESULT_PREFIX}{done}'; kill -9 $$")));
        assert_eq!(
            why,
            "the measuring process of cgroup-fair wrote a result and then did not end as it should (signal: 9 \
             (SIGKILL))"
        );
        let why = run(sh(&format!("echo '{RESULT_PREFIX}{done}'; exit 1")));
        assert!(why.ends_with("did not end as it should (exit status: 1)"), "{why}");
        // It cannot be started at all.
        let why = run(Hooks { launch: Some(vec!["/nonexistent/helicoprion".into()]), ..fast() });
        assert!(why.starts_with("starting the measuring process of cgroup-fair (/nonexistent/helicoprion): "), "{why}");
        // It writes something that is no result.
        let why = run(sh(&format!("echo '{RESULT_PREFIX}{{\"valid\": tru'")));
        assert!(why.starts_with("the result of the measuring process of cgroup-fair could not be read: "), "{why}");
        // It is a program that knows nothing of all this, and says so.
        let why = run(sh("echo usage: frobnicate FILE; exit 2"));
        assert_eq!(why, "the measuring process of cgroup-fair gave no result (exit status: 2)");

        // It is in a process group of its own, out of reach of an
        // interrupt from the terminal that is meant for the benchmark.
        let why = run(sh("set -- $(cat /proc/$$/stat); [ \"$5\" = \"$$\" ] && echo leads its own group >&2; exit 4"));
        assert_eq!(why, "the measuring process of cgroup-fair gave no result (exit status: 4): leads its own group");

        // A result among other output is taken, the last one if there
        // are several, and its own verdict stands.
        let result = |why: &str| {
            let r = RunResult { invalid_reason: Some(why.into()), ..Default::default() };
            format!("{RESULT_PREFIX}{}", serde_json::to_string(&r).unwrap())
        };
        let script = format!("echo running 1 test; echo '{}'; echo '{}'; echo ok", result("first"), result("second"));
        assert_eq!(run(sh(&script)), "second");
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
        // Nothing was created or started for it.
        assert_eq!(own(), before);

        // Sized for two CPUs under a quota, run on sixteen: four threads
        // with a CPU each. Both groups get half, as they would under a
        // scheduler that follows the weights, and it must not be scored.
        let w = CgroupFair { case: "weighted", weights: (100, 300), threads: (2, 2), cpus: 2, sched_diag: false };
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
        let _one = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        let before = own();
        let r = quick().run_with(Duration::from_millis(50), &Hooks { no_cpu_controller: false, ..fast() });
        if let Some(why) = r.invalid_reason {
            assert!(
                why.contains("cpu controller is not available") || no_groups(&why) || why.contains("writing"),
                "{why}"
            );
            assert!(r.metrics.is_empty());
        }
        assert_eq!(own(), before);
        assert!(run_group().is_none_or(|g| !g.exists()));
    }
}
