//! `matrix`: interleaved sessions across several schedulers, switched with
//! scxcli (scx.service).
//!
//! Each round visits every scheduler once, in alternating order (A B C,
//! C B A, A B C, …), and runs every workload during each visit, so slow
//! drift between sessions (thermal state, background activity) hits every
//! scheduler alike. Runs carry their `round`, so a report can pair
//! schedulers within a round.
//!
//! Safety rules:
//! * every scxcli/systemctl call runs in its own process group (a Ctrl-C
//!   reaches only helicoprion) and is killed after a timeout; a switch's
//!   deadline covers those calls too;
//! * a switch has settled only when scxcli reports the expected process
//!   (and options, when known) with no failure, the kernel shows a BPF
//!   scheduler whose ops name matches it, and that state, `enable_seq`
//!   included, holds for the settle period;
//! * the selection from before the session is restored, active *and*
//!   enabled state included, and verified; states that cannot be restored
//!   exactly (an `SCX_*_OVERRIDE`, a runtime-only enablement) are refused
//!   up front;
//! * results are written after every visit, atomically, into a fresh
//!   directory, with an index in each file name.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use helicoprion_core::{Design, ResultFile, SCHEMA_VERSION, Scheduler, SystemInfo, WorkloadResult};
use helicoprion_workloads::Workload;
use helicoprion_workloads::runner::{Event, RunConfig, run_workload};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Specs and resolved targets

/// One entry of the matrix, as given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// `default`: the kernel's own scheduler (`scxcli start default`).
    Default,
    /// `scx_lavd` or a catalog profile id (`scxcli start ID`).
    Start(String),
    /// `"scx_bpfland -s 5000"`: a scheduler with options (`scxcli run`).
    Run { sched: String, args: Vec<String> },
}

impl Spec {
    pub fn parse(s: &str) -> Result<Spec> {
        let mut words = s.split_whitespace();
        let first = words.next().ok_or_else(|| anyhow!("empty scheduler spec"))?;
        let rest: Vec<String> = words.map(str::to_string).collect();
        Ok(match (first, rest.is_empty()) {
            ("default", true) => Spec::Default,
            ("default", false) => bail!("`default` takes no options"),
            (id, true) => Spec::Start(id.to_string()),
            (sched, false) => Spec::Run { sched: sched.to_string(), args: rest },
        })
    }

    fn scxcli_args(&self) -> Vec<String> {
        match self {
            Spec::Default => vec!["start".into(), "default".into()],
            Spec::Start(id) => vec!["start".into(), id.clone()],
            Spec::Run { sched, args } => {
                let mut v = vec!["run".into(), sched.clone()];
                v.extend(args.iter().cloned());
                v
            }
        }
    }
}

/// A spec with what it must look like once running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub spec: Spec,
    /// Process name scxcli must report, or `default`.
    pub process: String,
    /// Options the process must run with, when known (profiles and `run`).
    pub args: Option<Vec<String>>,
}

