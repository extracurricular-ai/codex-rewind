//! Session-side controller: the policy layer gluing scope resolution,
//! capture, and pre-edit attach for one thread (RFC §6.2–§6.4). Lives in
//! this crate (not codex-core) so the logic stays testable and reusable
//! by other consumers (e.g. app-server).
//!
//! Wiring rules (RFC §6.2–§6.4):
//! - **Session-scoped binding**: whether a thread tracks is decided at
//!   session start — new sessions follow `Feature::FileSnapshots`; resumed
//!   sessions follow the persisted marker (snapshot-log existence). The
//!   state never changes mid-session.
//! - **Scope**: workspace mode when a marker directory is found by walk-up
//!   from the turn cwd (complete scans); otherwise fallback mode with a
//!   seeded, capped tracked set (bounded scans). Files the agent edits are
//!   registered as extras so later checkpoints observe them, wherever they
//!   live.
//! - Capture failures degrade to "no snapshot for this turn" — they are
//!   logged and never fail the turn.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use crate::error::Result;
use crate::scope::SeedPolicy;
use crate::scope::TrackedSet;
use crate::scope::find_workspace_root;
use crate::scope::seed_fallback_files;
use crate::scope::workspace_files;
use crate::store::SnapshotStore;
use tracing::info;
use tracing::warn;

/// Workspace root markers for v1, matching the turn-diff display-root probe
/// (`turn_diff_display_roots`). A dedicated project marker can join later.
const WORKSPACE_MARKERS: &[&str] = &[".git"];

/// Directory under `CODEX_HOME` holding the snapshot store.
const STORE_DIR_NAME: &str = "file_snapshots";

pub struct FileSnapshotsController {
    store: SnapshotStore,
    thread_id: String,
    state: Mutex<TrackState>,
}

#[derive(Default)]
struct TrackState {
    /// Fallback-mode tracked set, seeded on first checkpoint outside any
    /// workspace root.
    fallback: Option<TrackedSet>,
    /// Agent-edited paths registered via pre-edit attach; unioned into
    /// every checkpoint scan so post-edit states keep being observed.
    /// In-memory for v1: lost on resume (recorded manifests stay valid).
    extras: BTreeSet<PathBuf>,
}

impl FileSnapshotsController {
    /// Build the controller if this session should track (see module doc).
    /// `is_new_thread` distinguishes fresh sessions (feature flag decides,
    /// marker gets created) from resumed ones (marker decides).
    pub fn maybe_new(
        codex_home: &Path,
        feature_enabled: bool,
        is_new_thread: bool,
        thread_id: String,
    ) -> Option<Arc<Self>> {
        let store = match SnapshotStore::open(codex_home.join(STORE_DIR_NAME)) {
            Ok(store) => store,
            Err(err) => {
                warn!("file_snapshots: failed to open store, tracking disabled: {err}");
                return None;
            }
        };
        let active = if is_new_thread {
            feature_enabled
        } else {
            store.thread_exists(&thread_id)
        };
        if !active {
            return None;
        }
        if let Err(err) = store.ensure_thread(&thread_id) {
            warn!("file_snapshots: failed to create thread marker, tracking disabled: {err}");
            return None;
        }
        info!("file_snapshots: tracking enabled for thread {thread_id}");
        Some(Arc::new(Self {
            store,
            thread_id,
            state: Mutex::new(TrackState::default()),
        }))
    }

    /// Capture the turn-start checkpoint. Blocking (stat-walk + hashing of
    /// changed files) — call via `spawn_blocking`.
    pub fn checkpoint_turn_start_blocking(&self, turn_id: &str, cwd: &Path) {
        if let Err(err) = self.checkpoint_inner(turn_id, cwd) {
            warn!("file_snapshots: turn-start checkpoint failed (turn {turn_id}): {err}");
        }
    }

