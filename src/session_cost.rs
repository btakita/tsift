use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::runtime_churn::{RestartChurnState, RestartChurnSummary};

const MAX_LARGEST_TURNS: usize = 5;
const MAX_RUNTIME_EVENTS: usize = 8;
const MAX_GUARDRAILS: usize = 8;
const MAX_LOOP_CLUSTERS: usize = 8;
const MAX_COMMANDS_PER_BUNDLE: usize = 6;
const PROMPT_BUDGET_WARN_TOKENS: u64 = 100_000;
const CACHED_RATIO_WARN_PERCENT: f64 = 90.0;
const CACHED_RATIO_WARN_PROMPT_TOKENS: u64 = 50_000;
const RESTART_LOOP_WARN_OCCURRENCES: usize = 3;
const NOOP_CLOSEOUT_WARN_OCCURRENCES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCostSource {
    ClaudeJsonl,
    CodexJsonl,
    AgentDocLog,
}

impl SessionCostSource {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-jsonl" => Ok(Self::ClaudeJsonl),
            "codex" | "codex-jsonl" => Ok(Self::CodexJsonl),
            "agent-doc-log" | "agent_doc_log" | "log" => Ok(Self::AgentDocLog),
            other => bail!(
                "unsupported session-cost source `{other}`; expected claude-jsonl, codex-jsonl, or agent-doc-log"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeJsonl => "claude_jsonl",
            Self::CodexJsonl => "codex_jsonl",
            Self::AgentDocLog => "agent_doc_log",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostTurn {
    pub label: String,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostRuntimeEvent {
    pub event: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostGuardrail {
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub guidance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostLoopCluster {
    pub kind: String,
    pub label: String,
    pub occurrences: usize,
    pub max_consecutive: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SessionCostGuardrailInput {
    pub largest_prompt_turn_tokens: u64,
    pub largest_prompt_turn_label: Option<String>,
    pub prompt_tokens: u64,
    pub cached_input_ratio: Option<f64>,
    pub fresh_restart_occurrences: usize,
    pub auto_trigger_timeout_occurrences: usize,
    pub ctrl_d_restart_loop_occurrences: usize,
    pub noop_closeout_occurrences: usize,
    pub max_restart_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionCostReport {
    pub source: String,
    pub record_count: usize,
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
    pub runtime_event_groups: usize,
    pub total_runtime_events: usize,
    pub restart_churn_groups: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_restart_count: Option<usize>,
    pub largest_turns: Vec<SessionCostTurn>,
    pub runtime_events: Vec<SessionCostRuntimeEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub loop_clusters: Vec<SessionCostLoopCluster>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub restart_churn: Vec<RestartChurnSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub guardrails: Vec<SessionCostGuardrail>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageTotals {
    prompt_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl UsageTotals {
    fn delta_from(self, previous: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_sub(previous.prompt_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(previous.cache_creation_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
        }
    }

    fn is_zero(self) -> bool {
        self.prompt_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
            && self.total_tokens == 0
    }
}

#[derive(Debug, Default)]
struct CostState {
    warnings: Vec<String>,
    usage_turns: Vec<SessionCostTurn>,
    runtime_events: BTreeMap<String, usize>,
    seen_document_cycle_events: BTreeSet<(String, String)>,
    total_runtime_events: usize,
    max_restart_count: Option<usize>,
    restart_churn: RestartChurnState,
    pending_commands: Vec<String>,
    loop_signals: Vec<LoopSignal>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LoopSignal {
    kind: LoopClusterKind,
    label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LoopClusterKind {
    PromptRepeat,
    CommandBundle,
    CloseoutChurn,
}

impl LoopClusterKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::PromptRepeat => "prompt_repeat",
            Self::CommandBundle => "command_bundle",
            Self::CloseoutChurn => "closeout_churn",
        }
    }
}

#[derive(Debug, Clone)]
enum TranscriptBlock {
    Text { role: Option<String>, text: String },
    ToolUse { name: String, input: Value },
}

pub fn compute(input: &str, source_hint: Option<&str>) -> Result<SessionCostReport> {
    if input.trim().is_empty() {
        bail!(
            "no session-cost input provided; pass --input <file> or pipe transcript/log data on stdin"
        );
    }

    let source = resolve_source(input, source_hint)?;
    let mut state = CostState::default();
    let record_count = input.lines().filter(|line| !line.trim().is_empty()).count();

    match source {
        SessionCostSource::ClaudeJsonl => ingest_claude_jsonl(input, &mut state)?,
        SessionCostSource::CodexJsonl => ingest_codex_jsonl(input, &mut state)?,
        SessionCostSource::AgentDocLog => ingest_agent_doc_log(input, &mut state),
    }

    let usage_samples = state.usage_turns.len();
    let mut prompt_tokens = 0_u64;
    let mut cached_input_tokens = 0_u64;
    let mut cache_creation_input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut reasoning_output_tokens = 0_u64;
    let mut total_tokens = 0_u64;
    let mut largest_turn_total_tokens = 0_u64;
    for turn in &state.usage_turns {
        prompt_tokens += turn.prompt_tokens;
        cached_input_tokens += turn.cached_input_tokens;
        cache_creation_input_tokens += turn.cache_creation_input_tokens;
        output_tokens += turn.output_tokens;
        reasoning_output_tokens += turn.reasoning_output_tokens;
        total_tokens += turn.total_tokens;
        largest_turn_total_tokens = largest_turn_total_tokens.max(turn.total_tokens);
    }

    let cached_input_ratio = (prompt_tokens > 0).then_some(
        ((cached_input_tokens as f64) / (prompt_tokens as f64) * 10_000.0).round() / 100.0,
    );
    let largest_prompt_turn = state
        .usage_turns
        .iter()
        .max_by(|left, right| {
            left.prompt_tokens
                .cmp(&right.prompt_tokens)
                .then(left.label.cmp(&right.label))
        })
        .map(|turn| (turn.prompt_tokens, turn.label.clone()));
    let noop_closeout_occurrences = state
        .runtime_events
        .get("commit_already_current")
        .copied()
        .unwrap_or(0);
    flush_pending_commands(&mut state);
    let loop_clusters = collect_loop_clusters(&state.loop_signals);

    let mut largest_turns = state.usage_turns;
    largest_turns.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then(right.prompt_tokens.cmp(&left.prompt_tokens))
            .then(left.label.cmp(&right.label))
    });
    largest_turns.truncate(MAX_LARGEST_TURNS);

    let mut runtime_events = state
        .runtime_events
        .into_iter()
        .map(|(event, occurrences)| SessionCostRuntimeEvent { event, occurrences })
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
    let guardrails = derive_guardrails(&SessionCostGuardrailInput {
        largest_prompt_turn_tokens: largest_prompt_turn.as_ref().map_or(0, |turn| turn.0),
        largest_prompt_turn_label: largest_prompt_turn.as_ref().map(|turn| turn.1.clone()),
        prompt_tokens,
        cached_input_ratio,
        fresh_restart_occurrences: count_restart_family(&restart_churn, "fresh_restart"),
        auto_trigger_timeout_occurrences: count_restart_family(
            &restart_churn,
            "auto_trigger_timeout",
        ),
        ctrl_d_restart_loop_occurrences: count_restart_family(
            &restart_churn,
            "ctrl_d_restart_loop",
        ),
        noop_closeout_occurrences,
        max_restart_count: state.max_restart_count,
    });

    if usage_samples == 0 && runtime_event_groups == 0 {
        state
            .warnings
            .push("no cost or runtime signals were detected in the provided input".to_string());
    }

    Ok(SessionCostReport {
        source: source.as_str().to_string(),
        record_count,
        usage_samples,
        prompt_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        cached_input_ratio,
        largest_turn_total_tokens,
        runtime_event_groups,
        total_runtime_events: state.total_runtime_events,
        restart_churn_groups,
        max_restart_count: state.max_restart_count,
        largest_turns,
        runtime_events,
        loop_clusters,
        restart_churn,
        guardrails,
        warnings: state.warnings,
    })
}

pub fn derive_guardrails(input: &SessionCostGuardrailInput) -> Vec<SessionCostGuardrail> {
    let mut guardrails = Vec::new();

    if input.largest_prompt_turn_tokens >= PROMPT_BUDGET_WARN_TOKENS {
        let label = input
            .largest_prompt_turn_label
            .as_deref()
            .map(|label| format!(" at {label}"))
            .unwrap_or_default();
        guardrails.push(SessionCostGuardrail {
            kind: "prompt_budget".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "largest prompt turn reached {} tokens{label}",
                input.largest_prompt_turn_tokens
            ),
            guidance:
                "compact the session or split the task before another large turn resends the same context"
                    .to_string(),
        });
    }

    if input.prompt_tokens >= CACHED_RATIO_WARN_PROMPT_TOKENS
        && input
            .cached_input_ratio
            .is_some_and(|ratio| ratio >= CACHED_RATIO_WARN_PERCENT)
    {
        guardrails.push(SessionCostGuardrail {
            kind: "cache_resend".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "cached input ratio was {:.2}% across {} prompt tokens",
                input.cached_input_ratio.unwrap_or_default(),
                input.prompt_tokens
            ),
            guidance:
                "compact or restart the session when most prompt spend is cached context instead of new work"
                    .to_string(),
        });
    }

    let restart_signal_count = input.fresh_restart_occurrences
        + input.auto_trigger_timeout_occurrences
        + input.ctrl_d_restart_loop_occurrences;
    if restart_signal_count >= RESTART_LOOP_WARN_OCCURRENCES
        || input.ctrl_d_restart_loop_occurrences > 0
        || input.auto_trigger_timeout_occurrences > 0
        || input.max_restart_count.is_some_and(|count| count >= 2)
    {
        let max_restart = input
            .max_restart_count
            .map(|count| format!(" max_restart={count}."))
            .unwrap_or_default();
        guardrails.push(SessionCostGuardrail {
            kind: "restart_loop".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "restart churn detected: fresh_restart={} auto_trigger_timeout={} ctrl_d_restart_loop={}.{}",
                input.fresh_restart_occurrences,
                input.auto_trigger_timeout_occurrences,
                input.ctrl_d_restart_loop_occurrences,
                max_restart
            )
            .trim()
            .to_string(),
            guidance:
                "fix the startup/retry issue before another restart, or compact and reopen cleanly instead of looping"
                    .to_string(),
        });
    }

    if input.noop_closeout_occurrences >= NOOP_CLOSEOUT_WARN_OCCURRENCES {
        guardrails.push(SessionCostGuardrail {
            kind: "noop_closeout".to_string(),
            severity: "warn".to_string(),
            message: format!(
                "commit_already_current appeared {} times",
                input.noop_closeout_occurrences
            ),
            guidance:
                "compact the document or avoid reopening it without new edits when closeouts are mostly no-ops"
                    .to_string(),
        });
    }

    guardrails.truncate(MAX_GUARDRAILS);
    guardrails
}

fn resolve_source(input: &str, source_hint: Option<&str>) -> Result<SessionCostSource> {
    if let Some(raw) = source_hint {
        return SessionCostSource::parse(raw);
    }

    let non_empty = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if non_empty.is_empty() {
        bail!(
            "no session-cost input provided; pass --input <file> or pipe transcript/log data on stdin"
        );
    }

    if non_empty
        .iter()
        .all(|line| line.starts_with('{') && serde_json::from_str::<Value>(line).is_ok())
    {
        for line in &non_empty {
            let value = serde_json::from_str::<Value>(line).unwrap_or(Value::Null);
            if value
                .get("message")
                .and_then(|message| message.get("usage"))
                .is_some()
            {
                return Ok(SessionCostSource::ClaudeJsonl);
            }
            if value.get("type").and_then(Value::as_str) == Some("event_msg")
                && value
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("token_count")
            {
                return Ok(SessionCostSource::CodexJsonl);
            }
        }
        if non_empty.iter().any(|line| line.contains("\"parentUuid\"")) {
            return Ok(SessionCostSource::ClaudeJsonl);
        }
        if non_empty
            .iter()
            .any(|line| line.contains("\"response_item\"") || line.contains("\"turn_context\""))
        {
            return Ok(SessionCostSource::CodexJsonl);
        }
    }

    if non_empty
        .iter()
        .all(|line| line.starts_with('[') && line.contains(']'))
    {
        return Ok(SessionCostSource::AgentDocLog);
    }

    bail!(
        "could not auto-detect session-cost input; pass --source claude-jsonl, codex-jsonl, or agent-doc-log"
    )
}

fn ingest_claude_jsonl(input: &str, state: &mut CostState) -> Result<()> {
    let mut seen_keys = BTreeSet::new();
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
        let Some(message) = value.get("message") else {
            collect_claude_loop_signals(&value, state);
            continue;
        };
        collect_claude_loop_signals(&value, state);
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(usage) = message.get("usage") else {
            continue;
        };

        let key = message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("requestId").and_then(Value::as_str))
            .or_else(|| value.get("uuid").and_then(Value::as_str))
            .map(|value| value.to_string())
            .unwrap_or_else(|| format!("line-{}", index + 1));
        if !seen_keys.insert(key.clone()) {
            continue;
        }

        let prompt_tokens = usage_u64(usage, "input_tokens")
            + usage_u64(usage, "cache_creation_input_tokens")
            + usage_u64(usage, "cache_read_input_tokens");
        let cached_input_tokens = usage_u64(usage, "cache_read_input_tokens");
        let cache_creation_input_tokens = usage_u64(usage, "cache_creation_input_tokens");
        let output_tokens = usage_u64(usage, "output_tokens");
        let total_tokens = prompt_tokens + output_tokens;
        if prompt_tokens == 0 && output_tokens == 0 {
            continue;
        }

        state.usage_turns.push(SessionCostTurn {
            label: value
                .get("timestamp")
                .and_then(Value::as_str)
                .map(|value| value.to_string())
                .unwrap_or(key),
            prompt_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            output_tokens,
            reasoning_output_tokens: 0,
            total_tokens,
        });
    }
    Ok(())
}

fn ingest_codex_jsonl(input: &str, state: &mut CostState) -> Result<()> {
    let mut previous = UsageTotals::default();
    let mut saw_token_count = false;
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
            Some("response_item") => {
                collect_codex_response_item_loop_signals(&value, index + 1, state)
            }
            Some("event_msg") => collect_codex_event_msg_loop_signals(&value, index + 1, state),
            _ => {}
        }
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        saw_token_count = true;

        let Some(total) = payload
            .get("info")
            .and_then(|info| info.get("total_token_usage"))
        else {
            state.warnings.push(format!(
                "codex token_count event on line {} did not include info.total_token_usage",
                index + 1
            ));
            continue;
        };
        let cumulative = UsageTotals {
            prompt_tokens: usage_u64(total, "input_tokens"),
            cached_input_tokens: usage_u64(total, "cached_input_tokens"),
            cache_creation_input_tokens: 0,
            output_tokens: usage_u64(total, "output_tokens"),
            reasoning_output_tokens: usage_u64(total, "reasoning_output_tokens"),
            total_tokens: usage_u64(total, "total_tokens"),
        };
        let delta = if previous.is_zero() {
            cumulative
        } else {
            cumulative.delta_from(previous)
        };
        previous = cumulative;
        if delta.is_zero() {
            continue;
        }

        state.usage_turns.push(SessionCostTurn {
            label: value
                .get("timestamp")
                .and_then(Value::as_str)
                .map(|value| value.to_string())
                .unwrap_or_else(|| format!("line-{}", index + 1)),
            prompt_tokens: delta.prompt_tokens,
            cached_input_tokens: delta.cached_input_tokens,
            cache_creation_input_tokens: 0,
            output_tokens: delta.output_tokens,
            reasoning_output_tokens: delta.reasoning_output_tokens,
            total_tokens: delta
                .total_tokens
                .max(delta.prompt_tokens + delta.output_tokens),
        });
    }

