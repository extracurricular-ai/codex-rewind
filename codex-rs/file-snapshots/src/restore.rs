//! Restore planning and application (RFC §6.5, §7).
//!
//! Restores are planned as a diff between the target manifest and the
//! **safety checkpoint** — a capture of the current state taken
//! immediately before any restore, which makes every restore reversible
//! (redo = restore the safety manifest).
//!
//! Deletion requires positive evidence of absence, and the only thing that
//! counts is the target having looked: a path it was asked about and did not
//! find. Nothing is inferred from a path merely being missing. Paths matched
//! by the protection predicate (the symmetric ignore rule) are untouched in
//! both directions.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
/// Deletion needs positive evidence that a file did not exist at the target,
/// which only `target.absent` supplies: the capture looked for that path and
/// found nothing. Leaving an extra file behind is recoverable; deleting one
/// the system never observed is not.
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

    // One ground for deleting: the target looked for this path and did not
    // find it. Nothing is inferred from a path simply being missing from the
    // target — a capture only ever sees what it was asked about, so absence
    // from `entries` alone says nothing about whether the file existed.
    for path in current.entries.keys() {
        // A path the target also has content for is never deleted, whatever
        // `absent` says. The two records cannot both be about the same file
        // unless two spellings of one path reached the manifest — different
        // case on APFS or NTFS, a separator that came from the git index —
        // and in that state the writes above run first, so deleting here
        // would restore the file and then remove it. Contradictory evidence
        // is not licence to delete; it is a reason to leave the file alone.
        if target.entries.contains_key(path) {
            continue;
        }
        if !is_protected(path) && target.absent.contains(path) {
            plan.deletes.push(path.clone());
        }
    }
    plan
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyStats {
    pub written: usize,
    pub deleted: usize,
    /// `(path, reason)` for everything the plan could not apply. Non-empty is
    /// not an error: the restore that did happen is recorded and undoable, and
    /// the caller tells the user what was left alone.
    pub failed: Vec<(String, String)>,
}

/// Apply a plan: write blob contents (atomically, restoring permissions)
/// and remove paths the target recorded as absent. Missing targets are fine.
///
/// **Nothing here abandons the loop.** A restore that stops half way is the
/// one outcome worse than a restore that fails: the caller files the undo
/// record *after* this returns, so an early `?` leaves writes landed, deletes
/// half done, and no way back. Whatever could not be applied is collected and
/// reported instead, so the undo record is always written and the user is told
/// which files were left alone. Windows reaches this constantly — a file open
/// in an editor cannot be replaced, and the read-only attribute blocks a
/// delete outright, where Unix goes by directory permissions.
pub fn apply_plan(blobs: &BlobStore, plan: &RestorePlan) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    for write in &plan.writes {
        let path = PathBuf::from(&write.path);
        let content = match blobs.load(&write.hash) {
            Ok(content) => content,
            Err(err) => {
                stats.failed.push((write.path.clone(), err.to_string()));
                continue;
            }
        };
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            stats.failed.push((write.path.clone(), err.to_string()));
            continue;
        }
        match write_then_replace(&path, &content, write.mode) {
            Ok(()) => stats.written += 1,
            Err(err) => stats.failed.push((write.path.clone(), err.to_string())),
        }
    }
    for del in &plan.deletes {
        let path = PathBuf::from(del);
        match fs::remove_file(&path) {
            Ok(()) => stats.deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => stats.failed.push((del.clone(), e.to_string())),
        }
    }
    Ok(stats)
}

/// Write `content` to a temp file beside `path` and rename it over the top.
///
/// The temp file is created **exclusively**, and that is a security property
/// rather than tidiness. The old name was `<file>.codex-restore-tmp` — fixed,
/// guessable, and opened with a plain `fs::write`, which follows symlinks. A
/// symlink sitting at that path sends the restored content wherever it points,
/// outside the workspace included. Nobody needs to attack the machine for
/// that: the agent whose work is being undone can create it, a crashed restore
/// can leave one behind, and a *cloned repository* can simply contain one.
///
/// `create_new` is `O_CREAT | O_EXCL`, which fails on an existing path instead
/// of following it. The pid and counter keep two restores, or two sessions,
/// from meeting on one name.
fn write_then_replace(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    write_exclusive(&tmp, content)?;
    let applied = set_mode(&tmp, mode)
        .map_err(|err| std::io::Error::other(err.to_string()))
        .and_then(|()| fs::rename(&tmp, path));
    if applied.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    applied
}

