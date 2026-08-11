//! Per-thread snapshot logs and garbage collection (RFC §8).
//!
//! Each thread has an append-only log of `(turn_id, manifest_id)` refs.
//! Snapshot lifetime is tied to thread lifetime: removing a thread drops
//! its refs, and a mark-and-sweep pass then deletes unreferenced
//! manifests and blobs. No timers, no retention windows.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

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

    /// Whether a log file exists for `thread_id`. With session-scoped
    /// binding, log existence *is* the persisted "tracking enabled" state.
    pub fn exists(&self, thread_id: &str) -> bool {
        self.log_path(thread_id).exists()
    }

    /// Create an empty log for `thread_id` if none exists — the durable
    /// marker that this thread is tracking (RFC §6.4).
    pub fn ensure(&self, thread_id: &str) -> Result<()> {
        let path = self.log_path(thread_id);
        if path.exists() {
            return Ok(());
        }
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&ThreadLog::default())?;
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

/// How many restores a thread remembers. Undo only needs the most recent
/// one; the rest is history that would otherwise keep manifests alive.
const MAX_RESTORE_HISTORY: usize = 20;

/// A restore that has been applied, recorded so it can be undone.
///
/// Filed under the thread the rewind switched **to** — the one the user is
/// sitting in when they ask to undo. Filing it under the thread that
/// *performed* the rewind would lose it immediately, since a rewind leaves
/// that thread behind; filing it per workspace makes concurrent sessions in
/// one directory share a stack, where one session's undo pops another's
/// record. The destination is the only key that is both reachable and
/// private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRecord {
    /// The state the rewind put the workspace into.
    pub target_manifest_id: String,
    /// What it looked like just before — where an undo returns to.
    ///
    /// Only the files are recorded here. The conversation an undo returns to
    /// is the thread the rewind superseded, which is archived rather than
    /// copied: archiving moves a rollout out of the sessions directory, and
    /// thread lookup only searches that directory, so an archived thread
    /// cannot be resumed or continued behind our back. Keeping the identity
    /// rather than a copy also preserves the turn ids the snapshot store is
    /// keyed on — a rebuilt conversation gets fresh ones, which would sever
    /// every later rewind on that line from its snapshots.
    pub safety_manifest_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreLog {
    pub entries: Vec<RestoreRecord>,
}

/// Maps turn ids to the state captured at their start, and threads to
/// their restore history. Both are deliberately independent of any thread:
/// a fork inherits its parent's turn ids, so a turn resolves to the same
/// snapshot from every branch, and an undo works wherever the user happens
/// to be.
pub struct TurnIndex {
    turns_root: PathBuf,
    restores_root: PathBuf,
}

