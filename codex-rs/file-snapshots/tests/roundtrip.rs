//! End-to-end scenario from the RFC: checkpoint → agent edits →
//! checkpoint → user edits → rewind (with safety checkpoint,
//! complete-scan deletion, symmetric ignore) → redo → GC.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use codex_file_snapshots::RestoreKind;
use codex_file_snapshots::SNAPSHOT_IGNORE_FILENAME;
use codex_file_snapshots::SnapshotStore;
use codex_file_snapshots::is_ignored;
use codex_file_snapshots::load_ignore;
use codex_file_snapshots::workspace_files;
use pretty_assertions::assert_eq;

const THREAD: &str = "thread-1";
const WS_KEY: &str = "/workspace/under-test";

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn full_rewind_redo_gc_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();

    // Workspace: two files, one ignored log, the (versioned) ignore file.
    fs::write(ws.join("a.txt"), "alpha v1").unwrap();
    fs::write(ws.join("b.txt"), "bravo v1").unwrap();
    fs::write(ws.join("build.log"), "log v1").unwrap();
    fs::write(ws.join(SNAPSHOT_IGNORE_FILENAME), "*.log\n").unwrap();

    // Turn 1 checkpoint.
    let cp1 = store
        .checkpoint(
            THREAD,
            "turn-1",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(
        cp1.manifest.entries.len(),
        2,
        "the ignored log and the (hidden) ignore file are both out of scope"
    );

    // Agent work during turn 1: modify a, delete b, create c.
    fs::write(ws.join("a.txt"), "alpha v2 (agent)").unwrap();
    fs::remove_file(ws.join("b.txt")).unwrap();
    fs::write(ws.join("c.txt"), "charlie (agent)").unwrap();

    // Turn 2 checkpoint observes the agent's changes.
    let cp2 = store
        .checkpoint(
            THREAD,
            "turn-2",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(
        cp2.manifest.entries.len(),
        2,
        "a.txt modified, b.txt deleted, c.txt created"
    );

    // Between checkpoints: the user creates a file (never yet captured)
    // and edits the ignored log.
    fs::write(ws.join("user-note.txt"), "user data").unwrap();
    fs::write(ws.join("build.log"), "log v2 (user)").unwrap();

    // Rewind to turn 1. Protection = current ignore rules.
    let ignore = load_ignore(&ws);
    let protect = move |path: &str| is_ignored(&ignore, Path::new(path));
    let outcome = store
        .restore_to(
            THREAD,
            WS_KEY,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind { conversation: None },
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
            &protect,
        )
        .unwrap();

    // Disk now matches turn 1 for tracked files…
    assert_eq!(read(&ws.join("a.txt")), "alpha v1");
    assert_eq!(
        read(&ws.join("b.txt")),
        "bravo v1",
        "deleted file recreated"
    );
    assert!(!ws.join("c.txt").exists(), "agent-born file deleted");
    assert!(
        !ws.join("user-note.txt").exists(),
        "user file born after turn 1 is deleted — but recoverable (safety checkpoint)"
    );
    // …while ignored paths were untouched in every direction.
    assert_eq!(read(&ws.join("build.log")), "log v2 (user)");
    assert_eq!(outcome.stats.written, 2);
    assert_eq!(outcome.stats.deleted, 2);
    assert!(
        store.last_restore_target(WS_KEY).unwrap().is_some(),
        "the rewind left an undo behind"
    );

    // Redo: restore the safety checkpoint → pre-rewind state returns.
    let ignore2 = load_ignore(&ws);
    let protect2 = move |path: &str| is_ignored(&ignore2, Path::new(path));
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &outcome.safety,
            RestoreKind::Undo,
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
            &protect2,
        )
        .unwrap();
    assert_eq!(read(&ws.join("a.txt")), "alpha v2 (agent)");
    assert!(!ws.join("b.txt").exists());
    assert_eq!(read(&ws.join("c.txt")), "charlie (agent)");
    assert_eq!(read(&ws.join("user-note.txt")), "user data");
    assert_eq!(read(&ws.join("build.log")), "log v2 (user)");
    assert!(
        store.last_restore_target(WS_KEY).unwrap().is_none(),
        "the undo consumed the rewind it reversed, so there is nothing left to undo"
    );

    // Session lifetime GC: with the undo spent, dropping the thread leaves
    // nothing reachable and the store empties.
    assert!(store.disk_usage().unwrap() > 0);
    store.remove_thread(THREAD).unwrap();
    let gc = store.gc().unwrap();
    assert!(gc.manifests_removed > 0);
    assert!(gc.blobs_removed > 0);
    assert_eq!(gc.manifests_kept, 0);
    assert_eq!(gc.blobs_kept, 0);
}

