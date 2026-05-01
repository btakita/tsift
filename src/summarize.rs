use crate::index::{IndexDb, ReadOnlyRecovery, error_mentions_locked_db, rollback_journal_path};
use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct SummaryDb {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
}

pub struct SummaryReadOnlyOpen {
    pub db: SummaryDb,
    pub recovery: Option<ReadOnlyRecovery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub id: i64,
    pub symbol_name: String,
    pub file_path: String,
    pub content_hash: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<Vec<Entity>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relationships: Option<Vec<Relationship>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concept_labels: Option<Vec<String>>,
    pub extracted_at: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    pub kind: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct SummaryStats {
    pub total_summaries: usize,
    pub total_files: usize,
    pub stale_count: usize,
    pub total_tokens_input: i64,
    pub total_tokens_output: i64,
    pub estimated_tokens_saved: i64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<SummaryStatsWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SummaryStatsWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct ExtractionResponse {
    summary: String,
    #[serde(default)]
    entities: Vec<Entity>,
    #[serde(default)]
    relationships: Vec<Relationship>,
    #[serde(default)]
    concept_labels: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ExtractionReport {
    pub files_processed: usize,
    pub symbols_extracted: usize,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChangedFiles {
    pub existing: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SummarizeConfig {
    pub model: String,
    pub max_file_tokens: usize,
    pub api_key_env: String,
}

const REPLACE_FILE_SAVEPOINT: &str = "tsift_summary_replace";

#[derive(Debug)]
pub(crate) struct SummaryWriteLockGuard {
    file: File,
}

#[derive(Debug)]
struct SnapshotCopyGuard {
    path: PathBuf,
}

impl Drop for SummaryWriteLockGuard {
    fn drop(&mut self) {
        let _ = clear_lock_metadata(&mut self.file);
        let _ = self.file.unlock();
    }
}

impl Drop for SnapshotCopyGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockFileMarker {
    Empty,
    Pid(u32),
    Invalid,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_file_tokens: 8000,
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
        }
    }
}

pub(crate) fn acquire_write_lock(db_path: &Path) -> Result<SummaryWriteLockGuard> {
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
            Ok(SummaryWriteLockGuard { file: lock_file })
        }
        Ok(false) => {
            let holder = match read_lock_marker(&mut lock_file)
                .with_context(|| format!("reading {}", lock_path.display()))?
            {
                LockFileMarker::Pid(pid) => format!(" (pid {})", pid),
                _ => String::new(),
            };
            bail!(
                "another tsift summarize extractor is already active for {}{} (lock: {}). \
                 A concurrent `tsift summarize --extract` is already updating this summary cache; \
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
        .unwrap_or("summaries");
    db_path.with_file_name(format!("{stem}.lock"))
}

impl SummaryDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory for {}", path.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening summaries db: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        if mode.to_lowercase() != "wal" {
            bail!(
                "summaries db {} requires WAL mode for concurrent reads, got {}",
                path.display(),
                mode
            );
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS summaries (
                id INTEGER PRIMARY KEY,
                symbol_name TEXT NOT NULL,
                file_path TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                summary TEXT NOT NULL,
                entities TEXT,
                relationships TEXT,
                concept_labels TEXT,
                extracted_at TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_input INTEGER,
                tokens_output INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_summaries_symbol ON summaries(symbol_name);
            CREATE INDEX IF NOT EXISTS idx_summaries_file ON summaries(file_path);
            CREATE INDEX IF NOT EXISTS idx_summaries_hash ON summaries(content_hash);",
        )?;
        Ok(Self {
            conn,
            _snapshot_copy: None,
        })
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening summaries db: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            _snapshot_copy: None,
        })
    }

    pub fn open_read_only_resilient(path: &Path) -> Result<Self> {
        Self::open_read_only_with_recovery(path).map(|result| result.db)
    }

    pub fn open_read_only_with_recovery(path: &Path) -> Result<SummaryReadOnlyOpen> {
        match Self::open_read_only(path).and_then(|db| {
            db.ensure_readable()?;
            Ok(db)
        }) {
            Ok(db) => Ok(SummaryReadOnlyOpen { db, recovery: None }),
            Err(err) if should_retry_read_only_with_snapshot(path, &err) => {
                let db = Self::open_read_only_snapshot(path)?;
                Ok(SummaryReadOnlyOpen {
                    db,
                    recovery: Some(ReadOnlyRecovery::SnapshotFallback),
                })
            }
            Err(err) => Err(err),
        }
    }

