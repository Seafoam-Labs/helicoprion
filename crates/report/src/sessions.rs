//! Several `matrix` sessions of the same experiment, side by side.
//!
//! One session answers "how did these schedulers differ this time". Its
//! intervals come from resampling its own rounds, and cannot see what
//! changes from one session to the next: the state the machine booted
//! into, what else was running that evening. Whether a result holds is a
//! question about sessions, and the session is the unit here: one session,
//! one observation, however many runs it took.
//!
//! That only means something when the sessions repeat one experiment.
//! This module first decides that (`check`), strictly: same machine, same
//! kernel, same schedulers with the same options, same workloads, same
//! protocol. A difference stops the analysis. What a file does not record
//! is neither equal nor different, and is reported as unverified.

use helicoprion_core::{Better, ResultFile, Scheduler, SystemInfo};

use crate::compare::{Comparison, Delta, Options, SessionKey, UnclearReason, Verdict, session_key};
use crate::conditions::utc;

/// The files of one `matrix` session, in the order of its scheduler list.
#[derive(Debug, Clone)]
pub struct Session {
    pub key: SessionKey,
    /// Where the files came from, for messages (a directory, usually).
    pub source: String,
    pub files: Vec<ResultFile>,
}

impl Session {
    /// The scheduler list of the session, baseline first.
    pub fn schedulers(&self) -> &[String] {
        self.files[0].design.as_ref().map_or(&[], |d| d.schedulers.as_slice())
    }

    fn spec(f: &ResultFile) -> &str {
        f.design.as_ref().map_or(f.label.as_str(), |d| d.spec.as_str())
    }

    /// The file measured for scheduler `spec`.
    pub fn file(&self, spec: &str) -> Option<&ResultFile> {
        self.files.iter().find(|f| Session::spec(f) == spec)
    }

    /// `2026-11-16 19:40 UTC (dir)`.
    pub fn name(&self) -> String {
        format!("{} ({})", utc(self.files[0].started_at as f64), self.source)
    }
}

/// Sort files into sessions, oldest session first. `files` are (where the
/// file came from, the file).
///
/// A session is only accepted whole: exactly one file for every scheduler
/// of its list, and no other. With the baseline's file missing, the first
/// file there is would be read as the baseline and every result under
/// the wrong name; with another one missing, the session answers a
/// smaller question than its list says. Also refused: files that are not
/// from `matrix`, and files of one session that disagree about its list.
pub fn group(files: Vec<(String, ResultFile)>) -> Result<Vec<Session>, String> {
    let mut sessions: Vec<Session> = Vec::new();
    for (source, f) in files {
        let Some(key) = session_key(&f) else {
            return Err(format!(
                "{source}: {:?} was not taken by `matrix`; only whole matrix sessions can be compared with each \
                 other, since a plain run has no rounds and no verified scheduler options",
                f.label
            ));
        };
        match sessions.iter_mut().find(|s| s.key == key) {
            Some(s) => {
                if s.file(Session::spec(&f)).is_some() {
                    return Err(format!("{source}: {:?} is in this session twice", Session::spec(&f)));
                }
                s.files.push(f);
            }
            None => sessions.push(Session { key, source, files: vec![f] }),
        }
    }
    for s in &mut sessions {
        let order: Vec<String> = s.schedulers().to_vec();
        if order.is_empty() {
            return Err(format!("{}: the session records no scheduler list", s.source));
        }
        for f in &s.files {
            let listed = f.design.as_ref().map_or(&[][..], |d| d.schedulers.as_slice());
            if listed != order.as_slice() {
                return Err(format!(
                    "{}: its files disagree about the schedulers of the session ({:?} lists {}, {:?} lists {})",
                    s.source,
                    Session::spec(&s.files[0]),
                    order.join(", "),
                    Session::spec(f),
                    listed.join(", ")
                ));
            }
            if !order.iter().any(|x| x == Session::spec(f)) {
                return Err(format!(
                    "{}: {:?} is not among the schedulers of its session ({})",
                    s.source,
                    Session::spec(f),
                    order.join(", ")
                ));
            }
        }
        let missing: Vec<&str> = order.iter().filter(|spec| s.file(spec).is_none()).map(String::as_str).collect();
        if !missing.is_empty() {
            return Err(format!(
                "{}: the session is incomplete, without results for {} (its schedulers: {}); give every file of the \
                 session, or leave out a session that was cut short before each scheduler was measured",
                s.source,
                missing.join(", "),
                order.join(", ")
            ));
        }
        let position = |f: &ResultFile| order.iter().position(|x| x == Session::spec(f)).unwrap_or(usize::MAX);
        s.files.sort_by_key(position);
    }
    sessions.sort_by_key(|s| s.files[0].started_at);
    Ok(sessions)
}

/// How one session compares with the reference session as a repetition
/// of the same experiment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Check {
    /// Known on both sides and not the same: not a repetition.
    pub differs: Vec<String>,
    /// Not recorded on at least one side: could not be compared. Never
    /// taken for equal, and never for "none" or a default.
    pub unverified: Vec<String>,
}

/// The scheduler without what changes at every attach.
fn identity(s: &Scheduler) -> String {
    match s {
        Scheduler::Fair { class, bore, scx_capable } => {
            format!(
                "{class}{}{}",
                if *bore == Some(true) { "+BORE" } else { "" },
                if *scx_capable { "" } else { ", no sched_ext" }
            )
        }
        Scheduler::Ext { ops, .. } => ops.clone(),
    }
}

/// Files written since the machine is described in full carry the kernel
/// build, which every Linux offers. Without it the newer fields are
/// missing because they were not recorded, not because the machine lacks
/// them.
fn described(s: &SystemInfo) -> bool {
    s.kernel_build.is_some()
}