    if !saw_token_count {
        state.warnings.push(
            "codex transcript did not contain any token_count events; no token cost summary could be derived"
                .to_string(),
        );
    }
    Ok(())
}

fn ingest_agent_doc_log(input: &str, state: &mut CostState) {
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
        let normalized = normalize_runtime_event(event_name, detail);
        let closeout_event = is_closeout_runtime_event(event_name, &normalized);
        if should_count_runtime_event(event_name, detail, &normalized, state) {
            *state.runtime_events.entry(normalized.clone()).or_default() += 1;
            state.total_runtime_events += 1;
            if closeout_event {
                push_closeout_signal(&normalized, state);
            }
        }
        state.restart_churn.observe(event_name, detail);
        if let Some(restart_count) =
            extract_field(detail, "restart_count").and_then(|value| value.parse::<usize>().ok())
        {
            state.max_restart_count = Some(
                state
                    .max_restart_count
                    .map_or(restart_count, |current| current.max(restart_count)),
            );
        }
    }
}

fn collect_claude_loop_signals(value: &Value, state: &mut CostState) {
    let mut blocks = Vec::new();
    collect_transcript_blocks(value, &mut blocks);
    if blocks.is_empty() && is_ignorable_claude_record(value) {
        return;
    }
    for block in blocks {
        match block {
            TranscriptBlock::Text { role, text } => {
                let user_bias = role
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case("user"));
                collect_text_loop_signals(&text, user_bias, state);
            }
            TranscriptBlock::ToolUse { name, input } => {
                collect_tool_use_loop_signals(&name, &input, state);
            }
        }
    }
}

