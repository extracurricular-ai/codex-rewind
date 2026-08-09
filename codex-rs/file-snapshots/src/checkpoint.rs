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
use std::time::Duration;
use std::time::SystemTime;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::manifest::FileEntry;
use crate::manifest::Manifest;
use crate::manifest::ManifestStore;
use crate::manifest::mode_of;
use crate::manifest::mtime_parts;

/// How recently a file may have been written before its `(size, mtime)`
/// fingerprint stops being proof that it is unchanged.
///
/// Two writes inside one timestamp tick are indistinguishable by stat alone,
/// so a file touched moments ago can differ while looking identical — git
/// calls such entries "racily clean". Re-reading anything this fresh costs
/// little, because a file just written is the one most likely to have
/// changed, and it removes a failure mode where a snapshot silently records
/// the previous contents.
const RACY_WINDOW: Duration = Duration::from_secs(2);

fn fingerprint_is_trustworthy(meta: &fs::Metadata, now: SystemTime) -> bool {
    meta.modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= RACY_WINDOW)
}

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
    let now = SystemTime::now();

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
            && fingerprint_is_trustworthy(&meta, now)
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

    fn set_mtime_secs_ago(path: &std::path::Path, secs: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs);
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    fn set_mtime_parts(path: &std::path::Path, secs: i64, nanos: u32) {
        let when = SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nanos);
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
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
        // Age the file past the racy window so its fingerprint is proof.
        set_mtime_secs_ago(&a, 60);

        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();
        let cp2 = capture(
            &f.blobs,
            &f.manifests,
            vec![a.clone()],
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
    fn a_just_written_file_is_reread_even_if_its_fingerprint_matches() {
        // Two writes can land in one timestamp tick, so a fresh file that
        // looks unchanged may not be. Same size on purpose: only re-reading
        // can tell these apart.
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"v1").unwrap();
        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None, true).unwrap();

        // Rewrite with identical size and forge the old fingerprint, exactly
        // what a same-tick write looks like.
        let key = a.to_string_lossy().into_owned();
        let stale = cp1.manifest.entries[&key].clone();
        fs::write(&a, b"v2").unwrap();
        set_mtime_parts(&a, stale.mtime_secs, stale.mtime_nanos);

        let cp2 = capture(
            &f.blobs,
            &f.manifests,
            vec![a.clone()],
            Some(&cp1.manifest),
            true,
        )
        .unwrap();
        assert_eq!(cp2.stats.hashed, 1, "a racily-clean entry must be re-read");
        assert_eq!(
            f.blobs
                .load(&cp2.manifest.entries[&key].hash)
                .unwrap()
                .as_slice(),
            b"v2"
        );
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
