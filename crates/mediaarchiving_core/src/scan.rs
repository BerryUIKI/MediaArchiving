//! Stage 1a — walk the library and fill the inventory.
//!
//! Two properties matter more than raw speed here:
//!
//! * **Resumable.** Rows are committed in batches, so Ctrl-C keeps what was
//!   already learned. Re-running only re-probes files whose size or mtime
//!   changed.
//! * **Never fatal.** A permission error or a vanished file is recorded as a
//!   row with `error` set; the walk continues.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::inventory::{record_from, FileRecord, Inventory};
use crate::probe;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Directories to scan.
    pub roots: Vec<PathBuf>,
    /// Directory names to skip entirely (matched on the final component).
    pub skip_dir_names: Vec<String>,
    /// Absolute paths to skip wholesale. Used so the run directory can live
    /// inside a scanned root without the inventory cataloguing itself.
    pub skip_paths: Vec<PathBuf>,
    /// Re-probe every file even when size and mtime are unchanged.
    pub force: bool,
    /// How many files to gather before writing a batch.
    pub batch_size: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            skip_dir_names: vec![
                ".dtrash".to_string(),
                ".git".to_string(),
                "node_modules".to_string(),
                "$RECYCLE.BIN".to_string(),
                "System Volume Information".to_string(),
            ],
            skip_paths: Vec::new(),
            force: false,
            batch_size: 2_000,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ScanSummary {
    pub files_seen: usize,
    pub new_or_changed: usize,
    pub unchanged: usize,
    pub skipped_dirs: usize,
    pub errors: usize,
    pub pruned: usize,
    pub elapsed_ms: u128,
}

/// Walk `options.roots` and update the inventory in place.
pub fn scan(inventory: &Inventory, options: &ScanOptions) -> Result<ScanSummary> {
    let started = std::time::Instant::now();
    let mut summary = ScanSummary::default();

    let known: std::collections::HashMap<PathBuf, (u64, i64)> = inventory
        .known()?
        .into_iter()
        .map(|(path, size, mtime)| (path, (size, mtime)))
        .collect();

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut pending: Vec<FileRecord> = Vec::with_capacity(options.batch_size);

    // The run directory is never part of the library, whether or not the
    // caller remembered to list it. An inventory that catalogues its own
    // SQLite files is wrong in a way that is easy to miss.
    let mut skip_paths = options.skip_paths.clone();
    let run_root = inventory.run_root().to_path_buf();
    if !skip_paths.contains(&run_root) {
        skip_paths.push(run_root);
    }

    for root in &options.roots {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let path = entry.path();
                if skip_paths.iter().any(|skip| path.starts_with(skip)) {
                    return false;
                }
                let name = entry.file_name().to_string_lossy();
                !options
                    .skip_dir_names
                    .iter()
                    .any(|skip| skip.eq_ignore_ascii_case(&name))
            })
        {
            let Ok(entry) = entry else {
                summary.errors += 1;
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            summary.files_seen += 1;

            let path = entry.path().to_path_buf();
            seen.insert(path.clone());

            let Ok(metadata) = entry.metadata() else {
                summary.errors += 1;
                continue;
            };
            let size = metadata.len();
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            if !options.force {
                if let Some((known_size, known_mtime)) = known.get(&path) {
                    if *known_size == size && *known_mtime == mtime {
                        summary.unchanged += 1;
                        continue;
                    }
                }
            }

            let probed = probe::probe(&path);
            if probed.error.is_some() {
                summary.errors += 1;
            }
            pending.push(record_from(path, size, mtime, probed));
            summary.new_or_changed += 1;

            if pending.len() >= options.batch_size {
                flush(inventory, &mut pending)?;
            }
        }
    }

    flush(inventory, &mut pending)?;

    // Remove anything that was in the inventory but has disappeared.
    //
    // `seen` holds every path the walk actually visited — including files that
    // were skipped as unchanged. Pruning must be driven by "did the walk see
    // this path", not by "did we write a row for it", or every unchanged file
    // would be deleted from the inventory on the second run.
    for root in &options.roots {
        summary.pruned += inventory.prune_under(root, &seen, &skip_paths)?;
    }
    summary.pruned += inventory.prune_missing()?;
    summary.elapsed_ms = started.elapsed().as_millis();

    Ok(summary)
}

fn flush(inventory: &Inventory, pending: &mut Vec<FileRecord>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    for record in pending.drain(..) {
        inventory.upsert(&record)?;
    }
    Ok(())
}

/// BLAKE3 the files that share a size with at least one other file.
///
/// Returns the number of files hashed. Only these can possibly be exact
/// duplicates, so the full-library hash pass is avoided.
pub fn hash_size_collisions(inventory: &Inventory) -> Result<usize> {
    let candidates = inventory.size_collisions()?;
    let done = std::sync::atomic::AtomicUsize::new(0);

    let results: Vec<(PathBuf, String)> = candidates
        .par_iter()
        .filter_map(|record| {
            let hash = hash_file(&record.path).ok()?;
            done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some((record.path.clone(), hash))
        })
        .collect();

    for (path, hash) in &results {
        inventory.set_hash(path, hash)?;
    }
    Ok(results.len())
}

pub fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Group files by identical BLAKE3 hash and mark one keeper per group.
///
/// The keeper is chosen deterministically so repeated runs agree with each
/// other: prefer the shortest path (usually the most "canonical" location),
/// then the oldest mtime.
pub fn group_duplicates(inventory: &Inventory) -> Result<DuplicateStats> {
    use std::collections::HashMap;

    inventory.clear_dup_groups()?;
    let records = inventory.all()?;

    let mut by_hash: HashMap<String, Vec<&FileRecord>> = HashMap::new();
    for record in &records {
        if let Some(hash) = &record.blake3 {
            by_hash.entry(hash.clone()).or_default().push(record);
        }
    }

    let mut stats = DuplicateStats::default();
    let mut group_id: i64 = 0;

    for (_hash, mut files) in by_hash {
        if files.len() < 2 {
            continue;
        }
        files.sort_by_key(|record| {
            (
                record.path.as_os_str().len(),
                record.mtime,
                record.path.clone(),
            )
        });

        group_id += 1;
        stats.groups += 1;
        stats.redundant_files += files.len() - 1;
        stats.wasted_bytes += files[0].size * (files.len() as u64 - 1);

        for (index, record) in files.iter().enumerate() {
            inventory.assign_dup_group(&record.path, group_id, index == 0)?;
        }
    }

    Ok(stats)
}

#[derive(Debug, Default, Clone)]
pub struct DuplicateStats {
    pub groups: usize,
    pub redundant_files: usize,
    pub wasted_bytes: u64,
}

/// Seconds since the epoch for a path, or 0 when unavailable.
pub fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
