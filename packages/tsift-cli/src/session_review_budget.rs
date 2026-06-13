use serde::Serialize;
use std::collections::BTreeSet;

use tsift_agent_doc::session_review;

use crate::output::ResponseBudget;
use crate::{
    CompactSymbolRefPreview, SESSION_REVIEW_FOLLOW_UP_CONTRACT_VERSION, TagOntologyPreviewContext,
    build_compact_symbol_ref_with_ontology, format_compact_count, format_symbol_preview_line,
    shell_quote, stable_handle, truncate_for_budget,
};

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewBudgetSessionPreview {
    pub(crate) handle: String,
    pub(crate) source: String,
    pub(crate) path: String,
    pub(crate) matched_by: Vec<String>,
    pub(crate) total_tokens: u64,
    pub(crate) largest_turn_total_tokens: u64,
    pub(crate) prompt_targets: usize,
    pub(crate) failures: usize,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewBudgetPromptPreview {
    pub(crate) handle: String,
    pub(crate) text: String,
    pub(crate) occurrences: usize,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewBudgetFailurePreview {
    pub(crate) handle: String,
    pub(crate) kind: String,
    pub(crate) message: String,
    pub(crate) occurrences: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_path: Option<String>,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewBudgetReport {
    pub(crate) target: String,
    pub(crate) target_kind: String,
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
    pub(crate) sessions_matched: usize,
    pub(crate) prompt_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_session_total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_session_largest_turn_total_tokens: Option<u64>,
    pub(crate) truncated: bool,
    pub(crate) sessions: Vec<SessionReviewBudgetSessionPreview>,
    pub(crate) prompt_targets: Vec<SessionReviewBudgetPromptPreview>,
    pub(crate) failures: Vec<SessionReviewBudgetFailurePreview>,
    pub(crate) guardrails: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewNextTokenAction {
    pub(crate) priority: usize,
    pub(crate) kind: String,
    pub(crate) severity: String,
    pub(crate) message: String,
    pub(crate) guidance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compact_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) restart_command: Option<String>,
    pub(crate) digest_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) rewrite_commands: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewAgentDocExpansionBudgetHandle {
    pub(crate) handle: String,
    pub(crate) label: String,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewAgentDocQueueBudgetProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active_queue_prompt: Option<String>,
    pub(crate) live_exchange_tail_total: usize,
    pub(crate) backlog_row_total: usize,
    pub(crate) review_row_total: usize,
    pub(crate) prompt_preset_total: usize,
    pub(crate) expansion_handle_total: usize,
    pub(crate) truncated: bool,
    pub(crate) live_exchange_tail: Vec<String>,
    pub(crate) backlog_rows: Vec<String>,
    pub(crate) review_rows: Vec<String>,
    pub(crate) prompt_presets: Vec<String>,
    pub(crate) expansion_handles: Vec<SessionReviewAgentDocExpansionBudgetHandle>,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionReviewNextContextBudgetReport {
    pub(crate) contract_version: &'static str,
    pub(crate) target: String,
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
    pub(crate) prompt_target_total: usize,
    pub(crate) touched_file_total: usize,
    pub(crate) touched_symbol_total: usize,
    pub(crate) unresolved_failure_total: usize,
    pub(crate) truncated: bool,
    pub(crate) prompt_targets: Vec<String>,
    pub(crate) touched_files: Vec<String>,
    pub(crate) touched_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) touched_symbol_refs: Vec<CompactSymbolRefPreview>,
    pub(crate) unresolved_failures: Vec<SessionReviewBudgetFailurePreview>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) next_token_actions: Vec<SessionReviewNextTokenAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_doc_queue: Option<SessionReviewAgentDocQueueBudgetProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_health: Option<session_review::SessionReviewPromptCacheHealth>,
    pub(crate) next_digest_commands: Vec<String>,
}

fn session_review_source_flag(source: &str) -> &'static str {
    match source {
        "claude_jsonl" => "claude-jsonl",
        "codex_jsonl" => "codex-jsonl",
        "agent_doc_log" => "agent-doc-log",
        _ => "markdown",
    }
}

pub(crate) fn build_session_review_budget_report(
    report: &session_review::SessionReviewReport,
    budget: ResponseBudget,
) -> SessionReviewBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let review_expand = format!(
        "tsift session-review {} --json",
        shell_quote(&report.target)
    );
    let sessions = report
        .sessions
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetSessionPreview {
            handle: stable_handle(
                "srev",
                &format!("{}:{}:{}", entry.source, entry.path, entry.total_tokens),
            ),
            source: entry.source.clone(),
            path: truncate_for_budget(&entry.path, max_bytes),
            matched_by: entry
                .matched_by
                .iter()
                .take(max_items)
                .map(|value| truncate_for_budget(value, max_bytes))
                .collect(),
            total_tokens: entry.total_tokens,
            largest_turn_total_tokens: entry.largest_turn_total_tokens,
            prompt_targets: entry.prompt_target_count,
            failures: entry.failure_groups,
            expand: format!(
                "tsift session-digest --path {} --input {} --source {}",
                shell_quote(&report.root),
                shell_quote(&entry.path),
                session_review_source_flag(&entry.source)
            ),
        })
        .collect();
    let prompt_targets = report
        .prompt_targets
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetPromptPreview {
            handle: stable_handle("spt", &entry.text),
            text: truncate_for_budget(&entry.text, max_bytes),
            occurrences: entry.occurrences,
            expand: review_expand.clone(),
        })
        .collect();
    let failures = report
        .failures
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetFailurePreview {
            handle: stable_handle("sfl", &format!("{}:{}", entry.kind, entry.message)),
            kind: entry.kind.clone(),
            message: truncate_for_budget(&entry.message, max_bytes),
            occurrences: entry.occurrences,
            command: entry
                .command
                .as_ref()
                .map(|command| truncate_for_budget(command, max_bytes)),
            session_path: entry
                .session_path
                .as_ref()
                .map(|path| truncate_for_budget(path, max_bytes)),
            expand: review_expand.clone(),
        })
        .collect();
    let guardrails = report
        .guardrails
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(&entry.message, max_bytes))
        .collect();
    let warnings = report
        .warnings
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(entry, max_bytes))
        .collect();

    SessionReviewBudgetReport {
        target: report.target.clone(),
        target_kind: report.target_kind.clone(),
        max_items,
        max_bytes,
        sessions_matched: report.sessions_matched,
        prompt_tokens: report.prompt_tokens,
        cached_input_tokens: report.cached_input_tokens,
        total_tokens: report.total_tokens,
        latest_session_total_tokens: report
            .latest_session_cost
            .as_ref()
            .map(|cost| cost.total_tokens),
        latest_session_largest_turn_total_tokens: report
            .latest_session_cost
            .as_ref()
            .map(|cost| cost.largest_turn_total_tokens),
        truncated: report.sessions.len() > max_items
            || report.prompt_targets.len() > max_items
            || report.failures.len() > max_items
            || report.guardrails.len() > max_items
            || report.warnings.len() > max_items,
        sessions,
        prompt_targets,
        failures,
        guardrails,
        warnings,
    }
}

