mod matrix;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use helicoprion_core::{MIN_READABLE_SCHEMA, ResultFile, SCHEMA_VERSION, Scheduler, SystemInfo};
use helicoprion_report::{Comparison, Options, html, svg};
use helicoprion_workloads::runner::{Event, RunConfig, run_workload};
use helicoprion_workloads::{SUITES, SuiteOpts, suite};

#[derive(Parser)]
#[command(name = "helicoprion", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show kernel, CPU and the scheduler currently in charge.
    Info {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a suite and write a result file.
    Run {
        /// Workload suite.
        #[arg(long, default_value = "quick", value_parser = clap::builder::PossibleValuesParser::new(SUITES))]
        suite: String,
        /// Measured runs per workload.
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Unrecorded warm-up runs per workload.
        #[arg(long, default_value_t = 1)]
        warmup: usize,
        /// Seconds per run.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Seconds of idle between runs.
        #[arg(long, default_value_t = 1.0)]
        cooldown: f64,
        /// Label for this session (default: scheduler name, e.g. scx_lavd).
        #[arg(long)]
        label: Option<String>,
        /// Output file (default: helicoprion-<label>-<unix time>.json).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Skip the same-/cross-CPU diagnostics (one sched_getcpu per round
        /// trip); results then compare with ones recorded without them.
        #[arg(long)]
        no_cpu_diag: bool,
    },
    /// Interleave several schedulers, switched with scxcli (scx.service).
    ///
    /// Every round visits each scheduler once, in alternating order, so
    /// slow drift affects all of them alike. Writes one result file per
    /// scheduler and a report into the output directory, then restores the
    /// previously selected scheduler. Run as root to avoid a polkit prompt
    /// at every switch.
    Matrix {
        /// Scheduler to include, as scxcli takes it: `default` (kernel),
        /// a scheduler (`scx_lavd`), a catalog profile id, or a scheduler
        /// with options in one argument (`"scx_bpfland -s 5000"`). Repeat;
        /// the first one is the report's baseline.
        #[arg(long = "sched", required = true)]
        scheds: Vec<String>,
        /// Rounds; each visits every scheduler once.
        #[arg(long, default_value_t = 5)]
        rounds: u32,
        #[arg(long, default_value = "quick", value_parser = clap::builder::PossibleValuesParser::new(SUITES))]
        suite: String,
        /// Measured runs per workload per visit.
        #[arg(long, default_value_t = 1)]
        runs_per_visit: usize,
        /// Unrecorded warm-up runs per workload per visit.
        #[arg(long, default_value_t = 1)]
        warmup: usize,
        /// Seconds per run.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Seconds of idle between runs.
        #[arg(long, default_value_t = 1.0)]
        cooldown: f64,
        /// Seconds a scheduler must stay unchanged after a switch before
        /// measuring.
        #[arg(long, default_value_t = 3.0)]
        settle: f64,
        /// Seconds to wait for a switch to take effect.
        #[arg(long, default_value_t = 30.0)]
        switch_timeout: f64,
        /// Seconds any single scxcli/systemctl call may take before it is
        /// killed.
        #[arg(long, default_value_t = 20.0)]
        command_timeout: f64,
        /// Accept a kernel ops name that differs from the scheduler's name
        /// (normally `scx_lavd` must attach as `lavd`).
        #[arg(long)]
        allow_ops_mismatch: bool,
        /// scxcli binary.
        #[arg(long, default_value = "scxcli")]
        scxcli: PathBuf,
        /// systemctl binary (used only to restore scx.service's enablement).
        #[arg(long, default_value = "systemctl")]
        systemctl: PathBuf,
        /// Skip the same-/cross-CPU diagnostics.
        #[arg(long)]
        no_cpu_diag: bool,
        /// Output directory (default: helicoprion-matrix-<unix time>).
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        thresholds: Thresholds,
    },
    /// Compare result files and write an HTML report.
    Compare {
        /// Result files; colours follow this order.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Baseline: index (0-based) or label. Default: the first file.
        #[arg(short, long)]
        baseline: Option<String>,
        /// HTML report path.
        #[arg(short, long, default_value = "helicoprion-report.html")]
        output: PathBuf,
        /// Also write each chart as a standalone .svg into this directory.
        #[arg(long)]
        svg_dir: Option<PathBuf>,
        #[command(flatten)]
        thresholds: Thresholds,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info { json } => info(json),
        Cmd::Run { suite, runs, warmup, duration, cooldown, label, output, no_cpu_diag } => run(RunArgs {
            suite,
            runs,
            warmup,
            duration,
            cooldown,
            label,
            output,
            suite_opts: SuiteOpts { cpu_diag: !no_cpu_diag },
        }),
        Cmd::Matrix {
            scheds,
            rounds,
            suite,
            runs_per_visit,
            warmup,
            duration,
            cooldown,
            settle,
            switch_timeout,
            command_timeout,
            allow_ops_mismatch,
            scxcli,
            systemctl,
            no_cpu_diag,
            output,
            thresholds,
        } => matrix_cmd(MatrixArgs {
            scheds,
            rounds,
            suite,
            runs_per_visit,
            warmup,
            duration,
            cooldown,
            settle,
            switch_timeout,
            command_timeout,
            allow_ops_mismatch,
            scxcli,
            systemctl,
            suite_opts: SuiteOpts { cpu_diag: !no_cpu_diag },
            output,
            opts: thresholds.options()?,
        }),
        Cmd::Compare { files, baseline, output, svg_dir, thresholds } => {
            compare(&files, baseline, &output, svg_dir, thresholds.options()?)
        }
    }
}