#[cfg(unix)]
#[test]
fn restore_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();

    let script = ws.join("run.sh");
    fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let cp1 = store
        .checkpoint(
            THREAD,
            "turn-1",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();

    fs::write(&script, "#!/bin/sh\necho changed\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).unwrap();
    store
        .checkpoint(
            THREAD,
            "turn-2",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();

    store
        .restore_to(
            THREAD,
            WS_KEY,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind { conversation: None },
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
            &|_| false,
        )
        .unwrap();

    assert_eq!(read(&script), "#!/bin/sh\necho hi\n");
    let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "executable bit restored");
}

#[test]
fn thread_marker_and_pre_edit_attach() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();

    // Log existence is the session-scoped "tracking on" marker.
    assert!(!store.thread_exists(THREAD));
    store.ensure_thread(THREAD).unwrap();
    assert!(store.thread_exists(THREAD));
    store.ensure_thread(THREAD).unwrap(); // idempotent

    // Turn-start scan sees only a.txt.
    fs::write(ws.join("a.txt"), "alpha").unwrap();
    let cp1 = store
        .checkpoint(
            THREAD,
            "turn-1",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();

    // Agent edits a file OUTSIDE the workspace scan: pre-image attaches
    // retroactively under the same turn.
    let outside = dir.path().join("outside.cfg");
    let attached = store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &outside.to_string_lossy(),
            Some(b"pre-edit state"),
        )
        .unwrap()
        .expect("new path should attach");
    fs::write(&outside, "post-edit state").unwrap();

    // Already covered by the turn-start scan → nothing to add.
    assert!(
        store
            .attach_pre_edit(
                THREAD,
                "turn-1",
                &ws.join("a.txt").to_string_lossy(),
                Some(b"x")
            )
            .unwrap()
            .is_none()
    );
    // Born by this edit: there is no pre-image, but "it did not exist" is
    // itself worth recording — it is the evidence a later restore needs in
    // order to remove the file, and outside a complete scan nothing else
    // supplies it.
    let tombstoned = store
        .attach_pre_edit(THREAD, "turn-1", "/brand/new.txt", None)
        .unwrap()
        .expect("a created path is recorded as absent");
    assert!(
        store
            .manifest(&tombstoned)
            .unwrap()
            .absent
            .contains("/brand/new.txt")
    );
    // Recording it twice adds nothing.
    assert!(
        store
            .attach_pre_edit(THREAD, "turn-1", "/brand/new.txt", None)
            .unwrap()
            .is_none()
    );

    // Each supplement extends the previous one, and the turn resolves to the
    // most complete of them.
    let history = store.thread_history(THREAD).unwrap();
    assert_eq!(history.len(), 3, "turn-start scan, pre-image, tombstone");
    assert!(history.iter().all(|(r, _)| r.turn_id == "turn-1"));
    assert_eq!(history[1].0.manifest_id, attached);
    assert_eq!(history[1].1.entries.len(), cp1.manifest.entries.len() + 1);
    assert_eq!(history[2].0.manifest_id, tombstoned);
    assert_eq!(
        history[2].1.entries.len(),
        cp1.manifest.entries.len() + 1,
        "the tombstone carries the pre-image forward"
    );
    assert_eq!(
        store.target_for_turn("turn-1").unwrap().unwrap().manifest_id(),
        tombstoned
    );

    // Restoring to the supplemental manifest recovers the pre-edit content.
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind { conversation: None },
            workspace_files(&ws, /*include_hidden*/ false)
                .unwrap()
                .into_iter()
                .chain([outside.clone()]),
            true,
            &|_| false,
        )
        .unwrap();
    assert_eq!(read(&outside), "pre-edit state");
}

