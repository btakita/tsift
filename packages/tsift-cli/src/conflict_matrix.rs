use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context as AnyhowContext, Result, bail};
use serde::{Deserialize, Serialize};
use substrate::{
    GraphEdge as SubstrateGraphEdge, GraphNode as SubstrateGraphNode, GraphPropertyFilter,
    GraphQueryOptions, GraphStore, SqliteGraphStore, TerseGraphNode as SubstrateTerseGraphNode,
};
use tsift_cache::cycle_packet_cache;
use tsift_digest::diff_digest;
use tsift_quality::lint;
use tsift_resolution as resolution;
use tsift_search::impact;
use tsift_sqlite as substrate;

use crate::context_pack::{ContextPackReport, build_context_pack_report_with_profile};
use crate::output::{OutputFormat, ResponseBudget, ResponseBudgetPreset, ToolEnvelopeSummary};
use crate::{
    CONFLICT_MATRIX_CONTRACT_VERSION, CONFLICT_MATRIX_GRAPH_PREPARATION_CACHE_VERSION,
    CONFLICT_MATRIX_PREPARATION_CACHE_VERSION, GraphDbBackendEvalPhaseTiming, GraphDbEvidenceInput,
    GraphDbEvidenceReport, GraphDbFreshnessReport, WORKER_PROMPT_PACKET_CONTRACT_VERSION,
    content_hash, dedupe_preserve_order, envelope_metric, estimated_tokens_from_bytes,
    graph_db_backend_eval_cached_refresh, graph_db_backend_eval_phase_timing,
    graph_db_backend_eval_timed_phase, graph_db_evidence_report_from_store,
    graph_db_read_recovery_diagnostic, graph_db_resolve_evidence_target, graph_db_scope_arg,
    graph_substrate_db_path, print_json_or_envelope, shell_quote, sqlite_graph_freshness,
    stable_handle, traversal_expand_command, traversal_source_watermark,
    write_traversal_graph_store,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConflictMatrixRisk {
    Low,
    Medium,
    High,
    FailClosed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixOverlap {
    pub(crate) files: Vec<String>,
    pub(crate) symbols: Vec<String>,
    pub(crate) tests: Vec<String>,
    pub(crate) config_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixSourceHandle {
    pub(crate) handle: String,
    pub(crate) file: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) reason: String,
    pub(crate) expand: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixSemanticRef {
    pub(crate) handle: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_symbol: Option<String>,
    pub(crate) expand: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixTokenBudget {
    pub(crate) prompt_estimated_tokens: usize,
    pub(crate) max_prompt_tokens: usize,
    pub(crate) source_window_count: usize,
    pub(crate) source_window_lines: usize,
    pub(crate) max_context_bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixRequiredContext {
    pub(crate) read_only_files: Vec<String>,
    pub(crate) source_handles: Vec<String>,
    pub(crate) worker_context_handles: Vec<String>,
    pub(crate) semantic_handles: Vec<String>,
    pub(crate) expansion_commands: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixGraphHandles {
    pub(crate) target_node_id: String,
    pub(crate) evidence_packet_id: String,
    pub(crate) worker_prompt_packet_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) projection_hash: Option<String>,
    pub(crate) source_handles: Vec<String>,
    pub(crate) worker_context_handles: Vec<String>,
    pub(crate) semantic_handles: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixWorkerFeedback {
    pub(crate) total: usize,
    pub(crate) completed: usize,
    pub(crate) blocked: usize,
    pub(crate) touched_files: Vec<String>,
    pub(crate) expected_tests: Vec<String>,
    pub(crate) follow_up_ids: Vec<String>,
    pub(crate) outcome_history: Vec<String>,
    pub(crate) repeated_blockage: bool,
    pub(crate) stale_expected_tests: Vec<String>,
    pub(crate) follow_up_debt: Vec<String>,
    pub(crate) closure_rank_score: usize,
    pub(crate) closure_rank_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixOwnershipBlock {
    pub(crate) contract_version: String,
    pub(crate) title: String,
    pub(crate) owned_files: Vec<String>,
    pub(crate) owned_symbols: Vec<String>,
    pub(crate) read_only_context: Vec<String>,
    pub(crate) read_only_files: Vec<String>,
    pub(crate) forbidden_files: Vec<String>,
    pub(crate) expected_tests: Vec<String>,
    pub(crate) expansion_commands: Vec<String>,
    pub(crate) token_budget: ConflictMatrixTokenBudget,
    pub(crate) prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixWorkerPromptPacket {
    pub(crate) contract_version: String,
    pub(crate) packet_id: String,
    pub(crate) target: String,
    pub(crate) rank: usize,
    pub(crate) risk: ConflictMatrixRisk,
    pub(crate) previously_completed: bool,
    pub(crate) parallel_safe: bool,
    pub(crate) blocks: Vec<String>,
    pub(crate) blocked_by: Vec<String>,
    pub(crate) required_context: ConflictMatrixRequiredContext,
    pub(crate) graph_handles: ConflictMatrixGraphHandles,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) projection_hash: Option<String>,
    pub(crate) title: String,
    pub(crate) owned_files: Vec<String>,
    pub(crate) owned_symbols: Vec<String>,
    pub(crate) read_only_context: Vec<String>,
    pub(crate) forbidden_files: Vec<String>,
    pub(crate) expected_tests: Vec<String>,
    pub(crate) expansion_commands: Vec<String>,
    pub(crate) token_budget: ConflictMatrixTokenBudget,
    pub(crate) semantic_dispatch_score: usize,
    pub(crate) semantic_dispatch_reasons: Vec<String>,
    pub(crate) worker_feedback: ConflictMatrixWorkerFeedback,
    pub(crate) prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixCandidate {
    pub(crate) rank: usize,
    pub(crate) target: String,
    pub(crate) evidence_packet_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) projection_hash: Option<String>,
    pub(crate) target_node_id: String,
    pub(crate) target_kind: String,
    pub(crate) target_label: String,
    pub(crate) risk: ConflictMatrixRisk,
    pub(crate) previously_completed: bool,
    pub(crate) parallel_safe: bool,
    pub(crate) blocks: Vec<String>,
    pub(crate) blocked_by: Vec<String>,
    pub(crate) required_context: ConflictMatrixRequiredContext,
    pub(crate) graph_handles: ConflictMatrixGraphHandles,
    pub(crate) risk_score: usize,
    pub(crate) risk_reasons: Vec<String>,
    pub(crate) owned_files: Vec<String>,
    pub(crate) owned_symbols: Vec<String>,
    pub(crate) config_files: Vec<String>,
    pub(crate) affected_tests: Vec<String>,
    pub(crate) worker_context: Vec<String>,
    pub(crate) semantic_related: Vec<ConflictMatrixSemanticRef>,
    pub(crate) semantic_dispatch_score: usize,
    pub(crate) semantic_dispatch_reasons: Vec<String>,
    pub(crate) worker_feedback: ConflictMatrixWorkerFeedback,
    pub(crate) source_handles: Vec<ConflictMatrixSourceHandle>,
    pub(crate) worker_context_handles: Vec<String>,
    pub(crate) staged_overlap: ConflictMatrixOverlap,
    pub(crate) ownership: ConflictMatrixOwnershipBlock,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPair {
    pub(crate) left: String,
    pub(crate) right: String,
    pub(crate) risk: ConflictMatrixRisk,
    pub(crate) risk_score: usize,
    pub(crate) shared_files: Vec<String>,
    pub(crate) shared_symbols: Vec<String>,
    pub(crate) shared_tests: Vec<String>,
    pub(crate) shared_config_files: Vec<String>,
    pub(crate) verdict: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixInputSummary {
    pub(crate) graph_db_evidence_targets: Vec<String>,
    pub(crate) evidence_packets: Vec<ConflictMatrixEvidencePacketSummary>,
    pub(crate) shared_preparation: ConflictMatrixSharedPreparationSummary,
    pub(crate) preparation_cache: ConflictMatrixPreparationCacheSummary,
    pub(crate) preparation_timings: Vec<GraphDbBackendEvalPhaseTiming>,
    pub(crate) context_pack_command: String,
    pub(crate) cached_diff_command: String,
    pub(crate) impact_command: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPreparedSourceWindow {
    pub(crate) file: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPreparedContext {
    pub(crate) target: String,
    pub(crate) target_kind: String,
    pub(crate) status_reminders: Vec<String>,
    pub(crate) prompt_targets: Vec<String>,
    pub(crate) touched_files: Vec<String>,
    pub(crate) touched_symbols: Vec<String>,
    pub(crate) files_changed: usize,
    pub(crate) worker_context: Vec<String>,
    pub(crate) source_windows: Vec<ConflictMatrixPreparedSourceWindow>,
}

impl ConflictMatrixPreparedContext {
    fn from_context_pack(context_pack: &ContextPackReport) -> Self {
        Self {
            target: context_pack.target.clone(),
            target_kind: context_pack.target_kind.clone(),
            status_reminders: context_pack.status_reminders.clone(),
            prompt_targets: context_pack.next_context.prompt_targets.clone(),
            touched_files: context_pack.next_context.touched_files.clone(),
            touched_symbols: context_pack.next_context.touched_symbols.clone(),
            files_changed: context_pack.diff_digest.files_changed,
            worker_context: context_pack
                .exploration
                .worker_context
                .iter()
                .map(|worker| worker.summary.clone())
                .collect(),
            source_windows: context_pack
                .exploration
                .source_windows
                .iter()
                .map(|window| ConflictMatrixPreparedSourceWindow {
                    file: window.file.clone(),
                    start: window.start,
                    end: window.end,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixEvidencePacketSummary {
    pub(crate) target: String,
    pub(crate) packet_id: String,
    pub(crate) target_node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) projection_hash: Option<String>,
    pub(crate) replay_command: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixSharedPreparationSummary {
    pub(crate) evidence_cache_status: String,
    pub(crate) graph_nodes: usize,
    pub(crate) graph_edges: usize,
    pub(crate) evidence_packets: usize,
    pub(crate) source_handles: usize,
    pub(crate) worker_context: usize,
    pub(crate) worker_results: usize,
    pub(crate) semantic_rows: usize,
    pub(crate) dispatch_trace_snapshot_nodes: usize,
    pub(crate) dispatch_trace_snapshot_edges: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPreparationCacheSummary {
    pub(crate) version: String,
    pub(crate) key: String,
    pub(crate) status: String,
    pub(crate) source_watermark: String,
    pub(crate) document_watermark: String,
    pub(crate) staged_diff_watermark: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixContextSummary {
    pub(crate) target: String,
    pub(crate) target_kind: String,
    pub(crate) prompt_targets: Vec<String>,
    pub(crate) touched_files: Vec<String>,
    pub(crate) touched_symbols: Vec<String>,
    pub(crate) files_changed: usize,
    pub(crate) worker_context: Vec<String>,
    pub(crate) source_windows: Vec<String>,
    pub(crate) status_reminders: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPerTargetFailClosed {
    pub(crate) target: String,
    pub(crate) previously_completed: bool,
    pub(crate) risk_reasons: Vec<String>,
    pub(crate) owned_files: Vec<String>,
    pub(crate) source_handle_count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixOrchestrationObservability {
    pub(crate) contract_version: String,
    pub(crate) projection_freshness: GraphDbFreshnessReport,
    pub(crate) projection_hashes: Vec<String>,
    pub(crate) evidence_packet_ids: Vec<String>,
    pub(crate) conflict_matrix_decisions: Vec<String>,
    pub(crate) worker_ownership_blocks: Vec<String>,
    pub(crate) follow_up_commands: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixReport {
    pub(crate) contract_version: String,
    pub(crate) root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) can_parallel: bool,
    pub(crate) fail_closed: bool,
    pub(crate) cross_target_parallel_safe: bool,
    pub(crate) per_target_fail_closed: Vec<ConflictMatrixPerTargetFailClosed>,
    pub(crate) inputs: ConflictMatrixInputSummary,
    pub(crate) context_pack: ConflictMatrixContextSummary,
    pub(crate) cached_diff: diff_digest::DiffDigestReport,
    pub(crate) impact: impact::ImpactReport,
    pub(crate) candidates: Vec<ConflictMatrixCandidate>,
    pub(crate) worker_prompt_packets: Vec<ConflictMatrixWorkerPromptPacket>,
    pub(crate) conflicts: Vec<ConflictMatrixPair>,
    pub(crate) orchestration: ConflictMatrixOrchestrationObservability,
    pub(crate) next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn conflict_risk_label(risk: ConflictMatrixRisk) -> &'static str {
    match risk {
        ConflictMatrixRisk::Low => "low",
        ConflictMatrixRisk::Medium => "medium",
        ConflictMatrixRisk::High => "high",
        ConflictMatrixRisk::FailClosed => "fail_closed",
    }
}

pub(crate) fn sorted_set(values: &BTreeSet<String>) -> Vec<String> {
    values.iter().cloned().collect()
}

pub(crate) fn sorted_intersection(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
) -> Vec<String> {
    left.intersection(right).cloned().collect()
}

pub(crate) fn normalize_conflict_target(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|ch: char| matches!(ch, '`' | ',' | ';' | '.'));
    let bracketed = trimmed
        .strip_prefix("[#")
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);
    let normalized = bracketed
        .trim()
        .trim_start_matches('#')
        .trim_matches(|ch: char| matches!(ch, '[' | ']'));
    (!normalized.is_empty()).then(|| normalized.to_string())
}

pub(crate) fn extract_conflict_target_refs(input: &str) -> Vec<String> {
    input
        .split(|ch: char| {
            !(ch.is_ascii_alphanumeric()
                || ch == '#'
                || ch == '_'
                || ch == '-'
                || ch == '['
                || ch == ']')
        })
        .filter_map(|token| {
            let hash = token.find('#')?;
            normalize_conflict_target(&token[hash..])
        })
        .collect()
}

fn conflict_targets_from_context_pack(
    store: &impl GraphStore,
    context_pack: &ConflictMatrixPreparedContext,
) -> Result<Vec<String>> {
    let mut candidates = Vec::new();
    for prompt in &context_pack.prompt_targets {
        candidates.extend(extract_conflict_target_refs(prompt));
    }
    for worker in &context_pack.worker_context {
        candidates.extend(extract_conflict_target_refs(worker));
    }

    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if graph_db_resolve_evidence_target(store, &candidate)?.is_some() {
            targets.push(candidate);
        }
    }
    Ok(targets)
}

pub(crate) fn resolve_conflict_matrix_targets(
    store: &impl GraphStore,
    raw_targets: &[String],
    context_pack: &ConflictMatrixPreparedContext,
) -> Result<Vec<String>> {
    let mut targets = raw_targets
        .iter()
        .filter_map(|target| normalize_conflict_target(target))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        targets = conflict_targets_from_context_pack(store, context_pack)?;
    }

    let mut seen = BTreeSet::new();
    targets.retain(|target| seen.insert(target.clone()));
    if targets.is_empty() {
        bail!(
            "conflict-matrix needs at least one resolvable backlog id, job handle, or graph node id"
        );
    }
    Ok(targets)
}

pub(crate) fn is_planner_config_path(path: &str) -> bool {
    resolution::is_planner_config_path(path)
}

pub(crate) fn conflict_matrix_source_handle(
    node: &SubstrateTerseGraphNode,
) -> Option<ConflictMatrixSourceHandle> {
    let file = node.properties.get("file")?.clone();
    let start = node
        .properties
        .get("start")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    let end = node
        .properties
        .get("end")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(start);
    Some(ConflictMatrixSourceHandle {
        handle: node
            .properties
            .get("handle")
            .cloned()
            .unwrap_or_else(|| node.id.clone()),
        file,
        start,
        end,
        reason: node.properties.get("reason").cloned().unwrap_or_default(),
        expand: node.properties.get("expand").cloned().unwrap_or_default(),
    })
}

pub(crate) fn conflict_matrix_semantic_ref(
    root: &Path,
    node: &SubstrateTerseGraphNode,
) -> ConflictMatrixSemanticRef {
    ConflictMatrixSemanticRef {
        handle: node
            .properties
            .get("handle")
            .cloned()
            .unwrap_or_else(|| node.id.clone()),
        kind: node.kind.clone(),
        label: node.label.clone(),
        source_file: node
            .properties
            .get("source_file")
            .or_else(|| node.properties.get("path"))
            .cloned(),
        source_symbol: node.properties.get("source_symbol").cloned(),
        expand: node
            .properties
            .get("expand")
            .cloned()
            .unwrap_or_else(|| traversal_expand_command(root, &node.id)),
    }
}

#[derive(Clone)]
pub(crate) struct ConflictMatrixGraphIndex {
    pub(crate) symbols_by_file: BTreeMap<String, Vec<String>>,
}

pub(crate) fn conflict_matrix_graph_index(
    graph_nodes: &[SubstrateGraphNode],
) -> ConflictMatrixGraphIndex {
    let mut symbols_by_file = BTreeMap::<String, Vec<String>>::new();
    for node in graph_nodes {
        if node.kind != "symbol" {
            continue;
        }
        if let Some(path) = node.properties.get("path") {
            symbols_by_file
                .entry(path.clone())
                .or_default()
                .push(node.label.clone());
        }
    }
    for symbols in symbols_by_file.values_mut() {
        symbols.sort();
        symbols.dedup();
    }
    ConflictMatrixGraphIndex { symbols_by_file }
}

fn conflict_matrix_symbols_for_files(
    graph_index: &ConflictMatrixGraphIndex,
    files: &BTreeSet<String>,
    target_node: &SubstrateTerseGraphNode,
) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    if target_node.kind == "symbol" {
        symbols.insert(target_node.label.clone());
    }
    for file in files {
        if let Some(file_symbols) = graph_index.symbols_by_file.get(file) {
            symbols.extend(file_symbols.iter().cloned());
        }
    }
    symbols
}

fn conflict_matrix_test_commands(target: &impact::ImpactTestTarget) -> Vec<String> {
    if target.commands.is_empty() {
        vec![target.path.clone()]
    } else {
        target.commands.clone()
    }
}

fn conflict_matrix_affected_tests(
    impact_report: &impact::ImpactReport,
    files: &BTreeSet<String>,
    symbols: &BTreeSet<String>,
    staged_overlap: &ConflictMatrixOverlap,
) -> Vec<String> {
    let mut tests = BTreeSet::new();
    for target in &impact_report.affected_tests {
        let path_match = files.contains(&target.path);
        let symbol_match = target.symbols.iter().any(|symbol| symbols.contains(symbol));
        if path_match || symbol_match {
            tests.extend(conflict_matrix_test_commands(target));
        }
    }

    if tests.is_empty()
        && (!staged_overlap.files.is_empty()
            || !staged_overlap.symbols.is_empty()
            || !staged_overlap.config_files.is_empty())
    {
        for target in &impact_report.affected_tests {
            tests.extend(conflict_matrix_test_commands(target));
        }
    }
    tests.into_iter().collect()
}

fn conflict_matrix_semantic_dispatch_score(
    semantic_related: &[ConflictMatrixSemanticRef],
    files: &BTreeSet<String>,
    symbols: &BTreeSet<String>,
) -> (usize, Vec<String>) {
    let mut score = 0usize;
    let mut reasons = Vec::new();
    for semantic in semantic_related {
        let base = match semantic.kind.as_str() {
            "semantic_concept" => 8,
            "semantic_entity" => 6,
            _ => 3,
        };
        let mut points = base;
        let mut detail = vec![format!("{} {}", semantic.kind, semantic.label)];
        if semantic
            .source_file
            .as_ref()
            .is_some_and(|file| files.contains(file))
        {
            points += 4;
            detail.push("owned file".to_string());
        }
        if semantic
            .source_symbol
            .as_ref()
            .is_some_and(|symbol| symbols.contains(symbol))
        {
            points += 2;
            detail.push("owned symbol".to_string());
        }
        score += points;
        reasons.push(format!("+{points} {}", detail.join(" / ")));
    }
    (score, reasons)
}

fn conflict_matrix_staged_overlap(
    files: &BTreeSet<String>,
    symbols: &BTreeSet<String>,
    cached_diff: &diff_digest::DiffDigestReport,
) -> ConflictMatrixOverlap {
    let staged_files = cached_diff
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let staged_symbols = cached_diff
        .files
        .iter()
        .flat_map(|file| file.touched_symbols.iter().cloned())
        .collect::<BTreeSet<_>>();
    let file_overlap = sorted_intersection(files, &staged_files);
    let symbol_overlap = sorted_intersection(symbols, &staged_symbols);
    let config_files = file_overlap
        .iter()
        .filter(|file| is_planner_config_path(file))
        .cloned()
        .collect::<Vec<_>>();
    ConflictMatrixOverlap {
        files: file_overlap,
        symbols: symbol_overlap,
        tests: Vec::new(),
        config_files,
    }
}

fn graph_node_list_property(node: &SubstrateTerseGraphNode, key: &str) -> Vec<String> {
    node.properties
        .get(key)
        .map(|value| {
            value
                .split([',', ';'])
                .flat_map(|part| part.split("&&"))
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn conflict_matrix_worker_feedback(
    worker_results: &[SubstrateTerseGraphNode],
) -> ConflictMatrixWorkerFeedback {
    let mut touched_files = BTreeSet::new();
    let mut expected_tests = BTreeSet::new();
    let mut follow_up_ids = BTreeSet::new();
    let mut outcome_history = Vec::new();
    let mut completed = 0usize;
    let mut blocked = 0usize;

    let mut results = worker_results.iter().collect::<Vec<_>>();
    results.sort_by(|left, right| {
        left.properties
            .get("line")
            .and_then(|value| value.parse::<i64>().ok())
            .cmp(
                &right
                    .properties
                    .get("line")
                    .and_then(|value| value.parse::<i64>().ok()),
            )
            .then(left.id.cmp(&right.id))
    });

    for node in results {
        let status = node
            .properties
            .get("status")
            .map(String::as_str)
            .unwrap_or("unknown");
        match status {
            "completed" => completed += 1,
            "blocked" => blocked += 1,
            _ => {}
        }
        touched_files.extend(graph_node_list_property(node, "touched_files"));
        expected_tests.extend(graph_node_list_property(node, "expected_tests"));
        follow_up_ids.extend(graph_node_list_property(node, "follow_up_ids"));
        let location = match (node.properties.get("path"), node.properties.get("line")) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.clone(),
            _ => node.id.clone(),
        };
        let detail = node
            .properties
            .get("detail")
            .cloned()
            .unwrap_or_else(|| node.label.clone());
        outcome_history.push(format!("{status} at {location}: {detail}"));
    }

    let repeated_blockage = blocked > 1;
    let warnings = if repeated_blockage {
        vec![format!(
            "repeated blockage observed in {blocked} worker_result rows; inspect outcome_history before redispatch"
        )]
    } else {
        Vec::new()
    };

    ConflictMatrixWorkerFeedback {
        total: worker_results.len(),
        completed,
        blocked,
        touched_files: touched_files.into_iter().collect(),
        expected_tests: expected_tests.into_iter().collect(),
        follow_up_ids: follow_up_ids.into_iter().collect(),
        outcome_history,
        repeated_blockage,
        stale_expected_tests: Vec::new(),
        follow_up_debt: Vec::new(),
        closure_rank_score: 0,
        closure_rank_reasons: Vec::new(),
        warnings,
    }
}

fn feedback_ref_list(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(",")
    }
}

fn stale_expected_tests_for_candidate(candidate: &ConflictMatrixCandidate) -> Vec<String> {
    if candidate.worker_feedback.expected_tests.is_empty() {
        return Vec::new();
    }
    let current_tests = candidate
        .affected_tests
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if current_tests.is_empty() {
        return candidate.worker_feedback.expected_tests.clone();
    }
    candidate
        .worker_feedback
        .expected_tests
        .iter()
        .filter(|test| !current_tests.contains(*test))
        .cloned()
        .collect()
}

fn apply_conflict_matrix_worker_feedback_controls(candidates: &mut [ConflictMatrixCandidate]) {
    for candidate in candidates.iter_mut() {
        let stale_expected_tests = stale_expected_tests_for_candidate(candidate);
        let follow_up_debt = candidate.worker_feedback.follow_up_ids.clone();
        let mut score = 0usize;
        let mut reasons = Vec::new();

        if candidate.worker_feedback.repeated_blockage {
            score += candidate.worker_feedback.blocked.saturating_mul(40);
            reasons.push(format!(
                "repeated blockage: {} blocked worker_result rows",
                candidate.worker_feedback.blocked
            ));
        }
        if !stale_expected_tests.is_empty() {
            score += stale_expected_tests.len().saturating_mul(25);
            let reason = if candidate.affected_tests.is_empty() {
                format!(
                    "stale expected tests: {} no longer match current impact output",
                    feedback_ref_list(&stale_expected_tests)
                )
            } else {
                format!(
                    "stale expected tests: {} not in current impacted tests {}",
                    feedback_ref_list(&stale_expected_tests),
                    feedback_ref_list(&candidate.affected_tests)
                )
            };
            reasons.push(reason.clone());
            candidate.worker_feedback.warnings.push(format!(
                "{reason}; refresh impact or rerun the listed tests before redispatch"
            ));
        }
        if !follow_up_debt.is_empty() {
            score += follow_up_debt.len().saturating_mul(10);
            let reason = format!("follow-up debt: {}", feedback_ref_list(&follow_up_debt));
            reasons.push(reason.clone());
            candidate.worker_feedback.warnings.push(format!(
                "{reason}; include or resolve the referenced backlog ids before closing dispatch"
            ));
        }

        candidate.worker_feedback.stale_expected_tests = stale_expected_tests;
        candidate.worker_feedback.follow_up_debt = follow_up_debt;
        candidate.worker_feedback.closure_rank_score = score;
        candidate.worker_feedback.closure_rank_reasons = reasons;
        candidate.worker_feedback.warnings =
            dedupe_preserve_order(std::mem::take(&mut candidate.worker_feedback.warnings));
    }
}

fn empty_conflict_matrix_ownership(target: &str) -> ConflictMatrixOwnershipBlock {
    ConflictMatrixOwnershipBlock {
        contract_version: WORKER_PROMPT_PACKET_CONTRACT_VERSION.to_string(),
        title: format!("Worker ownership for {target}"),
        owned_files: Vec::new(),
        owned_symbols: Vec::new(),
        read_only_context: Vec::new(),
        read_only_files: Vec::new(),
        forbidden_files: Vec::new(),
        expected_tests: Vec::new(),
        expansion_commands: Vec::new(),
        token_budget: ConflictMatrixTokenBudget::default(),
        prompt: String::new(),
    }
}

pub(crate) fn conflict_matrix_candidate_from_evidence(
    root: &Path,
    evidence: &GraphDbEvidenceReport,
    graph_index: &ConflictMatrixGraphIndex,
    cached_diff: &diff_digest::DiffDigestReport,
    impact_report: &impact::ImpactReport,
) -> ConflictMatrixCandidate {
    let mut files = BTreeSet::new();
    let source_handles = evidence
        .source_handles
        .iter()
        .filter_map(|node| {
            let handle = conflict_matrix_source_handle(node)?;
            files.insert(handle.file.clone());
            Some(handle)
        })
        .collect::<Vec<_>>();
    if matches!(
        evidence.target_node.kind.as_str(),
        "file" | "symbol" | "route"
    ) && let Some(path) = evidence.target_node.properties.get("path")
    {
        files.insert(path.clone());
    }

    let symbols = conflict_matrix_symbols_for_files(graph_index, &files, &evidence.target_node);
    let config_files = files
        .iter()
        .filter(|file| is_planner_config_path(file))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut staged_overlap = conflict_matrix_staged_overlap(&files, &symbols, cached_diff);
    let affected_tests =
        conflict_matrix_affected_tests(impact_report, &files, &symbols, &staged_overlap);
    staged_overlap.tests = affected_tests.clone();
    let mut worker_feedback = conflict_matrix_worker_feedback(&evidence.worker_results);
    let previously_completed = worker_feedback.completed > 0;

    let mut risk_score = 0usize;
    let mut risk_reasons = Vec::new();
    if files.is_empty() && previously_completed {
        worker_feedback.warnings.push(format!(
            "previously completed: {} completed worker_result row(s) exist without source ownership evidence; treating no-owned-files as informational instead of per-target fail-closed",
            worker_feedback.completed
        ));
    } else if files.is_empty() {
        risk_score += 120;
        risk_reasons.push("no source ownership evidence; fail closed before dispatch".to_string());
    }
    if !config_files.is_empty() {
        risk_score += 80 * config_files.len();
        risk_reasons.push("candidate owns config or workflow files".to_string());
    }
    if !staged_overlap.config_files.is_empty() {
        risk_score += 100 * staged_overlap.config_files.len();
        risk_reasons.push("staged diff already touches candidate config files".to_string());
    }
    if !staged_overlap.files.is_empty() {
        risk_score += 70 * staged_overlap.files.len();
        risk_reasons.push("staged diff already touches candidate files".to_string());
    }
    if !staged_overlap.symbols.is_empty() {
        risk_score += 35 * staged_overlap.symbols.len();
        risk_reasons.push("staged diff already touches candidate symbols".to_string());
    }
    if affected_tests.len() > 1 {
        risk_score += affected_tests.len() * 5;
        risk_reasons.push("candidate fans into multiple affected test commands".to_string());
    }
    let risk = if (files.is_empty() && !previously_completed)
        || !staged_overlap.config_files.is_empty()
        || !staged_overlap.files.is_empty()
    {
        ConflictMatrixRisk::FailClosed
    } else if !config_files.is_empty() || !staged_overlap.symbols.is_empty() {
        ConflictMatrixRisk::High
    } else if affected_tests.len() > 1 {
        ConflictMatrixRisk::Medium
    } else {
        ConflictMatrixRisk::Low
    };

    let worker_context = evidence
        .worker_context
        .iter()
        .map(|node| {
            node.properties
                .get("summary")
                .cloned()
                .unwrap_or_else(|| node.label.clone())
        })
        .collect::<Vec<_>>();
    let worker_context_handles = evidence
        .worker_context
        .iter()
        .map(|node| {
            node.properties
                .get("handle")
                .cloned()
                .unwrap_or_else(|| node.id.clone())
        })
        .collect::<Vec<_>>();
    let semantic_related = evidence
        .semantic_related
        .iter()
        .map(|node| conflict_matrix_semantic_ref(root, node))
        .collect::<Vec<_>>();
    let (semantic_dispatch_score, semantic_dispatch_reasons) =
        conflict_matrix_semantic_dispatch_score(&semantic_related, &files, &symbols);

    ConflictMatrixCandidate {
        rank: 0,
        target: evidence.target.clone(),
        evidence_packet_id: evidence.packet_id.clone(),
        projection_hash: evidence.projection_hash.clone(),
        target_node_id: evidence.target_node.id.clone(),
        target_kind: evidence.target_node.kind.clone(),
        target_label: evidence.target_node.label.clone(),
        risk,
        previously_completed,
        parallel_safe: false,
        blocks: Vec::new(),
        blocked_by: Vec::new(),
        required_context: ConflictMatrixRequiredContext::default(),
        graph_handles: ConflictMatrixGraphHandles::default(),
        risk_score,
        risk_reasons,
        owned_files: sorted_set(&files),
        owned_symbols: sorted_set(&symbols),
        config_files: sorted_set(&config_files),
        affected_tests,
        worker_context,
        semantic_related,
        semantic_dispatch_score,
        semantic_dispatch_reasons,
        worker_feedback,
        source_handles,
        worker_context_handles,
        staged_overlap,
        ownership: empty_conflict_matrix_ownership(&evidence.target),
    }
}

fn set_from_vec(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

fn conflict_pair_risk(
    shared_files: &[String],
    shared_symbols: &[String],
    shared_tests: &[String],
    shared_config_files: &[String],
) -> (ConflictMatrixRisk, usize, String) {
    let score = shared_files.len() * 100
        + shared_config_files.len() * 100
        + shared_symbols.len() * 40
        + shared_tests.len() * 10;
    if !shared_files.is_empty() || !shared_config_files.is_empty() {
        (
            ConflictMatrixRisk::FailClosed,
            score,
            "serialize or assign one worker as the sole owner of the shared files".to_string(),
        )
    } else if !shared_symbols.is_empty() {
        (
            ConflictMatrixRisk::High,
            score,
            "split by file or serialize; shared symbols are not safe parallel ownership"
                .to_string(),
        )
    } else if !shared_tests.is_empty() {
        (
            ConflictMatrixRisk::Medium,
            score,
            "parallel work is possible, but keep a shared test gate after merge".to_string(),
        )
    } else {
        (
            ConflictMatrixRisk::Low,
            score,
            "no direct file, symbol, config, or test overlap found".to_string(),
        )
    }
}

fn build_conflict_matrix_pairs(candidates: &[ConflictMatrixCandidate]) -> Vec<ConflictMatrixPair> {
    let mut pairs = Vec::new();
    for left_idx in 0..candidates.len() {
        for right_idx in (left_idx + 1)..candidates.len() {
            let left = &candidates[left_idx];
            let right = &candidates[right_idx];
            let left_files = set_from_vec(&left.owned_files);
            let right_files = set_from_vec(&right.owned_files);
            let left_symbols = set_from_vec(&left.owned_symbols);
            let right_symbols = set_from_vec(&right.owned_symbols);
            let left_tests = set_from_vec(&left.affected_tests);
            let right_tests = set_from_vec(&right.affected_tests);
            let left_config = set_from_vec(&left.config_files);
            let right_config = set_from_vec(&right.config_files);
            let shared_files = sorted_intersection(&left_files, &right_files);
            let shared_symbols = sorted_intersection(&left_symbols, &right_symbols);
            let shared_tests = sorted_intersection(&left_tests, &right_tests);
            let shared_config_files = sorted_intersection(&left_config, &right_config);
            let (risk, risk_score, verdict) = conflict_pair_risk(
                &shared_files,
                &shared_symbols,
                &shared_tests,
                &shared_config_files,
            );
            pairs.push(ConflictMatrixPair {
                left: left.target.clone(),
                right: right.target.clone(),
                risk,
                risk_score,
                shared_files,
                shared_symbols,
                shared_tests,
                shared_config_files,
                verdict,
            });
        }
    }
    pairs.sort_by(|left, right| {
        right
            .risk
            .cmp(&left.risk)
            .then_with(|| right.risk_score.cmp(&left.risk_score))
            .then_with(|| left.left.cmp(&right.left))
            .then_with(|| left.right.cmp(&right.right))
    });
    pairs
}

fn conflict_matrix_per_target_fail_closed(
    candidates: &[ConflictMatrixCandidate],
) -> Vec<ConflictMatrixPerTargetFailClosed> {
    candidates
        .iter()
        .filter(|candidate| candidate.risk == ConflictMatrixRisk::FailClosed)
        .map(|candidate| ConflictMatrixPerTargetFailClosed {
            target: candidate.target.clone(),
            previously_completed: candidate.previously_completed,
            risk_reasons: candidate.risk_reasons.clone(),
            owned_files: candidate.owned_files.clone(),
            source_handle_count: candidate.source_handles.len(),
        })
        .collect()
}

fn markdown_list(values: &[String]) -> String {
    if values.is_empty() {
        return "- none".to_string();
    }
    values
        .iter()
        .map(|value| format!("- {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn conflict_matrix_expansion_commands(candidate: &ConflictMatrixCandidate) -> Vec<String> {
    let mut commands = candidate
        .source_handles
        .iter()
        .filter(|handle| !handle.expand.trim().is_empty())
        .map(|handle| handle.expand.clone())
        .chain(
            candidate
                .semantic_related
                .iter()
                .map(|semantic| semantic.expand.clone()),
        )
        .chain(candidate.affected_tests.iter().cloned())
        .collect::<Vec<_>>();
    if commands.is_empty() {
        commands.push(format!(
            "tsift graph-db evidence {} --depth 3 --limit 8 --json",
            shell_quote(&candidate.target)
        ));
    }
    dedupe_preserve_order(commands)
}

fn conflict_matrix_token_budget(
    prompt: &str,
    source_handles: &[ConflictMatrixSourceHandle],
) -> ConflictMatrixTokenBudget {
    let source_window_lines = source_handles
        .iter()
        .map(|handle| handle.end.saturating_sub(handle.start).saturating_add(1))
        .sum::<usize>();
    let max_context_bytes = source_window_lines.saturating_mul(120).max(prompt.len());
    ConflictMatrixTokenBudget {
        prompt_estimated_tokens: estimated_tokens_from_bytes(prompt.len()),
        max_prompt_tokens: estimated_tokens_from_bytes(max_context_bytes),
        source_window_count: source_handles.len(),
        source_window_lines,
        max_context_bytes,
    }
}

fn conflict_matrix_worker_prompt_packet_id(candidate: &ConflictMatrixCandidate) -> String {
    stable_handle(
        "wpp",
        &format!(
            "{}:{}:{}:{}",
            WORKER_PROMPT_PACKET_CONTRACT_VERSION,
            candidate.target,
            candidate.target_node_id,
            candidate.projection_hash.as_deref().unwrap_or("no-hash")
        ),
    )
}

fn conflict_matrix_required_context(
    candidate: &ConflictMatrixCandidate,
) -> ConflictMatrixRequiredContext {
    ConflictMatrixRequiredContext {
        read_only_files: candidate.ownership.read_only_files.clone(),
        source_handles: candidate
            .source_handles
            .iter()
            .map(|handle| handle.handle.clone())
            .collect(),
        worker_context_handles: candidate.worker_context_handles.clone(),
        semantic_handles: candidate
            .semantic_related
            .iter()
            .map(|semantic| semantic.handle.clone())
            .collect(),
        expansion_commands: candidate.ownership.expansion_commands.clone(),
    }
}

fn conflict_matrix_graph_handles(
    candidate: &ConflictMatrixCandidate,
) -> ConflictMatrixGraphHandles {
    ConflictMatrixGraphHandles {
        target_node_id: candidate.target_node_id.clone(),
        evidence_packet_id: candidate.evidence_packet_id.clone(),
        worker_prompt_packet_id: conflict_matrix_worker_prompt_packet_id(candidate),
        projection_hash: candidate.projection_hash.clone(),
        source_handles: candidate
            .source_handles
            .iter()
            .map(|handle| handle.handle.clone())
            .collect(),
        worker_context_handles: candidate.worker_context_handles.clone(),
        semantic_handles: candidate
            .semantic_related
            .iter()
            .map(|semantic| semantic.handle.clone())
            .collect(),
    }
}

fn apply_conflict_matrix_ownership_blocks(candidates: &mut [ConflictMatrixCandidate]) {
    let all_files_by_target = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.target.clone(),
                candidate
                    .owned_files
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<Vec<_>>();

    for candidate in candidates.iter_mut() {
        let mut read_only = BTreeSet::new();
        for (target, files) in &all_files_by_target {
            if target != &candidate.target {
                read_only.extend(files.iter().cloned());
            }
        }
        let mut forbidden = read_only.clone();
        forbidden.extend(candidate.staged_overlap.files.iter().cloned());
        forbidden.extend(candidate.staged_overlap.config_files.iter().cloned());
        let read_only_files = sorted_set(&read_only);
        let forbidden_files = sorted_set(&forbidden);
        let expected_tests = candidate.affected_tests.clone();
        let mut read_only_context = read_only_files.clone();
        read_only_context.extend(
            candidate
                .worker_context
                .iter()
                .map(|summary| format!("worker_context: {summary}")),
        );
        read_only_context.extend(candidate.semantic_related.iter().map(|semantic| {
            format!(
                "semantic:{}:{}{}",
                semantic.kind,
                semantic.label,
                semantic
                    .source_file
                    .as_ref()
                    .map(|file| format!(" ({file})"))
                    .unwrap_or_default()
            )
        }));
        read_only_context.extend(
            candidate
                .semantic_dispatch_reasons
                .iter()
                .map(|reason| format!("semantic_rank: {reason}")),
        );
        if candidate.worker_feedback.total > 0 {
            read_only_context.push(format!(
                "worker_feedback: completed={} blocked={} touched_files={} expected_tests={} follow_up_ids={}",
                candidate.worker_feedback.completed,
                candidate.worker_feedback.blocked,
                feedback_ref_list(&candidate.worker_feedback.touched_files),
                feedback_ref_list(&candidate.worker_feedback.expected_tests),
                feedback_ref_list(&candidate.worker_feedback.follow_up_ids),
            ));
        }
        if candidate.worker_feedback.closure_rank_score > 0 {
            read_only_context.push(format!(
                "worker_feedback_closure: score={} stale_expected_tests={} follow_up_debt={}",
                candidate.worker_feedback.closure_rank_score,
                feedback_ref_list(&candidate.worker_feedback.stale_expected_tests),
                feedback_ref_list(&candidate.worker_feedback.follow_up_debt),
            ));
        }
        read_only_context.extend(
            candidate
                .worker_feedback
                .warnings
                .iter()
                .map(|warning| format!("worker_feedback_warning: {warning}")),
        );
        read_only_context = dedupe_preserve_order(read_only_context);
        let expansion_commands = conflict_matrix_expansion_commands(candidate);
        let title = format!(
            "Worker {} owns {} ({})",
            candidate.rank, candidate.target, candidate.target_label
        );
        let prompt_body = format!(
            "{title}\n\nOwned files:\n{}\n\nOwned symbols:\n{}\n\nRead-only context:\n{}\n\nForbidden files:\n{}\n\nExpected tests:\n{}\n\nExpansion commands:\n{}\n\nSemantic dispatch score: {}\n{}\n\nFail closed if the task requires a forbidden/shared file, an unowned config file, or a public contract change outside this ownership block.",
            markdown_list(&candidate.owned_files),
            markdown_list(&candidate.owned_symbols),
            markdown_list(&read_only_context),
            markdown_list(&forbidden_files),
            markdown_list(&expected_tests),
            markdown_list(&expansion_commands),
            candidate.semantic_dispatch_score,
            markdown_list(&candidate.semantic_dispatch_reasons),
        );
        let token_budget = conflict_matrix_token_budget(&prompt_body, &candidate.source_handles);
        let prompt = format!(
            "{prompt_body}\n\nToken budget: prompt_estimated_tokens={} max_prompt_tokens={} source_windows={} source_window_lines={} max_context_bytes={}",
            token_budget.prompt_estimated_tokens,
            token_budget.max_prompt_tokens,
            token_budget.source_window_count,
            token_budget.source_window_lines,
            token_budget.max_context_bytes,
        );
        candidate.ownership = ConflictMatrixOwnershipBlock {
            contract_version: WORKER_PROMPT_PACKET_CONTRACT_VERSION.to_string(),
            title,
            owned_files: candidate.owned_files.clone(),
            owned_symbols: candidate.owned_symbols.clone(),
            read_only_context,
            read_only_files,
            forbidden_files,
            expected_tests,
            expansion_commands,
            token_budget,
            prompt,
        };
    }
}

fn conflict_matrix_pair_requires_serial(pair: &ConflictMatrixPair) -> bool {
    matches!(
        pair.risk,
        ConflictMatrixRisk::High | ConflictMatrixRisk::FailClosed
    )
}

fn apply_conflict_matrix_scheduler_fields(
    candidates: &mut [ConflictMatrixCandidate],
    conflicts: &[ConflictMatrixPair],
) {
    let rank_by_target = candidates
        .iter()
        .map(|candidate| (candidate.target.clone(), candidate.rank))
        .collect::<BTreeMap<_, _>>();
    let mut blocks = BTreeMap::<String, BTreeSet<String>>::new();
    let mut blocked_by = BTreeMap::<String, BTreeSet<String>>::new();

    for pair in conflicts {
        if !conflict_matrix_pair_requires_serial(pair) {
            continue;
        }
        let left_rank = rank_by_target
            .get(&pair.left)
            .copied()
            .unwrap_or(usize::MAX);
        let right_rank = rank_by_target
            .get(&pair.right)
            .copied()
            .unwrap_or(usize::MAX);
        let (blocker, blocked) = if left_rank <= right_rank {
            (&pair.left, &pair.right)
        } else {
            (&pair.right, &pair.left)
        };
        blocks
            .entry(blocker.clone())
            .or_default()
            .insert(blocked.clone());
        blocked_by
            .entry(blocked.clone())
            .or_default()
            .insert(blocker.clone());
    }

    for candidate in candidates.iter() {
        for follow_up in &candidate.worker_feedback.follow_up_debt {
            blocks
                .entry(candidate.target.clone())
                .or_default()
                .insert(follow_up.clone());
            if rank_by_target.contains_key(follow_up) {
                blocked_by
                    .entry(follow_up.clone())
                    .or_default()
                    .insert(candidate.target.clone());
            }
        }
    }

    for candidate in candidates.iter_mut() {
        let candidate_blocks: Vec<String> = blocks
            .remove(&candidate.target)
            .map(|values| values.into_iter().collect())
            .unwrap_or_default();
        let candidate_blocked_by: Vec<String> = blocked_by
            .remove(&candidate.target)
            .map(|values| values.into_iter().collect())
            .unwrap_or_default();
        let has_serial_edges = !candidate_blocks.is_empty() || !candidate_blocked_by.is_empty();
        candidate.parallel_safe =
            candidate.risk != ConflictMatrixRisk::FailClosed && !has_serial_edges;
        candidate.blocks = candidate_blocks;
        candidate.blocked_by = candidate_blocked_by;
        candidate.required_context = conflict_matrix_required_context(candidate);
        candidate.graph_handles = conflict_matrix_graph_handles(candidate);
    }
}

fn conflict_matrix_worker_prompt_packets(
    candidates: &[ConflictMatrixCandidate],
) -> Vec<ConflictMatrixWorkerPromptPacket> {
    candidates
        .iter()
        .map(|candidate| ConflictMatrixWorkerPromptPacket {
            contract_version: WORKER_PROMPT_PACKET_CONTRACT_VERSION.to_string(),
            packet_id: conflict_matrix_worker_prompt_packet_id(candidate),
            target: candidate.target.clone(),
            rank: candidate.rank,
            risk: candidate.risk,
            previously_completed: candidate.previously_completed,
            parallel_safe: candidate.parallel_safe,
            blocks: candidate.blocks.clone(),
            blocked_by: candidate.blocked_by.clone(),
            required_context: candidate.required_context.clone(),
            graph_handles: candidate.graph_handles.clone(),
            projection_hash: candidate.projection_hash.clone(),
            title: candidate.ownership.title.clone(),
            owned_files: candidate.ownership.owned_files.clone(),
            owned_symbols: candidate.ownership.owned_symbols.clone(),
            read_only_context: candidate.ownership.read_only_context.clone(),
            forbidden_files: candidate.ownership.forbidden_files.clone(),
            expected_tests: candidate.ownership.expected_tests.clone(),
            expansion_commands: candidate.ownership.expansion_commands.clone(),
            token_budget: candidate.ownership.token_budget.clone(),
            semantic_dispatch_score: candidate.semantic_dispatch_score,
            semantic_dispatch_reasons: candidate.semantic_dispatch_reasons.clone(),
            worker_feedback: candidate.worker_feedback.clone(),
            prompt: candidate.ownership.prompt.clone(),
        })
        .collect()
}

fn conflict_matrix_orchestration_observability(
    freshness: &GraphDbFreshnessReport,
    candidates: &[ConflictMatrixCandidate],
    conflicts: &[ConflictMatrixPair],
    next_commands: &[String],
) -> ConflictMatrixOrchestrationObservability {
    let evidence_packet_ids = candidates
        .iter()
        .map(|candidate| candidate.evidence_packet_id.clone())
        .collect::<Vec<_>>();
    let projection_hashes = candidates
        .iter()
        .filter_map(|candidate| candidate.projection_hash.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut conflict_matrix_decisions = candidates
        .iter()
        .map(|candidate| {
            format!(
                "candidate #{} {} risk={} previously_completed={} closure_score={} semantic_score={} owned_files={} forbidden_files={}",
                candidate.rank,
                candidate.target,
                conflict_risk_label(candidate.risk),
                candidate.previously_completed,
                candidate.worker_feedback.closure_rank_score,
                candidate.semantic_dispatch_score,
                candidate.ownership.owned_files.len(),
                candidate.ownership.forbidden_files.len()
            )
        })
        .collect::<Vec<_>>();
    conflict_matrix_decisions.extend(conflicts.iter().map(|pair| {
        format!(
            "pair {}<->{} risk={} verdict={}",
            pair.left,
            pair.right,
            conflict_risk_label(pair.risk),
            pair.verdict
        )
    }));
    let worker_ownership_blocks = candidates
        .iter()
        .map(|candidate| candidate.ownership.title.clone())
        .collect::<Vec<_>>();
    ConflictMatrixOrchestrationObservability {
        contract_version: CONFLICT_MATRIX_CONTRACT_VERSION.to_string(),
        projection_freshness: freshness.clone(),
        projection_hashes,
        evidence_packet_ids,
        conflict_matrix_decisions,
        worker_ownership_blocks,
        follow_up_commands: next_commands.to_vec(),
    }
}

fn conflict_matrix_context_summary(
    context_pack: &ConflictMatrixPreparedContext,
) -> ConflictMatrixContextSummary {
    ConflictMatrixContextSummary {
        target: context_pack.target.clone(),
        target_kind: context_pack.target_kind.clone(),
        prompt_targets: context_pack.prompt_targets.clone(),
        touched_files: context_pack.touched_files.clone(),
        touched_symbols: context_pack.touched_symbols.clone(),
        files_changed: context_pack.files_changed,
        worker_context: context_pack.worker_context.clone(),
        source_windows: context_pack
            .source_windows
            .iter()
            .map(|window| format!("{}:{}-{}", window.file, window.start, window.end))
            .collect(),
        status_reminders: context_pack.status_reminders.clone(),
    }
}

fn conflict_matrix_next_commands(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    targets: &[String],
    depth: usize,
    limit: usize,
    impact_limit: usize,
) -> Vec<String> {
    let mut commands = Vec::new();
    for target in targets {
        commands.push(format!(
            "tsift graph-db --path {}{} evidence {} --depth {} --limit {} --json",
            shell_quote(root.to_string_lossy().as_ref()),
            graph_db_scope_arg(scope),
            shell_quote(target),
            depth,
            limit
        ));
    }
    commands.push(format!(
        "tsift --envelope context-pack {} --budget normal",
        shell_quote(path.to_string_lossy().as_ref())
    ));
    commands.push(format!(
        "tsift diff-digest --cached {} --json",
        shell_quote(root.to_string_lossy().as_ref())
    ));
    commands.push(format!(
        "tsift impact {} --cached{} --limit {} --json",
        shell_quote(root.to_string_lossy().as_ref()),
        scope
            .map(|scope| format!(" --scope {}", shell_quote(scope)))
            .unwrap_or_default(),
        impact_limit
    ));
    dedupe_preserve_order(commands)
}

fn print_conflict_matrix_human(report: &ConflictMatrixReport, compact: bool) {
    if compact {
        println!(
            "conflict-matrix targets:{} candidates:{} conflicts:{} can_parallel:{} fail_closed:{} cross_safe:{} per_target_fail_closed:{}",
            report.targets.len(),
            report.candidates.len(),
            report.conflicts.len(),
            report.can_parallel,
            report.fail_closed,
            report.cross_target_parallel_safe,
            report.per_target_fail_closed.len()
        );
    } else {
        println!("Conflict matrix");
        println!("  targets:      {}", report.targets.join(", "));
        println!("  can parallel: {}", report.can_parallel);
        println!("  fail closed:  {}", report.fail_closed);
        println!(
            "  cross target parallel safe: {}",
            report.cross_target_parallel_safe
        );
        println!(
            "  per target fail closed: {}",
            report.per_target_fail_closed.len()
        );
    }
    for candidate in &report.candidates {
        println!(
            "candidate #{} {} risk:{} score:{} semantic:{} files:{} symbols:{} tests:{}",
            candidate.rank,
            candidate.target,
            conflict_risk_label(candidate.risk),
            candidate.risk_score,
            candidate.semantic_dispatch_score,
            candidate.owned_files.len(),
            candidate.owned_symbols.len(),
            candidate.affected_tests.len()
        );
        if candidate.previously_completed {
            println!("  previously completed: true");
        }
        for reason in &candidate.risk_reasons {
            println!("  reason: {reason}");
        }
        if candidate.worker_feedback.total > 0 {
            println!(
                "  worker feedback: completed:{} blocked:{} files:{} tests:{} follow-ups:{} closure:{}",
                candidate.worker_feedback.completed,
                candidate.worker_feedback.blocked,
                candidate.worker_feedback.touched_files.len(),
                candidate.worker_feedback.expected_tests.len(),
                candidate.worker_feedback.follow_up_ids.len(),
                candidate.worker_feedback.closure_rank_score
            );
            for reason in &candidate.worker_feedback.closure_rank_reasons {
                println!("  closure: {reason}");
            }
            for warning in &candidate.worker_feedback.warnings {
                println!("  warning: {warning}");
            }
        }
    }
    for pair in &report.conflicts {
        println!(
            "conflict {} <-> {} risk:{} score:{} verdict:{}",
            pair.left,
            pair.right,
            conflict_risk_label(pair.risk),
            pair.risk_score,
            pair.verdict
        );
        for file in &pair.shared_files {
            println!("  shared file: {file}");
        }
        for symbol in &pair.shared_symbols {
            println!("  shared symbol: {symbol}");
        }
    }
    for command in &report.next_commands {
        println!("next: {command}");
    }
    for packet in &report.worker_prompt_packets {
        println!("worker-prompt #{} {}", packet.rank, packet.title);
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    if !report.per_target_fail_closed.is_empty() {
        println!(
            "per-target fail closed: {} target(s)",
            report.per_target_fail_closed.len()
        );
        for target in &report.per_target_fail_closed {
            println!(
                "  {} source_handles:{} owned_files:{} reasons:{}",
                target.target,
                target.source_handle_count,
                target.owned_files.len(),
                target.risk_reasons.join("; ")
            );
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPreparedInputs {
    pub(crate) context_pack: ConflictMatrixPreparedContext,
    pub(crate) cached_diff: diff_digest::DiffDigestReport,
    pub(crate) impact_report: impact::ImpactReport,
    pub(crate) preparation_cache: ConflictMatrixPreparationCacheSummary,
    pub(crate) preparation_timings: Vec<GraphDbBackendEvalPhaseTiming>,
}

pub(crate) struct ConflictMatrixGraphSnapshot {
    pub(crate) nodes: Vec<SubstrateGraphNode>,
    pub(crate) edges: Vec<SubstrateGraphEdge>,
    pub(crate) index: ConflictMatrixGraphIndex,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixPreparedEvidence {
    pub(crate) report: GraphDbEvidenceReport,
    pub(crate) summary: ConflictMatrixEvidencePacketSummary,
}

pub(crate) struct ConflictMatrixGraphPreparedInputs {
    pub(crate) targets: Vec<String>,
    pub(crate) graph: ConflictMatrixGraphSnapshot,
    pub(crate) evidence: Vec<ConflictMatrixPreparedEvidence>,
    pub(crate) shared_preparation: ConflictMatrixSharedPreparationSummary,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ConflictMatrixGraphPreparedCache {
    pub(crate) version: String,
    pub(crate) key: String,
    pub(crate) targets: Vec<String>,
    pub(crate) nodes: Vec<SubstrateGraphNode>,
    pub(crate) edges: Vec<SubstrateGraphEdge>,
    pub(crate) evidence: Vec<ConflictMatrixPreparedEvidence>,
    pub(crate) shared_preparation: ConflictMatrixSharedPreparationSummary,
}

static CONFLICT_MATRIX_PREPARATION_CACHE: OnceLock<
    Mutex<BTreeMap<String, ConflictMatrixPreparedInputs>>,
> = OnceLock::new();

fn conflict_matrix_preparation_cache()
-> &'static Mutex<BTreeMap<String, ConflictMatrixPreparedInputs>> {
    CONFLICT_MATRIX_PREPARATION_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn hash_bytes_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn conflict_matrix_disk_cache_dir(root: &Path) -> PathBuf {
    root.join(".tsift/conflict-matrix-cache")
}

fn conflict_matrix_disk_cache_path(root: &Path, kind: &str, key: &str) -> PathBuf {
    conflict_matrix_disk_cache_dir(root)
        .join(kind)
        .join(format!("{key}.json"))
}

fn conflict_matrix_read_disk_cache<T: for<'de> Deserialize<'de>>(
    root: &Path,
    kind: &str,
    key: &str,
) -> Option<T> {
    let path = conflict_matrix_disk_cache_path(root, kind, key);
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn conflict_matrix_write_disk_cache<T: Serialize>(root: &Path, kind: &str, key: &str, value: &T) {
    let path = conflict_matrix_disk_cache_path(root, kind, key);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = fs::write(path, bytes);
    }
}

fn conflict_matrix_document_watermark(path: &Path) -> Result<String> {
    if path.is_dir() {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        return Ok(hash_bytes_hex(
            format!("directory:{}", canonical.display()).as_bytes(),
        ));
    }
    let bytes = fs::read(path)
        .with_context(|| format!("reading conflict-matrix document {}", path.display()))?;
    Ok(hash_bytes_hex(&bytes))
}

fn conflict_matrix_staged_diff_watermark(root: &Path) -> String {
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--raw", "--no-ext-diff"])
        .output()
    {
        Ok(output) => {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(output.status.to_string().as_bytes());
            bytes.extend_from_slice(&output.stdout);
            bytes.extend_from_slice(&output.stderr);
            hash_bytes_hex(&bytes)
        }
        Err(err) => hash_bytes_hex(format!("git-diff-cached-unavailable:{err:#}").as_bytes()),
    }
}

fn conflict_matrix_preparation_cache_summary(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
) -> Result<ConflictMatrixPreparationCacheSummary> {
    let source_watermark = traversal_source_watermark(root, path, scope, false)?
        .unwrap_or_else(|| "unavailable".to_string());
    let document_watermark = conflict_matrix_document_watermark(path)?;
    let staged_diff_watermark = conflict_matrix_staged_diff_watermark(root);
    let key = content_hash(&vec![
        format!("version:{CONFLICT_MATRIX_PREPARATION_CACHE_VERSION}"),
        format!("root:{}", root.display()),
        format!("path:{}", path.display()),
        format!("scope:{}", scope.unwrap_or("root")),
        format!("source:{source_watermark}"),
        format!("document:{document_watermark}"),
        format!("staged_diff:{staged_diff_watermark}"),
    ])?;
    Ok(ConflictMatrixPreparationCacheSummary {
        version: CONFLICT_MATRIX_PREPARATION_CACHE_VERSION.to_string(),
        key,
        status: "memory_miss".to_string(),
        source_watermark,
        document_watermark,
        staged_diff_watermark,
    })
}

fn conflict_matrix_prepared_inputs_cache_hit(
    mut cached: ConflictMatrixPreparedInputs,
    status: &str,
    duration_micros: u128,
    detail: &str,
) -> ConflictMatrixPreparedInputs {
    cached.preparation_cache.status = status.to_string();
    let cached_detail = format!(
        "reused from {status} conflict-matrix preparation cache by source/document/staged-diff watermark; cost accounted in preparation_cache_lookup"
    );
    cached.preparation_timings = vec![
        graph_db_backend_eval_phase_timing("preparation_cache_lookup", duration_micros, detail),
        graph_db_backend_eval_phase_timing("session_review_compute", 0, &cached_detail),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.target_context_build",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.session_discovery",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.session_digest_total",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.session_cost_total",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.session_aggregation",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "session_review_compute.report_assembly",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing("status_index_gate", 0, &cached_detail),
        graph_db_backend_eval_phase_timing(
            "status_index_gate.prepare_agent_doc_index_gate",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "status_index_gate.context_pack_status_reminders",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing(
            "status_index_gate.load_tag_ontology_preview_context",
            0,
            &cached_detail,
        ),
        graph_db_backend_eval_phase_timing("context_pack_diff", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("exploration_materialization", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("graph_orchestration", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("staged_diff", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.context_resolution", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.diff_digest", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.test_path_scan", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.index_open", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.call_edge_impacts", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.route_handler_impacts", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.import_impacts", 0, &cached_detail),
        graph_db_backend_eval_phase_timing("impact.report_assembly", 0, &cached_detail),
    ];
    cached
}

pub(crate) fn prepare_conflict_matrix_inputs(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    impact_limit: usize,
) -> Result<ConflictMatrixPreparedInputs> {
    let cache_lookup_started = Instant::now();
    let mut cache_summary = conflict_matrix_preparation_cache_summary(root, path, scope)?;
    if let Some(cached) = conflict_matrix_preparation_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("conflict-matrix preparation cache lock poisoned"))?
        .get(&cache_summary.key)
        .cloned()
    {
        return Ok(conflict_matrix_prepared_inputs_cache_hit(
            cached,
            "memory_hit",
            cache_lookup_started.elapsed().as_micros(),
            "reused prepared context-pack, staged diff, and impact packet from memory by source/document/staged-diff watermark",
        ));
    }
    if let Some(cached) = conflict_matrix_read_disk_cache::<ConflictMatrixPreparedInputs>(
        root,
        "inputs",
        &cache_summary.key,
    ) {
        let cached = conflict_matrix_prepared_inputs_cache_hit(
            cached,
            "disk_hit",
            cache_lookup_started.elapsed().as_micros(),
            "reused prepared context-pack, staged diff, and impact packet from .tsift/conflict-matrix-cache by source/document/staged-diff watermark",
        );
        conflict_matrix_preparation_cache()
            .lock()
            .map_err(|_| anyhow::anyhow!("conflict-matrix preparation cache lock poisoned"))?
            .insert(cached.preparation_cache.key.clone(), cached.clone());
        return Ok(cached);
    }

    let mut preparation_timings = vec![graph_db_backend_eval_phase_timing(
        "preparation_cache_lookup",
        cache_lookup_started.elapsed().as_micros(),
        "no prepared packet matched the source/document/staged-diff watermark",
    )];
    cache_summary.status = "computed".to_string();
    let (context_pack_report, context_pack_timings) = build_context_pack_report_with_profile(
        path,
        None,
        None,
        None,
        ResponseBudget::from_cli(None, None, Some(ResponseBudgetPreset::Normal), false),
    )?;
    preparation_timings.extend(context_pack_timings);
    let context_pack = ConflictMatrixPreparedContext::from_context_pack(&context_pack_report);
    let cached_diff = graph_db_backend_eval_timed_phase(
        &mut preparation_timings,
        "staged_diff",
        "cached/staged diff digest used for ownership overlap checks",
        || {
            diff_digest::compute(
                root,
                diff_digest::DiffDigestOptions {
                    cached: true,
                    revision: None,
                    max_parsed_files: None,
                },
            )
            .with_context(|| format!("computing cached diff digest for {}", root.display()))
        },
    )?;
    let impact_started = Instant::now();
    let (impact_report, impact_sub_phases) = impact::compute_with_phases(
        root,
        impact::ImpactOptions {
            cached: true,
            revision: None,
            scope,
            limit: impact_limit,
        },
    )
    .with_context(|| format!("computing cached impact report for {}", root.display()))?;
    let impact_total_micros = impact_started.elapsed().as_micros();
    preparation_timings.push(graph_db_backend_eval_phase_timing(
        "impact",
        impact_total_micros,
        "cached impact analysis used for affected-test ownership checks",
    ));
    for sub in &impact_sub_phases {
        preparation_timings.push(graph_db_backend_eval_phase_timing(
            &format!("impact.{}", sub.name),
            sub.duration_micros,
            &sub.detail,
        ));
    }
    let prepared = ConflictMatrixPreparedInputs {
        context_pack,
        cached_diff,
        impact_report,
        preparation_cache: cache_summary,
        preparation_timings,
    };
    conflict_matrix_preparation_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("conflict-matrix preparation cache lock poisoned"))?
        .insert(prepared.preparation_cache.key.clone(), prepared.clone());
    conflict_matrix_write_disk_cache(root, "inputs", &prepared.preparation_cache.key, &prepared);
    Ok(prepared)
}

fn conflict_matrix_evidence_packet_summary(
    root: &Path,
    scope: Option<&str>,
    target: &str,
    depth: usize,
    limit: usize,
    evidence: &GraphDbEvidenceReport,
) -> ConflictMatrixEvidencePacketSummary {
    ConflictMatrixEvidencePacketSummary {
        target: evidence.target.clone(),
        packet_id: evidence.packet_id.clone(),
        target_node_id: evidence.target_node.id.clone(),
        projection_hash: evidence.projection_hash.clone(),
        replay_command: evidence
            .replay_commands
            .first()
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "tsift graph-db --path {}{} evidence {} --depth {} --limit {} --json",
                    shell_quote(root.to_string_lossy().as_ref()),
                    graph_db_scope_arg(scope),
                    shell_quote(target),
                    depth,
                    limit
                )
            }),
    }
}

pub(crate) fn conflict_matrix_shared_preparation_summary(
    graph: &ConflictMatrixGraphSnapshot,
    evidence: &[ConflictMatrixPreparedEvidence],
    evidence_cache_status: &str,
) -> ConflictMatrixSharedPreparationSummary {
    ConflictMatrixSharedPreparationSummary {
        evidence_cache_status: evidence_cache_status.to_string(),
        graph_nodes: graph.nodes.len(),
        graph_edges: graph.edges.len(),
        evidence_packets: evidence.len(),
        source_handles: evidence
            .iter()
            .map(|entry| entry.report.source_handles.len())
            .sum(),
        worker_context: evidence
            .iter()
            .map(|entry| entry.report.worker_context.len())
            .sum(),
        worker_results: evidence
            .iter()
            .map(|entry| entry.report.worker_results.len())
            .sum(),
        semantic_rows: evidence
            .iter()
            .map(|entry| entry.report.semantic_related.len())
            .sum(),
        dispatch_trace_snapshot_nodes: graph.nodes.len(),
        dispatch_trace_snapshot_edges: graph.edges.len(),
    }
}

#[allow(dead_code)]
fn conflict_matrix_graph_snapshot(store: &impl GraphStore) -> Result<ConflictMatrixGraphSnapshot> {
    let nodes = store.all_nodes()?;
    let edges = store.all_edges()?;
    let index = conflict_matrix_graph_index(&nodes);
    Ok(ConflictMatrixGraphSnapshot {
        nodes,
        edges,
        index,
    })
}

fn insert_conflict_graph_node(
    nodes: &mut BTreeMap<String, SubstrateGraphNode>,
    node: SubstrateGraphNode,
) {
    nodes.entry(node.id.clone()).or_insert(node);
}

fn insert_conflict_graph_edge(
    edges: &mut BTreeMap<(String, String, String), SubstrateGraphEdge>,
    edge: SubstrateGraphEdge,
) {
    edges
        .entry((edge.from_id.clone(), edge.kind.clone(), edge.to_id.clone()))
        .or_insert(edge);
}

fn conflict_matrix_files_from_evidence(evidence: &GraphDbEvidenceReport) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    if matches!(
        evidence.target_node.kind.as_str(),
        "file" | "symbol" | "route"
    ) && let Some(path) = evidence.target_node.properties.get("path")
    {
        files.insert(path.clone());
    }
    for node in &evidence.source_handles {
        if let Some(handle) = conflict_matrix_source_handle(node) {
            files.insert(handle.file);
        }
    }
    files
}

fn conflict_matrix_add_path_nodes<S: GraphStore>(
    store: &S,
    nodes: &mut BTreeMap<String, SubstrateGraphNode>,
    evidence: &GraphDbEvidenceReport,
) -> Result<()> {
    for path in &evidence.shortest_paths {
        let Some(graph_path) = &path.path else {
            continue;
        };
        for id in &graph_path.nodes {
            if nodes.contains_key(id) {
                continue;
            }
            if let Some(node) = store.node(id)? {
                insert_conflict_graph_node(nodes, node);
            }
        }
    }
    Ok(())
}

fn conflict_matrix_add_file_symbol_nodes<S: GraphStore>(
    store: &S,
    nodes: &mut BTreeMap<String, SubstrateGraphNode>,
    files: &BTreeSet<String>,
) -> Result<()> {
    for file in files {
        for kind in ["file", "route", "symbol"] {
            let page = store.paged_nodes_by_kind(
                kind,
                GraphQueryOptions {
                    property_filters: vec![GraphPropertyFilter {
                        key: "path".to_string(),
                        value: file.clone(),
                    }],
                    ..GraphQueryOptions::default()
                },
            )?;
            for node in page.nodes {
                insert_conflict_graph_node(nodes, node);
            }
        }
    }
    Ok(())
}

fn conflict_matrix_add_target_ref_nodes<S: GraphStore>(
    store: &S,
    nodes: &mut BTreeMap<String, SubstrateGraphNode>,
    target_node: &SubstrateTerseGraphNode,
) -> Result<()> {
    let Some(ref_id) = target_node.properties.get("ref_id") else {
        return Ok(());
    };
    for kind in ["backlog", "job_packet", "worker_result"] {
        let page = store.paged_nodes_by_kind(
            kind,
            GraphQueryOptions {
                property_filters: vec![GraphPropertyFilter {
                    key: "ref_id".to_string(),
                    value: ref_id.clone(),
                }],
                ..GraphQueryOptions::default()
            },
        )?;
        for node in page.nodes {
            insert_conflict_graph_node(nodes, node);
        }
    }
    Ok(())
}

fn conflict_matrix_add_target_neighborhood<S: GraphStore>(
    store: &S,
    nodes: &mut BTreeMap<String, SubstrateGraphNode>,
    edges: &mut BTreeMap<(String, String, String), SubstrateGraphEdge>,
    target_node: &SubstrateTerseGraphNode,
    depth: usize,
    limit: usize,
) -> Result<()> {
    let node_limit = if limit == 0 {
        None
    } else {
        Some(limit.saturating_mul(depth.max(1)).saturating_mul(8).max(64))
    };
    if let Some(page) = store.paged_neighborhood(
        &target_node.id,
        depth,
        None,
        GraphQueryOptions {
            limit: node_limit,
            ..GraphQueryOptions::default()
        },
    )? {
        for node in page.nodes {
            insert_conflict_graph_node(nodes, node);
        }
        for edge in page.edges {
            insert_conflict_graph_edge(edges, edge);
        }
    }
    Ok(())
}

fn conflict_matrix_add_scoped_edges<S: GraphStore>(
    store: &S,
    nodes: &BTreeMap<String, SubstrateGraphNode>,
    edges: &mut BTreeMap<(String, String, String), SubstrateGraphEdge>,
) -> Result<()> {
    let node_ids = nodes.keys().cloned().collect::<BTreeSet<_>>();
    for edge in store.edges_between_nodes(&node_ids)? {
        insert_conflict_graph_edge(edges, edge);
    }
    Ok(())
}

pub(crate) fn conflict_matrix_target_scoped_graph_snapshot<S: GraphStore>(
    store: &S,
    evidence: &[ConflictMatrixPreparedEvidence],
    depth: usize,
    limit: usize,
) -> Result<ConflictMatrixGraphSnapshot> {
    let mut nodes = BTreeMap::<String, SubstrateGraphNode>::new();
    let mut edges = BTreeMap::<(String, String, String), SubstrateGraphEdge>::new();
    let mut files = BTreeSet::new();

    for prepared in evidence {
        let report = &prepared.report;
        insert_conflict_graph_node(&mut nodes, report.target_node.clone().into());
        for node in report
            .worker_context
            .iter()
            .chain(report.source_handles.iter())
            .chain(report.worker_results.iter())
            .chain(report.semantic_related.iter())
        {
            insert_conflict_graph_node(&mut nodes, node.clone().into());
        }
        files.extend(conflict_matrix_files_from_evidence(report));
        conflict_matrix_add_target_ref_nodes(store, &mut nodes, &report.target_node)?;
        conflict_matrix_add_path_nodes(store, &mut nodes, report)?;
        conflict_matrix_add_target_neighborhood(
            store,
            &mut nodes,
            &mut edges,
            &report.target_node,
            depth,
            limit,
        )?;
    }

    conflict_matrix_add_file_symbol_nodes(store, &mut nodes, &files)?;
    conflict_matrix_add_scoped_edges(store, &nodes, &mut edges)?;

    let nodes = nodes.into_values().collect::<Vec<_>>();
    let edges = edges.into_values().collect::<Vec<_>>();
    let index = conflict_matrix_graph_index(&nodes);
    Ok(ConflictMatrixGraphSnapshot {
        nodes,
        edges,
        index,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_evidence_for_target<S: GraphStore>(
    root: &Path,
    scope: Option<&str>,
    backend: &str,
    target: &str,
    depth: usize,
    limit: usize,
    store: &S,
    freshness: GraphDbFreshnessReport,
) -> Result<ConflictMatrixPreparedEvidence> {
    let cache_key = cycle_packet_cache::cycle_packet_evidence_key(target);
    if let Some(cached) =
        cycle_packet_cache::cycle_packet_read_cache::<ConflictMatrixPreparedEvidence>(
            root,
            cycle_packet_cache::CyclePacketKind::Evidence,
            &cache_key,
        )
    {
        return Ok(cached);
    }
    let report = graph_db_evidence_report_from_store(GraphDbEvidenceInput {
        root,
        scope,
        backend,
        target,
        preferred_path: None,
        depth,
        limit,
        cursor: None,
        store,
        freshness,
        warnings: Vec::new(),
    })
    .with_context(|| format!("collecting graph-db evidence for {target}"))?;
    let summary =
        conflict_matrix_evidence_packet_summary(root, scope, target, depth, limit, &report);
    let prepared = ConflictMatrixPreparedEvidence {
        report,
        summary: summary.clone(),
    };
    cycle_packet_cache::cycle_packet_write_cache(
        root,
        cycle_packet_cache::CyclePacketKind::Evidence,
        &cache_key,
        &prepared,
    );
    Ok(prepared)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_conflict_matrix_evidence_packets<S: GraphStore>(
    root: &Path,
    scope: Option<&str>,
    backend: &str,
    targets: &[String],
    depth: usize,
    limit: usize,
    store: &S,
    freshness: GraphDbFreshnessReport,
) -> Result<Vec<ConflictMatrixPreparedEvidence>> {
    let db_path = graph_substrate_db_path(root, scope);
    let db_path_exists = db_path.exists();
    let mut evidence: Vec<Option<ConflictMatrixPreparedEvidence>> = vec![None; targets.len()];
    let mut miss_indices: Vec<usize> = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        let cache_key = cycle_packet_cache::cycle_packet_evidence_key(target);
        if let Some(cached) =
            cycle_packet_cache::cycle_packet_read_cache::<ConflictMatrixPreparedEvidence>(
                root,
                cycle_packet_cache::CyclePacketKind::Evidence,
                &cache_key,
            )
        {
            evidence[i] = Some(cached);
        } else {
            miss_indices.push(i);
        }
    }
    if miss_indices.is_empty() {
        return Ok(evidence.into_iter().map(|e| e.unwrap()).collect());
    }
    if db_path_exists && miss_indices.len() > 1 {
        std::thread::scope(|s| {
            let handles: Vec<_> = miss_indices
                .iter()
                .map(|&i| {
                    let target = targets[i].clone();
                    let root_owned = root.to_path_buf();
                    let scope_owned = scope.map(String::from);
                    let backend_owned = backend.to_string();
                    let freshness_clone = freshness.clone();
                    let db_path_clone = db_path.clone();
                    s.spawn(move || {
                        let ro_store = SqliteGraphStore::open_read_only(&db_path_clone)?;
                        collect_evidence_for_target(
                            &root_owned,
                            scope_owned.as_deref(),
                            &backend_owned,
                            &target,
                            depth,
                            limit,
                            &ro_store,
                            freshness_clone,
                        )
                        .map(|e| (i, e))
                    })
                })
                .collect();
            for handle in handles {
                let (i, prepared) = handle
                    .join()
                    .expect("evidence collection thread panicked")
                    .with_context(|| "parallel evidence collection failed")?;
                evidence[i] = Some(prepared);
            }
            Ok::<(), anyhow::Error>(())
        })?;
    } else {
        for i in &miss_indices {
            let target = &targets[*i];
            let prepared = collect_evidence_for_target(
                root,
                scope,
                backend,
                target,
                depth,
                limit,
                store,
                freshness.clone(),
            )?;
            evidence[*i] = Some(prepared);
        }
    }
    Ok(evidence.into_iter().map(|e| e.unwrap()).collect())
}

fn conflict_matrix_graph_preparation_cache_key(
    prepared: &ConflictMatrixPreparedInputs,
    scope: Option<&str>,
    backend: &str,
    targets: &[String],
    depth: usize,
    limit: usize,
    freshness: &GraphDbFreshnessReport,
) -> Result<String> {
    content_hash(&serde_json::json!({
        "version": CONFLICT_MATRIX_GRAPH_PREPARATION_CACHE_VERSION,
        "prepared_inputs_key": prepared.preparation_cache.key.as_str(),
        "scope": scope.unwrap_or("root"),
        "backend": backend,
        "targets": targets,
        "depth": depth,
        "limit": limit,
        "projection_version": freshness.projection_version.as_deref(),
        "projection_hash": freshness.content_hash.as_deref(),
        "source_watermark": freshness.source_watermark.as_deref(),
    }))
}

fn conflict_matrix_graph_prepared_cache_hit(
    cached: ConflictMatrixGraphPreparedCache,
    status: &str,
) -> ConflictMatrixGraphPreparedInputs {
    let mut shared_preparation = cached.shared_preparation;
    shared_preparation.evidence_cache_status = status.to_string();
    let index = conflict_matrix_graph_index(&cached.nodes);
    ConflictMatrixGraphPreparedInputs {
        targets: cached.targets,
        graph: ConflictMatrixGraphSnapshot {
            nodes: cached.nodes,
            edges: cached.edges,
            index,
        },
        evidence: cached.evidence,
        shared_preparation,
    }
}

fn conflict_matrix_graph_prepared_cache_from_inputs(
    key: &str,
    prepared: &ConflictMatrixGraphPreparedInputs,
) -> ConflictMatrixGraphPreparedCache {
    ConflictMatrixGraphPreparedCache {
        version: CONFLICT_MATRIX_GRAPH_PREPARATION_CACHE_VERSION.to_string(),
        key: key.to_string(),
        targets: prepared.targets.clone(),
        nodes: prepared.graph.nodes.clone(),
        edges: prepared.graph.edges.clone(),
        evidence: prepared.evidence.clone(),
        shared_preparation: prepared.shared_preparation.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_conflict_matrix_graph_orchestration<S: GraphStore>(
    root: &Path,
    scope: Option<&str>,
    backend: &str,
    raw_targets: &[String],
    prepared: &ConflictMatrixPreparedInputs,
    depth: usize,
    limit: usize,
    store: &S,
    freshness: GraphDbFreshnessReport,
) -> Result<ConflictMatrixGraphPreparedInputs> {
    let targets = resolve_conflict_matrix_targets(store, raw_targets, &prepared.context_pack)?;
    let graph_cache_key = conflict_matrix_graph_preparation_cache_key(
        prepared, scope, backend, &targets, depth, limit, &freshness,
    )?;
    if let Some(cached) = conflict_matrix_read_disk_cache::<ConflictMatrixGraphPreparedCache>(
        root,
        "graph",
        &graph_cache_key,
    ) && cached.version == CONFLICT_MATRIX_GRAPH_PREPARATION_CACHE_VERSION
        && cached.key == graph_cache_key
        && cached.targets == targets
    {
        return Ok(conflict_matrix_graph_prepared_cache_hit(cached, "disk_hit"));
    }
    let evidence = collect_conflict_matrix_evidence_packets(
        root, scope, backend, &targets, depth, limit, store, freshness,
    )?;
    let graph = conflict_matrix_target_scoped_graph_snapshot(store, &evidence, depth, limit)?;
    let shared_preparation =
        conflict_matrix_shared_preparation_summary(&graph, &evidence, "computed");

    let prepared_graph = ConflictMatrixGraphPreparedInputs {
        targets,
        graph,
        evidence,
        shared_preparation,
    };
    let cache = conflict_matrix_graph_prepared_cache_from_inputs(&graph_cache_key, &prepared_graph);
    conflict_matrix_write_disk_cache(root, "graph", &graph_cache_key, &cache);
    Ok(prepared_graph)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_conflict_matrix_report_from_prepared_graph(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    depth: usize,
    limit: usize,
    impact_limit: usize,
    freshness: GraphDbFreshnessReport,
    extra_warnings: Vec<String>,
    prepared: &ConflictMatrixPreparedInputs,
    graph_prepared: &ConflictMatrixGraphPreparedInputs,
) -> Result<ConflictMatrixReport> {
    let context_pack = &prepared.context_pack;
    let targets = graph_prepared.targets.clone();
    let graph_index = &graph_prepared.graph.index;

    let mut warnings = context_pack.status_reminders.clone();
    warnings.extend(extra_warnings);
    let mut candidates = Vec::new();
    let mut evidence_packets = Vec::new();
    for prepared_evidence in &graph_prepared.evidence {
        let evidence = &prepared_evidence.report;
        warnings.extend(evidence.warnings.clone());
        evidence_packets.push(prepared_evidence.summary.clone());
        candidates.push(conflict_matrix_candidate_from_evidence(
            root,
            evidence,
            graph_index,
            &prepared.cached_diff,
            &prepared.impact_report,
        ));
    }

    apply_conflict_matrix_worker_feedback_controls(&mut candidates);
    candidates.sort_by(|left, right| {
        left.risk
            .cmp(&right.risk)
            .then_with(|| left.risk_score.cmp(&right.risk_score))
            .then_with(|| {
                right
                    .worker_feedback
                    .closure_rank_score
                    .cmp(&left.worker_feedback.closure_rank_score)
            })
            .then_with(|| {
                right
                    .semantic_dispatch_score
                    .cmp(&left.semantic_dispatch_score)
            })
            .then_with(|| left.target.cmp(&right.target))
    });
    for (idx, candidate) in candidates.iter_mut().enumerate() {
        candidate.rank = idx + 1;
    }
    warnings.extend(candidates.iter().flat_map(|candidate| {
        candidate
            .worker_feedback
            .warnings
            .iter()
            .map(|warning| format!("{}: {warning}", candidate.target))
    }));
    let conflicts = build_conflict_matrix_pairs(&candidates);
    apply_conflict_matrix_ownership_blocks(&mut candidates);
    apply_conflict_matrix_scheduler_fields(&mut candidates, &conflicts);
    let worker_prompt_packets = conflict_matrix_worker_prompt_packets(&candidates);

    let per_target_fail_closed = conflict_matrix_per_target_fail_closed(&candidates);
    let cross_target_parallel_safe = conflicts
        .iter()
        .all(|pair| pair.risk <= ConflictMatrixRisk::Medium);
    let fail_closed = !per_target_fail_closed.is_empty()
        || conflicts
            .iter()
            .any(|pair| pair.risk == ConflictMatrixRisk::FailClosed);
    let can_parallel = !fail_closed && cross_target_parallel_safe;
    let next_commands =
        conflict_matrix_next_commands(root, path, scope, &targets, depth, limit, impact_limit);
    let orchestration = conflict_matrix_orchestration_observability(
        &freshness,
        &candidates,
        &conflicts,
        &next_commands,
    );
    let inputs = ConflictMatrixInputSummary {
        graph_db_evidence_targets: targets.clone(),
        evidence_packets,
        shared_preparation: graph_prepared.shared_preparation.clone(),
        preparation_cache: prepared.preparation_cache.clone(),
        preparation_timings: prepared.preparation_timings.clone(),
        context_pack_command: format!(
            "tsift --envelope context-pack {} --budget normal",
            shell_quote(path.to_string_lossy().as_ref())
        ),
        cached_diff_command: format!(
            "tsift diff-digest --cached {} --json",
            shell_quote(root.to_string_lossy().as_ref())
        ),
        impact_command: format!(
            "tsift impact {} --cached{} --limit {} --json",
            shell_quote(root.to_string_lossy().as_ref()),
            scope
                .map(|scope| format!(" --scope {}", shell_quote(scope)))
                .unwrap_or_default(),
            impact_limit
        ),
    };
    let context_summary = conflict_matrix_context_summary(context_pack);
    Ok(ConflictMatrixReport {
        contract_version: CONFLICT_MATRIX_CONTRACT_VERSION.to_string(),
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        targets,
        can_parallel,
        fail_closed,
        cross_target_parallel_safe,
        per_target_fail_closed,
        inputs,
        context_pack: context_summary,
        cached_diff: prepared.cached_diff.clone(),
        impact: prepared.impact_report.clone(),
        candidates,
        worker_prompt_packets,
        conflicts,
        orchestration,
        next_commands,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_conflict_matrix_report_with_prepared<S: GraphStore>(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    raw_targets: &[String],
    depth: usize,
    limit: usize,
    impact_limit: usize,
    store: &S,
    freshness: GraphDbFreshnessReport,
    extra_warnings: Vec<String>,
    prepared: &ConflictMatrixPreparedInputs,
) -> Result<ConflictMatrixReport> {
    let graph_prepared = prepare_conflict_matrix_graph_orchestration(
        root,
        scope,
        "sqlite",
        raw_targets,
        prepared,
        depth,
        limit,
        store,
        freshness.clone(),
    )?;
    build_conflict_matrix_report_from_prepared_graph(
        root,
        path,
        scope,
        depth,
        limit,
        impact_limit,
        freshness,
        extra_warnings,
        prepared,
        &graph_prepared,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_conflict_matrix_report_with_store<S: GraphStore>(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    raw_targets: &[String],
    depth: usize,
    limit: usize,
    impact_limit: usize,
    store: &S,
    freshness: GraphDbFreshnessReport,
    extra_warnings: Vec<String>,
) -> Result<ConflictMatrixReport> {
    let prepared = prepare_conflict_matrix_inputs(root, path, scope, impact_limit)?;
    build_conflict_matrix_report_with_prepared(
        root,
        path,
        scope,
        raw_targets,
        depth,
        limit,
        impact_limit,
        store,
        freshness,
        extra_warnings,
        &prepared,
    )
}

pub(crate) fn build_conflict_matrix_report(
    path: &Path,
    scope: Option<&str>,
    raw_targets: &[String],
    depth: usize,
    limit: usize,
    impact_limit: usize,
) -> Result<ConflictMatrixReport> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let source_watermark = traversal_source_watermark(&root, path, scope, false)?;
    if graph_db_backend_eval_cached_refresh(&root, scope, source_watermark.as_deref())?.is_none() {
        write_traversal_graph_store(&root, path, scope)
            .with_context(|| format!("refreshing graph-db projection for {}", root.display()))?;
    }
    let graph_db = graph_substrate_db_path(&root, scope);
    let store = SqliteGraphStore::open_read_only_resilient(&graph_db)
        .with_context(|| format!("opening graph-db projection: {}", graph_db.display()))?;
    let freshness = sqlite_graph_freshness(&store, scope.unwrap_or("root"))?;
    let mut warnings = Vec::new();
    if let Some(recovery) = store.read_only_recovery() {
        warnings.push(graph_db_read_recovery_diagnostic(recovery));
    }
    build_conflict_matrix_report_with_store(
        &root,
        path,
        scope,
        raw_targets,
        depth,
        limit,
        impact_limit,
        &store,
        freshness,
        warnings,
    )
}

pub(crate) fn cmd_conflict_matrix(
    path: &Path,
    scope: Option<&str>,
    raw_targets: &[String],
    depth: usize,
    limit: usize,
    impact_limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let report =
        build_conflict_matrix_report(path, scope, raw_targets, depth, limit, impact_limit)?;
    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "conflict-matrix",
            "parallel-planning",
            ToolEnvelopeSummary {
                text: format!(
                    "Conflict matrix for {} target(s): can_parallel={} fail_closed={} cross_target_parallel_safe={} per_target_fail_closed={}",
                    report.targets.len(),
                    report.can_parallel,
                    report.fail_closed,
                    report.cross_target_parallel_safe,
                    report.per_target_fail_closed.len()
                ),
                metrics: vec![
                    envelope_metric("targets", report.targets.len()),
                    envelope_metric("candidates", report.candidates.len()),
                    envelope_metric("conflicts", report.conflicts.len()),
                    envelope_metric("can_parallel", report.can_parallel),
                    envelope_metric("fail_closed", report.fail_closed),
                    envelope_metric(
                        "cross_target_parallel_safe",
                        report.cross_target_parallel_safe,
                    ),
                    envelope_metric(
                        "per_target_fail_closed",
                        report.per_target_fail_closed.len(),
                    ),
                ],
            },
            report.fail_closed,
            report.next_commands.clone(),
        )
    } else {
        print_conflict_matrix_human(&report, format.compact);
        Ok(())
    }
}
