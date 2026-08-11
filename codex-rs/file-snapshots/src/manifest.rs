//! Checkpoint manifests: the inventory of one captured state.
//!
//! A manifest maps file paths to `(mode, size, mtime, content hash)`.
//! The `(size, mtime)` pair doubles as the persistent stat cache: a later
//! checkpoint reuses the recorded hash for any file whose stat fingerprint
//! is unchanged, so unchanged files are never re-read (RFC §6.1).
//!
//! Manifests are themselves content-addressed: the manifest id is the
//! SHA-256 of the canonical JSON serialization (`BTreeMap` keys give a
//! stable order), so identical states dedup to a single stored manifest.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Unix permission bits (`mode & 0o7777`); `0o644` on non-unix hosts.
    pub mode: u32,
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// SHA-256 of the file content, stored in the blob store.
    pub hash: String,
}

impl FileEntry {
    /// Stat-cache check: does `meta` match this entry's fingerprint?
    pub fn stat_matches(&self, meta: &fs::Metadata) -> bool {
        // A zero mtime is the marker for an entry whose fingerprint was never
        // trustworthy — captured within its own write's timestamp tick, or
        // reconstructed from a pre-edit image that had no stat at all. Such an
        // entry must never satisfy the fast path, including against a file
        // that genuinely carries an epoch mtime.
        if (self.mtime_secs, self.mtime_nanos) == (0, 0) {
            return false;
        }
        let (secs, nanos) = mtime_parts(meta);
        self.size == meta.len() && self.mtime_secs == secs && self.mtime_nanos == nanos
    }
}

/// Extract `(secs, nanos)` of the mtime relative to the unix epoch.
/// Pre-epoch or unreadable mtimes collapse to `(0, 0)`, which simply
/// disables the stat-cache fast path for that file.
pub fn mtime_parts(meta: &fs::Metadata) -> (i64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs() as i64, d.subsec_nanos()))
}

/// Permission bits to record for a file (`mode & 0o7777`; `0o644` off-unix).
pub fn mode_of(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Path → entry. Paths are stored as strings (lossy UTF-8) so
    /// manifests serialize portably; keys are absolute paths.
    pub entries: BTreeMap<String, FileEntry>,
    /// Paths this capture looked for and did not find.
    ///
    /// The only evidence a restore may delete on. Earlier designs inferred it
    /// instead — "the scan covered this directory exhaustively, so absence
    /// proves non-existence" — which made the inference only as sound as the
    /// scan was exhaustive, and therefore required scanning everything. That
    /// cost was unbounded and the claim was fragile: a manifest also carries
    /// paths the edit hook picked up from anywhere on disk, which no scan ever
    /// covered.
    ///
    /// Recording the observation directly costs one set and needs no premise.
    /// Every path the capture was asked about either has an entry or appears
    /// here, whether it came from the index, from recency, or from an edit
    /// that created a file where none was.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub absent: BTreeSet<String>,
}

impl Manifest {
    /// Content-addressed id: SHA-256 of the canonical JSON serialization.
    pub fn id(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(BlobStore::hash_bytes(&bytes))
    }
}

pub struct ManifestStore {
    root: PathBuf,
}

impl ManifestStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    /// Persist `manifest`, returning its id. Idempotent.
    pub fn save(&self, manifest: &Manifest) -> Result<String> {
        let id = manifest.id()?;
        let path = self.manifest_path(&id);
        if !path.exists() {
            let tmp = path.with_extension("tmp");
            let bytes = serde_json::to_vec_pretty(manifest)?;
            fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        }
        Ok(id)
    }

    pub fn load(&self, id: &str) -> Result<Manifest> {
        let path = self.manifest_path(id);
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SnapshotError::MissingManifest(id.to_string())
            } else {
                SnapshotError::io(&path, e)
            }
        })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Where `id` lives, so the sweep can ask how old it is.
    pub(crate) fn path_for(&self, id: &str) -> PathBuf {
        self.manifest_path(id)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.manifest_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Enumerate stored manifest ids (for garbage collection sweeps).
    pub fn ids(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json") {
                out.insert(id.to_string());
            }
        }
        Ok(out)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    fn entry(hash: &str) -> FileEntry {
        FileEntry {
            mode: 0o644,
            size: 1,
            mtime_secs: 10,
            mtime_nanos: 0,
            hash: hash.to_string(),
        }
    }

    #[test]
    fn id_is_stable_and_content_addressed() {
        let mut a = Manifest::default();
        a.entries.insert("/x".to_string(), entry("h1"));
        let mut b = Manifest::default();
        b.entries.insert("/x".to_string(), entry("h1"));
        assert_eq!(a.id().unwrap(), b.id().unwrap());

        b.entries.insert("/y".to_string(), entry("h2"));
        assert_ne!(a.id().unwrap(), b.id().unwrap());
    }

    #[test]
    fn save_load_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path().join("manifests")).unwrap();

        let mut m = Manifest::default();
        m.entries.insert("/a".to_string(), entry("abc"));

        let id1 = store.save(&m).unwrap();
        let id2 = store.save(&m).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.ids().unwrap().len(), 1);
        assert_eq!(store.load(&id1).unwrap(), m);
    }

    #[test]
    fn missing_manifest_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path().join("manifests")).unwrap();
        assert!(matches!(
            store.load("nope"),
            Err(SnapshotError::MissingManifest(_))
        ));
    }

    #[test]
    fn stat_matches_tracks_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"ab").unwrap();
        let meta = fs::metadata(&file).unwrap();
        let (secs, nanos) = mtime_parts(&meta);

        let e = FileEntry {
            mode: mode_of(&meta),
            size: 2,
            mtime_secs: secs,
            mtime_nanos: nanos,
            hash: "h".to_string(),
        };
        assert!(e.stat_matches(&meta));

        fs::write(&file, b"abc").unwrap();
        let meta2 = fs::metadata(&file).unwrap();
        assert!(!e.stat_matches(&meta2));
    }
}
