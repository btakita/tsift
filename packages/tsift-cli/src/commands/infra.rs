use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use substrate::{
    ConvexGraphStore as SubstrateConvexGraphStore, ConvexProjectionRows, ConvexRowsGraphClient,
    SqliteGraphStore,
};
use tsift_index::init;
use tsift_quality::lint;
use tsift_sqlite as substrate;
use tsift_status::status;

use crate::cli::{GraphDbBackend, GraphDbQuery};
use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{
    ConvexHttpTransport, ConvexSyncOptions, EditBatch, EditResult, EditStatus,
    GRAPH_DB_BACKEND_EVAL_DIRECT_PATH_HOPS, GRAPH_DB_BACKEND_EVAL_EXTENDED_PATH_HOPS,
    GRAPH_DB_BACKEND_EVAL_NORMALIZATION_ROW_UNIT, GRAPH_DB_BACKEND_EVAL_PATH_MAX_HOPS,
    GRAPH_PROJECTION_VERSION, GraphDbBackendEvalConfig, GraphDbBackendEvalOptions,
    GraphDbBackendEvalPhaseTiming, GraphDbBackendEvalReport, GraphDbCompactionReport,
    GraphDbDoctorReport, GraphDbDriftInput, GraphDbEvidenceInput, GraphDbExperimentalBackend,
    GraphDbRefreshSummary, append_convex_snapshot_doctor_checks,
    append_graph_db_backend_eval_normalized_duration_metric,
    append_graph_db_backend_eval_phase_metrics, append_sqlite_graph_doctor_checks,
    append_tokensave_graph_doctor_checks, apply_edit_plan_atomically, apply_rewrite_output_format,
    apply_status_fixes, autoindex_missing_workspace_scopes, build_convex_sync_report_with_snapshot,
    build_edit_plan, classify_task, convex_graph_freshness, convex_rows_from_graph_store,
    dedupe_preserve_order, envelope_metric, execute_query, execute_rewritten_command,
    graph_db_backend_eval_cached_refresh, graph_db_backend_eval_dataset,
    graph_db_backend_eval_full_projection_with_profile, graph_db_backend_eval_graph_rows,
    graph_db_backend_eval_metric_digest_command, graph_db_backend_eval_metrics,
    graph_db_backend_eval_performance_gate, graph_db_backend_eval_phase_timing,
    graph_db_backend_eval_promotion, graph_db_backend_eval_refresh_operation,
    graph_db_backend_eval_refresh_total_micros, graph_db_backend_eval_refresh_with_profile,
    graph_db_backend_eval_reused_cached_projection, graph_db_backend_eval_synthetic_projection,
    graph_db_backend_eval_targets, graph_db_backend_eval_timed_phase,
    graph_db_backend_eval_update_source_watermark, graph_db_compaction_policy,
    graph_db_drift_report, graph_db_evidence_report_from_store, graph_db_operator_report_from_disk,
    graph_db_operator_status_warnings, graph_db_read_recovery_diagnostic,
    graph_db_report_from_store, graph_db_resolve_evidence_target, graph_db_scope_arg,
    graph_projection_content_hash, graph_substrate_db_path, load_convex_projection_rows,
    load_convex_projection_snapshot_value, no_rewrite_message, open_db,
    prepare_conflict_matrix_inputs, print_convex_sync_human, print_graph_db_backend_eval_human,
    print_graph_db_compaction_human, print_graph_db_doctor_human, print_graph_db_drift_human,
    print_graph_db_evidence_report, print_graph_db_human, print_graph_db_operator_report,
    print_json_or_envelope, rewrite_command, schema_overview, shell_quote,
    sqlite_convex_rows_from_conn, sqlite_graph_freshness, status_missing_workspace_scopes,
    table_columns, to_json_schema, tokensave_graph_freshness, traversal_source_watermark,
    truncate_for_compact, validate_convex_projection_rows, write_traversal_graph_store,
    write_traversal_graph_store_with_options,
};
use tsift_tokensave::TokensaveDb;

pub(crate) fn cmd_route(task: &str, id_only: bool) -> Result<()> {
    let (tier, model_id) = classify_task(task);
    if id_only {
        println!("{}", model_id);
    } else {
        println!("tier:  {}", tier);
        println!("model: {}", model_id);
        println!("task:  {}", task);
    }
    Ok(())
}

