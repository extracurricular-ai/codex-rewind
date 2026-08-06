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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws).unwrap(), true)
        .unwrap();
    assert_eq!(cp1.manifest.entries.len(), 3, "log is not snapshotted");

    // Agent work during turn 1: modify a, delete b, create c.
    fs::write(ws.join("a.txt"), "alpha v2 (agent)").unwrap();
    fs::remove_file(ws.join("b.txt")).unwrap();
    fs::write(ws.join("c.txt"), "charlie (agent)").unwrap();

    // Turn 2 checkpoint observes the agent's changes.
    let cp2 = store
        .checkpoint(THREAD, "turn-2", workspace_files(&ws).unwrap(), true)
        .unwrap();
    assert!(cp2.manifest.entries.len() == 3);

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
            workspace_files(&ws).unwrap(),
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
            workspace_files(&ws).unwrap(),
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
        .checkpoint(THREAD, "turn-1", workspace_files(&ws).unwrap(), true)
        .unwrap();

    fs::write(&script, "#!/bin/sh\necho changed\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).unwrap();
    store
        .checkpoint(THREAD, "turn-2", workspace_files(&ws).unwrap(), true)
        .unwrap();

    store
        .restore_to(
            THREAD,
            &cp1.id,
            workspace_files(&ws).unwrap(),
            true,
            &|_| false,
        )
        .unwrap();

    assert_eq!(read(&script), "#!/bin/sh\necho hi\n");
    let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "executable bit restored");
}
