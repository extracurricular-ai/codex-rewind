//! Content-addressed blob storage.
//!
//! Blobs are stored under `<root>/<first two hex chars>/<remaining hex>`,
//! keyed by the SHA-256 of their content. Identical content across files,
//! checkpoints, and threads is stored exactly once.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use crate::error::Result;
use crate::error::SnapshotError;

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
    pub fn store_bytes(&self, content: &[u8]) -> Result<String> {
        let hash = Self::hash_bytes(content);
        let path = self.blob_path(&hash);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
            }
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, content).map_err(|e| SnapshotError::io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        }
        Ok(hash)
    }

    /// Read and store the file at `path`, returning `(hash, size)`.
    ///
    /// The file is read exactly once so the stored blob and the returned
    /// hash are always consistent even if the file changes concurrently.
    // TODO(reflink): for large files, stream-hash then clone via
    // copy_file_range/FICLONE instead of buffering the whole content.
    pub fn store_file(&self, path: &Path) -> Result<(String, u64)> {
        let content = fs::read(path).map_err(|e| SnapshotError::io(path, e))?;
        let size = content.len() as u64;
        let hash = self.store_bytes(&content)?;
        Ok((hash, size))
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
                if name.ends_with(".tmp") {
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
    fn store_file_matches_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, b"content").unwrap();

        let (hash, size) = store.store_file(&file).unwrap();
        assert_eq!(size, 7);
        assert_eq!(hash, BlobStore::hash_bytes(b"content"));
        assert_eq!(store.load(&hash).unwrap(), b"content");
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
