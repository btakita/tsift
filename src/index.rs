use crate::lang::Lang;
use crate::walk::{self, FileEntry};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct IndexDb {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    New,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexSummary {
    pub total_tracked: usize,
    pub new: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredSymbol {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub signature: Option<String>,
    pub file: String,
    pub line: i64,
    pub end_line: Option<i64>,
    pub parent_module: Option<String>,
    pub visibility: Option<String>,
}

fn system_time_to_pair(t: SystemTime) -> (i64, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    (d.as_secs() as i64, d.subsec_nanos())
}

fn pair_to_system_time(secs: i64, nanos: u32) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs as u64, nanos)
}

impl IndexDb {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating index dir: {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening index db: {}", db_path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_state (
                path TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                mtime_nanos INTEGER NOT NULL,
                language TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                language TEXT NOT NULL,
                signature TEXT,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                end_line INTEGER,
                parent_module TEXT,
                visibility TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_language ON symbols(language);
            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);"
        )?;
        Ok(Self { conn })
    }

    pub fn compute_changes(&self, root: &Path) -> Result<IndexSummary> {
        let entries = walk::walk_files(root)?;
        let disk_files: HashMap<PathBuf, &FileEntry> = entries
            .iter()
            .map(|e| (e.path.clone(), e))
            .collect();

        let mut stored: HashMap<PathBuf, (i64, u32, String)> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT path, mtime_secs, mtime_nanos, language FROM file_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, i64>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (path, secs, nanos, lang) = row?;
            stored.insert(path, (secs, nanos, lang));
        }

        let mut changes = Vec::new();
        let mut unchanged = 0usize;

        for entry in &entries {
            match stored.get(&entry.path) {
                Some((secs, nanos, _lang)) => {
                    let stored_mtime = pair_to_system_time(*secs, *nanos);
                    if entry.mtime != stored_mtime {
                        changes.push(FileChange {
                            path: entry.path.clone(),
                            kind: ChangeKind::Modified,
                            language: Some(entry.lang.name().to_string()),
                        });
                    } else {
                        unchanged += 1;
                    }
                }
                None => {
                    changes.push(FileChange {
                        path: entry.path.clone(),
                        kind: ChangeKind::New,
                        language: Some(entry.lang.name().to_string()),
                    });
                }
            }
        }

        for stored_path in stored.keys() {
            if !disk_files.contains_key(stored_path) {
                changes.push(FileChange {
                    path: stored_path.clone(),
                    kind: ChangeKind::Deleted,
                    language: None,
                });
            }
        }

        let new_count = changes.iter().filter(|c| c.kind == ChangeKind::New).count();
        let mod_count = changes.iter().filter(|c| c.kind == ChangeKind::Modified).count();
        let del_count = changes.iter().filter(|c| c.kind == ChangeKind::Deleted).count();

        Ok(IndexSummary {
            total_tracked: entries.len(),
            new: new_count,
            modified: mod_count,
            deleted: del_count,
            unchanged,
            changes,
        })
    }

    pub fn apply_changes(&self, root: &Path) -> Result<IndexSummary> {
        let summary = self.compute_changes(root)?;

        let mut insert_file = self.conn.prepare(
            "INSERT OR REPLACE INTO file_state (path, mtime_secs, mtime_nanos, language) VALUES (?1, ?2, ?3, ?4)"
        )?;
        let mut delete_file = self.conn.prepare("DELETE FROM file_state WHERE path = ?1")?;
        let mut delete_symbols = self.conn.prepare("DELETE FROM symbols WHERE file = ?1")?;
        let mut insert_symbol = self.conn.prepare(
            "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        )?;

        for change in &summary.changes {
            let path_str = change.path.to_string_lossy();
            match change.kind {
                ChangeKind::New | ChangeKind::Modified => {
                    let metadata = std::fs::metadata(&change.path)
                        .with_context(|| format!("stat {}", change.path.display()))?;
                    let mtime = metadata.modified()?;
                    let (secs, nanos) = system_time_to_pair(mtime);
                    insert_file.execute(rusqlite::params![
                        &path_str,
                        secs,
                        nanos,
                        change.language.as_deref().unwrap_or("unknown"),
                    ])?;

                    delete_symbols.execute(rusqlite::params![&path_str])?;
                    let lang = change.path.extension()
                        .and_then(|e| e.to_str())
                        .and_then(Lang::from_extension);
                    if let Some(lang) = lang {
                        let source = std::fs::read(&change.path).ok();
                        let symbols = source.as_ref().and_then(|s| lang.extract_symbols(s).ok());
                        if let Some(symbols) = symbols {
                            let lang_name = lang.name();
                            for sym in &symbols {
                                insert_symbol.execute(rusqlite::params![
                                    sym.name,
                                    sym.kind,
                                    lang_name,
                                    Option::<String>::None,
                                    &path_str,
                                    sym.line as i64,
                                    sym.end_line as i64,
                                    Option::<String>::None,
                                    Option::<String>::None,
                                ])?;
                            }
                        }
                    }
                }
                ChangeKind::Deleted => {
                    delete_file.execute(rusqlite::params![&path_str])?;
                    delete_symbols.execute(rusqlite::params![&path_str])?;
                }
            }
        }
        Ok(summary)
    }

    pub fn rebuild(&self, root: &Path) -> Result<IndexSummary> {
        self.conn.execute("DELETE FROM file_state", [])?;
        self.conn.execute("DELETE FROM symbols", [])?;
        self.apply_changes(root)
    }

    pub fn file_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM file_state", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn symbols_for_file(&self, file: &str) -> Result<Vec<StoredSymbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, language, signature, file, line, end_line, parent_module, visibility FROM symbols WHERE file = ?1 ORDER BY line"
        )?;
        let rows = stmt.query_map(rusqlite::params![file], |row| {
            Ok(StoredSymbol {
                name: row.get(0)?,
                kind: row.get(1)?,
                language: row.get(2)?,
                signature: row.get(3)?,
                file: row.get(4)?,
                line: row.get(5)?,
                end_line: row.get(6)?,
                parent_module: row.get(7)?,
                visibility: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("lib.py"), "def hello(): pass").unwrap();
        fs::write(root.join("app.tsx"), "export default () => <div/>").unwrap();
        fs::write(root.join("readme.txt"), "not code").unwrap();
        dir
    }

    fn db_in(dir: &Path) -> IndexDb {
        IndexDb::open(&dir.join(".tsift/index.db")).unwrap()
    }

    #[test]
    fn first_index_all_new() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        let summary = db.apply_changes(dir.path()).unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.unchanged, 0);
        assert_eq!(summary.total_tracked, 3);
    }

    #[test]
    fn second_index_all_unchanged() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.new, 0);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.unchanged, 3);
    }

    #[test]
    fn modified_file_detected() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(dir.path().join("main.rs"), "fn main() { println!(\"hi\"); }").unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.unchanged, 2);
        let modified = summary.changes.iter().find(|c| c.kind == ChangeKind::Modified).unwrap();
        assert!(modified.path.ends_with("main.rs"));
    }

    #[test]
    fn new_file_detected() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        fs::write(dir.path().join("extra.rs"), "fn extra() {}").unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.new, 1);
        assert_eq!(summary.unchanged, 3);
        let new = summary.changes.iter().find(|c| c.kind == ChangeKind::New).unwrap();
        assert!(new.path.ends_with("extra.rs"));
    }

    #[test]
    fn deleted_file_detected() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        fs::remove_file(dir.path().join("lib.py")).unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.unchanged, 2);
        let deleted = summary.changes.iter().find(|c| c.kind == ChangeKind::Deleted).unwrap();
        assert!(deleted.path.ends_with("lib.py"));
    }

    #[test]
    fn rebuild_resets_state() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);
        let summary = db.rebuild(dir.path()).unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(db.file_count().unwrap(), 3);
    }

    #[test]
    fn unsupported_extensions_ignored() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        let summary = db.apply_changes(dir.path()).unwrap();
        let paths: Vec<String> = summary.changes.iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(!paths.contains(&"readme.txt".to_string()));
    }

    #[test]
    fn check_mode_does_not_update_state() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(db.file_count().unwrap(), 0);
    }

    #[test]
    fn apply_then_delete_then_apply() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);
        fs::remove_file(dir.path().join("app.tsx")).unwrap();
        let summary = db.apply_changes(dir.path()).unwrap();
        assert_eq!(summary.deleted, 1);
        assert_eq!(db.file_count().unwrap(), 2);
    }

    #[test]
    fn symbols_extracted_on_index() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        assert!(db.symbol_count().unwrap() > 0, "expected symbols from indexed files");
    }

    #[test]
    fn symbols_have_correct_properties() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let main_rs = dir.path().join("main.rs").to_string_lossy().to_string();
        let symbols = db.symbols_for_file(&main_rs).unwrap();
        assert!(!symbols.is_empty(), "expected symbols in main.rs");

        let main_sym = symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, "function");
        assert_eq!(main_sym.language, "rust");
        assert_eq!(main_sym.line, 0);
    }

    #[test]
    fn symbols_updated_on_modification() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let initial = db.symbol_count().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(dir.path().join("main.rs"), "fn main() {}\nfn extra() {}\nfn another() {}").unwrap();
        db.apply_changes(dir.path()).unwrap();
        assert!(db.symbol_count().unwrap() > initial, "expected more symbols after adding functions");
    }

    #[test]
    fn symbols_deleted_with_file() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let initial = db.symbol_count().unwrap();
        assert!(initial > 0);

        fs::remove_file(dir.path().join("main.rs")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        assert!(db.symbol_count().unwrap() < initial, "expected fewer symbols after deleting file");
    }

    #[test]
    fn rebuild_clears_and_reextracts_symbols() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let count = db.symbol_count().unwrap();
        assert!(count > 0);

        db.rebuild(dir.path()).unwrap();
        assert_eq!(db.symbol_count().unwrap(), count, "rebuild should produce same symbol count");
    }

    #[test]
    fn symbols_for_nonexistent_file_returns_empty() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let symbols = db.symbols_for_file("/no/such/file.rs").unwrap();
        assert!(symbols.is_empty());
    }
}
