//! Restore planning and application (RFC §6.5, §7).
//!
//! Restores are planned as a diff between the target manifest and the
//! **safety checkpoint** — a capture of the current state taken
//! immediately before any restore, which makes every restore reversible
//! (redo = restore the safety manifest).
//!
//! Deletion requires positive evidence of absence: only a complete capture
//! (a full scope scan) licenses removing a file that is missing from the
//! target. Bounded captures never delete. Paths matched by the protection
//! predicate (the symmetric ignore rule) are untouched in both directions.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;
use crate::manifest::Manifest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAction {
    pub path: String,
    pub hash: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub writes: Vec<WriteAction>,
    pub deletes: Vec<String>,
}

/// Plan the move from `current` to `target`.
///
/// Deletion needs positive evidence that a file did not exist at the target.
/// A complete capture provides it — the scan covered the whole scope, so
/// absence means absence. A bounded capture (fallback mode) does not, so
/// nothing is deleted from it: leaving an extra file behind is recoverable,
/// deleting one the system never saw is not.
///
/// `is_protected` is the symmetric-ignore predicate over manifest path keys.
/// Both directions honour it, so an ignored path is neither written nor
/// removed.
pub fn plan_restore(
    target: &Manifest,
    current: &Manifest,
    is_protected: &dyn Fn(&str) -> bool,
) -> RestorePlan {
    let mut plan = RestorePlan::default();

    for (path, entry) in &target.entries {
        if is_protected(path) {
            continue;
        }
        let differs = current
            .entries
            .get(path)
            .is_none_or(|cur| cur.hash != entry.hash || cur.mode != entry.mode);
        if differs {
            plan.writes.push(WriteAction {
                path: path.clone(),
                hash: entry.hash.clone(),
                mode: entry.mode,
            });
        }
    }

    if target.complete {
        for path in current.entries.keys() {
            if !target.entries.contains_key(path) && !is_protected(path) {
                plan.deletes.push(path.clone());
            }
        }
    }
    plan
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyStats {
    pub written: usize,
    pub deleted: usize,
}

/// Apply a plan: write blob contents (atomically, restoring permissions)
/// and delete witnessed-birth files. Missing delete targets are fine.
pub fn apply_plan(blobs: &BlobStore, plan: &RestorePlan) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    for write in &plan.writes {
        let path = PathBuf::from(&write.path);
        let content = blobs.load(&write.hash)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| SnapshotError::io(parent, e))?;
        }
        let tmp = tmp_path(&path);
        fs::write(&tmp, &content).map_err(|e| SnapshotError::io(&tmp, e))?;
        set_mode(&tmp, write.mode)?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        stats.written += 1;
    }
    for del in &plan.deletes {
        let path = PathBuf::from(del);
        match fs::remove_file(&path) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SnapshotError::io(&path, e)),
        }
    }
    Ok(stats)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".codex-restore-tmp");
    path.with_file_name(name)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| SnapshotError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::manifest::FileEntry;
    use pretty_assertions::assert_eq;

    fn manifest(entries: &[(&str, &str)], complete: bool) -> Manifest {
        let mut m = Manifest {
            complete,
            ..Default::default()
        };
        for (path, hash) in entries {
            m.entries.insert(
                (*path).to_string(),
                FileEntry {
                    mode: 0o644,
                    size: hash.len() as u64,
                    mtime_secs: 0,
                    mtime_nanos: 0,
                    hash: (*hash).to_string(),
                },
            );
        }
        m
    }

    #[test]
    fn writes_files_that_differ_or_are_missing() {
        let target = manifest(&[("/a", "h-old"), ("/b", "h-b")], true);
        let current = manifest(&[("/a", "h-new")], true);

        let plan = plan_restore(&target, &current, &|_| false);
        let paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b"]);
    }

    #[test]
    fn identical_states_need_no_work() {
        let m = manifest(&[("/a", "h")], true);
        assert_eq!(plan_restore(&m, &m, &|_| false), RestorePlan::default());
    }

    #[test]
    fn a_complete_target_licenses_deleting_what_it_lacks() {
        let target = manifest(&[("/kept", "h-k")], /*complete*/ true);
        let current = manifest(&[("/kept", "h-k"), ("/added", "h-a")], true);

        let plan = plan_restore(&target, &current, &|_| false);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.deletes, vec!["/added"]);
    }

    #[test]
    fn a_bounded_target_never_deletes() {
        // Absence from a partial capture is not evidence of absence.
        let target = manifest(&[("/kept", "h-k")], /*complete*/ false);
        let current = manifest(&[("/kept", "h-k"), ("/unknown", "h-u")], false);

        let plan = plan_restore(&target, &current, &|_| false);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn protected_paths_are_untouched_in_both_directions() {
        let target = manifest(&[("/secret/a", "h-1"), ("/ok", "h-ok")], true);
        let current = manifest(&[("/secret/b", "h-2")], true);

        let protect = |p: &str| p.starts_with("/secret/");
        let plan = plan_restore(&target, &current, &protect);
        let write_paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(
            write_paths,
            vec!["/ok"],
            "protected target entry not restored"
        );
        assert!(
            plan.deletes.is_empty(),
            "protected current entry not deleted"
        );
    }

    #[test]
    fn restoring_is_idempotent_for_a_given_target() {
        // The same target must plan the same way no matter what happened in
        // between — the property that broke when targets were located by
        // content and a redo made an older state recur.
        let target = manifest(&[("/a", "h-old")], true);
        let current = manifest(&[("/a", "h-new")], true);

        let first = plan_restore(&target, &current, &|_| false);
        let after_undo = plan_restore(&target, &current, &|_| false);
        assert_eq!(first, after_undo);
    }
}
