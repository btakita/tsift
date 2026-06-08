use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tsift_core::{GraphEdge, GraphFreshness, GraphNode, GraphProjection, GraphProvenance};

pub const MEMORY_CONTRACT_VERSION: &str = "tsift-memory-v1";
pub const MEMORY_SCHEMA_VERSION: i64 = 1;
pub const DEFAULT_MAX_PROMPT_TOKENS: usize = 4096;
pub const DEFAULT_RESERVE_TOKENS: usize = 512;
pub const DEFAULT_MAX_EVENT_TOKENS: usize = 1536;
pub const MEMORY_BUDGET_GUARD_CONTRACT_VERSION: &str = "tsift-memory-budget-guard-v1";

const MAX_IMPORT_EVENT_IDS: usize = 100;

const MEMORY_SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS memory_schema_versions (
  version INTEGER PRIMARY KEY,
  applied_at_unix INTEGER NOT NULL
);

INSERT OR IGNORE INTO memory_schema_versions(version, applied_at_unix)
VALUES (1, strftime('%s','now'));

CREATE TABLE IF NOT EXISTS memory_events (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  session_id TEXT,
  source_ref TEXT NOT NULL,
  text TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  observed_at_unix INTEGER,
  token_estimate INTEGER NOT NULL,
  imported_from TEXT,
  imported_id TEXT,
  created_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE INDEX IF NOT EXISTS idx_memory_events_kind
ON memory_events(kind);

CREATE INDEX IF NOT EXISTS idx_memory_events_session
ON memory_events(session_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_events_import_source
ON memory_events(imported_from, imported_id)
WHERE imported_from IS NOT NULL AND imported_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS memory_session_summaries (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  summary TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  observed_at_unix INTEGER,
  token_estimate INTEGER NOT NULL,
  created_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS memory_artifacts (
  id TEXT PRIMARY KEY,
  session_id TEXT,
  source_ref TEXT NOT NULL,
  artifact_kind TEXT NOT NULL,
  path TEXT,
  handle TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  token_estimate INTEGER NOT NULL DEFAULT 0,
  created_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS memory_tool_spans (
  id TEXT PRIMARY KEY,
  session_id TEXT,
  tool_name TEXT NOT NULL,
  input_artifact_id TEXT,
  output_artifact_id TEXT,
  status TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  started_at_unix INTEGER,
  completed_at_unix INTEGER,
  FOREIGN KEY(input_artifact_id) REFERENCES memory_artifacts(id),
  FOREIGN KEY(output_artifact_id) REFERENCES memory_artifacts(id)
);

CREATE TABLE IF NOT EXISTS memory_embeddings (
  id TEXT PRIMARY KEY,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  vector_ref TEXT NOT NULL,
  dimensions INTEGER,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  created_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS memory_graph_links (
  id TEXT PRIMARY KEY,
  memory_event_id TEXT NOT NULL,
  graph_node_id TEXT NOT NULL,
  graph_edge_id TEXT,
  link_kind TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  created_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  FOREIGN KEY(memory_event_id) REFERENCES memory_events(id)
);

CREATE TABLE IF NOT EXISTS memory_import_runs (
  id TEXT PRIMARY KEY,
  source TEXT NOT NULL,
  source_ref TEXT NOT NULL,
  status TEXT NOT NULL,
  imported_events INTEGER NOT NULL DEFAULT 0,
  warnings_json TEXT NOT NULL DEFAULT '[]',
  started_at_unix INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  completed_at_unix INTEGER
);
"#;

pub fn memory_schema_sql() -> &'static str {
    MEMORY_SCHEMA_SQL
}

pub fn default_memory_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".tsift").join("memory.db")
}

pub fn default_claude_mem_db_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude-mem").join("claude-mem.db"))
}

pub fn estimate_tokens(text: &str) -> usize {
    let bytes = text.trim().len();
    if bytes == 0 { 0 } else { bytes.div_ceil(4) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEventKind {
    PromptTarget,
    ToolCall,
    ToolResultArtifact,
    ResponseSummary,
    CloseoutProof,
    SessionCheck,
    ImportedObservation,
    ImportedSessionSummary,
    ImportedUserPrompt,
}

impl MemoryEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PromptTarget => "prompt_target",
            Self::ToolCall => "tool_call",
            Self::ToolResultArtifact => "tool_result_artifact",
            Self::ResponseSummary => "response_summary",
            Self::CloseoutProof => "closeout_proof",
            Self::SessionCheck => "session_check",
            Self::ImportedObservation => "imported_observation",
            Self::ImportedSessionSummary => "imported_session_summary",
            Self::ImportedUserPrompt => "imported_user_prompt",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "prompt_target" => Ok(Self::PromptTarget),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result_artifact" => Ok(Self::ToolResultArtifact),
            "response_summary" => Ok(Self::ResponseSummary),
            "closeout_proof" => Ok(Self::CloseoutProof),
            "session_check" => Ok(Self::SessionCheck),
            "imported_observation" => Ok(Self::ImportedObservation),
            "imported_session_summary" => Ok(Self::ImportedSessionSummary),
            "imported_user_prompt" => Ok(Self::ImportedUserPrompt),
            other => bail!("unsupported memory event kind `{other}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub kind: MemoryEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub source_ref: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at_unix: Option<i64>,
    pub token_estimate: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_id: Option<String>,
}

impl MemoryEvent {
    pub fn new(
        kind: MemoryEventKind,
        source_ref: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        Self {
            kind,
            session_id: None,
            source_ref: source_ref.into(),
            token_estimate: estimate_tokens(&text),
            text,
            metadata: BTreeMap::new(),
            observed_at_unix: None,
            imported_from: None,
            imported_id: None,
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn with_observed_at_unix(mut self, observed_at_unix: i64) -> Self {
        self.observed_at_unix = Some(observed_at_unix);
        self
    }

    pub fn with_import(mut self, source: impl Into<String>, id: impl Into<String>) -> Self {
        self.imported_from = Some(source.into());
        self.imported_id = Some(id.into());
        self
    }

    pub fn stable_id(&self) -> String {
        let raw = serde_json::json!([
            self.kind.as_str(),
            self.session_id,
            self.source_ref,
            self.text,
            self.observed_at_unix,
            self.imported_from,
            self.imported_id
        ])
        .to_string();
        format!("memevt:{}", blake3::hash(raw.as_bytes()).to_hex())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    pub max_prompt_tokens: usize,
    pub reserve_tokens: usize,
    pub max_event_tokens: usize,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            max_prompt_tokens: DEFAULT_MAX_PROMPT_TOKENS,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            max_event_tokens: DEFAULT_MAX_EVENT_TOKENS,
        }
    }
}

impl MemoryBudget {
    pub fn new(max_prompt_tokens: usize) -> Self {
        Self {
            max_prompt_tokens,
            ..Self::default()
        }
    }

    pub fn available_tokens(self) -> usize {
        self.max_prompt_tokens.saturating_sub(self.reserve_tokens)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetedMemoryEvent {
    pub id: String,
    pub kind: String,
    pub source_ref: String,
    pub token_estimate: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredMemoryEvent {
    pub id: String,
    pub kind: String,
    pub source_ref: String,
    pub token_estimate: usize,
    pub reason: String,
    pub recommended_chunks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHandoffPlan {
    pub contract_version: String,
    pub status: String,
    pub max_prompt_tokens: usize,
    pub reserve_tokens: usize,
    pub available_tokens: usize,
    pub estimated_included_tokens: usize,
    pub included_events: Vec<BudgetedMemoryEvent>,
    pub deferred_events: Vec<DeferredMemoryEvent>,
    pub next_commands: Vec<String>,
}

pub fn plan_capture_handoff(events: &[MemoryEvent], budget: MemoryBudget) -> MemoryHandoffPlan {
    let mut used = 0usize;
    let mut included_events = Vec::new();
    let mut deferred_events = Vec::new();
    let available = budget.available_tokens();

    for event in events {
        let id = event.stable_id();
        if event.token_estimate > budget.max_event_tokens {
            deferred_events.push(DeferredMemoryEvent {
                id,
                kind: event.kind.as_str().to_string(),
                source_ref: event.source_ref.clone(),
                token_estimate: event.token_estimate,
                reason: "event_exceeds_max_event_tokens".to_string(),
                recommended_chunks: event
                    .token_estimate
                    .div_ceil(budget.max_event_tokens.max(1)),
            });
            continue;
        }

        if used + event.token_estimate <= available {
            used += event.token_estimate;
            included_events.push(BudgetedMemoryEvent {
                id,
                kind: event.kind.as_str().to_string(),
                source_ref: event.source_ref.clone(),
                token_estimate: event.token_estimate,
            });
        } else {
            deferred_events.push(DeferredMemoryEvent {
                id,
                kind: event.kind.as_str().to_string(),
                source_ref: event.source_ref.clone(),
                token_estimate: event.token_estimate,
                reason: "handoff_budget_exhausted".to_string(),
                recommended_chunks: 1,
            });
        }
    }

    MemoryHandoffPlan {
        contract_version: MEMORY_CONTRACT_VERSION.to_string(),
        status: if deferred_events.is_empty() {
            "ready".to_string()
        } else {
            "split_required".to_string()
        },
        max_prompt_tokens: budget.max_prompt_tokens,
        reserve_tokens: budget.reserve_tokens,
        available_tokens: available,
        estimated_included_tokens: used,
        included_events,
        deferred_events,
        next_commands: vec![
            "tsift memory handoff-plan '<event text>' --budget-tokens <n> --json".to_string(),
            "tsift --envelope context-pack <session.md> --budget normal".to_string(),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudgetGuardInput {
    pub source_ref: String,
    pub payload_kind: String,
    pub text: String,
}

impl MemoryBudgetGuardInput {
    pub fn new(
        source_ref: impl Into<String>,
        payload_kind: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            payload_kind: payload_kind.into(),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudgetReplacement {
    pub strategy: String,
    pub artifact_ref: String,
    pub digest_command: String,
    pub context_command: String,
    pub session_review_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRetryChunk {
    pub index: usize,
    pub source_ref: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub token_estimate: usize,
    pub retry_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudgetGuardReport {
    pub contract_version: String,
    pub status: String,
    pub allowed: bool,
    pub source_ref: String,
    pub payload_kind: String,
    pub byte_count: usize,
    pub estimated_tokens: usize,
    pub max_prompt_tokens: usize,
    pub reserve_tokens: usize,
    pub available_tokens: usize,
    pub max_chunk_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<MemoryBudgetReplacement>,
    pub retryable_chunk_plan: Vec<MemoryRetryChunk>,
    pub warnings: Vec<String>,
    pub next_commands: Vec<String>,
}

pub fn guard_memory_handoff(
    input: MemoryBudgetGuardInput,
    budget: MemoryBudget,
) -> MemoryBudgetGuardReport {
    let estimated_tokens = estimate_tokens(&input.text);
    let available_tokens = budget.available_tokens();
    let allowed =
        estimated_tokens <= available_tokens && estimated_tokens <= budget.max_event_tokens;
    let replacement = (!allowed).then(|| replacement_for_budget_guard(&input));
    let retryable_chunk_plan = if allowed {
        Vec::new()
    } else {
        retry_chunks_for_budget_guard(&input, budget.max_event_tokens.min(available_tokens).max(1))
    };
    let mut warnings = Vec::new();
    if estimated_tokens > available_tokens {
        warnings.push("payload_exceeds_available_prompt_budget".to_string());
    }
    if estimated_tokens > budget.max_event_tokens {
        warnings.push("payload_exceeds_max_chunk_tokens".to_string());
    }

    MemoryBudgetGuardReport {
        contract_version: MEMORY_BUDGET_GUARD_CONTRACT_VERSION.to_string(),
        status: if allowed {
            "ready".to_string()
        } else {
            "blocked_split_required".to_string()
        },
        allowed,
        source_ref: input.source_ref.clone(),
        payload_kind: input.payload_kind.clone(),
        byte_count: input.text.len(),
        estimated_tokens,
        max_prompt_tokens: budget.max_prompt_tokens,
        reserve_tokens: budget.reserve_tokens,
        available_tokens,
        max_chunk_tokens: budget.max_event_tokens,
        replacement,
        retryable_chunk_plan,
        warnings,
        next_commands: vec![
            format!(
                "tsift memory budget-guard --file {} --json",
                shell_quote(&input.source_ref)
            ),
            "tsift --envelope context-pack <session.md> --budget normal".to_string(),
            "tsift --envelope session-review <session.md> --next-context --budget normal"
                .to_string(),
        ],
    }
}

fn replacement_for_budget_guard(input: &MemoryBudgetGuardInput) -> MemoryBudgetReplacement {
    let quoted_ref = shell_quote(&input.source_ref);
    let is_transcript = matches!(
        input.payload_kind.as_str(),
        "transcript" | "session" | "session_transcript" | "agent_doc"
    ) || input.source_ref.ends_with(".jsonl")
        || input.source_ref.ends_with(".md");
    let strategy = if is_transcript {
        "replace_raw_transcript_with_session_review_or_context_pack_handle"
    } else {
        "replace_raw_tool_or_log_payload_with_digest_artifact_handle"
    };
    MemoryBudgetReplacement {
        strategy: strategy.to_string(),
        artifact_ref: format!(
            "artifact:{}",
            blake3::hash(input.source_ref.as_bytes()).to_hex()
        ),
        digest_command: if is_transcript {
            format!("tsift --envelope session-review {quoted_ref} --next-context --budget normal")
        } else {
            format!("tsift log-digest --path . --input {quoted_ref} --json")
        },
        context_command: format!("tsift --envelope context-pack {quoted_ref} --budget normal"),
        session_review_command: format!(
            "tsift --envelope session-review {quoted_ref} --next-context --budget normal"
        ),
    }
}

fn retry_chunks_for_budget_guard(
    input: &MemoryBudgetGuardInput,
    max_chunk_tokens: usize,
) -> Vec<MemoryRetryChunk> {
    let byte_budget = max_chunk_tokens.saturating_mul(4).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < input.text.len() {
        let mut end = (start + byte_budget).min(input.text.len());
        while end > start && !input.text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            if let Some((offset, ch)) = input.text[start..].char_indices().next() {
                end = start + offset + ch.len_utf8();
            } else {
                break;
            }
        }
        let token_estimate = estimate_tokens(&input.text[start..end]);
        let index = chunks.len() + 1;
        chunks.push(MemoryRetryChunk {
            index,
            source_ref: format!("{}#chunk-{index}", input.source_ref),
            byte_start: start,
            byte_end: end,
            token_estimate,
            retry_command: retry_chunk_command(input, index, start, end),
        });
        start = end;
    }
    chunks
}

fn retry_chunk_command(
    input: &MemoryBudgetGuardInput,
    index: usize,
    byte_start: usize,
    byte_end: usize,
) -> String {
    let chunk_ref = format!("{}#chunk-{index}", input.source_ref);
    if input.source_ref == "inline" {
        format!(
            "tsift memory budget-guard --text '<chunk {index} payload>' --source-ref {} --budget-tokens <n> --json",
            shell_quote(&chunk_ref)
        )
    } else {
        format!(
            "tsift memory budget-guard --file {} --byte-start {byte_start} --byte-end {byte_end} --source-ref {} --budget-tokens <n> --json",
            shell_quote(&input.source_ref),
            shell_quote(&chunk_ref)
        )
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHookSpec {
    pub name: String,
    pub event_kind: String,
    pub required_fields: Vec<String>,
    pub budget_behavior: String,
}

pub fn agent_doc_hook_contract() -> Vec<MemoryHookSpec> {
    vec![
        MemoryHookSpec {
            name: "prompt_target".to_string(),
            event_kind: MemoryEventKind::PromptTarget.as_str().to_string(),
            required_fields: vec!["session_path".to_string(), "prompt_target".to_string()],
            budget_behavior:
                "capture text plus graph/source handles; never inline raw transcript blobs"
                    .to_string(),
        },
        MemoryHookSpec {
            name: "tool_result_artifact".to_string(),
            event_kind: MemoryEventKind::ToolResultArtifact.as_str().to_string(),
            required_fields: vec!["tool_name".to_string(), "artifact_handle".to_string()],
            budget_behavior: "store artifact handle and digest, not full raw output".to_string(),
        },
        MemoryHookSpec {
            name: "response_summary".to_string(),
            event_kind: MemoryEventKind::ResponseSummary.as_str().to_string(),
            required_fields: vec!["response_topic".to_string(), "summary".to_string()],
            budget_behavior: "summaries are capped before write and linked to source handles"
                .to_string(),
        },
        MemoryHookSpec {
            name: "closeout_proof".to_string(),
            event_kind: MemoryEventKind::CloseoutProof.as_str().to_string(),
            required_fields: vec!["commit_hash".to_string(), "changed_paths".to_string()],
            budget_behavior: "proof records are structured metadata with compact prose".to_string(),
        },
        MemoryHookSpec {
            name: "session_check".to_string(),
            event_kind: MemoryEventKind::SessionCheck.as_str().to_string(),
            required_fields: vec!["status".to_string(), "session_path".to_string()],
            budget_behavior: "persist pass/fail state and recovery commands".to_string(),
        },
    ]
}

pub fn agent_doc_closeout_events(
    session_path: &Path,
    prompt_target: &str,
    response_summary: &str,
    commit_hash: Option<&str>,
    session_check_status: &str,
) -> Vec<MemoryEvent> {
    let session_id = session_path.display().to_string();
    let mut events = vec![
        MemoryEvent::new(
            MemoryEventKind::PromptTarget,
            session_path.display().to_string(),
            prompt_target,
        )
        .with_session_id(session_id.clone()),
        MemoryEvent::new(
            MemoryEventKind::ResponseSummary,
            session_path.display().to_string(),
            response_summary,
        )
        .with_session_id(session_id.clone()),
        MemoryEvent::new(
            MemoryEventKind::SessionCheck,
            session_path.display().to_string(),
            session_check_status,
        )
        .with_session_id(session_id.clone())
        .with_metadata("status", session_check_status),
    ];

    if let Some(commit_hash) = commit_hash {
        events.push(
            MemoryEvent::new(
                MemoryEventKind::CloseoutProof,
                session_path.display().to_string(),
                commit_hash,
            )
            .with_session_id(session_id)
            .with_metadata("commit_hash", commit_hash),
        );
    }

    events
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInsertResult {
    pub id: String,
    pub inserted: bool,
}

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(MEMORY_SCHEMA_SQL)
            .with_context(|| format!("initialize {}", path.display()))?;
        Ok(Self { conn })
    }

    pub fn insert_event(&self, event: &MemoryEvent) -> Result<String> {
        Ok(self.insert_event_result(event)?.id)
    }

    pub fn insert_event_result(&self, event: &MemoryEvent) -> Result<MemoryInsertResult> {
        insert_event_on(&self.conn, event)
    }

    pub fn insert_events(&mut self, events: &[MemoryEvent]) -> Result<Vec<MemoryInsertResult>> {
        let tx = self.conn.transaction()?;
        let mut results = Vec::with_capacity(events.len());
        for event in events {
            results.push(insert_event_on(&tx, event)?);
        }
        tx.commit()?;
        Ok(results)
    }

    pub fn event_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM memory_events", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}

fn insert_event_on(conn: &Connection, event: &MemoryEvent) -> Result<MemoryInsertResult> {
    let id = event.stable_id();
    let metadata_json = serde_json::to_string(&event.metadata)?;
    let changed = conn.execute(
        r#"
            INSERT OR IGNORE INTO memory_events(
              id, kind, session_id, source_ref, text, metadata_json,
              observed_at_unix, token_estimate, imported_from, imported_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
        params![
            &id,
            event.kind.as_str(),
            event.session_id.as_deref(),
            &event.source_ref,
            &event.text,
            &metadata_json,
            event.observed_at_unix,
            event.token_estimate as i64,
            event.imported_from.as_deref(),
            event.imported_id.as_deref()
        ],
    )?;
    Ok(MemoryInsertResult {
        id,
        inserted: changed > 0,
    })
}

pub fn read_memory_events(memory_db_path: &Path, limit: usize) -> Result<Vec<MemoryEvent>> {
    if !memory_db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        memory_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open memory db {}", memory_db_path.display()))?;
    let mut stmt = conn.prepare(
        r#"
        SELECT kind, session_id, source_ref, text, metadata_json,
               observed_at_unix, token_estimate, imported_from, imported_id
        FROM memory_events
        ORDER BY COALESCE(observed_at_unix, created_at_unix), id
        LIMIT ?1
        "#,
    )?;
    let mut rows = stmt.query([limit as i64])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let kind_raw: String = row.get(0)?;
        let session_id: Option<String> = row.get(1)?;
        let source_ref: String = row.get(2)?;
        let text: String = row.get(3)?;
        let metadata_json: String = row.get(4)?;
        let observed_at_unix: Option<i64> = row.get(5)?;
        let token_estimate: i64 = row.get(6)?;
        let imported_from: Option<String> = row.get(7)?;
        let imported_id: Option<String> = row.get(8)?;
        let metadata = serde_json::from_str::<BTreeMap<String, String>>(&metadata_json)
            .with_context(|| format!("parse memory metadata for {source_ref}"))?;
        let mut event = MemoryEvent::new(MemoryEventKind::parse(&kind_raw)?, source_ref, text);
        event.session_id = session_id;
        event.metadata = metadata;
        event.observed_at_unix = observed_at_unix;
        event.token_estimate = token_estimate.max(0) as usize;
        event.imported_from = imported_from;
        event.imported_id = imported_id;
        events.push(event);
    }
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemTablePlan {
    pub table: String,
    pub supported: bool,
    pub rows: usize,
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemImportPlan {
    pub db_path: String,
    pub exists: bool,
    pub readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma_path: Option<String>,
    pub chroma_present: bool,
    pub observations: ClaudeMemTablePlan,
    pub session_summaries: ClaudeMemTablePlan,
    pub user_prompts: ClaudeMemTablePlan,
    pub pending_messages: ClaudeMemTablePlan,
    pub warnings: Vec<String>,
    pub next_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemTableReconciliation {
    pub table: String,
    pub source_rows: usize,
    pub read_events: usize,
    pub planned_events: usize,
    pub imported_events: usize,
    pub already_present_events: usize,
    pub skipped_source_rows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_per_table: Option<usize>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemImportReconciliation {
    pub observations: ClaudeMemTableReconciliation,
    pub session_summaries: ClaudeMemTableReconciliation,
    pub user_prompts: ClaudeMemTableReconciliation,
    pub total_source_rows: usize,
    pub total_read_events: usize,
    pub total_planned_events: usize,
    pub total_imported_events: usize,
    pub total_already_present_events: usize,
    pub total_skipped_source_rows: usize,
    pub complete: bool,
}

fn empty_table_plan(table: &str) -> ClaudeMemTablePlan {
    ClaudeMemTablePlan {
        table: table.to_string(),
        supported: false,
        rows: 0,
        columns: Vec::new(),
        unsupported_reason: None,
    }
}

const PENDING_MESSAGES_EXCLUSION_REASON: &str = "pending_messages is intentionally excluded from claude-mem import because it contains transient queue state and raw tool payload fields, not durable replacement memory";

pub fn inspect_claude_mem(db_path: &Path) -> Result<ClaudeMemImportPlan> {
    let chroma_path = db_path.parent().map(|parent| parent.join("chroma"));
    let chroma_present = chroma_path.as_ref().is_some_and(|path| path.exists());
    let mut plan = ClaudeMemImportPlan {
        db_path: db_path.display().to_string(),
        exists: db_path.exists(),
        readable: false,
        chroma_path: chroma_path.as_ref().map(|path| path.display().to_string()),
        chroma_present,
        observations: empty_table_plan("observations"),
        session_summaries: empty_table_plan("session_summaries"),
        user_prompts: empty_table_plan("user_prompts"),
        pending_messages: empty_table_plan("pending_messages"),
        warnings: Vec::new(),
        next_commands: vec![
            "tsift memory import-claude-mem . --all --apply --json".to_string(),
            "tsift graph-db --path . --json refresh".to_string(),
        ],
    };

    if !plan.exists {
        plan.warnings
            .push("claude-mem database not found; pass --db to inspect another path".to_string());
        return Ok(plan);
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open claude-mem db {}", db_path.display()))?;
    plan.readable = true;
    plan.observations = inspect_table(&conn, "observations", true, None)?;
    plan.session_summaries = inspect_table(&conn, "session_summaries", true, None)?;
    plan.user_prompts = inspect_table(&conn, "user_prompts", true, None)?;
    plan.pending_messages = inspect_table(
        &conn,
        "pending_messages",
        false,
        Some(PENDING_MESSAGES_EXCLUSION_REASON),
    )?;
    if plan.pending_messages.rows > 0 {
        plan.warnings
            .push(PENDING_MESSAGES_EXCLUSION_REASON.to_string());
    }

    if !plan.chroma_present {
        plan.warnings
            .push("chroma directory was not found next to the claude-mem SQLite DB".to_string());
    }

    Ok(plan)
}

fn inspect_table(
    conn: &Connection,
    table: &str,
    supported: bool,
    unsupported_reason: Option<&str>,
) -> Result<ClaudeMemTablePlan> {
    if !table_exists(conn, table)? {
        return Ok(empty_table_plan(table));
    }
    let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    let columns = table_columns(conn, table)?;
    Ok(ClaudeMemTablePlan {
        table: table.to_string(),
        supported,
        rows: rows as usize,
        columns,
        unsupported_reason: unsupported_reason.map(str::to_string),
    })
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next()? {
        columns.push(row.get::<_, String>(1)?);
    }
    Ok(columns)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemImportReport {
    pub contract_version: String,
    pub source: String,
    pub target: String,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_per_table: Option<usize>,
    pub import_all: bool,
    pub imported_events: usize,
    pub already_present_events: usize,
    pub planned_events: usize,
    pub event_ids: Vec<String>,
    pub event_ids_total: usize,
    pub event_ids_truncated: bool,
    pub reconciliation: ClaudeMemImportReconciliation,
    pub plan: ClaudeMemImportPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMemReadReport {
    pub contract_version: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_per_table: Option<usize>,
    pub import_all: bool,
    pub events: Vec<MemoryEvent>,
    pub reconciliation: ClaudeMemImportReconciliation,
    pub plan: ClaudeMemImportPlan,
}

pub fn read_claude_mem_events(
    source_db_path: &Path,
    limit_per_table: Option<usize>,
) -> Result<ClaudeMemReadReport> {
    let plan = inspect_claude_mem(source_db_path)?;
    if !plan.exists || !plan.readable {
        let reconciliation =
            reconcile_claude_mem_import(&plan, limit_per_table, &[], &BTreeMap::new());
        return Ok(ClaudeMemReadReport {
            contract_version: MEMORY_CONTRACT_VERSION.to_string(),
            source: source_db_path.display().to_string(),
            limit_per_table,
            import_all: limit_per_table.is_none(),
            events: Vec::new(),
            reconciliation,
            plan,
        });
    }

    let conn = Connection::open_with_flags(
        source_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut events = Vec::new();
    if plan.observations.supported {
        events.extend(read_claude_mem_observations(&conn, limit_per_table)?);
    }
    if plan.session_summaries.supported {
        events.extend(read_claude_mem_summaries(&conn, limit_per_table)?);
    }
    if plan.user_prompts.supported {
        events.extend(read_claude_mem_user_prompts(&conn, limit_per_table)?);
    }
    let reconciliation =
        reconcile_claude_mem_import(&plan, limit_per_table, &events, &BTreeMap::new());

    Ok(ClaudeMemReadReport {
        contract_version: MEMORY_CONTRACT_VERSION.to_string(),
        source: source_db_path.display().to_string(),
        limit_per_table,
        import_all: limit_per_table.is_none(),
        events,
        reconciliation,
        plan,
    })
}

pub fn import_claude_mem(
    source_db_path: &Path,
    target_memory_db_path: &Path,
    limit_per_table: Option<usize>,
    dry_run: bool,
) -> Result<ClaudeMemImportReport> {
    let read_report = read_claude_mem_events(source_db_path, limit_per_table)?;
    let plan = read_report.plan;
    if !plan.exists || !plan.readable {
        let reconciliation =
            reconcile_claude_mem_import(&plan, limit_per_table, &[], &BTreeMap::new());
        return Ok(ClaudeMemImportReport {
            contract_version: MEMORY_CONTRACT_VERSION.to_string(),
            source: source_db_path.display().to_string(),
            target: target_memory_db_path.display().to_string(),
            dry_run,
            limit_per_table,
            import_all: limit_per_table.is_none(),
            imported_events: 0,
            already_present_events: 0,
            planned_events: 0,
            event_ids: Vec::new(),
            event_ids_total: 0,
            event_ids_truncated: false,
            reconciliation,
            plan,
        });
    }

    let events = read_report.events;
    let planned_events = events.len();
    let mut event_ids = Vec::new();
    let mut event_ids_total = 0;
    let mut write_results = BTreeMap::new();
    if !dry_run {
        let mut store = MemoryStore::open_or_create(target_memory_db_path)?;
        let results = store.insert_events(&events)?;
        for (event, result) in events.iter().zip(results) {
            record_claude_mem_write(&mut write_results, event, result.inserted);
            event_ids_total += 1;
            if event_ids.len() < MAX_IMPORT_EVENT_IDS {
                event_ids.push(result.id);
            }
        }
    }
    let reconciliation =
        reconcile_claude_mem_import(&plan, limit_per_table, &events, &write_results);

    Ok(ClaudeMemImportReport {
        contract_version: MEMORY_CONTRACT_VERSION.to_string(),
        source: source_db_path.display().to_string(),
        target: target_memory_db_path.display().to_string(),
        dry_run,
        limit_per_table,
        import_all: limit_per_table.is_none(),
        imported_events: reconciliation.total_imported_events,
        already_present_events: reconciliation.total_already_present_events,
        planned_events,
        event_ids,
        event_ids_total,
        event_ids_truncated: event_ids_total > MAX_IMPORT_EVENT_IDS,
        reconciliation,
        plan,
    })
}

fn read_claude_mem_observations(
    conn: &Connection,
    limit: Option<usize>,
) -> Result<Vec<MemoryEvent>> {
    let sql = format!(
        r#"
        SELECT id, memory_session_id, project, type, title, subtitle, text, facts,
               narrative, concepts, prompt_number, discovery_tokens, created_at_epoch,
               content_hash
        FROM observations
        ORDER BY created_at_epoch ASC, id ASC{}
        "#,
        claude_mem_limit_clause(limit)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(limit.map(|value| value as i64)), |row| {
        let id: i64 = row.get(0)?;
        let session_id: String = row.get(1)?;
        let project: String = row.get(2)?;
        let observation_type: String = row.get(3)?;
        let title: Option<String> = row.get(4)?;
        let subtitle: Option<String> = row.get(5)?;
        let text: Option<String> = row.get(6)?;
        let facts: Option<String> = row.get(7)?;
        let narrative: Option<String> = row.get(8)?;
        let concepts: Option<String> = row.get(9)?;
        let prompt_number: Option<i64> = row.get(10)?;
        let discovery_tokens: i64 = row.get(11)?;
        let created_at_epoch: i64 = row.get(12)?;
        let content_hash: Option<String> = row.get(13)?;
        let body = join_non_empty([title, subtitle, text, facts, narrative, concepts]);
        let mut event = MemoryEvent::new(
            MemoryEventKind::ImportedObservation,
            format!("claude-mem:observations:{id}"),
            body,
        )
        .with_session_id(session_id)
        .with_observed_at_unix(created_at_epoch)
        .with_import("claude-mem", format!("observations:{id}"))
        .with_metadata("project", project)
        .with_metadata("observation_type", observation_type)
        .with_metadata("discovery_tokens", discovery_tokens.to_string());
        if let Some(prompt_number) = prompt_number {
            event = event.with_metadata("prompt_number", prompt_number.to_string());
        }
        if let Some(content_hash) = content_hash {
            event = event.with_metadata("content_hash", content_hash);
        }
        Ok(event)
    })?;
    collect_rows(rows)
}

fn read_claude_mem_summaries(conn: &Connection, limit: Option<usize>) -> Result<Vec<MemoryEvent>> {
    let sql = format!(
        r#"
        SELECT id, memory_session_id, project, request, investigated, learned,
               completed, next_steps, notes, prompt_number, discovery_tokens,
               created_at_epoch
        FROM session_summaries
        ORDER BY created_at_epoch ASC, id ASC{}
        "#,
        claude_mem_limit_clause(limit)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(limit.map(|value| value as i64)), |row| {
        let id: i64 = row.get(0)?;
        let session_id: String = row.get(1)?;
        let project: String = row.get(2)?;
        let request: Option<String> = row.get(3)?;
        let investigated: Option<String> = row.get(4)?;
        let learned: Option<String> = row.get(5)?;
        let completed: Option<String> = row.get(6)?;
        let next_steps: Option<String> = row.get(7)?;
        let notes: Option<String> = row.get(8)?;
        let prompt_number: Option<i64> = row.get(9)?;
        let discovery_tokens: i64 = row.get(10)?;
        let created_at_epoch: i64 = row.get(11)?;
        let body = join_non_empty([request, investigated, learned, completed, next_steps, notes]);
        let mut event = MemoryEvent::new(
            MemoryEventKind::ImportedSessionSummary,
            format!("claude-mem:session_summaries:{id}"),
            body,
        )
        .with_session_id(session_id)
        .with_observed_at_unix(created_at_epoch)
        .with_import("claude-mem", format!("session_summaries:{id}"))
        .with_metadata("project", project)
        .with_metadata("discovery_tokens", discovery_tokens.to_string());
        if let Some(prompt_number) = prompt_number {
            event = event.with_metadata("prompt_number", prompt_number.to_string());
        }
        Ok(event)
    })?;
    collect_rows(rows)
}

fn read_claude_mem_user_prompts(
    conn: &Connection,
    limit: Option<usize>,
) -> Result<Vec<MemoryEvent>> {
    let sql = format!(
        r#"
        SELECT id, content_session_id, prompt_number, prompt_text, created_at_epoch
        FROM user_prompts
        ORDER BY created_at_epoch ASC, id ASC{}
        "#,
        claude_mem_limit_clause(limit)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(limit.map(|value| value as i64)), |row| {
        let id: i64 = row.get(0)?;
        let session_id: String = row.get(1)?;
        let prompt_number: i64 = row.get(2)?;
        let prompt_text: String = row.get(3)?;
        let created_at_epoch: i64 = row.get(4)?;
        Ok(MemoryEvent::new(
            MemoryEventKind::ImportedUserPrompt,
            format!("claude-mem:user_prompts:{id}"),
            prompt_text,
        )
        .with_session_id(session_id)
        .with_observed_at_unix(created_at_epoch)
        .with_import("claude-mem", format!("user_prompts:{id}"))
        .with_metadata("prompt_number", prompt_number.to_string()))
    })?;
    collect_rows(rows)
}

fn claude_mem_limit_clause(limit: Option<usize>) -> &'static str {
    if limit.is_some() {
        "\n        LIMIT ?1"
    } else {
        ""
    }
}

fn record_claude_mem_write(
    write_results: &mut BTreeMap<String, ClaudeMemTableWriteCounts>,
    event: &MemoryEvent,
    inserted: bool,
) {
    let Some(table) = claude_mem_event_table(event) else {
        return;
    };
    let counts = write_results.entry(table.to_string()).or_default();
    if inserted {
        counts.imported_events += 1;
    } else {
        counts.already_present_events += 1;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ClaudeMemTableWriteCounts {
    imported_events: usize,
    already_present_events: usize,
}

fn reconcile_claude_mem_import(
    plan: &ClaudeMemImportPlan,
    limit_per_table: Option<usize>,
    events: &[MemoryEvent],
    write_results: &BTreeMap<String, ClaudeMemTableWriteCounts>,
) -> ClaudeMemImportReconciliation {
    let event_counts = claude_mem_event_counts(events);
    let observations = reconcile_claude_mem_table(
        &plan.observations,
        limit_per_table,
        event_counts
            .get("observations")
            .copied()
            .unwrap_or_default(),
        write_results
            .get("observations")
            .copied()
            .unwrap_or_default(),
    );
    let session_summaries = reconcile_claude_mem_table(
        &plan.session_summaries,
        limit_per_table,
        event_counts
            .get("session_summaries")
            .copied()
            .unwrap_or_default(),
        write_results
            .get("session_summaries")
            .copied()
            .unwrap_or_default(),
    );
    let user_prompts = reconcile_claude_mem_table(
        &plan.user_prompts,
        limit_per_table,
        event_counts
            .get("user_prompts")
            .copied()
            .unwrap_or_default(),
        write_results
            .get("user_prompts")
            .copied()
            .unwrap_or_default(),
    );
    let total_source_rows =
        observations.source_rows + session_summaries.source_rows + user_prompts.source_rows;
    let total_read_events =
        observations.read_events + session_summaries.read_events + user_prompts.read_events;
    let total_planned_events = observations.planned_events
        + session_summaries.planned_events
        + user_prompts.planned_events;
    let total_imported_events = observations.imported_events
        + session_summaries.imported_events
        + user_prompts.imported_events;
    let total_already_present_events = observations.already_present_events
        + session_summaries.already_present_events
        + user_prompts.already_present_events;
    let total_skipped_source_rows = observations.skipped_source_rows
        + session_summaries.skipped_source_rows
        + user_prompts.skipped_source_rows;
    let complete = observations.complete && session_summaries.complete && user_prompts.complete;
    ClaudeMemImportReconciliation {
        observations,
        session_summaries,
        user_prompts,
        total_source_rows,
        total_read_events,
        total_planned_events,
        total_imported_events,
        total_already_present_events,
        total_skipped_source_rows,
        complete,
    }
}

fn reconcile_claude_mem_table(
    table_plan: &ClaudeMemTablePlan,
    limit_per_table: Option<usize>,
    read_events: usize,
    write_counts: ClaudeMemTableWriteCounts,
) -> ClaudeMemTableReconciliation {
    let source_rows = table_plan.rows;
    let skipped_source_rows = source_rows.saturating_sub(read_events);
    ClaudeMemTableReconciliation {
        table: table_plan.table.clone(),
        source_rows,
        read_events,
        planned_events: read_events,
        imported_events: write_counts.imported_events,
        already_present_events: write_counts.already_present_events,
        skipped_source_rows,
        limit_per_table,
        complete: skipped_source_rows == 0,
    }
}

fn claude_mem_event_counts(events: &[MemoryEvent]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        if let Some(table) = claude_mem_event_table(event) {
            *counts.entry(table.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

fn claude_mem_event_table(event: &MemoryEvent) -> Option<&str> {
    if event.imported_from.as_deref() != Some("claude-mem") {
        return None;
    }
    event
        .imported_id
        .as_deref()?
        .split_once(':')
        .map(|(table, _)| table)
}

fn collect_rows<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn join_non_empty(values: impl IntoIterator<Item = Option<String>>) -> String {
    let parts: Vec<String> = values
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    parts.join("\n\n")
}

/// Authored finding-graph node kinds (`#trt1`): human/agent-authored knowledge
/// anchored to code, distinct from passively-captured `MemoryEvent`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthoredNodeKind {
    Finding,
    Decision,
    Note,
}

impl AuthoredNodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Decision => "decision",
            Self::Note => "note",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "finding" => Ok(Self::Finding),
            "decision" => Ok(Self::Decision),
            "note" => Ok(Self::Note),
            other => {
                bail!("unsupported authored node kind `{other}` (expected finding|decision|note)")
            }
        }
    }
}

/// Build a `GraphProjection` for an authored finding/decision/note node
/// (`#trt1`), anchored to a stable symbol handle (graph node id / tagpath — NOT
/// a line number) via an `annotates` edge. `confidence` is clamped to `0..=1`;
/// `observed_at_unix` is the freshness stamp. The node id is content-stable, so
/// re-authoring the same text on the same anchor dedupes instead of duplicating.
pub fn authored_node_projection(
    kind: AuthoredNodeKind,
    text: &str,
    anchor_handle: &str,
    confidence: f64,
    observed_at_unix: i64,
    session_id: Option<&str>,
) -> GraphProjection {
    let mut projection = GraphProjection::default();
    let confidence = confidence.clamp(0.0, 1.0);
    let node_id = format!(
        "{}:{}",
        kind.as_str(),
        blake3::hash(format!("{}|{}|{}", kind.as_str(), anchor_handle, text).as_bytes()).to_hex()
    );
    let label: String = text.chars().take(80).collect();
    let mut node = GraphNode::new(&node_id, kind.as_str(), label)
        .with_property("text", text)
        .with_property("anchor_handle", anchor_handle)
        .with_property("confidence", format!("{confidence:.3}"))
        .with_property("observed_at_unix", observed_at_unix.to_string())
        .with_provenance(GraphProvenance::new("tsift-findings", anchor_handle))
        .with_freshness(GraphFreshness {
            content_hash: None,
            observed_at_unix: Some(observed_at_unix),
        });
    if let Some(session_id) = session_id {
        node = node.with_property("session_id", session_id);
    }
    projection.nodes.push(node);
    projection.edges.push(
        GraphEdge::new(node_id, anchor_handle, "annotates")
            .with_property("authored_kind", kind.as_str())
            .with_provenance(GraphProvenance::new("tsift-findings", anchor_handle)),
    );
    projection
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn handoff_plan_defers_oversized_events_before_model_call() {
        let small = MemoryEvent::new(MemoryEventKind::PromptTarget, "session.md", "short");
        let large = MemoryEvent::new(
            MemoryEventKind::ToolResultArtifact,
            "artifact:log",
            "x".repeat(10_000),
        );
        let plan = plan_capture_handoff(
            &[small, large],
            MemoryBudget {
                max_prompt_tokens: 1000,
                reserve_tokens: 100,
                max_event_tokens: 500,
            },
        );
        assert_eq!(plan.status, "split_required");
        assert_eq!(plan.included_events.len(), 1);
        assert_eq!(
            plan.deferred_events[0].reason,
            "event_exceeds_max_event_tokens"
        );
    }

    #[test]
    fn authored_node_projection_anchors_and_dedupes() {
        let p = authored_node_projection(
            AuthoredNodeKind::Finding,
            "decay ranking ships in tsift-memory",
            "symbol:rank_memory_events",
            1.7, // out of range -> clamps to 1.0
            1_700_000_000,
            Some("sess-1"),
        );
        assert_eq!(p.nodes.len(), 1);
        assert_eq!(p.edges.len(), 1);
        let node = &p.nodes[0];
        assert_eq!(node.kind, "finding");
        assert_eq!(node.properties.get("confidence").unwrap(), "1.000");
        assert_eq!(
            node.properties.get("anchor_handle").unwrap(),
            "symbol:rank_memory_events"
        );
        assert_eq!(p.edges[0].kind, "annotates");
        assert_eq!(p.edges[0].to_id, "symbol:rank_memory_events");
        assert_eq!(node.freshness.as_ref().unwrap().observed_at_unix, Some(1_700_000_000));

        // Same kind+anchor+text -> identical (deduping) node id.
        let again = authored_node_projection(
            AuthoredNodeKind::Finding,
            "decay ranking ships in tsift-memory",
            "symbol:rank_memory_events",
            0.5,
            1_700_000_999,
            None,
        );
        assert_eq!(again.nodes[0].id, node.id);
        assert!(AuthoredNodeKind::parse("note").is_ok());
        assert!(AuthoredNodeKind::parse("bogus").is_err());
    }

    #[test]
    fn memory_store_initializes_schema_and_dedupes_imported_events() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("memory.db");
        let store = MemoryStore::open_or_create(&db).unwrap();
        let event = MemoryEvent::new(
            MemoryEventKind::ImportedObservation,
            "claude-mem:observations:1",
            "fact",
        )
        .with_session_id("session-a")
        .with_import("claude-mem", "observations:1");

        let first = store.insert_event(&event).unwrap();
        let second = store.insert_event(&event).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.event_count().unwrap(), 1);
    }

    #[test]
    fn claude_mem_pending_messages_are_reported_but_not_imported() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("claude-mem.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE pending_messages (
              id INTEGER PRIMARY KEY,
              session_db_id INTEGER NOT NULL,
              content_session_id TEXT NOT NULL,
              message_type TEXT NOT NULL CHECK(message_type IN ('observation', 'summarize')),
              tool_name TEXT,
              tool_input TEXT,
              tool_response TEXT,
              cwd TEXT,
              last_user_message TEXT,
              last_assistant_message TEXT,
              prompt_number INTEGER,
              status TEXT NOT NULL DEFAULT 'pending',
              retry_count INTEGER NOT NULL DEFAULT 0,
              created_at_epoch INTEGER NOT NULL,
              started_processing_at_epoch INTEGER,
              completed_at_epoch INTEGER,
              failed_at_epoch INTEGER
            );
            INSERT INTO pending_messages (
              id, session_db_id, content_session_id, message_type, tool_name,
              tool_input, tool_response, cwd, last_user_message,
              last_assistant_message, prompt_number, status, retry_count,
              created_at_epoch
            ) VALUES (
              1, 10, 'session-a', 'observation', 'bash',
              '{"cmd":"cat large.log"}', 'raw tool response', '/repo',
              'run tests', 'done', 7, 'pending', 0, 1700000000
            );
            "#,
        )
        .unwrap();

        let plan = inspect_claude_mem(&db).unwrap();
        assert_eq!(plan.pending_messages.rows, 1);
        assert!(!plan.pending_messages.supported);
        assert_eq!(
            plan.pending_messages.unsupported_reason.as_deref(),
            Some(PENDING_MESSAGES_EXCLUSION_REASON)
        );
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("pending_messages"))
        );

        let read_report = read_claude_mem_events(&db, Some(100)).unwrap();
        assert!(read_report.events.is_empty());
    }

    #[test]
    fn claude_mem_import_all_reconciles_supported_table_counts() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("claude-mem.db");
        let target = dir.path().join("memory.db");
        let conn = Connection::open(&source).unwrap();
        create_supported_claude_mem_fixture(&conn);

        let dry_run = import_claude_mem(&source, &target, None, true).unwrap();
        assert!(dry_run.import_all);
        assert!(dry_run.reconciliation.complete);
        assert_eq!(dry_run.reconciliation.total_source_rows, 6);
        assert_eq!(dry_run.reconciliation.total_read_events, 6);
        assert_eq!(dry_run.reconciliation.total_skipped_source_rows, 0);
        assert_eq!(dry_run.reconciliation.observations.source_rows, 2);
        assert_eq!(dry_run.reconciliation.session_summaries.source_rows, 1);
        assert_eq!(dry_run.reconciliation.user_prompts.source_rows, 3);

        let applied = import_claude_mem(&source, &target, None, false).unwrap();
        assert_eq!(applied.planned_events, 6);
        assert_eq!(applied.imported_events, 6);
        assert_eq!(applied.already_present_events, 0);
        assert_eq!(applied.event_ids_total, 6);
        assert_eq!(applied.event_ids.len(), 6);
        assert!(!applied.event_ids_truncated);
        assert_eq!(
            MemoryStore::open_or_create(&target)
                .unwrap()
                .event_count()
                .unwrap(),
            6
        );

        let second_apply = import_claude_mem(&source, &target, None, false).unwrap();
        assert_eq!(second_apply.imported_events, 0);
        assert_eq!(second_apply.already_present_events, 6);
        assert_eq!(second_apply.reconciliation.total_already_present_events, 6);
    }

    #[test]
    fn claude_mem_limited_import_reports_skipped_source_rows() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("claude-mem.db");
        let conn = Connection::open(&source).unwrap();
        create_supported_claude_mem_fixture(&conn);

        let read_report = read_claude_mem_events(&source, Some(1)).unwrap();
        assert!(!read_report.import_all);
        assert!(!read_report.reconciliation.complete);
        assert_eq!(read_report.reconciliation.total_source_rows, 6);
        assert_eq!(read_report.reconciliation.total_read_events, 3);
        assert_eq!(read_report.reconciliation.total_skipped_source_rows, 3);
        assert_eq!(
            read_report.reconciliation.observations.skipped_source_rows,
            1
        );
        assert_eq!(
            read_report
                .reconciliation
                .session_summaries
                .skipped_source_rows,
            0
        );
        assert_eq!(
            read_report.reconciliation.user_prompts.skipped_source_rows,
            2
        );
    }

    #[test]
    fn claude_mem_import_caps_reported_event_ids() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("claude-mem.db");
        let target = dir.path().join("memory.db");
        let conn = Connection::open(&source).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE user_prompts (
              id INTEGER PRIMARY KEY,
              content_session_id TEXT NOT NULL,
              prompt_number INTEGER NOT NULL,
              prompt_text TEXT NOT NULL,
              created_at_epoch INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();
        for id in 1..=(MAX_IMPORT_EVENT_IDS + 1) {
            conn.execute(
                "INSERT INTO user_prompts (id, content_session_id, prompt_number, prompt_text, created_at_epoch) VALUES (?1, 'session-a', ?2, ?3, ?4)",
                params![id as i64, id as i64, format!("prompt {id}"), 1700000000_i64 + id as i64],
            )
            .unwrap();
        }

        let applied = import_claude_mem(&source, &target, None, false).unwrap();
        assert_eq!(applied.planned_events, MAX_IMPORT_EVENT_IDS + 1);
        assert_eq!(applied.event_ids_total, MAX_IMPORT_EVENT_IDS + 1);
        assert_eq!(applied.event_ids.len(), MAX_IMPORT_EVENT_IDS);
        assert!(applied.event_ids_truncated);
        assert!(applied.reconciliation.complete);
    }

    fn create_supported_claude_mem_fixture(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE observations (
              id INTEGER PRIMARY KEY,
              memory_session_id TEXT NOT NULL,
              project TEXT NOT NULL,
              type TEXT NOT NULL,
              title TEXT,
              subtitle TEXT,
              text TEXT,
              facts TEXT,
              narrative TEXT,
              concepts TEXT,
              prompt_number INTEGER,
              discovery_tokens INTEGER NOT NULL,
              created_at_epoch INTEGER NOT NULL,
              content_hash TEXT
            );
            CREATE TABLE session_summaries (
              id INTEGER PRIMARY KEY,
              memory_session_id TEXT NOT NULL,
              project TEXT NOT NULL,
              request TEXT,
              investigated TEXT,
              learned TEXT,
              completed TEXT,
              next_steps TEXT,
              notes TEXT,
              prompt_number INTEGER,
              discovery_tokens INTEGER NOT NULL,
              created_at_epoch INTEGER NOT NULL
            );
            CREATE TABLE user_prompts (
              id INTEGER PRIMARY KEY,
              content_session_id TEXT NOT NULL,
              prompt_number INTEGER NOT NULL,
              prompt_text TEXT NOT NULL,
              created_at_epoch INTEGER NOT NULL
            );
            INSERT INTO observations (
              id, memory_session_id, project, type, title, subtitle, text, facts,
              narrative, concepts, prompt_number, discovery_tokens, created_at_epoch,
              content_hash
            ) VALUES
              (1, 'session-a', 'agent-loop', 'fact', 'Title A', NULL, 'Text A', NULL, NULL, 'tsift', 1, 42, 1700000001, 'hash-a'),
              (2, 'session-b', 'agent-loop', 'fact', 'Title B', NULL, 'Text B', NULL, NULL, 'memory', 2, 43, 1700000002, 'hash-b');
            INSERT INTO session_summaries (
              id, memory_session_id, project, request, investigated, learned,
              completed, next_steps, notes, prompt_number, discovery_tokens,
              created_at_epoch
            ) VALUES (
              1, 'session-a', 'agent-loop', 'replace claude-mem', 'tables',
              'all rows', 'imported', 'refresh graph', NULL, 3, 44, 1700000003
            );
            INSERT INTO user_prompts (
              id, content_session_id, prompt_number, prompt_text, created_at_epoch
            ) VALUES
              (1, 'session-a', 1, 'first prompt', 1700000004),
              (2, 'session-a', 2, 'second prompt', 1700000005),
              (3, 'session-b', 1, 'third prompt', 1700000006);
            "#,
        )
        .unwrap();
    }

    #[test]
    fn read_memory_events_round_trips_closeout_events() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("memory.db");
        let store = MemoryStore::open_or_create(&db).unwrap();
        for event in agent_doc_closeout_events(
            Path::new("tasks/software/tsift.md"),
            "do [#tsiftmemhooks]",
            "wired closeout capture",
            Some("abc123"),
            "ok",
        ) {
            store.insert_event(&event).unwrap();
        }

        let events = read_memory_events(&db, 10).unwrap();
        assert!(events.iter().any(|event| {
            event.kind == MemoryEventKind::PromptTarget
                && event.text == "do [#tsiftmemhooks]"
                && event.session_id.as_deref() == Some("tasks/software/tsift.md")
        }));
        assert!(events.iter().any(|event| {
            event.kind == MemoryEventKind::CloseoutProof
                && event.metadata.get("commit_hash") == Some(&"abc123".to_string())
        }));
    }

    #[test]
    fn budget_guard_fails_closed_with_retryable_chunks() {
        let report = guard_memory_handoff(
            MemoryBudgetGuardInput::new("tool.log", "tool_result", "x".repeat(5_000)),
            MemoryBudget {
                max_prompt_tokens: 1000,
                reserve_tokens: 100,
                max_event_tokens: 400,
            },
        );

        assert!(!report.allowed);
        assert_eq!(report.status, "blocked_split_required");
        assert!(report.replacement.is_some());
        assert!(report.retryable_chunk_plan.len() > 1);
        assert!(
            report
                .retryable_chunk_plan
                .iter()
                .all(|chunk| chunk.token_estimate <= 400)
        );
    }

    #[test]
    fn budget_guard_replaces_transcripts_with_session_review_commands() {
        let report = guard_memory_handoff(
            MemoryBudgetGuardInput::new("session.jsonl", "transcript", "x".repeat(5_000)),
            MemoryBudget {
                max_prompt_tokens: 1000,
                reserve_tokens: 100,
                max_event_tokens: 400,
            },
        );
        let replacement = report.replacement.unwrap();
        assert_eq!(
            replacement.strategy,
            "replace_raw_transcript_with_session_review_or_context_pack_handle"
        );
        assert!(
            replacement
                .session_review_command
                .contains("session-review")
        );
        assert!(replacement.context_command.contains("context-pack"));
    }
}
