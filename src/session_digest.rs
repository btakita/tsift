use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::runtime_churn::{RestartChurnState, RestartChurnSummary};

const MAX_PROMPT_TARGETS: usize = 8;
const MAX_COMMANDS: usize = 12;
const MAX_FILES: usize = 12;
const MAX_SYMBOLS: usize = 12;
const MAX_FAILURES: usize = 12;
const MAX_CLOSEOUT: usize = 10;
const MAX_RUNTIME_EVENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDigestSource {
    Markdown,
    ClaudeJsonl,
    CodexJsonl,
    AgentDocLog,
}

impl SessionDigestSource {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "jsonl" | "json-lines" | "claude" | "claude-jsonl" => Ok(Self::ClaudeJsonl),
            "codex" | "codex-jsonl" => Ok(Self::CodexJsonl),
            "agent-doc-log" | "agent_doc_log" | "log" => Ok(Self::AgentDocLog),
            other => bail!(
                "unsupported session source `{other}`; expected markdown, claude-jsonl, codex-jsonl, or agent-doc-log"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::ClaudeJsonl => "claude_jsonl",
            Self::CodexJsonl => "codex_jsonl",
            Self::AgentDocLog => "agent_doc_log",
        }
    }

    pub fn cli_arg(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::ClaudeJsonl => "claude-jsonl",
            Self::CodexJsonl => "codex-jsonl",
            Self::AgentDocLog => "agent-doc-log",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestCommand {
    pub command: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestFileRef {
    pub path: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestSymbolRef {
    pub symbol: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestFailure {
    pub kind: String,
    pub message: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestCloseout {
    pub kind: String,
    pub detail: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestRuntimeEvent {
    pub event: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDigestReport {
    pub root: String,
    pub source: String,
    pub total_lines: usize,
    pub transcript_items: usize,
    pub prompt_target_count: usize,
    pub command_groups: usize,
    pub file_groups: usize,
    pub symbol_groups: usize,
    pub failure_groups: usize,
    pub runtime_event_groups: usize,
    pub restart_churn_groups: usize,
    pub closeout_groups: usize,
    pub prompt_targets: Vec<String>,
    pub commands: Vec<SessionDigestCommand>,
    pub touched_files: Vec<SessionDigestFileRef>,
    pub touched_symbols: Vec<SessionDigestSymbolRef>,
    pub failures: Vec<SessionDigestFailure>,
    pub runtime_events: Vec<SessionDigestRuntimeEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub restart_churn: Vec<RestartChurnSummary>,
    pub closeout: Vec<SessionDigestCloseout>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct DigestState {
    prompt_targets: Vec<String>,
    commands: BTreeMap<String, usize>,
    files: BTreeMap<String, usize>,
    symbols: BTreeMap<String, usize>,
    failures: BTreeMap<(String, String), usize>,
    runtime_events: BTreeMap<String, usize>,
    seen_document_cycle_events: BTreeSet<(String, String)>,
    seen_document_cycle_closeout: BTreeSet<(String, String, String)>,
    restart_churn: RestartChurnState,
    closeout: BTreeMap<(String, String), usize>,
    warnings: Vec<String>,
    transcript_items: usize,
}

#[derive(Debug, Clone)]
enum TranscriptBlock {
    Text { role: Option<String>, text: String },
    ToolUse { name: String, input: Value },
}

pub fn compute(path: &Path, input: &str, source_hint: Option<&str>) -> Result<SessionDigestReport> {
    if input.trim().is_empty() {
        bail!("no session input provided; pass --input <file> or pipe transcript on stdin");
    }

    let root = crate::lint::resolve_harness_root_or_canonical_path(path)?;
    let source = resolve_source(input, source_hint)?;
    let total_lines = input.lines().count();
    let mut state = DigestState::default();

    match source {
        SessionDigestSource::Markdown => ingest_markdown(&root, input, &mut state)?,
        SessionDigestSource::ClaudeJsonl => ingest_claude_jsonl(&root, input, &mut state)?,
        SessionDigestSource::CodexJsonl => ingest_codex_jsonl(&root, input, &mut state)?,
        SessionDigestSource::AgentDocLog => ingest_agent_doc_log(&root, input, &mut state),
    }

    let prompt_target_count = state.prompt_targets.len();

    let mut commands = state
        .commands
        .into_iter()
        .map(|(command, occurrences)| SessionDigestCommand {
            command,
            occurrences,
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.command.cmp(&right.command))
    });
    let command_groups = commands.len();
    commands.truncate(MAX_COMMANDS);

    let mut touched_files = state
        .files
        .into_iter()
        .map(|(path, occurrences)| SessionDigestFileRef { path, occurrences })
        .collect::<Vec<_>>();
    touched_files.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.path.cmp(&right.path))
    });
    let file_groups = touched_files.len();
    touched_files.truncate(MAX_FILES);

    let mut touched_symbols = state
        .symbols
        .into_iter()
        .map(|(symbol, occurrences)| SessionDigestSymbolRef {
            symbol,
            occurrences,
        })
        .collect::<Vec<_>>();
    touched_symbols.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.symbol.cmp(&right.symbol))
    });
    let symbol_groups = touched_symbols.len();
    touched_symbols.truncate(MAX_SYMBOLS);

    let mut failures = state
        .failures
        .into_iter()
        .map(|((kind, message), occurrences)| SessionDigestFailure {
            kind,
            message,
            occurrences,
        })
        .collect::<Vec<_>>();
    failures.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.kind.cmp(&right.kind))
            .then(left.message.cmp(&right.message))
    });
    let failure_groups = failures.len();
    failures.truncate(MAX_FAILURES);

    let mut runtime_events = state
        .runtime_events
        .into_iter()
        .map(|(event, occurrences)| SessionDigestRuntimeEvent { event, occurrences })
        .collect::<Vec<_>>();
    runtime_events.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.event.cmp(&right.event))
    });
    let runtime_event_groups = runtime_events.len();
    runtime_events.truncate(MAX_RUNTIME_EVENTS);
    let restart_churn_groups = state.restart_churn.groups();
    let restart_churn = state.restart_churn.summaries();

    let mut closeout = state
        .closeout
        .into_iter()
        .map(|((kind, detail), occurrences)| SessionDigestCloseout {
            kind,
            detail,
            occurrences,
        })
        .collect::<Vec<_>>();
    closeout.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(left.kind.cmp(&right.kind))
            .then(left.detail.cmp(&right.detail))
    });
    let closeout_groups = closeout.len();
    closeout.truncate(MAX_CLOSEOUT);

    Ok(SessionDigestReport {
        root: root.display().to_string(),
        source: source.as_str().to_string(),
        total_lines,
        transcript_items: state.transcript_items,
        prompt_target_count,
        command_groups,
        file_groups,
        symbol_groups,
        failure_groups,
        runtime_event_groups,
        restart_churn_groups,
        closeout_groups,
        prompt_targets: state.prompt_targets,
        commands,
        touched_files,
        touched_symbols,
        failures,
        runtime_events,
        restart_churn,
        closeout,
        warnings: state.warnings,
    })
}

