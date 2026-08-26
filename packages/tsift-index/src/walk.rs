use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tsift_graph::lang::Lang;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub lang: Lang,
}

/// Files the walk saw and dropped because no indexer language claims their
/// extension (`#goindex`).
///
/// Skips used to be invisible: a Go module with 26 tracked files indexed 8 of
/// them and `status` still called the scope `fresh`, so the failure surfaced
/// only as confident empty search results. Counting them here is what lets
/// `index` and `status` say a parseable language was left out.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkipStats {
    pub files: usize,
    pub by_extension: BTreeMap<String, usize>,
}

impl SkipStats {
    fn record(&mut self, extension: Option<&str>) {
        self.files += 1;
        let key = extension
            .map(|ext| format!(".{ext}"))
            .unwrap_or_else(|| "(no extension)".to_string());
        *self.by_extension.entry(key).or_insert(0) += 1;
    }

    /// Extensions ordered by how many files they cost, most first.
    pub fn ranked_extensions(&self) -> Vec<(&str, usize)> {
        let mut ranked = self
            .by_extension
            .iter()
            .map(|(ext, count)| (ext.as_str(), *count))
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        ranked
    }

    /// The single extension that cost the most files, if any were skipped.
    pub fn dominant_extension(&self) -> Option<(&str, usize)> {
        self.ranked_extensions().into_iter().next()
    }
}

#[derive(Debug)]
pub struct WalkResult {
    pub entries: Vec<FileEntry>,
    pub dir_mtimes: HashMap<PathBuf, SystemTime>,
    pub pruned_dirs: HashSet<PathBuf>,
    pub stats: PruneStats,
    pub skips: SkipStats,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PruneStats {
    pub dirs_walked: usize,
    pub dirs_pruned: usize,
    pub files_pruned: usize,
}

pub fn walk_files(root: &Path) -> Result<Vec<FileEntry>> {
    walk_files_with_skips(root).map(|(entries, _skips)| entries)
}

/// `walk_files`, plus the count of files dropped for want of an indexer
/// language (`#goindex`).
pub fn walk_files_with_skips(root: &Path) -> Result<(Vec<FileEntry>, SkipStats)> {
    walk_files_with_skips_excluding(root, &[])
}

/// Walk indexable files while pruning whole source roots owned by other
/// workspace scopes. The exclusions are directory roots, not glob patterns,
/// so a workspace root index can cover only the meta-repository's own files.
pub fn walk_files_with_skips_excluding(
    root: &Path,
    excluded_roots: &[PathBuf],
) -> Result<(Vec<FileEntry>, SkipStats)> {
    let mut entries = Vec::new();
    let mut skips = SkipStats::default();
    let walker = build_walker(root, excluded_roots);
    for result in walker {
        let dir_entry = result.with_context(|| format!("walking {}", root.display()))?;
        if !dir_entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = dir_entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        let lang = match ext.and_then(Lang::from_extension) {
            Some(l) => l,
            None => {
                skips.record(ext);
                continue;
            }
        };
        let metadata = dir_entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        let mtime = metadata
            .modified()
            .with_context(|| format!("mtime {}", path.display()))?;
        entries.push(FileEntry {
            path: path.to_path_buf(),
            mtime,
            lang,
        });
    }
    Ok((entries, skips))
}

pub fn walk_files_pruned(
    root: &Path,
    stored_dirs: HashMap<PathBuf, SystemTime>,
) -> Result<WalkResult> {
    walk_files_pruned_excluding(root, stored_dirs, &[])
}

pub fn walk_files_pruned_excluding(
    root: &Path,
    _stored_dirs: HashMap<PathBuf, SystemTime>,
    excluded_roots: &[PathBuf],
) -> Result<WalkResult> {
    let mut entries = Vec::new();
    let mut skips = SkipStats::default();
    let mut dir_mtimes = HashMap::new();
    let pruned_dirs = HashSet::new();
    let dirs_pruned = 0usize;
    let mut dirs_walked = 0usize;
    let files_pruned = 0usize;

    let walker = build_walker(root, excluded_roots);

    for result in walker {
        let dir_entry = result.with_context(|| format!("walking {}", root.display()))?;
        let path = dir_entry.path();

        if dir_entry.file_type().is_some_and(|ft| ft.is_dir()) {
            let metadata = dir_entry
                .metadata()
                .with_context(|| format!("stat dir {}", path.display()))?;
            let mtime = metadata
                .modified()
                .with_context(|| format!("mtime dir {}", path.display()))?;
            dir_mtimes.insert(path.to_path_buf(), mtime);
            dirs_walked += 1;
            continue;
        }

        if !dir_entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str());
        let lang = match ext.and_then(Lang::from_extension) {
            Some(l) => l,
            None => {
                skips.record(ext);
                continue;
            }
        };
        let metadata = dir_entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        let mtime = metadata
            .modified()
            .with_context(|| format!("mtime {}", path.display()))?;
        entries.push(FileEntry {
            path: path.to_path_buf(),
            mtime,
            lang,
        });
    }

    Ok(WalkResult {
        entries,
        dir_mtimes,
        pruned_dirs,
        stats: PruneStats {
            dirs_walked,
            dirs_pruned,
            files_pruned,
        },
        skips,
    })
}

fn build_walker(root: &Path, excluded_roots: &[PathBuf]) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    if !excluded_roots.is_empty() {
        let excluded_roots = excluded_roots.to_vec();
        builder.filter_entry(move |entry| {
            !excluded_roots
                .iter()
                .any(|excluded| entry.path() == excluded || entry.path().starts_with(excluded))
        });
    }
    builder.build()
}

