//! Stage 1c — turn the analysis into reviewable batches.
//!
//! Batch composition is deliberately two-dimensional, per the agreed design:
//!
//! 1. **Layering by risk first.** Low-risk work is the bulk and can be approved
//!    in one sweep; high-risk work is what a human actually needs to look at.
//!    Mixing them means either reviewing 90k files or rubber-stamping the
//!    dangerous ones.
//! 2. **Then splitting by directory.** Inside each risk tier, files are packed
//!    by their parent directory so a batch reads as "this figure set" rather
//!    than "these 400 unrelated paths". It also keeps a review batch from
//!    scattering across the tree.
//!
//! Batch size is capped so a single review decision never covers an
//! unreasonable amount of work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::analysis::{Analysis, Decision, FileVerdict, RiskTier};

/// Maximum bytes a single batch may cover before it is split.
const MAX_BATCH_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
/// Maximum files a single batch may contain.
const MAX_BATCH_FILES: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    pub risk: RiskTier,
    /// Common directory prefix, for display.
    pub root: String,
    pub files: usize,
    pub bytes: u64,
    pub estimated_saving: u64,
    /// File paths in this batch, sorted for stable output.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub intent: String,
    pub batches: Vec<Batch>,
    pub totals: PlanTotals,
    /// Files that cannot be planned until something is fixed.
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTotals {
    pub batches: usize,
    pub files: usize,
    pub bytes: u64,
    pub estimated_saving: u64,
}

/// Group the transcode candidates into review batches.
pub fn build_plan(analysis: &Analysis) -> Plan {
    // --- Layer 1: risk tier. Layer 2: parent directory. ---
    let mut buckets: BTreeMap<(u8, String), Vec<&FileVerdict>> = BTreeMap::new();

    let mut blocked = Vec::new();

    for verdict in &analysis.files {
        match verdict.decision {
            Decision::Transcode => {
                let dir = parent_dir(&verdict.path);
                buckets
                    .entry((risk_rank(verdict.risk), dir))
                    .or_default()
                    .push(verdict);
            }
            Decision::Blocked => blocked.push(verdict.path.clone()),
            _ => {}
        }
    }

    let mut batches = Vec::new();
    for ((rank, dir), files) in buckets {
        let risk = rank_to_risk(rank);
        // Split oversized buckets along the same directory order so batch
        // boundaries stay predictable between runs.
        let mut current: Vec<&FileVerdict> = Vec::new();
        let mut current_bytes = 0u64;
        let mut part = 0usize;

        let flush = |batch_files: &mut Vec<&FileVerdict>,
                         bytes: u64,
                         part: &mut usize,
                         batches: &mut Vec<Batch>| {
            if batch_files.is_empty() {
                return;
            }
            *part += 1;
            let saving = batch_files.iter().map(|f| f.estimated_saving).sum();
            let mut paths: Vec<String> = batch_files.iter().map(|f| f.path.clone()).collect();
            paths.sort();
            batches.push(Batch {
                id: format!("{}-{}-{:03}", risk.as_str(), sanitise(&dir), part),
                risk,
                root: dir.clone(),
                files: batch_files.len(),
                bytes,
                estimated_saving: saving,
                paths,
            });
            batch_files.clear();
        };

        for file in files {
            current.push(file);
            current_bytes += file.size;
            if current.len() >= MAX_BATCH_FILES || current_bytes >= MAX_BATCH_BYTES {
                flush(&mut current, current_bytes, &mut part, &mut batches);
                current_bytes = 0;
            }
        }
        flush(&mut current, current_bytes, &mut part, &mut batches);
    }

    // High risk first: that is the order a reviewer should work in.
    batches.sort_by(|a, b| {
        risk_rank(b.risk)
            .cmp(&risk_rank(a.risk))
            .then_with(|| a.id.cmp(&b.id))
    });

    let totals = PlanTotals {
        batches: batches.len(),
        files: batches.iter().map(|b| b.files).sum(),
        bytes: batches.iter().map(|b| b.bytes).sum(),
        estimated_saving: batches.iter().map(|b| b.estimated_saving).sum(),
    };

    Plan {
        intent: analysis.intent.label().to_string(),
        batches,
        totals,
        blocked,
    }
}

fn risk_rank(risk: RiskTier) -> u8 {
    match risk {
        RiskTier::Low => 0,
        RiskTier::Medium => 1,
        RiskTier::High => 2,
    }
}

fn rank_to_risk(rank: u8) -> RiskTier {
    match rank {
        0 => RiskTier::Low,
        1 => RiskTier::Medium,
        _ => RiskTier::High,
    }
}

/// Parent directory of a `/`-separated display path.
fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => String::new(),
    }
}

/// Keep batch ids readable and filename-safe.
fn sanitise(dir: &str) -> String {
    let tail = dir.rsplit('/').next().unwrap_or(dir);
    let cleaned: String = tail
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.chars().take(24).collect()
    }
}
