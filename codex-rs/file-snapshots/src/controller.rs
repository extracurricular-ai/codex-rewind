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
use crate::scope::is_ignored;
use crate::scope::load_ignore;
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
    /// Seeding limits for fallback mode, from `[file_snapshots]`.
    seed_policy: SeedPolicy,
    /// Whether scans descend into dot-files and dot-directories.
    include_hidden: bool,
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
    /// Directory whose ignore rules scope this session's captures: the
    /// workspace root when one was found, else the invocation directory.
    /// Recorded by the turn-start checkpoint, which always precedes tool
    /// execution within a turn.
    ignore_root: Option<PathBuf>,
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
        seed_policy: SeedPolicy,
        include_hidden: bool,
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
            // A resumed thread keeps tracking if it has snapshots. One that
            // never captured any has nothing to resume tracking *from*, so
            // whether it once had the feature on makes no observable
            // difference — see the lazy marker below.
            store.thread_exists(&thread_id)
        };
        if !active {
            return None;
        }
        // The log is written by the first checkpoint, not here. Codex creates
        // a thread id whenever the TUI starts and abandons it if the user
        // immediately resumes something else or quits, and it writes the
        // rollout lazily for exactly that reason — a session that never ran
        // leaves nothing behind. Marking eagerly made this subsystem the one
        // component that littered: better than half the logs on this machine
        // were empty. Deferring costs nothing, because the marker's only
        // consumer asks whether snapshots exist.
        info!("file_snapshots: tracking enabled for thread {thread_id}");
        Some(Arc::new(Self {
            store,
            thread_id,
            seed_policy,
            include_hidden,
            state: Mutex::new(TrackState::default()),
        }))
    }

    /// Capture the turn-start checkpoint. Blocking (stat-walk + hashing of
    /// changed files) — call via `spawn_blocking`.
    pub fn checkpoint_turn_start_blocking(
        &self,
        turn_id: &str,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) {
        if let Err(err) = self.checkpoint_inner(turn_id, cwd, workspace_roots) {
            warn!("file_snapshots: turn-start checkpoint failed (turn {turn_id}): {err}");
        }
    }

    fn checkpoint_inner(
        &self,
        turn_id: &str,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<()> {
        // The session's own workspace roots come first: they are what the user
        // configured and what the sandbox consults to decide where the agent
        // may write, so scoping to them makes "whatever the agent can change
        // can be reverted" structural rather than coincidental. The marker
        // walk-up is a guess about intent and only stands in when there is
        // nothing to go on.
        //
        // Roots unrelated to the turn's cwd are dropped. A configured root can
        // describe a different environment, or simply be stale, and scanning
        // an unrelated tree is the over-capture failure this feature exists to
        // avoid — a root that neither contains nor sits under the directory
        // being worked in is not this session's workspace.
        let related: Vec<PathBuf> = workspace_roots
            .iter()
            .filter(|root| cwd.starts_with(root) || root.starts_with(cwd))
            .cloned()
            .collect();
        let roots: Vec<PathBuf> = if related.is_empty() {
            let markers: Vec<String> = WORKSPACE_MARKERS.iter().map(ToString::to_string).collect();
            find_workspace_root(cwd, &markers).into_iter().collect()
        } else {
            related
        };
        // Whichever directory scoped this scan also scopes the ignore rules
        // applied to edit-hook captures (see `attach_pre_edits_blocking`).
        self.lock_state().ignore_root =
            Some(roots.first().cloned().unwrap_or_else(|| cwd.to_path_buf()));
        let (mut files, complete): (BTreeSet<PathBuf>, bool) = if !roots.is_empty() {
            let mut all = BTreeSet::new();
            for root in &roots {
                all.extend(workspace_files(root, self.include_hidden)?);
            }
            (all, true)
        } else {
            let policy = self.seed_policy.clone();
            let mut state = self.lock_state();
            let tracked = match &mut state.fallback {
                Some(tracked) => tracked,
                None => {
                    let seed = seed_fallback_files(cwd, &policy, self.include_hidden)?;
                    state
                        .fallback
                        .get_or_insert(TrackedSet::new(seed, policy.cap))
                }
            };
            (tracked.files().cloned().collect(), false)
        };
        files.extend(self.lock_state().extras.iter().cloned());

        let scope = crate::CaptureScope {
            roots: if roots.is_empty() {
                vec![cwd.to_path_buf()]
            } else {
                roots
            },
            complete,
        };
        let checkpoint = self
            .store
            .checkpoint(&self.thread_id, turn_id, files, scope)?;
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
        // Symmetric ignore (RFC correctness rule 4): a path the user excluded
        // from snapshots must not enter the store through the edit hook
        // either. Without this, editing an ignored file would both store its
        // pre-edit content and register the path as an extra, so every later
        // checkpoint would capture it as well — bypassing the scan's own
        // ignore filter. The rules are read fresh so the current ignore file
        // governs (rule 5).
        let ignore = self.lock_state().ignore_root.as_deref().map(load_ignore);
        for (path, pre_content) in pre_images {
            if ignore
                .as_ref()
                .is_some_and(|rules| is_ignored(rules, &path))
            {
                continue;
            }
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
            FileSnapshotsController::maybe_new(
                home.path(),
                false,
                true,
                "t1".into(),
                SeedPolicy::default(),
                /*include_hidden*/ false,
            )
            .is_none()
        );
        // New session, feature on → active. The marker is not written yet:
        // a session that never captures anything must leave nothing behind.
        let controller = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .expect("feature on for a new session");
        assert!(
            !home.path().join("file_snapshots/refs/t1.json").exists(),
            "no snapshots yet, so no marker"
        );

        // Capturing one writes it.
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        controller.checkpoint_turn_start_blocking("turn-1", &ws, &[]);
        assert!(home.path().join("file_snapshots/refs/t1.json").exists());

        // Resume with feature now OFF → marker wins, still tracking.
        assert!(
            FileSnapshotsController::maybe_new(
                home.path(),
                false,
                false,
                "t1".into(),
                SeedPolicy::default(),
                /*include_hidden*/ false,
            )
            .is_some()
        );
        // Resume of a session that never tracked, feature now ON → stays off.
        assert!(
            FileSnapshotsController::maybe_new(
                home.path(),
                true,
                false,
                "t2".into(),
                SeedPolicy::default(),
                /*include_hidden*/ false,
            )
            .is_none()
        );
    }

    #[test]
    fn hidden_entries_are_skipped_unless_edited() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(ws.path().join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(ws.path().join(".github/workflows")).unwrap();
        std::fs::write(ws.path().join(".github/workflows/ci.yml"), "on: push").unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);
        let scanned = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            scanned.iter().all(|p| !p.contains("/.env")),
            "tool state and credentials stay out of snapshots: {scanned:?}"
        );
        assert!(scanned.iter().all(|p| !p.contains("/.git")));
        assert!(scanned.iter().any(|p| p.ends_with("src.rs")));

        // An edited hidden file is a different matter: it is work product,
        // so the edit hook tracks it and a rewind can restore it.
        let workflow = ws.path().join(".github/workflows/ci.yml");
        ctl.attach_pre_edits_blocking(
            "turn-1",
            vec![(workflow.clone(), Some(b"on: push".to_vec()))],
        );
        assert!(
            ctl.store
                .tracked_paths("t1")
                .unwrap()
                .contains(&workflow.to_string_lossy().into_owned()),
            "explicitly edited hidden files must remain restorable"
        );
    }

    #[test]
    fn seed_policy_from_config_bounds_fallback_tracking() {
        let home = tempfile::tempdir().unwrap();
        // Deliberately tiny limits: a directory of 5 files exceeds
        // `full_limit`, so only `recent_seed` files may be tracked.
        let policy = SeedPolicy {
            full_limit: 2,
            recent_seed: 3,
            cap: 4,
        };
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            policy,
            /*include_hidden*/ false,
        )
        .unwrap();

        // No `.git` marker → fallback mode, where seeding applies.
        let loose = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(loose.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        ctl.checkpoint_turn_start_blocking("turn-1", loose.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(
            history[0].1.entries.len(),
            3,
            "configured seed_recent must bound the tracked set"
        );
        assert!(!history[0].1.complete, "fallback captures are partial");
    }

    #[test]
    fn ignored_paths_are_not_captured_through_the_edit_hook() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(
            ws.path().join(crate::scope::SNAPSHOT_IGNORE_FILENAME),
            "secrets/**\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.path().join("secrets")).unwrap();
        std::fs::write(ws.path().join("secrets/key.pem"), "private").unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();

        // Turn-start scan establishes the ignore scope for the session.
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);

        let secret = ws.path().join("secrets/key.pem");
        let tracked = ws.path().join("src.rs");
        ctl.attach_pre_edits_blocking(
            "turn-1",
            vec![
                (secret.clone(), Some(b"private".to_vec())),
                (tracked.clone(), Some(b"code".to_vec())),
            ],
        );
        ctl.checkpoint_turn_start_blocking("turn-2", ws.path(), &[]);

        let paths = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            !paths.contains(&secret.to_string_lossy().into_owned()),
            "ignored path must never reach the store, not even via the edit hook: {paths:?}"
        );
        assert!(paths.contains(&tracked.to_string_lossy().into_owned()));
    }

    #[test]
    fn checkpoint_workspace_and_fallback_modes() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .unwrap();

        // Workspace mode: .git marker present → complete scan.
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].1.complete);
        assert_eq!(history[0].1.entries.len(), 1);

        // Fallback mode: no marker → bounded scan, extras registered via
        // pre-edit attach are observed by later checkpoints.
        let loose = tempfile::tempdir().unwrap();
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t2".into(),
            SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .unwrap();
        ctl2.checkpoint_turn_start_blocking("turn-1", loose.path(), &[]);
        let outside = home.path().join("elsewhere.cfg");
        ctl2.attach_pre_edits_blocking("turn-1", vec![(outside.clone(), Some(b"pre".to_vec()))]);
        std::fs::write(&outside, "post").unwrap();
        ctl2.checkpoint_turn_start_blocking("turn-2", loose.path(), &[]);

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