pub fn changed_since(entries: &[FileEntry], since: SystemTime) -> Vec<&FileEntry> {
    entries.iter().filter(|e| e.mtime > since).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_temp_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("lib.py"), "def hello(): pass").unwrap();
        fs::write(root.join("app.tsx"), "export default () => <div/>").unwrap();
        fs::write(root.join("notes.txt"), "not a code file").unwrap();
        fs::write(root.join("data.json"), "{}").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/mod.rs"), "pub mod inner;").unwrap();
        dir
    }

    #[test]
    fn walk_finds_supported_files() {
        let dir = setup_temp_tree();
        let entries = walk_files(dir.path()).unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"main.rs".to_string()));
        assert!(names.contains(&"lib.py".to_string()));
        assert!(names.contains(&"app.tsx".to_string()));
        assert!(names.contains(&"mod.rs".to_string()));
        assert!(!names.contains(&"notes.txt".to_string()));
        assert!(!names.contains(&"data.json".to_string()));
    }

    #[test]
    fn walk_assigns_correct_language() {
        let dir = setup_temp_tree();
        let entries = walk_files(dir.path()).unwrap();
        let rs = entries
            .iter()
            .find(|e| e.path.ends_with("main.rs"))
            .unwrap();
        assert_eq!(rs.lang.name(), "rust");
        let py = entries.iter().find(|e| e.path.ends_with("lib.py")).unwrap();
        assert_eq!(py.lang.name(), "python");
        let tsx = entries
            .iter()
            .find(|e| e.path.ends_with("app.tsx"))
            .unwrap();
        assert_eq!(tsx.lang.name(), "tsx");
    }

    #[test]
    fn walk_captures_mtime() {
        let dir = setup_temp_tree();
        let entries = walk_files(dir.path()).unwrap();
        for entry in &entries {
            assert!(
                entry.mtime.elapsed().unwrap().as_secs() < 10,
                "mtime should be recent for {}",
                entry.path.display()
            );
        }
    }

    #[test]
    fn walk_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // ignore crate needs a git repo for .gitignore to take effect
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.generated.rs\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/debug.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("output.generated.rs"), "fn gen() {}").unwrap();
        fs::write(root.join("src.rs"), "fn keep() {}").unwrap();
        let entries = walk_files(root).unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"src.rs".to_string()));
        assert!(!names.contains(&"debug.rs".to_string()));
        assert!(!names.contains(&"output.generated.rs".to_string()));
    }

    #[test]
    fn walk_exclusions_prune_entire_workspace_scopes() {
        let dir = setup_temp_tree();
        let root = dir.path();
        let excluded = root.join("sub");

        let (entries, _) = walk_files_with_skips_excluding(root, &[excluded]).unwrap();
        assert!(
            entries
                .iter()
                .all(|entry| !entry.path.ends_with("sub/mod.rs"))
        );
        assert!(entries.iter().any(|entry| entry.path.ends_with("main.rs")));
    }

    #[test]
    fn changed_since_filters_by_mtime() {
        let dir = setup_temp_tree();
        let entries = walk_files(dir.path()).unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let changed = changed_since(&entries, future);
        assert!(changed.is_empty(), "no files should be newer than future");
        let past = SystemTime::UNIX_EPOCH;
        let changed = changed_since(&entries, past);
        assert_eq!(
            changed.len(),
            entries.len(),
            "all files should be newer than epoch"
        );
    }

    #[test]
    fn walk_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let entries = walk_files(dir.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn walk_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden/secret.rs"), "fn secret() {}").unwrap();
        fs::write(root.join("visible.rs"), "fn visible() {}").unwrap();
        let entries = walk_files(root).unwrap();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"visible.rs".to_string()));
        assert!(!names.contains(&"secret.rs".to_string()));
    }

    #[test]
    fn pruned_walk_no_stored_dirs_walks_everything() {
        let dir = setup_temp_tree();
        let result = walk_files_pruned(dir.path(), HashMap::new()).unwrap();
        assert_eq!(result.entries.len(), 4); // main.rs, lib.py, app.tsx, sub/mod.rs
        assert!(result.pruned_dirs.is_empty());
        assert_eq!(result.stats.dirs_pruned, 0);
    }

    #[test]
    fn pruned_walk_still_visits_files_when_dir_mtime_matches() {
        let dir = setup_temp_tree();
        let root = dir.path();
        let sub_path = root.join("sub");
        let sub_mtime = fs::metadata(&sub_path).unwrap().modified().unwrap();
        let mut stored = HashMap::new();
        stored.insert(sub_path.clone(), sub_mtime);

        let result = walk_files_pruned(root, stored).unwrap();
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"mod.rs".to_string()),
            "sub/mod.rs should still be visited for correctness"
        );
        assert!(names.contains(&"main.rs".to_string()));
        assert_eq!(result.stats.dirs_pruned, 0);
        assert!(!result.pruned_dirs.contains(&sub_path));
    }

    #[test]
    fn pruned_walk_walks_changed_subdir() {
        let dir = setup_temp_tree();
        let root = dir.path();
        let sub_path = root.join("sub");
        let old_mtime = SystemTime::UNIX_EPOCH;
        let mut stored = HashMap::new();
        stored.insert(sub_path, old_mtime);

        let result = walk_files_pruned(root, stored).unwrap();
        let names: Vec<String> = result
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"mod.rs".to_string()),
            "sub/mod.rs should be walked"
        );
        assert_eq!(result.stats.dirs_pruned, 0);
    }

    #[test]
    fn pruned_walk_collects_dir_mtimes() {
        let dir = setup_temp_tree();
        let result = walk_files_pruned(dir.path(), HashMap::new()).unwrap();
        assert!(
            !result.dir_mtimes.is_empty(),
            "should collect mtimes for walked directories"
        );
    }
}
