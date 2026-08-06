//! Git-free file snapshot storage for session rewind.
//!
//! See `docs/rfc-file-snapshot-rewind.md` for the full design. This crate
//! implements Phase 1: the storage layer (content-addressed blobs,
//! stat-cached manifests) with no dependencies on other codex crates and
//! zero interaction with the user's git state.

mod blob;
mod error;
mod manifest;

pub use blob::BlobStore;
pub use error::Result;
pub use error::SnapshotError;
pub use manifest::FileEntry;
pub use manifest::Manifest;
pub use manifest::ManifestStore;
pub use manifest::mode_of;
pub use manifest::mtime_parts;
