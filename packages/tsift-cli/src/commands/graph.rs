use std::path::Path;

use anyhow::Result;

use crate::cli::TraverseFormat;
use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    CommunityDetectionReport, EdgeSide, TagpathAnnotationDiagnostic, TagpathSearchOpts,
    abbreviate_kind, annotate_communities_with_tagpath, annotate_path_nodes_with_tagpath,
    annotate_stored_edges_with_tagpath, annotate_stored_symbols_with_tagpath,
    build_explain_budget_report, build_traversal_graph, community_tagpath_cache_part,
    compact_members, detect_communities_cached, envelope_metric, format_edge_groups,
    inject_tagpath_stale_into_json, open_index_db, print_explain_budget_human,
    print_json_or_envelope, query_tagpath_root, relativize_edges, relativize_symbols,
    shell_quote, should_collapse_edge_groups, symbol_path_summary, to_json_schema,
    traversal_report, traversal_report_html, update_community_annotation_diagnostics,
    verify_convex_projection_snapshot,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_graph(
    symbol: &str,
    path: &std::path::Path,
    callers: bool,
    callees: bool,
    scope: Option<&str>,
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
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;
    let db = open_index_db(path, scope)?;

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

    if callers || show_both {
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
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        if truncated {
            edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let mut out = serde_json::json!({
                    "callers": edges,
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
                println!("{}", to_json_schema(&out, pretty, terse, schema)?);
            }
        } else if tabular {
            println!("direction\tname\tfile\tline");
            for edge in &edges {
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
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
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
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
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

    if callees || show_both {
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
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        if truncated {
            edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let mut out = serde_json::json!({
                    "callees": edges,
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
                println!("{}", to_json_schema(&out, pretty, terse, schema)?);
            }
        } else if tabular {
            if !show_both {
                println!("direction\tname\tfile\tline");
            }
            for edge in &edges {
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
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
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
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
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
        let mut callers_edges = db.callers_of(symbol)?;
        let mut callees_edges = db.callees_of(symbol)?;
        let caller_diag = annotate_stored_edges_with_tagpath(
            &mut callers_edges,
            &db,
            &root,
            scope,
            EdgeSide::Caller,
            &tagpath_opts,
        )?;
        let callee_diag = annotate_stored_edges_with_tagpath(
            &mut callees_edges,
            &db,
            &root,
            scope,
            EdgeSide::Callee,
            &tagpath_opts,
        )?;
        maybe_emit_stale_diagnostic(&caller_diag);
        maybe_emit_stale_diagnostic(&callee_diag);
        if !absolute {
            relativize_edges(&mut callers_edges, &root);
            relativize_edges(&mut callees_edges, &root);
        }
        let callers_total = callers_edges.len();
        let callees_total = callees_edges.len();
        let callers_truncated = limit > 0 && callers_total > limit;
        let callees_truncated = limit > 0 && callees_total > limit;
        if callers_truncated {
            callers_edges.truncate(limit);
        }
        if callees_truncated {
            callees_edges.truncate(limit);
        }
        let mut combined = serde_json::json!({
            "symbol": symbol,
            "callers": callers_edges,
            "callers_total": callers_total,
            "callers_truncated": callers_truncated,
            "callees": callees_edges,
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
        println!("{}", to_json_schema(&combined, pretty, terse, schema)?);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_communities(
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
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;
    let tagpath_root = query_tagpath_root(&root, path, scope)?;
    let db = open_index_db(path, scope)?;
    let tagpath_part = community_tagpath_cache_part(&tagpath_root, &tagpath_opts)?;
    let CommunityDetectionReport {
        result,
        mut diagnostics,
    } = detect_communities_cached(&db, &root, scope, &tagpath_part, &tagpath_root)?;
    let mut tagpath_stale = false;
    let mut tagpath_stale_reason: Option<String> = None;

    let filtered: Vec<tsift::graph::Community> = result
        .communities
        .iter()
        .filter(|c| c.members.len() >= min_size)
        .cloned()
        .collect();

    let total = filtered.len();
    let truncated = limit > 0 && total > limit;
    let mut display: Vec<tsift::graph::Community> = if truncated {
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
        println!("{}", to_json_schema(&out, pretty, terse, schema)?);
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
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;
    let graph = build_traversal_graph(&root, path, scope)?;
    if let Some(snapshot) = convex_snapshot {
        verify_convex_projection_snapshot(&root, scope, snapshot)?;
    }
    let report = traversal_report(&root, scope, graph, node, to, depth, limit)?;
    match format {
        TraverseFormat::Json => {
            println!("{}", to_json_schema(&report, pretty, terse, schema)?);
        }
        TraverseFormat::Html => {
            println!("{}", traversal_report_html(&report)?);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_path(
    from: &str,
    to: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    match tsift::graph::shortest_path(&edges, from, to) {
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
                println!("{}", to_json_schema(&value, pretty, terse, schema)?);
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
pub(crate) fn cmd_explain(
    symbol: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    cmd_explain_with_budget(
        symbol,
        path,
        scope,
        limit,
        json_output,
        compact,
        pretty,
        terse,
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
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
    envelope: bool,
    budget: ResponseBudget,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;
    let community_tagpath_root = query_tagpath_root(&root, path, scope)?;
    let format = OutputFormat {
        json_output,
        compact,
        pretty,
        terse,
        schema,
        envelope,
    };
    let db = open_index_db(path, scope)?;

    let mut symbols = db.symbol_info(symbol)?;
    let mut callers = db.callers_of(symbol)?;
    let mut callees = db.callees_of(symbol)?;

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
    Ok(())
}