fn info(json: bool) -> Result<()> {
    let sched = Scheduler::detect();
    let sys = SystemInfo::collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "scheduler": sched, "system": sys }))?);
        return Ok(());
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "?".into());
    println!("Scheduler:  {sched}");
    println!("Kernel:     {}", sys.kernel);
    println!("CPU:        {}", opt(&sys.cpu_model));
    println!(
        "CPUs:       {} usable (online: {}, SMT: {})",
        sys.usable_cpus,
        opt(&sys.online_cpus),
        sys.smt_active.map_or("?", |s| if s { "on" } else { "off" })
    );
    println!("Governor:   {} (EPP: {})", opt(&sys.governor), opt(&sys.epp));
    if let Some(p) = &sys.platform_profile {
        println!("Profile:    {p}");
    }
    println!("THP:        {}", opt(&sys.thp));
    if let Some(l) = sys.loadavg_1m {
        println!("Load (1m):  {l:.2}");
    }
    Ok(())
}

struct RunArgs {
    suite: String,
    runs: usize,
    warmup: usize,
    duration: f64,
    cooldown: f64,
    label: Option<String>,
    output: Option<PathBuf>,
    suite_opts: SuiteOpts,
}

fn run(a: RunArgs) -> Result<()> {
    let RunArgs { suite: suite_name, runs, warmup, duration, cooldown, label, output, suite_opts } = a;
    let suite_name = suite_name.as_str();
    if runs == 0 {
        bail!("--runs must be at least 1");
    }
    if !(duration.is_finite() && duration > 0.0 && cooldown.is_finite() && cooldown >= 0.0) {
        bail!("--duration must be > 0 and --cooldown >= 0");
    }
    let sched = Scheduler::detect();
    let sys = SystemInfo::collect();
    let workloads = suite(suite_name, sys.usable_cpus, suite_opts).context("unknown suite")?;
    let label = label.unwrap_or_else(|| sched.label());
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let output = output.unwrap_or_else(|| PathBuf::from(format!("helicoprion-{}-{started_at}.json", sanitize(&label))));

    eprintln!("scheduler: {sched}");
    eprintln!("kernel:    {}  ({} CPUs)", sys.kernel, sys.usable_cpus);
    if let Some(l) = sys.loadavg_1m
        && l > 0.25 * sys.usable_cpus as f64
    {
        eprintln!("warning:   load average {l:.2} — the system is not idle, results will be noisy");
    }
    let per_run = duration + cooldown;
    let total = workloads.len() as f64 * (runs + warmup) as f64 * per_run;
    eprintln!(
        "suite '{suite_name}': {} workloads × ({warmup} warm-up + {runs} runs) × {duration}s ≈ {:.0}s\n",
        workloads.len(),
        total
    );

    if !sched.restart_detectable() {
        eprintln!(
            "warning:   {} exposes no enable_seq; a crash-and-restart of the same scheduler cannot be detected",
            sched.label()
        );
    }

    let cfg = RunConfig {
        expected: sched.clone(),
        detect: &Scheduler::detect,
        cancel: &|| false,
        runs,
        warmup,
        duration: Duration::from_secs_f64(duration),
        cooldown: Duration::from_secs_f64(cooldown),
    };
    let mut results = Vec::new();
    for w in &workloads {
        let res = run_workload(w.as_ref(), &cfg, progress);
        results.push(res);
    }

    let file = ResultFile {
        schema: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        label,
        started_at,
        system: sys,
        restart_detectable: sched.restart_detectable(),
        design: None,
        scheduler: sched,
        workloads: results,
    };
    fs::write(&output, serde_json::to_string_pretty(&file)?)
        .with_context(|| format!("writing {}", output.display()))?;
    eprintln!("\nwrote {}", output.display());
    Ok(())
}

