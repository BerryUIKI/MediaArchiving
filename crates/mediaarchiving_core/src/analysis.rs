//! Stage 1b — decide, per file, what should happen to it.
//!
//! Nothing here touches the disk. The output is a verdict per file plus an
//! *estimated* saving, so the user can see the size of the prize before any
//! byte is rewritten.
//!
//! ## Why savings are estimated
//!
//! Knowing the real saving means actually encoding the file, which for 90k
//! images is the same cost as doing the work. Instead we estimate from
//! **bits per pixel** — total bytes divided by pixel count — which tracks
//! source quality closely enough to make the go/no-go decision:
//!
//! | bits/px | typical source                | expected AVIF saving |
//! |---------|-------------------------------|----------------------|
//! | > 2.0   | near-lossless / camera        | ~65%                 |
//! | 1.0-2.0 | decent web quality            | ~50%                 |
//! | 0.5-1.0 | already re-compressed         | ~30%                 |
//! | < 0.5   | heavily re-compressed, small  | ~10% (not worth it)  |
//!
//! The estimate is deliberately conservative: a file that looks like it will
//! save 10% is reported as a *skip*, because a re-encode that does not pay for
//! its own risk is a bad trade.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::inventory::FileRecord;
use crate::policy::{Intent, Policy};
use crate::probe::MediaKind;

/// What the workflow proposes to do with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Re-encode to the policy's target format.
    Transcode,
    /// Leave the pixels alone; candidate for packing into an archive.
    ArchiveOnly,
    /// Nothing to gain; leave it entirely alone.
    Skip,
    /// Cannot be planned safely until something is fixed.
    Blocked,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Transcode => "transcode",
            Decision::ArchiveOnly => "archive_only",
            Decision::Skip => "skip",
            Decision::Blocked => "blocked",
        }
    }
}

/// How much human attention a file needs before its original may be released.
///
/// The tiers exist to make review batches tractable: 90k files cannot be
/// looked at one by one, but a few thousand "high risk" ones can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    Low,
    Medium,
    High,
}

