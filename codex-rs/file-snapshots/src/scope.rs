//! Tracking scope: which files a checkpoint observes (RFC §6.2).
//!
//! Three partitions, unioned. Each answers a different question, and each is
//! bounded by something other than the size of the directory tree — which is
//! the property a plain subtree walk lacks, and the reason it was abandoned:
//! on this repository it enumerated 57k files and 100 GB, almost all of it
//! build output.
//!
//! 1. **Git-tracked** — what the project itself considers its files, read
//!    from the index. Bounded by the project rather than by what has been
//!    built into it: the same tree that scanned to 100 GB is 6k files and
//!    56 MB here, because build output is precisely what is not committed.
//!    Empty when there is no repository, which costs nothing — the other two
//!    partitions carry that case.
//! 2. **Edit-touched** — paths the agent's own tools have written, carried
//!    forward for the session. Unbounded on purpose: its size is set by what
//!    the agent did, not by what is on disk, and every entry is a file
//!    someone deliberately changed.
//! 3. **Recently modified** — the residue. Catches shell-made changes to
//!    files outside the other two, where "changed recently" is exactly the
//!    signal that matters. Capped, since this is the one partition a large
//!    tree can flood.
//!
//! The ignore file uses gitignore syntax but is deliberately separate from
//! `.gitignore`: session history may track files git ignores, and ignore
//! semantics here are symmetric — an ignored path is never snapshotted,
//! never restored, and never deleted by a restore.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;

use crate::error::Result;

/// Name of the dedicated snapshot ignore file (provisional; the final
/// name is an open question in the RFC).
pub const SNAPSHOT_IGNORE_FILENAME: &str = ".codexsnapignore";

/// Git-style marker walk-up: return the nearest ancestor of `start`
/// (inclusive) containing one of `markers`.
pub fn find_workspace_root(start: &Path, markers: &[String]) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for marker in markers {
            if d.join(marker).exists() {
                return Some(d.to_path_buf());
            }
        }
        dir = d.parent();
    }
    None
}

/// Compile the ignore matcher for `root` from its snapshot ignore file.
/// A missing ignore file yields an empty matcher (nothing ignored).
pub fn load_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(SNAPSHOT_IGNORE_FILENAME));
    // An unparsable ignore file degrades to "nothing ignored" rather than
    // failing the checkpoint; restores stay conservative either way.
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Symmetric protection check: is `path` invisible to snapshot operations?
pub fn is_ignored(ignore: &Gitignore, path: &Path) -> bool {
    ignore.matched_path_or_any_parents(path, false).is_ignore()
}

/// Enumerate the tracked files of a workspace: every regular file under
/// `root`, minus `.git`, minus paths matched by the snapshot ignore file,
/// and — unless `include_hidden` — minus dot-files and dot-directories.
///
/// Hidden entries are excluded by default because they are mostly tool
/// state rather than work product: editor settings, virtualenvs, caches,
/// credentials. Rewinding a turn should not roll those back. Anything the
/// agent actually edits is tracked through the edit hook regardless, so an
/// explicitly modified `.github/workflows/ci.yml` still restores.
/// Results are sorted for deterministic manifests.
pub fn workspace_files(root: &Path, include_hidden: bool) -> Result<Vec<PathBuf>> {
    let ignore = load_ignore(root);
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        // `.git` is excluded even when hidden entries are tracked: restoring
        // repository internals would corrupt the user's history.
        .hidden(!include_hidden)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();

    let mut out = BTreeSet::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if is_ignored(&ignore, entry.path()) {
            continue;
        }
        out.insert(entry.into_path());
    }
    Ok(out.into_iter().collect())
}

/// Largest file the recency partition will pick up.
///
/// Its job is to exclude pathological objects — media, model weights,
/// database files — not to draw a line through text. Generated code, lock
/// files, notebooks and fixtures routinely reach a few MB and are exactly
/// what a user wants back, so the limit sits well above them. Git-tracked
/// files are not subject to it: whatever the project commits is the
/// project's own content, however large.
pub const RECENT_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// How many recently-modified files the residue partition carries.
pub const RECENT_LIMIT: usize = 100;

/// Directory names the recency partition never descends into.
///
/// Decision 6 rejected exclusion lists as too inflexible, and preferred an
/// activity model. That preference stands for *what to keep* — this is a
/// different question. These directories churn constantly, so recency
/// selects them ahead of everything else and the partition fills with build
/// output before reaching a single source file. Excluding them is what makes
/// the activity signal usable, not a replacement for it.
const RECENT_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
];

/// Everything a capture should look at: the three partitions over `roots`,
/// plus `already_known` — paths this thread has observed before, whatever
/// partition first brought them in.
///
/// Both the turn-start checkpoint and the safety checkpoint a restore takes
/// go through here. They have to agree: a path the turn captured but the
/// safety checkpoint missed would be restorable and then not undoable, which
/// is worse than not having captured it at all. Sharing the function is the
/// only way that symmetry survives future edits to either side.
pub fn tracked_files(
    roots: &[PathBuf],
    already_known: impl IntoIterator<Item = PathBuf>,
    include_hidden: bool,
) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    for root in roots {
        let ignore = load_ignore(root);
        files.extend(git_tracked_files(root, &ignore));
        files.extend(recent_files(root, &ignore, include_hidden));
    }
    files.extend(already_known);
    files
}