pub(crate) fn build_session_review_next_context_budget_report(
    report: &session_review::SessionReviewReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> SessionReviewNextContextBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let follow_up_items = budget.follow_up_items();
    let next_token_actions = build_next_token_actions(report, max_items, max_bytes);
    let actionable_guardrail_failures = next_token_actions
        .iter()
        .map(|action| format!("guardrail:{}", action.kind))
        .collect::<BTreeSet<_>>();
    let unresolved_failures = report
        .next_context
        .unresolved_failures
        .iter()
        .filter(|entry| !actionable_guardrail_failures.contains(&entry.kind))
        .collect::<Vec<_>>();
    let unresolved_failure_total = unresolved_failures.len();
    let agent_doc_queue = report.next_context.agent_doc_queue.as_ref().map(|profile| {
        build_agent_doc_queue_budget_profile(profile, max_items, max_bytes, follow_up_items)
    });
    let agent_doc_queue_truncated = agent_doc_queue
        .as_ref()
        .is_some_and(|profile| profile.truncated);
    SessionReviewNextContextBudgetReport {
        contract_version: SESSION_REVIEW_FOLLOW_UP_CONTRACT_VERSION,
        target: report.next_context.target.clone(),
        max_items,
        max_bytes,
        prompt_target_total: report.next_context.active_prompt_targets.len(),
        touched_file_total: report.next_context.touched_files.len(),
        touched_symbol_total: report.next_context.touched_symbols.len(),
        unresolved_failure_total,
        truncated: report.next_context.active_prompt_targets.len() > max_items
            || report.next_context.touched_files.len() > max_items
            || report.next_context.touched_symbols.len() > max_items
            || unresolved_failure_total > max_items
            || report.next_context.next_digest_commands.len() > follow_up_items
            || agent_doc_queue_truncated,
        prompt_targets: report
            .next_context
            .active_prompt_targets
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_files: report
            .next_context
            .touched_files
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_symbols: report
            .next_context
            .touched_symbols
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_symbol_refs: report
            .next_context
            .touched_symbols
            .iter()
            .take(max_items)
            .map(|entry| {
                build_compact_symbol_ref_with_ontology(
                    "ncsym",
                    &format!("{}:{}", report.next_context.target, entry),
                    entry,
                    None,
                    max_bytes,
                    ontology,
                )
            })
            .collect(),
        unresolved_failures: unresolved_failures
            .iter()
            .take(max_items)
            .map(|entry| SessionReviewBudgetFailurePreview {
                handle: stable_handle("snf", &format!("{}:{}", entry.kind, entry.message)),
                kind: entry.kind.clone(),
                message: truncate_for_budget(&entry.message, max_bytes),
                occurrences: entry.occurrences,
                command: entry
                    .command
                    .as_ref()
                    .map(|command| truncate_for_budget(command, max_bytes)),
                session_path: entry
                    .session_path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                expand: format!(
                    "tsift session-review {} --next-context --json",
                    shell_quote(&report.target)
                ),
            })
            .collect(),
        next_token_actions,
        agent_doc_queue,
        prompt_cache_health: report.next_context.prompt_cache_health.clone(),
        next_digest_commands: report
            .next_context
            .next_digest_commands
            .iter()
            .take(follow_up_items)
            .cloned()
            .collect(),
    }
}

