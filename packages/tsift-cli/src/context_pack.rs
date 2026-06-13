use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use substrate::{
    GraphEdge as SubstrateGraphEdge, GraphNode as SubstrateGraphNode, GraphProjection,
    GraphProvenance, GraphStore, SqliteGraphStore,
};
use tsift_agent_doc::session_review;
use tsift_digest::{diff_digest, log_digest, test_digest};
use tsift_index::index;
use tsift_sqlite as substrate;
use tsift_status::status;

use crate::output::ResponseBudget;
use crate::session_review_budget::{
    SessionReviewNextContextBudgetReport, build_session_review_next_context_budget_report,
};
use crate::{
    CONTEXT_PACK_GRAPH_ORCHESTRATION_CONTRACT_VERSION, CompactOntologyRefPreview,
    CompactSymbolRefPreview, ExplorationBudget, ExplorationPacket, ExplorationRelation,
    ExplorationSourceWindow, ExplorationWorkerContext, GraphDbBackendEvalPhaseTiming,
    GraphDbFreshnessReport, GraphEffectivenessReadiness, TagOntologyPreviewContext,
    build_compact_symbol_ref_with_ontology, compact_symbol_ref_token, dedupe_preserve_order,
    diff_digest_mode_label, diff_digest_status_label, diff_digest_summary_label,
    edge_with_content_freshness, exploration_budget_for_counts, extract_conflict_target_refs,
    format_summary_ref_line, format_symbol_preview_line, graph_db_backend_eval_phase_timing,
    graph_db_backend_eval_timed_phase, graph_db_evidence_packet_id,
    graph_db_read_recovery_diagnostic, graph_db_resolve_evidence_target,
    graph_db_semantic_readiness, graph_store_semantic_node_count, graph_substrate_db_path,
    load_tag_ontology_preview_context, log_digest_summary_label, node_with_content_freshness,
    ontology_refs_for_alias, prepare_agent_doc_index_gate_cached, shell_quote, source_read_command,
    sqlite_graph_freshness, stable_handle, tag_alias_from_name, test_digest_summary_label,
    truncate_for_budget,
};

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackGraphOrchestration {
    pub(crate) contract_version: &'static str,
    pub(crate) graph_db_command: String,
    pub(crate) projection_freshness: GraphDbFreshnessReport,
    pub(crate) readiness: GraphEffectivenessReadiness,
    pub(crate) projection_hashes: Vec<String>,
    pub(crate) evidence_packet_ids: Vec<String>,
    pub(crate) conflict_matrix_decisions: Vec<String>,
    pub(crate) worker_ownership_blocks: Vec<String>,
    pub(crate) follow_up_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackReport {
    pub(crate) root: String,
    pub(crate) target: String,
    pub(crate) target_kind: String,
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) status_reminders: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) ontology_refs: Vec<CompactOntologyRefPreview>,
    pub(crate) next_context: SessionReviewNextContextBudgetReport,
    pub(crate) diff_digest: ContextPackDiffPreview,
    pub(crate) test_digest: ContextPackOptionalSection<ContextPackTestPreview>,
    pub(crate) log_digest: ContextPackOptionalSection<ContextPackLogPreview>,
    pub(crate) exploration: ExplorationPacket,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) findings: Vec<ContextPackFindingPreview>,
    pub(crate) graph_orchestration: ContextPackGraphOrchestration,
    pub(crate) resume_commands: Vec<String>,
}

/// A trusted, fresh authored finding folded into the context-pack envelope
/// because it `concerns` a symbol/file already in the result set (#trt1p2
/// hot-path injection). The agent gets the authored "why" without reading a
/// separate document; `expand` resolves the full finding set for the anchor.
#[derive(Clone, Serialize)]
pub(crate) struct ContextPackFindingPreview {
    pub(crate) handle: String,
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) about: String,
    pub(crate) anchor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<f64>,
    pub(crate) body: String,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackOptionalSection<T> {
    pub(crate) status: String,
    pub(crate) command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) report: Option<T>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackDiffPreview {
    pub(crate) mode: String,
    pub(crate) files_changed: usize,
    pub(crate) files_with_current_summaries: usize,
    pub(crate) symbols_touched: usize,
    pub(crate) call_edges_added: usize,
    pub(crate) call_edges_removed: usize,
    pub(crate) truncated: bool,
    pub(crate) files: Vec<ContextPackDiffFilePreview>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackDiffFilePreview {
    pub(crate) path: String,
    pub(crate) status: String,
    pub(crate) touched_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) touched_symbol_refs: Vec<CompactSymbolRefPreview>,
    pub(crate) summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) summary_refs: Vec<ContextPackSummaryRefPreview>,
    pub(crate) added_call_edges: usize,
    pub(crate) removed_call_edges: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackSummaryRefPreview {
    pub(crate) handle: String,
    pub(crate) symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tag_alias: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) ontology_refs: Vec<CompactOntologyRefPreview>,
    pub(crate) summary: String,
    pub(crate) expand: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackTestPreview {
    pub(crate) runner: String,
    pub(crate) failures: usize,
    pub(crate) grouped_failures: usize,
    pub(crate) counts: ContextPackTestCounts,
    pub(crate) truncated: bool,
    pub(crate) failure_groups: Vec<ContextPackTestFailurePreview>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackTestCounts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) passed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) skipped: Option<usize>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackTestFailurePreview {
    pub(crate) tests: Vec<String>,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<usize>,
    pub(crate) occurrences: usize,
    pub(crate) summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackLogPreview {
    pub(crate) total_lines: usize,
    pub(crate) non_empty_lines: usize,
    pub(crate) signal_groups: usize,
    pub(crate) repeated_line_groups: usize,
    pub(crate) file_ref_groups: usize,
    pub(crate) symbol_ref_groups: usize,
    pub(crate) stack_groups: usize,
    pub(crate) truncated: bool,
    pub(crate) signals: Vec<ContextPackLogSignalPreview>,
    pub(crate) repeated_lines: Vec<ContextPackLogRepeatedLinePreview>,
    pub(crate) file_refs: Vec<ContextPackLogFileRefPreview>,
    pub(crate) symbol_refs: Vec<ContextPackLogSymbolRefPreview>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) raw_log_artifact: Option<log_digest::LogDigestArtifactRef>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackLogSignalPreview {
    pub(crate) severity: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<usize>,
    pub(crate) occurrences: usize,
    pub(crate) summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackLogRepeatedLinePreview {
    pub(crate) line: String,
    pub(crate) occurrences: usize,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackLogFileRefPreview {
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<usize>,
    pub(crate) occurrences: usize,
    pub(crate) summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Clone, Serialize)]
pub(crate) struct ContextPackLogSymbolRefPreview {
    pub(crate) handle: String,
    pub(crate) symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tag_alias: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) ontology_refs: Vec<CompactOntologyRefPreview>,
    pub(crate) occurrences: usize,
    pub(crate) summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) summary_refs: Vec<ContextPackSummaryRefPreview>,
}
fn effective_context_budget(budget: ResponseBudget) -> ResponseBudget {
    ResponseBudget::new(Some(budget.preview_items()), Some(budget.preview_bytes()))
}