fn compare(
    files: &[PathBuf],
    baseline: Option<String>,
    output: &Path,
    svg_dir: Option<PathBuf>,
    opts: Options,
) -> Result<()> {
    const MAX: usize = 8;
    if files.len() > MAX {
        bail!("at most {MAX} result files per report (got {}); split into several reports", files.len());
    }
    let parsed: Vec<ResultFile> = files
        .iter()
        .map(|p| {
            let text = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            let f: ResultFile = serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
            if !(MIN_READABLE_SCHEMA..=SCHEMA_VERSION).contains(&f.schema) {
                bail!(
                    "{}: schema {} not supported (this build reads {MIN_READABLE_SCHEMA}..={SCHEMA_VERSION})",
                    p.display(),
                    f.schema
                );
            }
            Ok(f)
        })
        .collect::<Result<_>>()?;

    let base = match baseline {
        None => 0,
        Some(b) => match b.parse::<usize>() {
            Ok(i) if i < parsed.len() => i,
            Ok(i) => bail!("baseline index {i} out of range (0..{})", parsed.len()),
            Err(_) => {
                parsed.iter().position(|f| f.label == b).with_context(|| format!("no result file labelled '{b}'"))?
            }
        },
    };

    let c = Comparison::build_with(parsed, base, opts);
    write_report_with(&c, output)?;

    if let Some(dir) = svg_dir {
        fs::create_dir_all(&dir)?;
        let mut charts: Vec<(String, String)> = Vec::new();
        if let Some(s) = svg::delta_chart(&c) {
            charts.push(("change-vs-baseline".into(), s));
        }
        for d in &c.distributions {
            if let Some(s) = svg::tail_chart(&c, d) {
                charts.push((format!("quantiles-{}-{}", sanitize(&d.workload), sanitize(&d.population)), s));
            }
        }
        for (name, s) in charts {
            let p = dir.join(format!("{name}.svg"));
            fs::write(&p, s)?;
            eprintln!("wrote {}", p.display());
        }
    }
    Ok(())
}

fn progress(ev: Event<'_>) {
    match ev {
        Event::Start { workload, run, total, warmup } => {
            if warmup {
                eprint!("  {workload:<22} warm-up {}… ", run + 1);
            } else {
                eprint!("  {workload:<22} run {}/{total}… ", run + 1);
            }
            let _ = std::io::stderr().flush();
        }
        Event::Done { warmup: true, .. } => eprintln!("discarded"),
        Event::Done { valid, summary, .. } => {
            eprintln!("{}{summary}", if valid { "" } else { "✗ " });
        }
    }
}

struct MatrixArgs {
    scheds: Vec<String>,
    rounds: u32,
    suite: String,
    runs_per_visit: usize,
    warmup: usize,
    duration: f64,
    cooldown: f64,
    settle: f64,
    switch_timeout: f64,
    command_timeout: f64,
    allow_ops_mismatch: bool,
    scxcli: PathBuf,
    systemctl: PathBuf,
    suite_opts: SuiteOpts,
    output: Option<PathBuf>,
    opts: Options,
}

