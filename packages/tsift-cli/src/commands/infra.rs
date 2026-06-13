use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Serialize;
use substrate::{
    ConvexGraphStore as SubstrateConvexGraphStore, ConvexProjectionRows, ConvexRowsGraphClient,
    GraphStore, SqliteGraphStore,
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
    GraphDbFreshnessReport, GraphDbOperatorCounts, GraphDbOperatorReport, GraphDbRefreshSummary,
    GraphEffectivenessReadiness, append_convex_snapshot_doctor_checks,
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
    graph_db_drift_report, graph_db_evidence_preferred_path, graph_db_evidence_report_from_store,
    graph_db_operator_report_from_disk, graph_db_operator_status_warnings,
    graph_db_read_recovery_diagnostic, graph_db_report_from_store,
    graph_db_resolve_evidence_target_with_path, graph_db_scope_arg, graph_projection_content_hash,
    graph_substrate_db_path, load_convex_projection_rows, load_convex_projection_snapshot_value,
    no_rewrite_message, open_db, prepare_conflict_matrix_inputs, print_convex_sync_human,
    print_graph_db_backend_eval_human, print_graph_db_compaction_human,
    print_graph_db_doctor_human, print_graph_db_drift_human, print_graph_db_evidence_report,
    print_graph_db_human, print_graph_db_operator_report, print_json_or_envelope, rewrite_command,
    schema_overview, shell_quote, sqlite_convex_rows_from_conn, sqlite_graph_freshness,
    status_missing_workspace_scopes, table_columns, to_json_schema, tokensave_graph_freshness,
    traversal_source_watermark, truncate_for_compact, validate_convex_projection_rows,
    write_traversal_graph_store, write_traversal_graph_store_with_options,
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
                false,
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

#[derive(Serialize)]
pub(crate) struct GraphDbSnapshotReport {
    pub(crate) root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<String>,
    pub(crate) graph_db: String,
    pub(crate) operation: String,
    pub(crate) status: String,
    pub(crate) artifact: String,
    pub(crate) compression: String,
    pub(crate) artifact_bytes: u64,
    pub(crate) database_bytes: u64,
    pub(crate) freshness: GraphDbFreshnessReport,
    pub(crate) readiness: GraphEffectivenessReadiness,
    pub(crate) counts: GraphDbOperatorCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recovery: Option<substrate::ReadOnlyRecovery>,
    pub(crate) doctor: GraphDbDoctorReport,
    pub(crate) next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

fn graph_db_snapshot_import_command(root: &Path, scope: Option<&str>, artifact: &Path) -> String {
    format!(
        "tsift graph-db --path {}{} snapshot-import {} --replace --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope),
        shell_quote(artifact.to_string_lossy().as_ref())
    )
}

fn graph_db_snapshot_status_command(root: &Path, scope: Option<&str>) -> String {
    format!(
        "tsift graph-db --path {}{} status --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope)
    )
}

fn graph_db_snapshot_doctor_command(root: &Path, scope: Option<&str>) -> String {
    format!(
        "tsift graph-db --path {}{} doctor --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope)
    )
}

fn graph_db_snapshot_compact_command(root: &Path, scope: Option<&str>) -> String {
    format!(
        "tsift graph-db --path {}{} compact --apply --json",
        shell_quote(root.to_string_lossy().as_ref()),
        graph_db_scope_arg(scope)
    )
}

fn graph_db_snapshot_path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn graph_db_snapshot_unique_path(parent: &Path, stem: &str, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    parent.join(format!(".{stem}-{}-{nanos}{suffix}", std::process::id()))
}

fn graph_db_snapshot_sidecar_diagnostics(
    root: &Path,
    scope: Option<&str>,
    graph_db: &Path,
) -> Vec<String> {
    let mut diagnostics = Vec::new();
    for (label, path) in [
        (
            "rollback journal",
            substrate::rollback_journal_path(graph_db),
        ),
        ("WAL", substrate::wal_sidecar_path(graph_db)),
        (
            "shared-memory",
            substrate::shared_memory_sidecar_path(graph_db),
        ),
    ] {
        if path.exists() {
            diagnostics.push(format!(
                "graph.db {label} sidecar exists at {}; run `{}` or stop the active writer before using a shareable snapshot",
                path.display(),
                graph_db_snapshot_compact_command(root, scope)
            ));
        }
    }
    diagnostics
}

fn graph_db_snapshot_export_sidecar_warnings(graph_db: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for (label, path) in [
        (
            "rollback journal",
            substrate::rollback_journal_path(graph_db),
        ),
        ("WAL", substrate::wal_sidecar_path(graph_db)),
        (
            "shared-memory",
            substrate::shared_memory_sidecar_path(graph_db),
        ),
    ] {
        if path.exists() {
            warnings.push(format!(
                "graph.db {label} sidecar exists at {}; snapshot-export used a clean SQLite VACUUM INTO copy before compression",
                path.display()
            ));
        }
    }
    warnings
}

fn graph_db_snapshot_doctor_report(
    root: &Path,
    scope: Option<&str>,
    graph_db: &Path,
    backend_name: &str,
) -> GraphDbDoctorReport {
    let mut report = GraphDbDoctorReport::new(root, scope, backend_name, graph_db, None);
    append_sqlite_graph_doctor_checks(&mut report, root, scope, graph_db);
    report.finalize();
    report
}

fn graph_db_snapshot_fail_if_doctor_closed(
    operation: &str,
    doctor: &GraphDbDoctorReport,
) -> Result<()> {
    if doctor.fail_closed {
        let summary = doctor.summary();
        bail!(
            "graph-db {operation} failed doctor checks: {}",
            if summary.is_empty() {
                "see diagnostics".to_string()
            } else {
                summary
            }
        );
    }
    Ok(())
}

fn graph_db_snapshot_fail_if_not_fresh(
    operation: &str,
    report: &GraphDbOperatorReport,
) -> Result<()> {
    if !report.materialized || report.freshness.fail_closed {
        bail!(
            "graph-db {operation} failed freshness checks: {}",
            report.freshness.diagnostics.join("; ")
        );
    }
    Ok(())
}

fn graph_db_snapshot_fail_if_unsafe_sidecars(
    root: &Path,
    scope: Option<&str>,
    graph_db: &Path,
) -> Result<()> {
    let diagnostics = graph_db_snapshot_sidecar_diagnostics(root, scope, graph_db);
    if !diagnostics.is_empty() {
        bail!(
            "graph-db snapshot blocked by live SQLite sidecar diagnostics: {}",
            diagnostics.join("; ")
        );
    }
    Ok(())
}

struct GraphDbSnapshotReportInput<'a> {
    operation: &'a str,
    success_status: &'a str,
    artifact: &'a Path,
    artifact_bytes: u64,
    database_bytes: u64,
    extra_next_commands: Vec<String>,
    extra_warnings: Vec<String>,
}

fn graph_db_snapshot_report_from_operator(
    operator: GraphDbOperatorReport,
    doctor: GraphDbDoctorReport,
    input: GraphDbSnapshotReportInput<'_>,
) -> GraphDbSnapshotReport {
    let GraphDbOperatorReport {
        root,
        scope,
        graph_db,
        operation: _,
        status: _,
        materialized: _,
        freshness,
        readiness,
        counts,
        refresh: _,
        compaction: _,
        recovery,
        next_commands,
        warnings,
    } = operator;
    let GraphDbSnapshotReportInput {
        operation,
        success_status,
        artifact,
        artifact_bytes,
        database_bytes,
        mut extra_next_commands,
        mut extra_warnings,
    } = input;
    extra_next_commands.extend(next_commands);
    extra_warnings.extend(warnings);
    let status = if readiness.fail_closed {
        format!("{success_status}_readiness_blocked")
    } else {
        success_status.to_string()
    };
    GraphDbSnapshotReport {
        root,
        scope,
        graph_db,
        operation: operation.to_string(),
        status,
        artifact: artifact.to_string_lossy().to_string(),
        compression: "gzip".to_string(),
        artifact_bytes,
        database_bytes,
        freshness,
        readiness,
        counts,
        recovery,
        doctor,
        next_commands: dedupe_preserve_order(extra_next_commands),
        warnings: dedupe_preserve_order(extra_warnings),
    }
}

fn graph_db_snapshot_compress_file(source: &Path, output: &Path, force: bool) -> Result<u64> {
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!("creating snapshot artifact directory {}", parent.display())
        })?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let output_file = options
        .open(output)
        .with_context(|| format!("creating snapshot artifact {}", output.display()))?;
    let mut input =
        fs::File::open(source).with_context(|| format!("opening graph db {}", source.display()))?;
    let mut encoder = GzEncoder::new(output_file, Compression::default());
    std::io::copy(&mut input, &mut encoder)
        .with_context(|| format!("compressing graph db into {}", output.display()))?;
    let output_file = encoder
        .finish()
        .with_context(|| format!("finalizing snapshot artifact {}", output.display()))?;
    output_file
        .sync_all()
        .with_context(|| format!("syncing snapshot artifact {}", output.display()))?;
    Ok(fs::metadata(output)
        .with_context(|| format!("stat snapshot artifact {}", output.display()))?
        .len())
}