fn build_context_summary_refs<'a>(
    prefix: &str,
    key_scope: &str,
    file_path: Option<&str>,
    snippets: impl Iterator<Item = (&'a str, &'a str)>,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> Vec<ContextPackSummaryRefPreview> {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    snippets
        .take(max_items)
        .map(|(symbol, summary)| {
            let tag_alias = tag_alias_from_name(symbol);
            let ontology_refs = tag_alias
                .as_deref()
                .map(|alias| ontology_refs_for_alias(ontology, alias))
                .unwrap_or_default();
            let expand = match file_path {
                Some(path) => format!("tsift summarize --file {}", shell_quote(path)),
                None => format!("tsift summarize {}", shell_quote(symbol)),
            };
            ContextPackSummaryRefPreview {
                handle: stable_handle(prefix, &format!("{key_scope}:{symbol}:{summary}")),
                symbol: truncate_for_budget(symbol, max_bytes),
                tag_alias: tag_alias.map(|alias| truncate_for_budget(&alias, max_bytes)),
                ontology_refs,
                summary: truncate_for_budget(summary, max_bytes),
                expand,
            }
        })
        .collect()
}

pub(crate) fn build_context_pack_diff_preview(
    report: &diff_digest::DiffDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackDiffPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackDiffPreview {
        mode: diff_digest_mode_label(report.mode).to_string(),
        files_changed: report.files_changed,
        files_with_current_summaries: report.files_with_current_summaries,
        symbols_touched: report.symbols_touched,
        call_edges_added: report.call_edges_added,
        call_edges_removed: report.call_edges_removed,
        truncated: report.files.len() > max_items,
        files: report
            .files
            .iter()
            .take(max_items)
            .map(|file| ContextPackDiffFilePreview {
                path: truncate_for_budget(&file.path, max_bytes),
                status: diff_digest_status_label(file.status).to_string(),
                touched_symbols: file
                    .touched_symbols
                    .iter()
                    .take(max_items)
                    .map(|symbol| truncate_for_budget(symbol, max_bytes))
                    .collect(),
                touched_symbol_refs: file
                    .touched_symbols
                    .iter()
                    .take(max_items)
                    .map(|symbol| {
                        build_compact_symbol_ref_with_ontology(
                            "cdsym",
                            &format!("{}:{}", file.path, symbol),
                            symbol,
                            None,
                            max_bytes,
                            ontology,
                        )
                    })
                    .collect(),
                summary_state: diff_digest_summary_label(file.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "cdsum",
                    &file.path,
                    Some(&file.path),
                    file.current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
                added_call_edges: file.added_call_edges.len(),
                removed_call_edges: file.removed_call_edges.len(),
                warnings: file
                    .warnings
                    .iter()
                    .take(max_items)
                    .map(|warning| truncate_for_budget(warning, max_bytes))
                    .collect(),
            })
            .collect(),
    }
}

fn enrich_next_context_with_diff_symbols(
    next_context: &mut SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
    ontology: Option<&TagOntologyPreviewContext>,
) {
    let mut symbols = next_context.touched_symbols.clone();
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            if !symbols.iter().any(|existing| existing == &symbol.name) {
                symbols.push(symbol.name.clone());
            }
        }
    }

    if symbols.is_empty() {
        return;
    }

    let max_items = next_context.max_items;
    let max_bytes = next_context.max_bytes;
    next_context.touched_symbol_total = next_context.touched_symbol_total.max(symbols.len());
    next_context.truncated |= symbols.len() > max_items;
    next_context.touched_symbols = symbols
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(entry, max_bytes))
        .collect();
    next_context.touched_symbol_refs = symbols
        .iter()
        .take(max_items)
        .map(|entry| {
            build_compact_symbol_ref_with_ontology(
                "ncsym",
                &format!("{}:{}", next_context.target, entry),
                entry,
                None,
                max_bytes,
                ontology,
            )
        })
        .collect();
}

fn context_exploration_source_window(
    root: &Path,
    file: &str,
    reason: String,
    budget: &ExplorationBudget,
) -> ExplorationSourceWindow {
    let start = 1;
    let end = budget.lines_per_window;
    ExplorationSourceWindow {
        handle: stable_handle("xwin", &format!("context:{file}:{start}:{end}:{reason}")),
        file: file.to_string(),
        start,
        end,
        reason,
        expand: source_read_command(root, file, start, budget.lines_per_window),
    }
}

fn build_context_pack_exploration_packet(
    root: &Path,
    next_context: &SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
) -> ExplorationPacket {
    let node_count = diff_digest
        .files_changed
        .saturating_add(next_context.touched_file_total)
        .saturating_add(next_context.touched_symbol_total);
    let edge_count = diff_digest
        .call_edges_added
        .saturating_add(diff_digest.call_edges_removed)
        .saturating_add(
            diff_digest
                .files
                .iter()
                .map(|file| file.touched_symbol_refs.len())
                .sum::<usize>(),
        );
    let budget = exploration_budget_for_counts(node_count, edge_count);

    let mut relationship_map = Vec::new();
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            if relationship_map.len() >= budget.relationship_limit {
                break;
            }
            relationship_map.push(ExplorationRelation {
                from: format!("file:{}", file.path),
                relation: "touches_symbol".to_string(),
                to: format!("symbol:{}", symbol.name),
                label: Some(format!("{} diff", file.status)),
            });
        }
    }
    for symbol in &next_context.touched_symbol_refs {
        if relationship_map.len() >= budget.relationship_limit {
            break;
        }
        relationship_map.push(ExplorationRelation {
            from: format!("context:{}", next_context.target),
            relation: "mentions_symbol".to_string(),
            to: format!("symbol:{}", symbol.name),
            label: Some("session next-context symbol".to_string()),
        });
    }

    let mut source_windows = Vec::new();
    let mut seen_files = BTreeSet::new();
    for file in &diff_digest.files {
        if source_windows.len() >= budget.max_source_windows {
            break;
        }
        if seen_files.insert(file.path.clone()) {
            source_windows.push(context_exploration_source_window(
                root,
                &file.path,
                format!("changed file ({})", file.status),
                &budget,
            ));
        }
    }
    for file in &next_context.touched_files {
        if source_windows.len() >= budget.max_source_windows {
            break;
        }
        if seen_files.insert(file.clone()) {
            source_windows.push(context_exploration_source_window(
                root,
                file,
                "session touched file".to_string(),
                &budget,
            ));
        }
    }

    let worker_seeds = if next_context.prompt_targets.is_empty() {
        next_context.next_digest_commands.clone()
    } else {
        next_context.prompt_targets.clone()
    };
    let mut worker_context = Vec::new();
    for (idx, prompt) in worker_seeds
        .iter()
        .take(budget.relationship_limit)
        .enumerate()
    {
        let summary = truncate_for_budget(prompt, next_context.max_bytes);
        worker_context.push(ExplorationWorkerContext {
            handle: stable_handle(
                "xwrk",
                &format!("{}:{}:{}", next_context.target, idx, prompt),
            ),
            target: next_context.target.clone(),
            summary,
            expand: format!(
                "tsift --envelope context-pack {} --budget normal",
                shell_quote(&next_context.target)
            ),
        });
    }

    ExplorationPacket {
        budget,
        relationship_map,
        source_windows,
        worker_context,
        no_reread_guidance:
            "Use worker_context for bounded handoff scope, then source_windows expand commands before broad file reads; relationship_map explains why each window is in the handoff."
                .to_string(),
    }
}