fn matrix_cmd(a: MatrixArgs) -> Result<()> {
    use matrix::{Control, Prior, ScxCli, Session, SwitchCfg, interrupted, prepare_out_dir, resolve, run_matrix};

    if a.rounds == 0 || a.runs_per_visit == 0 {
        bail!("--rounds and --runs-per-visit must be at least 1");
    }
    for (name, v) in [
        ("--duration", a.duration),
        ("--settle", a.settle),
        ("--switch-timeout", a.switch_timeout),
        ("--command-timeout", a.command_timeout),
    ] {
        if !(v.is_finite() && v > 0.0) {
            bail!("{name} must be > 0");
        }
    }
    if !(a.cooldown.is_finite() && a.cooldown >= 0.0) {
        bail!("--cooldown must be >= 0");
    }
    if a.scheds.len() > 8 {
        bail!("at most 8 schedulers per matrix (one report colour each)");
    }
    let cmd_timeout = Duration::from_secs_f64(a.command_timeout);
    let ctl = ScxCli { bin: a.scxcli.clone(), systemctl: a.systemctl.clone() };
    let initial = ctl.get(cmd_timeout).context("is scxcli installed? (--scxcli PATH)")?;
    let prior = Prior::plan(&initial)?;
    let targets = resolve(&a.scheds, &ctl, cmd_timeout)?;
    let sys = SystemInfo::collect();
    let workloads = suite(&a.suite, sys.usable_cpus, a.suite_opts).context("unknown suite")?;
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let out_dir = a.output.clone().unwrap_or_else(|| PathBuf::from(format!("helicoprion-matrix-{started_at}")));
    prepare_out_dir(&out_dir)?;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("warning:   not root: scxcli will ask polkit at every switch");
    }
    let per_visit =
        a.settle + workloads.len() as f64 * (a.warmup + a.runs_per_visit) as f64 * (a.duration + a.cooldown);
    eprintln!("kernel:    {}  ({} CPUs)", sys.kernel, sys.usable_cpus);
    eprintln!("now:       {}  (restored at the end)", prior.describe());
    eprintln!(
        "           by hand, if needed: {}",
        prior.commands().iter().map(matrix::Cmd::shell).collect::<Vec<_>>().join(" && ")
    );
    eprintln!(
        "matrix:    {} schedulers × {} rounds, {} workloads, ≈{:.0} min → {}",
        targets.len(),
        a.rounds,
        workloads.len(),
        per_visit * (targets.len() as u32 * a.rounds) as f64 / 60.0,
        out_dir.display()
    );
    matrix::install_sigint();

    let session = Session {
        targets,
        rounds: a.rounds,
        runs_per_visit: a.runs_per_visit,
        warmup: a.warmup,
        duration: Duration::from_secs_f64(a.duration),
        cooldown: Duration::from_secs_f64(a.cooldown),
        switch: SwitchCfg {
            timeout: Duration::from_secs_f64(a.switch_timeout),
            settle: Duration::from_secs_f64(a.settle),
            poll: Duration::from_millis(200),
            cmd_timeout,
            allow_ops_mismatch: a.allow_ops_mismatch,
        },
        workloads: &workloads,
        out_dir: out_dir.clone(),
        system: sys,
        started_at,
        session_id: matrix::new_session_id(),
    };
    let out = run_matrix(&session, &ctl, &Scheduler::detect, &interrupted, prior, progress);

    if out.interrupted {
        eprintln!("\ninterrupted after {} visits; results so far are in {}", out.visits, out_dir.display());
    }
    // Report the restore problem first: a later failure (writing the
    // report on a full disk, say) must not hide it.
    if let Some(e) = &out.restore_error {
        eprintln!("error:     the previous scx.service state was NOT restored: {e:#}");
    }
    let report =
        if out.files.is_empty() { Ok(()) } else { write_report(out.files, &out_dir.join("report.html"), a.opts) };
    if let Err(e) = &report {
        eprintln!("error:     writing the report failed: {e:#}");
    }
    // Most important first: the error that ended the session, then the
    // restore, then the report.
    match (out.error, out.restore_error, report) {
        (Some(e), _, _) | (None, Some(e), _) | (None, None, Err(e)) => Err(e),
        (None, None, Ok(())) => Ok(()),
    }
}

fn write_report(files: Vec<ResultFile>, path: &Path, opts: Options) -> Result<()> {
    write_report_with(&Comparison::build_with(files, 0, opts), path)
}

fn write_report_with(c: &Comparison, path: &Path) -> Result<()> {
    print!("{}", html::text_table(c));
    for w in &c.warnings {
        eprintln!("warning: {w}");
    }
    for n in &c.env_notes {
        eprintln!("note:    {n}");
    }
    fs::write(path, html::report(c)).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

/// What a report highlights; shared by `compare` and `matrix`.
#[derive(clap::Args, Clone, Copy)]
struct Thresholds {
    /// Smallest change (in %) that is highlighted. Calibrate with an A/A run
    /// (the same scheduler twice): between-session drift is invisible to the
    /// per-session bootstrap.
    #[arg(long, default_value_t = 5.0)]
    min_effect: f64,
    /// Run-to-run spread (± half the relative IQR, in %) above which a cell
    /// is marked unstable and kept off the chart.
    #[arg(long, default_value_t = 10.0)]
    unstable_spread: f64,
}

impl Thresholds {
    fn options(self) -> Result<Options> {
        if !(self.min_effect.is_finite() && self.min_effect >= 0.0) {
            bail!("--min-effect must be >= 0");
        }
        if !(self.unstable_spread.is_finite() && self.unstable_spread > 0.0) {
            bail!("--unstable-spread must be > 0");
        }
        Ok(Options { min_effect_pct: self.min_effect, unstable_spread_pct: self.unstable_spread })
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}