fn graph_db_snapshot_decompress_to_staged(artifact: &Path, staged: &Path) -> Result<u64> {
    let input = fs::File::open(artifact)
        .with_context(|| format!("opening graph-db snapshot artifact {}", artifact.display()))?;
    let mut decoder = GzDecoder::new(input);
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged)
        .with_context(|| format!("creating staged graph-db snapshot {}", staged.display()))?;
    std::io::copy(&mut decoder, &mut output)
        .with_context(|| format!("decompressing graph-db snapshot {}", artifact.display()))?;
    output
        .sync_all()
        .with_context(|| format!("syncing staged graph-db snapshot {}", staged.display()))?;
    Ok(fs::metadata(staged)
        .with_context(|| format!("stat staged graph-db snapshot {}", staged.display()))?
        .len())
}

pub(crate) fn graph_db_snapshot_clean_export_copy(graph_db: &Path, clean_path: &Path) -> Result<u64> {
    // A concurrent graph-db refresh / snapshot-import can hold an exclusive
    // database lock, which surfaces as a raw SQLite "database is locked" from
    // either the read-only open below or the VACUUM INTO copy. Map that to the
    // same actionable live-lock diagnostic the write-lock path emits instead of
    // leaking the bare SQLite error to the operator.
    let map_live_lock = |err: anyhow::Error, fallback: String| -> anyhow::Error {
        if substrate::error_mentions_locked_db(&err) {
            anyhow::anyhow!(
                "graph-db snapshot-export could not read {}: a concurrent graph-db \
                 refresh or snapshot-import is in progress (database is locked); \
                 wait for it to finish before retrying the export",
                graph_db.display()
            )
        } else {
            err.context(fallback)
        }
    };

    let conn = substrate::open_graph_read_only_connection_resilient(graph_db).map_err(|err| {
        map_live_lock(
            err,
            format!("opening graph db for snapshot export {}", graph_db.display()),
        )
    })?;
    if let Some(recovery) = conn.recovery() {
        bail!(
            "graph-db snapshot-export refused recovered read path: {}; resolve the live database lock before exporting a shareable artifact",
            graph_db_read_recovery_diagnostic(recovery)
        );
    }
    let clean_path_str = clean_path.to_string_lossy().to_string();
    if let Err(err) = conn
        .conn()
        .execute("VACUUM INTO ?1", [clean_path_str.as_str()])
    {
        return Err(map_live_lock(
            anyhow::Error::new(err),
            format!(
                "creating clean graph-db export copy {} from {}",
                clean_path.display(),
                graph_db.display()
            ),
        ));
    }
    Ok(fs::metadata(clean_path)
        .with_context(|| format!("stat clean graph-db export copy {}", clean_path.display()))?
        .len())
}

