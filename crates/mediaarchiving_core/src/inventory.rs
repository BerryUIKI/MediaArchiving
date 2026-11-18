//! The inventory: a SQLite catalogue of every file in the library.
//!
//! Why SQLite rather than JSON: a 90k-file library is walked over SMB, and the
//! scan must be *resumable*. Keeping rows in a transactional store means an
//! interrupted scan keeps everything it already learned, and the next run only
//! re-probes files whose size or mtime changed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::probe::{MediaKind, Probe};

/// One row of the inventory.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the Unix epoch, as reported by the filesystem.
    pub mtime: i64,
    pub kind: MediaKind,
    pub container: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub has_exif: bool,
    pub has_icc: bool,
    pub has_xmp: bool,
    pub orientation: Option<u16>,
    pub extension_mismatch: bool,
    pub error: Option<String>,
    /// BLAKE3 of the full file, filled in when exact-duplicate detection runs.
    pub blake3: Option<String>,
    /// Identifier of the duplicate group this file belongs to, if any.
    pub dup_group: Option<i64>,
    /// Whether this file is the one copy the user keeps of a duplicate group.
    pub is_keeper: bool,
}

impl FileRecord {
    pub fn pixels(&self) -> Option<u64> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(u64::from(w) * u64::from(h)),
            _ => None,
        }
    }
}

pub struct Inventory {
    conn: Connection,
    /// Directory holding this inventory, so a scan can refuse to catalogue its
    /// own bookkeeping when the run directory lives inside a scanned root.
    run_root: PathBuf,
}

impl Inventory {
    /// Open (or create) the inventory at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        std::fs::create_dir_all(&parent)?;
        let conn = Connection::open(path)
            .with_context(|| format!("opening inventory at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let inv = Self {
            conn,
            run_root: normalise_run_root(&parent),
        };
        inv.migrate()?;
        Ok(inv)
    }

