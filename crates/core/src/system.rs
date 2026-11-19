//! Machine state that affects results and must travel with them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::read_trimmed;
use crate::sched::kernel_release;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    pub kernel: String,
    pub cpu_model: Option<String>,
    /// CPUs this process may run on (affinity-aware).
    pub usable_cpus: usize,
    /// Which ones, e.g. `0-15` or `8-15` (`Cpus_allowed_list`). Two
    /// sessions on eight CPUs each are not the same setup when one has
    /// 0-7 and the other 8-15: other cores, perhaps another cache domain
    /// or another kind of core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_cpus: Option<String>,
    /// `/sys/devices/system/cpu/online`, e.g. `0-15`.
    pub online_cpus: Option<String>,
    pub smt_active: Option<bool>,
    /// Across all cpufreq policies: the value if uniform, otherwise
    /// `mixed: performance×8, powersave×8`.
    pub governor: Option<String>,
    pub epp: Option<String>,
    pub platform_profile: Option<String>,
    pub thp: Option<String>,
    /// 1-minute load average when the run started.
    pub loadavg_1m: Option<f64>,
    /// Where `Reading::temp_c` comes from, e.g. `k10temp/Tctl`. `None`
    /// when no CPU temperature sensor was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_sensor: Option<String>,
    /// The build behind the release string: `#1 SMP PREEMPT_DYNAMIC` and
    /// the build date (`/proc/sys/kernel/version`). Two builds of one
    /// release differ here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_build: Option<String>,
    /// The preemption model in effect (`none`, `voluntary`, `full`,
    /// `lazy`), from debugfs: readable by root only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preempt: Option<String>,
    /// The kernel's timer tick in Hz: how often a busy CPU looks whether
    /// the running task should give way. Worked out from the resolution
    /// of the coarse clock, not read from the kernel's configuration: it
    /// is `CONFIG_HZ` as far as that clock tells, and absent where the
    /// resolution is no plausible tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_hz: Option<u32>,
    /// Kernel command line parameters that change scheduling, timing or
    /// CPU power management, as given. Only those: the rest of a command
    /// line (root device, UUIDs) says nothing about a result and does not
    /// belong in a file that gets shared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmdline: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<Topology>,
    /// CPUs isolated from the scheduler, e.g. `2-3`; absent when none are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated_cpus: Option<String>,
    /// cpufreq driver, e.g. `amd-pstate-epp`, summarised like `governor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpufreq_driver: Option<String>,
    /// Highest frequency of the cpufreq policies in kHz, summarised like
    /// `governor`: differs between the cores of a hybrid CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_freq_khz: Option<String>,
    /// Frequency boost (turbo) allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boost: Option<bool>,
    /// Running on mains power; `None` on a machine without a battery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ac: Option<bool>,
    /// Seconds since boot when the session started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<f64>,
}

/// How the online CPUs are grouped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub packages: u32,
    /// Physical cores; fewer than the CPUs when SMT is on.
    pub cores: u32,
    /// Groups of CPUs sharing a last-level (L3) cache; `None` when the
    /// kernel reports no L3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l3_domains: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_nodes: Option<u32>,
}

impl SystemInfo {
    pub fn collect() -> Self {
        let cpufreq = Path::new("/sys/devices/system/cpu/cpufreq");
        SystemInfo {
            kernel: kernel_release(),
            cpu_model: cpu_model(),
            usable_cpus: usable_cpus(),
            allowed_cpus: fs::read_to_string("/proc/self/status").ok().and_then(|s| allowed_cpus(&s)),
            online_cpus: read_trimmed("/sys/devices/system/cpu/online"),
            smt_active: read_trimmed("/sys/devices/system/cpu/smt/active").map(|v| v == "1"),
            governor: across_policies(cpufreq, "scaling_governor"),
            epp: across_policies(cpufreq, "energy_performance_preference"),
            platform_profile: read_trimmed("/sys/firmware/acpi/platform_profile"),
            thp: read_trimmed("/sys/kernel/mm/transparent_hugepage/enabled").map(|s| selected_option(&s)),
            loadavg_1m: loadavg_1m(),
            temp_sensor: Probe::find().sensor().map(str::to_string),
            kernel_build: read_trimmed("/proc/sys/kernel/version"),
            preempt: read_trimmed("/sys/kernel/debug/sched/preempt").map(|s| selected_option(&s)),
            tick_hz: coarse_clock_resolution_ns().and_then(tick_hz),
            cmdline: read_trimmed("/proc/cmdline").map(|s| relevant_cmdline(&s)).unwrap_or_default(),
            topology: topology(Path::new("/sys/devices/system")),
            isolated_cpus: read_trimmed("/sys/devices/system/cpu/isolated"),
            cpufreq_driver: across_policies(cpufreq, "scaling_driver"),
            max_freq_khz: across_policies(cpufreq, "cpuinfo_max_freq"),
            boost: boost(Path::new("/sys/devices/system/cpu")),
            on_ac: on_ac(Path::new("/sys/class/power_supply")),
            uptime_s: read_trimmed("/proc/uptime")
                .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok())),
        }
    }
}

