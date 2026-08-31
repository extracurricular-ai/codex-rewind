//! Content-addressed blob storage.
//!
//! Blobs are stored under `<root>/<first two hex chars>/<remaining hex>`,
//! keyed by the SHA-256 of their content. Identical content across files,
//! checkpoints, and threads is stored exactly once.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use sha2::Digest;
use sha2::Sha256;

use crate::error::Result;
use crate::error::SnapshotError;

/// Distinguishes concurrent writers *within* one process; the pid covers
/// the cross-process half.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    pub fn hash_bytes(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
    }

    /// Store `content`, returning its hash. Writing is atomic (tmp file +
    /// rename) and idempotent: existing blobs are never rewritten.
    ///
    /// Concurrent writers are not coordinated, and do not need to be. Content
    /// addressing means two writers racing for one hash are writing the same
    /// bytes, so there is nothing to arbitrate — only a temp file to avoid
    /// sharing. A lock would buy the same guarantee at the cost of a
    /// cross-platform dependency, stale-lock handling after a crash, and a
    /// queue in front of the tens-to-hundreds of blobs a single capture
    /// writes. This is what git does for the same reason.
    pub fn store_bytes(&self, content: &[u8]) -> Result<String> {
        let hash = Self::hash_bytes(content);
        let path = self.blob_path(&hash);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
            }
            // Unique per writer. Deriving the temp name from the hash alone
            // let two processes sharing a CODEX_HOME truncate each other's
            // in-flight file. The `.tmp` suffix stays last so `hashes()` keeps
            // recognising it — a name ending in anything else would be read
            // back as a blob hash.
            let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let tmp = path.with_extension(format!("{}.{counter}.tmp", std::process::id()));
            fs::write(&tmp, content).map_err(|e| SnapshotError::io(&tmp, e))?;
            if let Err(err) = fs::rename(&tmp, &path) {
                // Losing the race is success: whatever is at `path` has this
                // hash, so it is this content. Windows reaches here whenever a
                // reader holds the destination open, where Unix would have
                // replaced it silently.
                if !path.exists() {
                    let _ = fs::remove_file(&tmp);
                    return Err(SnapshotError::io(&path, err));
                }
                let _ = fs::remove_file(&tmp);
            }
        }
        Ok(hash)
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.blob_path(hash).exists()
    }

    pub fn load(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(hash);
        fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SnapshotError::MissingBlob(hash.to_string())
            } else {
                SnapshotError::io(&path, e)
            }
        })
    }

    /// Where `hash` lives, so the sweep can ask how old it is.
    pub(crate) fn path_for(&self, hash: &str) -> PathBuf {
        self.blob_path(hash)
    }

    pub fn remove(&self, hash: &str) -> Result<()> {
        let path = self.blob_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Enumerate every stored blob hash (for garbage collection sweeps).
    pub fn hashes(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let dirs = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        for dir in dirs {
            let dir = dir.map_err(|e| SnapshotError::io(&self.root, e))?;
            if !dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let prefix = dir.file_name().to_string_lossy().into_owned();
            let entries = fs::read_dir(dir.path()).map_err(|e| SnapshotError::io(dir.path(), e))?;
            for entry in entries {
                let entry = entry.map_err(|e| SnapshotError::io(dir.path(), e))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // A blob's name is the rest of its hex hash and nothing else.
                // Reconstructing a hash from whatever is present would have
                // the sweep delete files it never wrote, on the strength of a
                // name it invented — `.DS_Store` is enough to trigger it, and
                // so is any in-flight `.tmp`.
                if !name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                out.insert(format!("{prefix}{name}"));
            }
        }
        Ok(out)
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = hash.split_at(2.min(hash.len()));
        self.root.join(prefix).join(rest)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();

        let h1 = store.store_bytes(b"hello").unwrap();
        let h2 = store.store_bytes(b"hello").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(store.load(&h1).unwrap(), b"hello");
        assert_eq!(store.hashes().unwrap().len(), 1);

        let h3 = store.store_bytes(b"world").unwrap();
        assert_ne!(h1, h3);
        assert_eq!(store.hashes().unwrap().len(), 2);
    }

    #[test]
    fn missing_blob_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        assert!(matches!(
            store.load("deadbeef"),
            Err(crate::error::SnapshotError::MissingBlob(_))
        ));
    }

    /// `hashes()` reads directory names back as blob hashes, so anything the
    /// sweep is shown that is not a published blob becomes a fabricated hash —
    /// which GC would then happily delete. Two things reach it: this store's
    /// own in-flight temp files, and whatever else lands in the tree
    /// (`.DS_Store` is the everyday one on macOS).
    ///
    /// The temp path is built here the way `store_bytes` builds it, so a
    /// change to that naming rule that silently breaks the `.tmp` filter shows
    /// up as a failure rather than as a corrupted sweep.
    #[test]
    fn an_in_flight_write_is_invisible_to_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        let hash = store.store_bytes(b"published").unwrap();

        let published = store.path_for(&hash);
        let blob_dir = published.parent().unwrap();

        // Another writer, mid-write, using the same shape store_bytes does.
        let in_flight = published.with_extension(format!("{}.7.tmp", std::process::id()));
        fs::write(&in_flight, b"half").unwrap();
        // And a stray nobody put there on purpose.
        let stray = blob_dir.join(".DS_Store");
        fs::write(&stray, b"finder").unwrap();

        let seen = store.hashes().unwrap();
        assert_eq!(
            seen,
            BTreeSet::from([hash]),
            "only published blobs are hashes; an in-flight temp and a stray \
             file are neither"
        );
        assert!(
            in_flight.exists(),
            "the sweep must not touch another writer"
        );
    }

    /// Losing the rename race is success, not an error: whatever sits at the
    /// destination has this hash, so it is this content. Windows takes this
    /// path whenever a reader holds the destination open.
    #[test]
    fn a_blob_already_published_by_someone_else_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();

        let first = store.store_bytes(b"same bytes").unwrap();
        let again = store.store_bytes(b"same bytes").unwrap();

        assert_eq!(first, again);
        assert_eq!(store.load(&first).unwrap(), b"same bytes");
        assert_eq!(
            store.hashes().unwrap().len(),
            1,
            "no temp file survives a completed write"
        );
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        let h = store.store_bytes(b"x").unwrap();
        store.remove(&h).unwrap();
        store.remove(&h).unwrap();
        assert!(!store.contains(&h));
    }
}
