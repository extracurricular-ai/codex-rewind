//! Compare the tracking partitions against a plain subtree walk.
//!
//! Reproduces the figures quoted in the RFC (§6.2). The claim that tracking is
//! bounded by the project rather than by the tree is the load-bearing one in
//! that section, and it is worth being checkable on someone else's repository
//! rather than only on ours:
//!
//! ```text
//! cargo run -p codex-file-snapshots --example scan_bench -- /path/to/repo
//! ```
fn main() {
    for root in std::env::args().skip(1) {
        let p = std::path::Path::new(&root);
        let ignore = codex_file_snapshots::load_ignore(p);

        let report = |label: &str, files: &[std::path::PathBuf], elapsed: std::time::Duration| {
            let bytes: u64 = files
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum();
            println!(
                "  {label:<16} {:>7} files {:>9.1} MB  {elapsed:?}",
                files.len(),
                bytes as f64 / 1_048_576.0
            );
        };

        println!("{root}");
        let t = std::time::Instant::now();
        let all = subtree_walk(p);
        report("subtree walk", &all, t.elapsed());

        let t = std::time::Instant::now();
        let git = codex_file_snapshots::git_tracked_files(p, &ignore);
        report("git-tracked", &git, t.elapsed());

        // The residue partition is told what git already covers, so its budget
        // buys only paths nothing else contributed. Run it both ways to show
        // what the exclusion is worth on this tree.
        let covered: std::collections::BTreeSet<_> = git.iter().cloned().collect();
        let t = std::time::Instant::now();
        let recent = codex_file_snapshots::recent_files(p, &ignore, false, &covered);
        report("recent (residue)", &recent, t.elapsed());

        let blind = codex_file_snapshots::recent_files(
            p,
            &ignore,
            false,
            &std::collections::BTreeSet::new(),
        );
        let wasted = blind.iter().filter(|f| covered.contains(*f)).count();
        println!(
            "  {:<16} {wasted:>7} of {} slots would go to files git already covers",
            "without it:",
            blind.len()
        );
    }
}

/// The naive walk this crate deliberately no longer performs, kept here so the
/// comparison has something to compare against.
fn subtree_walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}