/// Files the project's own index lists, relative to `root`.
///
/// Reads the index directly rather than shelling out: the index is a file,
/// `git ls-files` is a process, and this runs at the head of every turn.
/// Returns empty for anything that is not a repository, which is not an
/// error — it just means this partition contributes nothing.
pub fn git_tracked_files(root: &Path, ignore: &Gitignore) -> Vec<PathBuf> {
    let Ok(repo) = gix::open(root) else {
        return Vec::new();
    };
    let Ok(index) = repo.index_or_empty() else {
        return Vec::new();
    };
    let workdir = repo.workdir().unwrap_or(root);
    index
        .entries()
        .iter()
        .filter_map(|entry| {
            let rel = entry.path(&index);
            let path = workdir.join(std::str::from_utf8(rel).ok()?);
            (!is_ignored(ignore, &path) && path.is_file()).then_some(path)
        })
        .collect()
}

/// The most recently modified files under `dir`, as the residue partition.
///
/// Bounded three ways — count, per-file size, and the churn directories
/// above — because this is the partition a large tree would otherwise flood.
pub fn recent_files(dir: &Path, ignore: &Gitignore, include_hidden: bool) -> Vec<PathBuf> {
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(!include_hidden)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != ".git" && !RECENT_SKIP_DIRS.iter().any(|skip| name == *skip)
        })
        .build();

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if is_ignored(ignore, &path) {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() > RECENT_MAX_FILE_BYTES {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        candidates.push((modified, path));
    }
    candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    candidates.truncate(RECENT_LIMIT);
    candidates.into_iter().map(|(_, path)| path).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    fn touch(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    #[test]
    fn walk_up_finds_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let deep = root.join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(root.join(".codex")).unwrap();

        let markers = vec![".codex".to_string()];
        assert_eq!(
            find_workspace_root(&deep, &markers),
            Some(root),
            "nearest ancestor with marker wins"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(find_workspace_root(elsewhere.path(), &markers), None);
    }

    #[test]
    fn workspace_files_respect_snapshot_ignore_and_skip_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("keep.txt"), "k");
        touch(&root.join("skip.log"), "s");
        fs::create_dir_all(root.join("sub")).unwrap();
        touch(&root.join("sub/also.log"), "s");
        touch(&root.join("sub/keep2.txt"), "k");
        fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join(".git/HEAD"), "ref");
        touch(&root.join(SNAPSHOT_IGNORE_FILENAME), "*.log\n");

        let files = workspace_files(root, /*include_hidden*/ false).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["keep.txt".to_string(), "sub/keep2.txt".to_string()],
            "logs ignored; .git and other dot-entries skipped — including the \
             ignore file itself, which follows the same rule as any other \
             hidden file and is tracked only if the agent edits it"
        );

        // Opting in reaches hidden entries, but never `.git`.
        let with_hidden = workspace_files(root, /*include_hidden*/ true).unwrap();
        let names: Vec<String> = with_hidden
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&SNAPSHOT_IGNORE_FILENAME.to_string()));
        assert!(
            names.iter().all(|name| !name.starts_with(".git/")),
            "repository internals are never scanned: {names:?}"
        );
    }

    #[test]
    fn recency_is_bounded_by_count_size_and_churn() {
        // The one partition a large tree can flood, so all three bounds are
        // load-bearing. Without the directory exclusions especially, build
        // output wins every recency contest and the partition never reaches a
        // source file.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..(RECENT_LIMIT + 20) {
            touch(&root.join(format!("src{i}.rs")), "fn main() {}");
        }
        std::fs::create_dir_all(root.join("target")).unwrap();
        touch(&root.join("target/artifact.o"), "built");
        touch(&root.join("huge.bin"), "x");
        std::fs::write(
            root.join("huge.bin"),
            vec![0u8; (RECENT_MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();

        let ignore = load_ignore(root);
        let picked = recent_files(root, &ignore, /*include_hidden*/ false);

        assert_eq!(picked.len(), RECENT_LIMIT, "count is capped");
        assert!(
            !picked.iter().any(|p| p.ends_with("artifact.o")),
            "churn directories are never descended into"
        );
        assert!(
            !picked.iter().any(|p| p.ends_with("huge.bin")),
            "oversized files are left out however recent"
        );
    }

    #[test]
    fn a_directory_without_a_repository_contributes_no_git_partition() {
        // Not an error, just an empty partition — the other two carry it.
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.txt"), "alpha");
        let ignore = load_ignore(dir.path());
        assert!(git_tracked_files(dir.path(), &ignore).is_empty());
    }

    #[test]
    fn ignored_paths_are_protected_symmetrically() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join(SNAPSHOT_IGNORE_FILENAME), "secret/**\n");
        let ignore = load_ignore(dir.path());
        assert!(is_ignored(&ignore, &dir.path().join("secret/key.pem")));
        assert!(!is_ignored(&ignore, &dir.path().join("src/main.rs")));
    }
}