/// Compare `other` with `reference`. Both come from `group`, so each is
/// complete: a file for every scheduler of its list.
///
/// Values are compared as they are and only put into words for the
/// message: two option lists are equal when they are the same arguments,
/// not when they read the same once joined (`["--config", "a b"]` is not
/// `["--config", "a", "b"]`).
pub fn check(reference: &Session, other: &Session) -> Check {
    let mut c = Check::default();
    let show = |o: &Option<String>| o.clone().unwrap_or_else(|| "none".into());
    let (a, b) = (&reference.files[0], &other.files[0]);

    // The experiment: which schedulers, in which order, and that every
    // one of them was measured.
    if reference.schedulers() != other.schedulers() {
        c.differs.push(format!(
            "schedulers: {} here, {} in the reference",
            other.schedulers().join(", "),
            reference.schedulers().join(", ")
        ));
        return c;
    }

    let mut differ = |what: &str, x: String, y: String| {
        if x != y {
            c.differs.push(format!("{what}: {y} here, {x} in the reference"));
        }
    };
    // For lists: equal as lists, shown with their boundaries.
    let list = |v: &[String]| format!("{v:?}");
    differ("tool version", a.tool_version.clone(), b.tool_version.clone());

    // The machine. These have been recorded since the first format.
    let (sa, sb) = (&a.system, &b.system);
    differ("kernel", sa.kernel.clone(), sb.kernel.clone());
    differ("CPU model", show(&sa.cpu_model), show(&sb.cpu_model));
    differ("usable CPUs", sa.usable_cpus.to_string(), sb.usable_cpus.to_string());
    // The CPUs the kernel schedules on, whatever this process is bound
    // to: a scheduler on 16 online CPUs decides differently from one on
    // 8, also for a benchmark that runs on the same 8 in both.
    match (&sa.online_cpus, &sb.online_cpus) {
        (Some(x), Some(y)) => differ("online CPUs", x.clone(), y.clone()),
        _ => c.unverified.push("online CPUs".into()),
    }
    differ("SMT", format!("{:?}", sa.smt_active), format!("{:?}", sb.smt_active));
    differ("governor", show(&sa.governor), show(&sb.governor));
    differ("EPP", show(&sa.epp), show(&sb.epp));
    differ("platform profile", show(&sa.platform_profile), show(&sb.platform_profile));
    differ("THP", show(&sa.thp), show(&sb.thp));
    // And these only by newer versions.
    if described(sa) && described(sb) {
        let switch = |o: Option<bool>, on: &str, off: &str| o.map_or("none", |v| if v { on } else { off }).to_string();
        differ("kernel build", show(&sa.kernel_build), show(&sb.kernel_build));
        differ("preemption model", show(&sa.preempt), show(&sb.preempt));
        differ("kernel parameters", list(&sa.cmdline), list(&sb.cmdline));
        differ("CPU topology", format!("{:?}", sa.topology), format!("{:?}", sb.topology));
        differ("CPUs the benchmark may use", show(&sa.allowed_cpus), show(&sb.allowed_cpus));
        differ("isolated CPUs", show(&sa.isolated_cpus), show(&sb.isolated_cpus));
        differ("cpufreq driver", show(&sa.cpufreq_driver), show(&sb.cpufreq_driver));
        differ("highest frequency", show(&sa.max_freq_khz), show(&sb.max_freq_khz));
        differ("frequency boost", switch(sa.boost, "on", "off"), switch(sb.boost, "on", "off"));
        differ("power source", switch(sa.on_ac, "mains", "battery"), switch(sb.on_ac, "mains", "battery"));
    } else {
        c.unverified.push(
            "kernel build, preemption model, kernel parameters, CPU topology and affinity, cpufreq driver, boost and \
             power source"
                .into(),
        );
    }

    // The protocol.
    match (&a.protocol, &b.protocol) {
        (Some(x), Some(y)) => {
            let taken = |p: &helicoprion_core::Protocol| {
                format!(
                    "suite {}, {} runs of {} s per visit, {} warm-up, {} s pause",
                    p.suite, p.runs, p.duration_s, p.warmup, p.cooldown_s
                )
            };
            differ("how the runs were taken", taken(x), taken(y));
        }
        _ => c.unverified.push("how the runs were taken (runs per visit, warm-up, run length, pause)".into()),
    }
    let (da, db) = (a.design.as_ref(), b.design.as_ref());
    // The rounds the session set out to run, not the ones it completed:
    // runs can fail, but three rounds and ten are two protocols, with
    // other intervals and other chances of a clear verdict.
    differ("rounds", da.map_or(0, |d| d.rounds).to_string(), db.map_or(0, |d| d.rounds).to_string());
    match (da.and_then(|d| d.settle_s), db.and_then(|d| d.settle_s)) {
        (Some(x), Some(y)) => differ("settle time", format!("{x} s"), format!("{y} s")),
        _ => c.unverified.push("settle time after a switch".into()),
    }
    match (da.and_then(|d| d.order.clone()), db.and_then(|d| d.order.clone())) {
        (Some(x), Some(y)) => differ("order of visits", x, y),
        _ => c.unverified.push("order of visits".into()),
    }

    // Every scheduler: the same build, the same options, the same workloads.
    for spec in reference.schedulers() {
        let (Some(x), Some(y)) = (reference.file(spec), other.file(spec)) else { continue };
        differ(&format!("{spec}: scheduler"), identity(&x.scheduler), identity(&y.scheduler));
        if x.scheduler.is_ext() || y.scheduler.is_ext() {
            let options = |f: &ResultFile| f.design.as_ref().and_then(|d| d.options.clone());
            match (options(x), options(y)) {
                (Some(ox), Some(oy)) => differ(&format!("{spec}: options"), list(&ox), list(&oy)),
                // Not recorded is not "no options".
                _ => c.unverified.push(format!("{spec}: scheduler options")),
            }
        }
        let names = |f: &ResultFile| f.workloads.iter().map(|w| w.name.clone()).collect::<Vec<_>>();
        differ(&format!("{spec}: workloads"), list(&names(x)), list(&names(y)));
        for wx in &x.workloads {
            if let Some(wy) = y.workloads.iter().find(|w| w.name == wx.name) {
                differ(&format!("{spec}: parameters of {}", wx.name), wx.params.to_string(), wy.params.to_string());
            }
        }
    }
    c
}

/// With fewer sessions than this, the report says that they are too few
/// for even a first idea of how often a result would repeat.
///
/// A practical floor, not a statistical one: reaching it guarantees
/// nothing. One could reckon that a change scored the same way in five
/// of five sessions would do so by chance once in sixteen times if its
/// direction were a coin toss, but that holds for one metric chosen
/// beforehand and independent sessions. A report lists dozens of
/// metrics that move together, and nobody picks one in advance.
pub const SESSIONS_TO_GENERALISE: usize = 5;

/// One session's own estimate of a change, with its own scoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome {
    pub delta: Delta,
    /// Why this session does not score the change; `None` when it does.
    pub unclear: Option<UnclearReason>,
}

