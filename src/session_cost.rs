use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::runtime_churn::{RestartChurnState, RestartChurnSummary};

const MAX_LARGEST_TURNS: usize = 5;
const MAX_RUNTIME_EVENTS: usize = 8;

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
    pub restart_churn: Vec<RestartChurnSummary>,
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
    total_runtime_events: usize,
    max_restart_count: Option<usize>,
    restart_churn: RestartChurnState,
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
        restart_churn,
        warnings: state.warnings,
    })
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
        let value = serde_json::from_str::<Value>(trimmed)
            .with_context(|| format!("parsing Claude transcript jsonl line {}", index + 1))?;
        let Some(message) = value.get("message") else {
            continue;
        };
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
        let value = serde_json::from_str::<Value>(trimmed)
            .with_context(|| format!("parsing Codex transcript jsonl line {}", index + 1))?;
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
        let mut normalized = event_name.to_string();
        if matches!(
            event_name,
            "claude_start" | "codex_start" | "claude_restart" | "codex_restart"
        ) && let Some(mode) = extract_field(detail, "mode")
        {
            normalized = format!("{event_name}:{mode}");
        }
        *state.runtime_events.entry(normalized).or_default() += 1;
        state.total_runtime_events += 1;
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

fn usage_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
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
";

        let report = compute(input, Some("agent-doc-log")).unwrap();
        assert_eq!(report.source, "agent_doc_log");
        assert_eq!(report.usage_samples, 0);
        assert_eq!(report.runtime_event_groups, 6);
        assert_eq!(report.total_runtime_events, 7);
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
    }
}