fn basename(s: &str) -> String {
    Path::new(s).file_name().map_or_else(|| s.to_string(), |n| n.to_string_lossy().into_owned())
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Resolve specs; profile ids are looked up in `scxcli list --json`, so the
/// scheduler and options a profile stands for can be verified after the
/// switch.
pub fn resolve(labels: &[String], ctl: &dyn Control, timeout: Duration) -> Result<Vec<Target>> {
    let specs: Vec<Spec> = labels.iter().map(|s| Spec::parse(s)).collect::<Result<_>>()?;
    for (i, s) in specs.iter().enumerate() {
        if specs[..i].contains(s) {
            bail!("{:?} is listed twice", labels[i]);
        }
    }
    let needs_catalog = specs.iter().any(|s| matches!(s, Spec::Start(id) if !id.starts_with("scx_")));
    let listing = if needs_catalog { Some(ctl.list(timeout)?) } else { None };

    labels
        .iter()
        .zip(specs)
        .map(|(label, spec)| {
            let (process, args) = match &spec {
                Spec::Default => ("default".to_string(), None),
                Spec::Start(id) if id.starts_with("scx_") => (id.clone(), None),
                Spec::Start(id) => {
                    let entry = listing
                        .as_ref()
                        .and_then(|l| l.get("recommended"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.iter().find(|e| e.get("id").and_then(Value::as_str) == Some(id)))
                        .ok_or_else(|| anyhow!("{id:?} is not a profile in `scxcli list`"))?;
                    if entry.get("available").and_then(Value::as_bool) == Some(false) {
                        bail!("profile {id:?} is not available (its scheduler is not installed)");
                    }
                    let sched = entry
                        .get("sched")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("profile {id:?} names no scheduler"))?;
                    (basename(sched), Some(str_list(entry.get("args"))))
                }
                Spec::Run { sched, args } => (basename(sched), Some(args.clone())),
            };
            Ok(Target { label: label.clone(), spec, process, args })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Bounded external commands

/// Run a command in its own process group (so a terminal Ctrl-C does not
/// reach it), with one deadline for the whole call: the process exiting
/// *and* both output pipes reaching EOF. A command that exits but leaves a
/// child holding its stdout/stderr open would otherwise block the read
/// forever. On the deadline the process group is killed; a descendant that
/// left the group (setsid) cannot be killed that way, so its reader thread
/// is abandoned and ends whenever that process closes the pipe.
pub fn run_bounded(bin: &Path, args: &[String], timeout: Duration) -> Result<std::process::Output> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let pgid = libc::pid_t::try_from(child.id()).ok();
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<u8>)>();
    let pipes: [Box<dyn Read + Send>; 2] =
        [Box::new(child.stdout.take().expect("piped")), Box::new(child.stderr.take().expect("piped"))];
    for (i, mut pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        // Drain on threads so a chatty child cannot block on a full pipe;
        // each sends its whole buffer at EOF.
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = pipe.read_to_end(&mut b);
            let _ = tx.send((i, b));
        });
    }
    drop(tx);

    let mut out: [Option<Vec<u8>>; 2] = [None, None];
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let (Some(st), [Some(_), Some(_)]) = (status, &out) {
            let [o, e] = out;
            return Ok(std::process::Output {
                status: st,
                stdout: o.unwrap_or_default(),
                stderr: e.unwrap_or_default(),
            });
        }
        if Instant::now() >= deadline {
            if let Some(pid) = pgid {
                // SAFETY: kill(2) with a negative pid signals the process
                // group created for this child (process_group(0) above).
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            let cmd = format!("{} {}", bin.display(), args.join(" "));
            match status {
                Some(_) => bail!(
                    "{cmd} exited, but a process it started kept its output open past {timeout:?}; killed its process group"
                ),
                None => bail!("{cmd} did not finish within {timeout:?}; killed"),
            }
        }
        // Wake on EOF promptly, otherwise poll the process every 10 ms. Once
        // both readers are done the channel is disconnected and returns at
        // once, so sleep instead of spinning while the process still runs.
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok((i, b)) => out[i] = Some(b),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// The scxcli/systemctl calls `matrix` needs; a trait so tests can fake it.
pub trait Control {
    /// `scxcli get --json`
    fn get(&self, timeout: Duration) -> Result<Value>;
    /// `scxcli list --json`
    fn list(&self, timeout: Duration) -> Result<Value>;
    /// `scxcli ARGS…`
    fn scxcli(&self, args: &[String], timeout: Duration) -> Result<()>;
    /// `systemctl enable|disable scx.service` (enablement only; scxcli has
    /// no command that sets it without also changing the selection).
    fn systemctl(&self, verb: &str, timeout: Duration) -> Result<()>;
}

pub struct ScxCli {
    pub bin: PathBuf,
    pub systemctl: PathBuf,
}

impl ScxCli {
    fn json(&self, args: &[&str], timeout: Duration) -> Result<Value> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let out = run_bounded(&self.bin, &args, timeout)?;
        if !out.status.success() {
            bail!("scxcli {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        let v: Value =
            serde_json::from_slice(&out.stdout).with_context(|| format!("parsing scxcli {}", args.join(" ")))?;
        match v.get("version").and_then(Value::as_u64) {
            Some(2) => Ok(v),
            other => bail!("scxcli {} JSON version {other:?} is not supported (expected 2)", args.join(" ")),
        }
    }
}

impl Control for ScxCli {
    fn get(&self, timeout: Duration) -> Result<Value> {
        self.json(&["get", "--json"], timeout)
    }

    fn list(&self, timeout: Duration) -> Result<Value> {
        self.json(&["list", "--json"], timeout)
    }

    fn scxcli(&self, args: &[String], timeout: Duration) -> Result<()> {
        let out = run_bounded(&self.bin, args, timeout)?;
        if out.status.success() {
            return Ok(());
        }
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        match out.status.code() {
            Some(3) => bail!("scxcli {}: permission denied (run helicoprion as root): {msg}", args.join(" ")),
            _ => bail!("scxcli {} failed: {msg}", args.join(" ")),
        }
    }

    fn systemctl(&self, verb: &str, timeout: Duration) -> Result<()> {
        let args = vec![verb.to_string(), "scx.service".to_string()];
        let out = run_bounded(&self.systemctl, &args, timeout)?;
        if !out.status.success() {
            bail!("systemctl {verb} scx.service failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Restoring the previous state

/// What scx.service looked like before the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prior {
    pub active: bool,
    pub enabled: bool,
    /// Configured scheduler and options (`next`), if any.
    pub selection: Option<(String, Vec<String>)>,
}

impl Prior {
    fn from_state(state: &Value) -> Prior {
        let flag = |p: &str| state.pointer(p).and_then(Value::as_bool).unwrap_or(false);
        let selection = state
            .get("next")
            .filter(|n| !n.is_null())
            .and_then(|n| Some((n.get("sched")?.as_str()?.to_string(), str_list(n.get("args")))));
        Prior { active: flag("/unit/active"), enabled: flag("/unit/enabled"), selection }
    }

    /// Decide from the initial `get --json`, refusing what could not be put
    /// back exactly.
    pub fn plan(state: &Value) -> Result<Prior> {
        if state.pointer("/next/override").and_then(Value::as_bool) == Some(true) {
            bail!(
                "an SCX_SCHEDULER_OVERRIDE/SCX_FLAGS_OVERRIDE is in effect; switching with scxcli clears it and it \
could not be restored, so `matrix` refuses to run (remove the override, or use `run` per scheduler)"
            );
        }
        if state.pointer("/unit/enabled_runtime").and_then(Value::as_bool) == Some(true) {
            bail!(
                "scx.service is enabled with --runtime; that enablement could not be restored exactly, so `matrix` \
refuses to run (enable or disable it persistently first)"
            );
        }
        if let Some(e) = state.get("config_error").filter(|e| !e.is_null()) {
            bail!(
                "scxcli could not fully resolve scx.service's environment ({e}); `next` may be incomplete, so `matrix` refuses to run"
            );
        }
        if state.get("scheduler").is_none_or(Value::is_null) {
            bail!(
                "scx.service and the kernel disagree (scxcli reports `scheduler: null`): settle that first, \
there is no consistent state to restore"
            );
        }
        let prior = Prior::from_state(state);
        if prior.active && prior.selection.is_none() {
            bail!(
                "scx.service is active but scxcli reports no configured scheduler; refusing to guess what to restore"
            );
        }
        Ok(prior)
    }

    /// Equal up to how the scheduler is spelled: `scxcli run` stores the
    /// resolved path, so `scx_cosmos` comes back as `/usr/bin/scx_cosmos`.
    pub fn same_as(&self, other: &Prior) -> bool {
        let norm = |p: &Prior| p.selection.as_ref().map(|(s, a)| (basename(s), a.clone()));
        self.active == other.active && self.enabled == other.enabled && norm(self) == norm(other)
    }

    /// Commands that recreate this state: (`scxcli` args) or (`systemctl` verb).
    pub fn commands(&self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        match &self.selection {
            None => {
                // `start default` stops and disables the unit and clears the selection.
                cmds.push(Cmd::Scx(vec!["start".into(), "default".into()]));
                if self.enabled {
                    cmds.push(Cmd::Systemctl("enable"));
                }
            }
            Some((sched, args)) => {
                // `run` writes the selection, enables and (re)starts the unit.
                let mut run = vec!["run".into(), sched.clone()];
                run.extend(args.iter().cloned());
                cmds.push(Cmd::Scx(run));
                if !self.active {
                    cmds.push(Cmd::Scx(vec!["stop".into()]));
                }
                if !self.enabled {
                    cmds.push(Cmd::Systemctl("disable"));
                }
            }
        }
        cmds
    }

    pub fn describe(&self) -> String {
        let sel = match &self.selection {
            None => "no scheduler configured (kernel default)".to_string(),
            Some((s, a)) => {
                std::iter::once(s.as_str()).chain(a.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
            }
        };
        format!(
            "{sel}; scx.service {}, {}",
            if self.active { "active" } else { "inactive" },
            if self.enabled { "enabled" } else { "disabled" }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Scx(Vec<String>),
    Systemctl(&'static str),
}

impl Cmd {
    pub fn shell(&self) -> String {
        match self {
            Cmd::Scx(a) => format!("scxcli {}", a.join(" ")),
            Cmd::Systemctl(v) => format!("systemctl {v} scx.service"),
        }
    }
}

/// Puts the prior state back when dropped (normal end, error, panic) and
/// verifies it against a fresh `get --json`.
pub struct RestoreGuard<'a> {
    ctl: &'a dyn Control,
    detect: &'a dyn Fn() -> Scheduler,
    prior: Option<Prior>,
    cfg: SwitchCfg,
}

impl<'a> RestoreGuard<'a> {
    pub fn new(ctl: &'a dyn Control, detect: &'a dyn Fn() -> Scheduler, prior: Prior, cfg: SwitchCfg) -> Self {
        RestoreGuard { ctl, detect, prior: Some(prior), cfg }
    }

    /// The runtime state the prior configuration should produce: its
    /// scheduler running and attached, or nothing attached.
    fn runtime_target(prior: &Prior) -> Target {
        match (&prior.selection, prior.active) {
            (Some((sched, args)), true) => Target {
                label: "previous scheduler".into(),
                spec: Spec::Default, // unused: nothing is started from this target
                process: basename(sched),
                args: Some(args.clone()),
            },
            _ => Target { label: "previous state".into(), spec: Spec::Default, process: "default".into(), args: None },
        }
    }

    pub fn restore(&mut self) -> Result<()> {
        let Some(prior) = self.prior.take() else { return Ok(()) };
        eprintln!("restoring: {}", prior.describe());
        let t = self.cfg.cmd_timeout;
        for cmd in prior.commands() {
            match &cmd {
                Cmd::Scx(a) => self.ctl.scxcli(a, t)?,
                Cmd::Systemctl(v) => self.ctl.systemctl(v, t)?,
            }
        }
        // 1. Configuration: selection, active, enabled.
        let now = Prior::from_state(&self.ctl.get(t)?);
        if !now.same_as(&prior) {
            bail!("restore could not be verified: expected {}, scxcli reports {}", prior.describe(), now.describe());
        }
        // 2. Runtime: the kernel must actually run what the configuration
        //    says (scxcli reports `scheduler: null` while the unit and the
        //    kernel disagree). `enable_seq` is not compared with its value
        //    from before the session: restarting the scheduler moves it.
        let target = Self::runtime_target(&prior);
        let deadline = Instant::now() + self.cfg.timeout;
        loop {
            let state = self.ctl.get(t)?;
            let failure = state.get("failure").filter(|f| !f.is_null()).and_then(|f| f.get("message")).cloned();
            let why = match failure {
                Some(msg) => Some(format!("scx.service failed: {msg}")),
                None => mismatch(&target, &state, &(self.detect)(), self.cfg.allow_ops_mismatch),
            };
            match why {
                None => return Ok(()),
                Some(why) if Instant::now() >= deadline => {
                    bail!("configuration restored, but the running scheduler does not match it: {why}")
                }
                Some(_) => std::thread::sleep(self.cfg.poll),
            }
        }
    }
}

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!("error:     could not restore the previous scheduler: {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Switching

/// Kernel ops name that belongs to a process: `scx_lavd` -> `lavd`.
fn ops_for(process: &str) -> &str {
    process.strip_prefix("scx_").unwrap_or(process)
}

#[derive(Debug, Clone, Copy)]
pub struct SwitchCfg {
    pub timeout: Duration,
    pub settle: Duration,
    pub poll: Duration,
    /// Per-call limit for scxcli/systemctl, further capped by the switch deadline.
    pub cmd_timeout: Duration,
    /// Accept a kernel ops name that does not match the process name.
    pub allow_ops_mismatch: bool,
}

/// Why the current state is not yet the target, or `None` if it is.
fn mismatch(t: &Target, state: &Value, k: &Scheduler, allow_ops_mismatch: bool) -> Option<String> {
    let reported = state.get("scheduler").and_then(Value::as_str);
    if t.process == "default" {
        return match (reported, k.is_ext()) {
            (Some("default"), false) => None,
            _ => Some(format!("scxcli reports {reported:?}, kernel shows {k}")),
        };
    }
    if reported != Some(t.process.as_str()) {
        return Some(format!("scxcli reports {reported:?}, expected {:?}", t.process));
    }
    if let Some(want) = &t.args {
        let got = str_list(state.pointer("/running/args"));
        if &got != want {
            return Some(format!("running with {got:?}, expected {want:?}"));
        }
    }
    match k {
        Scheduler::Ext { name, .. } if allow_ops_mismatch || name == ops_for(&t.process) => None,
        Scheduler::Ext { name, .. } => Some(format!(
            "kernel ops {name:?} does not match {:?} (use --allow-ops-mismatch for a scheduler whose ops name differs)",
            t.process
        )),
        _ => Some(format!("kernel shows {k}")),
    }
}

/// Switch to `t` and wait until scxcli and the kernel both show it and it
/// holds for `settle`. Returns the scheduler the runs must stay on.
pub fn switch_and_settle(
    ctl: &dyn Control,
    detect: &dyn Fn() -> Scheduler,
    interrupted: &dyn Fn() -> bool,
    t: &Target,
    cfg: &SwitchCfg,
) -> Result<Scheduler> {
    let deadline = Instant::now() + cfg.timeout;
    let left = || deadline.saturating_duration_since(Instant::now()).min(cfg.cmd_timeout);
    ctl.scxcli(&t.spec.scxcli_args(), left())?;
    let mut stable_since: Option<(Instant, Scheduler)> = None;
    let mut why;
    loop {
        if interrupted() {
            bail!("interrupted while switching to {}", t.label);
        }
        let state = ctl.get(left())?;
        if let Some(f) = state.get("failure").filter(|f| !f.is_null()) {
            let msg = f.get("message").and_then(Value::as_str).unwrap_or("unknown failure");
            bail!("{} failed to start: {msg}", t.label);
        }
        let k = detect();
        why = mismatch(t, &state, &k, cfg.allow_ops_mismatch);
        let now = Instant::now();
        match (&stable_since, &why) {
            (Some((since, s)), None) if *s == k => {
                if now.duration_since(*since) >= cfg.settle {
                    return Ok(k);
                }
            }
            (_, None) => stable_since = Some((now, k)),
            (_, Some(_)) => stable_since = None,
        }
        if now >= deadline {
            bail!(
                "{} did not settle within {:?}: {}",
                t.label,
                cfg.timeout,
                why.unwrap_or_else(|| "still changing".into())
            );
        }
        std::thread::sleep(cfg.poll);
    }
}

/// Visit order of a round: forward on even rounds, reversed on odd ones.
pub fn round_order(n: usize, round: u32) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    if round % 2 == 1 {
        v.reverse();
    }
    v
}

// ---------------------------------------------------------------------------
// Output

/// `00-default.json`, `01-scx_lavd.json`: unique even when two specs
/// sanitize to the same text.
pub fn file_name(index: usize, label: &str) -> String {
    let clean: String =
        label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{index:02}-{clean}.json")
}

/// Refuse to mix a new session into an existing result set.
pub fn prepare_out_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        let mut entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
        if entries.next().is_some() {
            bail!(
                "{} already exists and is not empty; choose a new --output so earlier results are kept",
                dir.display()
            );
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn write_atomic(path: &Path, data: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

/// 128 random bits as hex, from getrandom(2); if that fails, time,
/// process id and a stack address, which is still unique in practice.
pub fn new_session_id() -> String {
    let mut buf = [0u8; 16];
    // SAFETY: getrandom writes at most buf.len() bytes into buf.
    let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    if n != buf.len() as isize {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let mix = t ^ (u128::from(std::process::id()) << 64) ^ (&buf as *const _ as usize as u128);
        buf = mix.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// The session

pub struct Session<'a> {
    pub targets: Vec<Target>,
    pub rounds: u32,
    pub runs_per_visit: usize,
    pub warmup: usize,
    pub duration: Duration,
    pub cooldown: Duration,
    pub switch: SwitchCfg,
    pub workloads: &'a [Box<dyn Workload>],
    pub out_dir: PathBuf,
    pub system: SystemInfo,
    pub started_at: u64,
    /// Shared by every file of this session (see `new_session_id`).
    pub session_id: String,
}

#[derive(Default)]
struct Accum {
    /// First scheduler reading of each visit.
    schedulers: Vec<Scheduler>,
    workloads: BTreeMap<usize, WorkloadResult>,
}

pub struct Outcome {
    /// One per target that completed at least one visit, in target order.
    pub files: Vec<ResultFile>,
    pub visits: u32,
    pub interrupted: bool,
    /// The error that ended the session early, if any.
    pub error: Option<anyhow::Error>,
    /// Restoring the prior state failed or could not be verified.
    pub restore_error: Option<anyhow::Error>,
}

impl Session<'_> {
    fn result_file(&self, i: usize, acc: &Accum) -> Option<ResultFile> {
        let first = acc.schedulers.first()?.clone();
        let labels: Vec<String> = self.targets.iter().map(|t| t.label.clone()).collect();
        Some(ResultFile {
            schema: SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").into(),
            label: self.targets[i].label.clone(),
            started_at: self.started_at,
            system: self.system.clone(),
            restart_detectable: acc.schedulers.iter().all(Scheduler::restart_detectable),
            scheduler: first,
            design: Some(Design {
                kind: "interleaved".into(),
                rounds: self.rounds,
                schedulers: labels,
                spec: self.targets[i].label.clone(),
                session_id: Some(self.session_id.clone()),
            }),
            workloads: acc.workloads.values().cloned().collect(),
        })
    }

    fn save(&self, i: usize, acc: &Accum) -> Result<()> {
        if let Some(f) = self.result_file(i, acc) {
            write_atomic(&self.out_dir.join(file_name(i, &self.targets[i].label)), &serde_json::to_string_pretty(&f)?)?;
        }
        Ok(())
    }
}

/// Run the whole session. `out_dir` must already be prepared. Results are
/// saved after every visit; the prior state is restored and verified at
/// the end, whatever happened.
pub fn run_matrix(
    s: &Session<'_>,
    ctl: &dyn Control,
    detect: &dyn Fn() -> Scheduler,
    interrupted: &dyn Fn() -> bool,
    prior: Prior,
    mut progress: impl FnMut(Event<'_>),
) -> Outcome {
    let mut guard = RestoreGuard::new(ctl, detect, prior, s.switch);
    let mut acc: Vec<Accum> = s.targets.iter().map(|_| Accum::default()).collect();
    let mut visits = 0u32;

    let error = (|| -> Result<()> {
        for round in 0..s.rounds {
            for ti in round_order(s.targets.len(), round) {
                if interrupted() {
                    return Ok(());
                }
                let t = &s.targets[ti];
                eprintln!("\nround {}/{} · {}", round + 1, s.rounds, t.label);
                let expected = switch_and_settle(ctl, detect, interrupted, t, &s.switch)?;
                eprintln!("  scheduler: {expected}");
                acc[ti].schedulers.push(expected.clone());
                let cfg = RunConfig {
                    expected,
                    detect,
                    cancel: interrupted,
                    runs: s.runs_per_visit,
                    warmup: s.warmup,
                    duration: s.duration,
                    cooldown: s.cooldown,
                };
                for (wi, w) in s.workloads.iter().enumerate() {
                    if interrupted() {
                        break;
                    }
                    let mut res = run_workload(w.as_ref(), &cfg, &mut progress);
                    for r in &mut res.runs {
                        r.visit = Some(visits);
                        r.round = Some(round);
                    }
                    match acc[ti].workloads.get_mut(&wi) {
                        Some(e) => e.runs.append(&mut res.runs),
                        None => {
                            acc[ti].workloads.insert(wi, res);
                        }
                    }
                }
                visits += 1;
                // Keep what this visit measured even if the machine dies next.
                s.save(ti, &acc[ti])?;
            }
        }
        Ok(())
    })()
    .err();

    let restore_error = guard.restore().err();
    let mut files = Vec::new();
    for (i, a) in acc.iter().enumerate() {
        if let Err(e) = s.save(i, a) {
            eprintln!("error:     {e:#}");
        }
        files.extend(s.result_file(i, a));
    }
    Outcome { files, visits, interrupted: interrupted(), error, restore_error }
}

// ---------------------------------------------------------------------------
// Ctrl-C

/// The first Ctrl-C asks the session to stop after the current run and
/// restore; the second terminates at once (without restoring).
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    // SAFETY: async-signal-safe; resets SIGINT to the default action.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

pub fn install_sigint() {
    let handler = on_sigint as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an atomic and resets the
    // disposition, both async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::{Metric, RunResult};
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    // -- A simulated scx.service + scxcli ---------------------------------

    #[derive(Clone, Debug)]
    struct Unit {
        active: bool,
        enabled: bool,
        next: Option<(String, Vec<String>)>,
        failure: Option<String>,
        enable_seq: u64,
        override_: bool,
        runtime: bool,
    }

    struct Sim {
        unit: RefCell<Unit>,
        log: RefCell<Vec<String>>,
        /// Schedulers that fail to start.
        broken: Vec<String>,
        /// Process name -> kernel ops name, when they differ.
        odd_ops: Vec<(String, String)>,
    }

    impl Sim {
        fn new(active: bool, enabled: bool, next: Option<(&str, &[&str])>) -> Sim {
            Sim {
                unit: RefCell::new(Unit {
                    active,
                    enabled,
                    next: next.map(|(s, a)| (s.to_string(), a.iter().map(|x| x.to_string()).collect())),
                    failure: None,
                    enable_seq: 1,
                    override_: false,
                    runtime: false,
                }),
                log: RefCell::default(),
                broken: vec![],
                odd_ops: vec![],
            }
        }

        fn process(&self) -> Option<String> {
            let u = self.unit.borrow();
            if u.active { u.next.as_ref().map(|(s, _)| basename(s)) } else { None }
        }

        fn detect(&self) -> Scheduler {
            match self.process() {
                Some(p) => {
                    let ops = self
                        .odd_ops
                        .iter()
                        .find(|(k, _)| *k == p)
                        .map_or_else(|| ops_for(&p).to_string(), |(_, v)| v.clone());
                    Scheduler::Ext { name: ops.clone(), ops, enable_seq: Some(self.unit.borrow().enable_seq) }
                }
                None => Scheduler::Fair { class: "EEVDF".into(), scx_capable: true, bore: None },
            }
        }

        fn launch(&self, sched: &str, args: Vec<String>) {
            let mut u = self.unit.borrow_mut();
            u.next = Some((sched.to_string(), args));
            u.enabled = true;
            u.override_ = false;
            if self.broken.contains(&basename(sched)) {
                u.active = false;
                u.failure = Some(format!("{sched} exited: BPF verifier said no"));
            } else {
                u.active = true;
                u.failure = None;
                u.enable_seq += 1;
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().iter().filter(|l| !l.starts_with("get") && !l.starts_with("list")).cloned().collect()
        }
    }

    impl Control for Sim {
        fn get(&self, _: Duration) -> Result<Value> {
            self.log.borrow_mut().push("get".into());
            let u = self.unit.borrow().clone();
            let process = self.process();
            Ok(json!({
                "version": 2,
                "scheduler": if u.active { process.clone() } else { Some("default".into()) },
                "unit": { "active": u.active, "enabled": u.enabled, "enabled_runtime": u.runtime },
                "running": process.as_ref().map(|p| json!({ "sched": p, "args": u.next.as_ref().unwrap().1 })),
                "next": u.next.as_ref().map(|(s, a)| json!({ "sched": s, "args": a, "override": u.override_ })),
                "kernel": null,
                "failure": u.failure.as_ref().map(|m| json!({ "message": m, "status": "exit-code/1" })),
            }))
        }
        fn list(&self, _: Duration) -> Result<Value> {
            self.log.borrow_mut().push("list".into());
            Ok(json!({ "version": 2, "recommended": [
                { "id": "default", "available": true },
                { "id": "latency", "sched": "scx_eevdf", "args": ["--slice-us", "1600"], "available": true },
                { "id": "missing", "sched": "scx_nope", "args": [], "available": false },
            ]}))
        }
        fn scxcli(&self, args: &[String], _: Duration) -> Result<()> {
            self.log.borrow_mut().push(format!("scxcli {}", args.join(" ")));
            match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                ["start", "default"] => {
                    let mut u = self.unit.borrow_mut();
                    *u = Unit { active: false, enabled: false, next: None, failure: None, ..u.clone() };
                }
                ["start", "latency"] => self.launch("scx_eevdf", vec!["--slice-us".into(), "1600".into()]),
                ["start", id] => self.launch(id, vec![]),
                ["run", sched, rest @ ..] => self.launch(sched, rest.iter().map(|s| s.to_string()).collect()),
                ["stop"] => self.unit.borrow_mut().active = false,
                other => bail!("sim: unsupported {other:?}"),
            }
            Ok(())
        }
        fn systemctl(&self, verb: &str, _: Duration) -> Result<()> {
            self.log.borrow_mut().push(format!("systemctl {verb}"));
            self.unit.borrow_mut().enabled = verb == "enable";
            Ok(())
        }
    }

    struct Quick;
    impl Workload for Quick {
        fn name(&self) -> String {
            "quick".into()
        }
        fn description(&self) -> String {
            String::new()
        }
        fn params(&self) -> Value {
            Value::Null
        }
        fn run(&self, _: Duration) -> RunResult {
            let mut r = RunResult::default();
            r.metrics.insert("x".into(), Metric::lower(1.0, "us"));
            r
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("helicoprion-matrix-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn session<'a>(sim: &Sim, labels: &[&str], rounds: u32, w: &'a [Box<dyn Workload>], dir: &Path) -> Session<'a> {
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        Session {
            targets: resolve(&labels, sim, Duration::from_secs(1)).unwrap(),
            rounds,
            runs_per_visit: 2,
            warmup: 0,
            duration: Duration::ZERO,
            cooldown: Duration::ZERO,
            switch: SwitchCfg {
                timeout: Duration::from_millis(300),
                settle: Duration::from_millis(5),
                poll: Duration::from_millis(1),
                cmd_timeout: Duration::from_secs(1),
                allow_ops_mismatch: false,
            },
            workloads: w,
            out_dir: dir.to_path_buf(),
            system: SystemInfo::collect(),
            started_at: 0,
            session_id: new_session_id(),
        }
    }

    /// Run a session against the simulator; with `stop_after_visits`, the
    /// interrupt flag goes up once that many visits' runs are done.
    fn go(sim: &Sim, s: &Session<'_>, stop_after_visits: Option<u32>) -> Outcome {
        prepare_out_dir(&s.out_dir).unwrap();
        let prior = Prior::plan(&sim.get(Duration::ZERO).unwrap()).unwrap();
        let done = Cell::new(0usize);
        let per_visit = s.runs_per_visit * s.workloads.len();
        let detect = || sim.detect();
        let interrupted = || stop_after_visits.is_some_and(|n| done.get() >= per_visit * n as usize);
        run_matrix(s, sim, &detect, &interrupted, prior, |ev| {
            if let Event::Done { warmup: false, .. } = ev {
                done.set(done.get() + 1);
            }
        })
    }

    fn prior_of(sim: &Sim) -> Prior {
        Prior::from_state(&sim.get(Duration::ZERO).unwrap())
    }

    // -- Tests ---------------------------------------------------------------

    #[test]
    fn spec_parsing() {
        assert_eq!(Spec::parse("default").unwrap(), Spec::Default);
        assert_eq!(Spec::parse("scx_lavd").unwrap(), Spec::Start("scx_lavd".into()));
        assert_eq!(
            Spec::parse("scx_bpfland -s 5000").unwrap(),
            Spec::Run { sched: "scx_bpfland".into(), args: vec!["-s".into(), "5000".into()] }
        );
        assert!(Spec::parse("default -x").is_err());
        assert!(Spec::parse("  ").is_err());
    }

    #[test]
    fn profiles_resolve_to_scheduler_and_options() {
        let sim = Sim::new(false, false, None);
        let labels = vec!["latency".to_string(), "/usr/bin/scx_cosmos -a".to_string()];
        let t = resolve(&labels, &sim, Duration::ZERO).unwrap();
        assert_eq!(
            (t[0].process.as_str(), t[0].args.clone()),
            ("scx_eevdf", Some(vec!["--slice-us".into(), "1600".into()]))
        );
        assert_eq!((t[1].process.as_str(), t[1].args.clone()), ("scx_cosmos", Some(vec!["-a".into()])));
        assert!(resolve(&["missing".to_string()], &sim, Duration::ZERO).is_err());
        assert!(resolve(&["nosuch".to_string()], &sim, Duration::ZERO).is_err());
        assert!(resolve(&["scx_a".to_string(), "scx_a".to_string()], &sim, Duration::ZERO).is_err());
    }

    #[test]
    fn alternating_rounds() {
        assert_eq!(round_order(3, 0), vec![0, 1, 2]);
        assert_eq!(round_order(3, 1), vec![2, 1, 0]);
    }

    #[test]
    fn refuses_unresolved_or_inconsistent_starting_states() {
        let sim = Sim::new(true, true, Some(("scx_lavd", &[])));
        let mut st = sim.get(Duration::ZERO).unwrap();
        st["config_error"] = json!("/etc/default/scx: permission denied");
        assert!(Prior::plan(&st).unwrap_err().to_string().contains("environment"));
        let mut st = sim.get(Duration::ZERO).unwrap();
        st["scheduler"] = Value::Null;
        assert!(Prior::plan(&st).unwrap_err().to_string().contains("disagree"));
    }

    /// The configuration comes back, but the kernel runs something else:
    /// the restore must fail loudly, not pass on configuration alone.
    #[test]
    fn restore_verifies_the_running_scheduler() {
        let mut sim = Sim::new(true, true, Some(("scx_cosmos", &[])));
        sim.odd_ops = vec![("scx_cosmos".into(), "something_else".into())];
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("rt");
        let s = session(&sim, &["default"], 1, &w, &dir);
        let out = go(&sim, &s, None);
        assert!(out.error.is_none(), "{:?}", out.error);
        let e = format!("{:#}", out.restore_error.expect("must not pass on configuration alone"));
        assert!(e.contains("running scheduler does not match"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_states_it_cannot_restore() {
        let sim = Sim::new(true, true, Some(("scx_lavd", &[])));
        sim.unit.borrow_mut().override_ = true;
        assert!(Prior::plan(&sim.get(Duration::ZERO).unwrap()).unwrap_err().to_string().contains("OVERRIDE"));
        let sim = Sim::new(true, true, Some(("scx_lavd", &[])));
        sim.unit.borrow_mut().runtime = true;
        assert!(Prior::plan(&sim.get(Duration::ZERO).unwrap()).unwrap_err().to_string().contains("--runtime"));
    }

    /// The four active/enabled combinations, with and without a selection
    /// where that is a valid state, end exactly where they started.
    #[test]
    fn full_session_restores_every_active_enabled_combination() {
        type Case<'a> = (bool, bool, Option<(&'a str, &'a [&'a str])>);
        let cases: &[Case] = &[
            (false, false, None),
            (false, true, None),
            (true, true, Some(("/usr/bin/scx_cosmos", &["-a"]))),
            (true, false, Some(("/usr/bin/scx_cosmos", &["-a"]))),
            (false, true, Some(("/usr/bin/scx_cosmos", &[]))),
            (false, false, Some(("/usr/bin/scx_cosmos", &[]))),
        ];
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        for (i, (active, enabled, next)) in cases.iter().enumerate() {
            let sim = Sim::new(*active, *enabled, *next);
            let before = prior_of(&sim);
            let dir = tmp_dir(&format!("combo{i}"));
            let s = session(&sim, &["default", "scx_lavd", "latency"], 2, &w, &dir);
            let out = go(&sim, &s, None);
            assert!(out.error.is_none(), "case {i}: {:?}", out.error);
            assert!(out.restore_error.is_none(), "case {i}: {:?}", out.restore_error);
            assert!(prior_of(&sim).same_as(&before), "case {i}: state not restored");
            assert_eq!(out.visits, 6);
            // Two runs per visit, two rounds: four runs per scheduler, rounds 0 and 1.
            for f in &out.files {
                let runs = &f.workloads[0].runs;
                assert_eq!(runs.len(), 4, "case {i}");
                assert!(runs.iter().all(|r| r.valid), "case {i}: {:?}", runs.iter().find(|r| !r.valid));
                assert_eq!(runs.iter().filter(|r| r.round == Some(1)).count(), 2);
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn switch_failure_restores_and_keeps_partial_results() {
        let mut sim = Sim::new(true, false, Some(("scx_cosmos", &[])));
        sim.broken = vec!["scx_lavd".into()];
        let before = prior_of(&sim);
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("broken");
        let s = session(&sim, &["default", "scx_lavd"], 3, &w, &dir);
        let out = go(&sim, &s, None);
        assert!(format!("{:#}", out.error.unwrap()).contains("verifier"));
        assert!(out.restore_error.is_none(), "{:?}", out.restore_error);
        assert!(prior_of(&sim).same_as(&before));
        // `default` was measured before the failure and is on disk.
        assert!(dir.join("00-default.json").exists());
        assert!(!dir.join("01-scx_lavd.json").exists());
        assert_eq!(out.files.len(), 1);
        // The unit was disabled before: restore had to disable it again.
        assert_eq!(sim.log().last().unwrap(), "systemctl disable");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rerun_into_the_same_directory_is_refused() {
        let sim = Sim::new(false, false, None);
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("rerun");
        let s = session(&sim, &["default"], 1, &w, &dir);
        let out = go(&sim, &s, None);
        assert!(out.error.is_none());
        assert!(prepare_out_dir(&dir).unwrap_err().to_string().contains("not empty"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupt_stops_restores_and_saves() {
        let sim = Sim::new(true, true, Some(("scx_cosmos", &[])));
        let before = prior_of(&sim);
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("intr");
        let s = session(&sim, &["default", "scx_lavd"], 5, &w, &dir);
        let out = go(&sim, &s, Some(1));
        assert!(out.interrupted);
        assert!(out.visits < 10);
        assert!(out.restore_error.is_none());
        assert!(prior_of(&sim).same_as(&before));
        assert!(dir.join("00-default.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ops_mismatch_does_not_settle_unless_allowed() {
        let mut sim = Sim::new(false, false, None);
        sim.odd_ops = vec![("scx_lavd".into(), "something_else".into())];
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("ops");
        let s = session(&sim, &["scx_lavd"], 1, &w, &dir);
        let out = go(&sim, &s, None);
        assert!(format!("{:#}", out.error.unwrap()).contains("does not match"));
        let _ = fs::remove_dir_all(&dir);

        let dir = tmp_dir("ops2");
        let mut s = session(&sim, &["scx_lavd"], 1, &w, &dir);
        s.switch.allow_ops_mismatch = true;
        assert!(go(&sim, &s, None).error.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_with_wrong_options_does_not_settle() {
        let t = Target {
            label: "latency".into(),
            spec: Spec::Start("latency".into()),
            process: "scx_eevdf".into(),
            args: Some(vec!["--slice-us".into(), "1600".into()]),
        };
        let state = json!({ "scheduler": "scx_eevdf", "running": { "sched": "scx_eevdf", "args": [] } });
        let k = Scheduler::Ext { name: "eevdf".into(), ops: "eevdf".into(), enable_seq: Some(1) };
        assert!(mismatch(&t, &state, &k, false).unwrap().contains("running with"));
    }

    #[test]
    fn bounded_commands_are_killed_on_timeout() {
        let t0 = Instant::now();
        let e = run_bounded(Path::new("sleep"), &["5".to_string()], Duration::from_millis(100)).unwrap_err();
        assert!(e.to_string().contains("killed"));
        assert!(t0.elapsed() < Duration::from_secs(2));
        let out = run_bounded(Path::new("sh"), &["-c".into(), "echo hi; echo err >&2".into()], Duration::from_secs(5))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
    }

    #[test]
    fn timeout_kills_grandchildren_too() {
        let marker = std::env::temp_dir().join(format!("helicoprion-gc-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        // The grandchild writes its pid, then would sleep far beyond the timeout.
        let script = format!("sh -c 'echo $$ > {m}; exec sleep 30' & wait", m = marker.display());
        assert!(run_bounded(Path::new("sh"), &["-c".into(), script], Duration::from_millis(300)).is_err());
        std::thread::sleep(Duration::from_millis(200));
        let pid = fs::read_to_string(&marker).unwrap().trim().to_string();
        let alive = Path::new(&format!("/proc/{pid}")).exists()
            && !fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z");
        let _ = fs::remove_file(&marker);
        assert!(!alive, "grandchild {pid} survived the timeout");
    }

    /// The command exits at once, but leaves a child that keeps its stdout
    /// open: waiting for EOF must be bounded too.
    #[test]
    fn exited_command_with_child_holding_output_is_bounded() {
        let t0 = Instant::now();
        let e = run_bounded(Path::new("sh"), &["-c".into(), "sleep 30 & exit 0".into()], Duration::from_millis(300))
            .unwrap_err();
        assert!(t0.elapsed() < Duration::from_secs(3), "took {:?}", t0.elapsed());
        assert!(e.to_string().contains("kept its output open"), "{e}");
    }

    /// Same, with the child escaping the process group (setsid): it cannot
    /// be killed with the group, but the call must still return in time.
    #[test]
    fn escaped_child_holding_output_is_still_bounded() {
        let t0 = Instant::now();
        let r =
            run_bounded(Path::new("sh"), &["-c".into(), "setsid sleep 3 & exit 0".into()], Duration::from_millis(300));
        assert!(r.is_err());
        assert!(t0.elapsed() < Duration::from_secs(2), "took {:?}", t0.elapsed());
    }

    /// CPU time consumed by the calling thread so far.
    fn thread_cpu() -> Duration {
        // SAFETY: getrusage fills the struct we pass; RUSAGE_THREAD is Linux.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
        let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
        Duration::from_micros(us(ru.ru_utime) + us(ru.ru_stime))
    }

    /// Both pipes close early (EOF, readers done, channel disconnected) while
    /// the process keeps running: waiting must still sleep between polls.
    #[test]
    fn closed_pipes_with_running_process_do_not_spin() {
        let cpu0 = thread_cpu();
        let t0 = Instant::now();
        let e =
            run_bounded(Path::new("sh"), &["-c".into(), "exec 1>&- 2>&-; sleep 5".into()], Duration::from_millis(500))
                .unwrap_err();
        let (wall, cpu) = (t0.elapsed(), thread_cpu() - cpu0);
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(wall < Duration::from_secs(2), "took {wall:?}");
        // A busy loop would burn close to the full 500 ms.
        assert!(cpu < Duration::from_millis(150), "spent {cpu:?} of CPU waiting");
    }

    #[test]
    fn bounded_commands_run_in_their_own_process_group() {
        let out =
            run_bounded(Path::new("sh"), &["-c".into(), "ps -o pgid= -p $$".into()], Duration::from_secs(5)).unwrap();
        let child_pgid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        // SAFETY: getpgrp has no preconditions.
        assert_ne!(child_pgid, unsafe { libc::getpgrp() });
    }

    #[test]
    fn restore_verification_ignores_path_spelling() {
        let a = Prior { active: true, enabled: true, selection: Some(("scx_cosmos".into(), vec![])) };
        let b = Prior { active: true, enabled: true, selection: Some(("/usr/bin/scx_cosmos".into(), vec![])) };
        assert!(a.same_as(&b));
        let c = Prior { enabled: false, ..b.clone() };
        assert!(!a.same_as(&c));
    }

    #[test]
    fn session_ids_are_random_hex_and_shared_by_a_session() {
        let (a, b) = (new_session_id(), new_session_id());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);

        let sim = Sim::new(false, false, None);
        let w: Vec<Box<dyn Workload>> = vec![Box::new(Quick)];
        let dir = tmp_dir("sid");
        let s = session(&sim, &["default", "scx_lavd"], 1, &w, &dir);
        let out = go(&sim, &s, None);
        let ids: Vec<_> = out.files.iter().map(|f| f.design.as_ref().unwrap().session_id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|i| *i == Some(s.session_id.clone())));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_stay_unique() {
        assert_eq!(file_name(0, "default"), "00-default.json");
        assert_ne!(file_name(1, "scx_a -x"), file_name(2, "scx_a_-x"));
    }
}