    pub fn get_by_symbol(&self, name: &str) -> Result<Vec<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol_name, file_path, content_hash, summary, entities, relationships,
                    concept_labels, extracted_at, model, tokens_input, tokens_output
             FROM summaries WHERE symbol_name = ?1 ORDER BY extracted_at DESC",
        )?;
        let rows = stmt
            .query_map([name], |row| Ok(row_to_summary(row)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_by_file(&self, path: &str) -> Result<Vec<Summary>> {
        let normalized = normalize_summary_file_key_str(path);
        let legacy = legacy_windows_summary_file_key(&normalized);
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol_name, file_path, content_hash, summary, entities, relationships,
                    concept_labels, extracted_at, model, tokens_input, tokens_output
             FROM summaries WHERE file_path = ?1 OR file_path = ?2 ORDER BY symbol_name",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![normalized, legacy], |row| {
                Ok(row_to_summary(row))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn insert(&self, summary: &Summary) -> Result<()> {
        insert_summary(&self.conn, summary)
    }

    pub fn replace_file(&self, file_path: &str, summaries: &[Summary]) -> Result<()> {
        self.replace_file_with_hook(file_path, summaries, |_| Ok(()))
    }

    pub fn is_current(&self, file_path: &str, content_hash: &str) -> Result<bool> {
        let normalized = normalize_summary_file_key_str(file_path);
        let legacy = legacy_windows_summary_file_key(&normalized);
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM summaries
             WHERE content_hash = ?2 AND (file_path = ?1 OR file_path = ?3)",
            rusqlite::params![normalized, content_hash, legacy],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn stats(&self, root: &Path) -> Result<SummaryStats> {
        let total_summaries: usize =
            self.conn
                .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))?;
        let cached_file_paths = self.cached_file_paths()?;
        let total_files = cached_file_paths.len();
        let (stale_count, warnings) = self.stale_file_count(root, &cached_file_paths)?;
        let total_tokens_input: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_input), 0) FROM summaries",
            [],
            |row| row.get(0),
        )?;
        let total_tokens_output: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_output), 0) FROM summaries",
            [],
            |row| row.get(0),
        )?;
        // Estimated tokens saved: each summary replaces ~2000 tokens of source reading
        // with ~75 tokens of cached summary. Net savings per summary = ~1925 tokens.
        let estimated_tokens_saved = (total_summaries as i64) * 1925;
        Ok(SummaryStats {
            total_summaries,
            total_files,
            stale_count,
            total_tokens_input,
            total_tokens_output,
            estimated_tokens_saved,
            warnings,
        })
    }

    pub fn delete_by_file(&self, file_path: &str) -> Result<usize> {
        let normalized = normalize_summary_file_key_str(file_path);
        let legacy = legacy_windows_summary_file_key(&normalized);
        let count = self.conn.execute(
            "DELETE FROM summaries WHERE file_path = ?1 OR file_path = ?2",
            rusqlite::params![normalized, legacy],
        )?;
        Ok(count)
    }

    pub fn cached_file_paths(&self) -> Result<BTreeSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT file_path FROM summaries ORDER BY file_path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let paths = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(paths
            .into_iter()
            .map(|path| normalize_summary_file_key_str(&path))
            .collect())
    }

    fn stats_live_path(root: &Path, cached_path: &str) -> Option<PathBuf> {
        let normalized_cached_path = normalize_lexical_path(Path::new(cached_path));
        if normalized_cached_path.is_absolute() {
            return None;
        }

        let live_path = normalize_lexical_path(&root.join(&normalized_cached_path));
        if !live_path.starts_with(root) {
            return None;
        }

        Some(live_path)
    }

    fn stale_file_count(
        &self,
        root: &Path,
        cached_file_paths: &BTreeSet<String>,
    ) -> Result<(usize, Vec<SummaryStatsWarning>)> {
        let mut stale_count = 0;
        let mut warnings = Vec::new();

        for cached_path in cached_file_paths {
            let Some(live_path) = Self::stats_live_path(root, cached_path) else {
                stale_count += 1;
                continue;
            };
            if !live_path.is_file() {
                stale_count += 1;
                continue;
            }

            let content = match std::fs::read(&live_path) {
                Ok(content) => content,
                Err(err) => {
                    stale_count += 1;
                    warnings.push(SummaryStatsWarning {
                        path: PathBuf::from(normalize_summary_file_key_str(cached_path)),
                        message: format!(
                            "counting cached summary as stale because the source file could not be read ({err})"
                        ),
                    });
                    continue;
                }
            };
            let live_hash = content_hash(&content);
            if !self.is_current(cached_path, &live_hash)? {
                stale_count += 1;
            }
        }

        Ok((stale_count, warnings))
    }

    fn replace_file_with_hook<F>(
        &self,
        file_path: &str,
        summaries: &[Summary],
        mut after_insert: F,
    ) -> Result<()>
    where
        F: FnMut(usize) -> Result<()>,
    {
        let normalized = normalize_summary_file_key_str(file_path);
        let legacy = legacy_windows_summary_file_key(&normalized);
        self.conn
            .execute_batch(&format!("SAVEPOINT {REPLACE_FILE_SAVEPOINT}"))
            .context("starting summary replacement transaction")?;

        let result = (|| -> Result<()> {
            self.conn.execute(
                "DELETE FROM summaries WHERE file_path = ?1 OR file_path = ?2",
                rusqlite::params![normalized, legacy],
            )?;
            for (idx, summary) in summaries.iter().enumerate() {
                insert_summary(&self.conn, summary)?;
                after_insert(idx)?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn
                    .execute_batch(&format!("RELEASE {REPLACE_FILE_SAVEPOINT}"))
                    .context("committing summary replacement transaction")?;
                Ok(())
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch(&format!(
                    "ROLLBACK TO {REPLACE_FILE_SAVEPOINT}; RELEASE {REPLACE_FILE_SAVEPOINT};"
                )) {
                    return Err(err.context(format!(
                        "rollback failed for summary replacement transaction: {rollback_err}"
                    )));
                }
                Err(err)
            }
        }
    }

    fn ensure_readable(&self) -> Result<()> {
        self.conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |_row| Ok(()))
            .map_err(anyhow::Error::from)
    }

    fn open_read_only_snapshot(path: &Path) -> Result<Self> {
        let snapshot_path = snapshot_copy_path(path);
        std::fs::copy(path, &snapshot_path).with_context(|| {
            format!(
                "copying locked summaries db {} to snapshot {}",
                path.display(),
                snapshot_path.display()
            )
        })?;
        let conn = Connection::open_with_flags(
            &snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening summaries snapshot {}", snapshot_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            _snapshot_copy: Some(SnapshotCopyGuard {
                path: snapshot_path,
            }),
        })
    }
}