fn collect_codex_response_item_loop_signals(
    value: &Value,
    line_number: usize,
    state: &mut CostState,
) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let Some(content) = payload.get("content").and_then(Value::as_array) else {
                return;
            };
            for item in content {
                let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("content").and_then(Value::as_str))
                else {
                    continue;
                };
                collect_text_loop_signals(text, false, state);
            }
        }
        Some("function_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("function_call");
            let Some(arguments) = payload.get("arguments").and_then(Value::as_str) else {
                return;
            };
            let input = serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| {
                state.warnings.push(format!(
                    "codex function_call arguments on line {} were not valid JSON; loop extraction may be incomplete",
                    line_number
                ));
                Value::String(arguments.to_string())
            });
            collect_tool_use_loop_signals(name, &input, state);
        }
        _ => {}
    }
}

fn collect_codex_event_msg_loop_signals(value: &Value, _line_number: usize, state: &mut CostState) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => {
            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                collect_text_loop_signals(message, true, state);
            }
        }
        Some("agent_message") => {
            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                collect_text_loop_signals(message, false, state);
            }
        }
        Some("exec_command_end") => {
            if let Some(command) = extract_codex_exec_command(payload) {
                push_command(command, state);
            }
            if let Some(output) = payload
                .get("aggregated_output")
                .and_then(Value::as_str)
                .or_else(|| payload.get("stdout").and_then(Value::as_str))
            {
                collect_text_loop_signals(output, false, state);
            }
        }
        _ => {}
    }
}

