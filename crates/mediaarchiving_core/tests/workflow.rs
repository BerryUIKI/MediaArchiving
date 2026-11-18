//! End-to-end tests over a synthetic library.
//!
//! These run entirely on temporary directories: no network share, no real
//! media. The point is to pin down the *decisions*, which is where the
//! interesting bugs live.

use std::fs;
use std::io::Write;
use std::path::Path;

use mediaarchiving_core::analysis::{analyse, Decision, RiskTier};
use mediaarchiving_core::plan::build_plan;
use mediaarchiving_core::policy::{Intent, Policy};
use mediaarchiving_core::{scan, Inventory, ScanOptions};

/// Smallest valid PNG: a 1x1 image. Written by hand so the test has no
/// dependency on an encoder.
fn tiny_png() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    // IHDR
    out.extend_from_slice(&13u32.to_be_bytes());
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&1u32.to_be_bytes()); // width
    out.extend_from_slice(&1u32.to_be_bytes()); // height
    out.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth, colour type, etc.
                                             // CRC covers the chunk type and data: bytes 12..29.
    let crc = crc32(&out[12..29]);
    out.extend_from_slice(&crc.to_be_bytes());
    // IEND
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"IEND");
    let crc = crc32(b"IEND");
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Count regular files under `root`, ignoring the run directory.
fn count_media_files(root: &Path) -> usize {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".run")
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count()
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = fs::File::create(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// A JPEG carrying a real SOF0 header, padded out to `byte_len`.
///
/// The dimensions matter: the savings model works in bits-per-pixel, so a
/// fixture without dimensions would be silently skipped and every assertion
/// about transcode candidates would pass for the wrong reason.
fn write_jpeg(path: &Path, width: u16, height: u16, byte_len: usize) {
    let mut bytes = vec![0xFF, 0xD8]; // SOI

    // APP0 / JFIF
    bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    bytes.extend_from_slice(b"JFIF\0");
    bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);

    // SOF0: precision, height, width, 1 component
    bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);

    assert!(byte_len > bytes.len(), "fixture too small for its header");
    bytes.resize(byte_len, 0x41);
    write(path, &bytes);
}

/// Build a library, scan it, and return (inventory records, run dir).
fn fixture(root: &Path) -> Vec<mediaarchiving_core::inventory::FileRecord> {
    // A large JPEG — should be a transcode candidate.
    // 1200x1200 px in 400 kB is 2.2 bits/px, i.e. a lightly-compressed source.
    write_jpeg(&root.join("set_a/photo1.jpg"), 1200, 1200, 400_000);
    // Identical duplicate of photo1 — the non-keeper must be blocked.
    write_jpeg(&root.join("set_a/photo1_copy.jpg"), 1200, 1200, 400_000);
    // A PNG — lossless source, high savings expected.
    write(&root.join("set_a/graphic.png"), &tiny_png());
    // A GIF — low priority.
    write(
        &root.join("set_b/anim.gif"),
        b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
    );

    let run = mediaarchiving_core::RunDir::new(root.join(".run"));
    run.ensure().unwrap();
    let inventory = Inventory::open(&run.inventory()).unwrap();
    let options = ScanOptions {
        roots: vec![root.to_path_buf()],
        ..ScanOptions::default()
    };
    scan(&inventory, &options).unwrap();
    // The run directory sits inside the scanned root. If the inventory
    // catalogued its own SQLite files, `inventory.all()` would contain more
    // rows than the library has files. Callers that write extra files assert
    // their own count on top of this one.
    let files_on_disk = count_media_files(root);
    assert_eq!(
        inventory.count().unwrap(),
        files_on_disk,
        "inventory must contain only library files, not its own database"
    );
    mediaarchiving_core::scan::hash_size_collisions(&inventory).unwrap();
    mediaarchiving_core::scan::group_duplicates(&inventory).unwrap();
    inventory.all().unwrap()
}

#[test]
fn probe_reads_the_real_container_not_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A PNG named .jpg — common in scraped libraries.
    write(&root.join("mislabelled.jpg"), &tiny_png());

    let records = fixture(root);
    let record = records
        .iter()
        .find(|r| r.path.ends_with("mislabelled.jpg"))
        .expect("file was not inventoried");
    assert_eq!(
        record.container, "png",
        "magic bytes must win over extension"
    );
    assert!(record.extension_mismatch, "mismatch should be flagged");
}

#[test]
fn duplicate_keeper_is_deterministic_and_marks_the_other_copy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);

    let group: Vec<_> = records
        .iter()
        .filter(|r| r.path.to_string_lossy().contains("photo1"))
        .collect();
    assert_eq!(group.len(), 2);
    assert!(group.iter().any(|r| r.is_keeper), "one copy must be keeper");
    assert!(
        group.iter().any(|r| !r.is_keeper),
        "the other must not be keeper"
    );
    let keepers = group.iter().filter(|r| r.is_keeper).count();
    assert_eq!(keepers, 1, "exactly one keeper per group");

    // The keeper is the shortest path, so `photo1.jpg` wins over `photo1_copy.jpg`.
    let keeper = group.iter().find(|r| r.is_keeper).unwrap();
    assert!(
        keeper.path.to_string_lossy().ends_with("photo1.jpg"),
        "keeper should be the shorter path, got {}",
        keeper.path.display()
    );
}