fn resolve_source(input: &str, source_hint: Option<&str>) -> Result<SessionDigestSource> {
    match source_hint {
        Some(raw) => SessionDigestSource::parse(raw),
        None => {
            let non_empty = input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if !non_empty.is_empty()
                && non_empty.iter().all(|line| {
                    line.starts_with('{') && serde_json::from_str::<Value>(line).is_ok()
                })
            {
                for line in &non_empty {
                    let value = serde_json::from_str::<Value>(line).unwrap_or(Value::Null);
                    if value
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .is_some()
                        || value
                            .get("message")
                            .and_then(|message| message.get("usage"))
                            .is_some()
                    {
                        return Ok(SessionDigestSource::ClaudeJsonl);
                    }
                    if value.get("type").and_then(Value::as_str) == Some("response_item")
                        || value.get("type").and_then(Value::as_str) == Some("event_msg")
                    {
                        return Ok(SessionDigestSource::CodexJsonl);
                    }
                }
                Ok(SessionDigestSource::ClaudeJsonl)
            } else if !non_empty.is_empty()
                && non_empty
                    .iter()
                    .all(|line| line.starts_with('[') && line.contains(']'))
            {
                Ok(SessionDigestSource::AgentDocLog)
            } else {
                Ok(SessionDigestSource::Markdown)
            }
        }
    }
}

fn ingest_markdown(root: &Path, input: &str, state: &mut DigestState) -> Result<()> {
    let mut in_frontmatter = false;
    let mut first_line = true;
    for line in input.lines() {
        let trimmed = line.trim();
        if first_line {
            first_line = false;
            if trimmed == "---" {
                in_frontmatter = true;
                state.transcript_items += 1;
                continue;
            }
        } else if in_frontmatter {
            state.transcript_items += 1;
            if trimmed == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        state.transcript_items += 1;
        ingest_text_line(root, line, false, state)?;
    }
    Ok(())
}

fn ingest_claude_jsonl(root: &Path, input: &str, state: &mut DigestState) -> Result<()> {
    for (index, raw_line) in input.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => {
                state.warnings.push(format!(
                    "skipping malformed Claude transcript jsonl line {}",
                    index + 1
                ));
                continue;
            }
        };
        let mut blocks = Vec::new();
        collect_transcript_blocks(&value, &mut blocks);
        if blocks.is_empty() {
            if !is_ignorable_claude_record(&value) {
                state.warnings.push(format!(
                    "jsonl line {} did not contain message content or tool_use blocks",
                    index + 1
                ));
            }
            continue;
        }
        for block in blocks {
            match block {
                TranscriptBlock::Text { role, text } => {
                    let user_bias = role
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case("user"));
                    ingest_text_block(root, &text, user_bias, state)?;
                }
                TranscriptBlock::ToolUse { name, input } => {
                    state.transcript_items += 1;
                    ingest_tool_use(root, &name, &input, state)?;
                }
            }
        }
    }
    Ok(())
}

fn ingest_codex_jsonl(root: &Path, input: &str, state: &mut DigestState) -> Result<()> {
    for (index, raw_line) in input.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(_) => {
                state.warnings.push(format!(
                    "skipping malformed Codex transcript jsonl line {}",
                    index + 1
                ));
                continue;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => ingest_codex_response_item(root, &value, index + 1, state)?,
            Some("event_msg") => ingest_codex_event_msg(root, &value, index + 1, state)?,
            _ => {}
        }
    }
    Ok(())
}

fn ingest_agent_doc_log(root: &Path, input: &str, state: &mut DigestState) {
    for raw_line in input.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((_, after_bracket)) = trimmed.split_once("] ") else {
            continue;
        };
        let detail = after_bracket.trim();
        let Some(event_name) = detail.split_whitespace().next() else {
            continue;
        };

        state.transcript_items += 1;
        let normalized_event = normalize_runtime_event(event_name, detail);
        if should_count_runtime_event(event_name, detail, &normalized_event, state) {
            *state.runtime_events.entry(normalized_event).or_default() += 1;
        }
        state.restart_churn.observe(event_name, detail);

        for key in ["file", "path", "project_root"] {
            if let Some(path) = extract_field(detail, key) {
                for normalized in extract_file_refs(path, root) {
                    *state.files.entry(normalized).or_default() += 1;
                }
            }
        }

        if matches!(event_name, "claude_exit" | "codex_exit")
            && extract_field(detail, "code").is_some_and(|code| code != "0")
        {
            let message = truncate_detail(
                &format!(
                    "{} exited with code {}",
                    event_name,
                    extract_field(detail, "code").unwrap_or("?")
                ),
                220,
            );
            *state
                .failures
                .entry(("exit".to_string(), message))
                .or_default() += 1;
        }

        if event_name.contains("timeout") {
            *state
                .failures
                .entry(("timeout".to_string(), truncate_detail(detail, 220)))
                .or_default() += 1;
        }

        for (kind, closeout) in detect_closeout(detail) {
            if should_count_closeout(event_name, detail, &kind, &closeout, state) {
                *state.closeout.entry((kind, closeout)).or_default() += 1;
            }
        }
    }
}

