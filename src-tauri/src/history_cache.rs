//! Small versioned on-disk caches for append-only usage logs, and the scan
//! loop both log scanners share: reuse a file's record when it is unchanged,
//! read only the appended tail when it grew, and rescan it when it was
//! rewritten.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    data: T,
}

fn cache_path(name: &str) -> Option<PathBuf> {
    let mut root = crate::util::config_dir()?;
    root.push(format!("{name}-history-cache.json"));
    Some(root)
}

pub fn load<T: Default + DeserializeOwned>(name: &str, version: u32) -> T {
    cache_path(name)
        .and_then(|path| load_path(&path))
        .filter(|envelope| envelope.version == version)
        .map(|envelope| envelope.data)
        .unwrap_or_default()
}

fn load_path<T: DeserializeOwned>(path: &Path) -> Option<Envelope<T>> {
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
}

pub fn save<T: Serialize>(name: &str, version: u32, data: &T) -> std::io::Result<()> {
    let path =
        cache_path(name).ok_or_else(|| std::io::Error::other("no configuration directory"))?;
    save_path(&path, version, data)
}

pub fn clear(name: &str) -> std::io::Result<()> {
    let path =
        cache_path(name).ok_or_else(|| std::io::Error::other("no configuration directory"))?;
    remove_path(&path)
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn save_path<T: Serialize>(path: &Path, version: u32, data: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(&Envelope { version, data })?;
    crate::util::write_atomic(path, &body)
}

// ---------------- incremental scanning ----------------

/// One log file found on disk, with what the scan needs to decide whether it
/// changed since the last pass.
#[derive(Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// Identity in the cache. Chosen by each scanner: the Codex one keys by file
    /// name so a session moved into `archived_sessions` keeps its record.
    pub key: String,
    pub size: u64,
    pub modified_ms: u64,
}