impl TurnIndex {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let turns_root = root.join("turns");
        let restores_root = root.join("restores");
        fs::create_dir_all(&turns_root).map_err(|e| SnapshotError::io(&turns_root, e))?;
        fs::create_dir_all(&restores_root).map_err(|e| SnapshotError::io(&restores_root, e))?;
        Ok(Self {
            turns_root,
            restores_root,
        })
    }

    /// Record the snapshot captured at `turn_id`'s start. Later writes win:
    /// a supplemental pre-edit attach extends that turn's capture.
    pub fn set_turn(&self, turn_id: &str, manifest_id: &str) -> Result<()> {
        let path = self.turn_path(turn_id);
        write_atomic(&path, manifest_id.as_bytes())
    }

    pub fn manifest_for_turn(&self, turn_id: &str) -> Result<Option<String>> {
        let path = self.turn_path(turn_id);
        match fs::read_to_string(&path) {
            Ok(id) => Ok(Some(id.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Every thread's restore history, for GC marking.
    pub fn restore_logs(&self) -> Result<Vec<RestoreLog>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.restores_root)
            .map_err(|e| SnapshotError::io(&self.restores_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.restores_root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|e| SnapshotError::io(entry.path(), e))?;
            out.push(serde_json::from_slice(&bytes)?);
        }
        Ok(out)
    }

    pub fn all_manifest_ids(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let entries =
            fs::read_dir(&self.turns_root).map_err(|e| SnapshotError::io(&self.turns_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.turns_root, e))?;
            if let Ok(id) = fs::read_to_string(entry.path()) {
                out.insert(id.trim().to_string());
            }
        }
        for log in self.all_restore_logs()? {
            for record in log.entries {
                out.insert(record.target_manifest_id);
                out.insert(record.safety_manifest_id);
            }
        }
        Ok(out)
    }

    pub fn push_restore(&self, thread_id: &str, record: RestoreRecord) -> Result<()> {
        let mut log = self.restore_log(thread_id)?;
        log.entries.push(record);
        // Undo reaches back one step, so a long tail only pins storage.
        if log.entries.len() > MAX_RESTORE_HISTORY {
            let excess = log.entries.len() - MAX_RESTORE_HISTORY;
            log.entries.drain(..excess);
        }
        let path = self.restore_path(thread_id);
        write_atomic(&path, &serde_json::to_vec_pretty(&log)?)
    }

    /// The most recent restore this thread has to undo, if any.
    pub fn last_restore(&self, thread_id: &str) -> Result<Option<RestoreRecord>> {
        Ok(self.restore_log(thread_id)?.entries.pop())
    }

    /// Discard the most recent restore record, because it has just been
    /// reversed. The log is a stack: rewinds push, undos pop, so repeated
    /// undos walk back through repeated rewinds instead of oscillating
    /// between the last two states.
    pub fn pop_restore(&self, thread_id: &str) -> Result<Option<RestoreRecord>> {
        let mut log = self.restore_log(thread_id)?;
        let popped = log.entries.pop();
        if popped.is_some() {
            let path = self.restore_path(thread_id);
            write_atomic(&path, &serde_json::to_vec_pretty(&log)?)?;
        }
        Ok(popped)
    }

    /// The manifests a thread's undo records name, for a sweep to consider
    /// once those records are dropped.
    pub fn all_restores_for(&self, thread_id: &str) -> Result<Vec<String>> {
        let log = self.restore_log(thread_id)?;
        Ok(log
            .entries
            .into_iter()
            .flat_map(|record| [record.target_manifest_id, record.safety_manifest_id])
            .collect())
    }

    /// Drop a thread's undo records outright, for when the conversation they
    /// belong to is deleted.
    ///
    /// Separate from `RefStore::remove` because the two files answer to
    /// different lifetimes: a thread's log is what it captured, its restore
    /// log is what was handed *to* it, and a rewind writes the second under a
    /// thread that may have no log of its own yet. Only deleting the
    /// conversation ends both — and leaving this behind would strand a GC
    /// root, pinning the manifests it names for good.
    pub fn remove_restores(&self, thread_id: &str) -> Result<()> {
        let path = self.restore_path(thread_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    fn restore_log(&self, thread_id: &str) -> Result<RestoreLog> {
        let path = self.restore_path(thread_id);
        match fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RestoreLog::default()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    fn all_restore_logs(&self) -> Result<Vec<RestoreLog>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.restores_root)
            .map_err(|e| SnapshotError::io(&self.restores_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.restores_root, e))?;
            if let Ok(bytes) = fs::read(entry.path())
                && let Ok(log) = serde_json::from_slice::<RestoreLog>(&bytes)
            {
                out.push(log);
            }
        }
        Ok(out)
    }

    /// Drop index entries for turns no thread holds any more.
    pub fn retain_turns(&self, live_turn_ids: &BTreeSet<String>) -> Result<()> {
        let entries =
            fs::read_dir(&self.turns_root).map_err(|e| SnapshotError::io(&self.turns_root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.turns_root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") || live_turn_ids.contains(&name) {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|e| SnapshotError::io(entry.path(), e))?;
        }
        Ok(())
    }

    fn turn_path(&self, turn_id: &str) -> PathBuf {
        self.turns_root.join(safe_file_name(turn_id))
    }

    fn restore_path(&self, thread_id: &str) -> PathBuf {
        self.restores_root
            .join(format!("{}.json", safe_file_name(thread_id)))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| SnapshotError::io(path, e))
}

/// Thread and turn ids are UUID-like; anything else is mapped to a
/// filename-safe character set.
fn safe_file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    pub manifests_kept: usize,
    pub manifests_removed: usize,
    pub blobs_kept: usize,
    pub blobs_removed: usize,
}

/// How new a file has to be for the sweep to leave it alone regardless.
///
/// A capture publishes in three steps — blobs, then the manifest, then the
/// log entry — and none of it is atomic across processes. A sweep that reads
/// the logs before that last step but lists the files after it would delete a
/// snapshot a live session believes it holds, which is worse than any amount
/// of retained garbage. Nothing coordinates the two: this workspace is
/// explicitly multi-session, and the sweep runs from whichever process
/// deleted a conversation.
///
/// Git answers the same race the same way rather than by locking
/// (`gc.pruneExpire`): fresh objects are simply never pruned, and whatever
/// garbage is among them waits for the next sweep. Reclamation is delayed;
/// nothing is lost.
const GC_GRACE: Duration = Duration::from_secs(300);

