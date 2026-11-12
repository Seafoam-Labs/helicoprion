//! Detect which CPU scheduler is driving SCHED_NORMAL tasks right now.
//!
//! * no `/sys/kernel/sched_ext`            -> fair class, kernel without sched_ext
//! * `/sys/kernel/sched_ext/state` != enabled -> fair class, sched_ext idle
//! * enabled + `root/ops`                  -> a BPF scheduler is attached
//!
//! For the fair class we only know it is *not* sched_ext. Its name is
//! inferred from the kernel version (EEVDF since 6.6, CFS before), which a
//! downstream patch set can make wrong; BORE is detected positively via
//! `/proc/sys/kernel/sched_bore`.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::read_trimmed;

const SCX_ROOT: &str = "/sys/kernel/sched_ext";
const BORE_SYSCTL: &str = "/proc/sys/kernel/sched_bore";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scheduler {
    /// The in-kernel fair class (EEVDF or, on old kernels, CFS).
    Fair {
        /// "EEVDF" or "CFS", *inferred* from the kernel version only.
        class: String,
        /// Kernel has sched_ext support but no BPF scheduler is attached.
        scx_capable: bool,
        /// `Some(enabled)` when the kernel carries the BORE patch.
        bore: Option<bool>,
    },
    /// A sched_ext BPF scheduler is attached.
    Ext {
        /// Short name, e.g. `lavd` (from `lavd_1.0.14_g…_x86_64…`).
        name: String,
        /// Raw contents of `/sys/kernel/sched_ext/root/ops`.
        ops: String,
        /// `/sys/kernel/sched_ext/enable_seq`; bumps on every attach.
        enable_seq: Option<u64>,
    },
}

impl Scheduler {
    pub fn detect() -> Self {
        Self::detect_at(Path::new(SCX_ROOT), Path::new(BORE_SYSCTL), &kernel_release())
    }

    /// Detection against arbitrary paths, so it can be tested without a live kernel.
    pub fn detect_at(scx_root: &Path, bore_sysctl: &Path, kernel_release: &str) -> Self {
        let fair = |scx_capable: bool| Scheduler::Fair {
            class: fair_class_name(kernel_release).to_string(),
            scx_capable,
            bore: read_trimmed(bore_sysctl).map(|v| v != "0"),
        };

        if !scx_root.is_dir() {
            return fair(false);
        }

        let ops = read_trimmed(scx_root.join("root/ops"));
        // Older sched_ext kernels lack `state`; fall back to "ops is non-empty".
        let enabled = match read_trimmed(scx_root.join("state")) {
            Some(state) => state == "enabled",
            None => ops.is_some(),
        };

        match (enabled, ops) {
            (true, Some(ops)) => Scheduler::Ext {
                name: short_ops_name(&ops).to_string(),
                enable_seq: read_trimmed(scx_root.join("enable_seq")).and_then(|s| s.parse().ok()),
                ops,
            },
            _ => fair(true),
        }
    }

    /// Whether a crash-and-restart of the same scheduler would be visible.
    /// Without `enable_seq` an identical ops string before and after a run
    /// cannot rule out a restart in between.
    pub fn restart_detectable(&self) -> bool {
        match self {
            Scheduler::Fair { .. } => true,
            Scheduler::Ext { enable_seq, .. } => enable_seq.is_some(),
        }
    }

    pub fn is_ext(&self) -> bool {
        matches!(self, Scheduler::Ext { .. })
    }

    /// Human label: `EEVDF`, `EEVDF+BORE`, `scx_lavd`.
    pub fn label(&self) -> String {
        match self {
            Scheduler::Fair { class, bore: Some(true), .. } => format!("{class}+BORE"),
            Scheduler::Fair { class, .. } => class.clone(),
            Scheduler::Ext { name, .. } if name.starts_with("scx_") => name.clone(),
            Scheduler::Ext { name, .. } => format!("scx_{name}"),
        }
    }
}