pub fn modified_ms(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// What a scanner's per-file record has to expose for the shared loop.
pub trait ScanRecord: Clone + Default {
    fn observed_size(&self) -> u64;
    fn observed_modified_ms(&self) -> u64;
    /// Byte offset just past the last complete line already read.
    fn processed_offset(&self) -> u64;
    /// Drop events older than `cutoff`, returning whether any went. Only the
    /// last month is ever shown, and without this a record keeps every event
    /// its file ever produced.
    fn prune_before(&mut self, cutoff: i64) -> bool;
    /// Called when an unchanged record is reused, in case the file moved.
    fn relocate(&mut self, _path: &Path) {}

    fn unchanged(&self, candidate: &Candidate) -> bool {
        self.observed_size() == candidate.size
            && self.observed_modified_ms() == candidate.modified_ms
    }
}

/// Bring one file's record up to date. `append` reads from the record's
/// processed offset to the end of the file; it is handed a default record when
/// the file has to be read from the start.
pub fn scan_one<R: ScanRecord>(
    candidate: &Candidate,
    existing: Option<R>,
    append: impl Fn(&Candidate, R) -> std::io::Result<R>,
) -> std::io::Result<R> {
    if let Some(mut record) = existing {
        if record.unchanged(candidate) {
            record.relocate(&candidate.path);
            return Ok(record);
        }
        // Grown past where the last pass stopped: only the tail is new. Same
        // length with a new mtime means it was rewritten, so it falls through.
        if candidate.size >= record.processed_offset() && candidate.size != record.observed_size()
        {
            return append(candidate, record);
        }
    }
    append(candidate, R::default())
}

#[derive(Default)]
pub struct ScanStats {
    pub scanned: u64,
    pub cached: u64,
    pub skipped: u64,
    /// Whether the cache differs from what was loaded, so it needs saving.
    pub changed: bool,
}

/// Scan every candidate into `files`, dropping records for files that are
/// gone and events older than `cutoff`. A file that fails to read keeps its
/// previous record rather than losing its history.
pub fn rescan<R: ScanRecord>(
    files: &mut BTreeMap<String, R>,
    candidates: &[Candidate],
    cutoff: i64,
    append: impl Fn(&Candidate, R) -> std::io::Result<R>,
) -> ScanStats {
    let mut stats = ScanStats::default();
    let mut seen = HashSet::new();
    for candidate in candidates {
        seen.insert(candidate.key.clone());
        let existing = files.remove(&candidate.key);
        let backup = existing.clone();
        let was_cached = existing
            .as_ref()
            .is_some_and(|record| record.unchanged(candidate));
        match scan_one(candidate, existing, &append) {
            Ok(mut record) => {
                if was_cached {
                    stats.cached += 1;
                } else {
                    stats.scanned += 1;
                    stats.changed = true;
                }
                stats.changed |= record.prune_before(cutoff);
                files.insert(candidate.key.clone(), record);
            }
            Err(_) => {
                stats.skipped += 1;
                if let Some(record) = backup {
                    files.insert(candidate.key.clone(), record);
                }
            }
        }
    }
    let before = files.len();
    files.retain(|key, _| seen.contains(key));
    stats.changed |= before != files.len();
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_save_replaces_an_existing_cache() {
        let root = std::env::temp_dir().join(format!("ai-usage-cache-{}", uuid::Uuid::new_v4()));
        let path = root.join("cache.json");
        save_path(&path, 1, &vec![1u64]).unwrap();
        save_path(&path, 1, &vec![2u64, 3]).unwrap();
        let loaded: Envelope<Vec<u64>> = load_path(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.data, vec![2, 3]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clear_is_idempotent() {
        let root = std::env::temp_dir().join(format!("ai-usage-cache-{}", uuid::Uuid::new_v4()));
        let path = root.join("cache.json");
        save_path(&path, 1, &vec![1u64]).unwrap();
        remove_path(&path).unwrap();
        remove_path(&path).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[derive(Clone, Default)]
    struct Record {
        size: u64,
        modified: u64,
        offset: u64,
        events: Vec<i64>,
        reads: u32,
    }

    impl ScanRecord for Record {
        fn observed_size(&self) -> u64 {
            self.size
        }
        fn observed_modified_ms(&self) -> u64 {
            self.modified
        }
        fn processed_offset(&self) -> u64 {
            self.offset
        }
        fn prune_before(&mut self, cutoff: i64) -> bool {
            let before = self.events.len();
            self.events.retain(|ts| *ts >= cutoff);
            before != self.events.len()
        }
    }

    fn candidate(key: &str, size: u64, modified_ms: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(key),
            key: key.to_string(),
            size,
            modified_ms,
        }
    }

    /// Stands in for reading a file: records what it was asked for.
    fn fake_append(candidate: &Candidate, mut record: Record) -> std::io::Result<Record> {
        record.reads += 1;
        record.size = candidate.size;
        record.modified = candidate.modified_ms;
        record.offset = candidate.size;
        record.events.push(100);
        Ok(record)
    }

    #[test]
    fn unchanged_files_are_reused_and_vanished_ones_dropped() {
        let mut files = BTreeMap::new();
        let first = rescan(&mut files, &[candidate("a", 10, 1), candidate("b", 5, 1)], 0, fake_append);
        assert_eq!(first.scanned, 2);
        assert!(first.changed);

        let second = rescan(&mut files, &[candidate("a", 10, 1)], 0, fake_append);
        assert_eq!(second.cached, 1);
        assert_eq!(second.scanned, 0);
        assert!(second.changed, "dropping b is a change");
        assert!(!files.contains_key("b"));
        assert_eq!(files["a"].reads, 1);
    }

    #[test]
    fn events_older_than_the_cutoff_are_pruned_from_the_record() {
        let mut files = BTreeMap::new();
        files.insert(
            "a".to_string(),
            Record {
                size: 10,
                modified: 1,
                offset: 10,
                events: vec![1, 50, 200],
                reads: 0,
            },
        );
        let stats = rescan(&mut files, &[candidate("a", 10, 1)], 100, fake_append);
        assert!(stats.changed);
        assert_eq!(files["a"].events, vec![200]);
    }
}
