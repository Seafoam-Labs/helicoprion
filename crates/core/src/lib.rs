//! Core types for helicoprion: which scheduler is running, what machine
//! we are on, and the on-disk result format.

pub mod hist;
pub mod result;
pub mod sched;
pub mod system;

pub use result::{Better, Metric, ResultFile, RunResult, SCHEMA_VERSION, Window, WorkloadResult};
pub use sched::Scheduler;
pub use system::SystemInfo;

use std::fs;
use std::path::Path;

/// Read a sysfs/procfs file and trim it. `None` if missing, unreadable or empty.
pub(crate) fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}