fn is_ignorable_claude_record(value: &Value) -> bool {
    value.get("attachment").is_some()
        || value.get("toolUseResult").is_some()
        || (value.get("message").is_none()
            && value.get("content").is_none()
            && value.get("text").is_none())
}

fn collect_transcript_blocks(value: &Value, out: &mut Vec<TranscriptBlock>) {
    if let Some(message) = value.get("message") {
        collect_message_blocks(message, out);
        return;
    }
    collect_message_blocks(value, out);
}

fn collect_message_blocks(value: &Value, out: &mut Vec<TranscriptBlock>) {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .map(|value| value.to_string());
    if let Some(content) = value.get("content") {
        match content {
            Value::String(text) => out.push(TranscriptBlock::Text {
                role,
                text: text.to_string(),
            }),
            Value::Array(items) => {
                for item in items {
                    collect_content_block(role.clone(), item, out);
                }
            }
            _ => {}
        }
    } else if let Some(text) = value.get("text").and_then(Value::as_str) {
        out.push(TranscriptBlock::Text {
            role,
            text: text.to_string(),
        });
    }
}

fn ingest_codex_response_item(
    root: &Path,
    value: &Value,
    line_number: usize,
    state: &mut DigestState,
) -> Result<()> {
    let Some(payload) = value.get("payload") else {
        return Ok(());
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "assistant" {
                return Ok(());
            }
            let Some(content) = payload.get("content").and_then(Value::as_array) else {
                return Ok(());
            };
            for item in content {
                let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("content").and_then(Value::as_str))
                else {
                    continue;
                };
                ingest_text_block(root, text, false, state)?;
            }
        }
        Some("function_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("function_call");
            let Some(arguments) = payload.get("arguments").and_then(Value::as_str) else {
                return Ok(());
            };
            let input = serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| {
                state.warnings.push(format!(
                    "codex function_call arguments on line {} were not valid JSON; command extraction may be incomplete",
                    line_number
                ));
                Value::String(arguments.to_string())
            });
            state.transcript_items += 1;
            ingest_tool_use(root, name, &input, state)?;
        }
        _ => {}
    }
    Ok(())
}

fn ingest_codex_event_msg(
    root: &Path,
    value: &Value,
    _line_number: usize,
    state: &mut DigestState,
) -> Result<()> {
    let Some(payload) = value.get("payload") else {
        return Ok(());
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => {
            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                ingest_text_block(root, message, true, state)?;
            }
        }
        Some("agent_message") => {
            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                ingest_text_block(root, message, false, state)?;
            }
        }
        Some("exec_command_end") => {
            state.transcript_items += 1;
            if let Some(command) = extract_codex_exec_command(payload) {
                *state.commands.entry(command.clone()).or_default() += 1;
                for path in extract_file_refs(&command, root) {
                    *state.files.entry(path).or_default() += 1;
                }
                for symbol in extract_symbol_refs(&command) {
                    *state.symbols.entry(symbol).or_default() += 1;
                }
            }
            if let Some(output) = payload
                .get("aggregated_output")
                .and_then(Value::as_str)
                .or_else(|| payload.get("stdout").and_then(Value::as_str))
            {
                for line in output.lines() {
                    ingest_text_line(root, line, false, state)?;
                }
            }
            if payload
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                != 0
            {
                let command =
                    extract_codex_exec_command(payload).unwrap_or_else(|| "command".to_string());
                let message = truncate_detail(
                    &format!(
                        "{} exited with code {}",
                        command,
                        payload
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .unwrap_or_default()
                    ),
                    220,
                );
                *state
                    .failures
                    .entry(("exit".to_string(), message))
                    .or_default() += 1;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_content_block(role: Option<String>, value: &Value, out: &mut Vec<TranscriptBlock>) {
    let block_type = value.get("type").and_then(Value::as_str);
    match block_type {
        Some("text") => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                out.push(TranscriptBlock::Text {
                    role,
                    text: text.to_string(),
                });
            }
        }
        Some("tool_use") => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool_use")
                .to_string();
            let input = value.get("input").cloned().unwrap_or(Value::Null);
            out.push(TranscriptBlock::ToolUse { name, input });
        }
        Some("tool_result") => match value.get("content") {
            Some(Value::String(text)) => out.push(TranscriptBlock::Text {
                role,
                text: text.to_string(),
            }),
            Some(Value::Array(items)) => {
                for item in items {
                    collect_content_block(role.clone(), item, out);
                }
            }
            _ => {}
        },
        _ => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                out.push(TranscriptBlock::Text {
                    role,
                    text: text.to_string(),
                });
            }
        }
    }
}

fn ingest_tool_use(root: &Path, name: &str, input: &Value, state: &mut DigestState) -> Result<()> {
    if let Some(command) = extract_tool_command(name, input) {
        *state.commands.entry(command.clone()).or_default() += 1;
        for path in extract_file_refs(&command, root) {
            *state.files.entry(path).or_default() += 1;
        }
        for symbol in extract_symbol_refs(&command) {
            *state.symbols.entry(symbol).or_default() += 1;
        }
    }

    if let Some(text) = extract_tool_text(input) {
        for line in text.lines() {
            ingest_text_line(root, line, false, state)?;
        }
    }
    Ok(())
}