fn build_agent_doc_queue_budget_profile(
    profile: &session_review::SessionReviewAgentDocQueueProfile,
    max_items: usize,
    max_bytes: usize,
    follow_up_items: usize,
) -> SessionReviewAgentDocQueueBudgetProfile {
    SessionReviewAgentDocQueueBudgetProfile {
        active_queue_prompt: profile
            .active_queue_prompt
            .as_ref()
            .map(|prompt| truncate_for_budget(prompt, max_bytes)),
        live_exchange_tail_total: profile.live_exchange_tail.len(),
        backlog_row_total: profile.backlog_rows.len(),
        review_row_total: profile.review_rows.len(),
        prompt_preset_total: profile.prompt_presets.len(),
        expansion_handle_total: profile.expansion_handles.len(),
        truncated: profile.live_exchange_tail.len() > max_items
            || profile.backlog_rows.len() > max_items
            || profile.review_rows.len() > max_items
            || profile.prompt_presets.len() > max_items
            || profile.expansion_handles.len() > follow_up_items,
        live_exchange_tail: profile
            .live_exchange_tail
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        backlog_rows: profile
            .backlog_rows
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        review_rows: profile
            .review_rows
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        prompt_presets: profile
            .prompt_presets
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        expansion_handles: profile
            .expansion_handles
            .iter()
            .take(follow_up_items)
            .map(|entry| SessionReviewAgentDocExpansionBudgetHandle {
                handle: entry.handle.clone(),
                label: truncate_for_budget(&entry.label, max_bytes),
                expand: entry.expand.clone(),
            })
            .collect(),
    }
}

fn build_next_token_actions(
    report: &session_review::SessionReviewReport,
    max_items: usize,
    max_bytes: usize,
) -> Vec<SessionReviewNextTokenAction> {
    let target = shell_quote(&report.target);
    let doc_command_target =
        (report.target_kind == "file" && report.target.ends_with(".md")).then_some(target.clone());
    let mut actions = report
        .guardrails
        .iter()
        .filter_map(|guardrail| {
            let priority = token_action_priority(&guardrail.kind)?;
            let compact_command = doc_command_target
                .as_ref()
                .map(|target| format!("agent-doc compact {target} --commit"));
            let restart_command = doc_command_target
                .as_ref()
                .map(|target| format!("agent-doc start {target}"));
            let digest_commands = token_action_digest_commands(&target);
            let rewrite_commands =
                guardrail_rewrite_commands(report, &guardrail.kind, &target, max_items);
            Some(SessionReviewNextTokenAction {
                priority,
                kind: guardrail.kind.clone(),
                severity: guardrail.severity.clone(),
                message: truncate_for_budget(&guardrail.message, max_bytes),
                guidance: truncate_for_budget(&guardrail.guidance, max_bytes),
                compact_command,
                restart_command,
                digest_commands,
                rewrite_commands,
            })
        })
        .collect::<Vec<_>>();
    if let Some(action) = repeated_raw_read_action(report, &target, max_items, max_bytes) {
        actions.push(action);
    }
    if let Some(action) = repeated_command_bundle_action(report, &target, max_items, max_bytes) {
        actions.push(action);
    }
    actions.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then(left.kind.cmp(&right.kind))
    });
    actions.dedup_by(|left, right| left.kind == right.kind);
    actions.truncate(max_items);
    actions
}

