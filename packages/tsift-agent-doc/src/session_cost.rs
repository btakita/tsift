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
const MAX_PROMPT_CACHE_PREFIX_DRIFT: usize = 6;
const MAX_PROMPT_CACHE_SCORECARD: usize = 6;
const MAX_PROMPT_CACHE_BREAKPOINTS: usize = 8;
const MAX_COMMANDS_PER_BUNDLE: usize = 6;
const PROMPT_CACHE_SCORECARD_DEFAULT_NEXT_COMMAND: &str =
    "tsift session-cost --input <session.jsonl> --json";
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
pub struct SessionCostPromptCacheMetadata {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    pub stable_prefix_fingerprint: String,
    // True when the provider supplied `stable_prefix_fingerprint` explicitly
    // rather than us deriving it from provider/cache_key/stable_prefix/
    // breakpoints. A derived fingerprint is a pure function of those tracked
    // sub-fields, so a derived change is always a redundant echo of one of them
    // and is suppressed from drift attribution; an explicit fingerprint is
    // independent signal and is always reported (#tsreviewcleanup). This is an
    // internal attribution detail, not part of the serialized report.
    #[serde(skip)]
    pub stable_prefix_fingerprint_explicit: bool,
    // Raw stable-prefix content, tracked independently of the fingerprint so
    // prefix-content drift is attributed even when a provider supplies an
    // explicit `stable_prefix_fingerprint` that bypasses the derived material
    // (#pcacheexplattr).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_prefix: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub breakpoints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_affinity: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_metadata: Option<SessionCostPromptCacheMetadata>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub scorecard: Vec<SessionCostPromptCacheRoiScorecard>,
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
pub struct SessionCostPromptCacheRoiScorecard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub provider: String,
    pub sample_count: usize,
    pub net_cached_read_tokens: i64,
    pub read_create_ratio: String,
    pub trend: String,
    pub suspected_invalidation_cause: String,
    pub next_command: String,
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
    pub prefix_drift_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub prefix_drift: Vec<SessionCostPromptCachePrefixDrift>,
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
pub struct SessionCostPromptCachePrefixDrift {
    pub previous_label: String,
    pub current_label: String,
    pub trigger: String,
    pub severity: String,
    pub first_changed_field: String,
    pub field_changes: Vec<SessionCostPromptCacheFieldChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_ratio_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_ratio: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCostPromptCacheFieldChange {
    pub field: String,
    pub previous: String,
    pub current: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_metadata: Option<SessionCostPromptCacheMetadata>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_regression_scenarios: Vec<String>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub regression_scenarios: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_prefix_drift_fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionCostPromptCacheEffectivenessReport {
    pub schema_version: u64,
    pub pass: bool,
    pub totals: SessionCostPromptCacheEffectivenessTotals,
    pub required_regression_scenarios: Vec<String>,
    pub covered_regression_scenarios: Vec<String>,
    pub missing_regression_scenarios: Vec<String>,
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
    pub regression_scenarios: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_prefix_drift_fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required_diagnostics: Vec<String>,
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

#[derive(Debug, Default)]
struct PromptCacheAdapterEvidence {
    anthropic_samples: usize,
    anthropic_cache_control_samples: usize,
    openai_samples: usize,
    openai_prompt_cache_key_samples: usize,
    openai_prompt_cache_keys: BTreeSet<String>,
    routed_provider_samples: usize,
    routing_affinity_samples: usize,
    routing_affinity_values: BTreeSet<String>,
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
        source,
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

pub fn set_prompt_cache_scorecard_next_command(report: &mut SessionCostReport, next_command: &str) {
    if let Some(plan) = &mut report.prompt_cache_plan {
        for row in &mut plan.scorecard {
            row.next_command = next_command.to_string();
        }
    }
}

pub fn prompt_cache_scorecard_for_session(
    report: &SessionCostReport,
    session_source: &str,
    session_path: &str,
    next_command: &str,
) -> Vec<SessionCostPromptCacheRoiScorecard> {
    report
        .prompt_cache_plan
        .as_ref()
        .map(|plan| {
            plan.scorecard
                .iter()
                .cloned()
                .map(|mut row| {
                    row.session_source = Some(session_source.to_string());
                    row.session_path = Some(session_path.to_string());
                    row.next_command = next_command.to_string();
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn build_prompt_cache_effectiveness_report(
    fixture: &SessionCostPromptCacheEffectivenessFixture,
) -> Result<SessionCostPromptCacheEffectivenessReport> {
    if fixture.cases.is_empty() {
        bail!("prompt-cache effectiveness fixture has no cases");
    }

    let mut cases = Vec::new();
    let required_regression_scenarios =
        normalized_prompt_cache_scenarios(&fixture.required_regression_scenarios);
    let mut covered_regression_scenarios = BTreeSet::new();
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
        // Count the regression from the raw token signal, not the display-
        // truncated diagnostics vec, so a long degraded session whose per-turn
        // diagnostics would truncate the session-level regression cannot pass
        // the read/create gate (#pcacheregtrunc).
        let read_create_regressions = usize::from(
            prompt_cache_read_create_regression(
                report.cached_input_tokens,
                report.cache_creation_input_tokens,
            )
            .is_some(),
        );
        let regression_scenarios = normalized_prompt_cache_scenarios(&case.regression_scenarios);
        covered_regression_scenarios.extend(regression_scenarios.iter().cloned());

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
        failures.extend(prompt_cache_provider_adapter_failures(
            case,
            report.prompt_cache_plan.as_ref(),
        ));
        failures.extend(prompt_cache_required_prefix_drift_failures(
            analytics,
            &case.required_prefix_drift_fields,
        ));
        failures.extend(prompt_cache_required_diagnostic_failures(
            analytics,
            &case.required_diagnostics,
        ));

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
            regression_scenarios,
            required_prefix_drift_fields: normalized_prompt_cache_scenarios(
                &case.required_prefix_drift_fields,
            ),
            required_diagnostics: normalized_prompt_cache_scenarios(&case.required_diagnostics),
            failures,
        });
    }

    let covered_regression_scenarios = covered_regression_scenarios.into_iter().collect::<Vec<_>>();
    let covered_set = covered_regression_scenarios
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing_regression_scenarios = required_regression_scenarios
        .iter()
        .filter(|scenario| !covered_set.contains(*scenario))
        .cloned()
        .collect::<Vec<_>>();

    Ok(SessionCostPromptCacheEffectivenessReport {
        schema_version: fixture.schema_version,
        pass: totals.failed == 0 && missing_regression_scenarios.is_empty(),
        totals,
        required_regression_scenarios,
        covered_regression_scenarios,
        missing_regression_scenarios,
        cases,
    })
}

fn normalized_prompt_cache_scenarios(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn prompt_cache_required_prefix_drift_failures(
    analytics: Option<&SessionCostPromptCacheAnalytics>,
    required_fields: &[String],
) -> Vec<String> {
    let required_fields = normalized_prompt_cache_scenarios(required_fields);
    if required_fields.is_empty() {
        return Vec::new();
    }
    let observed_fields = analytics
        .map(|analytics| {
            analytics
                .prefix_drift
                .iter()
                .flat_map(|drift| {
                    drift
                        .field_changes
                        .iter()
                        .map(|change| change.field.clone())
                })
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    required_fields
        .into_iter()
        .filter(|field| !observed_fields.contains(field))
        .map(|field| format!("missing required prompt-cache prefix drift field `{field}`"))
        .collect()
}

fn prompt_cache_required_diagnostic_failures(
    analytics: Option<&SessionCostPromptCacheAnalytics>,
    required_kinds: &[String],
) -> Vec<String> {
    let required_kinds = normalized_prompt_cache_scenarios(required_kinds);
    if required_kinds.is_empty() {
        return Vec::new();
    }
    let observed_kinds = analytics
        .map(|analytics| {
            analytics
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.kind.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    required_kinds
        .into_iter()
        .filter(|kind| !observed_kinds.contains(kind))
        .map(|kind| format!("missing required prompt-cache diagnostic `{kind}`"))
        .collect()
}

fn prompt_cache_provider_adapter_failures(
    case: &SessionCostPromptCacheEffectivenessCase,
    plan: Option<&SessionCostPromptCachePlan>,
) -> Vec<String> {
    let mut failures = Vec::new();
    let Some(plan) = plan else {
        return failures;
    };
    let Ok(source) = SessionCostSource::parse(&case.source) else {
        return failures;
    };

    match source {
        SessionCostSource::ClaudeJsonl => require_prompt_cache_provider_adapter(
            plan,
            "anthropic",
            &["cache_control"],
            "Anthropic cache_control",
            &mut failures,
        ),
        SessionCostSource::CodexJsonl => require_prompt_cache_provider_adapter(
            plan,
            "openai",
            if case_has_regression_scenario(case, "openai_prompt_cache_key_churn") {
                &["prompt_cache_key", "prompt_cache_key_churn"]
            } else {
                &["prompt_cache_key"]
            },
            "OpenAI prompt_cache_key",
            &mut failures,
        ),
        SessionCostSource::AgentDocLog => {}
    }
    if matches!(
        source,
        SessionCostSource::ClaudeJsonl | SessionCostSource::CodexJsonl
    ) {
        require_prompt_cache_provider_adapter(
            plan,
            "replica_local",
            if case_has_regression_scenario(case, "replica_routing_churn") {
                &["routing_affinity", "routing_affinity_churn"]
            } else {
                &["routing_affinity"]
            },
            "replica-local routing_affinity",
            &mut failures,
        );
    }

    failures
}

fn require_prompt_cache_provider_adapter(
    plan: &SessionCostPromptCachePlan,
    provider: &str,
    expected_statuses: &[&str],
    label: &str,
    failures: &mut Vec<String>,
) {
    match plan
        .provider_adapters
        .iter()
        .find(|adapter| adapter.provider == provider)
    {
        Some(adapter) if expected_statuses.contains(&adapter.status.as_str()) => {}
        Some(adapter) => failures.push(format!(
            "{label} adapter status `{}`; expected one of {}",
            adapter.status,
            expected_statuses.join(", ")
        )),
        None => failures.push(format!("missing {label} adapter")),
    }
}

fn case_has_regression_scenario(
    case: &SessionCostPromptCacheEffectivenessCase,
    scenario: &str,
) -> bool {
    case.regression_scenarios
        .iter()
        .any(|value| value.trim() == scenario)
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
    source: SessionCostSource,
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

    let adapter_evidence = prompt_cache_adapter_evidence(usage_turns);
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
    push_prompt_cache_adapter_actions(&adapter_evidence, &mut actions);

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
        scorecard: derive_prompt_cache_roi_scorecard(
            usage_turns,
            default_prompt_cache_provider(source),
            PROMPT_CACHE_SCORECARD_DEFAULT_NEXT_COMMAND,
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
        provider_adapters: derive_prompt_cache_provider_adapters(&adapter_evidence),
        actions,
    })
}

fn derive_prompt_cache_provider_adapters(
    evidence: &PromptCacheAdapterEvidence,
) -> Vec<SessionCostPromptCacheProvider> {
    vec![
        SessionCostPromptCacheProvider {
            provider: "anthropic".to_string(),
            status: anthropic_cache_control_status(evidence).to_string(),
            requirements: vec![
                "attach cache_control to the stable system block".to_string(),
                "attach cache_control to the final tool definition when tools are sent".to_string(),
                "attach cache_control to the last two user-role messages; skip one-off compaction instructions"
                    .to_string(),
            ],
        },
        SessionCostPromptCacheProvider {
            provider: "openai".to_string(),
            status: openai_prompt_cache_key_status(evidence).to_string(),
            requirements: vec![
                "derive prompt_cache_key from the stable thread/session id".to_string(),
                "keep prefixes byte-identical across consecutive calls for the same key".to_string(),
            ],
        },
        SessionCostPromptCacheProvider {
            provider: "replica_local".to_string(),
            status: replica_local_routing_affinity_status(evidence).to_string(),
            requirements: vec![
                "route consecutive calls for the same cache key to the same replica when the provider cache is replica-local"
                    .to_string(),
            ],
        },
    ]
}

fn prompt_cache_adapter_evidence(usage_turns: &[SessionCostTurn]) -> PromptCacheAdapterEvidence {
    let mut evidence = PromptCacheAdapterEvidence::default();
    for metadata in usage_turns
        .iter()
        .filter_map(|turn| turn.prompt_cache_metadata.as_ref())
    {
        let anthropic = is_anthropic_provider(&metadata.provider);
        let openai = is_openai_provider(&metadata.provider);
        if anthropic {
            evidence.anthropic_samples += 1;
            if metadata_has_cache_control_breakpoint(metadata) {
                evidence.anthropic_cache_control_samples += 1;
            }
        }
        if openai {
            evidence.openai_samples += 1;
            if let Some(cache_key) = metadata.cache_key.as_ref() {
                evidence.openai_prompt_cache_key_samples += 1;
                evidence.openai_prompt_cache_keys.insert(cache_key.clone());
            }
        }
        if anthropic || openai {
            evidence.routed_provider_samples += 1;
            if let Some(routing_affinity) = metadata.routing_affinity.as_ref() {
                evidence.routing_affinity_samples += 1;
                evidence
                    .routing_affinity_values
                    .insert(routing_affinity.clone());
            }
        }
    }
    evidence
}

fn anthropic_cache_control_status(evidence: &PromptCacheAdapterEvidence) -> &'static str {
    if evidence.anthropic_samples == 0 {
        "not_observed"
    } else if evidence.anthropic_cache_control_samples == evidence.anthropic_samples {
        "cache_control"
    } else if evidence.anthropic_cache_control_samples > 0 {
        "partial_cache_control"
    } else {
        "missing_cache_control"
    }
}

fn openai_prompt_cache_key_status(evidence: &PromptCacheAdapterEvidence) -> &'static str {
    if evidence.openai_samples == 0 {
        "not_observed"
    } else if evidence.openai_prompt_cache_key_samples < evidence.openai_samples {
        if evidence.openai_prompt_cache_key_samples == 0 {
            "missing_prompt_cache_key"
        } else {
            "partial_prompt_cache_key"
        }
    } else if evidence.openai_prompt_cache_keys.len() > 1 {
        "prompt_cache_key_churn"
    } else {
        "prompt_cache_key"
    }
}

fn replica_local_routing_affinity_status(evidence: &PromptCacheAdapterEvidence) -> &'static str {
    if evidence.routed_provider_samples == 0 {
        "not_observed"
    } else if evidence.routing_affinity_samples < evidence.routed_provider_samples {
        if evidence.routing_affinity_samples == 0 {
            "missing_routing_affinity"
        } else {
            "partial_routing_affinity"
        }
    } else if evidence.routing_affinity_values.len() > 1 {
        "routing_affinity_churn"
    } else {
        "routing_affinity"
    }
}

fn push_prompt_cache_adapter_actions(
    evidence: &PromptCacheAdapterEvidence,
    actions: &mut Vec<SessionCostPromptCacheAction>,
) {
    match anthropic_cache_control_status(evidence) {
        "missing_cache_control" | "partial_cache_control" => {
            actions.push(SessionCostPromptCacheAction {
                kind: "fix_anthropic_cache_control".to_string(),
                severity: "recommend".to_string(),
                message: "Anthropic prompt-cache calls are missing cache_control breakpoints"
                    .to_string(),
                guidance: "attach cache_control to the stable Anthropic system/tool/user blocks that should be cached"
                    .to_string(),
            });
        }
        _ => {}
    }
    match openai_prompt_cache_key_status(evidence) {
        "missing_prompt_cache_key" | "partial_prompt_cache_key" | "prompt_cache_key_churn" => {
            actions.push(SessionCostPromptCacheAction {
                kind: "fix_openai_prompt_cache_key".to_string(),
                severity: "recommend".to_string(),
                message: "OpenAI prompt-cache calls need a stable prompt_cache_key".to_string(),
                guidance: "derive prompt_cache_key from the stable session/thread id and keep it unchanged across warm-prefix calls"
                    .to_string(),
            });
        }
        _ => {}
    }
    match replica_local_routing_affinity_status(evidence) {
        "missing_routing_affinity" | "partial_routing_affinity" | "routing_affinity_churn" => {
            actions.push(SessionCostPromptCacheAction {
                kind: "fix_replica_routing_affinity".to_string(),
                severity: "recommend".to_string(),
                message: "prompt-cache calls need stable replica-local routing affinity"
                    .to_string(),
                guidance: "route consecutive calls for the same cache key to the same provider replica or deployment"
                    .to_string(),
            });
        }
        _ => {}
    }
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
    let (prefix_drift, prefix_drift_truncated) = derive_prompt_cache_prefix_drift(usage_turns);
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
        prefix_drift_truncated,
        prefix_drift,
        timeline,
    })
}

fn derive_prompt_cache_roi_scorecard(
    usage_turns: &[SessionCostTurn],
    fallback_provider: &str,
    next_command: &str,
) -> Vec<SessionCostPromptCacheRoiScorecard> {
    let mut by_provider = BTreeMap::<String, Vec<SessionCostTurn>>::new();
    for turn in usage_turns {
        let provider = turn
            .prompt_cache_metadata
            .as_ref()
            .map(|metadata| metadata.provider.trim())
            .filter(|provider| !provider.is_empty())
            .unwrap_or(fallback_provider)
            .to_ascii_lowercase();
        by_provider.entry(provider).or_default().push(turn.clone());
    }

    let mut rows = by_provider
        .into_iter()
        .map(|(provider, turns)| prompt_cache_roi_scorecard_row(provider, &turns, next_command))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .net_cached_read_tokens
            .cmp(&left.net_cached_read_tokens)
            .then(left.provider.cmp(&right.provider))
    });
    rows.truncate(MAX_PROMPT_CACHE_SCORECARD);
    rows
}

fn prompt_cache_roi_scorecard_row(
    provider: String,
    turns: &[SessionCostTurn],
    next_command: &str,
) -> SessionCostPromptCacheRoiScorecard {
    let prompt_tokens = turns.iter().map(|turn| turn.prompt_tokens).sum::<u64>();
    let cached_input_tokens = turns
        .iter()
        .map(|turn| turn.cached_input_tokens)
        .sum::<u64>();
    let cache_creation_input_tokens = turns
        .iter()
        .map(|turn| turn.cache_creation_input_tokens)
        .sum::<u64>();
    let first_ratio = turns
        .first()
        .and_then(|turn| percent_ratio(turn.cached_input_tokens, turn.prompt_tokens));
    let last_ratio = turns
        .last()
        .and_then(|turn| percent_ratio(turn.cached_input_tokens, turn.prompt_tokens));
    let ratio_delta = first_ratio
        .zip(last_ratio)
        .map(|(first, last)| last - first);
    let diagnostics =
        derive_prompt_cache_diagnostics(turns, cached_input_tokens, cache_creation_input_tokens);
    let (prefix_drift, _) = derive_prompt_cache_prefix_drift(turns);
    let adapter_evidence = prompt_cache_adapter_evidence(turns);

    SessionCostPromptCacheRoiScorecard {
        session_source: None,
        session_path: None,
        provider: provider.clone(),
        sample_count: turns.len(),
        net_cached_read_tokens: signed_token_delta(
            cached_input_tokens,
            cache_creation_input_tokens,
        ),
        read_create_ratio: prompt_cache_read_create_ratio(
            cached_input_tokens,
            cache_creation_input_tokens,
        ),
        trend: prompt_cache_trend(turns.len(), ratio_delta).to_string(),
        suspected_invalidation_cause: prompt_cache_scorecard_cause(
            &provider,
            &diagnostics,
            &prefix_drift,
            &adapter_evidence,
            prompt_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        ),
        next_command: next_command.to_string(),
    }
}

fn prompt_cache_read_create_ratio(
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> String {
    if cache_creation_input_tokens > 0 {
        format!(
            "{:.2}x",
            (cached_input_tokens as f64) / (cache_creation_input_tokens as f64)
        )
    } else if cached_input_tokens > 0 {
        "read_only".to_string()
    } else {
        "-".to_string()
    }
}

fn prompt_cache_scorecard_cause(
    provider: &str,
    diagnostics: &[SessionCostPromptCacheDiagnostic],
    prefix_drift: &[SessionCostPromptCachePrefixDrift],
    adapter_evidence: &PromptCacheAdapterEvidence,
    prompt_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> String {
    if let Some(diagnostic) = diagnostics.first() {
        return diagnostic
            .likely_causes
            .first()
            .cloned()
            .unwrap_or_else(|| diagnostic.kind.clone());
    }
    if let Some(drift) = prefix_drift
        .iter()
        .find(|drift| drift.severity == "warn")
        .or_else(|| prefix_drift.first())
    {
        return format!("{} changed ({})", drift.first_changed_field, drift.trigger);
    }
    if let Some(adapter_cause) = prompt_cache_adapter_scorecard_cause(provider, adapter_evidence) {
        return adapter_cause;
    }
    if cached_input_tokens == 0 && prompt_tokens >= PROMPT_CACHE_CANDIDATE_TOKENS {
        return "no provider cache reads observed".to_string();
    }
    if cache_creation_input_tokens > cached_input_tokens {
        return "cache creation exceeded cache reads".to_string();
    }
    "none observed".to_string()
}

fn prompt_cache_adapter_scorecard_cause(
    provider: &str,
    evidence: &PromptCacheAdapterEvidence,
) -> Option<String> {
    if is_anthropic_provider(provider) {
        match anthropic_cache_control_status(evidence) {
            "missing_cache_control" => {
                return Some("missing Anthropic cache_control breakpoints".to_string());
            }
            "partial_cache_control" => {
                return Some("partial Anthropic cache_control breakpoint coverage".to_string());
            }
            _ => {}
        }
    }
    if is_openai_provider(provider) {
        match openai_prompt_cache_key_status(evidence) {
            "missing_prompt_cache_key" => {
                return Some("missing OpenAI prompt_cache_key".to_string());
            }
            "partial_prompt_cache_key" => {
                return Some("partial OpenAI prompt_cache_key coverage".to_string());
            }
            "prompt_cache_key_churn" => {
                return Some("OpenAI prompt_cache_key changed between calls".to_string());
            }
            _ => {}
        }
    }
    if is_anthropic_provider(provider) || is_openai_provider(provider) {
        match replica_local_routing_affinity_status(evidence) {
            "missing_routing_affinity" => {
                return Some("missing replica-local routing affinity".to_string());
            }
            "partial_routing_affinity" => {
                return Some("partial replica-local routing affinity coverage".to_string());
            }
            "routing_affinity_churn" => {
                return Some("replica-local routing affinity changed between calls".to_string());
            }
            _ => {}
        }
    }
    None
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
            let first_changed_field = prompt_cache_first_changed_field(previous, current);
            let drift_suffix = first_changed_field
                .as_ref()
                .map(|change| format!("; first changed prompt-cache field: {}", change.field))
                .unwrap_or_else(|| {
                    "; no prompt-cache metadata field changed between adjacent turns".to_string()
                });
            let mut likely_causes = vec![
                "stable prefix bytes changed before the cache boundary".to_string(),
                "prompt_cache_key or thread/session id changed".to_string(),
                "replica-local cache affinity was lost".to_string(),
            ];
            if let Some(change) = first_changed_field {
                likely_causes.insert(
                    0,
                    format!(
                        "first changed prompt-cache field: {} ({} -> {})",
                        change.field, change.previous, change.current
                    ),
                );
            }
            diagnostics.push(SessionCostPromptCacheDiagnostic {
                kind: "cached_ratio_drop".to_string(),
                severity: "warn".to_string(),
                label: current.label.clone(),
                message: format!(
                    "cached input ratio dropped from {} to {} at {}{}",
                    format_percent(previous_ratio),
                    format_percent(current_ratio),
                    current.label,
                    drift_suffix
                ),
                likely_causes,
                guidance:
                    "compare the prefix, tool set, cache key, compaction boundary, and routing between the previous turn and this turn"
                        .to_string(),
            });
        }
    }

    for (index, turn) in usage_turns.iter().enumerate() {
        let Some(creation_ratio) =
            percent_ratio(turn.cache_creation_input_tokens, turn.prompt_tokens)
        else {
            continue;
        };
        if turn.cache_creation_input_tokens > 0
            && creation_ratio >= PROMPT_CACHE_CREATION_SPIKE_WARN_PERCENT
        {
            let first_changed_field = index
                .checked_sub(1)
                .and_then(|previous_index| usage_turns.get(previous_index))
                .and_then(|previous| prompt_cache_first_changed_field(previous, turn));
            let drift_suffix = first_changed_field
                .as_ref()
                .map(|change| format!("; first changed prompt-cache field: {}", change.field))
                .unwrap_or_else(|| {
                    "; no adjacent prompt-cache metadata drift was detected".to_string()
                });
            let mut likely_causes = vec![
                "provider created a fresh cached prefix instead of reusing the warm prefix"
                    .to_string(),
                "system, developer, or tool block changed before the cache boundary".to_string(),
                "compaction or transient instructions entered the cached prefix".to_string(),
            ];
            if let Some(change) = first_changed_field {
                likely_causes.insert(
                    0,
                    format!(
                        "first changed prompt-cache field: {} ({} -> {})",
                        change.field, change.previous, change.current
                    ),
                );
            }
            diagnostics.push(SessionCostPromptCacheDiagnostic {
                kind: "cache_creation_spike".to_string(),
                severity: "warn".to_string(),
                label: turn.label.clone(),
                message: format!(
                    "cache creation was {} of prompt tokens at {}{}",
                    format_percent(creation_ratio),
                    turn.label,
                    drift_suffix
                ),
                likely_causes,
                guidance:
                    "inspect the cached prefix and provider breakpoint placement for this turn before treating the cache as effective"
                        .to_string(),
            });
        }
    }

    // The session-level read/create regression is computed once per session.
    let read_create_regression = prompt_cache_read_create_regression(
        cached_input_tokens,
        cache_creation_input_tokens,
    )
    .map(|read_to_creation| SessionCostPromptCacheDiagnostic {
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

    // Reserve a slot for the session-level regression before truncating the
    // per-turn diagnostics, so a long degraded session with many per-turn
    // ratio-drop/creation-spike diagnostics cannot truncate the regression away
    // and silently pass the read/create gate (#pcacheregtrunc).
    let per_turn_cap = if read_create_regression.is_some() {
        MAX_PROMPT_CACHE_DIAGNOSTICS.saturating_sub(1)
    } else {
        MAX_PROMPT_CACHE_DIAGNOSTICS
    };
    diagnostics.truncate(per_turn_cap);
    if let Some(regression) = read_create_regression {
        diagnostics.push(regression);
    }
    diagnostics
}

/// The session-level cache read/create regression signal: returns the
/// read-to-creation ratio when creation tokens exist and the ratio is below the
/// regression threshold. Computed from raw token totals so it is independent of
/// the display-truncated diagnostics vec.
fn prompt_cache_read_create_regression(
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> Option<f64> {
    if cache_creation_input_tokens == 0 {
        return None;
    }
    let read_to_creation = (cached_input_tokens as f64) / (cache_creation_input_tokens as f64);
    (read_to_creation < PROMPT_CACHE_READ_CREATE_REGRESSION_RATIO).then_some(read_to_creation)
}

fn derive_prompt_cache_prefix_drift(
    usage_turns: &[SessionCostTurn],
) -> (Vec<SessionCostPromptCachePrefixDrift>, bool) {
    let mut drift = Vec::new();

    for pair in usage_turns.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        let field_changes = prompt_cache_field_changes(previous, current);
        let Some(first_changed_field) = field_changes.first().map(|change| change.field.clone())
        else {
            continue;
        };

        let ratio_drop = prompt_cache_ratio_drop_triggered(previous, current);
        let creation_spike = prompt_cache_creation_spike_triggered(current);
        let trigger = match (ratio_drop, creation_spike) {
            (true, true) => "cached_ratio_drop_and_cache_creation_spike",
            (true, false) => "cached_ratio_drop",
            (false, true) => "cache_creation_spike",
            (false, false) => "metadata_drift",
        };

        drift.push(SessionCostPromptCachePrefixDrift {
            previous_label: previous.label.clone(),
            current_label: current.label.clone(),
            trigger: trigger.to_string(),
            severity: if ratio_drop || creation_spike {
                "warn".to_string()
            } else {
                "info".to_string()
            },
            first_changed_field,
            field_changes,
            cached_input_ratio_before: percent_ratio(
                previous.cached_input_tokens,
                previous.prompt_tokens,
            )
            .map(format_percent),
            cached_input_ratio_after: percent_ratio(
                current.cached_input_tokens,
                current.prompt_tokens,
            )
            .map(format_percent),
            cache_creation_ratio: percent_ratio(
                current.cache_creation_input_tokens,
                current.prompt_tokens,
            )
            .map(format_percent),
        });
    }

    let truncated = drift.len() > MAX_PROMPT_CACHE_PREFIX_DRIFT;
    drift.truncate(MAX_PROMPT_CACHE_PREFIX_DRIFT);
    (drift, truncated)
}

fn prompt_cache_ratio_drop_triggered(
    previous: &SessionCostTurn,
    current: &SessionCostTurn,
) -> bool {
    let Some(previous_ratio) = percent_ratio(previous.cached_input_tokens, previous.prompt_tokens)
    else {
        return false;
    };
    let Some(current_ratio) = percent_ratio(current.cached_input_tokens, current.prompt_tokens)
    else {
        return false;
    };
    previous_ratio - current_ratio >= PROMPT_CACHE_RATIO_DROP_WARN_PERCENT
}

fn prompt_cache_creation_spike_triggered(turn: &SessionCostTurn) -> bool {
    turn.cache_creation_input_tokens > 0
        && percent_ratio(turn.cache_creation_input_tokens, turn.prompt_tokens)
            .is_some_and(|ratio| ratio >= PROMPT_CACHE_CREATION_SPIKE_WARN_PERCENT)
}

fn prompt_cache_first_changed_field(
    previous: &SessionCostTurn,
    current: &SessionCostTurn,
) -> Option<SessionCostPromptCacheFieldChange> {
    prompt_cache_field_changes(previous, current)
        .into_iter()
        .next()
}

fn prompt_cache_field_changes(
    previous: &SessionCostTurn,
    current: &SessionCostTurn,
) -> Vec<SessionCostPromptCacheFieldChange> {
    let Some(previous) = previous.prompt_cache_metadata.as_ref() else {
        return Vec::new();
    };
    let Some(current) = current.prompt_cache_metadata.as_ref() else {
        return Vec::new();
    };

    // Attribute the most specific concrete cause first. The
    // `stable_prefix_fingerprint` is usually a *derived* composite of
    // provider + cache_key + stable_prefix + breakpoints, so any sub-field
    // change also flips the fingerprint; reporting the fingerprint first made
    // `first_changed_field` always read `stable_prefix_fingerprint` and never
    // named the real cause (#pcacheattr). With concrete fields ordered first,
    // the fingerprint only becomes the first change when no tracked sub-field
    // moved — i.e. the stable prefix content itself drifted — which is the
    // correct residual attribution.
    let mut changes = Vec::new();
    push_prompt_cache_field_change(
        &mut changes,
        "cache_key",
        &prompt_cache_optional_value(previous.cache_key.as_deref()),
        &prompt_cache_optional_value(current.cache_key.as_deref()),
    );
    push_prompt_cache_field_change(
        &mut changes,
        "breakpoints",
        &prompt_cache_breakpoint_value(&previous.breakpoints),
        &prompt_cache_breakpoint_value(&current.breakpoints),
    );
    push_prompt_cache_field_change(
        &mut changes,
        "routing_affinity",
        &prompt_cache_optional_value(previous.routing_affinity.as_deref()),
        &prompt_cache_optional_value(current.routing_affinity.as_deref()),
    );
    push_prompt_cache_field_change(
        &mut changes,
        "provider",
        &previous.provider,
        &current.provider,
    );
    // Raw stable-prefix content is ordered *before* the fingerprint. When a
    // provider supplies an explicit `stable_prefix_fingerprint`, the derived
    // material (which folds in `stable_prefix`) is bypassed, so a real prefix
    // CONTENT drift would otherwise change nothing tracked and go unattributed.
    // Tracking the raw prefix as its own field attributes that drift regardless
    // of explicit-vs-derived fingerprint; and because it precedes the
    // fingerprint, a derived-path prefix change is named `stable_prefix` (the
    // concrete cause) rather than the composite fingerprint (#pcacheexplattr).
    push_prompt_cache_field_change(
        &mut changes,
        "stable_prefix",
        &prompt_cache_optional_value(previous.stable_prefix.as_deref()),
        &prompt_cache_optional_value(current.stable_prefix.as_deref()),
    );
    // A *derived* fingerprint is a pure function of already-tracked sub-fields
    // (provider, cache_key, stable_prefix, breakpoints), so whenever it changes
    // one of those entries changed too and is reported above — the fingerprint
    // entry is a redundant echo. Suppress it on the derived path and report the
    // fingerprint only when the provider supplied it *explicitly* (independent
    // signal that is not captured by any tracked sub-field) (#tsreviewcleanup).
    if current.stable_prefix_fingerprint_explicit {
        push_prompt_cache_field_change(
            &mut changes,
            "stable_prefix_fingerprint",
            &previous.stable_prefix_fingerprint,
            &current.stable_prefix_fingerprint,
        );
    }
    changes
}

fn push_prompt_cache_field_change(
    changes: &mut Vec<SessionCostPromptCacheFieldChange>,
    field: &str,
    previous: &str,
    current: &str,
) {
    if previous != current {
        changes.push(SessionCostPromptCacheFieldChange {
            field: field.to_string(),
            previous: previous.to_string(),
            current: current.to_string(),
        });
    }
}

fn prompt_cache_optional_value(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("-")
        .to_string()
}

fn prompt_cache_breakpoint_value(breakpoints: &[String]) -> String {
    if breakpoints.is_empty() {
        "-".to_string()
    } else {
        breakpoints.join("; ")
    }
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
            prompt_cache_metadata: turn.prompt_cache_metadata.clone(),
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
            prompt_cache_metadata: Some(prompt_cache_metadata(
                &value,
                SessionCostSource::ClaudeJsonl,
            )),
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
            prompt_cache_metadata: Some(prompt_cache_metadata(
                &value,
                SessionCostSource::CodexJsonl,
            )),
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

fn prompt_cache_metadata(
    value: &Value,
    source: SessionCostSource,
) -> SessionCostPromptCacheMetadata {
    let provider = find_first_string_field(
        value,
        &[
            "provider",
            "model_provider",
            "provider_id",
            "model_provider_id",
        ],
    )
    .unwrap_or_else(|| default_prompt_cache_provider(source).to_string());
    let cache_key = find_first_string_field(
        value,
        &[
            "prompt_cache_key",
            "promptCacheKey",
            "cache_key",
            "cacheKey",
        ],
    );
    let routing_affinity = find_first_string_field(
        value,
        &[
            "routing_affinity",
            "routingAffinity",
            "replica",
            "replica_id",
            "replicaId",
            "deployment_id",
            "deploymentId",
        ],
    );
    let explicit_fingerprint = find_first_string_field(
        value,
        &[
            "stable_prefix_fingerprint",
            "stablePrefixFingerprint",
            "prefix_fingerprint",
            "prefixFingerprint",
        ],
    );
    let stable_prefix = find_first_string_field(
        value,
        &[
            "stable_prefix",
            "stablePrefix",
            "cached_prefix",
            "cachedPrefix",
            "prompt_prefix",
            "promptPrefix",
        ],
    );
    let mut breakpoints = Vec::new();
    collect_prompt_cache_breakpoints(value, "$", &mut breakpoints);
    // Breakpoint identity is position-independent (array indices stripped in
    // collection); collapse identical entries into a counted entry so multiple
    // ephemeral breakpoints keep their cardinality without the per-position
    // churn that read as false drift when a block was inserted (#pcachebp).
    let mut breakpoints = aggregate_prompt_cache_breakpoint_counts(breakpoints);
    breakpoints.truncate(MAX_PROMPT_CACHE_BREAKPOINTS);

    let stable_prefix_fingerprint_explicit = explicit_fingerprint.is_some();
    let stable_prefix_fingerprint = explicit_fingerprint.unwrap_or_else(|| {
        let mut material = vec![format!("provider={provider}")];
        if let Some(cache_key) = &cache_key {
            material.push(format!("cache_key={cache_key}"));
        }
        if let Some(stable_prefix) = &stable_prefix {
            material.push(format!("stable_prefix={stable_prefix}"));
        }
        for breakpoint in &breakpoints {
            material.push(format!("breakpoint={breakpoint}"));
        }
        stable_prompt_cache_fingerprint(&material.join("\n"))
    });

    SessionCostPromptCacheMetadata {
        provider,
        cache_key,
        stable_prefix_fingerprint,
        stable_prefix_fingerprint_explicit,
        stable_prefix,
        breakpoints,
        routing_affinity,
    }
}

fn default_prompt_cache_provider(source: SessionCostSource) -> &'static str {
    match source {
        SessionCostSource::ClaudeJsonl => "anthropic",
        SessionCostSource::CodexJsonl => "openai",
        SessionCostSource::AgentDocLog => "agent_doc_log",
    }
}

fn find_first_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let mut matches = Vec::new();
    collect_string_field_matches(value, "$", keys, &mut matches);
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    matches
        .into_iter()
        .map(|(_, value)| value)
        .find(|value| !value.trim().is_empty())
}

fn collect_string_field_matches(
    value: &Value,
    path: &str,
    keys: &[&str],
    matches: &mut Vec<(String, String)>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let child_path = json_child_path(path, key);
                if metadata_key_matches(key, keys)
                    && let Some(text) = child.as_str()
                {
                    matches.push((child_path.clone(), text.to_string()));
                }
                collect_string_field_matches(child, &child_path, keys, matches);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let child_path = format!("{path}[{index}]");
                collect_string_field_matches(child, &child_path, keys, matches);
            }
        }
        _ => {}
    }
}

fn collect_prompt_cache_breakpoints(value: &Value, path: &str, breakpoints: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let child_path = json_child_path(path, key);
                if metadata_key_matches(
                    key,
                    &[
                        "cache_control",
                        "cacheControl",
                        "cache_breakpoint",
                        "cacheBreakpoint",
                        "prompt_cache_breakpoint",
                        "promptCacheBreakpoint",
                    ],
                ) {
                    breakpoints.push(format!(
                        "{}={}",
                        strip_json_array_indices(child_path.trim_start_matches("$.")),
                        describe_prompt_cache_breakpoint(child)
                    ));
                }
                collect_prompt_cache_breakpoints(child, &child_path, breakpoints);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let child_path = format!("{path}[{index}]");
                collect_prompt_cache_breakpoints(child, &child_path, breakpoints);
            }
        }
        _ => {}
    }
}