fn ingest_text_block(
    root: &Path,
    text: &str,
    user_bias: bool,
    state: &mut DigestState,
) -> Result<()> {
    state.transcript_items += 1;
    for line in text.lines() {
        ingest_text_line(root, line, user_bias, state)?;
    }
    Ok(())
}

fn extract_tool_command(name: &str, input: &Value) -> Option<String> {
    if !matches!(
        name.to_ascii_lowercase().as_str(),
        "bash" | "exec_command" | "shell" | "terminal" | "sh"
    ) {
        return None;
    }

    match input {
        Value::Object(map) => {
            for key in ["command", "cmd", "shell_command"] {
                if let Some(raw) = map.get(key).and_then(Value::as_str) {
                    let normalized = normalize_whitespace(raw);
                    if looks_like_command(&normalized) {
                        return Some(normalized);
                    }
                }
            }
            None
        }
        Value::String(raw) => {
            let normalized = normalize_whitespace(raw);
            looks_like_command(&normalized).then_some(normalized)
        }
        _ => None,
    }
}

fn extract_tool_text(input: &Value) -> Option<String> {
    match input {
        Value::Object(map) => {
            for key in ["text", "output", "stderr", "stdout", "content", "message"] {
                if let Some(raw) = map.get(key).and_then(Value::as_str) {
                    return Some(raw.to_string());
                }
            }
            None
        }
        Value::String(raw) => Some(raw.to_string()),
        _ => None,
    }
}

fn extract_codex_exec_command(payload: &Value) -> Option<String> {
    if let Some(parsed) = payload.get("parsed_cmd").and_then(Value::as_array) {
        for item in parsed {
            if let Some(command) = item.get("cmd").and_then(Value::as_str) {
                let normalized = normalize_whitespace(command);
                if looks_like_command(&normalized) {
                    return Some(normalized);
                }
            }
        }
    }

    if let Some(command) = payload.get("command").and_then(Value::as_array)
        && let Some(last) = command.last().and_then(Value::as_str)
    {
        let normalized = normalize_whitespace(last);
        if looks_like_command(&normalized) {
            return Some(normalized);
        }
    }
    None
}

fn ingest_text_line(
    root: &Path,
    raw_line: &str,
    user_bias: bool,
    state: &mut DigestState,
) -> Result<()> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if looks_like_instruction_ballast(trimmed) {
        return Ok(());
    }

    let prompt_candidate = trimmed
        .strip_prefix("❯ ")
        .or_else(|| trimmed.strip_prefix("> "))
        .unwrap_or(trimmed)
        .trim();
    if looks_like_prompt_target(prompt_candidate, user_bias || prompt_candidate != trimmed) {
        push_prompt_target(prompt_candidate, &mut state.prompt_targets);
    }

    for command in extract_commands(trimmed) {
        *state.commands.entry(command.clone()).or_default() += 1;
        for path in extract_file_refs(&command, root) {
            *state.files.entry(path).or_default() += 1;
        }
        for symbol in extract_symbol_refs(&command) {
            *state.symbols.entry(symbol).or_default() += 1;
        }
    }

    for path in extract_file_refs(trimmed, root) {
        *state.files.entry(path).or_default() += 1;
    }
    for symbol in extract_symbol_refs(trimmed) {
        *state.symbols.entry(symbol).or_default() += 1;
    }

    if let Some((kind, message)) = classify_failure(trimmed) {
        *state.failures.entry((kind, message)).or_default() += 1;
    }
    for (kind, detail) in detect_closeout(trimmed) {
        *state.closeout.entry((kind, detail)).or_default() += 1;
    }

    Ok(())
}

fn push_prompt_target(prompt: &str, targets: &mut Vec<String>) {
    let normalized = normalize_whitespace(prompt);
    if normalized.is_empty() || targets.iter().any(|existing| existing == &normalized) {
        return;
    }
    if targets.len() < MAX_PROMPT_TARGETS {
        targets.push(normalized);
    }
}

pub(crate) fn extract_prompt_targets_from_text_block(input: &str, user_bias: bool) -> Vec<String> {
    let mut targets = Vec::new();
    for raw_line in input.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || looks_like_instruction_ballast(trimmed) {
            continue;
        }
        let prompt_candidate = trimmed
            .strip_prefix("❯ ")
            .or_else(|| trimmed.strip_prefix("> "))
            .unwrap_or(trimmed)
            .trim();
        if looks_like_prompt_target(prompt_candidate, user_bias || prompt_candidate != trimmed) {
            push_prompt_target(prompt_candidate, &mut targets);
        }
    }
    targets
}

fn looks_like_prompt_target(text: &str, user_bias: bool) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || looks_like_markdown_heading(trimmed)
        || looks_like_slash_command_example(trimmed)
        || trimmed == "#"
        || trimmed.starts_with("#!")
        || trimmed.starts_with("#[")
        || trimmed.starts_with("/**")
        || trimmed.starts_with("*/")
        || trimmed.starts_with("//")
        || trimmed.starts_with("###")
        || trimmed.starts_with("<!--")
        || trimmed.starts_with("- [")
        || trimmed == "###"
    {
        return false;
    }

    if trimmed.starts_with("do ")
        || trimmed.starts_with('#')
        || looks_like_slash_prompt_target(trimmed)
        || trimmed.ends_with('?')
    {
        return true;
    }

    if user_bias
        && (trimmed.contains("commit + push")
            || trimmed.contains("run tests")
            || trimmed.contains("build + install")
            || trimmed.contains("#spec-test"))
    {
        return true;
    }

    false
}

