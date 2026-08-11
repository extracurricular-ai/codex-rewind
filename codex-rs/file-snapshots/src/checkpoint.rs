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

/// How close to the moment of capture a file's mtime may be before its
/// `(size, mtime)` fingerprint stops being proof that it is unchanged.
///
/// Two writes inside one timestamp tick are indistinguishable by stat alone,
/// so a file written again right after we read it differs while looking
/// identical — git calls such entries "racily clean". The hash cannot go into
/// the fingerprint to settle this: the fingerprint exists precisely so the
/// file need not be read, and the hash is the value being cached, not part of
/// the key.
///
/// So raciness is decided once, when the entry is written, and recorded in the
/// entry itself (below). Judging it later — "is this file young *now*" —
/// would only cover the first couple of seconds, and a second write in the
/// original tick would be trusted forever after that.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// The fingerprint stored for an entry captured too soon after its own last
/// write to be sure we read the final bytes.
///
/// A zero mtime never matches a real one, so the entry can only ever be
/// re-read — the fast path stays disabled for it until some later capture
/// sees the file settled and records a real fingerprint. The same convention
/// marks pre-edit images, which have no stat of their own.
const RACY_FINGERPRINT: (i64, u32) = (0, 0);

/// Whether `meta`'s mtime is far enough from `now` that a matching
/// fingerprint proves the contents are unchanged.
fn settled_before(meta: &fs::Metadata, now: SystemTime) -> bool {
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
/// serves as the stat cache. `files` is what the capture is asked about:
/// each one either gets an entry or is recorded as absent, and that record
/// is the only thing a later restore may delete on. Paths that exist but
/// cannot be read (unreadable, non-regular) are skipped rather than declared
/// absent — a checkpoint records what it verified, and a read failure
/// verifies nothing.
pub fn capture(
    blobs: &BlobStore,
    manifests: &ManifestStore,
    files: impl IntoIterator<Item = PathBuf>,
    prev: Option<&Manifest>,
) -> Result<Checkpoint> {
    capture_at(blobs, manifests, files, prev, SystemTime::now())
}

/// `capture` with the capture instant supplied, so that the racy-window
/// decision can be exercised without waiting for wall-clock time to pass.
fn capture_at(
    blobs: &BlobStore,
    manifests: &ManifestStore,
    files: impl IntoIterator<Item = PathBuf>,
    prev: Option<&Manifest>,
    now: SystemTime,
) -> Result<Checkpoint> {
    let mut manifest = Manifest::default();
    let mut stats = CheckpointStats::default();

    for path in files {
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Asked about, and not there. Recorded rather than skipped:
                // this is the whole of a restore's licence to delete, and the
                // only alternative is inferring absence from the scan having
                // been exhaustive — a premise that costs a full tree walk and
                // still does not cover paths the edit hook contributed.
                manifest.absent.insert(path.to_string_lossy().into_owned());
                continue;
            }
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
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
        let (mtime_secs, mtime_nanos) = if settled_before(&meta, now) {
            mtime_parts(&meta)
        } else {
            RACY_FINGERPRINT
        };
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

        let cp = capture(&f.blobs, &f.manifests, vec![a.clone()], None).unwrap();
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

        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None).unwrap();
        let cp2 = capture(&f.blobs, &f.manifests, vec![a], Some(&cp1.manifest)).unwrap();

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
        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None).unwrap();

        // Rewrite with identical size and forge the old fingerprint, exactly
        // what a same-tick write looks like.
        let key = a.to_string_lossy().into_owned();
        let stale = cp1.manifest.entries[&key].clone();
        fs::write(&a, b"v2").unwrap();
        set_mtime_parts(&a, stale.mtime_secs, stale.mtime_nanos);

        let cp2 = capture(&f.blobs, &f.manifests, vec![a], Some(&cp1.manifest)).unwrap();
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
    fn time_passing_never_restores_trust_in_a_racy_capture() {
        // The dangerous write is the one that lands in the same timestamp
        // tick as the read that preceded it: nothing observable changes, so
        // it can only be caught by refusing to trust that reading ever again.
        // Asking "was this file written recently?" at the *next* capture is
        // not enough — by then the write is old, and the stale hash would be
        // reused forever.
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"v1").unwrap();

        let captured_at = fs::symlink_metadata(&a).unwrap().modified().unwrap();
        let cp1 = capture_at(&f.blobs, &f.manifests, vec![a.clone()], None, captured_at).unwrap();

        // Second write inside the tick: same length, and the stat restored to
        // exactly what the capture above saw.
        let key = a.to_string_lossy().into_owned();
        fs::write(&a, b"v2").unwrap();
        let (orig_secs, orig_nanos) = {
            let d = captured_at.duration_since(SystemTime::UNIX_EPOCH).unwrap();
            (d.as_secs() as i64, d.subsec_nanos())
        };
        set_mtime_parts(&a, orig_secs, orig_nanos);

        // A minute later the file looks entirely settled.
        let cp2 = capture_at(
            &f.blobs,
            &f.manifests,
            vec![a.clone()],
            Some(&cp1.manifest),
            captured_at + Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(
            cp2.stats.hashed, 1,
            "an entry captured inside its own write's tick stays untrusted"
        );
        assert_eq!(
            f.blobs
                .load(&cp2.manifest.entries[&key].hash)
                .unwrap()
                .as_slice(),
            b"v2"
        );

        // And the doubt is not permanent: once a capture sees the file
        // settled, the fingerprint is recorded and the fast path resumes.
        let cp3 = capture_at(
            &f.blobs,
            &f.manifests,
            vec![a],
            Some(&cp2.manifest),
            captured_at + Duration::from_secs(120),
        )
        .unwrap();
        assert_eq!(cp3.stats.reused, 1, "a settled file is cached again");
    }

    #[test]
    fn changed_files_are_rehashed() {
        let f = fixture();
        let a = f.ws.join("a.txt");
        fs::write(&a, b"alpha").unwrap();
        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None).unwrap();

        fs::write(&a, b"alpha-2").unwrap();
        let cp2 = capture(&f.blobs, &f.manifests, vec![a.clone()], Some(&cp1.manifest)).unwrap();

        assert_eq!(cp2.stats.hashed, 1);
        assert_ne!(cp1.id, cp2.id);
        let key = a.to_string_lossy().into_owned();
        assert_ne!(
            cp1.manifest.entries[&key].hash,
            cp2.manifest.entries[&key].hash
        );
    }

    #[test]
    fn a_path_that_is_not_there_is_recorded_as_not_there() {
        // The distinction the whole deletion rule rests on: a capture that
        // looked and found nothing says something, and it is the only thing
        // that ever licenses a restore to delete. Skipping it silently — the
        // old behaviour — threw that evidence away.
        let f = fixture();
        let ghost = f.ws.join("ghost.txt");
        let cp = capture(&f.blobs, &f.manifests, vec![ghost.clone()], None).unwrap();
        assert!(cp.manifest.entries.is_empty());
        assert!(
            cp.manifest
                .absent
                .contains(&ghost.to_string_lossy().into_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn mode_changes_are_recorded_even_on_stat_cache_hit() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let a = f.ws.join("a.sh");
        fs::write(&a, b"#!/bin/sh").unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o644)).unwrap();

        let cp1 = capture(&f.blobs, &f.manifests, vec![a.clone()], None).unwrap();
        // chmod alone may leave size+mtime untouched → stat-cache hit path.
        fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
        let cp2 = capture(&f.blobs, &f.manifests, vec![a.clone()], Some(&cp1.manifest)).unwrap();

        let key = a.to_string_lossy().into_owned();
        assert_eq!(cp2.manifest.entries[&key].mode, 0o755);
    }
}