    /// The directory this inventory lives in. Scans must never catalogue it.
    pub fn run_root(&self) -> &Path {
        &self.run_root
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files (
                path               TEXT PRIMARY KEY,
                size               INTEGER NOT NULL,
                mtime              INTEGER NOT NULL,
                kind               TEXT NOT NULL,
                container          TEXT NOT NULL,
                width              INTEGER,
                height             INTEGER,
                duration_ms        INTEGER,
                has_exif           INTEGER NOT NULL DEFAULT 0,
                has_icc            INTEGER NOT NULL DEFAULT 0,
                has_xmp            INTEGER NOT NULL DEFAULT 0,
                orientation        INTEGER,
                extension_mismatch INTEGER NOT NULL DEFAULT 0,
                error              TEXT,
                blake3             TEXT,
                dup_group          INTEGER,
                is_keeper          INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_files_size ON files(size);
            CREATE INDEX IF NOT EXISTS idx_files_blake3 ON files(blake3);
            CREATE INDEX IF NOT EXISTS idx_files_container ON files(container);

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Insert or update a file. Returns `true` when the row is new.
    pub fn upsert(&self, record: &FileRecord) -> Result<bool> {
        let path = record.path.to_string_lossy().to_string();
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT mtime FROM files WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;

        self.conn.execute(
            r#"
            INSERT INTO files (
                path, size, mtime, kind, container, width, height, duration_ms,
                has_exif, has_icc, has_xmp, orientation, extension_mismatch, error
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
            ON CONFLICT(path) DO UPDATE SET
                size = excluded.size,
                mtime = excluded.mtime,
                kind = excluded.kind,
                container = excluded.container,
                width = excluded.width,
                height = excluded.height,
                duration_ms = excluded.duration_ms,
                has_exif = excluded.has_exif,
                has_icc = excluded.has_icc,
                has_xmp = excluded.has_xmp,
                orientation = excluded.orientation,
                extension_mismatch = excluded.extension_mismatch,
                error = excluded.error,
                blake3 = NULL,
                dup_group = NULL,
                is_keeper = 0
            "#,
            params![
                path,
                record.size as i64,
                record.mtime,
                record.kind.as_str(),
                record.container,
                record.width.map(i64::from),
                record.height.map(i64::from),
                record.duration_ms.map(|d| d as i64),
                record.has_exif as i32,
                record.has_icc as i32,
                record.has_xmp as i32,
                record.orientation.map(i32::from),
                record.extension_mismatch as i32,
                record.error,
            ],
        )?;
        Ok(existing.is_none())
    }

    /// Every known path with its size and mtime — used to spot deletions and
    /// to skip unchanged files.
    pub fn known(&self) -> Result<Vec<(PathBuf, u64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT path, size, mtime FROM files")?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let mtime: i64 = row.get(2)?;
            Ok((PathBuf::from(path), size as u64, mtime))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove rows whose path no longer exists on disk.
    pub fn prune_missing(&self) -> Result<usize> {
        let mut to_delete = Vec::new();
        for (path, _, _) in self.known()? {
            if !path.exists() {
                to_delete.push(path);
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        for path in &to_delete {
            tx.execute(
                "DELETE FROM files WHERE path = ?1",
                params![path.to_string_lossy()],
            )?;
        }
        tx.commit()?;
        Ok(to_delete.len())
    }

    pub fn all(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT path, size, mtime, kind, container, width, height, duration_ms,
                   has_exif, has_icc, has_xmp, orientation, extension_mismatch,
                   error, blake3, dup_group, is_keeper
            FROM files
            ORDER BY size DESC
            "#,
        )?;
        let rows = stmt.query_map([], row_to_record)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Candidate files for exact hashing: same size, at least two of them.
    pub fn size_collisions(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT f.path, f.size, f.mtime, f.kind, f.container, f.width, f.height,
                   f.duration_ms, f.has_exif, f.has_icc, f.has_xmp, f.orientation,
                   f.extension_mismatch, f.error, f.blake3, f.dup_group, f.is_keeper
            FROM files f
            JOIN (SELECT size FROM files GROUP BY size HAVING COUNT(*) > 1) g
              ON f.size = g.size
            ORDER BY f.size DESC
            "#,
        )?;
        let rows = stmt.query_map([], row_to_record)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_hash(&self, path: &Path, hash: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET blake3 = ?2 WHERE path = ?1",
            params![path.to_string_lossy(), hash],
        )?;
        Ok(())
    }

    pub fn assign_dup_group(&self, path: &Path, group: i64, keeper: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET dup_group = ?2, is_keeper = ?3 WHERE path = ?1",
            params![path.to_string_lossy(), group, keeper as i32],
        )?;
        Ok(())
    }

    pub fn clear_dup_groups(&self) -> Result<()> {
        self.conn
            .execute("UPDATE files SET dup_group = NULL, is_keeper = 0", [])?;
        Ok(())
    }

    /// Drop rows under `root` that were not seen during this scan pass.
    ///
    /// `seen` is every path the walk visited, so unchanged files are protected.
    /// `skip_paths` are prefixes the walk refused to enter; their rows are left
    /// alone rather than being mistaken for deletions.
    ///
    /// The containment test is done on components, not on a `LIKE` prefix:
    /// `<root>2` must not be treated as living under `<root>`.
    pub fn prune_under(
        &self,
        root: &Path,
        seen: &std::collections::HashSet<PathBuf>,
        skip_paths: &[PathBuf],
    ) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            Ok(PathBuf::from(path))
        })?;

        let mut to_delete = Vec::new();
        for path in rows {
            let path = path?;
            if !path.starts_with(root) {
                continue;
            }
            if skip_paths.iter().any(|skip| path.starts_with(skip)) {
                continue;
            }
            if !seen.contains(&path) {
                to_delete.push(path);
            }
        }

        let tx = self.conn.unchecked_transaction()?;
        for path in &to_delete {
            tx.execute(
                "DELETE FROM files WHERE path = ?1",
                params![path.to_string_lossy()],
            )?;
        }
        tx.commit()?;
        Ok(to_delete.len())
    }
}

/// Canonicalise a directory for path-prefix comparisons.
///
/// `fs::canonicalize` on Windows returns a verbatim path (`\\?\C:\...`), while
/// the paths produced by directory walks do not carry that prefix. Comparing
/// the two directly silently never matches, so the prefix is stripped here —
/// `starts_with` is component-wise, and the reminder of the path is identical.
fn normalise_run_root(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().to_string();
    let stripped = text
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| text.strip_prefix(r"\\?\").map(str::to_string));
    match stripped {
        Some(clean) => PathBuf::from(clean),
        None => canonical,
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {    let path: String = row.get(0)?;
    let kind: String = row.get(3)?;
    Ok(FileRecord {
        path: PathBuf::from(path),
        size: row.get::<_, i64>(1)? as u64,
        mtime: row.get(2)?,
        kind: MediaKind::from_str_opt(&kind).unwrap_or(MediaKind::Other),
        container: row.get(4)?,
        width: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        duration_ms: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        has_exif: row.get::<_, i64>(8)? != 0,
        has_icc: row.get::<_, i64>(9)? != 0,
        has_xmp: row.get::<_, i64>(10)? != 0,
        orientation: row.get::<_, Option<i64>>(11)?.map(|v| v as u16),
        extension_mismatch: row.get::<_, i64>(12)? != 0,
        error: row.get(13)?,
        blake3: row.get(14)?,
        dup_group: row.get(15)?,
        is_keeper: row.get::<_, i64>(16)? != 0,
    })
}

/// Build a `FileRecord` from a walk result and a probe.
pub fn record_from(path: PathBuf, size: u64, mtime: i64, probe: Probe) -> FileRecord {
    FileRecord {
        path,
        size,
        mtime,
        kind: probe.kind,
        container: probe.container,
        width: probe.width,
        height: probe.height,
        duration_ms: probe.duration_ms,
        has_exif: probe.has_exif,
        has_icc: probe.has_icc,
        has_xmp: probe.has_xmp,
        orientation: probe.orientation,
        extension_mismatch: probe.extension_mismatch,
        error: probe.error,
        blake3: None,
        dup_group: None,
        is_keeper: false,
    }
}