pub(crate) fn cmd_edit(
    dry_run: bool,
    file: Option<PathBuf>,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let input = match file {
        Some(path) => fs::read_to_string(&path)
            .with_context(|| format!("reading edit file: {}", path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading edits from stdin")?;
            buf
        }
    };
    let batch: EditBatch = serde_json::from_str(&input).context("parsing edit JSON")?;

    if batch.edits.is_empty() {
        println!("No edits provided.");
        return Ok(());
    }

    let plan = build_edit_plan(&batch)?;
    let results: Vec<EditResult> = if dry_run {
        plan.iter()
            .map(|entry| EditResult {
                file: entry.file.clone(),
                status: EditStatus::Skipped,
                error: Some("dry run".into()),
                replacements: Some(entry.replacements),
            })
            .collect()
    } else {
        apply_edit_plan_atomically(plan)?
    };

    // Summary output
    let ok_count = results
        .iter()
        .filter(|r| matches!(r.status, EditStatus::Ok))
        .count();
    let skip_count = results
        .iter()
        .filter(|r| matches!(r.status, EditStatus::Skipped))
        .count();
    let err_count = 0usize;

    if compact {
        println!(
            "applied:{} skipped:{} errors:{}",
            ok_count, skip_count, err_count
        );
    } else {
        println!(
            "{}",
            to_json_schema(
                &serde_json::json!({
                    "applied": ok_count,
                    "skipped": skip_count,
                    "errors": err_count,
                    "results": results,
                }),
                pretty,
                terse,
                schema
            )?
        );
    }

    if err_count > 0 {
        bail!("{} edit(s) failed", err_count);
    }
    Ok(())
}

pub(crate) fn cmd_convex_sync(options: ConvexSyncOptions<'_>, format: OutputFormat) -> Result<()> {
    let transport = if options.remote_snapshot || options.apply {
        Some(ConvexHttpTransport::from_options(
            options.endpoint,
            options.auth_token_env,
        )?)
    } else {
        None
    };
    let mut snapshot_diagnostics = Vec::new();
    let snapshot_rows = if options.remote_snapshot {
        let local_report = build_convex_sync_report_with_snapshot(
            options.path,
            options.scope,
            None,
            options.chunk_size,
            true,
        )?;
        let local_rows = ConvexProjectionRows {
            nodes: local_report.node_upserts.clone(),
            edges: local_report.edge_upserts.clone(),
        };
        let (rows, diagnostics) = transport
            .as_ref()
            .expect("transport is initialized when remote_snapshot is set")
            .fetch_snapshot(
                GRAPH_PROJECTION_VERSION,
                options.scope,
                local_report.projection_hash.as_deref(),
                Some(&local_rows),
            )?;
        snapshot_diagnostics = diagnostics;
        Some(rows)
    } else {
        options
            .snapshot
            .map(load_convex_projection_rows)
            .transpose()?
    };
    let mut report = build_convex_sync_report_with_snapshot(
        options.path,
        options.scope,
        snapshot_rows,
        options.chunk_size,
        !options.apply,
    )?;
    report.diagnostics.extend(snapshot_diagnostics);
    if let Some(transport) = &transport {
        let mut receipts = Vec::new();
        if options.apply {
            for chunk in &report.chunks {
                receipts.push(transport.apply_chunk(&report, chunk)?);
            }
        }
        report.transport = Some(transport.summary(options.remote_snapshot, receipts.len()));
        report.receipts = receipts;
        if options.apply {
            report
                .diagnostics
                .push("live Convex transport completed all planned chunks".to_string());
        } else if options.remote_snapshot {
            report
                .diagnostics
                .push("remote Convex snapshot was pulled before diffing".to_string());
        }
    }
    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "convex-sync",
            if report.dry_run { "dry-run" } else { "apply" },
            ToolEnvelopeSummary {
                text: format!(
                    "Convex graph sync {}: {} node upserts, {} edge upserts, {} chunks, freshness {}",
                    if report.dry_run { "plan" } else { "apply" },
                    report.node_upserts.len(),
                    report.edge_upserts.len(),
                    report.chunks.len(),
                    report.freshness.status
                ),
                metrics: vec![
                    envelope_metric("node_upserts", report.node_upserts.len()),
                    envelope_metric("edge_upserts", report.edge_upserts.len()),
                    envelope_metric("chunks", report.chunks.len()),
                    envelope_metric("freshness", &report.freshness.status),
                ],
            },
            report.freshness.fail_closed,
            vec![
                "Apply the planned chunks in order, then rerun with --snapshot to verify freshness"
                    .to_string(),
            ],
        )
    } else {
        print_convex_sync_human(&report, format.compact);
        Ok(())
    }
}

pub(crate) fn cmd_graph_db_status(
    root: &Path,
    scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let graph_db = graph_substrate_db_path(root, scope);
    let report =
        graph_db_operator_report_from_disk(root, scope, &graph_db, "status", None, Vec::new())?;
    print_graph_db_operator_report(&report, format)
}

pub(crate) fn cmd_graph_db_refresh(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let source_watermark = traversal_source_watermark(root, path, scope, false)?;
    let cached_refresh =
        graph_db_backend_eval_cached_refresh(root, scope, source_watermark.as_deref())?;
    let (mode, mut warnings, refresh, phase_timings) =
        if let Some((_graph, refresh, phase_timings)) = cached_refresh {
            (
                "cached_source_watermark_reuse".to_string(),
                Vec::new(),
                refresh,
                phase_timings,
            )
        } else {
            let (graph, refresh) = write_traversal_graph_store(root, path, scope)?;
            let phase_timings = refresh
                .phase_timings
                .iter()
                .map(|phase| GraphDbBackendEvalPhaseTiming {
                    name: phase.name.clone(),
                    duration_micros: phase.duration_micros,
                    detail: phase.detail.clone(),
                })
                .collect::<Vec<_>>();
            (
                "cold_source_graph_rebuild".to_string(),
                graph.warnings,
                refresh,
                phase_timings,
            )
        };
    let graph_db = graph_substrate_db_path(root, scope);
    warnings.extend(graph_db_operator_status_warnings(root, scope));
    let warnings = dedupe_preserve_order(warnings);
    let refresh = GraphDbRefreshSummary {
        scope: refresh.scope,
        projection_version: refresh.projection_version,
        mode,
        source_watermark: refresh.source_watermark,
        tombstoned_nodes: refresh.tombstoned_nodes.len(),
        tombstoned_edges: refresh.tombstoned_edges.len(),
        upserted_nodes: refresh.upserted_nodes,
        upserted_edges: refresh.upserted_edges,
        unchanged_nodes: refresh.unchanged_nodes,
        unchanged_edges: refresh.unchanged_edges,
        upserted_properties: refresh.upserted_properties,
        unchanged_properties: refresh.unchanged_properties,
        deleted_properties: refresh.deleted_properties,
        deleted_nodes: refresh.deleted_nodes,
        deleted_edges: refresh.deleted_edges,
        pruned_tombstones: refresh.pruned_tombstones,
        file_size_bytes_before: refresh.file_size_bytes_before,
        file_size_bytes_after: refresh.file_size_bytes_after,
        phase_timings,
    };
    let report = graph_db_operator_report_from_disk(
        root,
        scope,
        &graph_db,
        "refresh",
        Some(refresh),
        warnings,
    )?;
    print_graph_db_operator_report(&report, format)
}

