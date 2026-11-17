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
}

impl SystemInfo {
    pub fn collect() -> Self {
        let cpufreq = Path::new("/sys/devices/system/cpu/cpufreq");
        SystemInfo {
            kernel: kernel_release(),
            cpu_model: cpu_model(),
            usable_cpus: usable_cpus(),
            online_cpus: read_trimmed("/sys/devices/system/cpu/online"),
            smt_active: read_trimmed("/sys/devices/system/cpu/smt/active").map(|v| v == "1"),
            governor: across_policies(cpufreq, "scaling_governor"),
            epp: across_policies(cpufreq, "energy_performance_preference"),
            platform_profile: read_trimmed("/sys/firmware/acpi/platform_profile"),
            thp: read_trimmed("/sys/kernel/mm/transparent_hugepage/enabled").map(|s| selected_option(&s)),
            loadavg_1m: loadavg_1m(),
            temp_sensor: Probe::find().sensor().map(str::to_string),
        }
    }
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

/// `always [madvise] never` -> `madvise`
fn selected_option(s: &str) -> String {
    s.split_whitespace().find_map(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']'))).unwrap_or(s).to_string()
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
    }
}