/// Resolution of `CLOCK_MONOTONIC_COARSE` in nanoseconds. That clock
/// advances once per timer tick, so this is the tick's length: the
/// configured rate is in no file that every distribution ships
/// (`/proc/config.gz` is optional and compressed).
fn coarse_clock_resolution_ns() -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write to.
    let r = unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC_COARSE, &mut ts) };
    (r == 0).then(|| ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

/// The tick rate for a tick of `tick_ns`: the nearest whole rate. This
/// is what the clock says, not a check against the rates a kernel can be
/// configured with, which differ between architectures and patched
/// kernels (100, 250, 300 and 1000 on x86, others elsewhere and in some
/// distribution kernels). Only a resolution that cannot be a tick at all
/// gives nothing: under 100 µs, where the clock is not coarse, or over
/// 50 ms.
fn tick_hz(tick_ns: u64) -> Option<u32> {
    const TICKS_NS: std::ops::RangeInclusive<u64> = 100_000..=50_000_000;
    // The range also keeps the division from a zero: of the tick, and of
    // a rate that a tick of two seconds or more would round to.
    TICKS_NS.contains(&tick_ns).then(|| ((1_000_000_000 + tick_ns / 2) / tick_ns) as u32)
}

/// `Cpus_allowed_list` of a `/proc/<pid>/status`.
fn allowed_cpus(status: &str) -> Option<String> {
    let list = status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?.trim();
    (!list.is_empty()).then(|| list.to_string())
}

/// Kernel parameters worth recording, by name (the part before `=`).
/// Scheduling and isolation, preemption and timer ticks, CPU frequency
/// and idle states, SMT, and CPU vulnerability mitigations, which change
/// the cost of every switch into the kernel.
const CMDLINE_KEYS: &[&str] = &[
    "isolcpus",
    "nohz",
    "nohz_full",
    "rcu_nocbs",
    "rcu_nocb_poll",
    "irqaffinity",
    "preempt",
    "threadirqs",
    "skew_tick",
    "nosmt",
    "maxcpus",
    "nr_cpus",
    "numa_balancing",
    "transparent_hugepage",
    "amd_pstate",
    "amd_prefcore",
    "intel_pstate",
    "idle",
    "tsc",
    "clocksource",
    "split_lock_detect",
    "mitigations",
    "nopti",
    "pti",
    "nospectre_v1",
    "nospectre_v2",
    "spectre_v2",
    "spectre_v2_user",
    "spectre_bhi",
    "spec_store_bypass_disable",
    "retbleed",
    "spec_rstack_overflow",
    "l1tf",
    "mds",
    "tsx_async_abort",
    "srbds",
    "mmio_stale_data",
    "gather_data_sampling",
    "reg_file_data_sampling",
];
/// And whole families, by prefix.
const CMDLINE_PREFIXES: &[&str] = &["sched_", "cpufreq.", "cpuidle.", "intel_idle.", "processor."];

fn relevant_cmdline(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|word| {
            let key = word.split('=').next().unwrap_or(word);
            CMDLINE_KEYS.contains(&key) || CMDLINE_PREFIXES.iter().any(|p| key.starts_with(p))
        })
        .map(str::to_string)
        .collect()
}