fn looks_like_instruction_ballast(text: &str) -> bool {
    let trimmed = strip_common_prefixes(text.trim());
    if trimmed.is_empty() {
        return false;
    }

    looks_like_markdown_heading(trimmed)
        || looks_like_slash_command_example(trimmed)
        || looks_like_frontmatter_prompt_preset(trimmed)
        || looks_like_completed_backlog_archive(trimmed)
        || trimmed.starts_with("<!-- tsift:")
        || trimmed.starts_with("<!-- /tsift:")
        || looks_like_instruction_label(trimmed)
}

fn looks_like_markdown_heading(text: &str) -> bool {
    let trimmed = text.trim_start();
    let heading_level = trimmed.chars().take_while(|ch| *ch == '#').count();
    heading_level > 0
        && heading_level <= 6
        && trimmed
            .chars()
            .nth(heading_level)
            .is_some_and(|ch| ch.is_whitespace())
}

fn looks_like_slash_command_example(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('/')
        && trimmed.contains('<')
        && trimmed.contains('>')
        && !trimmed.contains('`')
}

fn looks_like_frontmatter_prompt_preset(text: &str) -> bool {
    let trimmed = strip_common_prefixes(text.trim());
    if trimmed == "prompt_presets:" || trimmed.starts_with("prompt_presets:") {
        return true;
    }
    let Some((key, _)) = trimmed.split_once(':') else {
        return false;
    };
    let key = key.trim().trim_matches(['"', '\'']);
    key.starts_with('#') && key.len() > 1 && key[1..].chars().all(is_prompt_preset_char)
}

fn is_prompt_preset_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

fn looks_like_completed_backlog_archive(text: &str) -> bool {
    let stripped = strip_common_prefixes(text.trim());
    let Some(date) = stripped.get(..10) else {
        return false;
    };
    date.chars().enumerate().all(|(index, ch)| match index {
        4 | 7 => ch == '-',
        _ => ch.is_ascii_digit(),
    }) && stripped[10..].contains("[#")
}

fn looks_like_slash_prompt_target(text: &str) -> bool {
    let Some(first_token) = text.split_whitespace().next() else {
        return false;
    };
    first_token.starts_with('/') && !first_token[1..].contains('/')
}

fn looks_like_instruction_label(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with("**") {
        return false;
    }
    let Some(label_end) = trimmed[2..].find("**") else {
        return false;
    };
    let label = &trimmed[..label_end + 4];
    if label.len() <= 4 {
        return false;
    }
    let remainder = trimmed[label_end + 4..]
        .trim_start_matches([' ', ':', '-', '—'])
        .trim_start();
    if remainder.is_empty() {
        return false;
    }
    let lower = remainder.to_ascii_lowercase();
    matches!(
        lower.split_whitespace().next(),
        Some("run")
            | Some("use")
            | Some("treat")
            | Some("respond")
            | Some("print")
            | Some("prefer")
            | Some("preserve")
            | Some("show")
            | Some("complete")
            | Some("append")
            | Some("when")
            | Some("if")
    )
}

fn extract_commands(text: &str) -> Vec<String> {
    let mut commands = BTreeSet::new();
    for span in extract_backtick_spans(text) {
        let normalized = normalize_whitespace(&span);
        if looks_like_command(&normalized) {
            commands.insert(normalized);
        }
    }

    let stripped = strip_common_prefixes(text.trim());
    let normalized = normalize_whitespace(stripped);
    if looks_like_command(&normalized) {
        commands.insert(normalized);
    }

    commands.into_iter().collect()
}

fn extract_backtick_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch != '`' {
            continue;
        }
        match start {
            Some(span_start) => {
                if index > span_start + 1 {
                    spans.push(text[span_start + 1..index].to_string());
                }
                start = None;
            }
            None => start = Some(index),
        }
    }
    spans
}

fn strip_common_prefixes(text: &str) -> &str {
    text.strip_prefix("❯ ")
        .or_else(|| text.strip_prefix("- "))
        .or_else(|| text.strip_prefix("* "))
        .or_else(|| text.strip_prefix("> "))
        .unwrap_or(text)
        .trim()
}

fn looks_like_command(text: &str) -> bool {
    if text.is_empty()
        || text.contains('\n')
        || text.contains("://")
        || text.starts_with('/')
        || text.starts_with("###")
    {
        return false;
    }

    let head = text.split_whitespace().next().unwrap_or_default();
    matches!(
        head,
        "agent-doc"
            | "cargo"
            | "git"
            | "make"
            | "pytest"
            | "python"
            | "uv"
            | "tsift"
            | "npm"
            | "pnpm"
            | "yarn"
            | "bash"
            | "zsh"
            | "rg"
            | "grep"
            | "./scripts/run_benchmark.sh"
    ) || head.starts_with("./")
}

fn extract_file_refs(text: &str, root: &Path) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for raw in text.split_whitespace() {
        if let Some(path) = normalize_file_token(raw, root) {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

fn normalize_file_token(raw: &str, root: &Path) -> Option<String> {
    let trimmed = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '{' | '}' | '*'
        )
    });
    if trimmed.is_empty() || trimmed == "." || trimmed == "-" || trimmed.contains("://") {
        return None;
    }

    let value = trimmed
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(trimmed);
    let without_line = strip_line_suffix(value);
    let candidate = without_line.trim_end_matches('/');
    if candidate.is_empty() || candidate == "." {
        return None;
    }
    if !looks_like_file_path(candidate) {
        return None;
    }

    Some(normalize_display_path(root, candidate))
}