#[test]
fn redundant_copy_is_blocked_not_planned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);
    let policy = Policy::default();
    let analysis = analyse(&records, &policy);

    let copy = analysis
        .files
        .iter()
        .find(|f| f.path.contains("photo1_copy"))
        .expect("copy is missing from analysis");
    assert_eq!(
        copy.decision,
        Decision::Blocked,
        "a redundant copy must not be transcoded before the duplicate is resolved"
    );
    assert!(copy.reasons.iter().any(|r| r.contains("redundant")));
}

#[test]
fn dataset_intent_refuses_lossy_transcoding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);

    let policy = Policy {
        intent: Intent::Dataset,
        ..Policy::default()
    };
    let analysis = analyse(&records, &policy);

    let photo = analysis
        .files
        .iter()
        .find(|f| f.path.contains("photo1.jpg"))
        .unwrap();
    assert_eq!(
        photo.decision,
        Decision::ArchiveOnly,
        "for a dataset the pixels are the data; lossy re-encoding must be refused"
    );
    assert!(photo.reasons.iter().any(|r| r.contains("dataset intent")));
    assert_eq!(analysis.totals.estimated_saving_bytes, 0);
}

#[test]
fn keep_target_plans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);

    let mut policy = Policy::default();
    policy.image.target = "keep".to_string();
    let analysis = analyse(&records, &policy);

    assert!(
        analysis
            .files
            .iter()
            .all(|f| f.decision != Decision::Transcode),
        "target=keep must produce no transcode work"
    );
}

#[test]
fn plan_batches_are_layered_by_risk_then_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);
    let policy = Policy::default();
    let analysis = analyse(&records, &policy);
    let plan = build_plan(&analysis);

    // Every planned file must appear in exactly one batch.
    let planned: usize = plan.batches.iter().map(|b| b.files).sum();
    let expected = analysis
        .files
        .iter()
        .filter(|f| f.decision == Decision::Transcode)
        .count();
    assert_eq!(planned, expected, "no file may be lost or duplicated");

    // Within a batch everything shares one risk tier and one directory.
    for batch in &plan.batches {
        let members: Vec<_> = analysis
            .files
            .iter()
            .filter(|f| batch.paths.contains(&f.path))
            .collect();
        for member in &members {
            assert_eq!(member.risk, batch.risk, "batch mixes risk tiers");
        }
    }

    // High-risk batches sort first.
    let ranks: Vec<u8> = plan
        .batches
        .iter()
        .map(|b| match b.risk {
            RiskTier::High => 0,
            RiskTier::Medium => 1,
            RiskTier::Low => 2,
        })
        .collect();
    let mut sorted = ranks.clone();
    sorted.sort_unstable();
    assert_eq!(ranks, sorted, "high-risk batches must come first");
}

#[test]
fn rescan_skips_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);

    let run = mediaarchiving_core::RunDir::new(root.join(".run"));
    let inventory = Inventory::open(&run.inventory()).unwrap();
    let before = inventory.count().unwrap();

    let options = ScanOptions {
        roots: vec![root.to_path_buf()],
        ..ScanOptions::default()
    };
    let summary = scan(&inventory, &options).unwrap();

    assert_eq!(summary.new_or_changed, 0, "nothing changed on disk");
    assert_eq!(summary.unchanged, before, "all files should be skipped");
    assert_eq!(inventory.count().unwrap(), before);
}

#[test]
fn deleted_files_are_pruned_from_the_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);

    let run = mediaarchiving_core::RunDir::new(root.join(".run"));
    let inventory = Inventory::open(&run.inventory()).unwrap();
    let before = inventory.count().unwrap();

    fs::remove_file(root.join("set_b/anim.gif")).unwrap();

    let options = ScanOptions {
        roots: vec![root.to_path_buf()],
        ..ScanOptions::default()
    };
    let summary = scan(&inventory, &options).unwrap();

    assert!(summary.pruned >= 1, "the deleted file must be pruned");
    assert_eq!(inventory.count().unwrap(), before - 1);
}

#[test]
fn policy_roundtrips_through_the_template() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");

    assert!(Policy::ensure_template(&path).unwrap(), "template written");
    assert!(!Policy::ensure_template(&path).unwrap(), "no overwrite");

    let policy = Policy::load(&path).unwrap();
    assert_eq!(policy.intent, Intent::Archive);
    assert_eq!(policy.image.target, "avif");
    assert!(policy.metadata.keep_icc, "colour profiles kept by default");
    assert!(policy.intent.allows_lossy());
}

#[test]
fn report_is_self_contained_html() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);
    let policy = Policy::default();
    let analysis = analyse(&records, &policy);
    let plan = build_plan(&analysis);

    let html = mediaarchiving_core::report::render(&analysis, &plan, &policy, "test-run");

    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("</html>"));
    // A report that references an external asset will not open offline.
    assert!(!html.contains("http://"), "no external references allowed");
    assert!(!html.contains("https://"), "no external references allowed");
    assert!(!html.contains("<script"), "no scripts needed");
    // It must say plainly that nothing was changed.
    assert!(html.contains("No files were modified"));
    // And that the numbers are estimates.
    assert!(html.contains("estimates"));
}

#[test]
fn html_escaping_neutralises_path_injection() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let records = fixture(root);
    let policy = Policy::default();
    let mut analysis = analyse(&records, &policy);
    analysis.files[0].path = "<script>alert(1)</script>".to_string();
    let plan = build_plan(&analysis);

    let html = mediaarchiving_core::report::render(&analysis, &plan, &policy, "run");
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "path text must be escaped"
    );
}
