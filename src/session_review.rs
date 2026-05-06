use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::runtime_churn::RestartChurnSummary;
use crate::{
    session_cost::{self, SessionCostGuardrail, SessionCostGuardrailInput},
    session_digest,
};

const MAX_SESSIONS: usize = 12;
const MAX_AGGREGATE_ITEMS: usize = 12;
const MAX_LARGEST_TURNS: usize = 8;
const MAX_WARNINGS: usize = 16;

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

#[derive(Debug, Clone, Serialize)]
pub struct SessionReviewNextContext {
    pub target: String,
    pub active_prompt_targets: Vec<String>,
    pub last_verification: SessionReviewVerificationState,
    pub touched_files: Vec<String>,
    pub touched_symbols: Vec<String>,
    pub unresolved_failures: Vec<SessionReviewFailure>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub guardrails: Vec<SessionCostGuardrail>,
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
    aliases: BTreeSet<String>,
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

pub fn compute(target: &Path) -> Result<SessionReviewReport> {
    compute_with_options(target, &SessionReviewOptions::default())
}

pub fn compute_with_options(
    target: &Path,
    options: &SessionReviewOptions,
) -> Result<SessionReviewReport> {
    let mut context = build_target_context(target)?;
    let mut candidates = BTreeMap::<String, PendingSession>::new();
    let mut sessions_considered = 0_usize;
    let mut warnings = Vec::new();

    let agent_doc_logs_dir = resolve_agent_doc_logs_dir(&context.root, options);
    if let Some(session_name) = &context.agent_doc_session {
        let session_log = agent_doc_logs_dir.join(format!("{session_name}.log"));
        if session_log.is_file()
            && let Ok(text) = fs::read_to_string(&session_log)
        {
            for alias in collect_agent_doc_aliases(&text, &context.root) {
                context.aliases.insert(alias);
            }
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
        for path in collect_files_with_extension(&claude_project_dir, "jsonl")? {
            sessions_considered += 1;
            maybe_add_claude_candidate(&mut candidates, &context, &path)?;
        }
    }

    let codex_sessions_dir = resolve_codex_sessions_dir(&context.root, options);
    if codex_sessions_dir.is_dir() {
        for path in collect_files_with_extension(&codex_sessions_dir, "jsonl")? {
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

    let mut prompt_targets = BTreeMap::<String, usize>::new();
    let mut commands = BTreeMap::<String, usize>::new();
    let mut touched_files = BTreeMap::<String, usize>::new();
    let mut touched_symbols = BTreeMap::<String, usize>::new();
    let mut failures = BTreeMap::<(String, String), usize>::new();
    let mut runtime_events = BTreeMap::<String, usize>::new();
    let mut closeout = BTreeMap::<(String, String), usize>::new();
    let mut restart_churn = BTreeMap::<String, RestartChurnSummary>::new();
    let mut aggregate_runtime_events = BTreeMap::<String, usize>::new();
    let mut largest_turns = Vec::<SessionReviewLargestTurn>::new();
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
        let digest = session_digest::compute(
            &context.root,
            &pending.text,
            Some(pending.source.digest_source()),
        )
        .with_context(|| format!("digesting {}", pending.path.display()))?;
        let cost = if pending.source.supports_cost() {
            Some(
                session_cost::compute(&pending.text, Some(pending.source.digest_source()))
                    .with_context(|| format!("costing {}", pending.path.display()))?,
            )
        } else {
            None
        };

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
                .entry((failure.kind.clone(), failure.message.clone()))
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
        });
    }

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

    session_rows.truncate(MAX_SESSIONS);
    warnings.sort();
    warnings.truncate(MAX_WARNINGS);

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
        |(kind, message), occurrences| SessionReviewFailure {
            kind,
            message,
            occurrences,
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
    let next_context = build_next_context(
        &context,
        &prompt_targets,
        &touched_files,
        &touched_symbols,
        &failures,
        last_verification.unwrap_or_else(|| SessionReviewVerificationState {
            status: "missing".to_string(),
            detail: "no verification closeout found in matched sessions".to_string(),
        }),
    );

    Ok(SessionReviewReport {
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
        guardrails,
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
    })
}

fn build_target_context(target: &Path) -> Result<TargetContext> {
    let canonical_target = target
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", target.display()))?;
    let root = crate::lint::resolve_harness_root_or_canonical_path(target)?;
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
        .then(|| parse_agent_doc_session(&canonical_target))
        .transpose()?
        .flatten();

    let mut aliases = BTreeSet::new();
    aliases.insert(canonical_target.display().to_string());
    if let Some(relative) = &relative_target {
        aliases.insert(relative.clone());
    }
    if let Some(name) = canonical_target
        .file_name()
        .and_then(|value| value.to_str())
    {
        aliases.insert(name.to_string());
    }

    Ok(TargetContext {
        root,
        canonical_target,
        relative_target,
        kind,
        agent_doc_session,
        aliases,
    })
}

fn build_next_context(
    context: &TargetContext,
    prompt_targets: &[SessionReviewPromptTarget],
    touched_files: &[SessionReviewFileRef],
    touched_symbols: &[SessionReviewSymbolRef],
    failures: &[SessionReviewFailure],
    last_verification: SessionReviewVerificationState,
) -> SessionReviewNextContext {
    let target = context
        .relative_target
        .clone()
        .unwrap_or_else(|| context.canonical_target.display().to_string());
    let session_target = match context.kind {
        TargetKind::Directory => ".".to_string(),
        TargetKind::File => target.clone(),
    };

    SessionReviewNextContext {
        target,
        active_prompt_targets: prompt_targets
            .iter()
            .map(|entry| entry.text.clone())
            .collect(),
        last_verification,
        touched_files: touched_files
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        touched_symbols: touched_symbols
            .iter()
            .map(|entry| entry.symbol.clone())
            .collect(),
        unresolved_failures: failures.to_vec(),
        next_digest_commands: vec![
            format!(
                "tsift session-review --next-context {}",
                shell_quote(&session_target)
            ),
            "tsift diff-digest .".to_string(),
            "tsift test-digest --path . < test.log".to_string(),
            "tsift log-digest --path . < build.log".to_string(),
        ],
    }
}

fn parse_agent_doc_session(path: &Path) -> Result<Option<String>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading target document {}", path.display()))?;
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Ok(None);
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("agent_doc_session:") {
            let session = value.trim().trim_matches('"').trim_matches('\'');
            if !session.is_empty() {
                return Ok(Some(session.to_string()));
            }
        }
    }
    Ok(None)
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

fn collect_agent_doc_aliases(text: &str, root: &Path) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    for line in text.lines() {
        let Some((_, detail)) = line.split_once("] ") else {
            continue;
        };
        if let Some(raw) = extract_field(detail, "file") {
            let normalized = normalize_relative_path(raw, root);
            aliases.insert(normalized);
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
        for alias in &context.aliases {
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
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading Claude session {}", path.display()))?;
    let cwd = extract_claude_cwd(&text)?;
    if !cwd_matches_target(context, cwd.as_deref()) {
        return Ok(());
    }
    let matched_by = match_reasons(context, &text, cwd.as_deref());
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
    let cwd = extract_codex_cwd(path)?;
    if !cwd_matches_target(context, cwd.as_deref()) {
        return Ok(());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading Codex session {}", path.display()))?;
    let matched_by = match_reasons(context, &text, cwd.as_deref());
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

fn extract_claude_cwd(text: &str) -> Result<Option<PathBuf>> {
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(trimmed)
            .with_context(|| format!("parsing Claude session jsonl line {}", index + 1))?;
        if let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str) {
            return Ok(Some(PathBuf::from(cwd)));
        }
    }
    Ok(None)
}

fn extract_codex_cwd(path: &Path) -> Result<Option<PathBuf>> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(trimmed)
            .with_context(|| format!("parsing Codex session jsonl line {}", index + 1))?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
            continue;
        }
        return Ok(value
            .get("payload")
            .and_then(|payload| payload.get("cwd"))
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from));
    }
    Ok(None)
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

fn match_reasons(context: &TargetContext, text: &str, cwd: Option<&Path>) -> Vec<String> {
    let mut reasons = BTreeSet::new();
    match context.kind {
        TargetKind::Directory => {
            if cwd_matches_target(context, cwd) {
                reasons.insert("cwd".to_string());
            }
        }
        TargetKind::File => {
            for alias in &context.aliases {
                if text.contains(alias) {
                    reasons.insert(format!("path:{alias}"));
                }
            }
            if let Some(session_name) = &context.agent_doc_session
                && text.contains(session_name)
            {
                reasons.insert("agent_doc_session".to_string());
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

fn collect_files_with_extension(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_with_extension_inner(root, extension, &mut files)?;
    Ok(files)
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
}
