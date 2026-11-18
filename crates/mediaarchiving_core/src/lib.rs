//! MediaArchiving — scan, analyse and plan the curation of large media libraries.
//!
//! The workflow is deliberately split into stages that can each be interrupted
//! and resumed, because a real library takes hours to walk and the human
//! decisions in the middle may happen days later:
//!
//! ```text
//! scan → analyse → plan → report      (this crate, stage 1)
//!   ↓
//! apply → verify → originals          (stage 2, interfaces only for now)
//! ```
//!
//! Every stage reads and writes a "run directory" — a folder holding the
//! inventory database, the policy, the plan and the report. Nothing outside
//! the run directory is ever modified by stage 1.

pub mod analysis;
pub mod inventory;
pub mod plan;
pub mod policy;
pub mod probe;
pub mod report;
pub mod scan;
/// Stage 2 — apply, verify and original handling. Interfaces only.
pub mod stage2;

pub use analysis::{analyse, Analysis, Decision, RiskTier};
pub use inventory::Inventory;
pub use plan::{build_plan, Batch, Plan};
pub use policy::Policy;
pub use probe::{MediaKind, Probe};
pub use scan::{scan, ScanOptions, ScanSummary};

use std::path::{Path, PathBuf};

/// Layout of a run directory.
///
/// ```text
/// <run>/
///   inventory.sqlite   file list + hashes + probe results
///   policy.toml        user decisions, meant to be reused across runs
///   analysis.json      per-file verdict
///   plan.json          batch manifest
///   report.html        the human-readable report
/// ```
#[derive(Debug, Clone)]
pub struct RunDir {
    pub root: PathBuf,
}

impl RunDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn inventory(&self) -> PathBuf {
        self.root.join("inventory.sqlite")
    }
    pub fn policy(&self) -> PathBuf {
        self.root.join("policy.toml")
    }
    pub fn analysis(&self) -> PathBuf {
        self.root.join("analysis.json")
    }
    pub fn plan(&self) -> PathBuf {
        self.root.join("plan.json")
    }
    pub fn report(&self) -> PathBuf {
        self.root.join("report.html")
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        Ok(())
    }
}

/// Human-readable byte size, e.g. `1.4 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Relative path for display, falling back to the absolute path when the file
/// is not under `root`.
pub fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}
