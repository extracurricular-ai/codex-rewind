//! Checkpoint capture: stat-walk the tracked set and produce a manifest.
//!
//! For every tracked file the previous manifest's `(size, mtime)`
//! fingerprint is consulted first; on a match the recorded hash is reused
//! without reading the file (the "persistent stat cache" from RFC §6.1 —
//! the mechanism that makes `git status` fast, owned by the snapshot
//! subsystem instead of a git index). Only changed or new files are read,
//! hashed, and stored.

use std::fs;
use std::path::PathBuf;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::manifest::FileEntry;
use crate::manifest::Manifest;
use crate::manifest::ManifestStore;
use crate::manifest::mode_of;
use crate::manifest::mtime_parts;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CheckpointStats {
    /// Files whose hash was reused via the stat cache (not read).
    pub reused: usize,
    /// Files read, hashed, and (if new) stored.
    pub hashed: usize,
    /// Paths skipped: vanished mid-walk or not regular files.
    pub skipped: usize,
}

#[derive(Debug)]
pub struct Checkpoint {
    pub id: String,
    pub manifest: Manifest,
    pub stats: CheckpointStats,
}

/// Capture the state of `files` into a new persisted manifest.
///
/// `prev` is the previous checkpoint of the same tracked set (if any) and
/// serves as the stat cache. `complete` records whether `files` covers
/// the entire scope (workspace scan) or a bounded subset (fallback mode);
/// it determines how restores may interpret absence (see `Manifest`).
/// Paths that cannot be read (deleted between enumeration and capture,
/// unreadable, non-regular) are skipped — a checkpoint records what
/// verifiably exists at capture time.
pub fn capture(
    blobs: &BlobStore,
    manifests: &ManifestStore,
    files: impl IntoIterator<Item = PathBuf>,
    prev: Option<&Manifest>,
    complete: bool,
) -> Result<Checkpoint> {
    let mut manifest = Manifest {
        complete,
        ..Default::default()
    };
    let mut stats = CheckpointStats::default();

    for path in files {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            stats.skipped += 1;
            continue;
        };
        if !meta.is_file() {
            // Symlinks and other non-regular files are out of scope for v1.
            stats.skipped += 1;
            continue;
        }
        let key = path.to_string_lossy().into_owned();

        if let Some(prev_entry) = prev.and_then(|m| m.entries.get(&key))
            && prev_entry.stat_matches(&meta)
        {
            let mut entry = prev_entry.clone();
            // Permission changes don't necessarily bump mtime; mode is
            // re-read from the (already fetched) metadata either way.
            entry.mode = mode_of(&meta);
            manifest.entries.insert(key, entry);
            stats.reused += 1;
            continue;
        }

        // Read once; hash and size derive from the same bytes so the
        // stored blob is always consistent with the manifest entry.
        let Ok(content) = fs::read(&path) else {
            stats.skipped += 1;
            continue;
        };
        let hash = blobs.store_bytes(&content)?;
        let (mtime_secs, mtime_nanos) = mtime_parts(&meta);
        manifest.entries.insert(
            key,
            FileEntry {
                mode: mode_of(&meta),
                size: content.len() as u64,
                mtime_secs,
                mtime_nanos,
                hash,
            },
        );
        stats.hashed += 1;
    }

    let id = manifests.save(&manifest)?;
    Ok(Checkpoint {
        id,
        manifest,
        stats,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    struct Fixture {
        _dir: tempfile::TempDir,
        blobs: BlobStore,
        manifests: ManifestStore,
        ws: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let manifests = ManifestStore::open(dir.path().join("manifests")).unwrap();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        Fixture {
            _dir: dir,
            blobs,
            manifests,
            ws,
        }
    }

    #[test]
    fn captures_files_and_stores_blobs() {
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"alpha").unwrap();

        let cp = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();
        assert_eq!(cp.stats.hashed, 1);
        assert_eq!(cp.stats.reused, 0);

        let entry = &cp.manifest.entries[&a.to_string_lossy().into_owned()];
        assert_eq!(f.blobs.load(&entry.hash).unwrap(), b"alpha");
        assert_eq!(entry.size, 5);
    }

    #[test]
    fn unchanged_files_reuse_hash_without_reading() {
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"alpha").unwrap();

        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();
        let cp2 = capture(
            &f.blobs,
            &f.manifests,
            vec![a],
            Some(&cp1.manifest),
            true,
        )
        .unwrap();

        assert_eq!(cp2.stats.reused, 1);
        assert_eq!(cp2.stats.hashed, 0);
        // Identical state → identical (deduped) manifest.
        assert_eq!(cp1.id, cp2.id);
    }

    #[test]
    fn changed_files_are_rehashed() {
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"alpha").unwrap();
        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();

        fs::write(&a, b"alpha-2").unwrap();
        let cp2 = capture(
            &f.blobs,
            &f.manifests,
            vec![a.clone()],
            Some(&cp1.manifest),
            true,
        )
        .unwrap();

        assert_eq!(cp2.stats.hashed, 1);
        assert_ne!(cp1.id, cp2.id);
        let key = a.to_string_lossy().into_owned();
        assert_ne!(
            cp1.manifest.entries[&key].hash,
            cp2.manifest.entries[&key].hash
        );
    }

    #[test]
    fn vanished_files_are_skipped() {
        let f = fixture();
        let ghost = f.ws.join("ghost.txt");
        let cp = capture(&f.blobs, &f.manifests, vec![ghost], None, true).unwrap();
        assert_eq!(cp.stats.skipped, 1);
        assert!(cp.manifest.entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn mode_changes_are_recorded_even_on_stat_cache_hit() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let a = f.ws.join("a.sh");
        fs::write(&a, b"#!/bin/sh").unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o644)).unwrap();

        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();
        // chmod alone may leave size+mtime untouched → stat-cache hit path.
        fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
        let cp2 = capture(
            &f.blobs,
            &f.manifests,
            vec![a.clone()],
            Some(&cp1.manifest),
            true,
        )
        .unwrap();

        let key = a.to_string_lossy().into_owned();
        assert_eq!(cp2.manifest.entries[&key].mode, 0o755);
    }
}
