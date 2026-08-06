//! `SnapshotStore`: the facade tying blobs, manifests, thread logs,
//! checkpoints, restore, and GC together under one root directory
//! (`CODEX_HOME/file_snapshots/` in production).

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::blob::BlobStore;
use crate::checkpoint::Checkpoint;
use crate::checkpoint::capture;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::Manifest;
use crate::manifest::ManifestStore;
use crate::refs::GcStats;
use crate::refs::RefStore;
use crate::refs::SnapshotRef;
use crate::refs::collect_garbage;
use crate::restore::ApplyStats;
use crate::restore::RestorePlan;
use crate::restore::apply_plan;
use crate::restore::plan_restore;

/// Turn-id prefix used for the safety checkpoint recorded before a restore.
pub const SAFETY_TURN_PREFIX: &str = "safety-restore:";

pub struct SnapshotStore {
    blobs: BlobStore,
    manifests: ManifestStore,
    refs: RefStore,
    root: PathBuf,
}

#[derive(Debug)]
pub struct RestoreOutcome {
    /// The safety checkpoint of the pre-restore state; restoring to it
    /// undoes this restore (redo).
    pub safety_manifest_id: String,
    pub plan: RestorePlan,
    pub stats: ApplyStats,
}

impl SnapshotStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        Ok(Self {
            blobs: BlobStore::open(root.join("blobs"))?,
            manifests: ManifestStore::open(root.join("manifests"))?,
            refs: RefStore::open(root.join("refs"))?,
            root,
        })
    }

    /// Capture a checkpoint of `files` for `thread_id` and append it to
    /// the thread's snapshot log. The previous checkpoint (if any) serves
    /// as the stat cache. `complete` declares whether `files` covers the
    /// entire tracking scope (workspace scan) or a bounded subset.
    pub fn checkpoint(
        &self,
        thread_id: &str,
        turn_id: &str,
        files: impl IntoIterator<Item = PathBuf>,
        complete: bool,
    ) -> Result<Checkpoint> {
        let prev = self.latest_manifest(thread_id)?;
        let cp = capture(&self.blobs, &self.manifests, files, prev.as_ref(), complete)?;
        self.refs.append(
            thread_id,
            SnapshotRef {
                turn_id: turn_id.to_string(),
                manifest_id: cp.id.clone(),
            },
        )?;
        Ok(cp)
    }

    /// Whether `thread_id` has a snapshot log. With session-scoped binding,
    /// log existence *is* the persisted "tracking enabled" state (RFC §6.4).
    pub fn thread_exists(&self, thread_id: &str) -> bool {
        self.refs.exists(thread_id)
    }

    /// Create the (empty) snapshot log for `thread_id` if missing — called
    /// at session start when the feature is enabled, marking the thread as
    /// tracking for its whole lifetime.
    pub fn ensure_thread(&self, thread_id: &str) -> Result<()> {
        self.refs.ensure(thread_id)
    }

    /// Retroactively record a pre-edit image for `path_key` under `turn_id`
    /// (RFC §6.3: pre-images from the apply-patch pipeline).
    ///
    /// If the latest manifest already covers the path (the turn-start scan
    /// saw it) or the file did not exist before the edit (`pre_content` is
    /// `None` — absence is already implied by absence from the manifest),
    /// nothing is appended and `Ok(None)` is returned. Otherwise a
    /// supplemental manifest (latest + the pre-edit entry) is appended
    /// under the same turn id, so restoring to this turn recovers the
    /// pre-edit content; returns its id. Restore resolution for a turn with
    /// several entries should pick the last (most complete) one.
    pub fn attach_pre_edit(
        &self,
        thread_id: &str,
        turn_id: &str,
        path_key: &str,
        pre_content: Option<&[u8]>,
    ) -> Result<Option<String>> {
        let latest = self.latest_manifest(thread_id)?.unwrap_or_default();
        if latest.entries.contains_key(path_key) {
            return Ok(None);
        }
        let Some(content) = pre_content else {
            return Ok(None);
        };

        let hash = self.blobs.store_bytes(content)?;
        let mut manifest = latest;
        manifest.entries.insert(
            path_key.to_string(),
            crate::manifest::FileEntry {
                // Pre-edit images come from patch content, not the
                // filesystem: no stat is available. The zero fingerprint
                // simply disables the stat-cache fast path for this entry.
                mode: 0o644,
                size: content.len() as u64,
                mtime_secs: 0,
                mtime_nanos: 0,
                hash,
            },
        );
        let id = self.manifests.save(&manifest)?;
        self.refs.append(
            thread_id,
            SnapshotRef {
                turn_id: turn_id.to_string(),
                manifest_id: id.clone(),
            },
        )?;
        Ok(Some(id))
    }

    /// Resolve "the state at `turn_id`'s start" to a manifest id: the
    /// **last** log entry recorded under that turn id (later entries for a
    /// turn are supplemental pre-edit attaches extending the turn-start
    /// scan, so last = most complete). `None` if the turn has no snapshot.
    pub fn manifest_id_for_turn(&self, thread_id: &str, turn_id: &str) -> Result<Option<String>> {
        let log = self.refs.load(thread_id)?;
        Ok(log
            .entries
            .iter()
            .rev()
            .find(|entry| entry.turn_id == turn_id)
            .map(|entry| entry.manifest_id.clone()))
    }

    /// Union of every path key observed across the thread's manifests.
    /// Used to build the safety-checkpoint scope for a restore: it covers
    /// outside-workspace paths recorded via pre-edit attach that a plain
    /// workspace scan would miss.
    pub fn tracked_paths(&self, thread_id: &str) -> Result<std::collections::BTreeSet<String>> {
        let mut out = std::collections::BTreeSet::new();
        for (_, manifest) in self.thread_history(thread_id)? {
            out.extend(manifest.entries.into_keys());
        }
        Ok(out)
    }

    /// Fork inheritance (RFC §6.5): copy the source thread's log entries up
    /// to and including the **last** entry for `through_turn_id` into
    /// `new_thread_id`'s log. Creates the new log if missing (which also
    /// marks the forked thread as tracking). Manifests are shared, not
    /// copied — GC marks from every log. Returns the number of entries
    /// inherited; 0 if the source has no entry for that turn.
    pub fn inherit_log(
        &self,
        source_thread_id: &str,
        new_thread_id: &str,
        through_turn_id: &str,
    ) -> Result<usize> {
        let log = self.refs.load(source_thread_id)?;
        let Some(cut) = log
            .entries
            .iter()
            .rposition(|entry| entry.turn_id == through_turn_id)
        else {
            self.refs.ensure(new_thread_id)?;
            return Ok(0);
        };
        self.refs.ensure(new_thread_id)?;
        for entry in &log.entries[..=cut] {
            self.refs.append(new_thread_id, entry.clone())?;
        }
        Ok(cut + 1)
    }

    /// The thread's snapshot log with each manifest loaded, in capture order.
    pub fn thread_history(&self, thread_id: &str) -> Result<Vec<(SnapshotRef, Manifest)>> {
        let log = self.refs.load(thread_id)?;
        let mut out = Vec::with_capacity(log.entries.len());
        for entry in log.entries {
            let manifest = self.manifests.load(&entry.manifest_id)?;
            out.push((entry, manifest));
        }
        Ok(out)
    }

    pub fn latest_manifest(&self, thread_id: &str) -> Result<Option<Manifest>> {
        let log = self.refs.load(thread_id)?;
        match log.entries.last() {
            Some(entry) => Ok(Some(self.manifests.load(&entry.manifest_id)?)),
            None => Ok(None),
        }
    }

    /// Restore `thread_id`'s tracked state to `target_manifest_id`.
    ///
    /// `current_files` is the present tracked set (it is re-captured as the
    /// safety checkpoint first, so the restore is reversible); `is_protected`
    /// is the symmetric-ignore predicate over manifest path keys, evaluated
    /// against the *current* ignore rules (RFC rule 5).
    pub fn restore_to(
        &self,
        thread_id: &str,
        target_manifest_id: &str,
        current_files: impl IntoIterator<Item = PathBuf>,
        current_complete: bool,
        is_protected: &dyn Fn(&str) -> bool,
    ) -> Result<RestoreOutcome> {
        // 1. Safety checkpoint (appends to the log → becomes history.last()).
        let safety = self.checkpoint(
            thread_id,
            &format!("{SAFETY_TURN_PREFIX}{target_manifest_id}"),
            current_files,
            current_complete,
        )?;

        // 2. Locate the target (latest occurrence: with content-addressed
        // manifests the same state may recur; deleting only files born
        // after its most recent occurrence is the conservative reading).
        let history = self.thread_history(thread_id)?;
        let manifests: Vec<Manifest> = history.into_iter().map(|(_, m)| m).collect();
        let target_index = manifests
            .iter()
            .rposition(|m| m.id().is_ok_and(|id| id == target_manifest_id))
            .ok_or_else(|| SnapshotError::MissingManifest(target_manifest_id.to_string()))?;

        // 3-4. Plan (writes + witnessed-birth deletes) and apply.
        let plan = plan_restore(&manifests, target_index, is_protected);
        let stats = apply_plan(&self.blobs, &plan)?;

        Ok(RestoreOutcome {
            safety_manifest_id: safety.id,
            plan,
            stats,
        })
    }

    /// Drop a thread's snapshot log (its data becomes garbage for `gc`).
    pub fn remove_thread(&self, thread_id: &str) -> Result<()> {
        self.refs.remove(thread_id)
    }

    /// Mark-and-sweep unreferenced manifests and blobs.
    pub fn gc(&self) -> Result<GcStats> {
        collect_garbage(&self.refs, &self.manifests, &self.blobs)
    }

    /// Total bytes on disk under the store root (for `/status` display).
    pub fn disk_usage(&self) -> Result<u64> {
        fn dir_size(path: &Path) -> std::io::Result<u64> {
            let mut total = 0;
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    total += dir_size(&entry.path())?;
                } else {
                    total += meta.len();
                }
            }
            Ok(total)
        }
        dir_size(&self.root).map_err(|e| SnapshotError::io(&self.root, e))
    }
}
