//! Git-free file snapshot storage for session rewind.
//!
//! See `docs/rfc-file-snapshot-rewind.md` for the full design. This crate
//! implements Phase 1: content-addressed blobs, stat-cached manifests,
//! per-thread snapshot logs with mark-and-sweep GC, and a restore planner
//! implementing the safety-checkpoint and witnessed-birth rules — with no
//! dependencies on other codex crates and zero interaction with the
//! user's git state.

mod blob;
mod checkpoint;
mod controller;
mod error;
mod manifest;
mod refs;
mod restore;
mod scope;
mod store;

pub use blob::BlobStore;
pub use checkpoint::Checkpoint;
pub use checkpoint::CheckpointStats;
pub use checkpoint::capture;
pub use controller::FileSnapshotsController;
pub use error::Result;
pub use error::SnapshotError;
pub use manifest::FileEntry;
pub use manifest::Manifest;
pub use manifest::ManifestStore;
pub use manifest::mode_of;
pub use manifest::mtime_parts;
pub use refs::GcStats;
pub use refs::RefStore;
pub use refs::SnapshotRef;
pub use refs::ThreadLog;
pub use refs::collect_garbage;
pub use restore::ApplyStats;
pub use restore::RestorePlan;
pub use restore::WriteAction;
pub use restore::apply_plan;
pub use restore::plan_restore;
pub use scope::SNAPSHOT_IGNORE_FILENAME;
pub use scope::find_workspace_root;
pub use scope::git_tracked_files;
pub use scope::is_ignored;
pub use scope::load_ignore;
pub use scope::recent_files;
pub use scope::tracked_files;
pub use store::RestoreKind;
pub use store::RestoreOutcome;
pub use store::RestoreTarget;
pub use store::SAFETY_TURN_PREFIX;
pub use store::SnapshotStore;
pub use store::forget_threads;