    fn checkpoint_inner(&self, turn_id: &str, cwd: &Path) -> Result<()> {
        let markers: Vec<String> = WORKSPACE_MARKERS.iter().map(ToString::to_string).collect();
        let (mut files, complete): (BTreeSet<PathBuf>, bool) =
            match find_workspace_root(cwd, &markers) {
                Some(root) => (workspace_files(&root)?.into_iter().collect(), true),
                None => {
                    let mut state = self.lock_state();
                    let policy = SeedPolicy::default();
                    let tracked = match &mut state.fallback {
                        Some(tracked) => tracked,
                        None => {
                            let seed = seed_fallback_files(cwd, &policy)?;
                            state
                                .fallback
                                .get_or_insert(TrackedSet::new(seed, policy.cap))
                        }
                    };
                    (tracked.files().cloned().collect(), false)
                }
            };
        files.extend(self.lock_state().extras.iter().cloned());

        let checkpoint = self
            .store
            .checkpoint(&self.thread_id, turn_id, files, complete)?;
        info!(
            "file_snapshots: turn {turn_id} checkpoint {} ({} reused, {} hashed, {} skipped)",
            checkpoint.id,
            checkpoint.stats.reused,
            checkpoint.stats.hashed,
            checkpoint.stats.skipped,
        );
        Ok(())
    }

    /// Record pre-edit images from an applied patch delta and register the
    /// paths for future checkpoints. Blocking — call via `spawn_blocking`.
    /// `pre_images` pairs each absolute path with its pre-edit content
    /// (`None` = the file did not exist before the edit).
    pub fn attach_pre_edits_blocking(
        &self,
        turn_id: &str,
        pre_images: Vec<(PathBuf, Option<Vec<u8>>)>,
    ) {
        for (path, pre_content) in pre_images {
            {
                let mut state = self.lock_state();
                if let Some(tracked) = &mut state.fallback
                    && tracked.track_edit(path.clone())
                {
                    // Fallback mode owns the path now; extras not needed.
                } else {
                    state.extras.insert(path.clone());
                }
            }
            let key = path.to_string_lossy().into_owned();
            if let Err(err) =
                self.store
                    .attach_pre_edit(&self.thread_id, turn_id, &key, pre_content.as_deref())
            {
                warn!("file_snapshots: pre-edit attach failed for {key}: {err}");
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, TrackState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn session_scoped_binding() {
        let home = tempfile::tempdir().unwrap();

        // New session, feature off → inactive, no marker.
        assert!(
            FileSnapshotsController::maybe_new(home.path(), false, true, "t1".into()).is_none()
        );
        // New session, feature on → active, marker created.
        assert!(FileSnapshotsController::maybe_new(home.path(), true, true, "t1".into()).is_some());
        // Resume with feature now OFF → marker wins, still tracking.
        assert!(
            FileSnapshotsController::maybe_new(home.path(), false, false, "t1".into()).is_some()
        );
        // Resume of a session that never tracked, feature now ON → stays off.
        assert!(
            FileSnapshotsController::maybe_new(home.path(), true, false, "t2".into()).is_none()
        );
    }

    #[test]
    fn checkpoint_workspace_and_fallback_modes() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(home.path(), true, true, "t1".into()).unwrap();

        // Workspace mode: .git marker present → complete scan.
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path());

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].1.complete);
        assert_eq!(history[0].1.entries.len(), 1);

        // Fallback mode: no marker → bounded scan, extras registered via
        // pre-edit attach are observed by later checkpoints.
        let loose = tempfile::tempdir().unwrap();
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 =
            FileSnapshotsController::maybe_new(home.path(), true, true, "t2".into()).unwrap();
        ctl2.checkpoint_turn_start_blocking("turn-1", loose.path());
        let outside = home.path().join("elsewhere.cfg");
        ctl2.attach_pre_edits_blocking("turn-1", vec![(outside.clone(), Some(b"pre".to_vec()))]);
        std::fs::write(&outside, "post").unwrap();
        ctl2.checkpoint_turn_start_blocking("turn-2", loose.path());

        let history = ctl2.store.thread_history("t2").unwrap();
        // turn-1 scan + turn-1 supplemental attach + turn-2 scan.
        assert_eq!(history.len(), 3);
        assert!(!history[0].1.complete);
        let last = &history[2].1;
        assert!(
            last.entries
                .contains_key(&outside.to_string_lossy().into_owned()),
            "extras are unioned into later checkpoints"
        );
    }
}