impl fmt::Display for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scheduler::Fair { scx_capable, .. } => {
                let scx = if *scx_capable { "sched_ext available, inactive" } else { "no sched_ext" };
                write!(f, "{} (fair class, name inferred from kernel version; {scx})", self.label())
            }
            Scheduler::Ext { ops, enable_seq, .. } => {
                write!(f, "{} (sched_ext, ops: {ops}", self.label())?;
                if let Some(seq) = enable_seq {
                    write!(f, ", enable_seq: {seq}")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// Strip the version/build suffix scx schedulers append to their ops name:
/// everything from the first `_` that is followed by a digit.
pub fn short_ops_name(ops: &str) -> &str {
    ops.match_indices('_')
        .find(|(i, _)| ops[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .map_or(ops, |(i, _)| &ops[..i])
}

pub fn kernel_release() -> String {
    read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into())
}

fn fair_class_name(release: &str) -> &'static str {
    let mut it = release.split(|c: char| !c.is_ascii_digit()).filter_map(|s| s.parse::<u32>().ok());
    match (it.next(), it.next()) {
        (Some(major), Some(minor)) if (major, minor) < (6, 6) => "CFS",
        _ => "EEVDF",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("helicoprion-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn short_names() {
        assert_eq!(short_ops_name("lavd_1.0.14_g6dc9f5a_x86_64_unknown_linux_gnu"), "lavd");
        assert_eq!(short_ops_name("bpfland_1.0.3"), "bpfland");
        assert_eq!(short_ops_name("rusty_lb"), "rusty_lb");
        assert_eq!(short_ops_name("simple"), "simple");
    }

    #[test]
    fn fair_class_by_version() {
        assert_eq!(fair_class_name("6.18.4-devario"), "EEVDF");
        assert_eq!(fair_class_name("6.6.0"), "EEVDF");
        assert_eq!(fair_class_name("6.5.13-arch1"), "CFS");
        assert_eq!(fair_class_name("garbage"), "EEVDF");
    }

    #[test]
    fn no_sched_ext() {
        let t = Tmp::new("none");
        let s = Scheduler::detect_at(&t.0.join("missing"), &t.0.join("nobore"), "6.18.0");
        assert_eq!(s, Scheduler::Fair { class: "EEVDF".into(), scx_capable: false, bore: None });
        assert_eq!(s.label(), "EEVDF");
    }

    #[test]
    fn sched_ext_idle_with_bore() {
        let t = Tmp::new("idle");
        let scx = t.0.join("sched_ext");
        fs::create_dir_all(scx.join("root")).unwrap();
        fs::write(scx.join("state"), "disabled\n").unwrap();
        fs::write(scx.join("root/ops"), "\n").unwrap();
        fs::write(t.0.join("bore"), "1\n").unwrap();
        let s = Scheduler::detect_at(&scx, &t.0.join("bore"), "6.18.0");
        assert!(!s.is_ext());
        assert_eq!(s.label(), "EEVDF+BORE");
    }

    #[test]
    fn sched_ext_enabled() {
        let t = Tmp::new("ext");
        let scx = t.0.join("sched_ext");
        fs::create_dir_all(scx.join("root")).unwrap();
        fs::write(scx.join("state"), "enabled\n").unwrap();
        fs::write(scx.join("root/ops"), "lavd_1.0.14_g6dc9f5a_x86_64\n").unwrap();
        fs::write(scx.join("enable_seq"), "7\n").unwrap();
        let s = Scheduler::detect_at(&scx, &t.0.join("nobore"), "6.18.0");
        assert_eq!(
            s,
            Scheduler::Ext { name: "lavd".into(), ops: "lavd_1.0.14_g6dc9f5a_x86_64".into(), enable_seq: Some(7) }
        );
        assert_eq!(s.label(), "scx_lavd");
        assert!(s.restart_detectable());
    }

    #[test]
    fn restart_undetectable_without_enable_seq() {
        let t = Tmp::new("noseq");
        let scx = t.0.join("sched_ext");
        fs::create_dir_all(scx.join("root")).unwrap();
        fs::write(scx.join("state"), "enabled\n").unwrap();
        fs::write(scx.join("root/ops"), "rusty_1.0.0\n").unwrap();
        let s = Scheduler::detect_at(&scx, &t.0.join("nobore"), "6.12.0");
        assert!(s.is_ext());
        assert!(!s.restart_detectable());
    }
}