fn should_retry_read_only_with_snapshot(path: &Path, err: &anyhow::Error) -> bool {
    rollback_journal_path(path).exists() && error_mentions_locked_db(err)
}

fn snapshot_copy_path(db_path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("summaries");
    let mut file_name = OsString::from(format!("tsift-{stem}-{}-{nanos}", std::process::id()));
    file_name.push(".db");
    std::env::temp_dir().join(file_name)
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

fn insert_summary(conn: &Connection, summary: &Summary) -> Result<()> {
    let normalized_file_path = normalize_summary_file_key_str(&summary.file_path);
    conn.execute(
        "INSERT OR REPLACE INTO summaries
         (symbol_name, file_path, content_hash, summary, entities, relationships,
          concept_labels, extracted_at, model, tokens_input, tokens_output)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            summary.symbol_name,
            normalized_file_path,
            summary.content_hash,
            summary.summary,
            summary
                .entities
                .as_ref()
                .map(|e| serde_json::to_string(e).unwrap_or_default()),
            summary
                .relationships
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default()),
            summary
                .concept_labels
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap_or_default()),
            summary.extracted_at,
            summary.model,
            summary.tokens_input,
            summary.tokens_output,
        ],
    )?;
    Ok(())
}

fn row_to_summary(row: &rusqlite::Row) -> Summary {
    let entities_json: Option<String> = row.get(5).unwrap_or(None);
    let relationships_json: Option<String> = row.get(6).unwrap_or(None);
    let labels_json: Option<String> = row.get(7).unwrap_or(None);
    Summary {
        id: row.get(0).unwrap_or(0),
        symbol_name: row.get(1).unwrap_or_default(),
        file_path: normalize_summary_file_key_str(&row.get::<_, String>(2).unwrap_or_default()),
        content_hash: row.get(3).unwrap_or_default(),
        summary: row.get(4).unwrap_or_default(),
        entities: entities_json.and_then(|j| serde_json::from_str(&j).ok()),
        relationships: relationships_json.and_then(|j| serde_json::from_str(&j).ok()),
        concept_labels: labels_json.and_then(|j| serde_json::from_str(&j).ok()),
        extracted_at: row.get(8).unwrap_or_default(),
        model: row.get(9).unwrap_or_default(),
        tokens_input: row.get(10).unwrap_or(None),
        tokens_output: row.get(11).unwrap_or(None),
    }
}

pub(crate) fn normalize_summary_file_key(path: &Path) -> String {
    normalize_summary_file_key_str(path.to_string_lossy().as_ref())
}

pub(crate) fn normalize_summary_file_key_str(path: &str) -> String {
    path.replace('\\', "/")
}

fn legacy_windows_summary_file_key(path: &str) -> String {
    path.replace('/', "\\")
}

pub fn content_hash(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

pub fn extract_for_file(
    file_path: &Path,
    symbols_db_path: Option<&Path>,
    symbols_source_root: Option<&Path>,
    config: &SummarizeConfig,
) -> Result<Vec<Summary>> {
    let source = std::fs::read_to_string(file_path)
        .with_context(|| format!("reading {}", file_path.display()))?;

    let token_estimate = source.len() / 4;
    if token_estimate > config.max_file_tokens {
        bail!(
            "file {} exceeds max_file_tokens ({} > {})",
            file_path.display(),
            token_estimate,
            config.max_file_tokens
        );
    }

    let hash = content_hash(source.as_bytes());
    let file_str = file_path.to_string_lossy().to_string();

    let symbols = if let Some(db_path) = symbols_db_path {
        load_symbols_for_file(db_path, file_path, symbols_source_root)?
    } else {
        Vec::new()
    };

    let api_key = std::env::var(&config.api_key_env).with_context(|| {
        format!(
            "missing API key: set {} environment variable",
            config.api_key_env
        )
    })?;

    let prompt = build_extraction_prompt(&file_str, &source, &symbols);

    let (response_text, tokens_in, tokens_out) =
        call_anthropic_api(&api_key, &config.model, &prompt)?;

    let parsed: ExtractionResponse = serde_json::from_str(&response_text)
        .with_context(|| format!("parsing extraction response for {}", file_path.display()))?;

    let now = chrono_now();
    let mut summaries = Vec::new();

    // File-level summary (symbol_name = filename)
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_str.clone());
    summaries.push(Summary {
        id: 0,
        symbol_name: file_name,
        file_path: file_str.clone(),
        content_hash: hash.clone(),
        summary: parsed.summary.clone(),
        entities: Some(parsed.entities.clone()),
        relationships: Some(parsed.relationships.clone()),
        concept_labels: Some(parsed.concept_labels.clone()),
        extracted_at: now.clone(),
        model: config.model.clone(),
        tokens_input: Some(tokens_in),
        tokens_output: Some(tokens_out),
    });

    // Per-entity summaries
    for entity in &parsed.entities {
        summaries.push(Summary {
            id: 0,
            symbol_name: entity.name.clone(),
            file_path: file_str.clone(),
            content_hash: hash.clone(),
            summary: entity.description.clone(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: now.clone(),
            model: config.model.clone(),
            tokens_input: None,
            tokens_output: None,
        });
    }

    Ok(summaries)
}

fn normalize_lookup_path(path: &Path) -> String {
    normalize_summary_file_key(path)
}

pub(crate) fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normalized.push(component.as_os_str()),
            },
            _ => normalized.push(component.as_os_str()),
        }
    }

    if normalized.as_os_str().is_empty() && !path.is_absolute() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn push_lookup_candidate(candidates: &mut Vec<String>, candidate: String) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

