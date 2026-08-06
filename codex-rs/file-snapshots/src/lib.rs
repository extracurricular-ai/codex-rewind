//! Git-free file snapshot storage for session rewind.
//!
//! See `docs/rfc-file-snapshot-rewind.md` for the full design. This crate
//! implements Phase 1: the storage layer (content-addressed blobs,
//! stat-cached manifests) with no dependencies on other codex crates and
//! zero interaction with the user's git state.

mod blob;
mod checkpoint;
mod error;
mod manifest;
mod scope;

pub use blob::BlobStore;
pub use checkpoint::Checkpoint;
pub use checkpoint::CheckpointStats;
pub use checkpoint::capture;
pub use error::Result;
pub use error::SnapshotError;
pub use manifest::FileEntry;
pub use manifest::Manifest;
pub use manifest::ManifestStore;
pub use manifest::mode_of;
pub use manifest::mtime_parts;
pub use scope::SNAPSHOT_IGNORE_FILENAME;
pub use scope::SeedPolicy;
pub use scope::TrackedSet;
pub use scope::find_workspace_root;
pub use scope::is_ignored;
pub use scope::load_ignore;
pub use scope::seed_fallback_files;
pub use scope::workspace_files;
