use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

const SESSION_HEADER_PROBE_BUDGET_BYTES: usize = 256 * 1024;

use crate::{
    session_cost::{
        self, SessionCostFileReadDiagnostic, SessionCostGuardrail, SessionCostGuardrailInput,
        SessionCostLoopCluster, SessionCostPromptCacheRoiScorecard,
    },
    session_digest, session_markdown,
};
use tsift_quality::runtime_churn::RestartChurnSummary;

const MAX_SESSIONS: usize = 12;
const MAX_AGGREGATE_ITEMS: usize = 12;
const MAX_LARGEST_TURNS: usize = 8;
const MAX_WARNINGS: usize = 16;
const MAX_LOOP_CLUSTERS: usize = 12;
const MAX_AGENT_DOC_QUEUE_PROFILE_ROWS: usize = 8;
const MAX_PROMPT_CACHE_ROI_SCORECARD: usize = 12;
/// Per-source candidate budget for session discovery. Each source can collect at
/// most this many most-recent files before content reads. Set generously above
/// `MAX_SESSIONS` so the global top-N after cross-source merge still comes from
/// the genuinely most recent matches even when a source has many rejected
/// candidates near the head.
const MAX_RECENT_CANDIDATES_PER_SOURCE: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct SessionReviewPhaseTiming {
    pub name: String,
    pub duration_micros: u128,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionReviewSession {
    pub source: String,
    pub path: String,
    pub matched_by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_unix_secs: Option<u64>,
    pub prompt_target_count: usize,
    pub command_groups: usize,
    pub file_groups: usize,
    pub symbol_groups: usize,
    pub failure_groups: usize,
    pub runtime_event_groups: usize,
    pub restart_churn_groups: usize,
    pub closeout_groups: usize,
    pub usage_samples: usize,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub largest_turn_total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionReviewCostSummary {
    pub scope: String,
    pub sessions: usize,
    pub usage_samples: usize,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio: Option<f64>,
    pub largest_turn_total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewPromptTarget {
    pub text: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewCommand {
    pub command: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewFileRef {
    pub path: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewSymbolRef {
    pub symbol: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewFailure {
    pub kind: String,
    pub message: String,
    pub occurrences: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewRuntimeEvent {
    pub event: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewCloseout {
    pub kind: String,
    pub detail: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewLargestTurn {
    pub source: String,
    pub session_path: String,
    pub label: String,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewVerificationState {
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionReviewAgentDocExpansionHandle {
    pub handle: String,
    pub label: String,
    pub expand: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionReviewAgentDocQueueProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_queue_prompt: Option<String>,
    pub live_exchange_tail: Vec<String>,
    pub backlog_rows: Vec<String>,
    pub review_rows: Vec<String>,
    pub prompt_presets: Vec<String>,
    pub expansion_handles: Vec<SessionReviewAgentDocExpansionHandle>,
}

impl SessionReviewAgentDocQueueProfile {
    fn is_empty(&self) -> bool {
        self.active_queue_prompt.is_none()
            && self.live_exchange_tail.is_empty()
            && self.backlog_rows.is_empty()
            && self.review_rows.is_empty()
            && self.prompt_presets.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionReviewNextContext {
    pub target: String,
    pub active_prompt_targets: Vec<String>,
    pub last_verification: SessionReviewVerificationState,
    pub touched_files: Vec<String>,
    pub touched_symbols: Vec<String>,
    pub unresolved_failures: Vec<SessionReviewFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_doc_queue: Option<SessionReviewAgentDocQueueProfile>,
    pub next_digest_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionReviewReport {
    pub root: String,
    pub target: String,
    pub target_kind: String,
    pub sessions_considered: usize,
    pub sessions_matched: usize,
    pub claude_sessions: usize,
    pub codex_sessions: usize,
    pub agent_doc_logs: usize,
    pub prompt_target_count: usize,
    pub command_groups: usize,
    pub file_groups: usize,
    pub symbol_groups: usize,
    pub failure_groups: usize,
    pub runtime_event_groups: usize,
    pub restart_churn_groups: usize,
    pub closeout_groups: usize,
    pub usage_samples: usize,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio: Option<f64>,
    pub largest_turn_total_tokens: u64,
    pub aggregate_cost: SessionReviewCostSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_session_cost: Option<SessionReviewCostSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub prompt_cache_roi_scorecard: Vec<SessionCostPromptCacheRoiScorecard>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub guardrails: Vec<SessionCostGuardrail>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub loop_clusters: Vec<SessionCostLoopCluster>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub file_read_diagnostics: Vec<SessionCostFileReadDiagnostic>,
    pub prompt_targets: Vec<SessionReviewPromptTarget>,
    pub commands: Vec<SessionReviewCommand>,
    pub touched_files: Vec<SessionReviewFileRef>,
    pub touched_symbols: Vec<SessionReviewSymbolRef>,
    pub failures: Vec<SessionReviewFailure>,
    pub runtime_events: Vec<SessionReviewRuntimeEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub restart_churn: Vec<RestartChurnSummary>,
    pub closeout: Vec<SessionReviewCloseout>,
    pub largest_turns: Vec<SessionReviewLargestTurn>,
    pub sessions: Vec<SessionReviewSession>,
    pub next_context: SessionReviewNextContext,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionReviewOptions {
    pub claude_projects_dir: Option<PathBuf>,
    pub codex_sessions_dir: Option<PathBuf>,
    pub agent_doc_logs_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewSource {
    ClaudeJsonl,
    CodexJsonl,
    AgentDocLog,
}

impl ReviewSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeJsonl => "claude_jsonl",
            Self::CodexJsonl => "codex_jsonl",
            Self::AgentDocLog => "agent_doc_log",
        }
    }

    fn digest_source(self) -> &'static str {
        match self {
            Self::ClaudeJsonl => "claude-jsonl",
            Self::CodexJsonl => "codex-jsonl",
            Self::AgentDocLog => "agent-doc-log",
        }
    }

    fn supports_cost(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    File,
    Directory,
}

impl TargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Debug, Clone)]
struct TargetContext {
    root: PathBuf,
    canonical_target: PathBuf,
    relative_target: Option<String>,
    kind: TargetKind,
    agent_doc_session: Option<String>,
    path_aliases: BTreeSet<String>,
    session_aliases: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct AgentDocAliases {
    path_aliases: BTreeSet<String>,
    session_aliases: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct MatchSignals {
    cwd: Option<PathBuf>,
    snippets: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct DocumentActiveContext {
    has_live_tail: bool,
    prompt_targets: Vec<String>,
    touched_files: Vec<SessionReviewFileRef>,
    touched_symbols: Vec<SessionReviewSymbolRef>,
    failures: Vec<SessionReviewFailure>,
    agent_doc_queue: Option<SessionReviewAgentDocQueueProfile>,
}

impl DocumentActiveContext {
    fn should_scope_next_context(&self) -> bool {
        self.has_live_tail
            || !self.prompt_targets.is_empty()
            || !self.touched_files.is_empty()
            || !self.touched_symbols.is_empty()
            || !self.failures.is_empty()
    }
}

struct NextContextBuildInput<'a> {
    context: &'a TargetContext,
    active_prompt_targets: Vec<String>,
    touched_files: &'a [SessionReviewFileRef],
    touched_symbols: &'a [SessionReviewSymbolRef],
    failures: &'a [SessionReviewFailure],
    guardrails: &'a [SessionCostGuardrail],
    last_verification: SessionReviewVerificationState,
    agent_doc_queue: Option<SessionReviewAgentDocQueueProfile>,
}

#[derive(Debug, Clone)]
struct PendingSession {
    source: ReviewSource,
    path: PathBuf,
    matched_by: BTreeSet<String>,
    modified_unix_secs: Option<u64>,
    text: String,
}

impl PendingSession {
    fn new(
        source: ReviewSource,
        path: PathBuf,
        matched_by: Vec<String>,
        modified_unix_secs: Option<u64>,
        text: String,
    ) -> Self {
        Self {
            source,
            path,
            matched_by: matched_by.into_iter().collect(),
            modified_unix_secs,
            text,
        }
    }
}

#[derive(Debug, Clone)]
struct FileReadDiagnosticAggregate {
    path: String,
    range: String,
    occurrences: usize,
    estimated_tokens: u64,
    duplicate_estimated_tokens: u64,
    follow_up_commands: BTreeSet<String>,
}

pub fn compute(target: &Path) -> Result<SessionReviewReport> {
    compute_with_options(target, &SessionReviewOptions::default())
}

pub fn compute_with_phases(
    target: &Path,
) -> Result<(SessionReviewReport, Vec<SessionReviewPhaseTiming>)> {
    compute_with_options_and_phases(target, &SessionReviewOptions::default())
}

pub fn compute_with_options(
    target: &Path,
    options: &SessionReviewOptions,
) -> Result<SessionReviewReport> {
    compute_with_options_and_phases(target, options).map(|(report, _phases)| report)
}

pub fn compute_with_options_and_phases(
    target: &Path,
    options: &SessionReviewOptions,
) -> Result<(SessionReviewReport, Vec<SessionReviewPhaseTiming>)> {
    let mut phases: Vec<SessionReviewPhaseTiming> = Vec::with_capacity(6);

    let target_context_started = Instant::now();
    let mut context = build_target_context(target)?;
    let target_context_micros = target_context_started.elapsed().as_micros();

    let session_discovery_started = Instant::now();
    let mut candidates = BTreeMap::<String, PendingSession>::new();
    let mut sessions_considered = 0_usize;
    let mut warnings = Vec::new();

    let agent_doc_logs_dir = resolve_agent_doc_logs_dir(&context.root, options);
    if let Some(session_name) = &context.agent_doc_session {
        let session_log = agent_doc_logs_dir.join(format!("{session_name}.log"));
        if session_log.is_file()
            && let Ok(text) = fs::read_to_string(&session_log)
        {
            let aliases = collect_agent_doc_aliases(&text, &context.root);
            context.path_aliases.extend(aliases.path_aliases);
            context.session_aliases.extend(aliases.session_aliases);
        }
    }

    if agent_doc_logs_dir.is_dir() {
        for path in collect_files_with_extension(&agent_doc_logs_dir, "log")? {
            sessions_considered += 1;
            maybe_add_agent_doc_candidate(&mut candidates, &context, &path)?;
        }
    }

    let claude_projects_dir = resolve_claude_projects_dir(&context.root, options);
    let claude_project_dir = claude_projects_dir.join(claude_project_slug(&context.root));
    if claude_project_dir.is_dir() {
        for path in collect_recent_files_with_extension(
            &claude_project_dir,
            "jsonl",
            MAX_RECENT_CANDIDATES_PER_SOURCE,
        )? {
            sessions_considered += 1;
            maybe_add_claude_candidate(&mut candidates, &context, &path)?;
        }
    }

    let codex_sessions_dir = resolve_codex_sessions_dir(&context.root, options);
    if codex_sessions_dir.is_dir() {
        for path in collect_recent_files_with_extension(
            &codex_sessions_dir,
            "jsonl",
            MAX_RECENT_CANDIDATES_PER_SOURCE,
        )? {
            sessions_considered += 1;
            maybe_add_codex_candidate(&mut candidates, &context, &path)?;
        }
    }

    let mut sessions = candidates.into_values().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .modified_unix_secs
            .cmp(&left.modified_unix_secs)
            .then_with(|| left.path.cmp(&right.path))
    });
    sessions.truncate(MAX_SESSIONS);
    let session_discovery_micros = session_discovery_started.elapsed().as_micros();

    let mut session_digest_micros: u128 = 0;
    let mut session_cost_micros: u128 = 0;
    let session_loop_started = Instant::now();

    let mut prompt_targets = BTreeMap::<String, usize>::new();
    let mut commands = BTreeMap::<String, usize>::new();
    let mut touched_files = BTreeMap::<String, usize>::new();
    let mut touched_symbols = BTreeMap::<String, usize>::new();
    let mut failures = BTreeMap::<(String, String, Option<String>, Option<String>), usize>::new();
    let mut runtime_events = BTreeMap::<String, usize>::new();
    let mut closeout = BTreeMap::<(String, String), usize>::new();
    let mut restart_churn = BTreeMap::<String, RestartChurnSummary>::new();
    let mut aggregate_runtime_events = BTreeMap::<String, usize>::new();
    let mut loop_clusters = BTreeMap::<(String, String), (usize, usize)>::new();
    let mut file_read_diagnostics =
        BTreeMap::<(String, String), FileReadDiagnosticAggregate>::new();
    let mut largest_turns = Vec::<SessionReviewLargestTurn>::new();
    let mut prompt_cache_roi_scorecard = Vec::<SessionCostPromptCacheRoiScorecard>::new();
    let mut session_rows = Vec::<SessionReviewSession>::new();

    let mut claude_sessions = 0_usize;
    let mut codex_sessions = 0_usize;
    let mut agent_doc_logs = 0_usize;
    let mut prompt_target_count = 0_usize;
    let mut command_groups = 0_usize;
    let mut file_groups = 0_usize;
    let mut symbol_groups = 0_usize;
    let mut failure_groups = 0_usize;
    let mut runtime_event_groups = 0_usize;
    let mut restart_churn_groups = 0_usize;
    let mut closeout_groups = 0_usize;
    let mut usage_samples = 0_usize;
    let mut prompt_tokens = 0_u64;
    let mut cached_input_tokens = 0_u64;
    let mut cache_creation_input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut reasoning_output_tokens = 0_u64;
    let mut total_tokens = 0_u64;
    let mut largest_turn_total_tokens = 0_u64;
    let mut last_verification = None::<SessionReviewVerificationState>;

    for pending in sessions {
        let digest_started = Instant::now();
        let digest = session_digest::compute(
            &context.root,
            &pending.text,
            Some(pending.source.digest_source()),
        )
        .with_context(|| format!("digesting {}", pending.path.display()))?;
        session_digest_micros += digest_started.elapsed().as_micros();
        let cost_started = Instant::now();
        let cost = if pending.source.supports_cost() {
            Some(
                session_cost::compute(&pending.text, Some(pending.source.digest_source()))
                    .with_context(|| format!("costing {}", pending.path.display()))?,
            )
        } else {
            None
        };
        session_cost_micros += cost_started.elapsed().as_micros();

        match pending.source {
            ReviewSource::ClaudeJsonl => claude_sessions += 1,
            ReviewSource::CodexJsonl => codex_sessions += 1,
            ReviewSource::AgentDocLog => agent_doc_logs += 1,
        }

        if last_verification.is_none()
            && let Some(entry) = digest
                .closeout
                .iter()
                .find(|entry| entry.kind == "verification")
        {
            last_verification = Some(SessionReviewVerificationState {
                status: "passed".to_string(),
                detail: entry.detail.clone(),
            });
        }

        prompt_target_count += digest.prompt_target_count;
        command_groups += digest.command_groups;
        file_groups += digest.file_groups;
        symbol_groups += digest.symbol_groups;
        failure_groups += digest.failure_groups;
        runtime_event_groups += digest.runtime_event_groups;
        restart_churn_groups += digest.restart_churn_groups;
        closeout_groups += digest.closeout_groups;

        for prompt in &digest.prompt_targets {
            *prompt_targets.entry(prompt.clone()).or_default() += 1;
        }
        for command in &digest.commands {
            *commands.entry(command.command.clone()).or_default() += command.occurrences;
        }
        for file_ref in &digest.touched_files {
            *touched_files.entry(file_ref.path.clone()).or_default() += file_ref.occurrences;
        }
        for symbol_ref in &digest.touched_symbols {
            *touched_symbols
                .entry(symbol_ref.symbol.clone())
                .or_default() += symbol_ref.occurrences;
        }
        for failure in &digest.failures {
            *failures
                .entry((
                    failure.kind.clone(),
                    failure.message.clone(),
                    failure.command.clone(),
                    Some(pending.path.display().to_string()),
                ))
                .or_default() += failure.occurrences;
        }
        for event in &digest.runtime_events {
            *runtime_events.entry(event.event.clone()).or_default() += event.occurrences;
            *aggregate_runtime_events
                .entry(event.event.clone())
                .or_default() += event.occurrences;
        }
        for entry in &digest.closeout {
            *closeout
                .entry((entry.kind.clone(), entry.detail.clone()))
                .or_default() += entry.occurrences;
        }
        for churn in &digest.restart_churn {
            restart_churn
                .entry(churn.family.clone())
                .and_modify(|existing| {
                    existing.occurrences += churn.occurrences;
                    if let Some(churn_max) = churn.max_restart_count {
                        existing.max_restart_count = Some(
                            existing
                                .max_restart_count
                                .map_or(churn_max, |current| current.max(churn_max)),
                        );
                    }
                    if churn.sample.len() > existing.sample.len() {
                        existing.sample = churn.sample.clone();
                    }
                })
                .or_insert_with(|| churn.clone());
        }

        if let Some(cost) = &cost {
            usage_samples += cost.usage_samples;
            prompt_tokens += cost.prompt_tokens;
            cached_input_tokens += cost.cached_input_tokens;
            cache_creation_input_tokens += cost.cache_creation_input_tokens;
            output_tokens += cost.output_tokens;
            reasoning_output_tokens += cost.reasoning_output_tokens;
            total_tokens += cost.total_tokens;
            largest_turn_total_tokens =
                largest_turn_total_tokens.max(cost.largest_turn_total_tokens);
            for turn in &cost.largest_turns {
                largest_turns.push(SessionReviewLargestTurn {
                    source: pending.source.as_str().to_string(),
                    session_path: pending.path.display().to_string(),
                    label: turn.label.clone(),
                    prompt_tokens: turn.prompt_tokens,
                    cached_input_tokens: turn.cached_input_tokens,
                    cache_creation_input_tokens: turn.cache_creation_input_tokens,
                    output_tokens: turn.output_tokens,
                    reasoning_output_tokens: turn.reasoning_output_tokens,
                    total_tokens: turn.total_tokens,
                });
            }
            let session_path = pending.path.display().to_string();
            let next_command = format!(
                "tsift session-cost --source {} --input {} --json",
                pending.source.digest_source(),
                shell_quote(&session_path)
            );
            prompt_cache_roi_scorecard.extend(session_cost::prompt_cache_scorecard_for_session(
                cost,
                pending.source.as_str(),
                &session_path,
                &next_command,
            ));
            for cluster in &cost.loop_clusters {
                let entry = loop_clusters
                    .entry((cluster.kind.clone(), cluster.label.clone()))
                    .or_insert((0, 0));
                entry.0 += cluster.occurrences;
                entry.1 = entry.1.max(cluster.max_consecutive);
            }
            for diagnostic in &cost.file_read_diagnostics {
                let entry = file_read_diagnostics
                    .entry((diagnostic.path.clone(), diagnostic.range.clone()))
                    .or_insert_with(|| FileReadDiagnosticAggregate {
                        path: diagnostic.path.clone(),
                        range: diagnostic.range.clone(),
                        occurrences: 0,
                        estimated_tokens: 0,
                        duplicate_estimated_tokens: 0,
                        follow_up_commands: BTreeSet::new(),
                    });
                entry.occurrences += diagnostic.occurrences;
                entry.estimated_tokens = entry
                    .estimated_tokens
                    .saturating_add(diagnostic.estimated_tokens);
                entry.duplicate_estimated_tokens = entry
                    .duplicate_estimated_tokens
                    .saturating_add(diagnostic.duplicate_estimated_tokens);
                entry
                    .follow_up_commands
                    .extend(diagnostic.follow_up_commands.iter().cloned());
            }
        }

        for warning in digest.warnings.iter().chain(
            cost.as_ref()
                .map(|report| report.warnings.iter())
                .into_iter()
                .flatten(),
        ) {
            warnings.push(format!("{}: {}", pending.path.display(), warning));
        }

        session_rows.push(SessionReviewSession {
            source: pending.source.as_str().to_string(),
            path: pending.path.display().to_string(),
            matched_by: pending.matched_by.into_iter().collect(),
            modified_unix_secs: pending.modified_unix_secs,
            prompt_target_count: digest.prompt_target_count,
            command_groups: digest.command_groups,
            file_groups: digest.file_groups,
            symbol_groups: digest.symbol_groups,
            failure_groups: digest.failure_groups,
            runtime_event_groups: digest.runtime_event_groups,
            restart_churn_groups: digest.restart_churn_groups,
            closeout_groups: digest.closeout_groups,
            usage_samples: cost.as_ref().map_or(0, |report| report.usage_samples),
            prompt_tokens: cost.as_ref().map_or(0, |report| report.prompt_tokens),
            cached_input_tokens: cost.as_ref().map_or(0, |report| report.cached_input_tokens),
            cache_creation_input_tokens: cost
                .as_ref()
                .map_or(0, |report| report.cache_creation_input_tokens),
            output_tokens: cost.as_ref().map_or(0, |report| report.output_tokens),
            reasoning_output_tokens: cost
                .as_ref()
                .map_or(0, |report| report.reasoning_output_tokens),
            total_tokens: cost.as_ref().map_or(0, |report| report.total_tokens),
            largest_turn_total_tokens: cost
                .as_ref()
                .map_or(0, |report| report.largest_turn_total_tokens),
        });
    }
    let session_loop_total_micros = session_loop_started.elapsed().as_micros();
    let session_aggregation_micros = session_loop_total_micros
        .saturating_sub(session_digest_micros)
        .saturating_sub(session_cost_micros);
    let report_assembly_started = Instant::now();

    let cached_input_ratio = (prompt_tokens > 0).then_some(
        ((cached_input_tokens as f64) / (prompt_tokens as f64) * 10_000.0).round() / 100.0,
    );
    let largest_prompt_turn = largest_turns
        .iter()
        .max_by(|left, right| {
            left.prompt_tokens
                .cmp(&right.prompt_tokens)
                .then(left.label.cmp(&right.label))
        })
        .cloned();
    let guardrails = session_cost::derive_guardrails(&SessionCostGuardrailInput {
        largest_prompt_turn_tokens: largest_prompt_turn
            .as_ref()
            .map_or(0, |turn| turn.prompt_tokens),
        largest_prompt_turn_label: largest_prompt_turn.as_ref().map(|turn| turn.label.clone()),
        prompt_tokens,
        cached_input_ratio,
        fresh_restart_occurrences: restart_churn
            .get("fresh_restart")
            .map_or(0, |entry| entry.occurrences),
        auto_trigger_timeout_occurrences: restart_churn
            .get("auto_trigger_timeout")
            .map_or(0, |entry| entry.occurrences),
        ctrl_d_restart_loop_occurrences: restart_churn
            .get("ctrl_d_restart_loop")
            .map_or(0, |entry| entry.occurrences),
        noop_closeout_occurrences: aggregate_runtime_events
            .get("commit_already_current")
            .copied()
            .unwrap_or(0),
        max_restart_count: restart_churn
            .values()
            .filter_map(|entry| entry.max_restart_count)
            .max(),
    });

    largest_turns.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then(right.prompt_tokens.cmp(&left.prompt_tokens))
            .then(left.session_path.cmp(&right.session_path))
            .then(left.label.cmp(&right.label))
    });
    largest_turns.truncate(MAX_LARGEST_TURNS);
    prompt_cache_roi_scorecard.truncate(MAX_PROMPT_CACHE_ROI_SCORECARD);

    session_rows.truncate(MAX_SESSIONS);
    let prompt_targets =
        collect_strings(prompt_targets, MAX_AGGREGATE_ITEMS, |text, occurrences| {
            SessionReviewPromptTarget { text, occurrences }
        });
    let commands = collect_strings(commands, MAX_AGGREGATE_ITEMS, |command, occurrences| {
        SessionReviewCommand {
            command,
            occurrences,
        }
    });
    let touched_files = collect_strings(touched_files, MAX_AGGREGATE_ITEMS, |path, occurrences| {
        SessionReviewFileRef { path, occurrences }
    });
    let touched_symbols = collect_strings(
        touched_symbols,
        MAX_AGGREGATE_ITEMS,
        |symbol, occurrences| SessionReviewSymbolRef {
            symbol,
            occurrences,
        },
    );
    let failures = collect_pairs(
        failures,
        MAX_AGGREGATE_ITEMS,
        |(kind, message, command, session_path), occurrences| SessionReviewFailure {
            kind,
            message,
            occurrences,
            command,
            session_path,
        },
    );
    let runtime_events =
        collect_strings(runtime_events, MAX_AGGREGATE_ITEMS, |event, occurrences| {
            SessionReviewRuntimeEvent { event, occurrences }
        });
    let restart_churn = collect_restart_churn(restart_churn, MAX_AGGREGATE_ITEMS);
    let closeout = collect_pairs(
        closeout,
        MAX_AGGREGATE_ITEMS,
        |(kind, detail), occurrences| SessionReviewCloseout {
            kind,
            detail,
            occurrences,
        },
    );
    let loop_clusters = collect_loop_clusters(loop_clusters, MAX_LOOP_CLUSTERS);
    let file_read_diagnostics =
        collect_file_read_diagnostics(file_read_diagnostics, MAX_AGGREGATE_ITEMS);
    let aggregate_cost = SessionReviewCostSummary {
        scope: "bounded_matched_sessions".to_string(),
        sessions: session_rows.len(),
        usage_samples,
        prompt_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        cached_input_ratio,
        largest_turn_total_tokens,
    };
    let latest_session_cost = session_rows
        .first()
        .map(|session| SessionReviewCostSummary {
            scope: "latest_matched_session".to_string(),
            sessions: 1,
            usage_samples: session.usage_samples,
            prompt_tokens: session.prompt_tokens,
            cached_input_tokens: session.cached_input_tokens,
            cache_creation_input_tokens: session.cache_creation_input_tokens,
            output_tokens: session.output_tokens,
            reasoning_output_tokens: session.reasoning_output_tokens,
            total_tokens: session.total_tokens,
            cached_input_ratio: (session.prompt_tokens > 0).then_some(
                ((session.cached_input_tokens as f64) / (session.prompt_tokens as f64) * 10_000.0)
                    .round()
                    / 100.0,
            ),
            largest_turn_total_tokens: session.largest_turn_total_tokens,
        });
    let document_active_context = match collect_document_active_context(&context) {
        Ok(active_context) => active_context,
        Err(error) => {
            warnings.push(format!(
                "{}: could not extract live document active context: {error:#}",
                context.canonical_target.display()
            ));
            DocumentActiveContext::default()
        }
    };
    let (active_prompt_targets, next_context_files, next_context_symbols, next_context_failures) =
        if document_active_context.should_scope_next_context() {
            (
                document_active_context.prompt_targets.clone(),
                document_active_context.touched_files.clone(),
                document_active_context.touched_symbols.clone(),
                document_active_context.failures.clone(),
            )
        } else {
            (
                prompt_targets
                    .iter()
                    .map(|entry| entry.text.clone())
                    .collect(),
                touched_files.clone(),
                touched_symbols.clone(),
                failures.clone(),
            )
        };
    let next_context = build_next_context(NextContextBuildInput {
        context: &context,
        active_prompt_targets,
        touched_files: &next_context_files,
        touched_symbols: &next_context_symbols,
        failures: &next_context_failures,
        guardrails: &guardrails,
        last_verification: last_verification.unwrap_or_else(|| SessionReviewVerificationState {
            status: "missing".to_string(),
            detail: "no verification closeout found in matched sessions".to_string(),
        }),
        agent_doc_queue: document_active_context.agent_doc_queue,
    });
    warnings.sort();
    warnings.truncate(MAX_WARNINGS);

    let report = SessionReviewReport {
        root: context.root.display().to_string(),
        target: context.canonical_target.display().to_string(),
        target_kind: context.kind.as_str().to_string(),
        sessions_considered,
        sessions_matched: session_rows.len(),
        claude_sessions,
        codex_sessions,
        agent_doc_logs,
        prompt_target_count,
        command_groups,
        file_groups,
        symbol_groups,
        failure_groups,
        runtime_event_groups,
        restart_churn_groups,
        closeout_groups,
        usage_samples,
        prompt_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        cached_input_ratio,
        largest_turn_total_tokens,
        aggregate_cost,
        latest_session_cost,
        prompt_cache_roi_scorecard,
        guardrails,
        loop_clusters,
        file_read_diagnostics,
        prompt_targets,
        commands,
        touched_files,
        touched_symbols,
        failures,
        runtime_events,
        restart_churn,
        closeout,
        largest_turns,
        sessions: session_rows,
        next_context,
        warnings,
    };
    let report_assembly_micros = report_assembly_started.elapsed().as_micros();

    phases.push(SessionReviewPhaseTiming {
        name: "target_context_build".to_string(),
        duration_micros: target_context_micros,
        detail:
            "build target context (root, canonical target, kind, aliases) before session discovery"
                .to_string(),
    });
    phases.push(SessionReviewPhaseTiming {
        name: "session_discovery".to_string(),
        duration_micros: session_discovery_micros,
        detail: "agent-doc + Claude JSONL + Codex JSONL session candidate discovery and ranking"
            .to_string(),
    });
    phases.push(SessionReviewPhaseTiming {
        name: "session_digest_total".to_string(),
        duration_micros: session_digest_micros,
        detail: "sum of session_digest::compute across matched sessions".to_string(),
    });
    phases.push(SessionReviewPhaseTiming {
        name: "session_cost_total".to_string(),
        duration_micros: session_cost_micros,
        detail: "sum of session_cost::compute across matched sessions".to_string(),
    });
    phases.push(SessionReviewPhaseTiming {
        name: "session_aggregation".to_string(),
        duration_micros: session_aggregation_micros,
        detail: "per-session prompt/file/symbol/failure aggregation into bounded BTreeMaps"
            .to_string(),
    });
    phases.push(SessionReviewPhaseTiming {
        name: "report_assembly".to_string(),
        duration_micros: report_assembly_micros,
        detail: "post-loop collect_strings + sort + next-context derivation + report construction"
            .to_string(),
    });

    Ok((report, phases))
}

fn build_target_context(target: &Path) -> Result<TargetContext> {
    let canonical_target = target
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", target.display()))?;
    let root = tsift_quality::lint::resolve_harness_root_or_canonical_path(target)?;
    let kind = if canonical_target.is_dir() {
        TargetKind::Directory
    } else if canonical_target.is_file() {
        TargetKind::File
    } else {
        bail!(
            "target `{}` is neither a file nor a directory",
            canonical_target.display()
        );
    };

    let relative_target = canonical_target
        .strip_prefix(&root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"));
    let agent_doc_session = (kind == TargetKind::File)
        .then(|| session_markdown::session_id_from_path(&canonical_target))
        .transpose()?
        .flatten();

    let mut path_aliases = BTreeSet::new();
    path_aliases.insert(canonical_target.display().to_string());
    if let Some(relative) = &relative_target {
        path_aliases.insert(relative.clone());
    }
    let mut session_aliases = BTreeSet::new();
    if let Some(session) = &agent_doc_session {
        session_aliases.insert(session.clone());
    }

    Ok(TargetContext {
        root,
        canonical_target,
        relative_target,
        kind,
        agent_doc_session,
        path_aliases,
        session_aliases,
    })
}

fn build_next_context(input: NextContextBuildInput<'_>) -> SessionReviewNextContext {
    let NextContextBuildInput {
        context,
        active_prompt_targets,
        touched_files,
        touched_symbols,
        failures,
        guardrails,
        last_verification,
        agent_doc_queue,
    } = input;
    let target = context
        .relative_target
        .clone()
        .unwrap_or_else(|| context.canonical_target.display().to_string());
    let session_target = match context.kind {
        TargetKind::Directory => ".".to_string(),
        TargetKind::File => target.clone(),
    };

    let mut unresolved_failures = failures.to_vec();
    unresolved_failures.extend(guardrail_next_context_failures(guardrails));
    let mut next_digest_commands = vec![
        format!(
            "tsift session-review --next-context {}",
            shell_quote(&session_target)
        ),
        "tsift diff-digest .".to_string(),
        "tsift test-digest --path . < test.log".to_string(),
        "tsift log-digest --path . < build.log".to_string(),
    ];
    let graph_targets = extract_backlog_refs(&active_prompt_targets);
    for target in &graph_targets {
        next_digest_commands.push(format!(
            "tsift graph-db --path . evidence {} --depth 3 --limit 8 --json",
            shell_quote(target)
        ));
    }
    if !graph_targets.is_empty() {
        next_digest_commands.push(format!(
            "tsift conflict-matrix --path {} {} --json",
            shell_quote(&session_target),
            graph_targets
                .iter()
                .map(|target| shell_quote(target))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    SessionReviewNextContext {
        target,
        active_prompt_targets,
        last_verification,
        touched_files: touched_files
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        touched_symbols: touched_symbols
            .iter()
            .map(|entry| entry.symbol.clone())
            .collect(),
        unresolved_failures,
        agent_doc_queue,
        next_digest_commands,
    }
}

fn extract_backlog_refs(inputs: &[String]) -> Vec<String> {
    let mut refs = Vec::new();
    let mut seen = BTreeSet::new();
    for input in inputs {
        for token in input.split(|ch: char| {
            !(ch.is_ascii_alphanumeric()
                || ch == '#'
                || ch == '_'
                || ch == '-'
                || ch == '['
                || ch == ']')
        }) {
            let Some(hash) = token.find('#') else {
                continue;
            };
            let normalized = token[hash + 1..]
                .trim()
                .trim_matches(|ch: char| matches!(ch, '[' | ']'))
                .trim();
            if !normalized.is_empty() && seen.insert(normalized.to_string()) {
                refs.push(normalized.to_string());
            }
        }
    }
    refs
}

fn guardrail_next_context_failures(
    guardrails: &[SessionCostGuardrail],
) -> impl Iterator<Item = SessionReviewFailure> + '_ {
    guardrails.iter().map(|guardrail| SessionReviewFailure {
        kind: format!("guardrail:{}", guardrail.kind),
        message: format!("{} Guidance: {}", guardrail.message, guardrail.guidance),
        occurrences: 1,
        command: None,
        session_path: None,
    })
}

fn collect_document_active_context(context: &TargetContext) -> Result<DocumentActiveContext> {
    if context.kind != TargetKind::File {
        return Ok(DocumentActiveContext::default());
    }
    let content = fs::read_to_string(&context.canonical_target).with_context(|| {
        format!(
            "reading target document {}",
            context.canonical_target.display()
        )
    })?;
    let tail = extract_agent_component(&content, "exchange")
        .map(active_exchange_tail)
        .unwrap_or_default();
    let agent_doc_queue = collect_agent_doc_queue_profile(&content, context, &tail);
    let has_live_tail = has_meaningful_live_tail(&tail);
    if !has_live_tail {
        let queue_prompt_target = agent_doc_queue
            .as_ref()
            .and_then(|profile| profile.active_queue_prompt.clone())
            .into_iter()
            .collect();
        return Ok(DocumentActiveContext {
            has_live_tail,
            prompt_targets: queue_prompt_target,
            touched_files: Vec::new(),
            touched_symbols: Vec::new(),
            failures: Vec::new(),
            agent_doc_queue,
        });
    }
    let digest = session_digest::compute(&context.root, &tail, Some("markdown"))?;
    let fallback_prompt_targets = if digest.prompt_targets.is_empty() {
        collect_live_tail_prompt_lines(&tail)
    } else {
        Vec::new()
    };
    let queue_prompt_target =
        if digest.prompt_targets.is_empty() && fallback_prompt_targets.is_empty() {
            agent_doc_queue
                .as_ref()
                .and_then(|profile| profile.active_queue_prompt.clone())
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
    Ok(DocumentActiveContext {
        has_live_tail,
        prompt_targets: if digest.prompt_targets.is_empty() {
            if fallback_prompt_targets.is_empty() {
                queue_prompt_target
            } else {
                fallback_prompt_targets
            }
        } else {
            digest.prompt_targets
        },
        touched_files: digest
            .touched_files
            .into_iter()
            .map(|entry| SessionReviewFileRef {
                path: entry.path,
                occurrences: entry.occurrences,
            })
            .collect(),
        touched_symbols: digest
            .touched_symbols
            .into_iter()
            .map(|entry| SessionReviewSymbolRef {
                symbol: entry.symbol,
                occurrences: entry.occurrences,
            })
            .collect(),
        failures: digest
            .failures
            .into_iter()
            .map(|entry| SessionReviewFailure {
                kind: entry.kind,
                message: entry.message,
                occurrences: entry.occurrences,
                command: entry.command,
                session_path: context
                    .relative_target
                    .clone()
                    .or_else(|| Some(context.canonical_target.display().to_string())),
            })
            .collect(),
        agent_doc_queue,
    })
}

fn collect_live_tail_prompt_lines(tail: &str) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut buffer = Vec::new();
    for raw_line in tail.lines() {
        let Some(line) = meaningful_live_tail_line(raw_line) else {
            if !buffer.is_empty() {
                prompts.push(buffer.join(" "));
                buffer.clear();
            }
            continue;
        };
        buffer.push(line.to_string());
    }
    if !buffer.is_empty() {
        prompts.push(buffer.join(" "));
    }
    prompts
}

fn has_meaningful_live_tail(tail: &str) -> bool {
    tail.lines()
        .any(|line| meaningful_live_tail_line(line).is_some())
}

fn meaningful_live_tail_line(line: &str) -> Option<&str> {
    let trimmed = line
        .trim()
        .strip_prefix("❯ ")
        .or_else(|| line.trim().strip_prefix("> "))
        .unwrap_or_else(|| line.trim())
        .trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("###")
        || trimmed == "#"
        || trimmed == "---"
    {
        return None;
    }
    Some(trimmed)
}

fn extract_agent_component<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    let open_prefix = format!("<!-- agent:{name}");
    let close_marker = format!("<!-- /agent:{name} -->");
    let open_start = content.find(&open_prefix)?;
    let after_open = content[open_start..].find("-->")? + open_start + 3;
    let close_start = content[after_open..].find(&close_marker)? + after_open;
    Some(&content[after_open..close_start])
}

fn active_exchange_tail(exchange: &str) -> String {
    let mut start = 0;
    for (index, _) in exchange.match_indices("<!-- agent:boundary:") {
        let marker_tail = &exchange[index..];
        let marker_end = marker_tail
            .find("-->")
            .map(|offset| index + offset + 3)
            .unwrap_or(index);
        start = marker_end;
    }
    let after_boundary = &exchange[start..];
    let mut response_seen = false;
    let mut prompt_region = String::new();
    for line in after_boundary.lines() {
        if line.trim_start().starts_with("### Re:") {
            response_seen = true;
            prompt_region.clear();
            continue;
        }
        if !response_seen
            || line.trim_start().starts_with("❯ ")
            || line.trim_start().starts_with("> ")
        {
            prompt_region.push_str(line);
            prompt_region.push('\n');
        }
    }
    prompt_region
}

fn collect_agent_doc_queue_profile(
    content: &str,
    context: &TargetContext,
    live_tail: &str,
) -> Option<SessionReviewAgentDocQueueProfile> {
    let queue_rows = extract_agent_component(content, "queue")
        .map(collect_agent_doc_component_rows)
        .unwrap_or_default();
    let backlog_rows = extract_agent_component(content, "backlog")
        .map(collect_agent_doc_component_rows)
        .unwrap_or_default();
    let review_rows = extract_agent_component(content, "review")
        .map(collect_agent_doc_component_rows)
        .unwrap_or_default();
    let prompt_presets = collect_agent_doc_prompt_presets(content);
    let live_exchange_tail = collect_meaningful_live_tail_lines(live_tail);

    let backlog_by_ref = backlog_rows
        .iter()
        .filter_map(|row| extract_first_backlog_ref(row).map(|id| (id, row.clone())))
        .collect::<BTreeMap<_, _>>();
    let active_queue_prompt = queue_rows.first().map(|queue_row| {
        extract_first_backlog_ref(queue_row)
            .and_then(|id| backlog_by_ref.get(&id).cloned())
            .unwrap_or_else(|| queue_row.clone())
    });

    let mut profile = SessionReviewAgentDocQueueProfile {
        active_queue_prompt,
        live_exchange_tail,
        backlog_rows,
        review_rows,
        prompt_presets,
        expansion_handles: Vec::new(),
    };
    if profile.is_empty() {
        return None;
    }
    profile.expansion_handles = agent_doc_queue_expansion_handles(context);
    Some(profile)
}

fn collect_agent_doc_component_rows(component: &str) -> Vec<String> {
    component
        .lines()
        .filter_map(normalize_agent_doc_component_row)
        .take(MAX_AGENT_DOC_QUEUE_PROFILE_ROWS)
        .collect()
}

fn normalize_agent_doc_component_row(raw_line: &str) -> Option<String> {
    let mut line = raw_line.trim();
    if line.is_empty() || line.starts_with("<!--") {
        return None;
    }
    if let Some(rest) = line.strip_prefix("- ") {
        line = rest.trim();
    }
    if line.starts_with("~~") || line.ends_with("~~") {
        return None;
    }
    if let Some(rest) = line.strip_prefix("[ ]") {
        line = rest.trim();
    } else if line.starts_with("[x]") || line.starts_with("[X]") {
        return None;
    }
    if line.is_empty() || line.starts_with("~~") {
        return None;
    }
    Some(collapse_inline_whitespace(line))
}

fn collect_meaningful_live_tail_lines(tail: &str) -> Vec<String> {
    tail.lines()
        .filter_map(meaningful_live_tail_line)
        .map(collapse_inline_whitespace)
        .take(MAX_AGENT_DOC_QUEUE_PROFILE_ROWS)
        .collect()
}

fn collect_agent_doc_prompt_presets(content: &str) -> Vec<String> {
    let Some(frontmatter) = extract_frontmatter(content) else {
        return Vec::new();
    };
    let mut in_prompt_presets = false;
    let mut presets = Vec::new();
    for raw_line in frontmatter.lines() {
        let trimmed = raw_line.trim();
        if trimmed == "prompt_presets:" {
            in_prompt_presets = true;
            continue;
        }
        if !in_prompt_presets {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !raw_line.starts_with(char::is_whitespace) {
            break;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('\'').trim_matches('"');
        if !key.starts_with('#') {
            continue;
        }
        let value = value.trim().trim_matches('\'').trim_matches('"');
        let preset = if value.is_empty() {
            key.to_string()
        } else {
            format!("{key}: {}", collapse_inline_whitespace(value))
        };
        presets.push(preset);
        if presets.len() >= MAX_AGENT_DOC_QUEUE_PROFILE_ROWS {
            break;
        }
    }
    presets
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn extract_first_backlog_ref(text: &str) -> Option<String> {
    extract_backlog_refs(&[text.to_string()]).into_iter().next()
}

fn agent_doc_queue_expansion_handles(
    context: &TargetContext,
) -> Vec<SessionReviewAgentDocExpansionHandle> {
    let target = context
        .relative_target
        .clone()
        .unwrap_or_else(|| context.canonical_target.display().to_string());
    vec![
        SessionReviewAgentDocExpansionHandle {
            handle: "adq-next-context".to_string(),
            label: "refresh next-context".to_string(),
            expand: format!(
                "tsift --envelope session-review {} --next-context --budget normal",
                shell_quote(&target)
            ),
        },
        SessionReviewAgentDocExpansionHandle {
            handle: "adq-context-pack".to_string(),
            label: "refresh context-pack".to_string(),
            expand: format!(
                "tsift --envelope context-pack {} --budget normal",
                shell_quote(&target)
            ),
        },
        SessionReviewAgentDocExpansionHandle {
            handle: "adq-document".to_string(),
            label: "expand document".to_string(),
            expand: format!(
                "tsift --envelope source-read {} --budget normal",
                shell_quote(&target)
            ),
        },
    ]
}

fn collapse_inline_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_claude_projects_dir(root: &Path, options: &SessionReviewOptions) -> PathBuf {
    options
        .claude_projects_dir
        .clone()
        .or_else(|| home_dir(root).map(|home| home.join(".claude/projects")))
        .unwrap_or_else(|| PathBuf::from(".claude/projects"))
}

fn resolve_codex_sessions_dir(root: &Path, options: &SessionReviewOptions) -> PathBuf {
    options
        .codex_sessions_dir
        .clone()
        .or_else(|| home_dir(root).map(|home| home.join(".codex/sessions")))
        .unwrap_or_else(|| PathBuf::from(".codex/sessions"))
}

fn resolve_agent_doc_logs_dir(root: &Path, options: &SessionReviewOptions) -> PathBuf {
    options
        .agent_doc_logs_dir
        .clone()
        .unwrap_or_else(|| root.join(".agent-doc/logs"))
}

fn home_dir(root: &Path) -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| {
        let root_home = root.components().take(3).collect::<PathBuf>();
        root_home.starts_with("/home").then_some(root_home)
    })
}

fn claude_project_slug(root: &Path) -> String {
    root.display().to_string().replace('/', "-")
}

fn collect_agent_doc_aliases(text: &str, root: &Path) -> AgentDocAliases {
    let mut aliases = AgentDocAliases::default();
    for line in text.lines() {
        let Some((_, detail)) = line.split_once("] ") else {
            continue;
        };
        if let Some(raw) = extract_field(detail, "file") {
            let normalized = normalize_relative_path(raw, root);
            aliases.path_aliases.insert(normalized);
        }
        if let Some(raw) = extract_field(detail, "session") {
            let session = raw.trim_matches('"');
            if !session.is_empty() {
                aliases.session_aliases.insert(session.to_string());
            }
        }
    }
    aliases
}

fn maybe_add_agent_doc_candidate(
    candidates: &mut BTreeMap<String, PendingSession>,
    context: &TargetContext,
    path: &Path,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading agent-doc log {}", path.display()))?;
    let mut matched_by = Vec::new();
    if let Some(session_name) = &context.agent_doc_session
        && path.file_stem().and_then(|value| value.to_str()) == Some(session_name.as_str())
    {
        matched_by.push("agent_doc_session".to_string());
    }
    if context.kind == TargetKind::Directory {
        if text.contains(&format!("cwd_resolved path={}", context.root.display())) {
            matched_by.push("cwd_resolved".to_string());
        }
    } else {
        for alias in &context.path_aliases {
            if text.contains(&format!("file={alias}")) {
                matched_by.push(format!("path:{alias}"));
            }
        }
    }
    if matched_by.is_empty() {
        return Ok(());
    }
    let modified_unix_secs = file_modified_unix_secs(path)?;
    insert_candidate(
        candidates,
        PendingSession::new(
            ReviewSource::AgentDocLog,
            path.to_path_buf(),
            matched_by,
            modified_unix_secs,
            text,
        ),
    );
    Ok(())
}

fn maybe_add_claude_candidate(
    candidates: &mut BTreeMap<String, PendingSession>,
    context: &TargetContext,
    path: &Path,
) -> Result<()> {
    let Some(text) = read_jsonl_session_text_if_cwd_matches(
        path,
        context,
        "Claude session",
        extract_claude_cwd_from_text,
    )?
    else {
        return Ok(());
    };
    let signals = extract_claude_match_signals(&text);
    if !cwd_matches_target(context, signals.cwd.as_deref()) {
        return Ok(());
    }
    let matched_by = match_reasons(context, &signals, signals.cwd.as_deref());
    if matched_by.is_empty() {
        return Ok(());
    }
    let modified_unix_secs = file_modified_unix_secs(path)?;
    insert_candidate(
        candidates,
        PendingSession::new(
            ReviewSource::ClaudeJsonl,
            path.to_path_buf(),
            matched_by,
            modified_unix_secs,
            text,
        ),
    );
    Ok(())
}

fn maybe_add_codex_candidate(
    candidates: &mut BTreeMap<String, PendingSession>,
    context: &TargetContext,
    path: &Path,
) -> Result<()> {
    let Some(text) = read_jsonl_session_text_if_cwd_matches(
        path,
        context,
        "Codex session",
        extract_codex_cwd_from_text,
    )?
    else {
        return Ok(());
    };
    let signals = extract_codex_match_signals(&text);
    if !cwd_matches_target(context, signals.cwd.as_deref()) {
        return Ok(());
    }
    let matched_by = match_reasons(context, &signals, signals.cwd.as_deref());
    if matched_by.is_empty() {
        return Ok(());
    }
    let modified_unix_secs = file_modified_unix_secs(path)?;
    insert_candidate(
        candidates,
        PendingSession::new(
            ReviewSource::CodexJsonl,
            path.to_path_buf(),
            matched_by,
            modified_unix_secs,
            text,
        ),
    );
    Ok(())
}

fn extract_claude_match_signals(text: &str) -> MatchSignals {
    let mut signals = MatchSignals::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if signals.cwd.is_none()
            && let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str)
        {
            signals.cwd = Some(PathBuf::from(cwd));
        }
        collect_claude_match_snippets(&value, &mut signals.snippets);
    }
    signals
}

fn extract_codex_match_signals(text: &str) -> MatchSignals {
    let mut signals = MatchSignals::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") if signals.cwd.is_none() => {
                signals.cwd = value
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from);
            }
            Some("event_msg") => {
                if let Some(payload) = value.get("payload")
                    && payload.get("type").and_then(serde_json::Value::as_str)
                        == Some("user_message")
                    && let Some(message) =
                        payload.get("message").and_then(serde_json::Value::as_str)
                {
                    signals.snippets.push(message.to_string());
                }
            }
            Some("response_item") => {
                if let Some(payload) = value.get("payload") {
                    match payload.get("type").and_then(serde_json::Value::as_str) {
                        Some("function_call") => {
                            if let Some(arguments) =
                                payload.get("arguments").and_then(serde_json::Value::as_str)
                            {
                                signals.snippets.push(arguments.to_string());
                            }
                        }
                        Some("message") => {
                            if payload.get("role").and_then(serde_json::Value::as_str)
                                == Some("user")
                                && let Some(content) =
                                    payload.get("content").and_then(serde_json::Value::as_array)
                            {
                                for item in content {
                                    if let Some(text) = item
                                        .get("text")
                                        .and_then(serde_json::Value::as_str)
                                        .or_else(|| {
                                            item.get("content").and_then(serde_json::Value::as_str)
                                        })
                                    {
                                        signals.snippets.push(text.to_string());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    signals
}

fn cwd_matches_target(context: &TargetContext, cwd: Option<&Path>) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let Ok(canonical_cwd) = cwd.canonicalize() else {
        return false;
    };
    canonical_cwd.starts_with(&context.root) || context.root.starts_with(canonical_cwd)
}

fn match_reasons(
    context: &TargetContext,
    signals: &MatchSignals,
    cwd: Option<&Path>,
) -> Vec<String> {
    let mut reasons = BTreeSet::new();
    match context.kind {
        TargetKind::Directory => {
            if cwd_matches_target(context, cwd) {
                reasons.insert("cwd".to_string());
            }
        }
        TargetKind::File => {
            for snippet in &signals.snippets {
                for alias in &context.path_aliases {
                    if snippet.contains(alias) {
                        reasons.insert(format!("path:{alias}"));
                    }
                }
                for session_alias in &context.session_aliases {
                    if snippet.contains(session_alias) {
                        reasons.insert("agent_doc_session".to_string());
                    }
                }
            }
            if reasons.is_empty() {
                return Vec::new();
            }
            if cwd_matches_target(context, cwd) {
                reasons.insert("cwd".to_string());
            }
        }
    }
    reasons.into_iter().collect()
}

fn collect_claude_match_snippets(value: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(message) = value.get("message") {
        collect_claude_message_snippets(message, out);
        return;
    }
    if value.get("attachment").is_some() {
        return;
    }
    collect_claude_message_snippets(value, out);
}

fn collect_claude_message_snippets(value: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(content) = value.get("content") {
        match content {
            serde_json::Value::String(text) => out.push(text.to_string()),
            serde_json::Value::Array(items) => {
                for item in items {
                    match item.get("type").and_then(serde_json::Value::as_str) {
                        Some("text") => {
                            if let Some(text) = item
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .or_else(|| item.get("content").and_then(serde_json::Value::as_str))
                            {
                                out.push(text.to_string());
                            }
                        }
                        Some("tool_use") => {
                            if let Some(command) = item
                                .get("input")
                                .and_then(|input| input.get("command"))
                                .and_then(serde_json::Value::as_str)
                            {
                                out.push(command.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    } else if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
        out.push(text.to_string());
    }
}

fn insert_candidate(candidates: &mut BTreeMap<String, PendingSession>, pending: PendingSession) {
    let key = pending.path.display().to_string();
    if let Some(existing) = candidates.get_mut(&key) {
        existing.matched_by.extend(pending.matched_by);
        existing.modified_unix_secs = existing.modified_unix_secs.max(pending.modified_unix_secs);
        return;
    }
    candidates.insert(key, pending);
}

fn normalize_relative_path(raw: &str, root: &Path) -> String {
    let path = PathBuf::from(raw);
    let joined = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    joined
        .strip_prefix(root)
        .ok()
        .unwrap_or(joined.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

fn extract_claude_cwd_from_text(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str) {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

fn extract_codex_cwd_from_text(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
            && let Some(cwd) = value
                .get("payload")
                .and_then(|payload| payload.get("cwd"))
                .and_then(serde_json::Value::as_str)
        {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

fn read_jsonl_session_text_if_cwd_matches(
    path: &Path,
    context: &TargetContext,
    label: &str,
    extract_cwd: fn(&str) -> Option<PathBuf>,
) -> Result<Option<String>> {
    let file =
        fs::File::open(path).with_context(|| format!("reading {label} {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut header = String::new();
    let mut line = String::new();
    let mut cwd: Option<PathBuf> = None;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("reading {label} {}", path.display()))?;
        if bytes == 0 {
            break;
        }
        header.push_str(&line);
        cwd = extract_cwd(&header);
        if cwd.is_some() || header.len() >= SESSION_HEADER_PROBE_BUDGET_BYTES {
            break;
        }
    }
    if !cwd_matches_target(context, cwd.as_deref()) {
        return Ok(None);
    }
    let mut rest = String::new();
    reader
        .read_to_string(&mut rest)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    header.push_str(&rest);
    Ok(Some(header))
}

fn collect_files_with_extension(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_with_extension_inner(root, extension, &mut files)?;
    Ok(files)
}

fn collect_recent_files_with_extension(
    root: &Path,
    extension: &str,
    limit: usize,
) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<(Option<u64>, PathBuf)> = Vec::new();
    collect_recent_files_with_extension_inner(root, extension, &mut entries)?;
    entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    entries.truncate(limit);
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

fn collect_recent_files_with_extension_inner(
    root: &Path,
    extension: &str,
    entries: &mut Vec<(Option<u64>, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_recent_files_with_extension_inner(&path, extension, entries)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            let modified = file_modified_unix_secs(&path).unwrap_or(None);
            entries.push((modified, path));
        }
    }
    Ok(())
}

fn collect_files_with_extension_inner(
    root: &Path,
    extension: &str,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_extension_inner(&path, extension, files)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            files.push(path);
        }
    }
    Ok(())
}

fn file_modified_unix_secs(path: &Path) -> Result<Option<u64>> {
    let modified = fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .modified()
        .ok();
    Ok(modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs()))
}

fn extract_field<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let start = detail.find(&needle)? + needle.len();
    let remainder = &detail[start..];
    let end = remainder
        .find(char::is_whitespace)
        .unwrap_or(remainder.len());
    Some(remainder[..end].trim_matches('"'))
}

fn collect_strings<T, F>(entries: BTreeMap<String, usize>, max_items: usize, build: F) -> Vec<T>
where
    F: Fn(String, usize) -> T,
{
    let mut rows = entries.into_iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    rows.truncate(max_items);
    rows.into_iter()
        .map(|(value, count)| build(value, count))
        .collect()
}

fn collect_pairs<K, T, F>(entries: BTreeMap<K, usize>, max_items: usize, build: F) -> Vec<T>
where
    K: Ord,
    F: Fn(K, usize) -> T,
{
    let mut rows = entries.into_iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    rows.truncate(max_items);
    rows.into_iter()
        .map(|(value, count)| build(value, count))
        .collect()
}

fn collect_restart_churn(
    entries: BTreeMap<String, RestartChurnSummary>,
    max_items: usize,
) -> Vec<RestartChurnSummary> {
    let mut rows = entries.into_values().collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.family.cmp(&right.family))
    });
    rows.truncate(max_items);
    rows
}

fn collect_loop_clusters(
    entries: BTreeMap<(String, String), (usize, usize)>,
    max_items: usize,
) -> Vec<SessionCostLoopCluster> {
    let mut rows = entries
        .into_iter()
        .map(
            |((kind, label), (occurrences, max_consecutive))| SessionCostLoopCluster {
                kind,
                label,
                occurrences,
                max_consecutive,
            },
        )
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(right.max_consecutive.cmp(&left.max_consecutive))
            .then(left.kind.cmp(&right.kind))
            .then(left.label.cmp(&right.label))
    });
    rows.truncate(max_items);
    rows
}

fn collect_file_read_diagnostics(
    entries: BTreeMap<(String, String), FileReadDiagnosticAggregate>,
    max_items: usize,
) -> Vec<SessionCostFileReadDiagnostic> {
    let mut rows = entries
        .into_values()
        .map(|entry| SessionCostFileReadDiagnostic {
            path: entry.path,
            range: entry.range,
            occurrences: entry.occurrences,
            estimated_tokens: entry.estimated_tokens,
            duplicate_estimated_tokens: entry.duplicate_estimated_tokens,
            follow_up_commands: entry.follow_up_commands.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .duplicate_estimated_tokens
            .cmp(&left.duplicate_estimated_tokens)
            .then(right.occurrences.cmp(&left.occurrences))
            .then(left.path.cmp(&right.path))
            .then(left.range.cmp(&right.range))
    });
    rows.truncate(max_items);
    rows
}

fn shell_quote(text: &str) -> String {
    if text.chars().any(char::is_whitespace) {
        format!("{text:?}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_recent_files_with_extension_caps_and_sorts_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            let path = dir.path().join(format!("session-{i:02}.jsonl"));
            fs::write(&path, format!("{{\"i\":{i}}}\n")).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            let modified = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + i as u64 * 60);
            file.set_modified(modified).unwrap();
        }
        fs::write(dir.path().join("ignored.txt"), "skip me").unwrap();

        let recent = collect_recent_files_with_extension(dir.path(), "jsonl", 3).unwrap();
        assert_eq!(recent.len(), 3, "should cap at 3 entries");
        let names: Vec<String> = recent
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "session-09.jsonl".to_string(),
                "session-08.jsonl".to_string(),
                "session-07.jsonl".to_string(),
            ],
            "should return newest-first by mtime"
        );

        let all = collect_recent_files_with_extension(dir.path(), "jsonl", 100).unwrap();
        assert_eq!(
            all.len(),
            10,
            "limit above population should return everything"
        );
        assert!(
            !all.iter()
                .any(|p| p.extension().and_then(|s| s.to_str()) == Some("txt")),
            "non-matching extensions must be filtered: {all:?}"
        );
    }

    #[test]
    fn read_jsonl_session_text_if_cwd_matches_skips_non_matching_files_without_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let target_root = dir.path().canonicalize().unwrap();
        let target = target_root.join("plan.md");
        fs::create_dir(target_root.join(".git")).unwrap();
        fs::write(&target, "---\nagent_doc_session: x\n---\n").unwrap();
        let context = build_target_context(&target).unwrap();

        let matching = dir.path().join("matching.jsonl");
        let matching_cwd = target_root.display().to_string();
        let matching_body = format!(
            "{{\"cwd\":\"{matching_cwd}\"}}\n{}\n",
            "x".repeat(64 * 1024)
        );
        fs::write(&matching, &matching_body).unwrap();

        let other = dir.path().join("other.jsonl");
        fs::write(
            &other,
            format!(
                "{{\"cwd\":\"/tmp/other-project-{}\"}}\n{}\n",
                std::process::id(),
                "y".repeat(64 * 1024)
            ),
        )
        .unwrap();

        let matched = read_jsonl_session_text_if_cwd_matches(
            &matching,
            &context,
            "test",
            extract_claude_cwd_from_text,
        )
        .unwrap();
        assert!(
            matched.is_some(),
            "file with matching cwd should return Some(text)"
        );
        let skipped = read_jsonl_session_text_if_cwd_matches(
            &other,
            &context,
            "test",
            extract_claude_cwd_from_text,
        )
        .unwrap();
        assert!(
            skipped.is_none(),
            "file with non-matching cwd should return None"
        );
    }

    #[test]
    fn session_review_discovers_cross_harness_logs_for_doc_target() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n",
                "[1776712374] codex_start mode=fresh restart_count=0\n",
                "[1776712375] auto_trigger_timeout harness=codex reason=no_prompt_after_30s\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let claude_dir = home
            .path()
            .join(".claude/projects")
            .join(claude_project_slug(root.path()));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("claude.jsonl"),
            concat!(
                r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"message":{"role":"assistant","id":"msg-1","usage":{"input_tokens":200,"cache_creation_input_tokens":20,"cache_read_input_tokens":180,"output_tokens":15},"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-1.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
                "\n",
                r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(report.target_kind, "file");
        assert_eq!(report.sessions_matched, 3);
        assert_eq!(report.claude_sessions, 1);
        assert_eq!(report.codex_sessions, 1);
        assert_eq!(report.agent_doc_logs, 1);
        assert!(report.prompt_tokens >= 1200);
        assert!(
            report
                .guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "restart_loop")
        );
        assert!(
            report
                .next_context
                .unresolved_failures
                .iter()
                .any(|failure| failure.kind == "guardrail:restart_loop"
                    && failure.message.contains("restart churn detected"))
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.command == "cargo test")
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.command == "cargo build --release")
        );
        assert!(report.sessions.iter().any(|session| {
            session
                .matched_by
                .iter()
                .any(|reason| reason == "agent_doc_session")
        }));
        assert_eq!(
            report.next_context.active_prompt_targets,
            Vec::<String>::new()
        );
        assert_eq!(report.next_context.last_verification.status, "missing");
        assert!(report.next_context.next_digest_commands.iter().any(
            |command| command == "tsift session-review --next-context tasks/software/tsift.md"
        ));
    }

    #[test]
    fn session_review_next_context_tracks_prompts_verification_and_failures() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let claude_dir = home
            .path()
            .join(".claude/projects")
            .join(claude_project_slug(root.path()));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("claude.jsonl"),
            concat!(
                r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"do [#ctxpack]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"message":{"role":"assistant","id":"msg-1","usage":{"input_tokens":300,"cache_creation_input_tokens":30,"cache_read_input_tokens":250,"output_tokens":25},"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test --manifest-path Cargo.toml"}},{"type":"text","text":"Verification in `src/tsift`: `cargo test`\nError: Symbol `run_sync` not found in src/lib.rs:7:9"}]}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-1.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#ctxpack]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(
            report.next_context.active_prompt_targets,
            vec!["do [#ctxpack]. spec-test-build-install-commit-push".to_string()]
        );
        assert_eq!(report.next_context.last_verification.status, "passed");
        assert!(
            report
                .next_context
                .last_verification
                .detail
                .contains("Verification in `src/tsift`")
        );
        assert!(
            report
                .next_context
                .touched_files
                .iter()
                .any(|path| path == "Cargo.toml")
        );
        assert!(
            report
                .next_context
                .touched_symbols
                .iter()
                .any(|symbol| symbol == "run_sync")
        );
        assert!(
            report
                .next_context
                .unresolved_failures
                .iter()
                .any(|failure| failure.kind == "missing" || failure.kind == "error")
        );
    }

    #[test]
    fn session_review_next_context_prefers_live_exchange_prompt_targets() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "\
---
agent_doc_session: tsift-v0.1
agent_doc_format: template
prompt_presets:
  '#spec-test-build-install-commit-push': update spec + tests. build + install for local testing. commit + push
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

Compacted content:
- Archived 2 response topic(s): #old1 search workflow; #old2 build workflow
<!-- agent:boundary:abc123 -->
do [#active]. spec-test-build-install-commit-push
<!-- /agent:exchange -->

## Queue

<!-- agent:queue preset=\"#spec-test-build-install-commit-push\" go -->
- ~~[#done]~~
- [#active]
- [#later]
<!-- /agent:queue -->

## Backlog

<!-- agent:backlog priority queue -->
- [ ] [#active] Add the active queue profile to context-pack.
- [ ] [#later] Later prompt should remain queued.
- [x] [#done] Completed prompt should stay out of the active profile.
<!-- /agent:backlog -->

## Review

<!-- agent:review -->
- [ ] [#review] Verify the queue profile output.
<!-- /agent:review -->

## Completed / Reaped

<!-- agent:done -->
- 2026-05-12 [#old1] do [#old1]. spec-test-build-install-commit-push
<!-- /agent:done -->
",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-old.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#old1]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#old1]. spec-test-build-install-commit-push"}}"#,
                "\n",
                r####"{"type":"event_msg","payload":{"type":"agent_message","message":"### Re: old work\nError: stale failure at /!\n`/!` should not become active handoff context"}}"####,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert!(
            report
                .prompt_targets
                .iter()
                .any(|prompt| { prompt.text == "do [#old1]. spec-test-build-install-commit-push" })
        );
        assert_eq!(
            report.next_context.active_prompt_targets,
            vec!["do [#active]. spec-test-build-install-commit-push".to_string()]
        );
        let queue_profile = report
            .next_context
            .agent_doc_queue
            .as_ref()
            .expect("agent-doc queue profile should be present");
        assert_eq!(
            queue_profile.active_queue_prompt.as_deref(),
            Some("[#active] Add the active queue profile to context-pack.")
        );
        assert_eq!(
            queue_profile.live_exchange_tail,
            vec!["do [#active]. spec-test-build-install-commit-push".to_string()]
        );
        assert!(
            queue_profile
                .backlog_rows
                .iter()
                .any(|row| row == "[#later] Later prompt should remain queued.")
        );
        assert!(
            queue_profile
                .backlog_rows
                .iter()
                .all(|row| !row.contains("#done"))
        );
        assert_eq!(
            queue_profile.review_rows,
            vec!["[#review] Verify the queue profile output.".to_string()]
        );
        assert!(
            queue_profile
                .prompt_presets
                .iter()
                .any(|preset| preset.starts_with("#spec-test-build-install-commit-push:"))
        );
        assert!(
            queue_profile
                .expansion_handles
                .iter()
                .any(|handle| handle.expand.contains("context-pack"))
        );
        assert!(
            report
                .touched_files
                .iter()
                .all(|file_ref| file_ref.path != "/!")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.message.contains("stale failure"))
        );
        assert!(
            report
                .next_context
                .touched_files
                .iter()
                .all(|path| path != "/!")
        );
        assert!(report.next_context.unresolved_failures.is_empty());
    }

    #[test]
    fn session_review_next_context_scopes_freeform_live_exchange_tail() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "\
---
agent_doc_session: tsift-v0.1
agent_doc_format: template
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

*Compacted. Content archived to `/tmp/archive.md`*

Compacted content:
- Archived 1 response topic(s): prior review
<!-- agent:boundary:freeform -->
Evaluate the logs for tsift effectiveness and bugs. #next-steps
<!-- /agent:exchange -->
",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-stale.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#stale]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r####"{"type":"event_msg","payload":{"type":"agent_message","message":"### Re: stale work\nError: old unresolved failure at /!\n`/!` should not be active context"}}"####,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(
            report.next_context.active_prompt_targets,
            vec!["Evaluate the logs for tsift effectiveness and bugs. #next-steps".to_string()]
        );
        assert!(report.next_context.touched_files.is_empty());
        assert!(report.next_context.unresolved_failures.is_empty());
    }

    #[test]
    fn session_review_ignores_assistant_failure_meta_progress() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "\
---
agent_doc_session: tsift-v0.1
agent_doc_format: template
---

## Exchange

<!-- agent:exchange patch=append -->
### Session Summary

Prior summary without active failures.
<!-- agent:boundary:abc123 -->
<!-- /agent:exchange -->
",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-progress.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I’m checking the session-review failure groups because --next-context reports zero unresolved failures.\nThe previous assessment sentence mentioned failure false positives and prior status updates around red CI checks.\nCI status prose from the progress update should not become a failure row."}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(report.sessions_matched, 2);
        assert!(report.failures.is_empty());
        assert!(report.next_context.unresolved_failures.is_empty());
    }

    #[test]
    fn session_review_failure_rows_keep_command_and_session_anchors() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        let rollout_path = codex_dir.join("rollout-failure.jsonl");
        fs::write(
            &rollout_path,
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#sfail]. Tighten failure extraction.\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"exec_command_end","exit_code":1,"aggregated_output":"After finalize, panic snippets and generic command exited with code 1 should not become failures.\npanic!(\"expected simulated swap failure\");\nthread 'suite::alpha_failure' panicked at src/lib.rs:3:5:\nassertion failed: left == right\n","parsed_cmd":[{"type":"unknown","cmd":"cargo test"}]}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert!(
            report
                .failures
                .iter()
                .all(|failure| !failure.message.contains("After finalize")
                    && !failure.message.contains("panic!(")
                    && failure.message != "command exited with code 1")
        );
        assert!(report.failures.iter().any(|failure| {
            failure.message == "cargo test exited with code 1"
                && failure.command.as_deref() == Some("cargo test")
                && failure.session_path.as_deref() == Some(rollout_path.to_str().unwrap())
        }));
        assert!(report.failures.iter().any(|failure| {
            failure.message.contains("assertion failed")
                && failure.command.as_deref() == Some("cargo test")
                && failure.session_path.as_deref() == Some(rollout_path.to_str().unwrap())
        }));
    }

    #[test]
    fn session_review_aggregates_loop_clusters() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n",
                "[1776712374] commit_already_current file=tasks/software/tsift.md basis=head\n",
                "[1776712375] commit_already_current file=tasks/software/tsift.md basis=head\n",
                "[1776712376] commit_already_current file=tasks/software/tsift.md basis=head\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-1.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#looprank]. spec-test-build-install-commit-push\nagent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,80p' src/session_review.rs\"}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,80p' src/session_review.rs\"}"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Committed and pushed in `src/tsift` as `abc123`."}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#looprank]. spec-test-build-install-commit-push"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Committed and pushed in `src/tsift` as `abc123`."}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert!(
            report
                .loop_clusters
                .iter()
                .any(|cluster| cluster.kind == "prompt_repeat"
                    && cluster.label == "do [#looprank]. spec-test-build-install-commit-push"
                    && cluster.occurrences == 2)
        );
        assert!(
            report
                .loop_clusters
                .iter()
                .any(|cluster| cluster.kind == "command_bundle"
                    && cluster.label == "cargo test -> cargo build --release"
                    && cluster.occurrences == 2)
        );
        assert!(
            report
                .loop_clusters
                .iter()
                .any(|cluster| cluster.kind == "closeout_churn"
                    && cluster.label == "commit_already_current"
                    && cluster.occurrences == 3)
        );
        assert!(
            report
                .file_read_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "src/session_review.rs"
                    && diagnostic.range == "1-80"
                    && diagnostic.occurrences == 2
                    && diagnostic.duplicate_estimated_tokens == 1_440
                    && diagnostic.follow_up_commands.iter().any(|command| {
                        command
                            == "tsift source-read src/session_review.rs --start 1 --lines 80 --budget normal"
                    }))
        );
    }

    #[test]
    fn session_review_skips_cwd_only_harness_logs_for_doc_target() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/software/tsift.md pane=%77 session=tsift-v0.1\n",
                "[1776712373] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let claude_dir = home
            .path()
            .join(".claude/projects")
            .join(claude_project_slug(root.path()));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("claude-target.jsonl"),
            concat!(
                r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();
        fs::write(
            claude_dir.join("claude-cwd-only.jsonl"),
            concat!(
                r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"help me inspect another task"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("codex-target.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"agent-doc /tmp/replace-me/tasks/software/tsift.md"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();
        fs::write(
            codex_dir.join("codex-cwd-only.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"open a different issue from this repo"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(report.sessions_considered, 5);
        assert_eq!(report.sessions_matched, 3);
        assert_eq!(report.claude_sessions, 1);
        assert_eq!(report.codex_sessions, 1);
        assert_eq!(report.agent_doc_logs, 1);
        assert!(report.sessions.iter().all(|session| {
            session.source == "agent_doc_log"
                || session
                    .matched_by
                    .iter()
                    .any(|reason| reason == "agent_doc_session" || reason.starts_with("path:"))
        }));
    }

    #[test]
    fn session_review_uses_historical_aliases_and_skips_noisy_transcript_records() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let target = root.path().join("tasks/software/tsift.md");
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            "---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n",
        )
        .unwrap();

        let agent_doc_logs = root.path().join(".agent-doc/logs");
        fs::create_dir_all(&agent_doc_logs).unwrap();
        fs::write(
            agent_doc_logs.join("tsift-v0.1.log"),
            concat!(
                "[1776712372] session_start file=tasks/tsift.md pane=%77 session=tsift-v0\n",
                "[1776712373] session_start file=tasks/software/tsift.md pane=%78 session=tsift-v0.1\n",
                "[1776712374] cwd_resolved path=/tmp/replace-me source=project_root\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let claude_dir = home
            .path()
            .join(".claude/projects")
            .join(claude_project_slug(root.path()));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("claude-target.jsonl"),
            concat!(
                "not-json\n",
                r#"{"cwd":"/tmp/replace-me","message":{"role":"user","content":"resume session tsift-v0\nagent-doc tasks/tsift.md"}}"#,
                "\n",
                r#"{"attachment":{"type":"hook_success","content":"tasks/software/tsift.md from context index only"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();
        fs::write(
            claude_dir.join("claude-noisy.jsonl"),
            concat!(
                r#"{"cwd":"/tmp/replace-me","attachment":{"type":"hook_success","content":"tasks/software/tsift.md only in hook output"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let codex_dir = home.path().join(".codex/sessions/2026/05/05");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("codex-target.jsonl"),
            concat!(
                "not-json\n",
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"resume tsift-v0\nagent-doc tasks/tsift.md"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","output":"tasks/software/tsift.md from stdout"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();
        fs::write(
            codex_dir.join("codex-noisy.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/replace-me"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","output":"tasks/software/tsift.md only in output"}}"#,
                "\n"
            )
            .replace("/tmp/replace-me", &root.path().display().to_string()),
        )
        .unwrap();

        let report = compute_with_options(
            &target,
            &SessionReviewOptions {
                claude_projects_dir: Some(home.path().join(".claude/projects")),
                codex_sessions_dir: Some(home.path().join(".codex/sessions")),
                agent_doc_logs_dir: Some(agent_doc_logs),
            },
        )
        .unwrap();

        assert_eq!(report.sessions_considered, 5);
        assert_eq!(report.sessions_matched, 3);
        assert_eq!(report.claude_sessions, 1);
        assert_eq!(report.codex_sessions, 1);
        assert_eq!(report.agent_doc_logs, 1);
        assert!(report.sessions.iter().any(|session| {
            session.path.ends_with("claude-target.jsonl")
                && session
                    .matched_by
                    .iter()
                    .any(|reason| reason == "agent_doc_session" || reason == "path:tasks/tsift.md")
        }));
        assert!(report.sessions.iter().any(|session| {
            session.path.ends_with("codex-target.jsonl")
                && session
                    .matched_by
                    .iter()
                    .any(|reason| reason == "agent_doc_session" || reason == "path:tasks/tsift.md")
        }));
        assert!(
            report.warnings.iter().any(
                |warning| warning.contains("skipping malformed Claude transcript jsonl line 1")
            )
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("skipping malformed Codex transcript jsonl line 1"))
        );
    }
}