/// Packages, cores, L3 groups and NUMA nodes of the online CPUs, from
/// `<root>/cpu/cpuN` and `<root>/node/nodeN`.
fn topology(root: &Path) -> Option<Topology> {
    use std::collections::BTreeSet;
    let numbered = |dir: PathBuf, prefix: &'static str| {
        fs::read_dir(dir).into_iter().flatten().flatten().filter_map(move |e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name.strip_prefix(prefix)?;
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| e.path())
        })
    };
    let (mut packages, mut cores, mut l3) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for cpu in numbered(root.join("cpu"), "cpu") {
        // Offline CPUs have no topology directory.
        let Some(package) = read_trimmed(cpu.join("topology/physical_package_id")) else { continue };
        let core = read_trimmed(cpu.join("topology/core_id")).unwrap_or_default();
        cores.insert((package.clone(), core));
        packages.insert(package);
        for index in numbered(cpu.join("cache"), "index") {
            if read_trimmed(index.join("level")).as_deref() == Some("3")
                && let Some(shared) = read_trimmed(index.join("shared_cpu_list"))
            {
                l3.insert(shared);
            }
        }
    }
    if packages.is_empty() {
        return None;
    }
    let nodes = numbered(root.join("node"), "node").count() as u32;
    Some(Topology {
        packages: packages.len() as u32,
        cores: cores.len() as u32,
        l3_domains: (!l3.is_empty()).then_some(l3.len() as u32),
        numa_nodes: (nodes > 0).then_some(nodes),
    })
}

/// Frequency boost allowed: the cpufreq switch, or intel_pstate's
/// inverted `no_turbo`.
fn boost(cpu: &Path) -> Option<bool> {
    read_trimmed(cpu.join("cpufreq/boost"))
        .map(|v| v == "1")
        .or_else(|| read_trimmed(cpu.join("intel_pstate/no_turbo")).map(|v| v == "0"))
}

/// On mains power, if there is a mains supply to ask.
fn on_ac(power_supply: &Path) -> Option<bool> {
    let mains: Vec<bool> = fs::read_dir(power_supply)
        .ok()?
        .flatten()
        .filter(|e| read_trimmed(e.path().join("type")).as_deref() == Some("Mains"))
        .filter_map(|e| read_trimmed(e.path().join("online")).map(|v| v == "1"))
        .collect();
    (!mains.is_empty()).then(|| mains.contains(&true))
}

fn loadavg_1m() -> Option<f64> {
    read_trimmed("/proc/loadavg").and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
}

/// What can drift while a session runs, read once: the clock, the load,
/// the CPU temperature and the frequency the CPUs run at. Taken right
/// before and right after every run, outside the measurement, so that a
/// result can be checked for signs of drift afterwards (a machine heating
/// up, a clock throttled, something else getting busy) and so that the
/// order and the timing of the runs are on record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// Unix time in seconds.
    pub at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loadavg_1m: Option<f64>,
    /// CPU temperature in °C, from `SystemInfo::temp_sensor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    /// The frequency the CPUs currently run at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_mhz: Option<Freq>,
}

/// `scaling_cur_freq` of the cpufreq policies, in MHz.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Freq {
    pub min: f64,
    /// Mean over the CPUs: every policy counts as often as it has CPUs
    /// (`related_cpus`), so a policy covering one core weighs less than
    /// one covering eight.
    pub mean: f64,
    pub max: f64,
}

/// The files a `Reading` comes from, looked up once per session.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// Sensor name and its `temp*_input` (millidegrees).
    temp: Option<(String, PathBuf)>,
    /// One `scaling_cur_freq` (kHz) per cpufreq policy, with the number
    /// of CPUs the policy covers.
    freq: Vec<(PathBuf, f64)>,
}

/// hwmon drivers that report the CPU's own temperature, best first.
const CPU_SENSORS: &[&str] = &["k10temp", "zenpower", "coretemp", "cpu_thermal", "cpu-thermal", "soc_thermal"];

impl Probe {
    pub fn find() -> Probe {
        Probe::find_at(
            Path::new("/sys/class/hwmon"),
            Path::new("/sys/class/thermal"),
            Path::new("/sys/devices/system/cpu/cpufreq"),
        )
    }

    /// Lookup against arbitrary directories, so it can be tested.
    pub fn find_at(hwmon: &Path, thermal: &Path, cpufreq: &Path) -> Probe {
        let dirs = |root: &Path, prefix: &str| {
            let mut v: Vec<PathBuf> = fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .map(|e| e.path())
                .collect();
            v.sort();
            v
        };
        // The driver's first temperature is the CPU's own (Tctl for
        // k10temp, the package for coretemp).
        let hw = dirs(hwmon, "hwmon");
        let from_hwmon = CPU_SENSORS.iter().find_map(|want| {
            hw.iter().find(|d| read_trimmed(d.join("name")).as_deref() == Some(want)).and_then(|d| {
                let input = d.join("temp1_input");
                read_trimmed(&input)?;
                let name = match read_trimmed(d.join("temp1_label")) {
                    Some(label) => format!("{want}/{label}"),
                    None => (*want).to_string(),
                };
                Some((name, input))
            })
        });
        let from_thermal = || {
            dirs(thermal, "thermal_zone").into_iter().find_map(|d| {
                let kind = read_trimmed(d.join("type"))?;
                let input = d.join("temp");
                (kind == "x86_pkg_temp" && read_trimmed(&input).is_some()).then_some((kind, input))
            })
        };
        let freq = dirs(cpufreq, "policy")
            .into_iter()
            .filter_map(|d| {
                let cur = d.join("scaling_cur_freq");
                read_trimmed(&cur)?;
                // A policy that does not list its CPUs counts once.
                let cpus = read_trimmed(d.join("related_cpus")).map_or(0, |s| s.split_whitespace().count());
                Some((cur, cpus.max(1) as f64))
            })
            .collect();
        Probe { temp: from_hwmon.or_else(from_thermal), freq }
    }

