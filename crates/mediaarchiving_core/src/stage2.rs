//! Stage 2 — apply, verify, and original-file handling.
//!
//! # Status: interfaces only
//!
//! Nothing in this module is implemented. It exists so that the shape of the
//! destructive half of the workflow is decided *before* the safe half ships,
//! rather than being improvised later under pressure to finish.
//!
//! The types below are the contract stage 1 already produces and stage 2 will
//! consume. `Plan` (in [`crate::plan`]) names batches; a [`Manifest`] is what an
//! approval turns a batch into; a [`LedgerEntry`] is the evidence left behind.
//!
//! # Why the split matters
//!
//! Stage 1 cannot modify a file. Stage 2 can, and the failure mode — losing
//! originals irreversibly — is the one a curation tool must never have. So
//! stage 2 is gated on a verification story (see `docs/verification.md`) that
//! defines "verified" as six independent, recorded checks rather than as a
//! feeling.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What happens to the source file once its output is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalsMode {
    /// Delete the source as soon as the output exists. Not recommended, and
    /// offered only because some users genuinely want it for disposable data.
    Replace,
    /// Delete the source only after it passes verification.
    VerifyThenReplace,
    /// Pack sources into an archive; nothing is deleted.
    Archive,
    /// Pack sources into an encrypted archive, with the key stored elsewhere.
    ArchiveEncrypted,
}

impl OriginalsMode {
    /// Whether this mode can destroy data.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            OriginalsMode::Replace | OriginalsMode::VerifyThenReplace
        )
    }
}

/// One file's worth of intended work, produced from a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// BLAKE3 of the source as recorded in the inventory.
    pub source_hash: String,
    pub source_bytes: u64,
    pub target_container: String,
    /// The risk tier carried over from planning; verification depth follows it.
    pub risk: crate::analysis::RiskTier,
}

/// A batch that has been approved for execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub batch_id: String,
    pub jobs: Vec<Job>,
    pub originals: OriginalsMode,
    /// Where outputs and archives are written. Never inside a scanned root
    /// unless the user explicitly chose that.
    pub staging: PathBuf,
    pub approved_at: String,
}

/// Result of the six verification checks for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub decoded: bool,
    pub dimensions_match: bool,
    /// `None` when the policy chose not to measure pixels.
    pub ssim: Option<f64>,
    pub icc_preserved: bool,
    pub orientation_preserved: bool,
    pub source_hash_matches: bool,
}

impl Verification {
    /// A file may only be released when every check that ran passed.
    pub fn passed(&self) -> bool {
        self.decoded
            && self.dimensions_match
            && self.icc_preserved
            && self.orientation_preserved
            && self.source_hash_matches
            && self.ssim.is_none_or(|score| score >= SSIM_THRESHOLD)
    }
}

/// Default SSIM floor for lossy output. Matches `docs/verification.md`.
pub const SSIM_THRESHOLD: f64 = 0.98;

/// Append-only record of what was done, so any deletion can be justified later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub at: String,
    pub batch_id: String,
    pub source: PathBuf,
    pub source_hash: String,
    pub output: PathBuf,
    pub output_hash: String,
    pub source_bytes: u64,
    pub output_bytes: u64,
    pub verification: Verification,
    /// What was done to the source after verification.
    pub originals_action: OriginalsMode,
    /// Recovery key fingerprint, when the originals went into an encrypted
    /// archive. Never the key itself.
    pub key_fingerprint: Option<String>,
}

/// Placeholder for the apply stage.
///
/// Deliberately returns an error rather than a no-op: a caller that believes
/// it ran apply must not silently succeed.
pub fn apply(_manifest: &Manifest) -> anyhow::Result<()> {
    anyhow::bail!(
        "apply is part of stage 2 and is not implemented yet; \
         stage 1 only plans and reports"
    )
}
