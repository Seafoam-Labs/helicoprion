//! Machine state that affects results and must travel with them.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::read_trimmed;
use crate::sched::kernel_release;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            loadavg_1m: read_trimmed("/proc/loadavg")
                .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok())),
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
        if entry.file_name().to_string_lossy().starts_with("policy") {
            if let Some(v) = read_trimmed(entry.path().join(file)) {
                *counts.entry(v).or_default() += 1;
            }
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
    fn thp_selected() {
        assert_eq!(super::selected_option("always [madvise] never"), "madvise");
        assert_eq!(super::selected_option("weird"), "weird");
    }
}