fn collect_tool_use_loop_signals(name: &str, input: &Value, state: &mut CostState) {
    if let Some(command) = extract_tool_command(name, input) {
        push_command(command, state);
    }
    if let Some(text) = extract_tool_text(input) {
        collect_text_loop_signals(&text, false, state);
    }
}

fn collect_text_loop_signals(text: &str, user_bias: bool, state: &mut CostState) {
    for raw_line in text.lines() {
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
            push_prompt_signal(prompt_candidate, state);
            continue;
        }
        for (kind, detail) in detect_closeout(trimmed) {
            push_closeout_signal(&format!("{kind}: {detail}"), state);
        }
    }
}

fn push_prompt_signal(text: &str, state: &mut CostState) {
    flush_pending_commands(state);
    push_loop_signal(LoopClusterKind::PromptRepeat, text, state);
}

fn push_closeout_signal(text: &str, state: &mut CostState) {
    flush_pending_commands(state);
    push_loop_signal(LoopClusterKind::CloseoutChurn, text, state);
}

fn push_command(command: String, state: &mut CostState) {
    let normalized = normalize_whitespace(&command);
    if normalized.is_empty() {
        return;
    }
    if state
        .pending_commands
        .last()
        .is_some_and(|existing| existing == &normalized)
    {
        return;
    }
    state.pending_commands.push(normalized);
}

