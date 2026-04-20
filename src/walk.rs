use crate::lang::Lang;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub lang: Lang,
}

pub fn walk_files(root: &Path) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true) // skip hidden files/dirs
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for result in walker {
        let dir_entry = result.with_context(|| format!("walking {}", root.display()))?;
        if !dir_entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = dir_entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let lang = match Lang::from_extension(ext) {
            Some(l) => l,
            None => continue,
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
    Ok(entries)
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
        let rs = entries.iter().find(|e| e.path.ends_with("main.rs")).unwrap();
        assert_eq!(rs.lang.name(), "rust");
        let py = entries.iter().find(|e| e.path.ends_with("lib.py")).unwrap();
        assert_eq!(py.lang.name(), "python");
        let tsx = entries.iter().find(|e| e.path.ends_with("app.tsx")).unwrap();
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
    fn changed_since_filters_by_mtime() {
        let dir = setup_temp_tree();
        let entries = walk_files(dir.path()).unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let changed = changed_since(&entries, future);
        assert!(changed.is_empty(), "no files should be newer than future");
        let past = SystemTime::UNIX_EPOCH;
        let changed = changed_since(&entries, past);
        assert_eq!(changed.len(), entries.len(), "all files should be newer than epoch");
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
}