/// Sweep only `doomed` — manifests that belonged to threads just deleted.
///
/// The grace window does not apply here, and must not: a candidate is only
/// considered because a thread that has just been removed named it, so no
/// live session can be in the middle of publishing it. Waiting would defeat
/// the point, since someone deleting a conversation is asking for its
/// contents to be gone now, not in five minutes — and with the sweep only
/// running on deletion, "later" can mean never.
///
/// Anything still reachable from a surviving thread stays: manifests are
/// shared across forks, and the deleted thread naming one says nothing about
/// whether its siblings still do.
pub fn collect_garbage_for(
    refs: &RefStore,
    turns: &TurnIndex,
    manifests: &ManifestStore,
    blobs: &BlobStore,
    doomed: &BTreeSet<String>,
) -> Result<GcStats> {
    let live_manifests = live_manifest_ids(refs, turns)?;
    let mut stats = GcStats::default();

    // Blobs are only candidates if a manifest being removed named them.
    let mut orphan_blobs = BTreeSet::new();
    for id in doomed {
        if live_manifests.contains(id) {
            stats.manifests_kept += 1;
            continue;
        }
        let Ok(manifest) = manifests.load(id) else {
            continue;
        };
        for entry in manifest.entries.values() {
            orphan_blobs.insert(entry.hash.clone());
        }
        manifests.remove(id)?;
        stats.manifests_removed += 1;
    }

    // A blob survives if any manifest still on disk references it.
    for id in manifests.ids()? {
        for entry in manifests.load(&id)?.entries.values() {
            orphan_blobs.remove(&entry.hash);
        }
    }
    for hash in orphan_blobs {
        blobs.remove(&hash)?;
        stats.blobs_removed += 1;
    }
    Ok(stats)
}

/// Manifests some surviving thread log or turn still points at.
fn live_manifest_ids(refs: &RefStore, turns: &TurnIndex) -> Result<BTreeSet<String>> {
    let mut live_manifests = BTreeSet::new();
    let mut live_turn_ids = BTreeSet::new();
    for log in refs.thread_logs()? {
        for entry in log.entries {
            live_turn_ids.insert(safe_file_name(&entry.turn_id));
            live_manifests.insert(entry.manifest_id);
        }
    }
    turns.retain_turns(&live_turn_ids)?;
    live_manifests.extend(turns.all_manifest_ids()?);
    Ok(live_manifests)
}

/// Mark-and-sweep: manifests referenced by any thread log are live; blobs
/// referenced by any live manifest are live; everything else is removed —
/// unless it was written within `GC_GRACE`, which is never touched.
pub fn collect_garbage(
    refs: &RefStore,
    turns: &TurnIndex,
    manifests: &ManifestStore,
    blobs: &BlobStore,
) -> Result<GcStats> {
    let cutoff = SystemTime::now()
        .checked_sub(GC_GRACE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    // Unreadable or undatable files count as young: the sweep declines to
    // delete anything it cannot age.
    let settled = |path: &Path| -> bool {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|written| written <= cutoff)
    };
    let mut live_manifests = BTreeSet::new();
    let mut live_turn_ids = BTreeSet::new();
    for log in refs.thread_logs()? {
        for entry in log.entries {
            live_turn_ids.insert(safe_file_name(&entry.turn_id));
            live_manifests.insert(entry.manifest_id);
        }
    }
    // A turn only matters while some thread still holds it.
    turns.retain_turns(&live_turn_ids)?;
    live_manifests.extend(turns.all_manifest_ids()?);

    let mut stats = GcStats::default();
    let mut live_blobs = BTreeSet::new();
    for id in manifests.ids()? {
        // A manifest too young to sweep is also too young to trust as dead:
        // its blobs stay live with it, or the next capture to reference it
        // would find its contents gone.
        if live_manifests.contains(&id) || !settled(&manifests.path_for(&id)) {
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
        if live_blobs.contains(&hash) || !settled(&blobs.path_for(&hash)) {
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

    /// Backdate a file past `GC_GRACE` so a sweep will consider it.
    ///
    /// Tests write everything milliseconds before they assert, which is
    /// exactly the state the grace window exists to protect. Reaching for the
    /// clock is the only way to exercise the other side of it.
    fn age_out(path: &Path) {
        let when = SystemTime::now() - GC_GRACE - Duration::from_secs(60);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

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
        let turns = TurnIndex::open(dir.path()).unwrap();
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

        // Everything written a moment ago is inside the grace window, so a
        // sweep right now must take nothing at all — the point being that a
        // concurrent capture's not-yet-referenced manifest is indistinguishable
        // from garbage, and guessing wrong destroys a live session's snapshot.
        let stats = collect_garbage(&refs, &turns, &manifests, &blobs).unwrap();
        assert_eq!(
            (stats.manifests_removed, stats.blobs_removed),
            (0, 0),
            "fresh objects are never swept"
        );
        assert!(manifests.load(&dead_id).is_ok());

        age_out(&manifests.path_for(&dead_id));
        age_out(&manifests.path_for(&live_id));
        age_out(&blobs.path_for(&dead_hash));
        age_out(&blobs.path_for(&live_hash));

        let stats = collect_garbage(&refs, &turns, &manifests, &blobs).unwrap();
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
        age_out(&manifests.path_for(&live_id));
        age_out(&blobs.path_for(&live_hash));
        let stats = collect_garbage(&refs, &turns, &manifests, &blobs).unwrap();
        assert_eq!(stats.manifests_removed, 1);
        assert_eq!(stats.blobs_removed, 1);
        assert_eq!(manifests.ids().unwrap().len(), 0);
        assert_eq!(blobs.hashes().unwrap().len(), 0);
    }
}