pub(crate) fn graph_db_snapshot_export_report(
    root: &Path,
    scope: Option<&str>,
    output: &Path,
    force: bool,
) -> Result<GraphDbSnapshotReport> {
    let graph_db = graph_substrate_db_path(root, scope);
    let doctor = graph_db_snapshot_doctor_report(root, scope, &graph_db, "sqlite");
    graph_db_snapshot_fail_if_doctor_closed("snapshot-export", &doctor)?;
    let operator = graph_db_operator_report_from_disk(
        root,
        scope,
        &graph_db,
        "snapshot-export",
        None,
        vec![],
    )?;
    graph_db_snapshot_fail_if_not_fresh("snapshot-export", &operator)?;
    if let Some(recovery) = operator.recovery {
        bail!(
            "graph-db snapshot-export refused recovered read path: {}; resolve the live database lock before exporting a shareable artifact",
            graph_db_read_recovery_diagnostic(recovery)
        );
    }
    let graph_parent = graph_db
        .parent()
        .with_context(|| format!("graph db path has no parent: {}", graph_db.display()))?;
    let clean_path = graph_db_snapshot_unique_path(graph_parent, "graph-snapshot-export", ".db");
    let export_result: Result<GraphDbSnapshotReport> = (|| {
        let database_bytes = graph_db_snapshot_clean_export_copy(&graph_db, &clean_path)?;
        let clean_doctor =
            graph_db_snapshot_doctor_report(root, scope, &clean_path, "sqlite-snapshot");
        graph_db_snapshot_fail_if_doctor_closed("snapshot-export clean copy", &clean_doctor)?;
        let clean_operator = graph_db_operator_report_from_disk(
            root,
            scope,
            &clean_path,
            "snapshot-export-clean",
            None,
            vec![],
        )?;
        graph_db_snapshot_fail_if_not_fresh("snapshot-export clean copy", &clean_operator)?;
        let artifact_bytes = graph_db_snapshot_compress_file(&clean_path, output, force)?;
        Ok(graph_db_snapshot_report_from_operator(
            operator,
            doctor,
            GraphDbSnapshotReportInput {
                operation: "snapshot-export",
                success_status: "exported",
                artifact: output,
                artifact_bytes,
                database_bytes,
                extra_next_commands: vec![graph_db_snapshot_import_command(root, scope, output)],
                extra_warnings: graph_db_snapshot_export_sidecar_warnings(&graph_db),
            },
        ))
    })();
    let _ = fs::remove_file(&clean_path);
    export_result
}

pub(crate) fn graph_db_snapshot_import_report(
    root: &Path,
    scope: Option<&str>,
    artifact: &Path,
    replace: bool,
) -> Result<GraphDbSnapshotReport> {
    let graph_db = graph_substrate_db_path(root, scope);
    let graph_parent = graph_db
        .parent()
        .with_context(|| format!("graph db path has no parent: {}", graph_db.display()))?;
    fs::create_dir_all(graph_parent)
        .with_context(|| format!("creating graph db directory {}", graph_parent.display()))?;
    let staged = graph_db_snapshot_unique_path(graph_parent, "graph-snapshot-import", ".db");
    let backup = graph_db_snapshot_path_with_suffix(&graph_db, ".pre-snapshot-import-bak");
    let import_result: Result<GraphDbSnapshotReport> = (|| {
        let database_bytes = graph_db_snapshot_decompress_to_staged(artifact, &staged)?;
        let validation_doctor =
            graph_db_snapshot_doctor_report(root, scope, &staged, "sqlite-snapshot");
        graph_db_snapshot_fail_if_doctor_closed("snapshot-import validation", &validation_doctor)?;
        let validation_operator = graph_db_operator_report_from_disk(
            root,
            scope,
            &staged,
            "snapshot-import-validate",
            None,
            vec![],
        )?;
        graph_db_snapshot_fail_if_not_fresh("snapshot-import validation", &validation_operator)?;
        // Hold the cross-process write lock across the whole rename window so a
        // concurrent `graph-db refresh` cannot create -wal/-shm sidecars between
        // the sidecar check and the rename and corrupt the imported db
        // (#gdbwritelock). The sidecar check below now runs under the lock.
        let _import_lock = crate::acquire_graph_db_write_lock(&graph_db)?;
        if graph_db.exists() {
            if !replace {
                bail!(
                    "graph-db snapshot-import would replace {}; pass --replace after reviewing snapshot diagnostics",
                    graph_db.display()
                );
            }
            graph_db_snapshot_fail_if_unsafe_sidecars(root, scope, &graph_db)?;
            if backup.exists() {
                bail!(
                    "graph-db snapshot-import backup path already exists: {}; move it before retrying",
                    backup.display()
                );
            }
            fs::rename(&graph_db, &backup).with_context(|| {
                format!(
                    "backing up existing graph db {} to {}",
                    graph_db.display(),
                    backup.display()
                )
            })?;
        }
        if let Err(err) = fs::rename(&staged, &graph_db).with_context(|| {
            format!(
                "installing staged graph-db snapshot {} to {}",
                staged.display(),
                graph_db.display()
            )
        }) {
            if backup.exists() {
                let _ = fs::rename(&backup, &graph_db);
            }
            return Err(err);
        }
        let post_install: Result<GraphDbSnapshotReport> = (|| {
            let doctor = graph_db_snapshot_doctor_report(root, scope, &graph_db, "sqlite");
            graph_db_snapshot_fail_if_doctor_closed("snapshot-import post-install", &doctor)?;
            let artifact_bytes = fs::metadata(artifact)
                .with_context(|| format!("stat graph-db snapshot artifact {}", artifact.display()))?
                .len();
            let operator = graph_db_operator_report_from_disk(
                root,
                scope,
                &graph_db,
                "snapshot-import",
                None,
                vec![],
            )?;
            graph_db_snapshot_fail_if_not_fresh("snapshot-import post-install", &operator)?;
            Ok(graph_db_snapshot_report_from_operator(
                operator,
                doctor,
                GraphDbSnapshotReportInput {
                    operation: "snapshot-import",
                    success_status: "imported",
                    artifact,
                    artifact_bytes,
                    database_bytes,
                    extra_next_commands: vec![
                        graph_db_snapshot_status_command(root, scope),
                        graph_db_snapshot_doctor_command(root, scope),
                    ],
                    extra_warnings: Vec::new(),
                },
            ))
        })();
        match post_install {
            Ok(report) => {
                if backup.exists() {
                    let _ = fs::remove_file(&backup);
                }
                Ok(report)
            }
            Err(err) => {
                let _ = fs::remove_file(&graph_db);
                if backup.exists() {
                    let _ = fs::rename(&backup, &graph_db);
                }
                Err(err)
                    .context("snapshot import failed post-install validation and was rolled back")
            }
        }
    })();
    if import_result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    import_result
}