pub(crate) fn exploration_ref_id(label: &str) -> String {
    stable_handle("xref", label)
}

fn context_pack_exploration_projection(packet: &ExplorationPacket) -> Result<GraphProjection> {
    let provenance = GraphProvenance::new("tsift.context-pack", "exploration");
    let mut nodes = BTreeMap::<String, SubstrateGraphNode>::new();
    let mut edges = Vec::new();

    for relation in &packet.relationship_map {
        for label in [&relation.from, &relation.to] {
            let id = exploration_ref_id(label);
            nodes.entry(id.clone()).or_insert_with(|| {
                SubstrateGraphNode::new(id, "exploration_ref", label.clone())
                    .with_property("label", label.clone())
                    .with_provenance(provenance.clone())
            });
        }
        let mut edge = SubstrateGraphEdge::new(
            exploration_ref_id(&relation.from),
            exploration_ref_id(&relation.to),
            relation.relation.clone(),
        )
        .with_provenance(provenance.clone());
        if let Some(label) = &relation.label {
            edge = edge.with_property("label", label.clone());
        }
        edges.push(edge_with_content_freshness(edge)?);
    }

    for window in &packet.source_windows {
        let label = format!("{}:{}-{}", window.file, window.start, window.end);
        let node = SubstrateGraphNode::new(window.handle.clone(), "source_handle", label)
            .with_property("handle", window.handle.clone())
            .with_property("file", window.file.clone())
            .with_property("start", window.start.to_string())
            .with_property("end", window.end.to_string())
            .with_property("reason", window.reason.clone())
            .with_property("expand", window.expand.clone())
            .with_provenance(provenance.clone());
        nodes.insert(window.handle.clone(), node_with_content_freshness(node)?);

        let file_ref = format!("file:{}", window.file);
        let file_ref_id = exploration_ref_id(&file_ref);
        nodes.entry(file_ref_id.clone()).or_insert_with(|| {
            SubstrateGraphNode::new(file_ref_id.clone(), "exploration_ref", file_ref.clone())
                .with_property("label", file_ref.clone())
                .with_provenance(provenance.clone())
        });
        let edge = SubstrateGraphEdge::new(window.handle.clone(), file_ref_id, "expands_source")
            .with_property("label", window.reason.clone())
            .with_provenance(provenance.clone());
        edges.push(edge_with_content_freshness(edge)?);
    }

    for worker in &packet.worker_context {
        let node = SubstrateGraphNode::new(
            worker.handle.clone(),
            "worker_context",
            worker.summary.clone(),
        )
        .with_property("handle", worker.handle.clone())
        .with_property("target", worker.target.clone())
        .with_property("summary", worker.summary.clone())
        .with_property("expand", worker.expand.clone())
        .with_provenance(provenance.clone());
        nodes.insert(worker.handle.clone(), node_with_content_freshness(node)?);

        let target_ref = format!("context:{}", worker.target);
        let target_ref_id = exploration_ref_id(&target_ref);
        nodes.entry(target_ref_id.clone()).or_insert_with(|| {
            SubstrateGraphNode::new(target_ref_id.clone(), "exploration_ref", target_ref.clone())
                .with_property("label", target_ref.clone())
                .with_provenance(provenance.clone())
        });
        edges.push(edge_with_content_freshness(
            SubstrateGraphEdge::new(worker.handle.clone(), target_ref_id, "scopes_context")
                .with_property("label", "bounded worker context".to_string())
                .with_provenance(provenance.clone()),
        )?);

        for window in &packet.source_windows {
            edges.push(edge_with_content_freshness(
                SubstrateGraphEdge::new(
                    worker.handle.clone(),
                    window.handle.clone(),
                    "scopes_source",
                )
                .with_property("label", window.reason.clone())
                .with_provenance(provenance.clone()),
            )?);
        }
    }

    let mut nodes = nodes.into_values().collect::<Vec<_>>();
    for node in &mut nodes {
        if node.freshness.is_none() {
            let fresh = node_with_content_freshness(node.clone())?;
            *node = fresh;
        }
    }

    Ok(GraphProjection { nodes, edges })
}

fn source_window_from_graph_node(node: SubstrateGraphNode) -> Result<ExplorationSourceWindow> {
    let file = node
        .properties
        .get("file")
        .cloned()
        .with_context(|| format!("source handle {} missing file property", node.id))?;
    let start = node
        .properties
        .get("start")
        .with_context(|| format!("source handle {} missing start property", node.id))?
        .parse::<usize>()
        .with_context(|| format!("source handle {} has invalid start", node.id))?;
    let end = node
        .properties
        .get("end")
        .with_context(|| format!("source handle {} missing end property", node.id))?
        .parse::<usize>()
        .with_context(|| format!("source handle {} has invalid end", node.id))?;
    Ok(ExplorationSourceWindow {
        handle: node
            .properties
            .get("handle")
            .cloned()
            .unwrap_or_else(|| node.id.clone()),
        file,
        start,
        end,
        reason: node
            .properties
            .get("reason")
            .cloned()
            .unwrap_or_else(|| "source context".to_string()),
        expand: node.properties.get("expand").cloned().unwrap_or_default(),
    })
}

pub(crate) fn materialize_context_pack_exploration_packet(
    root: &Path,
    packet: ExplorationPacket,
) -> Result<ExplorationPacket> {
    let projection = context_pack_exploration_projection(&packet)?;
    let graph_db = graph_substrate_db_path(root, None);
    let mut store = SqliteGraphStore::open(&graph_db)?;
    store.upsert_projection(&projection)?;

    let mut source_windows = Vec::new();
    for window in &packet.source_windows {
        let node = store
            .node(&window.handle)?
            .with_context(|| format!("source handle {} was not materialized", window.handle))?;
        source_windows.push(source_window_from_graph_node(node)?);
    }

    let mut relationship_map = Vec::new();
    for relation in &packet.relationship_map {
        let from_id = exploration_ref_id(&relation.from);
        let to_id = exploration_ref_id(&relation.to);
        let from = store
            .node(&from_id)?
            .with_context(|| format!("exploration ref {} was not materialized", relation.from))?;
        let to = store
            .node(&to_id)?
            .with_context(|| format!("exploration ref {} was not materialized", relation.to))?;
        let edge = store
            .outgoing_edges(&from_id, Some(&relation.relation))?
            .into_iter()
            .find(|edge| edge.to_id == to_id)
            .with_context(|| {
                format!(
                    "exploration relation {} -> {} ({}) was not materialized",
                    relation.from, relation.to, relation.relation
                )
            })?;
        relationship_map.push(ExplorationRelation {
            from: from.label,
            relation: edge.kind,
            to: to.label,
            label: edge.properties.get("label").cloned(),
        });
    }

    Ok(ExplorationPacket {
        budget: packet.budget,
        relationship_map,
        source_windows,
        worker_context: packet.worker_context,
        no_reread_guidance: packet.no_reread_guidance,
    })
}