pub(crate) fn cmd_graph_db_doctor(
    root: &Path,
    scope: Option<&str>,
    backend: GraphDbBackend,
    convex_snapshot: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let graph_db = graph_substrate_db_path(root, scope);
    let backend_name = match backend {
        GraphDbBackend::Sqlite => "sqlite",
        GraphDbBackend::ConvexSnapshot => "convex-snapshot",
        GraphDbBackend::Tokensave => "tokensave",
    };
    let doctor_path = if backend == GraphDbBackend::Tokensave {
        root.join(".tokensave").join("tokensave.db")
    } else {
        graph_db.clone()
    };
    let mut report =
        GraphDbDoctorReport::new(root, scope, backend_name, &doctor_path, convex_snapshot);
    let conn = if backend == GraphDbBackend::Tokensave {
        append_tokensave_graph_doctor_checks(&mut report, root);
        None
    } else {
        append_sqlite_graph_doctor_checks(&mut report, root, scope, &graph_db)
    };
    let local_rows = conn
        .as_ref()
        .and_then(|conn| sqlite_convex_rows_from_conn(conn.conn()).ok());
    if backend == GraphDbBackend::ConvexSnapshot {
        append_convex_snapshot_doctor_checks(
            &mut report,
            root,
            scope,
            local_rows.as_ref(),
            convex_snapshot,
        );
    }
    report.finalize();

    let fail_closed = report.fail_closed;
    let summary = report.summary();
    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "doctor",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB doctor {} for {} backend with {} check(s)",
                    report.status,
                    report.backend,
                    report.checks.len()
                ),
                metrics: vec![
                    envelope_metric("backend", &report.backend),
                    envelope_metric("status", &report.status),
                    envelope_metric("checks", report.checks.len()),
                ],
            },
            false,
            report.repair_commands.clone(),
        )?;
    } else {
        print_graph_db_doctor_human(&report);
    }

    if fail_closed {
        bail!(
            "graph-db doctor failed closed for {} backend: {}",
            backend_name,
            if summary.is_empty() {
                "see diagnostics".to_string()
            } else {
                summary
            }
        );
    }
    Ok(())
}

pub(crate) fn cmd_graph_db_drift(
    root: &Path,
    path: &Path,
    scope: Option<&str>,
    convex_snapshot: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let snapshot_path =
        convex_snapshot.context("graph-db drift requires --convex-snapshot <rows.json>")?;
    let (graph, _refresh) = write_traversal_graph_store(root, path, scope)?;
    let graph_db = graph_substrate_db_path(root, scope);
    let store = SqliteGraphStore::open_read_only_resilient(&graph_db)?;
    let local = convex_rows_from_graph_store(&store)?;
    let (snapshot, snapshot_value) = load_convex_projection_snapshot_value(snapshot_path)?;
    let mut warnings = graph.warnings;
    if let Some(recovery) = store.read_only_recovery() {
        warnings.push(graph_db_read_recovery_diagnostic(recovery));
    }
    let report = graph_db_drift_report(GraphDbDriftInput {
        root,
        scope,
        graph_db: &graph_db,
        snapshot_path,
        local: &local,
        snapshot: &snapshot,
        snapshot_value: &snapshot_value,
        warnings,
    });

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "drift",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB drift status {} with {} node upsert(s), {} edge upsert(s), {} node tombstone(s), {} edge tombstone(s)",
                    report.status,
                    report.summary.node_upserts,
                    report.summary.edge_upserts,
                    report.summary.node_tombstones,
                    report.summary.edge_tombstones
                ),
                metrics: vec![
                    envelope_metric("status", &report.status),
                    envelope_metric("node_upserts", report.summary.node_upserts),
                    envelope_metric("edge_upserts", report.summary.edge_upserts),
                    envelope_metric("node_tombstones", report.summary.node_tombstones),
                    envelope_metric("edge_tombstones", report.summary.edge_tombstones),
                ],
            },
            false,
            report.next_commands.clone(),
        )
    } else {
        print_graph_db_drift_human(&report);
        Ok(())
    }
}

pub(crate) fn cmd_graph_db_compact(
    root: &Path,
    scope: Option<&str>,
    apply: bool,
    prune_tombstones: bool,
    confirmed_convex_reconciled: bool,
    format: OutputFormat,
) -> Result<()> {
    if prune_tombstones && !confirmed_convex_reconciled {
        bail!(
            "graph-db compact --prune-tombstones requires --confirmed-convex-reconciled after Convex deletion reconciliation has completed"
        );
    }
    let graph_db = graph_substrate_db_path(root, scope);
    if apply && !graph_db.exists() {
        bail!("graph-db compact --apply requires an existing graph.db; run graph-db refresh first");
    }
    let before =
        graph_db_operator_report_from_disk(root, scope, &graph_db, "compact", None, Vec::new())?;
    let mut warnings = before.warnings.clone();
    let mut pruned_tombstones = 0usize;
    if apply {
        let mut store = SqliteGraphStore::open(&graph_db)?;
        pruned_tombstones = store.compact_storage(scope.unwrap_or("root"), prune_tombstones)?;
        if prune_tombstones {
            warnings.push(format!(
                "pruned {pruned_tombstones} retained tombstone row(s) after explicit Convex reconciliation confirmation"
            ));
        }
    }
    let after = graph_db_operator_report_from_disk(
        root,
        scope,
        &graph_db,
        "compact",
        None,
        warnings.clone(),
    )?;
    let before_file = before.counts.file_size_bytes.unwrap_or(0) as i64;
    let after_file = after.counts.file_size_bytes.unwrap_or(0) as i64;
    let mut next_commands = after.compaction.recommendations.clone();
    next_commands.push(format!(
        "tsift graph-db --path {}{} status --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope)
    ));
    next_commands.push(format!(
        "tsift graph-db --path {}{} doctor --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope)
    ));
    let compaction_after =
        graph_db_compaction_policy(root, scope, &after.counts, confirmed_convex_reconciled);
    let report = GraphDbCompactionReport {
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        graph_db: graph_db.to_string_lossy().to_string(),
        applied: apply,
        pruned_tombstones,
        counts_before: before.counts,
        counts_after: after.counts,
        compaction_before: before.compaction,
        compaction_after,
        reclaimed_bytes: before_file - after_file,
        next_commands: dedupe_preserve_order(next_commands),
        warnings,
    };

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "compact",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB compact {} with {} reclaimed byte(s) and {} tombstone row(s) pruned",
                    if apply { "applied" } else { "dry-run" },
                    report.reclaimed_bytes,
                    report.pruned_tombstones
                ),
                metrics: vec![
                    envelope_metric("applied", report.applied),
                    envelope_metric("reclaimed_bytes", report.reclaimed_bytes),
                    envelope_metric("pruned_tombstones", report.pruned_tombstones),
                    envelope_metric("tombstones_after", report.counts_after.tombstones.total),
                ],
            },
            false,
            report.next_commands.clone(),
        )
    } else {
        print_graph_db_compaction_human(&report);
        Ok(())
    }
}

