//! End-to-end scenario from the RFC: checkpoint → agent edits →
//! checkpoint → user edits → rewind (with safety checkpoint,
//! witnessed-birth deletion, symmetric ignore) → redo → GC.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use codex_file_snapshots::SNAPSHOT_IGNORE_FILENAME;
use codex_file_snapshots::SnapshotStore;
use codex_file_snapshots::is_ignored;
use codex_file_snapshots::load_ignore;
use codex_file_snapshots::workspace_files;
use pretty_assertions::assert_eq;

const THREAD: &str = "thread-1";

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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
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
        .checkpoint(THREAD, "turn-2", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
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
            &cp1.id,
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

    // Redo: restore the safety checkpoint → pre-rewind state returns.
    let ignore2 = load_ignore(&ws);
    let protect2 = move |path: &str| is_ignored(&ignore2, Path::new(path));
    store
        .restore_to(
            THREAD,
            &outcome.safety_manifest_id,
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

    // Session lifetime GC: dropping the thread empties the store.
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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
        .unwrap();

    fs::write(&script, "#!/bin/sh\necho changed\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).unwrap();
    store
        .checkpoint(THREAD, "turn-2", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
        .unwrap();

    store
        .restore_to(
            THREAD,
            &cp1.id,
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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
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

    // Already covered by the scan → no-op. Born-by-edit (no pre-image) → no-op.
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
    assert!(
        store
            .attach_pre_edit(THREAD, "turn-1", "/brand/new.txt", None)
            .unwrap()
            .is_none()
    );

    // The supplemental manifest extends the turn-start one.
    let history = store.thread_history(THREAD).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].0.turn_id, "turn-1");
    assert_eq!(history[1].0.manifest_id, attached);
    assert!(history[1].1.entries.len() == cp1.manifest.entries.len() + 1);

    // Restoring to the supplemental manifest recovers the pre-edit content.
    store
        .restore_to(
            THREAD,
            &attached,
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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
        .unwrap();
    // Supplemental attach under the same turn: resolution must pick it.
    let outside = dir.path().join("ext.cfg");
    let supplemental = store
        .attach_pre_edit(THREAD, "turn-1", &outside.to_string_lossy(), Some(b"pre"))
        .unwrap()
        .unwrap();
    fs::write(ws.join("a.txt"), "v2").unwrap();
    store
        .checkpoint(THREAD, "turn-2", workspace_files(&ws, /*include_hidden*/ false).unwrap(), true)
        .unwrap();

    assert_eq!(
        store.manifest_id_for_turn(THREAD, "turn-1").unwrap(),
        Some(supplemental.clone()),
        "last entry for the turn wins"
    );
    assert_eq!(store.manifest_id_for_turn(THREAD, "nope").unwrap(), None);

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
