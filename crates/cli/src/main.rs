use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use helicoprion_core::{MIN_READABLE_SCHEMA, ResultFile, SCHEMA_VERSION, Scheduler, SystemInfo};
use helicoprion_report::{Comparison, Options, html, svg};
use helicoprion_workloads::runner::{Event, RunConfig, run_workload};
use helicoprion_workloads::{SUITES, suite};

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
        Cmd::Run { suite, runs, warmup, duration, cooldown, label, output } => {
            run(&suite, runs, warmup, duration, cooldown, label, output)
        }
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

fn run(
    suite_name: &str,
    runs: usize,
    warmup: usize,
    duration: f64,
    cooldown: f64,
    label: Option<String>,
    output: Option<PathBuf>,
) -> Result<()> {
    if runs == 0 {
        bail!("--runs must be at least 1");
    }
    if !(duration.is_finite() && duration > 0.0 && cooldown.is_finite() && cooldown >= 0.0) {
        bail!("--duration must be > 0 and --cooldown >= 0");
    }
    let sched = Scheduler::detect();
    let sys = SystemInfo::collect();
    let workloads = suite(suite_name, sys.usable_cpus).context("unknown suite")?;
    let label = label.unwrap_or_else(|| sched.label());
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let output = output.unwrap_or_else(|| PathBuf::from(format!("helicoprion-{}-{started_at}.json", sanitize(&label))));

    eprintln!("scheduler: {sched}");
    eprintln!("kernel:    {}  ({} CPUs)", sys.kernel, sys.usable_cpus);
    if let Some(l) = sys.loadavg_1m {
        if l > 0.25 * sys.usable_cpus as f64 {
            eprintln!("warning:   load average {l:.2} — the system is not idle, results will be noisy");
        }
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
        runs,
        warmup,
        duration: Duration::from_secs_f64(duration),
        cooldown: Duration::from_secs_f64(cooldown),
    };
    let mut results = Vec::new();
    for w in &workloads {
        let res = run_workload(w.as_ref(), &cfg, |ev| match ev {
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
        });
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
    output: &PathBuf,
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
    print!("{}", html::text_table(&c));
    for w in &c.warnings {
        eprintln!("warning: {w}");
    }
    for n in &c.env_notes {
        eprintln!("note:    {n}");
    }

    fs::write(output, html::report(&c)).with_context(|| format!("writing {}", output.display()))?;
    eprintln!("\nwrote {}", output.display());

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