fn print_graph_db_snapshot_report(
    report: &GraphDbSnapshotReport,
    format: OutputFormat,
) -> Result<()> {
    if format.json_output {
        print_json_or_envelope(
            report,
            &format,
            "graph-db",
            &report.operation,
            ToolEnvelopeSummary {
                text: format!(
                    "Graph DB {} {} with {} node(s), {} edge(s), {} byte artifact",
                    report.operation,
                    report.status,
                    report.counts.nodes,
                    report.counts.edges,
                    report.artifact_bytes
                ),
                metrics: vec![
                    envelope_metric("operation", &report.operation),
                    envelope_metric("status", &report.status),
                    envelope_metric("nodes", report.counts.nodes),
                    envelope_metric("edges", report.counts.edges),
                    envelope_metric("artifact_bytes", report.artifact_bytes),
                    envelope_metric("readiness", &report.readiness.status),
                ],
            },
            false,
            report.next_commands.clone(),
        )
    } else {
        println!(
            "graph-db {} status: {} artifact: {}",
            report.operation, report.status, report.artifact
        );
        println!("graph_db: {}", report.graph_db);
        println!(
            "projection: version={} hash={} watermark={}",
            report
                .freshness
                .projection_version
                .as_deref()
                .unwrap_or("<missing>"),
            report
                .freshness
                .content_hash
                .as_deref()
                .unwrap_or("<missing>"),
            report
                .freshness
                .source_watermark
                .as_deref()
                .unwrap_or("<missing>")
        );
        println!(
            "counts: {} node(s), {} edge(s), {} tombstone(s)",
            report.counts.nodes, report.counts.edges, report.counts.tombstones.total
        );
        println!(
            "bytes: artifact={} database={}",
            report.artifact_bytes, report.database_bytes
        );
        println!(
            "readiness: {} fail_closed={}",
            report.readiness.status, report.readiness.fail_closed
        );
        for diagnostic in &report.readiness.diagnostics {
            println!("readiness diagnostic: {diagnostic}");
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        for command in &report.next_commands {
            println!("next: {command}");
        }
        Ok(())
    }
}

pub(crate) fn cmd_graph_db_snapshot_export(
    root: &Path,
    scope: Option<&str>,
    output: &Path,
    force: bool,
    format: OutputFormat,
) -> Result<()> {
    let report = graph_db_snapshot_export_report(root, scope, output, force)?;
    print_graph_db_snapshot_report(&report, format)
}

pub(crate) fn cmd_graph_db_snapshot_import(
    root: &Path,
    scope: Option<&str>,
    artifact: &Path,
    replace: bool,
    format: OutputFormat,
) -> Result<()> {
    let report = graph_db_snapshot_import_report(root, scope, artifact, replace)?;
    print_graph_db_snapshot_report(&report, format)
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
            GraphDbExperimentalBackend::Surrealdb,
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

/// A trusted, fresh authored finding attached to a map node (community / hub /
/// focus) because it `concerns` a member of that node (#trt1p3 graph menu). The
/// graph store is the source of truth; this is the compact projection the map
/// overview carries so an agent can drill systems-overview → community → the
/// finding explaining the coupling.
#[derive(Serialize, Clone)]
struct GraphDbMapFindingRef {
    id: String,
    kind: String,
    title: String,
    about: String,
    anchor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
}

#[derive(Serialize)]
struct GraphDbMapCommunitySummary {
    id: usize,
    size: usize,
    top_members: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    findings: Vec<GraphDbMapFindingRef>,
}

#[derive(Serialize)]
struct GraphDbMapHub {
    id: String,
    kind: String,
    label: String,
    degree: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    findings: Vec<GraphDbMapFindingRef>,
}

#[derive(Serialize, Clone)]
struct GraphDbMapModuleEntry {
    module: String,
    node_count: usize,
    kinds: BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct GraphDbMapOverview {
    node_count: usize,
    edge_count: usize,
    community_count: usize,
    communities: Vec<GraphDbMapCommunitySummary>,
    top_hubs: Vec<GraphDbMapHub>,
    edge_kind_histogram: BTreeMap<String, usize>,
    modules: Vec<GraphDbMapModuleEntry>,
}

#[derive(Serialize)]
struct GraphDbMapReport {
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    backend: String,
    overview: GraphDbMapOverview,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<GraphDbMapFocusReport>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct GraphDbMapFocusReport {
    symbol: String,
    node_id: String,
    node_kind: String,
    node_label: String,
    degree: usize,
    community_id: Option<usize>,
    neighbor_count: usize,
    neighbor_kinds: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    findings: Vec<GraphDbMapFindingRef>,
}

/// Collect trusted, fresh findings concerning any of `about_keys`, bucketed by
/// the `about` anchor, for map annotation (#trt1p3 graph menu). Reuses the
/// Phase 2 trusted+fresh injection contract (`collect_injectable_findings`).
/// Fails open to an empty map when the findings store is absent or unreadable.
fn collect_map_findings_by_about(
    root: &Path,
    scope: Option<&str>,
    about_keys: &BTreeSet<String>,
) -> BTreeMap<String, Vec<GraphDbMapFindingRef>> {
    let findings =
        match crate::commands::finding::collect_injectable_findings(root, about_keys, scope) {
            Ok(findings) => findings,
            Err(_) => return BTreeMap::new(),
        };
    let mut by_about: BTreeMap<String, Vec<GraphDbMapFindingRef>> = BTreeMap::new();
    for finding in findings {
        by_about
            .entry(finding.about.clone())
            .or_default()
            .push(GraphDbMapFindingRef {
                id: finding.id,
                kind: finding.kind,
                title: finding.title,
                anchor_kind: finding.anchor_kind,
                confidence: finding.confidence,
                about: finding.about,
            });
    }
    by_about
}

/// Gather the distinct findings attached to any of `keys` (dedup by finding id,
/// ordered by id) for a single map-node annotation.
fn map_findings_for_keys<'a>(
    by_about: &BTreeMap<String, Vec<GraphDbMapFindingRef>>,
    keys: impl Iterator<Item = &'a String>,
) -> Vec<GraphDbMapFindingRef> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<GraphDbMapFindingRef> = Vec::new();
    for key in keys {
        if let Some(findings) = by_about.get(key) {
            for finding in findings {
                if seen.insert(finding.id.clone()) {
                    out.push(finding.clone());
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_graph_db_map(
    root: &Path,
    scope: Option<&str>,
    backend: &str,
    store: &impl GraphStore,
    focus: Option<&str>,
    top_hubs_limit: usize,
    community_limit: usize,
    _focus_depth: usize,
    format: OutputFormat,
    map_format: Option<crate::cli::MapFormat>,
    warnings: Vec<String>,
) -> Result<()> {
    let nodes = store.all_nodes()?;
    let edges = store.all_edges()?;

    let mut degree: HashMap<&str, usize> = HashMap::new();
    let mut edge_kind_histogram: BTreeMap<String, usize> = BTreeMap::new();
    for edge in &edges {
        *degree.entry(&edge.from_id).or_default() += 1;
        *degree.entry(&edge.to_id).or_default() += 1;
        *edge_kind_histogram.entry(edge.kind.clone()).or_default() += 1;
    }

    let node_by_id: HashMap<&str, &substrate::GraphNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut hubs: Vec<GraphDbMapHub> = degree
        .iter()
        .filter_map(|(&id, &deg)| {
            node_by_id.get(id).map(|node| GraphDbMapHub {
                id: id.to_string(),
                kind: node.kind.clone(),
                label: node.label.clone(),
                degree: deg,
                findings: Vec::new(),
            })
        })
        .collect();
    hubs.sort_by_key(|b| std::cmp::Reverse(b.degree));
    if top_hubs_limit > 0 && hubs.len() > top_hubs_limit {
        hubs.truncate(top_hubs_limit);
    }

    let mut modules: BTreeMap<String, GraphDbMapModuleEntry> = BTreeMap::new();
    for node in &nodes {
        let module = match node.properties.get("file") {
            Some(f) => {
                let path = Path::new(f);
                path.parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            }
            None => "unknown".to_string(),
        };
        let module_name = module.clone();
        let entry = modules
            .entry(module)
            .or_insert_with(|| GraphDbMapModuleEntry {
                module: module_name,
                node_count: 0,
                kinds: BTreeMap::new(),
            });
        entry.node_count += 1;
        *entry.kinds.entry(node.kind.clone()).or_default() += 1;
    }
    let modules: Vec<GraphDbMapModuleEntry> = modules.into_values().collect();

    let mut communities: Vec<GraphDbMapCommunitySummary> = Vec::new();
    {
        let mut from_map: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &edges {
            from_map.entry(&edge.from_id).or_default().push(&edge.to_id);
        }
        let edge_pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.from_id.clone(), e.to_id.clone()))
            .collect();
        let comm_result = tsift_graph::detect_communities(&edge_pairs);
        let mut comm_list: Vec<(usize, usize, Vec<String>)> = comm_result
            .communities
            .iter()
            .enumerate()
            .filter(|(_, c)| c.members.len() >= 2)
            .map(|(i, c)| {
                let mut names: Vec<String> = c.members.iter().map(|m| m.name.clone()).collect();
                names.sort();
                (i, c.members.len(), names)
            })
            .collect();
        comm_list.sort_by_key(|b| std::cmp::Reverse(b.1));
        if community_limit > 0 && comm_list.len() > community_limit {
            comm_list.truncate(community_limit);
        }
        for (i, size, members) in comm_list {
            let top_members: Vec<String> = members.into_iter().take(10).collect();
            communities.push(GraphDbMapCommunitySummary {
                id: i + 1,
                size,
                top_members,
                findings: Vec::new(),
            });
        }
    }

    // #trt1p3 graph menu: annotate communities and hubs with the trusted, fresh
    // authored findings that concern their members, so the overview drills
    // systems → community → the finding explaining the coupling. Anchored by the
    // displayed member names / hub labels (bounded), reusing the Phase 2
    // trusted+fresh injection contract. Fails open to no annotations.
    let mut finding_about_keys: BTreeSet<String> = BTreeSet::new();
    for community in &communities {
        for member in &community.top_members {
            finding_about_keys.insert(member.clone());
        }
    }
    for hub in &hubs {
        finding_about_keys.insert(hub.label.clone());
    }
    if let Some(symbol) = focus {
        finding_about_keys.insert(symbol.to_string());
    }
    let findings_by_about = collect_map_findings_by_about(root, scope, &finding_about_keys);
    if !findings_by_about.is_empty() {
        for community in &mut communities {
            community.findings =
                map_findings_for_keys(&findings_by_about, community.top_members.iter());
        }
        for hub in &mut hubs {
            hub.findings = map_findings_for_keys(&findings_by_about, std::iter::once(&hub.label));
        }
    }

    let overview = GraphDbMapOverview {
        node_count: nodes.len(),
        edge_count: edges.len(),
        community_count: communities.len(),
        communities,
        top_hubs: hubs,
        edge_kind_histogram,
        modules,
    };

    let focus_report = if let Some(symbol) = focus {
        let matches: Vec<&substrate::GraphNode> = nodes
            .iter()
            .filter(|n| n.label == symbol || n.id.contains(symbol))
            .collect();
        match matches.first() {
            Some(focus_node) => {
                let node_deg = degree.get(focus_node.id.as_str()).copied().unwrap_or(0);
                let incident = store.incident_edges(&focus_node.id, None)?;
                let mut neighbor_kinds: BTreeMap<String, usize> = BTreeMap::new();
                for edge in &incident {
                    let neighbor_id = if edge.from_id == focus_node.id {
                        &edge.to_id
                    } else {
                        &edge.from_id
                    };
                    if let Some(neighbor) = node_by_id.get(neighbor_id.as_str()) {
                        *neighbor_kinds.entry(neighbor.kind.clone()).or_default() += 1;
                    }
                }
                let comm_id = {
                    let edge_pairs: Vec<(String, String)> = edges
                        .iter()
                        .map(|e| (e.from_id.clone(), e.to_id.clone()))
                        .collect();
                    let comm_result = tsift_graph::detect_communities(&edge_pairs);
                    comm_result
                        .communities
                        .iter()
                        .position(|c| c.members.iter().any(|m| m.name == symbol))
                };
                let focus_findings = map_findings_for_keys(
                    &findings_by_about,
                    [symbol.to_string(), focus_node.label.clone()].iter(),
                );
                Some(GraphDbMapFocusReport {
                    symbol: symbol.to_string(),
                    node_id: focus_node.id.clone(),
                    node_kind: focus_node.kind.clone(),
                    node_label: focus_node.label.clone(),
                    degree: node_deg,
                    community_id: comm_id.map(|i| i + 1),
                    neighbor_count: incident.len(),
                    neighbor_kinds,
                    findings: focus_findings,
                })
            }
            None => None,
        }
    } else {
        None
    };

    let report = GraphDbMapReport {
        root: root.to_string_lossy().to_string(),
        scope: scope.map(str::to_string),
        backend: backend.to_string(),
        overview,
        focus: focus_report,
        warnings,
    };

    // #trt1p3 on-demand projection: md (greppable, commit-friendly) / html
    // (interactive human view) rendered from the same report the JSON view
    // serializes — the graph store stays the single source of truth.
    if let Some(map_format) = map_format {
        let rendered = match map_format {
            crate::cli::MapFormat::Md => render_graph_db_map_markdown(&report),
            crate::cli::MapFormat::Html => render_graph_db_map_html(&report),
        };
        println!("{rendered}");
        return Ok(());
    }

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "graph-db",
            "map",
            ToolEnvelopeSummary {
                text: format!(
                    "Graph map: {} nodes, {} edges, {} communities, {} hubs",
                    report.overview.node_count,
                    report.overview.edge_count,
                    report.overview.community_count,
                    report.overview.top_hubs.len(),
                ),
                metrics: vec![
                    envelope_metric("nodes", report.overview.node_count),
                    envelope_metric("edges", report.overview.edge_count),
                    envelope_metric("communities", report.overview.community_count),
                    envelope_metric("hubs", report.overview.top_hubs.len()),
                    envelope_metric("modules", report.overview.modules.len()),
                    envelope_metric("edge_kinds", report.overview.edge_kind_histogram.len()),
                ],
            },
            false,
            if report.focus.is_some() {
                vec![]
            } else {
                vec!["Use --focus <symbol> to add a focused deep-dive tier".to_string()]
            },
        )
    } else {
        print_graph_db_map_human(&report, format.compact);
        Ok(())
    }
}

fn print_graph_db_map_human(report: &GraphDbMapReport, compact: bool) {
    let ov = &report.overview;
    if compact {
        println!(
            "map n:{} e:{} com:{} hubs:{} mods:{} ek:{}",
            ov.node_count,
            ov.edge_count,
            ov.community_count,
            ov.top_hubs.len(),
            ov.modules.len(),
            ov.edge_kind_histogram.len(),
        );
        for hub in &ov.top_hubs {
            println!("  hub {} [{}] deg:{}", hub.label, hub.kind, hub.degree);
        }
        return;
    }

    println!("Graph Map ({}, {})", report.backend, report.root);
    if let Some(scope) = &report.scope {
        println!("scope: {}", scope);
    }
    println!(
        "Overview: {} nodes, {} edges, {} communities",
        ov.node_count, ov.edge_count, ov.community_count
    );

    if !ov.edge_kind_histogram.is_empty() {
        println!();
        println!("Edge kinds:");
        let mut kinds: Vec<_> = ov.edge_kind_histogram.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in kinds {
            println!("  {}: {}", kind, count);
        }
    }

    if !ov.top_hubs.is_empty() {
        println!();
        println!("Top hubs by degree:");
        for hub in &ov.top_hubs {
            println!("  {} [{}] degree={}", hub.label, hub.kind, hub.degree);
            for finding in &hub.findings {
                println!(
                    "    finding [{}] {} (about {})",
                    finding.kind, finding.title, finding.about
                );
            }
        }
    }

    if !ov.communities.is_empty() {
        println!();
        println!("Communities:");
        for comm in &ov.communities {
            let members_display = if comm.top_members.len() < comm.size {
                format!(
                    "{} ... (+{} more)",
                    comm.top_members.join(", "),
                    comm.size - comm.top_members.len()
                )
            } else {
                comm.top_members.join(", ")
            };
            println!("  [{}] {} members: {}", comm.id, comm.size, members_display);
            for finding in &comm.findings {
                println!(
                    "    finding [{}] {} (about {})",
                    finding.kind, finding.title, finding.about
                );
            }
        }
    }

    if !ov.modules.is_empty() {
        println!();
        let mut modules = ov.modules.clone();
        modules.sort_by_key(|b| std::cmp::Reverse(b.node_count));
        let display_modules: Vec<_> = if modules.len() > 15 {
            modules[..15].to_vec()
        } else {
            modules.clone()
        };
        println!("Modules (top {}):", display_modules.len());
        for module in display_modules {
            println!("  {}: {} nodes", module.module, module.node_count);
        }
    }

    if let Some(focus) = &report.focus {
        println!();
        println!("Focus: {} [{}]", focus.node_label, focus.node_kind);
        println!("  id: {}", focus.node_id);
        println!("  degree: {}", focus.degree);
        if let Some(comm_id) = focus.community_id {
            println!("  community: {}", comm_id);
        }
        println!("  neighbors: {}", focus.neighbor_count);
        if !focus.neighbor_kinds.is_empty() {
            print!("  neighbor kinds:");
            for (kind, count) in &focus.neighbor_kinds {
                print!(" {}={}", kind, count);
            }
            println!();
        }
        for finding in &focus.findings {
            println!(
                "  finding [{}] {} (about {})",
                finding.kind, finding.title, finding.about
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {}", warning);
    }
}

/// Render the map overview + attached findings as Markdown (#trt1p3). Greppable,
/// commit-friendly projection; the graph store remains the source of truth.
fn render_graph_db_map_markdown(report: &GraphDbMapReport) -> String {
    let ov = &report.overview;
    let mut out = String::new();
    out.push_str(&format!("# Graph Map — {}\n\n", report.backend));
    out.push_str(&format!("Root: `{}`\n", report.root));
    if let Some(scope) = &report.scope {
        out.push_str(&format!("Scope: `{scope}`\n"));
    }
    out.push_str(&format!(
        "\n**Overview:** {} nodes, {} edges, {} communities\n",
        ov.node_count, ov.edge_count, ov.community_count
    ));

    if !ov.edge_kind_histogram.is_empty() {
        out.push_str("\n## Edge kinds\n\n");
        let mut kinds: Vec<_> = ov.edge_kind_histogram.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (kind, count) in kinds {
            out.push_str(&format!("- `{kind}`: {count}\n"));
        }
    }

    if !ov.top_hubs.is_empty() {
        out.push_str("\n## Top hubs by degree\n\n");
        for hub in &ov.top_hubs {
            out.push_str(&format!(
                "- **{}** [{}] degree={}\n",
                hub.label, hub.kind, hub.degree
            ));
            for finding in &hub.findings {
                out.push_str(&map_finding_markdown_line(finding));
            }
        }
    }

    if !ov.communities.is_empty() {
        out.push_str("\n## Communities\n");
        for comm in &ov.communities {
            out.push_str(&format!(
                "\n### Community {} ({} members)\n\n",
                comm.id, comm.size
            ));
            let members = if comm.top_members.len() < comm.size {
                format!(
                    "{} … (+{} more)",
                    comm.top_members.join(", "),
                    comm.size - comm.top_members.len()
                )
            } else {
                comm.top_members.join(", ")
            };
            out.push_str(&format!("{members}\n"));
            for finding in &comm.findings {
                out.push_str(&map_finding_markdown_line(finding));
            }
        }
    }

    if !ov.modules.is_empty() {
        let mut modules = ov.modules.clone();
        modules.sort_by_key(|b| std::cmp::Reverse(b.node_count));
        out.push_str("\n## Modules\n\n");
        for module in modules.iter().take(15) {
            out.push_str(&format!(
                "- `{}`: {} nodes\n",
                module.module, module.node_count
            ));
        }
    }

    if let Some(focus) = &report.focus {
        out.push_str(&format!(
            "\n## Focus: {} [{}]\n\n",
            focus.node_label, focus.node_kind
        ));
        out.push_str(&format!("- id: `{}`\n", focus.node_id));
        out.push_str(&format!("- degree: {}\n", focus.degree));
        if let Some(community_id) = focus.community_id {
            out.push_str(&format!("- community: {community_id}\n"));
        }
        out.push_str(&format!("- neighbors: {}\n", focus.neighbor_count));
        for finding in &focus.findings {
            out.push_str(&map_finding_markdown_line(finding));
        }
    }

    out
}

fn map_finding_markdown_line(finding: &GraphDbMapFindingRef) -> String {
    let confidence = finding
        .confidence
        .map(|value| format!(", confidence {value}"))
        .unwrap_or_default();
    format!(
        "- 📌 {}: {} (about `{}`{})\n",
        finding.kind, finding.title, finding.about, confidence
    )
}

fn map_html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the map overview + attached findings as a self-contained HTML page
/// (#trt1p3) — an interactive human view of a large graph. Same data as the
/// JSON/Markdown projections; the graph store remains the source of truth.
fn render_graph_db_map_html(report: &GraphDbMapReport) -> String {
    let ov = &report.overview;
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!(
        "<title>Graph Map — {}</title>\n",
        map_html_escape(&report.backend)
    ));
    out.push_str("<style>body{font-family:system-ui,-apple-system,sans-serif;margin:2rem;max-width:60rem;color:#1a1a1a}h1{margin-bottom:.2rem}h2{margin-top:1.6rem;border-bottom:1px solid #ddd;padding-bottom:.2rem}h3{margin-bottom:.2rem}code{background:#f4f4f4;padding:.1rem .3rem;border-radius:3px}ul{margin-top:.3rem}.finding{color:#7a3e00;background:#fff7ec;border-left:3px solid #d98c2b;padding:.2rem .5rem;margin:.2rem 0;list-style:none}.meta{color:#666;font-size:.9rem}</style>\n");
    out.push_str("</head>\n<body>\n");
    out.push_str(&format!(
        "<h1>Graph Map — {}</h1>\n",
        map_html_escape(&report.backend)
    ));
    out.push_str(&format!(
        "<p class=\"meta\">Root: <code>{}</code>",
        map_html_escape(&report.root)
    ));
    if let Some(scope) = &report.scope {
        out.push_str(&format!(
            " · scope: <code>{}</code>",
            map_html_escape(scope)
        ));
    }
    out.push_str("</p>\n");
    out.push_str(&format!(
        "<p><strong>Overview:</strong> {} nodes, {} edges, {} communities</p>\n",
        ov.node_count, ov.edge_count, ov.community_count
    ));

    if !ov.edge_kind_histogram.is_empty() {
        out.push_str("<h2>Edge kinds</h2>\n<ul>\n");
        let mut kinds: Vec<_> = ov.edge_kind_histogram.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (kind, count) in kinds {
            out.push_str(&format!(
                "<li><code>{}</code>: {}</li>\n",
                map_html_escape(kind),
                count
            ));
        }
        out.push_str("</ul>\n");
    }

    if !ov.top_hubs.is_empty() {
        out.push_str("<h2>Top hubs by degree</h2>\n<ul>\n");
        for hub in &ov.top_hubs {
            out.push_str(&format!(
                "<li><strong>{}</strong> [{}] degree={}",
                map_html_escape(&hub.label),
                map_html_escape(&hub.kind),
                hub.degree
            ));
            let findings = map_findings_html_list(&hub.findings);
            if !findings.is_empty() {
                out.push_str(&format!("<ul>{findings}</ul>"));
            }
            out.push_str("</li>\n");
        }
        out.push_str("</ul>\n");
    }

    if !ov.communities.is_empty() {
        out.push_str("<h2>Communities</h2>\n");
        for comm in &ov.communities {
            out.push_str(&format!(
                "<h3>Community {} ({} members)</h3>\n",
                comm.id, comm.size
            ));
            let members = if comm.top_members.len() < comm.size {
                format!(
                    "{} … (+{} more)",
                    comm.top_members.join(", "),
                    comm.size - comm.top_members.len()
                )
            } else {
                comm.top_members.join(", ")
            };
            out.push_str(&format!("<p>{}</p>\n", map_html_escape(&members)));
            let findings = map_findings_html_list(&comm.findings);
            if !findings.is_empty() {
                out.push_str(&format!("<ul>{findings}</ul>\n"));
            }
        }
    }

    if !ov.modules.is_empty() {
        let mut modules = ov.modules.clone();
        modules.sort_by_key(|b| std::cmp::Reverse(b.node_count));
        out.push_str("<h2>Modules</h2>\n<ul>\n");
        for module in modules.iter().take(15) {
            out.push_str(&format!(
                "<li><code>{}</code>: {} nodes</li>\n",
                map_html_escape(&module.module),
                module.node_count
            ));
        }
        out.push_str("</ul>\n");
    }

    if let Some(focus) = &report.focus {
        out.push_str(&format!(
            "<h2>Focus: {} [{}]</h2>\n<ul>\n",
            map_html_escape(&focus.node_label),
            map_html_escape(&focus.node_kind)
        ));
        out.push_str(&format!(
            "<li>id: <code>{}</code></li>\n",
            map_html_escape(&focus.node_id)
        ));
        out.push_str(&format!("<li>degree: {}</li>\n", focus.degree));
        if let Some(community_id) = focus.community_id {
            out.push_str(&format!("<li>community: {community_id}</li>\n"));
        }
        out.push_str(&format!("<li>neighbors: {}</li>\n", focus.neighbor_count));
        out.push_str("</ul>\n");
        let findings = map_findings_html_list(&focus.findings);
        if !findings.is_empty() {
            out.push_str(&format!("<ul>{findings}</ul>\n"));
        }
    }

    out.push_str("</body>\n</html>\n");
    out
}

/// Render attached findings as `<li class="finding">` rows (no wrapping
/// `<ul>` — the caller wraps so hub/community/focus contexts control nesting).
fn map_findings_html_list(findings: &[GraphDbMapFindingRef]) -> String {
    let mut out = String::new();
    for finding in findings {
        let confidence = finding
            .confidence
            .map(|value| format!(" · confidence {value}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "<li class=\"finding\">📌 {}: {} (about <code>{}</code>{})</li>",
            map_html_escape(&finding.kind),
            map_html_escape(&finding.title),
            map_html_escape(&finding.about),
            confidence
        ));
    }
    out
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
    let preferred_path = if matches!(&query, GraphDbQuery::Evidence { .. }) {
        graph_db_evidence_preferred_path(&root, path)
    } else {
        None
    };
    let preferred_path = preferred_path.as_deref();
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
        GraphDbQuery::SnapshotExport { output, force } => {
            return cmd_graph_db_snapshot_export(&root, scope, output, *force, format);
        }
        GraphDbQuery::SnapshotImport { artifact, replace } => {
            return cmd_graph_db_snapshot_import(&root, scope, artifact, *replace, format);
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
                || graph_db_resolve_evidence_target_with_path(&store, target, preferred_path)?
                    .is_none()
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
                cursor,
            } = &query
            {
                let report = graph_db_evidence_report_from_store(GraphDbEvidenceInput {
                    root: &root,
                    scope,
                    backend: "sqlite",
                    target,
                    preferred_path,
                    depth: *depth,
                    limit: *limit,
                    cursor: cursor.as_deref(),
                    store: &store,
                    freshness,
                    warnings,
                })?;
                return print_graph_db_evidence_report(&report, format);
            }
            if let GraphDbQuery::Map {
                focus,
                top_hubs,
                community_limit,
                focus_depth,
                format: map_format,
            } = &query
            {
                return cmd_graph_db_map(
                    &root,
                    scope,
                    "sqlite",
                    &store,
                    focus.as_deref(),
                    *top_hubs,
                    *community_limit,
                    *focus_depth,
                    format,
                    *map_format,
                    warnings,
                );
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
                cursor,
            } = &query
            {
                let report = graph_db_evidence_report_from_store(GraphDbEvidenceInput {
                    root: &root,
                    scope,
                    backend: "convex-snapshot",
                    target,
                    preferred_path,
                    depth: *depth,
                    limit: *limit,
                    cursor: cursor.as_deref(),
                    store: &store,
                    freshness,
                    warnings,
                })?;
                return print_graph_db_evidence_report(&report, format);
            }
            if let GraphDbQuery::Map {
                focus,
                top_hubs,
                community_limit,
                focus_depth,
                format: map_format,
            } = &query
            {
                return cmd_graph_db_map(
                    &root,
                    scope,
                    "convex-snapshot",
                    &store,
                    focus.as_deref(),
                    *top_hubs,
                    *community_limit,
                    *focus_depth,
                    format,
                    *map_format,
                    warnings,
                );
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
        let mut metrics = vec![
            envelope_metric("backend", &report.backend),
            envelope_metric(
                "nodes",
                report.nodes.len() + usize::from(report.node.is_some()),
            ),
            envelope_metric("edges", report.edges.len()),
            envelope_metric("freshness", &report.freshness.status),
        ];
        let mut next_commands = Vec::new();
        if let Some(readiness) = &report.readiness {
            metrics.push(envelope_metric("readiness", &readiness.status));
            next_commands.extend(readiness.next_commands.clone());
        }
        next_commands.push(format!(
            "Use tsift convex-sync {} --json to inspect or refresh Convex projection rows",
            shell_quote(root.to_string_lossy().as_ref())
        ));
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
                metrics,
            },
            false,
            next_commands,
        )
    } else {
        print_graph_db_human(&report, format.compact);
        Ok(())
    }
}

fn status_index_needs_auto_fix(report: &status::StatusReport) -> bool {
    match &report.index {
        status::IndexStatus::Fresh { .. } => false,
        status::IndexStatus::Stale { recovery: None, .. } => true,
        status::IndexStatus::Stale {
            recovery: Some(_), ..
        } => false,
        status::IndexStatus::Missing { .. } => true,
    }
}

pub(crate) struct StatusCommandOptions {
    pub fix: bool,
    pub no_fix: bool,
    pub json_output: bool,
    pub compact: bool,
    pub pretty: bool,
    pub terse: bool,
    pub schema: bool,
}

pub(crate) fn cmd_status(path: &std::path::Path, options: StatusCommandOptions) -> Result<()> {
    if options.fix {
        eprintln!(
            "warning: --fix is deprecated; auto-fix is now the default. Use --no-fix to skip."
        );
    }
    let auto_fix = !options.no_fix;
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let status_cache = status::StatusCheckCache::new();
    let mut report = status::check_status_with_cache(&root, &status_cache)?;
    if status_missing_workspace_scopes(&report) {
        autoindex_missing_workspace_scopes(&root, &report)?;
        status_cache.invalidate_all();
        report = status::check_status_with_cache(&root, &status_cache)?;
    }
    if auto_fix && status_index_needs_auto_fix(&report) {
        apply_status_fixes(&root, &report)?;
        status_cache.invalidate_all();
        report = status::check_status_with_cache(&root, &status_cache)?;
        if status_missing_workspace_scopes(&report) {
            autoindex_missing_workspace_scopes(&root, &report)?;
            status_cache.invalidate_all();
            report = status::check_status_with_cache(&root, &status_cache)?;
        }
    }
    if options.json_output {
        println!(
            "{}",
            to_json_schema(
                &report,
                options.pretty,
                options.terse,
                false,
                options.schema
            )?
        );
    } else {
        print!("{}", status::format_human(&report, options.compact));
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
        println!("{}", to_json_schema(&report, pretty, terse, false, schema)?);
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
                println!(
                    "{}",
                    to_json_schema(&json_rows, pretty, terse, false, schema)?
                );
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
                println!("{}", to_json_schema(&cols, pretty, terse, false, schema)?);
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
                println!("{}", to_json_schema(&tables, pretty, terse, false, schema)?);
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
