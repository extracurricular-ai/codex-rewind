//! Restore planning and application (RFC §6.5, §7).
//!
//! Restores are planned as a diff between the target manifest and the
//! **safety checkpoint** — a capture of the current state taken
//! immediately before any restore, which makes every restore reversible
//! (redo = restore the safety manifest).
//!
//! Deletions follow the **witnessed-birth rule**: a file is deleted only
//! if the snapshot system actually observed its creation (its first
//! appearance in the thread's manifest history is *after* the target).
//! Files never seen before, and paths matched by the protection predicate
//! (the symmetric ignore rule), are never touched.

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

/// Plan a restore over a thread's manifest `history` (in capture order,
/// whose **last element must be the safety checkpoint** of the current
/// state). `target_index` selects the state to restore; `is_protected`
/// is the symmetric-ignore predicate evaluated on manifest path keys.
pub fn plan_restore(
    history: &[Manifest],
    target_index: usize,
    is_protected: &dyn Fn(&str) -> bool,
) -> RestorePlan {
    let mut plan = RestorePlan::default();
    let Some(current) = history.last() else {
        return plan;
    };
    let Some(target) = history.get(target_index) else {
        return plan;
    };

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

    for path in current.entries.keys() {
        if target.entries.contains_key(path) || is_protected(path) {
            continue;
        }
        // Delete only on positive evidence of non-existence at the target:
        // either the target was a complete scope scan (absence is
        // definitive), or the file's birth was witnessed (its earliest
        // appearance in history is strictly after the target). Either way
        // the file is recoverable from the safety checkpoint.
        let deletable = target.complete || {
            let first_seen = history.iter().position(|m| m.entries.contains_key(path));
            first_seen.is_some_and(|idx| idx > target_index)
        };
        if deletable {
            plan.deletes.push(path.clone());
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

    fn manifest(entries: &[(&str, &str)]) -> Manifest {
        let mut m = Manifest::default();
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
    fn plans_writes_for_changed_and_missing_files() {
        let target = manifest(&[("/a", "h-old"), ("/b", "h-b")]);
        let current = manifest(&[("/a", "h-new")]); // /b deleted since target
        let history = vec![target, current];

        let plan = plan_restore(&history, 0, &|_| false);
        let paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b"]);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn deletes_only_witnessed_births() {
        let target = manifest(&[("/a", "h-a")]);
        let mid = manifest(&[("/a", "h-a"), ("/born-later", "h-c")]);
        // Safety checkpoint: /born-later still present, /user-file appears
        // for the first time ever (scanned only now).
        let current = manifest(&[("/a", "h-a"), ("/born-later", "h-c"), ("/user-file", "h-u")]);
        let history = vec![target, mid, current];

        let plan = plan_restore(&history, 0, &|_| false);
        assert!(plan.writes.is_empty());
        // Both were first seen after the target (index 1 and 2) → deletable,
        // and both are recoverable from the safety checkpoint.
        assert_eq!(plan.deletes, vec!["/born-later", "/user-file"]);
    }

    #[test]
    fn files_seen_at_or_before_target_are_never_deleted() {
        // /pre existed at the target and was deleted before `mid`; it is
        // back on disk now. Restoring to target keeps it (hash matches) —
        // and a file first seen at an *earlier* index than the target is
        // likewise not deletable.
        let earlier = manifest(&[("/pre", "h-p"), ("/gone-at-target", "h-g")]);
        let target = manifest(&[("/pre", "h-p")]);
        let current = manifest(&[("/pre", "h-p"), ("/gone-at-target", "h-g")]);
        let history = vec![earlier, target, current];

        let plan = plan_restore(&history, 1, &|_| false);
        assert!(plan.writes.is_empty());
        assert!(
            plan.deletes.is_empty(),
            "first seen at index 0 <= target 1 → protected by witnessed-birth rule"
        );
    }

    #[test]
    fn protected_paths_are_untouched_in_both_directions() {
        let target = manifest(&[("/secret/a", "h-1"), ("/ok", "h-ok")]);
        let current = manifest(&[("/secret/b", "h-2")]);
        let history = vec![target, current];

        let protect = |p: &str| p.starts_with("/secret/");
        let plan = plan_restore(&history, 0, &protect);
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
    fn complete_target_deletes_on_definitive_absence() {
        // A file recreated by a prior restore predates the target in
        // history (first seen at index 0), but the target was a complete
        // scan, so its absence there is definitive → deletable.
        let earlier = manifest(&[("/recreated", "h-r")]);
        let mut target = manifest(&[("/kept", "h-k")]);
        target.complete = true;
        let current = manifest(&[("/kept", "h-k"), ("/recreated", "h-r")]);
        let history = vec![earlier, target, current];

        let plan = plan_restore(&history, 1, &|_| false);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.deletes, vec!["/recreated"]);
    }

    #[test]
    fn restoring_to_latest_is_a_noop() {
        let m = manifest(&[("/a", "h")]);
        let history = vec![m.clone(), m];
        let plan = plan_restore(&history, 1, &|_| false);
        assert_eq!(plan, RestorePlan::default());
    }
}