pub(crate) fn cmd_graph_db_backend_eval(
    options: GraphDbBackendEvalOptions<'_>,
    format: OutputFormat,
) -> Result<()> {
    let GraphDbBackendEvalOptions {
        path,
        scope,
        candidates,
        targets,
        full_projection,
    } = options;
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let candidates = if candidates.is_empty() {
        vec![
            GraphDbExperimentalBackend::DuckdbDuckpgq,
            GraphDbExperimentalBackend::Falkordb,
            GraphDbExperimentalBackend::Ladybug,
            GraphDbExperimentalBackend::Kuzu,
        ]
    } else {
        candidates
            .iter()
            .map(|candidate| GraphDbExperimentalBackend::parse(candidate))
            .collect::<Result<Vec<_>>>()?
    };
    let high_degree_nodes = 128;
    let high_degree_fanout = 8;
    let deep_chain_nodes = 640;
    let deep_chain_fanout = 1;
    let depth = 3;
    let limit = 8;
    let impact_limit = 20;

    let (graph, _refresh, mut phase_timings) =
        graph_db_backend_eval_refresh_with_profile(&root, path, scope)
            .with_context(|| format!("refreshing graph-db projection for {}", root.display()))?;
    let refresh_micros = graph_db_backend_eval_refresh_total_micros(&phase_timings);
    let reused_cached_projection = graph_db_backend_eval_reused_cached_projection(&phase_timings);
    let prepared = graph_db_backend_eval_timed_phase(
        &mut phase_timings,
        "conflict_matrix_preparation",
        "context-pack, cached diff digest, and cached impact inputs shared by conflict-matrix and dispatch-trace measurements",
        || prepare_conflict_matrix_inputs(&root, path, scope, impact_limit),
    )?;
    phase_timings.extend(prepared.preparation_timings.iter().map(|phase| {
        GraphDbBackendEvalPhaseTiming {
            name: format!("conflict_matrix_preparation.{}", phase.name),
            duration_micros: phase.duration_micros,
            detail: phase.detail.clone(),
        }
    }));
    if !reused_cached_projection {
        graph_db_backend_eval_update_source_watermark(&root, path, scope)?;
    }
    let graph_db = graph_substrate_db_path(&root, scope);
    let real_store = SqliteGraphStore::open_read_only_resilient(&graph_db)
        .with_context(|| format!("opening graph-db projection: {}", graph_db.display()))?;
    let real_freshness = sqlite_graph_freshness(&real_store, scope.unwrap_or("root"))?;
    let real_targets = graph_db_backend_eval_targets(&real_store, targets)?;
    let real_rows = convex_rows_from_graph_store(&real_store)?;
    let real_refresh = graph_db_backend_eval_refresh_operation(
        refresh_micros,
        real_rows.nodes.len() + real_rows.edges.len(),
        serde_json::json!({
            "nodes": real_rows.nodes.len(),
            "edges": real_rows.edges.len(),
        }),
    );
    let mut real_warnings = graph.warnings;
    if let Some(recovery) = real_store.read_only_recovery() {
        real_warnings.push(graph_db_read_recovery_diagnostic(recovery));
    }
    let real_dataset = graph_db_backend_eval_dataset(
        "real",
        &root,
        path,
        scope,
        &real_targets,
        depth,
        limit,
        impact_limit,
        &candidates,
        &real_store,
        real_freshness,
        real_refresh,
        real_rows,
        real_warnings,
        &prepared,
    )?;

    let high_degree_projection =
        graph_db_backend_eval_synthetic_projection(high_degree_nodes, high_degree_fanout);
    let high_degree_started = Instant::now();
    let mut high_degree_store = SqliteGraphStore::in_memory()?;
    let _high_degree_refresh = high_degree_store.replace_projection_with_version(
        "root",
        &high_degree_projection,
        Some(GRAPH_PROJECTION_VERSION),
        Some(format!(
            "synthetic-high-degree:{high_degree_nodes}:{high_degree_fanout}"
        )),
    )?;
    let high_degree_micros = high_degree_started.elapsed().as_micros();
    let high_degree_freshness = sqlite_graph_freshness(&high_degree_store, "root")?;
    let high_degree_targets = graph_db_backend_eval_targets(&high_degree_store, &[])?;
    let high_degree_rows = convex_rows_from_graph_store(&high_degree_store)?;
    let high_degree_refresh = graph_db_backend_eval_refresh_operation(
        high_degree_micros,
        high_degree_rows.nodes.len() + high_degree_rows.edges.len(),
        serde_json::json!({
            "nodes": high_degree_rows.nodes.len(),
            "edges": high_degree_rows.edges.len(),
        }),
    );
    let high_degree_dataset = graph_db_backend_eval_dataset(
        "synthetic_high_degree",
        &root,
        path,
        scope,
        &high_degree_targets,
        depth,
        limit,
        impact_limit,
        &candidates,
        &high_degree_store,
        high_degree_freshness,
        high_degree_refresh,
        high_degree_rows,
        Vec::new(),
        &prepared,
    )?;

    let deep_chain_projection =
        graph_db_backend_eval_synthetic_projection(deep_chain_nodes, deep_chain_fanout);
    let deep_chain_started = Instant::now();
    let mut deep_chain_store = SqliteGraphStore::in_memory()?;
    let _deep_chain_refresh = deep_chain_store.replace_projection_with_version(
        "root",
        &deep_chain_projection,
        Some(GRAPH_PROJECTION_VERSION),
        Some(format!(
            "synthetic-deep-chain:{deep_chain_nodes}:{deep_chain_fanout}"
        )),
    )?;
    let deep_chain_micros = deep_chain_started.elapsed().as_micros();
    let deep_chain_freshness = sqlite_graph_freshness(&deep_chain_store, "root")?;
    let deep_chain_targets = graph_db_backend_eval_targets(&deep_chain_store, &[])?;
    let deep_chain_rows = convex_rows_from_graph_store(&deep_chain_store)?;
    let deep_chain_refresh = graph_db_backend_eval_refresh_operation(
        deep_chain_micros,
        deep_chain_rows.nodes.len() + deep_chain_rows.edges.len(),
        serde_json::json!({
            "nodes": deep_chain_rows.nodes.len(),
            "edges": deep_chain_rows.edges.len(),
        }),
    );
    let deep_chain_dataset = graph_db_backend_eval_dataset(
        "synthetic_deep_chain",
        &root,
        path,
        scope,
        &deep_chain_targets,
        depth,
        limit,
        impact_limit,
        &candidates,
        &deep_chain_store,
        deep_chain_freshness,
        deep_chain_refresh,
        deep_chain_rows,
        Vec::new(),
        &prepared,
    )?;

    let mut all_targets = real_targets
        .iter()
        .chain(high_degree_targets.iter())
        .chain(deep_chain_targets.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut datasets = vec![real_dataset, high_degree_dataset, deep_chain_dataset];
    let mut full_projection_phase_timings = Vec::new();
    let mut full_projection_cache_stats = None;
    if full_projection {
        let (full_projection_rows, full_projection_warnings, mut full_phases, cache_stats) =
            graph_db_backend_eval_full_projection_with_profile(&root, scope)?;
        full_projection_cache_stats = Some(cache_stats);
        let sqlite_open_started = Instant::now();
        let mut full_store = SqliteGraphStore::in_memory()?;
        let sqlite_open_micros = sqlite_open_started.elapsed().as_micros();
        full_phases.push(graph_db_backend_eval_phase_timing(
            "full_projection.sqlite.in_memory_open",
            sqlite_open_micros,
            "open in-memory SQLite graph store for the opt-in full-project projection write",
        ));
        let sqlite_replace_started = Instant::now();
        let full_refresh_op = full_store.replace_projection_with_version(
            scope.unwrap_or("root"),
            &full_projection_rows,
            Some(GRAPH_PROJECTION_VERSION),
            graph_projection_content_hash(&full_projection_rows),
        )?;
        let sqlite_replace_micros = sqlite_replace_started.elapsed().as_micros();
        full_phases.push(graph_db_backend_eval_phase_timing(
            "full_projection.sqlite.replace_projection_total",
            sqlite_replace_micros,
            "wall-clock total of the SQLite replace_projection_with_version write (see sub-phases below)",
        ));
        for sub_phase in full_refresh_op.phase_timings.iter() {
            full_phases.push(GraphDbBackendEvalPhaseTiming {
                name: format!("full_projection.sqlite.{}", sub_phase.name),
                duration_micros: sub_phase.duration_micros,
                detail: sub_phase.detail.clone(),
            });
        }
        let post_write_started = Instant::now();
        let full_freshness = sqlite_graph_freshness(&full_store, scope.unwrap_or("root"))?;
        let full_targets = graph_db_backend_eval_targets(&full_store, targets)?;
        all_targets.extend(full_targets.iter().cloned());
        let full_rows = convex_rows_from_graph_store(&full_store)?;
        let post_write_micros = post_write_started.elapsed().as_micros();
        full_phases.push(graph_db_backend_eval_phase_timing(
            "full_projection.sqlite.post_write_reads",
            post_write_micros,
            "post-write freshness, target resolution, and convex row materialization reads",
        ));
        let full_projection_micros = sqlite_open_micros + sqlite_replace_micros + post_write_micros;
        let full_refresh = graph_db_backend_eval_refresh_operation(
            full_projection_micros,
            full_rows.nodes.len() + full_rows.edges.len(),
            serde_json::json!({
                "nodes": full_rows.nodes.len(),
                "edges": full_rows.edges.len(),
            }),
        );
        phase_timings.extend(full_phases.clone());
        full_projection_phase_timings = full_phases;
        datasets.push(graph_db_backend_eval_dataset(
            "full_projection",
            &root,
            path,
            scope,
            &full_targets,
            depth,
            limit,
            impact_limit,
            &candidates,
            &full_store,
            full_freshness,
            full_refresh,
            full_rows,
            full_projection_warnings,
            &prepared,
        )?);
    }
    let targets = all_targets.into_iter().collect::<Vec<_>>();
    let promotion = graph_db_backend_eval_promotion(&datasets, &candidates);
    let mut metrics = graph_db_backend_eval_metrics(&datasets);
    if let Some(cache_stats) = &full_projection_cache_stats {
        metrics.insert(
            "full_projection.cache.disk_bytes".to_string(),
            cache_stats.disk_bytes as f64,
        );
        metrics.insert(
            "full_projection.cache.json_bytes".to_string(),
            cache_stats.json_bytes as f64,
        );
        metrics.insert(
            "full_projection.cache.compression_ratio".to_string(),
            if cache_stats.json_bytes == 0 {
                0.0
            } else {
                cache_stats.disk_bytes as f64 / cache_stats.json_bytes as f64
            },
        );
        metrics.insert(
            "full_projection.cache.hit".to_string(),
            if cache_stats.hit { 1.0 } else { 0.0 },
        );
        metrics.insert(
            "full_projection.cache.pruned_files".to_string(),
            cache_stats.pruned_files as f64,
        );
        metrics.insert(
            "full_projection.cache.pruned_bytes".to_string(),
            cache_stats.pruned_bytes as f64,
        );
    }
    if let Some(real_dataset) = datasets.iter().find(|dataset| dataset.name == "real") {
        let real_phase_timings = phase_timings
            .iter()
            .filter(|phase| !phase.name.starts_with("full_projection."))
            .cloned()
            .collect::<Vec<_>>();
        append_graph_db_backend_eval_phase_metrics(
            &mut metrics,
            "real",
            graph_db_backend_eval_graph_rows(real_dataset),
            &real_phase_timings,
        );
    }
    if let Some(full_dataset) = datasets
        .iter()
        .find(|dataset| dataset.name == "full_projection")
    {
        let normalized_full_projection_phases = full_projection_phase_timings
            .iter()
            .filter_map(|phase| {
                Some(GraphDbBackendEvalPhaseTiming {
                    name: phase.name.strip_prefix("full_projection.")?.to_string(),
                    duration_micros: phase.duration_micros,
                    detail: phase.detail.clone(),
                })
            })
            .collect::<Vec<_>>();
        append_graph_db_backend_eval_phase_metrics(
            &mut metrics,
            "full_projection",
            graph_db_backend_eval_graph_rows(full_dataset),
            &normalized_full_projection_phases,
        );
        for phase in &full_projection_phase_timings {
            if let Some(sqlite_phase) = phase.name.strip_prefix("full_projection.sqlite.") {
                metrics.insert(
                    format!("full_projection.sqlite.{sqlite_phase}.duration_micros"),
                    phase.duration_micros as f64,
                );
                append_graph_db_backend_eval_normalized_duration_metric(
                    &mut metrics,
                    &format!(
                        "full_projection.sqlite.{sqlite_phase}.duration_micros_per_1k_graph_rows"
                    ),
                    phase.duration_micros,
                    graph_db_backend_eval_graph_rows(full_dataset),
                );
            }
        }
    }
    let report = GraphDbBackendEvalReport {
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        label: "graph-db backend-eval".to_string(),
        baseline_backend: "sqlite".to_string(),
        candidates: candidates
            .iter()
            .map(|candidate| candidate.name().to_string())
            .collect(),
        targets,
        config: GraphDbBackendEvalConfig {
            high_degree_nodes,
            high_degree_fanout,
            deep_chain_nodes,
            deep_chain_fanout,
            depth,
            limit,
            impact_limit,
            path_max_hops: GRAPH_DB_BACKEND_EVAL_PATH_MAX_HOPS,
            path_direct_hop_budget: GRAPH_DB_BACKEND_EVAL_DIRECT_PATH_HOPS,
            path_deep_chain_hop_budget: GRAPH_DB_BACKEND_EVAL_PATH_MAX_HOPS,
            path_extended_hop_budgets: GRAPH_DB_BACKEND_EVAL_EXTENDED_PATH_HOPS.to_vec(),
            path_hop_policy:
                "default path reads stay capped at 64 hops; 128/256/512-hop probes are opt-in benchmark evidence until real and synthetic regression gates pass"
                    .to_string(),
            path_probe_strategy:
                "adaptive: use one-hop direct probes for high-degree/direct edges, 64-hop deep-chain default coverage, and measured 128/256/512-hop tiers without raising user-facing defaults"
                    .to_string(),
            path_query_plan_checks: vec![
                "SQLite bounded path probes must continue using idx_graph_edges_from_kind for frontier expansion".to_string(),
                "128/256/512-hop tiers are benchmark fixtures until repeated samples and query-plan checks pass".to_string(),
            ],
            full_projection_enabled: full_projection,
            full_projection_profile: if full_projection {
                "included opt-in full_projection dataset built from the project root".to_string()
            } else {
                "disabled by default; pass --full-projection to add the full-project dataset"
                    .to_string()
            },
            normalization_row_unit: GRAPH_DB_BACKEND_EVAL_NORMALIZATION_ROW_UNIT as usize,
        },
        phase_timings,
        datasets,
        promotion,
        performance_gate: graph_db_backend_eval_performance_gate(&root, scope, full_projection),
        metrics,
        metric_digest_command: graph_db_backend_eval_metric_digest_command(
            &root,
            scope,
            full_projection,
        ),
        warnings: Vec::new(),
    };

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "backend-eval",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB backend evaluation ran {} dataset(s) against {} candidate(s)",
                    report.datasets.len(),
                    report.candidates.len()
                ),
                metrics: vec![
                    envelope_metric("datasets", report.datasets.len()),
                    envelope_metric("candidates", report.candidates.len()),
                ],
            },
            false,
            vec![
                format!(
                    "Re-run with `tsift graph-db --path {} --json backend-eval --target <id>` after adding a production candidate adapter",
                    shell_quote(root.to_string_lossy().as_ref())
                ),
                report.metric_digest_command.clone(),
            ],
        )
    } else {
        print_graph_db_backend_eval_human(&report);
        Ok(())
    }
}