pub(crate) fn build_context_pack_test_preview(
    report: &test_digest::TestDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackTestPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackTestPreview {
        runner: report.runner.clone(),
        failures: report.failures,
        grouped_failures: report.grouped_failures,
        counts: ContextPackTestCounts {
            passed: report.counts.passed,
            failed: report.counts.failed,
            skipped: report.counts.skipped,
        },
        truncated: report.failure_groups.len() > max_items || report.warnings.len() > max_items,
        failure_groups: report
            .failure_groups
            .iter()
            .take(max_items)
            .map(|failure| ContextPackTestFailurePreview {
                tests: failure
                    .tests
                    .iter()
                    .take(max_items)
                    .map(|test| truncate_for_budget(test, max_bytes))
                    .collect(),
                message: truncate_for_budget(&failure.message, max_bytes),
                path: failure
                    .path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                line: failure.line,
                occurrences: failure.occurrences,
                summary_state: test_digest_summary_label(failure.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "ctsum",
                    failure.path.as_deref().unwrap_or("test-failure"),
                    failure.path.as_deref(),
                    failure
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        warnings: report
            .warnings
            .iter()
            .take(max_items)
            .map(|warning| truncate_for_budget(warning, max_bytes))
            .collect(),
    }
}

pub(crate) fn build_context_pack_log_preview(
    report: &log_digest::LogDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackLogPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackLogPreview {
        total_lines: report.total_lines,
        non_empty_lines: report.non_empty_lines,
        signal_groups: report.signal_groups,
        repeated_line_groups: report.repeated_line_groups,
        file_ref_groups: report.file_ref_groups,
        symbol_ref_groups: report.symbol_ref_groups,
        stack_groups: report.stack_groups,
        truncated: report.signals.len() > max_items
            || report.repeated_lines.len() > max_items
            || report.file_refs.len() > max_items
            || report.symbol_refs.len() > max_items
            || report.warnings.len() > max_items,
        signals: report
            .signals
            .iter()
            .take(max_items)
            .map(|signal| ContextPackLogSignalPreview {
                severity: signal.severity.clone(),
                message: truncate_for_budget(&signal.message, max_bytes),
                path: signal
                    .path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                line: signal.line,
                occurrences: signal.occurrences,
                summary_state: log_digest_summary_label(signal.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clsum",
                    signal.path.as_deref().unwrap_or("log-signal"),
                    signal.path.as_deref(),
                    signal
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        repeated_lines: report
            .repeated_lines
            .iter()
            .take(max_items)
            .map(|line| ContextPackLogRepeatedLinePreview {
                line: truncate_for_budget(&line.line, max_bytes),
                occurrences: line.occurrences,
            })
            .collect(),
        file_refs: report
            .file_refs
            .iter()
            .take(max_items)
            .map(|file| ContextPackLogFileRefPreview {
                path: truncate_for_budget(&file.path, max_bytes),
                line: file.line,
                occurrences: file.occurrences,
                summary_state: log_digest_summary_label(file.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clfsum",
                    &file.path,
                    Some(&file.path),
                    file.current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        symbol_refs: report
            .symbol_refs
            .iter()
            .take(max_items)
            .map(|symbol| ContextPackLogSymbolRefPreview {
                handle: stable_handle("clsym", &symbol.symbol),
                symbol: truncate_for_budget(&symbol.symbol, max_bytes),
                tag_alias: tag_alias_from_name(&symbol.symbol)
                    .map(|alias| truncate_for_budget(&alias, max_bytes)),
                ontology_refs: tag_alias_from_name(&symbol.symbol)
                    .as_deref()
                    .map(|alias| ontology_refs_for_alias(ontology, alias))
                    .unwrap_or_default(),
                occurrences: symbol.occurrences,
                summary_state: log_digest_summary_label(symbol.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clssum",
                    &symbol.symbol,
                    None,
                    symbol
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        raw_log_artifact: report.raw_log_artifact.clone(),
        warnings: report
            .warnings
            .iter()
            .take(max_items)
            .map(|warning| truncate_for_budget(warning, max_bytes))
            .collect(),
    }
}

fn enrich_log_preview_with_diff_symbols(
    log_preview: &mut ContextPackLogPreview,
    diff_digest: &ContextPackDiffPreview,
    ontology: Option<&TagOntologyPreviewContext>,
) {
    if !log_preview.symbol_refs.is_empty() {
        return;
    }

    let mut symbols = Vec::new();
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            if !symbols
                .iter()
                .any(|existing: &String| existing == &symbol.name)
            {
                symbols.push(symbol.name.clone());
            }
        }
    }

    if symbols.is_empty() {
        return;
    }

    log_preview.symbol_ref_groups = log_preview.symbol_ref_groups.max(symbols.len());
    log_preview.symbol_refs = symbols
        .into_iter()
        .map(|symbol| ContextPackLogSymbolRefPreview {
            handle: stable_handle("clsym", &symbol),
            symbol: symbol.clone(),
            tag_alias: tag_alias_from_name(&symbol),
            ontology_refs: tag_alias_from_name(&symbol)
                .as_deref()
                .map(|alias| ontology_refs_for_alias(ontology, alias))
                .unwrap_or_default(),
            occurrences: 1,
            summary_state: "unavailable".to_string(),
            summary_refs: Vec::new(),
        })
        .collect();
}

fn insert_ontology_refs(
    refs: &mut BTreeMap<String, CompactOntologyRefPreview>,
    candidates: &[CompactOntologyRefPreview],
) {
    for candidate in candidates {
        refs.entry(candidate.handle.clone())
            .or_insert_with(|| candidate.clone());
    }
}

fn collect_context_pack_ontology_refs(
    next_context: &SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
    test_digest: &ContextPackOptionalSection<ContextPackTestPreview>,
    log_digest: &ContextPackOptionalSection<ContextPackLogPreview>,
) -> Vec<CompactOntologyRefPreview> {
    let mut refs = BTreeMap::new();
    for symbol in &next_context.touched_symbol_refs {
        insert_ontology_refs(&mut refs, &symbol.ontology_refs);
    }
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            insert_ontology_refs(&mut refs, &symbol.ontology_refs);
        }
        for summary in &file.summary_refs {
            insert_ontology_refs(&mut refs, &summary.ontology_refs);
        }
    }
    if let Some(test) = &test_digest.report {
        for failure in &test.failure_groups {
            for summary in &failure.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
    }
    if let Some(log) = &log_digest.report {
        for signal in &log.signals {
            for summary in &signal.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
        for file in &log.file_refs {
            for summary in &file.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
        for symbol in &log.symbol_refs {
            insert_ontology_refs(&mut refs, &symbol.ontology_refs);
            for summary in &symbol.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
    }
    refs.into_values().collect()
}

/// Collect the symbol/file identifiers already in the context-pack result set —
/// touched files/symbols, diff-preview files and their touched symbols, and the
/// exploration packet's source-window files plus relationship endpoints. These
/// are the anchors a finding must `concern` to ride the hot path (#trt1p2): the
/// layer injects authored context for nodes the agent is *already* looking at,
/// never a blanket dump of every finding in the store.
fn context_pack_result_set_keys(
    next_context: &SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
    exploration: &ExplorationPacket,
) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for file in &next_context.touched_files {
        keys.insert(file.clone());
    }
    for symbol in &next_context.touched_symbols {
        keys.insert(symbol.clone());
    }
    for file in &diff_digest.files {
        keys.insert(file.path.clone());
        for symbol in &file.touched_symbols {
            keys.insert(symbol.clone());
        }
    }
    for window in &exploration.source_windows {
        keys.insert(window.file.clone());
    }
    for relation in &exploration.relationship_map {
        keys.insert(relation.from.clone());
        keys.insert(relation.to.clone());
    }
    keys
}

/// Build the trusted-fresh findings preview for the context-pack envelope. Fails
/// open to an empty section when the findings store is absent or unreadable, so
/// hot-path injection never breaks a context-pack for a project that has no
/// authored knowledge.
fn build_context_pack_findings(
    root: &Path,
    keys: &BTreeSet<String>,
    budget: ResponseBudget,
) -> Vec<ContextPackFindingPreview> {
    let findings = match crate::commands::finding::collect_injectable_findings(root, keys, None) {
        Ok(findings) => findings,
        Err(_) => return Vec::new(),
    };
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    findings
        .into_iter()
        .take(max_items)
        .map(|finding| ContextPackFindingPreview {
            handle: stable_handle("cpfind", &format!("{}:{}", finding.about, finding.id)),
            id: finding.id,
            kind: finding.kind,
            title: truncate_for_budget(&finding.title, max_bytes),
            anchor_kind: finding.anchor_kind,
            confidence: finding.confidence,
            body: truncate_for_budget(&finding.body, max_bytes),
            expand: format!(
                "tsift finding list --about {} --json",
                shell_quote(&finding.about)
            ),
            about: finding.about,
        })
        .collect()
}

pub(crate) fn build_context_pack_report(
    path: &Path,
    test_input: Option<&Path>,
    runner: Option<&str>,
    log_input: Option<&Path>,
    budget: ResponseBudget,
) -> Result<ContextPackReport> {
    Ok(build_context_pack_report_with_profile(path, test_input, runner, log_input, budget)?.0)
}

pub(crate) fn build_context_pack_report_with_profile(
    path: &Path,
    test_input: Option<&Path>,
    runner: Option<&str>,
    log_input: Option<&Path>,
    budget: ResponseBudget,
) -> Result<(ContextPackReport, Vec<GraphDbBackendEvalPhaseTiming>)> {
    // #gdbgatecold: trusted scope share — `prepare_agent_doc_index_gate_cached`
    // and `context_pack_status_reminders` both call `IndexDb::inspect_read_only`
    // on the same `(root, .tsift/index.db)` cold path. While this guard is
    // alive, the second call reuses the cached inspection on the same thread
    // instead of paying the disk/SQLite walk a second time. Search runs
    // entirely outside this scope, so freshness re-checks after a file
    // mutation are unaffected.
    let _inspect_scope = index::InspectScopeGuard::new();
    let budget = effective_context_budget(budget);
    let mut phases = Vec::new();
    let session_review_started = Instant::now();
    let (review, session_review_sub_phases) = session_review::compute_with_phases(path)?;
    let session_review_total_micros = session_review_started.elapsed().as_micros();
    phases.push(graph_db_backend_eval_phase_timing(
        "session_review_compute",
        session_review_total_micros,
        "session-review prompt/touched-file/touched-symbol/failure aggregation for the context-pack handoff",
    ));
    for sub_phase in &session_review_sub_phases {
        phases.push(graph_db_backend_eval_phase_timing(
            &format!("session_review_compute.{}", sub_phase.name),
            sub_phase.duration_micros,
            &sub_phase.detail,
        ));
    }
    let root = PathBuf::from(&review.root);
    let status_index_gate_started = Instant::now();
    let mut status_index_gate_sub_phases: Vec<(String, u128, String)> = Vec::with_capacity(3);
    let index_gate_started = Instant::now();
    let (gate, gate_cache_detail) =
        prepare_agent_doc_index_gate_cached(&root, path, None, "context-pack handoff");
    let index_gate_micros = index_gate_started.elapsed().as_micros();
    status_index_gate_sub_phases.push((
        "prepare_agent_doc_index_gate".to_string(),
        index_gate_micros,
        gate_cache_detail,
    ));

    let reminders_started = Instant::now();
    let mut status_reminders = gate.diagnostics.clone();
    status_reminders.extend(context_pack_status_reminders(&root));
    let reminders_micros = reminders_started.elapsed().as_micros();
    status_index_gate_sub_phases.push((
        "context_pack_status_reminders".to_string(),
        reminders_micros,
        "tsift status reminders for the cached preparation context".to_string(),
    ));

    let ontology_started = Instant::now();
    let ontology = load_tag_ontology_preview_context(&root);
    let ontology_micros = ontology_started.elapsed().as_micros();
    status_index_gate_sub_phases.push((
        "load_tag_ontology_preview_context".to_string(),
        ontology_micros,
        "tag ontology preview context load".to_string(),
    ));

    let status_index_gate_total_micros = status_index_gate_started.elapsed().as_micros();
    phases.push(graph_db_backend_eval_phase_timing(
        "status_index_gate",
        status_index_gate_total_micros,
        "agent-doc index gate, tsift status reminders, and ontology preview loading",
    ));
    for (name, micros, detail) in &status_index_gate_sub_phases {
        phases.push(graph_db_backend_eval_phase_timing(
            &format!("status_index_gate.{name}"),
            *micros,
            detail,
        ));
    }
    let ontology_ref = ontology.as_ref();
    let mut next_context =
        build_session_review_next_context_budget_report(&review, budget, ontology_ref);
    // #gdbprephot: cap working-tree diff_digest parsing to the preview budget.
    // build_context_pack_diff_preview only emits files.take(preview_items),
    // and enrich_next_context_with_diff_symbols / build_context_pack_exploration_packet
    // only iterate diff_digest.files (the preview window). The full-fat parse
    // of every working-tree changed file dominated context_pack_diff cost on
    // repos with many unstaged edits.
    let diff_parse_budget = budget.preview_items();
    let diff_digest = graph_db_backend_eval_timed_phase(
        &mut phases,
        "context_pack_diff",
        "working-tree diff digest preview used to enrich next-context symbols",
        || {
            Ok(build_context_pack_diff_preview(
                &diff_digest::compute(
                    &root,
                    diff_digest::DiffDigestOptions {
                        cached: false,
                        revision: None,
                        max_parsed_files: Some(diff_parse_budget),
                    },
                )
                .with_context(|| {
                    format!("computing context-pack diff digest for {}", root.display())
                })?,
                budget,
                ontology_ref,
            ))
        },
    )?;
    enrich_next_context_with_diff_symbols(&mut next_context, &diff_digest, ontology_ref);
    let test_digest = match test_input {
        Some(file_path) => {
            let input = fs::read_to_string(file_path)
                .with_context(|| format!("reading test output: {}", file_path.display()))?;
            if input.trim().is_empty() {
                bail!("no test output provided in {}", file_path.display());
            }
            let report = test_digest::compute(&root, &input, runner)?;
            ContextPackOptionalSection {
                status: "included".to_string(),
                command: format!(
                    "tsift test-digest --path . --input {}{}",
                    shell_quote(file_path.to_str().unwrap_or_default()),
                    runner
                        .map(|value| format!(" --runner {}", shell_quote(value)))
                        .unwrap_or_default()
                ),
                source: Some(file_path.display().to_string()),
                report: Some(build_context_pack_test_preview(
                    &report,
                    budget,
                    ontology_ref,
                )),
            }
        }
        None => ContextPackOptionalSection {
            status: "not_provided".to_string(),
            command: "tsift test-digest --path . < test.log".to_string(),
            source: None,
            report: None,
        },
    };
    let log_digest = match log_input {
        Some(file_path) => {
            let input = fs::read_to_string(file_path)
                .with_context(|| format!("reading log output: {}", file_path.display()))?;
            if input.trim().is_empty() {
                bail!("no log output provided in {}", file_path.display());
            }
            let mut report = log_digest::compute(&root, &input)?;
            if log_digest::raw_log_artifact_recommended(&report, input.len()) {
                let expand = format!(
                    "tsift log-digest --path . --input {} --json",
                    shell_quote(file_path.to_str().unwrap_or_default())
                );
                report.raw_log_artifact = Some(log_digest::LogDigestArtifactRef {
                    handle: stable_handle("logdg", &format!("ctxpack:{}", file_path.display())),
                    path: file_path.display().to_string(),
                    bytes: input.len(),
                    lines: input.lines().count(),
                    expand,
                });
            }
            let mut preview = build_context_pack_log_preview(&report, budget, ontology_ref);
            enrich_log_preview_with_diff_symbols(&mut preview, &diff_digest, ontology_ref);
            ContextPackOptionalSection {
                status: "included".to_string(),
                command: format!(
                    "tsift log-digest --path . --input {}",
                    shell_quote(file_path.to_str().unwrap_or_default())
                ),
                source: Some(file_path.display().to_string()),
                report: Some(preview),
            }
        }
        None => ContextPackOptionalSection {
            status: "not_provided".to_string(),
            command: "tsift log-digest --path . < build.log".to_string(),
            source: None,
            report: None,
        },
    };

    let ontology_refs =
        collect_context_pack_ontology_refs(&next_context, &diff_digest, &test_digest, &log_digest);
    let exploration = graph_db_backend_eval_timed_phase(
        &mut phases,
        "exploration_materialization",
        "context-pack source-window and worker-context exploration packet projection",
        || {
            materialize_context_pack_exploration_packet(
                &root,
                build_context_pack_exploration_packet(&root, &next_context, &diff_digest),
            )
        },
    )?;
    let findings = graph_db_backend_eval_timed_phase(
        &mut phases,
        "findings_injection",
        "trusted, fresh authored findings folded in for nodes already in the result set (#trt1p2)",
        || {
            Ok(build_context_pack_findings(
                &root,
                &context_pack_result_set_keys(&next_context, &diff_digest, &exploration),
                budget,
            ))
        },
    )?;
    let graph_orchestration = graph_db_backend_eval_timed_phase(
        &mut phases,
        "graph_orchestration",
        "context-pack graph freshness, evidence packet ids, and conflict-matrix follow-up commands",
        || context_pack_graph_orchestration(&root, path, &next_context, &exploration),
    )?;

    Ok((
        ContextPackReport {
            root: review.root,
            target: review.target,
            target_kind: review.target_kind,
            max_items: budget.preview_items(),
            max_bytes: budget.preview_bytes(),
            status_reminders,
            ontology_refs,
            next_context,
            diff_digest,
            test_digest,
            log_digest,
            exploration,
            findings,
            graph_orchestration,
            resume_commands: review.next_context.next_digest_commands,
        },
        phases,
    ))
}

pub(crate) fn context_pack_status_reminders(root: &Path) -> Vec<String> {
    status::check_status(root)
        .map(|report| report.reminders)
        .unwrap_or_default()
}

fn context_pack_graph_orchestration(
    root: &Path,
    path: &Path,
    next_context: &SessionReviewNextContextBudgetReport,
    exploration: &ExplorationPacket,
) -> Result<ContextPackGraphOrchestration> {
    let graph_db = graph_substrate_db_path(root, None);
    let store = SqliteGraphStore::open_read_only_resilient(&graph_db)
        .with_context(|| format!("opening graph-db projection: {}", graph_db.display()))?;
    let projection_freshness = sqlite_graph_freshness(&store, "root")?;
    let mut warnings = projection_freshness.diagnostics.clone();
    if let Some(recovery) = store.read_only_recovery() {
        warnings.push(graph_db_read_recovery_diagnostic(recovery));
    }
    let readiness =
        graph_db_semantic_readiness(root, None, graph_store_semantic_node_count(&store).ok());
    if readiness.fail_closed {
        warnings.extend(readiness.diagnostics.clone());
    }

    let (evidence_packet_ids, resolvable_targets, conflict_matrix_decisions) = if readiness
        .fail_closed
    {
        (
            Vec::new(),
            Vec::new(),
            vec![format!(
                "graph readiness blocked ({}); resolve readiness warnings before running conflict-matrix",
                readiness.reason,
            )],
        )
    } else {
        let mut targets = next_context
            .prompt_targets
            .iter()
            .flat_map(|prompt| extract_conflict_target_refs(prompt))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            targets.extend(
                exploration
                    .worker_context
                    .iter()
                    .flat_map(|worker| extract_conflict_target_refs(&worker.summary)),
            );
        }
        targets = dedupe_preserve_order(targets);

        let mut ev_ids = Vec::new();
        let mut resolved = Vec::new();
        for target in &targets {
            match graph_db_resolve_evidence_target(&store, target)? {
                Some(node) => {
                    ev_ids.push(graph_db_evidence_packet_id(
                        target,
                        &node,
                        &projection_freshness,
                    ));
                    resolved.push(target.clone());
                }
                None => warnings.push(format!("graph evidence target not found: {target}")),
            }
        }

        let decisions = if resolved.is_empty() {
            vec!["no resolvable backlog/job targets found for conflict-matrix".to_string()]
        } else {
            vec![format!(
                "run conflict-matrix before parallel dispatch for {} target(s)",
                resolved.len()
            )]
        };
        (ev_ids, resolved, decisions)
    };

    let mut follow_up_commands = vec![format!(
        "tsift graph-db --path {} status --json",
        shell_quote(root.to_string_lossy().as_ref())
    )];
    follow_up_commands.extend(readiness.next_commands.clone());
    for target in &resolvable_targets {
        follow_up_commands.push(format!(
            "tsift graph-db --path {} evidence {} --depth 3 --limit 8 --json",
            shell_quote(root.to_string_lossy().as_ref()),
            shell_quote(target)
        ));
    }
    if !resolvable_targets.is_empty() {
        follow_up_commands.push(format!(
            "tsift conflict-matrix --path {} {} --json",
            shell_quote(path.to_string_lossy().as_ref()),
            resolvable_targets
                .iter()
                .map(|target| shell_quote(target))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    let worker_ownership_blocks = exploration
        .worker_context
        .iter()
        .map(|worker| format!("{} scopes {}", worker.handle, worker.summary))
        .collect::<Vec<_>>();
    let projection_hashes = projection_freshness
        .content_hash
        .clone()
        .into_iter()
        .collect();

    Ok(ContextPackGraphOrchestration {
        contract_version: CONTEXT_PACK_GRAPH_ORCHESTRATION_CONTRACT_VERSION,
        graph_db_command: format!(
            "tsift graph-db --path {} status --json",
            shell_quote(root.to_string_lossy().as_ref())
        ),
        projection_freshness,
        readiness,
        projection_hashes,
        evidence_packet_ids,
        conflict_matrix_decisions,
        worker_ownership_blocks,
        follow_up_commands: dedupe_preserve_order(follow_up_commands),
        warnings,
    })
}

pub(crate) fn print_context_pack_human(report: &ContextPackReport, compact: bool) {
    if compact {
        println!(
            "context-pack target:{} prompts:{}/{} diff:{}/{} test:{} log:{}",
            shell_quote(&report.target),
            report.next_context.prompt_targets.len(),
            report.next_context.prompt_target_total,
            report.diff_digest.files.len(),
            report.diff_digest.files_changed,
            report.test_digest.status,
            report.log_digest.status
        );
        for reminder in &report.status_reminders {
            println!("reminder {reminder}");
        }
        if let Some(health) = &report.next_context.prompt_cache_health {
            println!("prompt-cache {}", health.summary_line);
        }
        for prompt in &report.next_context.prompt_targets {
            println!("prompt {prompt}");
        }
        for action in &report.next_context.next_token_actions {
            println!(
                "token-action {} {} commands:{}",
                action.priority,
                action.kind,
                action.digest_commands.len()
                    + action.rewrite_commands.len()
                    + usize::from(action.compact_command.is_some())
                    + usize::from(action.restart_command.is_some())
            );
        }
        if let Some(queue) = &report.next_context.agent_doc_queue {
            println!(
                "agent-doc-queue active:{} exchange_tail:{}/{} backlog:{}/{} review:{}/{} presets:{}/{}",
                queue.active_queue_prompt.as_deref().unwrap_or("-"),
                queue.live_exchange_tail.len(),
                queue.live_exchange_tail_total,
                queue.backlog_rows.len(),
                queue.backlog_row_total,
                queue.review_rows.len(),
                queue.review_row_total,
                queue.prompt_presets.len(),
                queue.prompt_preset_total
            );
        }
        for file in &report.diff_digest.files {
            println!(
                "diff {} status:{} syms:{} sums:{}",
                file.path,
                file.status,
                if file.touched_symbol_refs.is_empty() {
                    "-".to_string()
                } else {
                    file.touched_symbol_refs
                        .iter()
                        .map(compact_symbol_ref_token)
                        .collect::<Vec<_>>()
                        .join(",")
                },
                if file.summary_refs.is_empty() {
                    "-".to_string()
                } else {
                    file.summary_refs
                        .iter()
                        .map(|summary| summary.handle.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                }
            );
        }
        if let Some(test) = &report.test_digest.report {
            println!(
                "test runner:{} failures:{} groups:{}",
                test.runner, test.failures, test.grouped_failures
            );
        } else {
            println!("test {}", report.test_digest.command);
        }
        if let Some(log) = &report.log_digest.report {
            println!(
                "log lines:{} signals:{} files:{} syms:{}",
                log.non_empty_lines, log.signal_groups, log.file_ref_groups, log.symbol_ref_groups
            );
        } else {
            println!("log {}", report.log_digest.command);
        }
        println!(
            "explore windows:{} relations:{} budget:{}",
            report.exploration.source_windows.len(),
            report.exploration.relationship_map.len(),
            report.exploration.budget.project_size
        );
        for finding in &report.findings {
            println!(
                "finding {} [{}] about:{} {}",
                finding.handle, finding.kind, finding.about, finding.title
            );
        }
        println!(
            "graph-orchestration freshness:{} readiness:{} evidence:{} ownership:{}",
            report.graph_orchestration.projection_freshness.status,
            report.graph_orchestration.readiness.status,
            report.graph_orchestration.evidence_packet_ids.len(),
            report.graph_orchestration.worker_ownership_blocks.len()
        );
        return;
    }

    println!("Context pack");
    println!("  target:                 {}", report.target);
    println!("  target kind:            {}", report.target_kind);
    println!("  root:                   {}", report.root);
    println!(
        "  preview budget:         {} items / {} bytes",
        report.max_items, report.max_bytes
    );
    if !report.status_reminders.is_empty() {
        println!("  status reminders:");
        for reminder in &report.status_reminders {
            println!("  - {reminder}");
        }
    }
    println!();
    println!("Next context");
    println!(
        "  prompt targets:         {}/{}",
        report.next_context.prompt_targets.len(),
        report.next_context.prompt_target_total
    );
    println!(
        "  touched files:          {}/{}",
        report.next_context.touched_files.len(),
        report.next_context.touched_file_total
    );
    println!(
        "  touched symbols:        {}/{}",
        report.next_context.touched_symbols.len(),
        report.next_context.touched_symbol_total
    );
    println!(
        "  unresolved failures:    {}/{}",
        report.next_context.unresolved_failures.len(),
        report.next_context.unresolved_failure_total
    );
    if let Some(health) = &report.next_context.prompt_cache_health {
        println!("  prompt-cache health:    {}", health.summary_line);
    }
    if !report.next_context.prompt_targets.is_empty() {
        for prompt in &report.next_context.prompt_targets {
            println!("  - prompt: {prompt}");
        }
    }
    if let Some(queue) = &report.next_context.agent_doc_queue {
        println!("  agent-doc queue:");
        if let Some(prompt) = &queue.active_queue_prompt {
            println!("  - active: {prompt}");
        }
        for line in &queue.live_exchange_tail {
            println!("  - exchange-tail: {line}");
        }
        for row in &queue.backlog_rows {
            println!("  - backlog: {row}");
        }
        for row in &queue.review_rows {
            println!("  - review: {row}");
        }
        for preset in &queue.prompt_presets {
            println!("  - preset: {preset}");
        }
        for handle in &queue.expansion_handles {
            println!(
                "  - expand {} [{}]: {}",
                handle.handle, handle.label, handle.expand
            );
        }
    }
    if !report.next_context.touched_files.is_empty() {
        for path in &report.next_context.touched_files {
            println!("  - file: {path}");
        }
    }
    if !report.next_context.touched_symbols.is_empty() {
        for symbol in &report.next_context.touched_symbol_refs {
            println!(
                "  - symbol: {}",
                format_symbol_preview_line(
                    &symbol.handle,
                    &symbol.name,
                    symbol.tag_alias.as_deref()
                )
            );
        }
    }
    if !report.next_context.next_token_actions.is_empty() {
        println!("  token actions:");
        for action in &report.next_context.next_token_actions {
            println!(
                "  - [{}:{}] {} | guidance: {}",
                action.priority, action.kind, action.message, action.guidance
            );
            if let Some(command) = &action.compact_command {
                println!("    compact: {command}");
            }
            if let Some(command) = &action.restart_command {
                println!("    restart: {command}");
            }
            for command in &action.digest_commands {
                println!("    digest: {command}");
            }
            for command in &action.rewrite_commands {
                println!("    rewrite: {command}");
            }
        }
    }

    println!();
    println!("Diff digest");
    println!("  mode:                   {}", report.diff_digest.mode);
    println!(
        "  files changed:          {}/{}",
        report.diff_digest.files.len(),
        report.diff_digest.files_changed
    );
    println!(
        "  touched symbols:        {}",
        report.diff_digest.symbols_touched
    );
    println!(
        "  call edges:             +{} / -{}",
        report.diff_digest.call_edges_added, report.diff_digest.call_edges_removed
    );
    for file in &report.diff_digest.files {
        println!("  - {} [{}]", file.path, file.status);
        if !file.touched_symbol_refs.is_empty() {
            println!(
                "    symbols: {}",
                file.touched_symbol_refs
                    .iter()
                    .map(|symbol| format_symbol_preview_line(
                        &symbol.handle,
                        &symbol.name,
                        symbol.tag_alias.as_deref()
                    ))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        if !file.warnings.is_empty() {
            println!("    warnings: {}", file.warnings.join(" | "));
        }
        if !file.summary_refs.is_empty() {
            println!(
                "    summaries: {}",
                file.summary_refs
                    .iter()
                    .map(format_summary_ref_line)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
    }

    println!();
    println!("Test digest");
    println!("  status:                 {}", report.test_digest.status);
    match &report.test_digest.report {
        Some(test) => {
            println!("  runner:                 {}", test.runner);
            println!("  failures:               {}", test.failures);
            println!("  failure groups:         {}", test.grouped_failures);
            for failure in &test.failure_groups {
                let location = match (&failure.path, failure.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}"),
                    (Some(path), None) => path.clone(),
                    _ => "(no file anchor)".to_string(),
                };
                println!(
                    "  - {} count:{} msg:{}",
                    location, failure.occurrences, failure.message
                );
                if !failure.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        failure
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
        }
        None => println!("  capture:                {}", report.test_digest.command),
    }

    println!();
    println!("Log digest");
    println!("  status:                 {}", report.log_digest.status);
    match &report.log_digest.report {
        Some(log) => {
            println!("  non-empty lines:        {}", log.non_empty_lines);
            println!("  signal groups:          {}", log.signal_groups);
            println!("  file refs:              {}", log.file_ref_groups);
            println!("  symbol refs:            {}", log.symbol_ref_groups);
            for signal in &log.signals {
                let location = match (&signal.path, signal.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}"),
                    (Some(path), None) => path.clone(),
                    _ => "(no file anchor)".to_string(),
                };
                println!(
                    "  - {} {} count:{} msg:{}",
                    location, signal.severity, signal.occurrences, signal.message
                );
                if !signal.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        signal
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
            for symbol in &log.symbol_refs {
                println!(
                    "  - symbol: {} count:{} state:{}",
                    format_symbol_preview_line(
                        &symbol.handle,
                        &symbol.symbol,
                        symbol.tag_alias.as_deref()
                    ),
                    symbol.occurrences,
                    symbol.summary_state
                );
                if !symbol.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        symbol
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
        }
        None => println!("  capture:                {}", report.log_digest.command),
    }

    println!();
    println!("Exploration packet");
    println!(
        "  budget:                 {} ({} windows x {} lines)",
        report.exploration.budget.project_size,
        report.exploration.budget.max_source_windows,
        report.exploration.budget.lines_per_window
    );
    for window in &report.exploration.source_windows {
        println!(
            "  - window {}:{}-{} ({})",
            window.file, window.start, window.end, window.reason
        );
        println!("    expand: {}", window.expand);
    }
    for relation in &report.exploration.relationship_map {
        println!(
            "  - relation {} -{}-> {}",
            relation.from, relation.relation, relation.to
        );
    }

    if !report.findings.is_empty() {
        println!();
        println!("Findings (authored why, anchored to the result set)");
        for finding in &report.findings {
            println!(
                "  - [{}] {} (about {} / {})",
                finding.kind, finding.title, finding.about, finding.anchor_kind
            );
            if let Some(confidence) = finding.confidence {
                println!("    confidence: {confidence}");
            }
            println!("    {}", finding.body);
            println!("    expand: {}", finding.expand);
        }
    }

    println!();
    println!("Graph orchestration");
    println!(
        "  projection freshness:   {}",
        report.graph_orchestration.projection_freshness.status
    );
    println!(
        "  readiness:              {} ({})",
        report.graph_orchestration.readiness.status, report.graph_orchestration.readiness.reason
    );
    for diagnostic in &report.graph_orchestration.readiness.diagnostics {
        println!("  - readiness diagnostic: {diagnostic}");
    }
    for evidence in &report.graph_orchestration.evidence_packet_ids {
        println!("  - evidence: {evidence}");
    }
    for decision in &report.graph_orchestration.conflict_matrix_decisions {
        println!("  - decision: {decision}");
    }
    for block in &report.graph_orchestration.worker_ownership_blocks {
        println!("  - ownership: {block}");
    }
    for command in &report.graph_orchestration.follow_up_commands {
        println!("  - next: {command}");
    }

    println!();
    println!("Resume commands:");
    for command in &report.resume_commands {
        println!("  - {}", command);
    }
}
