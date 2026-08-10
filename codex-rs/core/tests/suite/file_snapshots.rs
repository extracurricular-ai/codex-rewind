//! End-to-end coverage for file-snapshot capture during real turns
//! (design: `docs/rfc-file-snapshot-rewind.md`). The unit tests in
//! `codex-file-snapshots` cover the store itself; these exercise the wiring:
//! that a tracking session actually records a checkpoint per turn, keyed by
//! the turn id a later rewind resolves against, and that a session without
//! the feature records nothing at all.

use anyhow::Result;
use codex_features::Feature;
use codex_file_snapshots::BlobStore;
use codex_file_snapshots::SnapshotStore;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use wiremock::MockServer;

/// A model turn that rewrites `notes.txt` from `before` to `after`, followed
/// by the assistant message that ends the turn.
fn patch_turn_bodies(call_id: &str, from: &str, to: &str) -> Vec<String> {
    let patch =
        format!("*** Begin Patch\n*** Update File: notes.txt\n@@\n-{from}\n+{to}\n*** End Patch\n");
    vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_apply_patch_custom_tool_call(call_id, &patch),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    ]
}

/// Session whose cwd is a snapshot workspace root (`.git`) holding a
/// `notes.txt` for the model to edit.
async fn tracking_session(server: &MockServer, feature_on: bool) -> Result<TestCodex> {
    let mut builder = test_codex()
        .with_config(move |config| {
            if feature_on {
                config
                    .features
                    .enable(Feature::FileSnapshots)
                    .expect("test config should allow enabling file_snapshots");
            }
        })
        .with_workspace_setup(|cwd, _fs| async move {
            std::fs::create_dir_all(cwd.as_path().join(".git"))?;
            std::fs::write(cwd.as_path().join("notes.txt"), "before\n")?;
            Ok(())
        });
    builder.build(server).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_start_checkpoint_captures_pre_turn_state() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = tracking_session(&server, /*feature_on*/ true).await?;
    mount_sse_sequence(&server, patch_turn_bodies("call-1", "before", "after")).await;

    test.submit_turn("please update notes.txt").await?;

    // The model's edit landed…
    assert_eq!(
        std::fs::read_to_string(test.workspace_path("notes.txt"))?,
        "after\n"
    );

    // …and the snapshot taken before the turn still holds the old content,
    // which is what makes rewinding to this turn possible.
    let store = SnapshotStore::open(test.codex_home_path().join("file_snapshots"))?;
    let thread_id = test.session_configured.thread_id.to_string();
    assert!(
        store.thread_exists(&thread_id),
        "thread marker should exist"
    );

    let history = store.thread_history(&thread_id)?;
    assert_eq!(history.len(), 1, "one turn, one checkpoint: {history:?}");
    let (snapshot_ref, manifest) = &history[0];
    assert!(
        manifest.complete,
        "a workspace-root scan is a complete capture"
    );

    let notes_key = test
        .workspace_path("notes.txt")
        .to_string_lossy()
        .into_owned();
    let entry = manifest
        .entries
        .get(&notes_key)
        .unwrap_or_else(|| panic!("notes.txt missing: {:?}", manifest.entries.keys()));
    let blobs = BlobStore::open(test.codex_home_path().join("file_snapshots").join("blobs"))?;
    assert_eq!(
        String::from_utf8(blobs.load(&entry.hash)?)?,
        "before\n",
        "the checkpoint must hold the pre-turn content"
    );

    // A rewind resolves the target through the global turn index, which is
    // what lets any branch holding that turn reach the same state.
    assert_eq!(
        store
            .target_for_turn(&snapshot_ref.turn_id)?
            .map(|target| target.manifest_id().to_string()),
        Some(snapshot_ref.manifest_id.clone())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_turn_records_its_own_checkpoint() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = tracking_session(&server, /*feature_on*/ true).await?;
    let mut bodies = patch_turn_bodies("call-1", "before", "after");
    bodies.extend(patch_turn_bodies("call-2", "after", "final"));
    mount_sse_sequence(&server, bodies).await;

    test.submit_turn("first edit").await?;
    test.submit_turn("second edit").await?;

    let store = SnapshotStore::open(test.codex_home_path().join("file_snapshots"))?;
    let thread_id = test.session_configured.thread_id.to_string();
    let history = store.thread_history(&thread_id)?;
    assert_eq!(history.len(), 2, "one checkpoint per turn: {history:?}");

    let turn_ids: Vec<&str> = history
        .iter()
        .map(|(entry, _)| entry.turn_id.as_str())
        .collect();
    assert_ne!(
        turn_ids[0], turn_ids[1],
        "checkpoints must be distinguishable by turn"
    );
    assert_ne!(
        history[0].1, history[1].1,
        "the second checkpoint observes the first turn's edit"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_feature_records_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = tracking_session(&server, /*feature_on*/ false).await?;
    mount_sse_sequence(&server, patch_turn_bodies("call-1", "before", "after")).await;

    test.submit_turn("please update notes.txt").await?;
    assert_eq!(
        std::fs::read_to_string(test.workspace_path("notes.txt"))?,
        "after\n",
        "the turn itself is unaffected by the feature"
    );

    let store_root = test.codex_home_path().join("file_snapshots");
    let thread_id = test.session_configured.thread_id.to_string();
    let tracking = SnapshotStore::open(&store_root)
        .map(|store| store.thread_exists(&thread_id))
        .unwrap_or(false);
    assert!(!tracking, "a session without the feature must not track");
    Ok(())
}
