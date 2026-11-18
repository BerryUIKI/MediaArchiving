//! Cheap media sniffing: kind, dimensions and metadata flags.
//!
//! Stage 1 never decodes pixels. Everything here is header-level, so probing
//! 90k files over SMB stays in the "minutes" range rather than hours.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Broad media class, derived from the *container magic* rather than the file
/// extension — libraries are full of mislabelled files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Archive,
    Other,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Archive => "archive",
            MediaKind::Other => "other",
        }
    }

    pub fn from_str_opt(text: &str) -> Option<Self> {
        Some(match text {
            "image" => MediaKind::Image,
            "video" => MediaKind::Video,
            "audio" => MediaKind::Audio,
            "archive" => MediaKind::Archive,
            "other" => MediaKind::Other,
            _ => return None,
        })
    }

    /// Whether the transcoding stage could apply to this kind at all.
    pub fn is_transcodable(self) -> bool {
        matches!(self, MediaKind::Image | MediaKind::Video | MediaKind::Audio)
    }
}

/// What we learned from the file header.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Probe {
    /// Container actually detected from magic bytes, e.g. `jpeg`, `png`.
    pub container: String,
    pub kind: MediaKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub has_exif: bool,
    pub has_icc: bool,
    pub has_xmp: bool,
    /// EXIF orientation tag, 1 = normal, 2-8 = needs transform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<u16>,
    /// True when the extension does not match the detected container.
    pub extension_mismatch: bool,
    /// Set when the header could not be read; the file is quarantined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for MediaKind {
    fn default() -> Self {
        MediaKind::Other
    }
}

/// Probe a file. Never fails: read errors are recorded in `Probe::error` so a
/// single unreadable file cannot abort a 90k-file scan.
pub fn probe(path: &Path) -> Probe {
    match probe_inner(path) {
        Ok(mut probe) => {
            probe.extension_mismatch = extension_mismatch(path, &probe.container);
            probe
        }
        Err(err) => Probe {
            kind: MediaKind::Other,
            container: "unknown".to_string(),
            error: Some(err.to_string()),
            ..Probe::default()
        },
    }
}

fn probe_inner(path: &Path) -> anyhow::Result<Probe> {
    let mut file = File::open(path)?;

    // 32 bytes is enough for every signature we recognise.
    let mut head = [0u8; 32];
    let read = read_full(&mut file, &mut head)?;
    let head = &head[..read];

    let (container, kind) = sniff(head);

    let mut probe = Probe {
        container: container.to_string(),
        kind,
        ..Probe::default()
    };

    // Dimensions come from a second, format-aware header read. `imagesize`
    // handles all the still-image formats without decoding pixels.
    if matches!(kind, MediaKind::Image) {
        if let Ok(size) = imagesize::size(path) {
            probe.width = Some(u32::try_from(size.width).unwrap_or(u32::MAX));
            probe.height = Some(u32::try_from(size.height).unwrap_or(u32::MAX));
        }
    }

    // EXIF / ICC live in different places per container; a light scan over the
    // first 128 KiB finds the metadata markers without parsing pixel data.
    if matches!(container, "jpeg" | "tiff" | "heic" | "webp" | "png") {
        if let Ok(sample) = read_prefix(path, 128 * 1024) {
            probe.has_exif = contains(&sample, b"Exif\0\0");
            probe.has_icc = contains(&sample, b"ICC_PROFILE") || contains(&sample, b"iCCP");
            probe.has_xmp = contains(&sample, b"http://ns.adobe.com/xap/1.0/")
                || contains(&sample, b"<x:xmpmeta");
        }
        probe.orientation = read_orientation(path);
    }

    if matches!(kind, MediaKind::Video) {
        probe.duration_ms = read_video_duration(path);
    }

    Ok(probe)
}

fn read_full(file: &mut File, buf: &mut [u8]) -> anyhow::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(filled)
}

fn read_prefix(path: &Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::with_capacity(limit.min(64 * 1024));
    reader.by_ref().take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Detect the real container from magic bytes.
fn sniff(head: &[u8]) -> (&'static str, MediaKind) {
    let starts = |sig: &[u8]| head.starts_with(sig);

    if starts(&[0xFF, 0xD8, 0xFF]) {
        return ("jpeg", MediaKind::Image);
    }
    if starts(b"\x89PNG\r\n\x1A\n") {
        return ("png", MediaKind::Image);
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return ("gif", MediaKind::Image);
    }
    if starts(b"BM") && head.len() > 14 {
        return ("bmp", MediaKind::Image);
    }
    if starts(&[0xFF, 0x0A]) || starts(&[0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20]) {
        return ("jxl", MediaKind::Image);
    }
    if starts(b"II*\0") || starts(b"MM\0*") {
        return ("tiff", MediaKind::Image);
    }
    if head.len() > 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        return match brand {
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                ("heic", MediaKind::Image)
            }
            b"avif" | b"avis" => ("avif", MediaKind::Image),
            _ => ("mp4", MediaKind::Video),
        };
    }
    if starts(b"RIFF") && head.len() > 12 {
        return match &head[8..12] {
            b"WEBP" => ("webp", MediaKind::Image),
            b"AVI " => ("avi", MediaKind::Video),
            b"WAVE" => ("wav", MediaKind::Audio),
            _ => ("riff", MediaKind::Other),
        };
    }
    if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ("matroska", MediaKind::Video);
    }
    if starts(b"OggS") {
        return ("ogg", MediaKind::Audio);
    }
    if starts(b"fLaC") {
        return ("flac", MediaKind::Audio);
    }
    if starts(b"ID3") {
        return ("mp3", MediaKind::Audio);
    }
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08") {
        return ("zip", MediaKind::Archive);
    }
    if starts(b"7z\xBC\xAF\x27\x1C") {
        return ("7z", MediaKind::Archive);
    }
    if starts(b"Rar!\x1A\x07") {
        return ("rar", MediaKind::Archive);
    }
    if head.len() > 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return ("mp3", MediaKind::Audio);
    }
    ("unknown", MediaKind::Other)
}