fn strip_line_suffix(token: &str) -> &str {
    let bytes = token.as_bytes();
    let mut cut = token.len();
    let mut colon_segments = 0;
    while let Some(colon_index) = token[..cut].rfind(':') {
        let suffix = &token[colon_index + 1..cut];
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            break;
        }
        colon_segments += 1;
        cut = colon_index;
        if colon_segments == 2 {
            break;
        }
        if colon_index == 0 || bytes[colon_index - 1] == b'/' {
            continue;
        }
    }
    &token[..cut]
}

fn looks_like_file_path(token: &str) -> bool {
    if token.starts_with("--") || token.starts_with('#') {
        return false;
    }

    if token.contains('/') {
        return true;
    }

    let lower = token.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "readme.md"
            | "agents.md"
            | "claude.md"
            | "spec.md"
            | "versions.md"
    ) || [
        ".rs", ".md", ".toml", ".json", ".jsonl", ".yaml", ".yml", ".txt", ".py", ".ts", ".tsx",
        ".js", ".jsx", ".sh", ".zsh", ".sql", ".db",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

fn normalize_display_path(root: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(root) {
            return normalize_path_string(relative);
        }
        return normalize_path_string(path);
    }
    normalize_path_string(path)
}

fn normalize_path_string(path: &Path) -> String {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            acc.push(component.as_os_str());
            acc
        })
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn extract_symbol_refs(text: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for span in extract_backtick_spans(text) {
        let candidate = span.trim().trim_end_matches("()");
        if looks_like_symbol(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    for raw in text.split(|ch: char| !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | ':')) {
        let candidate = raw.trim().trim_end_matches("()");
        if looks_like_symbol(candidate) {
            symbols.insert(candidate.to_string());
        }
    }

    symbols.into_iter().collect()
}

fn looks_like_symbol(candidate: &str) -> bool {
    if candidate.len() < 3
        || candidate.contains('/')
        || candidate.contains('.')
        || candidate.starts_with('#')
        || matches!(
            candidate,
            "Error" | "FAILED" | "cargo" | "pytest" | "agent" | "commit" | "push"
        )
    {
        return false;
    }

    let lower = candidate.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "none"
            | "error"
            | "failed"
            | "warning"
            | "commit"
            | "pushed"
            | "status"
            | "stdout"
            | "stderr"
    ) {
        return false;
    }

    candidate.contains('_') || candidate.contains("::")
}

fn classify_failure(text: &str) -> Option<(String, String)> {
    let normalized = normalize_whitespace(strip_common_prefixes(text));
    if is_non_failure_summary(&normalized) {
        return None;
    }
    let lower = normalized.to_ascii_lowercase();
    let kind = if lower.contains("timed out") {
        "timeout"
    } else if lower.starts_with("error") || lower.contains(" error:") || lower.contains("error:") {
        "error"
    } else if lower.contains("panicked") || lower.contains("panic") {
        "panic"
    } else if lower.contains("not found")
        || lower.contains(" is missing")
        || lower.contains(" missing ")
    {
        "missing"
    } else if lower.contains("failed") || lower.contains("failure") {
        "failure"
    } else {
        return None;
    };
    Some((kind.to_string(), truncate_detail(&normalized, 220)))
}

fn is_non_failure_summary(text: &str) -> bool {
    let normalized = text.trim();
    if normalized.is_empty() {
        return false;
    }
    let lower = normalized.to_ascii_lowercase();
    let compact = lower.trim_matches(['.', ':', ';', ',']).trim();
    if matches!(
        compact,
        "failure" | "failures" | "failure summary" | "failure summaries"
    ) {
        return true;
    }
    if lower.starts_with("no failures detected")
        || lower.starts_with("no failure detected")
        || lower.starts_with("no unresolved failures")
    {
        return true;
    }
    if lower.starts_with("test result: ok.") || lower.starts_with("test result: ok;") {
        return true;
    }
    if lower.contains("0 failed")
        && (lower.contains(" passed") || lower.contains(" ok") || lower.contains("filtered out"))
        && !lower.contains("failed to")
        && !lower.contains("assertion failed")
        && !lower.contains("test result: failed")
    {
        return true;
    }
    false
}