fn token_action_priority(kind: &str) -> Option<usize> {
    match kind {
        "prompt_budget" => Some(1),
        "cache_resend" => Some(2),
        "repeated_raw_read" => Some(3),
        "repeated_command_bundle" => Some(4),
        "restart_loop" => Some(5),
        "noop_closeout" => Some(6),
        _ => None,
    }
}

fn token_action_digest_commands(target: &str) -> Vec<String> {
    vec![
        format!("tsift --envelope session-review {target} --next-context --budget normal"),
        format!("tsift --envelope context-pack {target} --budget normal"),
    ]
}

fn guardrail_rewrite_commands(
    report: &session_review::SessionReviewReport,
    kind: &str,
    target: &str,
    max_items: usize,
) -> Vec<String> {
    let limit = max_items.max(1);
    let mut commands = Vec::new();
    let mut seen = BTreeSet::new();
    match kind {
        "prompt_budget" | "cache_resend" => {
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift --envelope context-pack {target} --budget normal"),
                limit,
            );
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift --envelope session-review {target} --next-context --budget normal"),
                limit,
            );
        }
        "restart_loop" | "noop_closeout" => {
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift --envelope session-review {target} --next-context --budget normal"),
                limit,
            );
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift --envelope context-pack {target} --budget normal"),
                limit,
            );
        }
        _ => {}
    }
    if matches!(kind, "prompt_budget" | "cache_resend") {
        for command in repeated_raw_read_commands(report, limit) {
            push_unique_command(&mut commands, &mut seen, command, limit);
        }
    }
    if matches!(kind, "prompt_budget" | "restart_loop") {
        for command in repeated_command_bundle_rewrite_commands(report, limit) {
            push_unique_command(&mut commands, &mut seen, command, limit);
        }
    }
    commands
}

fn repeated_raw_read_action(
    report: &session_review::SessionReviewReport,
    target: &str,
    max_items: usize,
    max_bytes: usize,
) -> Option<SessionReviewNextTokenAction> {
    if report.file_read_diagnostics.is_empty() {
        return None;
    }
    let rewrite_commands = repeated_raw_read_commands(report, max_items.max(1));
    if rewrite_commands.is_empty() {
        return None;
    }
    Some(SessionReviewNextTokenAction {
        priority: token_action_priority("repeated_raw_read").expect("known token action"),
        kind: "repeated_raw_read".to_string(),
        severity: "warn".to_string(),
        message: truncate_for_budget(
            &format!(
                "repeated raw file reads detected across {} path/range group(s)",
                report.file_read_diagnostics.len()
            ),
            max_bytes,
        ),
        guidance: truncate_for_budget(
            "run the listed tsift rewrite/source-read commands instead of repeating cat/head/tail/sed or raw transcript/log reads",
            max_bytes,
        ),
        compact_command: None,
        restart_command: None,
        digest_commands: token_action_digest_commands(target),
        rewrite_commands,
    })
}

fn repeated_command_bundle_action(
    report: &session_review::SessionReviewReport,
    target: &str,
    max_items: usize,
    max_bytes: usize,
) -> Option<SessionReviewNextTokenAction> {
    let bundle_count = report
        .loop_clusters
        .iter()
        .filter(|cluster| cluster.kind == "command_bundle")
        .count();
    if bundle_count == 0 {
        return None;
    }
    let rewrite_commands = repeated_command_bundle_rewrite_commands(report, max_items.max(1));
    if rewrite_commands.is_empty() {
        return None;
    }
    Some(SessionReviewNextTokenAction {
        priority: token_action_priority("repeated_command_bundle").expect("known token action"),
        kind: "repeated_command_bundle".to_string(),
        severity: "warn".to_string(),
        message: truncate_for_budget(
            &format!("repeated command bundles detected across {bundle_count} cluster(s)"),
            max_bytes,
        ),
        guidance: truncate_for_budget(
            "run the listed tsift rewrite commands before repeating broad searches, raw reads, or verbose test/build commands",
            max_bytes,
        ),
        compact_command: None,
        restart_command: None,
        digest_commands: token_action_digest_commands(target),
        rewrite_commands,
    })
}