    /// The temperature sensor in use, e.g. `k10temp/Tctl`.
    pub fn sensor(&self) -> Option<&str> {
        self.temp.as_ref().map(|(name, _)| name.as_str())
    }

    pub fn read(&self) -> Reading {
        let number = |p: &PathBuf| read_trimmed(p).and_then(|s| s.parse::<f64>().ok());
        let khz: Vec<(f64, f64)> = self.freq.iter().filter_map(|(p, cpus)| Some((number(p)?, *cpus))).collect();
        let freq_mhz = (!khz.is_empty()).then(|| Freq {
            min: khz.iter().map(|(f, _)| *f).fold(f64::INFINITY, f64::min) / 1e3,
            mean: khz.iter().map(|(f, n)| f * n).sum::<f64>() / khz.iter().map(|(_, n)| n).sum::<f64>() / 1e3,
            max: khz.iter().map(|(f, _)| *f).fold(f64::NEG_INFINITY, f64::max) / 1e3,
        });
        Reading {
            // Milliseconds are plenty for ordering runs seconds apart.
            at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64 / 1e3),
            loadavg_1m: loadavg_1m(),
            temp_c: self.temp.as_ref().and_then(|(_, p)| number(p)).map(|m| m / 1e3),
            freq_mhz,
        }
    }
}

pub fn usable_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Read `file` from every `policy*` directory and summarise.
pub fn across_policies(cpufreq: &Path, file: &str) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in fs::read_dir(cpufreq).ok()?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("policy")
            && let Some(v) = read_trimmed(entry.path().join(file))
        {
            *counts.entry(v).or_default() += 1;
        }
    }
    summarize(&counts)
}

fn summarize(counts: &BTreeMap<String, usize>) -> Option<String> {
    match counts.len() {
        0 => None,
        1 => counts.keys().next().cloned(),
        _ => Some(format!("mixed: {}", counts.iter().map(|(v, n)| format!("{v}×{n}")).collect::<Vec<_>>().join(", "))),
    }
}

fn cpu_model() -> Option<String> {
    let info = fs::read_to_string("/proc/cpuinfo").ok()?;
    // x86: "model name", arm64: no model name, fall back to "Hardware"/"CPU part".
    ["model name", "Hardware", "CPU part"].iter().find_map(|key| {
        info.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == *key).then(|| v.trim().to_string())
        })
    })
}