pub(crate) fn cmd_graph_db(
    path: &Path,
    scope: Option<&str>,
    backend: GraphDbBackend,
    convex_snapshot: Option<&Path>,
    query: GraphDbQuery,
    format: OutputFormat,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    match &query {
        GraphDbQuery::Refresh => {
            return cmd_graph_db_refresh(&root, path, scope, format);
        }
        GraphDbQuery::Status => {
            return cmd_graph_db_status(&root, scope, format);
        }
        GraphDbQuery::Doctor => {
            return cmd_graph_db_doctor(&root, scope, backend, convex_snapshot, format);
        }
        GraphDbQuery::Drift => {
            return cmd_graph_db_drift(&root, path, scope, convex_snapshot, format);
        }
        GraphDbQuery::Compact {
            apply,
            prune_tombstones,
            confirmed_convex_reconciled,
        } => {
            return cmd_graph_db_compact(
                &root,
                scope,
                *apply,
                *prune_tombstones,
                *confirmed_convex_reconciled,
                format,
            );
        }
        GraphDbQuery::BackendEval {
            candidates,
            targets,
            full_projection,
        } => {
            return cmd_graph_db_backend_eval(
                GraphDbBackendEvalOptions {
                    path,
                    scope,
                    candidates,
                    targets,
                    full_projection: *full_projection,
                },
                format,
            );
        }
        _ => {}
    }
    let graph_db = graph_substrate_db_path(&root, scope);
    let mut warnings = Vec::new();
    if matches!(
        backend,
        GraphDbBackend::Sqlite | GraphDbBackend::ConvexSnapshot
    ) && let GraphDbQuery::Evidence { target, .. } = &query
    {
        let needs_refresh = if graph_db.exists() {
            let store = SqliteGraphStore::open_read_only_resilient(&graph_db)?;
            sqlite_graph_freshness(&store, scope.unwrap_or("root"))?.fail_closed
                || graph_db_resolve_evidence_target(&store, target)?.is_none()
        } else {
            true
        };
        if needs_refresh {
            let (graph, _refresh) =
                write_traversal_graph_store_with_options(&root, path, scope, true)?;
            warnings = graph.warnings;
        }
    }
    let report = match backend {
        GraphDbBackend::Sqlite => {
            let store = SqliteGraphStore::open_read_only_resilient(&graph_db)?;
            if let Some(recovery) = store.read_only_recovery() {
                warnings.push(graph_db_read_recovery_diagnostic(recovery));
            }
            let freshness = sqlite_graph_freshness(&store, scope.unwrap_or("root"))?;
            if let GraphDbQuery::Evidence {
                target,
                depth,
                limit,
            } = &query
            {
                let report = graph_db_evidence_report_from_store(GraphDbEvidenceInput {
                    root: &root,
                    scope,
                    backend: "sqlite",
                    target,
                    depth: *depth,
                    limit: *limit,
                    store: &store,
                    freshness,
                    warnings,
                })?;
                return print_graph_db_evidence_report(&report, format);
            }
            graph_db_report_from_store(&root, scope, "sqlite", query, &store, freshness, warnings)?
        }
        GraphDbBackend::ConvexSnapshot => {
            let snapshot_path = convex_snapshot
                .context("--backend convex-snapshot requires --convex-snapshot <rows.json>")?;
            let local_store = SqliteGraphStore::open_read_only_resilient(&graph_db)?;
            if let Some(recovery) = local_store.read_only_recovery() {
                warnings.push(graph_db_read_recovery_diagnostic(recovery));
            }
            let local = convex_rows_from_graph_store(&local_store)?;
            let snapshot = load_convex_projection_rows(snapshot_path)?;
            validate_convex_projection_rows(&snapshot)?;
            let freshness = convex_graph_freshness(&local, &snapshot, scope);
            let client = ConvexRowsGraphClient::from_rows(snapshot);
            let store = SubstrateConvexGraphStore::new(client);
            if let GraphDbQuery::Evidence {
                target,
                depth,
                limit,
            } = &query
            {
                let report = graph_db_evidence_report_from_store(GraphDbEvidenceInput {
                    root: &root,
                    scope,
                    backend: "convex-snapshot",
                    target,
                    depth: *depth,
                    limit: *limit,
                    store: &store,
                    freshness,
                    warnings,
                })?;
                return print_graph_db_evidence_report(&report, format);
            }
            graph_db_report_from_store(
                &root,
                scope,
                "convex-snapshot",
                query,
                &store,
                freshness,
                warnings,
            )?
        }
        GraphDbBackend::Tokensave => {
            let store = TokensaveDb::discover(&root)?.with_context(|| {
                format!(
                    "--backend tokensave requires {}",
                    root.join(".tokensave").join("tokensave.db").display()
                )
            })?;
            let freshness = tokensave_graph_freshness(&store)?;
            if let GraphDbQuery::Evidence { .. } = &query {
                bail!(
                    "graph-db evidence is not supported for --backend tokensave; use node, kind, edges, incident, neighborhood, or path queries"
                );
            }
            graph_db_report_from_store(
                &root,
                scope,
                "tokensave",
                query,
                &store,
                freshness,
                warnings,
            )?
        }
    };

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "query",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB {} query returned {} node(s), {} edge(s), freshness {}",
                    report.backend,
                    report.nodes.len() + usize::from(report.node.is_some()),
                    report.edges.len(),
                    report.freshness.status
                ),
                metrics: vec![
                    envelope_metric("backend", &report.backend),
                    envelope_metric(
                        "nodes",
                        report.nodes.len() + usize::from(report.node.is_some()),
                    ),
                    envelope_metric("edges", report.edges.len()),
                    envelope_metric("freshness", &report.freshness.status),
                ],
            },
            false,
            vec![format!(
                "Use tsift convex-sync {} --json to inspect or refresh Convex projection rows",
                shell_quote(root.to_string_lossy().as_ref())
            )],
        )
    } else {
        print_graph_db_human(&report, format.compact);
        Ok(())
    }
}