/// Create `tmp` and write `content` into it, refusing to touch anything that
/// is already there.
///
/// Split out so the refusal can be tested directly: `tmp_path` randomises the
/// name, so a test cannot plant anything at the path a real restore will pick
/// — which is the point of randomising it, and also why the guarantee has to
/// be pinned here instead.
fn write_exclusive(tmp: &Path, content: &[u8]) -> std::io::Result<()> {
    // `create_new` is O_CREAT | O_EXCL: it fails on an existing path rather
    // than following it. A plain `fs::write` would open through a symlink.
    let mut file = fs::File::create_new(tmp)?;
    std::io::Write::write_all(&mut file, content)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.codex-restore-tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

/// Distinguishes concurrent restores within one process; the pid covers the
/// cross-process half.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    /// `plan.deletes` is computed from the safety checkpoint, and the
    /// workspace is not frozen between that and `apply_plan` — a formatter, a
    /// file watcher or a shell command can remove one of those paths in the
    /// window. Without the NotFound arm the loop returns Err mid-way, `store`
    /// propagates it with `?` *before* the undo record is filed, and the user
    /// is left with writes landed, some deletes landed, some not, and no way
    /// to undo. An extra file left behind is recoverable; that state is not.
    ///
    /// The already-gone path is ordered first on purpose. That is what makes
    /// this an assertion about the loop continuing rather than about a count.
    ///
    /// Unreachable through `SnapshotStore::restore`, which is why it lives
    /// here: a path already missing when the safety checkpoint runs never
    /// enters `current.entries`, so it never becomes a delete.
    #[test]
    fn a_delete_target_that_is_already_gone_does_not_abandon_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let hash = blobs.store_bytes(b"restored").unwrap();

        let survivor = dir.path().join("still-here.txt");
        std::fs::write(&survivor, "delete me").unwrap();
        let written = dir.path().join("written.txt");

        let plan = RestorePlan {
            writes: vec![WriteAction {
                path: written.to_string_lossy().into_owned(),
                hash,
                mode: 0o644,
            }],
            deletes: vec![
                dir.path()
                    .join("vanished.txt")
                    .to_string_lossy()
                    .into_owned(),
                survivor.to_string_lossy().into_owned(),
            ],
        };

        let stats = apply_plan(&blobs, &plan).unwrap();

        assert_eq!(
            stats,
            ApplyStats {
                written: 1,
                deleted: 1,
                failed: Vec::new()
            },
            "the vanished path is not counted, and does not stop the one after it"
        );
        assert!(!survivor.exists(), "the delete after the vanished one ran");
        assert_eq!(std::fs::read(&written).unwrap(), b"restored");
    }

    /// A symlink sitting where the restore is about to write must stop it,
    /// not redirect it. It does not take a compromised machine to plant one:
    /// a cloned repository can contain it, and so can the agent whose work is
    /// being undone.
    ///
    /// Tested on `write_exclusive` rather than through `apply_plan`, because
    /// `tmp_path` randomises the name — a test cannot plant anything at the
    /// path a real restore picks, which is itself part of the defence.
    ///
    /// Unix-only because Windows needs Developer Mode or a privilege to create
    /// a symlink at all; that is what makes the attack largely unreachable
    /// there, not the code.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_temp_path_cannot_redirect_a_restore() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"do not touch").unwrap();

        let planted = dir.path().join("target.txt.codex-restore-tmp");
        std::os::unix::fs::symlink(&outside, &planted).unwrap();

        let err = write_exclusive(&planted, b"restored content")
            .expect_err("an existing path must be refused, not followed");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"do not touch",
            "nothing was written through the link"
        );
    }

    /// Contradictory evidence is not a licence to delete. If one file reaches
    /// a manifest under two spellings — different case on APFS or NTFS, a
    /// separator that came from the git index — it can hold both a content
    /// record and a tombstone. Writes run before deletes, so without this
    /// guard a rewind would restore the file and then remove it, which is
    /// strictly worse than either half alone.
    ///
    /// The aliasing is constructed here rather than provoked, so the guard is
    /// pinned on every platform and not only where the filesystem can produce
    /// it.
    #[test]
    fn a_path_the_target_still_has_content_for_is_never_deleted() {
        let target = Manifest {
            entries: manifest(&[("/ws/a.txt", "hash-a")]).entries,
            absent: std::collections::BTreeSet::from(["/ws/a.txt".to_string()]),
        };
        let current = manifest(&[("/ws/a.txt", "different")]);

        let plan = plan_restore(&target, &current, &|_| false);

        assert!(
            plan.deletes.is_empty(),
            "a tombstone contradicted by a content record deletes nothing: {:?}",
            plan.deletes
        );
        assert_eq!(plan.writes.len(), 1, "the content is still restored");
    }

    /// A restore that stops half way is worse than one that fails: the undo
    /// record is filed by the caller *after* this returns, so an early `?`
    /// would leave writes landed, deletes half done, and no way back. Windows
    /// meets this routinely — a file open in an editor cannot be replaced.
    #[test]
    fn a_write_that_cannot_land_does_not_abandon_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let hash = blobs.store_bytes(b"content").unwrap();

        // First write is impossible: its parent is a regular file, so
        // `create_dir_all` cannot make the directory it needs.
        let wall = dir.path().join("a-file");
        std::fs::write(&wall, b"not a directory").unwrap();
        let blocked = wall.join("nested.txt");
        let reachable = dir.path().join("reachable.txt");

        let plan = RestorePlan {
            writes: vec![
                WriteAction {
                    path: blocked.to_string_lossy().into_owned(),
                    hash: hash.clone(),
                    mode: 0o644,
                },
                WriteAction {
                    path: reachable.to_string_lossy().into_owned(),
                    hash,
                    mode: 0o644,
                },
            ],
            deletes: Vec::new(),
        };
        let stats = apply_plan(&blobs, &plan).unwrap();

        assert_eq!(stats.written, 1, "the write after the failure still ran");
        assert_eq!(stats.failed.len(), 1);
        assert!(stats.failed[0].0.contains("nested.txt"));
        assert_eq!(std::fs::read(&reachable).unwrap(), b"content");
    }

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
    fn writes_files_that_differ_or_are_missing() {
        let target = manifest(&[("/a", "h-old"), ("/b", "h-b")]);
        let current = manifest(&[("/a", "h-new")]);

        let plan = plan_restore(&target, &current, &|_| false);
        let paths: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b"]);
    }

    #[test]
    fn identical_states_need_no_work() {
        let m = manifest(&[("/a", "h")]);
        assert_eq!(plan_restore(&m, &m, &|_| false), RestorePlan::default());
    }

    #[test]
    fn deletion_needs_the_target_to_have_looked() {
        // The single rule. Earlier designs inferred absence from a scan having
        // been exhaustive, which made every deletion depend on a premise that
        // cost a full tree walk — and still did not hold for paths the edit
        // hook contributed from outside any scan.
        let target = manifest(&[("/kept", "h-k")]);
        let current = manifest(&[("/kept", "h-k"), ("/added", "h-a")]);

        let plan = plan_restore(&target, &current, &|_| false);
        assert!(
            plan.deletes.is_empty(),
            "missing from the target says nothing on its own"
        );

        // Recorded as looked-for-and-absent, it says everything.
        let mut target = target;
        target.absent.insert("/added".to_string());
        let plan = plan_restore(&target, &current, &|_| false);
        assert_eq!(plan.deletes, vec!["/added"]);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn protected_paths_are_untouched_in_both_directions() {
        let target = manifest(&[("/secret/a", "h-1"), ("/ok", "h-ok")]);
        let current = manifest(&[("/secret/b", "h-2")]);

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
        let target = manifest(&[("/a", "h-old")]);
        let current = manifest(&[("/a", "h-new")]);

        let first = plan_restore(&target, &current, &|_| false);
        let after_undo = plan_restore(&target, &current, &|_| false);
        assert_eq!(first, after_undo);
    }
}
