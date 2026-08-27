use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use tsift_algorithms as algorithms;
use tsift_graph as graph;
use tsift_index::index;
use tsift_quality::lint;

use crate::cli::TraverseFormat;
use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    CommunityDetectionReport, EdgeSide, TagpathAnnotationDiagnostic, TagpathSearchOpts,
    abbreviate_kind, annotate_communities_with_tagpath, annotate_path_nodes_with_tagpath,
    annotate_stored_edges_with_tagpath, annotate_stored_symbols_with_tagpath,
    build_explain_budget_report, build_traversal_graph, community_tagpath_cache_part,
    compact_members, detect_communities_cached, envelope_metric, format_edge_groups,
    inject_tagpath_stale_into_json, open_graph_index_db, open_index_db, print_explain_budget_human,
    print_json_or_envelope, query_tagpath_root, relativize_edges, relativize_symbols, shell_quote,
    should_collapse_edge_groups, symbol_path_summary, to_json_schema, traversal_report,
    traversal_report_html, update_community_annotation_diagnostics,
    verify_convex_projection_snapshot,
};

fn normalize_user_facing_edge_lines(edges: &mut [index::StoredEdge]) {
    for edge in edges {
        edge.caller_line = edge.caller_line.saturating_add(1);
        edge.call_site_line = edge.call_site_line.saturating_add(1);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_graph(
    symbol: &str,
    path: &std::path::Path,
    callers: bool,
    callees: bool,
    scope: Option<&str>,
    federated: bool,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    // #graphfed: at a workspace root with no shared root index, resolve which
    // scope owns the symbol instead of failing and telling the caller to supply
    // the scope they were about to ask tsift for.
    let (db, resolved_scope) = open_graph_index_db(path, scope, federated, symbol)?;
    let scope = resolved_scope.as_deref();

    let show_both = !callers && !callees;
    // Shared mutable state for diagnostic aggregation across the per-side
    // annotation passes. Cell-style indirection avoids a closure that
    // mutably borrows these locals (the immutable borrow used by the JSON
    // emit sites would otherwise conflict).
    let tagpath_state = std::cell::RefCell::new((
        false,                  // emitted to stderr yet?
        false,                  // any diag.stale=true?
        Option::<String>::None, // first stale reason
    ));
    let maybe_emit_stale_diagnostic = |diag: &TagpathAnnotationDiagnostic| {
        let mut state = tagpath_state.borrow_mut();
        if diag.stale {
            state.1 = true;
            if state.2.is_none() {
                state.2 = diag.reason.clone();
            }
        }
        if !state.0 && diag.stale && !tagpath_opts.no_tagpath {
            eprintln!(
                "tagpath_index_stale: true (reason={}); falling back to live extraction",
                diag.reason.as_deref().unwrap_or("unknown"),
            );
            state.0 = true;
        }
    };

    let callers_result = if callers || show_both {
        let mut edges = db.callers_of(symbol)?;
        let diag = annotate_stored_edges_with_tagpath(
            &mut edges,
            &db,
            &root,
            scope,
            EdgeSide::Caller,
            &tagpath_opts,
        )?;
        maybe_emit_stale_diagnostic(&diag);
        normalize_user_facing_edge_lines(&mut edges);
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        Some(edges)
    } else {
        None
    };

    let callees_result = if callees || show_both {
        let mut edges = db.callees_of(symbol)?;
        let diag = annotate_stored_edges_with_tagpath(
            &mut edges,
            &db,
            &root,
            scope,
            EdgeSide::Callee,
            &tagpath_opts,
        )?;
        maybe_emit_stale_diagnostic(&diag);
        normalize_user_facing_edge_lines(&mut edges);
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        Some(edges)
    } else {
        None
    };

    if let Some(ref edges) = callers_result {
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        let mut display_edges = edges.clone();
        if truncated {
            display_edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let mut out = serde_json::json!({
                    "callers": display_edges,
                    "total": total,
                    "truncated": truncated,
                });
                {
                    let state = tagpath_state.borrow();
                    inject_tagpath_stale_into_json(
                        &mut out,
                        state.1 && !tagpath_opts.no_tagpath,
                        state.2.as_deref(),
                    );
                }
                println!("{}", to_json_schema(&out, pretty, terse, false, schema)?);
            }
        } else if tabular {
            println!("direction\tname\tfile\tline");
            for edge in &display_edges {
                println!(
                    "caller\t{}\t{}\t{}",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if truncated {
                println!("# (+{} more)", total - limit);
            }
        } else if compact {
            println!("crs[{}]:", total);
            if display_edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &display_edges {
                    println!(
                        "  {} {}:{}",
                        edge.caller_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more)", total - limit);
                }
            }
        } else {
            println!("Callers of `{}`:", symbol);
            if display_edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &display_edges {
                    println!(
                        "  {} ({}:{})",
                        edge.caller_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more, use --limit 0 to show all)", total - limit);
                }
            }
        }
        if show_both && !json_output && !compact && !tabular {
            println!();
        }
    }

    if let Some(ref edges) = callees_result {
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        let mut display_edges = edges.clone();
        if truncated {
            display_edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let mut out = serde_json::json!({
                    "callees": display_edges,
                    "total": total,
                    "truncated": truncated,
                });
                {
                    let state = tagpath_state.borrow();
                    inject_tagpath_stale_into_json(
                        &mut out,
                        state.1 && !tagpath_opts.no_tagpath,
                        state.2.as_deref(),
                    );
                }
                println!("{}", to_json_schema(&out, pretty, terse, false, schema)?);
            }
        } else if tabular {
            if !show_both {
                println!("direction\tname\tfile\tline");
            }
            for edge in &display_edges {
                println!(
                    "callee\t{}\t{}\t{}",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if truncated {
                println!("# (+{} more)", total - limit);
            }
        } else if compact {
            println!("ces[{}]:", total);
            if display_edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &display_edges {
                    println!(
                        "  {} {}:{}",
                        edge.callee_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more)", total - limit);
                }
            }
        } else {
            println!("Callees of `{}`:", symbol);
            if display_edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &display_edges {
                    println!(
                        "  {} ({}:{})",
                        edge.callee_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more, use --limit 0 to show all)", total - limit);
                }
            }
        }
    }

    if show_both && json_output {
        let callers_edges = callers_result.unwrap_or_default();
        let callees_edges = callees_result.unwrap_or_default();
        let callers_total = callers_edges.len();
        let callees_total = callees_edges.len();
        let callers_truncated = limit > 0 && callers_total > limit;
        let callees_truncated = limit > 0 && callees_total > limit;
        let mut callers_display = callers_edges;
        let mut callees_display = callees_edges;
        if callers_truncated {
            callers_display.truncate(limit);
        }
        if callees_truncated {
            callees_display.truncate(limit);
        }
        let mut combined = serde_json::json!({
            "symbol": symbol,
            "callers": callers_display,
            "callers_total": callers_total,
            "callers_truncated": callers_truncated,
            "callees": callees_display,
            "callees_total": callees_total,
            "callees_truncated": callees_truncated,
        });
        {
            let state = tagpath_state.borrow();
            inject_tagpath_stale_into_json(
                &mut combined,
                state.1 && !tagpath_opts.no_tagpath,
                state.2.as_deref(),
            );
        }
        println!(
            "{}",
            to_json_schema(&combined, pretty, terse, false, schema)?
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// `communities` across a workspace's federated scopes (`#wsfedrest`).
///
/// A scoped index holds only its own call edges, so there is no such thing as a
/// cross-scope community: running detection per scope and reporting each is the
/// exact answer, not an approximation of a whole-workspace one. That is why this
/// federates by concatenation rather than by merging graphs.
pub(crate) fn cmd_communities(
    path: &std::path::Path,
    scope: Option<&str>,
    federated: bool,
    min_size: usize,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    tabular: bool,
    schema: bool,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    if federated || crate::should_auto_federate(&root, path, scope, federated)? {
        let scopes = crate::federated_scope_ids(&root)?;
        if scopes.is_empty() {
            anyhow::bail!(
                "workspace root {} has no federated scopes to analyze: every scope opts out of federation. Pass `--scope <scope>` to analyze one directly.",
                root.display()
            );
        }
        if json_output {
            let mut per_scope = Vec::with_capacity(scopes.len());
            for scope_id in &scopes {
                per_scope.push(build_communities_value(
                    path,
                    Some(scope_id),
                    min_size,
                    limit,
                    &tagpath_opts,
                )?);
            }
            let out = serde_json::json!({ "scopes": per_scope });
            println!("{}", to_json_schema(&out, pretty, terse, false, schema)?);
            return Ok(());
        }
        for (idx, scope_id) in scopes.iter().enumerate() {
            if idx > 0 {
                println!();
            }
            println!("scope {scope_id}:");
            cmd_communities_for_scope(
                path,
                Some(scope_id),
                min_size,
                limit,
                false,
                compact,
                pretty,
                terse,
                tabular,
                schema,
                tagpath_opts,
            )?;
        }
        return Ok(());
    }
    cmd_communities_for_scope(
        path,
        scope,
        min_size,
        limit,
        json_output,
        compact,
        pretty,
        terse,
        tabular,
        schema,
        tagpath_opts,
    )
}

/// The JSON body of `cmd_communities_for_scope`, returned instead of printed so
/// a federated run can carry one document per scope (`#wsfedrest`).
fn build_communities_value(
    path: &std::path::Path,
    scope: Option<&str>,
    min_size: usize,
    limit: usize,
    tagpath_opts: &TagpathSearchOpts,
) -> Result<serde_json::Value> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let tagpath_root = query_tagpath_root(&root, path, scope)?;
    let db = open_index_db(path, scope)?;
    let tagpath_part = community_tagpath_cache_part(&tagpath_root, tagpath_opts)?;
    let CommunityDetectionReport {
        result,
        mut diagnostics,
    } = detect_communities_cached(&db, &root, scope, &tagpath_part, &tagpath_root)?;

    let filtered: Vec<graph::Community> = result
        .communities
        .iter()
        .filter(|c| c.members.len() >= min_size)
        .cloned()
        .collect();
    let total = filtered.len();
    let truncated = limit > 0 && total > limit;
    let mut display: Vec<graph::Community> = if truncated {
        filtered[..limit].to_vec()
    } else {
        filtered
    };

    let community_annotation =
        annotate_communities_with_tagpath(&mut display, &db, &tagpath_root, tagpath_opts)?;
    let tagpath_stale = community_annotation.as_ref().is_some_and(|diag| diag.stale);
    let tagpath_stale_reason = community_annotation
        .as_ref()
        .and_then(|diag| diag.reason.clone());
    update_community_annotation_diagnostics(
        &mut diagnostics,
        &display,
        community_annotation.as_ref(),
    );

    let mut out = serde_json::json!({
        "scope": scope,
        "modularity": result.modularity,
        "iterations": result.iterations,
        "node_count": result.node_count,
        "edge_count": result.edge_count,
        "community_count": total,
        "communities": &display,
        "truncated": truncated,
        "community_diagnostics": diagnostics,
    });
    inject_tagpath_stale_into_json(
        &mut out,
        tagpath_stale && !tagpath_opts.no_tagpath,
        tagpath_stale_reason.as_deref(),
    );
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn cmd_communities_for_scope(
    path: &std::path::Path,
    scope: Option<&str>,
    min_size: usize,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    tabular: bool,
    schema: bool,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let tagpath_root = query_tagpath_root(&root, path, scope)?;
    let db = open_index_db(path, scope)?;
    let tagpath_part = community_tagpath_cache_part(&tagpath_root, &tagpath_opts)?;
    let CommunityDetectionReport {
        result,
        mut diagnostics,
    } = detect_communities_cached(&db, &root, scope, &tagpath_part, &tagpath_root)?;
    let mut tagpath_stale = false;
    let mut tagpath_stale_reason: Option<String> = None;

    let filtered: Vec<graph::Community> = result
        .communities
        .iter()
        .filter(|c| c.members.len() >= min_size)
        .cloned()
        .collect();

    let total = filtered.len();
    let truncated = limit > 0 && total > limit;
    let mut display: Vec<graph::Community> = if truncated {
        filtered[..limit].to_vec()
    } else {
        filtered
    };

    let community_annotation =
        annotate_communities_with_tagpath(&mut display, &db, &tagpath_root, &tagpath_opts)?;
    if let Some(diag) = community_annotation.as_ref() {
        if diag.stale && !tagpath_opts.no_tagpath {
            eprintln!(
                "tagpath_index_stale: true (reason={}); falling back to live extraction",
                diag.reason.as_deref().unwrap_or("unknown"),
            );
        }
        if diag.stale {
            tagpath_stale = true;
            tagpath_stale_reason = diag.reason.clone();
        }
    }
    update_community_annotation_diagnostics(
        &mut diagnostics,
        &display,
        community_annotation.as_ref(),
    );

    if json_output {
        let mut out = serde_json::json!({
            "modularity": result.modularity,
            "iterations": result.iterations,
            "node_count": result.node_count,
            "edge_count": result.edge_count,
            "community_count": total,
            "communities": &display,
            "truncated": truncated,
            "community_diagnostics": diagnostics,
        });
        inject_tagpath_stale_into_json(
            &mut out,
            tagpath_stale && !tagpath_opts.no_tagpath,
            tagpath_stale_reason.as_deref(),
        );
        println!("{}", to_json_schema(&out, pretty, terse, false, schema)?);
    } else if tabular {
        println!("id\tsize\tmembers");
        for (i, community) in display.iter().enumerate() {
            let names: Vec<&str> = community.members.iter().map(|m| m.name.as_str()).collect();
            println!(
                "{}\t{}\t{}",
                i + 1,
                community.members.len(),
                names.join(",")
            );
        }
        if truncated {
            println!("# (+{} more)", total - limit);
        }
    } else if compact {
        println!(
            "comms n:{} e:{} iter:{} q:{:.4} cnt:{}",
            result.node_count, result.edge_count, result.iterations, result.modularity, total
        );
        if display.is_empty() {
            println!("  (none >= {})", min_size);
        } else {
            for (i, community) in display.iter().enumerate() {
                println!(
                    "  {}. {} mbrs {}",
                    i + 1,
                    community.members.len(),
                    compact_members(&community.members, 5)
                );
            }
            if truncated {
                println!("  (+{} more)", total - limit);
            }
        }
    } else {
        println!(
            "Communities ({} nodes, {} edges, {} iterations, Q={:.4})",
            result.node_count, result.edge_count, result.iterations, result.modularity
        );
        if display.is_empty() {
            println!("  (no communities with {} or more members)", min_size);
        } else {
            println!();
            for (i, c) in display.iter().enumerate() {
                println!(
                    "  [{}] {} members (Q={:.4}):",
                    i + 1,
                    c.members.len(),
                    c.modularity_contribution
                );
                for m in &c.members {
                    match &m.tagpath_handle {
                        Some(handle) => println!("    {}  [{}]", m.name, handle),
                        None => println!("    {}", m.name),
                    }
                }
                if i + 1 < display.len() {
                    println!();
                }
            }
            if truncated {
                println!();
                println!(
                    "  (+{} more communities, use --limit 0 to show all)",
                    total - limit
                );
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct GraphAnalysisReport {
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    node_count: usize,
    edge_count: usize,
    symbol_count: usize,
    entry_points: Vec<String>,
    scc: algorithms::SccResult,
    health: algorithms::HealthReport,
    dead_code: algorithms::DeadCodeResult,
    coupling: algorithms::CouplingReport,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
}

pub(crate) fn cmd_analyze(
    path: &Path,
    scope: Option<&str>,
    entry_points: &[String],
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    let symbols = db.all_symbols()?;
    let mut module_of = analysis_module_map(&symbols);
    for (from, to) in &edges {
        module_of
            .entry(from.clone())
            .or_insert_with(|| "unknown".to_string());
        module_of
            .entry(to.clone())
            .or_insert_with(|| "unknown".to_string());
    }
    let entries = if entry_points.is_empty() {
        default_analysis_entry_points(&edges)
    } else {
        dedupe_analysis_strings(entry_points.iter().cloned())
    };
    let mut warnings = Vec::new();
    if edges.is_empty() {
        warnings
            .push("call graph is empty; run `tsift index` after adding code symbols".to_string());
    }
    if entries.is_empty() {
        warnings.push(
            "dead-code reachability has no entry point; pass --entry <symbol> for stricter results"
                .to_string(),
        );
    }

    let node_count = analysis_node_count(&edges);
    let report = GraphAnalysisReport {
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        node_count,
        edge_count: edges.len(),
        symbol_count: symbols.len(),
        entry_points: entries.clone(),
        scc: algorithms::tarjan_scc(&edges),
        health: algorithms::composite_health_score(&edges),
        dead_code: algorithms::detect_dead_code(&edges, &entries),
        coupling: algorithms::coupling_analysis(&edges, &module_of),
        warnings,
    };

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "analyze",
            "graph",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph analysis ran {} edge(s) through SCC, health, dead-code, and coupling algorithms",
                    report.edge_count
                ),
                metrics: vec![
                    envelope_metric("nodes", report.node_count),
                    envelope_metric("edges", report.edge_count),
                    envelope_metric("non_trivial_scc", report.scc.non_trivial_count),
                    envelope_metric("dead_nodes", report.dead_code.dead_count),
                    envelope_metric("modules", report.coupling.total_modules),
                ],
            },
            false,
            vec![format!(
                "Use `tsift graph --path {} <symbol>` or `tsift explain --path {} <symbol>` to inspect individual call edges",
                shell_quote(root.to_string_lossy().as_ref()),
                shell_quote(root.to_string_lossy().as_ref())
            )],
        )
    } else {
        print_graph_analysis_human(&report, limit, format.compact);
        Ok(())
    }
}

fn dedupe_analysis_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

fn analysis_node_count(edges: &[(String, String)]) -> usize {
    edges
        .iter()
        .flat_map(|(from, to)| [from.as_str(), to.as_str()])
        .collect::<BTreeSet<_>>()
        .len()
}

fn default_analysis_entry_points(edges: &[(String, String)]) -> Vec<String> {
    let mut nodes = BTreeSet::new();
    let mut inbound = BTreeSet::new();
    let mut outbound = BTreeSet::new();
    for (from, to) in edges {
        nodes.insert(from.clone());
        nodes.insert(to.clone());
        outbound.insert(from.clone());
        inbound.insert(to.clone());
    }

    let preferred = nodes
        .iter()
        .filter(|node| matches!(node.as_str(), "main" | "run" | "start" | "init"))
        .cloned()
        .collect::<Vec<_>>();
    if !preferred.is_empty() {
        return preferred;
    }

    let roots = outbound
        .difference(&inbound)
        .cloned()
        .collect::<Vec<String>>();
    if !roots.is_empty() {
        return roots;
    }

    nodes.into_iter().next().into_iter().collect()
}

fn analysis_module_map(symbols: &[index::StoredSymbol]) -> HashMap<String, String> {
    let mut module_of = HashMap::new();
    for symbol in symbols {
        module_of
            .entry(symbol.name.clone())
            .or_insert_with(|| analysis_module_name(symbol));
    }
    module_of
}

fn analysis_module_name(symbol: &index::StoredSymbol) -> String {
    if let Some(module) = symbol
        .parent_module
        .as_ref()
        .filter(|module| !module.trim().is_empty())
    {
        return module.clone();
    }
    let path = Path::new(&symbol.file);
    if let Some(parent) = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .filter(|parent| !parent.is_empty())
    {
        return parent;
    }
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn print_graph_analysis_human(report: &GraphAnalysisReport, limit: usize, compact: bool) {
    if compact {
        println!(
            "analysis nodes:{} edges:{} scc:{} cycles:{} health:{:.3} dead:{} modules:{}",
            report.node_count,
            report.edge_count,
            report.scc.total_components,
            report.scc.non_trivial_count,
            report.health.avg_overall,
            report.dead_code.dead_count,
            report.coupling.total_modules
        );
        return;
    }

    println!(
        "Graph analysis ({} node(s), {} edge(s), {} indexed symbol(s))",
        report.node_count, report.edge_count, report.symbol_count
    );
    println!("entry_points: {}", report.entry_points.join(", "));
    println!(
        "scc: {} component(s), {} non-trivial, largest {}",
        report.scc.total_components,
        report.scc.non_trivial_count,
        report.scc.largest_component_size
    );
    println!(
        "health: avg_overall {:.3}, avg_cycle_risk {:.3}",
        report.health.avg_overall, report.health.avg_cycle_risk
    );
    println!(
        "dead_code: {} / {} node(s) ({:.1}%)",
        report.dead_code.dead_count,
        report.dead_code.total_nodes,
        report.dead_code.dead_ratio * 100.0
    );
    println!(
        "coupling: {} module(s), max fan-out {}, max fan-in {}",
        report.coupling.total_modules, report.coupling.max_fan_out, report.coupling.max_fan_in
    );

    let display_limit = |len: usize| if limit == 0 { len } else { len.min(limit) };
    let scc_limit = display_limit(report.scc.components.len());
    if scc_limit > 0 {
        println!();
        println!("largest_scc:");
        for component in report.scc.components.iter().take(scc_limit) {
            if component.is_trivial {
                continue;
            }
            println!(
                "  {} node(s): {}",
                component.size,
                component.nodes.join(", ")
            );
        }
    }
    let dead_limit = display_limit(report.dead_code.dead_nodes.len());
    if dead_limit > 0 {
        println!();
        println!("dead_nodes:");
        for node in report.dead_code.dead_nodes.iter().take(dead_limit) {
            println!(
                "  {} ({}, in:{}, out:{})",
                node.name, node.reason, node.inbound_count, node.outbound_count
            );
        }
    }
    let module_limit = display_limit(report.coupling.modules.len());
    if module_limit > 0 {
        println!();
        println!("coupled_modules:");
        for module in report.coupling.modules.iter().take(module_limit) {
            println!(
                "  {} fan_out:{} fan_in:{} instability:{:.3}",
                module.module, module.fan_out, module.fan_in, module.instability
            );
        }
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_traverse(
    node: Option<&str>,
    to: Option<&str>,
    path: &Path,
    scope: Option<&str>,
    depth: usize,
    limit: usize,
    format: TraverseFormat,
    pretty: bool,
    terse: bool,
    schema: bool,
    convex_snapshot: Option<&Path>,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let graph = build_traversal_graph(&root, path, scope)?;
    if let Some(snapshot) = convex_snapshot {
        verify_convex_projection_snapshot(&root, scope, snapshot)?;
    }
    let report = traversal_report(&root, scope, graph, node, to, depth, limit)?;
    match format {
        TraverseFormat::Json => {
            println!("{}", to_json_schema(&report, pretty, terse, false, schema)?);
        }
        TraverseFormat::Html => {
            println!("{}", traversal_report_html(&report)?);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// `path` at a workspace root resolves *both* endpoints (`#wsfedrest`).
///
/// A scoped index carries only its own call edges, so a path between symbols in
/// two different scopes does not exist to be found — there is no edge that could
/// cross. When both endpoints resolve to the same scope the query is answerable
/// and runs there; when they resolve to different scopes the answer is a precise
/// refusal naming both, not a silent empty result.
pub(crate) fn cmd_path(
    from: &str,
    to: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    federated: bool,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let scope_owned = if federated || crate::should_auto_federate(&root, path, scope, federated)? {
        let from_scope = crate::resolve_symbol_scope(&root, from)?;
        let to_scope = crate::resolve_symbol_scope(&root, to)?;
        match (from_scope, to_scope) {
            (Some(a), Some(b)) if a.scope == b.scope => Some(a.scope),
            (Some(a), Some(b)) => anyhow::bail!(
                "`{from}` resolves in scope `{}` and `{to}` in scope `{}`. A scoped index holds only its own call edges, so no path between them exists to find. Query one scope with `--scope <scope>`.",
                a.scope,
                b.scope
            ),
            (None, _) => anyhow::bail!(
                "symbol `{from}` was not found in any federated scope of {}. Searched: {}.",
                root.display(),
                crate::federated_scope_ids(&root)?.join(", ")
            ),
            (_, None) => anyhow::bail!(
                "symbol `{to}` was not found in any federated scope of {}. Searched: {}.",
                root.display(),
                crate::federated_scope_ids(&root)?.join(", ")
            ),
        }
    } else {
        scope.map(str::to_string)
    };
    let scope = scope_owned.as_deref();
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    match graph::shortest_path(&edges, from, to) {
        Some(mut result) => {
            let tagpath_diag =
                annotate_path_nodes_with_tagpath(&mut result.path, &db, &root, &tagpath_opts)?;
            if tagpath_diag.stale && !tagpath_opts.no_tagpath {
                eprintln!(
                    "tagpath_index_stale: true (reason={}); falling back to live extraction",
                    tagpath_diag.reason.as_deref().unwrap_or("unknown"),
                );
            }
            if json_output {
                let mut value = serde_json::to_value(&result)?;
                inject_tagpath_stale_into_json(
                    &mut value,
                    tagpath_diag.stale && !tagpath_opts.no_tagpath,
                    tagpath_diag.reason.as_deref(),
                );
                println!("{}", to_json_schema(&value, pretty, terse, false, schema)?);
            } else if compact {
                println!(
                    "{} ({} hop{})",
                    symbol_path_summary(&result.path),
                    result.hops,
                    if result.hops == 1 { "" } else { "s" }
                );
            } else {
                println!(
                    "{} → {} ({} hop{})",
                    result.from,
                    result.to,
                    result.hops,
                    if result.hops == 1 { "" } else { "s" }
                );
                println!();
                for (i, node) in result.path.iter().enumerate() {
                    if i > 0 {
                        println!("  ↓");
                    }
                    match &node.tagpath_handle {
                        Some(handle) => println!("  {}  [{}]", node.name, handle),
                        None => println!("  {}", node.name),
                    }
                }
            }
        }
        None => {
            if json_output {
                println!(
                    "{}",
                    to_json_schema(
                        &serde_json::json!({
                            "from": from,
                            "to": to,
                            "path": null,
                            "hops": null,
                        }),
                        pretty,
                        terse,
                        false,
                        schema
                    )?
                );
            } else if compact {
                println!("no path {} -> {}", from, to);
            } else {
                println!("No path found between `{}` and `{}`.", from, to);
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_explain(
    symbol: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    federated: bool,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    ultra_terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    cmd_explain_with_budget(
        symbol,
        path,
        scope,
        federated,
        limit,
        json_output,
        compact,
        pretty,
        terse,
        ultra_terse,
        absolute,
        tabular,
        schema,
        false,
        ResponseBudget::default(),
        TagpathSearchOpts::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_explain_with_budget(
    symbol: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    federated: bool,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    ultra_terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
    envelope: bool,
    budget: ResponseBudget,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    // #graphfed: resolve the owning scope first so the tagpath root, the
    // community cache key, and the opened index all agree on which scope
    // answered.
    let (db, resolved_scope) = open_graph_index_db(path, scope, federated, symbol)?;
    let scope = resolved_scope.as_deref();
    let community_tagpath_root = query_tagpath_root(&root, path, scope)?;
    let format = OutputFormat {
        json_output,
        compact,
        pretty,
        terse,
        ultra_terse,
        schema,
        envelope,
    };

    let mut symbols = db.symbol_info(symbol)?;
    let mut callers = db.callers_of(symbol)?;
    let mut callees = db.callees_of(symbol)?;

    if scope.is_some() && symbols.is_empty() && callers.is_empty() && callees.is_empty() {
        let scope_hint = scope
            .map(|scope| format!(" in scope `{scope}`"))
            .unwrap_or_default();
        anyhow::bail!(
            "symbol `{symbol}` was not found{scope_hint}. Run `tsift index --workspace {}` if the index is stale, or choose another `--scope`.",
            root.display()
        );
    }

    // Annotate against absolute paths so the tagpath adapter can resolve
    // each symbol's source file. Relativization happens after annotation.
    let def_diag = annotate_stored_symbols_with_tagpath(&mut symbols, &root, &tagpath_opts)?;
    let caller_diag = annotate_stored_edges_with_tagpath(
        &mut callers,
        &db,
        &root,
        scope,
        EdgeSide::Caller,
        &tagpath_opts,
    )?;
    let callee_diag = annotate_stored_edges_with_tagpath(
        &mut callees,
        &db,
        &root,
        scope,
        EdgeSide::Callee,
        &tagpath_opts,
    )?;
    // Stored symbol rows use tree-sitter's zero-based coordinates. Every
    // user-facing read command uses one-based lines, so normalize both ends
    // before explain builds any human, JSON, tabular, or budgeted projection.
    for symbol in &mut symbols {
        symbol.line = symbol.line.saturating_add(1);
        symbol.end_line = symbol.end_line.map(|end_line| end_line.saturating_add(1));
    }
    normalize_user_facing_edge_lines(&mut callers);
    normalize_user_facing_edge_lines(&mut callees);
    let mut tagpath_stale = def_diag.stale || caller_diag.stale || callee_diag.stale;
    let mut tagpath_stale_reason = def_diag
        .reason
        .or(caller_diag.reason)
        .or(callee_diag.reason);
    if tagpath_stale && !tagpath_opts.no_tagpath {
        eprintln!(
            "tagpath_index_stale: true (reason={}); falling back to live extraction",
            tagpath_stale_reason.as_deref().unwrap_or("unknown"),
        );
    }
    if !absolute {
        relativize_symbols(&mut symbols, &root);
        relativize_edges(&mut callers, &root);
        relativize_edges(&mut callees, &root);
    }

    let callers_total = callers.len();
    let callees_total = callees.len();
    let callers_truncated = limit > 0 && callers_total > limit;
    let callees_truncated = limit > 0 && callees_total > limit;
    if callers_truncated {
        callers.truncate(limit);
    }
    if callees_truncated {
        callees.truncate(limit);
    }

    let tagpath_part = community_tagpath_cache_part(&community_tagpath_root, &tagpath_opts)?;
    let CommunityDetectionReport {
        result: comm_result,
        diagnostics: mut community_diagnostics,
    } = detect_communities_cached(&db, &root, scope, &tagpath_part, &community_tagpath_root)?;
    let mut focused_community = comm_result
        .communities
        .iter()
        .find(|c| c.members.iter().any(|m| m.name == symbol))
        .cloned();
    if let Some(community) = focused_community.as_mut() {
        let community_slice = std::slice::from_mut(community);
        let community_annotation = annotate_communities_with_tagpath(
            community_slice,
            &db,
            &community_tagpath_root,
            &tagpath_opts,
        )?;
        if let Some(comm_diag) = community_annotation.as_ref() {
            if comm_diag.stale && !tagpath_opts.no_tagpath && !tagpath_stale {
                eprintln!(
                    "tagpath_index_stale: true (reason={}); falling back to live extraction",
                    comm_diag.reason.as_deref().unwrap_or("unknown"),
                );
            }
            if comm_diag.stale {
                tagpath_stale = true;
                if tagpath_stale_reason.is_none() {
                    tagpath_stale_reason = comm_diag.reason.clone();
                }
            }
        }
        update_community_annotation_diagnostics(
            &mut community_diagnostics,
            community_slice,
            community_annotation.as_ref(),
        );
    }

    // #trt1p2b hot-path injection: fold trusted, fresh findings concerning the
    // explain result set (focused symbol, displayed callers/callees, community
    // members) into the envelope, reusing the Phase 2 trusted+fresh contract.
    let result_set_findings = {
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        keys.insert(symbol.to_string());
        for edge in &callers {
            keys.insert(edge.caller_name.clone());
        }
        for edge in &callees {
            keys.insert(edge.callee_name.clone());
        }
        if let Some(community) = focused_community.as_ref() {
            for member in &community.members {
                keys.insert(member.name.clone());
            }
        }
        crate::commands::finding::collect_result_set_finding_previews(&root, &keys, scope, 10, 240)
    };

    let combined_stale = tagpath_stale && !tagpath_opts.no_tagpath;
    if budget.is_active() {
        let report = build_explain_budget_report(
            symbol,
            &root,
            &symbols,
            &callers,
            callers_total,
            callers_truncated,
            &callees,
            callees_total,
            callees_truncated,
            focused_community.as_ref(),
            budget,
        );
        if format.json_output {
            let mut value = serde_json::to_value(&report)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "community_diagnostics".to_string(),
                    serde_json::to_value(&community_diagnostics)?,
                );
                if !result_set_findings.is_empty() {
                    obj.insert(
                        "findings".to_string(),
                        serde_json::to_value(&result_set_findings)?,
                    );
                }
            }
            inject_tagpath_stale_into_json(
                &mut value,
                combined_stale,
                tagpath_stale_reason.as_deref(),
            );
            print_json_or_envelope(
                &value,
                &format,
                "explain",
                "preview",
                ToolEnvelopeSummary {
                    text: format!("explain preview for {}", symbol),
                    metrics: vec![
                        envelope_metric("definitions", report.definition_total),
                        envelope_metric("callers", report.callers_total),
                        envelope_metric("callees", report.callees_total),
                    ],
                },
                report.truncated,
                vec![format!(
                    "tsift explain {} --path {} --limit 0{}",
                    shell_quote(symbol),
                    shell_quote(path.to_string_lossy().as_ref()),
                    scope
                        .map(|value| format!(" --scope {}", shell_quote(value)))
                        .unwrap_or_default()
                )],
            )?;
        } else {
            print_explain_budget_human(&report);
        }
    } else if format.json_output {
        let mut out = serde_json::json!({
            "symbol": symbol,
            "definitions": symbols,
            "callers": callers,
            "callers_total": callers_total,
            "callers_truncated": callers_truncated,
            "callees": callees,
            "callees_total": callees_total,
            "callees_truncated": callees_truncated,
            "community": focused_community.as_ref(),
            "community_diagnostics": community_diagnostics,
        });
        if !result_set_findings.is_empty()
            && let Some(obj) = out.as_object_mut()
        {
            obj.insert(
                "findings".to_string(),
                serde_json::to_value(&result_set_findings)?,
            );
        }
        inject_tagpath_stale_into_json(&mut out, combined_stale, tagpath_stale_reason.as_deref());
        print_json_or_envelope(
            &out,
            &format,
            "explain",
            "report",
            ToolEnvelopeSummary {
                text: format!("explain results for {}", symbol),
                metrics: vec![
                    envelope_metric("definitions", symbols.len()),
                    envelope_metric("callers", callers_total),
                    envelope_metric("callees", callees_total),
                ],
            },
            callers_truncated || callees_truncated,
            vec![format!(
                "tsift explain {} --path {} --limit 0{}",
                shell_quote(symbol),
                shell_quote(path.to_string_lossy().as_ref()),
                scope
                    .map(|value| format!(" --scope {}", shell_quote(value)))
                    .unwrap_or_default()
            )],
        )?;
    } else if tabular {
        if !symbols.is_empty() {
            println!("section\tkind\tname\tfile\tline");
            for sym in &symbols {
                println!(
                    "def\t{}\t{}\t{}\t{}",
                    sym.kind, sym.name, sym.file, sym.line
                );
            }
        }
        if !callers.is_empty() {
            if !symbols.is_empty() {
                println!();
            }
            println!("direction\tname\tfile\tline");
            for edge in &callers {
                println!(
                    "caller\t{}\t{}\t{}",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if callers_truncated {
                println!("# (+{} more callers)", callers_total - limit);
            }
        }
        if !callees.is_empty() {
            for edge in &callees {
                println!(
                    "callee\t{}\t{}\t{}",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if callees_truncated {
                println!("# (+{} more callees)", callees_total - limit);
            }
        }
        if let Some(comm) = focused_community.as_ref() {
            println!();
            let names: Vec<&str> = comm.members.iter().map(|m| m.name.as_str()).collect();
            println!("community\t{}\t{}", comm.members.len(), names.join(","));
        }
    } else if compact {
        if symbols.is_empty() {
            println!("sym: {} (defs: none)", symbol);
        } else {
            for sym in &symbols {
                println!(
                    "sym: {} ({}) {}:{}",
                    sym.name,
                    abbreviate_kind(&sym.kind),
                    sym.file,
                    sym.line
                );
            }
        }

        println!("crs[{}]:", callers_total);
        if callers.is_empty() {
            println!("  (none)");
        } else {
            for line in format_edge_groups(&callers, true) {
                println!("{line}");
            }
            if callers_truncated {
                println!("  (+{} more)", callers_total - limit);
            }
        }

        println!("ces[{}]:", callees_total);
        if callees.is_empty() {
            println!("  (none)");
        } else {
            for line in format_edge_groups(&callees, false) {
                println!("{line}");
            }
            if callees_truncated {
                println!("  (+{} more)", callees_total - limit);
            }
        }

        if let Some(comm) = focused_community.as_ref() {
            println!(
                "comm[{}]: {}",
                comm.members.len(),
                compact_members(&comm.members, 5)
            );
        }
    } else {
        if symbols.is_empty() {
            println!("Symbol `{}` not found in index.", symbol);
            println!("(Checking call graph for references...)");
            println!();
        } else {
            for sym in &symbols {
                println!("{} ({}, {})", sym.name, sym.kind, sym.language);
                println!("  {}:{}", sym.file, sym.line);
            }
            println!();
        }

        println!("Callers ({}):", callers_total);
        if callers.is_empty() {
            println!("  (none)");
        } else if should_collapse_edge_groups(&callers) {
            for line in format_edge_groups(&callers, true) {
                println!("{line}");
            }
            if callers_truncated {
                println!(
                    "  (+{} more callers, use --limit 0 to show all)",
                    callers_total - limit
                );
            }
        } else {
            for edge in &callers {
                println!(
                    "  {} ({}:{})",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if callers_truncated {
                println!(
                    "  (+{} more, use --limit 0 to show all)",
                    callers_total - limit
                );
            }
        }
        println!();

        println!("Callees ({}):", callees_total);
        if callees.is_empty() {
            println!("  (none)");
        } else if should_collapse_edge_groups(&callees) {
            for line in format_edge_groups(&callees, false) {
                println!("{line}");
            }
            if callees_truncated {
                println!(
                    "  (+{} more callees, use --limit 0 to show all)",
                    callees_total - limit
                );
            }
        } else {
            for edge in &callees {
                println!(
                    "  {} ({}:{})",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if callees_truncated {
                println!(
                    "  (+{} more, use --limit 0 to show all)",
                    callees_total - limit
                );
            }
        }

        if let Some(comm) = focused_community.as_ref() {
            println!();
            println!("Community {} ({} members):", comm.id, comm.members.len());
            for m in &comm.members {
                let marker = if m.name == symbol { "→ " } else { "  " };
                match &m.tagpath_handle {
                    Some(handle) => println!("{}{}  [{}]", marker, m.name, handle),
                    None => println!("{}{}", marker, m.name),
                }
            }
        }
    }
    // #trt1p2b: authored findings for the result set, in non-JSON output.
    if !format.json_output && !result_set_findings.is_empty() {
        println!();
        println!("Findings (authored why, anchored to the result set):");
        for finding in &result_set_findings {
            println!(
                "  [{}] {} (about {})",
                finding.kind, finding.title, finding.about
            );
        }
    }
    Ok(())
}
