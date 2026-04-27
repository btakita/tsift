use crate::graph;
use crate::lang::Lang;
use crate::walk::{self, FileEntry, PruneStats};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tagpath::parser as tagpath_parser;

pub struct IndexDb {
    conn: Connection,
    _write_lock: Option<WriteLockGuard>,
}

struct WriteLockGuard {
    path: PathBuf,
}

impl Drop for WriteLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prune_stats: Option<PruneStats>,
}

impl IndexSummary {
    pub fn has_changes(&self) -> bool {
        self.new > 0 || self.modified > 0 || self.deleted > 0
    }
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
    pub tags: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredEdge {
    pub caller_file: String,
    pub caller_name: String,
    pub caller_line: i64,
    pub callee_name: String,
    pub call_site_line: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolHit {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file: String,
    pub line: i64,
    pub end_line: Option<i64>,
    pub tags: Option<String>,
    pub score: f64,
    pub match_type: String,
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
        let write_lock = acquire_write_lock(db_path)?;
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening index db: {}", db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        if mode.to_lowercase() != "wal" {
            bail!(
                "index db {} requires WAL mode for concurrent reads, got {}",
                db_path.display(),
                mode
            );
        }
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
                visibility TEXT,
                tags TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_language ON symbols(language);
            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
            CREATE TABLE IF NOT EXISTS call_edges (
                id INTEGER PRIMARY KEY,
                caller_file TEXT NOT NULL,
                caller_name TEXT NOT NULL,
                caller_line INTEGER NOT NULL,
                callee_name TEXT NOT NULL,
                call_site_line INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_call_edges_caller ON call_edges(caller_name);
            CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_name);
            CREATE INDEX IF NOT EXISTS idx_call_edges_file ON call_edges(caller_file);
            CREATE TABLE IF NOT EXISTS dir_state (
                path TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                mtime_nanos INTEGER NOT NULL
            );",
        )?;
        let _ = conn.execute("ALTER TABLE symbols ADD COLUMN tags TEXT", []);
        Ok(Self {
            conn,
            _write_lock: Some(write_lock),
        })
    }

    pub fn open_read_only(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening index db: {}", db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            _write_lock: None,
        })
    }

    fn load_dir_state(&self) -> Result<HashMap<PathBuf, SystemTime>> {
        let mut dirs = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos FROM dir_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, i64>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })?;
        for row in rows {
            let (path, secs, nanos) = row?;
            dirs.insert(path, pair_to_system_time(secs, nanos));
        }
        Ok(dirs)
    }

    fn save_dir_state(&self, dir_mtimes: &HashMap<PathBuf, SystemTime>) -> Result<()> {
        self.conn.execute("DELETE FROM dir_state", [])?;
        let mut stmt = self
            .conn
            .prepare("INSERT INTO dir_state (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)")?;
        for (path, mtime) in dir_mtimes {
            let (secs, nanos) = system_time_to_pair(*mtime);
            stmt.execute(rusqlite::params![path.to_string_lossy(), secs, nanos])?;
        }
        Ok(())
    }

    fn load_stored_files(&self) -> Result<HashMap<PathBuf, (i64, u32, String)>> {
        let mut stored = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime_secs, mtime_nanos, language FROM file_state")?;
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
        Ok(stored)
    }

    fn diff_entries(
        entries: &[FileEntry],
        stored: &HashMap<PathBuf, (i64, u32, String)>,
        pruned_dirs: &HashSet<PathBuf>,
    ) -> (Vec<FileChange>, usize) {
        let disk_files: HashSet<&PathBuf> = entries.iter().map(|e| &e.path).collect();
        let mut changes = Vec::new();
        let mut unchanged = 0usize;

        for entry in entries {
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
            if disk_files.contains(stored_path) {
                continue;
            }
            let in_pruned = pruned_dirs.iter().any(|d| stored_path.starts_with(d));
            if in_pruned {
                unchanged += 1;
            } else {
                changes.push(FileChange {
                    path: stored_path.clone(),
                    kind: ChangeKind::Deleted,
                    language: None,
                });
            }
        }

        (changes, unchanged)
    }

    pub fn compute_changes(&self, root: &Path) -> Result<IndexSummary> {
        self.compute_changes_inner(root, false)
    }

    pub fn compute_changes_pruned(&self, root: &Path) -> Result<IndexSummary> {
        self.compute_changes_inner(root, true)
    }

    fn compute_changes_inner(&self, root: &Path, prune: bool) -> Result<IndexSummary> {
        let stored = self.load_stored_files()?;

        let (entries, pruned_dirs, prune_stats) = if prune {
            let stored_dirs = self.load_dir_state().unwrap_or_default();
            let walk_result = walk::walk_files_pruned(root, stored_dirs)?;
            let mut stats = walk_result.stats;
            let pruned_file_count = stored
                .keys()
                .filter(|p| walk_result.pruned_dirs.iter().any(|d| p.starts_with(d)))
                .count();
            stats.files_pruned = pruned_file_count;
            (walk_result.entries, walk_result.pruned_dirs, Some(stats))
        } else {
            let entries = walk::walk_files(root)?;
            (entries, HashSet::new(), None)
        };

        let (changes, unchanged) = Self::diff_entries(&entries, &stored, &pruned_dirs);

        let new_count = changes.iter().filter(|c| c.kind == ChangeKind::New).count();
        let mod_count = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Modified)
            .count();
        let del_count = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Deleted)
            .count();

        Ok(IndexSummary {
            total_tracked: entries.len() + prune_stats.as_ref().map_or(0, |s| s.files_pruned),
            new: new_count,
            modified: mod_count,
            deleted: del_count,
            unchanged,
            changes,
            prune_stats,
        })
    }

    pub fn apply_changes(&self, root: &Path) -> Result<IndexSummary> {
        self.apply_changes_inner(root, false)
    }

    pub fn apply_changes_pruned(&self, root: &Path) -> Result<IndexSummary> {
        self.apply_changes_inner(root, true)
    }

    fn apply_changes_inner(&self, root: &Path, prune: bool) -> Result<IndexSummary> {
        let stored = self.load_stored_files()?;

        let (entries, pruned_dirs, dir_mtimes, prune_stats) = if prune {
            let stored_dirs = self.load_dir_state().unwrap_or_default();
            let walk_result = walk::walk_files_pruned(root, stored_dirs)?;
            let mut stats = walk_result.stats;
            let pruned_file_count = stored
                .keys()
                .filter(|p| walk_result.pruned_dirs.iter().any(|d| p.starts_with(d)))
                .count();
            stats.files_pruned = pruned_file_count;
            (
                walk_result.entries,
                walk_result.pruned_dirs,
                Some(walk_result.dir_mtimes),
                Some(stats),
            )
        } else {
            let entries = walk::walk_files(root)?;
            (entries, HashSet::new(), None, None)
        };

        let (changes, unchanged) = Self::diff_entries(&entries, &stored, &pruned_dirs);

        let new_count = changes.iter().filter(|c| c.kind == ChangeKind::New).count();
        let mod_count = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Modified)
            .count();
        let del_count = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Deleted)
            .count();

        let summary = IndexSummary {
            total_tracked: entries.len() + prune_stats.as_ref().map_or(0, |s| s.files_pruned),
            new: new_count,
            modified: mod_count,
            deleted: del_count,
            unchanged,
            changes,
            prune_stats,
        };

        let mut insert_file = self.conn.prepare(
            "INSERT OR REPLACE INTO file_state (path, mtime_secs, mtime_nanos, language) VALUES (?1, ?2, ?3, ?4)"
        )?;
        let mut delete_file = self
            .conn
            .prepare("DELETE FROM file_state WHERE path = ?1")?;
        let mut delete_symbols = self.conn.prepare("DELETE FROM symbols WHERE file = ?1")?;
        let mut insert_symbol = self.conn.prepare(
            "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility, tags) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        )?;
        let mut delete_edges = self
            .conn
            .prepare("DELETE FROM call_edges WHERE caller_file = ?1")?;
        let mut insert_edge = self.conn.prepare(
            "INSERT INTO call_edges (caller_file, caller_name, caller_line, callee_name, call_site_line) VALUES (?1, ?2, ?3, ?4, ?5)"
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
                    delete_edges.execute(rusqlite::params![&path_str])?;
                    let lang = change
                        .path
                        .extension()
                        .and_then(|e| e.to_str())
                        .and_then(Lang::from_extension);
                    if let Some(lang) = lang {
                        let source = std::fs::read(&change.path).ok();
                        let symbols = source.as_ref().and_then(|s| lang.extract_symbols(s).ok());
                        if let Some(ref symbols) = symbols {
                            let lang_name = lang.name();
                            for sym in symbols {
                                let tags = compute_tags(&sym.name);
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
                                    tags,
                                ])?;
                            }
                        }
                        if let Some(ref source) = source {
                            let call_sites = graph::extract_call_sites(lang, source).ok();
                            if let (Some(sites), Some(symbols)) = (call_sites, &symbols) {
                                let edges = graph::resolve_edges(symbols, &sites);
                                for edge in &edges {
                                    insert_edge.execute(rusqlite::params![
                                        &path_str,
                                        edge.caller,
                                        edge.caller_line as i64,
                                        edge.callee,
                                        edge.call_site_line as i64,
                                    ])?;
                                }
                            }
                        }
                    }
                }
                ChangeKind::Deleted => {
                    delete_file.execute(rusqlite::params![&path_str])?;
                    delete_symbols.execute(rusqlite::params![&path_str])?;
                    delete_edges.execute(rusqlite::params![&path_str])?;
                }
            }
        }

        if let Some(ref dm) = dir_mtimes {
            let mut all_dirs = dm.clone();
            // Preserve stored mtimes for pruned dirs (they weren't walked)
            if let Ok(stored_dirs) = self.load_dir_state() {
                for (path, mtime) in stored_dirs {
                    if pruned_dirs.contains(&path) {
                        all_dirs.insert(path, mtime);
                    }
                }
            }
            self.save_dir_state(&all_dirs)?;
        }

        Ok(summary)
    }

    pub fn rebuild(&self, root: &Path) -> Result<IndexSummary> {
        self.conn.execute("DELETE FROM file_state", [])?;
        self.conn.execute("DELETE FROM symbols", [])?;
        self.conn.execute("DELETE FROM call_edges", [])?;
        self.conn.execute("DELETE FROM dir_state", [])?;
        self.apply_changes(root)
    }

    pub fn file_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM file_state", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn symbols_for_file(&self, file: &str) -> Result<Vec<StoredSymbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, language, signature, file, line, end_line, parent_module, visibility, tags FROM symbols WHERE file = ?1 ORDER BY line"
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
                tags: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn edge_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM call_edges", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn all_edges(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT caller_name, callee_name FROM call_edges")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn symbol_info(&self, name: &str) -> Result<Vec<StoredSymbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, language, signature, file, line, end_line, parent_module, visibility, tags FROM symbols WHERE name = ?1 ORDER BY file, line"
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |row| {
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
                tags: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn callers_of(&self, name: &str) -> Result<Vec<StoredEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT caller_file, caller_name, caller_line, callee_name, call_site_line FROM call_edges WHERE callee_name = ?1 ORDER BY caller_file, call_site_line"
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |row| {
            Ok(StoredEdge {
                caller_file: row.get(0)?,
                caller_name: row.get(1)?,
                caller_line: row.get(2)?,
                callee_name: row.get(3)?,
                call_site_line: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn callees_of(&self, name: &str) -> Result<Vec<StoredEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT caller_file, caller_name, caller_line, callee_name, call_site_line FROM call_edges WHERE caller_name = ?1 ORDER BY caller_file, call_site_line"
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |row| {
            Ok(StoredEdge {
                caller_file: row.get(0)?,
                caller_name: row.get(1)?,
                caller_line: row.get(2)?,
                callee_name: row.get(3)?,
                call_site_line: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn symbol_search(&self, query: &str, limit: usize) -> Result<Vec<SymbolHit>> {
        let query_tags = compute_tags(query);
        let query_tag_list: Vec<&str> = query_tags.split(',').collect();
        let query_lower = query.to_lowercase();

        let mut stmt = self
            .conn
            .prepare("SELECT name, kind, language, file, line, end_line, tags FROM symbols")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;

        let mut hits: Vec<SymbolHit> = Vec::new();
        for row in rows {
            let (name, kind, language, file, line, end_line, tags) = row?;
            let name_lower = name.to_lowercase();

            if name_lower == query_lower {
                hits.push(SymbolHit {
                    name,
                    kind,
                    language,
                    file,
                    line,
                    end_line,
                    tags,
                    score: 1.0,
                    match_type: "exact_name".to_string(),
                });
                continue;
            }

            if let Some(ref sym_tags) = tags {
                let sym_tag_list: Vec<&str> = sym_tags.split(',').collect();
                let matching: usize = query_tag_list
                    .iter()
                    .filter(|qt| sym_tag_list.contains(qt))
                    .count();

                if matching == 0 {
                    continue;
                }

                let precision = matching as f64 / query_tag_list.len() as f64;
                let recall = matching as f64 / sym_tag_list.len() as f64;
                let f1 = if precision + recall > 0.0 {
                    2.0 * precision * recall / (precision + recall)
                } else {
                    0.0
                };

                let match_type = if matching == query_tag_list.len() {
                    "all_tags"
                } else {
                    "partial_tags"
                };

                hits.push(SymbolHit {
                    name,
                    kind,
                    language,
                    file,
                    line,
                    end_line,
                    tags,
                    score: f1,
                    match_type: match_type.to_string(),
                });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

fn acquire_write_lock(db_path: &Path) -> Result<WriteLockGuard> {
    let lock_path = writer_lock_path(db_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating lock dir: {}", parent.display()))?;
    }

    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut lock_file) => {
                use std::io::Write;
                writeln!(lock_file, "{}", std::process::id())
                    .with_context(|| format!("writing {}", lock_path.display()))?;
                return Ok(WriteLockGuard { path: lock_path });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if clear_stale_lock_if_possible(&lock_path)? {
                    continue;
                }
                let holder = read_lock_pid(&lock_path)
                    .map(|pid| format!(" (pid {})", pid))
                    .unwrap_or_default();
                bail!(
                    "another tsift index writer is already active for {}{} (lock: {}). \
                     A concurrent `tsift index` or `tsift search --autoindex` is already updating this index; \
                     wait for it to finish, or remove the lock file if that writer crashed.",
                    db_path.display(),
                    holder,
                    lock_path.display()
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", lock_path.display()));
            }
        }
    }

    unreachable!("lock acquisition retries are bounded")
}

fn writer_lock_path(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("index");
    db_path.with_file_name(format!("{stem}.lock"))
}

fn clear_stale_lock_if_possible(lock_path: &Path) -> Result<bool> {
    let Some(pid) = read_lock_pid(lock_path) else {
        return Ok(false);
    };
    if process_exists(pid) {
        return Ok(false);
    }
    std::fs::remove_file(lock_path)
        .with_context(|| format!("removing stale lock {}", lock_path.display()))?;
    Ok(true)
}

fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(lock_path).ok()?;
    content.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(_pid: u32) -> bool {
    true
}

fn compute_tags(name: &str) -> String {
    let convention = tagpath_parser::detect_convention(name);
    let parsed = tagpath_parser::parse(name, convention);
    parsed.tags.join(",")
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
        fs::write(
            dir.path().join("main.rs"),
            "fn main() { println!(\"hi\"); }",
        )
        .unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.unchanged, 2);
        let modified = summary
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Modified)
            .unwrap();
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
        let new = summary
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::New)
            .unwrap();
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
        let deleted = summary
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Deleted)
            .unwrap();
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
        let paths: Vec<String> = summary
            .changes
            .iter()
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
        assert!(
            db.symbol_count().unwrap() > 0,
            "expected symbols from indexed files"
        );
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
        fs::write(
            dir.path().join("main.rs"),
            "fn main() {}\nfn extra() {}\nfn another() {}",
        )
        .unwrap();
        db.apply_changes(dir.path()).unwrap();
        assert!(
            db.symbol_count().unwrap() > initial,
            "expected more symbols after adding functions"
        );
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
        assert!(
            db.symbol_count().unwrap() < initial,
            "expected fewer symbols after deleting file"
        );
    }

    #[test]
    fn rebuild_clears_and_reextracts_symbols() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let count = db.symbol_count().unwrap();
        assert!(count > 0);

        db.rebuild(dir.path()).unwrap();
        assert_eq!(
            db.symbol_count().unwrap(),
            count,
            "rebuild should produce same symbol count"
        );
    }

    #[test]
    fn symbols_for_nonexistent_file_returns_empty() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let symbols = db.symbols_for_file("/no/such/file.rs").unwrap();
        assert!(symbols.is_empty());
    }

    #[test]
    fn symbols_have_tags() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn get_user_name() {}\nstruct UserProfile;",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let lib_rs = dir.path().join("lib.rs").to_string_lossy().to_string();
        let symbols = db.symbols_for_file(&lib_rs).unwrap();
        let get_user = symbols.iter().find(|s| s.name == "get_user_name").unwrap();
        assert_eq!(get_user.tags.as_deref(), Some("get,user,name"));

        let user_profile = symbols.iter().find(|s| s.name == "UserProfile").unwrap();
        assert_eq!(user_profile.tags.as_deref(), Some("user,profile"));
    }

    #[test]
    fn symbol_search_exact_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn main() {}\nfn helper() {}").unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("main", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "main");
        assert_eq!(hits[0].match_type, "exact_name");
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn symbol_search_cross_convention() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn get_user_name() {}").unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("getUserName", 10).unwrap();
        assert!(
            !hits.is_empty(),
            "should find snake_case via camelCase query"
        );
        assert_eq!(hits[0].name, "get_user_name");
        assert_eq!(hits[0].match_type, "all_tags");
    }

    #[test]
    fn symbol_search_partial_tags() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn get_user_name() {}\nfn set_user_name() {}",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("user", 10).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "should find both functions containing 'user' tag"
        );
    }

    #[test]
    fn symbol_search_no_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn main() {}").unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("nonexistent_function", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn symbol_search_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn a_test() {}\nfn b_test() {}\nfn c_test() {}",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("test", 2).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn call_edges_extracted_on_index() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        assert!(
            db.edge_count().unwrap() > 0,
            "expected call edges from indexed files"
        );
    }

    #[test]
    fn callers_of_query() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let callers = db.callers_of("helper").unwrap();
        assert!(!callers.is_empty(), "expected callers of helper");
        assert_eq!(callers[0].caller_name, "main");
    }

    #[test]
    fn callees_of_query() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let callees = db.callees_of("main").unwrap();
        let names: Vec<&str> = callees.iter().map(|e| e.callee_name.as_str()).collect();
        assert!(
            names.contains(&"helper"),
            "expected main to call helper, got: {:?}",
            names
        );
        assert!(
            names.contains(&"new"),
            "expected main to call new, got: {:?}",
            names
        );
    }

    #[test]
    fn edges_deleted_with_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let initial = db.edge_count().unwrap();
        assert!(initial > 0);
        fs::remove_file(dir.path().join("main.rs")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.edge_count().unwrap(), 0);
    }

    #[test]
    fn rebuild_clears_and_reextracts_edges() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let count = db.edge_count().unwrap();
        db.rebuild(dir.path()).unwrap();
        assert_eq!(db.edge_count().unwrap(), count);
    }

    #[test]
    fn edges_updated_on_modification() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() { foo(); }").unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let initial = db.edge_count().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(
            dir.path().join("main.rs"),
            "fn main() { foo(); bar(); baz(); }",
        )
        .unwrap();
        db.apply_changes(dir.path()).unwrap();
        assert!(db.edge_count().unwrap() > initial);
    }

    #[test]
    fn python_edges_extracted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "def helper(): pass\ndef main(): helper()",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let callers = db.callers_of("helper").unwrap();
        assert!(!callers.is_empty(), "expected python call edges");
        assert_eq!(callers[0].caller_name, "main");
    }

    #[test]
    fn pruned_first_index_same_as_regular() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        let summary = db.apply_changes_pruned(dir.path()).unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.total_tracked, 3);
        assert!(summary.prune_stats.is_some());
        assert_eq!(summary.prune_stats.as_ref().unwrap().dirs_pruned, 0);
    }

    #[test]
    fn pruned_second_index_prunes_unchanged() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes_pruned(dir.path()).unwrap();

        let summary = db.compute_changes_pruned(dir.path()).unwrap();
        assert_eq!(summary.new, 0);
        assert_eq!(summary.deleted, 0);
        let ps = summary.prune_stats.as_ref().unwrap();
        assert!(
            ps.dirs_pruned > 0,
            "expected some dirs to be pruned on second run"
        );
    }

    #[test]
    fn pruned_detects_new_file_in_changed_dir() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes_pruned(dir.path()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(dir.path().join("extra.rs"), "fn extra() {}").unwrap();

        let summary = db.compute_changes_pruned(dir.path()).unwrap();
        assert_eq!(summary.new, 1);
        let new_file = summary
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::New)
            .unwrap();
        assert!(new_file.path.ends_with("extra.rs"));
    }

    #[test]
    fn pruned_does_not_report_pruned_files_as_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("sub/lib.rs"), "fn lib() {}").unwrap();

        let db = db_in(root);
        db.apply_changes_pruned(root).unwrap();
        assert_eq!(db.file_count().unwrap(), 2);

        let summary = db.compute_changes_pruned(root).unwrap();
        assert_eq!(
            summary.deleted, 0,
            "pruned files should not appear as deleted"
        );
        assert_eq!(summary.unchanged, 2);
    }

    #[test]
    fn rebuild_clears_dir_state() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes_pruned(dir.path()).unwrap();

        let dirs = db.load_dir_state().unwrap();
        assert!(
            !dirs.is_empty(),
            "dir_state should be populated after pruned index"
        );

        db.rebuild(dir.path()).unwrap();
        let dirs = db.load_dir_state().unwrap();
        assert!(dirs.is_empty(), "dir_state should be cleared after rebuild");
    }

    #[test]
    fn all_edges_returns_distinct_pairs() {
        let dir = tempfile::tempdir().unwrap();
        // main calls helper twice (two call sites) — all_edges should deduplicate
        fs::write(
            dir.path().join("main.rs"),
            "fn helper() {}\nfn main() { helper(); helper(); }",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        let edges = db.all_edges().unwrap();
        assert!(!edges.is_empty());
        let pairs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        assert!(pairs.contains(&("main", "helper")));
        let count = pairs
            .iter()
            .filter(|&&(a, b)| a == "main" && b == "helper")
            .count();
        assert_eq!(count, 1, "all_edges should deduplicate parallel edges");
    }

    #[test]
    fn has_changes_true_when_new() {
        let s = IndexSummary {
            total_tracked: 1,
            new: 1,
            modified: 0,
            deleted: 0,
            unchanged: 0,
            changes: vec![],
            prune_stats: None,
        };
        assert!(s.has_changes());
    }

    #[test]
    fn has_changes_true_when_modified() {
        let s = IndexSummary {
            total_tracked: 1,
            new: 0,
            modified: 1,
            deleted: 0,
            unchanged: 0,
            changes: vec![],
            prune_stats: None,
        };
        assert!(s.has_changes());
    }

    #[test]
    fn has_changes_true_when_deleted() {
        let s = IndexSummary {
            total_tracked: 0,
            new: 0,
            modified: 0,
            deleted: 1,
            unchanged: 0,
            changes: vec![],
            prune_stats: None,
        };
        assert!(s.has_changes());
    }

    #[test]
    fn has_changes_false_when_unchanged() {
        let s = IndexSummary {
            total_tracked: 3,
            new: 0,
            modified: 0,
            deleted: 0,
            unchanged: 3,
            changes: vec![],
            prune_stats: None,
        };
        assert!(!s.has_changes());
    }

    #[test]
    fn has_changes_false_when_empty() {
        let s = IndexSummary {
            total_tracked: 0,
            new: 0,
            modified: 0,
            deleted: 0,
            unchanged: 0,
            changes: vec![],
            prune_stats: None,
        };
        assert!(!s.has_changes());
    }

    #[test]
    fn open_configures_sqlite_for_concurrent_access() {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();

        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let timeout_ms: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(mode.to_lowercase(), "wal");
        assert_eq!(timeout_ms, 5000);
    }

    #[test]
    fn open_read_only_uses_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        let _ = IndexDb::open(&db_path).unwrap();

        let db = IndexDb::open_read_only(&db_path).unwrap();
        let timeout_ms: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(timeout_ms, 5000);
    }

    #[test]
    fn open_fails_fast_when_writer_lock_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, std::process::id().to_string()).unwrap();

        let err = match IndexDb::open(&dir.path().join(".tsift/index.db")) {
            Ok(_) => panic!("expected writer lock conflict"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("another tsift index writer is already active"));
        assert!(msg.contains("search --autoindex"));
    }

    #[test]
    fn open_removes_stale_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "999999").unwrap();

        {
            let _db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
            let contents = fs::read_to_string(&lock_path).unwrap();
            assert_eq!(contents.trim(), std::process::id().to_string());
        }

        assert!(!lock_path.exists());
    }
}