fn detect_closeout(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let normalized = normalize_whitespace(strip_common_prefixes(text));
    let lower = normalized.to_ascii_lowercase();

    if normalized.starts_with("document_cycle ") {
        let phase = extract_field(&normalized, "phase");
        let event = extract_field(&normalized, "event");
        if phase == Some("committed")
            && let Some(event) = event
        {
            out.push((
                "commit".to_string(),
                format!("document_cycle phase=committed event={event}"),
            ));
        }
        return dedupe_pairs(out);
    }

    if lower.contains("verification passed") || lower.starts_with("verification in ") {
        out.push((
            "verification".to_string(),
            truncate_detail(&normalized, 220),
        ));
    }
    if lower.contains("cargo build")
        || lower.contains("make check")
        || lower.contains("cargo test")
        || lower.contains("pytest")
    {
        out.push((
            "verification".to_string(),
            truncate_detail(&normalized, 220),
        ));
    }
    if lower.contains("cargo install") || lower.contains("installed") {
        out.push(("install".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("committed and pushed") {
        out.push(("push".to_string(), truncate_detail(&normalized, 220)));
    } else if lower.contains("committed") {
        out.push(("commit".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("tsift --version") || lower.contains("tsift v0.") {
        out.push(("version".to_string(), truncate_detail(&normalized, 220)));
    }
    if lower.contains("agent-doc finalize") || lower.contains("session-check") {
        out.push(("closeout".to_string(), truncate_detail(&normalized, 220)));
    }

    dedupe_pairs(out)
}

fn normalize_runtime_event(event_name: &str, detail: &str) -> String {
    if event_name == "document_cycle"
        && let Some(document_event) = extract_field(detail, "event")
    {
        return document_event.to_string();
    }
    if matches!(
        event_name,
        "claude_start" | "codex_start" | "claude_restart" | "codex_restart"
    ) && let Some(mode) = extract_field(detail, "mode")
    {
        return format!("{event_name}:{mode}");
    }
    event_name.to_string()
}

fn should_count_runtime_event(
    event_name: &str,
    detail: &str,
    normalized: &str,
    state: &mut DigestState,
) -> bool {
    if event_name == "document_cycle"
        && let Some(cycle) = extract_field(detail, "cycle")
    {
        return state
            .seen_document_cycle_events
            .insert((cycle.to_string(), normalized.to_string()));
    }
    true
}

fn should_count_closeout(
    event_name: &str,
    detail: &str,
    kind: &str,
    closeout: &str,
    state: &mut DigestState,
) -> bool {
    if event_name == "document_cycle"
        && let Some(cycle) = extract_field(detail, "cycle")
    {
        return state.seen_document_cycle_closeout.insert((
            cycle.to_string(),
            kind.to_string(),
            closeout.to_string(),
        ));
    }
    true
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

fn dedupe_pairs(items: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            deduped.push(item);
        }
    }
    deduped
}

fn normalize_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_detail(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = String::new();
    for ch in text.chars().take(max_chars.saturating_sub(1)) {
        truncated.push(ch);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_digest_extracts_prompt_commands_failures_and_closeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

        let input = "\
❯ Why was this symbol search attempted?
Symbol `run_sync` not found in index.
Error: tsift search timed out after 30s at src/lib.rs:7:9
Verification in `src/tsift`: `cargo test`, `make check`, `cargo build --release`, `cargo install --path . --force`
Committed and pushed in `src/tsift` as `1af09d3` (`feat: add metric run digest`).
do [#sessiondigest]. spec-test-build-install-commit-push
";

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(report.source, "markdown");
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("Why was this symbol search attempted?"))
        );
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("[#sessiondigest]"))
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.command == "cargo test")
        );
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "src/lib.rs")
        );
        assert!(
            report
                .touched_symbols
                .iter()
                .any(|symbol| symbol.symbol == "run_sync")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == "timeout")
        );
        assert!(
            report
                .closeout
                .iter()
                .any(|entry| entry.kind == "verification")
        );
        assert!(report.closeout.iter().any(|entry| entry.kind == "push"));
    }

    #[test]
    fn jsonl_digest_extracts_user_prompt_and_shell_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

        let input = concat!(
            r#"{"message":{"role":"user","content":"do [#sessiondigest]. spec-test-build-install-commit-push"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test --release --manifest-path Cargo.toml"}},{"type":"text","text":"Symbol `run_sync` not found in index.\nCommitted and pushed in `src/tsift` as `1af09d3`."}]}}"#,
            "\n"
        );

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(report.source, "claude_jsonl");
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("[#sessiondigest]"))
        );
        assert!(report
            .commands
            .iter()
            .any(|command| command.command == "cargo test --release --manifest-path Cargo.toml"));
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "Cargo.toml")
        );
        assert!(
            report
                .touched_symbols
                .iter()
                .any(|symbol| symbol.symbol == "run_sync")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| matches!(failure.kind.as_str(), "error" | "missing"))
        );
        assert!(report.closeout.iter().any(|entry| entry.kind == "push"));
    }

    #[test]
    fn codex_jsonl_digest_extracts_prompt_command_failures_and_closeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn run_sync() {}\n").unwrap();

        let input = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"ignore this instruction blob"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#cdxlog]. spec-test-build-install-commit-push"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test --manifest-path Cargo.toml\"}","call_id":"call_1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","exit_code":1,"aggregated_output":"Error: Symbol `run_sync` not found in src/lib.rs:7:9\nVerification in `src/tsift`: `cargo test`\nCommitted and pushed in `src/tsift` as `943d77d`.","parsed_cmd":[{"type":"unknown","cmd":"cargo test --manifest-path Cargo.toml"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I’m checking `src/tsift/SPEC.md` next."}}"#,
            "\n"
        );

        let report = compute(dir.path(), input, Some("codex-jsonl")).unwrap();
        assert_eq!(report.source, "codex_jsonl");
        assert!(
            report
                .prompt_targets
                .iter()
                .any(|target| target.contains("[#cdxlog]"))
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.command == "cargo test --manifest-path Cargo.toml")
        );
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "Cargo.toml")
        );
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "src/lib.rs")
        );
        assert!(
            report
                .touched_symbols
                .iter()
                .any(|symbol| symbol.symbol == "run_sync")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| matches!(failure.kind.as_str(), "error" | "missing"))
        );
        assert!(report.failures.iter().any(|failure| failure.kind == "exit"));
        assert!(report.closeout.iter().any(|entry| entry.kind == "push"));
    }

    #[test]
    fn markdown_digest_ignores_copied_instruction_ballast() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