impl RiskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskTier::Low => "low",
            RiskTier::Medium => "medium",
            RiskTier::High => "high",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileVerdict {
    pub path: String,
    pub size: u64,
    pub kind: MediaKind,
    pub container: String,
    pub decision: Decision,
    pub risk: RiskTier,
    /// Estimated bytes after transcoding; `None` when we refuse to guess.
    pub estimated_size: Option<u64>,
    /// Estimated bytes freed (0 when not transcoding).
    pub estimated_saving: u64,
    /// Machine-readable reasons, rendered in the report.
    pub reasons: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub dup_group: Option<i64>,
    pub is_keeper: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub intent: Intent,
    pub files: Vec<FileVerdict>,
    pub totals: Totals,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub files: usize,
    pub total_bytes: u64,
    pub transcodable_files: usize,
    pub transcodable_bytes: u64,
    pub estimated_saving_bytes: u64,
    pub redundant_bytes: u64,
    pub blocked_files: usize,
    pub by_container: HashMap<String, ContainerTotal>,
    pub by_risk: HashMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContainerTotal {
    pub files: usize,
    pub bytes: u64,
    pub saving_bytes: u64,
    pub transcodable: usize,
}

/// Analyse the whole inventory against `policy`.
pub fn analyse(records: &[FileRecord], policy: &Policy) -> Analysis {
    let files: Vec<FileVerdict> = records.iter().map(|r| verdict_for(r, policy)).collect();

    let mut totals = Totals {
        files: files.len(),
        ..Totals::default()
    };

    for verdict in &files {
        totals.total_bytes += verdict.size;
        if verdict.decision == Decision::Transcode {
            totals.transcodable_files += 1;
            totals.transcodable_bytes += verdict.size;
            totals.estimated_saving_bytes += verdict.estimated_saving;
        }
        if verdict.decision == Decision::Blocked {
            totals.blocked_files += 1;
        }
        if verdict.dup_group.is_some() && !verdict.is_keeper {
            totals.redundant_bytes += verdict.size;
        }

        let entry = totals
            .by_container
            .entry(verdict.container.clone())
            .or_default();
        entry.files += 1;
        entry.bytes += verdict.size;
        if verdict.decision == Decision::Transcode {
            entry.transcodable += 1;
            entry.saving_bytes += verdict.estimated_saving;
        }

        *totals
            .by_risk
            .entry(verdict.risk.as_str().to_string())
            .or_insert(0) += 1;
    }

    Analysis {
        intent: policy.intent,
        files,
        totals,
    }
}

fn verdict_for(record: &FileRecord, policy: &Policy) -> FileVerdict {
    let mut reasons = Vec::new();
    let mut verdict = FileVerdict {
        path: record.path.to_string_lossy().replace('\\', "/"),
        size: record.size,
        kind: record.kind,
        container: record.container.clone(),
        decision: Decision::Skip,
        risk: RiskTier::Low,
        estimated_size: None,
        estimated_saving: 0,
        reasons: Vec::new(),
        width: record.width,
        height: record.height,
        duration_ms: record.duration_ms,
        dup_group: record.dup_group,
        is_keeper: record.is_keeper,
    };

    // --- Hard blockers: fix the file before planning anything ---
    if let Some(error) = &record.error {
        verdict.decision = Decision::Blocked;
        reasons.push(format!("unreadable header: {error}"));
    }
    if record.extension_mismatch {
        verdict.decision = Decision::Blocked;
        reasons.push(format!(
            "extension does not match content ({})",
            record.container
        ));
    }

    // --- Duplicates: never plan work on a redundant copy ---
    if verdict.decision != Decision::Blocked && record.dup_group.is_some() {
        if record.is_keeper {
            reasons.push("keeper of a duplicate group".to_string());
        } else {
            verdict.decision = Decision::Blocked;
            reasons.push("redundant duplicate copy — resolve first".to_string());
            verdict.risk = RiskTier::Low;
            return finish(verdict, reasons);
        }
    }

    if verdict.decision == Decision::Blocked {
        return finish(verdict, reasons);
    }

    // --- Rules that apply to every kind ---
    if record.size < policy.skip.min_bytes {
        reasons.push("below minimum size".to_string());
        return finish(verdict, reasons);
    }

    verdict.risk = risk_tier(record, &mut reasons);

    match record.kind {
        MediaKind::Image => analyse_image(record, policy, &mut verdict, &mut reasons),
        MediaKind::Video => analyse_video(record, policy, &mut verdict, &mut reasons),
        MediaKind::Archive => {
            verdict.decision = Decision::Skip;
            reasons.push("already an archive container".to_string());
        }
        MediaKind::Audio => {
            verdict.decision = Decision::Skip;
            reasons.push("audio transcoding not implemented in stage 1".to_string());
        }
        MediaKind::Other => {
            verdict.decision = Decision::Skip;
            reasons.push("not recognised as media".to_string());
        }
    }

    finish(verdict, reasons)
}

fn analyse_image(
    record: &FileRecord,
    policy: &Policy,
    verdict: &mut FileVerdict,
    reasons: &mut Vec<String>,
) {
    // Dataset intent: pixels are the data, so lossy re-encoding is refused
    // outright regardless of how much space it would save.
    if !policy.intent.allows_lossy() {
        verdict.decision = Decision::ArchiveOnly;
        reasons.push("dataset intent: lossy re-encoding refused".to_string());
        return;
    }

    if !policy.considers_image(&record.container) {
        if record.container == policy.image.target {
            reasons.push(format!("already {}", record.container));
        } else {
            reasons.push("format excluded by policy".to_string());
        }
        return;
    }

    let Some(pixels) = record.pixels() else {
        verdict.decision = Decision::Blocked;
        reasons.push("image dimensions could not be read".to_string());
        return;
    };
    if pixels < policy.skip.min_pixels {
        reasons.push(format!("below minimum pixel count ({pixels})"));
        return;
    }

    let Some(ratio) = estimated_saving_ratio(&record.container, record.size, pixels) else {
        reasons.push("no savings model for this format".to_string());
        return;
    };
    let estimated_size = (record.size as f64 * (1.0 - ratio)) as u64;
    let saving = record.size.saturating_sub(estimated_size);
    let saving_pct = if record.size > 0 {
        (saving as f64 / record.size as f64 * 100.0) as u32
    } else {
        0
    };

    if saving_pct < policy.image.min_savings_pct {
        reasons.push(format!(
            "estimated saving {saving_pct}% below threshold {}%",
            policy.image.min_savings_pct
        ));
        return;
    }

    verdict.decision = Decision::Transcode;
    verdict.estimated_size = Some(estimated_size);
    verdict.estimated_saving = saving;
    reasons.push(format!(
        "{} → {}, estimated {saving_pct}% smaller",
        record.container, policy.image.target
    ));
}

fn analyse_video(
    record: &FileRecord,
    policy: &Policy,
    verdict: &mut FileVerdict,
    reasons: &mut Vec<String>,
) {
    if !policy.intent.allows_lossy() {
        verdict.decision = Decision::ArchiveOnly;
        reasons.push("dataset intent: lossy re-encoding refused".to_string());
        return;
    }
    if !policy.considers_video(&record.container) {
        reasons.push("codec excluded by policy".to_string());
        return;
    }

    let ratio = match record.container.as_str() {
        "mp4" => 0.40,
        "avi" | "matroska" | "riff" => 0.65,
        _ => 0.35,
    };
    let estimated_size = (record.size as f64 * (1.0 - ratio)) as u64;
    let saving = record.size.saturating_sub(estimated_size);
    let saving_pct = (ratio * 100.0) as u32;

    if saving_pct < policy.video.min_savings_pct {
        reasons.push(format!(
            "estimated saving {saving_pct}% below threshold {}%",
            policy.video.min_savings_pct
        ));
        return;
    }

    verdict.decision = Decision::Transcode;
    verdict.estimated_size = Some(estimated_size);
    verdict.estimated_saving = saving;
    reasons.push(format!(
        "{} → {}, estimated {saving_pct}% smaller",
        record.container, policy.video.target
    ));
}

/// Risk is about *what could be lost*, not about file size.
fn risk_tier(record: &FileRecord, reasons: &mut Vec<String>) -> RiskTier {
    let mut tier = RiskTier::Low;

    if record.has_icc {
        tier = tier.max(RiskTier::High);
        reasons.push("embedded colour profile — must survive re-encode".to_string());
    }
    if let Some(orientation) = record.orientation {
        if orientation != 1 {
            tier = tier.max(RiskTier::High);
            reasons.push(format!("EXIF orientation {orientation} — rotation risk"));
        }
    }
    if record.has_xmp {
        tier = tier.max(RiskTier::Medium);
        reasons.push("has XMP metadata".to_string());
    }
    if record.has_exif && !record.has_icc {
        tier = tier.max(RiskTier::Medium);
    }
    // Large files cost the most to redo if something goes wrong.
    if record.size >= 100 * 1024 * 1024 {
        tier = tier.max(RiskTier::High);
        reasons.push("large file — expensive to redo".to_string());
    }

    tier
}

/// Estimated fractional saving for re-encoding `container` at `size` bytes and
/// `pixels` pixels to the policy target.
///
/// Returns `None` for containers we have no model for.
fn estimated_saving_ratio(container: &str, size: u64, pixels: u64) -> Option<f64> {
    if pixels == 0 {
        return None;
    }
    let bits_per_pixel = size as f64 * 8.0 / pixels as f64;

    let ratio = match container {
        // Lossy sources: the saving is inversely related to how hard the
        // source was already compressed.
        "jpeg" => {
            if bits_per_pixel > 2.0 {
                0.65
            } else if bits_per_pixel > 1.0 {
                0.50
            } else if bits_per_pixel > 0.5 {
                0.30
            } else {
                0.10
            }
        }
        // Lossless sources: a photo-like PNG compresses far better as AVIF.
        "png" => {
            if bits_per_pixel > 8.0 {
                0.80
            } else if bits_per_pixel > 3.0 {
                0.70
            } else {
                0.45
            }
        }
        "bmp" | "tiff" => 0.90,
        "gif" => 0.50,
        "heic" => 0.25,
        _ => return None,
    };
    Some(ratio)
}

fn finish(mut verdict: FileVerdict, reasons: Vec<String>) -> FileVerdict {
    verdict.reasons = reasons;
    verdict
}