pub fn file_lookup_candidates(
    file_query: &Path,
    query_base: &Path,
    project_root: &Path,
) -> Vec<String> {
    let mut candidates = Vec::new();
    push_lookup_candidate(
        &mut candidates,
        normalize_lookup_path(&normalize_lexical_path(file_query)),
    );

    let resolved = if file_query.is_absolute() {
        file_query
            .canonicalize()
            .unwrap_or_else(|_| normalize_lexical_path(file_query))
    } else {
        normalize_lexical_path(&query_base.join(file_query))
    };
    let project_relative = resolved.strip_prefix(project_root).unwrap_or(&resolved);
    push_lookup_candidate(&mut candidates, normalize_lookup_path(project_relative));

    candidates
}

fn symbol_lookup_candidates(file_path: &Path, source_root: Option<&Path>) -> Vec<String> {
    let mut candidates = vec![normalize_lookup_path(file_path)];
    if let Some(root) = source_root
        && let Ok(relative) = file_path.strip_prefix(root)
    {
        let relative = normalize_lookup_path(relative);
        if !candidates.iter().any(|candidate| candidate == &relative) {
            candidates.push(relative);
        }
    }
    candidates
}

fn load_symbols_for_file(
    db_path: &Path,
    file_path: &Path,
    source_root: Option<&Path>,
) -> Result<Vec<(String, String)>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let candidates = symbol_lookup_candidates(file_path, source_root);
    IndexDb::file_symbols_read_only(db_path, &candidates)
}

fn build_extraction_prompt(file_path: &str, source: &str, symbols: &[(String, String)]) -> String {
    let mut prompt = format!(
        "Analyze this source file and extract structured information.\n\n\
         File: {}\n",
        file_path
    );

    if !symbols.is_empty() {
        prompt.push_str("\nKnown symbols:\n");
        for (name, kind) in symbols {
            prompt.push_str(&format!("- {} ({})\n", name, kind));
        }
    }

    prompt.push_str(&format!(
        "\nSource:\n```\n{}\n```\n\n\
         Respond with ONLY a JSON object (no markdown fences):\n\
         {{\n\
           \"summary\": \"1-3 sentence description of the file/module purpose\",\n\
           \"entities\": [{{\"name\": \"...\", \"kind\": \"function|class|type|trait|module\", \"description\": \"1 sentence\"}}],\n\
           \"relationships\": [{{\"from\": \"...\", \"to\": \"...\", \"kind\": \"calls|implements|uses|extends\"}}],\n\
           \"concept_labels\": [\"domain concept 1\", \"domain concept 2\"]\n\
         }}",
        source
    ));

    prompt
}

fn parse_anthropic_api_response(
    status: u16,
    response: serde_json::Value,
) -> Result<(String, i64, i64)> {
    if !(200..300).contains(&status) {
        let message = response["error"]["message"]
            .as_str()
            .or_else(|| response["message"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| response.to_string());
        let error_type = response["error"]["type"].as_str();

        match error_type {
            Some(error_type) => bail!(
                "Anthropic API returned HTTP {} ({}): {}",
                status,
                error_type,
                message
            ),
            None => bail!("Anthropic API returned HTTP {}: {}", status, message),
        }
    }

    let content = response["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or("")
        .to_string();

    let tokens_in = response["usage"]["input_tokens"].as_i64().unwrap_or(0);
    let tokens_out = response["usage"]["output_tokens"].as_i64().unwrap_or(0);

    if content.is_empty() {
        bail!("empty response from Anthropic API");
    }

    // Strip markdown code fences if the model wrapped the response
    let cleaned = content
        .trim()
        .strip_prefix("```json")
        .or_else(|| content.trim().strip_prefix("```"))
        .unwrap_or(content.trim());
    let cleaned = cleaned
        .strip_suffix("```")
        .unwrap_or(cleaned)
        .trim()
        .to_string();

    Ok((cleaned, tokens_in, tokens_out))
}

fn call_anthropic_api(api_key: &str, model: &str, prompt: &str) -> Result<(String, i64, i64)> {
    if let Some(result) = maybe_mock_anthropic_api(prompt)? {
        return Ok(result);
    }

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 4096,
        "messages": [
            {"role": "user", "content": prompt}
        ]
    });

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send_json(&body)
        .with_context(|| "calling Anthropic API")?;
    let status = response.status();
    let response_body = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading Anthropic API response body (HTTP {})", status))?;
    let response_json: serde_json::Value = serde_json::from_str(&response_body)
        .with_context(|| format!("parsing Anthropic API response JSON (HTTP {})", status))?;

    parse_anthropic_api_response(status.as_u16(), response_json)
}