/// Map a detected container onto the extensions it is legitimately allowed to
/// use, so a `.jpeg` is not flagged as a mismatch of `jpeg`.
fn extension_mismatch(path: &Path, container: &str) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    let expected: &[&str] = match container {
        "jpeg" => &["jpg", "jpeg", "jpe", "jfif"],
        "png" => &["png"],
        "gif" => &["gif"],
        "bmp" => &["bmp"],
        "tiff" => &["tif", "tiff"],
        "webp" => &["webp"],
        "avif" => &["avif"],
        "heic" => &["heic", "heif"],
        "jxl" => &["jxl"],
        "mp4" => &["mp4", "m4v", "m4a", "mov"],
        "matroska" => &["mkv", "webm", "mka"],
        "avi" => &["avi"],
        "wav" => &["wav", "wave"],
        "flac" => &["flac"],
        "mp3" => &["mp3"],
        "ogg" => &["ogg", "oga", "opus"],
        "zip" => &["zip"],
        "7z" => &["7z"],
        "rar" => &["rar", "r00"],
        "unknown" => return false,
        _ => return false,
    };
    !expected.contains(&ext.as_str())
}

/// Read the EXIF orientation tag. Returns `None` when there is no EXIF block or
/// the tag is absent.
fn read_orientation(path: &Path) -> Option<u16> {
    let file = File::open(path).ok()?;
    let mut buf_reader = BufReader::new(file);
    let exif = exif::Reader::new()
        .read_from_container(&mut buf_reader)
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    if let exif::Value::Short(values) = &field.value {
        values.first().copied()
    } else {
        None
    }
}

/// Pull a duration out of an MP4 `mvhd` atom or a Matroska `Info` element.
///
/// This is a best-effort header walk; a missing duration is not an error.
fn read_video_duration(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut head = vec![0u8; 64 * 1024];
    let read = read_full(&mut file, &mut head).ok()?;
    head.truncate(read);

    if head.len() > 12 && &head[4..8] == b"ftyp" {
        return read_mp4_duration(&mut file, &head);
    }
    None
}

fn read_mp4_duration(file: &mut File, head: &[u8]) -> Option<u64> {
    // Walk top-level atoms to `moov`, then find `mvhd` inside it.
    let mut offset = 0usize;
    let mut moov_range: Option<(u64, u64)> = None;

    while offset + 8 <= head.len() {
        let size = u32::from_be_bytes(head[offset..offset + 4].try_into().ok()?) as u64;
        let kind = &head[offset + 4..offset + 8];
        if kind == b"moov" {
            let start = offset as u64 + 8;
            let end = if size == 0 {
                start
            } else {
                (offset as u64 + size).min(head.len() as u64)
            };
            moov_range = Some((start, end));
            break;
        }
        if size < 8 {
            break;
        }
        offset += size as usize;
    }

    let (start, end) = moov_range?;
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?.min(head.len());
    let moov = &head[start..end];

    let mut i = 0usize;
    while i + 8 <= moov.len() {
        let size = u32::from_be_bytes(moov[i..i + 4].try_into().ok()?) as usize;
        let kind = &moov[i + 4..i + 8];
        if kind == b"mvhd" {
            let body = &moov[i + 8..(i + size).min(moov.len())];
            if body.len() < 20 {
                return None;
            }
            let version = body[0];
            let (timescale, duration) = if version == 1 {
                if body.len() < 28 {
                    return None;
                }
                (
                    u32::from_be_bytes(body[20..24].try_into().ok()?) as u64,
                    u64::from_be_bytes(body[24..32.min(body.len())].try_into().ok()?) as u64,
                )
            } else {
                (
                    u32::from_be_bytes(body[12..16].try_into().ok()?) as u64,
                    u32::from_be_bytes(body[16..20].try_into().ok()?) as u64,
                )
            };
            if timescale == 0 {
                return None;
            }
            return Some(duration * 1000 / timescale);
        }
        if size < 8 {
            break;
        }
        i += size;
    }
    let _ = file;
    None
}

/// Count files by detected container, for the report summary.
pub fn container_histogram(probes: &[Probe]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for probe in probes {
        *map.entry(probe.container.clone()).or_insert(0) += 1;
    }
    map
}
