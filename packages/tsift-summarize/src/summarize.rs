use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use lazily::{Computed, Context as LazyContext, Source};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;
use tsift_index::index::IndexDb;
use tsift_sqlite::{ReadOnlyRecovery, copy_read_only_snapshot, read_only_snapshot_recovery};

pub struct SummaryDb {
    conn: Connection,
    _snapshot_copy: Option<SnapshotCopyGuard>,
}

pub struct SummaryReadOnlyOpen {
    pub db: SummaryDb,
    pub recovery: Option<ReadOnlyRecovery>,
}

type CachedSummaryFileSnapshot = std::result::Result<SummaryFileSnapshot, String>;

#[derive(Debug, Clone)]
pub struct SummaryFileSnapshot {
    pub file_path: String,
    pub requested_content_hash: Option<String>,
    pub summaries: Vec<Summary>,
    pub current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryCacheSource {
    Cached,
    Extracted,
}

#[derive(Debug, Clone)]
pub struct SummaryCacheLookup {
    pub summaries: Vec<Summary>,
    pub source: SummaryCacheSource,
}

#[derive(Clone, Copy)]
struct SummaryFileSlot {
    content_hash: Source<Option<String>>,
    epoch: Source<u64>,
    snapshot: Computed<CachedSummaryFileSnapshot>,
}

pub struct SummaryCache {
    db: Rc<SummaryDb>,
    ctx: LazyContext,
    slots: RefCell<HashMap<String, SummaryFileSlot>>,
    hits: Cell<usize>,
    misses: Cell<usize>,
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

#[derive(Debug, Deserialize)]
struct ClaudeCliResponse {
    result: String,
    usage: ClaudeCliUsage,
}

#[derive(Debug, Deserialize)]
struct ClaudeCliUsage {
    input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    output_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct ExtractionReport {
    pub files_processed: usize,
    pub symbols_extracted: usize,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub terminal_failures_skipped: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionFailureKind {
    TooLarge,
    UnparseableResponse,
}

impl ExtractionFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::UnparseableResponse => "unparseable_response",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedExtractionFailure {
    pub file_path: String,
    pub content_hash: String,
    pub kind: ExtractionFailureKind,
    pub message: String,
    pub failed_at: String,
}

#[derive(Debug)]
struct TerminalExtractionError {
    kind: ExtractionFailureKind,
    message: String,
}

impl std::fmt::Display for TerminalExtractionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TerminalExtractionError {}

pub fn terminal_extraction_failure(
    error: &anyhow::Error,
) -> Option<(ExtractionFailureKind, String)> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<TerminalExtractionError>()
            .map(|terminal| (terminal.kind, terminal.message.clone()))
    })
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

pub struct ExtractionClient {
    model: String,
    backend: ExtractionBackend,
}

enum ExtractionBackend {
    AnthropicApi { api_key: String },
    ClaudeCli { command: PathBuf },
}

const REPLACE_FILE_SAVEPOINT: &str = "tsift_summary_replace";

#[derive(Debug)]
pub struct SummaryWriteLockGuard {
    file: File,
}

#[derive(Debug)]
struct SnapshotCopyGuard {
    paths: Vec<PathBuf>,
}

impl Drop for SummaryWriteLockGuard {
    fn drop(&mut self) {
        let _ = clear_lock_metadata(&mut self.file);
        let _ = self.file.unlock();
    }
}

impl Drop for SnapshotCopyGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
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

pub fn is_extraction_candidate_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "rs" | "py"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "kt"
                | "kts"
                | "zig"
                | "gd"
                | "sh"
                | "bash"
                | "zsh"
        )
    )
}

impl ExtractionClient {
    pub fn resolve(config: &SummarizeConfig) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let claude_command = find_command_on_path("claude");
        let prefer_claude = [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ]
        .into_iter()
        .any(env_flag_enabled);
        let backend = select_extraction_backend(api_key, claude_command, prefer_claude)
            .with_context(|| {
                format!(
                    "tsift summarize --extract: no LLM credentials found. Set {}, or install and authenticate Claude Code so `claude -p` can use the host's direct, Bedrock, Vertex, or Foundry credentials",
                    config.api_key_env
                )
            })?;
        if let ExtractionBackend::ClaudeCli { command } = &backend {
            ensure_claude_cli_authenticated(command).with_context(|| {
                format!(
                    "tsift summarize --extract: Claude Code CLI at {} is not a usable extraction backend; run `claude auth login` or configure the selected hosted provider",
                    command.display()
                )
            })?;
        }
        Ok(Self {
            model: config.model.clone(),
            backend,
        })
    }

    fn complete(&self, prompt: &str) -> Result<(String, i64, i64)> {
        match &self.backend {
            ExtractionBackend::AnthropicApi { api_key } => {
                call_anthropic_api(api_key, &self.model, prompt)
            }
            ExtractionBackend::ClaudeCli { command } => {
                call_claude_cli(command, &self.model, prompt)
            }
        }
    }
}