fn maybe_mock_anthropic_api(prompt: &str) -> Result<Option<(String, i64, i64)>> {
    if let Ok(capture_path) = std::env::var("TSIFT_TEST_ANTHROPIC_CAPTURE_PROMPT") {
        std::fs::write(&capture_path, prompt)
            .with_context(|| format!("writing prompt capture: {capture_path}"))?;
    }

    let Ok(response) = std::env::var("TSIFT_TEST_ANTHROPIC_RESPONSE_JSON") else {
        return Ok(None);
    };
    Ok(Some((response, 0, 0)))
}

pub fn git_changed_files(root: &Path) -> Result<GitChangedFiles> {
    let (tracked, deleted) = if git_has_head_commit(root)? {
        git_diff_changed_files(root)?
    } else {
        (Vec::new(), Vec::new())
    };
    let untracked = git_list_paths(
        root,
        &["ls-files", "--others", "--exclude-standard"],
        "git ls-files",
    )?;
    let existing = tracked
        .into_iter()
        .chain(untracked)
        .filter(|path| path.is_file())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let deleted = deleted
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(GitChangedFiles { existing, deleted })
}

fn git_diff_changed_files(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-status", "--find-renames", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| "running git diff --name-status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff --name-status failed: {}", stderr.trim());
    }

    let mut tracked = Vec::new();
    let mut deleted = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or_default();
        match status.chars().next() {
            Some('D') => {
                let path = fields
                    .next()
                    .with_context(|| format!("parsing deleted git diff path: {line}"))?;
                deleted.push(root.join(path));
            }
            Some('R') => {
                let old_path = fields
                    .next()
                    .with_context(|| format!("parsing renamed git diff old path: {line}"))?;
                let new_path = fields
                    .next()
                    .with_context(|| format!("parsing renamed git diff new path: {line}"))?;
                deleted.push(root.join(old_path));
                tracked.push(root.join(new_path));
            }
            Some(_) => {
                let path = fields
                    .next_back()
                    .or_else(|| fields.next())
                    .with_context(|| format!("parsing changed git diff path: {line}"))?;
                tracked.push(root.join(path));
            }
            None => {}
        }
    }

    Ok((tracked, deleted))
}

fn git_has_head_commit(root: &Path) -> Result<bool> {
    let inside_work_tree = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .with_context(|| "running git rev-parse --is-inside-work-tree")?;

    if !inside_work_tree.status.success() {
        let stderr = String::from_utf8_lossy(&inside_work_tree.stderr);
        bail!(
            "git rev-parse --is-inside-work-tree failed: {}",
            stderr.trim()
        );
    }

    let verify_head = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| "running git rev-parse --verify HEAD")?;

    Ok(verify_head.status.success())
}