#[test]
fn turn_resolution_and_fork_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store
        .checkpoint(
            THREAD,
            "turn-1",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();
    // Supplemental attach under the same turn: resolution must pick it.
    let outside = dir.path().join("ext.cfg");
    let supplemental = store
        .attach_pre_edit(THREAD, "turn-1", &outside.to_string_lossy(), Some(b"pre"))
        .unwrap()
        .unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();
    store
        .checkpoint(
            THREAD,
            "turn-2",
            workspace_files(&ws, /*include_hidden*/ false).unwrap(),
            true,
        )
        .unwrap();

    assert_eq!(
        store
            .target_for_turn("turn-1")
            .unwrap()
            .map(|t| t.manifest_id().to_string()),
        Some(supplemental.clone()),
        "last entry for the turn wins"
    );
    assert!(store.target_for_turn("nope").unwrap().is_none());

    // tracked_paths covers the outside-workspace attach.
    let paths = store.tracked_paths(THREAD).unwrap();
    assert!(paths.contains(&outside.to_string_lossy().into_owned()));
    assert!(paths.iter().any(|p| p.ends_with("a.txt")));

    // Fork inherits entries through turn-1 (scan + supplemental), not turn-2.
    let inherited = store.inherit_log(THREAD, "fork-1", "turn-1").unwrap();
    assert_eq!(inherited, 2);
    assert!(store.thread_exists("fork-1"));
    let fork_history = store.thread_history("fork-1").unwrap();
    assert_eq!(fork_history.len(), 2);
    assert_eq!(fork_history.last().unwrap().0.manifest_id, supplemental);

    // Unknown turn: log created (tracking marker) but nothing inherited.
    assert_eq!(store.inherit_log(THREAD, "fork-2", "nope").unwrap(), 0);
    assert!(store.thread_exists("fork-2"));

    // Shared manifests survive GC while either thread references them.
    store.remove_thread(THREAD).unwrap();
    let gc = store.gc().unwrap();
    assert!(gc.manifests_kept >= 2, "fork-1 still references manifests");
}

#[test]
fn rewinding_twice_still_restores() {
    // Mirrors real use: rewind, undo it, rewind again. The second rewind must
    // put the files back, not quietly decide there is nothing to do.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();
    let scan = || workspace_files(&ws, /*include_hidden*/ false).unwrap();

    fs::write(ws.join("a.txt"), "v1").unwrap();
    let turn1 = store.checkpoint(THREAD, "turn-1", scan(), true).unwrap();

    fs::write(ws.join("a.txt"), "v2").unwrap();
    store.checkpoint(THREAD, "turn-2", scan(), true).unwrap();

    // Rewind to turn 1.
    let turn1_target = store.target_for_turn("turn-1").unwrap().unwrap();
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &turn1_target,
            RestoreKind::Rewind { conversation: None },
            scan(),
            true,
            &|_| false,
        )
        .unwrap();
    assert_eq!(read(&ws.join("a.txt")), "v1", "first rewind");

    // Undo that rewind.
    let safety = store.last_restore_target(WS_KEY).unwrap().unwrap();
    let out = store
        .restore_to(
            THREAD,
            WS_KEY,
            &safety,
            RestoreKind::Undo,
            scan(),
            true,
            &|_| false,
        )
        .unwrap();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v2",
        "undo restores the newer state"
    );

    // Rewind to turn 1 again — the case that regressed in real use. The
    // target must resolve to the same entry as before, even though the undo
    // recorded that very state again further down the log.
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &turn1_target,
            RestoreKind::Rewind { conversation: None },
            scan(),
            true,
            &|_| false,
        )
        .unwrap();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v1",
        "second rewind must still work"
    );
}

#[test]
fn the_conversation_an_undo_returns_to_is_kept_by_the_store() {
    // The thread a rewind leaves behind is an ordinary session: the user can
    // archive it, delete it, or keep talking in it. Undo therefore keeps its
    // own copy of the conversation rather than trusting that thread to still
    // be there, and in the state it was left in.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();
    let scan = || workspace_files(&ws, /*include_hidden*/ false).unwrap();

    fs::write(ws.join("a.txt"), "v1").unwrap();
    store.checkpoint(THREAD, "turn-1", scan(), true).unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();
    store.checkpoint(THREAD, "turn-2", scan(), true).unwrap();

    let rollout = b"{\"turn\":1}\n{\"turn\":2}\n".to_vec();
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind {
                conversation: Some(rollout.clone()),
            },
            scan(),
            true,
            &|_| false,
        )
        .unwrap();

    assert_eq!(
        store.last_restore_conversation(WS_KEY).unwrap(),
        Some(rollout),
        "undo can rebuild the conversation from the store alone"
    );

    // GC must treat that copy as live even though no manifest mentions it.
    let gc = store.gc().unwrap();
    assert!(gc.blobs_removed == 0 || gc.blobs_kept > 0);
    assert!(
        store.last_restore_conversation(WS_KEY).unwrap().is_some(),
        "the conversation copy survives collection"
    );

    // Spending the undo releases it.
    let safety = store.last_restore_target(WS_KEY).unwrap().unwrap();
    store
        .restore_to(
            THREAD,
            WS_KEY,
            &safety,
            RestoreKind::Undo,
            scan(),
            true,
            &|_| false,
        )
        .unwrap();
    assert!(store.last_restore_conversation(WS_KEY).unwrap().is_none());
}

