use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use tsift_quality::runtime_churn::{RestartChurnState, RestartChurnSummary};

const MAX_LARGEST_TURNS: usize = 5;
const MAX_RUNTIME_EVENTS: usize = 8;
const MAX_GUARDRAILS: usize = 8;
const MAX_LOOP_CLUSTERS: usize = 8;
const MAX_FILE_READ_DIAGNOSTICS: usize = 8;
const MAX_PROMPT_CACHE_TIMELINE: usize = 8;
const MAX_PROMPT_CACHE_DIAGNOSTICS: usize = 6;
const MAX_COMMANDS_PER_BUNDLE: usize = 6;
const PROMPT_BUDGET_WARN_TOKENS: u64 = 100_000;
const CACHED_RATIO_WARN_PERCENT: f64 = 90.0;
const CACHED_RATIO_WARN_PROMPT_TOKENS: u64 = 50_000;
const PROMPT_CACHE_CANDIDATE_TOKENS: u64 = 16_000;
const PROMPT_CACHE_GOOD_HIT_PERCENT: f64 = 75.0;
const PROMPT_CACHE_TREND_DELTA_PERCENT: f64 = 5.0;
const PROMPT_CACHE_RATIO_DROP_WARN_PERCENT: f64 = 20.0;
const PROMPT_CACHE_CREATION_SPIKE_WARN_PERCENT: f64 = 20.0;
const PROMPT_CACHE_READ_CREATE_REGRESSION_RATIO: f64 = 2.0;
const RESTART_LOOP_WARN_OCCURRENCES: usize = 3;
const NOOP_CLOSEOUT_WARN_OCCURRENCES: usize = 3;
const DEFAULT_FULL_FILE_READ_TOKENS: u64 = 4_000;
const ESTIMATED_TOKENS_PER_SOURCE_LINE: u64 = 18;

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
pub struct SessionCostPromptCachePlan {
    pub status: String,
    pub feasible: bool,
    pub observed_cached_input_tokens: u64,
    pub observed_cache_creation_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_cached_input_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics: Option<SessionCostPromptCacheAnalytics>,
    pub invariants: Vec<String>,
    pub provider_adapters: Vec<SessionCostPromptCacheProvider>,
    pub actions: Vec<SessionCostPromptCacheAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheProvider {
    pub provider: String,
    pub status: String,
    pub requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheAction {
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub guidance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheAnalytics {
    pub sample_count: usize,
    pub effective: bool,
    pub trend: String,
    pub total_prompt_tokens: u64,
    pub total_cached_input_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub net_cached_input_tokens: i64,
    pub timeline_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_cached_input_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_cached_input_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cached_input_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio_delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_to_creation_ratio: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<SessionCostPromptCacheDiagnostic>,
    pub timeline: Vec<SessionCostPromptCacheTimelineEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheDiagnostic {
    pub kind: String,
    pub severity: String,
    pub label: String,
    pub message: String,
    pub likely_causes: Vec<String>,
    pub guidance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheTimelineEntry {
    pub label: String,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_ratio: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostLoopCluster {
    pub kind: String,
    pub label: String,
    pub occurrences: usize,
    pub max_consecutive: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostFileReadDiagnostic {
    pub path: String,
    pub range: String,
    pub occurrences: usize,
    pub estimated_tokens: u64,
    pub duplicate_estimated_tokens: u64,
    pub follow_up_commands: Vec<String>,
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
    pub file_read_diagnostics: Vec<SessionCostFileReadDiagnostic>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub restart_churn: Vec<RestartChurnSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub guardrails: Vec<SessionCostGuardrail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_plan: Option<SessionCostPromptCachePlan>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCostPromptCacheEffectivenessFixture {
    pub schema_version: u64,
    #[serde(default)]
    pub description: String,
    pub cases: Vec<SessionCostPromptCacheEffectivenessCase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCostPromptCacheEffectivenessCase {
    pub name: String,
    pub source: String,
    pub input_lines: Vec<String>,
    pub minimum_cached_input_ratio: f64,
    pub minimum_net_cached_input_tokens: i64,
    pub maximum_read_create_regressions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionCostPromptCacheEffectivenessReport {
    pub schema_version: u64,
    pub pass: bool,
    pub totals: SessionCostPromptCacheEffectivenessTotals,
    pub cases: Vec<SessionCostPromptCacheEffectivenessCaseReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheEffectivenessTotals {
    pub cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub net_cached_input_tokens: i64,
    pub read_create_regressions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionCostPromptCacheEffectivenessCaseReport {
    pub name: String,
    pub source: String,
    pub status: String,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio: Option<f64>,
    pub minimum_cached_input_ratio: f64,
    pub net_cached_input_tokens: i64,
    pub minimum_net_cached_input_tokens: i64,
    pub read_create_regressions: usize,
    pub maximum_read_create_regressions: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
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
    file_read_signals: Vec<FileReadSignal>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LoopSignal {
    kind: LoopClusterKind,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileReadSignal {
    path: String,
    range: String,
    start: Option<usize>,
    lines: Option<usize>,
    estimated_tokens: u64,
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
    let file_read_diagnostics = collect_file_read_diagnostics(&state.file_read_signals);
    let prompt_cache_plan = derive_prompt_cache_plan(
        prompt_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        cached_input_ratio,
        &state.usage_turns,
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
        file_read_diagnostics,
        restart_churn,
        guardrails,
        prompt_cache_plan,
        warnings: state.warnings,
    })
}

pub fn build_prompt_cache_effectiveness_report(
    fixture: &SessionCostPromptCacheEffectivenessFixture,
) -> Result<SessionCostPromptCacheEffectivenessReport> {
    if fixture.cases.is_empty() {
        bail!("prompt-cache effectiveness fixture has no cases");
    }

    let mut cases = Vec::new();
    let mut totals = SessionCostPromptCacheEffectivenessTotals {
        cases: 0,
        passed: 0,
        failed: 0,
        prompt_tokens: 0,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        net_cached_input_tokens: 0,
        read_create_regressions: 0,
    };

    for case in &fixture.cases {
        if case.input_lines.is_empty() {
            bail!(
                "prompt-cache fixture case `{}` has no input_lines",
                case.name
            );
        }
        let input = format!("{}\n", case.input_lines.join("\n"));
        let report = compute(&input, Some(&case.source))
            .map_err(|err| err.context(format!("evaluating prompt-cache fixture {}", case.name)))?;
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref());
        let net_cached_input_tokens = analytics.map_or(
            signed_token_delta(
                report.cached_input_tokens,
                report.cache_creation_input_tokens,
            ),
            |analytics| analytics.net_cached_input_tokens,
        );
        let read_create_regressions = analytics.map_or(0, |analytics| {
            analytics
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.kind == "read_create_regression")
                .count()
        });

        let mut failures = Vec::new();
        if report.prompt_cache_plan.is_none() {
            failures.push("missing prompt_cache_plan".to_string());
        }
        if analytics.is_none() {
            failures.push("missing prompt_cache_plan.analytics".to_string());
        }
        match report.cached_input_ratio {
            Some(ratio) if ratio >= case.minimum_cached_input_ratio => {}
            Some(ratio) => failures.push(format!(
                "cached_input_ratio {:.2}% below required {:.2}%",
                ratio, case.minimum_cached_input_ratio
            )),
            None => failures.push(format!(
                "cached_input_ratio missing; required {:.2}%",
                case.minimum_cached_input_ratio
            )),
        }
        if net_cached_input_tokens < case.minimum_net_cached_input_tokens {
            failures.push(format!(
                "net_cached_input_tokens {} below required {}",
                net_cached_input_tokens, case.minimum_net_cached_input_tokens
            ));
        }
        if read_create_regressions > case.maximum_read_create_regressions {
            failures.push(format!(
                "read_create_regressions {} exceeded allowed {}",
                read_create_regressions, case.maximum_read_create_regressions
            ));
        }

        let status = if failures.is_empty() {
            "pass".to_string()
        } else {
            "fail".to_string()
        };
        totals.cases += 1;
        if status == "pass" {
            totals.passed += 1;
        } else {
            totals.failed += 1;
        }
        totals.prompt_tokens += report.prompt_tokens;
        totals.cached_input_tokens += report.cached_input_tokens;
        totals.cache_creation_input_tokens += report.cache_creation_input_tokens;
        totals.net_cached_input_tokens += net_cached_input_tokens;
        totals.read_create_regressions += read_create_regressions;

        cases.push(SessionCostPromptCacheEffectivenessCaseReport {
            name: case.name.clone(),
            source: report.source,
            status,
            prompt_tokens: report.prompt_tokens,
            cached_input_tokens: report.cached_input_tokens,
            cache_creation_input_tokens: report.cache_creation_input_tokens,
            cached_input_ratio: report.cached_input_ratio,
            minimum_cached_input_ratio: case.minimum_cached_input_ratio,
            net_cached_input_tokens,
            minimum_net_cached_input_tokens: case.minimum_net_cached_input_tokens,
            read_create_regressions,
            maximum_read_create_regressions: case.maximum_read_create_regressions,
            failures,
        });
    }

    Ok(SessionCostPromptCacheEffectivenessReport {
        schema_version: fixture.schema_version,
        pass: totals.failed == 0,
        totals,
        cases,
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

fn derive_prompt_cache_plan(
    prompt_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cached_input_ratio: Option<f64>,
    usage_turns: &[SessionCostTurn],
) -> Option<SessionCostPromptCachePlan> {
    let usage_samples = usage_turns.len();
    if usage_samples == 0 {
        return None;
    }

    let observed = cached_input_tokens > 0 || cache_creation_input_tokens > 0;
    let candidate = prompt_tokens >= PROMPT_CACHE_CANDIDATE_TOKENS;
    if !observed && !candidate {
        return None;
    }

    let mut actions = Vec::new();
    if !observed {
        actions.push(SessionCostPromptCacheAction {
            kind: "enable_provider_cache".to_string(),
            severity: "recommend".to_string(),
            message: format!(
                "prompt volume reached {prompt_tokens} tokens without observed cache reads"
            ),
            guidance: "add a provider adapter that keeps stable context byte-identical and passes the provider cache hint on each turn"
                .to_string(),
        });
    } else if cached_input_ratio.is_some_and(|ratio| ratio < PROMPT_CACHE_GOOD_HIT_PERCENT) {
        actions.push(SessionCostPromptCacheAction {
            kind: "improve_cache_hit_rate".to_string(),
            severity: "recommend".to_string(),
            message: format!(
                "cached input ratio was {:.2}% across {prompt_tokens} prompt tokens",
                cached_input_ratio.unwrap_or_default()
            ),
            guidance:
                "move volatile timestamps, generated headers, and one-off compaction prompts after the cached prefix"
                    .to_string(),
        });
    } else {
        actions.push(SessionCostPromptCacheAction {
            kind: "preserve_cache_shape".to_string(),
            severity: "info".to_string(),
            message: format!(
                "cache reads were observed across {cached_input_tokens} input tokens"
            ),
            guidance:
                "keep the stable prefix and append-only transcript shape intact while adding new tools or context"
                    .to_string(),
        });
    }

    if cache_creation_input_tokens > cached_input_tokens && cached_input_tokens > 0 {
        actions.push(SessionCostPromptCacheAction {
            kind: "reduce_cache_rewrites".to_string(),
            severity: "recommend".to_string(),
            message: format!(
                "cache creation tokens ({cache_creation_input_tokens}) exceeded cache read tokens ({cached_input_tokens})"
            ),
            guidance:
                "check for prefix churn before each model call; repeated writes can erase the economics of prompt caching"
                    .to_string(),
        });
    }

    Some(SessionCostPromptCachePlan {
        status: if observed { "observed" } else { "candidate" }.to_string(),
        feasible: true,
        observed_cached_input_tokens: cached_input_tokens,
        observed_cache_creation_tokens: cache_creation_input_tokens,
        observed_cached_input_ratio: cached_input_ratio.map(|ratio| format!("{ratio:.2}%")),
        analytics: derive_prompt_cache_analytics(
            usage_turns,
            prompt_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            cached_input_ratio,
        ),
        invariants: vec![
            "place stable system/developer context before per-turn content".to_string(),
            "treat conversation history as append-only until an intentional compaction boundary"
                .to_string(),
            "exclude volatile timestamps, random ids, and transient instructions from the cached prefix"
                .to_string(),
            "run compaction against the same live prefix whenever the provider cache is still warm"
                .to_string(),
        ],
        provider_adapters: vec![
            SessionCostPromptCacheProvider {
                provider: "anthropic".to_string(),
                status: "explicit_breakpoints".to_string(),
                requirements: vec![
                    "attach cache_control to the stable system block".to_string(),
                    "attach cache_control to the final tool definition when tools are sent"
                        .to_string(),
                    "attach cache_control to the last two user-role messages; skip one-off compaction instructions"
                        .to_string(),
                ],
            },
            SessionCostPromptCacheProvider {
                provider: "openai".to_string(),
                status: "cache_key".to_string(),
                requirements: vec![
                    "derive prompt_cache_key from the stable thread/session id".to_string(),
                    "keep prefixes byte-identical across consecutive calls for the same key".to_string(),
                ],
            },
            SessionCostPromptCacheProvider {
                provider: "self_hosted_or_edge".to_string(),
                status: "affinity_required".to_string(),
                requirements: vec![
                    "route consecutive calls for the same cache key to the same replica when the provider cache is replica-local"
                        .to_string(),
                ],
            },
        ],
        actions,
    })
}

fn derive_prompt_cache_analytics(
    usage_turns: &[SessionCostTurn],
    prompt_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cached_input_ratio: Option<f64>,
) -> Option<SessionCostPromptCacheAnalytics> {
    if usage_turns.is_empty() {
        return None;
    }

    let first_ratio = usage_turns
        .first()
        .and_then(|turn| percent_ratio(turn.cached_input_tokens, turn.prompt_tokens));
    let last_ratio = usage_turns
        .last()
        .and_then(|turn| percent_ratio(turn.cached_input_tokens, turn.prompt_tokens));
    let ratio_delta = first_ratio
        .zip(last_ratio)
        .map(|(first, last)| last - first);
    let trend = prompt_cache_trend(usage_turns.len(), ratio_delta).to_string();
    let effective = cached_input_ratio.is_some_and(|ratio| ratio >= PROMPT_CACHE_GOOD_HIT_PERCENT)
        && cached_input_tokens >= cache_creation_input_tokens;
    let cache_read_to_creation_ratio = (cache_creation_input_tokens > 0).then(|| {
        format!(
            "{:.2}x",
            (cached_input_tokens as f64) / (cache_creation_input_tokens as f64)
        )
    });
    let timeline = prompt_cache_timeline(usage_turns);
    let diagnostics = derive_prompt_cache_diagnostics(
        usage_turns,
        cached_input_tokens,
        cache_creation_input_tokens,
    );

    Some(SessionCostPromptCacheAnalytics {
        sample_count: usage_turns.len(),
        effective,
        trend,
        total_prompt_tokens: prompt_tokens,
        total_cached_input_tokens: cached_input_tokens,
        total_cache_creation_tokens: cache_creation_input_tokens,
        net_cached_input_tokens: signed_token_delta(
            cached_input_tokens,
            cache_creation_input_tokens,
        ),
        timeline_truncated: usage_turns.len() > MAX_PROMPT_CACHE_TIMELINE,
        average_cached_input_ratio: cached_input_ratio.map(format_percent),
        first_cached_input_ratio: first_ratio.map(format_percent),
        last_cached_input_ratio: last_ratio.map(format_percent),
        cached_input_ratio_delta: ratio_delta.map(format_signed_percent),
        cache_read_to_creation_ratio,
        diagnostics,
        timeline,
    })
}

fn derive_prompt_cache_diagnostics(
    usage_turns: &[SessionCostTurn],
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> Vec<SessionCostPromptCacheDiagnostic> {
    let mut diagnostics = Vec::new();

    for pair in usage_turns.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        let Some(previous_ratio) =
            percent_ratio(previous.cached_input_tokens, previous.prompt_tokens)
        else {
            continue;
        };
        let Some(current_ratio) = percent_ratio(current.cached_input_tokens, current.prompt_tokens)
        else {
            continue;
        };
        let drop = previous_ratio - current_ratio;
        if drop >= PROMPT_CACHE_RATIO_DROP_WARN_PERCENT {
            diagnostics.push(SessionCostPromptCacheDiagnostic {
                kind: "cached_ratio_drop".to_string(),
                severity: "warn".to_string(),
                label: current.label.clone(),
                message: format!(
                    "cached input ratio dropped from {} to {} at {}",
                    format_percent(previous_ratio),
                    format_percent(current_ratio),
                    current.label
                ),
                likely_causes: vec![
                    "stable prefix bytes changed before the cache boundary".to_string(),
                    "prompt_cache_key or thread/session id changed".to_string(),
                    "replica-local cache affinity was lost".to_string(),
                ],
                guidance:
                    "compare the prefix, tool set, cache key, compaction boundary, and routing between the previous turn and this turn"
                        .to_string(),
            });
        }
    }

    for turn in usage_turns {
        let Some(creation_ratio) =
            percent_ratio(turn.cache_creation_input_tokens, turn.prompt_tokens)
        else {
            continue;
        };
        if turn.cache_creation_input_tokens > 0
            && creation_ratio >= PROMPT_CACHE_CREATION_SPIKE_WARN_PERCENT
        {
            diagnostics.push(SessionCostPromptCacheDiagnostic {
                kind: "cache_creation_spike".to_string(),
                severity: "warn".to_string(),
                label: turn.label.clone(),
                message: format!(
                    "cache creation was {} of prompt tokens at {}",
                    format_percent(creation_ratio),
                    turn.label
                ),
                likely_causes: vec![
                    "provider created a fresh cached prefix instead of reusing the warm prefix"
                        .to_string(),
                    "system, developer, or tool block changed before the cache boundary"
                        .to_string(),
                    "compaction or transient instructions entered the cached prefix".to_string(),
                ],
                guidance:
                    "inspect the cached prefix and provider breakpoint placement for this turn before treating the cache as effective"
                        .to_string(),
            });
        }
    }

    if cache_creation_input_tokens > 0 {
        let read_to_creation = (cached_input_tokens as f64) / (cache_creation_input_tokens as f64);
        if read_to_creation < PROMPT_CACHE_READ_CREATE_REGRESSION_RATIO {
            diagnostics.push(SessionCostPromptCacheDiagnostic {
                kind: "read_create_regression".to_string(),
                severity: "recommend".to_string(),
                label: "session".to_string(),
                message: format!(
                    "cache read/create ratio was {read_to_creation:.2}x ({cached_input_tokens} read tokens, {cache_creation_input_tokens} creation tokens)"
                ),
                likely_causes: vec![
                    "cached prefix is being rewritten too often for warm reuse".to_string(),
                    "volatile values are inside the cached prefix".to_string(),
                    "cache key or replica routing is changing between turns".to_string(),
                ],
                guidance:
                    "stabilize the prefix/key/routing path until cache reads clearly exceed creation work"
                        .to_string(),
            });
        }
    }

    diagnostics.truncate(MAX_PROMPT_CACHE_DIAGNOSTICS);
    diagnostics
}

fn prompt_cache_timeline(
    usage_turns: &[SessionCostTurn],
) -> Vec<SessionCostPromptCacheTimelineEntry> {
    let selected = if usage_turns.len() <= MAX_PROMPT_CACHE_TIMELINE {
        usage_turns.iter().collect::<Vec<_>>()
    } else {
        let tail_count = MAX_PROMPT_CACHE_TIMELINE.saturating_sub(1);
        let mut selected = Vec::with_capacity(MAX_PROMPT_CACHE_TIMELINE);
        if let Some(first) = usage_turns.first() {
            selected.push(first);
        }
        selected.extend(usage_turns.iter().skip(usage_turns.len() - tail_count));
        selected
    };

    selected
        .into_iter()
        .map(|turn| SessionCostPromptCacheTimelineEntry {
            label: turn.label.clone(),
            prompt_tokens: turn.prompt_tokens,
            cached_input_tokens: turn.cached_input_tokens,
            cache_creation_input_tokens: turn.cache_creation_input_tokens,
            cached_input_ratio: percent_ratio(turn.cached_input_tokens, turn.prompt_tokens)
                .map(format_percent),
            cache_creation_ratio: percent_ratio(
                turn.cache_creation_input_tokens,
                turn.prompt_tokens,
            )
            .map(format_percent),
        })
        .collect()
}

fn prompt_cache_trend(sample_count: usize, ratio_delta: Option<f64>) -> &'static str {
    if sample_count < 2 {
        return "single_sample";
    }
    let Some(delta) = ratio_delta else {
        return "insufficient_data";
    };
    if delta >= PROMPT_CACHE_TREND_DELTA_PERCENT {
        "improving"
    } else if delta <= -PROMPT_CACHE_TREND_DELTA_PERCENT {
        "declining"
    } else {
        "stable"
    }
}

fn percent_ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0)
        .then_some(((numerator as f64) / (denominator as f64) * 10_000.0).round() / 100.0)
}

fn format_percent(value: f64) -> String {
    format!("{value:.2}%")
}

fn format_signed_percent(value: f64) -> String {
    format!("{value:+.2}%")
}

fn signed_token_delta(read_tokens: u64, creation_tokens: u64) -> i64 {
    if read_tokens >= creation_tokens {
        i64::try_from(read_tokens - creation_tokens).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(creation_tokens - read_tokens).unwrap_or(i64::MAX)
    }
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
    let mut seen_cumulative_snapshots = BTreeSet::<UsageTotals>::new();
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
        let cumulative = codex_usage_totals(total);
        let duplicate_snapshot = !seen_cumulative_snapshots.insert(cumulative);
        let delta = if duplicate_snapshot {
            UsageTotals::default()
        } else if let Some(last) = payload
            .get("info")
            .and_then(|info| info.get("last_token_usage"))
            .map(codex_usage_totals)
            .filter(|last| !last.is_zero())
        {
            last
        } else if previous.is_zero() {
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
            if let Some(command) = extract_raw_codex_exec_command(payload) {
                collect_file_read_command_signals(&command, state);
            }
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
    collect_file_read_tool_signals(name, input, state);
    if let Some(command) = extract_raw_tool_command(name, input) {
        collect_file_read_command_signals(&command, state);
    }
    if let Some(command) = extract_tool_command(name, input) {
        push_command(command, state);
    }
    if let Some(text) = extract_tool_text(input) {
        collect_text_loop_signals(&text, false, state);
    }
}

fn collect_file_read_tool_signals(name: &str, input: &Value, state: &mut CostState) {
    let lower = name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "read" | "file_read" | "read_file") {
        return;
    }
    let Value::Object(map) = input else {
        return;
    };
    let Some(path) = ["file_path", "path"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .map(normalize_file_read_path)
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let start = ["offset", "start", "line"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0);
    let lines = ["limit", "lines", "line_count"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0);
    push_file_read_signal(path, start, lines, state);
}

fn collect_file_read_command_signals(command: &str, state: &mut CostState) {
    if let Some(signal) = parse_file_read_command(command) {
        state.file_read_signals.push(signal);
    }
}

fn parse_file_read_command(command: &str) -> Option<FileReadSignal> {
    let tokens = shell_words(command);
    let head = tokens.first()?.as_str();
    match head {
        "cat" | "bat" | "batcat" | "nl" => {
            let path = first_non_option_arg(&tokens[1..])?;
            Some(file_read_signal(
                normalize_file_read_path(path),
                "full".to_string(),
                None,
                None,
            ))
        }
        "sed" => parse_sed_file_read(&tokens),
        "head" => parse_head_file_read(&tokens),
        "tail" => parse_tail_file_read(&tokens),
        _ => None,
    }
}

fn parse_sed_file_read(tokens: &[String]) -> Option<FileReadSignal> {
    let mut expr = None::<String>;
    let mut path = None::<String>;
    let mut skip_next = false;
    for token in tokens.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "-n" {
            continue;
        }
        if token == "-e" {
            skip_next = true;
            continue;
        }
        if expr.is_none() && parse_sed_range(token).is_some() {
            expr = Some(token.clone());
            continue;
        }
        if !token.starts_with('-') {
            path = Some(token.clone());
        }
    }
    let expr = expr?;
    let path = path?;
    let (start, lines) = parse_sed_range(&expr)?;
    Some(file_read_signal(
        normalize_file_read_path(&path),
        format!("{}-{}", start, start + lines - 1),
        Some(start),
        Some(lines),
    ))
}

fn parse_sed_range(expr: &str) -> Option<(usize, usize)> {
    let trimmed = expr.trim_matches(['\'', '"']).trim();
    let body = trimmed.strip_suffix('p')?;
    let (start_raw, end_raw) = body.split_once(',')?;
    let start = start_raw.trim().parse::<usize>().ok()?;
    let lines = if let Some(relative) = end_raw.trim().strip_prefix('+') {
        relative.trim().parse::<usize>().ok()?.saturating_add(1)
    } else {
        let end = end_raw.trim().parse::<usize>().ok()?;
        end.checked_sub(start)?.saturating_add(1)
    };
    (lines > 0).then_some((start, lines))
}

fn parse_head_file_read(tokens: &[String]) -> Option<FileReadSignal> {
    let mut lines = 10_usize;
    let mut path = None::<String>;
    let mut index = 1_usize;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "-n" || token == "--lines" {
            index += 1;
            lines = tokens.get(index)?.parse::<usize>().ok()?;
        } else if let Some(value) = token.strip_prefix("-n") {
            lines = value.parse::<usize>().ok()?;
        } else if token.starts_with('-') && token[1..].chars().all(|ch| ch.is_ascii_digit()) {
            lines = token[1..].parse::<usize>().ok()?;
        } else if !token.starts_with('-') {
            path = Some(token.clone());
        }
        index += 1;
    }
    let path = path?;
    Some(file_read_signal(
        normalize_file_read_path(&path),
        format!("head:{lines}"),
        Some(1),
        Some(lines),
    ))
}

fn parse_tail_file_read(tokens: &[String]) -> Option<FileReadSignal> {
    let mut lines = 10_usize;
    let mut path = None::<String>;
    let mut index = 1_usize;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "-n" || token == "--lines" {
            index += 1;
            lines = tokens.get(index)?.parse::<usize>().ok()?;
        } else if let Some(value) = token.strip_prefix("-n") {
            lines = value.trim_start_matches('+').parse::<usize>().ok()?;
        } else if token.starts_with('-') && token[1..].chars().all(|ch| ch.is_ascii_digit()) {
            lines = token[1..].parse::<usize>().ok()?;
        } else if !token.starts_with('-') {
            path = Some(token.clone());
        }
        index += 1;
    }
    let path = path?;
    Some(file_read_signal(
        normalize_file_read_path(&path),
        format!("tail:{lines}"),
        None,
        Some(lines),
    ))
}

fn first_non_option_arg(tokens: &[String]) -> Option<&str> {
    tokens
        .iter()
        .find(|token| !token.starts_with('-'))
        .map(String::as_str)
}

fn push_file_read_signal(
    path: String,
    start: Option<usize>,
    lines: Option<usize>,
    state: &mut CostState,
) {
    let range = match (start, lines) {
        (Some(start), Some(lines)) => format!("{}-{}", start, start + lines - 1),
        (Some(start), None) => format!("{start}-end"),
        (None, Some(lines)) => format!("window:{lines}"),
        (None, None) => "full".to_string(),
    };
    state
        .file_read_signals
        .push(file_read_signal(path, range, start, lines));
}

fn file_read_signal(
    path: String,
    range: String,
    start: Option<usize>,
    lines: Option<usize>,
) -> FileReadSignal {
    FileReadSignal {
        path,
        range,
        start,
        lines,
        estimated_tokens: estimate_file_read_tokens(lines),
    }
}

fn estimate_file_read_tokens(lines: Option<usize>) -> u64 {
    lines
        .map(|lines| (lines as u64).saturating_mul(ESTIMATED_TOKENS_PER_SOURCE_LINE))
        .unwrap_or(DEFAULT_FULL_FILE_READ_TOKENS)
        .max(80)
}

fn collect_file_read_diagnostics(signals: &[FileReadSignal]) -> Vec<SessionCostFileReadDiagnostic> {
    let mut grouped = BTreeMap::<(String, String), FileReadDiagnosticBuilder>::new();
    for signal in signals {
        let entry = grouped
            .entry((signal.path.clone(), signal.range.clone()))
            .or_insert_with(|| FileReadDiagnosticBuilder {
                path: signal.path.clone(),
                range: signal.range.clone(),
                start: signal.start,
                lines: signal.lines,
                occurrences: 0,
                estimated_tokens: 0,
                max_single_read_tokens: 0,
            });
        entry.occurrences += 1;
        entry.estimated_tokens = entry
            .estimated_tokens
            .saturating_add(signal.estimated_tokens);
        entry.max_single_read_tokens = entry.max_single_read_tokens.max(signal.estimated_tokens);
        entry.start = entry.start.or(signal.start);
        entry.lines = entry.lines.or(signal.lines);
    }

    let mut diagnostics = grouped
        .into_values()
        .filter(|entry| entry.occurrences >= 2)
        .map(|entry| {
            let duplicate_estimated_tokens = entry
                .estimated_tokens
                .saturating_sub(entry.max_single_read_tokens);
            SessionCostFileReadDiagnostic {
                path: entry.path.clone(),
                range: entry.range.clone(),
                occurrences: entry.occurrences,
                estimated_tokens: entry.estimated_tokens,
                duplicate_estimated_tokens,
                follow_up_commands: file_read_follow_up_commands(
                    &entry.path,
                    entry.start,
                    entry.lines,
                ),
            }
        })
        .collect::<Vec<_>>();
    diagnostics.sort_by(|left, right| {
        right
            .duplicate_estimated_tokens
            .cmp(&left.duplicate_estimated_tokens)
            .then(right.occurrences.cmp(&left.occurrences))
            .then(left.path.cmp(&right.path))
            .then(left.range.cmp(&right.range))
    });
    diagnostics.truncate(MAX_FILE_READ_DIAGNOSTICS);
    diagnostics
}

#[derive(Debug)]
struct FileReadDiagnosticBuilder {
    path: String,
    range: String,
    start: Option<usize>,
    lines: Option<usize>,
    occurrences: usize,
    estimated_tokens: u64,
    max_single_read_tokens: u64,
}

fn file_read_follow_up_commands(
    path: &str,
    start: Option<usize>,
    lines: Option<usize>,
) -> Vec<String> {
    let start = start.unwrap_or(1);
    let lines = lines.unwrap_or(120).max(1);
    vec![
        format!(
            "tsift source-read {} --start {} --lines {} --budget normal",
            shell_quote(path),
            start,
            lines
        ),
        format!("tsift summarize --file {}", shell_quote(path)),
    ]
}

fn normalize_file_read_path(raw: &str) -> String {
    raw.trim()
        .trim_matches(['\'', '"'])
        .trim_start_matches("./")
        .to_string()
}

fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None::<char>;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote_ch) = quote {
            if ch == quote_ch {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
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
    let normalized = extract_raw_tool_command(name, input)?;
    looks_like_command(&normalized).then_some(normalized)
}

fn extract_raw_tool_command(name: &str, input: &Value) -> Option<String> {
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
                    if !normalized.is_empty() {
                        return Some(normalized);
                    }
                }
            }
            None
        }
        Value::String(raw) => {
            let normalized = normalize_whitespace(raw);
            (!normalized.is_empty()).then_some(normalized)
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
    let normalized = extract_raw_codex_exec_command(payload)?;
    looks_like_command(&normalized).then_some(normalized)
}

fn extract_raw_codex_exec_command(payload: &Value) -> Option<String> {
    if let Some(parsed) = payload.get("parsed_cmd").and_then(Value::as_array) {
        for item in parsed {
            if let Some(command) = item.get("cmd").and_then(Value::as_str) {
                let normalized = normalize_whitespace(command);
                if !normalized.is_empty() {
                    return Some(normalized);
                }
            }
        }
    }

    if let Some(command) = payload.get("command").and_then(Value::as_array)
        && let Some(last) = command.last().and_then(Value::as_str)
    {
        let normalized = normalize_whitespace(last);
        if !normalized.is_empty() {
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

fn codex_usage_totals(value: &Value) -> UsageTotals {
    UsageTotals {
        prompt_tokens: usage_u64(value, "input_tokens"),
        cached_input_tokens: usage_u64(value, "cached_input_tokens"),
        cache_creation_input_tokens: 0,
        output_tokens: usage_u64(value, "output_tokens"),
        reasoning_output_tokens: usage_u64(value, "reasoning_output_tokens"),
        total_tokens: usage_u64(value, "total_tokens"),
    }
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
    fn codex_jsonl_prefers_last_usage_for_interleaved_cumulative_streams() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520},"last_token_usage":{"input_tokens":500,"cached_input_tokens":450,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":520}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":1400,"output_tokens":90,"reasoning_output_tokens":20,"total_tokens":1690},"last_token_usage":{"input_tokens":600,"cached_input_tokens":500,"output_tokens":40,"reasoning_output_tokens":10,"total_tokens":640}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":800,"output_tokens":45,"reasoning_output_tokens":10,"total_tokens":945},"last_token_usage":{"input_tokens":400,"cached_input_tokens":350,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":425}}}}"#,
            "\n"
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();
        assert_eq!(report.usage_samples, 4);
        assert_eq!(report.prompt_tokens, 2500);
        assert_eq!(report.cached_input_tokens, 2200);
        assert_eq!(report.output_tokens, 135);
        assert_eq!(report.reasoning_output_tokens, 30);
        assert_eq!(report.total_tokens, 2635);
        assert_eq!(report.largest_turn_total_tokens, 1050);
    }

    #[test]
    fn prompt_cache_plan_summarizes_effectiveness_over_time() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":1050}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":600,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":500,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":1050}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":1500,"output_tokens":150,"reasoning_output_tokens":0,"total_tokens":3150},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":1050}}}}"#,
            "\n",
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present");

        assert_eq!(analytics.sample_count, 3);
        assert!(!analytics.effective);
        assert_eq!(analytics.trend, "improving");
        assert_eq!(
            analytics.average_cached_input_ratio.as_deref(),
            Some("50.00%")
        );
        assert_eq!(
            analytics.first_cached_input_ratio.as_deref(),
            Some("10.00%")
        );
        assert_eq!(analytics.last_cached_input_ratio.as_deref(), Some("90.00%"));
        assert_eq!(
            analytics.cached_input_ratio_delta.as_deref(),
            Some("+80.00%")
        );
        assert_eq!(analytics.net_cached_input_tokens, 1500);
        assert_eq!(analytics.timeline.len(), 3);
        assert_eq!(
            analytics.timeline[2].cached_input_ratio.as_deref(),
            Some("90.00%")
        );
    }

    #[test]
    fn prompt_cache_plan_classifies_likely_invalidation_diagnostics() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","message":{"id":"msg-1","role":"assistant","usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","message":{"id":"msg-2","role":"assistant","usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:03Z","message":{"id":"msg-3","role":"assistant","usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();
        let diagnostics = &report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present")
            .diagnostics;

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "cached_ratio_drop"
                && diagnostic.label == "2026-05-05T00:00:02Z"
                && diagnostic
                    .likely_causes
                    .iter()
                    .any(|cause| cause.contains("prompt_cache_key"))
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "cache_creation_spike" && diagnostic.message.contains("60.00%")
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "read_create_regression" && diagnostic.message.contains("0.92x")
        }));
    }

    #[test]
    fn prompt_cache_effectiveness_fixture_passes_thresholds() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            cases: vec![SessionCostPromptCacheEffectivenessCase {
                name: "warm-codex-prefix".to_string(),
                source: "codex-jsonl".to_string(),
                input_lines: vec![
                    r#"{"timestamp":"2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#.to_string(),
                    r#"{"timestamp":"2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#.to_string(),
                ],
                minimum_cached_input_ratio: 90.0,
                minimum_net_cached_input_tokens: 40_000,
                maximum_read_create_regressions: 0,
            }],
        };

        let report = build_prompt_cache_effectiveness_report(&fixture).unwrap();

        assert!(report.pass);
        assert_eq!(report.totals.passed, 1);
        assert_eq!(report.totals.failed, 0);
        assert_eq!(report.cases[0].status, "pass");
        assert_eq!(report.cases[0].cached_input_ratio, Some(96.0));
        assert_eq!(report.cases[0].net_cached_input_tokens, 48_000);
        assert_eq!(report.cases[0].read_create_regressions, 0);
    }

    #[test]
    fn prompt_cache_effectiveness_fixture_fails_read_create_regression() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            cases: vec![SessionCostPromptCacheEffectivenessCase {
                name: "cold-rewrite".to_string(),
                source: "claude-jsonl".to_string(),
                input_lines: vec![
                    r#"{"timestamp":"2026-05-05T00:00:01Z","message":{"id":"msg-1","role":"assistant","usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#.to_string(),
                    r#"{"timestamp":"2026-05-05T00:00:02Z","message":{"id":"msg-2","role":"assistant","usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#.to_string(),
                    r#"{"timestamp":"2026-05-05T00:00:03Z","message":{"id":"msg-3","role":"assistant","usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#.to_string(),
                ],
                minimum_cached_input_ratio: 70.0,
                minimum_net_cached_input_tokens: 1,
                maximum_read_create_regressions: 0,
            }],
        };

        let report = build_prompt_cache_effectiveness_report(&fixture).unwrap();

        assert!(!report.pass);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(report.cases[0].status, "fail");
        assert_eq!(report.cases[0].read_create_regressions, 1);
        assert!(
            report.cases[0]
                .failures
                .iter()
                .any(|failure| failure.contains("read_create_regressions"))
        );
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
    fn codex_jsonl_surfaces_repeated_file_read_diagnostics() {
        let input = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,220p' src/session_cost.rs\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sed -n '1,220p' src/session_cost.rs\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cat src/main.rs\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cat src/main.rs\"}"}}"#,
            "\n"
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();

        assert!(report.file_read_diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "src/session_cost.rs"
                && diagnostic.range == "1-220"
                && diagnostic.occurrences == 2
                && diagnostic.duplicate_estimated_tokens == 3_960
                && diagnostic.follow_up_commands.iter().any(|command| {
                    command == "tsift source-read src/session_cost.rs --start 1 --lines 220 --budget normal"
                })
        }));
        assert!(report.file_read_diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "src/main.rs"
                && diagnostic.range == "full"
                && diagnostic.duplicate_estimated_tokens == 4_000
                && diagnostic
                    .follow_up_commands
                    .iter()
                    .any(|command| command == "tsift summarize --file src/main.rs")
        }));
    }

    #[test]
    fn claude_jsonl_surfaces_repeated_native_read_tool_diagnostics() {
        let input = concat!(
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs","offset":40,"limit":80}}]}}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs","offset":40,"limit":80}}]}}"#,
            "\n"
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();

        assert_eq!(report.file_read_diagnostics.len(), 1);
        let diagnostic = &report.file_read_diagnostics[0];
        assert_eq!(diagnostic.path, "src/lib.rs");
        assert_eq!(diagnostic.range, "40-119");
        assert_eq!(diagnostic.occurrences, 2);
        assert_eq!(diagnostic.duplicate_estimated_tokens, 1_440);
        assert!(diagnostic.follow_up_commands.iter().any(|command| {
            command == "tsift source-read src/lib.rs --start 40 --lines 80 --budget normal"
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

    #[test]
    fn derive_guardrails_ignores_restart_count_without_churn() {
        let guardrails = derive_guardrails(&SessionCostGuardrailInput {
            max_restart_count: Some(3),
            ..SessionCostGuardrailInput::default()
        });

        assert!(
            guardrails
                .iter()
                .all(|guardrail| guardrail.kind != "restart_loop")
        );
    }
}
