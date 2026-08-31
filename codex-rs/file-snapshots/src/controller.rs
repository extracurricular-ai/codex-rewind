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
//! - **Scope**: the union of three partitions (see `scope`) — what the
//!   project's index lists, what the agent has edited this session, and what
//!   changed most recently. Rooted at the session's own workspace roots,
//!   falling back to the turn cwd when none of them relate to it.
//! - Capture failures degrade to "no snapshot for this turn" — they are
//!   logged and never fail the turn.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use crate::error::Result;
use crate::scope::is_ignored;
use crate::scope::load_ignore;
use crate::scope::tracked_files;
use crate::store::SnapshotStore;
use tracing::info;
use tracing::warn;

/// Directory under `CODEX_HOME` holding the snapshot store.
const STORE_DIR_NAME: &str = "file_snapshots";

pub struct FileSnapshotsController {
    store: SnapshotStore,
    thread_id: String,
    /// Whether captures descend into dot-files and dot-directories.
    include_hidden: bool,
    state: Mutex<TrackState>,
}

#[derive(Default)]
struct TrackState {
    /// Agent-edited paths registered via pre-edit attach; unioned into
    /// every checkpoint scan so post-edit states keep being observed.
    /// In-memory for v1: lost on resume (recorded manifests stay valid).
    extras: BTreeSet<PathBuf>,
    /// Directories whose ignore rules scope this session's captures: the
    /// session's workspace roots, else the invocation directory. Recorded by
    /// the turn-start checkpoint, which always precedes tool execution within
    /// a turn.
    ///
    /// All of them rather than the first. A session can be configured with
    /// several roots, `tracked_files` already applies every root's matcher to
    /// the scan, and consulting only one here would leave a rule living in the
    /// second root with no effect on edits made in the second root. Symmetric
    /// ignore is achieved structurally on the restore side — nothing in
    /// `restore` or `store` consults these rules — so a path that leaks into a
    /// manifest is written back on rewind and its tombstone licenses a
    /// deletion. The leak is the whole failure.
    ignore_roots: Vec<PathBuf>,
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
            vec![cwd.to_path_buf()]
        } else {
            related
        };
        // Whichever directories scoped this capture also scope the ignore
        // rules applied to edit-hook captures (see `attach_pre_edits_blocking`).
        self.lock_state().ignore_roots = roots.clone();

        // Three partitions, unioned (see `scope`), plus what the agent has
        // written this session — wherever it lives. Walking the subtree
        // instead was unbounded by construction: on a repository of any age
        // most of what is on disk is build output, which is both the bulk of
        // the cost and the least worth keeping.
        let extras: Vec<PathBuf> = self.lock_state().extras.iter().cloned().collect();
        let files = tracked_files(&roots, extras, self.include_hidden);
        let checkpoint = self.store.checkpoint(&self.thread_id, turn_id, files)?;
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
        let ignores: Vec<_> = self
            .lock_state()
            .ignore_roots
            .iter()
            .map(|root| load_ignore(root))
            .collect();
        for (path, pre_content) in pre_images {
            if ignores.iter().any(|rules| is_ignored(rules, &path)) {
                continue;
            }
            // The edit-touched partition: unbounded on purpose, since its size
            // follows what the agent did rather than what is on disk.
            self.lock_state().extras.insert(path.clone());
            let key = path.to_string_lossy().into_owned();
            // Read before the edit lands, which is the whole reason this hook
            // runs when it does. `symlink_metadata` rather than `metadata`
            // because a symlink's own mode says nothing about the file a
            // restore would write, and capture skips links for the same reason.
            let mode = std::fs::symlink_metadata(&path)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map_or(crate::manifest::DEFAULT_MODE, |meta| {
                    crate::manifest::mode_of(&meta)
                });
            if let Err(err) = self.store.attach_pre_edit(
                &self.thread_id,
                turn_id,
                &key,
                pre_content.as_deref(),
                mode,
            ) {
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
    fn a_large_directory_cannot_flood_a_capture() {
        // The property a plain subtree walk lacked. Without a bound, a capture
        // costs whatever happens to be on disk — on a real repository that was
        // 57k files and 100 GB, nearly all of it build output.
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let loose = tempfile::tempdir().unwrap();
        for i in 0..(crate::scope::RECENT_LIMIT + 50) {
            std::fs::write(loose.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        ctl.checkpoint_turn_start_blocking("turn-1", loose.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(
            history[0].1.entries.len(),
            crate::scope::RECENT_LIMIT,
            "no repository here, so only the recency partition contributes"
        );
    }

    #[test]
    fn ignored_paths_are_not_captured_through_the_edit_hook() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
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

    /// The edit hook is handed absolute paths from wherever the agent wrote,
    /// which is not necessarily under the root that scoped the session. The
    /// ignore matcher is built for one root, and `ignore`'s
    /// `matched_path_or_any_parents` is documented to panic when asked about a
    /// path outside it — so the first such edit would abort the whole batch,
    /// losing the `absent` tombstones that are the only evidence licensing a
    /// rewind to delete an agent-created file.
    #[test]
    fn an_edit_outside_the_ignore_root_still_records_its_evidence() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git")).unwrap();
        // Non-empty on purpose: an empty matcher short-circuits before the
        // bounds check and would hide this entirely.
        std::fs::write(
            ws.path().join(crate::scope::SNAPSHOT_IGNORE_FILENAME),
            "*.log\n",
        )
        .unwrap();
        std::fs::write(ws.path().join("src.rs"), "code").unwrap();
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);

        // A sibling directory: a real absolute path, under no snapshot root.
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("created.txt");
        let inside = ws.path().join("src.rs");

        ctl.attach_pre_edits_blocking(
            "turn-1",
            vec![
                (outside.clone(), None),
                (inside.clone(), Some(b"code".to_vec())),
            ],
        );

        let paths = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            paths.contains(&outside.to_string_lossy().into_owned()),
            "a path under no ignore root is not ignored, and its tombstone is \
             what licenses a later rewind to remove the file: {paths:?}"
        );
        assert!(
            paths.contains(&inside.to_string_lossy().into_owned()),
            "one unmatchable path must not drop the rest of the batch"
        );
    }

    /// The other half of the same rule. A session can be configured with more
    /// than one workspace root, and each carries its own ignore file — so a
    /// rule living in the second root has to govern edits made in the second
    /// root. Consulting only the first leaves the second root's exclusions
    /// with no effect on the edit hook at all, and nothing downstream catches
    /// it: `restore` never consults these rules, so once a path is in a
    /// manifest a rewind writes it back.
    #[test]
    fn every_workspace_root_ignore_file_governs_the_edit_hook() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        // Two roots, both related to the cwd because the cwd is their parent.
        let ws = tempfile::tempdir().unwrap();
        let first = ws.path().join("first");
        let second = ws.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join(crate::scope::SNAPSHOT_IGNORE_FILENAME), "*.a\n").unwrap();
        std::fs::write(second.join(crate::scope::SNAPSHOT_IGNORE_FILENAME), "*.b\n").unwrap();
        std::fs::write(first.join("keep.txt"), "keep").unwrap();

        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[first.clone(), second.clone()]);

        let ignored_by_first = first.join("x.a");
        let ignored_by_second = second.join("x.b");
        let kept = first.join("keep.txt");
        ctl.attach_pre_edits_blocking(
            "turn-1",
            vec![
                (ignored_by_first.clone(), Some(b"a".to_vec())),
                (ignored_by_second.clone(), Some(b"b".to_vec())),
                (kept.clone(), Some(b"keep".to_vec())),
            ],
        );

        let paths = ctl.store.tracked_paths("t1").unwrap();
        assert!(
            !paths.contains(&ignored_by_first.to_string_lossy().into_owned()),
            "the first root's rules apply: {paths:?}"
        );
        assert!(
            !paths.contains(&ignored_by_second.to_string_lossy().into_owned()),
            "and so do the second root's — consulting only the first is the bug \
             this pins: {paths:?}"
        );
        assert!(paths.contains(&kept.to_string_lossy().into_owned()));
    }

    /// `checkpoint_inner`'s `else { related }` arm is the **only** branch
    /// production ever takes — `core/src/session/turn.rs` always passes the
    /// environment's workspace roots — and until this test no test in the
    /// crate had executed it: every other one passes `&[]` and exercises the
    /// cwd fallback instead.
    ///
    /// Both directions are real. A stale root wrongly kept means the scan
    /// writes `absent` tombstones for index entries missing from that tree,
    /// and a later rewind may delete files there. A related root wrongly
    /// dropped means files the sandbox lets the agent write are never
    /// snapshotted, and `/rewind` silently restores nothing.
    #[test]
    fn a_capture_follows_the_session_workspace_roots() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        let root = ws.path().join("project");
        let nested = root.join("crate-a");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("top.txt"), "at the root").unwrap();
        std::fs::write(nested.join("inner.txt"), "below the cwd").unwrap();

        // Configured but unrelated: neither contains the cwd nor sits under it.
        let unrelated = ws.path().join("elsewhere");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(unrelated.join("stale.txt"), "different environment").unwrap();

        // The turn runs in the nested directory; the root is above it.
        ctl.checkpoint_turn_start_blocking("turn-1", &nested, &[root.clone(), unrelated.clone()]);

        let manifest = ctl
            .store
            .latest_manifest("t1")
            .unwrap()
            .expect("the checkpoint recorded something");
        let has = |path: &std::path::Path| {
            manifest
                .entries
                .contains_key(&path.to_string_lossy().into_owned())
        };

        assert!(
            has(&root.join("top.txt")),
            "the root bounds the capture, not the turn's cwd — a file above \
             the cwd but inside the configured root is in scope"
        );
        assert!(has(&nested.join("inner.txt")));
        assert!(
            !has(&unrelated.join("stale.txt")),
            "a root that neither contains nor sits under the cwd describes a \
             different environment, and scanning it is the over-capture this \
             feature exists to avoid"
        );
        assert!(
            !manifest
                .absent
                .iter()
                .any(|path| path.contains("elsewhere")),
            "and it must not leave tombstones there either — those are what \
             license a later rewind to delete: {:?}",
            manifest.absent
        );
    }

    /// The sibling shape production actually emits: several roots, all
    /// related, with the cwd inside one of them. Asserted explicitly so that
    /// whether the *other* root is in scope is a decision on record rather
    /// than something nobody noticed either way.
    #[test]
    fn every_related_workspace_root_is_in_scope_not_just_the_one_holding_the_cwd() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        let a = ws.path().join("a");
        let b = ws.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("in-a.txt"), "a").unwrap();
        std::fs::write(b.join("in-b.txt"), "b").unwrap();

        // cwd is the parent, so both roots sit under it and both are related.
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[a.clone(), b.clone()]);

        let manifest = ctl.store.latest_manifest("t1").unwrap().unwrap();
        let has = |path: &std::path::Path| {
            manifest
                .entries
                .contains_key(&path.to_string_lossy().into_owned())
        };
        assert!(has(&a.join("in-a.txt")));
        assert!(
            has(&b.join("in-b.txt")),
            "every related root is scanned; the first is not privileged"
        );
    }

    /// A pre-edit image used to record a constant `0o644`, on the stated
    /// grounds that patch content carries no stat. It carries no stat for the
    /// *content*; the file itself is still on disk when this hook runs, and
    /// its mode is right there. The cost of the constant was that rewinding to
    /// a turn where the agent edited a script restored the bytes and chmodded
    /// the executable bit away in the same operation.
    #[cfg(unix)]
    #[test]
    fn a_pre_edit_image_records_the_file_permissions_it_actually_had() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        let script = ws.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);
        // A path the turn-start scan did not reach, so the edit hook is what
        // records it — which is the case the constant applied to.
        let outside = ws.path().join("nested/tool.sh");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();

        ctl.attach_pre_edits_blocking(
            "turn-1",
            vec![(outside.clone(), Some(b"#!/bin/sh\n".to_vec()))],
        );

        let manifest = ctl
            .store
            .latest_manifest("t1")
            .unwrap()
            .expect("the edit hook recorded something");
        let entry = &manifest.entries[&outside.to_string_lossy().into_owned()];
        assert_eq!(
            entry.mode, 0o755,
            "the executable bit is part of what a rewind has to put back"
        );
    }

    #[test]
    fn checkpoint_workspace_and_fallback_modes() {
        let home = tempfile::tempdir().unwrap();
        let ctl = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t1".into(),
            /*include_hidden*/ false,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        ctl.checkpoint_turn_start_blocking("turn-1", ws.path(), &[]);

        let history = ctl.store.thread_history("t1").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1.entries.len(), 1);

        // Paths registered via pre-edit attach are observed by later
        // checkpoints, wherever they live.
        let loose = tempfile::tempdir().unwrap();
        std::fs::write(loose.path().join("note.md"), "n1").unwrap();
        let ctl2 = FileSnapshotsController::maybe_new(
            home.path(),
            true,
            true,
            "t2".into(),
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
        let outside_key = outside.to_string_lossy().into_owned();
        assert!(
            !history[0].1.entries.contains_key(&outside_key),
            "a path nothing had pointed at yet is simply not observed"
        );
        let last = &history[2].1;
        assert!(
            last.entries
                .contains_key(&outside.to_string_lossy().into_owned()),
            "extras are unioned into later checkpoints"
        );
    }
}