fn flush_pending_commands(state: &mut CostState) {
    if state.pending_commands.is_empty() {
        return;
    }
    let label = truncate_detail(
        &state
            .pending_commands
            .iter()
            .take(MAX_COMMANDS_PER_BUNDLE)
            .cloned()
            .collect::<Vec<_>>()
            .join(" -> "),
        220,
    );
    state.pending_commands.clear();
    push_loop_signal(LoopClusterKind::CommandBundle, &label, state);
}

fn push_loop_signal(kind: LoopClusterKind, label: &str, state: &mut CostState) {
    let normalized = truncate_detail(&normalize_whitespace(label), 220);
    if normalized.is_empty() {
        return;
    }
    state.loop_signals.push(LoopSignal {
        kind,
        label: normalized,
    });
}

fn collect_loop_clusters(signals: &[LoopSignal]) -> Vec<SessionCostLoopCluster> {
    let mut summary = BTreeMap::<(LoopClusterKind, String), (usize, usize)>::new();
    let mut previous = None::<(LoopClusterKind, String)>;
    let mut streak = 0_usize;

    for signal in signals {
        let key = (signal.kind, signal.label.clone());
        let entry = summary.entry(key.clone()).or_insert((0, 0));
        entry.0 += 1;
        if previous.as_ref() == Some(&key) {
            streak += 1;
        } else {
            previous = Some(key.clone());
            streak = 1;
        }
        entry.1 = entry.1.max(streak);
    }

    let mut clusters = summary
        .into_iter()
        .filter_map(|((kind, label), (occurrences, max_consecutive))| {
            (occurrences >= 2).then_some(SessionCostLoopCluster {
                kind: kind.as_str().to_string(),
                label,
                occurrences,
                max_consecutive,
            })
        })
        .collect::<Vec<_>>();
    clusters.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then(right.max_consecutive.cmp(&left.max_consecutive))
            .then(left.kind.cmp(&right.kind))
            .then(left.label.cmp(&right.label))
    });
    clusters.truncate(MAX_LOOP_CLUSTERS);
    clusters
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

