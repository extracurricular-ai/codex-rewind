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
use crate::refs::RestoreRecord;
use crate::refs::SnapshotRef;
use crate::refs::TurnIndex;
use crate::refs::collect_garbage;
use crate::refs::collect_garbage_for;
use crate::restore::ApplyStats;
use crate::restore::RestorePlan;
use crate::restore::apply_plan;
use crate::restore::plan_restore;
use tracing::info;
use tracing::warn;

/// Turn-id prefix used for the safety checkpoint recorded before a restore.
pub const SAFETY_TURN_PREFIX: &str = "safety-restore:";

/// The state a restore moves the workspace to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreTarget {
    manifest_id: String,
}

/// Which direction a restore moves the workspace's undo history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreKind {
    /// Going back in the conversation; leaves an undo behind.
    Rewind,
    /// Reversing the most recent rewind; consumes that undo, so undoing
    /// twice walks back through two rewinds rather than oscillating.
    Undo,
}

impl RestoreTarget {
    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }
}

pub struct SnapshotStore {
    blobs: BlobStore,
    manifests: ManifestStore,
    refs: RefStore,
    turns: TurnIndex,
    root: PathBuf,
}

#[derive(Debug)]
pub struct RestoreOutcome {
    /// The pre-restore state; restoring to it undoes this restore (redo).
    pub safety: RestoreTarget,
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
            turns: TurnIndex::open(&root)?,
            root,
        })
    }

    /// Capture a checkpoint of `files` for `thread_id` and append it to
    /// the thread's snapshot log. The previous checkpoint (if any) serves
    /// as the stat cache. Every path in `files` is either recorded or noted
    /// as absent, and absence is the only evidence a later restore has for
    /// deleting anything — so what is passed here bounds what can be undone.
    pub fn checkpoint(
        &self,
        thread_id: &str,
        turn_id: &str,
        files: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Checkpoint> {
        let prev = self.latest_manifest(thread_id)?;
        let cp = capture(&self.blobs, &self.manifests, files, prev.as_ref())?;
        self.refs.append(
            thread_id,
            SnapshotRef {
                turn_id: turn_id.to_string(),
                manifest_id: cp.id.clone(),
            },
        )?;
        // Turn ids survive forking, so this index is what lets a rewind
        // resolve the same state from any branch.
        self.turns.set_turn(turn_id, &cp.id)?;
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
            // The edit created this file. Record that it did not exist rather
            // than recording nothing: absence has to be *stated* to be usable
            // as evidence outside a complete scan, which is precisely the case
            // here — the path may well be outside the scanned scope.
            if latest.absent.contains(path_key) {
                return Ok(None);
            }
            let mut manifest = latest;
            manifest.absent.insert(path_key.to_string());
            let id = self.manifests.save(&manifest)?;
            self.refs.append(
                thread_id,
                SnapshotRef {
                    turn_id: turn_id.to_string(),
                    manifest_id: id.clone(),
                },
            )?;
            self.turns.set_turn(turn_id, &id)?;
            return Ok(Some(id));
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
        // A supplemental attach extends this turn's capture, so it becomes
        // the state the turn resolves to.
        self.turns.set_turn(turn_id, &id)?;
        Ok(Some(id))
    }

    /// The state captured at `turn_id`'s start (or extended by a later
    /// pre-edit attach). Resolved through the turn index rather than any
    /// thread's log, so every branch holding that turn gets the same answer.
    pub fn target_for_turn(&self, turn_id: &str) -> Result<Option<RestoreTarget>> {
        Ok(self
            .turns
            .manifest_for_turn(turn_id)?
            .map(|manifest_id| RestoreTarget { manifest_id }))
    }

    /// Where an undo returns to: the state captured just before the restore
    /// that produced `thread_id`. Filed under the thread a rewind switched
    /// **to**, which is where the user is when they ask to undo — and which
    /// no other session can reach, so concurrent sessions in one directory
    /// cannot consume each other's undos.
    pub fn last_restore_target(&self, thread_id: &str) -> Result<Option<RestoreTarget>> {
        Ok(self
            .turns
            .last_restore(thread_id)?
            .map(|record| RestoreTarget {
                manifest_id: record.safety_manifest_id,
            }))
    }

    /// Paths that have moved since the restore `thread_id` would undo.
    ///
    /// A rewind records the state it left the workspace in. If a path no
    /// longer matches that state, something else changed it after the fact —
    /// another session sharing the directory, or the user's own editor — and
    /// undoing would overwrite that change without ever mentioning it. The
    /// files are shared even though the undo records are not, so this is the
    /// only thing standing between a concurrent edit and silent loss.
    ///
    /// Contents are hashed rather than compared by stat fingerprint. The fast
    /// path can miss a same-length rewrite inside one timestamp tick, and here
    /// a false "unchanged" means overwriting someone's work — the opposite of
    /// what this is for. The set being checked is small: only what the restore
    /// would touch.
    ///
    /// Empty when there is nothing to undo, or when nothing has moved.
    pub fn undo_conflicts(
        &self,
        thread_id: &str,
        is_protected: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<String>> {
        let Some(record) = self.turns.last_restore(thread_id)? else {
            return Ok(Vec::new());
        };
        let expected = self.manifests.load(&record.target_manifest_id)?;

        let mut moved = Vec::new();
        for (path, entry) in &expected.entries {
            if is_protected(path) {
                continue;
            }
            let matches = fs::read(path)
                .map(|bytes| BlobStore::hash_bytes(&bytes) == entry.hash)
                .unwrap_or(false);
            if !matches {
                moved.push(path.clone());
            }
        }
        // A path the rewind removed, which is back: someone recreated it.
        for path in &expected.absent {
            if !is_protected(path) && Path::new(path).exists() {
                moved.push(path.clone());
            }
        }

        // The target only knows the paths the rewind had an opinion about.
        // An undo restores the whole safety manifest, which is wider — it was
        // captured from the performing thread's scope, so it holds paths the
        // rewind left untouched. For those the target cannot say what the
        // rewind left, and comparing against it silently checks nothing.
        //
        // The safety manifest can, and for exactly these paths it is the
        // right thing to compare against: the rewind did not touch them, so
        // what the capture found is *also* what the rewind left — and it is
        // simultaneously what the undo is about to write over.
        let restoring = self.manifests.load(&record.safety_manifest_id)?;
        for (path, entry) in &restoring.entries {
            if is_protected(path)
                || expected.entries.contains_key(path)
                || expected.absent.contains(path)
            {
                continue;
            }
            let matches = fs::read(path)
                .map(|bytes| BlobStore::hash_bytes(&bytes) == entry.hash)
                .unwrap_or(false);
            if !matches {
                moved.push(path.clone());
            }
        }

        // And what the undo will *delete*, which the target likewise cannot
        // tell us: only the safety manifest knows a file existing now was
        // absent then and is about to be removed.
        for path in &restoring.absent {
            if !is_protected(path) && Path::new(path).exists() {
                moved.push(path.clone());
            }
        }

        moved.sort();
        moved.dedup();
        Ok(moved)
    }

    /// Union of every path key observed across the thread's manifests.
    /// Used to build the safety-checkpoint scope for a restore: it covers
    /// outside-workspace paths recorded via pre-edit attach that a plain
    /// workspace scan would miss.
    pub fn tracked_paths(&self, thread_id: &str) -> Result<std::collections::BTreeSet<String>> {
        let mut out = std::collections::BTreeSet::new();
        for (_, manifest) in self.thread_history(thread_id)? {
            out.extend(manifest.entries.into_keys());
            // Tombstoned paths count as observed too. They are precisely the
            // ones a restore may need to remove, and a path the safety scope
            // misses is a path the plan can never delete.
            out.extend(manifest.absent);
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

    /// Load a manifest by id.
    pub fn manifest(&self, manifest_id: &str) -> Result<Manifest> {
        self.manifests.load(manifest_id)
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
        record_under: Option<&str>,
        target: &RestoreTarget,
        kind: RestoreKind,
        current_files: impl IntoIterator<Item = PathBuf>,
        is_protected: &dyn Fn(&str) -> bool,
    ) -> Result<RestoreOutcome> {
        // 1. Capture what is about to be replaced, so this restore can be
        // undone. The checkpoint is appended to the thread doing the work;
        // where the *undo record* is filed is a separate question, answered
        // by `record_under` below.
        //
        // Whatever the caller scanned, the target's own paths are added to
        // it. This makes the safety capture sufficient *by construction*
        // rather than by argument: a plan can only write `target.entries`
        // and only delete `target.absent`, so observing both means every
        // path the plan could touch has been looked at.
        //
        // Without it the sufficiency rested on the caller passing the
        // thread's whole history, which silently fails for an undo — the
        // undo record is filed under the thread the rewind switched *to*,
        // and `inherit_log` copies only up to the fork turn, so that
        // thread's history contains neither the safety manifest nor
        // anything the source learned afterwards. A path recorded absent by
        // the safety capture and put back on disk by something other than
        // the rewind (a shell command, an editor, another session) was then
        // in `target.absent` but missing from `current.entries`, and
        // `plan_restore` needs both to delete. It survived the undo in
        // silence.
        let target_manifest = self.manifests.load(&target.manifest_id)?;
        let observed = current_files
            .into_iter()
            .chain(target_manifest.entries.keys().map(PathBuf::from))
            .chain(target_manifest.absent.iter().map(PathBuf::from));
        let safety = self.checkpoint(
            thread_id,
            &format!("{SAFETY_TURN_PREFIX}{}", target.manifest_id),
            observed,
        )?;

        // 2. Compare the two states directly. Nothing here consults a
        // thread's history, so the outcome depends only on where the
        // workspace is and where it is going.
        let current = self.manifests.load(&safety.id)?;
        let plan = plan_restore(&target_manifest, &current, is_protected);
        let stats = apply_plan(&self.blobs, &plan)?;

        // `record_under` is the thread this restore hands the workspace to.
        // A rewind names the branch it creates; an undo names itself, since
        // that is where the record it spends was filed. A restore with no
        // destination — rewinding to the first prompt, which restarts the
        // conversation rather than branching — records nothing, and is
        // therefore not undoable. That is stated to the user up front.
        if let Some(owner) = record_under {
            match kind {
                RestoreKind::Rewind => self.turns.push_restore(
                    owner,
                    RestoreRecord {
                        target_manifest_id: target.manifest_id.clone(),
                        safety_manifest_id: safety.id.clone(),
                    },
                )?,
                RestoreKind::Undo => {
                    self.turns.pop_restore(owner)?;
                }
            }
        }

        Ok(RestoreOutcome {
            safety: RestoreTarget {
                manifest_id: safety.id,
            },
            plan,
            stats,
        })
    }

    /// Drop a thread's snapshot log (its data becomes garbage for `gc`).
    pub fn remove_thread(&self, thread_id: &str) -> Result<()> {
        self.refs.remove(thread_id)
    }

    /// Mark-and-sweep unreferenced manifests and blobs. Roots are the thread
    /// logs plus everything the turn index and workspace restore logs still
    /// point at.
    pub fn gc(&self) -> Result<GcStats> {
        collect_garbage(&self.refs, &self.turns, &self.manifests, &self.blobs)
    }

    /// Paths this thread has observed that `target` has no opinion about and
    /// that lie outside `roots` — so a restore to it will leave them exactly
    /// as they are, whatever the user expected.
    ///
    /// Tracking is *discovered*, not retroactive. Files under a root are
    /// enumerated at every checkpoint, so every turn knows about them and
    /// rewinding further back restores strictly more. Files outside one
    /// arrive only when the edit hook first touches them, which means an
    /// earlier turn genuinely has no record of a file a later turn does —
    /// and rewinding *further back* then restores *less*, which is the
    /// opposite of what anyone expects.
    ///
    /// Leaving them alone is the right call: this turn has no content to
    /// write, and inventing some from a later turn's pre-image would put
    /// bytes of unknown provenance on disk. But it is invisible, so callers
    /// use this to say so before the user commits to it.
    pub fn unrestorable_outside(
        &self,
        thread_id: &str,
        target: &RestoreTarget,
        roots: &[PathBuf],
    ) -> Result<Vec<String>> {
        let manifest = self.manifests.load(&target.manifest_id)?;
        let mut out: Vec<String> = self
            .tracked_paths(thread_id)?
            .into_iter()
            .filter(|path| !manifest.entries.contains_key(path) && !manifest.absent.contains(path))
            .filter(|path| {
                let path = Path::new(path);
                !roots.iter().any(|root| path.starts_with(root))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Sweep just the manifests named by threads that have been removed.
    /// Unlike `gc`, this reclaims immediately — see `collect_garbage_for`.
    pub fn gc_for(&self, doomed: &std::collections::BTreeSet<String>) -> Result<GcStats> {
        collect_garbage_for(
            &self.refs,
            &self.turns,
            &self.manifests,
            &self.blobs,
            doomed,
        )
    }

    /// Also delete the undo records filed under `thread_id`, which
    /// `remove_thread` deliberately leaves alone — a thread's log and the
    /// restores handed *to* it are separate lifetimes, and only deleting the
    /// conversation ends both.
    pub fn remove_restores(&self, thread_id: &str) -> Result<()> {
        self.turns.remove_restores(thread_id)
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

/// Forget everything the snapshot store holds for `thread_ids`, then sweep.
///
/// This is what makes "snapshot lifetime = session lifetime" (RFC §8) true.
/// Deleting a conversation removes its rollout, and until this ran, every
/// file captured during that conversation stayed on disk indefinitely —
/// contents included. That is a disk leak, but more importantly it is not
/// what a user deleting a conversation is asking for: they get the index
/// removed and the data kept.
///
/// Deliberately infallible and best-effort. Deleting a conversation must not
/// fail because its snapshots could not be tidied up, and a partial sweep is
/// not a corrupt store — the next one finishes the job. Failures are logged
/// rather than propagated, so callers need one line and no error handling.
///
/// Takes the whole set at once because deletion is by subtree: sweeping once
/// after the last thread beats sweeping once per thread, and a thread's
/// manifests are routinely shared with the siblings being deleted alongside
/// it — swept individually, each would still be pinned by the next.
pub fn forget_threads(codex_home: &Path, thread_ids: &[String]) {
    if thread_ids.is_empty() {
        return;
    }
    let root = codex_home.join("file_snapshots");
    if !root.exists() {
        return;
    }
    let store = match SnapshotStore::open(&root) {
        Ok(store) => store,
        Err(err) => {
            warn!("could not open the snapshot store to forget deleted threads: {err}");
            return;
        }
    };

    // Read what these threads named *before* dropping their logs, so the
    // sweep afterwards has an exact candidate set instead of having to
    // re-derive one by elimination.
    let mut doomed = std::collections::BTreeSet::new();
    for thread_id in thread_ids {
        if let Ok(log) = store.refs.load(thread_id) {
            doomed.extend(log.entries.into_iter().map(|entry| entry.manifest_id));
        }
        if let Ok(records) = store.turns.all_restores_for(thread_id) {
            doomed.extend(records);
        }
        if let Err(err) = store.remove_thread(thread_id) {
            warn!("could not drop the snapshot log for {thread_id}: {err}");
        }
        if let Err(err) = store.remove_restores(thread_id) {
            warn!("could not drop the undo records for {thread_id}: {err}");
        }
    }

    match store.gc_for(&doomed) {
        Ok(stats) => info!(
            "swept snapshots for {} deleted thread(s): {} manifests, {} blobs",
            thread_ids.len(),
            stats.manifests_removed,
            stats.blobs_removed
        ),
        Err(err) => warn!("could not sweep snapshots after deleting threads: {err}"),
    }
}