fn git_list_paths(root: &Path, args: &[&str], label: &str) -> Result<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running {label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{label} failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| root.join(line))
        .collect())
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-ish timestamp without chrono dependency
    format!("{}", now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::rollback_journal_path;
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::NamedTempFile;

    fn test_db() -> (NamedTempFile, SummaryDb) {
        let tmp = NamedTempFile::new().unwrap();
        let db = SummaryDb::open(tmp.path()).unwrap();
        (tmp, db)
    }

    fn make_summary(symbol: &str, file: &str, hash: &str) -> Summary {
        Summary {
            id: 0,
            symbol_name: symbol.to_string(),
            file_path: file.to_string(),
            content_hash: hash.to_string(),
            summary: format!("Summary for {}", symbol),
            entities: Some(vec![Entity {
                name: "helper".to_string(),
                kind: "function".to_string(),
                description: "A helper function".to_string(),
            }]),
            relationships: Some(vec![Relationship {
                from: "main".to_string(),
                to: "helper".to_string(),
                kind: "calls".to_string(),
            }]),
            concept_labels: Some(vec!["cli".to_string(), "parsing".to_string()]),
            extracted_at: "1700000000".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            tokens_input: Some(500),
            tokens_output: Some(200),
        }
    }

    #[test]
    fn db_create_and_insert() {
        let (_tmp, db) = test_db();
        let s = make_summary("main", "src/main.rs", "abc123");
        db.insert(&s).unwrap();
        let results = db.get_by_symbol("main").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_name, "main");
        assert_eq!(results[0].summary, "Summary for main");
    }

    #[test]
    fn db_get_by_file() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("fn_a", "src/lib.rs", "hash1"))
            .unwrap();
        db.insert(&make_summary("fn_b", "src/lib.rs", "hash1"))
            .unwrap();
        db.insert(&make_summary("fn_c", "src/other.rs", "hash2"))
            .unwrap();
        let results = db.get_by_file("src/lib.rs").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn db_get_by_file_normalizes_legacy_windows_separator_rows() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("fn_a", r"src\lib.rs", "hash1"))
            .unwrap();

        let results = db.get_by_file("src/lib.rs").unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/lib.rs");
    }

    #[test]
    fn replace_file_reaps_legacy_windows_separator_rows() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("stale", r"src\lib.rs", "hash1"))
            .unwrap();

        db.replace_file(
            "src/lib.rs",
            &[make_summary("fresh", "src/lib.rs", "hash2")],
        )
        .unwrap();

        let results = db.get_by_file("src/lib.rs").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_name, "fresh");
        assert_eq!(results[0].file_path, "src/lib.rs");
    }

    #[test]
    fn file_lookup_candidates_normalize_dot_prefixed_root_relative_query() {
        let candidates = file_lookup_candidates(
            Path::new("./src/lib.rs"),
            Path::new("/repo"),
            Path::new("/repo"),
        );

        assert_eq!(candidates, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn file_lookup_candidates_include_anchor_relative_project_key() {
        let candidates = file_lookup_candidates(
            Path::new("../lib.rs"),
            Path::new("/repo/src/nested"),
            Path::new("/repo"),
        );

        assert_eq!(
            candidates,
            vec!["../lib.rs".to_string(), "src/lib.rs".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_lookup_candidates_canonicalize_absolute_symlink_queries() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        std::fs::create_dir_all(real_root.join("src")).unwrap();
        std::fs::write(real_root.join("src/lib.rs"), "fn alpha_helper() {}\n").unwrap();
        let link_root = dir.path().join("link");
        symlink(&real_root, &link_root).unwrap();

        let candidates =
            file_lookup_candidates(&link_root.join("src/lib.rs"), &real_root, &real_root);

        assert_eq!(
            candidates,
            vec![
                link_root
                    .join("src/lib.rs")
                    .to_string_lossy()
                    .replace('\\', "/"),
                "src/lib.rs".to_string()
            ]
        );
    }

    #[test]
    fn db_is_current() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("main", "src/main.rs", "hash_v1"))
            .unwrap();
        assert!(db.is_current("src/main.rs", "hash_v1").unwrap());
        assert!(!db.is_current("src/main.rs", "hash_v2").unwrap());
    }

    #[test]
    fn db_stats() {
        let root = tempfile::tempdir().unwrap();
        let f1 = b"fn a() {}\n";
        let f2 = b"fn c() {}\n";
        std::fs::write(root.path().join("f1.rs"), f1).unwrap();
        std::fs::write(root.path().join("f2.rs"), f2).unwrap();
        let (_tmp, db) = test_db();
        let f1_hash = content_hash(f1);
        let f2_hash = content_hash(f2);
        db.insert(&make_summary("a", "f1.rs", &f1_hash)).unwrap();
        db.insert(&make_summary("b", "f1.rs", &f1_hash)).unwrap();
        db.insert(&make_summary("c", "f2.rs", &f2_hash)).unwrap();
        let stats = db.stats(root.path()).unwrap();
        assert_eq!(stats.total_summaries, 3);
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.stale_count, 0);
        assert_eq!(stats.total_tokens_input, 1500); // 3 * 500
        assert_eq!(stats.total_tokens_output, 600); // 3 * 200
    }

    #[test]
    fn db_stats_counts_missing_and_hash_mismatched_files_as_stale() {
        let root = tempfile::tempdir().unwrap();
        let fresh = b"fn fresh() {}\n";
        let changed_current = b"fn changed() { new_impl(); }\n";
        let changed_old = b"fn changed() { old_impl(); }\n";
        std::fs::write(root.path().join("fresh.rs"), fresh).unwrap();
        std::fs::write(root.path().join("changed.rs"), changed_current).unwrap();

        let (_tmp, db) = test_db();
        db.insert(&make_summary("fresh", "fresh.rs", &content_hash(fresh)))
            .unwrap();
        db.insert(&make_summary(
            "changed",
            "changed.rs",
            &content_hash(changed_old),
        ))
        .unwrap();
        db.insert(&make_summary("missing", "missing.rs", "stale-hash"))
            .unwrap();

        let stats = db.stats(root.path()).unwrap();

        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.stale_count, 2);
    }

    #[test]
    fn db_cached_file_paths() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("a", "f1.rs", "h1")).unwrap();
        db.insert(&make_summary("b", "f1.rs", "h1")).unwrap();
        db.insert(&make_summary("c", "f2.rs", "h2")).unwrap();

        let paths = db.cached_file_paths().unwrap();

        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            vec!["f1.rs".to_string(), "f2.rs".to_string()]
        );
    }

    #[test]
    fn stats_live_path_rejects_paths_outside_root() {
        let root = Path::new("/tmp/project");

        assert_eq!(
            SummaryDb::stats_live_path(root, "src/lib.rs").unwrap(),
            PathBuf::from("/tmp/project/src/lib.rs")
        );
        assert_eq!(
            SummaryDb::stats_live_path(root, "src/../src/lib.rs").unwrap(),
            PathBuf::from("/tmp/project/src/lib.rs")
        );
        assert!(SummaryDb::stats_live_path(root, "../secret.rs").is_none());
        assert!(SummaryDb::stats_live_path(root, "/etc/passwd").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn stats_marks_unreadable_files_stale_with_warning() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("src/lib.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        let source = b"fn alpha_helper() {}\n";
        std::fs::write(&file_path, source).unwrap();

        let (_tmp, db) = test_db();
        db.insert(&make_summary(
            "alpha_helper",
            "src/lib.rs",
            &content_hash(source),
        ))
        .unwrap();

        let metadata = std::fs::metadata(&file_path).unwrap();
        let original_mode = metadata.permissions().mode();
        let mut unreadable = metadata.permissions();
        unreadable.set_mode(0o000);
        std::fs::set_permissions(&file_path, unreadable).unwrap();

        let stats = db.stats(root.path()).unwrap();

        let mut restored = std::fs::metadata(&file_path).unwrap().permissions();
        restored.set_mode(original_mode);
        std::fs::set_permissions(&file_path, restored).unwrap();

        assert_eq!(stats.stale_count, 1);
        assert_eq!(stats.warnings.len(), 1);
        assert_eq!(stats.warnings[0].path, PathBuf::from("src/lib.rs"));
        assert!(
            stats.warnings[0]
                .message
                .contains("counting cached summary as stale"),
            "warning was: {}",
            stats.warnings[0].message
        );
    }

    #[test]
    fn db_delete_by_file() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("a", "f1.rs", "h1")).unwrap();
        db.insert(&make_summary("b", "f1.rs", "h1")).unwrap();
        db.insert(&make_summary("c", "f2.rs", "h2")).unwrap();
        let deleted = db.delete_by_file("f1.rs").unwrap();
        assert_eq!(deleted, 2);
        assert!(db.get_by_file("f1.rs").unwrap().is_empty());
        assert_eq!(db.get_by_file("f2.rs").unwrap().len(), 1);
    }

    #[test]
    fn db_replace_file_rolls_back_on_failure() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("alpha", "f1.rs", "old_hash"))
            .unwrap();
        db.insert(&make_summary("beta", "f1.rs", "old_hash"))
            .unwrap();

        let replacements = vec![
            make_summary("gamma", "f1.rs", "new_hash"),
            make_summary("delta", "f1.rs", "new_hash"),
        ];

        let err = db
            .replace_file_with_hook("f1.rs", &replacements, |idx| {
                if idx == 0 {
                    bail!("injected summary replace failure");
                }
                Ok(())
            })
            .unwrap_err();
        assert!(err.to_string().contains("injected summary replace failure"));

        let remaining = db.get_by_file("f1.rs").unwrap();
        let remaining_symbols = remaining
            .iter()
            .map(|summary| summary.symbol_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(remaining_symbols, vec!["alpha", "beta"]);
        assert!(
            remaining
                .iter()
                .all(|summary| summary.content_hash == "old_hash")
        );
    }

    #[test]
    fn db_open_configures_sqlite_for_concurrent_access() {
        let (_tmp, db) = test_db();

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
    fn db_open_read_only_uses_busy_timeout() {
        let (tmp, _db) = test_db();
        let db = SummaryDb::open_read_only(tmp.path()).unwrap();
        let timeout_ms: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(timeout_ms, 5000);
    }

    #[test]
    fn summary_write_lock_records_pid_and_clears_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/summaries.db");
        let lock_path = writer_lock_path(&db_path);

        {
            let _lock = acquire_write_lock(&db_path).unwrap();
            let marker = std::fs::read_to_string(&lock_path).unwrap();
            assert_eq!(marker.trim(), std::process::id().to_string());
        }

        let marker = std::fs::read_to_string(&lock_path).unwrap();
        assert!(marker.trim().is_empty());
        acquire_write_lock(&db_path).unwrap();
    }

    #[test]
    fn summary_write_lock_fails_fast_when_live() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/summaries.db");
        let _lock = acquire_write_lock(&db_path).unwrap();

        let err = acquire_write_lock(&db_path).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("another tsift summarize extractor is already active"));
        assert!(message.contains("tsift summarize --extract"));
        assert!(message.contains(&writer_lock_path(&db_path).display().to_string()));
    }

    #[test]
    fn db_entities_roundtrip() {
        let (_tmp, db) = test_db();
        let s = make_summary("main", "src/main.rs", "abc");
        db.insert(&s).unwrap();
        let results = db.get_by_symbol("main").unwrap();
        let entities = results[0].entities.as_ref().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "helper");
        let rels = results[0].relationships.as_ref().unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].from, "main");
        assert_eq!(rels[0].to, "helper");
        let labels = results[0].concept_labels.as_ref().unwrap();
        assert_eq!(labels, &["cli", "parsing"]);
    }

    #[test]
    fn db_no_results_returns_empty() {
        let (_tmp, db) = test_db();
        assert!(db.get_by_symbol("nonexistent").unwrap().is_empty());
        assert!(db.get_by_file("no/such/file.rs").unwrap().is_empty());
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2);
        let h3 = content_hash(b"hello world!");
        assert_ne!(h1, h3);
    }

    #[test]
    fn content_hash_is_blake3() {
        let h = content_hash(b"test");
        assert_eq!(h.len(), 64); // blake3 hex is 64 chars
    }

    #[test]
    fn build_prompt_includes_file_and_source() {
        let prompt = build_extraction_prompt("src/lib.rs", "fn main() {}", &[]);
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("fn main() {}"));
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn build_prompt_includes_symbols() {
        let symbols = vec![
            ("main".to_string(), "function".to_string()),
            ("Config".to_string(), "struct".to_string()),
        ];
        let prompt = build_extraction_prompt("src/lib.rs", "code", &symbols);
        assert!(prompt.contains("- main (function)"));
        assert!(prompt.contains("- Config (struct)"));
    }

    #[test]
    fn anthropic_api_response_rejects_http_errors() {
        let err = parse_anthropic_api_response(
            429,
            json!({
                "error": {
                    "type": "rate_limit_error",
                    "message": "too many requests"
                }
            }),
        )
        .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("HTTP 429"));
        assert!(message.contains("rate_limit_error"));
        assert!(message.contains("too many requests"));
    }

    #[test]
    fn anthropic_api_response_reports_raw_body_when_error_message_missing() {
        let response = json!({"unexpected": "shape"});
        let err = parse_anthropic_api_response(502, response.clone()).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("HTTP 502"));
        assert!(message.contains(&response.to_string()));
    }

    #[test]
    fn anthropic_api_response_extracts_content_and_usage() {
        let (content, tokens_in, tokens_out) = parse_anthropic_api_response(
            200,
            json!({
                "content": [
                    {
                        "text": "```json\n{\"summary\":\"ok\"}\n```"
                    }
                ],
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 34
                }
            }),
        )
        .unwrap();

        assert_eq!(content, "{\"summary\":\"ok\"}");
        assert_eq!(tokens_in, 12);
        assert_eq!(tokens_out, 34);
    }

    #[test]
    fn extract_skips_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let big_file = dir.path().join("big.rs");
        std::fs::write(&big_file, "x".repeat(100_000)).unwrap();
        let config = SummarizeConfig {
            max_file_tokens: 8000,
            ..Default::default()
        };
        let result = extract_for_file(&big_file, None, None, &config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds max_file_tokens")
        );
    }

    #[test]
    fn extract_requires_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("small.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let config = SummarizeConfig {
            api_key_env: "TSIFT_TEST_NONEXISTENT_KEY".to_string(),
            ..Default::default()
        };
        let result = extract_for_file(&file, None, None, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing API key"));
    }

    #[test]
    fn load_symbols_for_file_uses_exact_relative_match() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
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
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility, tags)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, NULL, NULL, NULL, NULL)",
            rusqlite::params!["target", "function", "rust", "src/lib.rs", 1_i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility, tags)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, NULL, NULL, NULL, NULL)",
            rusqlite::params!["wrong", "function", "rust", "nested/src/lib.rs", 1_i64],
        )
        .unwrap();

        let file_path = Path::new("/workspace/src/lib.rs");
        let symbols =
            load_symbols_for_file(&db_path, file_path, Some(Path::new("/workspace"))).unwrap();

        assert_eq!(
            symbols,
            vec![("target".to_string(), "function".to_string())]
        );
    }

    #[test]
    fn load_symbols_for_file_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE symbols (
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
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, language, signature, file, line, end_line, parent_module, visibility, tags)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, NULL, NULL, NULL, NULL)",
            rusqlite::params!["target", "function", "rust", "src/lib.rs", 1_i64],
        )
        .unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        std::fs::write(rollback_journal_path(&db_path), "locked").unwrap();

        let file_path = Path::new("/workspace/src/lib.rs");
        let symbols =
            load_symbols_for_file(&db_path, file_path, Some(Path::new("/workspace"))).unwrap();

        assert_eq!(
            symbols,
            vec![("target".to_string(), "function".to_string())]
        );
    }

    #[test]
    fn summary_read_only_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("summaries.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE summaries (
                 id INTEGER PRIMARY KEY,
                 symbol_name TEXT NOT NULL,
                 file_path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 entities TEXT,
                 relationships TEXT,
                 concept_labels TEXT,
                 extracted_at TEXT NOT NULL,
                 model TEXT NOT NULL,
                 tokens_input INTEGER,
                 tokens_output INTEGER
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO summaries
             (symbol_name, file_path, content_hash, summary, entities, relationships, concept_labels, extracted_at, model, tokens_input, tokens_output)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6, NULL, NULL)",
            rusqlite::params![
                "main",
                "src/main.rs",
                "hash1",
                "cached summary",
                "1700000000",
                "test-model",
            ],
        )
        .unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        std::fs::write(rollback_journal_path(&db_path), "locked").unwrap();

        let opened = SummaryDb::open_read_only_with_recovery(&db_path).unwrap();

        assert_eq!(
            opened.recovery,
            Some(crate::index::ReadOnlyRecovery::SnapshotFallback)
        );
        let results = opened.db.get_by_symbol("main").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "cached summary");
    }

    #[test]
    fn db_insert_replaces_on_conflict() {
        let (_tmp, db) = test_db();
        let mut s = make_summary("main", "src/main.rs", "v1");
        s.summary = "version 1".to_string();
        db.insert(&s).unwrap();

        let mut s2 = make_summary("main", "src/main.rs", "v2");
        s2.summary = "version 2".to_string();
        db.insert(&s2).unwrap();

        let results = db.get_by_symbol("main").unwrap();
        assert_eq!(results.len(), 2);
    }
}