fn collect_content_block(role: Option<String>, value: &Value, out: &mut Vec<TranscriptBlock>) {
    match value.get("type").and_then(Value::as_str) {
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

fn strip_common_prefixes(text: &str) -> &str {
    text.strip_prefix("❯ ")
        .or_else(|| text.strip_prefix("- "))
        .or_else(|| text.strip_prefix("* "))
        .or_else(|| text.strip_prefix("> "))
        .unwrap_or(text)
        .trim()
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

fn is_closeout_runtime_event(event_name: &str, normalized: &str) -> bool {
    event_name == "document_cycle"
        || matches!(
            normalized,
            "preflight_started"
                | "response_captured"
                | "commit_staging"
                | "commit_success"
                | "commit_already_current"
                | "snapshot_save"
                | "write_origin"
                | "ipc_write_attempt"
                | "ipc_write_consumed"
                | "out_of_band_write"
        )
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
    state: &mut CostState,
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

fn usage_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn count_restart_family(restart_churn: &[RestartChurnSummary], family: &str) -> usize {
    restart_churn
        .iter()
        .find(|entry| entry.family == family)
        .map_or(0, |entry| entry.occurrences)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detects_claude_jsonl_and_dedupes_usage_by_message_id() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","requestId":"req-1","message":{"id":"msg-1","role":"assistant","usage":{"input_tokens":9,"cache_creation_input_tokens":300,"cache_read_input_tokens":1200,"output_tokens":10}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","requestId":"req-1","message":{"id":"msg-1","role":"assistant","usage":{"input_tokens":9,"cache_creation_input_tokens":300,"cache_read_input_tokens":1200,"output_tokens":10}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","requestId":"req-2","message":{"id":"msg-2","role":"assistant","usage":{"input_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":800,"output_tokens":8}}}"#,
            "\n"
        );

        let report = compute(input, None).unwrap();
        assert_eq!(report.source, "claude_jsonl");
        assert_eq!(report.usage_samples, 2);
        assert_eq!(report.prompt_tokens, 2321);
        assert_eq!(report.cached_input_tokens, 2000);
        assert_eq!(report.cache_creation_input_tokens, 300);
        assert_eq!(report.output_tokens, 18);
        assert_eq!(report.total_tokens, 2339);
        assert_eq!(report.cached_input_ratio, Some(86.17));
    }

    #[test]
    fn codex_jsonl_uses_cumulative_deltas_and_skips_duplicate_snapshots() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1400,"output_tokens":90,"reasoning_output_tokens":20,"total_tokens":1690}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1400,"output_tokens":90,"reasoning_output_tokens":20,"total_tokens":1690}}}}"#,
            "\n"
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();
        assert_eq!(report.usage_samples, 2);
        assert_eq!(report.prompt_tokens, 1600);
        assert_eq!(report.cached_input_tokens, 1400);
        assert_eq!(report.output_tokens, 90);
        assert_eq!(report.reasoning_output_tokens, 20);
        assert_eq!(report.total_tokens, 1690);
        assert_eq!(report.largest_turn_total_tokens, 1050);
        assert_eq!(report.largest_turns[0].total_tokens, 1050);
        assert_eq!(report.largest_turns[1].total_tokens, 640);
    }

    #[test]
    fn agent_doc_log_summarizes_runtime_churn() {
        let input = "\
[1776452736] claude_start mode=fresh restart_count=0
[1776528398] claude_start mode=fresh_restart restart_count=1
[1776528446] auto_trigger_timeout (no prompt after 30s)
[1776528450] ctrl_d_restart_fresh restart_count=2
[1776528582] claude_start mode=fresh_restart restart_count=2
[1776528599] codex_start mode=continue restart_count=3
[1776528601] user_quit_after_ctrl_d
[1776528602] commit_already_current file=tasks/software/tsift.md basis=head
[1776528603] commit_already_current file=tasks/software/tsift.md basis=head
[1776528604] commit_already_current file=tasks/software/tsift.md basis=head
";

        let report = compute(input, Some("agent-doc-log")).unwrap();
        assert_eq!(report.source, "agent_doc_log");
        assert_eq!(report.usage_samples, 0);
        assert_eq!(report.runtime_event_groups, 7);
        assert_eq!(report.total_runtime_events, 10);
        assert_eq!(report.restart_churn_groups, 4);
        assert_eq!(report.max_restart_count, Some(3));
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "claude_start:fresh_restart" && event.occurrences == 2)
        );
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "auto_trigger_timeout" && event.occurrences == 1)
        );
        assert!(
            report
                .restart_churn
                .iter()
                .any(|entry| entry.family == "fresh_restart" && entry.occurrences == 3)
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
        assert!(
            report
                .guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "restart_loop")
        );
        assert!(
            report
                .guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "noop_closeout")
        );
        assert!(
            report
                .loop_clusters
                .iter()
                .any(|cluster| cluster.kind == "closeout_churn"
                    && cluster.label == "commit_already_current"
                    && cluster.occurrences == 3)
        );
    }

    #[test]
    fn agent_doc_log_dedupes_document_cycle_runtime_events_by_cycle() {
        let input = "\
[1777603275] document_cycle phase=response_captured cycle=cycle-1 event=response_captured capture_id=cycle-1
[1777603276] document_cycle phase=committed cycle=cycle-1 event=commit_success capture_id=cycle-1
[1777603403] document_cycle phase=committed cycle=cycle-1 event=commit_already_current capture_id=cycle-1
[1777603404] document_cycle phase=committed cycle=cycle-1 event=commit_already_current capture_id=cycle-1
[1777603405] document_cycle phase=committed cycle=cycle-1 event=commit_already_current capture_id=cycle-1
[1777603500] document_cycle phase=preflight_started cycle=cycle-2 event=preflight_started
[1777603600] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
[1777603601] document_cycle phase=committed cycle=cycle-2 event=commit_already_current
[1777603700] document_cycle phase=committed cycle=cycle-3 event=commit_already_current
";

        let report = compute(input, Some("agent-doc-log")).unwrap();

        assert_eq!(report.total_runtime_events, 6);
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "commit_already_current" && event.occurrences == 3)
        );
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "commit_success" && event.occurrences == 1)
        );
        assert!(
            report
                .runtime_events
                .iter()
                .any(|event| event.event == "response_captured" && event.occurrences == 1)
        );
        assert!(
            report
                .guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "noop_closeout")
        );
        assert!(
            report
                .loop_clusters
                .iter()
                .any(|cluster| cluster.kind == "closeout_churn"
                    && cluster.label == "commit_already_current"
                    && cluster.occurrences == 3)
        );
    }

    #[test]
    fn codex_jsonl_surfaces_prompt_and_command_loop_clusters() {
        let input = concat!(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#looprank]. spec-test-build-install-commit-push"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo build --release\"}"}}"#,
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
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();

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
        assert!(report.loop_clusters.iter().any(|cluster| {
            cluster.kind == "closeout_churn"
                && cluster
                    .label
                    .contains("Committed and pushed in `src/tsift`")
                && cluster.occurrences == 2
        }));
    }

    #[test]
    fn derive_guardrails_flags_large_prompt_turns() {
        let guardrails = derive_guardrails(&SessionCostGuardrailInput {
            largest_prompt_turn_tokens: 140_000,
            largest_prompt_turn_label: Some("2026-05-05T00:00:01Z".to_string()),
            ..SessionCostGuardrailInput::default()
        });

        assert!(
            guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "prompt_budget")
        );
    }

    #[test]
    fn derive_guardrails_flags_cached_resend_ratio() {
        let guardrails = derive_guardrails(&SessionCostGuardrailInput {
            prompt_tokens: 80_000,
            cached_input_ratio: Some(96.0),
            ..SessionCostGuardrailInput::default()
        });

        assert!(
            guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "cache_resend")
        );
    }
}