impl Outcome {
    /// Scored, and better than the baseline (`Some(true)`) or worse.
    pub fn clear(&self) -> Option<bool> {
        self.unclear.is_none().then_some(self.delta.median > 0.0)
    }
}

/// How the sessions' scorings of one change agree. A count of sessions,
/// not a test: it says what happened in these sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agreement {
    /// Scored in every session that measured it, always in this direction
    /// (`true`: better than the baseline).
    Every { better: bool },
    /// Scored in `clear` sessions, always in this direction; not scored
    /// in the others.
    Some { better: bool, clear: usize },
    /// Scored as better in some sessions and as worse in others.
    Opposite { better: usize, worse: usize },
    /// Scored in no session.
    Never,
}

/// A scored metric of one scheduler against the baseline, session by
/// session.
#[derive(Debug, Clone)]
pub struct Effect {
    /// Index into the scheduler list; never 0, the baseline.
    pub scheduler: usize,
    pub workload: String,
    pub metric: String,
    /// One per session; `None` where the session has no estimate (a side
    /// lacks the metric there).
    pub sessions: Vec<Option<Outcome>>,
}

impl Effect {
    /// Sessions with an estimate.
    pub fn measured(&self) -> usize {
        self.sessions.iter().flatten().count()
    }

    pub fn agreement(&self) -> Agreement {
        let clear: Vec<bool> = self.sessions.iter().flatten().filter_map(Outcome::clear).collect();
        let better = clear.iter().filter(|b| **b).count();
        let worse = clear.len() - better;
        match (better, worse) {
            (0, 0) => Agreement::Never,
            (b, w) if b > 0 && w > 0 => Agreement::Opposite { better: b, worse: w },
            (b, _) if clear.len() == self.measured() => Agreement::Every { better: b > 0 },
            (b, _) => Agreement::Some { better: b > 0, clear: clear.len() },
        }
    }

    /// The smallest and the largest of the sessions' estimates, as
    /// changes in percent (positive is better). `None` with fewer than two.
    pub fn range(&self) -> Option<(f64, f64)> {
        let mut changes = self.sessions.iter().flatten().map(|o| o.delta.median);
        let first = changes.next()?;
        let (lo, hi, n) = changes.fold((first, first, 1), |(lo, hi, n), c| (lo.min(c), hi.max(c), n + 1));
        (n >= 2).then_some((lo, hi))
    }
}

/// The measured value of one metric under one scheduler, session by
/// session: the stability of the result itself, apart from any
/// comparison. Every metric has one, the baseline's and unscored ones too.
#[derive(Debug, Clone)]
pub struct Level {
    /// Index into the scheduler list; 0 is the baseline.
    pub scheduler: usize,
    pub workload: String,
    pub metric: String,
    pub unit: String,
    /// The session's median over its valid runs.
    pub sessions: Vec<Option<f64>>,
}

impl Level {
    /// The largest session median over the smallest: 1.0 when all agree.
    /// `None` with fewer than two values, or when one is not positive.
    pub fn factor(&self) -> Option<f64> {
        let v: Vec<f64> = self.sessions.iter().flatten().copied().collect();
        let (lo, hi) = (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(0.0, f64::max));
        (v.len() >= 2 && lo > 0.0).then(|| hi / lo)
    }
}

/// One scheduler on one workload: the verdict each session gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdicts {
    pub scheduler: usize,
    pub workload: String,
    pub sessions: Vec<Option<Verdict>>,
}

/// What the sessions' verdicts on one scheduler and workload add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consensus {
    /// Every session has a verdict, and it is the same one.
    Same(Verdict),
    /// Sessions with a verdict disagree.
    Differs,
    /// Some session has no verdict (no comparable measurement there).
    /// Whatever the others say, it does not hold for every session;
    /// `agreed` is their common verdict, if they have one.
    Incomplete { agreed: Option<Verdict>, measured: usize },
}

impl Verdicts {
    /// Sessions with a verdict.
    pub fn measured(&self) -> usize {
        self.sessions.iter().flatten().count()
    }

    pub fn consensus(&self) -> Consensus {
        let mut given = self.sessions.iter().flatten();
        let agreed = given.next().copied().filter(|first| given.all(|x| x == first));
        match (self.measured() == self.sessions.len(), agreed) {
            (true, Some(v)) => Consensus::Same(v),
            (true, None) => Consensus::Differs,
            (false, agreed) => Consensus::Incomplete { agreed, measured: self.measured() },
        }
    }

    /// Every session has a verdict and all are the same. A session
    /// without one is not a session that agrees.
    pub fn same(&self) -> bool {
        matches!(self.consensus(), Consensus::Same(_))
    }
}

/// Sessions of one experiment, each analysed on its own and laid side by
/// side.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Scheduler specs, baseline first.
    pub schedulers: Vec<String>,
    /// Oldest first; the first is the reference of the check.
    pub sessions: Vec<Session>,
    /// Each session's own comparison, series in scheduler order.
    pub comparisons: Vec<Comparison>,
    /// What could not be compared with the reference, per session (empty
    /// for the reference itself).
    pub unverified: Vec<Vec<String>>,
    pub verdicts: Vec<Verdicts>,
    pub effects: Vec<Effect>,
    pub levels: Vec<Level>,
}

impl Analysis {
    /// Too few sessions for even a first idea of how often a result would
    /// repeat (`SESSIONS_TO_GENERALISE`).
    pub fn few(&self) -> bool {
        self.sessions.len() < SESSIONS_TO_GENERALISE
    }
}