fn repeated_raw_read_commands(
    report: &session_review::SessionReviewReport,
    limit: usize,
) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = BTreeSet::new();
    for diagnostic in &report.file_read_diagnostics {
        if let Some(raw_read) = raw_read_command_for_diagnostic(&diagnostic.path, &diagnostic.range)
        {
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift rewrite --run {}", shell_quote(&raw_read)),
                limit,
            );
        }
        for command in &diagnostic.follow_up_commands {
            push_unique_command(
                &mut commands,
                &mut seen,
                normalize_file_read_follow_up_command(command),
                limit,
            );
        }
        if commands.len() >= limit {
            break;
        }
    }
    commands
}

fn repeated_command_bundle_rewrite_commands(
    report: &session_review::SessionReviewReport,
    limit: usize,
) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = BTreeSet::new();
    for cluster in report
        .loop_clusters
        .iter()
        .filter(|cluster| cluster.kind == "command_bundle")
    {
        for command in split_command_bundle_label(&cluster.label) {
            push_unique_command(
                &mut commands,
                &mut seen,
                format!("tsift rewrite --run {}", shell_quote(&command)),
                limit,
            );
        }
        if commands.len() >= limit {
            break;
        }
    }
    commands
}

fn push_unique_command(
    commands: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    command: String,
    limit: usize,
) {
    if commands.len() < limit && seen.insert(command.clone()) {
        commands.push(command);
    }
}

fn normalize_file_read_follow_up_command(command: &str) -> String {
    command
        .strip_prefix("tsift source-read ")
        .map(|rest| format!("tsift --envelope source-read {rest}"))
        .unwrap_or_else(|| command.to_string())
}

fn split_command_bundle_label(label: &str) -> Vec<String> {
    label
        .split(" -> ")
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

fn raw_read_command_for_diagnostic(path: &str, range: &str) -> Option<String> {
    let path = shell_quote(path);
    if range == "full" {
        return Some(format!("cat {path}"));
    }
    if let Some(lines) = range.strip_prefix("head:") {
        return Some(format!("head -n {lines} {path}"));
    }
    if let Some(lines) = range
        .strip_prefix("tail:")
        .or_else(|| range.strip_prefix("window:"))
    {
        return Some(format!("tail -n {lines} {path}"));
    }
    let (start, end) = range.split_once('-')?;
    if start.chars().all(|ch| ch.is_ascii_digit()) && end.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(format!("sed -n {start},{end}p {path}"));
    }
    None
}

