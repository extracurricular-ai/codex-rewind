# codex-file-snapshots

Git-free file snapshot storage for session rewind (RFC: `docs/rfc-file-snapshot-rewind.md`).

Content-addressed blob store + per-checkpoint manifests with a persistent
stat cache, thread-scoped snapshot logs, refcount-style garbage collection,
and a restore planner implementing the safety-checkpoint and
witnessed-birth deletion rules.

This crate is self-contained: it never touches the user's git state and has
no dependencies on other codex crates.
