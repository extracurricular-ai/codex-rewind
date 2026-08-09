//! Tracking scope: which files a checkpoint observes (RFC §6.2).
//!
//! Two modes:
//! - **Workspace mode**: a project root was found by marker walk-up; the
//!   tracked set is the workspace subtree filtered by the dedicated
//!   snapshot ignore file.
//! - **Fallback mode**: no marker; tracking is seeded from the invocation
//!   directory (all files if few, else the most recently modified) and
//!   grows via track-on-edit, bounded by a hard cap.
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
use crate::manifest::mtime_parts;

/// Name of the dedicated snapshot ignore file (provisional; the final
/// name is an open question in the RFC).
pub const SNAPSHOT_IGNORE_FILENAME: &str = ".codexsnapignore";

/// Seed guardrails for fallback mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedPolicy {
    /// If the directory holds at most this many eligible files, track all.
    pub full_limit: usize,
    /// Otherwise seed with this many most-recently-modified files.
    pub recent_seed: usize,
    /// Hard cap on the tracked set (seed + track-on-edit growth).
    pub cap: usize,
}

impl Default for SeedPolicy {
    fn default() -> Self {
        Self {
            full_limit: 50,
            recent_seed: 30,
            cap: 100,
        }
    }
}

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

/// Fallback-mode seed: all eligible files if at most `policy.full_limit`,
/// otherwise the `policy.recent_seed` most recently modified.
pub fn seed_fallback_files(
    dir: &Path,
    policy: &SeedPolicy,
    include_hidden: bool,
) -> Result<Vec<PathBuf>> {
    let all = workspace_files(dir, include_hidden)?;
    if all.len() <= policy.full_limit {
        return Ok(all);
    }
    let mut with_mtime: Vec<(PathBuf, (i64, u32))> = all
        .into_iter()
        .filter_map(|p| {
            let meta = fs::symlink_metadata(&p).ok()?;
            Some((p, mtime_parts(&meta)))
        })
        .collect();
    with_mtime.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    with_mtime.truncate(policy.recent_seed);
    let mut seed: Vec<PathBuf> = with_mtime.into_iter().map(|(p, _)| p).collect();
    seed.sort();
    Ok(seed)
}

/// The bounded tracked set: seed + track-on-edit growth, capped.
#[derive(Debug, Clone)]
pub struct TrackedSet {
    files: BTreeSet<PathBuf>,
    cap: usize,
}

impl TrackedSet {
    pub fn new(seed: impl IntoIterator<Item = PathBuf>, cap: usize) -> Self {
        let mut files: BTreeSet<PathBuf> = seed.into_iter().collect();
        while files.len() > cap {
            // Deterministic trim; callers should seed within cap.
            if let Some(last) = files.iter().next_back().cloned() {
                files.remove(&last);
            }
        }
        Self { files, cap }
    }

    /// Track a file the agent is about to edit. Returns `false` when the
    /// cap is reached and the path was not already tracked.
    // TODO(eviction): RFC open question 1 — evict least-recently-changed
    // instead of refusing, never evicting agent-edited files.
    pub fn track_edit(&mut self, path: PathBuf) -> bool {
        if self.files.contains(&path) {
            return true;
        }
        if self.files.len() >= self.cap {
            return false;
        }
        self.files.insert(path);
        true
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.files.contains(path)
    }

    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs::File;
    use std::time::Duration;
    use std::time::SystemTime;

    fn touch(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        let f = File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
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
    fn seed_takes_all_when_small() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            touch(&dir.path().join(format!("f{i}.txt")), "x");
        }
        let seed = seed_fallback_files(
            dir.path(),
            &SeedPolicy::default(),
            /*include_hidden*/ false,
        )
        .unwrap();
        assert_eq!(seed.len(), 5);
    }

    #[test]
    fn seed_takes_most_recent_when_large() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // 60 files with strictly increasing mtimes; the newest 30 win.
        for i in 0..60 {
            let p = dir.path().join(format!("f{i:02}.txt"));
            touch(&p, "x");
            set_mtime(&p, base + Duration::from_secs(i));
        }
        let policy = SeedPolicy::default();
        let seed = seed_fallback_files(dir.path(), &policy, /*include_hidden*/ false).unwrap();
        assert_eq!(seed.len(), policy.recent_seed);
        assert!(seed.contains(&dir.path().join("f59.txt")));
        assert!(seed.contains(&dir.path().join("f30.txt")));
        assert!(!seed.contains(&dir.path().join("f29.txt")));
    }

    #[test]
    fn tracked_set_caps_growth() {
        let mut set = TrackedSet::new(vec![PathBuf::from("/a")], 2);
        assert!(set.track_edit(PathBuf::from("/b")));
        assert!(!set.track_edit(PathBuf::from("/c")), "cap reached");
        assert!(set.track_edit(PathBuf::from("/a")), "already tracked is ok");
        assert_eq!(set.len(), 2);
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