# agent-doc
## Invocation
/agent-doc <FILE>
**Auto-update skill:** Run `agent-doc --version` and compare against `agent-doc-version`.
- **Imperative edits are executable directives** — when the user writes `do #id`, `run tests`, `build + install`, or `commit + push`
**Compound task steering:** if one directive mixes commit + push, normalize it before execution.
/home/brian/work/btakita/agent-loop/src/boost-client
#[test]
//!
/**
#!/usr/bin/env bash
#
do [#sessiondigest]. spec-test-build-install-commit-push
";

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(
            report.prompt_targets,
            vec!["do [#sessiondigest]. spec-test-build-install-commit-push".to_string()]
        );
        assert!(report.failures.is_empty());
    }

    #[test]
    fn prompt_target_digest_ignores_frontmatter_presets_and_completed_archives() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
---
agent_doc_format: template
prompt_presets:
  '#agent-doc-bug': Please create a plan for agent-doc to fix this issue.
  '#spec-test-build-install-commit-push': update spec + tests. build + install for local testing. commit + push
---

## Exchange

<!-- agent:exchange patch=append -->
do [#active]. spec-test-build-install-commit-push
<!-- /agent:exchange -->

## Completed / Reaped

<!-- agent:done -->
- 2026-05-12 [#old1] Add an old completed task.
- 2026-05-12 [#old2] do [#old2]. spec-test-build-install-commit-push
<!-- /agent:done -->
";

        let report = compute(dir.path(), input, None).unwrap();
        assert_eq!(
            report.prompt_targets,
            vec!["do [#active]. spec-test-build-install-commit-push".to_string()]
        );
    }

    #[test]
    fn codex_digest_ignores_copied_frontmatter_prompt_presets() {
        let dir = tempfile::tempdir().unwrap();
        let input = concat!(
            r##"{"type":"event_msg","payload":{"type":"user_message","message":"---\nprompt_presets:\n  '#spec-test-build-install-commit-push': update spec + tests. build + install for local testing. commit + push\n---\n/agent-doc <FILE>\ndo [#cdxactive]. spec-test-build-install-commit-push"}}"##,
            "\n"
        );

        let report = compute(dir.path(), input, Some("codex-jsonl")).unwrap();
        assert_eq!(
            report.prompt_targets,
            vec!["do [#cdxactive]. spec-test-build-install-commit-push".to_string()]
        );
    }

    #[test]
    fn markdown_digest_ignores_successful_test_summaries_and_failure_labels() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
failures:
No failures detected (runner: cargo).
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out
pytest summary: 4 passed, 0 failed in 0.02s
";

        let report = compute(dir.path(), input, None).unwrap();
        assert!(report.failures.is_empty());
    }

    #[test]
    fn markdown_digest_keeps_real_failure_lines() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
thread 'suite::alpha_failure' panicked at src/lib.rs:3:5:
assertion failed: left == right
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";

        let report = compute(dir.path(), input, None).unwrap();
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == "panic")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.message.contains("assertion failed"))
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.message.contains("test result: FAILED"))
        );
    }

    #[test]
    fn codex_jsonl_digest_filters_instruction_blob_lines_but_keeps_user_directive() {
        let dir = tempfile::tempdir().unwrap();
        let input = concat!(
            r##"{"type":"event_msg","payload":{"type":"user_message","message":"# agent-doc\n## Workflow\n/agent-doc <FILE>\n**Auto-update skill:** Run `agent-doc --version` and compare against `agent-doc-version`.\ndo [#cdxlog]. spec-test-build-install-commit-push"}}"##,
            "\n"
        );

        let report = compute(dir.path(), input, Some("codex-jsonl")).unwrap();
        assert_eq!(
            report.prompt_targets,
            vec!["do [#cdxlog]. spec-test-build-install-commit-push".to_string()]
        );
        assert!(report.failures.is_empty());
    }

    #[test]
    fn agent_doc_log_digest_extracts_runtime_events_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tasks/software")).unwrap();
        std::fs::write(dir.path().join("tasks/software/tsift.md"), "# tsift\n").unwrap();

        let input = "\
[1776452736] session_start file=tasks/software/tsift.md pane=%141 session=tsift-v0
[1776528398] claude_start mode=fresh_restart restart_count=1
[1776528446] auto_trigger_timeout (no prompt after 30s)
[1776528450] ctrl_d_restart_fresh restart_count=2
[1776528532] claude_exit code=1 restart_count=0
[1776528534] user_quit_after_ctrl_d
";

        let report = compute(dir.path(), input, Some("agent-doc-log")).unwrap();
        assert_eq!(report.source, "agent_doc_log");
        assert_eq!(report.runtime_event_groups, 6);
        assert_eq!(report.restart_churn_groups, 4);
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "claude_start:fresh_restart")
        );
        assert!(
            report
                .touched_files
                .iter()
                .any(|path| path.path == "tasks/software/tsift.md")
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == "timeout")
        );
        assert!(report.failures.iter().any(|failure| failure.kind == "exit"));
        assert!(
            report
                .restart_churn
                .iter()
                .any(|entry| entry.family == "fresh_restart" && entry.occurrences == 2)
        );
        assert!(
            report
                .restart_churn
                .iter()
                .any(|entry| entry.family == "ctrl_d_restart_loop" && entry.occurrences == 1)
        );
        assert!(
            report
                .restart_churn
                .iter()
                .any(|entry| entry.family == "quit_after_eof" && entry.occurrences == 1)
        );
    }

    #[test]
    fn agent_doc_log_digest_dedupes_document_cycle_closeouts_by_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let input = "\
[1777603275] document_cycle phase=response_captured cycle=cycle-1 event=response_captured capture_id=cycle-1
[1777603276] document_cycle phase=committed cycle=cycle-1 event=commit_success capture_id=cycle-1
[1777603403] document_cycle phase=committed cycle=cycle-1 event=commit_already_current capture_id=cycle-1
[1777603404] document_cycle phase=committed cycle=cycle-1 event=commit_already_current capture_id=cycle-1
[1777603600] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
[1777603601] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
[1777603700] document_cycle phase=committed cycle=cycle-3 event=commit_already_current
";

        let report = compute(dir.path(), input, Some("agent-doc-log")).unwrap();

        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "commit_already_current" && event.occurrences == 3)
        );
        assert!(report.closeout.iter().any(|entry| {
            entry.kind == "commit"
                && entry.detail == "document_cycle phase=committed event=commit_already_current"
                && entry.occurrences == 3
        }));
        assert!(report.closeout.iter().any(|entry| {
            entry.kind == "commit"
                && entry.detail == "document_cycle phase=committed event=commit_success"
                && entry.occurrences == 1
        }));
    }
}