/// Analyse `files` as sessions of one experiment. `Err` lists why they
/// are not: fewer than two sessions, or what differs from the oldest
/// session, session by session. Nothing is computed then.
pub fn analyse(files: Vec<(String, ResultFile)>, opts: Options) -> Result<Analysis, Vec<String>> {
    let sessions = group(files).map_err(|e| vec![e])?;
    if sessions.len() < 2 {
        return Err(vec![format!(
            "{} matrix session given; comparing sessions takes at least two (`compare` reports a single one)",
            sessions.len()
        )]);
    }
    let checks: Vec<Check> = sessions.iter().map(|s| check(&sessions[0], s)).collect();
    let differs: Vec<String> = sessions
        .iter()
        .zip(&checks)
        .flat_map(|(s, c)| c.differs.iter().map(move |d| format!("{}: {d}", s.name())))
        .collect();
    if !differs.is_empty() {
        let mut why = vec![format!(
            "these sessions do not repeat one experiment; compared with the oldest, {}:",
            sessions[0].name()
        )];
        why.extend(differs);
        return Err(why);
    }

    let schedulers: Vec<String> = sessions[0].schedulers().to_vec();
    let comparisons: Vec<Comparison> =
        sessions.iter().map(|s| Comparison::build_with(s.files.clone(), 0, opts)).collect();

    // Rows in the order they first appear, session by session.
    let mut keys: Vec<(String, String)> = Vec::new();
    for c in &comparisons {
        for r in &c.rows {
            let key = (r.workload.clone(), r.metric.clone());
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    let row = |c: &'_ Comparison, key: &(String, String)| -> Option<usize> {
        c.rows.iter().position(|r| r.workload == key.0 && r.metric == key.1)
    };

    let mut effects = Vec::new();
    let mut levels = Vec::new();
    for key in &keys {
        for scheduler in 0..schedulers.len() {
            let first = comparisons.iter().find_map(|c| row(c, key).map(|i| &c.rows[i]));
            let Some(first) = first else { continue };
            levels.push(Level {
                scheduler,
                workload: key.0.clone(),
                metric: key.1.clone(),
                unit: first.unit.clone(),
                sessions: comparisons
                    .iter()
                    .map(|c| {
                        let cell = c.rows[row(c, key)?].cells.get(scheduler)?.as_ref()?;
                        (!cell.values.is_empty()).then_some(cell.spread.median)
                    })
                    .collect(),
            });
            // Scored metrics only: an extreme or an informational metric
            // has no outcome to agree on.
            if scheduler == 0 || first.extreme || first.better == Better::Neither {
                continue;
            }
            let outcomes: Vec<Option<Outcome>> = comparisons
                .iter()
                .map(|c| {
                    let r = &c.rows[row(c, key)?];
                    let delta = r.improvement(0, scheduler)?;
                    Some(Outcome { delta, unclear: c.unclear_reason(r, scheduler, &delta) })
                })
                .collect();
            if outcomes.iter().any(Option::is_some) {
                effects.push(Effect { scheduler, workload: key.0.clone(), metric: key.1.clone(), sessions: outcomes });
            }
        }
    }

    let mut verdicts: Vec<Verdicts> = Vec::new();
    for (si, c) in comparisons.iter().enumerate() {
        for s in c.summary() {
            let at = match verdicts.iter().position(|v| v.scheduler == s.series && v.workload == s.workload) {
                Some(at) => at,
                None => {
                    verdicts.push(Verdicts {
                        scheduler: s.series,
                        workload: s.workload.clone(),
                        sessions: vec![None; comparisons.len()],
                    });
                    verdicts.len() - 1
                }
            };
            verdicts[at].sessions[si] = Some(s.verdict());
        }
    }
    verdicts.sort_by_key(|v| v.scheduler);

    let unverified = checks.into_iter().map(|c| c.unverified).collect();
    Ok(Analysis { schedulers, sessions, comparisons, unverified, verdicts, effects, levels })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use helicoprion_core::{Design, Metric, Protocol, RunResult, Topology, WorkloadResult};

    /// The files of a session as `matrix` writes them today, fully
    /// described: schedulers `scheds` (baseline first), `rounds` rounds of
    /// two runs, a latency `p99` (µs, lower is better) of `value(spec,
    /// round)` and a rate `ops` of a million over it.
    pub(crate) fn session(
        id: &str,
        started_at: u64,
        scheds: &[&str],
        rounds: u32,
        value: impl Fn(&str, u32) -> f64,
    ) -> Vec<(String, ResultFile)> {
        scheds
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                let mut runs = Vec::new();
                for round in 0..rounds {
                    for jitter in [0.995, 1.005] {
                        let v = value(spec, round) * jitter;
                        let mut r =
                            RunResult { valid: true, round: Some(round), duration_s: 5.0, ..Default::default() };
                        r.metrics.insert("p99".into(), Metric::lower(v, "us"));
                        r.metrics.insert("ops".into(), Metric::higher(1e6 / v, "1/s"));
                        runs.push(r);
                    }
                }
                let ext = spec.starts_with("scx_");
                let name = spec.trim_start_matches("scx_").to_string();
                let file = ResultFile {
                    schema: helicoprion_core::SCHEMA_VERSION,
                    tool_version: "0.5.0".into(),
                    label: (*spec).into(),
                    started_at,
                    system: SystemInfo {
                        kernel: "7.3.4-1".into(),
                        cpu_model: Some("AMD Ryzen 7 8845HS".into()),
                        usable_cpus: 16,
                        online_cpus: Some("0-15".into()),
                        allowed_cpus: Some("0-15".into()),
                        smt_active: Some(true),
                        governor: Some("performance".into()),
                        epp: Some("performance".into()),
                        kernel_build: Some("#1 SMP PREEMPT_DYNAMIC Mon, 16 Nov 2026".into()),
                        preempt: Some("full".into()),
                        cmdline: vec!["mitigations=auto".into()],
                        topology: Some(Topology { packages: 1, cores: 8, l3_domains: Some(1), numa_nodes: Some(1) }),
                        cpufreq_driver: Some("amd-pstate-epp".into()),
                        boost: Some(true),
                        on_ac: Some(true),
                        ..Default::default()
                    },
                    scheduler: if ext {
                        // `enable_seq` differs at every attach, in every session.
                        Scheduler::Ext {
                            ops: format!("{name}_1.1.4_x86_64"),
                            name,
                            enable_seq: Some(started_at + i as u64),
                        }
                    } else {
                        Scheduler::Fair { class: "EEVDF".into(), scx_capable: true, bore: None }
                    },
                    restart_detectable: true,
                    design: Some(Design {
                        kind: "interleaved".into(),
                        rounds,
                        schedulers: scheds.iter().map(|s| s.to_string()).collect(),
                        spec: (*spec).into(),
                        session_id: Some(id.into()),
                        settle_s: Some(3.0),
                        order: Some("alternating".into()),
                        options: ext.then(Vec::new),
                    }),
                    protocol: Some(Protocol {
                        suite: "quick".into(),
                        runs: 2,
                        warmup: 1,
                        duration_s: 5.0,
                        cooldown_s: 1.0,
                    }),
                    workloads: vec![WorkloadResult {
                        name: "w".into(),
                        description: String::new(),
                        params: serde_json::json!({ "pairs": 8 }),
                        runs,
                    }],
                };
                (format!("{id}/{i:02}-{spec}.json"), file)
            })
            .collect()
    }

    const SCHEDS: [&str; 3] = ["default", "scx_lavd", "scx_cosmos"];

    fn flat(spec: &str, _round: u32) -> f64 {
        if spec == "default" { 100.0 } else { 150.0 }
    }

    fn two(change: impl Fn(&mut Vec<(String, ResultFile)>)) -> Check {
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        let mut other = session("tue", 2000, &SCHEDS, 3, flat);
        change(&mut other);
        files.extend(other);
        let sessions = group(files).unwrap();
        check(&sessions[0], &sessions[1])
    }

    #[test]
    fn files_are_sorted_into_sessions() {
        // Given in any order: the later session first, its files shuffled.
        let mut files = session("tue", 2000, &SCHEDS, 3, flat);
        files.reverse();
        files.extend(session("mon", 1000, &SCHEDS, 3, flat));
        let sessions = group(files).unwrap();
        assert_eq!(sessions.len(), 2);
        // Oldest first, and within a session the order of its scheduler list.
        assert_eq!(sessions[0].files[0].started_at, 1000);
        for s in &sessions {
            assert_eq!(s.files.iter().map(|f| f.label.as_str()).collect::<Vec<_>>(), SCHEDS);
            assert_eq!(s.schedulers(), SCHEDS);
        }
        // Named by its start and by where its first file came from.
        assert_eq!(sessions[1].name(), "1970-01-01 00:33 UTC (tue/02-scx_cosmos.json)");

        // A plain run is not a session; a file twice is a mistake.
        let mut plain = session("mon", 1000, &SCHEDS, 1, flat);
        plain[1].1.design = None;
        let e = group(plain).unwrap_err();
        assert!(e.contains("mon/01-scx_lavd.json") && e.contains("was not taken by `matrix`"), "{e}");
        let mut twice = session("mon", 1000, &SCHEDS, 1, flat);
        twice.push(twice[2].clone());
        assert!(group(twice).unwrap_err().contains("\"scx_cosmos\" is in this session twice"));
    }

    #[test]
    fn a_repetition_passes_the_check() {
        // Another day, other enable_seq values, other measurements.
        assert_eq!(two(|_| {}), Check::default());
        assert_eq!(
            two(|other| {
                for (_, f) in other.iter_mut() {
                    for r in &mut f.workloads[0].runs {
                        r.metrics.get_mut("p99").unwrap().value *= 3.0;
                    }
                }
            }),
            Check::default()
        );
    }

    #[test]
    fn what_differs_stops_and_is_named() {
        type Change = fn(&mut Vec<(String, ResultFile)>);
        let cases: [(Change, &str); 16] = [
            (
                |o| o.iter_mut().for_each(|(_, f)| f.system.kernel = "7.3.5-1".into()),
                "kernel: 7.3.5-1 here, 7.3.4-1 in",
            ),
            (
                |o| o.iter_mut().for_each(|(_, f)| f.system.kernel_build = Some("#2 SMP".into())),
                "kernel build: #2 SMP here",
            ),
            (
                |o| o.iter_mut().for_each(|(_, f)| f.system.governor = Some("powersave".into())),
                "governor: powersave here",
            ),
            // Eight CPUs less, and the same number on other CPUs.
            (|o| o.iter_mut().for_each(|(_, f)| f.system.usable_cpus = 8), "usable CPUs: 8 here, 16 in"),
            (
                |o| o.iter_mut().for_each(|(_, f)| f.system.allowed_cpus = Some("8-15,0-7".into())),
                "CPUs the benchmark may use: 8-15,0-7 here, 0-15 in",
            ),
            // The benchmark on the same CPUs, the machine with half of them online.
            (
                |o| o.iter_mut().for_each(|(_, f)| f.system.online_cpus = Some("0-7".into())),
                "online CPUs: 0-7 here, 0-15 in",
            ),
            (|o| o.iter_mut().for_each(|(_, f)| f.system.on_ac = Some(false)), "power source: battery here, mains in"),
            (|o| o.iter_mut().for_each(|(_, f)| f.system.preempt = Some("lazy".into())), "preemption model: lazy here"),
            (|o| o.iter_mut().for_each(|(_, f)| f.tool_version = "0.6.0".into()), "tool version: 0.6.0 here, 0.5.0 in"),
            (
                |o| o.iter_mut().for_each(|(_, f)| f.protocol.as_mut().unwrap().cooldown_s = 0.0),
                "how the runs were taken: suite quick, 2 runs of 5 s per visit, 1 warm-up, 0 s pause here",
            ),
            (
                |o| o.iter_mut().for_each(|(_, f)| f.design.as_mut().unwrap().settle_s = Some(1.0)),
                "settle time: 1 s here, 3 s in",
            ),
            // One scheduler: another build, other options, other workload parameters.
            (
                |o| {
                    if let Scheduler::Ext { ops, .. } = &mut o[1].1.scheduler {
                        *ops = "lavd_1.1.5_x86_64".into();
                    }
                },
                "scx_lavd: scheduler: lavd_1.1.5_x86_64 here, lavd_1.1.4_x86_64 in",
            ),
            (
                |o| o[2].1.design.as_mut().unwrap().options = Some(vec!["-s".into(), "5000".into()]),
                "scx_cosmos: options: [\"-s\", \"5000\"] here, [] in",
            ),
            (
                |o| o[1].1.workloads[0].params = serde_json::json!({ "pairs": 4 }),
                "scx_lavd: parameters of w: {\"pairs\":4} here, {\"pairs\":8} in",
            ),
            (|o| o[0].1.workloads[0].name = "other".into(), "default: workloads: [\"other\"] here, [\"w\"] in"),
            // Three rounds planned in one session, ten in the other.
            (|o| o.iter_mut().for_each(|(_, f)| f.design.as_mut().unwrap().rounds = 10), "rounds: 10 here, 3 in"),
        ];
        for (change, expected) in cases {
            let c = two(change);
            assert!(c.differs.iter().any(|d| d.contains(expected)), "{expected}\n{:?}", c.differs);
            assert_eq!(c.differs.len(), 1, "only that: {:?}", c.differs);
            assert!(c.unverified.is_empty(), "{:?}", c.unverified);
        }

        // Another experiment altogether: other schedulers, or one missing.
        let c = two(|o| {
            *o = session("tue", 2000, &["default", "scx_lavd", "scx_cake"], 3, flat);
        });
        assert_eq!(c.differs.len(), 1);
        assert!(
            c.differs[0].starts_with("schedulers: default, scx_lavd, scx_cake here, default, scx_lavd, scx_cosmos in")
        );
        // The same schedulers in another order visit them in another order.
        let c = two(|o| *o = session("tue", 2000, &["default", "scx_cosmos", "scx_lavd"], 3, flat));
        assert!(c.differs[0].starts_with("schedulers:"), "{:?}", c.differs);

        // The same words, other arguments: one value with a space is not
        // two values. Equal only as lists.
        let argv = |args: &[&str]| Some(args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let mut reference = session("mon", 1000, &SCHEDS, 3, flat);
        reference[1].1.design.as_mut().unwrap().options = argv(&["--config", "foo bar"]);
        for (other, same) in [(argv(&["--config", "foo bar"]), true), (argv(&["--config", "foo", "bar"]), false)] {
            let mut files = reference.clone();
            let mut tue = session("tue", 2000, &SCHEDS, 3, flat);
            tue[1].1.design.as_mut().unwrap().options = other;
            files.extend(tue);
            let s = group(files).unwrap();
            let c = check(&s[0], &s[1]);
            if same {
                assert_eq!(c, Check::default());
            } else {
                assert_eq!(
                    c.differs,
                    [
                        "scx_lavd: options: [\"--config\", \"foo\", \"bar\"] here, [\"--config\", \"foo bar\"] in the reference"
                    ]
                );
            }
        }
        // And kernel parameters likewise.
        let c =
            two(|o| o.iter_mut().for_each(|(_, f)| f.system.cmdline = vec!["mitigations=auto".into(), "nosmt".into()]));
        assert!(
            c.differs[0].starts_with("kernel parameters: [\"mitigations=auto\", \"nosmt\"] here"),
            "{:?}",
            c.differs
        );

        // Runs that failed do not make another protocol: a session whose
        // last round lost a run still planned the same rounds.
        let c = two(|o| {
            o[2].1.workloads[0].runs.pop();
        });
        assert_eq!(c, Check::default());
    }

    /// A session is taken whole or not at all. With the baseline's file
    /// missing in every session, the sessions would agree with each
    /// other, and the first file left would be read as the baseline.
    #[test]
    fn incomplete_sessions_are_refused() {
        let without = |drop: usize, in_both: bool| {
            let mut files = session("mon", 1000, &SCHEDS, 3, flat);
            let mut tue = session("tue", 2000, &SCHEDS, 3, flat);
            tue.remove(drop);
            if in_both {
                files.remove(drop);
            }
            files.extend(tue);
            group(files).unwrap_err()
        };
        // The baseline, in one session and in both.
        for in_both in [false, true] {
            let e = without(0, in_both);
            assert!(e.contains("the session is incomplete, without results for default"), "{e}");
            assert!(e.contains("its schedulers: default, scx_lavd, scx_cosmos"), "{e}");
            assert!(e.starts_with(if in_both { "mon/" } else { "tue/" }), "the first one found: {e}");
        }
        // Any other scheduler, e.g. a session interrupted before the last.
        assert!(without(2, false).contains("without results for scx_cosmos"));
        assert!(without(1, true).contains("without results for scx_lavd"));
        // A file that does not belong: a spec outside the list, or files
        // of one session that list different schedulers.
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        files[2].1.design.as_mut().unwrap().spec = "scx_cake".into();
        let e = group(files).unwrap_err();
        assert!(e.contains("\"scx_cake\" is not among the schedulers of its session"), "{e}");
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        files[1].1.design.as_mut().unwrap().schedulers.pop();
        let e = group(files).unwrap_err();
        assert!(e.contains("its files disagree about the schedulers of the session"), "{e}");
    }

    /// What a file does not record was not compared. It is not equal, it
    /// is not different, and missing options are not "no options".
    #[test]
    fn what_is_not_recorded_is_unverified() {
        // Options: not recorded in this session, recorded as none in the
        // reference; and the other way round; and in neither.
        let unset = |o: &mut Vec<(String, ResultFile)>| o[1].1.design.as_mut().unwrap().options = None;
        let c = two(unset);
        assert_eq!((c.differs.len(), c.unverified.clone()), (0, vec!["scx_lavd: scheduler options".to_string()]));
        let mut reference = session("mon", 1000, &SCHEDS, 3, flat);
        unset(&mut reference);
        for other_has_them in [true, false] {
            let mut other = session("tue", 2000, &SCHEDS, 3, flat);
            if !other_has_them {
                unset(&mut other);
            }
            let mut files = reference.clone();
            files.extend(other);
            let s = group(files).unwrap();
            let c = check(&s[0], &s[1]);
            assert_eq!((c.differs.len(), c.unverified), (0, vec!["scx_lavd: scheduler options".to_string()]));
        }
        // The online CPUs, where a file could not read them.
        let c = two(|o| o.iter_mut().for_each(|(_, f)| f.system.online_cpus = None));
        assert_eq!((c.differs.len(), c.unverified), (0, vec!["online CPUs".to_string()]));

        // The kernel's scheduler has no options to record.
        assert!(two(|o| o[0].1.design.as_mut().unwrap().options = None).unverified.is_empty());

        // Sessions written before the protocol and the fuller description
        // of the machine: everything newer is unverified, nothing differs,
        // and what older files do record is still compared.
        let old = |o: &mut Vec<(String, ResultFile)>| {
            for (_, f) in o.iter_mut() {
                f.protocol = None;
                let d = f.design.as_mut().unwrap();
                (d.settle_s, d.order, d.options) = (None, None, None);
                let s = &mut f.system;
                (s.kernel_build, s.preempt, s.topology, s.allowed_cpus) = (None, None, None, None);
                (s.cpufreq_driver, s.boost, s.on_ac) = (None, None, None);
                s.cmdline.clear();
            }
        };
        let c = two(old);
        assert!(c.differs.is_empty(), "{:?}", c.differs);
        assert_eq!(c.unverified.len(), 6, "{:?}", c.unverified);
        assert!(c.unverified[0].starts_with("kernel build, preemption model"));
        assert!(
            c.unverified.contains(&"how the runs were taken (runs per visit, warm-up, run length, pause)".to_string())
        );
        assert!(c.unverified.contains(&"scx_cosmos: scheduler options".to_string()));
        let c = two(|o| {
            old(o);
            o.iter_mut().for_each(|(_, f)| f.system.kernel = "7.3.5-1".into());
        });
        assert_eq!(c.differs.len(), 1);

        // A described machine that has no battery or no debugfs access has
        // "none" on both sides: equal, not unverified.
        let bare = |o: &mut Vec<(String, ResultFile)>| {
            o.iter_mut().for_each(|(_, f)| (f.system.on_ac, f.system.preempt) = (None, None))
        };
        let mut reference = session("mon", 1000, &SCHEDS, 3, flat);
        bare(&mut reference);
        let mut other = session("tue", 2000, &SCHEDS, 3, flat);
        bare(&mut other);
        reference.extend(other);
        let s = group(reference).unwrap();
        assert_eq!(check(&s[0], &s[1]), Check::default());
        // But against a machine that does say: a difference.
        assert!(two(bare).differs.iter().any(|d| d.contains("power source: none here, mains in")));
    }

    /// Three sessions of default, scx_lavd and scx_cosmos on one workload.
    /// lavd is 1.5× slower every time. cosmos is 20% slower than a
    /// baseline of 100 µs; the baseline is 100, 100 and 150 µs, so in the
    /// third session cosmos (120 µs) comes out ahead.
    fn drifting_baseline() -> Vec<(String, ResultFile)> {
        let mut files = Vec::new();
        for (k, base) in [100.0, 100.0, 150.0].into_iter().enumerate() {
            let value = move |spec: &str, round: u32| {
                // Rounds drift together; the effect within a round does not.
                let drift = 1.0 + 0.02 * f64::from(round);
                drift
                    * match spec {
                        "default" => base,
                        "scx_lavd" => base * 1.5,
                        _ => 120.0,
                    }
            };
            files.extend(session(&format!("s{k}"), 1000 * (k as u64 + 1), &SCHEDS, 4, value));
        }
        files
    }

    #[test]
    fn sessions_are_analysed_on_their_own_and_laid_side_by_side() {
        let a = analyse(drifting_baseline(), Options::default()).unwrap();
        assert_eq!(a.schedulers, SCHEDS);
        assert_eq!((a.sessions.len(), a.comparisons.len()), (3, 3));
        assert!(a.few() && a.unverified.iter().all(Vec::is_empty));
        let effect =
            |sched: usize, metric: &str| a.effects.iter().find(|e| e.scheduler == sched && e.metric == metric).unwrap();

        // lavd: scored as worse in every session, by the same factor.
        let lavd = effect(1, "p99");
        assert_eq!((lavd.measured(), lavd.agreement()), (3, Agreement::Every { better: false }));
        let (lo, hi) = lavd.range().unwrap();
        assert!((lo + 50.0).abs() < 1.0 && (hi + 50.0).abs() < 1.0, "{lo} {hi}");
        // Each session's estimate is its own paired one.
        assert!(lavd.sessions.iter().flatten().all(|o| o.delta.paired_rounds == Some(4) && o.unclear.is_none()));

        // cosmos: worse twice, better once. Every session is sure of
        // itself; together they contradict each other.
        let cosmos = effect(2, "p99");
        assert_eq!(cosmos.agreement(), Agreement::Opposite { better: 1, worse: 2 });
        let (lo, hi) = cosmos.range().unwrap();
        assert!((lo + 20.0).abs() < 1.0 && (hi - 20.0).abs() < 1.0, "{lo} {hi}");
        assert_eq!(
            cosmos.sessions.iter().flatten().map(|o| o.clear()).collect::<Vec<_>>(),
            [Some(false), Some(false), Some(true)]
        );
        // The rate moves with the latency: the same picture.
        assert_eq!(effect(2, "ops").agreement(), Agreement::Opposite { better: 1, worse: 2 });

        // The verdicts, session by session.
        let verdict = |sched: usize| a.verdicts.iter().find(|v| v.scheduler == sched).unwrap();
        assert_eq!(verdict(1).sessions, [Some(Verdict::Worse); 3]);
        assert!(verdict(1).same());
        assert_eq!(verdict(2).sessions, [Some(Verdict::Worse), Some(Verdict::Worse), Some(Verdict::Better)]);
        assert!(!verdict(2).same());

        assert_eq!(verdict(1).consensus(), Consensus::Same(Verdict::Worse));
        assert_eq!(verdict(2).consensus(), Consensus::Differs);

        // The levels show where the contradiction comes from: cosmos
        // measured the same every time, the baseline did not.
        let level = |sched: usize| a.levels.iter().find(|l| l.scheduler == sched && l.metric == "p99").unwrap();
        assert!((level(2).factor().unwrap() - 1.0).abs() < 0.01, "{:?}", level(2));
        assert!((level(0).factor().unwrap() - 1.5).abs() < 0.01, "{:?}", level(0));
        assert_eq!(level(0).unit, "us");
        assert_eq!(a.levels.len(), 6, "every scheduler, both metrics");
        assert_eq!(a.effects.len(), 4, "the two candidates, both metrics");
    }

    /// A session without a verdict is not a session that agrees: "better,
    /// better, nothing measured" is not "better in every session".
    #[test]
    fn a_missing_verdict_is_not_agreement() {
        let verdicts = |sessions: Vec<Option<Verdict>>| Verdicts { scheduler: 1, workload: "w".into(), sessions };
        let (b, w) = (Some(Verdict::Better), Some(Verdict::Worse));
        let v = verdicts(vec![b, b, None]);
        assert!(!v.same());
        assert_eq!(v.consensus(), Consensus::Incomplete { agreed: Some(Verdict::Better), measured: 2 });
        assert_eq!(verdicts(vec![b, None, w]).consensus(), Consensus::Incomplete { agreed: None, measured: 2 });
        assert_eq!(verdicts(vec![None, None]).consensus(), Consensus::Incomplete { agreed: None, measured: 0 });
        assert!(verdicts(vec![b, b, b]).same());
        assert!(!verdicts(vec![b, w, b]).same());

        // Through the analysis: in the third session every run of lavd is
        // invalid, so that session has nothing to say about it.
        let mut files = Vec::new();
        for k in 0..3u64 {
            let mut s =
                session(
                    &format!("s{k}"),
                    1000 * (k + 1),
                    &SCHEDS,
                    4,
                    |spec, _| {
                        if spec == "scx_lavd" { 50.0 } else { 100.0 }
                    },
                );
            if k == 2 {
                s[1].1.workloads[0].runs.iter_mut().for_each(|r| r.valid = false);
            }
            files.extend(s);
        }
        let a = analyse(files, Options::default()).unwrap();
        let lavd = a.verdicts.iter().find(|v| v.scheduler == 1).unwrap();
        assert_eq!(lavd.sessions, [Some(Verdict::Better), Some(Verdict::Better), None]);
        assert_eq!(lavd.consensus(), Consensus::Incomplete { agreed: Some(Verdict::Better), measured: 2 });
        // Its effects were measured in two sessions of three.
        let e = a.effects.iter().find(|e| e.scheduler == 1 && e.metric == "p99").unwrap();
        assert_eq!((e.measured(), e.agreement()), (2, Agreement::Every { better: true }));
        // The scheduler that was measured throughout is unaffected.
        let cosmos = a.verdicts.iter().find(|v| v.scheduler == 2).unwrap();
        assert_eq!(cosmos.consensus(), Consensus::Same(Verdict::NoClearDifference));
    }

    #[test]
    fn agreement_counts_sessions() {
        let outcome = |change: f64, unclear: Option<UnclearReason>| {
            Some(Outcome {
                delta: Delta {
                    median: change,
                    ci: Some((change - 1.0, change + 1.0)),
                    ratio: 1.0 + change / 100.0,
                    paired_rounds: Some(5),
                    unmatched_rounds: false,
                    paired_spread: None,
                    undefined_rounds: 0,
                    better: Better::Higher,
                },
                unclear,
            })
        };
        let of = |sessions: Vec<Option<Outcome>>| {
            let e = Effect { scheduler: 1, workload: "w".into(), metric: "m".into(), sessions };
            (e.measured(), e.agreement())
        };
        let (better, worse) = (outcome(20.0, None), outcome(-20.0, None));
        let noise = outcome(1.0, Some(UnclearReason::BelowThreshold));
        // A change that is not scored has a sign too, and does not count.
        let hint = outcome(-30.0, Some(UnclearReason::IntervalIncludesNoChange));
        assert_eq!(of(vec![better, better, better]), (3, Agreement::Every { better: true }));
        assert_eq!(of(vec![worse, worse]), (2, Agreement::Every { better: false }));
        assert_eq!(of(vec![worse, noise, worse]), (3, Agreement::Some { better: false, clear: 2 }));
        assert_eq!(of(vec![better, hint, hint]), (3, Agreement::Some { better: true, clear: 1 }));
        assert_eq!(of(vec![better, worse, noise]), (3, Agreement::Opposite { better: 1, worse: 1 }));
        assert_eq!(of(vec![noise, hint]), (2, Agreement::Never));
        // A session without the metric is not a session that disagrees.
        assert_eq!(of(vec![worse, None, worse]), (2, Agreement::Every { better: false }));
        assert_eq!(of(vec![None, None]), (0, Agreement::Never));

        let e = Effect { scheduler: 1, workload: "w".into(), metric: "m".into(), sessions: vec![better, None, worse] };
        assert_eq!(e.range(), Some((-20.0, 20.0)));
        let e = Effect { sessions: vec![better, None], ..e };
        assert_eq!(e.range(), None, "one estimate has no range");

        let level = |sessions: Vec<Option<f64>>| {
            Level { scheduler: 0, workload: "w".into(), metric: "m".into(), unit: "us".into(), sessions }.factor()
        };
        assert_eq!(level(vec![Some(33.5), Some(124.0), Some(553.0)]).map(|f| (f * 10.0).round()), Some(165.0));
        assert_eq!(level(vec![Some(5.0), None, Some(5.0)]), Some(1.0));
        assert_eq!(level(vec![Some(5.0), None]), None);
        assert_eq!(level(vec![Some(0.0), Some(5.0)]), None, "no factor from zero");
    }

    #[test]
    fn nothing_is_computed_for_sessions_that_do_not_match() {
        // One session is not a comparison of sessions.
        let e = analyse(session("mon", 1000, &SCHEDS, 3, flat), Options::default()).unwrap_err();
        assert!(e[0].starts_with("1 matrix session given"), "{e:?}");

        // A third session on another kernel, with one scheduler updated:
        // every difference is listed, under the session it belongs to.
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        files.extend(session("tue", 2000, &SCHEDS, 3, flat));
        let mut wed = session("wed", 3000, &SCHEDS, 3, flat);
        for (_, f) in wed.iter_mut() {
            f.system.kernel = "7.3.5-1".into();
        }
        if let Scheduler::Ext { ops, .. } = &mut wed[1].1.scheduler {
            *ops = "lavd_1.1.5_x86_64".into();
        }
        files.extend(wed);
        let e = analyse(files, Options::default()).unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e[0].starts_with(
            "these sessions do not repeat one experiment; compared with the oldest, 1970-01-01 00:16 UTC"
        ));
        assert!(
            e[1].starts_with("1970-01-01 00:50 UTC (wed/00-default.json): kernel: 7.3.5-1 here, 7.3.4-1 in"),
            "{e:?}"
        );
        assert!(e[2].contains("scx_lavd: scheduler: lavd_1.1.5_x86_64 here"), "{e:?}");

        // An incomplete session, even when every session lacks the same
        // file: nothing is computed, rather than a scheduler renamed.
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        files.remove(0);
        let mut tue = session("tue", 2000, &SCHEDS, 3, flat);
        tue.remove(0);
        files.extend(tue);
        let e = analyse(files, Options::default()).unwrap_err();
        assert!(e.len() == 1 && e[0].contains("incomplete, without results for default"), "{e:?}");

        // What is unverified does not stop the analysis, and is kept.
        let mut files = session("mon", 1000, &SCHEDS, 3, flat);
        let mut tue = session("tue", 2000, &SCHEDS, 3, flat);
        tue[1].1.design.as_mut().unwrap().options = None;
        files.extend(tue);
        let a = analyse(files, Options::default()).unwrap();
        assert_eq!(a.unverified, [vec![], vec!["scx_lavd: scheduler options".to_string()]]);
    }
}