pub(crate) fn print_session_review_budget_human(report: &SessionReviewBudgetReport) {
    let latest_total = report
        .latest_session_total_tokens
        .map(format_compact_count)
        .unwrap_or_else(|| "-".to_string());
    let latest_largest_turn = report
        .latest_session_largest_turn_total_tokens
        .map(format_compact_count)
        .unwrap_or_else(|| "-".to_string());
    println!(
        "session-review-budget target:{} kind:{} sessions:{}/{} aggregate_prompt:{} aggregate_cached:{} aggregate_total:{} latest_total:{} latest_largest_turn:{}",
        shell_quote(&report.target),
        report.target_kind,
        report.sessions.len(),
        report.sessions_matched,
        format_compact_count(report.prompt_tokens),
        format_compact_count(report.cached_input_tokens),
        format_compact_count(report.total_tokens),
        latest_total,
        latest_largest_turn
    );
    for session in &report.sessions {
        println!(
            "session {} {} total:{} largest_turn:{} prompts:{} fails:{} expand:{}",
            session.handle,
            session.path,
            format_compact_count(session.total_tokens),
            format_compact_count(session.largest_turn_total_tokens),
            session.prompt_targets,
            session.failures,
            session.expand
        );
    }
    for prompt in &report.prompt_targets {
        println!(
            "prompt {} count:{} {} expand:{}",
            prompt.handle, prompt.occurrences, prompt.text, prompt.expand
        );
    }
    for failure in &report.failures {
        println!(
            "fail {} {} count:{} {}{}{} expand:{}",
            failure.handle,
            failure.kind,
            failure.occurrences,
            failure.message,
            failure
                .command
                .as_ref()
                .map(|command| format!(" command:{command}"))
                .unwrap_or_default(),
            failure
                .session_path
                .as_ref()
                .map(|path| format!(" session:{path}"))
                .unwrap_or_default(),
            failure.expand
        );
    }
    for guardrail in &report.guardrails {
        println!("guardrail {guardrail}");
    }
    for warning in &report.warnings {
        println!("warning {warning}");
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
}

pub(crate) fn print_session_review_next_context_budget_human(
    report: &SessionReviewNextContextBudgetReport,
) {
    println!(
        "next-context-budget target:{} prompts:{}/{} files:{}/{} symbols:{}/{} failures:{}/{}",
        shell_quote(&report.target),
        report.prompt_targets.len(),
        report.prompt_target_total,
        report.touched_files.len(),
        report.touched_file_total,
        report.touched_symbols.len(),
        report.touched_symbol_total,
        report.unresolved_failures.len(),
        report.unresolved_failure_total
    );
    if let Some(health) = &report.prompt_cache_health {
        println!("{}", health.summary_line);
    }
    for prompt in &report.prompt_targets {
        println!("prompt {prompt}");
    }
    for file in &report.touched_files {
        println!("file {file}");
    }
    for symbol in &report.touched_symbols {
        if let Some(symbol_ref) = report
            .touched_symbol_refs
            .iter()
            .find(|entry| entry.name == *symbol)
        {
            println!(
                "symbol {}",
                format_symbol_preview_line(
                    &symbol_ref.handle,
                    &symbol_ref.name,
                    symbol_ref.tag_alias.as_deref()
                )
            );
        } else {
            println!("symbol {symbol}");
        }
    }
    for failure in &report.unresolved_failures {
        println!(
            "fail {} {} count:{} {}{}{} expand:{}",
            failure.handle,
            failure.kind,
            failure.occurrences,
            failure.message,
            failure
                .command
                .as_ref()
                .map(|command| format!(" command:{command}"))
                .unwrap_or_default(),
            failure
                .session_path
                .as_ref()
                .map(|path| format!(" session:{path}"))
                .unwrap_or_default(),
            failure.expand
        );
    }
    for action in &report.next_token_actions {
        println!(
            "token-action {} {} severity:{} {} guidance:{}",
            action.priority, action.kind, action.severity, action.message, action.guidance
        );
        if let Some(command) = &action.compact_command {
            println!("token-action-command {} compact {}", action.kind, command);
        }
        if let Some(command) = &action.restart_command {
            println!("token-action-command {} restart {}", action.kind, command);
        }
        for command in &action.digest_commands {
            println!("token-action-command {} digest {}", action.kind, command);
        }
        for command in &action.rewrite_commands {
            println!("token-action-command {} rewrite {}", action.kind, command);
        }
    }
    if let Some(queue) = &report.agent_doc_queue {
        println!(
            "agent-doc-queue active:{} exchange_tail:{}/{} backlog:{}/{} review:{}/{} presets:{}/{} handles:{}/{}",
            queue.active_queue_prompt.as_deref().unwrap_or("-"),
            queue.live_exchange_tail.len(),
            queue.live_exchange_tail_total,
            queue.backlog_rows.len(),
            queue.backlog_row_total,
            queue.review_rows.len(),
            queue.review_row_total,
            queue.prompt_presets.len(),
            queue.prompt_preset_total,
            queue.expansion_handles.len(),
            queue.expansion_handle_total
        );
        for line in &queue.live_exchange_tail {
            println!("agent-doc-exchange-tail {line}");
        }
        for row in &queue.backlog_rows {
            println!("agent-doc-backlog {row}");
        }
        for row in &queue.review_rows {
            println!("agent-doc-review {row}");
        }
        for preset in &queue.prompt_presets {
            println!("agent-doc-preset {preset}");
        }
        for handle in &queue.expansion_handles {
            println!(
                "agent-doc-expand {} {} {}",
                handle.handle, handle.label, handle.expand
            );
        }
    }
    for command in &report.next_digest_commands {
        println!("next {command}");
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
}