/// Drop `[N]` array-index segments from a JSON path so a cache breakpoint's
/// identity does not change when a non-cached block is inserted ahead of the
/// cached one (e.g. `message.content[0].cache_control` and
/// `message.content[1].cache_control` both become `message.content.cache_control`).
fn strip_json_array_indices(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_index = false;
    for ch in path.chars() {
        match ch {
            '[' => in_index = true,
            ']' => in_index = false,
            _ if !in_index => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Collapse identical position-independent breakpoints into a sorted list where
/// repeated entries carry an `(xN)` count, so multiple breakpoints of the same
/// shape keep their cardinality without re-introducing positional churn.
fn aggregate_prompt_cache_breakpoint_counts(mut breakpoints: Vec<String>) -> Vec<String> {
    // Escape any provider-supplied breakpoint text that already ends in a
    // `(xN)`-shaped suffix, so a literal cannot masquerade as the count suffix
    // this function appends. Without it, one literal `foo (x2)` would serialize
    // identically to two plain `foo` breakpoints (which aggregate to
    // `foo (x2)`), hiding real cache-boundary drift behind a false match
    // (#tsreviewcleanup).
    for breakpoint in &mut breakpoints {
        if let Some(open) = breakpoint_count_suffix_start(breakpoint) {
            *breakpoint = format!("{} (\\x{}", &breakpoint[..open], &breakpoint[open + 3..]);
        }
    }
    breakpoints.sort();
    let mut aggregated: Vec<String> = Vec::new();
    let mut index = 0;
    while index < breakpoints.len() {
        let breakpoint = &breakpoints[index];
        let mut count = 1;
        while index + count < breakpoints.len() && breakpoints[index + count] == *breakpoint {
            count += 1;
        }
        if count > 1 {
            aggregated.push(format!("{breakpoint} (x{count})"));
        } else {
            aggregated.push(breakpoint.clone());
        }
        index += count;
    }
    aggregated
}

/// Byte offset of a trailing ` (x<digits>)` count-shaped suffix, if present.
/// The real aggregation suffix never contains a backslash, so an escaped
/// literal (` (\xN)`) is not matched and cannot be double-escaped.
fn breakpoint_count_suffix_start(breakpoint: &str) -> Option<usize> {
    let inner = breakpoint.strip_suffix(')')?;
    let open = inner.rfind(" (x")?;
    let digits = &inner[open + 3..];
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        Some(open)
    } else {
        None
    }
}

fn describe_prompt_cache_breakpoint(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(enabled) = value.as_bool() {
        return enabled.to_string();
    }
    if let Some(object) = value.as_object()
        && let Some(kind) = object.get("type").and_then(Value::as_str)
    {
        return format!("type:{kind}");
    }
    value.to_string()
}

fn metadata_has_cache_control_breakpoint(metadata: &SessionCostPromptCacheMetadata) -> bool {
    metadata.breakpoints.iter().any(|breakpoint| {
        let key = breakpoint
            .split_once('=')
            .map_or(breakpoint.as_str(), |(key, _)| key);
        normalize_metadata_key(key).contains("cachecontrol")
    })
}

fn is_anthropic_provider(provider: &str) -> bool {
    let provider = normalize_metadata_key(provider);
    provider.contains("anthropic") || provider.contains("claude")
}

fn is_openai_provider(provider: &str) -> bool {
    let provider = normalize_metadata_key(provider);
    provider.contains("openai") || provider.contains("azureopenai") || provider.contains("codex")
}

fn metadata_key_matches(key: &str, candidates: &[&str]) -> bool {
    let key = normalize_metadata_key(key);
    candidates
        .iter()
        .any(|candidate| key == normalize_metadata_key(candidate))
}

fn normalize_metadata_key(key: &str) -> String {
    key.chars()
        .filter(|value| *value != '_' && *value != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn json_child_path(parent: &str, key: &str) -> String {
    if parent == "$" {
        format!("$.{key}")
    } else {
        format!("{parent}.{key}")
    }
}

fn stable_prompt_cache_fingerprint(material: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in material.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("spfx-{hash:016x}")
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

    fn prompt_cache_adapter_status<'a>(
        plan: &'a SessionCostPromptCachePlan,
        provider: &str,
    ) -> Option<&'a str> {
        plan.provider_adapters
            .iter()
            .find(|adapter| adapter.provider == provider)
            .map(|adapter| adapter.status.as_str())
    }

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
    fn prompt_cache_timeline_emits_attribution_metadata() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":10}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1100,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000,"output_tokens":12}}}"#,
            "\n",
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();
        let plan = report
            .prompt_cache_plan
            .as_ref()
            .expect("prompt cache plan should be present");
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present");
        let first = analytics.timeline[0]
            .prompt_cache_metadata
            .as_ref()
            .expect("timeline should include prompt cache metadata");
        let second = analytics.timeline[1]
            .prompt_cache_metadata
            .as_ref()
            .expect("timeline should include prompt cache metadata");

        assert_eq!(first.provider, "anthropic");
        assert_eq!(first.cache_key.as_deref(), Some("agent-doc:tsift"));
        assert_eq!(first.routing_affinity.as_deref(), Some("replica-a"));
        // Breakpoint identity is position-independent: array indices are stripped
        // so an inserted block ahead of the cached one is not read as drift (#pcachebp).
        assert!(
            first.breakpoints.iter().any(|breakpoint| {
                breakpoint == "message.content.cache_control=type:ephemeral"
            })
        );
        assert!(first.stable_prefix_fingerprint.starts_with("spfx-"));
        assert_eq!(
            first.stable_prefix_fingerprint,
            second.stable_prefix_fingerprint
        );
        assert_eq!(
            prompt_cache_adapter_status(plan, "anthropic"),
            Some("cache_control")
        );
        assert_eq!(
            prompt_cache_adapter_status(plan, "replica_local"),
            Some("routing_affinity")
        );
    }

    #[test]
    fn prompt_cache_plan_marks_missing_provider_adapter_evidence() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#,
            "\n",
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();
        let plan = report
            .prompt_cache_plan
            .as_ref()
            .expect("prompt cache plan should be present");

        assert_eq!(
            prompt_cache_adapter_status(plan, "openai"),
            Some("missing_prompt_cache_key")
        );
        assert_eq!(
            prompt_cache_adapter_status(plan, "replica_local"),
            Some("missing_routing_affinity")
        );
        assert!(plan.actions.iter().any(|action| {
            action.kind == "fix_openai_prompt_cache_key"
                && action.guidance.contains("prompt_cache_key")
        }));
        assert!(plan.actions.iter().any(|action| {
            action.kind == "fix_replica_routing_affinity"
                && action.guidance.contains("same provider replica")
        }));

        let anthropic = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","routing_affinity":"replica-a","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":10}}}"#,
            "\n",
        );
        let report = compute(anthropic, Some("claude-jsonl")).unwrap();
        let plan = report
            .prompt_cache_plan
            .as_ref()
            .expect("prompt cache plan should be present");
        assert_eq!(
            prompt_cache_adapter_status(plan, "anthropic"),
            Some("missing_cache_control")
        );
        assert!(plan.actions.iter().any(|action| {
            action.kind == "fix_anthropic_cache_control"
                && action.guidance.contains("cache_control")
        }));
    }

    #[test]
    fn prompt_cache_plan_marks_routing_affinity_churn() {
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-b","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#,
            "\n",
        );

        let report = compute(input, Some("codex-jsonl")).unwrap();
        let plan = report
            .prompt_cache_plan
            .as_ref()
            .expect("prompt cache plan should be present");

        assert_eq!(
            prompt_cache_adapter_status(plan, "openai"),
            Some("prompt_cache_key")
        );
        assert_eq!(
            prompt_cache_adapter_status(plan, "replica_local"),
            Some("routing_affinity_churn")
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.kind == "fix_replica_routing_affinity")
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
    fn prompt_cache_prefix_drift_points_regressions_at_first_changed_field() {
        // Only the cache_key changes between turns; provider, routing, prefix,
        // and breakpoints are identical. The derived fingerprint flips too (it
        // hashes the cache_key), but attribution must name the concrete cause —
        // `cache_key` — not the composite fingerprint (#pcacheattr).
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift-cold","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present");

        assert_eq!(analytics.prefix_drift.len(), 1);
        let drift = &analytics.prefix_drift[0];
        assert_eq!(drift.trigger, "cached_ratio_drop_and_cache_creation_spike");
        assert_eq!(drift.severity, "warn");
        // Attribution names the real cause, not the derived composite.
        assert_eq!(drift.first_changed_field, "cache_key");
        assert_eq!(drift.cached_input_ratio_before.as_deref(), Some("90.00%"));
        assert_eq!(drift.cached_input_ratio_after.as_deref(), Some("10.00%"));
        assert_eq!(drift.cache_creation_ratio.as_deref(), Some("60.00%"));
        // Only the changed concrete field is reported. The derived fingerprint
        // flips too (it hashes the cache_key), but that is a redundant echo of
        // the cache_key change and is suppressed (#tsreviewcleanup).
        assert!(
            drift
                .field_changes
                .iter()
                .any(|change| change.field == "cache_key"),
            "expected drift field cache_key"
        );
        for field in [
            "provider",
            "routing_affinity",
            "breakpoints",
            "stable_prefix",
            "stable_prefix_fingerprint",
        ] {
            assert!(
                !drift
                    .field_changes
                    .iter()
                    .any(|change| change.field == field),
                "unchanged field {field} must not be reported as drift"
            );
        }
        assert!(analytics.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "cached_ratio_drop"
                && diagnostic
                    .message
                    .contains("first changed prompt-cache field: cache_key")
        }));
        assert!(analytics.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == "cache_creation_spike"
                && diagnostic
                    .likely_causes
                    .iter()
                    .any(|cause| cause.contains("first changed prompt-cache field: cache_key"))
        }));
    }

    #[test]
    fn prompt_cache_drift_attributes_pure_prefix_content_change_to_stable_prefix() {
        // Provider, cache_key, routing, and breakpoints are identical across
        // turns; only the stable prefix *content* drifts. The raw `stable_prefix`
        // is now tracked as its own field ordered before the fingerprint, so the
        // drift is attributed to the concrete `stable_prefix` cause rather than
        // the derived composite fingerprint (#pcacheexplattr).
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v2 with edits","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present");

        assert_eq!(analytics.prefix_drift.len(), 1);
        let drift = &analytics.prefix_drift[0];
        // The concrete `stable_prefix` content change is the attributed cause.
        assert_eq!(drift.first_changed_field, "stable_prefix");
        // On the derived path the fingerprint folds in the prefix, so it would
        // also flip — but a derived fingerprint is a redundant echo of the
        // tracked sub-field that fed it and is suppressed (#tsreviewcleanup),
        // leaving `stable_prefix` as the sole reported change.
        assert_eq!(drift.field_changes.len(), 1);
        assert_eq!(drift.field_changes[0].field, "stable_prefix");
        assert!(
            !drift
                .field_changes
                .iter()
                .any(|change| change.field == "stable_prefix_fingerprint"),
            "derived fingerprint echo must be suppressed when a sub-field changed"
        );
    }

    #[test]
    fn prompt_cache_drift_attributes_prefix_change_under_explicit_fingerprint() {
        // A provider supplies an explicit `stable_prefix_fingerprint` that stays
        // CONSTANT across turns while the raw `stable_prefix` content drifts.
        // Before #pcacheexplattr the explicit fingerprint bypassed the derived
        // material, so nothing tracked changed and the prefix drift went
        // unattributed. The raw `stable_prefix` field now captures it.
        let input = concat!(
            r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix_fingerprint":"provider-fpr-constant","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":9000,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix_fingerprint":"provider-fpr-constant","stable_prefix":"agent-doc stable prefix v2 with edits","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}],"usage":{"input_tokens":3000,"cache_creation_input_tokens":6000,"cache_read_input_tokens":1000,"output_tokens":50}}}"#,
            "\n",
        );

        let report = compute(input, Some("claude-jsonl")).unwrap();
        let analytics = report
            .prompt_cache_plan
            .as_ref()
            .and_then(|plan| plan.analytics.as_ref())
            .expect("prompt cache analytics should be present");

        assert_eq!(analytics.prefix_drift.len(), 1);
        let drift = &analytics.prefix_drift[0];
        // The explicit fingerprint is unchanged, so the ONLY attributed cause is
        // the raw prefix content — which would have been invisible before.
        assert_eq!(drift.first_changed_field, "stable_prefix");
        assert_eq!(drift.field_changes.len(), 1);
        assert_eq!(drift.field_changes[0].field, "stable_prefix");
        assert!(
            !drift
                .field_changes
                .iter()
                .any(|change| change.field == "stable_prefix_fingerprint"),
            "constant explicit fingerprint must not be reported as drift"
        );
    }

    #[test]
    fn prompt_cache_breakpoint_identity_is_position_independent() {
        let turn_a = serde_json::json!({
            "message": { "content": [
                { "type": "text", "text": "sys", "cache_control": { "type": "ephemeral" } }
            ]}
        });
        // A non-cached block inserted ahead shifts content[0] -> content[1].
        let turn_b = serde_json::json!({
            "message": { "content": [
                { "type": "thinking", "text": "..." },
                { "type": "text", "text": "sys", "cache_control": { "type": "ephemeral" } }
            ]}
        });

        let mut bp_a = Vec::new();
        collect_prompt_cache_breakpoints(&turn_a, "$", &mut bp_a);
        let bp_a = aggregate_prompt_cache_breakpoint_counts(bp_a);

        let mut bp_b = Vec::new();
        collect_prompt_cache_breakpoints(&turn_b, "$", &mut bp_b);
        let bp_b = aggregate_prompt_cache_breakpoint_counts(bp_b);

        assert_eq!(
            bp_a,
            vec!["message.content.cache_control=type:ephemeral".to_string()]
        );
        assert_eq!(
            bp_a, bp_b,
            "inserting a non-cached block must not change breakpoint identity (#pcachebp)"
        );
    }

    #[test]
    fn prompt_cache_breakpoints_keep_count_of_repeated_shapes() {
        let turn = serde_json::json!({
            "message": { "content": [
                { "type": "text", "cache_control": { "type": "ephemeral" } },
                { "type": "text", "cache_control": { "type": "ephemeral" } }
            ]}
        });
        let mut bp = Vec::new();
        collect_prompt_cache_breakpoints(&turn, "$", &mut bp);
        let bp = aggregate_prompt_cache_breakpoint_counts(bp);
        assert_eq!(
            bp,
            vec!["message.content.cache_control=type:ephemeral (x2)".to_string()]
        );
    }

    #[test]
    fn prompt_cache_breakpoint_literal_count_suffix_does_not_collide_with_aggregation() {
        // A provider breakpoint whose text literally ends in `(x2)` must not
        // serialize identically to two plain breakpoints that aggregate to
        // `... (x2)`, or real cache-boundary drift between the two states would
        // be hidden behind a false match (#tsreviewcleanup).
        let literal = aggregate_prompt_cache_breakpoint_counts(vec!["foo (x2)".to_string()]);
        let aggregated =
            aggregate_prompt_cache_breakpoint_counts(vec!["foo".to_string(), "foo".to_string()]);
        assert_ne!(literal, aggregated);
        // The literal is escaped (`(\x2)`); the genuine count suffix is `(x2)`.
        assert_eq!(literal, vec!["foo (\\x2)".to_string()]);
        assert_eq!(aggregated, vec!["foo (x2)".to_string()]);
        // Two copies of the literal aggregate on the escaped form, still
        // distinct from a single literal.
        let two_literals = aggregate_prompt_cache_breakpoint_counts(vec![
            "foo (x2)".to_string(),
            "foo (x2)".to_string(),
        ]);
        assert_eq!(two_literals, vec!["foo (\\x2) (x2)".to_string()]);
        assert_ne!(two_literals, literal);
    }

    #[test]
    fn read_create_regression_survives_diagnostics_truncation() {
        // Eight turns each trigger a cache-creation spike (50% creation ratio),
        // producing more per-turn diagnostics than MAX_PROMPT_CACHE_DIAGNOSTICS,
        // plus an overall read/create regression (800 read / 4000 creation =
        // 0.2x, far below the 2.0 threshold).
        let turns: Vec<SessionCostTurn> = (0..8)
            .map(|idx| SessionCostTurn {
                label: format!("t{idx}"),
                prompt_tokens: 1000,
                cached_input_tokens: 100,
                cache_creation_input_tokens: 500,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 1100,
                prompt_cache_metadata: None,
            })
            .collect();

        let diagnostics = derive_prompt_cache_diagnostics(&turns, 800, 4000);

        assert_eq!(diagnostics.len(), MAX_PROMPT_CACHE_DIAGNOSTICS);
        // Before the fix the session-level regression was pushed last and
        // truncated away, so the read/create gate read 0 and passed (#pcacheregtrunc).
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == "read_create_regression"),
            "session-level read/create regression must survive diagnostics truncation: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.kind.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn prompt_cache_effectiveness_fixture_passes_thresholds() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            required_regression_scenarios: Vec::new(),
            cases: vec![SessionCostPromptCacheEffectivenessCase {
                name: "warm-codex-prefix".to_string(),
                source: "codex-jsonl".to_string(),
                input_lines: vec![
                    r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#.to_string(),
                    r#"{"timestamp":"2026-05-05T00:00:04Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#.to_string(),
                ],
                minimum_cached_input_ratio: 90.0,
                minimum_net_cached_input_tokens: 40_000,
                maximum_read_create_regressions: 0,
                regression_scenarios: Vec::new(),
                required_prefix_drift_fields: Vec::new(),
                required_diagnostics: Vec::new(),
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
    fn prompt_cache_effectiveness_fixture_fails_missing_adapter_evidence() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            required_regression_scenarios: Vec::new(),
            cases: vec![
                SessionCostPromptCacheEffectivenessCase {
                    name: "missing-openai-key".to_string(),
                    source: "codex-jsonl".to_string(),
                    input_lines: vec![
                        r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#.to_string(),
                        r#"{"timestamp":"2026-05-05T00:00:04Z","provider":"openai","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#.to_string(),
                    ],
                    minimum_cached_input_ratio: 90.0,
                    minimum_net_cached_input_tokens: 40_000,
                    maximum_read_create_regressions: 0,
                    regression_scenarios: Vec::new(),
                    required_prefix_drift_fields: Vec::new(),
                    required_diagnostics: Vec::new(),
                },
                SessionCostPromptCacheEffectivenessCase {
                    name: "missing-anthropic-cache-control".to_string(),
                    source: "claude-jsonl".to_string(),
                    input_lines: vec![
                        r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-1","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1000,"cache_creation_input_tokens":100,"cache_read_input_tokens":9000,"output_tokens":10}}}"#.to_string(),
                        r#"{"timestamp":"2026-05-05T00:00:02Z","provider":"anthropic","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix v1","message":{"id":"msg-2","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1100,"cache_creation_input_tokens":0,"cache_read_input_tokens":10000,"output_tokens":12}}}"#.to_string(),
                    ],
                    minimum_cached_input_ratio: 70.0,
                    minimum_net_cached_input_tokens: 1,
                    maximum_read_create_regressions: 0,
                    regression_scenarios: Vec::new(),
                    required_prefix_drift_fields: Vec::new(),
                    required_diagnostics: Vec::new(),
                },
            ],
        };

        let report = build_prompt_cache_effectiveness_report(&fixture).unwrap();

        assert!(!report.pass);
        assert_eq!(report.totals.failed, 2);
        assert!(report.cases[0].failures.iter().any(|failure| {
            failure.contains("OpenAI prompt_cache_key")
                && failure.contains("missing_prompt_cache_key")
        }));
        assert!(report.cases[0].failures.iter().any(|failure| {
            failure.contains("replica-local routing_affinity")
                && failure.contains("missing_routing_affinity")
        }));
        assert!(report.cases[1].failures.iter().any(|failure| {
            failure.contains("Anthropic cache_control") && failure.contains("missing_cache_control")
        }));
    }

    #[test]
    fn prompt_cache_effectiveness_fixture_fails_read_create_regression() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            required_regression_scenarios: Vec::new(),
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
                regression_scenarios: Vec::new(),
                required_prefix_drift_fields: Vec::new(),
                required_diagnostics: Vec::new(),
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
    fn prompt_cache_effectiveness_fixture_requires_regression_coverage_and_drift_fields() {
        let fixture = SessionCostPromptCacheEffectivenessFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            required_regression_scenarios: vec![
                "volatile_prefix_generated_header".to_string(),
                "openai_prompt_cache_key_churn".to_string(),
            ],
            cases: vec![SessionCostPromptCacheEffectivenessCase {
                name: "volatile-prefix".to_string(),
                source: "codex-jsonl".to_string(),
                input_lines: vec![
                    r#"{"timestamp":"2026-05-05T00:00:01Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix\nGenerated: 2026-05-05T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":24000,"cached_input_tokens":23000,"output_tokens":300,"reasoning_output_tokens":100,"total_tokens":24300}}}}"#.to_string(),
                    r#"{"timestamp":"2026-05-05T00:00:04Z","provider":"openai","prompt_cache_key":"agent-doc:tsift","routing_affinity":"replica-a","stable_prefix":"agent-doc stable prefix\nGenerated: 2026-05-05T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"cached_input_tokens":48000,"output_tokens":650,"reasoning_output_tokens":180,"total_tokens":50650}}}}"#.to_string(),
                ],
                minimum_cached_input_ratio: 90.0,
                minimum_net_cached_input_tokens: 40_000,
                maximum_read_create_regressions: 0,
                regression_scenarios: vec!["volatile_prefix_generated_header".to_string()],
                // Prefix-content drift (the `Generated:` timestamp) is now
                // attributed to the concrete `stable_prefix` field; the derived
                // fingerprint echo is suppressed (#tsreviewcleanup).
                required_prefix_drift_fields: vec!["stable_prefix".to_string()],
                required_diagnostics: Vec::new(),
            }],
        };

        let report = build_prompt_cache_effectiveness_report(&fixture).unwrap();

        assert!(!report.pass);
        assert_eq!(
            report.missing_regression_scenarios,
            vec!["openai_prompt_cache_key_churn".to_string()]
        );
        assert_eq!(
            report.covered_regression_scenarios,
            vec!["volatile_prefix_generated_header".to_string()]
        );
        assert!(report.cases[0].failures.is_empty());
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