pub(crate) fn cmd_status(
    path: &std::path::Path,
    fix: bool,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let mut report = status::check_status(&root)?;
    if status_missing_workspace_scopes(&report) {
        autoindex_missing_workspace_scopes(&root, &report)?;
        report = status::check_status(&root)?;
    }
    if fix {
        apply_status_fixes(&root, &report)?;
        report = status::check_status(&root)?;
        if status_missing_workspace_scopes(&report) {
            autoindex_missing_workspace_scopes(&root, &report)?;
            report = status::check_status(&root)?;
        }
    }
    if json_output {
        println!("{}", to_json_schema(&report, pretty, terse, schema)?);
    } else {
        print!("{}", status::format_human(&report, compact));
    }
    Ok(())
}

pub(crate) fn cmd_locks(
    path: &std::path::Path,
    scope: Option<&str>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let report = status::check_locks(&root, Some(path), scope)?;
    if json_output {
        println!("{}", to_json_schema(&report, pretty, terse, schema)?);
    } else {
        print!("{}", status::format_locks_human(&report, compact));
    }
    Ok(())
}

pub(crate) fn cmd_init(
    path: &std::path::Path,
    codex: bool,
    opencode: bool,
    workspace: bool,
) -> Result<()> {
    let resolved = if workspace {
        init::resolve_workspace_dir(path)?
    } else {
        init::resolve_project_dir(path)?
    };
    if resolved != path {
        println!("resolved: {} → {}", path.display(), resolved.display());
    }
    let codex_workspace = codex && (workspace || init::has_submodules(&resolved)?);
    let result = init::init_with_integrations(&resolved, codex, codex_workspace, opencode)?;
    for update in result.updates {
        println!(
            "{}: {} ({})",
            update.file.display(),
            update.action,
            match update.action {
                init::InitAction::Created => "tsift Code Navigation section added",
                init::InitAction::Updated => "tsift Code Navigation section updated to latest",
                init::InitAction::AlreadyPresent => "no changes needed",
            }
        );
    }
    if result.gitignore_added {
        println!(".gitignore: added .tsift/");
    }
    if let Some(codex_result) = &result.codex_hooks {
        let scope_label = match codex_result.scope {
            init::CodexHookScope::Project => "project",
            init::CodexHookScope::Workspace => "workspace",
        };
        match codex_result.action {
            init::CodexHookAction::Added => {
                println!(
                    ".codex/hooks.json: tsift {} auto-reindex hook added",
                    scope_label
                );
            }
            init::CodexHookAction::Updated => {
                println!(
                    ".codex/hooks.json: tsift {} auto-reindex hook updated",
                    scope_label
                );
            }
            init::CodexHookAction::AlreadyPresent => {
                println!(
                    ".codex/hooks.json: tsift {} hook already present",
                    scope_label
                );
            }
            init::CodexHookAction::Created => {
                println!(
                    ".codex/hooks.json: created with tsift {} auto-reindex hook",
                    scope_label
                );
            }
        }
    }
    if let Some(opencode_commands) = &result.opencode_commands {
        for update in opencode_commands {
            println!(
                "{}: {} (OpenCode /{} tsift shortcut)",
                update.file.display(),
                update.action,
                update.command_name
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_sql(
    db_path: &std::path::Path,
    query: Option<String>,
    table: Option<String>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let conn = open_db(db_path)?;

    match (query, table) {
        (Some(sql), _) => {
            let (columns, rows) = execute_query(&conn, &sql)?;
            if json_output {
                let json_rows: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|row| {
                        let obj: serde_json::Map<String, serde_json::Value> = columns
                            .iter()
                            .zip(row.iter())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        serde_json::Value::Object(obj)
                    })
                    .collect();
                println!("{}", to_json_schema(&json_rows, pretty, terse, schema)?);
            } else if compact {
                println!("rows:{} cols:{}", rows.len(), columns.len());
                for row in &rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::Null => "NULL".to_string(),
                            serde_json::Value::String(s) => truncate_for_compact(s, 40),
                            other => other.to_string(),
                        })
                        .collect();
                    println!("  {}", cells.join(" | "));
                }
            } else {
                // Tabular output
                if columns.is_empty() {
                    println!("Query returned no columns.");
                    return Ok(());
                }
                // Header
                println!("{}", columns.join(" | "));
                println!(
                    "{}",
                    columns
                        .iter()
                        .map(|c| "-".repeat(c.len().max(4)))
                        .collect::<Vec<_>>()
                        .join("-+-")
                );
                for row in &rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::Null => "NULL".to_string(),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    println!("{}", cells.join(" | "));
                }
                println!("\n{} row(s)", rows.len());
            }
        }
        (None, Some(tbl)) => {
            let cols = table_columns(&conn, &tbl)?;
            if cols.is_empty() {
                bail!("table '{}' not found or has no columns", tbl);
            }
            if json_output {
                println!("{}", to_json_schema(&cols, pretty, terse, schema)?);
            } else if compact {
                println!("table:{} columns:{}", tbl, cols.len());
                for col in &cols {
                    println!("  {} {}", col.name, col.col_type);
                }
            } else {
                println!("Table: {}", tbl);
                println!("{:<20} {:<12} {:<8} PK", "Column", "Type", "NotNull");
                println!("{}", "-".repeat(50));
                for col in &cols {
                    println!(
                        "{:<20} {:<12} {:<8} {}",
                        col.name,
                        col.col_type,
                        col.notnull,
                        if col.pk { "PK" } else { "" }
                    );
                }
            }
        }
        (None, None) => {
            let tables = schema_overview(&conn)?;
            if json_output {
                println!("{}", to_json_schema(&tables, pretty, terse, schema)?);
            } else if compact {
                println!("tables:{}", tables.len());
                for tbl in &tables {
                    println!(
                        "  {} rows:{} cols:{}",
                        tbl.name,
                        tbl.row_count,
                        tbl.columns.len()
                    );
                }
            } else {
                println!("Database: {}", db_path.display());
                println!("{} table(s)\n", tables.len());
                for tbl in &tables {
                    println!("  {} ({} rows)", tbl.name, tbl.row_count);
                    for col in &tbl.columns {
                        let flags = [
                            if col.pk { "PK" } else { "" },
                            if col.notnull { "NOT NULL" } else { "" },
                        ]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                        let suffix = if flags.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", flags)
                        };
                        println!("    {} {}{}", col.name, col.col_type, suffix);
                    }
                    println!();
                }
            }
        }
    }
    Ok(())
}

/// Exit codes for `tsift rewrite` (matches rtk protocol):
///   0 + stdout → rewrite found, auto-allow
///   1 + stderr → no tsift equivalent, pass through
pub(crate) fn cmd_rewrite(command: &str, run: bool, format: OutputFormat) -> Result<()> {
    let rewritten = match rewrite_command(command) {
        Some(rewritten) => rewritten,
        None => {
            eprintln!("{}", no_rewrite_message(command, run));
            std::process::exit(1);
        }
    };
    let rewritten = apply_rewrite_output_format(&rewritten, format);

    if !run {
        print!("{}", rewritten);
        return Ok(());
    }

    let status_code = execute_rewritten_command(&rewritten)?;
    std::process::exit(status_code);
}
