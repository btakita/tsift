use crate::graph;
use crate::lang::Lang;
use crate::walk::{self, FileEntry, PruneStats};
use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tagpath::parser as tagpath_parser;

pub struct IndexDb {
    conn: Connection,
    _write_lock: Option<WriteLockGuard>,
    _snapshot_copy: Option<SnapshotCopyGuard>,
}

struct WriteLockGuard {
    file: File,
}

impl Drop for WriteLockGuard {
    fn drop(&mut self) {
        let _ = clear_lock_metadata(&mut self.file);
        let _ = self.file.unlock();
    }
}

struct SnapshotCopyGuard {
    paths: Vec<PathBuf>,
}

impl Drop for SnapshotCopyGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

const INDEX_DB_WAL_AUTOCHECKPOINT_PAGES: i64 = 256;

#[cfg(test)]
thread_local! {
    static FAIL_APPLY_CHANGES_AFTER_FILE_MUTATIONS: Cell<bool> = const { Cell::new(false) };
    static FAIL_REBUILD_AFTER_CLEAR: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
enum TestFailpoint {
    ApplyChangesAfterFileMutations,
    RebuildAfterClear,
}

#[cfg(test)]
struct TestFailpointGuard(TestFailpoint);

#[cfg(test)]
impl Drop for TestFailpointGuard {
    fn drop(&mut self) {
        match self.0 {
            TestFailpoint::ApplyChangesAfterFileMutations => {
                FAIL_APPLY_CHANGES_AFTER_FILE_MUTATIONS.with(|flag| flag.set(false));
            }
            TestFailpoint::RebuildAfterClear => {
                FAIL_REBUILD_AFTER_CLEAR.with(|flag| flag.set(false));
            }
        }
    }
}

#[cfg(test)]
fn arm_apply_changes_failpoint() -> TestFailpointGuard {
    FAIL_APPLY_CHANGES_AFTER_FILE_MUTATIONS.with(|flag| flag.set(true));
    TestFailpointGuard(TestFailpoint::ApplyChangesAfterFileMutations)
}

#[cfg(test)]
fn arm_rebuild_failpoint() -> TestFailpointGuard {
    FAIL_REBUILD_AFTER_CLEAR.with(|flag| flag.set(true));
    TestFailpointGuard(TestFailpoint::RebuildAfterClear)
}

#[cfg(test)]
fn maybe_fail_apply_changes_after_file_mutations() -> Result<()> {
    if FAIL_APPLY_CHANGES_AFTER_FILE_MUTATIONS.with(|flag| flag.replace(false)) {
        bail!("injected apply_changes failure after file mutations");
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_apply_changes_after_file_mutations() -> Result<()> {
    Ok(())
}

#[cfg(test)]
fn maybe_fail_rebuild_after_clear() -> Result<()> {
    if FAIL_REBUILD_AFTER_CLEAR.with(|flag| flag.replace(false)) {
        bail!("injected rebuild failure after clearing index tables");
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_rebuild_after_clear() -> Result<()> {
    Ok(())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexWarningStage {
    ReadSource,
    ExtractSymbols,
    ExtractCallSites,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexWarning {
    pub path: PathBuf,
    pub stage: IndexWarningStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct IndexSummary {
    pub total_tracked: usize,
    pub new: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub changes: Vec<FileChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<IndexWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prune_stats: Option<PruneStats>,
}

impl IndexSummary {
    pub fn has_changes(&self) -> bool {
        self.new > 0 || self.modified > 0 || self.deleted > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyRecovery {
    SnapshotFallback,
    SnapshotFallbackWal,
}

#[derive(Debug)]
pub struct ReadOnlyInspectResult {
    pub total_files: usize,
    pub summary: IndexSummary,
    pub recovery: Option<ReadOnlyRecovery>,
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

fn warning_on_error<T>(
    result: Result<T>,
    warnings: &mut Vec<IndexWarning>,
    path: &Path,
    language: Option<&str>,
    stage: IndexWarningStage,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            warnings.push(IndexWarning {
                path: path.to_path_buf(),
                stage,
                language: language.map(str::to_string),
                message: format!("{err:#}"),
            });
            None
        }
    }
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
        conn.pragma_update(
            None,
            "wal_autocheckpoint",
            INDEX_DB_WAL_AUTOCHECKPOINT_PAGES,
        )?;
        let checkpoint_pages: i64 =
            conn.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))?;
        if checkpoint_pages != INDEX_DB_WAL_AUTOCHECKPOINT_PAGES {
            bail!(
                "index db {} requires wal_autocheckpoint={}, got {}",
                db_path.display(),
                INDEX_DB_WAL_AUTOCHECKPOINT_PAGES,
                checkpoint_pages
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
            _snapshot_copy: None,
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
            _snapshot_copy: None,
        })
    }

    pub fn open_read_only_resilient(db_path: &Path) -> Result<Self> {
        match Self::open_read_only(db_path).and_then(|db| {
            db.ensure_readable()?;
            Ok(db)
        }) {
            Ok(db) => Ok(db),
            Err(err) => match read_only_snapshot_recovery(db_path, &err) {
                Some(_) => Self::open_read_only_snapshot(db_path),
                None => Err(err),
            },
        }
    }

    pub fn symbol_names_read_only_min_len(db_path: &Path, min_len: usize) -> Result<Vec<String>> {
        let db = Self::open_read_only_resilient(db_path)?;
        db.symbol_names_min_len(min_len)
    }

    pub fn file_symbols_read_only(
        db_path: &Path,
        candidates: &[String],
    ) -> Result<Vec<(String, String)>> {
        let db = Self::open_read_only_resilient(db_path)?;
        db.file_symbols(candidates)
    }

    pub fn file_paths_read_only(db_path: &Path) -> Result<Vec<String>> {
        let db = Self::open_read_only_resilient(db_path)?;
        db.file_paths()
    }

    pub fn inspect_read_only(
        db_path: &Path,
        root: &Path,
        prune: bool,
    ) -> Result<ReadOnlyInspectResult> {
        match Self::inspect_read_only_once(db_path, root, prune) {
            Ok(result) => Ok(result),
            Err(err) => {
                let Some(recovery) = read_only_snapshot_recovery(db_path, &err) else {
                    return Err(err);
                };
                let db = Self::open_read_only_snapshot(db_path)?;
                let total_files = db.file_count()?;
                let summary = if prune {
                    db.compute_changes_pruned(root)?
                } else {
                    db.compute_changes(root)?
                };
                Ok(ReadOnlyInspectResult {
                    total_files,
                    summary,
                    recovery: Some(recovery),
                })
            }
        }
    }

    fn inspect_read_only_once(
        db_path: &Path,
        root: &Path,
        prune: bool,
    ) -> Result<ReadOnlyInspectResult> {
        let db = Self::open_read_only(db_path)?;
        let total_files = db.file_count()?;
        let summary = if prune {
            db.compute_changes_pruned(root)?
        } else {
            db.compute_changes(root)?
        };
        Ok(ReadOnlyInspectResult {
            total_files,
            summary,
            recovery: None,
        })
    }

    fn open_read_only_snapshot(db_path: &Path) -> Result<Self> {
        let (snapshot_path, cleanup_paths) = copy_read_only_snapshot(db_path, "index")?;
        let conn = Connection::open_with_flags(
            &snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening index snapshot {}", snapshot_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            _write_lock: None,
            _snapshot_copy: Some(SnapshotCopyGuard {
                paths: cleanup_paths,
            }),
        })
    }

    fn ensure_readable(&self) -> Result<()> {
        self.conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |_row| Ok(()))
            .map_err(anyhow::Error::from)
    }

    fn file_symbols(&self, candidates: &[String]) -> Result<Vec<(String, String)>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let table_exists: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='symbols'",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(Vec::new());
        }

        let placeholders = (1..=candidates.len())
            .map(|idx| format!("?{idx}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql =
            format!("SELECT name, kind FROM symbols WHERE file IN ({placeholders}) ORDER BY line");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(candidates.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
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
            warnings: Vec::new(),
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
            warnings: Vec::new(),
            prune_stats,
        };

        self.conn.execute_batch("SAVEPOINT sp_apply")?;
        let apply_result: Result<Vec<IndexWarning>> = (|| {
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
            let mut warnings = Vec::new();

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
                            let lang_name = lang.name();
                            let source = warning_on_error(
                                std::fs::read(&change.path)
                                    .with_context(|| format!("reading {}", change.path.display())),
                                &mut warnings,
                                &change.path,
                                Some(lang_name),
                                IndexWarningStage::ReadSource,
                            );
                            let symbols = source.as_ref().and_then(|source| {
                                warning_on_error(
                                    lang.extract_symbols(source),
                                    &mut warnings,
                                    &change.path,
                                    Some(lang_name),
                                    IndexWarningStage::ExtractSymbols,
                                )
                            });
                            if let Some(ref symbols) = symbols {
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
                                let call_sites = warning_on_error(
                                    graph::extract_call_sites(lang, source),
                                    &mut warnings,
                                    &change.path,
                                    Some(lang_name),
                                    IndexWarningStage::ExtractCallSites,
                                );
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

            maybe_fail_apply_changes_after_file_mutations()?;

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

            Ok(warnings)
        })();
        let mut summary = summary;
        match apply_result {
            Ok(warnings) => {
                self.conn.execute_batch("RELEASE sp_apply")?;
                summary.warnings = warnings;
            }
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK TO sp_apply");
                let _ = self.conn.execute_batch("RELEASE sp_apply");
                return Err(err);
            }
        }

        Ok(summary)
    }

    pub fn rebuild(&self, root: &Path) -> Result<IndexSummary> {
        self.conn.execute_batch("SAVEPOINT sp_rebuild")?;
        let result: Result<IndexSummary> = (|| {
            self.conn.execute("DELETE FROM file_state", [])?;
            self.conn.execute("DELETE FROM symbols", [])?;
            self.conn.execute("DELETE FROM call_edges", [])?;
            self.conn.execute("DELETE FROM dir_state", [])?;
            maybe_fail_rebuild_after_clear()?;
            self.apply_changes(root)
        })();
        match result {
            Ok(summary) => {
                self.conn.execute_batch("RELEASE sp_rebuild")?;
                Ok(summary)
            }
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK TO sp_rebuild");
                let _ = self.conn.execute_batch("RELEASE sp_rebuild");
                Err(err)
            }
        }
    }

    pub fn file_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM file_state", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM file_state ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn symbol_names_min_len(&self, min_len: usize) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT name FROM symbols WHERE length(name) >= ?1")?;
        let rows = stmt.query_map(rusqlite::params![min_len as i64], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query_tags = compute_tags(query);
        let query_tag_list: Vec<&str> = query_tags
            .split(',')
            .filter(|tag| !tag.is_empty())
            .collect();
        let query_lower = query.to_lowercase();

        let exact_match_expr = "name = ?1 COLLATE NOCASE";
        let mut where_clauses = vec![exact_match_expr.to_string()];
        let mut match_count_terms = Vec::new();
        let mut params = vec![rusqlite::types::Value::from(query.to_string())];

        for tag in &query_tag_list {
            let param_idx = params.len() + 1;
            let placeholder = format!("?{param_idx}");
            match_count_terms.push(format!(
                "CASE WHEN instr(',' || COALESCE(tags, '') || ',', {placeholder}) > 0 THEN 1 ELSE 0 END"
            ));
            where_clauses.push(format!(
                "instr(',' || COALESCE(tags, '') || ',', {placeholder}) > 0"
            ));
            params.push(rusqlite::types::Value::from(format!(",{tag},")));
        }

        // Keep the SQL candidate ordering aligned with the Rust-side F1 ranking so the
        // bounded query still yields the same top hits without scanning the full table.
        let match_count_expr = if match_count_terms.is_empty() {
            "0".to_string()
        } else {
            match_count_terms.join(" + ")
        };
        let tag_count_expr = "CASE WHEN tags IS NULL OR tags = '' THEN 0 ELSE LENGTH(tags) - LENGTH(REPLACE(tags, ',', '')) + 1 END";
        let limit_param_idx = params.len() + 1;
        let sql = format!(
            "SELECT name, kind, language, file, line, end_line, tags, {match_count_expr} AS match_count, {tag_count_expr} AS tag_count \
             FROM symbols \
             WHERE {} \
             ORDER BY \
                 CASE WHEN {exact_match_expr} THEN 1 ELSE 0 END DESC, \
                 match_count DESC, \
                 tag_count ASC, \
                 name COLLATE NOCASE ASC, \
                 file ASC, \
                 line ASC \
             LIMIT ?{limit_param_idx}",
            where_clauses.join(" OR ")
        );
        params.push(rusqlite::types::Value::from(
            i64::try_from(limit).unwrap_or(i64::MAX),
        ));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
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

    let mut lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;

    match lock_file.try_lock_exclusive() {
        Ok(true) => {
            write_lock_pid(&mut lock_file, &lock_path)?;
            Ok(WriteLockGuard { file: lock_file })
        }
        Ok(false) => {
            let holder = match read_lock_marker(&mut lock_file)
                .with_context(|| format!("reading {}", lock_path.display()))?
            {
                LockFileMarker::Pid(pid) => format!(" (pid {})", pid),
                _ => String::new(),
            };
            bail!(
                "another tsift index writer is already active for {}{} (lock: {}). \
                 A concurrent `tsift index` or `tsift search --autoindex` is already updating this index; \
                 wait for it to finish before retrying.",
                db_path.display(),
                holder,
                lock_path.display()
            );
        }
        Err(err) => Err(err).with_context(|| format!("locking {}", lock_path.display())),
    }
}

pub(crate) fn writer_lock_path(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("index");
    db_path.with_file_name(format!("{stem}.lock"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockFileMarker {
    Empty,
    Pid(u32),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriterLockProbe {
    Absent { path: PathBuf },
    Live { path: PathBuf, pid: Option<u32> },
    Stale { path: PathBuf, pid: Option<u32> },
    Unknown { path: PathBuf },
}

pub(crate) fn probe_writer_lock(lock_path: &Path) -> Result<WriterLockProbe> {
    if !lock_path.exists() {
        return Ok(WriterLockProbe::Absent {
            path: lock_path.to_path_buf(),
        });
    }

    let mut lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;

    match lock_file.try_lock_exclusive() {
        Ok(true) => {
            let marker = read_lock_marker(&mut lock_file)
                .with_context(|| format!("reading {}", lock_path.display()))?;
            lock_file
                .unlock()
                .with_context(|| format!("unlocking {}", lock_path.display()))?;
            Ok(match marker {
                LockFileMarker::Empty => WriterLockProbe::Absent {
                    path: lock_path.to_path_buf(),
                },
                LockFileMarker::Pid(pid) => WriterLockProbe::Stale {
                    path: lock_path.to_path_buf(),
                    pid: Some(pid),
                },
                LockFileMarker::Invalid => WriterLockProbe::Unknown {
                    path: lock_path.to_path_buf(),
                },
            })
        }
        Ok(false) => {
            let marker = read_lock_marker(&mut lock_file)
                .with_context(|| format!("reading {}", lock_path.display()))?;
            Ok(match marker {
                LockFileMarker::Pid(pid) => WriterLockProbe::Live {
                    path: lock_path.to_path_buf(),
                    pid: Some(pid),
                },
                LockFileMarker::Empty | LockFileMarker::Invalid => WriterLockProbe::Live {
                    path: lock_path.to_path_buf(),
                    pid: None,
                },
            })
        }
        Err(err) => Err(err).with_context(|| format!("locking {}", lock_path.display())),
    }
}

pub(crate) fn read_only_snapshot_recovery(
    db_path: &Path,
    err: &anyhow::Error,
) -> Option<ReadOnlyRecovery> {
    if !error_mentions_locked_db(err) {
        return None;
    }
    if wal_sidecar_path(db_path).exists() || shared_memory_sidecar_path(db_path).exists() {
        Some(ReadOnlyRecovery::SnapshotFallbackWal)
    } else if rollback_journal_path(db_path).exists() {
        Some(ReadOnlyRecovery::SnapshotFallback)
    } else {
        None
    }
}

pub(crate) fn rollback_journal_path(db_path: &Path) -> PathBuf {
    let mut journal = db_path.as_os_str().to_os_string();
    journal.push("-journal");
    PathBuf::from(journal)
}

pub(crate) fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut wal = db_path.as_os_str().to_os_string();
    wal.push("-wal");
    PathBuf::from(wal)
}

pub(crate) fn shared_memory_sidecar_path(db_path: &Path) -> PathBuf {
    let mut shm = db_path.as_os_str().to_os_string();
    shm.push("-shm");
    PathBuf::from(shm)
}

pub(crate) fn copy_read_only_snapshot(
    db_path: &Path,
    default_stem: &str,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let snapshot_path = snapshot_copy_path(db_path, default_stem);
    std::fs::copy(db_path, &snapshot_path).with_context(|| {
        format!(
            "copying locked db {} to snapshot {}",
            db_path.display(),
            snapshot_path.display()
        )
    })?;
    let mut cleanup_paths = vec![snapshot_path.clone()];
    copy_optional_snapshot_sidecar(
        &wal_sidecar_path(db_path),
        &wal_sidecar_path(&snapshot_path),
        &mut cleanup_paths,
    )?;
    copy_optional_snapshot_sidecar(
        &shared_memory_sidecar_path(db_path),
        &shared_memory_sidecar_path(&snapshot_path),
        &mut cleanup_paths,
    )?;
    Ok((snapshot_path, cleanup_paths))
}

fn copy_optional_snapshot_sidecar(
    source_path: &Path,
    snapshot_path: &Path,
    cleanup_paths: &mut Vec<PathBuf>,
) -> Result<()> {
    match std::fs::copy(source_path, snapshot_path) {
        Ok(_) => {
            cleanup_paths.push(snapshot_path.to_path_buf());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "copying SQLite sidecar {} to snapshot {}",
                source_path.display(),
                snapshot_path.display()
            )
        }),
    }
}

fn snapshot_copy_path(db_path: &Path, default_stem: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(default_stem);
    let mut file_name = OsString::from(format!("tsift-{stem}-{}-{nanos}", std::process::id()));
    file_name.push(".db");
    std::env::temp_dir().join(file_name)
}

pub(crate) fn error_mentions_locked_db(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("database is locked"))
}

fn read_lock_marker(file: &mut File) -> std::io::Result<LockFileMarker> {
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        Ok(LockFileMarker::Empty)
    } else if let Ok(pid) = trimmed.parse::<u32>() {
        Ok(LockFileMarker::Pid(pid))
    } else {
        Ok(LockFileMarker::Invalid)
    }
}

fn write_lock_pid(file: &mut File, lock_path: &Path) -> Result<()> {
    file.set_len(0)
        .with_context(|| format!("clearing {}", lock_path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seeking {}", lock_path.display()))?;
    writeln!(file, "{}", std::process::id())
        .with_context(|| format!("writing {}", lock_path.display()))?;
    file.sync_data()
        .with_context(|| format!("syncing {}", lock_path.display()))?;
    Ok(())
}

fn clear_lock_metadata(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.sync_data()?;
    Ok(())
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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

    fn hold_wal_lock(db_path: &Path) -> Connection {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE IF NOT EXISTS wal_lock_probe (id INTEGER PRIMARY KEY);
             INSERT INTO wal_lock_probe DEFAULT VALUES;
             PRAGMA locking_mode=EXCLUSIVE;
             BEGIN EXCLUSIVE;",
        )
        .unwrap();
        assert!(wal_sidecar_path(db_path).exists());
        conn
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

    #[cfg(unix)]
    #[test]
    fn apply_changes_warns_on_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.rs");
        fs::write(&main_path, "fn main() {}\n").unwrap();

        let original_mode = fs::metadata(&main_path).unwrap().permissions().mode();
        let mut unreadable = fs::metadata(&main_path).unwrap().permissions();
        unreadable.set_mode(0o000);
        fs::set_permissions(&main_path, unreadable).unwrap();

        let db = db_in(dir.path());
        let summary = db.apply_changes(dir.path()).unwrap();

        let mut restored = fs::metadata(&main_path).unwrap().permissions();
        restored.set_mode(original_mode);
        fs::set_permissions(&main_path, restored).unwrap();

        assert_eq!(summary.warnings.len(), 1);
        let warning = &summary.warnings[0];
        assert_eq!(warning.path, main_path);
        assert_eq!(warning.stage, IndexWarningStage::ReadSource);
        assert_eq!(warning.language.as_deref(), Some("rust"));
        assert!(warning.message.contains("reading"));
        assert_eq!(db.symbol_count().unwrap(), 0);
        assert_eq!(db.edge_count().unwrap(), 0);
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
    fn symbol_search_limit_keeps_best_tag_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn get_user_name() {}\n\
             fn get_cached_user_name() {}\n\
             fn set_user_name() {}\n\
             fn user_id() {}\n",
        )
        .unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("user_name", 2).unwrap();
        let names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["get_user_name", "set_user_name"]);
    }

    #[test]
    fn symbol_search_zero_limit_returns_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn get_user_name() {}").unwrap();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let hits = db.symbol_search("get_user_name", 0).unwrap();
        assert!(hits.is_empty());
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
    fn pruned_second_index_keeps_full_scan_correctness() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes_pruned(dir.path()).unwrap();

        let summary = db.compute_changes_pruned(dir.path()).unwrap();
        assert_eq!(summary.new, 0);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
        let ps = summary.prune_stats.as_ref().unwrap();
        assert_eq!(ps.dirs_pruned, 0);
    }

    #[test]
    fn pruned_detects_modified_file_even_if_dir_state_matches_current_dir_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sub_path = root.join("sub");
        fs::create_dir_all(&sub_path).unwrap();
        fs::write(sub_path.join("lib.rs"), "fn helper() -> i32 { 1 }\n").unwrap();

        let db = db_in(root);
        db.apply_changes_pruned(root).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(sub_path.join("lib.rs"), "fn helper() -> i32 { 2 }\n").unwrap();

        let mut dir_state = db.load_dir_state().unwrap();
        dir_state.insert(
            sub_path.clone(),
            fs::metadata(&sub_path).unwrap().modified().unwrap(),
        );
        db.save_dir_state(&dir_state).unwrap();

        let summary = db.compute_changes_pruned(root).unwrap();
        assert_eq!(summary.modified, 1);
        let modified = summary
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Modified)
            .unwrap();
        assert!(modified.path.ends_with("sub/lib.rs"));
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
    fn apply_changes_savepoint_releases_cleanly() {
        let dir = setup_tree();
        let db = db_in(dir.path());

        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);

        let summary = db.apply_changes(dir.path()).unwrap();
        assert_eq!(summary.unchanged, 3);
        assert_eq!(db.file_count().unwrap(), 3);

        fs::write(dir.path().join("extra.rs"), "fn extra() {}").unwrap();
        let summary = db.apply_changes(dir.path()).unwrap();
        assert_eq!(summary.new, 1);
        assert_eq!(db.file_count().unwrap(), 4);
    }

    #[test]
    fn apply_changes_rolls_back_on_failure() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let main_path = dir.path().join("main.rs");
        let main_key = main_path.to_string_lossy().into_owned();
        assert_eq!(db.symbols_for_file(&main_key).unwrap().len(), 1);

        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&main_path, "fn main() {}\nfn extra() {}").unwrap();

        let _guard = arm_apply_changes_failpoint();
        let err = db.apply_changes(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("injected apply_changes failure after file mutations")
        );

        assert_eq!(db.file_count().unwrap(), 3);
        assert_eq!(db.symbols_for_file(&main_key).unwrap().len(), 1);

        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.symbols_for_file(&main_key).unwrap().len(), 2);
    }

    #[test]
    fn rebuild_nested_savepoints_work() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);

        db.rebuild(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);

        db.rebuild(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), 3);
    }

    #[test]
    fn rebuild_rolls_back_on_failure_after_clear() {
        let dir = setup_tree();
        let db = db_in(dir.path());
        db.apply_changes(dir.path()).unwrap();

        let initial_file_count = db.file_count().unwrap();
        let initial_symbol_count = db.symbol_count().unwrap();
        let initial_edge_count = db.edge_count().unwrap();

        let _guard = arm_rebuild_failpoint();
        let err = db.rebuild(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("injected rebuild failure after clearing index tables")
        );

        assert_eq!(db.file_count().unwrap(), initial_file_count);
        assert_eq!(db.symbol_count().unwrap(), initial_symbol_count);
        assert_eq!(db.edge_count().unwrap(), initial_edge_count);

        db.rebuild(dir.path()).unwrap();
        assert_eq!(db.file_count().unwrap(), initial_file_count);
        assert_eq!(db.symbol_count().unwrap(), initial_symbol_count);
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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
        let checkpoint_pages: i64 = db
            .conn
            .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))
            .unwrap();
        let timeout_ms: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(mode.to_lowercase(), "wal");
        assert_eq!(checkpoint_pages, INDEX_DB_WAL_AUTOCHECKPOINT_PAGES);
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
    fn inspect_read_only_uses_snapshot_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let source = dir.path().join("main.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        let (secs, nanos) = system_time_to_pair(modified);

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE file_state (
                 path TEXT PRIMARY KEY,
                 mtime_secs INTEGER NOT NULL,
                 mtime_nanos INTEGER NOT NULL,
                 language TEXT NOT NULL
             );
             CREATE TABLE dir_state (
                 path TEXT PRIMARY KEY,
                 mtime_secs INTEGER NOT NULL,
                 mtime_nanos INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_state (path, mtime_secs, mtime_nanos, language) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![source.to_string_lossy(), secs, nanos, "rust"],
        )
        .unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        std::fs::write(rollback_journal_path(&db_path), "locked").unwrap();

        let inspection = IndexDb::inspect_read_only(&db_path, dir.path(), false).unwrap();
        assert_eq!(inspection.total_files, 1);
        assert_eq!(inspection.summary.new, 0);
        assert_eq!(inspection.summary.modified, 0);
        assert_eq!(inspection.summary.deleted, 0);
        assert_eq!(
            inspection.recovery,
            Some(ReadOnlyRecovery::SnapshotFallback)
        );
    }

    #[test]
    fn open_read_only_resilient_uses_snapshot_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        let _ = IndexDb::open(&db_path).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        std::fs::write(rollback_journal_path(&db_path), "locked").unwrap();

        let db = IndexDb::open_read_only_resilient(&db_path).unwrap();
        assert!(db._snapshot_copy.is_some());
        assert!(db.file_count().is_ok());
    }

    #[test]
    fn inspect_read_only_reports_wal_snapshot_recovery_when_wal_db_is_locked() {
        let dir = setup_tree();
        let db_path = dir.path().join(".tsift/index.db");
        let db = IndexDb::open(&db_path).unwrap();
        db.apply_changes(dir.path()).unwrap();
        drop(db);

        let _lock = hold_wal_lock(&db_path);

        let inspection = IndexDb::inspect_read_only(&db_path, dir.path(), false).unwrap();
        assert_eq!(
            inspection.recovery,
            Some(ReadOnlyRecovery::SnapshotFallbackWal)
        );
    }

    #[test]
    fn open_read_only_resilient_copies_wal_sidecars_for_locked_wal_db() {
        let dir = setup_tree();
        let db_path = dir.path().join(".tsift/index.db");
        let db = IndexDb::open(&db_path).unwrap();
        db.apply_changes(dir.path()).unwrap();
        drop(db);

        let _lock = hold_wal_lock(&db_path);

        let db = IndexDb::open_read_only_resilient(&db_path).unwrap();
        assert!(db._snapshot_copy.is_some());
        assert!(db.file_count().is_ok());
    }

    #[test]
    fn open_fails_fast_when_writer_lock_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let mut lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock_file.try_lock_exclusive().unwrap();
        write_lock_pid(&mut lock_file, &lock_path).unwrap();

        let err = match IndexDb::open(&dir.path().join(".tsift/index.db")) {
            Ok(_) => panic!("expected writer lock conflict"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("another tsift index writer is already active"));
        assert!(msg.contains("search --autoindex"));
    }

    #[test]
    fn open_reuses_stale_writer_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "999999").unwrap();

        {
            let _db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
            let contents = fs::read_to_string(&lock_path).unwrap();
            assert_eq!(contents.trim(), std::process::id().to_string());
        }

        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "");
    }

    #[test]
    fn probe_writer_lock_ignores_empty_unlocked_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "").unwrap();

        let probe = probe_writer_lock(&lock_path).unwrap();
        assert!(matches!(probe, WriterLockProbe::Absent { .. }));
    }
}
