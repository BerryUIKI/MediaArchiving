//! The policy file: every human decision, written down once and reused.
//!
//! The point of a policy file rather than interactive prompts is that a real
//! library has ~90k files. Decisions are made *about classes of files*, not
//! about individual files, and the same policy should apply to the next batch
//! without being re-answered. The file is heavily commented so it doubles as
//! the documentation for what each knob does.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What the library is *for*. This changes defaults more than any other knob:
/// a dataset must not be lossily re-encoded, while an archive wants the
/// smallest safe output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Long-term storage of personal media. Lossy transcoding allowed.
    Archive,
    /// Machine-learning corpus. Lossy transcoding is refused; the pixels are
    /// the data.
    Dataset,
    /// Will be sent to other people. Compatibility beats file size.
    Sharing,
}

impl Intent {
    pub fn allows_lossy(self) -> bool {
        !matches!(self, Intent::Dataset)
    }

    pub fn label(self) -> &'static str {
        match self {
            Intent::Archive => "archive",
            Intent::Dataset => "dataset",
            Intent::Sharing => "sharing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub intent: Intent,
    pub image: ImagePolicy,
    pub video: VideoPolicy,
    pub skip: SkipPolicy,
    pub metadata: MetadataPolicy,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            intent: Intent::Archive,
            image: ImagePolicy::default(),
            video: VideoPolicy::default(),
            skip: SkipPolicy::default(),
            metadata: MetadataPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImagePolicy {
    /// `avif`, `webp`, `jxl`, or `keep` to leave images untouched.
    pub target: String,
    /// Encoder quality, 0-100. Meaning is encoder-specific.
    pub quality: u32,
    /// Skip files that would save less than this percentage.
    pub min_savings_pct: u32,
    /// `none`, or `max:<long-edge>` to cap the long edge.
    pub resize: String,
}

impl Default for ImagePolicy {
    fn default() -> Self {
        Self {
            target: "avif".to_string(),
            quality: 60,
            min_savings_pct: 15,
            resize: "none".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoPolicy {
    /// `hevc`, `av1`, or `keep`.
    pub target: String,
    pub crf: u32,
    /// `nvenc`, `cpu`, or `auto`.
    pub hardware: String,
    pub min_savings_pct: u32,
}

impl Default for VideoPolicy {
    fn default() -> Self {
        Self {
            target: "hevc".to_string(),
            crf: 24,
            hardware: "auto".to_string(),
            min_savings_pct: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkipPolicy {
    /// Containers that are already efficiently compressed.
    pub already_compressed: Vec<String>,
    /// Images smaller than this many pixels are left alone.
    pub min_pixels: u64,
    /// Files smaller than this are left alone.
    pub min_bytes: u64,
}

impl Default for SkipPolicy {
    fn default() -> Self {
        Self {
            already_compressed: vec![
                "avif".to_string(),
                "webp".to_string(),
                "jxl".to_string(),
                "heic".to_string(),
            ],
            min_pixels: 200 * 200,
            min_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataPolicy {
    /// Keep the EXIF block. Costs a little size but preserves capture data.
    pub keep_exif: bool,
    /// Keep the embedded colour profile. Losing this shifts colours.
    pub keep_icc: bool,
    /// Bake the EXIF orientation into the pixels instead of carrying the tag.
    pub bake_rotation: bool,
}

impl Default for MetadataPolicy {
    fn default() -> Self {
        Self {
            keep_exif: true,
            keep_icc: true,
            bake_rotation: true,
        }
    }
}

/// Template written to a fresh run directory. The comments are the manual.
pub const TEMPLATE: &str = r#"# MediaArchiving policy
#
# Every decision about what may be changed lives in this file. Edit it, re-run
# `ma plan`, and the dry-run output updates. The same policy applies to every
# future run unless you point a run at a different file.

# What this library is for. This is the most important setting:
#   archive — long-term personal storage; lossy transcoding allowed
#   dataset — training corpus; lossy transcoding REFUSED (pixels are the data)
#   sharing — will be shared; compatibility beats file size
intent = "archive"

[image]
# Target format: avif | webp | jxl | keep
#   avif — saves the most; slowest to encode; older software may not open it
#   webp — broad compatibility, saves less than avif
#   jxl  — lossless-capable and future-facing, but poor browser support today
target = "avif"
quality = 60
# Leave a file alone unless re-encoding saves at least this much.
min_savings_pct = 15
# "none", or "max:<long edge in pixels>" e.g. "max:2560"
resize = "none"

[video]
# Target codec: hevc | av1 | keep
target = "hevc"
crf = 24
# nvenc | cpu | auto
hardware = "auto"
min_savings_pct = 20

[skip]
# Containers that are already efficiently compressed.
already_compressed = ["avif", "webp", "jxl", "heic"]
# Ignore images below this pixel count (thumbnails, icons).
min_pixels = 40000
# Ignore files below this size in bytes.
min_bytes = 16384

[metadata]
keep_exif = true
keep_icc = true
bake_rotation = true
"#;

impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading policy at {}", path.display()))?;
        let policy: Policy =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(policy)
    }

    /// Write the commented template if the file does not exist yet.
    pub fn ensure_template(path: &Path) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        std::fs::write(path, TEMPLATE)?;
        Ok(true)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// True when an image in `container` should be considered for transcoding.
    pub fn considers_image(&self, container: &str) -> bool {
        if self.image.target == "keep" {
            return false;
        }
        if self.image.target == container {
            return false;
        }
        !self
            .skip
            .already_compressed
            .iter()
            .any(|c| c.eq_ignore_ascii_case(container))
    }

    pub fn considers_video(&self, container: &str) -> bool {
        if self.video.target == "keep" {
            return false;
        }
        // HVEC/AV1 sources are already efficient; only consider older codecs.
        !matches!(container, "hevc" | "av1")
    }
}