fn select_extraction_backend(
    api_key: Option<String>,
    claude_command: Option<PathBuf>,
    prefer_claude: bool,
) -> Result<ExtractionBackend> {
    if prefer_claude && let Some(command) = claude_command.as_ref() {
        return Ok(ExtractionBackend::ClaudeCli {
            command: command.clone(),
        });
    }
    if let Some(api_key) = api_key {
        return Ok(ExtractionBackend::AnthropicApi { api_key });
    }
    if let Some(command) = claude_command {
        return Ok(ExtractionBackend::ClaudeCli { command });
    }
    bail!("no Anthropic API key or authenticated Claude Code CLI is available")
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn find_command_on_path(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find_map(|candidate| executable_candidate(&candidate))
}

fn executable_candidate(candidate: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .map(|_| candidate.to_path_buf())
    }

    #[cfg(windows)]
    {
        if candidate.is_file() {
            return Some(candidate.to_path_buf());
        }
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| candidate.with_extension(extension))
            .find(|path| path.is_file())
    }

    #[cfg(not(any(unix, windows)))]
    {
        candidate.is_file().then(|| candidate.to_path_buf())
    }
}

fn ensure_claude_cli_authenticated(command: &Path) -> Result<()> {
    let output = Command::new(command)
        .args(["auth", "status"])
        .output()
        .with_context(|| format!("running `{} auth status`", command.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "`{} auth status` failed with {}: {}",
        command.display(),
        output.status,
        stderr.trim()
    )
}

pub fn acquire_write_lock(db_path: &Path) -> Result<SummaryWriteLockGuard> {
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

pub fn writer_lock_path(db_path: &Path) -> PathBuf {
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
CREATE INDEX IF NOT EXISTS idx_summaries_hash ON summaries(content_hash);
CREATE TABLE IF NOT EXISTS extraction_failures (
    file_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    failed_at TEXT NOT NULL,
    PRIMARY KEY (file_path, content_hash)
);
CREATE INDEX IF NOT EXISTS idx_extraction_failures_file
    ON extraction_failures(file_path);",
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
            Err(err) => {
                let Some(recovery) = read_only_snapshot_recovery(path, &err) else {
                    return Err(err);
                };
                let db = Self::open_read_only_snapshot(path)?;
                Ok(SummaryReadOnlyOpen {
                    db,
                    recovery: Some(recovery),
                })
            }
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

    pub fn all(&self) -> Result<Vec<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol_name, file_path, content_hash, summary, entities, relationships,
                    concept_labels, extracted_at, model, tokens_input, tokens_output
             FROM summaries ORDER BY file_path, symbol_name, id",
        )?;
        let rows = stmt
            .query_map([], |row| Ok(row_to_summary(row)))?
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
        let total_summaries_raw: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))?;
        let total_summaries =
            usize::try_from(total_summaries_raw).context("summary count out of range")?;
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
        if self.has_extraction_failures_table()? {
            self.conn.execute(
                "DELETE FROM extraction_failures WHERE file_path = ?1 OR file_path = ?2",
                rusqlite::params![normalized, legacy],
            )?;
        }
        Ok(count)
    }

    pub fn terminal_failure(
        &self,
        file_path: &str,
        content_hash: &str,
    ) -> Result<Option<CachedExtractionFailure>> {
        if !self.has_extraction_failures_table()? {
            return Ok(None);
        }
        let normalized = normalize_summary_file_key_str(file_path);
        let legacy = legacy_windows_summary_file_key(&normalized);
        let mut stmt = self.conn.prepare(
            "SELECT file_path, content_hash, kind, message, failed_at
             FROM extraction_failures
             WHERE content_hash = ?2 AND (file_path = ?1 OR file_path = ?3)
             LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![normalized, content_hash, legacy])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let kind = match row.get::<_, String>(2)?.as_str() {
            "too_large" => ExtractionFailureKind::TooLarge,
            "unparseable_response" => ExtractionFailureKind::UnparseableResponse,
            _ => return Ok(None),
        };
        Ok(Some(CachedExtractionFailure {
            file_path: normalize_summary_file_key_str(&row.get::<_, String>(0)?),
            content_hash: row.get(1)?,
            kind,
            message: row.get(3)?,
            failed_at: row.get(4)?,
        }))
    }

    pub fn record_terminal_failure(
        &self,
        file_path: &str,
        content_hash: &str,
        kind: ExtractionFailureKind,
        message: &str,
    ) -> Result<()> {
        let normalized = normalize_summary_file_key_str(file_path);
        self.conn.execute(
            "INSERT INTO extraction_failures
                 (file_path, content_hash, kind, message, failed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_path, content_hash) DO UPDATE SET
                 kind = excluded.kind,
                 message = excluded.message,
                 failed_at = excluded.failed_at",
            rusqlite::params![
                normalized,
                content_hash,
                kind.as_str(),
                message,
                chrono_now()
            ],
        )?;
        Ok(())
    }

    pub fn current_terminal_failure_paths(&self, root: &Path) -> Result<BTreeSet<String>> {
        if !self.has_extraction_failures_table()? {
            return Ok(BTreeSet::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT file_path, content_hash FROM extraction_failures ORDER BY file_path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut current = BTreeSet::new();
        for row in rows {
            let (file_path, expected_hash) = row?;
            let normalized = normalize_summary_file_key_str(&file_path);
            let Some(live_path) = Self::stats_live_path(root, &normalized) else {
                continue;
            };
            let Ok(content) = std::fs::read(live_path) else {
                continue;
            };
            if content_hash(&content) == expected_hash {
                current.insert(normalized);
            }
        }
        Ok(current)
    }

    fn has_extraction_failures_table(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'extraction_failures'",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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
            self.conn.execute(
                "DELETE FROM extraction_failures WHERE file_path = ?1 OR file_path = ?2",
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
        let (snapshot_path, cleanup_paths) = copy_read_only_snapshot(path, "summaries")?;
        let conn = Connection::open_with_flags(
            &snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening summaries snapshot {}", snapshot_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn,
            _snapshot_copy: Some(SnapshotCopyGuard {
                paths: cleanup_paths,
            }),
        })
    }
}

impl SummaryCache {
    pub fn new(db: SummaryDb) -> Self {
        Self {
            db: Rc::new(db),
            ctx: LazyContext::new(),
            slots: RefCell::new(HashMap::new()),
            hits: Cell::new(0),
            misses: Cell::new(0),
        }
    }

    pub fn db(&self) -> &SummaryDb {
        &self.db
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.hits.get(), self.misses.get())
    }

    pub fn file_snapshot(
        &self,
        file_path: &str,
        content_hash: Option<&str>,
    ) -> Result<SummaryFileSnapshot> {
        let normalized = normalize_summary_file_key_str(file_path);
        let requested_content_hash = content_hash.map(str::to_string);
        let slot = {
            let mut slots = self.slots.borrow_mut();
            if let Some(slot) = slots.get(&normalized) {
                self.ctx
                    .set(&slot.content_hash, requested_content_hash.clone());
                *slot
            } else {
                let db = Rc::clone(&self.db);
                let file_key = normalized.clone();
                let content_hash_cell = self.ctx.source(requested_content_hash.clone());
                let epoch = self.ctx.source(0u64);
                let snapshot = self.ctx.slot(move |ctx| {
                    let requested_content_hash = ctx.get(&content_hash_cell);
                    let _epoch = ctx.get(&epoch);
                    let summaries = db
                        .get_by_file(&file_key)
                        .map_err(|err| format!("{err:#}"))?;
                    let current = requested_content_hash.as_ref().is_some_and(|hash| {
                        summaries
                            .iter()
                            .any(|summary| summary.content_hash == *hash)
                    });
                    Ok(SummaryFileSnapshot {
                        file_path: file_key.clone(),
                        requested_content_hash,
                        summaries,
                        current,
                    })
                });
                let slot = SummaryFileSlot {
                    content_hash: content_hash_cell,
                    epoch,
                    snapshot,
                };
                slots.insert(normalized.clone(), slot);
                slot
            }
        };

        if self.ctx.is_set(&slot.snapshot) {
            self.hits.set(self.hits.get() + 1);
        } else {
            self.misses.set(self.misses.get() + 1);
        }
        let result = self
            .ctx
            .get(&slot.snapshot)
            .map_err(|message| anyhow::anyhow!("{message}"));
        if result.is_err() {
            slot.snapshot.clear(&self.ctx);
        }
        result
    }

    pub fn current_by_file(
        &self,
        file_path: &str,
        content_hash: &str,
    ) -> Result<Option<Vec<Summary>>> {
        let snapshot = self.file_snapshot(file_path, Some(content_hash))?;
        if snapshot.current {
            Ok(Some(snapshot.summaries))
        } else {
            Ok(None)
        }
    }

    pub fn get_or_extract_file<F>(
        &self,
        file_path: &str,
        content_hash: &str,
        extract: F,
    ) -> Result<SummaryCacheLookup>
    where
        F: FnOnce() -> Result<Vec<Summary>>,
    {
        if let Some(summaries) = self.current_by_file(file_path, content_hash)? {
            return Ok(SummaryCacheLookup {
                summaries,
                source: SummaryCacheSource::Cached,
            });
        }

        let summaries = extract()?;
        self.db.replace_file(file_path, &summaries)?;
        self.invalidate_file(file_path, Some(content_hash));
        Ok(SummaryCacheLookup {
            summaries,
            source: SummaryCacheSource::Extracted,
        })
    }

    pub fn invalidate_file(&self, file_path: &str, content_hash: Option<&str>) {
        let normalized = normalize_summary_file_key_str(file_path);
        let Some(slot) = self.slots.borrow().get(&normalized).copied() else {
            return;
        };
        self.ctx
            .set(&slot.content_hash, content_hash.map(str::to_string));
        let epoch = self.ctx.get(&slot.epoch);
        self.ctx.set(&slot.epoch, epoch.wrapping_add(1));
    }
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

pub fn normalize_summary_file_key(path: &Path) -> String {
    normalize_summary_file_key_str(path.to_string_lossy().as_ref())
}

pub fn normalize_summary_file_key_str(path: &str) -> String {
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
    let client = ExtractionClient::resolve(config)?;
    extract_for_file_with_client(
        file_path,
        symbols_db_path,
        symbols_source_root,
        config,
        &client,
    )
}

pub fn extract_for_file_with_client(
    file_path: &Path,
    symbols_db_path: Option<&Path>,
    symbols_source_root: Option<&Path>,
    config: &SummarizeConfig,
    client: &ExtractionClient,
) -> Result<Vec<Summary>> {
    let source = std::fs::read_to_string(file_path)
        .with_context(|| format!("reading {}", file_path.display()))?;

    let token_estimate = source.len() / 4;
    if token_estimate > config.max_file_tokens {
        return Err(anyhow::Error::new(TerminalExtractionError {
            kind: ExtractionFailureKind::TooLarge,
            message: format!(
                "file {} exceeds max_file_tokens ({} > {}); raise it with --max-file-tokens or [summarize].max_file_tokens",
                file_path.display(),
                token_estimate,
                config.max_file_tokens
            ),
        }));
    }

    let hash = content_hash(source.as_bytes());
    let file_str = file_path.to_string_lossy().to_string();

    let symbols = if let Some(db_path) = symbols_db_path {
        load_symbols_for_file(db_path, file_path, symbols_source_root)?
    } else {
        Vec::new()
    };

    let prompt = build_extraction_prompt(&file_str, &source, &symbols);

    let (response_text, tokens_in, tokens_out) = client.complete(&prompt)?;

    let parsed = parse_extraction_response(file_path, &response_text)?;

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

fn parse_extraction_response(file_path: &Path, response_text: &str) -> Result<ExtractionResponse> {
    serde_json::from_str(response_text).map_err(|error| {
        let preview = response_text.chars().take(240).collect::<String>();
        anyhow::Error::new(TerminalExtractionError {
            kind: ExtractionFailureKind::UnparseableResponse,
            message: format!(
                "parsing extraction response for {} failed: {error}; response preview: {preview:?}",
                file_path.display()
            ),
        })
    })
}

fn normalize_lookup_path(path: &Path) -> String {
    normalize_summary_file_key(path)
}

pub fn normalize_lexical_path(path: &Path) -> PathBuf {
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

    Ok((
        strip_markdown_fences(&content).to_string(),
        tokens_in,
        tokens_out,
    ))
}

fn strip_markdown_fences(content: &str) -> &str {
    let cleaned = content
        .trim()
        .strip_prefix("```json")
        .or_else(|| content.trim().strip_prefix("```"))
        .unwrap_or(content.trim());
    cleaned.strip_suffix("```").unwrap_or(cleaned).trim()
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

fn call_claude_cli(command: &Path, model: &str, prompt: &str) -> Result<(String, i64, i64)> {
    let mut child = Command::new(command)
        .arg("-p")
        .arg("--model")
        .arg(model)
        .arg("--safe-mode")
        .arg("--tools")
        .arg("")
        .arg("--no-session-persistence")
        .args(["--output-format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting Claude Code CLI at {}", command.display()))?;

    child
        .stdin
        .take()
        .context("opening Claude Code CLI stdin")?
        .write_all(prompt.as_bytes())
        .context("writing extraction prompt to Claude Code CLI")?;
    let output = child
        .wait_with_output()
        .context("waiting for Claude Code CLI extraction")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Claude Code CLI extraction failed with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let response = String::from_utf8(output.stdout)
        .context("Claude Code CLI extraction returned non-UTF-8 output")?;
    parse_claude_cli_response(&response)
}

fn parse_claude_cli_response(response: &str) -> Result<(String, i64, i64)> {
    let response: ClaudeCliResponse = serde_json::from_str(response.trim())
        .context("parsing Claude Code CLI JSON response and token usage")?;
    let content = strip_markdown_fences(response.result.trim());
    if content.is_empty() {
        bail!("Claude Code CLI extraction returned an empty response");
    }
    let tokens_input = response
        .usage
        .input_tokens
        .saturating_add(response.usage.cache_creation_input_tokens)
        .saturating_add(response.usage.cache_read_input_tokens);
    Ok((
        content.to_string(),
        tokens_input,
        response.usage.output_tokens,
    ))
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
            "git rev-parse --is-inside-work-tree failed in {}: {}",
            root.display(),
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
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::NamedTempFile;
    use tsift_sqlite::{rollback_journal_path, wal_sidecar_path};

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
    fn summary_cache_reuses_file_snapshot_until_content_hash_changes() {
        let (_tmp, db) = test_db();
        db.insert(&make_summary("stale", "src/lib.rs", "hash_v1"))
            .unwrap();
        let cache = SummaryCache::new(db);

        let first = cache
            .current_by_file("src/lib.rs", "hash_v1")
            .unwrap()
            .unwrap();
        assert_eq!(first[0].symbol_name, "stale");
        assert_eq!(cache.stats(), (0, 1));

        cache
            .db()
            .replace_file(
                "src/lib.rs",
                &[make_summary("fresh", "src/lib.rs", "hash_v2")],
            )
            .unwrap();
        let second = cache
            .current_by_file("src/lib.rs", "hash_v1")
            .unwrap()
            .unwrap();
        assert_eq!(
            second[0].symbol_name, "stale",
            "same content hash should reuse the cached Slot"
        );
        assert_eq!(cache.stats(), (1, 1));

        let third = cache
            .current_by_file("src/lib.rs", "hash_v2")
            .unwrap()
            .unwrap();
        assert_eq!(third[0].symbol_name, "fresh");
        assert_eq!(cache.stats(), (1, 2));
    }

    #[test]
    fn summary_cache_get_or_extract_file_computes_once_until_hash_changes() {
        let (_tmp, db) = test_db();
        let cache = SummaryCache::new(db);
        let extractions = Cell::new(0usize);

        let first = cache
            .get_or_extract_file("src/lib.rs", "hash_v1", || {
                extractions.set(extractions.get() + 1);
                Ok(vec![make_summary("first", "src/lib.rs", "hash_v1")])
            })
            .unwrap();
        assert_eq!(first.source, SummaryCacheSource::Extracted);
        assert_eq!(first.summaries[0].symbol_name, "first");
        assert_eq!(extractions.get(), 1);

        let second = cache
            .get_or_extract_file("src/lib.rs", "hash_v1", || {
                bail!("same hash should reuse cached summaries")
            })
            .unwrap();
        assert_eq!(second.source, SummaryCacheSource::Cached);
        assert_eq!(second.summaries[0].symbol_name, "first");
        assert_eq!(extractions.get(), 1);

        let third = cache
            .get_or_extract_file("src/lib.rs", "hash_v2", || {
                extractions.set(extractions.get() + 1);
                Ok(vec![make_summary("second", "src/lib.rs", "hash_v2")])
            })
            .unwrap();
        assert_eq!(third.source, SummaryCacheSource::Extracted);
        assert_eq!(third.summaries[0].symbol_name, "second");
        assert_eq!(extractions.get(), 2);
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
    fn terminal_extraction_failures_are_keyed_by_content_and_cleared_on_success() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let source = b"fn main() {}\n";
        std::fs::write(root.path().join("src/main.rs"), source).unwrap();
        let hash = content_hash(source);
        let (_tmp, db) = test_db();

        db.record_terminal_failure(
            "src/main.rs",
            &hash,
            ExtractionFailureKind::TooLarge,
            "raise --max-file-tokens",
        )
        .unwrap();
        let cached = db
            .terminal_failure("src/main.rs", &hash)
            .unwrap()
            .expect("same content should retain its terminal failure");
        assert_eq!(cached.kind, ExtractionFailureKind::TooLarge);
        assert_eq!(cached.message, "raise --max-file-tokens");
        assert!(
            db.terminal_failure("src/main.rs", "different-hash")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.current_terminal_failure_paths(root.path()).unwrap(),
            BTreeSet::from(["src/main.rs".to_string()])
        );

        db.replace_file("src/main.rs", &[make_summary("main", "src/main.rs", &hash)])
            .unwrap();
        assert!(db.terminal_failure("src/main.rs", &hash).unwrap().is_none());
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
    fn claude_cli_response_extracts_content_and_measured_usage() {
        let response = json!({
            "result": "```json\n{\"summary\":\"ok\"}\n```",
            "usage": {
                "input_tokens": 12,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 40,
                "output_tokens": 7
            }
        })
        .to_string();

        let (content, tokens_in, tokens_out) = parse_claude_cli_response(&response).unwrap();
        assert_eq!(content, "{\"summary\":\"ok\"}");
        assert_eq!(tokens_in, 55);
        assert_eq!(tokens_out, 7);
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
            api_key_env: "PATH".to_string(),
            ..Default::default()
        };
        let error = extract_for_file(&big_file, None, None, &config).unwrap_err();
        assert!(error.to_string().contains("exceeds max_file_tokens"));
        assert!(error.to_string().contains("--max-file-tokens"));
        assert_eq!(
            terminal_extraction_failure(&error).map(|(kind, _)| kind),
            Some(ExtractionFailureKind::TooLarge)
        );
    }

    #[test]
    fn extraction_parse_errors_name_the_file_reason_and_response_preview() {
        let error =
            parse_extraction_response(Path::new("src/broken.rs"), "not-json output").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("src/broken.rs"), "{message}");
        assert!(message.contains("expected ident"), "{message}");
        assert!(message.contains("not-json output"), "{message}");
        assert_eq!(
            terminal_extraction_failure(&error).map(|(kind, _)| kind),
            Some(ExtractionFailureKind::UnparseableResponse)
        );
    }

    #[test]
    fn extraction_backend_requires_an_api_key_or_claude_cli() {
        let result = select_extraction_backend(None, None, false);
        assert!(result.is_err());
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("missing credentials unexpectedly resolved a backend"),
        };
        assert!(
            error
                .to_string()
                .contains("no Anthropic API key or authenticated Claude Code CLI")
        );
    }

    #[test]
    fn hosted_claude_provider_prefers_the_cli_over_a_direct_api_key() {
        let command = PathBuf::from("/mock/claude");
        let backend =
            select_extraction_backend(Some("direct-key".to_string()), Some(command.clone()), true)
                .unwrap();
        assert!(matches!(
            backend,
            ExtractionBackend::ClaudeCli { command: selected } if selected == command
        ));
    }

    #[test]
    fn direct_api_key_stays_preferred_without_a_hosted_claude_provider() {
        let backend = select_extraction_backend(
            Some("direct-key".to_string()),
            Some(PathBuf::from("/mock/claude")),
            false,
        )
        .unwrap();
        assert!(matches!(backend, ExtractionBackend::AnthropicApi { .. }));
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
            Some(tsift_sqlite::ReadOnlyRecovery::SnapshotFallback)
        );
        let results = opened.db.get_by_symbol("main").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "cached summary");
    }

    #[test]
    fn summary_read_only_reports_wal_snapshot_fallback_when_wal_db_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("summaries.db");
        let db = SummaryDb::open(&db_path).unwrap();
        db.insert(&make_summary("main", "src/main.rs", "hash1"))
            .unwrap();
        drop(db);

        let _lock = hold_wal_lock(&db_path);

        let opened = SummaryDb::open_read_only_with_recovery(&db_path).unwrap();
        assert_eq!(
            opened.recovery,
            Some(tsift_sqlite::ReadOnlyRecovery::SnapshotFallbackWal)
        );
        let results = opened.db.get_by_symbol("main").unwrap();
        assert_eq!(results.len(), 1);
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
