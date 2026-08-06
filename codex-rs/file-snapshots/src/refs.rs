//! Per-thread snapshot logs and garbage collection (RFC §8).
//!
//! Each thread has an append-only log of `(turn_id, manifest_id)` refs.
//! Snapshot lifetime is tied to thread lifetime: removing a thread drops
//! its refs, and a mark-and-sweep pass then deletes unreferenced
//! manifests and blobs. No timers, no retention windows.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::ManifestStore;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub turn_id: String,
    pub manifest_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLog {
    pub entries: Vec<SnapshotRef>,
}

pub struct RefStore {
    root: PathBuf,
}

impl RefStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    pub fn append(&self, thread_id: &str, entry: SnapshotRef) -> Result<()> {
        let mut log = self.load(thread_id)?;
        log.entries.push(entry);
        let path = self.log_path(thread_id);
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&log)?;
        fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        Ok(())
    }

    /// Load a thread's log; a thread with no snapshots yields an empty log.
    pub fn load(&self, thread_id: &str) -> Result<ThreadLog> {
        let path = self.log_path(thread_id);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ThreadLog::default()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Drop a thread's refs (its snapshots become garbage for the next GC).
    pub fn remove(&self, thread_id: &str) -> Result<()> {
        let path = self.log_path(thread_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    pub fn thread_logs(&self) -> Result<Vec<ThreadLog>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|e| SnapshotError::io(entry.path(), e))?;
            out.push(serde_json::from_slice(&bytes)?);
        }
        Ok(out)
    }

    fn log_path(&self, thread_id: &str) -> PathBuf {
        // Thread ids are UUID-like; anything else is defensively mapped to
        // a filename-safe character set.
        let safe: String = thread_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{safe}.json"))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    pub manifests_kept: usize,
    pub manifests_removed: usize,
    pub blobs_kept: usize,
    pub blobs_removed: usize,
}

/// Mark-and-sweep: manifests referenced by any thread log are live; blobs
/// referenced by any live manifest are live; everything else is removed.
pub fn collect_garbage(
    refs: &RefStore,
    manifests: &ManifestStore,
    blobs: &BlobStore,
) -> Result<GcStats> {
    let mut live_manifests = BTreeSet::new();
    for log in refs.thread_logs()? {
        for entry in log.entries {
            live_manifests.insert(entry.manifest_id);
        }
    }

    let mut stats = GcStats::default();
    let mut live_blobs = BTreeSet::new();
    for id in manifests.ids()? {
        if live_manifests.contains(&id) {
            stats.manifests_kept += 1;
            for entry in manifests.load(&id)?.entries.values() {
                live_blobs.insert(entry.hash.clone());
            }
        } else {
            manifests.remove(&id)?;
            stats.manifests_removed += 1;
        }
    }

    for hash in blobs.hashes()? {
        if live_blobs.contains(&hash) {
            stats.blobs_kept += 1;
        } else {
            blobs.remove(&hash)?;
            stats.blobs_removed += 1;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn append_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path().join("refs")).unwrap();

        assert_eq!(refs.load("t1").unwrap(), ThreadLog::default());
        refs.append(
            "t1",
            SnapshotRef {
                turn_id: "turn-1".into(),
                manifest_id: "m1".into(),
            },
        )
        .unwrap();
        refs.append(
            "t1",
            SnapshotRef {
                turn_id: "turn-2".into(),
                manifest_id: "m2".into(),
            },
        )
        .unwrap();

        let log = refs.load("t1").unwrap();
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[1].manifest_id, "m2");
    }

    #[test]
    fn gc_sweeps_unreferenced_manifests_and_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::open(dir.path().join("refs")).unwrap();
        let manifests = ManifestStore::open(dir.path().join("manifests")).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();

        let live_hash = blobs.store_bytes(b"live").unwrap();
        let dead_hash = blobs.store_bytes(b"dead").unwrap();

        let mut live = crate::manifest::Manifest::default();
        live.entries.insert(
            "/f".into(),
            crate::manifest::FileEntry {
                mode: 0o644,
                size: 4,
                mtime_secs: 1,
                mtime_nanos: 0,
                hash: live_hash.clone(),
            },
        );
        let live_id = manifests.save(&live).unwrap();

        let mut dead = live.clone();
        dead.entries.get_mut("/f").unwrap().hash = dead_hash.clone();
        let dead_id = manifests.save(&dead).unwrap();

        refs.append(
            "t1",
            SnapshotRef {
                turn_id: "turn".into(),
                manifest_id: live_id.clone(),
            },
        )
        .unwrap();

        let stats = collect_garbage(&refs, &manifests, &blobs).unwrap();
        assert_eq!(stats.manifests_kept, 1);
        assert_eq!(stats.manifests_removed, 1);
        assert_eq!(stats.blobs_kept, 1);
        assert_eq!(stats.blobs_removed, 1);
        assert!(manifests.load(&live_id).is_ok());
        assert!(manifests.load(&dead_id).is_err());
        assert!(blobs.contains(&live_hash));
        assert!(!blobs.contains(&dead_hash));

        // Dropping the thread makes everything garbage.
        refs.remove("t1").unwrap();
        let stats = collect_garbage(&refs, &manifests, &blobs).unwrap();
        assert_eq!(stats.manifests_removed, 1);
        assert_eq!(stats.blobs_removed, 1);
        assert_eq!(manifests.ids().unwrap().len(), 0);
        assert_eq!(blobs.hashes().unwrap().len(), 0);
    }
}