/// `always [madvise] never` -> `madvise`; `none voluntary (full) lazy` ->
/// `full`.
fn selected_option(s: &str) -> String {
    s.split_whitespace()
        .find_map(|w| {
            w.strip_prefix('[')
                .and_then(|w| w.strip_suffix(']'))
                .or_else(|| w.strip_prefix('(').and_then(|w| w.strip_suffix(')')))
        })
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn policy_summary() {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::new();
        assert_eq!(super::summarize(&m), None);
        m.insert("performance".to_string(), 8);
        assert_eq!(super::summarize(&m).as_deref(), Some("performance"));
        m.insert("powersave".to_string(), 8);
        assert_eq!(super::summarize(&m).as_deref(), Some("mixed: performance×8, powersave×8"));
    }

    #[test]
    fn probe_finds_the_cpu_sensor_and_reads_it() {
        use super::Probe;
        use std::fs;
        let root = std::env::temp_dir().join(format!("helicoprion-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let put = |rel: &str, text: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        // A disk, the GPU and the CPU, in an order that is not the CPU's.
        put("hwmon/hwmon0/name", "nvme\n");
        put("hwmon/hwmon0/temp1_input", "38850\n");
        put("hwmon/hwmon1/name", "amdgpu\n");
        put("hwmon/hwmon1/temp1_input", "51000\n");
        put("hwmon/hwmon2/name", "k10temp\n");
        put("hwmon/hwmon2/temp1_label", "Tctl\n");
        put("hwmon/hwmon2/temp1_input", "67250\n");
        put("thermal/thermal_zone0/type", "acpitz\n");
        put("thermal/thermal_zone0/temp", "20000\n");
        put("cpufreq/policy0/scaling_cur_freq", "3000000\n");
        put("cpufreq/policy1/scaling_cur_freq", "5100000\n");
        put("cpufreq/policy10/scaling_cur_freq", "1200000\n");
        // A policy without a readable frequency is left out, not read as 0.
        put("cpufreq/policy2/scaling_governor", "performance\n");

        let at = |sub: [&str; 3]| Probe::find_at(&root.join(sub[0]), &root.join(sub[1]), &root.join(sub[2]));
        let p = at(["hwmon", "thermal", "cpufreq"]);
        assert_eq!(p.sensor(), Some("k10temp/Tctl"));
        let r = p.read();
        assert_eq!(r.temp_c, Some(67.25));
        let f = r.freq_mhz.unwrap();
        assert_eq!((f.min, f.mean, f.max), (1200.0, 3100.0, 5100.0));
        assert!(r.at > 1.6e9, "a Unix time: {}", r.at);

        // Without a known hwmon driver: the package zone, never an
        // unrelated one (acpitz is often the mainboard, or a constant).
        assert_eq!(at(["none", "thermal", "cpufreq"]).sensor(), None);
        put("thermal/thermal_zone1/type", "x86_pkg_temp\n");
        put("thermal/thermal_zone1/temp", "45000\n");
        let p = at(["none", "thermal", "cpufreq"]);
        assert_eq!((p.sensor(), p.read().temp_c), (Some("x86_pkg_temp"), Some(45.0)));

        // The mean is over CPUs, not over policies: one policy for six
        // CPUs at 2 GHz and two for one CPU each at 4 GHz are 2.5 GHz, not
        // 3.33. A policy that does not list its CPUs counts once.
        put("weighted/policy0/scaling_cur_freq", "2000000\n");
        put("weighted/policy0/related_cpus", "0 1 2 3 4 5\n");
        put("weighted/policy6/scaling_cur_freq", "4000000\n");
        put("weighted/policy6/related_cpus", "6\n");
        put("weighted/policy7/scaling_cur_freq", "4000000\n");
        let f = at(["none", "none", "weighted"]).read().freq_mhz.unwrap();
        assert_eq!((f.min, f.mean, f.max), (2000.0, 2500.0, 4000.0));

        // Nothing to read: a time, and no made-up values.
        let r = at(["none", "none", "none"]).read();
        assert_eq!((r.temp_c, r.freq_mhz), (None, None));
        // Optional readings are left out of the file, not written as null.
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("temp_c") && !json.contains("freq_mhz"), "{json}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn thp_selected() {
        assert_eq!(super::selected_option("always [madvise] never"), "madvise");
        assert_eq!(super::selected_option("weird"), "weird");
        // The preemption model in debugfs.
        assert_eq!(super::selected_option("none voluntary (full) lazy"), "full");
    }

    #[test]
    fn allowed_cpus_are_read_from_the_status() {
        let status = "Name:\thelicoprion\nCpus_allowed:\tff00\nCpus_allowed_list:\t8-15\nMems_allowed_list:\t0\n";
        assert_eq!(super::allowed_cpus(status).as_deref(), Some("8-15"));
        assert_eq!(super::allowed_cpus("Name:\tx\nCpus_allowed:\tff\n"), None);
        // This process has some.
        assert!(super::SystemInfo::collect().allowed_cpus.is_some());
    }

    #[test]
    fn the_tick_rate_comes_from_the_tick_length() {
        assert_eq!(super::tick_hz(1_000_000), Some(1000));
        assert_eq!(super::tick_hz(4_000_000), Some(250));
        assert_eq!(super::tick_hz(3_333_333), Some(300));
        assert_eq!(super::tick_hz(10_000_000), Some(100));
        // The nearest whole rate, whatever it is: this is what the clock
        // says, not a list of what kernels are built with.
        assert_eq!(super::tick_hz(2_000_000), Some(500));
        assert_eq!(super::tick_hz(1_287_001), Some(777));
        assert_eq!(super::tick_hz(999_848), Some(1000));
        // Not a tick: a high-resolution clock, nothing at all, or longer
        // than any tick, up to values a rate could not be worked out for.
        assert_eq!(super::tick_hz(1), None);
        assert_eq!(super::tick_hz(0), None);
        assert_eq!(super::tick_hz(99_999), None);
        assert_eq!(super::tick_hz(50_000_001), None);
        assert_eq!(super::tick_hz(1_000_000_000), None);
        assert_eq!(super::tick_hz(3_000_000_000), None);
        assert_eq!(super::tick_hz(u64::MAX), None);
        // The ends of what is taken.
        assert_eq!((super::tick_hz(100_000), super::tick_hz(50_000_000)), (Some(10_000), Some(20)));
        // Whatever this machine has, it is within them.
        let here = super::coarse_clock_resolution_ns().and_then(super::tick_hz);
        assert!(here.is_none_or(|hz| (20..=10_000).contains(&hz)), "{here:?}");
    }

    #[test]
    fn only_relevant_kernel_parameters_are_kept() {
        let cmdline = "BOOT_IMAGE=/vmlinuz-linux root=UUID=0a1b2c3d-1111-2222-3333-444455556666 rw \
                       cryptdevice=UUID=deadbeef:root quiet splash mitigations=off nowatchdog \
                       amd_pstate=active isolcpus=2-3 nohz_full=2-3 preempt=full threadirqs \
                       intel_idle.max_cstate=1 processor.max_cstate=1 sched_verbose resume=/dev/nvme0n1p3";
        assert_eq!(
            super::relevant_cmdline(cmdline),
            [
                "mitigations=off",
                "amd_pstate=active",
                "isolcpus=2-3",
                "nohz_full=2-3",
                "preempt=full",
                "threadirqs",
                "intel_idle.max_cstate=1",
                "processor.max_cstate=1",
                "sched_verbose"
            ]
        );
        // Nothing that identifies the machine or its disks.
        assert!(super::relevant_cmdline("root=UUID=abc rootflags=subvol=@ init=/sbin/init").is_empty());
    }

    #[test]
    fn topology_boost_and_power_are_read() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("helicoprion-topology-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let put = |rel: String, text: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        };
        // One package, 4 cores with SMT, two L3 groups of 4 CPUs; CPU 8 is
        // offline and has no topology.
        for cpu in 0..8 {
            put(format!("sys/cpu/cpu{cpu}/topology/physical_package_id"), "0\n");
            put(format!("sys/cpu/cpu{cpu}/topology/core_id"), &format!("{}\n", cpu / 2));
            put(format!("sys/cpu/cpu{cpu}/cache/index0/level"), "1\n");
            put(format!("sys/cpu/cpu{cpu}/cache/index0/shared_cpu_list"), &format!("{cpu}\n"));
            put(format!("sys/cpu/cpu{cpu}/cache/index3/level"), "3\n");
            let shared = if cpu < 4 { "0-3\n" } else { "4-7\n" };
            put(format!("sys/cpu/cpu{cpu}/cache/index3/shared_cpu_list"), shared);
        }
        put("sys/cpu/cpu8/online".into(), "0\n");
        // Not CPUs: cpufreq, cpuidle.
        put("sys/cpu/cpufreq/boost".into(), "1\n");
        put("sys/cpu/cpuidle/current_driver".into(), "acpi_idle\n");
        put("sys/node/node0/cpulist".into(), "0-7\n");
        put("sys/node/has_cpu".into(), "0\n");

        let t = super::topology(&root.join("sys")).unwrap();
        assert_eq!((t.packages, t.cores, t.l3_domains, t.numa_nodes), (1, 4, Some(2), Some(1)));
        assert_eq!(super::topology(&root.join("missing")), None);

        assert_eq!(super::boost(&root.join("sys/cpu")), Some(true));
        put("intel/intel_pstate/no_turbo".into(), "1\n");
        assert_eq!(super::boost(&root.join("intel")), Some(false));
        assert_eq!(super::boost(&root.join("missing")), None);

        // A laptop on battery; a desktop has no mains supply to ask.
        put("power/BAT0/type".into(), "Battery\n");
        assert_eq!(super::on_ac(&root.join("power")), None);
        put("power/AC/type".into(), "Mains\n");
        put("power/AC/online".into(), "0\n");
        assert_eq!(super::on_ac(&root.join("power")), Some(false));
        put("power/AC/online".into(), "1\n");
        assert_eq!(super::on_ac(&root.join("power")), Some(true));
        fs::remove_dir_all(&root).unwrap();
    }
}