#[test]
fn a_file_created_outside_the_scanned_scope_is_removed_by_a_rewind() {
    // The hard case for deletion: fallback mode (no project marker, so the
    // capture is bounded) and a file the agent creates *above* the scanned
    // directory. Absence from a bounded manifest proves nothing — the scan
    // never looked there — so the only thing that can license removing it is
    // the tombstone written when the edit created it.
    let dir = tempfile::tempdir().unwrap();
    let outer = dir.path().join("project");
    let cwd = outer.join("inner");
    fs::create_dir_all(&cwd).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();

    // Turn 1 starts with an empty scope: `inner/` holds nothing.
    let cp1 = store
        .checkpoint(
            THREAD,
            "turn-1",
            workspace_files(&cwd, /*include_hidden*/ false).unwrap(),
            /*complete*/ false,
        )
        .unwrap();
    assert!(cp1.manifest.entries.is_empty());
    assert!(!cp1.manifest.complete, "fallback captures are bounded");

    // The agent creates a file in the parent directory.
    let created = outer.join("hello.html");
    store
        .attach_pre_edit(
            THREAD,
            "turn-1",
            &created.to_string_lossy(),
            /*pre_content*/ None,
        )
        .unwrap()
        .expect("creating a file records that it did not exist");
    fs::write(&created, "generated").unwrap();

    // Rewind to turn 1. The safety scope has to include what the thread has
    // observed, or the new file is not even a candidate.
    let mut current: Vec<_> = workspace_files(&cwd, false).unwrap();
    current.extend(
        store
            .tracked_paths(THREAD)
            .unwrap()
            .into_iter()
            .map(std::path::PathBuf::from),
    );
    let outcome = store
        .restore_to(
            THREAD,
            WS_KEY,
            &store.target_for_turn("turn-1").unwrap().unwrap(),
            RestoreKind::Rewind { conversation: None },
            current,
            /*current_complete*/ false,
            &|_| false,
        )
        .unwrap();

    assert!(
        !created.exists(),
        "a tombstoned path is removed even though the capture was bounded"
    );
    assert_eq!(outcome.stats.deleted, 1);
}

#[test]
fn undo_walks_back_through_successive_rewinds() {
    // Rewinding twice in a row and then undoing twice has to retrace those
    // steps in reverse. If the undo target were simply "the last restore",
    // the second undo would reverse the first undo and the files would
    // oscillate between two states while the conversation kept moving back.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = SnapshotStore::open(dir.path().join("file_snapshots")).unwrap();
    let scan = || workspace_files(&ws, /*include_hidden*/ false).unwrap();

    for (turn, contents) in [("turn-1", "v1"), ("turn-2", "v2"), ("turn-3", "v3")] {
        fs::write(ws.join("a.txt"), contents).unwrap();
        store.checkpoint(THREAD, turn, scan(), true).unwrap();
    }

    let rewind = |turn: &str| {
        let target = store.target_for_turn(turn).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                WS_KEY,
                &target,
                RestoreKind::Rewind { conversation: None },
                scan(),
                true,
                &|_| false,
            )
            .unwrap();
    };
    let undo = || {
        let target = store.last_restore_target(WS_KEY).unwrap().unwrap();
        store
            .restore_to(
                THREAD,
                WS_KEY,
                &target,
                RestoreKind::Undo,
                scan(),
                true,
                &|_| false,
            )
            .unwrap();
    };

    rewind("turn-2");
    assert_eq!(read(&ws.join("a.txt")), "v2");
    rewind("turn-1");
    assert_eq!(read(&ws.join("a.txt")), "v1");

    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v2",
        "first undo retraces one step"
    );
    undo();
    assert_eq!(
        read(&ws.join("a.txt")),
        "v3",
        "second undo retraces the earlier rewind, back to where we started"
    );
    assert!(
        store.last_restore_target(WS_KEY).unwrap().is_none(),
        "both rewinds have been spent"
    );
}
